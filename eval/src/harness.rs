//! The driver (docs/eval.md, "Driver"): one scenario run in one process,
//! the logic VM, the input router and the renderer on a virtual clock, as
//! the runtime wires them. Every reaction of the shell's main thread
//! (logic's diff arriving, the loop waking, an input event, a surface
//! attached, configured or detached, a frame painted) is the shell's own
//! code, `strand_render::host_loop`, run with the driver's side behind
//! `LoopHost` and `LoopSync` (docs/decisions.md, eval-host-loop); what is
//! the driver's own is when they run, and its surfaces.
//!
//! After each reaction the loop turns ([`Harness::turn`]): a wake the
//! reaction owed runs `host_loop::woke`, then logic answers what routing
//! told it (`Instance::flush`) and the answer is applied, each as a
//! reaction of its own, as the shell's loop runs its ping and logic's
//! diff as callbacks after the one that caused them.
//!
//! Each tick `k` at time `t(k)`:
//! 1. the instant steps before it have acted (state, input, environment);
//!    an input event is a reaction (`host_loop::input`);
//! 2. logic runs its timers due by `t` and steps to `t`; a diff that is
//!    not empty is applied (`host_loop::apply`), as the shell's logic
//!    thread sends only diffs with something in them;
//! 3. the loop wakes between frames only for what wakes the shell's
//!    (crates/strand/src/demo/mod.rs `text_ready`): texts released `via
//!    waker`, or the renderer's own due time ([`Renderer::next_wake`]: a
//!    hover popup's delay or grace, a tooltip's delay, a capped clock's
//!    tick, on the virtual clock). So do the reactions that owe a wake
//!    (surface changes left behind, input that wakes an idle surface),
//!    and text shaped inline (a text worker wakes the loop for each
//!    layout it delivers, `Renderer::take_inline_shaped`);
//! 4. a frame held for logic's answer to its layout facts (a container
//!    query on an idle surface, the renderer's `query_hold`) gets it;
//! 5. texts released `via paint` are delivered, then every surface that
//!    wants a frame (or was just configured) paints at `t`, once, each
//!    paint a reaction (`host_loop::painted`);
//! 6. the frame traces, held texts and commands are collected, and the
//!    layout facts the frames laid out go to logic, whose answer is
//!    applied before the next tick's frame (the shell's logic thread
//!    answers between frames).
//!
//! Wall-clock holds are neutralised with the renderer's own setters (no
//! first-frame, new-text, estimate, busy or resize wait; a long query
//! wait, since logic here always answers before the frame it holds), and
//! the router's and renderer's timers read the virtual clock
//! (`set_now`), and a wallpaper palette being quantised off-thread is
//! waited for as the next tick starts (`Harness::settle_images`), so the
//! run is a function of the scenario.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, NodeFlag, Storage, Update, parse_text};
use strand_compiler::lower::Program;
use strand_compiler::reconcile::Build;
use strand_compiler::schema::Schema;
use strand_compiler::ty::Ty;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::{MOCK_TIME, SchemaHost};
use strand_core::Runtime;
use strand_render::host_loop::{self, Cause, LoopHost, LoopSync};
use strand_render::probe::{Axes, FrameTrace, Hidden, NodeTrace, TextDrawn};
use strand_render::{Renderer, Router, TextBackend};
use strand_scene::{
    ButtonState, Damage, InputEvent, KeyInput, Keyboard, LogicalPoint, LogicalSize, Modifiers,
    NodeId, PaintTarget, Painter, Prop, PropValue, SceneDiff, Size, SurfaceId,
};
use strand_text::{FontConfig, TextEngine};

use crate::Error;
use crate::checks::redraw;

/// (history) The boot clock at `T0` for what a run's kept history and its
/// graphs read: the run's own virtual clock, as the live shell's is the
/// machine's, so a graph's backlog lands where its run's time puts it.
pub const HISTORY_BOOT: Duration = Duration::from_secs(1_000_000);

thread_local! {
    /// (history) The virtual boot clock now: its time at `T0` plus the
    /// run's time since, set as each logic step and tick starts.
    static VIRTUAL_BOOT: std::cell::Cell<Duration> = const { std::cell::Cell::new(HISTORY_BOOT) };
    /// (graph-follow) The virtual boot clock at `T0`: the scenario's
    /// `boot` (a replay's: the live shell's at its window's start), else
    /// `HISTORY_BOOT`.
    static BOOT_AT_T0: std::cell::Cell<Duration> = const { std::cell::Cell::new(HISTORY_BOOT) };
}

fn boot_at_t0() -> Duration {
    BOOT_AT_T0.with(std::cell::Cell::get)
}

/// (graph-follow) The virtual boot clock at a frame's time (the run's
/// clock, `T0` at its start): what render lays a graph's columns on, the
/// clock logic's kept history is timed on.
fn render_boot(p: Duration) -> Duration {
    boot_at_t0() + p.saturating_sub(T0)
}

fn virtual_boot() -> Duration {
    VIRTUAL_BOOT.with(std::cell::Cell::get)
}

/// The virtual clocks: the machine never sleeps in a run.
fn virtual_clocks() -> strand_scene::backlog::Clocks {
    let b = virtual_boot();
    strand_scene::backlog::Clocks { boot: b, awake: b }
}

/// (history) A `[seed]`'s `history:<key>` value: `[[at, value, ramp],
/// …]`, oldest first, `at` in seconds from the run's start (before it:
/// negative), a `null` value a gap's start; on the virtual boot clock.
fn history_points(j: &serde_json::Value) -> Result<Vec<strand_scene::BacklogPoint>, String> {
    let bad = || format!("`{j}` is not a list of [at, value, ramp]");
    let base = boot_at_t0().as_secs_f64();
    j.as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|p| {
            let at = p
                .get(0)
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(bad)?;
            let value = match p.get(1) {
                Some(serde_json::Value::Null) => f32::NAN,
                Some(v) => v.as_f64().ok_or_else(bad)? as f32,
                None => return Err(bad()),
            };
            let ramp = p
                .get(2)
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(bad)? as f32;
            Ok(strand_scene::BacklogPoint {
                at: base + at,
                value,
                ramp,
            })
        })
        .collect()
}

/// The virtual boot clock at the run's time `t`.
fn set_virtual_boot(t: Duration) {
    VIRTUAL_BOOT.with(|c| c.set(boot_at_t0() + t.saturating_sub(T0)));
}
use crate::clock::{Clock, T0};
use crate::gpu::{Device, Fresh};
use crate::input::{Pointer, to_logic};
use crate::labels::Labels;
use crate::scenario::{BackendMode, Config, Fonts, Scenario, Screen, TextMode};
use crate::select::Selector;
use crate::steps::{
    Button, ExpectKind, Intention, KeyMods, KeyPhase, Point, REST_MAX, Step, StepKind, Via,
};
use crate::surfaces::{Change, Surfaces, to_rgba};
use crate::text::TextRules;
use crate::trace::{
    BlurRec, BuildInfo, CommandRec, Cost, EventRec, ExpectRec, Frame, HeldRec, IntendRec, PoseRec,
    RedrawRec, Regions, RestRec, Row, RunTrace, ScenarioInfo, Segment, SurfaceInfo, q,
};
use crate::wire::{Leg, Snapshot};

/// How to run a scenario.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Motion reduced from tick 0, whatever the scenario says (the twin).
    pub reduced_motion: bool,
    /// Keep the image each painted surface showed.
    pub images: bool,
    pub build: BuildInfo,
    /// The leg to run, whatever the scenario's `backend` says: `Cpu` or
    /// `Gpu` (a `both` scenario runs once on each, docs/eval.md "CPU and
    /// GPU"). `None` runs the scenario's own, `both` as its CPU leg.
    pub backend: Option<BackendMode>,
    /// Compare every painted frame's buffer with a full repaint of the
    /// same frame into a fresh one (the `redraw` check: `Frame::redraw`).
    pub redraw: bool,
    /// The `redraw` check's own tests only: each frame's own damage
    /// shrunk by this many pixels on every side
    /// (`Renderer::set_damage_inset`, a mutant). 0 is none.
    pub damage_inset: u32,
    /// The `redraw` check's own tests only: the inspector's damage flash
    /// on (`Renderer::set_damage_flash`), so every frame draws an overlay
    /// over its own items.
    pub damage_flash: bool,
    /// (eval-surface-leg) Run the surface leg (`Some(true)`) or not
    /// (`Some(false)`: the twin and the determinism run, which compare
    /// no commits), whatever the scenario says; `None` follows its
    /// `surface`. The frames go through the real surface manager to a
    /// fake compositor, recorded in `RunTrace::wire`.
    pub surface: Option<bool>,
    /// `late-present`'s own tests only: the fake compositor's refreshes
    /// at these ticks never come (an output that stalls).
    pub lost_vblanks: Vec<u32>,
    /// (report) Keep what a replay compares with the live shell
    /// ([`RunOutput::log`]): each press's hit and each cell write.
    pub log: bool,
    /// (replay-owner review) After every painted frame, hold
    /// `Renderer::drawn_nodes` (what a glitch report's `drawn.json` is
    /// taken from) to the frame's own trace, node by node: the same
    /// nodes in paint order, each at its `visual` box with its own
    /// opacity. What parts goes to [`ReplayLog::drawn`].
    pub drawn: bool,
}

/// (report) A node as a replay compares it with the live shell's
/// (`nodes.json`): where it is written, its `id:` and its `for` row key.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeRef {
    /// `file.strand:LINE:COL`, the file under the config directory.
    pub origin: String,
    pub id: Option<String>,
    pub key: Option<String>,
}

impl std::fmt::Display for NodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}", self.origin)?;
        if let Some(k) = &self.key {
            write!(f, "[{k}]")?;
        }
        if let Some(i) = &self.id {
            write!(f, " #{i}")?;
        }
        Ok(())
    }
}

/// (report) One press of a run: its tick and time, and the innermost
/// node it hit (`Renderer::hit`'s first), as the shell records a press.
#[derive(Clone, Debug, PartialEq)]
pub struct PressLog {
    pub k: u32,
    pub t: Duration,
    pub hit: Option<NodeRef>,
}

/// (report) One cell write of a run (`Instance::set_cell_tap`): its tick
/// and time, the cell's path and its new value as compact JSON, a text
/// that is not a literal of the program held back (`{"~text": N}`), as
/// the shell records it.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteLog {
    pub k: u32,
    pub t: Duration,
    pub path: String,
    pub value: String,
}

/// (services) A config's write to a service, as the sandbox's stand-in
/// took it: a field's (`audio.sink.muted`, as a `WRITE` record names
/// it) or, `item`, an item's of a keyed list (`AudioDevice(56).volume`,
/// the item's record and key), with the value written, as JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceWriteLog {
    pub k: u32,
    pub t: Duration,
    pub path: String,
    pub value: serde_json::Value,
    pub item: bool,
}

/// (report review) A replay's step the sandbox could not take: its
/// line, when, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct SkippedStep {
    pub line: u32,
    pub t: Duration,
    pub msg: String,
}

/// (report) What a run kept for a replay's comparison.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayLog {
    pub presses: Vec<PressLog>,
    pub writes: Vec<WriteLog>,
    /// (services) The config's writes to services, in order.
    pub service_writes: Vec<ServiceWriteLog>,
    /// (report review) Steps skipped (`RunOptions::log` only).
    pub skipped: Vec<SkippedStep>,
    /// Each fetch: its tick, method, and whether a queued `answer` gave
    /// it (else the mock's own did).
    pub fetches: Vec<(u32, String, bool)>,
    /// (replay-owner) Each traced node's key, as a replay names the node
    /// (`nodes.json`'s and `drawn.json`'s origin, `id:` and row key),
    /// taken when it was first drawn: what the replay's boxes at the
    /// report are matched to the live shell's by.
    pub refs: BTreeMap<String, NodeRef>,
    /// (replay-owner review) With `RunOptions::drawn`: each painted
    /// frame where `Renderer::drawn_nodes` parts from the frame's trace,
    /// one line per difference. Empty when they agree.
    pub drawn: Vec<String>,
    /// How many painted frames were held so.
    pub drawn_frames: usize,
}

/// A painted surface's image, PNG-encoded as it is collected (a run's
/// images stay small in memory; `report::png::decode` reads one back).
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// The PNG file (straight-alpha RGBA, 8 bits).
    pub png: Vec<u8>,
}

/// What a run made.
#[derive(Debug)]
pub struct RunOutput {
    pub trace: RunTrace,
    /// The images frames name (`Frame::png`), by name.
    pub images: BTreeMap<String, Image>,
    /// What the shell refused or the sandbox could not do, as it ran.
    pub notes: Vec<String>,
    /// (report) With `RunOptions::log`: presses and cell writes.
    pub log: ReplayLog,
}

/// Runs `sc`.
pub fn run(sc: &Scenario, opts: &RunOptions) -> Result<RunOutput, Error> {
    let mut h = Harness::boot(Rc::new(sc.clone()), opts)?;
    for step in &sc.steps {
        match h.step(step) {
            // (report review) A replay's step the sandbox cannot take (a
            // surface it never shows, a node it never mounted: the live
            // shell parted from it before) is skipped and kept for the
            // comparison, so a replay always ends with its findings.
            Err(Error::Scenario { line, msg, .. }) if opts.log => {
                let t = h.clock.now();
                h.log.skipped.push(SkippedStep { line, t, msg });
            }
            r => r?,
        }
    }
    h.finish()
}

/// (eval-live) A run driven one step, or one tick, at a time: the live
/// sandbox's (`strand-eval live`, docs/eval.md "Live sandbox"). It is
/// [`run`]'s driver, the same reactions in the same order: a step
/// [`begin`](Driver::begin)s as [`run`] begins it, and a time step's
/// ticks [`advance`](Driver::advance) as [`run`] advances them, so a
/// session's steps, written out as a scenario, run to the same trace.
/// Between steps the clock may run with no step driving it (a
/// [`Span`]), which the session writes as the `rest` or `frames N`
/// that runs the same ticks.
pub struct Driver {
    h: Harness,
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("scenario", &self.h.sc.id)
            .field("k", &self.h.clock.k())
            .finish_non_exhaustive()
    }
}

impl Driver {
    /// Boots `sc`'s config (its steps are not run: the caller runs them).
    pub fn boot(sc: Scenario, opts: &RunOptions) -> Result<Driver, Error> {
        Ok(Driver {
            h: Harness::boot(Rc::new(sc), opts)?,
        })
    }

    /// Begins `step` at the tick about to run: an instant step is done,
    /// a time step's ticks are left to [`Driver::advance`].
    pub fn begin(&mut self, step: &Step) -> Result<Option<TimeRun>, Error> {
        self.h.begin(step)
    }

    /// One tick of `run`; whether it is done.
    pub fn advance(&mut self, run: &mut TimeRun) -> Result<bool, Error> {
        self.h.advance(run)
    }

    /// The clock runs with no step driving it, from the tick about to
    /// run; `line` is the line its step will be written on.
    pub fn span_begin(&mut self, line: u32) -> Span {
        self.h.span_begin(line)
    }

    /// One tick of `span`; whether its segment has now come to rest
    /// (where a `rest` begun with it would end).
    pub fn span_tick(&mut self, span: &mut Span) -> Result<bool, Error> {
        self.h.span_tick(span)
    }

    /// `span` ended (`rested`: at the tick its segment came to rest); the
    /// step that runs its ticks, as it is written.
    pub fn span_end(&mut self, span: Span, rested: bool) -> Result<String, Error> {
        self.h.span_end(span, rested)
    }

    /// The tick about to run, and its time.
    pub fn k(&self) -> u32 {
        self.h.clock.k()
    }

    pub fn now(&self) -> Duration {
        self.h.clock.now()
    }

    pub fn hz(&self) -> u32 {
        self.h.clock.hz()
    }

    /// The surface the pointer is on (`None`: none, or it left).
    pub fn pointer_on(&self) -> Option<u32> {
        self.h.pointer.surface.map(|s| s.0)
    }

    /// Whether motion is reduced now (render's last `reduced_motion`).
    pub fn reduced(&self) -> bool {
        self.h.reduced
    }

    pub fn scenario(&self) -> &Scenario {
        &self.h.sc
    }

    /// The shown surfaces, as the sandbox lays them out: each one's
    /// buffer, where it lies on the screen and its size.
    pub fn surfaces(&self) -> &Surfaces {
        &self.h.surfaces
    }

    /// The trace so far (the open segment still open, its nodes not yet
    /// gathered: [`Driver::snapshot`] is the trace a run ending here
    /// would leave).
    pub fn trace(&self) -> &RunTrace {
        &self.h.trace
    }

    /// What the shell refused or the sandbox could not do, so far.
    pub fn notes(&self) -> &[String] {
        &self.h.notes
    }

    /// Where surface `id` takes input, logical (`None`: everywhere).
    pub fn input_region(&self, id: SurfaceId) -> Option<Vec<strand_scene::LogicalRect>> {
        Painter::input_region(&self.h.r, id)
    }

    /// A selector naming surface `id` that resolves to it now, as a
    /// step's `on` resolves: its root's `#id`, else its kind's place
    /// among the surface roots (`popup[1]`, which an edit of the config
    /// above it keeps), else where it is written.
    pub fn surface_selector(&mut self, id: SurfaceId) -> Option<String> {
        let node = self.h.surfaces.get(id)?.node;
        let views = self
            .h
            .labels
            .views(&self.h.inst, self.h.r.tree(), &self.h.sources);
        let roots = self.h.r.tree().roots().to_vec();
        let view = views.iter().find(|v| v.node == node)?;
        let mut cands = Vec::new();
        if let Some(i) = &view.id {
            cands.push(format!("#{i}"));
        }
        let nth = roots
            .iter()
            .filter_map(|r| views.iter().find(|v| v.node == *r))
            .filter(|v| v.kind == view.kind)
            .position(|v| v.node == node);
        if let Some(n) = nth {
            cands.push(format!("{}[{n}]", view.kind.name()));
        }
        if let Some((f, l, c)) = &view.origin {
            cands.push(format!("@{f}:{l}:{c}"));
            cands.push(format!("@{f}:{l}:{c}[{}]", view.sibling_index));
        }
        cands.into_iter().find(|c| {
            Selector::parse(c)
                .ok()
                .and_then(|sel| self.h.surface_for(&Some(sel), 0).ok())
                == Some(id)
        })
    }

    /// The value at `path` as `set` takes it: a bool's `true` or
    /// `false`, an enum's value name (`None` for anything else).
    pub fn value_text(&self, path: &str) -> Option<String> {
        match self.h.value_at(path).ok()? {
            Value::Bool(b) => Some(b.to_string()),
            Value::Enum(e, i) => self
                .h
                .inst
                .vm()
                .prog
                .types
                .enum_(e)
                .variants
                .get(i as usize)
                .cloned(),
            _ => None,
        }
    }

    /// The trace a run ending at this tick would leave: the open segment
    /// closed unsettled, every node seen gathered, sealed.
    pub fn snapshot(&self) -> RunTrace {
        let mut t = self.h.trace.clone();
        if let Some(seg) = self.h.open.and_then(|i| t.segments.get_mut(i)) {
            seg.e = self.h.last_k.unwrap_or(0);
            seg.settled = false;
            seg.hz = self.h.clock.hz();
        }
        t.nodes = self.h.labels.metas.clone();
        t.written = self.h.labels.written.clone();
        t.seal();
        t
    }
}

/// A logic wake later than this counts as nothing to wait for.
const REST_HORIZON: Duration = Duration::from_secs(10);
/// Rounds of layout facts a held frame waits on within one tick.
const FACT_ROUNDS: usize = 8;
/// Turns of the loop in a row with a wake or logic's answer still owed
/// (logic answering routing's settle, which settles again; text shaped
/// inline on each wake) before the run is called stuck.
const TURN_ROUNDS: usize = 64;
/// Logic timers run within one tick before the run is called stuck.
const TIMER_ROUNDS: usize = 10_000;
/// How long a tick waits for a wallpaper palette being quantised
/// ([`Harness::settle_images`]) before the run notes it.
const IMAGE_WAIT: Duration = Duration::from_secs(30);

/// The sources of `sc`'s config, named as they are written in it.
fn sources(sc: &Scenario) -> Result<SourceMap, Error> {
    sources_of(&sc.config)
}

/// The sources of `config`, named as they are written in it.
fn sources_of(config: &Config) -> Result<SourceMap, Error> {
    let mut map = SourceMap::new();
    match config {
        Config::Inline(sections) => {
            for s in sections {
                map.add(s.path.clone(), s.text.clone());
            }
        }
        Config::Path(p) => {
            let found = strand_compiler::source::find_files(p)
                .map_err(|e| Error::Io(p.display().to_string(), e))?;
            let root = if p.is_file() {
                p.parent().unwrap_or(p).to_path_buf()
            } else {
                p.clone()
            };
            for f in found.files {
                let text = std::fs::read_to_string(&f)
                    .map_err(|e| Error::Io(f.display().to_string(), e))?;
                let name = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
                map.add(name, text);
            }
        }
    }
    if map.is_empty() {
        return Err(Error::Config("the config has no `.strand` file".into()));
    }
    Ok(map)
}

/// The directory `sc`'s config is read from: a named config's (the
/// directory named, or a file's parent), else the scenario file's. A
/// matrix cell's file is named under `eval/matrix/cells/`, which holds
/// none: its config is the fixture filled in, read against the fixture's
/// directory, as the shell reads a config against its own
/// (eval-coverage-3: a relative image path through `cells/..` named no
/// file). (wallpaper-follow) A docs scenario's dark twin, under
/// `eval/scenarios/docs/dark/`, is its scenario in dark mode: its config
/// is read against the scenario's own directory, so a relative image
/// path names the same file in both.
fn config_dir(sc: &Scenario) -> std::path::PathBuf {
    let dir = |p: &std::path::Path| {
        p.parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf()
    };
    match &sc.config {
        Config::Path(p) if p.is_dir() => p.clone(),
        Config::Path(p) => dir(p),
        Config::Inline(_) => {
            let d = dir(&sc.file);
            if d == crate::matrix::dir().join("cells") {
                crate::matrix::dir()
            } else if d.ends_with("scenarios/docs/dark") {
                dir(&d)
            } else {
                d
            }
        }
    }
}

/// `sc`'s config compiled, with its sources: what it declares (the
/// starter of `explore` reads it).
pub fn compile(sc: &Scenario) -> Result<(Arc<Program>, SourceMap), Error> {
    let map = sources(sc)?;
    let services = strand_services_schema::schemas();
    let schema = Schema::builtin_with(&services)
        .map_err(|(i, e)| Error::Config(format!("service schema {i}: {e:?}")))?;
    let build =
        Build::compile_with(None, map.clone(), &schema).map_err(|d| load_failed(sc, &map, &d))?;
    Ok((build.program, map))
}

/// (services) A builtin service field's default (`audio.sink`), as the
/// sandbox's stand-in holds it unseeded (its type's default: false, 0,
/// "", an empty list, null) and as a service's store holds it before its
/// first report, as JSON. `None` for no such field, and for `system`,
/// whose store starts at defaults of its own and is then seeded with
/// what it kept across restarts before the recorder starts, so no record
/// says where it began.
pub fn service_default(field: &str) -> Option<serde_json::Value> {
    let (svc, name) = field.split_once('.')?;
    if svc == "system" {
        return None;
    }
    let services = strand_services_schema::schemas();
    let schema = Schema::builtin_with(&services).ok()?;
    let r = schema.services.get(svc)?;
    let def = schema.types.records.get(r.0 as usize)?;
    let f = def.fields.iter().find(|f| f.name == name)?;
    let v = strand_compiler::vm::schema_host::default_value(&schema.types, &f.ty);
    serde_json::from_str(&strand_compiler::vm::json::to_json_string(
        &schema.types,
        &v,
    ))
    .ok()
}

/// (fidelity) The config in directory `dir` compiled, as a run compiles
/// it: what a replay's planner reads of the shell it replays.
pub fn compile_dir(dir: &std::path::Path) -> Result<Arc<Program>, Error> {
    compile_dir_with_sources(dir).map(|(p, _)| p)
}

/// (services) [`compile_dir`], with the sources its spans name (each
/// file named under `dir`).
pub fn compile_dir_with_sources(dir: &std::path::Path) -> Result<(Arc<Program>, SourceMap), Error> {
    let map = sources_of(&Config::Path(dir.to_path_buf()))?;
    let services = strand_services_schema::schemas();
    let schema = Schema::builtin_with(&services)
        .map_err(|(i, e)| Error::Config(format!("service schema {i}: {e:?}")))?;
    let build = Build::compile_with(None, map.clone(), &schema).map_err(|d| {
        Error::Config(format!(
            "{}: the config does not compile ({} diagnostics)",
            dir.display(),
            d.len()
        ))
    })?;
    Ok((build.program, map))
}

/// (services) A place in a config that writes an item of a service's
/// keyed list (`dev.volume = …` for `dev` an `AudioDevice`, as
/// `ServiceHost::write_item` takes it): a handler's assignment or list
/// mutation, or a two-way binding (`<->`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemWriteSite {
    /// `file:line`, the file named under the config directory.
    pub at: String,
    /// The item's field written (`volume`).
    pub field: String,
    /// The keyed lists whose items have that field writable
    /// (`audio.sinks`, `audio.sources`): the list the write lands in is
    /// the one holding the item's key, known only as it runs.
    pub lists: Vec<String>,
}

/// (services) Every place `prog` writes an item of a service's keyed
/// list ([`ItemWriteSite`]), by file and line, one per line and field. A recorder
/// before changes dump version 3 kept nothing of such a write.
pub fn item_write_sites(prog: &Program, map: &SourceMap) -> Vec<ItemWriteSite> {
    use strand_compiler::lower::code::{Op, Place, PlaceRoot, PlaceSeg};
    use strand_compiler::lower::{Node, Prop};
    let services = strand_services_schema::schemas();
    let schema = Schema::builtin_with(&services).ok();
    // The keyed lists whose items have `field` writable.
    let lists_of = |field: &str| -> Vec<String> {
        let Some(schema) = &schema else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (svc, r) in &schema.services {
            let Some(def) = schema.types.records.get(r.0 as usize) else {
                continue;
            };
            for f in &def.fields {
                let Ty::List(item, _) = f.ty.non_null() else {
                    continue;
                };
                let Ty::Record(ir) = item.non_null() else {
                    continue;
                };
                let Some(idef) = schema.types.records.get(ir.0 as usize) else {
                    continue;
                };
                if idef.key.is_some() && idef.fields.iter().any(|x| x.name == field && x.rw) {
                    out.push(format!("{svc}.{}", f.name));
                }
            }
        }
        out
    };
    let mut sites = BTreeMap::new();
    let mut site = |file: strand_compiler::source::FileId, start: u32, place: &Place| {
        if place.root != PlaceRoot::Item {
            return;
        }
        let Some(f) = map.get(file) else {
            return;
        };
        let (line, _) = crate::labels::line_col(&f.text, start as usize);
        let field = match place.segs.first() {
            Some(PlaceSeg::Field(n)) => n.clone(),
            _ => String::new(),
        };
        let lists = lists_of(&field);
        sites.insert((f.name.clone(), line, field), lists);
    };
    // Handlers, `fn`s and lambdas: every chunk's stores and mutations.
    for c in &prog.chunks {
        for (i, op) in c.ops.iter().enumerate() {
            let (Op::Store { place, .. } | Op::Mutate { place, .. }) = op else {
                continue;
            };
            let (Some(p), Some(span)) = (c.places.get(*place as usize), c.spans.get(i)) else {
                continue;
            };
            site(c.file, span.start, p);
        }
    }
    // Two-way bindings.
    fn props<'a>(ps: &'a [Prop], out: &mut Vec<&'a Prop>) {
        for p in ps {
            out.push(p);
            props(&p.sub, out);
        }
    }
    fn walk<'a>(ns: &'a [Node], out: &mut Vec<&'a Prop>) {
        for n in ns {
            match n {
                Node::Element(e) => {
                    props(&e.props, out);
                    props(e.arg.as_slice(), out);
                    walk(&e.children, out);
                    walk(&e.call_handlers, out);
                }
                Node::Surface(s) => {
                    props(&s.element.props, out);
                    props(s.element.arg.as_slice(), out);
                    walk(&s.element.children, out);
                    walk(&s.body.nodes, out);
                }
                Node::If { then, else_, .. } => {
                    walk(then, out);
                    walk(else_, out);
                }
                Node::For(f) => walk(&f.body.nodes, out),
                Node::Match { arms, .. } => {
                    for a in arms {
                        walk(a, out);
                    }
                }
                Node::When { props: ps, .. } | Node::Pose { props: ps, .. } => props(ps, out),
                _ => {}
            }
        }
    }
    let mut bound = Vec::new();
    for f in &prog.files {
        walk(&f.items, &mut bound);
    }
    for c in prog.components.values() {
        walk(&c.body.nodes, &mut bound);
    }
    for p in bound {
        let Some(tw) = &p.two_way else {
            continue;
        };
        let c = prog.chunk(p.value);
        if let Some(start) = c.spans.iter().map(|s| s.start).min() {
            site(c.file, start, &tw.place);
        }
    }
    sites
        .into_iter()
        .map(|((file, line, field), lists)| ItemWriteSite {
            at: format!("{file}:{line}"),
            field,
            lists,
        })
        .collect()
}

/// (fidelity) The cells `prog` keeps across restarts (`persist`), each
/// as a cell's path names it with its instance qualifiers taken off
/// (`Owner.name`, `module.name`): the live shell starts them from its
/// persist store, the sandbox, which has none, as the config declares
/// them.
pub fn persisted_cells(prog: &Program) -> std::collections::BTreeSet<String> {
    use strand_compiler::lower::Node;
    fn walk(ns: &[Node], out: &mut Vec<strand_compiler::hir::DefId>) {
        for n in ns {
            match n {
                Node::State(st) if st.persist => out.push(st.def),
                Node::Element(e) => walk(&e.children, out),
                Node::Surface(s) => {
                    walk(&s.element.children, out);
                    walk(&s.body.nodes, out);
                }
                Node::If { then, else_, .. } => {
                    walk(then, out);
                    walk(else_, out);
                }
                Node::For(f) => walk(&f.body.nodes, out),
                Node::Match { arms, .. } => {
                    for a in arms {
                        walk(a, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut defs = Vec::new();
    for f in &prog.files {
        walk(&f.items, &mut defs);
    }
    for c in prog.components.values() {
        walk(&c.body.nodes, &mut defs);
    }
    defs.into_iter()
        .map(|d| {
            let info = prog.def(d);
            let owner = info
                .owner
                .map_or_else(|| info.module.clone(), |o| prog.owner_name(o));
            format!("{owner}.{}", info.name)
        })
        .collect()
}

/// (fidelity) A cell's path with its instance qualifiers taken off:
/// `Notch[<screen id>].grown` is `Notch.grown`.
pub fn bare_cell_path(path: &str) -> String {
    let mut bare = String::with_capacity(path.len());
    let mut depth = 0usize;
    for c in path.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => bare.push(c),
            _ => {}
        }
    }
    bare
}

/// The values `paths` (top-level states) have once `sc`'s config has
/// booted, as `set` reads them: a bool's `true` or `false`, an enum's
/// value name. A path that is neither is left out (the starter of
/// `explore` puts each state back where it started).
pub fn state_values(sc: &Scenario, paths: &[String]) -> Result<BTreeMap<String, String>, Error> {
    let h = Harness::boot(Rc::new(sc.clone()), &RunOptions::default())?;
    let types = &h.inst.vm().prog.types;
    let mut out = BTreeMap::new();
    for p in paths {
        let text = match h.value_at(p) {
            Ok(Value::Bool(b)) => b.to_string(),
            Ok(Value::Enum(e, i)) => match types.enum_(e).variants.get(i as usize) {
                Some(v) => v.clone(),
                None => continue,
            },
            _ => continue,
        };
        out.insert(p.clone(), text);
    }
    Ok(out)
}

/// A config that does not load: its errors, then (builtin-name-diagnostics)
/// its warnings about meaning, often the cause and far from the errors (a
/// `state` a node boolean hides warns where it is declared, and its uses
/// fail as type errors), as a refused step says them.
fn load_failed(sc: &Scenario, map: &SourceMap, d: &[strand_compiler::Diagnostic]) -> Error {
    let errors = diagnostics(sc, map, d, true);
    let warnings = diagnostics(sc, map, d, false);
    if warnings.is_empty() {
        return Error::Config(errors);
    }
    let w = warnings.replace('\n', "\n  ");
    Error::Config(format!("{errors}\n  the config's warnings:\n  {w}"))
}

/// The compile errors (`errors`) or warnings, one per line, at
/// `file:line`.
fn diagnostics(
    sc: &Scenario,
    map: &SourceMap,
    diags: &[strand_compiler::Diagnostic],
    errors: bool,
) -> String {
    let mut out = Vec::new();
    // Warnings about meaning: the raw-colour lint is about style.
    let wanted = |d: &&strand_compiler::Diagnostic| {
        if errors {
            d.is_error()
        } else {
            d.severity == strand_compiler::diagnostic::Severity::Warning
                && d.code != "check::raw_color"
        }
    };
    for d in diags.iter().filter(wanted) {
        let file = map.get(d.file());
        let name = file.map_or("?", |f| f.name.as_str());
        let line = match (file, d.primary_span()) {
            (Some(f), Some(s)) => {
                f.text[..(s.start as usize).min(f.text.len())]
                    .matches('\n')
                    .count()
                    + 1
            }
            _ => 0,
        };
        let at = match &sc.config {
            Config::Inline(sections) => sections
                .iter()
                .find(|s| s.path == name)
                .map(|s| format!(" (scenario line {})", s.line as usize + line - 1))
                .unwrap_or_default(),
            Config::Path(_) => String::new(),
        };
        out.push(format!("{name}:{line}{at}: {}", d.message));
    }
    out.join("\n")
}

/// The font files under `dir`, its subdirectories included, sorted.
fn font_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), Error> {
    let rd = std::fs::read_dir(dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
    let mut here: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    here.sort();
    for p in here {
        if p.is_dir() {
            font_files(&p, out)?;
        } else if p
            .extension()
            .is_some_and(|x| x == "ttf" || x == "otf" || x == "ttc")
        {
            out.push(p);
        }
    }
    Ok(())
}

/// The fonts text is shaped with: the bundled pair, isolated; or, for
/// `fonts = "local"`, the machine's (fontconfig's, as the shell finds
/// them; under `run.sh eval local` the host's font directories) and the
/// files under each directory of `$STRAND_EVAL_FONTS` (`:`-separated).
fn fonts(sc: &Scenario) -> Result<FontConfig, Error> {
    let read = |p: std::path::PathBuf| {
        std::fs::read(&p)
            .map(Arc::new)
            .map_err(|e| Error::Io(p.display().to_string(), e))
    };
    match sc.fonts {
        Fonts::Bundled => Ok(FontConfig::isolated(vec![
            read(strand_text::test_font_path())?,
            read(strand_text::test_bold_font_path())?,
        ])),
        Fonts::Local => {
            let mut paths = Vec::new();
            if let Ok(dirs) = std::env::var("STRAND_EVAL_FONTS") {
                for d in dirs.split(':').filter(|d| !d.is_empty()) {
                    font_files(std::path::Path::new(d), &mut paths)?;
                }
            }
            Ok(FontConfig {
                system_fonts: true,
                fonts: paths.into_iter().map(read).collect::<Result<Vec<_>, _>>()?,
                ..FontConfig::default()
            })
        }
    }
}

/// The type at a field path inside `ty`.
fn field_ty(prog: &Program, mut ty: Ty, fields: &[&str]) -> Option<Ty> {
    for f in fields {
        ty = match ty.non_null() {
            Ty::Record(r) => prog
                .types
                .record(*r)
                .fields
                .iter()
                .find(|x| x.name == *f)?
                .ty
                .clone(),
            _ => return None,
        };
    }
    Some(ty)
}

/// The run in progress.
struct Harness {
    /// (eval-live) Owned, so a live session keeps its driver between
    /// steps ([`Driver`]).
    sc: Rc<Scenario>,
    opts: RunOptions,
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    schema: Schema,
    sources: Arc<SourceMap>,
    r: Renderer,
    router: Router,
    /// The router's instant at `T0`.
    i0: Instant,
    clock: Clock,
    surfaces: Surfaces,
    labels: Labels,
    text: TextRules,
    pointer: Pointer,
    trace: RunTrace,
    /// The segment open now, and intends waiting for the next.
    open: Option<usize>,
    waiting_intends: Vec<IntendRec>,
    /// (eval-coverage) `intend` steps whose selector matched nothing when
    /// they ran: they name a node the segment mounts later (a popup's
    /// card opening, a row entering), and resolve at the first tick that
    /// shows one. One still unmatched when its segment ends is an error
    /// at its line.
    pending_intends: Vec<(Selector, u32)>,
    /// Ticks left without a paint: every surface's, one surface's.
    drop_all: u32,
    drop_one: HashMap<SurfaceId, u32>,
    /// Motion is reduced now (render's last `reduced_motion`).
    reduced: bool,
    /// Motion is reduced whatever the run sets (the twin, a
    /// `--reduced-motion` run). (eval-rules-5) The scenario's own
    /// `reduce_motion` is where it starts: a `reduce-motion off` step
    /// turns it off.
    forced_reduced: bool,
    /// (eval-host-loop) A reaction asked the loop to wake
    /// (`LoopHost::wake`): `host_loop::woke` runs at its next turn.
    wake_owed: bool,
    /// (eval-host-loop) Routing told logic something since its last
    /// answer: logic answers (`Instance::flush`) at the loop's next
    /// turn, after the reaction that told it, as the shell's logic
    /// thread answers with a diff the main loop applies once its current
    /// callback returns.
    logic_owed: bool,
    /// The loop is turning now (`Harness::turn`).
    turning: bool,
    /// Logic's diff with changes, or input, reached the renderer since
    /// the last frame (`Frame::driven`).
    driven: bool,
    /// The pose each surface painted this tick commits with its frame,
    /// read right after its paint, as the surface manager reads it
    /// (`commit.rs`), before the loop handles the surface changes the
    /// paint left (a closing surface's removal).
    poses: BTreeMap<u32, PoseRec>,
    /// (popup-shrink) What each surface painted this tick committed: its
    /// logical size, origin, buffer size and pixels (with images), read
    /// right after its paint as `poses` is. A paint that leaves the
    /// surface a new size is configured at it before the tick is
    /// collected, and the frame shows the buffer the paint drew, where
    /// it lay then.
    committed: BTreeMap<u32, Committed>,
    /// (integration-r7) Nodes logic created as a list window's rows, and
    /// the tick, until first drawn (`RunTrace::windowed`).
    window_mounts: HashMap<NodeId, u32>,
    /// (integration-r7) Surface roots logic created, and the tick, until
    /// first drawn (`RunTrace::opened`).
    surface_opens: HashMap<NodeId, u32>,
    /// The last tick run.
    last_k: Option<u32>,
    images: BTreeMap<String, Image>,
    notes: Vec<String>,
    /// The config's load warnings but its style lint, one per line, at
    /// `file:line`: a step the shell refuses says them, since a warning
    /// (a bare name reading a builtin where a variant was meant) is often
    /// why.
    warnings: String,
    /// (png-aspect) Image files' own sizes by path ([`crate::filesize`]).
    file_sizes: HashMap<String, Option<[u32; 2]>>,
    /// The GPU leg's device, once started, and whether surfaces are drawn
    /// by it now (the `backend` header or step).
    gpu: Option<Device>,
    on_gpu: bool,
    /// Surfaces attached to the GPU thread.
    gpu_attached: HashSet<SurfaceId>,
    /// This tick's `redraw` comparisons by surface, with the difference
    /// each found (`None`: none).
    redraws: BTreeMap<u32, Option<RedrawRec>>,
    /// (eval-rules-4) This tick's crossfade blends redone by the check's
    /// own arithmetic, by surface.
    blends: BTreeMap<u32, crate::trace::BlendRec>,
    /// The `redraw` check's GPU device, once a GPU-drawn frame needs it.
    fresh_gpu: Option<Fresh>,
    /// (eval-surface-leg) The surface leg, when it runs.
    leg: Option<Leg>,
    /// (report) The surface the compositor gave the keyboard, as the
    /// router was told, and the surfaces that took it, newest last (it
    /// returns to the one before when a surface goes).
    kb: Option<SurfaceId>,
    kb_stack: Vec<SurfaceId>,
    /// (report) `[seed]` cells not mounted yet, seeded as they mount.
    seeds: Vec<(String, serde_json::Value)>,
    /// (report) The wall time `T0` stands for, less `T0` (`wall`).
    wall0: SystemTime,
    /// (report) The `warmup` is over (its last tick painted every
    /// surface).
    warmed: bool,
    /// (report) `RunOptions::log`: presses kept here, cell writes by the
    /// tap, with the tick the instance was stepped at.
    log: ReplayLog,
    tapped: Rc<std::cell::RefCell<Vec<(String, String)>>>,
    /// (fidelity) The cells a `[seed]` set, with the value the tap tells
    /// of it: a seed is where the replay starts, never a write of the
    /// shell's, so its tap is not kept.
    seeded: HashMap<String, String>,
    /// (flex review) The layout facts' batch handed to logic by
    /// [`LoopHost::facts`] (before an input's intents, as the shell's
    /// main thread sends them), which logic's next answer says it saw.
    facts_seq: Option<u64>,
    /// (flex review) Answers queued for the next calls of each result
    /// action (`answer action calc "4"`, a recording's answers): JSON,
    /// read as the action's type when its call comes.
    exec_answers: std::cell::RefCell<
        HashMap<String, std::collections::VecDeque<Result<serde_json::Value, String>>>,
    >,
    /// (lockmon) Each detached action's command the sandbox counts as
    /// running, as the shell's runner keeps them (`run/exec.rs`): one
    /// running (its start, wall time), from a call until `end action
    /// <name>` (a recording's end), and one re-run queued behind it,
    /// which a call while it runs sets and `rec.stop()` drops.
    sandbox_runs: std::cell::RefCell<HashMap<String, SandboxRun>>,
}

/// (lockmon review) A detached action's run in the sandbox, as the
/// shell's runner keeps it: the command running (when it started) and
/// whether a re-run is queued behind it.
#[derive(Debug, Default, Clone, Copy)]
struct SandboxRun {
    started: Option<SystemTime>,
    queued: bool,
}

/// (eval-live) A time step under way: its line, the tick it began at,
/// the ticks done and what it does.
#[derive(Clone, Debug)]
pub struct TimeRun {
    line: u32,
    k0: u32,
    done: u32,
    plan: Plan,
}

impl TimeRun {
    /// The ticks it has run.
    pub fn done(&self) -> u32 {
        self.done
    }

    /// It sends input as it runs (a glide or a drag): cut short, it
    /// would not be the step a scenario can write.
    pub fn sends_input(&self) -> bool {
        matches!(
            self.plan,
            Plan::Glide { .. } | Plan::Swipe { .. } | Plan::Type { .. }
        )
    }
}

#[derive(Clone, Debug)]
enum Plan {
    Ticks(u32),
    Rest {
        max: u32,
    },
    Glide {
        from: Point,
        to: Point,
        n: u32,
        surface: SurfaceId,
        drag: bool,
    },
    /// (integration-r4) A swipe: `dy` spread over `n` ticks, one axis
    /// frame each, the last lifting the fingers.
    Swipe {
        dy: f64,
        n: u32,
    },
    /// (report) `type N over MS`: `n` keys spread over `ticks` ticks.
    Type {
        n: u32,
        ticks: u32,
    },
}

/// (eval-live) The clock running with no step driving it (`Driver::span_tick`).
#[derive(Clone, Debug)]
pub struct Span {
    line: u32,
    k0: u32,
    ticks: u32,
    event: usize,
}

impl Span {
    pub fn ticks(&self) -> u32 {
        self.ticks
    }
}

/// Input a time step sends at the start of a tick.
enum Input {
    Move(SurfaceId, LogicalPoint),
    Button(Button, ButtonState),
    /// A touchpad axis frame: logical px, and whether the fingers lift.
    Swipe(f64, bool),
    /// (report) Keys typed on the surface with the keyboard.
    Keys(u32),
}

impl Harness {
    fn boot(sc: Rc<Scenario>, opts: &RunOptions) -> Result<Harness, Error> {
        let leg = match opts.backend.unwrap_or(sc.backend) {
            BackendMode::Gpu => BackendMode::Gpu,
            BackendMode::Cpu | BackendMode::Both => BackendMode::Cpu,
        };
        if sc.text == TextMode::Worker {
            return Err(Error::Unsupported("text = \"worker\"".into()));
        }
        let map = sources(&sc)?;
        let services = strand_services_schema::schemas();
        let schema = Schema::builtin_with(&services)
            .map_err(|(i, e)| Error::Config(format!("service schema {i}: {e:?}")))?;
        // As `Build::compile_with`, with the checked program kept long
        // enough to read (clip-data) each element's props as written.
        let compiled = strand_compiler::compile_with(&map, &schema);
        if compiled.errors() > 0 {
            return Err(load_failed(&sc, &map, &compiled.diagnostics));
        }
        let written = crate::labels::written_index(&compiled.program, &map);
        let build = Build::lowered(None, map.clone(), &compiled, &schema);
        drop(compiled);
        let warnings = diagnostics(&sc, &map, &build.warnings, false);
        let rt = Runtime::new();
        // (report) `wall`: the mock's clock and local time at it.
        let (wall0, host) = match sc.wall {
            Some(w) => {
                let at = UNIX_EPOCH + w.unix;
                let host = SchemaHost::mock_at(&rt, &build.program.types, at, w.offset);
                (at.checked_sub(T0).unwrap_or(UNIX_EPOCH), host)
            }
            None => (
                UNIX_EPOCH + Duration::from_secs(MOCK_TIME),
                SchemaHost::mock(&rt, &build.program.types),
            ),
        };
        let host = Rc::new(host);
        // (history) How the live services sample: the polled builtins
        // at their periods, a config's `from poll` every its `every`, so
        // a kept series' hidden samples ramp as the live shell's did.
        for (name, every) in strand_services_schema::POLLED {
            host.set_polls(name, Some(every));
        }
        for s in build.program.services.values() {
            if let strand_compiler::hir::SourceSpec::Poll { every, .. } = &s.source {
                host.set_polls(&s.name, Some(*every));
            }
        }
        // (report) `icons`: the config's icon folder, for this run.
        strand_icons::set_config_icons(sc.icons.clone());
        let reduced = opts.reduced_motion || sc.reduce_motion;
        let mut trace = RunTrace::new(
            ScenarioInfo {
                id: sc.id.clone(),
                file: sc.file.display().to_string(),
                hz: sc.hz,
                scale: sc.screen.scale,
                backend: leg.name().into(),
                text: "inline".into(),
                reduced_motion: reduced,
            },
            opts.build.clone(),
        );
        trace.segments.push(Segment {
            name: "boot".into(),
            hz: sc.hz,
            reduced_motion: reduced,
            ..Segment::default()
        });
        let engine = TextEngine::new(fonts(&sc)?);
        let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
        let boot = sc
            .boot
            .and_then(|b| Duration::try_from_secs_f64(b).ok())
            .unwrap_or(HISTORY_BOOT);
        BOOT_AT_T0.with(|c| c.set(boot));
        set_virtual_boot(T0);
        r.set_history_clock(render_boot);
        r.set_first_frame_wait(Duration::ZERO);
        r.set_new_text_wait(Duration::ZERO);
        r.set_estimate_wait(Duration::ZERO);
        r.set_busy_window(Duration::ZERO);
        r.set_resize_wait(Duration::ZERO);
        // Container queries are answered within the frame (`answer`).
        r.set_query_wait(Duration::from_secs(30));
        // Never `Duration::MAX`: pose.rs adds MAX_MOTION to it.
        r.set_exit_stall(Duration::from_secs(3600));
        if let Some(ms) = sc.tooltip_delay {
            r.set_tooltip_delay(Duration::from_millis(ms));
        }
        r.set_promotion(false);
        r.set_frame_trace(true);
        r.set_keep_frames(opts.redraw);
        r.set_damage_inset(opts.damage_inset);
        r.set_damage_flash(opts.damage_flash);
        // The compositor applies surface poses (as Hyprland, with
        // `wp_alpha_modifier_v1` and the viewporter, does): what each
        // frame commits besides its pixels is traced.
        r.set_compositor_poses(sc.compositor_poses);
        let gpu = match leg {
            BackendMode::Gpu => Some(Device::start(&mut r)?),
            _ => None,
        };
        let leg = if opts.surface.unwrap_or(sc.surface) {
            Some(Leg::start(
                sc.screen,
                &sc.identity,
                opts.lost_vblanks.iter().copied().collect(),
            )?)
        } else {
            None
        };
        let mut router = Router::new();
        let i0 = Instant::now();
        router.set_now(Some(i0));
        r.set_now(Some(i0));
        // Palette files (`import("base16:…")`) are read against the
        // config's directory, as the shell reads them against its own.
        let storage = Storage {
            config_dir: Some(config_dir(&sc)),
            ..Storage::none()
        };
        let inst = Instance::from_build(&rt, &build, host.clone(), storage);
        inst.set_history_clock(virtual_clocks);
        let mut h = Harness {
            sc: sc.clone(),
            opts: opts.clone(),
            rt,
            host,
            inst,
            schema,
            sources: Arc::new(map),
            r,
            router,
            i0,
            clock: Clock::new(sc.hz),
            surfaces: Surfaces::of_screens(&sc.screens, sc.main),
            labels: Labels::with_written(written),
            text: TextRules::default(),
            pointer: Pointer::default(),
            trace,
            open: Some(0),
            waiting_intends: Vec::new(),
            pending_intends: Vec::new(),
            drop_all: 0,
            drop_one: HashMap::new(),
            reduced: false,
            forced_reduced: opts.reduced_motion,
            wake_owed: false,
            logic_owed: false,
            turning: false,
            driven: false,
            poses: BTreeMap::new(),
            committed: BTreeMap::new(),
            window_mounts: HashMap::new(),
            surface_opens: HashMap::new(),
            last_k: None,
            images: BTreeMap::new(),
            notes: Vec::new(),
            warnings,
            file_sizes: HashMap::new(),
            on_gpu: gpu.is_some(),
            gpu,
            gpu_attached: HashSet::new(),
            redraws: BTreeMap::new(),
            blends: BTreeMap::new(),
            fresh_gpu: None,
            leg,
            kb: None,
            kb_stack: Vec::new(),
            seeds: Vec::new(),
            wall0,
            warmed: false,
            log: ReplayLog::default(),
            tapped: Rc::default(),
            seeded: HashMap::new(),
            facts_seq: None,
            exec_answers: Default::default(),
            sandbox_runs: Default::default(),
        };
        if opts.log {
            let prog = h.inst.vm().prog.clone();
            let out = h.tapped.clone();
            h.inst.set_cell_tap(Some(Box::new(
                move |w: strand_compiler::instantiate::CellWrite<'_>| {
                    let overlay = w.path.starts_with('$');
                    let mut s = String::new();
                    let _ =
                        strand_compiler::vm::json::to_json(&prog.types, w.new, &mut s, &mut |t| {
                            overlay || prog.is_text_literal(t)
                        });
                    out.borrow_mut().push((w.path.to_string(), s));
                },
            )));
        }
        if h.leg.is_some() {
            h.surfaces.tee = Some(Vec::new());
        }
        h.set_screens()?;
        for (path, text) in &sc.services {
            h.set_service(path, text)
                .map_err(|m| Error::Config(format!("[services] `{path}`: {m}")))?;
        }
        // (report) `[seed]`: services before boot, cells and tokens as
        // they mount.
        for (path, json) in &sc.seed {
            let j: serde_json::Value = serde_json::from_str(json)
                .map_err(|e| Error::Config(format!("[seed] `{path}`: {e}")))?;
            h.seed(path, j)
                .map_err(|m| Error::Config(format!("[seed] `{path}`: {m}")))?;
        }
        // Reduced motion is `system.reduced_motion`, which logic reads
        // and its diffs carry to render, as in the shell.
        if reduced {
            h.set_reduced_motion(true)?;
        }
        set_virtual_boot(T0);
        let (u, _) = h.inst.step(T0, h.wall(T0));
        h.update(u)?;
        if h.seed_cells()? {
            let u = h.inst.flush();
            h.update(u)?;
        }
        Ok(h)
    }

    /// (report) Node `n` as a replay compares it (`NodeRef`).
    fn node_ref(&self, n: NodeId) -> Option<NodeRef> {
        let (file, _, span) = self.inst.origin(n)?;
        let f = self.sources.get(file)?;
        let (l, c) = crate::labels::line_col(&f.text, span.start as usize);
        let ident = self.inst.identity(n);
        Some(NodeRef {
            origin: format!("{}:{l}:{c}", f.name),
            id: ident.as_ref().and_then(|i| i.id.clone()),
            key: ident.as_ref().and_then(|i| row_key(i, &self.sc.screens)),
        })
    }

    /// (report) The cell writes the tap was told of, kept at tick `k`.
    fn keep_writes(&mut self) {
        // The mock's logs are drained whether kept or not.
        let fetches = self.host.take_fetches();
        let service_writes = self.host.take_writes();
        if !self.opts.log {
            return;
        }
        let k = self.clock.k();
        let t = self.clock.now();
        // (services) The config's writes to services: an item's is named
        // by its record, which has a key (`AudioDevice(56).volume`), a
        // field's by its service (`audio.sink.volume`), whose record of
        // fields has none.
        let types = &self.inst.vm().prog.types;
        for w in service_writes {
            let head = w.path.split(['.', '[', '(']).next().unwrap_or("");
            let item = types
                .records
                .iter()
                .any(|r| r.name == head && r.key.is_some());
            let value =
                serde_json::from_str(&strand_compiler::vm::json::to_json_string(types, &w.value))
                    .unwrap_or(serde_json::Value::Null);
            self.log.service_writes.push(ServiceWriteLog {
                k,
                t,
                path: w.path,
                value,
                item,
            });
        }
        for (path, value) in self.tapped.borrow_mut().drain(..) {
            if self.seeded.get(&path) == Some(&value) {
                self.seeded.remove(&path);
                continue;
            }
            self.log.writes.push(WriteLog { k, t, path, value });
        }
        // A seed that changed nothing told the tap of nothing.
        self.seeded.clear();
        for (m, queued) in fetches {
            self.log.fetches.push((k, m, queued));
        }
    }

    /// (report) The wall time at virtual time `t`.
    fn wall(&self, t: Duration) -> SystemTime {
        self.wall0 + t
    }

    /// (lockmon) Tells the instance action `action`'s run: running since
    /// `oldest`, or not running.
    fn set_run(&self, action: &str, oldest: Option<SystemTime>) {
        let started = oldest.map(|t| chrono::DateTime::<chrono::Local>::from(t).naive_local());
        self.inst.set_action_run(action, oldest.is_some(), started);
    }

    /// (report) A `[seed]` entry: a `$token` overlay or a service field
    /// now (as a boot value), a cell once it is mounted, (history) or a
    /// kept series' history before the run (`history:<key>`).
    fn seed(&mut self, path: &str, j: serde_json::Value) -> Result<(), String> {
        if let Some(key) = path.strip_prefix("history:") {
            let points = history_points(&j)?;
            // A series this config does not keep is not seeded: the
            // planner names what it seeds from the same config.
            let _ = self.inst.seed_history(key, &points);
            return Ok(());
        }
        if let Some(token) = path.strip_prefix('$') {
            let text = match &j {
                serde_json::Value::String(s) => s.clone(),
                o => o.to_string(),
            };
            return self
                .inst
                .set_token_overlay(token, &text)
                .map_err(|e| e.to_string());
        }
        let first = path.split('.').next().unwrap_or(path);
        if self.is_service(first) {
            let ty = self.service_path_ty(path)?;
            let prog = &self.inst.vm().prog;
            let v = strand_compiler::vm::json::from_json(&prog.types, &ty, &j)?;
            return self
                .host
                .set_initial(&self.rt, path, v)
                .map_err(|e| e.to_string());
        }
        self.seeds.push((path.to_string(), j));
        Ok(())
    }

    /// (report) Seeds the `[seed]` cells mounted now (`Instance::seed_cell`:
    /// readers update, no `on change` fires); whether any was.
    fn seed_cells(&mut self) -> Result<bool, Error> {
        if self.seeds.is_empty() {
            return Ok(false);
        }
        let mut any = false;
        let mut left = Vec::new();
        for (path, j) in std::mem::take(&mut self.seeds) {
            // (replayfix) A cell the program declares but has not mounted
            // (a component in a popup not yet open) waits for its mount.
            let Some(ty) = self
                .inst
                .cell_type(&path)
                .filter(|_| self.inst.cell_mounted(&path))
            else {
                left.push((path, j));
                continue;
            };
            let prog = &self.inst.vm().prog;
            let v = strand_compiler::vm::json::from_json(&prog.types, &ty, &j)
                .map_err(|m| Error::Config(format!("[seed] `{path}`: {m}")))?;
            if self.opts.log {
                let mut text = String::new();
                let _ = strand_compiler::vm::json::to_json(&prog.types, &v, &mut text, &mut |t| {
                    prog.is_text_literal(t)
                });
                self.seeded.insert(path.clone(), text);
            }
            self.inst
                .seed_cell(&path, v)
                .map_err(|e| Error::Config(format!("[seed] `{path}`: {e}")))?;
            any = true;
        }
        self.seeds = left;
        Ok(any)
    }

    /// (report) `name` is a service: the schema's or one the config
    /// declares.
    fn is_service(&self, name: &str) -> bool {
        self.schema.service(name).is_some()
            || self
                .inst
                .vm()
                .prog
                .services
                .values()
                .any(|s| s.name == name)
    }

    /// The record of service `svc`: the schema's, else (report) the one a
    /// service the config declares has.
    fn service_record(&self, svc: &str) -> Option<strand_compiler::ty::RecordId> {
        self.schema.service(svc).or_else(|| {
            self.inst
                .vm()
                .prog
                .services
                .values()
                .find(|s| s.name == svc)
                .map(|s| s.record)
        })
    }

    /// The type of the service field at `path` (`service.field[.more]`).
    fn service_path_ty(&self, path: &str) -> Result<Ty, String> {
        let parts: Vec<&str> = path.split('.').collect();
        let [svc, field, rest @ ..] = parts.as_slice() else {
            return Err("a service field is `service.field`".into());
        };
        let prog = &self.inst.vm().prog;
        let rid = self
            .service_record(svc)
            .ok_or_else(|| format!("no service `{svc}`"))?;
        let ty = prog
            .types
            .record(rid)
            .fields
            .iter()
            .find(|f| f.name == *field)
            .map(|f| f.ty.clone())
            .ok_or_else(|| format!("`{svc}` has no field `{field}`"))?;
        field_ty(prog, ty, rest).ok_or_else(|| format!("no field at `{path}`"))
    }

    /// (report) `answer <method> …`: the method's answer read by its
    /// return type, queued ahead of the mock's own.
    fn answer_step(&self, method: &str, answer: &Result<String, String>) -> Result<(), String> {
        // (flex review) A result action's next call.
        if let Some(action) = method.strip_prefix("action ") {
            let prog = &self.inst.vm().prog;
            if !prog
                .execs
                .values()
                .any(|a| a.name == action && a.ret.is_some())
            {
                return Err(format!("no action `{action}` with a result (`-> T`)"));
            }
            let answer = match answer {
                Ok(j) => Ok(serde_json::from_str(j).map_err(|e| e.to_string())?),
                Err(e) => Err(e.clone()),
            };
            self.exec_answers
                .borrow_mut()
                .entry(action.to_string())
                .or_default()
                .push_back(answer);
            return Ok(());
        }
        let (svc, name) = method
            .split_once('.')
            .ok_or_else(|| format!("`{method}` is not `service.method`"))?;
        let rid = self
            .service_record(svc)
            .ok_or_else(|| format!("no service `{svc}`"))?;
        let prog = &self.inst.vm().prog;
        let m = prog
            .types
            .record(rid)
            .methods
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| format!("`{svc}` has no method `{name}`"))?;
        let json = match answer {
            Err(e) => {
                self.host.answer(method, Err(e.clone()));
                return Ok(());
            }
            Ok(j) => serde_json::from_str::<serde_json::Value>(j).map_err(|e| e.to_string())?,
        };
        let mut why = String::new();
        for sig in &m.sigs {
            let ret = match &sig.ret {
                Ty::Async(inner) => inner.as_ref(),
                t => t,
            };
            match strand_compiler::vm::json::from_json(&prog.types, ret, &json) {
                Ok(v) => {
                    self.host.answer(method, Ok(v));
                    return Ok(());
                }
                Err(e) => why = e,
            }
        }
        Err(format!("`{json}` is no answer of `{method}`: {why}"))
    }

    /// A value for `ty` from a step's text: JSON for a text that starts
    /// as an object or a list does (report: a record, a list, a held
    /// text), else as `strand set` reads it, else JSON.
    fn read_value(&self, ty: &Ty, text: &str) -> Result<Value, String> {
        let prog = &self.inst.vm().prog;
        let text = text.trim();
        let json = || -> Result<Value, String> {
            let j: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
            strand_compiler::vm::json::from_json(&prog.types, ty, &j)
        };
        if text.starts_with(['{', '['])
            && let Ok(v) = json()
        {
            return Ok(v);
        }
        if let Some(v) = parse_text(&prog.types, ty, text) {
            return Ok(v);
        }
        json().map_err(|_| format!("`{text}` is not a {}", prog.types.show(ty)))
    }

    /// (report) The keyboard's events now: a leave and an enter when the
    /// surface that should have it (the newest shown one that took it)
    /// is not the one that has it.
    fn kb_events(&mut self) -> Vec<InputEvent> {
        let surfaces = &self.surfaces;
        self.kb_stack.retain(|s| surfaces.get(*s).is_some());
        let want = self.kb_stack.last().copied();
        if want == self.kb {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(old) = self.kb.filter(|s| self.surfaces.get(*s).is_some()) {
            out.push(InputEvent::KeyboardLeave { surface: old });
        }
        if let Some(new) = want {
            out.push(InputEvent::KeyboardEnter { surface: new });
        }
        self.kb = want;
        out
    }

    /// (report) Surface `s` takes the keyboard.
    fn kb_take(&mut self, s: SurfaceId) {
        self.kb_stack.retain(|x| *x != s);
        self.kb_stack.push(s);
    }

    /// (report) The keyboard's events now, sent.
    fn kb_sync(&mut self) -> Result<(), Error> {
        let ev = self.kb_events();
        self.send(ev)
    }

    /// (report) A key on the surface with the keyboard.
    fn key_event(
        &self,
        name: &str,
        text: &str,
        m: KeyMods,
        pressed: bool,
        t: Duration,
    ) -> Option<InputEvent> {
        Some(InputEvent::Key {
            surface: self.kb?,
            key: KeyInput {
                name: name.to_string(),
                text: text.to_string(),
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
                repeat: false,
                modifiers: Modifiers {
                    ctrl: m.ctrl,
                    alt: m.alt,
                    shift: m.shift,
                    logo: m.logo,
                },
                time: ms(t),
                latin: None,
            },
        })
    }

    /// (report) `n` placeholder `x`s typed, each a press and a release.
    fn typed(&self, n: u32, t: Duration) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for _ in 0..n {
            for pressed in [true, false] {
                out.extend(self.key_event("x", "x", KeyMods::default(), pressed, t));
            }
        }
        out
    }

    /// `system.reduced_motion` set, as the portal sets it in the shell.
    fn set_reduced_motion(&mut self, on: bool) -> Result<(), Error> {
        self.host
            .set(&self.rt, "system.reduced_motion", Value::Bool(on))
            .map_err(|e| Error::Run(format!("system.reduced_motion: {e}")))
    }

    /// `system.reduced_motion` as logic has it now, into `diff` when
    /// render does not have it yet (crates/strand/src/run/logic.rs, the
    /// step's `reduced_motion`).
    fn carry_reduced(&mut self, diff: &mut SceneDiff) {
        let now = self
            .host
            .get(&self.rt, "system.reduced_motion")
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if now != self.reduced {
            diff.reduced_motion = Some(now);
            self.reduced = now;
            if let Some(i) = self.open {
                self.trace.segments[i].reduced_motion |= now;
            }
        }
    }

    /// `screens.all` and `screens.focused`: (fidelity) every screen, in
    /// the scenario's order, the first focused, as
    /// crates/strand/src/run/mod.rs `set_screens` makes them.
    fn set_screens(&mut self) -> Result<(), Error> {
        let records: Vec<Value> = self
            .sc
            .screens
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let s: Screen = self.surfaces.screen_of(i);
                let who = &d.identity;
                self.host.record(
                    "Screen",
                    &[
                        ("id", Value::text(who.id.as_str())),
                        ("name", Value::text(who.name.as_str())),
                        ("make", Value::text(who.make.as_str())),
                        ("model", Value::text(who.model.as_str())),
                        ("description", Value::text(who.description.as_str())),
                        ("scale", Value::float(s.scale)),
                        ("width", Value::float(f64::from(s.width))),
                        ("height", Value::float(f64::from(s.height))),
                        ("focused", Value::Bool(i == 0)),
                    ],
                )
            })
            .collect();
        let focused = records.first().cloned().unwrap_or(Value::Null);
        let run = |e: strand_core::Error| Error::Run(format!("screens: {e}"));
        self.host
            .set(&self.rt, "screens.all", Value::list(records))
            .map_err(run)?;
        self.host
            .set(&self.rt, "screens.focused", focused)
            .map_err(run)
    }

    /// A service field set from text, read by its schema type (report:
    /// or the type a service the config declares gives it; JSON for a
    /// record or a list).
    fn set_service(&self, path: &str, text: &str) -> Result<(), String> {
        let ty = self.service_path_ty(path)?;
        let v = self.read_value(&ty, text)?;
        self.host.set(&self.rt, path, v).map_err(|e| e.to_string())
    }

    /// `set <path> <text>`: a `$token` overlay, a service field, an
    /// exported state, else a file's top-level state by name.
    fn set(&self, path: &str, text: &str) -> Result<(), String> {
        if let Some(token) = path.strip_prefix('$') {
            return self
                .inst
                .set_token_overlay(token, text)
                .map_err(|e| e.to_string());
        }
        let first = path.split('.').next().unwrap_or(path);
        if self.is_service(first) {
            return self.set_service(path, text);
        }
        let exported = self.inst.set_text(path, text);
        let Err(e) = exported else { return Ok(()) };
        let (d, module, rest) = self.state_def(path).ok_or_else(|| e.to_string())?;
        let prog = &self.inst.vm().prog;
        let fields: Vec<&str> = rest.split('.').skip(1).collect();
        let ty = field_ty(prog, prog.defs[d].ty.clone(), &fields)
            .ok_or_else(|| format!("no field at `{path}`"))?;
        let v = self.read_value(&ty, text)?;
        self.inst
            .set_value(&module, &rest, v)
            .map_err(|e| e.to_string())
    }

    /// A top-level `state` named by `path` (`dnd`, `shell.dnd`,
    /// `shell.prefs.compact`): its def, module and the path from its
    /// name on.
    fn state_def(&self, path: &str) -> Option<(usize, String, String)> {
        let prog = &self.inst.vm().prog;
        let parts: Vec<&str> = path.split('.').collect();
        let top = |module: Option<&str>, name: &str| -> Vec<usize> {
            prog.defs
                .iter()
                .enumerate()
                .filter(|(_, d)| {
                    d.owner.is_none()
                        && d.kind == strand_compiler::hir::DefKind::State
                        && d.name == name
                        && module.is_none_or(|m| d.module == m)
                })
                .map(|(i, _)| i)
                .collect()
        };
        if parts.len() >= 2
            && let [d] = top(Some(parts[0]), parts[1])[..]
        {
            return Some((d, parts[0].to_string(), parts[1..].join(".")));
        }
        match top(None, parts[0])[..] {
            [d] => Some((d, prog.defs[d].module.clone(), path.to_string())),
            _ => None,
        }
    }

    /// The value at `path`: an exported value, or a top-level `state`
    /// and a path into it.
    fn value_at(&self, path: &str) -> Result<Value, String> {
        match self.inst.get(path) {
            Ok(v) => Ok(v),
            Err(e) => {
                let (_, module, rest) = self.state_def(path).ok_or_else(|| e.to_string())?;
                let mut it = rest.split('.');
                let name = it.next().unwrap_or("");
                let mut v = self
                    .inst
                    .value_of(&module, name)
                    .map_err(|e| e.to_string())?;
                for f in it {
                    v = v
                        .field(&self.inst.vm().prog.types, f)
                        .cloned()
                        .ok_or_else(|| format!("no field `{f}`"))?;
                }
                Ok(v)
            }
        }
    }

    fn toggle(&self, path: &str) -> Result<(), String> {
        match self.value_at(path)? {
            Value::Bool(b) => self.set(path, if b { "false" } else { "true" }),
            _ => Err(format!("`{path}` is not a bool")),
        }
    }

    fn emit(&self, path: &str, args: &[String]) -> Result<(), String> {
        let (svc, event) = path
            .split_once('.')
            .ok_or_else(|| format!("`{path}` is not `service.event`"))?;
        let rid = self
            .service_record(svc)
            .ok_or_else(|| format!("no service `{svc}`"))?;
        let prog = &self.inst.vm().prog;
        let ev = prog
            .types
            .record(rid)
            .events
            .iter()
            .find(|e| e.name == event)
            .ok_or_else(|| format!("`{svc}` has no event `{event}`"))?;
        if ev.params.len() != args.len() {
            return Err(format!(
                "`{path}` takes {} arguments, not {}",
                ev.params.len(),
                args.len()
            ));
        }
        let mut values = Vec::new();
        for (p, a) in ev.params.iter().zip(args) {
            values.push(self.read_value(&p.ty, a)?);
        }
        self.host
            .emit(&self.rt, path, values)
            .map_err(|e| e.to_string())
    }

    /// (png-aspect) The size the header of the image file `src` gives
    /// (a decode's `source`, a path the compiler made absolute or a
    /// `file://` URL), read once per path. (integration-r7) Each decode
    /// drawn names its own source: an image swap draws the old file's
    /// decode under the new one, and the node's `source` is the new
    /// one's.
    fn file_size(&mut self, src: &str) -> Option<[u32; 2]> {
        let src = src.trim();
        let path = src.strip_prefix("file://").unwrap_or(src);
        if !path.starts_with('/') {
            return None;
        }
        *self
            .file_sizes
            .entry(path.to_string())
            .or_insert_with(|| crate::filesize::of(std::path::Path::new(path)))
    }

    fn err(&self, line: u32, msg: String) -> Error {
        Error::Scenario {
            file: self.sc.file.display().to_string(),
            line,
            msg,
        }
    }

    /// A step the shell refused, at `line`: its reason, and the config's
    /// warnings, often the cause (a bare name that reads a builtin where
    /// a variant was meant).
    fn refused(&self, line: u32, msg: String) -> Error {
        if self.warnings.is_empty() {
            return self.err(line, msg);
        }
        let w = self.warnings.replace('\n', "\n  ");
        self.err(line, format!("{msg}\n  the config's warnings:\n  {w}"))
    }

    /// The router's instant for presentation time `t`.
    fn instant(&self, t: Duration) -> Instant {
        self.i0 + t.saturating_sub(T0)
    }

    /// Applies a logic update: its errors noted, its diff (with
    /// `system.reduced_motion` when it changed) through the host loop
    /// unless it is empty, as the shell's logic thread sends only diffs
    /// with something in them.
    fn update(&mut self, u: Update) -> Result<(), Error> {
        self.keep_writes();
        for e in &u.errors {
            self.notes.push(format!("runtime error: {}", e.message()));
        }
        let mut diff = u.diff;
        if let Some(seq) = self.facts_seq.take() {
            diff.layout_seen = Some(seq);
        }
        self.carry_reduced(&mut diff);
        // (report) `[seed]` cells this update mounted are seeded before
        // they are drawn: logic answers at the loop's next turn.
        if !diff.ops.is_empty() && self.seed_cells()? {
            self.logic_owed = true;
        }
        if diff.is_empty() {
            return Ok(());
        }
        self.apply(diff)
    }

    /// Logic's diff through the host loop as the shell's main thread
    /// takes it (`host_loop::apply`: the router observes it, render
    /// applies it, routing settles, the input wake follows, surfaces
    /// sync), then the loop's turn.
    fn apply(&mut self, mut diff: SceneDiff) -> Result<(), Error> {
        self.driven |= !diff.ops.is_empty();
        // (window) What its handlers asked of windows, once the windows
        // it opens are there (`demo::apply`).
        let asks = std::mem::take(&mut diff.window_asks);
        // (integration-r7) The rows a list's window mounts, as logic
        // declares them, noted by key when first drawn
        // (`RunTrace::windowed`).
        let k = self.clock.k();
        for op in &diff.ops {
            if let strand_scene::SceneOp::Create {
                id, window, kind, ..
            } = op
            {
                if *window {
                    self.window_mounts.insert(*id, k);
                } else {
                    self.window_mounts.remove(id);
                }
                // (integration-r7) A surface logic opens (or opens
                // again), noted by key when first drawn
                // (`RunTrace::opened`).
                use strand_scene::NodeKind as K;
                if matches!(
                    kind,
                    K::Bar | K::Panel | K::Osd | K::Popup | K::Lock | K::Window
                ) {
                    self.surface_opens.insert(*id, k);
                } else {
                    self.surface_opens.remove(id);
                }
            }
            // (integration-r7) A surface opened by its `open:` prop
            // (spec.md: `open: open` on a panel or popup).
            if let strand_scene::SceneOp::SetProp {
                id,
                prop: Prop::Open,
                value,
                ..
            } = op
                && self.r.tree().get(*id).is_some_and(|n| n.kind.is_surface())
            {
                if matches!(value, PropValue::Bool(true)) {
                    self.surface_opens.insert(*id, k);
                } else {
                    self.surface_opens.remove(id);
                }
            }
        }
        // (integration-r7 part 2) A surface logic closes: its root (or a
        // node above it) removed, or its `open` set false
        // (`RunTrace::closed`), noted by key while the tree still holds
        // it.
        let mut closes: Vec<NodeId> = Vec::new();
        // (evalfix) The root of a subtree logic removes, not a surface's
        // and not a list window's unmount (`RunTrace::removed`).
        let mut removes: Vec<NodeId> = Vec::new();
        for op in &diff.ops {
            match op {
                strand_scene::SceneOp::Remove { id, window } => {
                    let tree = self.r.tree();
                    closes.extend(tree.surface_nodes().filter(|s| tree.is_ancestor(*id, *s)));
                    if !*window && tree.get(*id).is_some_and(|n| !n.kind.is_surface()) {
                        removes.push(*id);
                    }
                }
                strand_scene::SceneOp::SetProp {
                    id,
                    prop: Prop::Open,
                    value: PropValue::Bool(false),
                    ..
                } if self.r.tree().get(*id).is_some_and(|n| n.kind.is_surface()) => {
                    closes.push(*id);
                }
                _ => {}
            }
        }
        for id in closes {
            let key = self
                .labels
                .key(id, &self.inst, self.r.tree(), &self.sources);
            let ks = self.trace.closed.entry(key).or_default();
            if ks.last() != Some(&k) {
                ks.push(k);
            }
        }
        for id in removes {
            let key = self
                .labels
                .key(id, &self.inst, self.r.tree(), &self.sources);
            let ks = self.trace.removed.entry(key).or_default();
            if ks.last() != Some(&k) {
                ks.push(k);
            }
        }
        host_loop::apply(self, diff)?;
        self.window_asks(asks)?;
        self.turn()
    }

    /// (window) What handlers asked of windows (`w.maximize()` …),
    /// answered as a compositor that offers every action: `close` is its
    /// close (a click away for the router), `maximize` and `fullscreen`
    /// toggle that state in its configure, `minimize` tells the window
    /// nothing (xdg-shell says nothing of it), and an interactive `move`
    /// or `resize` needs a compositor's own pointer grab, so it is a note.
    /// A maximize or fullscreen asked of a window its spec opens but not
    /// shown yet is kept for its first configure, as the surface manager
    /// asks it at the window's creation.
    fn window_asks(&mut self, asks: Vec<(NodeId, strand_scene::WindowAsk)>) -> Result<(), Error> {
        use strand_scene::WindowAsk;
        for (node, ask) in asks {
            let Some(id) = self
                .surfaces
                .shown
                .iter()
                .find(|s| s.node == node && s.kind == strand_scene::NodeKind::Window)
                .map(|s| s.id)
            else {
                if self.surfaces.opening_window(node)
                    && matches!(ask, WindowAsk::Maximize | WindowAsk::Fullscreen)
                {
                    let mut st = self.surfaces.window_state(node);
                    match ask {
                        WindowAsk::Maximize => st.maximized = !st.maximized,
                        _ => st.fullscreen = !st.fullscreen,
                    }
                    let changes = self.surfaces.set_window(node, st)?;
                    self.surface_changes(changes)?;
                }
                continue;
            };
            let mut st = self.surfaces.window_state(node);
            match ask {
                WindowAsk::Close => {
                    self.send(vec![InputEvent::ClickAway { surface: id }])?;
                    continue;
                }
                // The sandbox's own size for the state, or the window's
                // own when it ends.
                WindowAsk::Maximize => {
                    st.maximized = !st.maximized;
                    (st.size, st.sized) = ([None, None], [false, false]);
                }
                WindowAsk::Fullscreen => {
                    st.fullscreen = !st.fullscreen;
                    (st.size, st.sized) = ([None, None], [false, false]);
                }
                WindowAsk::Minimize => continue,
                WindowAsk::Move | WindowAsk::Resize(_) => {
                    let note = "a window's interactive move or resize (`w.move()`, \
                                `w.resize(edge)`) is the compositor's pointer grab, not \
                                modelled by the sandbox"
                        .to_string();
                    if !self.notes.contains(&note) {
                        self.notes.push(note);
                    }
                    continue;
                }
            }
            let changes = self.surfaces.set_window(node, st)?;
            self.surface_changes(changes)?;
        }
        Ok(())
    }

    /// (window) A `window` step: the compositor configures the window
    /// `on` names (the window itself, or one in it) as `configure` says,
    /// or (`None`) closes it. A configure of a window not shown yet is
    /// kept for when it shows (a replay's, the live map a tick ahead of
    /// the sandbox's); a close of one not shown closes nothing.
    fn window_step(
        &mut self,
        on: &Selector,
        configure: Option<&crate::surfaces::WindowState>,
        line: u32,
    ) -> Result<(), Error> {
        let nodes = self.resolve(on, line)?;
        let tree = self.r.tree();
        let is_window = |n: NodeId| {
            tree.get(n)
                .is_some_and(|x| x.kind == strand_scene::NodeKind::Window)
        };
        let node = nodes.iter().find_map(|(n, _)| {
            let root = tree.root_of(*n).unwrap_or(*n);
            [*n, root].into_iter().find(|c| is_window(*c))
        });
        let Some(node) = node else {
            return Err(self.err(line, format!("`{on}` is in no `window`")));
        };
        match configure {
            Some(st) => {
                let changes = self.surfaces.set_window(node, *st)?;
                self.surface_changes(changes)?;
            }
            None => {
                let shown = self
                    .surfaces
                    .shown
                    .iter()
                    .find(|s| s.node == node)
                    .map(|s| s.id);
                if let Some(id) = shown {
                    // (oi-window) The surface leg's compositor closes it too.
                    self.surfaces.tee_close(id);
                    self.send(vec![InputEvent::ClickAway { surface: id }])?;
                }
            }
        }
        self.turn()
    }

    /// (window) The compositor's configure of window `id` told, as the
    /// shell's host tells `SurfaceHost::window_state`: render lays it out
    /// at the size given on the axes the compositor sized, within its
    /// bounds, and logic hears its states on its root; the compositor's
    /// focus moves the keyboard with it.
    fn tell_window(&mut self, id: SurfaceId) -> Result<(), Error> {
        let Some((node, st)) = self.surfaces.told(id) else {
            return Ok(());
        };
        self.r.set_window_sized(id, st.sized);
        if let Some((w, h)) = st.bounds {
            self.r
                .set_surface_bounds(id, Some(LogicalSize::new(w as f32, h as f32)));
        }
        for (flag, on) in [
            (NodeFlag::Focused, st.activated),
            (NodeFlag::Maximized, st.maximized),
            (NodeFlag::Fullscreen, st.fullscreen),
            (NodeFlag::Tiled, st.tiled),
            (NodeFlag::Decorated, st.decorated),
        ] {
            self.inst.set_flag(node, flag, on);
        }
        self.logic_owed = true;
        if st.activated {
            self.kb_take(id);
        } else {
            self.kb_stack.retain(|s| *s != id);
        }
        Ok(())
    }

    /// The sandbox compositor's surface changes run through the host
    /// loop, as the surface manager's callbacks run them: attached,
    /// (window) told, configured where it lies, placed, detached; then
    /// noted in the trace.
    fn surface_changes(&mut self, changes: Vec<Change>) -> Result<(), Error> {
        let screen = self.surfaces.screen;
        let bounds = Some(LogicalSize::new(screen.width as f32, screen.height as f32));
        for c in &changes {
            self.surface_change(*c, bounds)?;
        }
        self.changed(changes)
    }

    fn surface_change(&mut self, c: Change, bounds: Option<LogicalSize>) -> Result<(), Error> {
        match c {
            Change::Attached(id, node) => host_loop::attached(self, id, node, bounds)?,
            Change::Window(id) => self.tell_window(id)?,
            Change::Configured(id, size, scale) => {
                let origin = self.surfaces.origin(id);
                self.r.set_surface_origin(id, origin);
                host_loop::configured(self, id, size, scale)?;
            }
            Change::Placed(id) => {
                let origin = self.surfaces.origin(id);
                self.r.set_surface_origin(id, origin);
            }
            Change::Detached(id) => host_loop::detached(self, id)?,
        }
        Ok(())
    }

    /// The loop's turn after a reaction (eval-host-loop): a wake owed
    /// runs `host_loop::woke` (so does text shaped inline meanwhile,
    /// which a text worker would wake the loop for), then logic answers
    /// what routing told it, each answer applied as a reaction of its
    /// own, until nothing is owed. The shell's loop runs these as
    /// callbacks of their own after the one that owed them; nothing runs
    /// inside a reaction.
    fn turn(&mut self) -> Result<(), Error> {
        if self.turning {
            return Ok(());
        }
        self.turning = true;
        let out = self.turn_owed();
        self.turning = false;
        out
    }

    fn turn_owed(&mut self) -> Result<(), Error> {
        for _ in 0..TURN_ROUNDS {
            if std::mem::take(&mut self.wake_owed) || self.r.take_inline_shaped() > 0 {
                host_loop::woke(self)?;
                continue;
            }
            if std::mem::take(&mut self.logic_owed) {
                let u = self.inst.flush();
                self.update(u)?;
                continue;
            }
            return Ok(());
        }
        Err(Error::Run(format!(
            "the loop never goes quiet: a wake or logic's answer is owed after each of {TURN_ROUNDS} turns"
        )))
    }

    /// The loop woken between frames (crates/strand/src/demo/mod.rs
    /// `text_ready`, `host_loop::woke`), and its turn.
    fn wake(&mut self) -> Result<(), Error> {
        self.wake_owed = true;
        self.turn()
    }

    /// Surface changes noted in the trace, and handed to the GPU leg.
    fn changed(&mut self, changes: Vec<Change>) -> Result<(), Error> {
        let k = self.clock.k();
        for c in changes {
            match c {
                Change::Configured(id, size, scale) => {
                    if self.on_gpu
                        && let Some(dev) = self.gpu.as_mut()
                    {
                        if self.gpu_attached.insert(id) {
                            dev.attach(&mut self.r, id, size, scale);
                        } else {
                            dev.resize(id, size, scale);
                        }
                    }
                }
                Change::Attached(id, node) => {
                    // (report) An `exclusive` surface takes the keyboard
                    // as it maps (sent at the loop's next input or tick).
                    if self.surfaces.keyboard(id) == Keyboard::Exclusive {
                        self.kb_take(id);
                    }
                    let root = self
                        .labels
                        .key(node, &self.inst, self.r.tree(), &self.sources);
                    let kind = self.r.tree().get(node).map_or("?", |n| n.kind.name());
                    let name = self
                        .r
                        .tree()
                        .get(node)
                        .and_then(|n| match n.get(Prop::Name) {
                            Some(PropValue::Text(t)) | Some(PropValue::Keyword(t)) => {
                                Some(t.clone())
                            }
                            _ => None,
                        });
                    let label = match name {
                        Some(n) => format!("{kind} {n}"),
                        None => self.trace_label(&root),
                    };
                    let content = crate::labels::content_sized(self.r.tree(), node);
                    self.trace.surfaces.push(SurfaceInfo {
                        id: id.0,
                        label,
                        kind: kind.to_string(),
                        root,
                        attached: k,
                        detached: None,
                        content,
                        places: Vec::new(),
                        output: self
                            .surfaces
                            .several()
                            .then(|| {
                                self.surfaces
                                    .get(id)
                                    .map(|s| self.surfaces.output_name(s.output).to_string())
                            })
                            .flatten(),
                    });
                }
                Change::Placed(_) | Change::Window(_) => {}
                Change::Detached(id) => {
                    if self.gpu_attached.remove(&id)
                        && let Some(dev) = self.gpu.as_mut()
                    {
                        dev.release(&mut self.r, id, true);
                    }
                    if let Some(s) = self
                        .trace
                        .surfaces
                        .iter_mut()
                        .rev()
                        .find(|s| s.id == id.0 && s.detached.is_none())
                    {
                        s.detached = Some(k);
                    }
                }
            }
        }
        // (eval-rules-4) Each shown surface's declared placement, noted
        // when it changes (`checks::placement`).
        let ids: Vec<SurfaceId> = self.surfaces.shown.iter().map(|s| s.id).collect();
        for id in ids {
            let Some((node, mut place)) = self.surfaces.declared(id, k) else {
                continue;
            };
            let parent = self
                .r
                .tree()
                .get(node)
                .filter(|n| n.kind == strand_scene::NodeKind::Popup)
                .and_then(|n| n.parent);
            if let Some(p) = parent {
                place.from = Some(self.labels.key(p, &self.inst, self.r.tree(), &self.sources));
            }
            if let Some(info) = self
                .trace
                .surfaces
                .iter_mut()
                .rev()
                .find(|s| s.id == id.0 && s.detached.is_none())
                && info.places.last().is_none_or(|l| {
                    crate::trace::PlaceRec {
                        k: l.k,
                        ..place.clone()
                    } != *l
                })
            {
                info.places.push(place);
            }
        }
        Ok(())
    }

    fn trace_label(&self, key: &str) -> String {
        self.labels
            .metas
            .get(key)
            .map_or_else(|| key.to_string(), |m| m.label.clone())
    }

    /// Logic to `t`: its timers due by then in order, then `t` itself.
    fn logic(&mut self, t: Duration) -> Result<(), Error> {
        let mut rounds = 0;
        while let Some(d) = self.inst.next_deadline()
            && d <= t
        {
            rounds += 1;
            if rounds > TIMER_ROUNDS {
                return Err(Error::Run(format!(
                    "logic's timers never get past {d:?} (a timer of zero?)"
                )));
            }
            set_virtual_boot(d);
            let (u, _) = self.inst.step(d, self.wall(d));
            self.update(u)?;
        }
        set_virtual_boot(t);
        let (u, _) = self.inst.step(t, self.wall(t));
        self.update(u)
    }

    /// Input as the surface manager hands it to the host, each event a
    /// reaction (`host_loop::input`: routed, its intents to logic, the
    /// pointer to lean and tilt, the renderer's input wake; surface
    /// changes it leaves, or a surface it wakes from idle, wake the
    /// loop), then the loop's turn.
    fn send(&mut self, events: Vec<strand_scene::InputEvent>) -> Result<(), Error> {
        self.driven |= !events.is_empty();
        let mut idle = Vec::new();
        for e in events {
            // (report) The compositor's focus: a press on a surface that
            // takes the keyboard (`on_demand`, `exclusive`) gives it it.
            if let InputEvent::PointerButton {
                surface,
                state: ButtonState::Pressed,
                ..
            } = &e
                && self.surfaces.keyboard(*surface) != Keyboard::None
                && self.kb != Some(*surface)
            {
                self.kb_take(*surface);
            }
            // (report) A press and the node it hits, kept for a replay.
            if self.opts.log
                && let InputEvent::PointerButton {
                    surface,
                    position,
                    state: ButtonState::Pressed,
                    ..
                } = &e
            {
                let hit = self.r.hit(*surface, *position).first().copied();
                let hit = hit.and_then(|n| self.node_ref(n));
                let k = self.clock.k();
                self.log.presses.push(PressLog {
                    k,
                    t: self.clock.now(),
                    hit,
                });
            }
            if !matches!(
                e,
                InputEvent::KeyboardEnter { .. } | InputEvent::KeyboardLeave { .. }
            ) {
                for f in self.kb_events() {
                    host_loop::input(self, &f, &mut idle)?;
                    self.turn()?;
                }
            }
            host_loop::input(self, &e, &mut idle)?;
            self.turn()?;
        }
        Ok(())
    }

    /// The surface a step's `on` names, else the pointer's, else the
    /// first shown; (fidelity) of those, one on the focused output (a
    /// `screen` step's) first: a `bar` named once is on every screen.
    fn surface_for(&mut self, on: &Option<Selector>, line: u32) -> Result<SurfaceId, Error> {
        let focused = self.surfaces.focused;
        self.surface_on(on, focused, line)
    }

    /// (fidelity) The index of screen `name` among the outputs.
    fn screen_index(&self, name: &str, line: u32) -> Result<usize, Error> {
        match self.surfaces.outputs.iter().position(|o| o.name == name) {
            Some(i) => Ok(i),
            None => {
                let names: Vec<&str> = self
                    .surfaces
                    .outputs
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect();
                Err(self.err(
                    line,
                    format!("screen `{name}`: the screens are {}", names.join(", ")),
                ))
            }
        }
    }

    /// [`Harness::surface_for`] with output `focused` taken first.
    fn surface_on(
        &mut self,
        on: &Option<Selector>,
        focused: usize,
        line: u32,
    ) -> Result<SurfaceId, Error> {
        if let Some(sel) = on {
            let nodes = self.resolve(sel, line)?;
            let mut found: Option<SurfaceId> = None;
            for (n, _) in &nodes {
                let root = self.r.tree().root_of(*n).unwrap_or(*n);
                // The innermost surface: the node itself, or its root.
                for cand in [*n, root] {
                    if let Some(s) = self.surfaces.shown.iter().find(|s| s.node == cand) {
                        if s.output == focused {
                            return Ok(s.id);
                        }
                        found.get_or_insert(s.id);
                        break;
                    }
                }
            }
            return found.ok_or_else(|| self.err(line, format!("`{sel}` is on no shown surface")));
        }
        if let Some(s) = self
            .pointer
            .surface
            .filter(|s| self.surfaces.get(*s).is_some())
        {
            return Ok(s);
        }
        self.surfaces
            .shown
            .iter()
            .find(|s| s.output == focused)
            .or_else(|| self.surfaces.shown.first())
            .map(|s| s.id)
            .ok_or_else(|| self.err(line, "no surface is shown".into()))
    }

    /// The nodes `sel` names now, with their keys.
    fn resolve(&mut self, sel: &Selector, line: u32) -> Result<Vec<(NodeId, String)>, Error> {
        let found = self.matching(sel);
        if found.is_empty() {
            return Err(self.err(
                line,
                format!("`{sel}` matches no node mounted now or seen before"),
            ));
        }
        Ok(found)
    }

    /// The nodes `sel` names now: those mounted, else those seen before
    /// (`expect hidden` after a node left); none when neither matches.
    fn matching(&mut self, sel: &Selector) -> Vec<(NodeId, String)> {
        let views = self.labels.views(&self.inst, self.r.tree(), &self.sources);
        let roots = self.r.tree().roots().to_vec();
        let found: Vec<(NodeId, String)> = sel
            .resolve(&views, &roots)
            .into_iter()
            .filter_map(|n| {
                let k = views.iter().find(|v| v.node == n)?.key.clone();
                Some((n, k))
            })
            .collect();
        if !found.is_empty() {
            return found;
        }
        // A node no longer mounted (`expect hidden` after it left), by
        // what was seen of it.
        self.labels
            .seen_matching(sel)
            .into_iter()
            .map(|(n, k)| (n, k.to_string()))
            .collect()
    }

    /// (eval-coverage) Resolves the pending intends that match a node
    /// now: their records (in the open segment, or waiting for the next)
    /// take its keys.
    fn resolve_pending(&mut self) {
        if self.pending_intends.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_intends);
        for (sel, line) in pending {
            let keys: Vec<String> = self.matching(&sel).into_iter().map(|(_, k)| k).collect();
            if keys.is_empty() {
                self.pending_intends.push((sel, line));
                continue;
            }
            let open = self.open.map(|i| &mut self.trace.segments[i].intends);
            let rec = open
                .into_iter()
                .flatten()
                .chain(self.waiting_intends.iter_mut())
                .find(|r| r.line == line && r.keys.is_empty());
            if let Some(r) = rec {
                r.keys = keys;
            }
        }
    }

    /// (eval-coverage) The first pending intend of `lines` (a segment's
    /// intends; every one when `None`), as the error at its line: it
    /// matched no node before its segment ended.
    fn unmatched(&self, lines: Option<&[IntendRec]>) -> Option<Error> {
        let (sel, line) = self
            .pending_intends
            .iter()
            .filter(|(_, l)| lines.is_none_or(|ls| ls.iter().any(|r| r.line == *l)))
            .min_by_key(|(_, l)| *l)?;
        Some(self.err(
            *line,
            format!("`{sel}` matched no node before its segment ended"),
        ))
    }

    fn event(&mut self, step: &Step) {
        let k = self.clock.k();
        self.trace.events.push(EventRec {
            k,
            line: step.line,
            text: step.text.clone(),
            segment: self.open,
            ..EventRec::default()
        });
    }

    /// Opens a segment at a step that changes something, unless one is
    /// open.
    fn opens(&mut self, name: String, line: u32) {
        if self.open.is_some() {
            return;
        }
        self.trace.segments.push(Segment {
            name,
            line,
            s: self.last_k,
            e: self.last_k.unwrap_or(0),
            hz: self.clock.hz(),
            reduced_motion: self.reduced,
            intends: std::mem::take(&mut self.waiting_intends),
            ..Segment::default()
        });
        self.open = Some(self.trace.segments.len() - 1);
    }

    fn close(&mut self, settled: bool) -> Result<(), Error> {
        if let Some(i) = self.open.take() {
            if let Some(e) = self.unmatched(Some(&self.trace.segments[i].intends)) {
                return Err(e);
            }
            let seg = &mut self.trace.segments[i];
            seg.e = self.last_k.unwrap_or(0);
            seg.settled = settled;
            seg.hz = self.clock.hz();
        }
        Ok(())
    }

    fn step(&mut self, step: &Step) -> Result<(), Error> {
        if let Some(mut run) = self.begin(step)? {
            while !self.advance(&mut run)? {}
        }
        Ok(())
    }

    /// A step begun, as [`Harness::begin_step`]; (clip-data) an input
    /// step's event then says where it acted ([`Harness::input_facts`]).
    fn begin(&mut self, step: &Step) -> Result<Option<TimeRun>, Error> {
        let n = self.trace.events.len();
        let run = self.begin_step(step)?;
        if self.trace.events.len() > n {
            self.input_facts(n, &step.kind);
        }
        Ok(run)
    }

    /// (clip-data) What the trace's event `i` of an input step records
    /// besides its text, once the step acted (a time step's where it
    /// starts): a pointer step's surface, the pointer's place there and
    /// the innermost node under it with a key (the hit chain of the
    /// surface's last painted frame, as routing reads it); a key's,
    /// `type`'s or `paste`'s surface with the keyboard and the node
    /// focused there. Never a character typed: a step types `x`s.
    fn input_facts(&mut self, i: usize, kind: &StepKind) {
        let pointer = matches!(
            kind,
            StepKind::Pointer { .. }
                | StepKind::Click { .. }
                | StepKind::Press { .. }
                | StepKind::Release { .. }
                | StepKind::Scroll { .. }
                | StepKind::Swipe { .. }
                | StepKind::Leave
                | StepKind::Glide { .. }
                | StepKind::Drag { .. }
        );
        let keys = matches!(
            kind,
            StepKind::Key { .. }
                | StepKind::Type { .. }
                | StepKind::Paste { .. }
                | StepKind::Focus { .. }
                | StepKind::Unfocus
        );
        let (s, at, under, focus) = if pointer {
            match self.pointer.surface {
                Some(s) => {
                    let at = self.pointer.at;
                    let under = self
                        .r
                        .hit(s, at)
                        .into_iter()
                        .find_map(|n| self.labels.known(n).map(str::to_string));
                    (Some(s.0), Some([at.x, at.y]), under, None)
                }
                None => (None, None, None, None),
            }
        } else if keys {
            let focus = self
                .kb
                .and_then(|s| self.router.focused(s))
                .and_then(|n| self.labels.known(n).map(str::to_string));
            (self.kb.map(|s| s.0), None, None, focus)
        } else {
            return;
        };
        if let Some(e) = self.trace.events.get_mut(i) {
            e.s = s;
            e.at = at;
            e.under = under;
            e.focus = focus;
        }
    }

    /// A step begun: an instant step done, a time step's plan returned
    /// for [`Harness::advance`] to run tick by tick.
    fn begin_step(&mut self, step: &Step) -> Result<Option<TimeRun>, Error> {
        // Instant steps act at the tick about to run.
        let t = self.clock.now();
        self.router.set_now(Some(self.instant(t)));
        if step.kind.opens_segment() {
            self.opens(step.text.clone(), step.line);
        }
        let line = step.line;
        let refused = |h: &Self, m: String| h.refused(line, m);
        match &step.kind {
            StepKind::Set { path, value } => {
                self.event(step);
                self.set(path, value).map_err(|m| refused(self, m))?;
            }
            StepKind::Toggle { path } => {
                self.event(step);
                self.toggle(path).map_err(|m| refused(self, m))?;
            }
            StepKind::Emit { path, args } => {
                self.event(step);
                self.emit(path, args).map_err(|m| refused(self, m))?;
            }
            StepKind::Notify { .. } => {
                return Err(Error::Unsupported(format!(
                    "{}:{line}: `notify` (set `notifications.*` and `emit` instead)",
                    self.sc.file.display()
                )));
            }
            StepKind::Pointer { at, on } => {
                self.event(step);
                let s = self.surface_for(on, line)?;
                let ev = self.pointer.move_to(s, lp(*at), ms(t));
                self.send(ev)?;
            }
            StepKind::Click { at, on, button } => {
                self.event(step);
                let s = self.surface_for(on, line)?;
                let mut ev = self.pointer.move_to(s, lp(*at), ms(t));
                ev.extend(self.pointer.button(*button, ButtonState::Pressed, ms(t)));
                ev.extend(self.pointer.button(*button, ButtonState::Released, ms(t)));
                self.send(ev)?;
            }
            StepKind::Press { button } | StepKind::Release { button } => {
                self.event(step);
                let state = match step.kind {
                    StepKind::Press { .. } => ButtonState::Pressed,
                    _ => ButtonState::Released,
                };
                let ev = self
                    .pointer
                    .button(*button, state, ms(t))
                    .ok_or_else(|| self.err(line, "the pointer is on no surface yet".into()))?;
                self.send(vec![ev])?;
            }
            StepKind::Key { name, mods, phase } => {
                self.event(step);
                self.kb_sync()?;
                // A key that types one character types it, unless a
                // modifier other than shift is held.
                let text = match (name.chars().count(), mods.ctrl || mods.alt || mods.logo) {
                    (1, false) if mods.shift => name.to_uppercase(),
                    (1, false) => name.clone(),
                    _ => String::new(),
                };
                let mut ev = Vec::new();
                for pressed in [true, false] {
                    let wanted = match phase {
                        KeyPhase::Tap => true,
                        KeyPhase::Down => pressed,
                        KeyPhase::Up => !pressed,
                    };
                    if wanted {
                        ev.extend(self.key_event(name, &text, *mods, pressed, t));
                    }
                }
                if ev.is_empty() {
                    return Err(self.err(
                        line,
                        "no surface has the keyboard (a `focus` step gives it one)".into(),
                    ));
                }
                self.send(ev)?;
            }
            StepKind::Type { n, ms: None } => {
                self.event(step);
                self.kb_sync()?;
                if self.kb.is_none() {
                    return Err(self.err(
                        line,
                        "no surface has the keyboard (a `focus` step gives it one)".into(),
                    ));
                }
                let ev = self.typed(*n, t);
                self.send(ev)?;
            }
            StepKind::Type { ms: Some(_), .. } => {
                self.event(step);
                self.kb_sync()?;
                if self.kb.is_none() {
                    return Err(self.err(
                        line,
                        "no surface has the keyboard (a `focus` step gives it one)".into(),
                    ));
                }
                return self.start_time(step).map(Some);
            }
            StepKind::Paste { n } => {
                self.event(step);
                self.kb_sync()?;
                let Some(surface) = self.kb else {
                    return Err(self.err(
                        line,
                        "no surface has the keyboard (a `focus` step gives it one)".into(),
                    ));
                };
                // ctrl+v asks the clipboard; its text reaches only the
                // input that asked (the router's rule).
                let ctrl = KeyMods {
                    ctrl: true,
                    ..KeyMods::default()
                };
                let mut ev: Vec<InputEvent> = [true, false]
                    .into_iter()
                    .filter_map(|p| self.key_event("v", "", ctrl, p, t))
                    .collect();
                ev.push(InputEvent::Paste {
                    surface,
                    text: "x".repeat(*n as usize),
                });
                self.send(ev)?;
            }
            StepKind::Focus { on } => {
                self.event(step);
                let s = self.surface_for(&Some(on.clone()), line)?;
                self.kb_take(s);
                self.kb_sync()?;
            }
            StepKind::Unfocus => {
                self.event(step);
                self.kb_stack.clear();
                self.kb_sync()?;
            }
            StepKind::Window { on, configure } => {
                self.event(step);
                self.window_step(on, configure.as_ref(), line)?;
            }
            StepKind::Answer { method, answer } => {
                self.event(step);
                self.answer_step(method, answer)
                    .map_err(|m| refused(self, m))?;
            }
            StepKind::EndAction { action } => {
                self.event(step);
                if !self
                    .inst
                    .vm()
                    .prog
                    .execs
                    .values()
                    .any(|a| &a.name == action)
                {
                    return Err(refused(self, format!("no action `{action}`")));
                }
                // The command running ends; the re-run queued behind it
                // starts now, as the runner starts it.
                let wall = self.wall(t);
                let mut runs = self.sandbox_runs.borrow_mut();
                let run = runs.entry(action.clone()).or_default();
                if run.started.is_some() {
                    run.started = run.queued.then_some(wall);
                    run.queued = false;
                }
                let started = run.started;
                drop(runs);
                self.set_run(action, started);
                self.logic_owed = true;
            }
            StepKind::Leave => {
                self.event(step);
                // The pointer leaves the shell's surfaces (for a window,
                // the desktop, or no surface at all): nothing when it is
                // on none already.
                let ev: Vec<_> = self.pointer.leave().into_iter().collect();
                self.send(ev)?;
            }
            StepKind::Scroll { dy } => {
                self.event(step);
                let ev = self
                    .pointer
                    .scroll(*dy, ms(t))
                    .ok_or_else(|| self.err(line, "the pointer is on no surface yet".into()))?;
                self.send(vec![ev])?;
            }
            StepKind::Swipe { .. } => {
                self.event(step);
                if self.pointer.surface.is_none() {
                    return Err(self.err(line, "the pointer is on no surface yet".into()));
                }
                return self.start_time(step).map(Some);
            }
            StepKind::TextHold { filter } => {
                self.event(step);
                self.text.hold(filter.clone());
                self.r.set_text_hold_with(self.text.pick());
            }
            StepKind::TextLate { n, filter, via } => {
                self.event(step);
                self.text.late(*n, filter.clone(), *via);
                self.r.set_text_hold_with(self.text.pick());
            }
            StepKind::TextRelease { filter, via } => {
                self.event(step);
                self.text.release(filter.clone(), *via);
                // Held texts stay held until the release's moment; new
                // ones follow the rules left.
                if let Some(pick) = self.text.pick() {
                    self.r.set_text_hold_with(Some(pick));
                } else {
                    self.r.set_text_hold_with(Some(Box::new(|_, _| false)));
                }
            }
            StepKind::Drop { n, surface, output } => {
                self.event(step);
                match surface {
                    None => self.drop_all = *n,
                    Some(sel) => {
                        let on = match output {
                            None => self.surfaces.focused,
                            Some(name) => self.screen_index(name, line)?,
                        };
                        let s = self.surface_on(&Some(sel.clone()), on, line)?;
                        self.drop_one.insert(s, *n);
                    }
                }
            }
            StepKind::Jitter(f) => {
                self.event(step);
                self.clock.set_jitter(f.clone());
            }
            StepKind::ReduceMotion(on) => {
                self.event(step);
                // Logic's next step carries it to render.
                self.set_reduced_motion(*on || self.forced_reduced)?;
            }
            StepKind::Scale(f) => {
                self.event(step);
                if self.leg.is_some() && *f != 1.0 {
                    return Err(Error::Unsupported(format!(
                        "`scale {f}` with the surface leg (its fake compositor's output has scale 1)"
                    )));
                }
                self.surfaces.set_main_scale(*f);
                self.surfaces.scale()?;
                self.set_screens()?;
                // Each surface configured at the new scale, as the
                // surface manager's next frame of it does.
                let changes = self.surfaces.rescale()?;
                for c in &changes {
                    if let Change::Configured(id, size, scale) = *c {
                        host_loop::configured(self, id, size, scale)?;
                    }
                }
                self.changed(changes)?;
                self.turn()?;
            }
            StepKind::Hz(n) => {
                self.event(step);
                self.clock.set_hz(*n);
            }
            StepKind::Screen(name) => {
                self.event(step);
                self.surfaces.focused = self.screen_index(name, line)?;
            }
            StepKind::Backend(b) => {
                self.event(step);
                self.switch(*b, line)?;
            }
            StepKind::Mark(label) => {
                self.close(false)?;
                self.opens(label.clone(), line);
                self.event(step);
            }
            StepKind::Shot(_) | StepKind::Log(_) => self.event(step),
            StepKind::Intend { sel, what, axes } => {
                self.event(step);
                // A node the segment mounts later resolves when it shows
                // (`resolve_pending`).
                let keys: Vec<String> = self.matching(sel).into_iter().map(|(_, k)| k).collect();
                if keys.is_empty() {
                    self.pending_intends.push((sel.clone(), line));
                }
                let rec = IntendRec {
                    line,
                    what: match what {
                        Intention::Moves => "moves",
                        Intention::Enters => "enters",
                        Intention::Exits => "exits",
                        Intention::Changes => "changes",
                    }
                    .into(),
                    keys,
                    axes: axes.0,
                };
                match self.open {
                    Some(i) => self.trace.segments[i].intends.push(rec),
                    None => self.waiting_intends.push(rec),
                }
            }
            StepKind::Expect {
                what,
                sel,
                axis,
                from,
            } => {
                let keys = self
                    .resolve(sel, line)?
                    .into_iter()
                    .map(|(_, k)| k)
                    .collect();
                let segment = self.open.or(Some(self.trace.segments.len() - 1));
                self.trace.expects.push(ExpectRec {
                    k: self.last_k.unwrap_or(0),
                    line,
                    text: step.text.clone(),
                    what: match what {
                        ExpectKind::Still => "still",
                        ExpectKind::Visible => "visible",
                        ExpectKind::Hidden => "hidden",
                        ExpectKind::Moves => "moves",
                    }
                    .into(),
                    keys,
                    axis: axis.map(|a| a.name().to_string()),
                    from: *from,
                    segment,
                });
            }
            StepKind::Frames(_)
            | StepKind::Wait(_)
            | StepKind::Rest { .. }
            | StepKind::Glide { .. }
            | StepKind::Drag { .. } => {
                self.event(step);
                return self.start_time(step).map(Some);
            }
        }
        Ok(None)
    }

    /// (eval-live) A time step's plan, from its tick about to run: the
    /// ticks it takes (a `wait` in ticks at the rate now), a `rest`'s
    /// max, a glide's path and surface.
    fn start_time(&mut self, step: &Step) -> Result<TimeRun, Error> {
        let plan = match &step.kind {
            StepKind::Frames(n) => Plan::Ticks(*n),
            StepKind::Wait(ms_) => Plan::Ticks(self.clock.ticks_for(*ms_)),
            StepKind::Rest { max } => Plan::Rest { max: *max },
            StepKind::Glide { from, to, ms, on } | StepKind::Drag { from, to, ms, on } => {
                Plan::Glide {
                    from: *from,
                    to: *to,
                    n: self.clock.ticks_for(*ms),
                    surface: self.surface_for(on, step.line)?,
                    drag: matches!(step.kind, StepKind::Drag { .. }),
                }
            }
            // One axis frame a tick, the last one lifting the fingers
            // as a touchpad's last frame does.
            StepKind::Swipe { dy, ms } => Plan::Swipe {
                dy: *dy,
                n: self.clock.ticks_for(*ms).max(1),
            },
            StepKind::Type { n, ms: Some(ms) } => Plan::Type {
                n: *n,
                ticks: self.clock.ticks_for(*ms).max(1),
            },
            _ => return Err(self.err(step.line, "not a time step".into())),
        };
        Ok(TimeRun {
            line: step.line,
            k0: self.clock.k(),
            done: 0,
            plan,
        })
    }

    /// (eval-live) One tick of a time step; whether it is done. A
    /// `rest` ends at the first tick after which the shell is at rest,
    /// or at its max; a glide sends its pointer's place for the tick
    /// (a drag presses at its first and releases at its last).
    fn advance(&mut self, run: &mut TimeRun) -> Result<bool, Error> {
        match run.plan {
            Plan::Ticks(n) => {
                if run.done >= n {
                    return Ok(true);
                }
                self.tick(Vec::new())?;
                run.done += 1;
                Ok(run.done >= n)
            }
            Plan::Rest { max } => {
                let mut settled = false;
                if run.done < max {
                    self.tick(Vec::new())?;
                    run.done += 1;
                    settled = self.at_rest();
                    if !settled && run.done < max {
                        return Ok(false);
                    }
                }
                self.trace.rests.push(RestRec {
                    line: run.line,
                    k: run.k0,
                    ticks: run.done,
                    max,
                    settled,
                });
                self.close(settled)?;
                Ok(true)
            }
            Plan::Glide {
                from,
                to,
                n,
                surface: s,
                drag,
            } => {
                if run.done >= n {
                    return Ok(true);
                }
                let i = run.done;
                let f = if n == 1 {
                    1.0
                } else {
                    f64::from(i) / f64::from(n - 1)
                };
                let at = Point {
                    x: from.x + (to.x - from.x) * f,
                    y: from.y + (to.y - from.y) * f,
                };
                let mut input = Vec::new();
                if i == 0 && (n > 1 || drag) {
                    input.push(Input::Move(s, lp(from)));
                }
                if i == 0 && drag {
                    input.push(Input::Button(Button::Left, ButtonState::Pressed));
                }
                input.push(Input::Move(s, lp(at)));
                if i + 1 == n && drag {
                    input.push(Input::Button(Button::Left, ButtonState::Released));
                }
                self.tick(input)?;
                run.done += 1;
                Ok(run.done >= n)
            }
            Plan::Type { n, ticks } => {
                if run.done >= ticks {
                    return Ok(true);
                }
                let at = |i: u32| u64::from(i) * u64::from(n) / u64::from(ticks);
                let keys = at(run.done + 1) - at(run.done);
                self.tick(vec![Input::Keys(keys as u32)])?;
                run.done += 1;
                Ok(run.done >= ticks)
            }
            Plan::Swipe { dy, n } => {
                if run.done >= n {
                    return Ok(true);
                }
                let last = run.done + 1 == n;
                self.tick(vec![Input::Swipe(dy / f64::from(n), last)])?;
                run.done += 1;
                Ok(run.done >= n)
            }
        }
    }

    /// (eval-live) A live span begins: the clock runs with no step
    /// driving it until the session's next step, and the span is written
    /// as the time step that runs those ticks (`Span::end`). Its event is
    /// the one that step's own would be.
    fn span_begin(&mut self, line: u32) -> Span {
        self.trace.events.push(EventRec {
            k: self.clock.k(),
            line,
            text: "frames".into(),
            segment: self.open,
            ..EventRec::default()
        });
        Span {
            line,
            k0: self.clock.k(),
            ticks: 0,
            event: self.trace.events.len() - 1,
        }
    }

    /// (eval-live) One tick of a span; whether the segment it is in has
    /// now come to rest (the tick a `rest` from its start would end at).
    fn span_tick(&mut self, span: &mut Span) -> Result<bool, Error> {
        self.tick(Vec::new())?;
        span.ticks += 1;
        Ok(self.open.is_some() && self.at_rest())
    }

    /// (eval-live) A span ended: as the `rest` it was when its segment
    /// came to rest at its last tick (`rest max N` past a plain rest's
    /// max), else `frames N`. The step's text.
    fn span_end(&mut self, span: Span, rested: bool) -> Result<String, Error> {
        let text = if rested {
            let max = span.ticks.max(REST_MAX);
            self.trace.rests.push(RestRec {
                line: span.line,
                k: span.k0,
                ticks: span.ticks,
                max,
                settled: true,
            });
            self.close(true)?;
            if max == REST_MAX {
                "rest".to_string()
            } else {
                format!("rest max {max}")
            }
        } else {
            format!("frames {}", span.ticks)
        };
        if let Some(e) = self.trace.events.get_mut(span.event) {
            e.text = text.clone();
        }
        Ok(text)
    }

    /// Nothing more will happen without a step: no surface wants a frame,
    /// the router waits for nothing, neither logic's next timer nor the
    /// renderer's own (a tooltip's delay, a capped clock's tick) is due
    /// within [`REST_HORIZON`], no text is held, no surface is changing.
    fn at_rest(&self) -> bool {
        let t = self.clock.now();
        let wants = self
            .surfaces
            .shown
            .iter()
            .any(|s| s.fresh || self.r.wants_frame(s.id));
        !wants
            && self.router.next_wake().is_none()
            && self
                .r
                .next_wake()
                .is_none_or(|w| w > self.instant(t + REST_HORIZON))
            && self
                .inst
                .next_deadline()
                .is_none_or(|d| d > t + REST_HORIZON)
            && self.r.held_text().is_empty()
            && !self.r.has_surface_changes()
            && !self.images_pending()
    }

    /// A wallpaper palette (`material(image:)`) is being quantised, or a
    /// finished one is not reported to logic yet.
    fn images_pending(&self) -> bool {
        self.inst
            .theme()
            .is_some_and(|t| self.rt.untrack(|rt| t.images_pending(rt)))
    }

    /// (wallpaper-follow) Wallpaper palettes are quantised off the logic
    /// thread in real time, where the run's clock is virtual: the jobs
    /// started up to the tick before are waited for, so their palettes
    /// reach this tick's logic step, as text is shaped inline. Without
    /// it a palette landed after however many ticks the machine ran
    /// meanwhile, or after the run had ended.
    fn settle_images(&mut self) {
        if !self.images_pending() {
            return;
        }
        let Some(theme) = self.inst.theme() else {
            return;
        };
        if !theme.wait_images(&self.rt, IMAGE_WAIT) {
            self.notes.push(format!(
                "a wallpaper palette took longer than {IMAGE_WAIT:?} to quantise"
            ));
        }
    }

    /// One tick (see the module docs).
    fn tick(&mut self, input: Vec<Input>) -> Result<(), Error> {
        let k = self.clock.k();
        let t = self.clock.now();
        let now = self.instant(t);
        set_virtual_boot(t);
        self.router.set_now(Some(now));
        self.r.set_now(Some(now));
        // Wallpaper palettes the last tick asked for, before anything
        // reaches logic.
        self.settle_images();
        // (report) The keyboard moved by surfaces mapped or gone.
        self.kb_sync()?;
        for i in input {
            let ev = match i {
                Input::Move(s, at) => self.pointer.move_to(s, at, ms(t)),
                Input::Button(b, state) => {
                    self.pointer.button(b, state, ms(t)).into_iter().collect()
                }
                Input::Keys(n) => self.typed(n, t),
                Input::Swipe(dy, stop) => match self.pointer.swipe(dy, stop, ms(t)) {
                    Some(ev) => vec![ev],
                    None => {
                        return Err(Error::Run("a swipe with the pointer on no surface".into()));
                    }
                },
            };
            self.send(ev)?;
        }
        self.logic(t)?;
        // The loop's wakes between frames: texts arriving through the
        // waker, the renderer's own due time (the router's included).
        let texts = self.release(k, Via::Waker);
        if texts > 0 || self.r.next_wake().is_some_and(|w| w <= now) {
            self.wake()?;
        }
        // A frame held for logic's answer to its facts gets it first.
        let mut rounds = 0;
        while self.held_for_facts() && self.answer()? {
            rounds += 1;
            if rounds > FACT_ROUNDS {
                return Err(Error::Run(format!(
                    "a frame stays held for layout facts after {FACT_ROUNDS} answers"
                )));
            }
        }
        self.release(k, Via::Paint);
        self.paint(t)?;
        self.collect(k, t)?;
        // (flex review) A result action answered in `collect` is taken
        // by logic now, as the shell's loop takes a command's output.
        self.turn()?;
        // What the frames laid out reaches logic now; its answer lands
        // before the next frame.
        self.answer()?;
        for v in self.drop_one.values_mut() {
            *v = v.saturating_sub(1);
        }
        self.drop_one.retain(|_, v| *v > 0);
        self.drop_all = self.drop_all.saturating_sub(1);
        if let Some(leg) = self.leg.as_mut() {
            if let Some(tee) = self.surfaces.tee.as_mut() {
                leg.surface_changes(tee.drain(..));
            }
            // (integration-r7) What each layer surface's state asks for
            // now, for the bare commits the tick's end sends.
            let layers: Vec<_> = self
                .surfaces
                .shown
                .iter()
                .filter_map(|s| Some((s.id.0, self.surfaces.layer_config(s.id)?.clone())))
                .collect();
            leg.layers(k, layers);
            leg.end_tick(k, t, self.clock.period())?;
        }
        self.last_k = Some(k);
        if let Some(i) = self.open {
            self.trace.segments[i].e = k;
        }
        self.resolve_pending();
        self.clock.advance();
        Ok(())
    }

    /// Delivers the held texts due at tick `k` by `via`; how many.
    fn release(&mut self, k: u32, via: Via) -> usize {
        let held = self.r.held_text();
        if held.is_empty() {
            return 0;
        }
        let due = self.text.due(&held, k, via);
        if due.is_empty() {
            return 0;
        }
        self.r.release_text(&mut |h| due.contains(&h.key))
    }

    /// A shown surface's next frame waits (for logic's answer to its
    /// layout facts: no text or resize wait is left in the sandbox).
    fn held_for_facts(&self) -> bool {
        self.surfaces
            .shown
            .iter()
            .any(|s| self.r.frame_deadline(s.id).is_some())
    }

    /// Layout facts to logic and its answer back; whether there were any.
    fn answer(&mut self) -> Result<bool, Error> {
        // Taken now, or given before an input and not yet answered.
        self.give_facts();
        let Some(seq) = self.facts_seq.take() else {
            return Ok(false);
        };
        self.logic_owed = false;
        let u = self.inst.flush();
        for e in &u.errors {
            self.notes.push(format!("runtime error: {}", e.message()));
        }
        let mut diff = u.diff;
        diff.layout_seen = Some(seq);
        self.carry_reduced(&mut diff);
        self.apply(diff)?;
        Ok(true)
    }

    /// The renderer's layout and pointer facts set on logic (not yet
    /// flushed); whether there were any. The batch is noted for the
    /// answer that says it saw them.
    fn give_facts(&mut self) -> bool {
        let facts = self.r.take_layout_facts();
        // (flex) The pointer over the nodes logic reads it of.
        let pointer = self.r.take_pointer_facts();
        if facts.is_empty() && pointer.is_empty() {
            return false;
        }
        for (n, b) in facts {
            self.inst.set_box(n, b);
        }
        for (n, p) in pointer {
            self.inst.set_pointer(n, p);
        }
        self.facts_seq = Some(self.r.layout_seq());
        true
    }

    /// `backend cpu|gpu` mid-run: every shown surface drawn by that
    /// backend from its next frame (`both` is the header's alone: it
    /// means two runs).
    fn switch(&mut self, b: BackendMode, line: u32) -> Result<(), Error> {
        match b {
            BackendMode::Both => Err(self.err(
                line,
                "`backend both` is a header value (two runs, compared); a step switches to cpu or gpu"
                    .into(),
            )),
            BackendMode::Cpu => {
                self.on_gpu = false;
                if let Some(dev) = self.gpu.as_mut() {
                    for id in self.gpu_attached.drain() {
                        dev.release(&mut self.r, id, false);
                    }
                }
                Ok(())
            }
            BackendMode::Gpu => {
                if self.gpu.is_none() {
                    self.gpu = Some(Device::start(&mut self.r)?);
                }
                self.on_gpu = true;
                if let Some(dev) = self.gpu.as_mut() {
                    for sh in &self.surfaces.shown {
                        if self.gpu_attached.insert(sh.id) {
                            let scale = self.surfaces.scale_for(sh.id)?;
                            dev.attach(&mut self.r, sh.id, sh.size, scale);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Paints every surface that wants a frame at `t`, once; surface
    /// changes the paints leave wake the loop (`Host::painted`).
    fn paint(&mut self, t: Duration) -> Result<(), Error> {
        // (report) The first tick past the `warmup` paints every shown
        // surface whole: the trace's first frame, which the traced part
        // is held to as a run is to its boot.
        let edge = !self.warmed && t >= T0 + Duration::from_millis(self.sc.warmup);
        if edge {
            self.warmed = true;
            if self.sc.warmup > 0 {
                for s in &self.surfaces.shown {
                    self.r.invalidate(s.id);
                }
            }
        }
        let edge = edge && self.sc.warmup > 0;
        let ids: Vec<SurfaceId> = self
            .surfaces
            .shown
            .iter()
            .filter(|s| edge || s.fresh || self.r.wants_frame(s.id))
            .map(|s| s.id)
            .filter(|id| self.drop_all == 0 && !self.drop_one.contains_key(id))
            .collect();
        // The surface leg's manager gets this tick's changes so far
        // before its frames, the ones the paints leave after them.
        if let (Some(leg), Some(tee)) = (self.leg.as_mut(), self.surfaces.tee.as_mut()) {
            leg.surface_changes(tee.drain(..));
        }
        for id in ids {
            let mut cpu_damage = Damage::new();
            let gpu_drawn = match self.gpu.as_mut() {
                Some(dev) if self.gpu_attached.contains(&id) => {
                    let drawn = self.surfaces.paint_gpu(&mut self.r, dev, id, t)?;
                    dev.changes(&mut self.r);
                    if drawn && self.opts.redraw {
                        self.redraw_gpu(id)?;
                    }
                    Some(drawn)
                }
                _ => {
                    cpu_damage = self.surfaces.paint(&mut self.r, id, t)?;
                    if self.opts.redraw {
                        self.redraw_cpu(id, t)?;
                    }
                    None
                }
            };
            self.mirror(id, gpu_drawn, cpu_damage);
            let pose = self.r.delegated_pose(id);
            match pose {
                Some(p) => {
                    self.poses.insert(
                        id.0,
                        PoseRec {
                            opacity: q(p.opacity),
                            scale: q(p.scale),
                            x: q(p.offset.x),
                            y: q(p.offset.y),
                        },
                    );
                }
                None => {
                    self.poses.remove(&id.0);
                }
            }
            // (eval-rules-6) The pose commits with the frame: the
            // manager sets the margins its offset moves a layer surface
            // to (`State::sync_pose`), and places it there for the host
            // (`surface_placed`, the host's `send_origin`).
            let placed = self
                .surfaces
                .set_pose(id, pose.map(|p| [p.offset.x, p.offset.y]))?;
            for c in &placed {
                if let Change::Placed(pid) | Change::Configured(pid, ..) = *c {
                    let origin = self.surfaces.origin(pid);
                    self.r.set_surface_origin(pid, origin);
                }
            }
            for c in &placed {
                if let Change::Configured(pid, size, scale) = *c {
                    host_loop::configured(self, pid, size, scale)?;
                }
            }
            self.changed(placed)?;
            if let Some(sh) = self.surfaces.get(id) {
                self.committed.insert(
                    id.0,
                    Committed {
                        logical: sh.logical,
                        origin: sh.rest,
                        shown_at: sh.origin,
                        size: sh.size,
                        pixels: self.opts.images.then(|| sh.pixels.clone()),
                    },
                );
            }
            // `Host::painted`: its facts (answered at the tick's end),
            // and a wake owed for the surface changes it left.
            host_loop::painted(self, id)?;
        }
        // `host_loop::painted` woke the loop for surface changes a paint
        // left; text the frames shaped wakes it too (a worker's layout
        // arrives with a wake).
        self.turn()
    }

    /// (eval-surface-leg) Surface `id`'s frame, just painted (on the GPU
    /// when `gpu_drawn` is `Some`: whether it drew), to the surface leg,
    /// as the renderer answers right after the paint.
    fn mirror(&mut self, id: SurfaceId, gpu_drawn: Option<bool>, cpu_damage: Damage) {
        let Some(leg) = self.leg.as_mut() else {
            return;
        };
        let Some(sh) = self.surfaces.get(id) else {
            return;
        };
        let damage = match gpu_drawn {
            Some(true) => Damage::full(sh.size),
            Some(false) => Damage::new(),
            None => cpu_damage,
        };
        let scale = match self.surfaces.scale_for(id) {
            Ok(s) => s,
            Err(_) => return,
        };
        leg.frame(Snapshot {
            k: self.clock.k(),
            surface: id.0,
            node: sh.node,
            gpu: gpu_drawn.is_some(),
            size: sh.size,
            scale,
            damage,
            wants: self.r.wants_frame(id),
            input: Painter::input_region(&self.r, id),
            opaque: Painter::opaque_region(&self.r, id),
            blur: Painter::blur_region(&self.r, id),
            pose: Painter::surface_pose(&self.r, id),
            layer: self.surfaces.layer_config(id).cloned(),
        });
    }

    /// The `redraw` check on the CPU: surface `id`'s buffer, as the
    /// paint at `t` left it, against the same frame repainted in full
    /// into a fresh buffer (`Renderer::repaint_fresh`).
    fn redraw_cpu(&mut self, id: SurfaceId, t: Duration) -> Result<(), Error> {
        let scale = self.surfaces.scale_for(id)?;
        let Some(sh) = self.surfaces.get(id) else {
            return Ok(());
        };
        let mut fresh = vec![0u8; sh.pixels.len()];
        let target = PaintTarget::new(&mut fresh, sh.size, sh.size.w * 4, scale, 0)
            .map_err(|e| Error::Run(format!("surface {}: {e:?}", id.0)))?;
        let mut target = target.at(t);
        if !self.r.repaint_fresh(id, &mut target) {
            return Ok(());
        }
        let redrawn: Vec<[u32; 4]> = self
            .r
            .last_damage(id)
            .unwrap_or_default()
            .rects()
            .iter()
            .map(|r| [r.x.max(0) as u32, r.y.max(0) as u32, r.w, r.h])
            .collect();
        let rec = redraw::compare(
            &sh.pixels,
            &fresh,
            sh.size.w,
            sh.size.h,
            &redrawn,
            redraw::CPU_TOLERANCE,
        );
        // (eval-rules-4) A crossfade frame: its blend redone here.
        let mut plain = vec![0u8; sh.pixels.len()];
        let target = PaintTarget::new(&mut plain, sh.size, sh.size.w * 4, scale, 0)
            .map_err(|e| Error::Run(format!("surface {}: {e:?}", id.0)))?;
        let mut target = target.at(t);
        if let Some((old, w)) = self.r.repaint_unblended(id, &mut target) {
            self.blends
                .insert(id.0, redraw::reblend(&plain, old, &fresh, w));
        }
        self.keep_redraw(id, rec, false, &fresh)
    }

    /// The `redraw` check on the GPU leg: surface `id`'s frame as the GPU
    /// drew it, against the same frame drawn fresh on a device of its own
    /// ([`Fresh`]). A GPU frame redraws the whole buffer.
    fn redraw_gpu(&mut self, id: SurfaceId) -> Result<(), Error> {
        let scale = self.surfaces.scale_for(id)?;
        let Some(sh) = self.surfaces.get(id) else {
            return Ok(());
        };
        if self.fresh_gpu.is_none() {
            self.fresh_gpu = Some(Fresh::start()?);
        }
        let Some(dev) = self.fresh_gpu.as_mut() else {
            return Ok(());
        };
        let mut fresh = vec![0u8; sh.pixels.len()];
        if !dev.draw(&self.r, id, sh.size, scale, &mut fresh)? {
            return Ok(());
        }
        let whole = [0, 0, sh.size.w, sh.size.h];
        let rec = redraw::compare(
            &sh.pixels,
            &fresh,
            sh.size.w,
            sh.size.h,
            &[whole],
            redraw::GPU_TOLERANCE,
        );
        self.keep_redraw(id, rec, true, &fresh)
    }

    /// Keeps a `redraw` comparison's difference for this tick's frame,
    /// with the full repaint's image when the run keeps images.
    fn keep_redraw(
        &mut self,
        id: SurfaceId,
        rec: Option<RedrawRec>,
        gpu: bool,
        fresh: &[u8],
    ) -> Result<(), Error> {
        let Some(mut rec) = rec else {
            self.redraws.insert(id.0, None);
            return Ok(());
        };
        rec.gpu = gpu;
        if self.opts.images
            && let Some(sh) = self.surfaces.get(id)
        {
            let rgba = to_rgba(fresh);
            let name = format!(
                "frames/{}.png",
                &blake3::hash(&rgba).to_hex().as_str()[..16]
            );
            if !self.images.contains_key(&name) {
                let png = crate::report::png::encode(sh.size.w, sh.size.h, &rgba)?;
                self.images.insert(
                    name.clone(),
                    Image {
                        width: sh.size.w,
                        height: sh.size.h,
                        png,
                    },
                );
            }
            rec.png = Some(name);
        }
        self.redraws.insert(id.0, Some(rec));
        Ok(())
    }

    /// (replay-owner review) `RunOptions::drawn`: what
    /// `Renderer::drawn_nodes` gives for the surface `f` painted, held
    /// to `f`'s own rows.
    fn hold_drawn(&mut self, k: u32, f: &FrameTrace) {
        let s = f.surface.0;
        self.log.drawn_frames += 1;
        let Some(d) = self.r.drawn_nodes(f.surface) else {
            self.log
                .drawn
                .push(format!("k{k} surface {s}: no drawn frame"));
            return;
        };
        if d.nodes.len() != f.nodes.len() {
            self.log.drawn.push(format!(
                "k{k} surface {s}: {} nodes drawn, {} traced",
                d.nodes.len(),
                f.nodes.len()
            ));
        }
        for (i, n) in f.nodes.iter().enumerate() {
            match d.nodes.get(i) {
                Some(x) if x.node != n.node => self.log.drawn.push(format!(
                    "k{k} surface {s}: node {i} is {:?} drawn, {:?} traced",
                    x.node, n.node
                )),
                Some(x) if x.rect != n.visual || x.opacity != n.opacity => {
                    self.log.drawn.push(format!(
                        "k{k} surface {s}: {:?} drawn at {:?} opacity {}, traced at {:?} opacity {}",
                        n.node, x.rect, x.opacity, n.visual, n.opacity
                    ));
                }
                Some(_) => {}
                None => self
                    .log
                    .drawn
                    .push(format!("k{k} surface {s}: {:?} traced, not drawn", n.node)),
            }
        }
    }

    /// The tick's frame traces, held texts and commands, into the trace.
    fn collect(&mut self, k: u32, t: Duration) -> Result<(), Error> {
        let traces = self.r.take_frame_traces();
        if self.opts.drawn {
            for f in &traces {
                self.hold_drawn(k, f);
            }
        }
        let mut redraws = std::mem::take(&mut self.redraws);
        let mut blends = std::mem::take(&mut self.blends);
        let held = self.r.held_text();
        self.text.seen(&held, k);
        for a in self.host.take_actions() {
            self.trace.commands.push(CommandRec {
                k,
                text: format!("{a:?}"),
            });
        }
        for x in self.inst.take_execs() {
            // (lockmon) `rec.stop()`: the re-run queued goes; the
            // command asked to stop runs until its end (`end action`),
            // as the shell's runs until its program ends.
            if x.stop {
                self.trace.commands.push(CommandRec {
                    k,
                    text: format!("stop {}", x.action),
                });
                if let Some(run) = self.sandbox_runs.borrow_mut().get_mut(&x.action) {
                    run.queued = false;
                }
                continue;
            }
            self.trace.commands.push(CommandRec {
                k,
                text: format!("exec {}: {:?}", x.action, x.argv),
            });
            // (lockmon) A detached command runs from its call until a
            // step ends it (`end action`, a recording's end); a call
            // while one runs queues one re-run in place of any before.
            if x.reply.is_none() {
                let wall = self.wall(t);
                let mut runs = self.sandbox_runs.borrow_mut();
                let run = runs.entry(x.action.clone()).or_default();
                match run.started {
                    Some(_) => run.queued = true,
                    None => run.started = Some(wall),
                }
                let started = run.started;
                drop(runs);
                self.set_run(&x.action, started);
                self.logic_owed = true;
            }
            // (flex review) The sandbox runs no command: a result
            // action's call is answered from what was queued for it (a
            // recording's answer), else with an error naming that, so
            // its `.error` shows rather than a pending call for ever.
            if let Some(reply) = &x.reply {
                let queued = self
                    .exec_answers
                    .borrow_mut()
                    .get_mut(&x.action)
                    .and_then(std::collections::VecDeque::pop_front);
                let answer = match &queued {
                    Some(Ok(j)) => {
                        strand_compiler::vm::json::from_json(reply.types(), reply.ty(), j)
                    }
                    Some(Err(e)) => Err(e.clone()),
                    None => Err(format!(
                        "`{}` was not run: the sandbox runs no command, and no answer was \
                         recorded for this call",
                        x.action
                    )),
                };
                reply.answer(answer);
                self.logic_owed = true;
                if self.opts.log {
                    self.log
                        .fetches
                        .push((k, format!("action {}", x.action), queued.is_some()));
                }
            }
        }
        let committed = std::mem::take(&mut self.committed);
        if traces.is_empty() {
            return Ok(());
        }
        // (report) `warmup`: its frames are painted, neither traced nor
        // checked.
        if t < T0 + Duration::from_millis(self.sc.warmup) {
            return Ok(());
        }
        let mut cost: BTreeMap<u32, Cost> = BTreeMap::new();
        let mut last: BTreeMap<u32, &FrameTrace> = BTreeMap::new();
        for f in &traces {
            if last.contains_key(&f.surface.0) {
                return Err(Error::Run(format!(
                    "surface {} painted twice at tick {k}: a frame would go unchecked",
                    f.surface.0
                )));
            }
            let c = cost.entry(f.surface.0).or_default();
            c.paints += 1;
            c.damage_px += f.damage_px;
            c.raster_px += f.raster_px;
            c.items = f.items;
            c.records = f.records;
            c.layout_passes += f.layout_passes;
            c.layout_nodes = f.layout_nodes;
            c.text_pending = f.text_pending;
            c.text_held = f.text_held;
            c.flatten_us += f.times.flatten.as_micros() as u64;
            c.text_wait_us += f.times.text_wait.as_micros() as u64;
            c.raster_us += f.times.raster.as_micros() as u64;
            last.insert(f.surface.0, f);
        }
        let mut frame = Frame {
            k,
            t_ns: t.as_nanos() as u64,
            painted: last.keys().copied().collect(),
            wants: self
                .surfaces
                .shown
                .iter()
                .filter(|s| self.r.wants_frame(s.id))
                .map(|s| s.id.0)
                .collect(),
            sizes: self
                .surfaces
                .shown
                .iter()
                .map(|s| {
                    (
                        s.id.0,
                        committed.get(&s.id.0).map_or(s.logical, |c| c.logical),
                    )
                })
                .collect(),
            fresh: last.iter().map(|(s, f)| (*s, f.fresh)).collect(),
            fades: last
                .iter()
                .filter_map(|(s, f)| Some((*s, f.fade?)))
                .collect(),
            held_text: held
                .iter()
                .map(|h| HeldRec {
                    key: h.key,
                    text: h.text.clone(),
                    paints: h.paints,
                })
                .collect(),
            scale: self.surfaces.screen.scale,
            scales: if self.surfaces.several() {
                self.surfaces
                    .shown
                    .iter()
                    .map(|s| (s.id.0, self.surfaces.scale_of_surface(s.id)))
                    .collect()
            } else {
                BTreeMap::new()
            },
            driven: std::mem::take(&mut self.driven),
            origins: self
                .surfaces
                .shown
                .iter()
                .map(|s| (s.id.0, committed.get(&s.id.0).map_or(s.rest, |c| c.origin)))
                .collect(),
            // What each surface painted this tick committed with, the
            // one closed by its paint included.
            shifts: committed
                .iter()
                .filter_map(|(s, c)| {
                    let d = [c.shown_at[0] - c.origin[0], c.shown_at[1] - c.origin[1]];
                    (d != [0.0, 0.0]).then_some((*s, d))
                })
                .collect(),
            poses: std::mem::take(&mut self.poses)
                .into_iter()
                .filter(|(s, _)| last.contains_key(s))
                .collect(),
            reduced: self.reduced,
            ..Frame::default()
        };
        // (integration-r8) The global tokens that read time, by eval's
        // own walk (`crate::timed`), once a frame.
        let time_paths = crate::timed::time_paths(&self.r.tree().tokens);
        for (s, f) in &last {
            frame.n.extend(self.rows(*s, &f.nodes));
            // (eval-rules-2) Its springs' samples and its rolls.
            if !f.springs.is_empty() {
                frame.period.insert(*s, f.period.as_nanos() as u64);
            }
            for sp in &f.springs {
                let rec = self.spring_rec(*s, t, sp);
                frame.sp.push(rec);
            }
            for n in f.nodes.iter() {
                if let Some(k0) = self.window_mounts.remove(&n.node) {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    self.trace.windowed.entry(key).or_default().push(k0);
                }
                if let Some(k0) = self.surface_opens.remove(&n.node) {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    self.trace.opened.entry(key).or_default().push(k0);
                }
                // (clip-data) Its value and the router's flags on it.
                if let Some(rec) = self.ui_rec(*s, n.node) {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    frame.ui.insert(key, rec);
                }
                if let Some(v) = n.drag {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    frame.drags.insert(key, v);
                }
                if let Some(st) = self.r.scroll_state(n.node) {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    frame.offsets.insert(key, st.offset);
                }
                if let Some(r) = &n.roll {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    let text = |t: &Option<std::sync::Arc<str>>| t.as_deref().map(String::from);
                    frame.rolls.push(crate::trace::RollRec {
                        key,
                        from: text(&r.from),
                        to: text(&r.to),
                        p: r.p,
                        was: r.was.as_ref().map(|(a, b, p)| (text(a), text(b), *p)),
                    });
                }
                // (eval-rules-6) Its border's ring, a container's gap, and
                // a text drawn whole that could break narrower.
                self.flow_facts(*s, n, &mut frame);
                // (eval-rules-8) What drives it by time, and its shadows.
                self.time_facts(*s, n, &time_paths, &mut frame);
                // (eval-rules-4) Its images' decodes and its cut lines.
                if !n.images.is_empty() || n.cut.is_some() {
                    let key = self
                        .labels
                        .key(n.node, &self.inst, self.r.tree(), &self.sources);
                    let rect = |r: strand_scene::LogicalRect| [r.x, r.y, r.w, r.h];
                    for i in &n.images {
                        let file = self.file_size(&i.file);
                        frame.imgs.push(crate::trace::ImageRec {
                            key: key.clone(),
                            s: *s,
                            rect: rect(i.rect),
                            dest: rect(i.dest),
                            px: i.pixmap,
                            src: i.source,
                            fit: i.fit.to_string(),
                            pfit: i.pixmap_fit.to_string(),
                            stand_in: i.stand_in,
                            file,
                        });
                    }
                    if let Some(c) = &n.cut {
                        frame.lines.push(crate::trace::LinesRec {
                            key,
                            s: *s,
                            text: c.text.to_string(),
                            ellipsis: c.ellipsis.map(String::from),
                            max_lines: c.max_lines,
                            stops: c.stops.clone(),
                            natural: c.natural.clone(),
                        });
                    }
                }
            }
            frame.regions.insert(*s, regions(&self.r, SurfaceId(*s)));
            if self.opts.images
                && cost.get(s).is_some_and(|c| c.damage_px > 0)
                && self.surfaces.get(SurfaceId(*s)).is_some()
                && let Some(c) = committed.get(s)
                && let Some(pixels) = &c.pixels
            {
                let rgba = to_rgba(pixels);
                let name = format!(
                    "frames/{}.png",
                    &blake3::hash(&rgba).to_hex().as_str()[..16]
                );
                if !self.images.contains_key(&name) {
                    let png = crate::report::png::encode(c.size.w, c.size.h, &rgba)?;
                    self.images.insert(
                        name.clone(),
                        Image {
                            width: c.size.w,
                            height: c.size.h,
                            png,
                        },
                    );
                }
                frame.png.insert(*s, name);
            }
        }
        frame.cost = cost;
        // The renderer samples sizes in a map's order: by node and what,
        // each one's samples in the order taken (a stable sort).
        frame
            .sp
            .sort_by(|a, b| (a.s, &a.key, &a.of).cmp(&(b.s, &b.key, &b.of)));
        frame.ptr = self.pointer.surface.map(|s| crate::trace::PtrRec {
            s: s.0,
            at: [self.pointer.at.x, self.pointer.at.y],
        });
        frame.cuts = self
            .surfaces
            .shown
            .iter()
            .map(|s| (s.id.0, self.surfaces.cuts(s.id)))
            .filter(|(_, c)| *c != 0)
            .collect();
        redraws.retain(|s, _| last.contains_key(s));
        blends.retain(|s, _| last.contains_key(s));
        frame.blend = blends;
        self.trace.redraw += redraws.len() as u32;
        frame.redraw = redraws
            .into_iter()
            .filter_map(|(s, r)| Some((s, r?)))
            .collect();
        self.trace.frames.push(frame);
        Ok(())
    }

    /// (clip-data) What `Frame::ui` records of `node` on surface `s`: a
    /// `slider`'s, `meter`'s, `arc`'s, `graph`'s or `segmented`'s value,
    /// `range` and `step` as logic sent them (a segmented's value as its
    /// option's label), and whether the router has it hovered, pressed
    /// or focused; `None` for any other node it has none of.
    fn ui_rec(&self, s: u32, node: NodeId) -> Option<crate::trace::UiRec> {
        use strand_scene::NodeKind;
        let w = self.r.widgets();
        let f: Vec<String> = [
            (w.hovered.contains(&node), "hover"),
            (w.pressed.contains(&node), "press"),
            (w.focused.contains(&node), "focus"),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| n.to_string())
        .collect();
        let n = self.r.tree().get(node)?;
        let control = matches!(
            n.kind,
            NodeKind::Slider
                | NodeKind::Meter
                | NodeKind::Arc
                | NodeKind::Graph
                | NodeKind::Segmented
        );
        if !control && f.is_empty() {
            return None;
        }
        let mut rec = crate::trace::UiRec {
            s,
            f,
            ..Default::default()
        };
        if control {
            rec.v = n.get(Prop::Value).map(|v| match n.kind {
                NodeKind::Segmented => {
                    serde_json::Value::String(strand_render::widgets::option_label(v))
                }
                _ => prop_json(v),
            });
            rec.range = n.get(Prop::Range).map(prop_json);
            rec.step = n.get(Prop::Step).map(prop_json);
        }
        Some(rec)
    }

    /// (eval-rules-6) What `frame` records of node `n` on surface `s`
    /// for the eval-rules-6 arms: its `border:` ring, measured along
    /// [`crate::ring::rays`] (one drawn along a `shape:` or squircle
    /// corners, or with a radius in a form `ring::radii` does not read,
    /// is left out); a flex container's `gap:`; and, on a surface that
    /// wants no more frames, a text drawn whole on one line that could
    /// break narrower (its text as it is now, two words or more apart by
    /// spaces, with no `ellipsis:` or `max_lines:` and no line break).
    /// (eval-rules-8) What time drives `n` by at this frame, as logic
    /// declared it (its `play`'s block and `seq`, a prop that reads time;
    /// the block recorded once by name), and each shadow it drew with
    /// what its props asked of it.
    fn time_facts(
        &mut self,
        s: u32,
        n: &strand_render::probe::NodeTrace,
        time_paths: &HashSet<String>,
        frame: &mut Frame,
    ) {
        let tree = self.r.tree();
        let Some(node) = tree.get(n.node) else {
            return;
        };
        let play = match node.get(Prop::Play) {
            Some(PropValue::Keyframes(k)) => Some(k.clone()),
            _ => None,
        };
        // (integration-r8) Read by eval's own walk of the declaration,
        // a timed `tokens` scope of an ancestor included, not by the
        // product's detection that decides what time drives.
        let t = crate::timed::reads_time(tree, node, time_paths);
        if play.is_none() && !t && n.paint.shadows.is_empty() {
            return;
        }
        let key = self
            .labels
            .key(n.node, &self.inst, self.r.tree(), &self.sources);
        if let Some(k) = &play {
            self.trace
                .keyframes
                .entry(k.name.clone())
                .or_insert_with(|| keyframes_rec(k));
        }
        if play.is_some() || t {
            frame.timed.insert(
                key.clone(),
                crate::trace::TimedRec {
                    play: play.map(|k| (k.name.clone(), k.seq)),
                    t,
                },
            );
        }
        let rect = |r: strand_scene::LogicalRect| [r.x, r.y, r.w, r.h];
        for sh in &n.paint.shadows {
            frame.shadows.push(crate::trace::ShadowRec {
                key: key.clone(),
                s,
                frame: rect(sh.frame),
                extent: rect(sh.extent),
                asked: sh.asked,
                a: sh.color.a,
            });
        }
    }

    fn flow_facts(&mut self, s: u32, n: &strand_render::probe::NodeTrace, frame: &mut Frame) {
        use strand_scene::{NodeKind, Prop, PropValue};
        let flex = matches!(n.kind, NodeKind::Row | NodeKind::Col | NodeKind::Scroll);
        let gap = n.gap.filter(|g| *g > 0.0 && flex);
        let ring = n
            .ring
            .as_ref()
            .filter(|r| !r.shaped)
            .and_then(|r| Some((r, crate::ring::radii(r.radius.as_ref(), r.size)?)));
        let at_rest = !frame.wants.contains(&s);
        let loose = at_rest
            && n.kind == NodeKind::Text
            && n.hidden.is_none()
            && n.fit.is_none()
            && n.text == Some(strand_render::probe::TextDrawn::Shaped)
            && self.r.tree().get(n.node).is_some_and(|t| {
                let words = match t.get(Prop::Text) {
                    Some(PropValue::Text(text)) if !text.contains('\n') => {
                        text.split(' ').filter(|w| !w.is_empty()).count()
                    }
                    _ => 0,
                };
                words >= 2 && t.get(Prop::Ellipsis).is_none() && t.get(Prop::MaxLines).is_none()
            });
        if gap.is_none() && ring.is_none() && !loose {
            return;
        }
        let key = self
            .labels
            .key(n.node, &self.inst, self.r.tree(), &self.sources);
        if let Some(g) = gap {
            frame.gaps.insert(key.clone(), g);
        }
        if let Some((r, radius)) = ring {
            let drawn = crate::ring::rays(r.frame)
                .into_iter()
                .map(|ray| {
                    crate::ring::crossings(&r.path, ray)
                        .into_iter()
                        .map(|t| t as f32)
                        .collect()
                })
                .collect();
            frame.rings.push(crate::ring::RingRec {
                key: key.clone(),
                s,
                frame: r.frame,
                scale: r.scale,
                width: r.width,
                radius,
                drawn,
            });
        }
        if loose {
            frame.loose.push(key);
        }
    }

    /// (eval-rules-2) A spring's sample on `surface` in the tick at `t`,
    /// as the trace records it.
    fn spring_rec(
        &mut self,
        surface: u32,
        t: Duration,
        sp: &strand_render::probe::SpringTrace,
    ) -> crate::trace::SpringRec {
        use strand_render::probe::SpringOf;
        // (eval-rules-3) A palette root is named by its token path.
        let key = match (&sp.what, &sp.token) {
            (SpringOf::Palette, Some(t)) => format!("${t}"),
            _ => self
                .labels
                .key(sp.node, &self.inst, self.r.tree(), &self.sources),
        };
        crate::trace::SpringRec {
            key,
            s: surface,
            of: match sp.what {
                SpringOf::Prop(p) => p.name().to_string(),
                SpringOf::Size(0) => "w".into(),
                SpringOf::Size(_) => "h".into(),
                SpringOf::Glide => "glide".into(),
                SpringOf::Palette => "palette".into(),
            },
            at: (sp.at != t).then_some(sp.at.as_nanos() as u64),
            v: sp.value.clone(),
            to: sp.target.clone(),
            c: crate::trace::CurveRec::of(&sp.curve),
            eps: sp.eps.clone(),
            rest: sp.rest.clone(),
            start: sp.start.as_ref().map(|st| crate::trace::StartRec {
                last: st.last.map(|l| l.as_nanos() as u64),
                shift: if st.shift.iter().all(|v| *v == 0.0) {
                    Vec::new()
                } else {
                    st.shift.clone()
                },
            }),
            enter: sp.entering,
            ask: sp.asked.as_ref().map(crate::trace::CurveRec::of),
        }
    }

    /// A surface's traced nodes as rows: keys, effective opacity, motions
    /// and authored axes (an ancestor's x/y authors x/y, its opacity
    /// opacity, its scale or rotation all geometry, its size nothing:
    /// [`inherited`]).
    fn rows(&mut self, surface: u32, nodes: &[NodeTrace]) -> Vec<Row> {
        // Per node: effective opacity, authored axes, motions, and the
        // offset in flight it gives its children (its ancestors' and its
        // own springs and glides, a drag's lift included, and
        // (integration-r8) a shared morph's move: it springs to nothing,
        // so a node inside a morphing card rests where its card does).
        let mut eff: HashMap<NodeId, (f32, Axes, u32, [f32; 2])> = HashMap::new();
        let mut out = Vec::with_capacity(nodes.len());
        for n in nodes {
            let (pop, pauth, pmot, flight) = n
                .parent
                .and_then(|p| eff.get(&p))
                .copied()
                .unwrap_or((1.0, Axes::NONE, 0, [0.0, 0.0]));
            let op = pop * n.opacity;
            let auth = n.authored | inherited(pauth);
            let emot = pmot | n.motions.0;
            let p = &n.parts;
            let carries = [
                flight[0] + p.spring[0] + p.glide[0] + p.morph[0],
                flight[1] + p.spring[1] + p.glide[1] + p.morph[1],
            ];
            eff.insert(n.node, (op, auth, emot, carries));
            let key = self
                .labels
                .key(n.node, &self.inst, self.r.tree(), &self.sources);
            if self.opts.log
                && !self.log.refs.contains_key(&key)
                && let Some(r) = self.node_ref(n.node)
            {
                self.log.refs.insert(key.clone(), r);
            }
            let rect = |r: strand_scene::LogicalRect| [q(r.x), q(r.y), q(r.w), q(r.h)];
            let pair = |p: [f32; 2]| [q(p[0]), q(p[1])];
            out.push(Row {
                key,
                surface,
                drawn: rect(n.drawn),
                visual: rect(n.visual),
                laid: rect(n.laid),
                op,
                own_op: n.opacity,
                scale: n.scale,
                rotate: n.rotate,
                mot: n.motions.0,
                emot,
                auth: auth.0,
                hidden: n.hidden.map(|h| {
                    match h {
                        Hidden::Opacity0 => "opacity0",
                        Hidden::Scale0 => "scale0",
                        Hidden::Inert => "inert",
                        Hidden::Clipped => "clipped",
                    }
                    .to_string()
                }),
                text: n.text.map(|t| {
                    match t {
                        TextDrawn::Shaped => "S",
                        TextDrawn::Estimate => "E",
                        TextDrawn::StandIn => "I",
                    }
                    .to_string()
                }),
                inherited: pair(n.parts.inherited),
                written: pair(n.parts.written),
                spring: pair(n.parts.spring),
                glide: pair(n.parts.glide),
                morph: pair(n.parts.morph),
                flight: pair(flight),
                fit: n.fit.map(q),
                ink: n.ink.map(rect),
                ov: n.overhang,
                clip: n.clip.map(rect),
                paint: crate::paint::PaintRec::of(
                    &n.paint,
                    &n.images,
                    n.drawn,
                    self.surfaces.scale_of_surface(SurfaceId(surface)),
                ),
            });
        }
        out
    }

    fn finish(mut self) -> Result<RunOutput, Error> {
        self.close(false)?;
        if let Some(e) = self.unmatched(None) {
            return Err(e);
        }
        if let Some(leg) = self.leg.take() {
            let log = leg.finish()?;
            self.notes
                .extend(log.notes.iter().map(|n| format!("the surface leg: {n}")));
            self.trace.wire = Some(log);
        }
        if let Some(dev) = self.gpu.take() {
            let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
            for d in &dev.dropped {
                *kinds.entry(d.as_str()).or_default() += 1;
            }
            for (what, n) in kinds {
                self.notes
                    .push(format!("the GPU leg dropped {what} ({n} times)"));
            }
            dev.finish();
        }
        if let Some(dev) = self.fresh_gpu.take() {
            dev.finish();
        }
        self.trace.nodes = std::mem::take(&mut self.labels.metas);
        self.trace.written = std::mem::take(&mut self.labels.written);
        if self.sc.warmup > 0 {
            trim_warmup(&mut self.trace);
        }
        for (path, _) in &self.seeds {
            self.notes
                .push(format!("[seed] `{path}`: no such cell was ever mounted"));
        }
        self.keep_writes();
        self.trace.seal();
        Ok(RunOutput {
            trace: self.trace,
            images: self.images,
            notes: self.notes,
            log: self.log,
        })
    }
}

/// (eval-host-loop) The driver's side of the host loop's reactions
/// (`strand_render::host_loop`, which the shell's main thread runs too):
/// routing's intents go to the logic VM, whose answer is owed to the
/// loop's next turn; layout facts wait for the driver's own points
/// (`answer`: a held frame's, and the tick's end), as the shell's logic
/// thread answers between frames; a wake is owed to the next turn.
impl LoopHost for Harness {
    type Error = Error;

    fn parts(&mut self) -> (&mut Renderer, Option<&mut Router>) {
        (&mut self.r, Some(&mut self.router))
    }

    fn shown(&self, out: &mut Vec<SurfaceId>) {
        out.extend(self.surfaces.shown.iter().map(|s| s.id));
    }

    fn applied(&mut self, errors: Vec<strand_render::SceneError>) -> Result<(), Error> {
        if errors.is_empty() {
            return Ok(());
        }
        Err(Error::Run(format!(
            "render refused logic's diff: {errors:?}"
        )))
    }

    fn intents(&mut self, _: Cause<'_>, intents: Vec<strand_render::Intent>) -> Result<(), Error> {
        if !intents.is_empty() && to_logic(&self.inst, &self.host, intents, &mut self.notes) {
            self.logic_owed = true;
        }
        Ok(())
    }

    /// (flex review) As the shell's main thread sends them before an
    /// input's intents: logic reads where a press went down in `on
    /// change self.pressed`. Its answer comes at the loop's turn.
    fn facts(&mut self) -> Result<(), Error> {
        if self.give_facts() {
            self.logic_owed = true;
        }
        Ok(())
    }

    fn wake(&mut self) -> Result<(), Error> {
        self.wake_owed = true;
        Ok(())
    }
}

impl LoopSync for Harness {
    /// The renderer's surface changes to the surfaces, as the surface
    /// manager takes them (`State::apply_surface_change`): each surface
    /// attached (on the one screen), placed and configured, or detached,
    /// through the host loop; then noted in the trace.
    fn sync(&mut self) -> Result<(), Error> {
        let k = self.clock.k();
        let screen = self.surfaces.screen;
        let bounds = Some(LogicalSize::new(screen.width as f32, screen.height as f32));
        let mut done = Vec::new();
        for (node, change) in self.r.take_surface_changes() {
            for c in self.surfaces.change(node, change, k)? {
                self.surface_change(c, bounds)?;
                done.push(c);
            }
        }
        // (eval-false-alarms) The leg's fake compositor stretches a layer
        // surface over the whole output: one another surface's zone
        // sized here would be configured there at another size.
        if self.leg.is_some()
            && let Some(id) = self.surfaces.sized_by_a_zone()
        {
            return Err(Error::Unsupported(format!(
                "the surface leg with surface {} sized by another surface's exclusive zone \
                 (the fake compositor arranges no zones)",
                id.0
            )));
        }
        self.changed(done)
    }
}

/// What the last frame of `surface` asks of the compositor besides its
/// pixels (the `Painter` regions): where it takes input (logical), where
/// it is opaque and where it blurs behind (buffer pixels).
fn regions(r: &Renderer, surface: SurfaceId) -> Regions {
    let rect = |x: strand_scene::Rect| [x.x as i64, x.y as i64, x.w as i64, x.h as i64];
    Regions {
        input: Painter::input_region(r, surface)
            .map(|v| v.iter().map(|x| [q(x.x), q(x.y), q(x.w), q(x.h)]).collect()),
        opaque: Painter::opaque_region(r, surface)
            .rects()
            .iter()
            .map(|x| rect(*x))
            .collect(),
        blur: Painter::blur_region(r, surface)
            .iter()
            .map(|b| BlurRec {
                rect: rect(b.rect),
                radii: b.radii,
                radius: b.radius,
            })
            .collect(),
    }
}

/// The axes a node's authored motion moves on its children: its offset
/// carries them on x and y and its opacity fades them; its scale or
/// rotation transforms their drawn boxes on every axis of geometry,
/// their laid boxes unchanged. Its size does not: a size that springs or
/// plays is laid out each frame, so its children's laid boxes follow it
/// and their drawn boxes are held to those (a child drawn off its layout
/// under a growing card is the bug-24 class, not authored).
pub fn inherited(parent: Axes) -> Axes {
    let mut a = Axes(parent.0 & (Axes::X | Axes::Y | Axes::OPACITY).0);
    if parent.intersects(Axes::SCALE | Axes::ROTATE) {
        a |= Axes::GEOMETRY;
    }
    a
}

fn lp(p: Point) -> LogicalPoint {
    LogicalPoint::new(p.x as f32, p.y as f32)
}

/// Milliseconds for an input event's timestamp.
fn ms(t: Duration) -> u32 {
    t.as_millis() as u32
}

/// (popup-shrink) A surface as its paint left it, before the loop
/// configured it again (`Harness::committed`).
struct Committed {
    logical: [f32; 2],
    /// Where it lies at rest (`Shown::rest`).
    origin: [f32; 2],
    /// (eval-rules-6) Where the compositor put it for this frame, at the
    /// margins its pose's offset set (`Shown::origin`).
    shown_at: [f32; 2],
    size: Size,
    pixels: Option<Vec<u8>>,
}

/// Logic's wall clock at presentation time `t`.
/// (report) The `for` row key a node is mounted under: `None` outside
/// any, and for a node directly in a per-screen surface, whose innermost
/// qualifier is its screen's id (fidelity: one of the scenario's
/// screens), which differs between the live shell's output and the
/// sandbox's (the binary's `row_key` in `run/request/report.rs`, which
/// `nodes.json` is written by).
fn row_key(
    i: &strand_compiler::instantiate::inspect::Identity,
    screens: &[crate::scenario::ScreenDef],
) -> Option<String> {
    let key = i.key.as_ref()?;
    (!screens.iter().any(|d| d.identity.id == *key)).then(|| key.clone())
}

/// (report) The trace of a run with a `warmup` from its first traced
/// frame `k0` on: the segments that ended in the warm-up fold into boot,
/// which ends at `k0` (the traced part starts from what the warm-up
/// left, as a run starts from its boot), one under way at `k0` starts
/// after it, and what was recorded of ticks before `k0` goes.
fn trim_warmup(trace: &mut RunTrace) {
    let Some(k0) = trace.frames.first().map(|f| f.k) else {
        return;
    };
    let mut map: Vec<Option<usize>> = Vec::with_capacity(trace.segments.len());
    let mut kept: Vec<Segment> = Vec::new();
    for (i, mut seg) in std::mem::take(&mut trace.segments).into_iter().enumerate() {
        if i == 0 {
            seg.s = None;
            seg.e = seg.e.max(k0);
            map.push(Some(0));
            kept.push(seg);
            continue;
        }
        let boot_e = kept[0].e;
        if seg.e <= boot_e {
            map.push(Some(0));
            continue;
        }
        if seg.s.is_none_or(|s| s < boot_e) {
            seg.s = Some(boot_e);
        }
        map.push(Some(kept.len()));
        kept.push(seg);
    }
    trace.segments = kept;
    let remap = |s: &mut Option<usize>| *s = s.and_then(|i| map.get(i).copied().flatten());
    trace.events.retain(|e| e.k >= k0);
    trace.events.iter_mut().for_each(|e| remap(&mut e.segment));
    trace.expects.retain(|e| e.k >= k0);
    trace.expects.iter_mut().for_each(|e| remap(&mut e.segment));
    trace.rests.retain(|r| r.k >= k0);
    trace.commands.retain(|c| c.k >= k0);
    for m in [&mut trace.windowed, &mut trace.opened, &mut trace.closed] {
        m.values_mut().for_each(|ks| ks.retain(|k| *k >= k0));
        m.retain(|_, ks| !ks.is_empty());
    }
    trace
        .surfaces
        .retain(|s| s.detached.is_none_or(|d| d >= k0));
    for s in &mut trace.surfaces {
        s.attached = s.attached.max(k0);
        // The placement it had at `k0` stands from `k0`.
        let before = s.places.iter().rposition(|p| p.k < k0);
        if let Some(i) = before {
            s.places.drain(..i);
            s.places[0].k = k0;
            if s.places.get(1).is_some_and(|p| p.k == k0) {
                s.places.remove(0);
            }
        }
    }
}

/// (clip-data) A control's prop value as `Frame::ui` records it: a
/// number as the shortest decimal of its `f32` (`0.69000244`), a length
/// in px as its number, a keyword or text as itself, a list item by
/// item; anything else as its debug form, cut to 80 characters.
fn prop_json(v: &strand_scene::PropValue) -> serde_json::Value {
    use serde_json::Value as J;
    use strand_scene::{Length, PropValue as P};
    let num = |n: f32| {
        format!("{n}")
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(J::Null, J::Number)
    };
    match v {
        P::Unset => J::Null,
        P::Bool(b) => J::Bool(*b),
        P::Number(n) | P::Length(Length::Px(n)) => num(*n),
        P::Keyword(k) => J::String(k.clone()),
        P::Text(t) => J::String(t.clone()),
        P::List(items) => J::Array(items.iter().map(prop_json).collect()),
        other => J::String(format!("{other:?}").chars().take(80).collect()),
    }
}

/// (eval-rules-8) A `keyframes` block as the trace records it
/// ([`crate::trace::KeyframesRec`]): the values its stops give the props
/// read as numbers (a length in px, an angle in degrees, a plain
/// number), the others named.
fn keyframes_rec(k: &strand_scene::Keyframes) -> crate::trace::KeyframesRec {
    let mut other: Vec<String> = Vec::new();
    let stops = k
        .stops
        .iter()
        .map(|(at, set)| {
            let mut vals = BTreeMap::new();
            for (p, v) in set {
                let name = p.name().to_string();
                let read = matches!(
                    p,
                    Prop::X | Prop::Y | Prop::Rotate | Prop::Opacity | Prop::Scale
                );
                let n = match v {
                    PropValue::Number(n)
                    | PropValue::Angle(n)
                    | PropValue::Length(strand_scene::Length::Px(n)) => Some(*n),
                    _ => None,
                };
                match (read, n) {
                    (true, Some(n)) if n.is_finite() => {
                        vals.insert(name, n);
                    }
                    _ => {
                        if !other.contains(&name) {
                            other.push(name);
                        }
                    }
                }
            }
            (*at, vals)
        })
        .collect();
    crate::trace::KeyframesRec {
        stops,
        other,
        d: k.duration.as_nanos() as u64,
        delay: k.delay.as_nanos() as u64,
        repeat: k.repeat,
        alternate: k.alternate,
        ease: match crate::trace::CurveRec::of(&strand_scene::Curve::Timed {
            duration: k.duration,
            easing: k.easing,
        }) {
            crate::trace::CurveRec::Timed { ease, .. } => ease,
            _ => crate::trace::EaseRec::Linear,
        },
    }
}

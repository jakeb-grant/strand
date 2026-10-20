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
//! (`set_now`), so the run is a function of the scenario.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, Storage, Update, parse_text};
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
    ButtonState, Damage, LogicalPoint, LogicalSize, NodeId, PaintTarget, Painter, Prop, PropValue,
    SceneDiff, Size, SurfaceId,
};
use strand_text::{FontConfig, TextEngine};

use crate::Error;
use crate::checks::redraw;
use crate::clock::{Clock, T0};
use crate::gpu::{Device, Fresh};
use crate::input::{Pointer, to_logic};
use crate::labels::Labels;
use crate::scenario::{BackendMode, Config, Fonts, Scenario, Screen, TextMode};
use crate::select::Selector;
use crate::steps::{Button, ExpectKind, Intention, Point, REST_MAX, Step, StepKind, Via};
use crate::surfaces::{Change, SCREEN_ID, Surfaces, to_rgba};
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
}

/// Runs `sc`.
pub fn run(sc: &Scenario, opts: &RunOptions) -> Result<RunOutput, Error> {
    let mut h = Harness::boot(Rc::new(sc.clone()), opts)?;
    for step in &sc.steps {
        h.step(step)?;
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

/// The sources of `sc`'s config, named as they are written in it.
fn sources(sc: &Scenario) -> Result<SourceMap, Error> {
    let mut map = SourceMap::new();
    match &sc.config {
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
/// file).
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
        matches!(self.plan, Plan::Glide { .. } | Plan::Swipe { .. })
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
        let build = Build::compile_with(None, map.clone(), &schema)
            .map_err(|d| load_failed(&sc, &map, &d))?;
        let warnings = diagnostics(&sc, &map, &build.warnings, false);
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
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
            surfaces: Surfaces::new(sc.screen),
            labels: Labels::new(),
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
        };
        if h.leg.is_some() {
            h.surfaces.tee = Some(Vec::new());
        }
        h.set_screens()?;
        for (path, text) in &sc.services {
            h.set_service(path, text)
                .map_err(|m| Error::Config(format!("[services] `{path}`: {m}")))?;
        }
        // Reduced motion is `system.reduced_motion`, which logic reads
        // and its diffs carry to render, as in the shell.
        if reduced {
            h.set_reduced_motion(true)?;
        }
        let (u, _) = h.inst.step(T0, wall(T0));
        h.update(u)?;
        Ok(h)
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

    /// `screens.all` and `screens.focused`: the one screen, as
    /// crates/strand/src/run/mod.rs `set_screens` makes it.
    fn set_screens(&mut self) -> Result<(), Error> {
        let s: Screen = self.surfaces.screen;
        let screen = self.host.record(
            "Screen",
            &[
                ("id", Value::text(SCREEN_ID)),
                ("name", Value::text(crate::surfaces::OUTPUT)),
                ("make", Value::text("Mock")),
                ("model", Value::text("DP-1")),
                ("description", Value::text("Mock DP-1")),
                ("scale", Value::float(s.scale)),
                ("width", Value::float(f64::from(s.width))),
                ("height", Value::float(f64::from(s.height))),
                ("focused", Value::Bool(true)),
            ],
        );
        let run = |e: strand_core::Error| Error::Run(format!("screens: {e}"));
        self.host
            .set(&self.rt, "screens.all", Value::list(vec![screen.clone()]))
            .map_err(run)?;
        self.host
            .set(&self.rt, "screens.focused", screen)
            .map_err(run)
    }

    /// A service field set from text, read by its schema type.
    fn set_service(&self, path: &str, text: &str) -> Result<(), String> {
        let parts: Vec<&str> = path.split('.').collect();
        let [svc, field, rest @ ..] = parts.as_slice() else {
            return Err("a service field is `service.field`".into());
        };
        let prog = &self.inst.vm().prog;
        let rid = self
            .schema
            .service(svc)
            .ok_or_else(|| format!("no service `{svc}`"))?;
        let ty = prog
            .types
            .record(rid)
            .fields
            .iter()
            .find(|f| f.name == *field)
            .map(|f| f.ty.clone())
            .ok_or_else(|| format!("`{svc}` has no field `{field}`"))?;
        let ty = field_ty(prog, ty, rest).ok_or_else(|| format!("no field at `{path}`"))?;
        let v = parse_text(&prog.types, &ty, text.trim())
            .ok_or_else(|| format!("`{text}` is not a {}", prog.types.show(&ty)))?;
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
        if self.schema.service(first).is_some() {
            return self.set_service(path, text);
        }
        let exported = self.inst.set_text(path, text);
        let Err(e) = exported else { return Ok(()) };
        let (d, module, rest) = self.state_def(path).ok_or_else(|| e.to_string())?;
        let prog = &self.inst.vm().prog;
        let fields: Vec<&str> = rest.split('.').skip(1).collect();
        let ty = field_ty(prog, prog.defs[d].ty.clone(), &fields)
            .ok_or_else(|| format!("no field at `{path}`"))?;
        let v = parse_text(&prog.types, &ty, text.trim())
            .ok_or_else(|| format!("`{text}` is not a {}", prog.types.show(&ty)))?;
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
            .schema
            .service(svc)
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
            values.push(
                parse_text(&prog.types, &p.ty, a.trim())
                    .ok_or_else(|| format!("`{a}` is not a {}", prog.types.show(&p.ty)))?,
            );
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
        for e in &u.errors {
            self.notes.push(format!("runtime error: {}", e.message()));
        }
        let mut diff = u.diff;
        self.carry_reduced(&mut diff);
        if diff.is_empty() {
            return Ok(());
        }
        self.apply(diff)
    }

    /// Logic's diff through the host loop as the shell's main thread
    /// takes it (`host_loop::apply`: the router observes it, render
    /// applies it, routing settles, the input wake follows, surfaces
    /// sync), then the loop's turn.
    fn apply(&mut self, diff: SceneDiff) -> Result<(), Error> {
        self.driven |= !diff.ops.is_empty();
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
                if matches!(kind, K::Bar | K::Panel | K::Osd | K::Popup | K::Lock) {
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
        for op in &diff.ops {
            match op {
                strand_scene::SceneOp::Remove { id, .. } => {
                    let tree = self.r.tree();
                    closes.extend(tree.surface_nodes().filter(|s| tree.is_ancestor(*id, *s)));
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
        host_loop::apply(self, diff)?;
        self.turn()
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
                    });
                }
                Change::Placed(_) => {}
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
            let (u, _) = self.inst.step(d, wall(d));
            self.update(u)?;
        }
        let (u, _) = self.inst.step(t, wall(t));
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
            host_loop::input(self, &e, &mut idle)?;
            self.turn()?;
        }
        Ok(())
    }

    /// The surface a step's `on` names, else the pointer's, else the
    /// first shown.
    fn surface_for(&mut self, on: &Option<Selector>, line: u32) -> Result<SurfaceId, Error> {
        if let Some(sel) = on {
            let nodes = self.resolve(sel, line)?;
            for (n, _) in &nodes {
                let root = self.r.tree().root_of(*n).unwrap_or(*n);
                // The innermost surface: the node itself, or its root.
                for cand in [*n, root] {
                    if let Some(s) = self.surfaces.shown.iter().find(|s| s.node == cand) {
                        return Ok(s.id);
                    }
                }
            }
            return Err(self.err(line, format!("`{sel}` is on no shown surface")));
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
            .first()
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

    /// A step begun: an instant step done, a time step's plan returned
    /// for [`Harness::advance`] to run tick by tick.
    fn begin(&mut self, step: &Step) -> Result<Option<TimeRun>, Error> {
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
            StepKind::Drop { n, surface } => {
                self.event(step);
                match surface {
                    None => self.drop_all = *n,
                    Some(sel) => {
                        let s = self.surface_for(&Some(sel.clone()), line)?;
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
                self.surfaces.screen.scale = *f;
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
    }

    /// One tick (see the module docs).
    fn tick(&mut self, input: Vec<Input>) -> Result<(), Error> {
        let k = self.clock.k();
        let t = self.clock.now();
        let now = self.instant(t);
        self.router.set_now(Some(now));
        self.r.set_now(Some(now));
        for i in input {
            let ev = match i {
                Input::Move(s, at) => self.pointer.move_to(s, at, ms(t)),
                Input::Button(b, state) => {
                    self.pointer.button(b, state, ms(t)).into_iter().collect()
                }
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
        let facts = self.r.take_layout_facts();
        if facts.is_empty() {
            return Ok(false);
        }
        for (n, b) in facts {
            self.inst.set_box(n, b);
        }
        let seq = self.r.layout_seq();
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
                            dev.attach(&mut self.r, sh.id, sh.size, self.surfaces.scale()?);
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
        let ids: Vec<SurfaceId> = self
            .surfaces
            .shown
            .iter()
            .filter(|s| s.fresh || self.r.wants_frame(s.id))
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
        let scale = match self.surfaces.scale() {
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
        let scale = self.surfaces.scale()?;
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
        let scale = self.surfaces.scale()?;
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

    /// The tick's frame traces, held texts and commands, into the trace.
    fn collect(&mut self, k: u32, t: Duration) -> Result<(), Error> {
        let traces = self.r.take_frame_traces();
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
            self.trace.commands.push(CommandRec {
                k,
                text: format!("exec {}: {:?}", x.action, x.argv),
            });
        }
        let committed = std::mem::take(&mut self.committed);
        if traces.is_empty() {
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
        // own springs and glides, a drag's lift included).
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
                flight[0] + p.spring[0] + p.glide[0],
                flight[1] + p.spring[1] + p.glide[1],
            ];
            eff.insert(n.node, (op, auth, emot, carries));
            let key = self
                .labels
                .key(n.node, &self.inst, self.r.tree(), &self.sources);
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
                    self.surfaces.screen.scale,
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
        self.trace.seal();
        Ok(RunOutput {
            trace: self.trace,
            images: self.images,
            notes: self.notes,
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

    fn facts(&mut self) -> Result<(), Error> {
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
                match c {
                    Change::Attached(id, node) => host_loop::attached(self, id, node, bounds)?,
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
fn wall(t: Duration) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(MOCK_TIME) + t
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

//! The driver (docs/eval.md, "Driver"): one scenario run in one process,
//! the logic VM, the input router and the renderer on a virtual clock, as
//! the runtime wires them (crates/strand/src/demo/mod.rs `apply`,
//! `text_ready` and `sync`; crates/strand-render/tests/morph.rs `Shell`).
//!
//! Each tick `k` at time `t(k)`:
//! 1. the instant steps before it have acted (state, input, environment);
//! 2. logic runs its timers due by `t` and steps to `t`; a diff that is
//!    not empty goes to the router, then to render (whose `apply` runs
//!    `Renderer::update`), routing settles on it and surfaces follow, as
//!    the shell's logic thread sends only diffs with something in them;
//! 3. the loop wakes between frames only for what wakes the shell's
//!    (crates/strand/src/demo/mod.rs `text_ready`): texts released `via
//!    waker`, or the renderer's own due time ([`Renderer::next_wake`]: a
//!    hover popup's delay or grace, a tooltip's delay, a capped clock's
//!    tick, on the virtual clock). A wake ticks the router and runs
//!    `Renderer::update`; so do surface changes a paint or input leaves,
//!    input that wakes an idle surface (`Host::painted`, `Host::input`),
//!    and text shaped inline (a text worker wakes the loop for each
//!    layout it delivers, `Renderer::take_inline_shaped`);
//! 4. a frame held for logic's answer to its layout facts (a container
//!    query on an idle surface, the renderer's `query_hold`) gets it;
//! 5. texts released `via paint` are delivered, then every surface that
//!    wants a frame (or was just configured) paints at `t`, once;
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
use strand_render::probe::{Axes, FrameTrace, Hidden, NodeTrace, TextDrawn};
use strand_render::{Renderer, Router, TextBackend};
use strand_scene::{
    ButtonState, LogicalPoint, NodeId, Painter, Prop, PropValue, SceneDiff, SurfaceId,
};
use strand_text::{FontConfig, TextEngine};

use crate::Error;
use crate::clock::{Clock, T0};
use crate::gpu::Device;
use crate::input::{Pointer, to_logic};
use crate::labels::Labels;
use crate::scenario::{BackendMode, Config, Fonts, Scenario, Screen, TextMode};
use crate::select::Selector;
use crate::steps::{Button, ExpectKind, Intention, Point, Step, StepKind, Via};
use crate::surfaces::{Change, SCREEN_ID, Surfaces, to_rgba};
use crate::text::TextRules;
use crate::trace::{
    BlurRec, BuildInfo, CommandRec, Cost, EventRec, ExpectRec, Frame, HeldRec, IntendRec, PoseRec,
    Regions, RestRec, Row, RunTrace, ScenarioInfo, Segment, SurfaceInfo, q,
};

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
    let mut h = Harness::boot(sc, opts)?;
    for step in &sc.steps {
        h.step(step)?;
    }
    h.finish()
}

/// A logic wake later than this counts as nothing to wait for.
const REST_HORIZON: Duration = Duration::from_secs(10);
/// Rounds of layout facts a held frame waits on within one tick.
const FACT_ROUNDS: usize = 8;
/// Diffs applied inside one another (logic answering routing's settle,
/// which settles again) before the run is called stuck.
const APPLY_DEPTH: u32 = 32;
/// Wakes in a row for text shaped inline before the run is called stuck.
const WAKE_ROUNDS: usize = 16;
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

/// The compile errors, one per line, at `file:line`.
fn diagnostics(sc: &Scenario, map: &SourceMap, diags: &[strand_compiler::Diagnostic]) -> String {
    let mut out = Vec::new();
    for d in diags.iter().filter(|d| d.is_error()) {
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

/// The fonts text is shaped with.
fn fonts(sc: &Scenario) -> Result<FontConfig, Error> {
    let read = |p: std::path::PathBuf| {
        std::fs::read(&p)
            .map(Arc::new)
            .map_err(|e| Error::Io(p.display().to_string(), e))
    };
    let list = match sc.fonts {
        Fonts::Bundled => vec![
            read(strand_text::test_font_path())?,
            read(strand_text::test_bold_font_path())?,
        ],
        Fonts::Local => {
            let dir = std::env::var("STRAND_EVAL_FONTS").map_err(|_| {
                Error::Config("fonts = \"local\" reads $STRAND_EVAL_FONTS, which is not set".into())
            })?;
            let mut paths: Vec<_> = std::fs::read_dir(&dir)
                .map_err(|e| Error::Io(dir.clone(), e))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension()
                        .is_some_and(|x| x == "ttf" || x == "otf" || x == "ttc")
                })
                .collect();
            paths.sort();
            paths.into_iter().map(read).collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(FontConfig::isolated(list))
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
struct Harness<'s> {
    sc: &'s Scenario,
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
    /// Ticks left without a paint: every surface's, one surface's.
    drop_all: u32,
    drop_one: HashMap<SurfaceId, u32>,
    /// Motion is reduced now (render's last `reduced_motion`).
    reduced: bool,
    /// Motion is reduced whatever the run sets (the twin, or the
    /// scenario's own `reduce_motion`).
    forced_reduced: bool,
    /// Diffs being applied inside one another now.
    depth: u32,
    /// Logic's diff with changes, or input, reached the renderer since
    /// the last frame (`Frame::driven`).
    driven: bool,
    /// The pose each surface painted this tick commits with its frame,
    /// read right after its paint, as the surface manager reads it
    /// (`commit.rs`), before the loop handles the surface changes the
    /// paint left (a closing surface's removal).
    poses: BTreeMap<u32, PoseRec>,
    /// The last tick run.
    last_k: Option<u32>,
    images: BTreeMap<String, Image>,
    notes: Vec<String>,
    /// The GPU leg's device, once started, and whether surfaces are drawn
    /// by it now (the `backend` header or step).
    gpu: Option<Device>,
    on_gpu: bool,
    /// Surfaces attached to the GPU thread.
    gpu_attached: HashSet<SurfaceId>,
}

/// Input a time step sends at the start of a tick.
enum Input {
    Move(SurfaceId, LogicalPoint),
    Button(Button, ButtonState),
}

impl<'s> Harness<'s> {
    fn boot(sc: &'s Scenario, opts: &RunOptions) -> Result<Harness<'s>, Error> {
        let leg = match opts.backend.unwrap_or(sc.backend) {
            BackendMode::Gpu => BackendMode::Gpu,
            BackendMode::Cpu | BackendMode::Both => BackendMode::Cpu,
        };
        if sc.text == TextMode::Worker {
            return Err(Error::Unsupported("text = \"worker\"".into()));
        }
        let map = sources(sc)?;
        let services = strand_services_schema::schemas();
        let schema = Schema::builtin_with(&services)
            .map_err(|(i, e)| Error::Config(format!("service schema {i}: {e:?}")))?;
        let build = Build::compile_with(None, map.clone(), &schema)
            .map_err(|d| Error::Config(diagnostics(sc, &map, &d)))?;
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
        let engine = TextEngine::new(fonts(sc)?);
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
        // The compositor applies surface poses (as Hyprland, with
        // `wp_alpha_modifier_v1` and the viewporter, does): what each
        // frame commits besides its pixels is traced.
        r.set_compositor_poses(sc.compositor_poses);
        let gpu = match leg {
            BackendMode::Gpu => Some(Device::start(&mut r)?),
            _ => None,
        };
        let mut router = Router::new();
        let i0 = Instant::now();
        router.set_now(Some(i0));
        r.set_now(Some(i0));
        let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
        let mut h = Harness {
            sc,
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
            drop_all: 0,
            drop_one: HashMap::new(),
            reduced: false,
            forced_reduced: reduced,
            depth: 0,
            driven: false,
            poses: BTreeMap::new(),
            last_k: None,
            images: BTreeMap::new(),
            notes: Vec::new(),
            on_gpu: gpu.is_some(),
            gpu,
            gpu_attached: HashSet::new(),
        };
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

    fn toggle(&self, path: &str) -> Result<(), String> {
        let v = match self.inst.get(path) {
            Ok(v) => v,
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
                v
            }
        };
        match v {
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

    fn err(&self, line: u32, msg: String) -> Error {
        Error::Scenario {
            file: self.sc.file.display().to_string(),
            line,
            msg,
        }
    }

    /// The router's instant for presentation time `t`.
    fn instant(&self, t: Duration) -> Instant {
        self.i0 + t.saturating_sub(T0)
    }

    /// Applies a logic update: its errors noted, its diff (with
    /// `system.reduced_motion` when it changed) to the router and render
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

    /// A diff to the router and render, then what the host does after
    /// it (crates/strand/src/demo/mod.rs `apply`): routing settles on the
    /// new scene (its intents to logic), the renderer's input wake
    /// follows the router's, surfaces sync.
    fn apply(&mut self, diff: SceneDiff) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > APPLY_DEPTH {
            self.depth = 0;
            return Err(Error::Run(
                "logic and routing never settle (diffs applied inside one another)".into(),
            ));
        }
        let out = self.apply_inner(diff);
        self.depth -= 1;
        out
    }

    fn apply_inner(&mut self, diff: SceneDiff) -> Result<(), Error> {
        self.driven |= !diff.ops.is_empty();
        self.router.observe(&diff);
        let errors = self.r.apply(diff);
        if !errors.is_empty() {
            return Err(Error::Run(format!(
                "render refused logic's diff: {errors:?}"
            )));
        }
        let intents = self.router.settle(&mut self.r);
        self.route(intents)?;
        self.r.set_input_wake(self.router.next_wake());
        self.sync()?;
        if self.r.take_inline_shaped() > 0 {
            self.wake()?;
        }
        Ok(())
    }

    /// The loop woken between frames (crates/strand/src/demo/mod.rs
    /// `text_ready`): the router ticks (a hover popup's delay or grace),
    /// its intents reach logic, the renderer's input wake follows, and
    /// `Renderer::update` runs, then surfaces sync. Text shaped inline
    /// meanwhile is text a worker would wake the loop for again.
    fn wake(&mut self) -> Result<(), Error> {
        for _ in 0..WAKE_ROUNDS {
            let intents = self.router.tick(&mut self.r);
            self.route(intents)?;
            self.r.set_input_wake(self.router.next_wake());
            self.r.update();
            self.sync()?;
            if self.r.take_inline_shaped() == 0 {
                return Ok(());
            }
        }
        Err(Error::Run(format!(
            "text is shaped again on each of {WAKE_ROUNDS} wakes in a row"
        )))
    }

    /// Surface changes to the surfaces, noted in the trace.
    fn sync(&mut self) -> Result<(), Error> {
        let k = self.clock.k();
        let changes = self.surfaces.sync(&mut self.r, &mut self.router, k)?;
        self.changed(changes)
    }

    /// Surface changes noted in the trace, and handed to the GPU leg.
    fn changed(&mut self, changes: Vec<Change>) -> Result<(), Error> {
        let k = self.clock.k();
        let mut detached = false;
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
                    self.trace.surfaces.push(SurfaceInfo {
                        id: id.0,
                        label,
                        kind: kind.to_string(),
                        root,
                        attached: k,
                        detached: None,
                    });
                }
                Change::Detached(id) => {
                    detached = true;
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
        // The pointer gone with a surface may start a hover popup's
        // grace (`Host::surface_detached` ticks the router).
        if detached {
            let intents = self.router.tick(&mut self.r);
            self.route(intents)?;
            self.r.set_input_wake(self.router.next_wake());
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

    /// Routing's intents to logic, and logic's answer back.
    fn route(&mut self, intents: Vec<strand_render::Intent>) -> Result<(), Error> {
        if intents.is_empty() {
            return Ok(());
        }
        if to_logic(&self.inst, &self.host, intents, &mut self.notes) {
            let u = self.inst.flush();
            self.update(u)?;
        }
        Ok(())
    }

    /// Input through the router, as `Host::input` sends it: intents to
    /// logic, the pointer to lean and tilt, the renderer's input wake;
    /// surface changes it leaves, or a surface it wakes from idle, wake
    /// the loop.
    fn send(&mut self, events: Vec<strand_scene::InputEvent>) -> Result<(), Error> {
        self.driven |= !events.is_empty();
        for e in events {
            let idle: Vec<SurfaceId> = self
                .surfaces
                .shown
                .iter()
                .map(|s| s.id)
                .filter(|s| !self.r.wants_frame(*s))
                .collect();
            let intents = self.router.handle(&e, &mut self.r);
            self.route(intents)?;
            let s = e.surface();
            self.r.set_pointer(s, self.router.pointer(s));
            self.r.set_input_wake(self.router.next_wake());
            if self.r.has_surface_changes() || idle.iter().any(|s| self.r.wants_frame(*s)) {
                self.wake()?;
            }
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
            return Ok(found);
        }
        // A node no longer mounted (`expect hidden` after it left), by
        // what was seen of it.
        let seen: Vec<(NodeId, String)> = self
            .labels
            .seen_matching(sel)
            .into_iter()
            .map(|(n, k)| (n, k.to_string()))
            .collect();
        if seen.is_empty() {
            return Err(self.err(
                line,
                format!("`{sel}` matches no node mounted now or seen before"),
            ));
        }
        Ok(seen)
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

    fn close(&mut self, settled: bool) {
        if let Some(i) = self.open.take() {
            let seg = &mut self.trace.segments[i];
            seg.e = self.last_k.unwrap_or(0);
            seg.settled = settled;
            seg.hz = self.clock.hz();
        }
    }

    fn step(&mut self, step: &Step) -> Result<(), Error> {
        // Instant steps act at the tick about to run.
        let t = self.clock.now();
        self.router.set_now(Some(self.instant(t)));
        if step.kind.opens_segment() {
            self.opens(step.text.clone(), step.line);
        }
        let line = step.line;
        let refused = |h: &Self, m: String| h.err(line, m);
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
            StepKind::Scroll { dy } => {
                self.event(step);
                let ev = self
                    .pointer
                    .scroll(*dy, ms(t))
                    .ok_or_else(|| self.err(line, "the pointer is on no surface yet".into()))?;
                self.send(vec![ev])?;
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
                self.surfaces.screen.scale = *f;
                self.surfaces.scale()?;
                self.set_screens()?;
                let changes = self.surfaces.rescale(&mut self.r)?;
                self.changed(changes)?;
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
                self.close(false);
                self.opens(label.clone(), line);
                self.event(step);
            }
            StepKind::Shot(_) | StepKind::Log(_) => self.event(step),
            StepKind::Intend { sel, what, axes } => {
                self.event(step);
                let keys = self
                    .resolve(sel, line)?
                    .into_iter()
                    .map(|(_, k)| k)
                    .collect();
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
            StepKind::Frames(n) => {
                self.event(step);
                for _ in 0..*n {
                    self.tick(Vec::new())?;
                }
            }
            StepKind::Wait(ms_) => {
                self.event(step);
                for _ in 0..self.clock.ticks_for(*ms_) {
                    self.tick(Vec::new())?;
                }
            }
            StepKind::Rest { max } => {
                self.event(step);
                let k = self.clock.k();
                let mut ticks = 0;
                let mut settled = false;
                while ticks < *max {
                    self.tick(Vec::new())?;
                    ticks += 1;
                    if self.at_rest() {
                        settled = true;
                        break;
                    }
                }
                self.trace.rests.push(RestRec {
                    line,
                    k,
                    ticks,
                    max: *max,
                    settled,
                });
                self.close(settled);
            }
            StepKind::Glide { from, to, ms, on } | StepKind::Drag { from, to, ms, on } => {
                self.event(step);
                let drag = matches!(step.kind, StepKind::Drag { .. });
                let s = self.surface_for(on, line)?;
                let n = self.clock.ticks_for(*ms);
                for i in 0..n {
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
                        input.push(Input::Move(s, lp(*from)));
                    }
                    if i == 0 && drag {
                        input.push(Input::Button(Button::Left, ButtonState::Pressed));
                    }
                    input.push(Input::Move(s, lp(at)));
                    if i + 1 == n && drag {
                        input.push(Input::Button(Button::Left, ButtonState::Released));
                    }
                    self.tick(input)?;
                }
            }
        }
        Ok(())
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
        self.last_k = Some(k);
        if let Some(i) = self.open {
            self.trace.segments[i].e = k;
        }
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
        for id in ids {
            match self.gpu.as_mut() {
                Some(dev) if self.gpu_attached.contains(&id) => {
                    self.surfaces.paint_gpu(&mut self.r, dev, id, t)?;
                    dev.changes(&mut self.r);
                }
                _ => {
                    self.surfaces.paint(&mut self.r, id, t)?;
                }
            }
            match self.r.delegated_pose(id) {
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
        }
        // `Host::painted`: surface changes wake the loop, and so does
        // text the frame shaped (a worker's layout arrives with a wake).
        if self.r.take_inline_shaped() > 0 || self.r.has_surface_changes() {
            self.wake()?;
        }
        Ok(())
    }

    /// The tick's frame traces, held texts and commands, into the trace.
    fn collect(&mut self, k: u32, t: Duration) -> Result<(), Error> {
        let traces = self.r.take_frame_traces();
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
                .map(|s| (s.id.0, s.logical))
                .collect(),
            fresh: last.iter().map(|(s, f)| (*s, f.fresh)).collect(),
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
                .map(|s| (s.id.0, s.origin))
                .collect(),
            poses: std::mem::take(&mut self.poses)
                .into_iter()
                .filter(|(s, _)| last.contains_key(s))
                .collect(),
            ..Frame::default()
        };
        for (s, f) in &last {
            frame.n.extend(self.rows(*s, &f.nodes));
            frame.regions.insert(*s, regions(&self.r, SurfaceId(*s)));
            if self.opts.images
                && cost.get(s).is_some_and(|c| c.damage_px > 0)
                && let Some(sh) = self.surfaces.get(SurfaceId(*s))
            {
                let rgba = to_rgba(&sh.pixels);
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
                frame.png.insert(*s, name);
            }
        }
        frame.cost = cost;
        self.trace.frames.push(frame);
        Ok(())
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
            });
        }
        out
    }

    fn finish(mut self) -> Result<RunOutput, Error> {
        self.close(false);
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
        self.trace.nodes = std::mem::take(&mut self.labels.metas);
        self.trace.seal();
        Ok(RunOutput {
            trace: self.trace,
            images: self.images,
            notes: self.notes,
        })
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

/// Logic's wall clock at presentation time `t`.
fn wall(t: Duration) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(MOCK_TIME) + t
}

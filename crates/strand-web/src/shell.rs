//! A config running on a virtual clock (docs/wasm-spike.md): the logic VM
//! with the mock services, the input router and the CPU renderer, wired
//! through the shell's own host loop (`strand_render::host_loop`), as
//! eval/src/harness.rs wires them, cut down to what a live example needs.
//!
//! What eval's driver has that this does not: its trace and checks, text
//! holds, the GPU and surface legs, windows, several screens, compositor
//! poses, keyboard focus, and the virtual clock for the renderer's and
//! router's own timers (`set_now`, behind strand-render's `eval-hooks`,
//! which no root crate may enable). Those timers (a tooltip's delay, a
//! hover popup's grace) read the real clock here; frames, springs and
//! logic run on the virtual one.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use strand_compiler::SourceMap;
use strand_compiler::diagnostic::{Severity, Style};
use strand_compiler::instantiate::{Instance, NodeFlag, Storage, Update, parse_text};
use strand_compiler::reconcile::Build;
use strand_compiler::schema::Schema;
use strand_compiler::ty::Ty;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::{MOCK_TIME, SchemaHost};
use strand_core::Runtime;
use strand_render::host_loop::{self, Cause, LoopHost, LoopSync};
use strand_render::{Flag, Intent, NodeEvent, Renderer, Router, TextBackend};
use strand_scene::input::button;
use strand_scene::{
    ButtonState, InputEvent, LogicalPoint, LogicalSize, Painter, SceneDiff, SurfaceId,
};
use strand_text::{FontConfig, TextEngine};

use crate::surfaces::{Change, Screen, Surfaces};

/// The first tick's presentation time, eval's `T0`: at time zero every
/// motion would snap.
pub const T0: Duration = Duration::from_secs(1);

/// Turns in a row with a wake or logic's answer still owed before the
/// run is called stuck (eval's `TURN_ROUNDS`).
const TURN_ROUNDS: usize = 64;
/// Logic timers run within one tick before the run is called stuck.
const TIMER_ROUNDS: usize = 10_000;
/// A frame held for layout facts: answers before the run is stuck.
const FACT_ROUNDS: usize = 8;
/// Nothing due within this is at rest (eval's `REST_HORIZON`).
const REST_HORIZON: Duration = Duration::from_secs(10);

/// Why a config did not load: the compiler's diagnostics, rendered as
/// `strand check` renders them, or another error.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadError {
    pub text: String,
    /// Each error as `file:line:col: message`.
    pub short: Vec<String>,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn plain(text: impl Into<String>) -> LoadError {
    let text = text.into();
    LoadError {
        short: vec![text.clone()],
        text,
    }
}

/// What a shell is made from.
#[derive(Debug, Clone)]
pub struct Options {
    pub screen: Screen,
    /// Frames per second of the virtual clock.
    pub hz: u32,
    /// Font files text is shaped with, and nothing else (no system
    /// fonts on the web).
    pub fonts: Vec<Arc<Vec<u8>>>,
}

/// A config running.
pub struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    schema: Schema,
    r: Renderer,
    router: Router,
    surfaces: Surfaces,
    hz: u32,
    /// The tick about to run, and its presentation time.
    k: u32,
    t: Duration,
    /// The wall time `T0` stands for, less `T0`.
    wall0: SystemTime,
    reduced: bool,
    wake_owed: bool,
    logic_owed: bool,
    turning: bool,
    facts_seq: Option<u64>,
    pointer: Option<SurfaceId>,
    pointer_at: LogicalPoint,
    /// Frames painted so far, every surface's: a page draws again only
    /// when it grew.
    painted: u64,
    /// What went wrong without stopping the run (runtime errors, input
    /// logic refused), newest last.
    pub notes: Vec<String>,
    /// The compiler's warnings, rendered.
    pub warnings: String,
}

impl std::fmt::Debug for Shell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("k", &self.k)
            .field("surfaces", &self.surfaces.shown.len())
            .finish_non_exhaustive()
    }
}

impl Shell {
    /// Compiles `files` (name, text), sets `services` (`battery.percent`,
    /// read by its schema type, JSON for a record or a list) and boots
    /// the shell to its first tick.
    pub fn load(
        files: &[(String, String)],
        services: &[(String, String)],
        opts: Options,
    ) -> Result<Shell, LoadError> {
        let mut map = SourceMap::new();
        for (name, text) in files {
            map.add(name.clone(), text.clone());
        }
        if map.is_empty() {
            return Err(plain("the config has no `.strand` file"));
        }
        let svc = strand_services_schema::schemas();
        let schema = Schema::builtin_with(&svc)
            .map_err(|(i, e)| plain(format!("service schema {i}: {e:?}")))?;
        let build = match Build::compile_with(None, map.clone(), &schema) {
            Ok(b) => b,
            Err(d) => {
                let errors: Vec<_> = d.into_iter().filter(|d| d.is_error()).collect();
                let text = strand_compiler::diagnostic::render(&errors, &map, Style::Plain);
                let short = strand_compiler::diagnostic::render_short(&errors, &map)
                    .lines()
                    .map(str::to_string)
                    .collect();
                return Err(LoadError { text, short });
            }
        };
        let shown: Vec<_> = build
            .warnings
            .iter()
            .filter(|d| d.severity == Severity::Warning && d.code != "check::raw_color")
            .cloned()
            .collect();
        let warnings = if shown.is_empty() {
            String::new()
        } else {
            strand_compiler::diagnostic::render_short(&shown, &map)
        };
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
        let engine = TextEngine::new(FontConfig::isolated(opts.fonts));
        let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
        // No frame waits for a wall-clock hold: the virtual clock paints
        // every frame it asks for (as eval's driver sets them).
        r.set_first_frame_wait(Duration::ZERO);
        r.set_new_text_wait(Duration::ZERO);
        r.set_estimate_wait(Duration::ZERO);
        r.set_busy_window(Duration::ZERO);
        r.set_resize_wait(Duration::ZERO);
        r.set_query_wait(Duration::from_secs(30));
        r.set_exit_stall(Duration::from_secs(3600));
        r.set_compositor_poses(false);
        let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
        let mut s = Shell {
            rt,
            host,
            inst,
            schema,
            r,
            router: Router::new(),
            surfaces: Surfaces::new(opts.screen),
            hz: opts.hz.max(1),
            k: 0,
            t: T0,
            wall0: UNIX_EPOCH + Duration::from_secs(MOCK_TIME) - T0,
            reduced: false,
            wake_owed: false,
            logic_owed: false,
            turning: false,
            facts_seq: None,
            pointer: None,
            pointer_at: LogicalPoint::new(0.0, 0.0),
            painted: 0,
            notes: Vec::new(),
            warnings,
        };
        s.set_screens().map_err(plain)?;
        for (path, text) in services {
            s.set_service(path, text)
                .map_err(|m| plain(format!("service `{path}`: {m}")))?;
        }
        let (u, _) = s.inst.step(T0, s.wall0 + T0);
        s.update(u).map_err(plain)?;
        Ok(s)
    }

    /// The tick about to run.
    pub fn k(&self) -> u32 {
        self.k
    }

    /// Its presentation time, from the clock's start.
    pub fn now(&self) -> Duration {
        self.t
    }

    /// Frames painted so far (every surface's).
    pub fn painted(&self) -> u64 {
        self.painted
    }

    pub fn surfaces(&self) -> &Surfaces {
        &self.surfaces
    }

    /// Kinds of surface the config asked for that this host does not
    /// show (a `window`).
    pub fn unsupported(&self) -> Vec<&'static str> {
        self.surfaces.unsupported.iter().map(|k| k.name()).collect()
    }

    fn period(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.hz))
    }

    fn set_screens(&mut self) -> Result<(), String> {
        let s = self.surfaces.screen;
        let rec = self.host.record(
            "Screen",
            &[
                ("id", Value::text("Mock | DP-1")),
                ("name", Value::text("DP-1")),
                ("make", Value::text("Mock")),
                ("model", Value::text("DP-1")),
                ("description", Value::text("Mock DP-1")),
                ("scale", Value::float(s.scale)),
                ("width", Value::float(f64::from(s.width))),
                ("height", Value::float(f64::from(s.height))),
                ("focused", Value::Bool(true)),
            ],
        );
        let run = |e: strand_core::Error| format!("screens: {e}");
        self.host
            .set(&self.rt, "screens.all", Value::list(vec![rec.clone()]))
            .map_err(run)?;
        self.host.set(&self.rt, "screens.focused", rec).map_err(run)
    }

    /// One tick: logic to its time, the loop's wakes, held frames
    /// answered, every surface that wants a frame painted at it, and the
    /// layout facts back to logic (eval's `Harness::tick` without its
    /// trace).
    pub fn tick(&mut self) -> Result<(), String> {
        let t = self.t;
        self.logic(t)?;
        // A wake between frames: inline-shaped text and the renderer's
        // own due times are collected here (eval wakes for exactly
        // those; without its hooks this host cannot tell, so it always
        // wakes, which only costs a layout pass when nothing changed).
        self.wake()?;
        let mut rounds = 0;
        while self.held_for_facts() && self.answer()? {
            rounds += 1;
            if rounds > FACT_ROUNDS {
                return Err(format!(
                    "a frame stays held for layout facts after {FACT_ROUNDS} answers"
                ));
            }
        }
        self.paint(t)?;
        self.turn()?;
        self.answer()?;
        self.k += 1;
        self.t += self.period();
        Ok(())
    }

    /// Ticks until nothing moves or waits (eval's `rest`), at most
    /// `max`; how many ran.
    pub fn rest(&mut self, max: u32) -> Result<u32, String> {
        let mut n = 0;
        loop {
            self.tick()?;
            n += 1;
            if self.at_rest() || n >= max {
                return Ok(n);
            }
        }
    }

    /// Nothing wants a frame and nothing is due soon.
    pub fn at_rest(&self) -> bool {
        let t = self.t;
        let wants = self
            .surfaces
            .shown
            .iter()
            .any(|s| s.fresh || self.r.wants_frame(s.id));
        !wants
            && self.router.next_wake().is_none()
            && self
                .inst
                .next_deadline()
                .is_none_or(|d| d > t + REST_HORIZON)
            && !self.r.has_surface_changes()
    }

    fn logic(&mut self, t: Duration) -> Result<(), String> {
        let mut rounds = 0;
        while let Some(d) = self.inst.next_deadline()
            && d <= t
        {
            rounds += 1;
            if rounds > TIMER_ROUNDS {
                return Err(format!(
                    "logic's timers never get past {d:?} (a timer of zero?)"
                ));
            }
            let (u, _) = self.inst.step(d, self.wall0 + d);
            self.update(u)?;
        }
        let (u, _) = self.inst.step(t, self.wall0 + t);
        self.update(u)
    }

    fn update(&mut self, u: Update) -> Result<(), String> {
        for e in &u.errors {
            self.notes.push(format!("runtime error: {}", e.message()));
        }
        let mut diff = u.diff;
        if let Some(seq) = self.facts_seq.take() {
            diff.layout_seen = Some(seq);
        }
        self.carry_reduced(&mut diff);
        if diff.is_empty() {
            return Ok(());
        }
        self.apply(diff)
    }

    fn apply(&mut self, mut diff: SceneDiff) -> Result<(), String> {
        let asks = std::mem::take(&mut diff.window_asks);
        if !asks.is_empty() {
            self.notes
                .push("window actions are not shown on the web".into());
        }
        host_loop::apply(self, diff)?;
        self.turn()
    }

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
        }
    }

    fn turn(&mut self) -> Result<(), String> {
        if self.turning {
            return Ok(());
        }
        self.turning = true;
        let out = self.turn_owed();
        self.turning = false;
        out
    }

    fn turn_owed(&mut self) -> Result<(), String> {
        for _ in 0..TURN_ROUNDS {
            if std::mem::take(&mut self.wake_owed) {
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
        Err(format!(
            "the loop never goes quiet: a wake or logic's answer is owed after each of {TURN_ROUNDS} turns"
        ))
    }

    fn wake(&mut self) -> Result<(), String> {
        self.wake_owed = true;
        self.turn()
    }

    fn held_for_facts(&self) -> bool {
        self.surfaces
            .shown
            .iter()
            .any(|s| self.r.frame_deadline(s.id).is_some())
    }

    fn give_facts(&mut self) -> bool {
        let facts = self.r.take_layout_facts();
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

    fn answer(&mut self) -> Result<bool, String> {
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

    fn paint(&mut self, t: Duration) -> Result<(), String> {
        let ids: Vec<SurfaceId> = self
            .surfaces
            .shown
            .iter()
            .filter(|s| s.fresh || self.r.wants_frame(s.id))
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.surfaces.paint(&mut self.r, id, t)?;
            self.painted += 1;
            host_loop::painted(self, id)?;
        }
        self.turn()
    }

    fn surface_change(&mut self, c: Change, bounds: Option<LogicalSize>) -> Result<(), String> {
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
            Change::Detached(id) => {
                if self.pointer == Some(id) {
                    self.pointer = None;
                }
                host_loop::detached(self, id)?;
            }
        }
        Ok(())
    }

    // --- Values and input ------------------------------------------------

    /// `set <path> <text>` as eval's step reads it: a `$token` overlay,
    /// a service field, an exported state, else a file's top-level state
    /// by name. Takes effect at the next tick.
    pub fn set(&mut self, path: &str, text: &str) -> Result<(), String> {
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
        let prog = self.inst.vm().prog.clone();
        let fields: Vec<&str> = rest.split('.').skip(1).collect();
        let ty = field_ty(&prog, prog.defs[d].ty.clone(), &fields)
            .ok_or_else(|| format!("no field at `{path}`"))?;
        let v = self.read_value(&ty, text)?;
        self.inst
            .set_value(&module, &rest, v)
            .map_err(|e| e.to_string())
    }

    /// The value at `path` as text (an exported value or a top-level
    /// state), for the page to show.
    pub fn get(&self, path: &str) -> Result<String, String> {
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
        Ok(strand_compiler::vm::json::to_json_string(
            &self.inst.vm().prog.types,
            &v,
        ))
    }

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

    fn set_service(&self, path: &str, text: &str) -> Result<(), String> {
        let parts: Vec<&str> = path.split('.').collect();
        let [svc, field, rest @ ..] = parts.as_slice() else {
            return Err("a service field is `service.field`".into());
        };
        let prog = &self.inst.vm().prog;
        let rid = self
            .schema
            .service(svc)
            .or_else(|| {
                prog.services
                    .values()
                    .find(|s| s.name == *svc)
                    .map(|s| s.record)
            })
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
        let v = self.read_value(&ty, text)?;
        self.host.set(&self.rt, path, v).map_err(|e| e.to_string())
    }

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

    /// The pointer moved to `at` (screen logical pixels): it leaves the
    /// surface it was on and enters the one under it, if any.
    pub fn pointer_move(&mut self, at: [f32; 2]) -> Result<(), String> {
        let time = self.ms();
        let hit = self.surfaces.hit(at);
        let mut events = Vec::new();
        match hit {
            Some((surface, position)) => {
                if self.pointer != Some(surface) {
                    if let Some(old) = self.pointer {
                        events.push(InputEvent::PointerLeave { surface: old });
                    }
                    events.push(InputEvent::PointerEnter { surface, position });
                }
                events.push(InputEvent::PointerMotion {
                    surface,
                    position,
                    time,
                });
                self.pointer = Some(surface);
                self.pointer_at = position;
            }
            None => {
                if let Some(old) = self.pointer.take() {
                    events.push(InputEvent::PointerLeave { surface: old });
                }
            }
        }
        self.send(events)
    }

    /// The pointer left the screen.
    pub fn pointer_leave(&mut self) -> Result<(), String> {
        match self.pointer.take() {
            Some(surface) => self.send(vec![InputEvent::PointerLeave { surface }]),
            None => Ok(()),
        }
    }

    /// The left button went down (`pressed`) or up where the pointer is.
    pub fn pointer_button(&mut self, pressed: bool) -> Result<(), String> {
        let Some(surface) = self.pointer else {
            return Ok(());
        };
        let ev = InputEvent::PointerButton {
            surface,
            position: self.pointer_at,
            button: button::LEFT,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
            time: self.ms(),
        };
        self.send(vec![ev])
    }

    fn ms(&self) -> u32 {
        u32::try_from(self.t.as_millis()).unwrap_or(u32::MAX)
    }

    fn send(&mut self, events: Vec<InputEvent>) -> Result<(), String> {
        let mut idle = Vec::new();
        for ev in &events {
            host_loop::input(self, ev, &mut idle)?;
            self.turn()?;
        }
        Ok(())
    }
}

/// Routing's intents to logic, as eval's `input::to_logic` hands them;
/// whether logic took any.
fn to_logic(
    inst: &Instance,
    host: &SchemaHost,
    intents: Vec<Intent>,
    notes: &mut Vec<String>,
) -> bool {
    let mut any = false;
    for intent in intents {
        match intent {
            Intent::Flag { node, flag, on } => {
                let flag = match flag {
                    Flag::Hover => NodeFlag::Hover,
                    Flag::Pressed => NodeFlag::Pressed,
                    Flag::Focused => NodeFlag::Focused,
                    Flag::Selected => NodeFlag::Selected,
                };
                inst.set_flag(node, flag, on);
                any = true;
            }
            Intent::Event { node, event } => {
                let (name, args) = match event {
                    NodeEvent::Click => ("click", Vec::new()),
                    NodeEvent::Secondary => ("secondary", Vec::new()),
                    NodeEvent::Middle => ("middle", Vec::new()),
                    NodeEvent::Activate => ("activate", Vec::new()),
                    NodeEvent::Dismiss => ("dismiss", Vec::new()),
                    NodeEvent::Scroll { dy, dx } => {
                        ("scroll", vec![Value::float(dy), Value::float(dx)])
                    }
                    NodeEvent::Key {
                        name,
                        text,
                        modifiers: m,
                    } => (
                        "key",
                        vec![host.record(
                            "Key",
                            &[
                                ("name", Value::text(name.as_str())),
                                ("text", Value::text(text.as_str())),
                                ("ctrl", Value::Bool(m.ctrl)),
                                ("shift", Value::Bool(m.shift)),
                                ("alt", Value::Bool(m.alt)),
                                ("logo", Value::Bool(m.logo)),
                            ],
                        )],
                    ),
                    NodeEvent::Drop { .. } => {
                        notes.push("a drop is not delivered on the web".into());
                        continue;
                    }
                };
                inst.event(node, name, args);
                any = true;
            }
            Intent::Write { node, prop, value } => {
                if let Err(e) = inst.write(node, prop, value) {
                    notes.push(format!("write to {prop}: {e}"));
                }
                any = true;
            }
            Intent::Clipboard(_) => notes.push("the clipboard is not on the web".into()),
        }
    }
    any
}

fn field_ty(prog: &strand_compiler::lower::Program, mut ty: Ty, fields: &[&str]) -> Option<Ty> {
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

impl LoopHost for Shell {
    type Error = String;

    fn parts(&mut self) -> (&mut Renderer, Option<&mut Router>) {
        (&mut self.r, Some(&mut self.router))
    }

    fn shown(&self, out: &mut Vec<SurfaceId>) {
        out.extend(self.surfaces.shown.iter().map(|s| s.id));
    }

    fn applied(&mut self, errors: Vec<strand_render::SceneError>) -> Result<(), String> {
        if errors.is_empty() {
            return Ok(());
        }
        Err(format!("render refused logic's diff: {errors:?}"))
    }

    fn intents(&mut self, _: Cause<'_>, intents: Vec<Intent>) -> Result<(), String> {
        if !intents.is_empty() && to_logic(&self.inst, &self.host, intents, &mut self.notes) {
            self.logic_owed = true;
        }
        Ok(())
    }

    fn facts(&mut self) -> Result<(), String> {
        if self.give_facts() {
            self.logic_owed = true;
        }
        Ok(())
    }

    fn wake(&mut self) -> Result<(), String> {
        self.wake_owed = true;
        Ok(())
    }
}

impl LoopSync for Shell {
    fn sync(&mut self) -> Result<(), String> {
        let screen = self.surfaces.screen;
        let bounds = Some(LogicalSize::new(screen.width as f32, screen.height as f32));
        for (node, change) in self.r.take_surface_changes() {
            for c in self.surfaces.change(node, change)? {
                self.surface_change(c, bounds)?;
            }
        }
        Ok(())
    }
}

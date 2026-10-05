//! Instantiation: a lowered [`Program`] mounted on a `strand-core`
//! runtime, emitting one [`SceneDiff`] per tick.
//!
//! [`Instance::new`] declares every file's `state`s and `let`s, starts
//! the top-level handlers and timers, mounts the surfaces (a `bar` once
//! per monitor of the `screens` service, with `screen` in scope) and
//! builds the token table. Every bound prop is a core memo the emitter
//! watches; `when` blocks are folded into it in source order (later
//! wins). `if`/`match` and `for` are effects that mount and unmount
//! fragments; keyed lists apply `VecDiff`s so items keep their identity
//! and state. Each [`Instance::tick`] runs the core tick and turns what it
//! changed into one diff: the structural ops made during the flush, then
//! the changed props, then a new token table if one changed.
//!
//! Input from the render thread comes back through
//! [`Instance::event`] (`on click` and friends, routed to the innermost
//! element with a handler, passed on by `propagate()`),
//! [`Instance::set_flag`] (`hover`, `pressed`, `focused`, `selected`),
//! [`Instance::set_size`] (layout facts) and [`Instance::write`] (`<->`
//! writes from widgets).

mod chain;
mod convert;
mod edges;
mod emit;
mod mirror;
mod mount;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use strand_core::{Diagnostic, Error, Memo, NodeId as CoreId, Runtime, Scope};
use strand_scene::{
    Color, NodeId, Prop as SceneProp, PropValue, SceneDiff, SceneOp, TokenTable, Transition,
};

pub use convert::{
    BEZIER_DURATION, from_prop, prop_value, prop_value_for, token_entry, transition,
};
pub use emit::PropOut;
pub use mirror::{SceneMirror, show, show_expr};

use crate::hir::{DefId, DefKind};
use crate::lower::{Node, Program};
use crate::source::FileId;
use crate::syntax::Span;
use crate::vm::value::{NodeState, Value};
use crate::vm::{EventCtx, ServiceHost, Vm, VmHooks};
use emit::{Emitter, FragId};

/// A fragment of the mounted tree (see the `emit` module).
pub(crate) struct Frag {
    pub parent: Option<FragId>,
    pub children: Vec<FragId>,
    /// Children taken off the scene but kept (parked bars): not in
    /// `children`, still unmounted with this fragment.
    pub parked: Vec<FragId>,
    pub scene: Option<NodeId>,
    pub scope: Option<Scope>,
}

/// Which node flag render reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeFlag {
    Hover,
    Pressed,
    Focused,
    Selected,
}

/// A runtime error, located: the error overlay's red outline and
/// click-to-`$EDITOR`, and the inspector's provenance, start here.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeError {
    /// What failed: `text.text` (a prop), `every timer in toasts`, `on
    /// click`, `for x in …`, `state dnd`.
    pub what: String,
    /// The error value.
    pub error: Error,
    /// The file and span of the failing operation (the innermost: a prop
    /// failing because a `let` it reads failed points into the `let`),
    /// else of the binding, handler or timer that failed.
    pub file: Option<FileId>,
    pub span: Option<Span>,
    /// The scene node whose prop (or handler) failed.
    pub node: Option<NodeId>,
    /// The component whose instance it happened in.
    pub component: Option<DefId>,
    /// The core scope of that instance: [`Instance::freeze`] suspends it.
    pub scope: Option<CoreId>,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.what, self.error)
    }
}

impl RuntimeError {
    /// The message as text (`what: error`).
    pub fn message(&self) -> String {
        self.to_string()
    }

    /// True if the message contains `s` (tests).
    pub fn contains(&self, s: &str) -> bool {
        self.to_string().contains(s)
    }
}

/// Drops a parked bar by its monitor's key; true if one was parked.
pub(crate) type Forget = dyn Fn(&Runtime, &Value) -> bool;

/// Services a mounted scope reads ([`Ctx::hold`]).
pub(crate) struct Hold {
    pub scope: Option<CoreId>,
    pub services: Arc<std::collections::BTreeSet<String>>,
    /// The scope wants them (mounted, or its surface shown).
    pub want: bool,
    /// Blocked scopes it is under: a hidden surface's content, a parked
    /// bar. Held only while this is 0.
    pub blocks: u32,
    /// Acquired now.
    pub acquired: bool,
}

/// Where a binding, handler or timer comes from, for its errors.
#[derive(Clone, Debug)]
pub(crate) struct Site {
    pub what: Rc<str>,
    pub file: FileId,
    pub span: Span,
    pub node: Option<NodeId>,
    pub component: Option<DefId>,
    pub scope: Option<CoreId>,
}

/// What one tick produced.
#[derive(Debug, Default)]
pub struct Update {
    /// The tick's scene diff (possibly empty).
    pub diff: SceneDiff,
    /// Runtime errors, as values: failing bindings and handlers, with
    /// what failed and where.
    pub errors: Vec<RuntimeError>,
    /// Write-rate throttling, cancelled handlers and other notices.
    pub diagnostics: Vec<Diagnostic>,
    /// Persisted cells kept although their default changed.
    pub notices: Vec<String>,
}

/// The shared state of an instance (held by the closures the runtime
/// runs).
pub(crate) struct Ctx {
    pub vm: Rc<Vm>,
    pub em: RefCell<Emitter>,
    pub storage: Storage,
    /// Live persisted cells by path (`@reset`, the reconciler's
    /// `redeclare`).
    pub persisted: RefCell<Vec<(String, strand_core::Persisted<Value>)>>,
    pub errors: RefCell<Vec<RuntimeError>>,
    /// Bindings, handlers and timers by core id, for locating their
    /// errors (removed when their scope goes).
    pub sites: RefCell<std::collections::HashMap<CoreId, Rc<Site>>>,
    /// Services each mounted scope reads, by token.
    pub holds: RefCell<std::collections::HashMap<u64, Hold>>,
    pub next_hold: Cell<u64>,
    /// Scopes whose holds are let go (a hidden surface's content, a
    /// parked bar), with how many reasons each.
    pub blocked: RefCell<std::collections::HashMap<CoreId, u32>>,
    /// Mounted settings files (the watcher's `reload_settings`).
    pub settings: RefCell<Vec<std::rc::Weak<crate::vm::SettingsSlot>>>,
    /// Per-monitor bar lists: forget a parked monitor's bar by key.
    pub forgetters: RefCell<Vec<std::rc::Weak<Forget>>>,
    pub notices: RefCell<Vec<String>>,
    play_seq: Cell<u32>,
}

impl VmHooks for Ctx {
    fn play(&self, node: &Rc<NodeState>, keyframes: &str) {
        let Some(id) = node.scene.get() else { return };
        let seq = self.play_seq.get().wrapping_add(1);
        self.play_seq.set(seq);
        // The sequence number makes a repeated `play shake` a new value.
        self.em.borrow_mut().force(
            id,
            SceneProp::Play,
            PropValue::List(vec![
                PropValue::Keyword(keyframes.to_string()),
                PropValue::Number(seq as f32),
            ]),
        );
    }

    fn propagate(&self, rt: &Runtime, ctx: &EventCtx) {
        let Some(scene) = ctx.scene else { return };
        if ctx.propagated.replace(true) {
            return;
        }
        let parent = self
            .em
            .borrow()
            .nodes
            .get(&scene)
            .and_then(|e| e.parent.filter(|_| !e.surface));
        if let Some(p) = parent {
            self.route(rt, p, &ctx.event, ctx.args.clone());
        }
    }
}

impl Ctx {
    /// An error at `site`, at the failing op's span if the VM noted one.
    pub(crate) fn located(&self, site: &Site, e: Error) -> RuntimeError {
        let (file, span) = self.vm.fault_of(&e).unwrap_or((site.file, site.span));
        RuntimeError {
            what: site.what.to_string(),
            error: e,
            file: Some(file),
            span: Some(span),
            node: site.node,
            component: site.component,
            scope: site.scope,
        }
    }

    /// An error with no site, located by the VM if it raised it.
    pub(crate) fn unlocated(&self, what: String, e: Error) -> RuntimeError {
        let at = self.vm.fault_of(&e);
        RuntimeError {
            what,
            error: e,
            file: at.map(|a| a.0),
            span: at.map(|a| a.1),
            node: None,
            component: None,
            scope: None,
        }
    }

    /// Remember where core node `id` (a binding, handler or timer) comes
    /// from, until its scope goes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn site(
        self: &Rc<Self>,
        rt: &Runtime,
        id: CoreId,
        what: impl Into<Rc<str>>,
        file: FileId,
        span: Span,
        env: &Rc<crate::vm::Env>,
        node: Option<NodeId>,
    ) -> Rc<Site> {
        let site = Rc::new(Site {
            what: what.into(),
            file,
            span,
            node,
            component: env.component,
            scope: env.fault_scope(),
        });
        self.sites.borrow_mut().insert(id, site.clone());
        let weak = Rc::downgrade(self);
        rt.on_cleanup(move || {
            if let Some(ctx) = weak.upgrade() {
                ctx.sites.borrow_mut().remove(&id);
            }
        });
        site
    }

    /// The module (file stem) of `file`.
    pub(crate) fn module_of(&self, file: FileId) -> String {
        self.vm
            .prog
            .files
            .iter()
            .find(|f| f.file == file)
            .map(|f| f.module.clone())
            .unwrap_or_default()
    }

    /// Deliver `event` to `node` or its nearest ancestor with a handler.
    fn route(&self, rt: &Runtime, node: NodeId, event: &str, args: Vec<Value>) -> bool {
        let mut cur = Some(node);
        while let Some(n) = cur {
            let (queues, state, parent) = {
                let em = self.em.borrow();
                let Some(entry) = em.nodes.get(&n) else {
                    return false;
                };
                (
                    entry.events.get(event).cloned().unwrap_or_default(),
                    entry.state.clone(),
                    // A surface is the top of its own event tree: a click in
                    // a popup does not reach the element it is anchored to.
                    entry.parent.filter(|_| !entry.surface),
                )
            };
            if !queues.is_empty() {
                // One context for every handler of the event on this
                // element: `propagate()` passes it on once.
                let ctx = Rc::new(EventCtx::new(Some(state), Some(n), event, args));
                let mut ok = false;
                for q in queues {
                    ok |= q.emit(rt, ctx.clone()).is_ok();
                }
                return ok;
            }
            cur = parent;
        }
        false
    }

    fn token_table(self: &Rc<Self>, rt: &Runtime) -> Result<TokenTable, Error> {
        let prog = self.vm.prog.clone();
        let types = &prog.types;
        let root = self.vm.root.clone();
        let mut t = TokenTable::default();
        let palette = match prog.use_palette {
            Some(c) => self.vm.eval(rt, c, &root)?,
            None => {
                let dark = self.vm.host.read(rt, "system", "dark")?.truthy();
                let seed = Color::from_hex(DEFAULT_SEED).unwrap_or(Color::BLACK);
                Value::Palette(Rc::new(crate::vm::palette_material(seed, dark)))
            }
        };
        let palette = match palette {
            Value::Async(a) => a.usable().cloned().unwrap_or(Value::Null),
            v => v,
        };
        if let Value::Palette(p) = &palette {
            for (role, c) in &p.roles {
                t.insert(role.as_str(), PropValue::Color(*c));
            }
        }
        // Component token defaults (`$Toast.radius`) before the sets, so
        // a set's `override Toast.radius` replaces them.
        for c in prog.components.values() {
            for e in &c.tokens {
                match self.vm.eval(rt, e.value, &root) {
                    Ok(v) => convert::token_entry(types, &mut t, &e.path, &e.ty, &v),
                    Err(err) => self.error(format!("token `${}`", e.path), err),
                }
            }
        }
        let set = match prog.use_tokens {
            Some(c) => match self.vm.eval(rt, c, &root)? {
                Value::TokenSet(d) => Some(d),
                _ => None,
            },
            None => prog.token_sets.keys().next().copied(),
        };
        let mut chain = Vec::new();
        let mut cur = set;
        while let Some(d) = cur {
            if chain.contains(&d) {
                break;
            }
            chain.push(d);
            cur = prog.token_sets.get(&d).and_then(|s| s.extends);
        }
        for d in chain.into_iter().rev() {
            let Some(s) = prog.token_sets.get(&d) else {
                continue;
            };
            for e in &s.entries {
                match self.vm.eval(rt, e.value, &root) {
                    Ok(v) => convert::token_entry(types, &mut t, &e.path, &e.ty, &v),
                    Err(err) => self.error(format!("token `${}`", e.path), err),
                }
            }
        }
        Ok(t)
    }
}

/// When the host loop should run [`Instance::step`] again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wake {
    /// The logic-clock time of the next timer or sleeping handler.
    pub deadline: Option<Duration>,
    /// The wall-clock time a service needs (the clock's next minute;
    /// every second only while a binding shows seconds).
    pub wall: Option<SystemTime>,
}

impl Wake {
    /// How long to sleep from logic time `now` and wall time `wall_now`;
    /// `None`: until something else wakes the loop (input, a monitor
    /// hook, an IO reply through the runtime's wake hook).
    pub fn sleep_for(&self, now: Duration, wall_now: SystemTime) -> Option<Duration> {
        let logic = self.deadline.map(|d| d.saturating_sub(now));
        let wall = self
            .wall
            .map(|t| t.duration_since(wall_now).unwrap_or_default());
        [logic, wall].into_iter().flatten().min()
    }
}

/// Where an instance keeps what outlives it: `persist` cells (core's
/// [`strand_core::PersistStore`]) and settings files (core's
/// [`strand_core::SettingsStore`], files resolved against the config
/// directory).
#[derive(Clone, Debug, Default)]
pub struct Storage {
    pub persist: Option<strand_core::PersistStore>,
    pub settings: Option<strand_core::SettingsStore>,
    /// The config directory `from "prefs.toml"` is relative to.
    pub config_dir: Option<PathBuf>,
}

impl Storage {
    /// Nothing kept (tests, `strand check`).
    pub fn none() -> Self {
        Self::default()
    }

    /// `$XDG_STATE_HOME/strand/persist` and `…/settings` (one IO thread),
    /// settings files resolved against `config_dir`.
    pub fn from_env(config_dir: impl Into<PathBuf>) -> Result<Self, strand_core::PersistError> {
        let persist = strand_core::PersistStore::from_env()?;
        Ok(Self {
            settings: Some(persist.settings()),
            persist: Some(persist),
            config_dir: Some(config_dir.into()),
        })
    }

    /// Persist cells under `state_dir/persist`, settings overlays under
    /// `state_dir/settings`, settings files resolved against `config_dir`
    /// (tests).
    pub fn in_dirs(state_dir: impl Into<PathBuf>, config_dir: impl Into<PathBuf>) -> Self {
        let persist = strand_core::PersistStore::new(state_dir.into().join("persist"));
        Self {
            settings: Some(persist.settings()),
            persist: Some(persist),
            config_dir: Some(config_dir.into()),
        }
    }

    /// A settings file's path: `~/…` from `$HOME`, absolute as is, else
    /// relative to the config directory.
    pub fn resolve(&self, file: &str) -> Option<PathBuf> {
        if let Some(rest) = file.strip_prefix("~/") {
            return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
        }
        let p = PathBuf::from(file);
        if p.is_absolute() {
            return Some(p);
        }
        self.config_dir.as_ref().map(|d| d.join(p))
    }
}

/// The seed of the palette a config without `use palette` gets: the
/// design's default accent.
pub const DEFAULT_SEED: &str = "#7aa2f7";

/// A program running on a runtime. See the module docs.
pub struct Instance {
    rt: Runtime,
    ctx: Rc<Ctx>,
    tokens: Option<Memo<TokenTable>>,
    root: Option<Scope>,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance").finish_non_exhaustive()
    }
}

impl Instance {
    /// Mount `program` on `rt` with its services and storage
    /// ([`Storage::none`] keeps nothing: `persist` cells start from their
    /// defaults and settings files are not read). The boot diff (token
    /// table first, then every surface) comes with the first
    /// [`Instance::flush`] or [`Instance::tick`].
    pub fn new(
        rt: &Runtime,
        program: Arc<Program>,
        host: Rc<dyn ServiceHost>,
        storage: Storage,
    ) -> Instance {
        for (name, record) in program.services.values() {
            host.declare(rt, name, *record);
        }
        let vm = Vm::new(program, host);
        let ctx = Rc::new(Ctx {
            vm: vm.clone(),
            em: RefCell::new(Emitter::default()),
            storage,
            persisted: RefCell::default(),
            errors: RefCell::default(),
            sites: RefCell::default(),
            forgetters: RefCell::default(),
            settings: RefCell::default(),
            holds: RefCell::default(),
            next_hold: Cell::new(0),
            blocked: RefCell::default(),
            notices: RefCell::default(),
            play_seq: Cell::new(0),
        });
        let weak: std::rc::Weak<Ctx> = Rc::downgrade(&ctx);
        let weak: std::rc::Weak<dyn VmHooks> = weak;
        vm.set_hooks(weak);
        let mut inst = Instance {
            rt: rt.clone(),
            ctx,
            tokens: None,
            root: None,
        };
        inst.boot();
        inst
    }

    fn boot(&mut self) {
        let rt = self.rt.clone();
        let ctx = self.ctx.clone();
        let prog = ctx.vm.prog.clone();
        let root_env = ctx.vm.root.clone();
        let (scope, ()) = rt.scope(|rt| {
            root_env.set_owner(rt.current_owner());
            // Every file's `let`s and `state`s first: other files read
            // them as `file.name`.
            let all: Vec<Node> = prog
                .files
                .iter()
                .flat_map(|f| f.items.iter().cloned())
                .collect();
            ctx.declare(rt, &all, &root_env);
            // What the top level reads, and `screens` for per-monitor bars.
            let mut services: std::collections::BTreeSet<String> = prog
                .files
                .iter()
                .flat_map(|f| f.services.iter().cloned())
                .collect();
            let bars = prog
                .files
                .iter()
                .flat_map(|f| f.items.iter())
                .any(|n| matches!(n, Node::Surface(s) if s.screen.is_some()));
            if bars {
                services.insert("screens".to_string());
            }
            ctx.acquire(rt, &services);
            let root_frag = ctx.em.borrow_mut().new_frag(None, None);
            for f in &prog.files {
                let items = Arc::new(f.items.clone());
                ctx.mount_nodes(rt, &items, &root_env, root_frag, None);
            }
        });
        self.root = Some(scope);
        // The token table: the boot diff starts with it (`Instant`, so the
        // first frame never shows default colours).
        let c = ctx.clone();
        let memo = rt.memo(move |rt| c.token_table(rt));
        rt.set_name(memo.id(), "tokens");
        let chunks: Vec<_> = prog
            .use_palette
            .iter()
            .chain(prog.use_tokens.iter())
            .copied()
            .chain(
                prog.token_sets
                    .values()
                    .flat_map(|s| s.entries.iter().map(|e| e.value)),
            )
            .chain(
                prog.components
                    .values()
                    .flat_map(|c| c.tokens.iter().map(|e| e.value)),
            )
            .collect();
        let system = ctx.vm.host.sources(&rt, "system", Some("dark"));
        ctx.declare_reads(&rt, memo.id(), &chunks, &root_env, &system);
        match memo.get_untracked(&rt) {
            Ok(table) => ctx.em.borrow_mut().ops.insert(
                0,
                SceneOp::SetTokens {
                    table,
                    transition: Transition::Instant,
                },
            ),
            Err(e) => ctx.error("tokens", e),
        }
        if rt.watch(memo.id()).is_ok() {
            self.tokens = Some(memo);
        }
    }

    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }

    pub fn vm(&self) -> &Rc<Vm> {
        &self.ctx.vm
    }

    /// Advance the logic clock to `now` and end the tick.
    pub fn tick(&self, now: Duration) -> Update {
        let tick = self.rt.tick(now);
        self.collect(tick)
    }

    /// End the tick without moving the clock.
    pub fn flush(&self) -> Update {
        let tick = self.rt.flush();
        self.collect(tick)
    }

    fn collect(&self, tick: strand_core::Tick) -> Update {
        let mut new_tokens = None;
        for id in &tick.changed {
            if self.tokens.is_some_and(|t| t.id() == *id) {
                new_tokens = self.tokens.and_then(|t| t.get_untracked(&self.rt).ok());
                continue;
            }
            self.emit_binding(*id);
        }
        let mut ops = std::mem::take(&mut self.ctx.em.borrow_mut().ops);
        if let Some(table) = new_tokens {
            ops.push(SceneOp::SetTokens {
                table,
                transition: Transition::Default,
            });
        }
        let mut errors: Vec<RuntimeError> = self.ctx.errors.borrow_mut().drain(..).collect();
        for (id, e) in tick.errors {
            let site = self.ctx.sites.borrow().get(&id).cloned();
            let err = match site {
                Some(site) => self.ctx.located(&site, e),
                None => self.ctx.unlocated(self.rt.name(id).to_string(), e),
            };
            errors.push(err);
        }
        self.ctx.vm.clear_faults();
        Update {
            diff: SceneDiff { ops },
            errors,
            diagnostics: tick.diagnostics,
            notices: self.ctx.notices.borrow_mut().drain(..).collect(),
        }
    }

    fn emit_binding(&self, id: CoreId) {
        let (memo, scene, prop, transitions, site) = {
            let em = self.ctx.em.borrow();
            let Some(b) = em.bindings.get(&id) else {
                return;
            };
            (
                b.memo,
                b.scene,
                b.prop,
                b.transitions.clone(),
                b.site.clone(),
            )
        };
        match memo.get_untracked(&self.rt) {
            Ok(out) => {
                let t = transitions.get(out.source).cloned().unwrap_or_default();
                self.ctx.em.borrow_mut().set(scene, prop, out.value, t);
            }
            // Errors are values: the prop keeps its last good value.
            Err(e) => {
                let err = self.ctx.located(&site, e);
                self.ctx.errors.borrow_mut().push(err);
            }
        }
    }

    /// One turn of the host loop (the `strand run` logic thread): services
    /// driven by the wall clock are set to `wall` (forward or back),
    /// the logic clock moves to `now` and the tick ends. Returns the
    /// tick's update (its diff goes to render, its errors to the overlay)
    /// and when to come back if nothing else (input, a monitor hook, an IO
    /// wake) happens first.
    pub fn step(&self, now: Duration, wall: SystemTime) -> (Update, Wake) {
        // Always: the wall clock can jump back (a manual set, an NTP
        // step), and a clock reader mounted by this step (a popup opened)
        // must see the time now, not when the clock last had a reader.
        // The clock's signals skip equal values, so this is cheap.
        self.wake(wall);
        let update = self.tick(now);
        let wake = Wake {
            deadline: self.next_deadline(),
            wall: self.next_wake(),
        };
        (update, wake)
    }

    /// When the host loop must run next: the runtime's next deadline
    /// (timers, sleeping handlers) as a logic-clock time.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.rt.next_deadline()
    }

    /// The next wall-clock time services need (the clock's next minute).
    pub fn next_wake(&self) -> Option<SystemTime> {
        self.ctx.vm.host.next_wake(&self.rt)
    }

    /// The wall clock reached `now` (the host loop woke for
    /// [`Instance::next_wake`]).
    pub fn wake(&self, now: SystemTime) {
        self.ctx.vm.host.wake(&self.rt, now);
    }

    /// Deliver `on <event>` to `node`, or to its nearest ancestor that
    /// handles it. Returns whether a handler got it.
    pub fn event(&self, node: NodeId, event: &str, args: Vec<Value>) -> bool {
        self.ctx.route(&self.rt, node, event, args)
    }

    /// Render reports a node flag (`hover` and friends).
    pub fn set_flag(&self, node: NodeId, flag: NodeFlag, on: bool) {
        let state = self
            .ctx
            .em
            .borrow()
            .nodes
            .get(&node)
            .map(|e| e.state.clone());
        if let Some(s) = state {
            let sig = match flag {
                NodeFlag::Hover => s.hover,
                NodeFlag::Pressed => s.pressed,
                NodeFlag::Focused => s.focused,
                NodeFlag::Selected => s.selected,
            };
            let _ = sig.set(&self.rt, on);
        }
    }

    /// Render reports a node's laid-out size (`self.width`).
    pub fn set_size(&self, node: NodeId, width: f32, height: f32) {
        let state = self
            .ctx
            .em
            .borrow()
            .nodes
            .get(&node)
            .map(|e| e.state.clone());
        if let Some(s) = state {
            let _ = s.width.set(&self.rt, Value::float(width as f64));
            let _ = s.height.set(&self.rt, Value::float(height as f64));
        }
    }

    /// A widget wrote `prop` (`value: <-> level`): write it to the bound
    /// place, outside any handler.
    pub fn write(&self, node: NodeId, prop: SceneProp, value: PropValue) -> Result<(), Error> {
        let target = self.ctx.em.borrow().nodes.get(&node).and_then(|e| {
            e.two_way
                .iter()
                .find(|(p, _, _)| *p == prop)
                .map(|(_, tw, env)| (tw.clone(), env.clone()))
        });
        let Some((tw, env)) = target else {
            return Err(Error::failed(format!(
                "`{}` is not bound two-way here",
                prop.name()
            )));
        };
        let vm = self.ctx.vm.clone();
        let v = convert::from_prop(&vm.prog.types, &tw.ty, &value)
            .ok_or_else(|| Error::failed("the written value does not fit the binding"))?;
        let indices = tw
            .indices
            .iter()
            .map(|c| self.rt.untrack(|rt| vm.eval(rt, *c, &env)))
            .collect::<Result<Vec<_>, _>>()?;
        vm.write_place(&self.rt, &tw.place, indices, &env, v)
    }

    /// The value of an exported `file.name` (the CLI's `strand get`), or
    /// of a field inside it (`theme.prefs.compact`).
    pub fn get(&self, path: &str) -> Result<Value, Error> {
        let (d, rest) = self.export(path)?;
        let root = self.ctx.vm.root.clone();
        let mut v = self.rt.untrack(|rt| self.ctx.vm.read_def(rt, d, &root))?;
        for f in rest {
            v = v
                .field(&self.ctx.vm.prog.types, &f)
                .cloned()
                .ok_or_else(|| Error::failed(format!("`{path}` has no field `{f}`")))?;
        }
        Ok(v)
    }

    /// Write an exported `state` or settings file, or a field inside one
    /// (`strand set theme.look mocha`, `strand set theme.prefs.compact
    /// true`): an ordinary write, checked against the declared type.
    pub fn set(&self, path: &str, value: Value) -> Result<(), Error> {
        let (d, rest) = self.export(path)?;
        let prog = self.ctx.vm.prog.clone();
        if prog.def(d).kind == DefKind::Let {
            return Err(Error::failed(format!("`{path}` is a `let`, not state")));
        }
        let mut ty = prog.def(d).ty.clone();
        for f in &rest {
            let field = match ty.non_null() {
                crate::ty::Ty::Record(r) => prog
                    .types
                    .record(*r)
                    .fields
                    .iter()
                    .find(|x| x.name == *f)
                    .map(|x| x.ty.clone()),
                _ => None,
            };
            ty = field.ok_or_else(|| Error::failed(format!("`{path}` has no field `{f}`")))?;
        }
        if !crate::vm::value::fits(&prog.types, &ty, &value) {
            return Err(Error::failed(format!(
                "`{path}` is a {}, not {}",
                prog.types.show(&ty),
                value.show(&prog.types)
            )));
        }
        let place = crate::lower::Place {
            root: crate::lower::PlaceRoot::Def(d),
            segs: rest
                .into_iter()
                .map(crate::lower::PlaceSeg::Field)
                .collect(),
        };
        let root = self.ctx.vm.root.clone();
        self.ctx
            .vm
            .write_place(&self.rt, &place, Vec::new(), &root, value)
    }

    /// The exported declaration a `file.name[.field…]` path starts with,
    /// and the fields after it.
    fn export(&self, path: &str) -> Result<(DefId, Vec<String>), Error> {
        let prog = &self.ctx.vm.prog;
        let parts: Vec<&str> = path.split('.').collect();
        for n in (2..=parts.len()).rev() {
            let head = parts[..n].join(".");
            if let Some((_, d)) = prog.exports().find(|(p, _)| *p == head)
                && matches!(
                    prog.def(d).kind,
                    DefKind::State | DefKind::Let | DefKind::Settings
                )
            {
                return Ok((d, parts[n..].iter().map(|s| s.to_string()).collect()));
            }
        }
        Err(Error::failed(format!("nothing is exported as `{path}`")))
    }

    /// A top-level `state` or `let` of a file by module and name
    /// (exported or not; tests and the inspector).
    pub fn value_of(&self, module: &str, name: &str) -> Result<Value, Error> {
        let d = self.def_of(module, name)?;
        let root = self.ctx.vm.root.clone();
        self.rt.untrack(|rt| self.ctx.vm.read_def(rt, d, &root))
    }

    /// Write a top-level `state` of a file by module and name, or a field
    /// inside it (`set_value("theme", "prefs.compact", true)`; tests and
    /// the inspector): an ordinary write, outside any handler.
    pub fn set_value(&self, module: &str, name: &str, value: Value) -> Result<(), Error> {
        let mut parts = name.split('.');
        let d = self.def_of(module, parts.next().unwrap_or(""))?;
        let place = crate::lower::Place {
            root: crate::lower::PlaceRoot::Def(d),
            segs: parts
                .map(|f| crate::lower::PlaceSeg::Field(f.to_string()))
                .collect(),
        };
        let root = self.ctx.vm.root.clone();
        self.ctx
            .vm
            .write_place(&self.rt, &place, Vec::new(), &root, value)
    }

    fn def_of(&self, module: &str, name: &str) -> Result<crate::hir::DefId, Error> {
        let prog = &self.ctx.vm.prog;
        prog.defs
            .iter()
            .position(|d| d.module == module && d.name == name && d.owner.is_none())
            .map(|i| crate::hir::DefId(i as u32))
            .ok_or_else(|| Error::failed(format!("no `{module}.{name}`")))
    }

    /// The source element a scene node instantiates: its file, its
    /// program-wide `NodeIdx` and its span (the overlay's click to
    /// `$EDITOR`, the inspector's provenance).
    pub fn origin(&self, node: NodeId) -> Option<(FileId, crate::hir::NodeIdx, Span)> {
        let em = self.ctx.em.borrow();
        em.nodes.get(&node).map(|e| (e.file, e.idx, e.span))
    }

    /// Freeze the component instance a runtime error happened in
    /// (design.md: "a runtime fault freezes one component outlined red"):
    /// its effects, timers and handlers stop, its state is kept. Returns
    /// whether something was frozen. [`Instance::thaw`] undoes it (the
    /// fixing reload).
    pub fn freeze(&self, err: &RuntimeError) -> bool {
        match err.scope {
            Some(s) if !self.rt.is_suspended(s) => self.rt.suspend(s).is_ok(),
            _ => false,
        }
    }

    /// Resume a scope [`Instance::freeze`] froze.
    pub fn thaw(&self, err: &RuntimeError) {
        if let Some(s) = err.scope {
            self.rt.resume(s);
        }
    }

    /// The monitor `id` is gone for good (the surface layer's
    /// `monitor_forgotten`, 30 s after an unplug): its parked bars and
    /// their state are dropped. A monitor that comes back before this
    /// gets its bars back as they were. Returns whether a bar was parked
    /// for it.
    pub fn forget_screen(&self, id: &str) -> bool {
        let fs: Vec<_> = self
            .ctx
            .forgetters
            .borrow_mut()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .collect();
        self.ctx
            .forgetters
            .borrow_mut()
            .retain(|f| f.strong_count() > 0);
        let key = Value::text(id);
        let mut any = false;
        for f in fs {
            any |= f(&self.rt, &key);
        }
        any
    }

    /// The settings files the mounted program reads (for the watcher).
    pub fn settings_files(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .ctx
            .settings
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter_map(|s| s.handle.as_ref().map(|h| h.path().to_path_buf()))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The watcher saw `path` change: every mounted handle on it re-reads
    /// it (core's `Settings::reload`: each field checked on its own, a
    /// syntax error keeps the last good values). Returns whether one
    /// was mounted.
    pub fn reload_settings(&self, path: &std::path::Path) -> bool {
        let slots: Vec<_> = self
            .ctx
            .settings
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .collect();
        let mut any = false;
        for s in slots {
            if let Some(h) = &s.handle
                && h.path() == path
            {
                h.reload(&self.rt);
                any = true;
            }
        }
        self.ctx
            .settings
            .borrow_mut()
            .retain(|w| w.strong_count() > 0);
        any
    }

    /// `@reset` and the overlay's `[reset]`: forget the stored value of
    /// the persisted cell at `path` (`toasts.dnd`, `TopBar[<id>].expanded`)
    /// and go back to its default.
    pub fn reset(&self, path: &str) -> Result<(), Error> {
        let cells = self.ctx.persisted.borrow();
        let mut found = false;
        for (p, cell) in cells.iter() {
            if p == path {
                cell.reset(&self.rt)?;
                found = true;
            }
        }
        if found {
            Ok(())
        } else {
            Err(Error::failed(format!("no persisted cell `{path}`")))
        }
    }

    /// Scene nodes with a handler for `event`, oldest first (tests:
    /// "click the first button").
    pub fn nodes_handling(&self, event: &str) -> Vec<NodeId> {
        let em = self.ctx.em.borrow();
        let mut v: Vec<NodeId> = em
            .nodes
            .iter()
            .filter(|(_, e)| e.events.contains_key(event))
            .map(|(id, _)| *id)
            .collect();
        v.sort();
        v
    }

    /// Unmount everything (the instance's scope is disposed: effects,
    /// timers and handler tasks stop, services are released). Dropping
    /// the instance does the same, so a reload that builds a new instance
    /// on the same runtime leaves nothing of the old one running.
    pub fn shutdown(&mut self) {
        if let Some(s) = self.root.take() {
            s.dispose(&self.rt);
        }
        if let Some(t) = self.tokens.take() {
            self.rt.dispose(t.id());
        }
        self.ctx.em.borrow_mut().ops.clear();
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.shutdown();
    }
}

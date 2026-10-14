//! The VM: evaluates a lowered [`Program`]'s bytecode against
//! `strand-core` signals.
//!
//! Bindings and `let`s run inside core memos, so every `state`, service
//! field or node flag a chunk reads is tracked exactly (only the branch a
//! ternary took is a dependency). Handlers run as core tasks: a
//! [`Machine`](exec::Machine) suspends at `await` and resumes where it
//! stopped, and disposing the handler drops it, which cancels it at that
//! `await`. Errors are values: a failing binding is an `Err` stored in its
//! memo, a failing handler an `Err` its task returns.
//!
//! - [`value`]: the dynamic [`Value`].
//! - [`host`]: the [`ServiceHost`] trait services are reached through;
//!   [`schema_host`] is the schema-populated host (the deterministic mock,
//!   and with the real [`clock`] the hello bar's runtime).
//! - [`persist`]: `persist` storage with the default's hash.

pub(crate) mod builtins;
pub mod clock;
mod exec;
pub mod host;
mod load;
pub(crate) use load::DeclareLoad;
pub mod persist;
pub mod schema_host;
pub mod theme;
pub mod value;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use strand_core::{Error, Runtime};

pub use exec::{EventCtx, Frame};
pub use host::{ActionCtx, ActionTarget, InputCtx, ServiceHost};
pub use value::{Num, Slot, Value, ValueKey};

use crate::hir::{DefId, DefKind, LocalId, NodeIdx};
use crate::lower::{ChunkId, Program};
use crate::source::FileId;
use crate::syntax::Span;
use value::NodeState;

/// Deepest nesting of `fn` and lambda calls before a call is an error
/// value (a recursive `fn` without a base case).
pub const MAX_CALL_DEPTH: u32 = 200;

/// What the VM asks of the instantiator while it runs: playing
/// keyframes on an element and passing an event on with `propagate()`.
pub trait VmHooks {
    /// `play name` on the element instance `node`.
    fn play(&self, rt: &Runtime, node: &Rc<NodeState>, keyframes: DefId);
    /// `propagate()` in a handler of `ctx`: deliver the event to the next
    /// ancestor that handles it.
    fn propagate(&self, rt: &Runtime, ctx: &EventCtx);
    /// (M6) The current palette's entry `name` (`mauve`, `name`, `dark`),
    /// tracked: `$palette.<name>` outside a template. `None` when the
    /// palette has no such entry (or there is no palette).
    fn palette(
        &self,
        _rt: &Runtime,
        _name: &str,
    ) -> Result<Option<strand_scene::PropValue>, Error> {
        Ok(None)
    }
}

/// A scope of names: a component, surface or `for` item instance (or the
/// config's top level). Child scopes point at their parent.
pub struct Env {
    pub parent: Option<Rc<Env>>,
    locals: RefCell<Vec<(LocalId, Slot)>>,
    defs: RefCell<Vec<(DefId, Slot)>>,
    /// Elements this scope owns (their `hover`, size and `id:` names).
    owned: Arc<BTreeSet<NodeIdx>>,
    nodes: RefCell<Vec<(NodeIdx, Rc<NodeState>)>>,
    /// For a component instance: the caller's children and scope, which
    /// `slot` mounts.
    pub slot: Option<(Arc<Vec<crate::lower::Node>>, Rc<Env>)>,
    /// The component this scope instantiates, if any.
    pub component: Option<DefId>,
    /// The core scope this one lives as long as: element states created
    /// on first use belong to it, not to whatever computation first read
    /// them.
    owner: Cell<Option<strand_core::NodeId>>,
    /// A `for` item's or per-monitor bar's identity (`[<key>]`), which
    /// qualifies the persist paths of state declared under it.
    instance: RefCell<Option<Rc<str>>>,
    /// Settings files declared here: one signal per field.
    settings: RefCell<Vec<(DefId, Rc<SettingsSlot>)>>,
    /// Where this scope sits in the mounted instance tree, in reload
    /// identities (`/c12/f7[3]`): what the reconciler keys state, scene
    /// nodes and handlers by.
    ident: RefCell<Rc<str>>,
    /// The loads of async service calls made in this scope's bindings
    /// ([`crate::lower::Op::AsyncSite`]), by call chunk.
    sites: RefCell<Vec<(ChunkId, strand_core::Memo<Value>)>>,
}

/// A settings file's fields (`state prefs from "prefs.toml" { … }`):
/// each field is its own signal (core's `Settings` handle when the file
/// is read), so writing `prefs.compact` writes that field only and a
/// binding reading `prefs.accent` does not re-run for it.
pub struct SettingsSlot {
    pub fields: Vec<(String, strand_core::Signal<Value>)>,
    /// The file's handle (reload, overlay, provenance), when the
    /// instance has a settings store.
    pub handle: Option<strand_core::Settings<Value>>,
}

impl std::fmt::Debug for SettingsSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsSlot")
            .field("fields", &self.fields.len())
            .finish_non_exhaustive()
    }
}

impl SettingsSlot {
    pub fn field(&self, name: &str) -> Option<strand_core::Signal<Value>> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, s)| *s)
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("locals", &self.locals.borrow().len())
            .field("defs", &self.defs.borrow().len())
            .finish_non_exhaustive()
    }
}

impl Env {
    /// The config's top-level scope.
    pub fn root() -> Rc<Env> {
        Rc::new(Env {
            parent: None,
            locals: RefCell::default(),
            defs: RefCell::default(),
            owned: Arc::default(),
            nodes: RefCell::default(),
            slot: None,
            component: None,
            owner: Cell::new(None),
            instance: RefCell::new(None),
            settings: RefCell::default(),
            ident: RefCell::new(Rc::from("")),
            sites: RefCell::default(),
        })
    }

    /// A child scope owning `owned`.
    pub fn child(
        parent: &Rc<Env>,
        owned: Arc<BTreeSet<NodeIdx>>,
        slot: Option<(Arc<Vec<crate::lower::Node>>, Rc<Env>)>,
        component: Option<DefId>,
    ) -> Rc<Env> {
        Rc::new(Env {
            parent: Some(parent.clone()),
            locals: RefCell::default(),
            defs: RefCell::default(),
            owned,
            nodes: RefCell::default(),
            slot,
            component: component.or(parent.component),
            owner: Cell::new(None),
            instance: RefCell::new(None),
            settings: RefCell::default(),
            ident: RefCell::new(parent.ident()),
            sites: RefCell::default(),
        })
    }

    /// Where this scope sits in the instance tree (reload identity).
    pub fn ident(&self) -> Rc<str> {
        self.ident.borrow().clone()
    }

    /// Place this scope in the instance tree (reload identity).
    pub fn set_ident(&self, ident: impl Into<Rc<str>>) {
        *self.ident.borrow_mut() = ident.into();
    }

    /// Point this scope's values at `new`'s, by name: a handler kept
    /// across a reload (its code unchanged) still runs the old bytecode
    /// against this scope, and from now on reads and writes the new
    /// program's cells. `old` and `new` name the programs' declarations
    /// and locals.
    pub fn redirect(&self, new: &Env, old: &Program, newp: &Program) {
        let mut defs = self.defs.borrow_mut();
        for (d, slot) in defs.iter_mut() {
            let name = &old.def(*d).name;
            let found = new
                .defs
                .borrow()
                .iter()
                .rev()
                .find(|(nd, _)| newp.def(*nd).name == *name)
                .map(|(_, s)| *s);
            if let Some(s) = found {
                *slot = s;
            }
        }
        let mut locals = self.locals.borrow_mut();
        for (l, slot) in locals.iter_mut() {
            let name = &old.local(*l).name;
            let found = new
                .locals
                .borrow()
                .iter()
                .rev()
                .find(|(nl, _)| newp.local(*nl).name == *name)
                .map(|(_, s)| *s);
            if let Some(s) = found {
                *slot = s;
            }
        }
    }

    /// Mark this scope as one instance among several (`[<key>]`).
    pub fn set_instance(&self, id: impl Into<Rc<str>>) {
        *self.instance.borrow_mut() = Some(id.into());
    }

    /// (M5) This scope's own instance qualifier, if it is one of several
    /// (a `for` item's key, a per-monitor bar's monitor).
    pub fn instance(&self) -> Option<Rc<str>> {
        self.instance.borrow().clone()
    }

    /// The instance qualifiers from the outermost scope in: `[DP-1][3]`.
    pub fn instance_path(&self) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(self);
        while let Some(e) = cur {
            if let Some(i) = &*e.instance.borrow() {
                parts.push(format!("[{i}]"));
            }
            cur = e.parent.as_deref();
        }
        parts.reverse();
        parts.concat()
    }

    pub fn bind_settings(&self, id: DefId, slot: Rc<SettingsSlot>) {
        self.settings.borrow_mut().push((id, slot));
    }

    /// The settings file `id` as declared in this scope chain.
    pub fn settings(&self, id: DefId) -> Option<Rc<SettingsSlot>> {
        let found = self
            .settings
            .borrow()
            .iter()
            .rev()
            .find(|(d, _)| *d == id)
            .map(|(_, s)| s.clone());
        found.or_else(|| self.parent.as_ref().and_then(|p| p.settings(id)))
    }

    /// The live state of element `idx` if one exists in this scope chain
    /// (none is created).
    pub fn existing_node_state(&self, idx: NodeIdx) -> Option<Rc<NodeState>> {
        if let Some((_, n)) = self.nodes.borrow().iter().find(|(i, _)| *i == idx) {
            return Some(n.clone());
        }
        if self.owned.contains(&idx) {
            return None;
        }
        self.parent.as_ref()?.existing_node_state(idx)
    }

    /// The core scope a runtime fault in this scope freezes: the
    /// instance of the innermost component (or the surface instance, or
    /// `for` item at the top level) this scope belongs to. `None` at
    /// the config's top level (a file's `let`s, handlers and timers):
    /// freezing that would freeze the whole program, and design.md says a
    /// fault freezes only its own component, so a top-level fault is
    /// outlined but nothing is frozen.
    pub fn fault_scope(self: &Rc<Self>) -> Option<strand_core::NodeId> {
        self.parent.as_ref()?;
        let mut cur = self.clone();
        loop {
            let Some(p) = cur.parent.clone() else {
                return cur.owner.get();
            };
            if p.parent.is_none() || p.component != cur.component {
                return cur.owner.get();
            }
            cur = p;
        }
    }

    /// (M5) The core scope this scope is tied to, if any.
    pub fn owner(&self) -> Option<strand_core::NodeId> {
        self.owner.get()
    }

    /// Tie this scope to the core scope `owner` (the one being mounted).
    pub fn set_owner(&self, owner: Option<strand_core::NodeId>) {
        self.owner.set(owner);
    }

    pub fn bind_local(&self, id: LocalId, slot: Slot) {
        self.locals.borrow_mut().push((id, slot));
    }

    pub fn bind_def(&self, id: DefId, slot: Slot) {
        self.defs.borrow_mut().push((id, slot));
    }

    pub fn local(&self, id: LocalId) -> Option<Slot> {
        let found = self
            .locals
            .borrow()
            .iter()
            .rev()
            .find(|(l, _)| *l == id)
            .map(|(_, s)| *s);
        found.or_else(|| self.parent.as_ref().and_then(|p| p.local(id)))
    }

    pub fn def(&self, id: DefId) -> Option<Slot> {
        let found = self
            .defs
            .borrow()
            .iter()
            .rev()
            .find(|(d, _)| *d == id)
            .map(|(_, s)| *s);
        found.or_else(|| self.parent.as_ref().and_then(|p| p.def(id)))
    }

    /// The live state of element `idx` in this scope chain, created on
    /// first use in the scope that owns it.
    pub fn node_state(self: &Rc<Self>, rt: &Runtime, idx: NodeIdx) -> Rc<NodeState> {
        let mut cur = Some(self.clone());
        let mut owner = None;
        while let Some(env) = cur {
            if let Some((_, n)) = env.nodes.borrow().iter().find(|(i, _)| *i == idx) {
                return n.clone();
            }
            // The scope that owns the element has the only state for it
            // (a second mount of a component's `slot` owns its own).
            if env.owned.contains(&idx) {
                owner = Some(env.clone());
                break;
            }
            cur = env.parent.clone();
        }
        let owner = owner.unwrap_or_else(|| self.clone());
        let make = |rt: &Runtime| NodeState {
            idx,
            scene: Cell::new(None),
            hover: rt.signal(false),
            pressed: rt.signal(false),
            focused: rt.signal(false),
            selected: rt.signal(false),
            width: rt.signal(Value::float(0.0)),
            height: rt.signal(Value::float(0.0)),
            watch: Cell::new(0),
            laid_out: Cell::new(false),
        };
        // The flags live as long as the scope that owns the element, even
        // when a binding (a memo) is what first asks for them.
        let state = Rc::new(match owner.owner.get() {
            Some(o) => rt.with_owner(o, make).unwrap_or_else(|_| make(rt)),
            None => make(rt),
        });
        owner.nodes.borrow_mut().push((idx, state.clone()));
        state
    }
}

/// The VM: a program, its services and the config's top-level values.
pub struct Vm {
    pub prog: Arc<Program>,
    pub host: Rc<dyn ServiceHost>,
    /// The config's top-level scope (file `state`s and `let`s).
    pub root: Rc<Env>,
    depth: Cell<u32>,
    /// Function values called so far ([`Vm::calls`]).
    calls: Cell<u64>,
    /// The longest frame or closure capture seen ([`Vm::peak_frame`]).
    peak: Cell<usize>,
    hooks: RefCell<Option<std::rc::Weak<dyn VmHooks>>>,
    /// `material(image:)` and file imports (the instance's, kept across
    /// reloads); without one those read nothing.
    theme: RefCell<Option<Rc<theme::ThemeHost>>>,
    /// Where errors raised since the last [`Vm::clear_faults`] came from:
    /// the failing op's file and span.
    faults: RefCell<std::collections::VecDeque<(Error, FileId, Span)>>,
    /// Elements whose laid-out size a binding read since the last
    /// [`Vm::take_watched`]: the instance tells render to report them.
    watched: RefCell<Vec<Rc<NodeState>>>,
    /// (M4) The ops a running `canvas { draw: }` lambda records
    /// ([`Vm::record_canvas`]); `None` outside one, where canvas methods
    /// do nothing.
    canvas: RefCell<Option<Vec<strand_scene::canvas::DrawOp>>>,
    /// (M5) The other mode's slots while an evaluation runs in it
    /// ([`Vm::in_other_mode`]): `system.dark` reads negated and the
    /// `let`s that reach it read from these slots.
    other: RefCell<Option<Rc<OtherEnv>>>,
    /// (M6) Commands declared actions asked to run since the last
    /// [`Vm::take_execs`].
    execs: RefCell<Vec<ExecRequest>>,
}

/// (M6) A command a handler's call of a declared action (`dispatch(3)`)
/// asked to run: the host starts it detached, off the logic thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    /// The action's name, for its notices.
    pub action: String,
    /// The program, then its arguments: each parameter exactly one.
    pub argv: Vec<String>,
}

/// (M5) The other mode's env (decisions.md m5-tools, "a template reads
/// the other mode"): a fresh slot, the same expression, for every
/// top-level `let` that reaches `system.dark`, so the theme evaluated in
/// it sees the negated mode through its `let`s too; every other name
/// keeps the instance's slot. Made and dropped with the other-mode memo.
#[derive(Default)]
pub(crate) struct OtherEnv {
    slots: RefCell<Vec<(DefId, Slot)>>,
}

impl OtherEnv {
    pub(crate) fn bind(&self, d: DefId, slot: Slot) {
        self.slots.borrow_mut().push((d, slot));
    }

    fn slot(&self, d: DefId) -> Option<Slot> {
        self.slots
            .borrow()
            .iter()
            .find(|(x, _)| *x == d)
            .map(|(_, s)| *s)
    }
}

/// Faults remembered between two [`Vm::clear_faults`] (a tick's worth).
const MAX_FAULTS: usize = 4096;

/// The same error value: one `Failed` message by identity (two
/// failures with the same text are two faults), others by equality.
fn same_error(a: &Error, b: &Error) -> bool {
    match (a, b) {
        (Error::Failed(x), Error::Failed(y)) => Arc::ptr_eq(x, y),
        (a, b) => a == b,
    }
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm").finish_non_exhaustive()
    }
}

impl Vm {
    pub fn new(prog: Arc<Program>, host: Rc<dyn ServiceHost>) -> Rc<Vm> {
        Rc::new(Vm {
            prog,
            host,
            root: Env::root(),
            depth: Cell::new(0),
            calls: Cell::new(0),
            peak: Cell::new(0),
            hooks: RefCell::new(None),
            theme: RefCell::new(None),
            faults: RefCell::default(),
            watched: RefCell::default(),
            canvas: RefCell::default(),
            other: RefCell::default(),
            execs: RefCell::default(),
        })
    }

    /// (M6) Asks for declared action `def`'s command with `args` filling
    /// its parameters, each one argument as its text (never split, never
    /// read by a shell). It runs when the host takes it
    /// ([`Vm::take_execs`]).
    pub(crate) fn exec(&self, def: DefId, args: &[Value]) -> Result<(), Error> {
        let Some(action) = self.prog.execs.get(&def) else {
            return Err(Error::failed("no such action"));
        };
        let types = self.types();
        let argv = action
            .argv
            .iter()
            .map(|p| match p {
                crate::hir::ArgvPart::Text(t) => t.clone(),
                crate::hir::ArgvPart::Param(i) => {
                    args.get(*i).map(|v| v.show(types)).unwrap_or_default()
                }
            })
            .collect();
        self.execs.borrow_mut().push(ExecRequest {
            action: action.name.clone(),
            argv,
        });
        Ok(())
    }

    /// (M6) The commands asked for since the last call, in order.
    pub fn take_execs(&self) -> Vec<ExecRequest> {
        std::mem::take(&mut *self.execs.borrow_mut())
    }

    /// (M5) Runs `f` in the other mode: `system.dark` reads negated (the
    /// service read and the built-in palette) and the `let`s `env` holds
    /// read from its slots. Nests and restores, so a slot's own memo,
    /// whenever core runs it, sets it again.
    pub(crate) fn in_other_mode<T>(&self, env: &Rc<OtherEnv>, f: impl FnOnce() -> T) -> T {
        let saved = self.other.replace(Some(env.clone()));
        let out = f();
        *self.other.borrow_mut() = saved;
        out
    }

    /// (M5) Whether an evaluation runs in the other mode.
    pub(crate) fn other_mode_active(&self) -> bool {
        self.other.borrow().is_some()
    }

    /// `service.field`, with `system.dark` negated in the other mode.
    pub(crate) fn read_field(
        &self,
        rt: &Runtime,
        service: &str,
        field: &str,
    ) -> Result<Value, Error> {
        let v = self.host.read(rt, service, field)?;
        if service == "system" && field == "dark" && self.other_mode_active() {
            return Ok(Value::Bool(!v.truthy()));
        }
        Ok(v)
    }

    /// Note where `e` was raised (the innermost chunk wins: a binding
    /// failing because a `let` it read failed points at the `let`).
    pub(crate) fn note_fault(&self, e: &Error, file: FileId, span: Span) {
        let mut f = self.faults.borrow_mut();
        if f.iter().any(|(x, _, _)| same_error(x, e)) {
            return;
        }
        if f.len() >= MAX_FAULTS {
            f.pop_front();
        }
        f.push_back((e.clone(), file, span));
    }

    /// Where `e` was raised, if the VM raised it since the last
    /// [`Vm::clear_faults`].
    pub fn fault_of(&self, e: &Error) -> Option<(FileId, Span)> {
        self.faults
            .borrow()
            .iter()
            .find(|(x, _, _)| same_error(x, e))
            .map(|(_, f, s)| (*f, *s))
    }

    /// Notes that a binding read `node`'s laid-out size (`bits`:
    /// [`NodeState::WATCH_SIZE`] or [`NodeState::WATCH_QUERY`]).
    /// (M4) Runs `f` (a `canvas`'s `draw:` lambda) recording the canvas
    /// methods it calls; returns its result and the ops.
    pub(crate) fn record_canvas<T>(
        &self,
        f: impl FnOnce() -> T,
    ) -> (T, Vec<strand_scene::canvas::DrawOp>) {
        let saved = self.canvas.replace(Some(Vec::new()));
        let out = f();
        let ops = self.canvas.replace(saved).unwrap_or_default();
        (out, ops)
    }

    /// (M4) A canvas method was called: recorded if a `draw:` runs.
    pub(crate) fn canvas_op(&self, op: strand_scene::canvas::DrawOp) {
        if let Some(ops) = self.canvas.borrow_mut().as_mut() {
            ops.push(op);
        }
    }

    pub(crate) fn watch(&self, node: &Rc<NodeState>, bits: u8) {
        let had = node.watch.get();
        node.watch.set(had | bits);
        // Every read is noted, not only the first: a remount or reload
        // may have dropped what render was told.
        self.watched.borrow_mut().push(node.clone());
    }

    /// Elements whose laid-out size was read since the last call.
    pub fn take_watched(&self) -> Vec<Rc<NodeState>> {
        std::mem::take(&mut *self.watched.borrow_mut())
    }

    /// Forget noted faults (the instance does after reporting a tick).
    pub fn clear_faults(&self) {
        self.faults.borrow_mut().clear();
    }

    /// The instantiator's hooks (held weakly: the instantiator owns the
    /// VM).
    pub fn set_hooks(&self, hooks: std::rc::Weak<dyn VmHooks>) {
        *self.hooks.borrow_mut() = Some(hooks);
    }

    /// Give the VM its theme host (`material(image:)`, file imports).
    pub fn set_theme(&self, theme: Rc<theme::ThemeHost>) {
        *self.theme.borrow_mut() = Some(theme);
    }

    /// The theme host, if the VM has one.
    pub fn theme(&self) -> Option<Rc<theme::ThemeHost>> {
        self.theme.borrow().clone()
    }

    pub(crate) fn hooks(&self) -> Option<Rc<dyn VmHooks>> {
        self.hooks
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade)
    }

    pub fn types(&self) -> &crate::ty::TypeTable {
        &self.prog.types
    }

    /// Evaluate `chunk` in `env` (tracked by whatever computation is
    /// running: a memo, an effect or nothing).
    pub fn eval(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: ChunkId,
        env: &Rc<Env>,
    ) -> Result<Value, Error> {
        self.eval_with(rt, chunk, env, Frame::default())
    }

    /// [`Vm::eval`] with frame locals bound (a `for` key's binding).
    pub fn eval_with(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: ChunkId,
        env: &Rc<Env>,
        frame: Frame,
    ) -> Result<Value, Error> {
        let mut m = exec::Machine::new(chunk, env.clone(), frame, None, None);
        match m.run(self, rt)? {
            exec::Exit::Done(v) => Ok(v),
            exec::Exit::Await(_) => Err(Error::failed("`await` outside a handler")),
        }
    }

    /// The receiver and arguments of the method call `chunk` ends with
    /// (`apps.search(query)`), evaluated (tracked) without making the
    /// call: the input of an async service `let`.
    pub fn eval_call_args(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: ChunkId,
        env: &Rc<Env>,
    ) -> Result<(Value, Vec<Value>), Error> {
        let mut m = exec::Machine::new(chunk, env.clone(), Frame::default(), None, None);
        m.call_args(self, rt)
    }

    /// A handler body as a coroutine for `rt.spawn*`: it runs until it
    /// returns, suspending at each `await`. Dropping it cancels it. While
    /// a handler of an input event ([`INPUT_EVENTS`]) runs up to its
    /// first `await`, [`in_input_handler`] is true on this thread; the
    /// stretches after an `await` resume outside the input (the press is
    /// long gone, so its point must not be read).
    pub fn handler(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: ChunkId,
        env: Rc<Env>,
        frame: Frame,
        ctx: Option<Rc<EventCtx>>,
    ) -> impl std::future::Future<Output = Result<(), Error>> + 'static {
        let vm = self.clone();
        let weak = rt.downgrade();
        let mut input = ctx
            .as_ref()
            .is_some_and(|c| INPUT_EVENTS.contains(&c.event.as_str()));
        // (M5) Numbered for its whole run; its press only until the
        // first `await` (docs/architecture.md, "Handler input context").
        let invocation = INVOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut press = ctx.as_ref().and_then(|c| c.input);
        async move {
            let mut m = exec::Machine::new(chunk, env, frame, None, ctx);
            loop {
                let step = {
                    let Some(rt) = weak.upgrade() else {
                        return Ok(());
                    };
                    // Only the first stretch is the input's.
                    let _input = std::mem::take(&mut input).then(InputHandler::enter);
                    let _acting = Acting::enter(ActionCtx {
                        input: press.take(),
                        invocation,
                    });
                    m.run(&vm, &rt)?
                };
                match step {
                    exec::Exit::Done(_) => return Ok(()),
                    exec::Exit::Await(v) => {
                        let r = exec::await_value(v).await?;
                        m.push(r);
                    }
                }
            }
        }
    }

    /// How many function values (lambdas, `fn` values) were called so
    /// far: a `filter` over n items is n calls (tests of incremental
    /// views).
    pub fn calls(&self) -> u64 {
        self.calls.get()
    }

    /// The longest local frame, or closure capture, any chunk has had so
    /// far: a measure of work that does not depend on the clock (a loop
    /// that kept its iterations' locals, or captures that doubled per
    /// nested lambda, would grow it with the input).
    pub fn peak_frame(&self) -> usize {
        self.peak.get()
    }

    pub(crate) fn note_frame(&self, len: usize) {
        if len > self.peak.get() {
            self.peak.set(len);
        }
    }

    /// Call a function value with arguments (lambdas given to `filter`,
    /// `sort_by`, `update`).
    pub fn call(
        self: &Rc<Self>,
        rt: &Runtime,
        f: &Value,
        args: Vec<Value>,
    ) -> Result<Value, Error> {
        let Value::Fn(c) = f else {
            return Err(Error::failed("not a function"));
        };
        let depth = self.depth.get();
        if depth >= MAX_CALL_DEPTH {
            return Err(Error::failed(format!(
                "calls nested more than {MAX_CALL_DEPTH} deep (a `fn` that never stops recursing?)"
            )));
        }
        self.depth.set(depth + 1);
        self.calls.set(self.calls.get() + 1);
        let mut frame: Frame = c.captured.clone();
        for (p, a) in c.params.iter().zip(args) {
            frame.push((*p, a));
        }
        self.note_frame(frame.len());
        let mut m = exec::Machine::new(c.chunk, c.env.clone(), frame, Some(c.clone()), None);
        let r = stacker::maybe_grow(64 * 1024, 1024 * 1024, || m.run(self, rt));
        self.depth.set(depth);
        match r? {
            exec::Exit::Done(v) => Ok(v),
            exec::Exit::Await(_) => Err(Error::failed("`await` outside a handler")),
        }
    }

    /// The value of a declaration read in `env`.
    pub fn read_def(
        self: &Rc<Self>,
        rt: &Runtime,
        d: DefId,
        env: &Rc<Env>,
    ) -> Result<Value, Error> {
        let info = self.prog.def(d);
        if matches!(info.kind, DefKind::Let) {
            let other = self.other.borrow().as_ref().and_then(|o| o.slot(d));
            if let Some(slot) = other {
                return slot.get(rt);
            }
        }
        match &info.kind {
            DefKind::State | DefKind::Settings | DefKind::Let => match env.def(d) {
                Some(slot) => slot.get(rt),
                None => Err(Error::failed(format!(
                    "`{}` is not available here",
                    info.name
                ))),
            },
            DefKind::Fn => match self.prog.fns.get(&d) {
                Some(f) => Ok(Value::Fn(Rc::new(value::Closure {
                    params: f.params.clone(),
                    chunk: f.body,
                    captured: Vec::new(),
                    env: self.root.clone(),
                }))),
                None => Err(Error::failed(format!("`{}` has no body", info.name))),
            },
            DefKind::Tokens => Ok(Value::TokenSet(d)),
            DefKind::Service(_) => Ok(Value::Service(info.name.as_str().into())),
            DefKind::Keyframes => Ok(Value::Keyframes(d)),
            // `options: Look`: the enum itself.
            DefKind::Enum(e) => Ok(Value::EnumType(*e)),
            _ => Ok(Value::Null),
        }
    }

    /// Call a config `fn` with arguments in parameter order.
    pub(crate) fn call_fn(
        self: &Rc<Self>,
        rt: &Runtime,
        d: DefId,
        args: Vec<Value>,
    ) -> Result<Value, Error> {
        let f = self.read_def(rt, d, &self.root.clone())?;
        self.call(rt, &f, args)
    }

    /// Write `value` to a place (a `<->` write from a widget, or `strand
    /// set`): outside any handler.
    pub fn write_place(
        self: &Rc<Self>,
        rt: &Runtime,
        place: &crate::lower::Place,
        indices: Vec<Value>,
        env: &Rc<Env>,
        value: Value,
    ) -> Result<(), Error> {
        exec::store(
            self,
            rt,
            place,
            indices,
            env,
            crate::hir::AssignOp::Set,
            value,
        )
    }

    /// Look up `name` as a local of `chunk`'s frame or scope (tests).
    #[doc(hidden)]
    pub fn local_names(&self) -> HashMap<String, LocalId> {
        self.prog
            .locals
            .iter()
            .enumerate()
            .map(|(i, l)| (l.name.clone(), LocalId(i as u32)))
            .collect()
    }
}

/// The element events render's input delivers (`Instance::event`) right
/// after the press that caused them, which set the tray's click point:
/// a button press on the node (`click`, `secondary`, `middle`, and
/// `activate` on a clicked row) or a key press, which sets (0, 0)
/// (`key`, and `activate` from Return). The others come with no press
/// of their own and must not take the last one's point: `scroll` (a
/// wheel), `dismiss` (a click away the compositor saw, or focus loss)
/// and `drop` (a press that started the drag elsewhere, or in another
/// program); a tray action they call sends (0, 0).
pub const INPUT_EVENTS: &[&str] = &["click", "secondary", "middle", "activate", "key"];

thread_local! {
    static INPUT_HANDLERS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// True while a handler of an input event runs on this thread (see
/// [`Vm::handler`]): the tray takes the press's point only then.
pub fn in_input_handler() -> bool {
    INPUT_HANDLERS.with(|c| c.get() > 0)
}

/// (M5) The next handler invocation's number (`ActionCtx::invocation`):
/// one count for the process, from 1, so no two invocations share one,
/// whichever instance or reload ran them; 0 is an action outside any
/// handler.
static INVOCATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// (M5) The context of the handler stretch running on this thread
    /// ([`action_ctx`]).
    static ACTION_CTX: std::cell::Cell<ActionCtx> = const {
        std::cell::Cell::new(ActionCtx {
            input: None,
            invocation: 0,
        })
    };
}

/// (M5) What an action called now knows of the handler running it: its
/// invocation's number and, until the handler's first `await`, the press
/// that started it (`Instance::event_with`); a `fn` the handler calls
/// shares it. Outside any handler, `ActionCtx::default()`.
pub fn action_ctx() -> ActionCtx {
    ACTION_CTX.with(std::cell::Cell::get)
}

/// Sets [`action_ctx`] while a handler stretch runs, restoring the one
/// before when dropped (a handler run from inside another's stretch).
struct Acting(ActionCtx);

impl Acting {
    fn enter(ctx: ActionCtx) -> Self {
        Acting(ACTION_CTX.with(|c| c.replace(ctx)))
    }
}

impl Drop for Acting {
    fn drop(&mut self) {
        ACTION_CTX.with(|c| c.set(self.0));
    }
}

/// Marks a handler of an input event running until dropped.
struct InputHandler;

impl InputHandler {
    fn enter() -> Self {
        INPUT_HANDLERS.with(|c| c.set(c.get().saturating_add(1)));
        InputHandler
    }
}

impl Drop for InputHandler {
    fn drop(&mut self) {
        INPUT_HANDLERS.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

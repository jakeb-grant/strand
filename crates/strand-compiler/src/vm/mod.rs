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

mod builtins;
pub mod clock;
mod exec;
pub mod host;
mod palette;
pub mod persist;
pub mod schema_host;
pub mod value;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use strand_core::{Error, Runtime};

pub use exec::{EventCtx, Frame};
pub use host::{ActionTarget, ServiceHost};
pub use value::{Num, Slot, Value, ValueKey};

use crate::hir::{DefId, DefKind, LocalId, NodeIdx};
use crate::lower::{ChunkId, Program};
use value::NodeState;

/// Deepest nesting of `fn` and lambda calls before a call is an error
/// value (a recursive `fn` without a base case).
pub const MAX_CALL_DEPTH: u32 = 200;

/// What the VM asks of the instantiator while it runs: playing
/// keyframes on an element and passing an event on with `propagate()`.
pub trait VmHooks {
    /// `play name` on the element instance `node`.
    fn play(&self, node: &Rc<NodeState>, keyframes: &str);
    /// `propagate()` in a handler of `ctx`: deliver the event to the next
    /// ancestor that handles it.
    fn propagate(&self, rt: &Runtime, ctx: &EventCtx);
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
        })
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
            if owner.is_none() && env.owned.contains(&idx) {
                owner = Some(env.clone());
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
    hooks: RefCell<Option<std::rc::Weak<dyn VmHooks>>>,
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
            hooks: RefCell::new(None),
        })
    }

    /// The instantiator's hooks (held weakly: the instantiator owns the
    /// VM).
    pub fn set_hooks(&self, hooks: std::rc::Weak<dyn VmHooks>) {
        *self.hooks.borrow_mut() = Some(hooks);
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

    /// A handler body as a coroutine for `rt.spawn*`: it runs until it
    /// returns, suspending at each `await`. Dropping it cancels it.
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
        async move {
            let mut m = exec::Machine::new(chunk, env, frame, None, ctx);
            loop {
                let step = {
                    let Some(rt) = weak.upgrade() else {
                        return Ok(());
                    };
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
        let mut frame: Frame = c.captured.clone();
        for (p, a) in c.params.iter().zip(args) {
            frame.push((*p, a));
        }
        let mut m = exec::Machine::new(c.chunk, c.env.clone(), frame, Some(c.clone()), None);
        let r = m.run(self, rt);
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
            DefKind::Keyframes => Ok(Value::text(info.name.as_str())),
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

/// `material(seed:, dark:)` with the default variant: the palette a
/// config without `use palette` gets.
pub fn palette_material(seed: strand_scene::Color, dark: bool) -> value::Palette {
    palette::material(seed, "tonal_spot", dark, 0.0)
}

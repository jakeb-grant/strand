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

mod convert;
mod emit;
mod mirror;
mod mount;

use std::cell::{Cell, RefCell};
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

use crate::hir::DefKind;
use crate::lower::{Node, Program};
use crate::vm::persist::PersistStore;
use crate::vm::value::{NodeState, Value};
use crate::vm::{EventCtx, ServiceHost, Slot, Vm, VmHooks};
use emit::{Emitter, FragId};

/// A fragment of the mounted tree (see the `emit` module).
pub(crate) struct Frag {
    pub parent: Option<FragId>,
    pub children: Vec<FragId>,
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

/// What one tick produced.
#[derive(Debug, Default)]
pub struct Update {
    /// The tick's scene diff (possibly empty).
    pub diff: SceneDiff,
    /// Runtime errors, as values: failing bindings and handlers, with
    /// what failed.
    pub errors: Vec<String>,
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
    pub store: Option<Rc<dyn PersistStore>>,
    pub errors: RefCell<Vec<(String, Error)>>,
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
        let parent = self.em.borrow().nodes.get(&scene).and_then(|e| e.parent);
        if let Some(p) = parent {
            self.route(rt, p, &ctx.event, ctx.args.clone());
        }
    }
}

impl Ctx {
    /// Deliver `event` to `node` or its nearest ancestor with a handler.
    fn route(&self, rt: &Runtime, node: NodeId, event: &str, args: Vec<Value>) -> bool {
        let mut cur = Some(node);
        while let Some(n) = cur {
            let (queue, state, parent) = {
                let em = self.em.borrow();
                let Some(entry) = em.nodes.get(&n) else {
                    return false;
                };
                (
                    entry.events.get(event).copied(),
                    entry.state.clone(),
                    entry.parent,
                )
            };
            if let Some(q) = queue {
                let ctx = Rc::new(EventCtx {
                    node: Some(state),
                    scene: Some(n),
                    event: event.to_string(),
                    args,
                });
                return q.emit(rt, ctx).is_ok();
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
        for c in prog.components.values() {
            for e in &c.tokens {
                match self.vm.eval(rt, e.value, &root) {
                    Ok(v) => convert::token_entry(types, &mut t, &e.path, &e.ty, &v),
                    Err(err) => self.error(format!("token `${}`", e.path), err),
                }
            }
        }
        Ok(t)
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
    /// Mount `program` on `rt` with its services and (optional) persist
    /// store. The boot diff (token table first, then every surface) comes
    /// with the first [`Instance::flush`] or [`Instance::tick`].
    pub fn new(
        rt: &Runtime,
        program: Arc<Program>,
        host: Rc<dyn ServiceHost>,
        store: Option<Rc<dyn PersistStore>>,
    ) -> Instance {
        for (name, record) in program.services.values() {
            host.declare(rt, name, *record);
        }
        let vm = Vm::new(program, host);
        let ctx = Rc::new(Ctx {
            vm: vm.clone(),
            em: RefCell::new(Emitter::default()),
            store,
            errors: RefCell::default(),
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
        let mut errors: Vec<String> = self
            .ctx
            .errors
            .borrow_mut()
            .drain(..)
            .map(|(what, e)| format!("{what}: {e}"))
            .collect();
        errors.extend(
            tick.errors
                .iter()
                .map(|(id, e)| format!("{}: {e}", self.rt.name(*id))),
        );
        Update {
            diff: SceneDiff { ops },
            errors,
            diagnostics: tick.diagnostics,
            notices: self.ctx.notices.borrow_mut().drain(..).collect(),
        }
    }

    fn emit_binding(&self, id: CoreId) {
        let (memo, scene, prop, transitions, what) = {
            let em = self.ctx.em.borrow();
            let Some(b) = em.bindings.get(&id) else {
                return;
            };
            (
                b.memo,
                b.scene,
                b.prop,
                b.transitions.clone(),
                b.what.clone(),
            )
        };
        match memo.get_untracked(&self.rt) {
            Ok(out) => {
                let t = transitions.get(out.source).cloned().unwrap_or_default();
                self.ctx.em.borrow_mut().set(scene, prop, out.value, t);
            }
            // Errors are values: the prop keeps its last good value.
            Err(e) => self.ctx.error(&*what, e),
        }
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

    /// The value of an exported `file.name` (the CLI's `strand get`).
    pub fn get(&self, path: &str) -> Result<Value, Error> {
        let d = self.export(path)?;
        let root = self.ctx.vm.root.clone();
        self.rt.untrack(|rt| self.ctx.vm.read_def(rt, d, &root))
    }

    /// Write an exported `state` (`strand set theme.look mocha`): an
    /// ordinary write.
    pub fn set(&self, path: &str, value: Value) -> Result<(), Error> {
        let d = self.export(path)?;
        match self.ctx.vm.root.def(d) {
            Some(Slot::Signal(s)) => s.set(&self.rt, value),
            _ => Err(Error::failed(format!("`{path}` is not state"))),
        }
    }

    fn export(&self, path: &str) -> Result<crate::hir::DefId, Error> {
        let prog = &self.ctx.vm.prog;
        prog.exports()
            .find(|(p, _)| p == path)
            .map(|(_, d)| d)
            .filter(|d| {
                matches!(
                    prog.def(*d).kind,
                    DefKind::State | DefKind::Let | DefKind::Settings
                )
            })
            .ok_or_else(|| Error::failed(format!("nothing is exported as `{path}`")))
    }

    /// A top-level `state` or `let` of a file by module and name
    /// (exported or not; tests and the inspector).
    pub fn value_of(&self, module: &str, name: &str) -> Result<Value, Error> {
        let d = self.def_of(module, name)?;
        let root = self.ctx.vm.root.clone();
        self.rt.untrack(|rt| self.ctx.vm.read_def(rt, d, &root))
    }

    /// Write a top-level `state` of a file by module and name (tests).
    pub fn set_value(&self, module: &str, name: &str, value: Value) -> Result<(), Error> {
        let d = self.def_of(module, name)?;
        match self.ctx.vm.root.def(d) {
            Some(Slot::Signal(s)) => s.set(&self.rt, value),
            _ => Err(Error::failed(format!("`{module}.{name}` is not state"))),
        }
    }

    fn def_of(&self, module: &str, name: &str) -> Result<crate::hir::DefId, Error> {
        let prog = &self.ctx.vm.prog;
        prog.defs
            .iter()
            .position(|d| d.module == module && d.name == name && d.owner.is_none())
            .map(|i| crate::hir::DefId(i as u32))
            .ok_or_else(|| Error::failed(format!("no `{module}.{name}`")))
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

    /// Unmount everything (the instance's scope is disposed).
    pub fn shutdown(&mut self) {
        if let Some(s) = self.root.take() {
            s.dispose(&self.rt);
        }
    }
}

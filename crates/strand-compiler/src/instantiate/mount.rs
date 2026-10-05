//! Mounting: lowered trees become scene nodes, watched prop memos,
//! structural effects, handlers and timers.

// `ValueKey` hashes and compares the value, never the pending future an
// `Async` may hold.
#![allow(clippy::mutable_key_type)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use strand_core::{Error, KeyedMemo, KeyedSource, Runtime};
use strand_scene::{NodeKind, Prop as SceneProp, PropValue, TokenTable, Transition};

use super::Ctx;
use super::convert;
use super::emit::{Binding, FragId, NodeEntry, PropOut};
use crate::hir::{DefId, LocalId, PoseKind, TimerKind};
use crate::lower::{
    ChunkId, Element, ElementKind, Event, For, ForKey, Handler, Node, Prop, StateInit, Surface,
    Timer, TokenDef,
};
use crate::ty::Ty;
use crate::vm::persist;
use crate::vm::value::{NodeState, Slot, Value, ValueKey};
use crate::vm::{Env, EventCtx, Frame};

/// The element a handler or timer sits on.
#[derive(Clone)]
pub(crate) struct ElemCtx {
    pub scene: strand_scene::NodeId,
    pub state: Rc<NodeState>,
}

/// One source of a prop's value: the base binding or a `when` block.
#[derive(Clone)]
struct Source {
    cond: Option<ChunkId>,
    value: SourceValue,
    transition: Transition,
}

#[derive(Clone)]
enum SourceValue {
    Chunk(ChunkId, Ty),
    Pose(Vec<Prop>),
    Tokens(Vec<TokenDef>),
}

type ListFn = Box<dyn Fn(&Runtime) -> Result<Vec<(ValueKey, Value)>, Error>>;
type ItemMount = Rc<dyn Fn(&Rc<Ctx>, &Runtime, &Rc<Env>, FragId, &Value)>;

/// A keyed list of instances to mount ([`Ctx::mount_keyed`]).
struct Keyed {
    /// The items with their keys, tracked.
    list: ListFn,
    /// The local each item's value is bound to.
    binding: Option<LocalId>,
    /// Elements each item owns.
    owned: Arc<std::collections::BTreeSet<crate::hir::NodeIdx>>,
    /// Mounts one item's content.
    item: ItemMount,
}

impl Ctx {
    pub(crate) fn eval(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: ChunkId,
        env: &Rc<Env>,
    ) -> Result<Value, Error> {
        self.vm.eval(rt, chunk, env)
    }

    pub(crate) fn error(&self, what: impl Into<String>, e: Error) {
        self.errors.borrow_mut().push((what.into(), e));
    }

    /// Mount `nodes` into fragment `frag`.
    pub(crate) fn mount_nodes(
        self: &Rc<Self>,
        rt: &Runtime,
        nodes: &Arc<Vec<Node>>,
        env: &Rc<Env>,
        frag: FragId,
        el: Option<&ElemCtx>,
    ) {
        for n in nodes.iter() {
            self.mount_node(rt, n, env, frag, el);
        }
    }

    fn mount_node(
        self: &Rc<Self>,
        rt: &Runtime,
        n: &Node,
        env: &Rc<Env>,
        frag: FragId,
        el: Option<&ElemCtx>,
    ) {
        // Trees nest up to 256 levels (the parser's bound); mounting
        // recurses once per level, so grow the stack on small threads.
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
            self.mount_node_inner(rt, n, env, frag, el)
        });
    }

    fn mount_node_inner(
        self: &Rc<Self>,
        rt: &Runtime,
        n: &Node,
        env: &Rc<Env>,
        frag: FragId,
        el: Option<&ElemCtx>,
    ) {
        match n {
            Node::Element(e) => self.mount_element(rt, e, env, frag, Vec::new()),
            Node::Surface(s) => self.mount_surface(rt, s, env, frag),
            Node::If { cond, then, else_ } => self.mount_switch(
                rt,
                *cond,
                vec![then.clone(), else_.clone()],
                true,
                env,
                frag,
            ),
            Node::Match { selector, arms } => {
                self.mount_switch(rt, *selector, arms.clone(), false, env, frag)
            }
            Node::For(f) => self.mount_for(rt, f, env, frag),
            Node::Slot => {
                let mut cur = Some(env.clone());
                while let Some(e) = cur {
                    if let Some((children, caller)) = &e.slot {
                        let children = children.clone();
                        let caller = Env::child(caller, Arc::default(), None, None);
                        self.mount_block(rt, frag, None, |ctx, rt, f| {
                            caller.set_owner(rt.current_owner());
                            ctx.declare(rt, &children, &caller);
                            ctx.mount_nodes(rt, &children, &caller, f, None);
                        });
                        return;
                    }
                    cur = e.parent.clone();
                }
            }
            Node::Handler(h) => self.mount_handler(rt, h, env, el),
            Node::Timer(t) => self.mount_timer(rt, t, env, el),
            Node::State(_)
            | Node::Let { .. }
            | Node::When { .. }
            | Node::Pose { .. }
            | Node::Set(_)
            | Node::Play(_) => {}
        }
    }

    /// Create the `let`s and `state`s of a body in `env`: every `let`
    /// first (they are lazy), then each `state` with its initial value.
    pub(crate) fn declare(self: &Rc<Self>, rt: &Runtime, nodes: &[Node], env: &Rc<Env>) {
        let mut lets = Vec::new();
        let mut states = Vec::new();
        collect_decls(nodes, &mut lets, &mut states);
        for (def, value) in lets {
            let (vm, e) = (self.vm.clone(), env.clone());
            let m = rt.memo(move |rt| vm.eval(rt, value, &e));
            rt.set_name(m.id(), self.vm.prog.def(def).name.as_str());
            env.bind_def(def, Slot::Memo(m));
        }
        for s in states {
            self.declare_state(rt, s, env);
        }
    }

    pub(crate) fn declare_state(
        self: &Rc<Self>,
        rt: &Runtime,
        s: &crate::lower::State,
        env: &Rc<Env>,
    ) {
        let types = &self.vm.prog.types;
        let info = self.vm.prog.def(s.def);
        let default = rt.untrack(|rt| match &s.init {
            StateInit::Value(c) => self.eval(rt, *c, env),
            StateInit::Settings { record, fields, .. } => {
                let def = types.record(*record);
                let mut values = Vec::with_capacity(def.fields.len());
                for (f, d) in def.fields.iter().zip(fields) {
                    values.push(match d {
                        Some(c) => self.eval(rt, *c, env)?,
                        None => crate::vm::schema_host::default_value(types, &f.ty),
                    });
                }
                Ok(Value::record(*record, values))
            }
        });
        let default = match default {
            Ok(v) => v,
            Err(e) => {
                self.error(format!("state `{}`", info.name), e);
                Value::Null
            }
        };
        // `toasts.dnd` for a file's state, `Clock.open` for a component's
        // (or a surface's).
        let key = match info.owner {
            Some(o) => format!("{}.{}", self.vm.prog.def(o).name, info.name),
            None => format!("{}.{}", info.module, info.name),
        };
        let initial = match (&self.store, s.persist) {
            (Some(store), true) => {
                let r = persist::restore(store.as_ref(), &key, types, &info.ty, &default);
                if r.kept {
                    self.notices.borrow_mut().push(format!(
                        "{key}: kept {} (default changed)",
                        r.value.show(types)
                    ));
                }
                r.value
            }
            _ => default.clone(),
        };
        let sig = rt.signal(initial);
        rt.set_name(sig.id(), key.as_str());
        env.bind_def(s.def, Slot::Signal(sig));
        if s.persist
            && let Some(store) = self.store.clone()
        {
            let ty = info.ty.clone();
            let prog = self.vm.prog.clone();
            rt.effect(move |rt| {
                let v = sig.get(rt)?;
                persist::save(store.as_ref(), &key, &prog.types, &ty, &default, &v);
                Ok(())
            });
        }
    }

    /// A fragment at `at` under `parent` with its own scope, filled by
    /// `fill` inside that scope.
    pub(crate) fn mount_block(
        self: &Rc<Self>,
        rt: &Runtime,
        parent: FragId,
        at: Option<usize>,
        fill: impl FnOnce(&Rc<Ctx>, &Runtime, FragId),
    ) -> FragId {
        let frag = self.em.borrow_mut().new_frag(Some(parent), at);
        let (scope, ()) = rt.scope(|rt| fill(self, rt, frag));
        self.em.borrow_mut().set_scope(frag, scope);
        frag
    }

    /// Unmount everything in `frag` (`keep_self`: its content only).
    pub(crate) fn unmount(&self, rt: &Runtime, frag: FragId, keep_self: bool) {
        let (scopes, states) = self.em.borrow_mut().unmount(frag, keep_self);
        for s in scopes {
            s.dispose(rt);
        }
        for st in states {
            for f in [st.hover, st.pressed, st.focused, st.selected] {
                let _ = f.set(rt, false);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Elements

    pub(crate) fn mount_element(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        parent: FragId,
        extra: Vec<(SceneProp, PropValue)>,
    ) {
        let kind = match &e.kind {
            ElementKind::Builtin(k) => *k,
            ElementKind::Component(d) => return self.mount_component(rt, *d, e, env, parent),
            ElementKind::Unknown(_) => return,
        };
        let frag = self.em.borrow_mut().new_frag(Some(parent), None);
        let id = self.em.borrow_mut().create(frag, kind);
        let state = env.node_state(rt, e.node);
        state.scene.set(Some(id));
        for &l in &e.scope {
            if env.local(l).is_none() {
                env.bind_local(l, Slot::Memo(rt.memo(|_| Ok(Value::int(0)))));
            }
        }
        {
            let mut em = self.em.borrow_mut();
            let parent_scene = em.scene_parent(frag);
            em.nodes.insert(
                id,
                NodeEntry {
                    surface: kind.is_surface(),
                    parent: parent_scene,
                    state: state.clone(),
                    events: HashMap::new(),
                    two_way: Vec::new(),
                    bindings: Vec::new(),
                },
            );
            for (p, v) in extra {
                em.set(id, p, v, Transition::Default);
            }
        }
        for (prop, sources) in self.sources(rt, e, env) {
            self.bind_prop(rt, id, prop, sources, env, kind);
        }
        for p in e.props.iter().chain(e.arg.iter()) {
            if let (Some(sp), Some(tw)) = (p.prop, &p.two_way) {
                let mut em = self.em.borrow_mut();
                if let Some(entry) = em.nodes.get_mut(&id) {
                    entry.two_way.push((sp, tw.clone(), env.clone()));
                }
            }
        }
        let ec = ElemCtx { scene: id, state };
        self.mount_nodes(rt, &e.children, env, frag, Some(&ec));
    }

    /// The sources of each scene prop of `e`, in first-mention order:
    /// the base binding (the positional argument, a prop, a sub-prop),
    /// then each `when` in source order (later wins), poses, `set { }`
    /// and `play`.
    fn sources(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
    ) -> Vec<(SceneProp, Vec<Source>)> {
        let mut out: Vec<(SceneProp, Vec<Source>)> = Vec::new();
        let mut push = |p: SceneProp, s: Source| match out.iter_mut().find(|(q, _)| *q == p) {
            Some((_, v)) => v.push(s),
            None => out.push((p, vec![s])),
        };
        let add = |p: &Prop, cond: Option<ChunkId>, push: &mut dyn FnMut(SceneProp, Source)| {
            if let Some(sp) = p.prop {
                let transition = p.transition.map_or(Transition::Default, |t| {
                    rt.untrack(|rt| self.eval(rt, t, env))
                        .map(|v| convert::transition(&self.vm.prog.types, &v))
                        .unwrap_or_default()
                });
                push(
                    sp,
                    Source {
                        cond,
                        value: SourceValue::Chunk(p.value, p.ty.clone()),
                        transition,
                    },
                );
            }
            for s in &p.sub {
                if let Some(sp) = s.prop {
                    push(
                        sp,
                        Source {
                            cond,
                            value: SourceValue::Chunk(s.value, s.ty.clone()),
                            transition: Transition::Default,
                        },
                    );
                }
            }
        };
        if let Some(a) = &e.arg {
            add(a, None, &mut push);
        }
        for p in &e.props {
            add(p, None, &mut push);
        }
        let mut sets: Vec<TokenDef> = Vec::new();
        for c in e.children.iter() {
            match c {
                Node::When { cond, props } => {
                    for p in props {
                        add(p, Some(*cond), &mut push);
                    }
                }
                Node::Pose { kind, props } => {
                    let p = match kind {
                        PoseKind::Enter => SceneProp::Enter,
                        PoseKind::Exit => SceneProp::Exit,
                    };
                    push(
                        p,
                        Source {
                            cond: None,
                            value: SourceValue::Pose(props.clone()),
                            transition: Transition::Default,
                        },
                    );
                }
                Node::Set(defs) => sets.extend(defs.iter().cloned()),
                Node::Play(c) => push(
                    SceneProp::Play,
                    Source {
                        cond: None,
                        value: SourceValue::Chunk(*c, Ty::TEXT),
                        transition: Transition::Default,
                    },
                ),
                _ => {}
            }
        }
        if !sets.is_empty() {
            push(
                SceneProp::Tokens,
                Source {
                    cond: None,
                    value: SourceValue::Tokens(sets),
                    transition: Transition::Default,
                },
            );
        }
        // `exit` mirrors `enter` unless given (design.md, "Poses").
        let has_exit = out.iter().any(|(p, _)| *p == SceneProp::Exit);
        if !has_exit && let Some((_, enter)) = out.iter().find(|(p, _)| *p == SceneProp::Enter) {
            let mirrored = enter.clone();
            out.push((SceneProp::Exit, mirrored));
        }
        out
    }

    fn source_value(
        self: &Rc<Self>,
        rt: &Runtime,
        prop: SceneProp,
        s: &SourceValue,
        env: &Rc<Env>,
    ) -> Result<PropValue, Error> {
        let types = &self.vm.prog.types;
        Ok(match s {
            SourceValue::Chunk(c, ty) => {
                convert::prop_value_for(types, prop, ty, &self.eval(rt, *c, env)?)
            }
            SourceValue::Pose(props) => {
                let mut pose = Vec::new();
                for p in props {
                    if let Some(sp) = p.prop {
                        let v = self.eval(rt, p.value, env)?;
                        pose.push((sp, convert::prop_value_for(types, sp, &p.ty, &v)));
                    }
                }
                PropValue::Pose(pose)
            }
            SourceValue::Tokens(defs) => {
                let mut t = TokenTable::default();
                for d in defs {
                    let v = self.eval(rt, d.value, env)?;
                    convert::token_entry(types, &mut t, &d.path, &d.ty, &v);
                }
                PropValue::Tokens(Box::new(t))
            }
        })
    }

    /// One watched memo per scene prop: the last `when` that holds wins,
    /// else the base binding, else unset.
    fn bind_prop(
        self: &Rc<Self>,
        rt: &Runtime,
        id: strand_scene::NodeId,
        prop: SceneProp,
        sources: Vec<Source>,
        env: &Rc<Env>,
        kind: NodeKind,
    ) {
        let transitions: Rc<Vec<Transition>> =
            Rc::new(sources.iter().map(|s| s.transition.clone()).collect());
        let sources = Rc::new(sources);
        let (ctx, e, srcs) = (self.clone(), env.clone(), sources.clone());
        let memo = rt.memo(move |rt| {
            for (i, s) in srcs.iter().enumerate().rev() {
                if let Some(c) = s.cond
                    && !ctx.eval(rt, c, &e)?.truthy()
                {
                    continue;
                }
                return Ok(PropOut {
                    value: ctx.source_value(rt, prop, &s.value, &e)?,
                    source: i,
                });
            }
            Ok(PropOut {
                value: PropValue::Unset,
                source: 0,
            })
        });
        let what: Rc<str> = format!("{}.{}", kind.name(), prop.name()).into();
        rt.set_name(memo.id(), &*what);
        match memo.get_untracked(rt) {
            Ok(out) => {
                let t = transitions.get(out.source).cloned().unwrap_or_default();
                self.em.borrow_mut().set(id, prop, out.value, t);
            }
            Err(e) => self.error(&*what, e),
        }
        if rt.watch(memo.id()).is_ok() {
            let mut em = self.em.borrow_mut();
            em.bindings.insert(
                memo.id(),
                Binding {
                    scene: id,
                    prop,
                    memo,
                    transitions,
                    what,
                },
            );
            if let Some(entry) = em.nodes.get_mut(&id) {
                entry.bindings.push(memo.id());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Structure

    /// `if`/`else` (`two`: the selector is a condition) and `match` (the
    /// selector gives an arm index): mounts the chosen branch now and
    /// swaps branches when the selector changes. A swapped-out branch is
    /// removed (render plays its `exit`) and its scope disposed.
    fn mount_switch(
        self: &Rc<Self>,
        rt: &Runtime,
        selector: ChunkId,
        branches: Vec<Arc<Vec<Node>>>,
        two: bool,
        env: &Rc<Env>,
        parent: FragId,
    ) {
        let frag = self.em.borrow_mut().new_frag(Some(parent), None);
        let (block, ()) = rt.scope(|_| ());
        let (ctx, e) = (self.clone(), env.clone());
        let pick = Rc::new(move |rt: &Runtime| -> Result<Option<usize>, Error> {
            let v = ctx.eval(rt, selector, &e)?;
            Ok(if two {
                Some(if v.truthy() { 0 } else { 1 })
            } else {
                v.as_f64().map(|i| i as usize)
            })
        });
        let current: Rc<Cell<Option<Option<usize>>>> = Rc::new(Cell::new(None));
        let branches = Rc::new(branches);
        let show = {
            let (ctx, env, branches) = (self.clone(), env.clone(), branches.clone());
            Rc::new(move |rt: &Runtime, which: Option<usize>| {
                ctx.unmount(rt, frag, true);
                if let Some(nodes) = which.and_then(|i| branches.get(i)).cloned() {
                    // The branch's own `let`s and `state`s live while it
                    // is mounted.
                    let benv = Env::child(&env, Arc::default(), None, None);
                    let _ = rt.with_owner(block.id(), |rt| {
                        ctx.mount_block(rt, frag, None, |ctx, rt, f| {
                            benv.set_owner(rt.current_owner());
                            ctx.declare(rt, &nodes, &benv);
                            ctx.mount_nodes(rt, &nodes, &benv, f, None);
                        });
                    });
                }
            })
        };
        match rt.untrack(|rt| pick(rt)) {
            Ok(which) => {
                show(rt, which);
                current.set(Some(which));
            }
            Err(e) => self.error("if/match", e),
        }
        let _ = rt.with_owner(block.id(), |rt| {
            rt.effect(move |rt| {
                let which = pick(rt)?;
                if current.get() != Some(which) {
                    rt.untrack(|rt| show(rt, which));
                    current.set(Some(which));
                }
                Ok(())
            })
        });
    }

    /// A keyed list of instances: `for` items and per-monitor bars. Items
    /// keep their identity (and state) by key; inserts, removes and moves
    /// are applied as they come, so only the changed items touch the
    /// scene.
    fn mount_keyed(self: &Rc<Self>, rt: &Runtime, parent: FragId, env: &Rc<Env>, k: Keyed) {
        let Keyed {
            list,
            binding,
            owned,
            item,
        } = k;
        let frag = self.em.borrow_mut().new_frag(Some(parent), None);
        let (block, ()) = rt.scope(|_| ());
        let Ok(keyed) = rt.with_owner(block.id(), |rt| {
            rt.keyed_memo(|t: &(ValueKey, Value)| t.0.clone(), move |rt| list(rt))
        }) else {
            return;
        };
        // Key → position, rebuilt once per snapshot version.
        let index: Rc<RefCell<(u64, HashMap<ValueKey, usize>)>> =
            Rc::new(RefCell::new((u64::MAX, HashMap::new())));
        let items: Rc<RefCell<Vec<(ValueKey, FragId)>>> = Rc::default();
        let version: Rc<Cell<Option<u64>>> = Rc::default();
        let mount_item = {
            let (ctx, env, owned, item, index) = (
                self.clone(),
                env.clone(),
                owned.clone(),
                item.clone(),
                index.clone(),
            );
            Rc::new(move |rt: &Runtime, at: usize, key: ValueKey| -> FragId {
                let item_env = Env::child(&env, owned.clone(), None, None);
                let (ctx2, ie) = (ctx.clone(), item_env.clone());
                let index = index.clone();
                let item = item.clone();
                rt.with_owner(block.id(), |rt| {
                    ctx.mount_block(rt, frag, Some(at), |_, rt, f| {
                        ie.set_owner(rt.current_owner());
                        let k = key.clone();
                        let value = rt.memo(move |rt| item_value(rt, keyed, &index, &k));
                        if let Some(b) = binding {
                            ie.bind_local(b, Slot::Memo(value));
                        }
                        let v = value.get_untracked(rt).unwrap_or(Value::Null);
                        item(&ctx2, rt, &ie, f, &v);
                    })
                })
                .unwrap_or(frag)
            })
        };
        let apply = {
            let (ctx, items) = (self.clone(), items.clone());
            Rc::new(
                move |rt: &Runtime, diffs: Vec<strand_core::VecDiff<ValueKey, (ValueKey, Value)>>| {
                    for d in diffs {
                        match d {
                            strand_core::VecDiff::Reset { items: new } => {
                                let keys: Vec<ValueKey> = new.into_iter().map(|(k, _)| k).collect();
                                let old: Vec<_> = items.borrow_mut().drain(..).collect();
                                let placed = ctx.reconcile(rt, frag, old, &keys, |rt, at, k| {
                                    mount_item(rt, at, k.clone())
                                });
                                *items.borrow_mut() = placed;
                            }
                            strand_core::VecDiff::Insert { index, key, .. } => {
                                let f = mount_item(rt, index, key.clone());
                                let mut it = items.borrow_mut();
                                let at = index.min(it.len());
                                it.insert(at, (key, f));
                            }
                            strand_core::VecDiff::Remove { index, .. } => {
                                let removed = {
                                    let mut it = items.borrow_mut();
                                    (index < it.len()).then(|| it.remove(index))
                                };
                                if let Some((_, f)) = removed {
                                    ctx.unmount(rt, f, false);
                                }
                            }
                            strand_core::VecDiff::Move { from, to, .. } => {
                                let moved = {
                                    let mut it = items.borrow_mut();
                                    if from < it.len() {
                                        let x = it.remove(from);
                                        let to = to.min(it.len());
                                        it.insert(to, x.clone());
                                        Some((x.1, to))
                                    } else {
                                        None
                                    }
                                };
                                if let Some((f, to)) = moved {
                                    ctx.em.borrow_mut().move_frag(f, to);
                                }
                            }
                            // The item's value memo follows by itself.
                            strand_core::VecDiff::Update { .. } => {}
                        }
                    }
                },
            )
        };
        match rt.untrack(|rt| keyed.snapshot(rt)) {
            Ok(snap) => {
                apply(rt, snap.diffs_or_reset(None));
                version.set(Some(snap.version()));
            }
            Err(e) => self.error("for", e),
        }
        let _ = rt.with_owner(block.id(), |rt| {
            rt.effect(move |rt| {
                let snap = keyed.snapshot(rt)?;
                if version.get() != Some(snap.version()) {
                    let diffs = snap.diffs_or_reset(version.get());
                    rt.untrack(|rt| apply(rt, diffs));
                    version.set(Some(snap.version()));
                }
                Ok(())
            })
        });
    }

    /// A whole new list (a first publish, or a reader that fell behind
    /// the diff log) against the mounted items, by key: items that left
    /// are unmounted, new ones mounted, and only the items outside the
    /// longest run already in order are moved, so identity and state are
    /// kept and a list where one item moved sends one `Move`.
    fn reconcile(
        self: &Rc<Self>,
        rt: &Runtime,
        frag: FragId,
        old: Vec<(ValueKey, FragId)>,
        keys: &[ValueKey],
        mut mount: impl FnMut(&Runtime, usize, &ValueKey) -> FragId,
    ) -> Vec<(ValueKey, FragId)> {
        let wanted: std::collections::HashSet<&ValueKey> = keys.iter().collect();
        let mut kept: HashMap<ValueKey, (usize, FragId)> = HashMap::new();
        for (k, f) in old {
            if wanted.contains(&k) && !kept.contains_key(&k) {
                let at = kept.len();
                kept.insert(k, (at, f));
            } else {
                self.unmount(rt, f, false);
            }
        }
        // Old positions of the kept items, in new order; the longest
        // increasing run of them stays put.
        let seq: Vec<usize> = keys
            .iter()
            .filter_map(|k| kept.get(k).map(|(i, _)| *i))
            .collect();
        let stay: std::collections::HashSet<usize> = longest_increasing(&seq).into_iter().collect();
        let mut out: Vec<(ValueKey, FragId)> = Vec::with_capacity(keys.len());
        let position = |ctx: &Rc<Ctx>, f: FragId| {
            ctx.em
                .borrow()
                .frag(frag)
                .and_then(|p| p.children.iter().position(|c| *c == f))
        };
        for k in keys {
            // Right after the previous item, wherever that is now.
            let prev = out.last().and_then(|(_, p)| position(self, *p));
            let f = match kept.get(k) {
                Some((i, f)) => {
                    if !stay.contains(i) {
                        // `move_frag` counts positions without the moved
                        // item.
                        let at = match (prev, position(self, *f)) {
                            (None, _) => 0,
                            (Some(p), Some(cur)) if cur < p => p,
                            (Some(p), _) => p + 1,
                        };
                        self.em.borrow_mut().move_frag(*f, at);
                    }
                    *f
                }
                None => mount(rt, prev.map_or(0, |p| p + 1), k),
            };
            out.push((k.clone(), f));
        }
        out
    }

    fn mount_for(self: &Rc<Self>, rt: &Runtime, f: &For, env: &Rc<Env>, parent: FragId) {
        let (ctx, e) = (self.clone(), env.clone());
        let (iter, key, binding) = (f.iter, f.key.clone(), f.binding);
        let list: ListFn = Box::new(move |rt| {
            let v = ctx.eval(rt, iter, &e)?;
            let types = &ctx.vm.prog.types;
            let items = v.as_list().unwrap_or(&[]);
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let k = match &key {
                    ForKey::Path(p) => item.key_path(types, p).cloned().unwrap_or(Value::Null),
                    ForKey::Value => item.clone(),
                    ForKey::Expr(c) => {
                        ctx.vm
                            .eval_with(rt, *c, &e, vec![(binding, item.clone())])?
                    }
                };
                out.push((ValueKey(k), item.clone()));
            }
            Ok(out)
        });
        let nodes = f.body.nodes.clone();
        let item: ItemMount = Rc::new(move |ctx, rt, env, frag, _v| {
            ctx.declare(rt, &nodes, env);
            ctx.mount_nodes(rt, &nodes, env, frag, None);
        });
        self.mount_keyed(
            rt,
            parent,
            env,
            Keyed {
                list,
                binding: Some(binding),
                owned: f.body.owned.clone(),
                item,
            },
        );
    }

    fn mount_component(
        self: &Rc<Self>,
        rt: &Runtime,
        def: DefId,
        call: &Element,
        caller: &Rc<Env>,
        parent: FragId,
    ) {
        let Some(comp) = self.vm.prog.components.get(&def).cloned() else {
            return;
        };
        let env = Env::child(
            &self.vm.root,
            comp.body.owned.clone(),
            Some((call.children.clone(), caller.clone())),
            Some(def),
        );
        let args: Vec<Option<ChunkId>> = comp
            .params
            .iter()
            .map(|(local, _)| {
                let name = &self.vm.prog.local(*local).name;
                call.arg
                    .iter()
                    .chain(call.props.iter())
                    .find(|p| &p.name == name)
                    .map(|p| p.value)
            })
            .collect();
        self.mount_block(rt, parent, None, |ctx, rt, frag| {
            env.set_owner(rt.current_owner());
            for ((local, default), arg) in comp.params.iter().zip(args) {
                let vm = ctx.vm.clone();
                let m = match (arg, default) {
                    (Some(c), _) => {
                        let e = caller.clone();
                        rt.memo(move |rt| vm.eval(rt, c, &e))
                    }
                    (None, Some(d)) => {
                        let (e, d) = (env.clone(), *d);
                        rt.memo(move |rt| vm.eval(rt, d, &e))
                    }
                    (None, None) => rt.memo(|_| Ok(Value::Null)),
                };
                env.bind_local(*local, Slot::Memo(m));
            }
            ctx.acquire(rt, &comp.body.services);
            ctx.declare(rt, &comp.body.nodes, &env);
            ctx.mount_nodes(rt, &comp.body.nodes, &env, frag, None);
        });
    }

    /// Acquire services for the current scope and release them when it
    /// goes.
    pub(crate) fn acquire(&self, rt: &Runtime, services: &std::collections::BTreeSet<String>) {
        for s in services {
            self.vm.host.acquire(s);
            let host = self.vm.host.clone();
            let s = s.clone();
            rt.on_cleanup(move || host.release(&s));
        }
    }

    /// A surface: once, or for a `bar`, once per monitor of the `screens`
    /// service with `screen` in scope, pinned to it with `screens:`.
    pub(crate) fn mount_surface(
        self: &Rc<Self>,
        rt: &Runtime,
        s: &Surface,
        env: &Rc<Env>,
        parent: FragId,
    ) {
        let mut extra = Vec::new();
        if let Some(n) = &s.name {
            extra.push((SceneProp::Name, PropValue::Text(n.clone())));
        }
        let element = Arc::new(s.element.clone());
        let services = s.body.services.clone();
        let Some(screen) = s.screen else {
            let env = Env::child(env, s.body.owned.clone(), None, None);
            self.mount_block(rt, parent, None, |ctx, rt, frag| {
                env.set_owner(rt.current_owner());
                ctx.acquire(rt, &services);
                ctx.declare(rt, &element.children, &env);
                ctx.mount_element(rt, &element, &env, frag, extra);
            });
            return;
        };
        let ctx = self.clone();
        let list: ListFn = Box::new(move |rt| {
            let all = ctx.vm.host.read(rt, "screens", "all")?;
            let types = &ctx.vm.prog.types;
            Ok(all
                .as_list()
                .unwrap_or(&[])
                .iter()
                .map(|sc| {
                    let name = sc.field(types, "name").cloned().unwrap_or(Value::Null);
                    (ValueKey(name), sc.clone())
                })
                .collect())
        });
        let item: ItemMount = Rc::new(move |ctx, rt, env, frag, v| {
            let mut extra = extra.clone();
            let name = v
                .field(&ctx.vm.prog.types, "name")
                .and_then(Value::as_text)
                .unwrap_or("")
                .to_string();
            extra.push((SceneProp::Screens, PropValue::Text(name)));
            ctx.acquire(rt, &services);
            ctx.declare(rt, &element.children, env);
            ctx.mount_element(rt, &element, env, frag, extra);
        });
        self.mount_keyed(
            rt,
            parent,
            env,
            Keyed {
                list,
                binding: Some(screen),
                owned: s.body.owned.clone(),
                item,
            },
        );
    }

    // -----------------------------------------------------------------------
    // Handlers and timers

    fn event_ctx(el: Option<&ElemCtx>, event: &str, args: Vec<Value>) -> Option<Rc<EventCtx>> {
        Some(Rc::new(EventCtx {
            node: el.map(|e| e.state.clone()),
            scene: el.map(|e| e.scene),
            event: event.to_string(),
            args,
        }))
    }

    pub(crate) fn mount_handler(
        self: &Rc<Self>,
        rt: &Runtime,
        h: &Handler,
        env: &Rc<Env>,
        el: Option<&ElemCtx>,
    ) {
        let (vm, body, params) = (self.vm.clone(), h.body, h.params.clone());
        match &h.event {
            Event::Element(name) => {
                let Some(el) = el else { return };
                let q = rt.input_events::<Rc<EventCtx>>();
                let site = rt.handler_site();
                let e = env.clone();
                let r = q.on(rt, move |rt, ctx| {
                    let frame: Frame = params
                        .iter()
                        .copied()
                        .zip(ctx.args.iter().cloned())
                        .collect();
                    let fut = vm.handler(rt, body, e.clone(), frame, Some(ctx.clone()));
                    rt.spawn_input(Some(site), fut);
                    Ok(())
                });
                if r.is_ok() {
                    let mut em = self.em.borrow_mut();
                    if let Some(entry) = em.nodes.get_mut(&el.scene) {
                        entry.events.insert(name.clone(), q);
                    }
                }
            }
            Event::Service { service, event } => {
                let Some(q) = self.vm.host.event(rt, service, event) else {
                    return;
                };
                let site = rt.handler_site();
                let e = env.clone();
                let ev = format!("{service}.{event}");
                let el = el.cloned();
                let _ = q.on(rt, move |rt, args: &Vec<Value>| {
                    let frame: Frame = params.iter().copied().zip(args.iter().cloned()).collect();
                    let ctx = Ctx::event_ctx(el.as_ref(), &ev, args.clone());
                    let fut = vm.handler(rt, body, e.clone(), frame, ctx);
                    rt.spawn_for(site, fut);
                    Ok(())
                });
            }
            Event::Change { targets, debounce } => {
                let targets = targets.clone();
                let (ctx, e) = (self.clone(), env.clone());
                let track = move |rt: &Runtime| -> Result<Value, Error> {
                    let mut out = Vec::with_capacity(targets.len());
                    for (t, _) in &targets {
                        out.push(ctx.eval(rt, *t, &e)?);
                    }
                    Ok(Value::list(out))
                };
                let keys: Vec<ChunkId> = match &h.event {
                    Event::Change { targets, .. } => targets.iter().filter_map(|t| t.1).collect(),
                    _ => Vec::new(),
                };
                let (ctx, e) = (self.clone(), env.clone());
                let key = move |rt: &Runtime| -> Result<Value, Error> {
                    let mut out = Vec::with_capacity(keys.len());
                    for k in &keys {
                        let item = ctx.eval(rt, *k, &e)?;
                        out.push(item.identity(&ctx.vm.prog.types));
                    }
                    Ok(Value::list(out))
                };
                let e = env.clone();
                let el = el.cloned();
                let start = move |rt: &Runtime| {
                    let ctx = Ctx::event_ctx(el.as_ref(), "change", Vec::new());
                    let fut = vm.handler(rt, body, e.clone(), Frame::new(), ctx);
                    rt.spawn(fut);
                };
                match debounce {
                    None => {
                        rt.on_change_keyed(key, track, move |rt, _| {
                            start(rt);
                            Ok(())
                        });
                    }
                    Some(d) => {
                        let delay = rt
                            .untrack(|rt| self.eval(rt, *d, env))
                            .ok()
                            .and_then(|v| v.as_duration())
                            .unwrap_or_default();
                        rt.on_change_after_keyed(key, track, delay, move |rt| {
                            start(rt);
                            Ok(())
                        });
                    }
                }
            }
        }
    }

    fn mount_timer(self: &Rc<Self>, rt: &Runtime, t: &Timer, env: &Rc<Env>, el: Option<&ElemCtx>) {
        let (ctx, e, duration) = (self.clone(), env.clone(), t.duration);
        let dur = move |rt: &Runtime| {
            ctx.eval(rt, duration, &e)?
                .as_duration()
                .ok_or_else(|| Error::failed("a timer needs a duration"))
        };
        let (ctx, e, cond) = (self.clone(), env.clone(), t.while_);
        let cond = move |rt: &Runtime| match cond {
            Some(c) => Ok(ctx.eval(rt, c, &e)?.truthy()),
            None => Ok(true),
        };
        let (vm, e, body) = (self.vm.clone(), env.clone(), t.body);
        let el = el.cloned();
        let run = move |rt: &Runtime| {
            let ctx = Ctx::event_ctx(el.as_ref(), "timer", Vec::new());
            let fut = vm.handler(rt, body, e.clone(), Frame::new(), ctx);
            rt.spawn(fut);
            Ok(())
        };
        match t.kind {
            TimerKind::After => {
                rt.after_dyn(dur, cond, run);
            }
            TimerKind::Every => {
                rt.every_dyn(dur, cond, run);
            }
        }
    }
}

/// The values of one longest strictly increasing subsequence of `seq`.
fn longest_increasing(seq: &[usize]) -> Vec<usize> {
    // tails[l]: index into seq of the smallest tail of a run of length l+1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, &x) in seq.iter().enumerate() {
        let l = tails.partition_point(|&t| seq[t] < x);
        if l > 0 {
            prev[i] = Some(tails[l - 1]);
        }
        if l == tails.len() {
            tails.push(i);
        } else {
            tails[l] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        out.push(seq[i]);
        cur = prev[i];
    }
    out.reverse();
    out
}

/// The current value of the item with key `k` in a keyed list.
fn item_value(
    rt: &Runtime,
    keyed: KeyedMemo<ValueKey, (ValueKey, Value)>,
    index: &RefCell<(u64, HashMap<ValueKey, usize>)>,
    k: &ValueKey,
) -> Result<Value, Error> {
    let snap = keyed.snapshot(rt)?;
    let mut idx = index.borrow_mut();
    if idx.0 != snap.version() {
        idx.1 = snap
            .items()
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (k.clone(), i))
            .collect();
        idx.0 = snap.version();
    }
    Ok(idx
        .1
        .get(k)
        .and_then(|&i| snap.items().get(i))
        .map_or(Value::Null, |(_, (_, v))| v.clone()))
}

/// The `let`s and `state`s of a body: in elements and `if`/`match`
/// branches, not in `for` items or components (they have their own
/// scope).
fn collect_decls<'a>(
    nodes: &'a [Node],
    lets: &mut Vec<(DefId, ChunkId)>,
    states: &mut Vec<&'a crate::lower::State>,
) {
    for n in nodes {
        match n {
            Node::Let { def, value } => lets.push((*def, *value)),
            Node::State(s) => states.push(s),
            Node::Element(e) if !matches!(e.kind, ElementKind::Component(_)) => {
                collect_decls(&e.children, lets, states)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::longest_increasing;

    #[test]
    fn longest_increasing_runs() {
        assert_eq!(longest_increasing(&[]), Vec::<usize>::new());
        assert_eq!(longest_increasing(&[0, 1, 2]), [0, 1, 2]);
        // One item moved from the front to the back.
        assert_eq!(longest_increasing(&[1, 2, 3, 0]), [1, 2, 3]);
        assert_eq!(longest_increasing(&[3, 0, 1, 2]), [0, 1, 2]);
        assert_eq!(longest_increasing(&[2, 1, 0]).len(), 1);
    }
}

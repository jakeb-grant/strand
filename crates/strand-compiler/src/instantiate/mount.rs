//! Mounting: lowered trees become scene nodes, watched prop memos,
//! structural effects, handlers and timers.

// `ValueKey` hashes and compares the value, never the pending future an
// `Async` may hold.
#![allow(clippy::mutable_key_type)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use strand_core::{Error, KeyedSource, Runtime};
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
/// A keyed list's diffs since a version (`None` when unchanged).
type ReadDiffs = Rc<
    dyn Fn(
        &Runtime,
        Option<u64>,
    ) -> Result<Option<(u64, Vec<strand_core::VecDiff<ValueKey, Value>>)>, Error>,
>;

/// Where a keyed list's items come from.
enum ListSource {
    /// A list expression with each item's key, published as keyed diffs
    /// by `rt.keyed_memo` (it compares whole lists when its inputs
    /// change).
    Memo(ListFn),
    /// A keyed collection (a keyed `state`, a service's keyed field):
    /// its own diffs, item by item.
    Keyed(strand_core::KeyedSignal<ValueKey, Value>),
}

/// One mounted item of a keyed list.
struct Item {
    key: ValueKey,
    frag: FragId,
    /// The item's value: set from `VecDiff::Update`, read by its bindings.
    cell: Option<strand_core::Signal<Value>>,
}

fn strip_key(
    d: strand_core::VecDiff<ValueKey, (ValueKey, Value)>,
) -> strand_core::VecDiff<ValueKey, Value> {
    use strand_core::VecDiff as D;
    match d {
        D::Reset { items } => D::Reset {
            items: items.into_iter().map(|(k, (_, v))| (k, v)).collect(),
        },
        D::Insert { index, key, value } => D::Insert {
            index,
            key,
            value: value.1,
        },
        D::Update { index, key, value } => D::Update {
            index,
            key,
            value: value.1,
        },
        D::Remove { index, key } => D::Remove { index, key },
        D::Move { from, to, key } => D::Move { from, to, key },
    }
}

/// A keyed list of instances to mount ([`Ctx::mount_keyed`]).
struct Keyed {
    /// The items with their keys, tracked.
    source: ListSource,
    /// The local each item's value is bound to.
    binding: Option<LocalId>,
    /// Elements each item owns.
    owned: Arc<std::collections::BTreeSet<crate::hir::NodeIdx>>,
    /// Mounts one item's content.
    item: ItemMount,
    /// What it is (`for x in … in bar`), and where, for its errors.
    what: String,
    at: (crate::source::FileId, crate::syntax::Span),
    /// Items that leave are parked, not unmounted (per-monitor bars).
    park: bool,
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
        let err = self.unlocated(what.into(), e);
        self.errors.borrow_mut().push(err);
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
                        // The caller's children have one set of element
                        // states in the caller's scope (its `id:` names
                        // read them). A component mounting `slot` again
                        // while that set is on screen gives the copy its
                        // own states, so hovering one copy is not hovering
                        // both.
                        let mut elements = std::collections::BTreeSet::new();
                        slot_elements(&children, &mut elements);
                        let in_use = elements.iter().any(|i| {
                            caller
                                .existing_node_state(*i)
                                .is_some_and(|st| st.scene.get().is_some())
                        });
                        let owned = if in_use {
                            Arc::new(elements)
                        } else {
                            Arc::default()
                        };
                        let caller = Env::child(caller, owned, None, None);
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
            if let Some(m) = self.async_let(rt, def, value, env) {
                rt.set_name(m.id(), self.vm.prog.def(def).name.as_str());
                env.bind_def(def, Slot::Memo(m));
                continue;
            }
            let m = rt.memo(move |rt| vm.eval(rt, value, &e));
            rt.set_name(m.id(), self.vm.prog.def(def).name.as_str());
            env.bind_def(def, Slot::Memo(m));
        }
        for s in states {
            self.declare_state(rt, s, env);
        }
    }

    /// `let hits = apps.search(query)`: a service method returning
    /// `Async`, as core's `rt.async_memo(args, fetch)` created on the
    /// `let`'s first read (so a closed launcher's search never runs). Each
    /// change of the arguments starts one [`ServiceHost::fetch`] and drops
    /// the superseded one; the value keeps its last result while pending.
    fn async_let(
        self: &Rc<Self>,
        rt: &Runtime,
        def: DefId,
        value: ChunkId,
        env: &Rc<Env>,
    ) -> Option<strand_core::Memo<Value>> {
        use crate::lower::Op;
        let prog = &self.vm.prog;
        if !matches!(prog.def(def).ty, Ty::Async(_)) {
            return None;
        }
        let chunk = prog.chunk(value);
        let (Some(Op::Service(_)), Some(Op::CallMethod { action: false, .. })) =
            (chunk.ops.first(), chunk.ops.last())
        else {
            return None;
        };
        let (vm, e) = (self.vm.clone(), env.clone());
        let owner = rt.current_owner();
        let weak = rt.downgrade();
        let cell: Rc<Cell<Option<strand_core::AsyncMemo<Value>>>> = Rc::default();
        Some(rt.memo(move |rt| {
            let am = match cell.get() {
                Some(am) => am,
                None => {
                    let (vm2, e2, w) = (vm.clone(), e.clone(), weak.clone());
                    let make = move |rt: &Runtime| {
                        let vm3 = vm2.clone();
                        rt.async_memo(
                            move |rt| {
                                let (recv, args) = vm2.eval_call_args(rt, value, &e2)?;
                                let Value::Service(s) = recv else {
                                    return Err(Error::failed("not a service call"));
                                };
                                let method = match vm2.prog.chunk(value).ops.last() {
                                    Some(Op::CallMethod { name, .. }) => {
                                        vm2.prog.chunk(value).names[*name as usize].clone()
                                    }
                                    _ => String::new(),
                                };
                                Ok((s, method, args))
                            },
                            move |(s, method, args): (Rc<str>, String, Vec<Value>)| {
                                let host = vm3.host.clone();
                                let rt = w.upgrade();
                                async move {
                                    let Some(rt) = rt else {
                                        return Err(Error::failed("the runtime is gone"));
                                    };
                                    let fut = host.fetch(&rt, &s, &method, args);
                                    drop(rt);
                                    fut.await
                                }
                            },
                        )
                    };
                    let am = rt.untrack(|rt| match owner {
                        Some(o) => rt.with_owner(o, make),
                        None => Ok(make(rt)),
                    })?;
                    cell.set(Some(am));
                    am
                }
            };
            let a = am.get(rt)?;
            Ok(Value::Async(Rc::new(crate::vm::value::AsyncValue {
                value: a.value().cloned(),
                pending: a.pending(),
                error: a.error().map(|e| e.to_string().into()),
                op: None,
            })))
        }))
    }

    pub(crate) fn declare_state(
        self: &Rc<Self>,
        rt: &Runtime,
        s: &crate::lower::State,
        env: &Rc<Env>,
    ) {
        let prog = self.vm.prog.clone();
        let info = prog.def(s.def);
        // `toasts.dnd` for a file's state, `Clock.open` for a component's
        // (or a surface's), qualified by the instance when there are
        // several: `TopBar[<monitor>].expanded`, `Row[<key>].open`.
        let owner = match info.owner {
            Some(o) => prog.def(o).name.clone(),
            None => info.module.clone(),
        };
        let path = format!("{owner}{}.{}", env.instance_path(), info.name);
        if let StateInit::Settings {
            path: file,
            record,
            fields,
        } = &s.init
        {
            self.declare_settings(rt, s.def, file, *record, fields, env, &path);
            return;
        }
        let StateInit::Value(c) = &s.init else {
            return;
        };
        let default = match rt.untrack(|rt| self.eval(rt, *c, env)) {
            Ok(v) => v,
            Err(e) => {
                self.error(format!("state `{}`", info.name), e);
                Value::Null
            }
        };
        // A keyed list (`state pins: [App] key id = []`) is a core keyed
        // collection: mutations are keyed operations and a `for` over it
        // follows its diffs.
        if let Ty::List(_, true) = &info.ty
            && !s.persist
        {
            let key = prog.state_keys.get(&s.def).cloned();
            let mut kv = crate::vm::value::keyed_vec(prog.clone(), |p| &p.types, key);
            let items = default.as_list().map(<[Value]>::to_vec).unwrap_or_default();
            if let Err(e) = kv.replace_all(items) {
                self.error(format!("state `{}`", info.name), e.into());
            }
            let k = rt.keyed(kv);
            rt.set_name(k.id(), path.as_str());
            env.bind_def(s.def, Slot::Keyed(k));
            return;
        }
        let sig = match (&self.storage.persist, s.persist) {
            (Some(store), true) => {
                let (pe, pd) = (prog.clone(), prog.clone());
                let (te, td) = (info.ty.clone(), info.ty.clone());
                let cell = rt.persisted(
                    store,
                    &path,
                    default,
                    move |v| crate::vm::persist::encode_bytes(&pe.types, &te, v),
                    move |b| crate::vm::persist::decode_bytes(&pd.types, &td, b),
                );
                if let strand_core::Restore::KeptOverNewDefault(_) = &cell.restored
                    && let Ok(v) = cell.signal.get_untracked(rt)
                {
                    self.notices.borrow_mut().push(format!(
                        "{path}: kept {} (default changed)",
                        v.show(&prog.types)
                    ));
                }
                let sig = cell.signal;
                // Kept for `@reset` (and the reconciler's `redeclare`).
                self.persisted.borrow_mut().push((path.clone(), cell));
                let weak = Rc::downgrade(self);
                rt.on_cleanup(move || {
                    if let Some(ctx) = weak.upgrade() {
                        ctx.persisted
                            .borrow_mut()
                            .retain(|(_, p)| p.signal.id() != sig.id());
                    }
                });
                sig
            }
            _ => rt.signal(default),
        };
        rt.set_name(sig.id(), path.as_str());
        env.bind_def(s.def, Slot::Signal(sig));
    }

    /// `state prefs from "prefs.toml" { typed fields }`: with a settings
    /// store, core's settings file (one `FieldSpec` per field: the
    /// declared default, decoded and written back as TOML by the field's
    /// type); without one, each field holds its default. Either way each
    /// field is its own signal and `prefs` reads as a record of them.
    #[allow(clippy::too_many_arguments)]
    fn declare_settings(
        self: &Rc<Self>,
        rt: &Runtime,
        def: DefId,
        file: &str,
        record: crate::ty::RecordId,
        defaults: &[Option<ChunkId>],
        env: &Rc<Env>,
        path: &str,
    ) {
        let prog = self.vm.prog.clone();
        let rec = prog.types.record(record).clone();
        let mut specs = Vec::with_capacity(rec.fields.len());
        for (f, d) in rec.fields.iter().zip(defaults) {
            let default = match d {
                Some(c) => match rt.untrack(|rt| self.eval(rt, *c, env)) {
                    Ok(v) => v,
                    Err(e) => {
                        self.error(format!("settings `{}.{}`", path, f.name), e);
                        crate::vm::schema_host::default_value(&prog.types, &f.ty)
                    }
                },
                None => crate::vm::schema_host::default_value(&prog.types, &f.ty),
            };
            specs.push((f.name.clone(), f.ty.clone(), default));
        }
        let resolved = self.storage.resolve(file);
        let (fields, handle) = match (&self.storage.settings, resolved) {
            (Some(store), Some(resolved)) => {
                let specs = specs
                    .into_iter()
                    .map(|(name, ty, default)| {
                        let (pd, pe) = (prog.clone(), prog.clone());
                        let (td, te) = (ty.clone(), ty.clone());
                        let shown = prog.types.show(&ty).to_string();
                        strand_core::FieldSpec::new(
                            name,
                            default,
                            move |item| crate::vm::persist::decode_item(&pd.types, &td, item),
                            move |v| crate::vm::persist::encode_item(&pe.types, &te, v),
                        )
                        .with_type(shown)
                    })
                    .collect();
                let handle = rt.settings_file(store, resolved, specs);
                let fields = handle
                    .signals()
                    .into_iter()
                    .map(|(n, s)| (n.to_string(), s))
                    .collect();
                (fields, Some(handle))
            }
            _ => (
                specs
                    .into_iter()
                    .map(|(name, _, default)| (name, rt.signal(default)))
                    .collect::<Vec<_>>(),
                None,
            ),
        };
        for (n, s) in &fields {
            rt.set_name(s.id(), format!("{path}.{n}"));
        }
        let slot = Rc::new(crate::vm::SettingsSlot { fields, handle });
        self.settings.borrow_mut().push(Rc::downgrade(&slot));
        let (sl, names): (Rc<crate::vm::SettingsSlot>, Vec<String>) = (
            slot.clone(),
            rec.fields.iter().map(|f| f.name.clone()).collect(),
        );
        let types = prog.clone();
        let memo = rt.memo(move |rt| {
            let mut values = Vec::with_capacity(names.len());
            for (n, f) in names.iter().zip(&types.types.record(record).fields) {
                values.push(match sl.field(n) {
                    Some(s) => s.get(rt)?,
                    None => crate::vm::schema_host::default_value(&types.types, &f.ty),
                });
            }
            Ok(Value::record(record, values))
        });
        rt.set_name(memo.id(), path);
        env.bind_settings(def, slot);
        env.bind_def(def, Slot::Memo(memo));
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
        self.mount_element_with(rt, e, env, parent, extra, None);
    }

    /// [`Ctx::mount_element`] for a surface whose body reads `services`:
    /// they are acquired while it is shown.
    pub(crate) fn mount_element_with(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        parent: FragId,
        extra: Vec<(SceneProp, PropValue)>,
        services: Option<&Arc<std::collections::BTreeSet<String>>>,
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
                    idx: e.node,
                    kind,
                    file: e.file,
                    span: e.span,
                    surface: kind.is_surface(),
                    parent: parent_scene,
                    state: state.clone(),
                    events: HashMap::new(),
                    two_way: Vec::new(),
                    bindings: Vec::new(),
                },
            );
            for (p, v) in &extra {
                em.set(id, *p, v.clone(), Transition::Default);
            }
        }
        for (prop, sources) in self.sources(rt, e, env) {
            // A prop the instance sets itself (a bar's monitor pin) wins.
            if extra.iter().any(|(p, _)| *p == prop) {
                continue;
            }
            self.bind_prop(rt, id, prop, sources, env, kind, (e.file, e.span));
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
        if kind.is_surface() {
            self.mount_surface_body(rt, e, env, frag, ec, services.cloned());
        } else {
            self.mount_nodes(rt, &e.children, env, frag, Some(&ec));
        }
    }

    /// A surface's children. Its own `on show`, `on hide` (and other
    /// element events) stay live; the rest is mounted the first time the
    /// surface is shown and frozen while it is hidden (state kept, nothing
    /// evaluated), and the services its body reads are acquired only
    /// while it is shown. The instance sends `show` when `open` turns true
    /// (or at mount for a surface without `open`) and `hide` when it turns
    /// false.
    fn mount_surface_body(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        frag: FragId,
        ec: ElemCtx,
        services: Option<Arc<std::collections::BTreeSet<String>>>,
    ) {
        let (own, content): (Vec<Node>, Vec<Node>) =
            e.children.iter().cloned().partition(
                |n| matches!(n, Node::Handler(h) if matches!(h.event, Event::Element(_))),
            );
        self.mount_nodes(rt, &Arc::new(own), env, frag, Some(&ec));
        let open = {
            let em = self.em.borrow();
            em.nodes.get(&ec.scene).and_then(|n| {
                n.bindings.iter().find_map(|b| {
                    em.bindings
                        .get(b)
                        .filter(|b| b.prop == SceneProp::Open)
                        .map(|b| b.memo)
                })
            })
        };
        let services = services.unwrap_or_default();
        let content = Arc::new(content);
        let inner = self.em.borrow_mut().new_frag(Some(frag), None);
        let (block, ()) = rt.scope(|_| ());
        let token = self.hold(rt, services);
        let mounted = Rc::new(Cell::new(false));
        let shown: Rc<Cell<Option<bool>>> = Rc::default();
        let (me, env) = (self.clone(), env.clone());
        let show = move |rt: &Runtime, is_open: bool| {
            let prev = shown.replace(Some(is_open));
            if prev == Some(is_open) {
                return;
            }
            if is_open {
                me.set_held(token, true);
                if !mounted.replace(true) {
                    let _ = rt.with_owner(block.id(), |rt| {
                        me.mount_nodes(rt, &content, &env, inner, Some(&ec));
                    });
                } else {
                    rt.resume(block.id());
                }
                me.route(rt, ec.scene, "show", Vec::new());
            } else {
                if mounted.get() {
                    let _ = rt.suspend(block.id());
                }
                me.set_held(token, false);
                if prev.is_some() {
                    me.route(rt, ec.scene, "hide", Vec::new());
                }
            }
        };
        match open {
            None => show(rt, true),
            Some(memo) => {
                let is_open = move |rt: &Runtime| {
                    memo.get(rt)
                        .map(|o| matches!(o.value, PropValue::Bool(true)))
                };
                if let Ok(o) = rt.untrack(|rt| is_open(rt)) {
                    show(rt, o);
                }
                let effect = rt.effect(move |rt| {
                    let o = is_open(rt)?;
                    rt.untrack(|rt| show(rt, o));
                    Ok(())
                });
                rt.set_name(effect.id(), "surface open");
            }
        }
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
    #[allow(clippy::too_many_arguments)]
    fn bind_prop(
        self: &Rc<Self>,
        rt: &Runtime,
        id: strand_scene::NodeId,
        prop: SceneProp,
        sources: Vec<Source>,
        env: &Rc<Env>,
        kind: NodeKind,
        at: (crate::source::FileId, crate::syntax::Span),
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
        let site = Rc::new(super::Site {
            what,
            file: at.0,
            span: at.1,
            node: Some(id),
            component: env.component,
            scope: env.fault_scope(),
        });
        match memo.get_untracked(rt) {
            Ok(out) => {
                let t = transitions.get(out.source).cloned().unwrap_or_default();
                self.em.borrow_mut().set(id, prop, out.value, t);
            }
            Err(e) => {
                let err = self.located(&site, e);
                self.errors.borrow_mut().push(err);
            }
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
                    site,
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
    /// scene, and each item's value is its own cell, so changing one item
    /// re-runs that item's bindings only. With `park`, an item that leaves
    /// is taken off the scene but kept, frozen, until it comes back (state
    /// and all) or is forgotten ([`Instance::forget_screen`]).
    fn mount_keyed(self: &Rc<Self>, rt: &Runtime, parent: FragId, env: &Rc<Env>, k: Keyed) {
        let Keyed {
            source,
            binding,
            owned,
            item,
            what,
            at,
            park,
        } = k;
        let frag = self.em.borrow_mut().new_frag(Some(parent), None);
        let (block, ()) = rt.scope(|_| ());
        let Ok(read) = rt.with_owner(block.id(), |rt| -> ReadDiffs {
            match source {
                ListSource::Memo(list) => {
                    let keyed =
                        rt.keyed_memo(|t: &(ValueKey, Value)| t.0.clone(), move |rt| list(rt));
                    rt.set_name(keyed.id(), what.as_str());
                    self.site(rt, keyed.id(), what.as_str(), at.0, at.1, env, None);
                    Rc::new(move |rt: &Runtime, since: Option<u64>| {
                        let snap = keyed.snapshot(rt)?;
                        if since == Some(snap.version()) {
                            return Ok(None);
                        }
                        let diffs = snap
                            .diffs_or_reset(since)
                            .into_iter()
                            .map(strip_key)
                            .collect();
                        Ok(Some((snap.version(), diffs)))
                    })
                }
                ListSource::Keyed(cell) => Rc::new(move |rt: &Runtime, since: Option<u64>| {
                    let snap = cell.snapshot(rt)?;
                    if since == Some(snap.version()) {
                        return Ok(None);
                    }
                    Ok(Some((snap.version(), snap.diffs_or_reset(since))))
                }),
            }
        }) else {
            return;
        };
        let items: Rc<RefCell<Vec<Item>>> = Rc::default();
        let parked: Rc<RefCell<HashMap<ValueKey, Item>>> = Rc::default();
        let version: Rc<Cell<Option<u64>>> = Rc::default();
        let mount_item = {
            let (ctx, env, owned, item) = (self.clone(), env.clone(), owned.clone(), item.clone());
            Rc::new(
                move |rt: &Runtime, at: usize, key: ValueKey, value: Value| -> Item {
                    let item_env = Env::child(&env, owned.clone(), None, None);
                    item_env.set_instance(key.0.show(&ctx.vm.prog.types));
                    let (ctx2, ie) = (ctx.clone(), item_env.clone());
                    let item = item.clone();
                    let cell: Rc<Cell<Option<strand_core::Signal<Value>>>> = Rc::default();
                    let c2 = cell.clone();
                    let f = rt
                        .with_owner(block.id(), |rt| {
                            ctx.mount_block(rt, frag, Some(at), |_, rt, f| {
                                ie.set_owner(rt.current_owner());
                                let value_cell = rt.signal(value.clone());
                                c2.set(Some(value_cell));
                                if let Some(b) = binding {
                                    ie.bind_local(b, Slot::Signal(value_cell));
                                }
                                item(&ctx2, rt, &ie, f, &value);
                            })
                        })
                        .unwrap_or(frag);
                    Item {
                        key,
                        frag: f,
                        cell: cell.get(),
                    }
                },
            )
        };
        let set_value = |rt: &Runtime, it: &Item, v: Value| {
            if let Some(c) = it.cell {
                let _ = c.set(rt, v);
            }
        };
        // An item that leaves: unmounted, or (`park`) taken off the scene
        // and frozen.
        let leave = {
            let (ctx, parked) = (self.clone(), parked.clone());
            Rc::new(move |rt: &Runtime, it: Item| {
                if !park {
                    ctx.unmount(rt, it.frag, false);
                    return;
                }
                let scope = ctx.em.borrow_mut().park(it.frag);
                if let Some(s) = scope {
                    ctx.park_services(rt, s.id(), true);
                    let _ = rt.suspend(s.id());
                }
                parked.borrow_mut().insert(it.key.clone(), it);
            })
        };
        // An item that arrives: a parked one comes back, else a new one.
        let arrive = {
            let (ctx, parked, mount_item) = (self.clone(), parked.clone(), mount_item.clone());
            Rc::new(
                move |rt: &Runtime, at: usize, key: ValueKey, value: Value| -> Item {
                    let back = parked.borrow_mut().remove(&key);
                    match back {
                        Some(it) => {
                            let scope = ctx.em.borrow_mut().unpark(it.frag, at);
                            set_value(rt, &it, value);
                            if let Some(s) = scope {
                                ctx.park_services(rt, s.id(), false);
                                rt.resume(s.id());
                            }
                            it
                        }
                        None => mount_item(rt, at, key, value),
                    }
                },
            )
        };
        if park {
            let (ctx, parked) = (self.clone(), parked.clone());
            let forget: Rc<super::Forget> = Rc::new(move |rt: &Runtime, key: &Value| {
                let gone = parked.borrow_mut().remove(&ValueKey(key.clone()));
                match gone {
                    Some(it) => {
                        ctx.unmount(rt, it.frag, false);
                        true
                    }
                    None => false,
                }
            });
            self.forgetters.borrow_mut().push(Rc::downgrade(&forget));
            // The list's scope keeps its forgetter alive.
            rt.on_cleanup(move || drop(forget));
        }
        let apply = {
            let (ctx, items) = (self.clone(), items.clone());
            Rc::new(
                move |rt: &Runtime, diffs: Vec<strand_core::VecDiff<ValueKey, Value>>| {
                    for d in diffs {
                        match d {
                            strand_core::VecDiff::Reset { items: new } => {
                                let old: Vec<Item> = items.borrow_mut().drain(..).collect();
                                let placed = ctx.reconcile(
                                    rt,
                                    frag,
                                    old,
                                    new,
                                    |rt, it, v| set_value(rt, it, v),
                                    |rt, it| leave(rt, it),
                                    |rt, at, k, v| arrive(rt, at, k, v),
                                );
                                *items.borrow_mut() = placed;
                            }
                            strand_core::VecDiff::Insert { index, key, value } => {
                                let it = arrive(rt, index, key, value);
                                let mut its = items.borrow_mut();
                                let at = index.min(its.len());
                                its.insert(at, it);
                            }
                            strand_core::VecDiff::Remove { index, .. } => {
                                let removed = {
                                    let mut its = items.borrow_mut();
                                    (index < its.len()).then(|| its.remove(index))
                                };
                                if let Some(it) = removed {
                                    leave(rt, it);
                                }
                            }
                            strand_core::VecDiff::Move { from, to, .. } => {
                                let moved = {
                                    let mut its = items.borrow_mut();
                                    if from < its.len() {
                                        let x = its.remove(from);
                                        let to = to.min(its.len());
                                        let f = x.frag;
                                        its.insert(to, x);
                                        Some((f, to))
                                    } else {
                                        None
                                    }
                                };
                                if let Some((f, to)) = moved {
                                    ctx.em.borrow_mut().move_frag(f, to);
                                }
                            }
                            // One item's new value: its cell only.
                            strand_core::VecDiff::Update { index, value, .. } => {
                                let cell = items.borrow().get(index).and_then(|it| it.cell);
                                if let Some(c) = cell {
                                    let _ = c.set(rt, value);
                                }
                            }
                        }
                    }
                },
            )
        };
        let r = read.clone();
        match rt.untrack(|rt| r(rt, None)) {
            Ok(Some((v, diffs))) => {
                apply(rt, diffs);
                version.set(Some(v));
            }
            Ok(None) => {}
            Err(e) => self.error(what.as_str(), e),
        }
        let env2 = env.clone();
        let _ = rt.with_owner(block.id(), |rt| {
            let effect = rt.effect(move |rt| {
                if let Some((v, diffs)) = read(rt, version.get())? {
                    rt.untrack(|rt| apply(rt, diffs));
                    version.set(Some(v));
                }
                Ok(())
            });
            rt.set_name(effect.id(), what.as_str());
            self.site(rt, effect.id(), what.as_str(), at.0, at.1, &env2, None);
        });
    }

    /// A whole new list (a first publish, or a reader that fell behind
    /// the diff log) against the mounted items, by key: items that left
    /// leave (`leave`), new ones arrive (`arrive`), kept ones take their
    /// new value (`update`), and only the items outside the longest run
    /// already in order are moved, so identity and state are kept and a
    /// list where one item moved sends one `Move`.
    #[allow(clippy::too_many_arguments)]
    fn reconcile(
        self: &Rc<Self>,
        rt: &Runtime,
        frag: FragId,
        old: Vec<Item>,
        new: Vec<(ValueKey, Value)>,
        update: impl Fn(&Runtime, &Item, Value),
        leave: impl Fn(&Runtime, Item),
        mut arrive: impl FnMut(&Runtime, usize, ValueKey, Value) -> Item,
    ) -> Vec<Item> {
        let wanted: std::collections::HashSet<&ValueKey> = new.iter().map(|(k, _)| k).collect();
        let mut kept: HashMap<ValueKey, (usize, Item)> = HashMap::new();
        for it in old {
            if wanted.contains(&it.key) && !kept.contains_key(&it.key) {
                let at = kept.len();
                kept.insert(it.key.clone(), (at, it));
            } else {
                leave(rt, it);
            }
        }
        // Old positions of the kept items, in new order; the longest
        // increasing run of them stays put.
        let seq: Vec<usize> = new
            .iter()
            .filter_map(|(k, _)| kept.get(k).map(|(i, _)| *i))
            .collect();
        let stay: std::collections::HashSet<usize> = longest_increasing(&seq).into_iter().collect();
        let mut out: Vec<Item> = Vec::with_capacity(new.len());
        let position = |ctx: &Rc<Ctx>, f: FragId| {
            ctx.em
                .borrow()
                .frag(frag)
                .and_then(|p| p.children.iter().position(|c| *c == f))
        };
        for (k, v) in new {
            // Right after the previous item, wherever that is now.
            let prev = out.last().and_then(|it| position(self, it.frag));
            let it = match kept.remove(&k) {
                Some((i, it)) => {
                    if !stay.contains(&i) {
                        // `move_frag` counts positions without the moved
                        // item.
                        let at = match (prev, position(self, it.frag)) {
                            (None, _) => 0,
                            (Some(p), Some(cur)) if cur < p => p,
                            (Some(p), _) => p + 1,
                        };
                        self.em.borrow_mut().move_frag(it.frag, at);
                    }
                    update(rt, &it, v);
                    it
                }
                None => arrive(rt, prev.map_or(0, |p| p + 1), k, v),
            };
            out.push(it);
        }
        out
    }

    fn mount_for(self: &Rc<Self>, rt: &Runtime, f: &For, env: &Rc<Env>, parent: FragId) {
        let source = match self.keyed_source(rt, f, env) {
            Some(k) => ListSource::Keyed(k),
            None => {
                let (ctx, e) = (self.clone(), env.clone());
                let (iter, key, binding) = (f.iter, f.key.clone(), f.binding);
                ListSource::Memo(Box::new(move |rt| {
                    let v = ctx.eval(rt, iter, &e)?;
                    let types = &ctx.vm.prog.types;
                    let items = v.as_list().unwrap_or(&[]);
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let k = match &key {
                            ForKey::Path(p) => {
                                item.key_path(types, p).cloned().unwrap_or(Value::Null)
                            }
                            ForKey::Value => item.clone(),
                            ForKey::Expr(c) => {
                                ctx.vm
                                    .eval_with(rt, *c, &e, vec![(binding, item.clone())])?
                            }
                        };
                        out.push((ValueKey(k), item.clone()));
                    }
                    Ok(out)
                }))
            }
        };
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
                source,
                binding: Some(f.binding),
                owned: f.body.owned.clone(),
                item,
                what: format!(
                    "for {} in … in {}",
                    self.vm.prog.local(f.binding).name,
                    self.module_of(f.file)
                ),
                at: (f.file, f.span),
                park: false,
            },
        );
    }

    /// The keyed collection a `for` iterates directly, if it does: a bare
    /// keyed `state` (`for p in pins`) or a service's keyed field (`for n
    /// in notifications.popups`), with the collection's own key. Then the
    /// loop follows the collection's diffs; any other expression (`xs.filter(…)`)
    /// is compared as a whole list when it changes.
    fn keyed_source(
        &self,
        rt: &Runtime,
        f: &For,
        env: &Rc<Env>,
    ) -> Option<strand_core::KeyedSignal<ValueKey, Value>> {
        use crate::lower::Op;
        let prog = &self.vm.prog;
        let chunk = prog.chunk(f.iter);
        match chunk.ops.as_slice() {
            [Op::Def(d)] => {
                let Some(Slot::Keyed(k)) = env.def(*d) else {
                    return None;
                };
                let own = prog.state_keys.get(d).cloned().or_else(|| {
                    match prog
                        .def(*d)
                        .ty
                        .list_elem()
                        .map(|(t, _)| t.non_null().clone())
                    {
                        Some(Ty::Record(r)) => prog.types.record(r).key.clone(),
                        _ => None,
                    }
                });
                let same = match (&f.key, own) {
                    (ForKey::Path(p), Some(o)) => *p == o,
                    (ForKey::Value, None) => true,
                    _ => false,
                };
                same.then_some(k)
            }
            [Op::Service(s), Op::Field(n)] => {
                if !matches!(f.key, ForKey::Path(_)) {
                    return None;
                }
                self.vm
                    .host
                    .read_keyed(rt, &chunk.names[*s as usize], &chunk.names[*n as usize])
            }
            _ => None,
        }
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
    pub(crate) fn acquire(
        self: &Rc<Self>,
        rt: &Runtime,
        services: &std::collections::BTreeSet<String>,
    ) {
        let token = self.hold(rt, Arc::new(services.clone()));
        self.set_held(token, true);
    }

    /// Register the services a scope (the current owner) reads, not yet
    /// held; released when the scope goes. [`Ctx::set_held`] holds them
    /// (a surface shown) or lets go (hidden), and a parked bar lets go of
    /// everything under it ([`Ctx::park_services`]).
    pub(crate) fn hold(
        self: &Rc<Self>,
        rt: &Runtime,
        services: Arc<std::collections::BTreeSet<String>>,
    ) -> u64 {
        let token = self.next_hold.get();
        self.next_hold.set(token + 1);
        self.holds.borrow_mut().insert(
            token,
            super::Hold {
                scope: rt.current_owner(),
                services,
                held: false,
                parked: false,
            },
        );
        let weak = Rc::downgrade(self);
        rt.on_cleanup(move || {
            if let Some(ctx) = weak.upgrade() {
                let gone = ctx.holds.borrow_mut().remove(&token);
                if let Some(h) = gone
                    && h.held
                {
                    for s in h.services.iter() {
                        ctx.vm.host.release(s);
                    }
                }
            }
        });
        token
    }

    /// Hold (`true`) or let go of a registered scope's services.
    pub(crate) fn set_held(&self, token: u64, on: bool) {
        let mut holds = self.holds.borrow_mut();
        let Some(h) = holds.get_mut(&token) else {
            return;
        };
        if h.parked {
            // Parked: it takes the visibility on return.
            h.parked = on;
            return;
        }
        if h.held == on {
            return;
        }
        h.held = on;
        for s in h.services.iter() {
            if on {
                self.vm.host.acquire(s);
            } else {
                self.vm.host.release(s);
            }
        }
    }

    /// A bar parked (`true`) or back: every scope under `root` lets go of
    /// its services, and takes them again on return.
    pub(crate) fn park_services(&self, rt: &Runtime, root: strand_core::NodeId, park: bool) {
        let mut holds = self.holds.borrow_mut();
        for h in holds.values_mut() {
            let Some(scope) = h.scope else { continue };
            let mut cur = Some(scope);
            let mut under = false;
            while let Some(c) = cur {
                if c == root {
                    under = true;
                    break;
                }
                cur = rt.owner_of(c).ok().flatten();
            }
            if !under {
                continue;
            }
            match (park, h.held, h.parked) {
                (true, true, _) => {
                    h.held = false;
                    h.parked = true;
                    for s in h.services.iter() {
                        self.vm.host.release(s);
                    }
                }
                (false, _, true) => {
                    h.parked = false;
                    h.held = true;
                    for s in h.services.iter() {
                        self.vm.host.acquire(s);
                    }
                }
                _ => {}
            }
        }
    }

    /// A surface: once, or for a `bar`, once per monitor of the `screens`
    /// service (those its own `screens:` picks) with `screen` in scope,
    /// pinned to that monitor with `screens: "<monitor id>"`. Bars are
    /// keyed by the monitor's identity (make, model and description), and
    /// one whose monitor goes is parked with its state until the monitor
    /// comes back or is forgotten.
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
                ctx.declare(rt, &element.children, &env);
                ctx.mount_element_with(rt, &element, &env, frag, extra, Some(&services));
            });
            return;
        };
        // The bar's own `screens:` (`"DP-1"`, a list, `focused`, `all`)
        // picks the monitors that get one.
        let pick = element
            .props
            .iter()
            .find(|p| p.prop == Some(SceneProp::Screens))
            .map(|p| p.value);
        let (ctx, outer) = (self.clone(), env.clone());
        let list: ListFn = Box::new(move |rt| {
            let all = ctx.vm.host.read(rt, "screens", "all")?;
            let types = &ctx.vm.prog.types;
            let wanted = match pick {
                Some(c) => Some(ctx.eval(rt, c, &outer)?),
                None => None,
            };
            let focused = match &wanted {
                Some(Value::Enum(e, i))
                    if types
                        .enum_(*e)
                        .variants
                        .get(*i as usize)
                        .map(String::as_str)
                        == Some("focused") =>
                {
                    Some(ctx.vm.host.read(rt, "screens", "focused")?)
                }
                _ => None,
            };
            let names: Option<Vec<String>> = match &wanted {
                Some(Value::Text(t)) => Some(vec![t.to_string()]),
                Some(Value::List(items)) => Some(
                    items
                        .iter()
                        .filter_map(|v| v.as_text().map(str::to_string))
                        .collect(),
                ),
                _ => None,
            };
            let mut out = Vec::new();
            for sc in all.as_list().unwrap_or(&[]) {
                let key = screen_key(types, sc);
                let keep = match (&focused, &names) {
                    (Some(f), _) => !f.is_null() && screen_key(types, f) == key,
                    (None, Some(names)) => names.iter().any(|n| {
                        sc.field(types, "name").and_then(Value::as_text) == Some(n.as_str())
                            || sc.field(types, "id").and_then(Value::as_text) == Some(n.as_str())
                    }),
                    (None, None) => true,
                };
                if keep {
                    out.push((ValueKey(Value::text(key)), sc.clone()));
                }
            }
            Ok(out)
        });
        let item: ItemMount = Rc::new(move |ctx, rt, env, frag, v| {
            let mut extra = extra.clone();
            // The instance's own pin wins over the bar's `screens:`.
            extra.push((
                SceneProp::Screens,
                PropValue::Text(screen_key(&ctx.vm.prog.types, v)),
            ));
            ctx.declare(rt, &element.children, env);
            ctx.mount_element_with(rt, &element, env, frag, extra, Some(&services));
        });
        self.mount_keyed(
            rt,
            parent,
            env,
            Keyed {
                source: ListSource::Memo(list),
                binding: Some(screen),
                owned: s.body.owned.clone(),
                item,
                what: "bar on every screen".to_string(),
                at: (s.element.file, s.element.span),
                park: true,
            },
        );
    }

    // -----------------------------------------------------------------------
    // Handlers and timers

    fn event_ctx(el: Option<&ElemCtx>, event: &str, args: Vec<Value>) -> Option<Rc<EventCtx>> {
        Some(Rc::new(EventCtx::new(
            el.map(|e| e.state.clone()),
            el.map(|e| e.scene),
            event,
            args,
        )))
    }

    /// A handler body's future, reporting its error (located) instead of
    /// returning it.
    pub(crate) fn reporting(
        self: &Rc<Self>,
        site: Rc<super::Site>,
        fut: impl std::future::Future<Output = Result<(), Error>> + 'static,
    ) -> impl std::future::Future<Output = Result<(), Error>> + 'static {
        let weak = Rc::downgrade(self);
        async move {
            match fut.await {
                Err(e) if e != Error::Cancelled => {
                    if let Some(ctx) = weak.upgrade() {
                        let err = ctx.located(&site, e);
                        ctx.errors.borrow_mut().push(err);
                    }
                    Ok(())
                }
                r => r,
            }
        }
    }

    pub(crate) fn mount_handler(
        self: &Rc<Self>,
        rt: &Runtime,
        h: &Handler,
        env: &Rc<Env>,
        el: Option<&ElemCtx>,
    ) {
        let (vm, body, params) = (self.vm.clone(), h.body, h.params.clone());
        let file = self.vm.prog.chunk(body).file;
        match &h.event {
            Event::Element(name) => {
                let Some(el) = el else { return };
                let q = rt.input_events::<Rc<EventCtx>>();
                let site = rt.handler_site();
                let loc = self.site(
                    rt,
                    site,
                    format!("on {name}"),
                    file,
                    h.span,
                    env,
                    Some(el.scene),
                );
                let (e, me) = (env.clone(), self.clone());
                let r = q.on(rt, move |rt, ctx| {
                    let frame: Frame = params
                        .iter()
                        .copied()
                        .zip(ctx.args.iter().cloned())
                        .collect();
                    let fut = vm.handler(rt, body, e.clone(), frame, Some(ctx.clone()));
                    rt.spawn_input(Some(site), me.reporting(loc.clone(), fut));
                    Ok(())
                });
                if r.is_ok() {
                    let mut em = self.em.borrow_mut();
                    if let Some(entry) = em.nodes.get_mut(&el.scene) {
                        // Several `on click` on one element all run, in
                        // source order.
                        entry.events.entry(name.clone()).or_default().push(q);
                    }
                }
            }
            Event::Service { service, event } => {
                let Some(q) = self.vm.host.event(rt, service, event) else {
                    return;
                };
                let site = rt.handler_site();
                let ev = format!("{service}.{event}");
                let loc = self.site(
                    rt,
                    site,
                    format!("on {ev}"),
                    file,
                    h.span,
                    env,
                    el.map(|e| e.scene),
                );
                let (e, me) = (env.clone(), self.clone());
                let el = el.cloned();
                let _ = q.on(rt, move |rt, args: &Vec<Value>| {
                    let frame: Frame = params.iter().copied().zip(args.iter().cloned()).collect();
                    let ctx = Ctx::event_ctx(el.as_ref(), &ev, args.clone());
                    let fut = vm.handler(rt, body, e.clone(), frame, ctx);
                    rt.spawn_for(site, me.reporting(loc.clone(), fut));
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
                let loc = Rc::new(super::Site {
                    what: "on change".into(),
                    file,
                    span: h.span,
                    node: el.map(|e| e.scene),
                    component: env.component,
                    scope: env.fault_scope(),
                });
                let (e, me, l) = (env.clone(), self.clone(), loc.clone());
                let el = el.cloned();
                let start = move |rt: &Runtime| {
                    let ctx = Ctx::event_ctx(el.as_ref(), "change", Vec::new());
                    let fut = vm.handler(rt, body, e.clone(), Frame::new(), ctx);
                    rt.spawn(me.reporting(l.clone(), fut));
                };
                let id = match debounce {
                    None => rt
                        .on_change_keyed(key, track, move |rt, _| {
                            start(rt);
                            Ok(())
                        })
                        .id(),
                    Some(d) => {
                        // The debounce follows a reactive duration: a new
                        // duration replaces the debounce, taking over a
                        // countdown in flight (`rescale_from`).
                        let d = *d;
                        let (ctx, e) = (self.clone(), env.clone());
                        let owner = rt.current_owner();
                        let (key, track, start) = (Rc::new(key), Rc::new(track), Rc::new(start));
                        let current: Rc<
                            Cell<Option<(std::time::Duration, strand_core::Debounced)>>,
                        > = Rc::default();
                        let effect = rt.effect(move |rt| {
                            let delay = ctx.eval(rt, d, &e)?.as_duration().ok_or_else(|| {
                                Error::failed("`on change … after` needs a duration from 0 up")
                            })?;
                            if current.get().is_some_and(|(cur, _)| cur == delay) {
                                return Ok(());
                            }
                            let (k, t, s) = (key.clone(), track.clone(), start.clone());
                            let make = move |rt: &Runtime| {
                                rt.on_change_after_keyed(
                                    move |rt| k(rt),
                                    move |rt| t(rt),
                                    delay,
                                    move |rt| {
                                        s(rt);
                                        Ok(())
                                    },
                                )
                            };
                            let new = rt.untrack(|rt| match owner {
                                Some(o) => rt.with_owner(o, make),
                                None => Ok(make(rt)),
                            })?;
                            if let Some((_, old)) = current.take() {
                                let _ = new.rescale_from(rt, old);
                                old.dispose(rt);
                            }
                            current.set(Some((delay, new)));
                            Ok(())
                        });
                        effect.id()
                    }
                };
                rt.set_name(id, "on change");
                self.sites.borrow_mut().insert(id, loc);
                let weak = Rc::downgrade(self);
                rt.on_cleanup(move || {
                    if let Some(ctx) = weak.upgrade() {
                        ctx.sites.borrow_mut().remove(&id);
                    }
                });
            }
        }
    }

    fn mount_timer(self: &Rc<Self>, rt: &Runtime, t: &Timer, env: &Rc<Env>, el: Option<&ElemCtx>) {
        let (ctx, e, duration, kind) = (self.clone(), env.clone(), t.duration, t.kind);
        let word = match kind {
            TimerKind::After => "after",
            TimerKind::Every => "every",
        };
        let dur = move |rt: &Runtime| {
            let v = ctx.eval(rt, duration, &e)?;
            match v.as_duration() {
                // A zero period would wake the host forever: an error
                // value naming the timer, not a silent pause.
                Some(d) if d.is_zero() && kind == TimerKind::Every => {
                    Err(Error::failed("`every` needs a positive period, not 0s"))
                }
                Some(d) => Ok(d),
                None => Err(Error::failed(format!(
                    "`{word}` needs a duration from 0 to {} s, not {}",
                    std::time::Duration::MAX.as_secs(),
                    v.show(&ctx.vm.prog.types)
                ))),
            }
        };
        let (ctx, e, cond) = (self.clone(), env.clone(), t.while_);
        let cond = move |rt: &Runtime| match cond {
            Some(c) => Ok(ctx.eval(rt, c, &e)?.truthy()),
            None => Ok(true),
        };
        let name = format!("{word} timer in {}", self.module_of(t.file));
        let scene = el.map(|e| e.scene);
        let loc = Rc::new(super::Site {
            what: name.as_str().into(),
            file: t.file,
            span: t.span,
            node: scene,
            component: env.component,
            scope: env.fault_scope(),
        });
        let (vm, e, body, me, l) = (
            self.vm.clone(),
            env.clone(),
            t.body,
            self.clone(),
            loc.clone(),
        );
        let el = el.cloned();
        let run = move |rt: &Runtime| {
            let ctx = Ctx::event_ctx(el.as_ref(), "timer", Vec::new());
            let fut = vm.handler(rt, body, e.clone(), Frame::new(), ctx);
            rt.spawn(me.reporting(l.clone(), fut));
            Ok(())
        };
        let timer = match t.kind {
            TimerKind::After => rt.after_dyn(dur, cond, run),
            TimerKind::Every => rt.every_dyn(dur, cond, run),
        };
        rt.set_name(timer.id(), name.as_str());
        self.site(rt, timer.id(), name, t.file, t.span, env, scene);
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

/// A monitor's identity as the bars know it: its `id` (make, model and
/// description), or its connector name when the host gives no id.
fn screen_key(types: &crate::ty::TypeTable, sc: &Value) -> String {
    match sc.field(types, "id").and_then(Value::as_text) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => sc
            .field(types, "name")
            .and_then(Value::as_text)
            .unwrap_or("")
            .to_string(),
    }
}

/// The elements of a component call's children that the caller's scope
/// owns (not inside `for` items, which own theirs).
fn slot_elements(nodes: &[Node], out: &mut std::collections::BTreeSet<crate::hir::NodeIdx>) {
    for n in nodes {
        match n {
            Node::Element(e) => {
                out.insert(e.node);
                slot_elements(&e.children, out);
            }
            Node::If { then, else_, .. } => {
                slot_elements(then, out);
                slot_elements(else_, out);
            }
            Node::Match { arms, .. } => {
                for a in arms {
                    slot_elements(a, out);
                }
            }
            _ => {}
        }
    }
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

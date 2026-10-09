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

/// How far past its threshold a container query that holds must move
/// before it stops holding, logical pixels (design.md: 4 px hysteresis).
pub const QUERY_HYSTERESIS: f64 = 4.0;

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

/// Which branch of a [`Switch`] to show (`None`: none).
type Pick = Rc<dyn Fn(&Runtime) -> Result<Option<usize>, Error>>;

/// Branches mounted one at a time ([`Ctx::mount_branches`]): an `if`, a
/// `match`, an on-demand element.
struct Switch {
    pick: Pick,
    branches: Vec<Arc<Vec<Node>>>,
    /// Declare the branch's own `let`s and `state`s in its scope and
    /// mount its nodes; else the branch is one on-demand element, mounted
    /// as it is (its `let`s and `state`s belong to the scope around it).
    declare: bool,
    /// Its place in the instance tree.
    tag: String,
    /// What it is, for its errors.
    what: String,
    /// What `pick` reads: chunks in the scope, and nodes directly.
    reads: (Vec<ChunkId>, Vec<strand_core::NodeId>),
    at: (crate::source::FileId, crate::syntax::Span),
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
    /// An incremental view over a keyed collection (`xs.filter(…)`):
    /// its own diffs, item by item.
    Derived(strand_core::KeyedMemo<ValueKey, Value>),
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
    /// Its place in the instance tree (`f12`: a `for`, `s3`: a bar).
    tag: String,
    /// What a [`ListSource::Memo`] reads: chunks evaluated in the list's
    /// scope, and nodes it reads directly (declared edges).
    reads: (Vec<ChunkId>, Vec<strand_core::NodeId>),
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
            Node::Element(e) => match e.kind {
                // Mounted only while shown (the schema's `on_demand`).
                ElementKind::Builtin(NodeKind::Page | NodeKind::Tooltip) => {
                    self.mount_on_demand(rt, e, env, frag)
                }
                _ => self.mount_element(rt, e, env, frag, Vec::new()),
            },
            Node::Surface(s) => self.mount_surface(rt, s, env, frag),
            Node::If {
                cond,
                then,
                else_,
                file,
                span,
            } => self.mount_switch(
                rt,
                *cond,
                vec![then.clone(), else_.clone()],
                true,
                env,
                frag,
                (*file, *span),
            ),
            Node::Match {
                selector,
                arms,
                file,
                span,
            } => self.mount_switch(
                rt,
                *selector,
                arms.clone(),
                false,
                env,
                frag,
                (*file, *span),
            ),
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
                        caller.set_ident(format!("{}/slot", e.ident()));
                        self.mount_block(rt, frag, None, |ctx, rt, f| {
                            caller.set_owner(rt.current_owner());
                            ctx.note_env(&caller);
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
        let mut made = Vec::with_capacity(lets.len());
        for (def, value) in lets {
            let (vm, e) = (self.vm.clone(), env.clone());
            let m = match self.async_let(rt, def, value, env) {
                Some(m) => m,
                None => rt.memo(move |rt| vm.eval(rt, value, &e)),
            };
            rt.set_name(m.id(), self.vm.prog.def(def).name.as_str());
            env.bind_def(def, Slot::Memo(m));
            made.push((def, m, value));
        }
        for s in states {
            self.declare_state(rt, s, env);
        }
        // View `let`s (`let shown = notifications.popups.filter(…)
        // .take(5)`) become core's incremental views now that the keyed
        // states they start from are bound, a view over another view of
        // this body after it; the plain memo bound above stays only for a
        // view this cannot build. A view declares its own edges.
        let prog = self.vm.prog.clone();
        let mut pending: Vec<DefId> = made
            .iter()
            .map(|(d, _, _)| *d)
            .filter(|d| prog.let_chains.contains_key(d))
            .collect();
        let mut viewed = Vec::new();
        while !pending.is_empty() {
            let ready: Vec<DefId> = pending
                .iter()
                .copied()
                .filter(|d| match prog.let_chains.get(d).map(|c| &c.root) {
                    Some(crate::lower::ChainRoot::Def(r)) => !pending.contains(r),
                    _ => true,
                })
                .collect();
            if ready.is_empty() {
                break;
            }
            pending.retain(|d| !ready.contains(d));
            for d in ready {
                if let Some(view) = self.let_view(rt, d, env) {
                    let list = rt.memo(move |rt| view.with(rt, crate::vm::value::list_of_items));
                    let _ = rt.reads_from(list.id(), &[view.id()]);
                    rt.set_name(list.id(), prog.def(d).name.as_str());
                    env.bind_def(d, Slot::View(view, list));
                    viewed.push(d);
                }
            }
        }
        // Every name of the body is bound now: declare what each `let`
        // reads (a `let` may read one declared after it).
        for (d, id, value) in made {
            if viewed.contains(&d) {
                // Never read: the view took its place.
                id.dispose(rt);
                continue;
            }
            self.declare_reads(rt, id.id(), &[value], env, &[]);
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
        // `let hits = apps.search(q)` lowers to the call's site: the
        // `let` is that load.
        let value = match prog.chunk(value).ops.as_slice() {
            [Op::AsyncSite(site)] => *site,
            _ => value,
        };
        let chunk = prog.chunk(value);
        let (Some(Op::Service(_)), Some(Op::CallMethod { action: false, .. })) =
            (chunk.ops.first(), chunk.ops.last())
        else {
            return None;
        };
        let env_for_reads = env.clone();
        // The chunk's own reads go on the load's effect.
        let me = Rc::downgrade(self);
        let declare: crate::vm::DeclareLoad = Rc::new(move |rt, effect| {
            if let Some(ctx) = me.upgrade() {
                ctx.declare_reads(rt, effect, &[value], &env_for_reads, &[]);
            }
        });
        Some(
            self.vm
                .async_load(rt, value, env, rt.current_owner(), declare),
        )
    }

    pub(crate) fn declare_state(
        self: &Rc<Self>,
        rt: &Runtime,
        s: &crate::lower::State,
        env: &Rc<Env>,
    ) {
        use super::reload::CellRec;
        use crate::reconcile::EditClass;
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
        // The cell a reload keeps: same place in the instance tree, same
        // owner and name. A surface's scope is its own identity already,
        // and its name is its namespace (`strand-<Name>`): renaming it
        // recreates the surface with its state kept (design.md, "What
        // each edit does"), so its cells are keyed without it.
        let scope_key = env.ident();
        let surface_owned = info
            .owner
            .is_some_and(|o| matches!(prog.def(o).kind, crate::hir::DefKind::Surface(_)));
        let key: Rc<str> = match surface_owned {
            true => format!("{scope_key}#.{}", info.name).into(),
            false => format!("{scope_key}#{owner}.{}", info.name).into(),
        };
        // A closed popup's content opening again: its cell, as it was.
        if self.closed.borrow_mut().remove(&key) && self.reopen_cell(rt, s, &key, env, &path) {
            return;
        }
        // The old cell and the program it was made by (a reload's old
        // program, or an older one for a bar parked across reloads).
        let carried = self
            .take_cell(&key)
            .or_else(|| self.pending.borrow_mut().remove(&key));
        let from = carried.as_ref().map(|c| c.1.clone());
        let carried = carried.map(|c| c.0);
        let ty = prog.types.show(&info.ty).to_string();
        if let StateInit::Settings {
            path: file,
            record,
            fields,
        } = &s.init
        {
            self.declare_settings(
                rt,
                s.def,
                file,
                *record,
                fields,
                env,
                &path,
                key,
                carried.zip(from),
            );
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
        // What a reload does with the old cell: keep it (same type),
        // reset it (`@reset`, a new type), or nothing was there.
        let mut start: Option<Value> = None;
        let mut keep: Option<CellRec> = None;
        match carried {
            None => self.fresh_cell(scope_key.clone(), info.name.clone()),
            Some(rec) => {
                let (old_ty, why) = match &rec {
                    CellRec::Plain { ty, .. } | CellRec::Keyed { ty, .. } => (ty.clone(), None),
                    CellRec::Settings { .. } => (String::new(), Some("now a plain state")),
                };
                let why = if s.reset {
                    Some("@reset".to_string())
                } else if let Some(w) = why {
                    Some(w.to_string())
                } else if old_ty != ty {
                    Some(format!("type changed ({old_ty} → {ty})"))
                } else {
                    None
                };
                match why {
                    Some(why) => {
                        self.report(|r| {
                            r.class(EditClass::StateReset);
                            r.reset.push((path.clone(), why));
                        });
                        // A persisted cell reset by `@reset` forgets what
                        // it stored.
                        if s.reset
                            && let CellRec::Plain {
                                persisted: Some(p), ..
                            } = &rec
                        {
                            let _ = p.reset(rt);
                        }
                    }
                    None => {
                        let (cur, old_default) = match &rec {
                            CellRec::Plain { sig, default, .. } => {
                                (sig.get_untracked(rt).ok(), default.clone())
                            }
                            CellRec::Keyed { list, default, .. } => {
                                (list.get_untracked(rt).ok(), default.clone())
                            }
                            CellRec::Settings { .. } => (None, Value::Null),
                        };
                        match cur.zip(from.as_ref()).and_then(|(cur, from)| {
                            self.adopt_value(&cur, &old_default, &default, &path, from)
                        }) {
                            Some(v) => {
                                self.report(|r| r.kept.push(path.clone()));
                                start = Some(v);
                                keep = Some(rec);
                            }
                            None => self.report(|r| {
                                r.class(EditClass::StateReset);
                                r.reset
                                    .push((path.clone(), "value does not fit".to_string()));
                            }),
                        }
                    }
                }
            }
        }
        // A keyed list (`state pins: [App] key id = []`) is a core keyed
        // collection: mutations are keyed operations and a `for` over it
        // follows its diffs. A reload keys the kept items again.
        if let Ty::List(_, true) = &info.ty
            && !s.persist
        {
            let key_path = prog.state_keys.get(&s.def).cloned();
            let items_of = start.clone().unwrap_or_else(|| default.clone());
            let (holder, (k, list)) = rt.scope(|rt| {
                let mut kv = crate::vm::value::keyed_vec(prog.clone(), |p| &p.types, key_path);
                let items = items_of
                    .as_list()
                    .map(<[Value]>::to_vec)
                    .unwrap_or_default();
                if let Err(e) = kv.replace_all(items) {
                    self.error(format!("state `{}`", info.name), e.into());
                }
                let k = rt.keyed(kv);
                rt.set_name(k.id(), path.as_str());
                let list = rt.memo(move |rt| k.with(rt, crate::vm::value::list_of));
                let _ = rt.reads_from(list.id(), &[k.id()]);
                (k, list)
            });
            env.bind_def(s.def, Slot::Keyed(k, list));
            self.note_cell(
                rt,
                key,
                CellRec::Keyed {
                    holder,
                    keyed: k,
                    list,
                    default,
                    ty,
                    path,
                },
            );
            return;
        }
        // A kept plain cell: moved to its new owner, the same signal.
        if let Some(CellRec::Plain {
            holder,
            sig,
            persisted,
            ..
        }) = keep
        {
            let adoptable = match (&persisted, s.persist && self.storage.persist.is_some()) {
                (None, false) => true,
                // Same path and a codec that reads the same: the handle
                // is kept and told the new default.
                (Some(p), true) => {
                    p.path() == path && start.is_some() && {
                        from.as_ref().is_some_and(|from| {
                            let cur = sig.get_untracked(rt).unwrap_or(Value::Null);
                            crate::vm::value::translate(&cur, &from.types, &prog.types)
                                .is_some_and(|(_, changed)| !changed)
                                && crate::vm::value::translate(&default, &prog.types, &from.types)
                                    .is_some_and(|(_, changed)| !changed)
                        })
                    }
                }
                _ => false,
            };
            if adoptable {
                let _ = rt.reparent(holder.id(), rt.current_owner());
                if let Some(p) = &persisted {
                    match p.redeclare(rt, default.clone()) {
                        Ok(strand_core::Redeclared::Kept) => {
                            let v = sig.get_untracked(rt).unwrap_or(Value::Null);
                            let shown = super::reload::shown_kept(&v, &prog.types);
                            self.report(|r| {
                                r.class(EditClass::StateDefault);
                                r.kept_over(crate::reconcile::KeptCell {
                                    path: path.clone(),
                                    shown,
                                });
                            });
                        }
                        Ok(strand_core::Redeclared::Adopted) => {
                            self.report(|r| r.class(EditClass::StateDefault))
                        }
                        _ => {}
                    }
                    self.persisted
                        .borrow_mut()
                        .insert(sig.id(), (path.clone(), p.clone()));
                    let weak = Rc::downgrade(self);
                    let id = sig.id();
                    let _ = rt.with_owner(holder.id(), |rt| {
                        rt.on_cleanup(move || {
                            if let Some(ctx) = weak.upgrade() {
                                ctx.persisted.borrow_mut().remove(&id);
                            }
                        })
                    });
                } else if let Some(v) = &start {
                    // A reload write: `on change` takes it as its
                    // baseline instead of firing.
                    let _ = sig.set_reloaded(rt, v.clone());
                }
                env.bind_def(s.def, Slot::Signal(sig));
                self.note_cell(
                    rt,
                    key,
                    CellRec::Plain {
                        holder,
                        sig,
                        persisted,
                        default,
                        ty,
                        path,
                    },
                );
                return;
            }
            if persisted.is_some() && s.persist {
                // The new cell takes the path over when the old one goes
                // (core's handover: it continues from the old value).
                self.handover_path(path.clone());
            }
        }
        let (holder, (sig, persisted)) = rt.scope(|rt| match (&self.storage.persist, s.persist) {
            (Some(store), true) => {
                let (pe, pd) = (prog.clone(), prog.clone());
                let (te, td) = (info.ty.clone(), info.ty.clone());
                let cell = Rc::new(rt.persisted(
                    store,
                    &path,
                    default.clone(),
                    move |v| crate::vm::persist::encode_bytes(&pe.types, &te, v),
                    move |b| crate::vm::persist::decode_bytes(&pd.types, &td, b),
                ));
                if let strand_core::Restore::KeptOverNewDefault(_) = &cell.restored
                    && let Ok(v) = cell.signal.get_untracked(rt)
                {
                    let cell = crate::reconcile::KeptCell {
                        path: path.clone(),
                        shown: super::reload::shown_kept(&v, &prog.types),
                    };
                    self.notices.borrow_mut().push(cell.notice());
                    self.kept.borrow_mut().push(cell);
                }
                // Newly persisted in a reload: the value it had.
                if matches!(cell.restored, strand_core::Restore::Default)
                    && let Some(v) = &start
                {
                    let _ = cell.signal.set(rt, v.clone());
                }
                let sig = cell.signal;
                // Kept for `@reset` (and the reconciler's `redeclare`).
                self.persisted
                    .borrow_mut()
                    .insert(sig.id(), (path.clone(), cell.clone()));
                let weak = Rc::downgrade(self);
                rt.on_cleanup(move || {
                    if let Some(ctx) = weak.upgrade() {
                        ctx.persisted.borrow_mut().remove(&sig.id());
                    }
                });
                (sig, Some(cell))
            }
            _ => (
                rt.signal(start.clone().unwrap_or_else(|| default.clone())),
                None,
            ),
        });
        rt.set_name(sig.id(), path.as_str());
        env.bind_def(s.def, Slot::Signal(sig));
        self.note_cell(
            rt,
            key,
            CellRec::Plain {
                holder,
                sig,
                persisted,
                default,
                ty,
                path,
            },
        );
    }

    /// `state prefs from "prefs.toml" { typed fields }`: with a settings
    /// store, core's settings file (one `FieldSpec` per field: the
    /// declared default, decoded and written back as TOML by the field's
    /// type); without one, each field holds its default. Either way each
    /// field is its own signal and `prefs` reads as a record of them.
    /// Bind the cell kept under `key` for a closed popup's content (see
    /// [`Ctx::keep_cells`]) to `s` again, moved to the current scope:
    /// the same signal, collection or settings file, value and all.
    /// False if no such cell is left (it is then declared afresh).
    fn reopen_cell(
        self: &Rc<Self>,
        rt: &Runtime,
        s: &crate::lower::State,
        key: &str,
        env: &Rc<Env>,
        path: &str,
    ) -> bool {
        use super::reload::CellRec;
        let found = {
            let reg = self.registry.borrow();
            match reg.cells.get(key) {
                Some(CellRec::Plain { holder, sig, .. }) => Some((*holder, Ok(Slot::Signal(*sig)))),
                Some(CellRec::Keyed {
                    holder,
                    keyed,
                    list,
                    ..
                }) => Some((*holder, Ok(Slot::Keyed(*keyed, *list)))),
                Some(CellRec::Settings { holder, slot, .. }) => Some((*holder, Err(slot.clone()))),
                None => None,
            }
        };
        let Some((holder, slot)) = found else {
            return false;
        };
        if rt.reparent(holder.id(), rt.current_owner()).is_err() {
            return false;
        }
        match (slot, &s.init) {
            (Ok(slot), _) => env.bind_def(s.def, slot),
            (Err(settings), StateInit::Settings { record, .. }) => {
                self.bind_settings(rt, s.def, *record, settings, env, path)
            }
            (Err(_), _) => return false,
        }
        true
    }

    /// A popup closes: the cells of what its content mounted (owned under
    /// `content`, the content's scope) are moved to `keep` before the
    /// content is unmounted, and noted in [`Ctx::closed`] so the content
    /// mounted again takes them back ([`Ctx::reopen_cell`]).
    fn keep_cells(&self, rt: &Runtime, content: strand_core::NodeId, keep: strand_core::NodeId) {
        let reg = self.registry.borrow();
        let mut closed = self.closed.borrow_mut();
        for (key, rec) in reg.cells.iter() {
            let holder = rec.holder().id();
            let mut cur = rt.owner_of(holder).ok().flatten();
            while let Some(c) = cur {
                if c == content {
                    break;
                }
                cur = rt.owner_of(c).ok().flatten();
            }
            if cur.is_some() && rt.reparent(holder, Some(keep)).is_ok() {
                closed.insert(key.clone());
            }
        }
    }

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
        key: Rc<str>,
        carried: Option<(super::reload::CellRec, Arc<crate::lower::Program>)>,
    ) {
        use super::reload::CellRec;
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
        let make_specs = |specs: Vec<(String, Ty, Value)>| -> Vec<strand_core::FieldSpec<Value>> {
            specs
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
                .collect()
        };
        // A reload keeps the file's handle (same file): its fields are
        // redeclared (kept by name, a new default adopted only where
        // nothing set the field, a changed type resets that field).
        let kept = match carried {
            Some((
                CellRec::Settings {
                    holder,
                    slot,
                    file: Some(f),
                    ..
                },
                from,
            )) if resolved.as_deref() == Some(f.as_path()) && slot.handle.is_some() => {
                Some((holder, slot, from))
            }
            Some((
                CellRec::Settings {
                    holder,
                    slot,
                    file: None,
                    ..
                },
                from,
            )) if self.storage.settings.is_none() => Some((holder, slot, from)),
            Some(_) => None,
            None => {
                self.fresh_cell(
                    env.ident(),
                    path.rsplit('.').next().unwrap_or("").to_string(),
                );
                None
            }
        };
        let (holder, (fields, handle)) = match kept {
            Some((holder, old, from)) => {
                let _ = rt.reparent(holder.id(), rt.current_owner());
                self.report(|r| r.kept.push(path.to_string()));
                let out = match &old.handle {
                    Some(h) => {
                        let _ = rt.with_owner(holder.id(), |rt| h.redeclare(rt, make_specs(specs)));
                        let fields = h
                            .signals()
                            .into_iter()
                            .map(|(n, s)| (n.to_string(), s))
                            .collect();
                        (fields, Some(h.clone()))
                    }
                    None => {
                        let fields = rt
                            .with_owner(holder.id(), |rt| {
                                specs
                                    .into_iter()
                                    .map(|(name, _, default)| {
                                        let v = old
                                            .field(&name)
                                            .and_then(|s| s.get_untracked(rt).ok())
                                            .and_then(|v| {
                                                crate::vm::value::translate(
                                                    &v,
                                                    &from.types,
                                                    &prog.types,
                                                )
                                                .map(|x| x.0)
                                            })
                                            .unwrap_or(default);
                                        (name, rt.signal(v))
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        (fields, None)
                    }
                };
                (holder, out)
            }
            None => rt.scope(|rt| match (&self.storage.settings, resolved.clone()) {
                (Some(store), Some(resolved)) => {
                    let handle = rt.settings_file(store, resolved, make_specs(specs));
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
            }),
        };
        let slot = Rc::new(crate::vm::SettingsSlot { fields, handle });
        {
            // Settings declared in a component or branch that was
            // unmounted since leave dead entries: drop them whenever the
            // list has doubled since the last prune (amortised O(1) per
            // mount, and the list stays within twice the live count).
            let mut all = self.settings.borrow_mut();
            if all.len() >= self.settings_prune_at.get() {
                all.retain(|w| w.strong_count() > 0);
                self.settings_prune_at
                    .set((all.len() * 2).max(super::SETTINGS_PRUNE_MIN));
            }
            all.push(Rc::downgrade(&slot));
        }
        self.note_cell(
            rt,
            key,
            CellRec::Settings {
                holder,
                slot: slot.clone(),
                file: resolved,
                path: path.to_string(),
            },
        );
        self.bind_settings(rt, def, record, slot, env, path);
    }

    /// Bind a settings file's `slot` to `def` in `env`: its fields, and
    /// the record a read of the whole state gives (a memo of the current
    /// scope).
    fn bind_settings(
        self: &Rc<Self>,
        rt: &Runtime,
        def: DefId,
        record: crate::ty::RecordId,
        slot: Rc<crate::vm::SettingsSlot>,
        env: &Rc<Env>,
        path: &str,
    ) {
        let prog = self.vm.prog.clone();
        let (sl, names): (Rc<crate::vm::SettingsSlot>, Vec<String>) = (
            slot.clone(),
            prog.types
                .record(record)
                .fields
                .iter()
                .map(|f| f.name.clone())
                .collect(),
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
        let ids: Vec<_> = slot.fields.iter().map(|(_, f)| f.id()).collect();
        let _ = rt.reads_from(memo.id(), &ids);
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
        let gone = self.em.borrow_mut().unmount(frag, keep_self);
        {
            let mut reg = self.registry.borrow_mut();
            for (k, id) in gone.keys {
                if reg.nodes.get(&k).is_some_and(|n| n.0 == id) {
                    reg.nodes.remove(&k);
                }
            }
        }
        for s in gone.scopes {
            s.dispose(rt);
        }
        for st in gone.states {
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
    /// they are acquired while it is shown. An element or component that
    /// would sit deeper than [`super::MAX_MOUNT_DEPTH`] is not mounted:
    /// a located error says so.
    pub(crate) fn mount_element_with(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        parent: FragId,
        extra: Vec<(SceneProp, PropValue)>,
        services: Option<&Arc<crate::lower::ServiceUses>>,
    ) {
        if let ElementKind::Component(d) = &e.kind
            && self.runaway.borrow().contains(d)
        {
            return;
        }
        if self.em.borrow().depth(parent) >= super::MAX_MOUNT_DEPTH {
            self.too_deep(e, env);
            return;
        }
        self.mounting.set(self.mounting.get() + 1);
        self.mount_element_inner(rt, e, env, parent, extra, services);
        let left = self.mounting.get().saturating_sub(1);
        self.mounting.set(left);
        if left == 0 {
            self.runaway.borrow_mut().clear();
        }
    }

    /// Reports an element or component past [`super::MAX_MOUNT_DEPTH`]
    /// and stops the component that got there from mounting again until
    /// the mount in progress returns.
    fn too_deep(&self, e: &Element, env: &Rc<Env>) {
        let (what, runaway) = match &e.kind {
            ElementKind::Component(d) => (
                format!("component `{}`", self.vm.prog.def(*d).name),
                Some(*d),
            ),
            ElementKind::Builtin(k) => (format!("`{}`", k.name()), env.component),
            ElementKind::Unknown(_) => ("an element".to_string(), env.component),
        };
        let what = match (&e.kind, env.component) {
            (ElementKind::Component(_), _) | (_, None) => what,
            (_, Some(c)) => format!("{what} in component `{}`", self.vm.prog.def(c).name),
        };
        if let Some(d) = runaway {
            self.runaway.borrow_mut().insert(d);
        }
        self.errors.borrow_mut().push(super::RuntimeError {
            what,
            error: Error::failed(format!(
                "not mounted: it would nest more than {} elements and components deep (a component that keeps mounting itself?)",
                super::MAX_MOUNT_DEPTH
            )),
            file: Some(e.file),
            span: Some(e.span),
            node: None,
            component: env.component,
            scope: env.fault_scope(),
        });
    }

    fn mount_element_inner(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        parent: FragId,
        extra: Vec<(SceneProp, PropValue)>,
        services: Option<&Arc<crate::lower::ServiceUses>>,
    ) {
        // A nested surface (a `popup` in a bar) holds what its children
        // read while it is shown.
        let services = services.or(e.services.as_ref());
        let kind = match &e.kind {
            ElementKind::Builtin(k) => *k,
            ElementKind::Component(d) => return self.mount_component(rt, *d, e, env, parent),
            ElementKind::Unknown(_) => return,
        };
        let frag = {
            let mut em = self.em.borrow_mut();
            let frag = em.new_frag(Some(parent), None);
            em.deepen(frag);
            frag
        };
        // A reload keeps the node this element was (same place in the
        // instance tree, same identity, same kind).
        let key: Rc<str> = format!("{}/e{}", env.ident(), self.sid(e.file, e.span)).into();
        let claimed = self.claim_node(&key, kind);
        let id = self
            .em
            .borrow_mut()
            .create_with(frag, kind, claimed.as_ref().map(|c| c.0));
        let state = env.node_state(rt, e.node);
        if let Some((_, was)) = &claimed
            && !Rc::ptr_eq(was, &state)
        {
            // Hovered stays hovered across a reload.
            for (a, b) in [
                (was.hover, state.hover),
                (was.pressed, state.pressed),
                (was.focused, state.focused),
                (was.selected, state.selected),
            ] {
                if let Ok(true) = a.get_untracked(rt) {
                    let _ = b.set(rt, true);
                }
            }
            for (a, b) in [(was.width, state.width), (was.height, state.height)] {
                if let Ok(v) = a.get_untracked(rt) {
                    let _ = b.set(rt, v);
                }
            }
        }
        state.scene.set(Some(id));
        self.note_node(key.clone(), id, kind, state.clone());
        for &l in &e.scope {
            if env.local(l).is_none() {
                // A letter's `index`/`count` is render's (per letter).
                let name = self.vm.prog.local(l).name.as_str();
                let v = crate::vm::builtins::scope_value(kind, name).unwrap_or(Value::int(0));
                let m = rt.memo(move |_| Ok(v.clone()));
                let _ = rt.reads_from(m.id(), &[]);
                env.bind_local(l, Slot::Memo(m));
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
                    key: Some(key),
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
        let mut two_way = Vec::new();
        for p in e.props.iter().chain(e.arg.iter()) {
            if let (Some(sp), Some(tw)) = (p.prop, &p.two_way) {
                let mut em = self.em.borrow_mut();
                if let Some(entry) = em.nodes.get_mut(&id) {
                    entry.two_way.push((sp, tw.clone(), env.clone()));
                    two_way.push(PropValue::Keyword(sp.name().into()));
                }
            }
        }
        if !two_way.is_empty() {
            // Render and input write only what is bound two-way (Escape
            // and click-away close an `open: <-> x` surface).
            self.em.borrow_mut().set(
                id,
                SceneProp::TwoWay,
                PropValue::List(two_way),
                Transition::Instant,
            );
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
    /// surface is shown, and the services its body reads are acquired
    /// only while it is shown. The instance sends `show` when `open`
    /// turns true (or at mount for a surface without `open`) and `hide`
    /// when it turns false.
    ///
    /// Hidden, a `bar`'s, `panel`'s or `lock`'s content is frozen (state
    /// kept, nothing evaluated, its nodes left on the scene). A `popup`'s
    /// is unmounted, as the schema's `on_demand` says: its nodes leave
    /// the scene and its scopes go, but the `state`s of the components
    /// in it are kept ([`Ctx::keep_cells`]) and bound again when it opens
    /// (a calendar's month survives closing it).
    fn mount_surface_body(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        frag: FragId,
        ec: ElemCtx,
        services: Option<Arc<crate::lower::ServiceUses>>,
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
        let on_demand = matches!(e.kind, ElementKind::Builtin(NodeKind::Popup));
        let services = services.unwrap_or_default();
        let content = Arc::new(content);
        let inner = self.em.borrow_mut().new_frag(Some(frag), None);
        // The scope the content is mounted in (a new one each time a
        // popup opens, made where this one is: `show` runs inside the
        // `open` effect), and where a closed popup keeps its cells.
        let outer = rt.current_owner();
        let (block, ()) = rt.scope(|_| ());
        let block = Rc::new(Cell::new(block));
        let keep = on_demand.then(|| rt.scope(|_| ()).0);
        let token = self.hold(rt, services);
        let mounted = Rc::new(Cell::new(false));
        let blocked = Rc::new(Cell::new(false));
        let shown: Rc<Cell<Option<bool>>> = Rc::default();
        let (me, env) = (self.clone(), env.clone());
        let show = move |rt: &Runtime, is_open: bool| {
            let prev = shown.replace(Some(is_open));
            if prev == Some(is_open) {
                return;
            }
            if is_open {
                me.set_held(rt, token, true);
                if blocked.replace(false) {
                    me.block_services(rt, block.get().id(), false);
                }
                if !mounted.replace(true) {
                    let _ = rt.with_owner(block.get().id(), |rt| {
                        me.mount_nodes(rt, &content, &env, inner, Some(&ec));
                    });
                } else {
                    rt.resume(block.get().id());
                }
                me.route(rt, ec.scene, "show", Vec::new());
            } else {
                if mounted.get() {
                    match keep {
                        Some(keep) => {
                            // Unmounted, its components' cells kept.
                            mounted.set(false);
                            let old = block.get();
                            me.keep_cells(rt, old.id(), keep.id());
                            me.unmount(rt, inner, true);
                            old.dispose(rt);
                            let fresh = match outer {
                                Some(o) => rt.with_owner(o, |rt| rt.scope(|_| ()).0),
                                None => Ok(rt.scope(|_| ()).0),
                            };
                            if let Ok(fresh) = fresh {
                                block.set(fresh);
                            }
                        }
                        None => {
                            let _ = rt.suspend(block.get().id());
                            // Components in the content let go of their
                            // services too, and nested surfaces of theirs.
                            if !blocked.replace(true) {
                                me.block_services(rt, block.get().id(), true);
                            }
                        }
                    }
                }
                me.set_held(rt, token, false);
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
                let _ = rt.reads_from(effect.id(), &[memo.id()]);
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
        // Which `when` held last time: a container query (a condition on
        // a laid-out size, `self.width < 300`) that held keeps holding
        // while it would hold 4 px either way, so it cannot flicker.
        let held: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(vec![false; sources.len()]));
        let memo = rt.memo(move |rt| {
            for (i, s) in srcs.iter().enumerate().rev() {
                if let Some(c) = s.cond {
                    let (v, query) = crate::vm::builtins::layout_query(0.0, || ctx.eval(rt, c, &e));
                    let mut on = v?.truthy();
                    let was = held.borrow().get(i).copied().unwrap_or(false);
                    if !on && query.read && was {
                        for bias in [-QUERY_HYSTERESIS, QUERY_HYSTERESIS] {
                            let (v, _) =
                                crate::vm::builtins::layout_query(bias, || ctx.eval(rt, c, &e));
                            if v?.truthy() {
                                on = true;
                                break;
                            }
                        }
                    }
                    // A size not laid out yet (boot's 0) seeds nothing: a
                    // container that first lays out at 301 px shows the
                    // same variant as one that grew to it.
                    if let Some(h) = held.borrow_mut().get_mut(i) {
                        *h = on && !query.boot_value;
                    }
                    if !on {
                        continue;
                    }
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
        let mut chunks = Vec::new();
        for s in sources.iter() {
            chunks.extend(s.cond);
            match &s.value {
                SourceValue::Chunk(c, _) => chunks.push(*c),
                SourceValue::Pose(props) => chunks.extend(props.iter().map(|p| p.value)),
                SourceValue::Tokens(defs) => chunks.extend(defs.iter().map(|d| d.value)),
            }
        }
        self.declare_reads(rt, memo.id(), &chunks, env, &[]);
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
                // A node named before it is mounted: set at the end of the
                // tick.
                if out.value == PropValue::Unset
                    && let Some(Source {
                        value: SourceValue::Chunk(c, _),
                        ..
                    }) = sources.get(out.source)
                    && let Ok(Value::Node(n)) = rt.untrack(|rt| self.eval(rt, *c, env))
                {
                    self.late_nodes.borrow_mut().push((id, prop, n));
                }
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
    #[allow(clippy::too_many_arguments)]
    fn mount_switch(
        self: &Rc<Self>,
        rt: &Runtime,
        selector: ChunkId,
        branches: Vec<Arc<Vec<Node>>>,
        two: bool,
        env: &Rc<Env>,
        parent: FragId,
        at: (crate::source::FileId, crate::syntax::Span),
    ) {
        // Its place in the instance tree (`i12`: an `if`, `m3`: a `match`).
        let tag = format!("{}{}", if two { "i" } else { "m" }, self.sid(at.0, at.1));
        let (ctx, e) = (self.clone(), env.clone());
        let pick: Pick = Rc::new(move |rt: &Runtime| -> Result<Option<usize>, Error> {
            let v = ctx.eval(rt, selector, &e)?;
            Ok(if two {
                Some(if v.truthy() { 0 } else { 1 })
            } else {
                v.as_f64().map(|i| i as usize)
            })
        });
        let what = format!(
            "{} in {}",
            if two { "if" } else { "match" },
            self.module_of(at.0)
        );
        let branches = Switch {
            pick,
            branches,
            declare: true,
            tag,
            what,
            reads: (vec![selector], Vec::new()),
            at,
        };
        self.mount_branches(rt, branches, env, parent);
    }

    /// A `page` or a `tooltip`, mounted only while it is shown (the
    /// schema's `on_demand`): a page while its name is its `pages`'
    /// `current`, a tooltip while the element it sits in is hovered.
    /// Hidden, it is unmounted: its nodes leave the scene and what they
    /// read stops being read. Its `state`s belong to the enclosing scope,
    /// so they are kept, as a closed popup keeps its own.
    fn mount_on_demand(self: &Rc<Self>, rt: &Runtime, e: &Element, env: &Rc<Env>, parent: FragId) {
        let ElementKind::Builtin(kind) = e.kind else {
            return;
        };
        // The element it sits in (through any `if`, `match` or `for`).
        let host = {
            let em = self.em.borrow();
            em.scene_at(parent).and_then(|h| {
                let n = em.nodes.get(&h)?;
                let current = n.bindings.iter().find_map(|b| {
                    em.bindings
                        .get(b)
                        .filter(|b| b.prop == SceneProp::Current)
                        .map(|b| b.memo)
                });
                Some((n.kind, n.state.clone(), current))
            })
        };
        let Some((host_kind, host_state, current)) = host else {
            return;
        };
        let (pick, reads): (Pick, (Vec<ChunkId>, Vec<strand_core::NodeId>)) = match kind {
            NodeKind::Tooltip => {
                let hover = host_state.hover;
                (
                    Rc::new(move |rt: &Runtime| Ok(hover.get(rt)?.then_some(0))),
                    (Vec::new(), vec![hover.id()]),
                )
            }
            NodeKind::Page => {
                // A page outside `pages`, or `pages` with no `current`,
                // shows nothing.
                let (Some(current), NodeKind::Pages, Some(name)) =
                    (current, host_kind, e.arg.as_ref())
                else {
                    return;
                };
                let (ctx, env2, chunk, ty) =
                    (self.clone(), env.clone(), name.value, name.ty.clone());
                (
                    Rc::new(move |rt: &Runtime| {
                        let shown = current.get(rt)?.value;
                        let v = ctx.eval(rt, chunk, &env2)?;
                        let name = convert::prop_value_for(
                            &ctx.vm.prog.types,
                            SceneProp::Current,
                            &ty,
                            &v,
                        );
                        Ok((shown == name).then_some(0))
                    }),
                    (vec![chunk], vec![current.id()]),
                )
            }
            _ => return,
        };
        let sid = self.sid(e.file, e.span);
        let branches = Switch {
            pick,
            branches: vec![Arc::new(vec![Node::Element(e.clone())])],
            declare: false,
            tag: format!("d{sid}"),
            what: format!("{} in {}", kind.name(), self.module_of(e.file)),
            reads,
            at: (e.file, e.span),
        };
        self.mount_branches(rt, branches, env, parent);
    }

    /// Mounts the branch `pick` chooses now, and swaps branches when it
    /// changes. A swapped-out branch is removed (render plays its
    /// `exit`) and its scope disposed.
    fn mount_branches(self: &Rc<Self>, rt: &Runtime, s: Switch, env: &Rc<Env>, parent: FragId) {
        let Switch {
            pick,
            branches,
            declare,
            tag,
            what,
            reads,
            at,
        } = s;
        let frag = self.em.borrow_mut().new_frag(Some(parent), None);
        let (block, ()) = rt.scope(|_| ());
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
                    benv.set_ident(format!("{}/{tag}.{}", env.ident(), which.unwrap_or(0)));
                    let _ = rt.with_owner(block.id(), |rt| {
                        ctx.mount_block(rt, frag, None, |ctx, rt, f| {
                            benv.set_owner(rt.current_owner());
                            ctx.note_env(&benv);
                            if declare {
                                ctx.declare(rt, &nodes, &benv);
                                ctx.mount_nodes(rt, &nodes, &benv, f, None);
                            } else {
                                // The on-demand element itself.
                                for n in nodes.iter() {
                                    if let Node::Element(e) = n {
                                        ctx.mount_element(rt, e, &benv, f, Vec::new());
                                    }
                                }
                            }
                        });
                    });
                }
            })
        };
        // A failing first pick is not reported here: the effect below
        // runs it again in this flush and reports it, located.
        if let Ok(which) = rt.untrack(|rt| pick(rt)) {
            show(rt, which);
            current.set(Some(which));
        }
        let env2 = env.clone();
        let _ = rt.with_owner(block.id(), |rt| {
            let effect = rt.effect(move |rt| {
                let which = pick(rt)?;
                if current.get() != Some(which) {
                    rt.untrack(|rt| show(rt, which));
                    current.set(Some(which));
                }
                Ok(())
            });
            rt.set_name(effect.id(), what.as_str());
            self.declare_reads(rt, effect.id(), &reads.0, &env2, &reads.1);
            self.site(rt, effect.id(), what, at.0, at.1, &env2, None);
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
            reads,
            tag,
        } = k;
        let prefix: Rc<str> = format!("{}/{tag}", env.ident()).into();
        let frag = self.em.borrow_mut().new_frag(Some(parent), None);
        let (block, ()) = rt.scope(|_| ());
        let list_id: Cell<Option<strand_core::NodeId>> = Cell::new(None);
        let Ok(read) = rt.with_owner(block.id(), |rt| -> ReadDiffs {
            match source {
                ListSource::Memo(list) => {
                    let keyed =
                        rt.keyed_memo(|t: &(ValueKey, Value)| t.0.clone(), move |rt| list(rt));
                    rt.set_name(keyed.id(), what.as_str());
                    self.declare_reads(rt, keyed.id(), &reads.0, env, &reads.1);
                    self.site(rt, keyed.id(), what.as_str(), at.0, at.1, env, None);
                    let kid = keyed.id();
                    list_id.set(Some(kid));
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
                ListSource::Derived(view) => {
                    list_id.set(Some(view.id()));
                    self.site(rt, view.id(), what.as_str(), at.0, at.1, env, None);
                    Rc::new(move |rt: &Runtime, since: Option<u64>| {
                        let snap = view.snapshot(rt)?;
                        if since == Some(snap.version()) {
                            return Ok(None);
                        }
                        Ok(Some((snap.version(), snap.diffs_or_reset(since))))
                    })
                }
                ListSource::Keyed(cell) => {
                    list_id.set(Some(cell.id()));
                    Rc::new(move |rt: &Runtime, since: Option<u64>| {
                        let snap = cell.snapshot(rt)?;
                        if since == Some(snap.version()) {
                            return Ok(None);
                        }
                        Ok(Some((snap.version(), snap.diffs_or_reset(since))))
                    })
                }
            }
        }) else {
            return;
        };
        let items: Rc<RefCell<Vec<Item>>> = Rc::default();
        let parked: Rc<RefCell<HashMap<ValueKey, Item>>> = Rc::default();
        let version: Rc<Cell<Option<u64>>> = Rc::default();
        let mount_item = {
            let (ctx, env, owned, item) = (self.clone(), env.clone(), owned.clone(), item.clone());
            let prefix = prefix.clone();
            Rc::new(
                move |rt: &Runtime, at: usize, key: ValueKey, value: Value| -> Item {
                    let item_env = Env::child(&env, owned.clone(), None, None);
                    let shown = key.0.show(&ctx.vm.prog.types);
                    item_env.set_ident(format!("{prefix}[{shown}]"));
                    item_env.set_instance(shown);
                    let (ctx2, ie) = (ctx.clone(), item_env.clone());
                    let item = item.clone();
                    let cell: Rc<Cell<Option<strand_core::Signal<Value>>>> = Rc::default();
                    let c2 = cell.clone();
                    let f = rt
                        .with_owner(block.id(), |rt| {
                            ctx.mount_block(rt, frag, Some(at), |ctx, rt, f| {
                                ie.set_owner(rt.current_owner());
                                ctx.note_env(&ie);
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
        let ident_of = {
            let (ctx, prefix) = (self.clone(), prefix.clone());
            move |key: &ValueKey| -> Rc<str> {
                format!("{prefix}[{}]", key.0.show(&ctx.vm.prog.types)).into()
            }
        };
        let leave = {
            let (ctx, parked) = (self.clone(), parked.clone());
            let ident_of = ident_of.clone();
            Rc::new(move |rt: &Runtime, it: Item| {
                if !park {
                    ctx.unmount(rt, it.frag, false);
                    return;
                }
                ctx.parked.borrow_mut().insert(ident_of(&it.key));
                let scope = ctx.em.borrow_mut().park(it.frag);
                if let Some(s) = scope {
                    ctx.block_services(rt, s.id(), true);
                    let _ = rt.suspend(s.id());
                }
                parked.borrow_mut().insert(it.key.clone(), it);
            })
        };
        // An item that arrives: a parked one comes back, else a new one.
        let arrive = {
            let (ctx, parked, mount_item) = (self.clone(), parked.clone(), mount_item.clone());
            let ident_of = ident_of.clone();
            Rc::new(
                move |rt: &Runtime, at: usize, key: ValueKey, value: Value| -> Item {
                    let back = parked.borrow_mut().remove(&key);
                    if park {
                        ctx.parked.borrow_mut().remove(&ident_of(&key));
                    }
                    match back {
                        Some(it) => {
                            let scope = ctx.em.borrow_mut().unpark(it.frag, at);
                            set_value(rt, &it, value);
                            if let Some(s) = scope {
                                ctx.block_services(rt, s.id(), false);
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
            let ident_of = ident_of.clone();
            let forget: Rc<super::Forget> = Rc::new(move |rt: &Runtime, key: &Value| {
                let ident = ident_of(&ValueKey(key.clone()));
                ctx.parked.borrow_mut().remove(&ident);
                // Cells a reload kept for it go too.
                let pending: Vec<Rc<str>> = ctx
                    .pending
                    .borrow()
                    .keys()
                    .filter(|k| k.starts_with(&*ident))
                    .cloned()
                    .collect();
                for k in pending {
                    let rec = ctx.pending.borrow_mut().remove(&k);
                    if let Some(rec) = rec {
                        rec.0.dispose(rt);
                    }
                }
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
            // The effect below reads again in this flush and reports
            // the failure, located: not twice.
            Err(_) => {}
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
            let _ = rt.reads_from(effect.id(), &list_id.get().into_iter().collect::<Vec<_>>());
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
            None if let Some(view) = self.keyed_chain(rt, f, env) => ListSource::Derived(view),
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
                tag: format!("f{}", self.sid(f.file, f.span)),
                reads: (
                    std::iter::once(f.iter)
                        .chain(match &f.key {
                            ForKey::Expr(c) => Some(*c),
                            _ => None,
                        })
                        .collect(),
                    Vec::new(),
                ),
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
                let Some(Slot::Keyed(k, _)) = env.def(*d) else {
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
        env.set_ident(format!(
            "{}/c{}",
            caller.ident(),
            self.sid(call.file, call.span)
        ));
        self.mount_block(rt, parent, None, |ctx, rt, frag| {
            ctx.em.borrow_mut().deepen(frag);
            env.set_owner(rt.current_owner());
            ctx.note_env(&env);
            let mut defaults = Vec::new();
            for ((local, default), arg) in comp.params.iter().zip(args) {
                let vm = ctx.vm.clone();
                let m = match (arg, default) {
                    (Some(c), _) => {
                        let e = caller.clone();
                        let m = rt.memo(move |rt| vm.eval(rt, c, &e));
                        ctx.declare_reads(rt, m.id(), &[c], caller, &[]);
                        m
                    }
                    (None, Some(d)) => {
                        let (e, d) = (env.clone(), *d);
                        let m = rt.memo(move |rt| vm.eval(rt, d, &e));
                        defaults.push((m.id(), d));
                        m
                    }
                    (None, None) => {
                        let m = rt.memo(|_| Ok(Value::Null));
                        let _ = rt.reads_from(m.id(), &[]);
                        m
                    }
                };
                env.bind_local(*local, Slot::Memo(m));
            }
            // Defaults may read other parameters: declared once all are
            // bound.
            for (id, d) in defaults.drain(..) {
                ctx.declare_reads(rt, id, &[d], &env, &[]);
            }
            ctx.acquire(rt, &comp.body.services);
            ctx.declare(rt, &comp.body.nodes, &env);
            ctx.mount_nodes(rt, &comp.body.nodes, &env, frag, None);
        });
    }

    /// Acquire services for the current scope and release them when it
    /// goes.
    pub(crate) fn acquire(self: &Rc<Self>, rt: &Runtime, services: &crate::lower::ServiceUses) {
        let token = self.hold(rt, Arc::new(services.clone()));
        self.set_held(rt, token, true);
    }

    /// Register the services a scope (the current owner) reads, not yet
    /// held; released when the scope goes. [`Ctx::set_held`] holds them
    /// (a surface shown) or lets go (hidden), and a scope under a hidden
    /// surface's content or a parked bar lets go of them until it is
    /// shown or back ([`Ctx::block_services`]).
    pub(crate) fn hold(
        self: &Rc<Self>,
        rt: &Runtime,
        services: Arc<crate::lower::ServiceUses>,
    ) -> u64 {
        let token = self.next_hold.get();
        self.next_hold.set(token + 1);
        let scope = rt.current_owner();
        let blocks = {
            let blocked = self.blocked.borrow();
            let mut n = 0;
            if !blocked.is_empty() {
                let mut cur = scope;
                while let Some(c) = cur {
                    n += blocked.get(&c).copied().unwrap_or(0);
                    cur = rt.owner_of(c).ok().flatten();
                }
            }
            n
        };
        self.holds.borrow_mut().insert(
            token,
            super::Hold {
                scope,
                services,
                want: false,
                blocks,
                acquired: false,
            },
        );
        let (weak, wrt) = (Rc::downgrade(self), rt.downgrade());
        rt.on_cleanup(move || {
            if let (Some(ctx), Some(rt)) = (weak.upgrade(), wrt.upgrade()) {
                let gone = ctx.holds.borrow_mut().remove(&token);
                if let Some(h) = gone
                    && h.acquired
                {
                    hold_services(&*ctx.vm.host, &rt, &h.services, false);
                }
            }
        });
        token
    }

    /// Acquire or release so a hold matches what it wants and its blocks.
    fn sync_hold(&self, rt: &Runtime, h: &mut super::Hold) {
        let should = h.want && h.blocks == 0;
        if should == h.acquired {
            return;
        }
        h.acquired = should;
        hold_services(&*self.vm.host, rt, &h.services, should);
    }

    /// Hold (`true`) or let go of a registered scope's services.
    pub(crate) fn set_held(&self, rt: &Runtime, token: u64, on: bool) {
        let mut holds = self.holds.borrow_mut();
        if let Some(h) = holds.get_mut(&token) {
            h.want = on;
            self.sync_hold(rt, h);
        }
    }

    /// Every scope under `root` lets go of its services (`true`: a surface
    /// hidden, a bar parked) or takes them again (`false`).
    pub(crate) fn block_services(&self, rt: &Runtime, root: strand_core::NodeId, block: bool) {
        {
            let mut blocked = self.blocked.borrow_mut();
            let n = blocked.entry(root).or_insert(0);
            if block {
                *n += 1;
            } else {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    blocked.remove(&root);
                }
            }
        }
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
            if block {
                h.blocks += 1;
            } else {
                h.blocks = h.blocks.saturating_sub(1);
            }
            self.sync_hold(rt, h);
        }
        drop(holds);
        // A scope gone for good (a forgotten bar) leaves no block behind.
        self.blocked.borrow_mut().retain(|&id, _| rt.exists(id));
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
            env.set_ident(format!(
                "{}/s{}",
                env.ident(),
                self.sid(s.element.file, s.element.span)
            ));
            self.note_surface(env.ident(), false);
            self.mount_block(rt, parent, None, |ctx, rt, frag| {
                env.set_owner(rt.current_owner());
                ctx.note_env(&env);
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
        let tag = format!("s{}", self.sid(s.element.file, s.element.span));
        let ident: Rc<str> = format!("{}/{tag}", env.ident()).into();
        self.note_surface(ident.clone(), true);
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
            // In a reload: a single surface turned bar hands its cells
            // to the bar's only instance.
            ctx.bar_instances(&ident, out.len());
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
                tag,
                reads: (
                    pick.into_iter().collect(),
                    ["all", "focused"]
                        .into_iter()
                        .flat_map(|f| self.vm.host.sources(rt, "screens", Some(f)))
                        .collect(),
                ),
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
                // The body's tasks write as the site.
                self.declare_writes(rt, site, body, env);
                self.keep_handler(rt, env, file, h.span, site);
                if let Ok(l) = r {
                    let _ = rt.reads_from(l, &[]);
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
                let r = q.on(rt, move |rt, args: &Vec<Value>| {
                    let frame: Frame = params.iter().copied().zip(args.iter().cloned()).collect();
                    let ctx = Ctx::event_ctx(el.as_ref(), &ev, args.clone());
                    let fut = vm.handler(rt, body, e.clone(), frame, ctx);
                    rt.spawn_for(site, me.reporting(loc.clone(), fut));
                    Ok(())
                });
                self.declare_writes(rt, site, body, env);
                self.keep_handler(rt, env, file, h.span, site);
                if let Ok(l) = r {
                    let _ = rt.reads_from(l, &[]);
                }
            }
            Event::Change { targets, debounce } => {
                // What the tracked expression reads: each target and the
                // item whose identity re-baselines it.
                let tracked: Vec<ChunkId> = targets
                    .iter()
                    .flat_map(|(t, k)| std::iter::once(*t).chain(*k))
                    .collect();
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
                    None => {
                        let id = rt
                            .on_change_keyed(key, track, move |rt, _| {
                                start(rt);
                                Ok(())
                            })
                            .id();
                        self.declare_reads(rt, id, &tracked, env, &[]);
                        self.declare_writes(rt, id, body, env);
                        if let Some(site) = rt.site_of(id) {
                            self.keep_handler(rt, env, file, h.span, site);
                        }
                        id
                    }
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
                        // A reload's old debounce, kept (frozen) until the
                        // new one exists to take its countdown over.
                        let carried: Rc<Cell<Option<strand_core::Debounced>>> = Rc::default();
                        {
                            let hkey: Rc<str> =
                                format!("{}/h{}", env.ident(), self.sid(file, h.span)).into();
                            let hash = self.hash(file, h.span);
                            let old = self.take_handler(
                                rt,
                                &hkey,
                                hash,
                                None,
                                crate::reconcile::EditClass::Handler,
                            );
                            if let Some(cur) = old.and_then(|o| o.debounce)
                                && let Some((_, d)) = cur.take()
                            {
                                d.effect.dispose(rt);
                                let _ = rt.reparent(d.timer.id(), rt.current_owner());
                                let _ = rt.suspend(d.timer.id());
                                carried.set(Some(d));
                            }
                            self.note_handler(
                                rt,
                                hkey,
                                super::reload::HandlerRec {
                                    site: None,
                                    hash,
                                    timer: None,
                                    debounce: Some(current.clone()),
                                },
                            );
                        }
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
                            // The tracked reads on the debounce's effect,
                            // the body's writes on its timer.
                            rt.untrack(|rt| {
                                ctx.declare_reads(rt, new.effect.id(), &tracked, &e, &[]);
                                ctx.declare_writes(rt, new.timer.id(), body, &e);
                                let _ = rt.reads_from(new.timer.id(), &[]);
                            });
                            if let Some((_, old)) = current.take() {
                                let _ = new.rescale_from(rt, old);
                                old.dispose(rt);
                            } else if let Some(old) = carried.take() {
                                let _ = new.rescale_from(rt, old);
                                old.dispose(rt);
                            }
                            current.set(Some((delay, new)));
                            Ok(())
                        });
                        self.declare_reads(rt, effect.id(), &[d], env, &[]);
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
        // A fire while the previous run is still suspended at an `await`
        // is skipped: one run of a timer at a time, so a slow `await`
        // neither piles up runs nor lets an older run write after a newer
        // one (docs/decisions.md, wave2-vm).
        let last: Rc<Cell<Option<strand_core::Task>>> = Rc::default();
        let run = move |rt: &Runtime| {
            if last.get().is_some_and(|t| !t.is_finished(rt)) {
                return Ok(());
            }
            let ctx = Ctx::event_ctx(el.as_ref(), "timer", Vec::new());
            let fut = vm.handler(rt, body, e.clone(), Frame::new(), ctx);
            last.set(Some(rt.spawn(me.reporting(l.clone(), fut))));
            Ok(())
        };
        let timer = match t.kind {
            TimerKind::After => rt.after_dyn(dur, cond, run),
            TimerKind::Every => rt.every_dyn(dur, cond, run),
        };
        rt.set_name(timer.id(), name.as_str());
        {
            // A reload: the new timer takes the old countdown over,
            // rescaled to its duration.
            let hkey: Rc<str> = format!("{}/t{}", env.ident(), self.sid(t.file, t.span)).into();
            let hash = self.hash(t.file, t.span);
            let site = rt.site_of(timer.id());
            if let Some(old) =
                self.take_handler(rt, &hkey, hash, site, crate::reconcile::EditClass::Timer)
                && let Some(ot) = old.timer
            {
                let _ = timer.rescale_from(rt, ot);
            }
            self.note_handler(
                rt,
                hkey,
                super::reload::HandlerRec {
                    site,
                    hash,
                    timer: Some(timer),
                    debounce: None,
                },
            );
        }
        let mut reads = vec![t.duration];
        reads.extend(t.while_);
        self.declare_reads(rt, timer.id(), &reads, env, &[]);
        self.declare_writes(rt, timer.id(), t.body, env);
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

/// Acquire (`on`) or release what a scope holds of the services: each
/// service, and the fields it reads directly. A service is acquired
/// before its fields and released after them.
pub(crate) fn hold_services(
    host: &dyn crate::vm::ServiceHost,
    rt: &Runtime,
    uses: &crate::lower::ServiceUses,
    on: bool,
) {
    if on {
        for (s, f) in uses {
            match f {
                None => host.acquire(rt, s),
                Some(f) => host.acquire_field(rt, s, f),
            }
        }
    } else {
        for (s, f) in uses.iter().rev() {
            match f {
                None => host.release(rt, s),
                Some(f) => host.release_field(rt, s, f),
            }
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

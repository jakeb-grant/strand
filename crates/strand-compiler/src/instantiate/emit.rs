//! The scene emitter: the ops of one tick, in order, and where each
//! scene node sits.
//!
//! Mounting builds a tree of fragments: an element instance is a fragment
//! with a scene node; a structural instance (`if` branch, `for` item,
//! component, `match` arm, slot) is a fragment without one, holding
//! however many scene nodes its content currently has. A new node is
//! created right after the scene node that precedes its fragment, and a
//! moved `for` item's nodes move behind theirs, so `Create`/`Move`
//! indices are always right however the structure around them changes.
//! The emitter mirrors each scene parent's child order as render sees it
//! to compute those indices.

use std::collections::HashMap;
use std::rc::Rc;

use strand_core::{Memo, NodeId as CoreId, Scope};
use strand_scene::{NodeId, NodeIdAllocator, NodeKind, Prop, PropValue, SceneOp, Transition};

use super::Frag;
use crate::vm::EventCtx;
use crate::vm::value::NodeState;

/// A fragment id.
pub type FragId = usize;

/// What a watched prop memo produces: the value and which source (base
/// or a `when`, by index) gave it, for its `~` transition.
#[derive(Clone, Debug, PartialEq)]
pub struct PropOut {
    pub value: PropValue,
    pub source: usize,
}

/// A watched binding.
pub(crate) struct Binding {
    pub scene: NodeId,
    pub prop: Prop,
    pub memo: Memo<PropOut>,
    pub transitions: Rc<Vec<Transition>>,
    pub site: Rc<super::Site>,
}

/// An element instance the emitter knows.
pub(crate) struct NodeEntry {
    /// The element it instantiates, and where it is written.
    pub idx: crate::hir::NodeIdx,
    pub kind: NodeKind,
    pub file: crate::source::FileId,
    pub span: crate::syntax::Span,
    /// A surface (a `popup` included): events do not bubble out of it.
    pub surface: bool,
    pub parent: Option<NodeId>,
    pub state: Rc<NodeState>,
    /// Input queues of its `on …` handlers, by event name, in source
    /// order.
    pub events: HashMap<String, Vec<strand_core::EventQueue<Rc<EventCtx>>>>,
    /// `<->` props: where a widget write goes.
    pub two_way: Vec<(Prop, crate::lower::TwoWay, Rc<crate::vm::Env>)>,
    /// Watched memos bound to this node.
    pub bindings: Vec<CoreId>,
    /// Its reload identity (the scope's place and the element's), which
    /// a reload's new instance claims it by.
    pub key: Option<Rc<str>>,
}

/// See the module docs.
#[derive(Default)]
pub(crate) struct Emitter {
    alloc: NodeIdAllocator,
    pub ops: Vec<SceneOp>,
    frags: Vec<Option<Frag>>,
    free_frags: Vec<FragId>,
    /// Scene children in render's order, per scene parent (`None`: the
    /// surface roots).
    order: HashMap<Option<NodeId>, Vec<NodeId>>,
    pub nodes: HashMap<NodeId, NodeEntry>,
    pub bindings: HashMap<CoreId, Binding>,
    /// The last value sent per node and prop, so unchanged values are
    /// not resent.
    pub sent: HashMap<NodeId, HashMap<Prop, PropValue>>,
    /// Fragments taken off the scene but kept (a parked monitor's bar).
    parked: std::collections::HashSet<FragId>,
    /// Nodes made outside the program (the error overlay), with their
    /// kind: kept as they are across reloads.
    pub external: HashMap<NodeId, NodeKind>,
}

/// What [`Emitter::unmount`] took off: scopes to dispose, element
/// states to reset, and the reload keys of the nodes it dropped.
pub(crate) struct Unmounted {
    pub scopes: Vec<Scope>,
    pub states: Vec<Rc<NodeState>>,
    pub keys: Vec<(Rc<str>, NodeId)>,
}

/// What [`Emitter::reduce`] made of a reload's boot ops.
#[derive(Debug, Default)]
pub struct Reduced {
    pub created: usize,
    pub removed: usize,
    /// Props of kept nodes that changed.
    pub patched: usize,
    /// Surfaces whose layer or namespace changed, or that were replaced.
    pub surfaces: usize,
}

impl Emitter {
    pub fn new_frag(&mut self, parent: Option<FragId>, at: Option<usize>) -> FragId {
        let frag = Frag {
            parent,
            children: Vec::new(),
            parked: Vec::new(),
            scene: None,
            scope: None,
        };
        let id = match self.free_frags.pop() {
            Some(id) => {
                self.frags[id] = Some(frag);
                id
            }
            None => {
                self.frags.push(Some(frag));
                self.frags.len() - 1
            }
        };
        if let Some(p) = parent
            && let Some(pf) = self.frag_mut(p)
        {
            let at = at.unwrap_or(pf.children.len()).min(pf.children.len());
            pf.children.insert(at, id);
        }
        id
    }

    pub fn frag(&self, id: FragId) -> Option<&Frag> {
        self.frags.get(id).and_then(Option::as_ref)
    }

    pub fn frag_mut(&mut self, id: FragId) -> Option<&mut Frag> {
        self.frags.get_mut(id).and_then(Option::as_mut)
    }

    pub fn set_scope(&mut self, id: FragId, scope: Scope) {
        if let Some(f) = self.frag_mut(id) {
            f.scope = Some(scope);
        }
    }

    /// The nearest scene node at or above `frag`'s parent.
    pub fn scene_parent(&self, frag: FragId) -> Option<NodeId> {
        let mut cur = self.frag(frag)?.parent;
        while let Some(p) = cur {
            let f = self.frag(p)?;
            if f.scene.is_some() {
                return f.scene;
            }
            cur = f.parent;
        }
        None
    }

    fn last_scene(&self, frag: FragId) -> Option<NodeId> {
        let f = self.frag(frag)?;
        if f.scene.is_some() {
            return f.scene;
        }
        f.children.iter().rev().find_map(|&c| self.last_scene(c))
    }

    /// The scene node that comes right before `frag`'s content under the
    /// same scene parent.
    fn predecessor(&self, frag: FragId) -> Option<NodeId> {
        let mut cur = frag;
        loop {
            let p = self.frag(cur)?.parent?;
            let pf = self.frag(p)?;
            let pos = pf.children.iter().position(|&c| c == cur)?;
            if let Some(n) = pf.children[..pos]
                .iter()
                .rev()
                .find_map(|&c| self.last_scene(c))
            {
                return Some(n);
            }
            if pf.scene.is_some() {
                return None;
            }
            cur = p;
        }
    }

    fn order_index(&self, parent: Option<NodeId>, pred: Option<NodeId>) -> usize {
        match pred {
            None => 0,
            Some(p) => self
                .order
                .get(&parent)
                .and_then(|o| o.iter().position(|&n| n == p))
                .map_or(0, |i| i + 1),
        }
    }

    /// Create the scene node of element fragment `frag`, under `reuse`'s
    /// id if given (a node a reload keeps, still live in the allocator
    /// this one continues).
    pub fn create_with(&mut self, frag: FragId, kind: NodeKind, reuse: Option<NodeId>) -> NodeId {
        let id = match reuse.filter(|r| self.alloc.is_live(*r)) {
            Some(r) => r,
            None => self.alloc.alloc(),
        };
        if let Some(f) = self.frag_mut(frag) {
            f.scene = Some(id);
        }
        let parent = if kind.is_surface() && self.scene_parent(frag).is_none() {
            None
        } else {
            self.scene_parent(frag)
        };
        let pred = self.predecessor(frag);
        let index = self.order_index(parent, pred);
        let o = self.order.entry(parent).or_default();
        let at = index.min(o.len());
        o.insert(at, id);
        self.ops.push(SceneOp::Create {
            id,
            kind,
            parent,
            index: at as u32,
        });
        id
    }

    /// Top-level scene nodes of `frag`, in order.
    pub fn top_nodes(&self, frag: FragId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_top(frag, &mut out);
        out
    }

    fn collect_top(&self, frag: FragId, out: &mut Vec<NodeId>) {
        let Some(f) = self.frag(frag) else { return };
        match f.scene {
            Some(n) => out.push(n),
            None => {
                for &c in &f.children {
                    self.collect_top(c, out);
                }
            }
        }
    }

    /// The top scene nodes of the fragment whose scope is `scope` (a
    /// component instance: what a runtime fault outlines).
    pub fn scope_nodes(&self, scope: CoreId) -> Vec<NodeId> {
        let frag = self.frags.iter().position(|f| {
            f.as_ref()
                .and_then(|f| f.scope)
                .is_some_and(|s| s.id() == scope)
        });
        frag.map(|f| self.top_nodes(f)).unwrap_or_default()
    }

    /// Live fragments, scene nodes and bindings (tests: nothing leaks).
    #[cfg(test)]
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.frags.iter().filter(|f| f.is_some()).count(),
            self.nodes.len(),
            self.bindings.len(),
        )
    }

    /// Every scene node in `frag`'s subtree.
    fn all_nodes(&self, frag: FragId, out: &mut Vec<NodeId>) {
        let Some(f) = self.frag(frag) else { return };
        if let Some(n) = f.scene {
            out.push(n);
        }
        for &c in f.children.iter().chain(&f.parked) {
            self.all_nodes(c, out);
        }
    }

    fn all_frags(&self, frag: FragId, out: &mut Vec<FragId>) {
        out.push(frag);
        if let Some(f) = self.frag(frag) {
            for &c in f.children.iter().chain(&f.parked) {
                self.all_frags(c, out);
            }
        }
    }

    /// Move fragment `frag` to position `to` among its parent's children
    /// and move its scene nodes after their new predecessor.
    pub fn move_frag(&mut self, frag: FragId, to: usize) {
        let Some(parent) = self.frag(frag).and_then(|f| f.parent) else {
            return;
        };
        if let Some(pf) = self.frag_mut(parent) {
            pf.children.retain(|&c| c != frag);
            let to = to.min(pf.children.len());
            pf.children.insert(to, frag);
        }
        let scene_parent = self.scene_parent(frag);
        let nodes = self.top_nodes(frag);
        let mut pred = self.predecessor(frag);
        for n in nodes {
            if let Some(o) = self.order.get_mut(&scene_parent) {
                o.retain(|&x| x != n);
            }
            let index = self.order_index(scene_parent, pred);
            let o = self.order.entry(scene_parent).or_default();
            let at = index.min(o.len());
            o.insert(at, n);
            self.ops.push(SceneOp::Move {
                id: n,
                parent: scene_parent,
                index: at as u32,
            });
            pred = Some(n);
        }
    }

    /// Take `frag` off the scene (`Remove` its top nodes, so render plays
    /// their exit) but keep it whole: nodes, bindings, state. Returns its
    /// scope, for the caller to freeze.
    pub fn park(&mut self, frag: FragId) -> Option<Scope> {
        let scene_parent = self.scene_parent(frag);
        for n in self.top_nodes(frag) {
            self.ops.push(SceneOp::Remove { id: n });
            if let Some(o) = self.order.get_mut(&scene_parent) {
                o.retain(|&x| x != n);
            }
        }
        if let Some(p) = self.frag(frag).and_then(|f| f.parent)
            && let Some(pf) = self.frag_mut(p)
        {
            pf.children.retain(|&c| c != frag);
            pf.parked.push(frag);
        }
        self.parked.insert(frag);
        self.frag(frag).and_then(|f| f.scope)
    }

    /// Put a parked `frag` back at position `at` among its parent's
    /// children: its nodes are created again under their ids, with every
    /// prop last sent (`Instant`: nothing animates from defaults). Returns
    /// its scope, for the caller to unfreeze.
    pub fn unpark(&mut self, frag: FragId, at: usize) -> Option<Scope> {
        if !self.parked.remove(&frag) {
            return None;
        }
        if let Some(p) = self.frag(frag).and_then(|f| f.parent)
            && let Some(pf) = self.frag_mut(p)
        {
            pf.parked.retain(|&c| c != frag);
            let at = at.min(pf.children.len());
            pf.children.insert(at, frag);
        }
        let scene_parent = self.scene_parent(frag);
        let mut pred = self.predecessor(frag);
        for n in self.top_nodes(frag) {
            let index = self.order_index(scene_parent, pred);
            let o = self.order.entry(scene_parent).or_default();
            let at = index.min(o.len());
            o.insert(at, n);
            self.recreate(n, scene_parent, at);
            pred = Some(n);
        }
        self.frag(frag).and_then(|f| f.scope)
    }

    fn recreate(&mut self, n: NodeId, parent: Option<NodeId>, index: usize) {
        let Some(kind) = self.nodes.get(&n).map(|e| e.kind) else {
            return;
        };
        self.ops.push(SceneOp::Create {
            id: n,
            kind,
            parent,
            index: index as u32,
        });
        let mut props: Vec<(Prop, PropValue)> = self
            .sent
            .get(&n)
            .map(|m| m.iter().map(|(p, v)| (*p, v.clone())).collect())
            .unwrap_or_default();
        props.sort_by_key(|(p, _)| p.name());
        for (prop, value) in props {
            self.ops.push(SceneOp::SetProp {
                id: n,
                prop,
                value,
                transition: Transition::Instant,
            });
        }
        let children = self.order.get(&Some(n)).cloned().unwrap_or_default();
        for (i, c) in children.into_iter().enumerate() {
            self.recreate(c, Some(n), i);
        }
    }

    /// Unmount everything under `frag` (and `frag` itself unless
    /// `keep_self`): `Remove` its top scene nodes, forget their bindings
    /// and return the scopes to dispose and node states to reset.
    pub fn unmount(&mut self, frag: FragId, keep_self: bool) -> Unmounted {
        let roots: Vec<FragId> = if keep_self {
            self.frag(frag)
                .map(|f| f.children.iter().chain(&f.parked).copied().collect())
                .unwrap_or_default()
        } else {
            vec![frag]
        };
        let mut scopes = Vec::new();
        let mut states = Vec::new();
        let mut keys = Vec::new();
        for root in roots {
            let root_parent = self.frag(root).and_then(|f| f.parent);
            let scene_parent = self.scene_parent(root);
            // A parked fragment is off the scene already.
            let on_scene = !self.parked.remove(&root);
            for n in self.top_nodes(root).into_iter().filter(|_| on_scene) {
                self.ops.push(SceneOp::Remove { id: n });
                if let Some(o) = self.order.get_mut(&scene_parent) {
                    o.retain(|&x| x != n);
                }
            }
            let mut all = Vec::new();
            self.all_nodes(root, &mut all);
            for n in all {
                self.order.remove(&Some(n));
                self.sent.remove(&n);
                self.alloc.free(n);
                if let Some(e) = self.nodes.remove(&n) {
                    for b in e.bindings {
                        self.bindings.remove(&b);
                    }
                    if let Some(k) = e.key {
                        keys.push((k, n));
                    }
                    e.state.scene.set(None);
                    states.push(e.state);
                }
            }
            let mut frags = Vec::new();
            self.all_frags(root, &mut frags);
            // Children first: inner scopes are owned by outer ones anyway.
            for f in frags.into_iter().rev() {
                self.parked.remove(&f);
                if let Some(fr) = self.frags.get_mut(f).and_then(Option::take) {
                    if let Some(s) = fr.scope {
                        scopes.push(s);
                    }
                    self.free_frags.push(f);
                }
            }
            if let Some(p) = root_parent
                && let Some(pf) = self.frag_mut(p)
            {
                pf.children.retain(|&c| c != root);
                pf.parked.retain(|&c| c != root);
            }
        }
        Unmounted {
            scopes,
            states,
            keys,
        }
    }

    /// An emitter for a reload's new instance: it continues `old`'s ids
    /// (every node `old` has stays live until the reload frees what it
    /// did not keep) and keeps its external nodes as they are.
    pub fn continuing(old: &Emitter) -> Emitter {
        let mut em = Emitter {
            alloc: old.alloc.clone(),
            ..Emitter::default()
        };
        em.external = old.external.clone();
        for &id in old.external.keys() {
            let parent = old
                .order
                .iter()
                .find(|(_, v)| v.contains(&id))
                .map(|(p, _)| *p);
            if let Some(p) = parent
                && p.is_none_or(|p| !old.external.contains_key(&p))
            {
                em.order.entry(p).or_default().push(id);
            }
            if let Some(kids) = old.order.get(&Some(id)) {
                em.order.insert(Some(id), kids.clone());
            }
            if let Some(s) = old.sent.get(&id) {
                em.sent.insert(id, s.clone());
            }
        }
        em
    }

    /// Free the ids of `old`'s nodes this emitter did not keep (after
    /// [`Emitter::reduce`] removed them from the scene).
    pub fn free_dropped(&mut self, old: &Emitter) {
        for id in old.nodes.keys() {
            if !self.nodes.contains_key(id) && !self.external.contains_key(id) {
                self.alloc.free(*id);
            }
        }
    }

    /// Turn a reload's boot ops (every node created, every prop set, as
    /// if from nothing) into the diff from what render shows (`old`'s
    /// scene): kept nodes (`old`'s ids this emitter reused) are moved
    /// only where their place changed and get only the props that
    /// changed (animating from their current values, with the prop's own
    /// transition), new nodes are created, and `old`'s nodes nobody kept
    /// are removed (render plays their `exit`) once kept descendants have
    /// been moved out. The result replaces [`Emitter::ops`]; token ops in
    /// the boot ops are dropped (the caller compares tables).
    pub fn reduce(&mut self, old: &Emitter) -> Reduced {
        let boot = std::mem::take(&mut self.ops);
        let mut trans: HashMap<(NodeId, Prop), Transition> = HashMap::new();
        for op in &boot {
            if let SceneOp::SetProp {
                id,
                prop,
                transition,
                ..
            } = op
            {
                trans.insert((*id, *prop), transition.clone());
            }
        }
        let kept = |id: &NodeId| old.nodes.contains_key(id) || old.external.contains_key(id);
        let live_now =
            |s: &Self, id: &NodeId| s.nodes.contains_key(id) || s.external.contains_key(id);
        let mut r = Reduced::default();
        // A kept surface whose layer or namespace changed is recreated
        // (the compositor cannot move a layer surface between layers):
        // a new node, its kept children moved under it, the old one
        // removed.
        let mut rekey: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(id, e)| {
                e.surface
                    && old.nodes.contains_key(id)
                    && [Prop::Layer, Prop::Name].iter().any(|p| {
                        self.sent.get(id).and_then(|s| s.get(p))
                            != old.sent.get(id).and_then(|s| s.get(p))
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        rekey.sort();
        for id in rekey {
            self.rekey(id);
            r.surfaces += 1;
        }
        // Render's tree as the ops so far leave it: what is on the scene
        // (a parked bar's nodes are not).
        let mut sim: HashMap<Option<NodeId>, Vec<NodeId>> = HashMap::new();
        let mut parent_of: HashMap<NodeId, Option<NodeId>> = HashMap::new();
        let mut walk = vec![None];
        while let Some(p) = walk.pop() {
            let kids = old.order.get(&p).cloned().unwrap_or_default();
            for k in &kids {
                parent_of.insert(*k, p);
                walk.push(Some(*k));
            }
            sim.insert(p, kids);
        }
        let mut out = Vec::new();
        // Old nodes on the scene that nothing keeps.
        let dropped: Vec<NodeId> = parent_of
            .keys()
            .copied()
            .filter(|id| !live_now(self, id))
            .collect();
        let dropped_set: std::collections::HashSet<NodeId> = dropped.iter().copied().collect();
        // A dropped node whose subtree keeps nothing goes first (fewer
        // moves); one holding kept nodes after they moved out.
        fn keeps_any(
            sim: &HashMap<Option<NodeId>, Vec<NodeId>>,
            id: NodeId,
            dropped: &std::collections::HashSet<NodeId>,
        ) -> bool {
            sim.get(&Some(id)).is_some_and(|kids| {
                kids.iter()
                    .any(|k| !dropped.contains(k) || keeps_any(sim, *k, dropped))
            })
        }
        let top = |id: &NodeId, parent_of: &HashMap<NodeId, Option<NodeId>>| {
            parent_of
                .get(id)
                .copied()
                .flatten()
                .is_none_or(|p| !dropped_set.contains(&p))
        };
        let mut later = Vec::new();
        let mut sorted = dropped.clone();
        sorted.sort();
        for id in sorted {
            if !top(&id, &parent_of) {
                continue;
            }
            if keeps_any(&sim, id, &dropped_set) {
                later.push(id);
                continue;
            }
            out.push(SceneOp::Remove { id });
            r.removed += 1;
            if let Some(p) = parent_of.remove(&id)
                && let Some(v) = sim.get_mut(&p)
            {
                v.retain(|x| *x != id);
            }
        }
        // Place every node of the new tree, top-down.
        let mut stack: Vec<Option<NodeId>> = vec![None];
        while let Some(parent) = stack.pop() {
            let kids = self.order.get(&parent).cloned().unwrap_or_default();
            for (i, c) in kids.iter().enumerate() {
                if kept(c) {
                    let cur = parent_of.get(c).copied();
                    let at =
                        cur.and_then(|p| sim.get(&p).and_then(|v| v.iter().position(|x| x == c)));
                    let moved = match (parent, cur) {
                        // Root order means nothing to render.
                        (None, Some(None)) => false,
                        _ => cur != Some(parent) || at != Some(i),
                    };
                    if moved {
                        if let Some(p) = cur
                            && let Some(v) = sim.get_mut(&p)
                        {
                            v.retain(|x| x != c);
                        }
                        let v = sim.entry(parent).or_default();
                        let i = i.min(v.len());
                        v.insert(i, *c);
                        parent_of.insert(*c, parent);
                        out.push(SceneOp::Move {
                            id: *c,
                            parent,
                            index: i as u32,
                        });
                    }
                } else {
                    let kind = self
                        .nodes
                        .get(c)
                        .map(|e| e.kind)
                        .or_else(|| self.external.get(c).copied());
                    let Some(kind) = kind else { continue };
                    let v = sim.entry(parent).or_default();
                    let i = if parent.is_none() {
                        v.len()
                    } else {
                        i.min(v.len())
                    };
                    v.insert(i, *c);
                    parent_of.insert(*c, parent);
                    out.push(SceneOp::Create {
                        id: *c,
                        kind,
                        parent,
                        index: i as u32,
                    });
                    r.created += 1;
                }
            }
            // Children in order after their parent: push in reverse.
            for c in kids.iter().rev() {
                stack.push(Some(*c));
            }
        }
        for id in later {
            out.push(SceneOp::Remove { id });
            r.removed += 1;
        }
        // Props: what changed on kept nodes, everything on new ones.
        let mut ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        ids.sort();
        for id in ids {
            let new = self.sent.get(&id).cloned().unwrap_or_default();
            let mut props: Vec<(&Prop, &PropValue)> = new.iter().collect();
            props.sort_by_key(|(p, _)| p.name());
            let was = if kept(&id) { old.sent.get(&id) } else { None };
            let is_new = !kept(&id);
            for (p, v) in props {
                if was.and_then(|w| w.get(p)) == Some(v) {
                    continue;
                }
                if !is_new {
                    r.patched += 1;
                }
                out.push(SceneOp::SetProp {
                    id,
                    prop: *p,
                    value: v.clone(),
                    transition: trans.get(&(id, *p)).cloned().unwrap_or_default(),
                });
            }
            if let Some(w) = was {
                let mut gone: Vec<&Prop> = w.keys().filter(|p| !new.contains_key(p)).collect();
                gone.sort_by_key(|p| p.name());
                for p in gone {
                    r.patched += 1;
                    out.push(SceneOp::SetProp {
                        id,
                        prop: *p,
                        value: PropValue::Unset,
                        transition: Transition::Default,
                    });
                }
            }
        }
        self.ops = out;
        r
    }

    /// Give node `id` a new id everywhere this emitter knows it (its
    /// entry, place, children, sent props, fragment, bindings and element
    /// state). Returns the new id. Pending ops are not rewritten: only
    /// [`Emitter::reduce`] calls it, with the ops taken.
    fn rekey(&mut self, id: NodeId) -> NodeId {
        let new = self.alloc.alloc();
        if let Some(e) = self.nodes.remove(&id) {
            e.state.scene.set(Some(new));
            self.nodes.insert(new, e);
        }
        for e in self.nodes.values_mut() {
            if e.parent == Some(id) {
                e.parent = Some(new);
            }
        }
        for v in self.order.values_mut() {
            for x in v.iter_mut() {
                if *x == id {
                    *x = new;
                }
            }
        }
        if let Some(kids) = self.order.remove(&Some(id)) {
            self.order.insert(Some(new), kids);
        }
        if let Some(s) = self.sent.remove(&id) {
            self.sent.insert(new, s);
        }
        for f in self.frags.iter_mut().flatten() {
            if f.scene == Some(id) {
                f.scene = Some(new);
            }
        }
        for b in self.bindings.values_mut() {
            if b.scene == id {
                b.scene = new;
            }
        }
        new
    }

    /// True if `id` is on the scene (not parked, not gone).
    pub fn is_on_scene(&self, id: NodeId) -> bool {
        self.order.values().any(|v| v.contains(&id))
    }

    /// `strand reload --hard`: remove every program node of `old`'s
    /// scene and free its ids here (external nodes stay).
    pub fn drop_all(&mut self, old: &Emitter) -> Vec<SceneOp> {
        let mut ops = Vec::new();
        for id in old.order.get(&None).into_iter().flatten() {
            if !old.external.contains_key(id) {
                ops.push(SceneOp::Remove { id: *id });
            }
        }
        for id in old.nodes.keys() {
            self.alloc.free(*id);
        }
        ops
    }

    // -----------------------------------------------------------------
    // External nodes (the error overlay)

    /// Create a node outside the program, at `index` under `parent`
    /// (`None`: a surface root).
    pub fn external_create(
        &mut self,
        kind: NodeKind,
        parent: Option<NodeId>,
        index: usize,
    ) -> NodeId {
        let id = self.alloc.alloc();
        let o = self.order.entry(parent).or_default();
        let at = index.min(o.len());
        o.insert(at, id);
        self.external.insert(id, kind);
        self.ops.push(SceneOp::Create {
            id,
            kind,
            parent,
            index: at as u32,
        });
        id
    }

    /// Remove an external node and its (external) subtree.
    pub fn external_remove(&mut self, id: NodeId) {
        if self.external.remove(&id).is_none() {
            return;
        }
        for v in self.order.values_mut() {
            v.retain(|x| *x != id);
        }
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Some(kids) = self.order.remove(&Some(n)) {
                for k in kids {
                    self.external.remove(&k);
                    stack.push(k);
                }
            }
            self.sent.remove(&n);
            self.alloc.free(n);
        }
        self.ops.push(SceneOp::Remove { id });
    }

    /// Set a prop now (initial values), skipping unchanged ones.
    pub fn set(&mut self, id: NodeId, prop: Prop, value: PropValue, transition: Transition) {
        if !self.alloc.is_live(id) {
            return;
        }
        let sent = self.sent.entry(id).or_default();
        match sent.get(&prop) {
            Some(v) if *v == value => return,
            None if matches!(value, PropValue::Unset) => return,
            _ => {}
        }
        sent.insert(prop, value.clone());
        self.ops.push(SceneOp::SetProp {
            id,
            prop,
            value,
            transition,
        });
    }

    /// Set a prop even if unchanged (`play`).
    pub fn force(&mut self, id: NodeId, prop: Prop, value: PropValue) {
        if !self.alloc.is_live(id) {
            return;
        }
        self.sent.entry(id).or_default().insert(prop, value.clone());
        self.ops.push(SceneOp::SetProp {
            id,
            prop,
            value,
            transition: Transition::Default,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parked fragment goes with the list that owns it: its frags,
    /// node ids and `parked` entry are freed, and no `Remove` is sent for
    /// what is already off the scene.
    #[test]
    fn parked_fragments_are_unmounted_with_their_list() {
        let mut em = Emitter::default();
        let root = em.new_frag(None, None);
        let list = em.new_frag(Some(root), None);
        let bar = em.new_frag(Some(list), None);
        let id = em.create_with(bar, NodeKind::Bar, None);
        let inner = em.new_frag(Some(bar), None);
        em.create_with(inner, NodeKind::Text, None);
        em.park(bar);
        em.ops.clear();
        em.unmount(list, false);
        assert!(em.parked.is_empty());
        assert_eq!(em.counts().0, 1, "only the root is left");
        assert!(!em.alloc.is_live(id));
        assert!(
            !em.ops.iter().any(|o| matches!(o, SceneOp::Remove { .. })),
            "{:?}",
            em.ops
        );
        // With `keep_self`, parked children go too.
        let list = em.new_frag(Some(root), None);
        let bar = em.new_frag(Some(list), None);
        em.create_with(bar, NodeKind::Bar, None);
        em.park(bar);
        em.unmount(list, true);
        assert!(em.parked.is_empty());
        assert_eq!(em.counts().0, 2);
    }
}

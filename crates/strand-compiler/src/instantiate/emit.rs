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
    pub what: Rc<str>,
}

/// An element instance the emitter knows.
pub(crate) struct NodeEntry {
    /// A surface (a `popup` included): events do not bubble out of it.
    pub surface: bool,
    pub parent: Option<NodeId>,
    pub state: Rc<NodeState>,
    /// Input queues of its `on …` handlers, by event name.
    pub events: HashMap<String, strand_core::EventQueue<Rc<EventCtx>>>,
    /// `<->` props: where a widget write goes.
    pub two_way: Vec<(Prop, crate::lower::TwoWay, Rc<crate::vm::Env>)>,
    /// Watched memos bound to this node.
    pub bindings: Vec<CoreId>,
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
}

impl Emitter {
    pub fn new_frag(&mut self, parent: Option<FragId>, at: Option<usize>) -> FragId {
        let frag = Frag {
            parent,
            children: Vec::new(),
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

    /// Create the scene node of element fragment `frag`.
    pub fn create(&mut self, frag: FragId, kind: NodeKind) -> NodeId {
        let id = self.alloc.alloc();
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

    /// Every scene node in `frag`'s subtree.
    fn all_nodes(&self, frag: FragId, out: &mut Vec<NodeId>) {
        let Some(f) = self.frag(frag) else { return };
        if let Some(n) = f.scene {
            out.push(n);
        }
        for &c in &f.children {
            self.all_nodes(c, out);
        }
    }

    fn all_frags(&self, frag: FragId, out: &mut Vec<FragId>) {
        out.push(frag);
        if let Some(f) = self.frag(frag) {
            for &c in &f.children {
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

    /// Unmount everything under `frag` (and `frag` itself unless
    /// `keep_self`): `Remove` its top scene nodes, forget their bindings
    /// and return the scopes to dispose and node states to reset.
    pub fn unmount(&mut self, frag: FragId, keep_self: bool) -> (Vec<Scope>, Vec<Rc<NodeState>>) {
        let roots: Vec<FragId> = if keep_self {
            self.frag(frag)
                .map(|f| f.children.clone())
                .unwrap_or_default()
        } else {
            vec![frag]
        };
        let mut scopes = Vec::new();
        let mut states = Vec::new();
        for root in roots {
            let root_parent = self.frag(root).and_then(|f| f.parent);
            let scene_parent = self.scene_parent(root);
            for n in self.top_nodes(root) {
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
                    e.state.scene.set(None);
                    states.push(e.state);
                }
            }
            let mut frags = Vec::new();
            self.all_frags(root, &mut frags);
            // Children first: inner scopes are owned by outer ones anyway.
            for f in frags.into_iter().rev() {
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
            }
        }
        (scopes, states)
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

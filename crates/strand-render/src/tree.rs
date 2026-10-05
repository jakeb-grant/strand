//! The retained scene tree the render thread owns, edited by `SceneDiff`s.

use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneDiff, SceneOp, TokenTable, Transition};

/// A scene op that could not be applied. Application continues with the
/// next op, so one bad op never blanks the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneError {
    /// The id is stale or was never created.
    UnknownNode(NodeId),
    /// `Create` named an id whose slot is already live.
    DuplicateNode(NodeId),
    /// `Move` would make a node its own ancestor.
    Cycle(NodeId),
    /// A non-surface node was created or moved without a parent.
    MissingParent(NodeId),
    /// `Create` named a slot index far beyond the live ones (ids are
    /// allocated densely, so this is a bug, and honouring it would
    /// allocate without bound).
    InvalidId(NodeId),
}

/// How far past the current slot count a new id's index may reach.
pub const MAX_INDEX_GAP: usize = 1 << 16;

impl std::fmt::Display for SceneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownNode(id) => write!(f, "unknown or stale node {id:?}"),
            Self::DuplicateNode(id) => write!(f, "node {id:?} already exists"),
            Self::Cycle(id) => write!(f, "moving {id:?} would create a cycle"),
            Self::MissingParent(id) => write!(f, "non-surface node {id:?} has no parent"),
            Self::InvalidId(id) => write!(f, "node id {id:?} is far beyond the allocated range"),
        }
    }
}

impl std::error::Error for SceneError {}

/// A prop as last set, with the transition it was set with. Springs land in
/// M2; until then values snap and the transition is kept for them.
#[derive(Clone, Debug, PartialEq)]
pub struct PropEntry {
    pub prop: Prop,
    pub value: PropValue,
    pub transition: Transition,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub props: Vec<PropEntry>,
    /// Bumped when the node moves, so its subtree repaints even though no
    /// prop changed (paint order may have).
    pub epoch: u32,
}

impl Node {
    pub fn get(&self, prop: Prop) -> Option<&PropValue> {
        self.props.iter().find(|e| e.prop == prop).map(|e| &e.value)
    }
}

#[derive(Debug, Default)]
pub struct SceneTree {
    slots: Vec<Option<Node>>,
    /// Surface roots in creation order.
    roots: Vec<NodeId>,
    pub tokens: TokenTable,
}

impl SceneTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.slots
            .get(id.index as usize)?
            .as_ref()
            .filter(|n| n.id == id)
    }

    fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.slots
            .get_mut(id.index as usize)?
            .as_mut()
            .filter(|n| n.id == id)
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.get(id).is_some()
    }

    /// The surface root `id` paints on: the nearest surface-kind node at
    /// or above it (a `popup` nested in a `bar` is its own root).
    pub fn root_of(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            let node = self.get(id)?;
            match node.parent {
                Some(p) if !node.kind.is_surface() => id = p,
                _ => return Some(id),
            }
        }
    }

    /// True if `ancestor` is `id` or above it.
    pub fn is_ancestor(&self, ancestor: NodeId, mut id: NodeId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            match self.get(id).and_then(|n| n.parent) {
                Some(p) => id = p,
                None => return false,
            }
        }
    }

    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Applies every op in order; returns the ops that failed.
    pub fn apply(&mut self, diff: SceneDiff) -> Vec<SceneError> {
        let mut errors = Vec::new();
        for op in diff.ops {
            if let Err(e) = self.apply_op(op) {
                errors.push(e);
            }
        }
        errors
    }

    pub fn apply_op(&mut self, op: SceneOp) -> Result<(), SceneError> {
        match op {
            SceneOp::Create {
                id,
                kind,
                parent,
                index,
            } => self.create(id, kind, parent, index),
            SceneOp::Remove { id } => self.remove(id),
            SceneOp::Move { id, parent, index } => self.move_node(id, parent, index),
            SceneOp::SetProp {
                id,
                prop,
                value,
                transition,
            } => {
                let node = self.get_mut(id).ok_or(SceneError::UnknownNode(id))?;
                let pos = node.props.iter().position(|e| e.prop == prop);
                match (value, pos) {
                    (PropValue::Unset, Some(i)) => {
                        node.props.swap_remove(i);
                    }
                    (PropValue::Unset, None) => {}
                    (value, Some(i)) => {
                        node.props[i].value = value;
                        node.props[i].transition = transition;
                    }
                    (value, None) => node.props.push(PropEntry {
                        prop,
                        value,
                        transition,
                    }),
                }
                Ok(())
            }
            SceneOp::SetTokens { table } => {
                self.tokens = table;
                Ok(())
            }
        }
    }

    fn create(
        &mut self,
        id: NodeId,
        kind: NodeKind,
        parent: Option<NodeId>,
        index: u32,
    ) -> Result<(), SceneError> {
        let i = id.index as usize;
        if i > self.slots.len() + MAX_INDEX_GAP {
            return Err(SceneError::InvalidId(id));
        }
        if self.slots.get(i).is_some_and(Option::is_some) {
            return Err(SceneError::DuplicateNode(id));
        }
        match parent {
            Some(p) if !self.contains(p) => return Err(SceneError::UnknownNode(p)),
            None if !kind.is_surface() => return Err(SceneError::MissingParent(id)),
            _ => {}
        }
        if self.slots.len() <= i {
            self.slots.resize_with(i + 1, || None);
        }
        self.slots[i] = Some(Node {
            id,
            kind,
            parent,
            children: Vec::new(),
            props: Vec::new(),
            epoch: 0,
        });
        self.attach(id, parent, index);
        Ok(())
    }

    fn attach(&mut self, id: NodeId, parent: Option<NodeId>, index: u32) {
        let list = match parent {
            Some(p) => match self.get_mut(p) {
                Some(n) => &mut n.children,
                None => return,
            },
            None => &mut self.roots,
        };
        let at = (index as usize).min(list.len());
        list.insert(at, id);
    }

    fn detach(&mut self, id: NodeId) {
        let parent = self.get(id).and_then(|n| n.parent);
        let list = match parent {
            Some(p) => match self.get_mut(p) {
                Some(n) => &mut n.children,
                None => return,
            },
            None => &mut self.roots,
        };
        list.retain(|c| *c != id);
    }

    fn remove(&mut self, id: NodeId) -> Result<(), SceneError> {
        if !self.contains(id) {
            return Err(SceneError::UnknownNode(id));
        }
        self.detach(id);
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Some(node) = self.slots.get_mut(n.index as usize).and_then(Option::take) {
                stack.extend(node.children);
            }
        }
        Ok(())
    }

    fn move_node(
        &mut self,
        id: NodeId,
        parent: Option<NodeId>,
        index: u32,
    ) -> Result<(), SceneError> {
        let kind = self.get(id).ok_or(SceneError::UnknownNode(id))?.kind;
        match parent {
            Some(p) => {
                if !self.contains(p) {
                    return Err(SceneError::UnknownNode(p));
                }
                // Walk up from the new parent: finding `id` means a cycle.
                let mut cur = Some(p);
                while let Some(c) = cur {
                    if c == id {
                        return Err(SceneError::Cycle(id));
                    }
                    cur = self.get(c).and_then(|n| n.parent);
                }
            }
            None if !kind.is_surface() => return Err(SceneError::MissingParent(id)),
            None => {}
        }
        self.detach(id);
        if let Some(n) = self.get_mut(id) {
            n.parent = parent;
            n.epoch = n.epoch.wrapping_add(1);
        }
        self.attach(id, parent, index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::Color;

    fn id(i: u32) -> NodeId {
        NodeId::new(i, 0)
    }

    fn base() -> SceneTree {
        let mut t = SceneTree::new();
        let mut d = SceneDiff::new();
        d.create(id(0), NodeKind::Bar, None, 0)
            .create(id(1), NodeKind::Box, Some(id(0)), 0)
            .create(id(2), NodeKind::Text, Some(id(0)), 1)
            .create(id(3), NodeKind::Box, Some(id(1)), 0);
        assert!(t.apply(d).is_empty());
        t
    }

    #[test]
    fn create_move_remove() {
        let mut t = base();
        assert_eq!(t.get(id(0)).unwrap().children, vec![id(1), id(2)]);
        // Reorder: text first.
        let mut d = SceneDiff::new();
        d.push(SceneOp::Move {
            id: id(2),
            parent: Some(id(0)),
            index: 0,
        });
        assert!(t.apply(d).is_empty());
        assert_eq!(t.get(id(0)).unwrap().children, vec![id(2), id(1)]);
        assert_eq!(t.get(id(2)).unwrap().epoch, 1);
        // Removing a subtree frees its descendants.
        t.apply_op(SceneOp::Remove { id: id(1) }).unwrap();
        assert!(!t.contains(id(3)));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn errors_do_not_stop_the_batch() {
        let mut t = base();
        let mut d = SceneDiff::new();
        d.create(id(1), NodeKind::Box, Some(id(0)), 0) // duplicate
            .set(NodeId::new(3, 7), Prop::Bg, PropValue::Bool(true)) // stale
            .create(id(9), NodeKind::Box, None, 0) // no parent
            .push(SceneOp::Move {
                id: id(1),
                parent: Some(id(3)),
                index: 0,
            }) // cycle
            .set(id(3), Prop::Bg, PropValue::Color(Color::WHITE))
            .create(NodeId::new(u32::MAX, 0), NodeKind::Box, Some(id(0)), 0); // absurd id
        let errs = t.apply(d);
        assert_eq!(
            errs,
            vec![
                SceneError::DuplicateNode(id(1)),
                SceneError::UnknownNode(NodeId::new(3, 7)),
                SceneError::MissingParent(id(9)),
                SceneError::Cycle(id(1)),
                SceneError::InvalidId(NodeId::new(u32::MAX, 0)),
            ]
        );
        assert!(t.slots.len() < 100);
        assert_eq!(
            t.get(id(3)).unwrap().get(Prop::Bg),
            Some(&PropValue::Color(Color::WHITE))
        );
    }

    #[test]
    fn set_and_unset_props_keep_transition() {
        let mut t = base();
        t.apply_op(SceneOp::SetProp {
            id: id(1),
            prop: Prop::Width,
            value: PropValue::Number(4.0),
            transition: Transition::Instant,
        })
        .unwrap();
        let n = t.get(id(1)).unwrap();
        assert_eq!(n.props[0].transition, Transition::Instant);
        t.apply_op(SceneOp::SetProp {
            id: id(1),
            prop: Prop::Width,
            value: PropValue::Unset,
            transition: Transition::Default,
        })
        .unwrap();
        assert!(t.get(id(1)).unwrap().get(Prop::Width).is_none());
    }

    #[test]
    fn generations_guard_reused_slots() {
        let mut t = base();
        t.apply_op(SceneOp::Remove { id: id(3) }).unwrap();
        let mut d = SceneDiff::new();
        d.create(NodeId::new(3, 1), NodeKind::Box, Some(id(1)), 0);
        assert!(t.apply(d).is_empty());
        assert!(!t.contains(id(3)));
        assert!(t.contains(NodeId::new(3, 1)));
    }
}

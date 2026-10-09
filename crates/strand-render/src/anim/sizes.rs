//! Size springs: laid-out sizes that spring between layout passes.

use std::collections::{HashMap, HashSet};

use strand_scene::{Curve, LogicalRect, Motion, NodeId, Prop, TokenScope, Transition};

use super::pose::{exit_pose, pose_sizes};
use super::{Animator, COLLAPSED, Forced, PX_EPS, SizeMap};
use crate::tree::SceneTree;

impl Animator {
    /// True while `id`'s laid-out size springs: it clips its content.
    pub fn sizing(&self, id: NodeId) -> bool {
        self.nodes
            .get(&id)
            .is_some_and(|n| n.size.iter().any(Option::is_some))
    }

    /// Nodes under `root` whose laid-out size may spring: the layout step
    /// lays them out at rest first to learn their targets.
    pub fn size_work(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: &NodeId| tree.root_of(*id) == Some(root);
        self.nodes
            .iter()
            .any(|(id, n)| (n.size_touched || n.size.iter().any(Option::is_some)) && under(id))
            || self.enter_size.iter().any(under)
    }

    /// A size spring under `root` waits to start (a size set, a pose
    /// begun): the next layout learns its target at rest.
    pub fn size_pending(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: &NodeId| tree.root_of(*id) == Some(root);
        self.nodes.iter().any(|(id, n)| n.size_touched && under(id))
            || self.enter_size.iter().any(under)
    }

    /// The sizes exiting nodes under `root` take at rest: their exit
    /// pose's `width`/`height`/`size` (toasts collapse to `height: 0`).
    pub fn rest_sizes(&self, tree: &SceneTree, root: NodeId) -> SizeMap {
        let mut out = SizeMap::new();
        if self.snapping() && self.exits.is_empty() {
            return out;
        }
        for id in self.exits.keys() {
            if tree.root_of(*id) != Some(root) {
                continue;
            }
            let Some(node) = tree.get(*id) else { continue };
            let Some(pose) = exit_pose(node) else {
                continue;
            };
            let size = pose_sizes(tree, node, pose);
            if size.iter().any(Option::is_some) {
                let collapse = size.map(|v| v.is_some_and(|v| v < COLLAPSED));
                out.insert(*id, Forced { size, collapse });
            }
        }
        out
    }

    /// Starts or retargets size springs of nodes under `root`, given
    /// their sizes laid out at rest (`targets`) and in the frame before
    /// (`old`).
    pub fn start_sizes(
        &mut self,
        tree: &SceneTree,
        root: NodeId,
        targets: &HashMap<NodeId, LogicalRect>,
        old: Option<&HashMap<NodeId, LogicalRect>>,
        partial: bool,
    ) {
        let snapping = self.snapping();
        let ids: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.size_touched || n.size.iter().any(Option::is_some))
            .map(|(id, _)| *id)
            .chain(self.enter_size.iter().copied())
            .filter(|id| tree.root_of(*id) == Some(root))
            // A partial pass knows only the subtrees it laid out.
            .filter(|id| !partial || targets.contains_key(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let last = self.prev;
        for id in ids {
            let entering = self.enter_size.remove(&id);
            let Some(node) = tree.get(id) else {
                self.nodes.remove(&id);
                continue;
            };
            let na = self.nodes.entry(id).or_default();
            let touched = std::mem::take(&mut na.size_touched);
            let Some(t) = targets.get(&id).filter(|_| !snapping) else {
                na.size = [None, None];
                na.collapse = [false; 2];
                continue;
            };
            let target = [t.w, t.h];
            let from_box = old.and_then(|o| o.get(&id)).map(|r| [r.w, r.h]);
            let pose = if entering {
                node.get(Prop::Enter)
                    .map(|p| pose_sizes(tree, node, p))
                    .unwrap_or_default()
            } else {
                [None, None]
            };
            let scopes = crate::flatten::scope_tables(tree, id);
            let scope = TokenScope::new(&scopes);
            for axis in 0..2 {
                let prop = if axis == 0 { Prop::Width } else { Prop::Height };
                let transition = node
                    .props
                    .iter()
                    .find(|e| e.prop == prop)
                    .or_else(|| node.props.iter().find(|e| e.prop == Prop::Size))
                    .map_or(Transition::Default, |e| e.transition.clone());
                let curve = Curve::of(&scope.transition(&transition, prop));
                match &mut na.size[axis] {
                    Some(m) => {
                        if (m.target()[0] - target[axis]).abs() > 0.01 {
                            m.retarget([target[axis]], curve);
                            na.collapse[axis] |= target[axis] < COLLAPSED;
                        }
                    }
                    slot @ None => {
                        let from = pose[axis].or(if touched {
                            from_box.map(|b| b[axis])
                        } else {
                            None
                        });
                        if let Some(f) = from
                            && (f - target[axis]).abs() > 0.01
                            && curve != Curve::Instant
                        {
                            let mut m = Motion::rest([f], PX_EPS).sampled_at(last);
                            m.retarget([target[axis]], curve);
                            *slot = Some(m);
                            na.collapse[axis] = f < COLLAPSED || target[axis] < COLLAPSED;
                        }
                    }
                }
            }
            if na.is_empty() {
                self.nodes.remove(&id);
            }
        }
    }

    /// The sizes springs give nodes under `root` in this frame, over
    /// `rest`. Settled springs end (their node lays out at rest).
    pub fn size_overrides(&mut self, tree: &SceneTree, root: NodeId, rest: &SizeMap) -> SizeMap {
        let mut out = rest.clone();
        let (at, commit) = (self.time, self.commit);
        if self.snapping() {
            // `reduced_motion` (or a frame without a clock): sizes in
            // flight snap to rest.
            if commit {
                for (id, na) in self.nodes.iter_mut() {
                    if tree.root_of(*id) == Some(root) {
                        na.size = [None, None];
                        na.collapse = [false; 2];
                    }
                }
                self.nodes.retain(|_, n| !n.is_empty());
            }
            return out;
        }
        let mut moving = false;
        for (id, na) in self.nodes.iter_mut() {
            if na.size.iter().all(Option::is_none) || tree.root_of(*id) != Some(root) {
                continue;
            }
            let mut o = rest.get(id).copied().unwrap_or_default();
            for axis in 0..2 {
                let Some(m) = na.size[axis].as_mut() else {
                    continue;
                };
                let v = if commit { m.sample(at) } else { m.peek(at) };
                if m.is_settled(at) {
                    if commit {
                        na.size[axis] = None;
                        na.collapse[axis] = false;
                    }
                } else {
                    moving = true;
                    o.size[axis] = Some(v[0].max(0.0));
                    o.collapse[axis] |= na.collapse[axis];
                }
            }
            if o.size.iter().any(Option::is_some) {
                out.insert(*id, o);
            }
        }
        self.nodes.retain(|_, n| !n.is_empty());
        if moving {
            self.active = true;
        }
        out
    }

    /// True if any size spring under `root` is moving.
    pub fn sizes_moving(&self, tree: &SceneTree, root: NodeId) -> bool {
        self.nodes
            .iter()
            .any(|(id, n)| n.size.iter().any(Option::is_some) && tree.root_of(*id) == Some(root))
    }

    /// Nodes with a size spring under `root`.
    pub fn sized_nodes(&self, tree: &SceneTree, root: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(id, n)| {
                n.size.iter().any(Option::is_some) && tree.root_of(**id) == Some(root)
            })
            .map(|(id, _)| *id)
            .collect()
    }
}

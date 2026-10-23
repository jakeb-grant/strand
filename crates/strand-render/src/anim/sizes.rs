//! Size springs: laid-out sizes that spring between layout passes.

use std::collections::{HashMap, HashSet};

use strand_scene::{Curve, LogicalRect, Motion, NodeId, Prop, TokenScope, Transition};

use super::pose::{exit_pose, pose_sizes};
use super::{Animator, COLLAPSED, Forced, LineHold, NodeAnim, PX_EPS, SizeMap, StepFills};
use crate::tree::{Node, SceneTree};

impl Animator {
    /// True while `node`'s laid-out size springs, or while it exits to
    /// a pose that sets its size (laid out at that size by
    /// [`Animator::rest_sizes`] once its spring settles, until it
    /// unmounts): it clips its content. A row collapsed to `height: 0`
    /// whose spring had settled drew its text whole (exit-height0).
    pub fn sizing(&self, tree: &SceneTree, node: &Node) -> bool {
        self.nodes
            .get(&node.id)
            .is_some_and(|n| n.size.iter().chain(&n.step).any(Option::is_some))
            || self.size_held.contains_key(&node.id)
            || self.step_fills.values().any(|f| f.contains(&node.id))
            || (self.exits.contains_key(&node.id)
                && exit_pose(node)
                    .is_some_and(|p| pose_sizes(tree, node, p).iter().any(Option::is_some)))
    }

    /// (bar-blank-on-exit) The line box `node`'s text keeps while an
    /// `enter` or `exit` pose squeezes its width ([`Forced::hold`]).
    pub fn line_hold(&self, node: NodeId) -> Option<f32> {
        self.line_holds
            .get(&node)
            .map(|h| h.own)
            .filter(|_| self.holds_line(node))
    }

    /// What `node` and its children keep of their layout at rest while
    /// a pose squeezes its width (`rects`: its layout there).
    fn line_hold_of(node: &Node, rects: &HashMap<NodeId, LogicalRect>) -> Option<LineHold> {
        Some(LineHold {
            own: rects.get(&node.id)?.w,
            children: node
                .children
                .iter()
                .filter_map(|c| Some((*c, rects.get(c)?.w)))
                .collect(),
        })
    }

    /// Adds to `out` the holds of the nodes squeezed in it: each one's
    /// line box, and its children's widths at rest.
    fn add_holds(&self, out: &mut SizeMap) {
        let squeezed: Vec<NodeId> = out
            .iter()
            .filter(|(id, f)| f.size[0].is_some() && self.line_holds.contains_key(*id))
            .map(|(id, _)| *id)
            .collect();
        for id in squeezed {
            let Some(h) = self.line_holds.get(&id) else {
                continue;
            };
            if let Some(f) = out.get_mut(&id) {
                f.hold = Some(h.own);
            }
            for (c, w) in &h.children {
                let f = out.entry(*c).or_default();
                if f.size[0].is_none() {
                    f.hold = Some(*w);
                }
            }
        }
    }

    /// A line box kept for `node` still holds: it exits, or its width
    /// springs (the enter that kept it, or an exit called off).
    fn holds_line(&self, node: NodeId) -> bool {
        self.exits.contains_key(&node) || self.nodes.get(&node).is_some_and(|n| n.size[0].is_some())
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

    /// (bar-blank-on-exit) Each node under `root` exiting to a pose that
    /// squeezes its width keeps the line box it leaves from: the one an
    /// enter it interrupts kept, else the width it was last laid out at
    /// (`old`). Kept before the pass that learns the exit's targets, so
    /// its height's target is its lines' at that width, not a column of
    /// letters'.
    pub fn hold_exits(
        &mut self,
        tree: &SceneTree,
        root: NodeId,
        old: &HashMap<NodeId, LogicalRect>,
    ) {
        self.drop_stale_holds();
        for id in self.exits.keys() {
            // Kept already: by this exit, or by an enter it interrupts.
            if self.line_holds.contains_key(id) || tree.root_of(*id) != Some(root) {
                continue;
            }
            let Some(node) = tree.get(*id) else { continue };
            if exit_pose(node).is_some_and(|p| pose_sizes(tree, node, p)[0].is_some())
                && let Some(h) = Self::line_hold_of(node, old)
            {
                self.line_holds.insert(*id, h);
            }
        }
    }

    /// Line boxes that no longer hold: their node neither exits nor has
    /// its width springing.
    fn drop_stale_holds(&mut self) {
        let (nodes, exits) = (&self.nodes, &self.exits);
        self.line_holds.retain(|id, _| {
            exits.contains_key(id) || nodes.get(id).is_some_and(|n| n.size[0].is_some())
        });
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
                out.insert(
                    *id,
                    Forced {
                        size,
                        collapse,
                        hold: None,
                        keep: [None, None],
                    },
                );
            }
        }
        self.add_holds(&mut out);
        out
    }

    /// Starts or retargets size springs of nodes under `root`, given
    /// their sizes laid out at rest (`targets`) and in the frame before
    /// (`old`). `late` is true for a node with text under it that is not
    /// shaped yet (`flatten::late_text_under`): an `enter` pose's sizes
    /// wait on it as its paint does (unshaped-enter).
    pub fn start_sizes(
        &mut self,
        tree: &SceneTree,
        root: NodeId,
        targets: &HashMap<NodeId, LogicalRect>,
        old: Option<&HashMap<NodeId, LogicalRect>>,
        partial: bool,
        late: impl Fn(NodeId) -> bool,
    ) {
        let snapping = self.snapping();
        if snapping && !self.commit && !self.reduced {
            // A preview with no clock yet (a surface just attached, laid
            // out by `configure_surface` before its first frame) is laid
            // out at rest: size springs in flight, and those waiting to
            // start (a size set, an `enter` pose), are left for the first
            // frame painted. Dropping them here snapped a card's size to
            // its target when its popup was made again mid-spring
            // (owner-testing.md bug 9), and lost an `enter` size of
            // content mounted as its popup opened (bug 11).
            return;
        }
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
            // (unshaped-enter) An unshaped text is laid out as nothing
            // (late-text-dip): its target here is not its size at rest.
            // While an enter pose that sets a size is held for the text
            // (it has not played: `enter` still holds it), so are the
            // pose's sizes: it is laid out at them (`size_held`), and
            // they spring from the pose to its size at rest on the pass
            // its text is shaped for. One whose hold ran out (text that
            // never came) lets go on the next pass, as its paint did.
            let sized_pose = tree
                .get(id)
                .and_then(|n| Some(pose_sizes(tree, n, n.get(Prop::Enter)?)))
                .filter(|p| p.iter().any(Option::is_some));
            if let Some(pose) = sized_pose
                && self.enter_size.contains(&id)
                && self.enter_starts(id)
                && late(id)
            {
                self.size_held.insert(id, pose);
                continue;
            }
            self.size_held.remove(&id);
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
            // (bar-blank-on-exit) An enter pose that squeezes the width
            // keeps the line box of the node at rest, where it lands (an
            // exit's is kept as it starts, [`Animator::hold_exits`]).
            if entering
                && pose[0].is_some()
                && na.size[0].is_some()
                && let Some(h) = Self::line_hold_of(node, targets)
            {
                self.line_holds.insert(id, h);
            }
            if na.is_empty() {
                self.nodes.remove(&id);
            }
        }
    }

    /// The sizes springs give nodes under `root` in this frame, over
    /// `rest`. Settled springs end (their node lays out at rest).
    /// (growth) `rest` with the sizes in flight under `root` as the
    /// surface's last frame drew them (one started since, at its start),
    /// sampling nothing: what a step of its content's size is measured
    /// against, so what springs is no step.
    pub fn sizes_drawn(&self, tree: &SceneTree, root: NodeId, rest: &SizeMap) -> SizeMap {
        let mut out = rest.clone();
        if self.snapping() {
            return out;
        }
        let at = self.prev.unwrap_or(self.time);
        for (id, na) in &self.nodes {
            if na.size.iter().all(Option::is_none) || tree.root_of(*id) != Some(root) {
                continue;
            }
            let mut o = rest.get(id).copied().unwrap_or_default();
            for axis in 0..2 {
                if let Some(m) = na.size[axis].as_ref() {
                    o.size[axis] = Some(m.peek(at)[0].max(0.0));
                    o.collapse[axis] |= na.collapse[axis];
                }
            }
            out.insert(*id, o);
        }
        out
    }

    pub fn size_overrides(&mut self, tree: &SceneTree, root: NodeId, rest: &SizeMap) -> SizeMap {
        let mut out = rest.clone();
        let (at, commit, lead) = (self.time, self.commit, self.lead());
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
                self.drop_stale_holds();
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
                #[cfg(feature = "eval-hooks")]
                let pre = m.view();
                let v = if commit {
                    m.sample_frame(at, lead)
                } else {
                    m.peek(at)
                };
                #[cfg(feature = "eval-hooks")]
                if commit {
                    self.springs.push(crate::probe::SpringTrace::of(
                        *id,
                        crate::probe::SpringOf::Size(axis as u8),
                        at,
                        &pre,
                        &v,
                    ));
                }
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
        // (unshaped-enter) Held at its pose's sizes for its text.
        for (id, pose) in &self.size_held {
            if tree.root_of(*id) != Some(root) || !self.enter_starts(*id) {
                continue;
            }
            let o = out.entry(*id).or_default();
            for (axis, v) in pose.iter().enumerate() {
                if let Some(v) = *v {
                    o.size[axis] = Some(v.max(0.0));
                    o.collapse[axis] = v < COLLAPSED;
                }
            }
        }
        if commit {
            // A line box is kept while its pose's width moves, or its node
            // exits (laid out at its exit pose until it unmounts).
            self.drop_stale_holds();
        }
        self.add_holds(&mut out);
        if moving {
            self.active = true;
        }
        out
    }

    /// True if any size spring under `root` is moving.
    pub fn sizes_moving(&self, tree: &SceneTree, root: NodeId) -> bool {
        self.nodes.iter().any(|(id, n)| {
            n.size.iter().chain(&n.step).any(Option::is_some) && tree.root_of(*id) == Some(root)
        })
            // (unshaped-enter) Laid out again each frame while held, so
            // a hold that ends (its text came, or its wait ran out) lets
            // go at once.
            || self.size_held.keys().any(|id| tree.root_of(*id) == Some(root))
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

    /// (growth) A step of `delta` logical px on `axis` (0 the width, 1 the
    /// height) of the box of `root`, a content-sized surface's root,
    /// against its content's size: its box is drawn `delta` larger than
    /// its content there, and springs back along `curve` (a step taken in
    /// flight adds to it and keeps its velocity, as a glide's does).
    /// Nothing moves under reduced motion or along `Curve::Instant`.
    pub fn step_size(&mut self, root: NodeId, axis: usize, delta: f32, curve: Curve) {
        if self.reduced || curve == Curve::Instant || !delta.is_finite() || axis > 1 {
            return;
        }
        let last = self.prev;
        self.nodes.entry(root).or_default().step[axis]
            .get_or_insert_with(|| Motion::rest([0.0], PX_EPS).sampled_at(last))
            .shift([delta], curve);
    }

    /// (growth) The step `root`'s box was drawn at on the surface's last
    /// frame, per axis, without sampling (one started since, at its
    /// start).
    pub fn step_was(&self, root: NodeId) -> [f32; 2] {
        let at = self.prev.unwrap_or(self.time);
        let Some(na) = self.nodes.get(&root) else {
            return [0.0; 2];
        };
        [0, 1].map(|a| na.step[a].as_ref().map_or(0.0, |m| m.peek(at)[0]))
    }

    /// (growth) The axes `root`'s box springs over a step on.
    pub fn step_axes(&self, root: NodeId) -> [bool; 2] {
        self.nodes
            .get(&root)
            .map_or([false; 2], |n| [n.step[0].is_some(), n.step[1].is_some()])
    }

    /// (growth) True while `root`'s box springs over a step.
    pub fn stepping(&self, root: NodeId) -> bool {
        self.nodes
            .get(&root)
            .is_some_and(|n| n.step.iter().any(Option::is_some))
    }

    /// (growth) The step `root`'s box takes in this frame, per axis
    /// (`None`: none in flight); a frame that will be painted samples it,
    /// and one that settles ends it. Snapping (reduced motion, a frame
    /// with no clock) shows none, and a painted one drops it.
    pub fn sample_step(&mut self, root: NodeId) -> [Option<f32>; 2] {
        let (at, commit, lead, snap) = (self.time, self.commit, self.lead(), self.snapping());
        let Some(na) = self.nodes.get_mut(&root) else {
            return [None, None];
        };
        let mut out = [None, None];
        for (axis, slot) in na.step.iter_mut().enumerate() {
            let Some(m) = slot.as_mut() else { continue };
            if snap {
                if commit {
                    *slot = None;
                }
                continue;
            }
            let v = if commit {
                m.sample_frame(at, lead)
            } else {
                m.peek(at)
            };
            if m.is_settled(at) {
                if commit {
                    *slot = None;
                }
            } else {
                out[axis] = Some(v[0]);
            }
        }
        if out.iter().any(Option::is_some) {
            self.active = true;
        }
        if self.nodes.get(&root).is_some_and(NodeAnim::is_empty) {
            self.nodes.remove(&root);
        }
        out
    }

    /// (growth) The boxes that follow `root`'s box while it springs over
    /// a step (`None`: it is at rest).
    pub fn set_step_fills(&mut self, root: NodeId, fills: Option<StepFills>) {
        match fills {
            Some(f) => {
                self.step_fills.insert(root, f);
            }
            None => {
                self.step_fills.remove(&root);
            }
        }
    }

    /// (growth) The boxes that follow `root`'s box while it springs over
    /// a step, if it does.
    pub fn step_fills(&self, root: NodeId) -> Option<&StepFills> {
        self.step_fills.get(&root)
    }
}

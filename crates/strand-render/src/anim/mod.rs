//! Springs on the render thread (design.md, "Layout, animation and
//! input"): every visual prop moves to a new value along its transition,
//! `enter`/`exit` poses, FLIP glides and size springs.
//!
//! State is kept only while something moves: a node at rest has no entry.
//! What starts a motion:
//!
//! - logic set an animatable prop ([`Animator::touch`], recorded by
//!   `Renderer::apply` with the value it had, so the motion starts there);
//! - a node created on a shown surface plays its `enter` pose
//!   ([`Animator::enter`]); a removed one, or a closing surface, plays its
//!   `exit` pose ([`Animator::exit`]);
//! - a structural change moves boxes: they glide from where they were
//!   ([`Animator::glide`]);
//! - `width`, `height` or `size` changed: the laid-out size springs
//!   ([`Animator::touch_size`]; see `Renderer`'s layout step).
//!
//! A target that changes for any other reason (a token swap, an inherited
//! colour) snaps at rest and steers a motion in flight. Paint at time zero
//! (a host with no clock), and `reduced_motion`, snap everything.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use strand_scene::{
    Color, Curve, LogicalRect, Motion, NodeId, Prop, PropValue, TokenScope, Transition,
};

use crate::tree::{Node, SceneTree};

mod motion;
mod pages;
mod pose;
mod sizes;

pub(crate) use motion::Extents;
use motion::{PropMotion, decode, encode};
pub use pages::PageSwap;
pub(crate) use pages::slide as page_slide;
pub(crate) use pose::{exit_pose, is_pose, pose_props};

/// Props that spring between values; the others snap.
pub(crate) const ANIMATED: [Prop; 12] = [
    Prop::X,
    Prop::Y,
    Prop::Opacity,
    Prop::Scale,
    Prop::Rotate,
    Prop::Bg,
    Prop::Color,
    Prop::Border,
    Prop::Shadow,
    Prop::Radius,
    // Widgets: a meter's or slider's fill and its track's colour.
    Prop::Value,
    Prop::Track,
];

/// Props whose change springs the laid-out size.
pub(crate) fn is_size(p: Prop) -> bool {
    matches!(p, Prop::Width | Prop::Height | Prop::Size)
}

/// Settling tolerance per prop, in its own units.
fn eps(p: Prop) -> f32 {
    match p {
        Prop::Opacity => 0.002,
        Prop::Scale => 0.0005,
        Prop::Rotate => 0.05,
        Prop::Bg | Prop::Color | Prop::Track => 0.002,
        Prop::Value => 0.0005,
        _ => 0.05,
    }
}

/// Settling tolerance of laid-out sizes and glides, logical pixels.
const PX_EPS: f32 = 0.05;

/// A size forced on a node for one layout pass.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct Forced {
    /// `[width, height]` in logical pixels.
    pub size: [Option<f32>; 2],
    /// Per axis: the size moves to or from zero (a toast's `exit {
    /// height: 0 }`). Its padding and the parent's gap beside it fold
    /// with it, so its slot reaches zero as the spring settles and
    /// unmounting it moves nothing.
    pub collapse: [bool; 2],
}

/// Laid-out sizes forced on nodes for one layout pass.
pub(crate) type SizeMap = HashMap<NodeId, Forced>;

/// A size this small is zero: its slot collapses.
const COLLAPSED: f32 = 0.5;

#[derive(Debug, Default)]
struct NodeAnim {
    props: Vec<(Prop, PropMotion)>,
    /// Props logic changed since the node was last drawn, with the value
    /// each had before (`None`: unset).
    touched: Vec<(Prop, Option<PropValue>)>,
    /// Every prop whose target changes springs to it (a pose started or
    /// was called off).
    respring: bool,
    glide: Option<Motion<2>>,
    size: [Option<Motion<1>>; 2],
    /// Per axis: the size spring moves to or from zero ([`Forced`]).
    collapse: [bool; 2],
    /// `width`, `height` or `size` changed: the laid-out size springs to
    /// its new value.
    size_touched: bool,
}

impl NodeAnim {
    fn is_empty(&self) -> bool {
        self.props.is_empty()
            && self.touched.is_empty()
            && self.glide.is_none()
            && self.size.iter().all(Option::is_none)
            && !self.size_touched
    }
}

/// Why a node plays its exit pose.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExitKind {
    /// Removed by logic: a ghost, unmounted once the pose settles.
    Ghost,
    /// A surface whose `open` went false: it closes once the pose settles.
    Close,
}

/// Every spring the render thread owns.
#[derive(Debug, Default)]
pub(crate) struct Animator {
    reduced: bool,
    /// The frame being drawn: its time, whether it is painted (samples
    /// start motions and settled ones are dropped) or only previewed.
    time: Duration,
    commit: bool,
    /// The previous frame of the surface being drawn: a change starts at
    /// most one frame before the frame that first shows it.
    prev: Option<Duration>,
    /// Something drawn since [`Animator::begin`] is still moving.
    active: bool,
    nodes: HashMap<NodeId, NodeAnim>,
    /// Nodes that play their `enter` pose when first drawn (paint props)
    /// and laid out (sizes).
    enter: HashSet<NodeId>,
    enter_size: HashSet<NodeId>,
    exits: HashMap<NodeId, ExitKind>,
    /// When each exit started (wall clock): an exit no frame samples
    /// (its output asleep) is ended by `Renderer` after a while.
    exit_started: HashMap<NodeId, Instant>,
    /// Exiting nodes drawn since [`Animator::begin`].
    drawn: HashSet<NodeId>,
    finished: Vec<(NodeId, ExitKind)>,
    /// Poses render chose for nodes in place of their own `enter`/`exit`
    /// (a page's directional slide): read when the enter starts and on
    /// every frame of the exit.
    poses: HashMap<NodeId, PropValue>,
    /// `pages` swaps (directional page transitions).
    pub pages: pages::PageSwaps,
}

impl Animator {
    pub fn set_reduced(&mut self, reduced: bool) {
        self.reduced = reduced;
    }

    pub fn reduced(&self) -> bool {
        self.reduced
    }

    /// Starts drawing a frame at `time` (`commit`: it will be painted).
    pub fn begin(&mut self, time: Duration, prev: Option<Duration>, commit: bool) {
        self.time = time;
        self.prev = prev.filter(|p| !p.is_zero());
        self.commit = commit;
        self.active = false;
        self.drawn.clear();
    }

    /// Something drawn since [`Animator::begin`] is still moving.
    pub fn active(&self) -> bool {
        self.active
    }

    /// Everything snaps: `reduced_motion`, or a frame at time zero (no
    /// clock).
    fn snapping(&self) -> bool {
        self.reduced || self.time.is_zero()
    }

    /// Logic set `prop` of `id`, which had the value `old`.
    pub fn touch(&mut self, id: NodeId, prop: Prop, old: Option<PropValue>) {
        let na = self.nodes.entry(id).or_default();
        if !na.touched.iter().any(|(p, _)| *p == prop) {
            na.touched.push((prop, old));
        }
    }

    /// Logic set `width`, `height` or `size` of `id`.
    pub fn touch_size(&mut self, id: NodeId) {
        self.nodes.entry(id).or_default().size_touched = true;
    }

    pub fn enter(&mut self, id: NodeId) {
        self.enter.insert(id);
        self.enter_size.insert(id);
        // A surface opened again: props held at its exit pose spring back.
        if let Some(na) = self.nodes.get_mut(&id) {
            na.respring = true;
        }
    }

    /// `id` plays its exit pose.
    pub fn exit(&mut self, id: NodeId, kind: ExitKind) {
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.exits.insert(id, kind);
        self.exit_started.insert(id, Instant::now());
        let na = self.nodes.entry(id).or_default();
        na.respring = true;
        na.size_touched = true;
    }

    /// A closing surface opened again: it springs back to its props.
    pub fn cancel_exit(&mut self, id: NodeId) {
        if self.exits.remove(&id).is_some() {
            let na = self.nodes.entry(id).or_default();
            na.respring = true;
            na.size_touched = true;
        }
    }

    /// Nodes with motion state drawn since [`Animator::begin`].
    pub fn drawn(&self) -> &HashSet<NodeId> {
        &self.drawn
    }

    /// Every exit in flight.
    pub fn exits(&self) -> impl Iterator<Item = (NodeId, ExitKind)> + '_ {
        self.exits.iter().map(|(id, k)| (*id, *k))
    }

    pub fn exiting(&self, id: NodeId) -> Option<ExitKind> {
        self.exits.get(&id).copied()
    }

    /// Every exit in flight, with when it started.
    pub fn exit_times(&mut self) -> Vec<(NodeId, Instant)> {
        let exits = &self.exits;
        self.exit_started.retain(|id, _| exits.contains_key(id));
        self.exit_started.iter().map(|(id, t)| (*id, *t)).collect()
    }

    /// When the exit of `id` started, if it is exiting.
    pub fn exit_started(&self, id: NodeId) -> Option<Instant> {
        self.exits
            .contains_key(&id)
            .then(|| self.exit_started.get(&id).copied())
            .flatten()
    }

    /// Ends the exit of `id` now, as if its pose had settled.
    pub fn finish_now(&mut self, id: NodeId) {
        if let Some(k) = self.exits.remove(&id) {
            self.exit_started.remove(&id);
            self.finished.push((id, k));
        }
    }

    /// Drops every motion of `id` (its id now names another node).
    pub fn forget(&mut self, id: NodeId) {
        self.poses.remove(&id);
        self.nodes.remove(&id);
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.exits.remove(&id);
        self.exit_started.remove(&id);
    }

    /// Drops the enter poses of nodes `under` the surface just painted
    /// that it did not draw (a row out of view, a subtree under a
    /// transparent parent): they show at rest when they come into view,
    /// and a pose never drawn keeps no frames coming.
    pub fn drop_undrawn_enters(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        self.enter.retain(|id| !under(*id));
    }

    /// Exits that finished in the frames painted since the last call.
    pub fn take_finished(&mut self) -> Vec<(NodeId, ExitKind)> {
        for (id, _) in &self.finished {
            self.poses.remove(id);
        }
        std::mem::take(&mut self.finished)
    }

    /// `id` plays `pose` instead of its own `enter` (when its enter
    /// starts) or `exit` (while it exits). Its props show at their
    /// values meanwhile: a page sliding in is drawn whole (its colours
    /// do not also spring from the values they had before it existed).
    pub fn set_pose(&mut self, id: NodeId, pose: PropValue) {
        self.poses.insert(id, pose);
        if let Some(na) = self.nodes.get_mut(&id) {
            na.touched.clear();
        }
    }

    /// Ends the exits of nodes under `root` (by `root_of`) that the
    /// frame just painted did not draw: nobody sees them.
    pub fn finish_undrawn(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        let gone: Vec<(NodeId, ExitKind)> = self
            .exits
            .iter()
            .filter(|(id, _)| !self.drawn.contains(id) && under(**id))
            .map(|(id, k)| (*id, *k))
            .collect();
        for (id, k) in gone {
            self.exits.remove(&id);
            self.finished.push((id, k));
        }
        // Paint state of nodes the frame did not draw (out of view, under
        // a transparent parent) is dropped: they show at rest when they
        // come back. Size springs belong to layout and stay.
        let drawn = &self.drawn;
        for (id, na) in self.nodes.iter_mut() {
            if !drawn.contains(id) && under(*id) {
                na.props.clear();
                na.touched.clear();
                na.respring = false;
                na.glide = None;
            }
        }
        self.nodes.retain(|_, n| !n.is_empty());
    }

    /// Drops the state of nodes `keep` rejects (gone from the tree).
    pub fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
        self.enter.retain(|id| keep(*id));
        self.enter_size.retain(|id| keep(*id));
        self.exits.retain(|id, _| keep(*id));
        let exits = &self.exits;
        self.exit_started.retain(|id, _| exits.contains_key(id));
    }

    /// A FLIP: `id` is drawn `delta` away from its new box, then glides
    /// there.
    pub fn glide(&mut self, id: NodeId, delta: [f32; 2], curve: Curve) {
        if self.reduced {
            return;
        }
        let last = self.prev;
        self.nodes
            .entry(id)
            .or_default()
            .glide
            .get_or_insert_with(|| Motion::rest([0.0, 0.0], PX_EPS).sampled_at(last))
            .shift(delta, curve);
    }

    /// The glide offset of `id` in this frame.
    pub fn offset(&mut self, id: NodeId) -> (f32, f32) {
        let (at, commit, snap) = (self.time, self.commit, self.snapping());
        let Some(na) = self.nodes.get_mut(&id) else {
            return (0.0, 0.0);
        };
        let Some(g) = na.glide.as_mut() else {
            return (0.0, 0.0);
        };
        if snap {
            if commit {
                na.glide = None;
            }
            return (0.0, 0.0);
        }
        let v = if commit { g.sample(at) } else { g.peek(at) };
        if g.is_settled(at) {
            if commit {
                na.glide = None;
            }
        } else {
            self.active = true;
        }
        (v[0], v[1])
    }

    /// Springs the animatable props of `node` (resolved in `props`, which
    /// get the values of this frame), playing its enter or exit pose.
    /// `inh` is the colour it inherits; `rect` its laid-out box, `parent`
    /// its parent's (lengths resolve against them).
    pub fn paint(
        &mut self,
        node: &Node,
        props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
        scope: &TokenScope<'_>,
        inh: Color,
        rect: Option<LogicalRect>,
        parent: LogicalRect,
    ) {
        let boxes = Extents {
            own: rect.map_or((0.0, 0.0), |r| (r.w, r.h)),
            parent: (parent.w, parent.h),
        };
        let id = node.id;
        let exiting = self.exits.get(&id).copied();
        // Drawn this frame: its motions (an enter pose that starts now
        // included) survive `finish_undrawn`.
        if exiting.is_some() || self.nodes.contains_key(&id) || self.enter.contains(&id) {
            self.drawn.insert(id);
        }
        if self.snapping() {
            if self.commit {
                if let Some(na) = self.nodes.get_mut(&id) {
                    na.props.clear();
                    na.touched.clear();
                    na.respring = false;
                }
                self.enter.remove(&id);
                if let Some(k) = self.exits.remove(&id) {
                    self.finished.push((id, k));
                }
            }
            return;
        }
        let entering = self.enter.remove(&id);
        if !entering && exiting.is_none() && !self.nodes.contains_key(&id) {
            return;
        }
        let resolve = |v: &PropValue| -> Vec<(Prop, PropValue)> {
            let v = scope
                .resolve(v)
                .map(Cow::into_owned)
                .unwrap_or(PropValue::Unset);
            pose_props(&v, rect)
                .into_iter()
                .filter_map(|(p, v)| Some((p, scope.resolve(&v)?.into_owned())))
                .collect()
        };
        let chosen = self.poses.get(&id);
        let enter_pose = entering
            .then(|| chosen.or(node.get(Prop::Enter)).map(resolve))
            .flatten()
            .unwrap_or_default();
        let exit_pose = exiting
            .and_then(|_| chosen.or(exit_pose(node)).map(resolve))
            .unwrap_or_default();
        if entering && exiting.is_none() {
            self.poses.remove(&id);
        }
        let (at, commit, last) = (self.time, self.commit, self.prev);
        let na = self.nodes.entry(id).or_default();
        let touched = std::mem::take(&mut na.touched);
        let respring = std::mem::take(&mut na.respring);
        let mut moving = false;
        let mut exit_done = true;
        for p in ANIMATED {
            let own = props.iter().position(|(q, _)| *q == p);
            let own_v = own.map(|i| props[i].1.as_ref().clone());
            let in_exit = exit_pose.iter().find(|(q, _)| *q == p).map(|(_, v)| v);
            let target_v = in_exit.cloned().or_else(|| own_v.clone());
            let Some(target) = encode(p, target_v.as_ref(), inh, boxes) else {
                // Cannot interpolate: snaps.
                na.props.retain(|(q, _)| *q != p);
                continue;
            };
            let slot = na.props.iter().position(|(q, _)| *q == p);
            let touch = touched.iter().find(|(q, _)| *q == p);
            let transition = node
                .props
                .iter()
                .find(|e| e.prop == p)
                .map_or(Transition::Default, |e| e.transition.clone());
            let curve = Curve::of(&scope.transition(&transition, p));
            match slot {
                Some(i) => {
                    let m = &mut na.props[i].1;
                    if m.target() != target {
                        let springs = respring || touch.is_some() || !m.settled(at);
                        if !(springs && m.retarget(&target, curve, last)) {
                            *m = PropMotion::rest(p, &target, last);
                        }
                    }
                }
                None => {
                    // Where it starts: the enter pose, the value logic
                    // replaced, or (an exit) the node's own value.
                    let from = if let Some((_, v)) = enter_pose.iter().find(|(q, _)| *q == p) {
                        Some(encode(p, Some(v), inh, boxes))
                    } else if let Some((_, old)) = touch {
                        Some(encode(p, old.as_ref(), inh, boxes))
                    } else if in_exit.is_some() || respring {
                        Some(encode(p, own_v.as_ref(), inh, boxes))
                    } else {
                        None
                    };
                    if let Some(Some(from)) = from
                        && from != target
                    {
                        let mut m = PropMotion::rest(p, &from, last);
                        if !m.retarget(&target, curve, last) {
                            continue;
                        }
                        na.props.push((p, m));
                    } else {
                        continue;
                    }
                }
            }
            let Some(i) = na.props.iter().position(|(q, _)| *q == p) else {
                continue;
            };
            let enc = na.props[i].1.value(at, commit);
            let settled = na.props[i].1.settled(at);
            let value = decode(p, &enc);
            match own {
                Some(j) => props[j].1 = Cow::Owned(value),
                None => props.push((p, Cow::Owned(value))),
            }
            if settled {
                // A settled exit prop stays at its pose until the exit
                // ends (dropped, it would start over from the node's own
                // value).
                if commit && exiting.is_none() {
                    if let PropMotion::Shadows { m, len } = &mut na.props[i].1 {
                        m.truncate(*len);
                    }
                    na.props.swap_remove(i);
                }
            } else {
                moving = true;
                if in_exit.is_some() {
                    exit_done = false;
                }
            }
        }
        if exiting.is_some() && na.size.iter().any(Option::is_some) {
            exit_done = false;
        }
        if moving {
            self.active = true;
        }
        if commit
            && let Some(k) = exiting
            && exit_done
        {
            self.exits.remove(&id);
            self.finished.push((id, k));
        }
        if self.nodes.get(&id).is_some_and(NodeAnim::is_empty) {
            self.nodes.remove(&id);
        }
    }

    // ---- Laid-out sizes ----------------------------------------------

    /// Anything under `root` moving or about to: frames are wanted.
    pub fn busy(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: &NodeId| tree.root_of(*id) == Some(root);
        self.nodes.keys().any(under) || self.enter.iter().any(under) || self.exits.keys().any(under)
    }
}

#[cfg(test)]
mod tests;

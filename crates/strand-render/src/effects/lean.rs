//! (M4) Pointer parallax and 2D tilt (design.md, "Motion and time":
//! `parallax: 6px`; `tilt: 8deg`, "Parallax trivial; true 3D tilt is
//! GPU"; architecture.md: `Router::pointer` read at flatten time).
//!
//! Both follow the pointer on the node's surface (the position the host
//! hands render from `Router::pointer`):
//!
//! - `parallax: d` moves the node up to `d` logical pixels towards the
//!   pointer, in proportion to how far the pointer is from the surface's
//!   centre (all the way at its edges). A negative `d` moves it away.
//! - `tilt: a` is the CPU's 2D tilt: the node turns in its plane by up to
//!   `a` about its centre, towards the side of its own box the pointer is
//!   on (all the way at or past its edge, nothing over its centre). With
//!   a GPU it turns in 3-D instead: up to `a` about its vertical axis as
//!   the pointer goes across its box and about its horizontal axis as it
//!   goes down, the side under the pointer pressed away (a bundled pass,
//!   `Bundled::Tilt`).
//!
//! The offset and turn spring towards their targets along the prop's
//! transition (`$motion.spatial` by default) and back to rest when the
//! pointer leaves. They add to the node's own `x`, `y` and `rotate` as
//! paint offsets, so layout never moves. `reduced_motion` turns both off.

use std::collections::{HashMap, HashSet};

use strand_scene::{Curve, LogicalPoint, LogicalRect, Motion, NodeId, Prop, PropValue};

use crate::shapes::morph::Frame;

/// Settling tolerance, logical pixels and degrees.
const EPS: f32 = 0.05;

/// Largest parallax distance and tilt, logical pixels and degrees.
const MAX_PARALLAX: f32 = 200.0;
const MAX_TILT: f32 = 45.0;

/// A node's `parallax` (logical pixels) and `tilt` (degrees).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct Lean {
    pub parallax: f32,
    pub tilt: f32,
}

impl Lean {
    /// The node's lean from its resolved props, `None` with neither.
    pub(crate) fn of<'v>(get: impl Fn(Prop) -> Option<&'v PropValue>) -> Option<Lean> {
        let n = |p: Prop| match get(p)? {
            PropValue::Number(n) | PropValue::Angle(n) => n.is_finite().then_some(*n),
            PropValue::Length(strand_scene::Length::Px(n)) => n.is_finite().then_some(*n),
            _ => None,
        };
        let parallax = n(Prop::Parallax).map(|v| v.clamp(-MAX_PARALLAX, MAX_PARALLAX));
        let tilt = n(Prop::Tilt).map(|v| v.clamp(-MAX_TILT, MAX_TILT));
        if parallax.is_none() && tilt.is_none() {
            return None;
        }
        Some(Lean {
            parallax: parallax.unwrap_or(0.0),
            tilt: tilt.unwrap_or(0.0),
        })
    }

    /// Where it leans with the pointer at `pointer` (`None`: off the
    /// surface): `[dx, dy, across, down]` (the turns for the pointer
    /// across and down its box), for a node laid out at `frame` on a
    /// surface `surface` (both logical pixels).
    pub(crate) fn target(
        &self,
        pointer: Option<LogicalPoint>,
        frame: LogicalRect,
        surface: LogicalRect,
    ) -> [f32; 4] {
        let Some(p) = pointer.filter(|p| p.x.is_finite() && p.y.is_finite()) else {
            return [0.0; 4];
        };
        // -1 at one edge of `r`, 1 at the other.
        let along = |v: f32, start: f32, len: f32| {
            if len > 0.0 {
                ((v - start) / len * 2.0 - 1.0).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        };
        let sx = along(p.x, surface.x, surface.w);
        let sy = along(p.y, surface.y, surface.h);
        let nx = along(p.x, frame.x, frame.w);
        let ny = along(p.y, frame.y, frame.h);
        [
            self.parallax * sx,
            self.parallax * sy,
            self.tilt * nx,
            self.tilt * ny,
        ]
    }
}

/// Every leaning node's spring, and the nodes that lean.
#[derive(Debug, Default)]
pub(crate) struct Leans {
    nodes: HashMap<NodeId, Motion<4>>,
    /// Nodes drawn with `parallax` or `tilt`: a pointer motion on their
    /// surface repaints it.
    users: HashSet<NodeId>,
}

impl Leans {
    /// The offset and turn node `id` draws in `frame`, springing towards
    /// `target` along `curve`; also whether it is still moving.
    pub(crate) fn sample(
        &mut self,
        id: NodeId,
        target: [f32; 4],
        curve: Curve,
        frame: Frame,
    ) -> ([f32; 4], bool) {
        self.users.insert(id);
        if frame.snap {
            if frame.commit {
                self.nodes.remove(&id);
            }
            return ([0.0; 4], false);
        }
        let m = self
            .nodes
            .entry(id)
            .or_insert_with(|| Motion::rest([0.0; 4], EPS).sampled_at(frame.prev));
        if m.target() != target {
            m.retarget(target, curve);
        }
        let v = if frame.commit {
            m.sample(frame.at)
        } else {
            m.peek(frame.at)
        };
        let moving = !m.is_settled(frame.at);
        if !moving && frame.commit && target == [0.0; 4] {
            self.nodes.remove(&id);
        }
        (v, moving)
    }

    /// True if a node `under` a surface leans.
    pub(crate) fn used(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.users.iter().any(|id| under(*id))
    }

    /// `id` does not lean (any more).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.users.remove(&id);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
        self.users.retain(|id| keep(*id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_follow_the_pointer_and_rest_without_it() {
        let lean = Lean {
            parallax: 6.0,
            tilt: 8.0,
        };
        let surface = LogicalRect::new(0.0, 0.0, 200.0, 100.0);
        let frame = LogicalRect::new(80.0, 30.0, 40.0, 40.0);
        assert_eq!(lean.target(None, frame, surface), [0.0; 4]);
        let centre = LogicalPoint { x: 100.0, y: 50.0 };
        assert_eq!(lean.target(Some(centre), frame, surface), [0.0; 4]);
        let corner = LogicalPoint { x: 200.0, y: 0.0 };
        assert_eq!(
            lean.target(Some(corner), frame, surface),
            [6.0, -6.0, 8.0, -8.0]
        );
        let left = LogicalPoint { x: 90.0, y: 50.0 };
        let t = lean.target(Some(left), frame, surface);
        assert!((t[2] + 4.0).abs() < 1e-4, "{t:?}");
    }
}

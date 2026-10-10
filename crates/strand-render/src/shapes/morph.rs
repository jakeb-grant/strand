//! `shape:` changes morph by spring (design.md: "`shape: cookie` →
//! `when loading { shape: burst }`; morphs by spring").
//!
//! A node's shape is remembered from the frames that drew it. When the
//! shape it resolves to changes, its outline moves from the one on screen
//! (a morph in flight included) to the new shape's, point by point, along
//! the prop's transition (`$motion.spatial` by default; `~ instant`
//! snaps). Progress is a one-channel spring from 0 to 1, so a bouncy
//! spring overshoots the new outline and comes back. `reduced_motion` and
//! frames with no clock snap.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use strand_scene::{Curve, Motion, NodeId};

use super::{Outline, Shape, outline};

/// Settling tolerance of a morph's progress.
const EPS: f32 = 0.001;

#[derive(Debug)]
struct Morph {
    /// The shape it shows, or morphs to.
    shape: Shape,
    /// Where a morph started (normalised points) and its progress.
    from: Option<(Vec<[f32; 2]>, Motion<1>)>,
}

/// Every shaped node's shape and morph.
#[derive(Debug, Default)]
pub(crate) struct Morphs {
    nodes: HashMap<NodeId, Morph>,
    /// Nodes a preview saw change shape: the next painted frame must
    /// flatten afresh to start the morph.
    pending: HashSet<NodeId>,
}

/// The frame being drawn, as the animator sees it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Frame {
    pub at: Duration,
    pub commit: bool,
    pub prev: Option<Duration>,
    /// Everything snaps (`reduced_motion`, no clock).
    pub snap: bool,
}

/// `a` lerped towards `b` by `p`.
fn lerp(a: &[[f32; 2]], b: &[[f32; 2]], p: f32) -> Vec<[f32; 2]> {
    a.iter()
        .zip(b)
        .map(|(a, b)| [a[0] + (b[0] - a[0]) * p, a[1] + (b[1] - a[1]) * p])
        .collect()
}

impl Morphs {
    /// What node `id`, whose shape is `shape` in a box of `aspect`,
    /// draws in `frame`, moving along `curve` when its shape changed; and
    /// whether it is still moving.
    pub(crate) fn outline(
        &mut self,
        id: NodeId,
        shape: Shape,
        aspect: f64,
        curve: Curve,
        frame: Frame,
    ) -> (Outline, bool) {
        let Some(m) = self.nodes.get_mut(&id) else {
            if frame.commit {
                self.nodes.insert(id, Morph { shape, from: None });
            }
            return (Outline::Shape(shape), false);
        };
        if m.shape != shape {
            if !frame.commit {
                // A preview shows the new shape; the painted frame starts
                // the morph from the old one.
                self.pending.insert(id);
                return (Outline::Shape(m.shape), false);
            }
            self.pending.remove(&id);
            if frame.snap || curve == Curve::Instant {
                m.shape = shape;
                m.from = None;
                return (Outline::Shape(shape), false);
            }
            let old = outline(m.shape, aspect);
            let shown = match &mut m.from {
                Some((from, motion)) => {
                    let p = motion.sample(frame.at)[0];
                    lerp(from, &old, p)
                }
                None => old,
            };
            let mut motion = Motion::rest([0.0], EPS).sampled_at(frame.prev);
            motion.retarget([1.0], curve);
            m.shape = shape;
            m.from = Some((shown, motion));
        }
        let Some((from, motion)) = &mut m.from else {
            return (Outline::Shape(shape), false);
        };
        if frame.snap {
            if frame.commit {
                m.from = None;
            }
            return (Outline::Shape(shape), false);
        }
        let p = if frame.commit {
            motion.sample(frame.at)[0]
        } else {
            motion.peek(frame.at)[0]
        };
        if motion.is_settled(frame.at) {
            if frame.commit {
                m.from = None;
            }
            return (Outline::Shape(shape), false);
        }
        let points = lerp(from, &outline(shape, aspect), p);
        (Outline::Points(points), true)
    }

    /// A preview saw a node under `under` change shape.
    pub(crate) fn pending(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.pending.iter().any(|id| under(*id))
    }

    /// Drops nodes `keep` rejects.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
        self.pending.retain(|id| keep(*id));
    }

    /// Forgets `id` (its id now names another node, or it lost `shape`).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.pending.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::Spring;

    fn frame(ms: u64) -> Frame {
        Frame {
            at: Duration::from_millis(ms),
            commit: true,
            prev: Some(Duration::from_millis(ms.saturating_sub(16))),
            snap: false,
        }
    }

    #[test]
    fn a_shape_change_morphs_by_spring_and_settles() {
        let id = NodeId::new(1, 0);
        let spring = Curve::Spring(Spring::new(700.0, 0.9).unwrap());
        let mut m = Morphs::default();
        assert_eq!(
            m.outline(id, Shape::Cookie, 1.0, spring, frame(1000)),
            (Outline::Shape(Shape::Cookie), false)
        );
        // Changed: the next frames draw points between the two.
        let (o, moving) = m.outline(id, Shape::Burst, 1.0, spring, frame(1016));
        assert!(moving);
        let Outline::Points(p) = o else { panic!() };
        let (c, b) = (outline(Shape::Cookie, 1.0), outline(Shape::Burst, 1.0));
        // At 12 o'clock the cookie reaches 1 and the burst too; between
        // two of the burst's points (15°) the cookie is at 0.9·… and the
        // burst lower: the morph lies between.
        let i = 5;
        let r = |v: [f32; 2]| (v[0] * v[0] + v[1] * v[1]).sqrt();
        let (rc, rb, rm) = (r(c[i]), r(b[i]), r(p[i]));
        assert!(rm < rc.max(rb) && rm > rc.min(rb), "{rc} {rm} {rb}");
        let mut t = 1016;
        loop {
            t += 16;
            let (o, moving) = m.outline(id, Shape::Burst, 1.0, spring, frame(t));
            if !moving {
                assert_eq!(o, Outline::Shape(Shape::Burst));
                break;
            }
            assert!(t < 3000, "settles");
        }
        // `~ instant`, and reduced motion, snap.
        let (o, moving) = m.outline(id, Shape::Heart, 1.0, Curve::Instant, frame(t + 16));
        assert_eq!((o, moving), (Outline::Shape(Shape::Heart), false));
        let snap = Frame {
            snap: true,
            ..frame(t + 32)
        };
        let (o, moving) = m.outline(id, Shape::Gem, 1.0, spring, snap);
        assert_eq!((o, moving), (Outline::Shape(Shape::Gem), false));
    }

    #[test]
    fn a_preview_leaves_the_morph_to_the_painted_frame() {
        let id = NodeId::new(1, 0);
        let spring = Curve::Spring(Spring::new(700.0, 0.9).unwrap());
        let mut m = Morphs::default();
        m.outline(id, Shape::Circle, 1.0, spring, frame(1000));
        let preview = Frame {
            commit: false,
            ..frame(1010)
        };
        let (o, _) = m.outline(id, Shape::Rect, 1.0, spring, preview);
        assert_eq!(o, Outline::Shape(Shape::Circle));
        assert!(m.pending(|_| true));
        let (_, moving) = m.outline(id, Shape::Rect, 1.0, spring, frame(1016));
        assert!(moving);
        assert!(!m.pending(|_| true));
    }
}

//! (M4) Jelly: squash and stretch while dragged (design.md, "Motion and
//! time": "Jelly and squash while dragging (vector content) | `jelly:
//! 0.4`").
//!
//! A `drag:` source with `jelly: a` stretches along the way it moves and
//! squashes across it, keeping its area, as it follows the pointer and as
//! it springs back to its box when let go. How much follows its speed: up
//! to `a` (0.4: 40 % longer, and as much narrower) at [`FULL_SPEED`] and
//! above. The deformation is a vector on a spring that wobbles (the
//! `jelly` entry's `~`, else [`WOBBLE`]): its amount, at twice the angle
//! of the motion's direction, so that motion one way or the other
//! stretches the same axis and the vector's opposite is a squash along
//! it. A sudden stop overshoots through zero into that squash and rings
//! out, which is the jelly.
//! It is drawn as an affine transform about the node's centre, so vector
//! content stays sharp and it costs nothing to draw.
//!
//! The speed is the node's drawn offset (the lift's, then the glide back)
//! between painted frames, smoothed over [`SMOOTHING`] by each frame's own
//! interval (so a drag stretches alike at 60 Hz and 144 Hz): render needs
//! no pointer velocity from the `Router`, and frames at fixed timestamps
//! give the same jelly every run. A node not lifted and at rest keeps no
//! state; a held node at rest keeps its state but wants no frames. `reduced_motion`
//! and frames with no clock show it undeformed.

use std::collections::HashMap;
use std::time::Duration;

use strand_scene::{Curve, Motion, NodeId, Spring};

use crate::shapes::morph::Frame;

/// The speed (logical pixels per second) at which the stretch is full.
pub(crate) const FULL_SPEED: f32 = 1500.0;

/// The largest `jelly` amount (a node twice as long and half as wide).
pub(crate) const MAX_AMOUNT: f32 = 1.0;

/// The default spring of the deformation: underdamped, so it wobbles.
pub(crate) const WOBBLE: (f32, f32) = (300.0, 0.35);

/// Settling tolerance of the deformation, and the speed counted as still.
const EPS: f32 = 0.002;
const STILL: f32 = 5.0;

/// The speed's smoothing time constant, seconds: each frame moves it
/// `1 − e^(−dt/τ)` of the way to the frame's own speed (half the way at
/// 60 Hz).
pub(crate) const SMOOTHING: f32 = 0.024;

#[derive(Debug)]
struct State {
    /// The offset drawn in the last painted frame, and its time.
    last: Option<([f32; 2], Duration)>,
    /// Smoothed speed, logical pixels per second.
    velocity: [f32; 2],
    /// The deformation: its amount at twice its axis's angle.
    motion: Motion<2>,
    /// It moved or wobbled in its last sample (a held node at rest does
    /// not keep its surface busy).
    moving: bool,
}

/// Every jellied node's motion.
#[derive(Debug, Default)]
pub(crate) struct Jellies {
    nodes: HashMap<NodeId, State>,
}

/// The default curve of the deformation (see [`WOBBLE`]).
pub(crate) fn wobble() -> Curve {
    Spring::new(WOBBLE.0, WOBBLE.1).map_or(Curve::Instant, Curve::Spring)
}

/// The affine transform (about the origin) a deformation `d` draws: a
/// stretch by `1 + |d|` along the axis at half `d`'s angle and a squash by
/// `1 / (1 + |d|)` across it. `[a, b, c, d]` of the 2×2 matrix (kurbo's
/// coefficient order).
pub(crate) fn matrix(d: [f32; 2]) -> Option<[f64; 4]> {
    let m = (d[0] as f64).hypot(d[1] as f64);
    if !m.is_finite() || m < 1e-4 {
        return None;
    }
    let half = (d[1] as f64).atan2(d[0] as f64) / 2.0;
    let (ux, uy) = (half.cos(), half.sin());
    let (along, across) = (1.0 + m, 1.0 / (1.0 + m));
    // R · diag(along, across) · Rᵀ with R's first column the direction.
    let a = along * ux * ux + across * uy * uy;
    let b = (along - across) * ux * uy;
    let c = along * uy * uy + across * ux * ux;
    Some([a, b, b, c])
}

impl Jellies {
    /// The deformation node `id` draws in `frame`: `amount` is its
    /// `jelly`, `offset` its drawn offset (lift or glide), `held` whether
    /// a drag holds it. Also whether it still moves.
    pub(crate) fn sample(
        &mut self,
        id: NodeId,
        amount: f32,
        offset: [f32; 2],
        held: bool,
        curve: Curve,
        frame: Frame,
    ) -> (Option<[f32; 2]>, bool) {
        let amount = if amount.is_finite() {
            amount.clamp(0.0, MAX_AMOUNT)
        } else {
            0.0
        };
        if frame.snap || amount <= 0.0 || curve == Curve::Instant {
            if frame.commit {
                self.nodes.remove(&id);
            }
            return (None, false);
        }
        if !held && !self.nodes.contains_key(&id) {
            return (None, false);
        }
        let s = self.nodes.entry(id).or_insert_with(|| State {
            last: None,
            velocity: [0.0; 2],
            motion: Motion::rest([0.0; 2], EPS).sampled_at(frame.prev),
            moving: false,
        });
        if frame.commit {
            if let Some((o, t)) = s.last
                && frame.at > t
            {
                let dt = (frame.at - t).as_secs_f32();
                let k = 1.0 - (-dt / SMOOTHING).exp();
                for i in 0..2 {
                    let v = (offset[i] - o[i]) / dt;
                    s.velocity[i] += (v - s.velocity[i]) * k;
                }
            }
            s.last = Some((offset, frame.at));
        }
        let speed = s.velocity[0].hypot(s.velocity[1]);
        let target = if speed > STILL && speed.is_finite() {
            let m = amount * (speed / FULL_SPEED).min(1.0);
            let twice = 2.0 * s.velocity[1].atan2(s.velocity[0]);
            [m * twice.cos(), m * twice.sin()]
        } else {
            [0.0; 2]
        };
        if s.motion.target() != target {
            s.motion.retarget(target, curve);
        }
        let v = if frame.commit {
            s.motion.sample(frame.at)
        } else {
            s.motion.peek(frame.at)
        };
        let moving = !s.motion.is_settled(frame.at) || speed > STILL;
        if frame.commit {
            s.moving = moving;
        }
        if frame.commit && !held && !moving {
            self.nodes.remove(&id);
            return (None, false);
        }
        (Some(v), moving)
    }

    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
    }

    /// Anything `under` a surface moving or wobbling (a held node at
    /// rest is not: its next move marks the surface dirty itself).
    pub(crate) fn busy(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.nodes.iter().any(|(id, s)| s.moving && under(*id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ms: u64) -> Frame {
        Frame {
            at: Duration::from_millis(ms),
            commit: true,
            prev: Some(Duration::from_millis(ms.saturating_sub(16))),
            snap: false,
        }
    }

    #[test]
    fn the_matrix_keeps_area_and_stretches_along_the_motion() {
        assert_eq!(matrix([0.0, 0.0]), None);
        let [a, b, c, d] = matrix([0.4, 0.0]).unwrap();
        assert!((a - 1.4).abs() < 1e-6 && b.abs() < 1e-9 && (d - 1.0 / 1.4).abs() < 1e-6);
        assert_eq!(b, c);
        // Twice the angle: (0, -0.4) stretches along (1, -1).
        let [a, b, c, d] = matrix([0.0, -0.4]).unwrap();
        assert!((a * d - b * c - 1.0).abs() < 1e-9, "area kept");
        let along = ((a - b) * (a - b) + (c - d) * (c - d)).sqrt();
        let across = ((a + b) * (a + b) + (c + d) * (c + d)).sqrt();
        assert!(along > across);
    }

    /// Dragged right at speed it stretches along x, up to its amount;
    /// stopped, it overshoots into a squash and settles with no state.
    #[test]
    fn a_drag_stretches_then_wobbles_out() {
        let id = NodeId::new(1, 0);
        let mut j = Jellies::default();
        let curve = wobble();
        let mut ms = 1000;
        let mut x = 0.0;
        let mut most: f32 = 0.0;
        for k in 0..20 {
            ms += 16;
            x += 40.0; // 2,500 px/s
            let (d, moving) = j.sample(id, 0.4, [x, 0.0], true, curve, frame(ms));
            let d = d.unwrap();
            // (The first frame has no speed yet.)
            assert!((moving || k == 0) && d[1].abs() < 1e-4);
            most = most.max(d[0]);
        }
        // (The wobbling spring overshoots its target a little.)
        assert!(most > 0.35 && most < 0.6, "{most}");
        // Let go where it is: it rings out through a squash.
        let mut least: f32 = 0.0;
        for _ in 0..200 {
            ms += 16;
            let (d, moving) = j.sample(id, 0.4, [x, 0.0], false, curve, frame(ms));
            if let Some(d) = d {
                least = least.min(d[0]);
            }
            if !moving {
                break;
            }
        }
        // The opposite vector: stretched along y, squashed along x.
        assert!(least < -0.02, "overshoots into a squash: {least}");
        let [a, _, _, d] = matrix([least, 0.0]).unwrap();
        assert!(a < 1.0 && d > 1.0);
        assert!(j.nodes.is_empty(), "no state at rest");
        assert!(!j.busy(|_| true));
        // Without `jelly`, or snapping, nothing.
        assert_eq!(j.sample(id, 0.0, [0.0; 2], true, curve, frame(ms)).0, None);
        let snap = Frame {
            snap: true,
            ..frame(ms)
        };
        assert_eq!(j.sample(id, 0.4, [0.0; 2], true, curve, snap).0, None);
    }

    /// The same drag (2,400 px/s) stretches alike at 60 Hz and 144 Hz:
    /// the speed's smoothing follows each frame's interval, not a fixed
    /// share per frame.
    #[test]
    fn the_stretch_does_not_depend_on_the_refresh_rate() {
        let at_hz = |hz: u64| {
            let id = NodeId::new(1, 0);
            let mut j = Jellies::default();
            let curve = wobble();
            // From rest at 1 s, a twelfth of a second of drag: 5 frames
            // at 60 Hz, 12 at 144 Hz.
            for k in 0..=hz / 12 {
                let us = 1_000_000 + k * 1_000_000 / hz;
                let at = Duration::from_micros(us);
                let f = Frame {
                    at,
                    commit: true,
                    prev: Some(Duration::from_micros(us - 1_000_000 / hz)),
                    snap: false,
                };
                let x = 2400.0 * (k as f32 / hz as f32);
                j.sample(id, 0.4, [x, 0.0], true, curve, f);
            }
            j.nodes[&id].velocity[0]
        };
        let (slow, fast) = (at_hz(60), at_hz(144));
        // Both 2,400 · (1 − e^(−83 ms / τ)) ≈ 2,325; a fixed half per
        // frame gave 2,325 at 60 Hz and 2,399 at 144 Hz.
        assert!((slow - 2325.0).abs() < 5.0, "{slow}");
        assert!((slow - fast).abs() < 3.0, "{slow} vs {fast}");
    }

    /// A held node that stopped moving keeps its state (the drag goes
    /// on) but no longer keeps its surface busy.
    #[test]
    fn a_held_node_at_rest_wants_no_frames() {
        let id = NodeId::new(1, 0);
        let mut j = Jellies::default();
        let curve = wobble();
        let mut ms = 1000;
        for k in 1..=10 {
            ms += 16;
            j.sample(id, 0.4, [k as f32 * 30.0, 0.0], true, curve, frame(ms));
        }
        assert!(j.busy(|_| true), "moving");
        for _ in 0..300 {
            ms += 16;
            j.sample(id, 0.4, [300.0, 0.0], true, curve, frame(ms));
        }
        assert!(j.nodes.contains_key(&id), "still held");
        assert!(!j.busy(|_| true), "at rest");
    }
}

//! (M4) Shared-element morph (design.md, "Motion and time": `morph:
//! "media"` on both nodes, "so the bar's media pill becomes the media
//! panel"; m4-plan: "Morph across surfaces needs surface origins, else
//! falls back to the enter pose").
//!
//! Render remembers where each `morph` name was last drawn (the node's
//! box with its paint offsets, logical pixels on its surface). A node
//! with a `morph` name that enters while another node of that name was
//! drawn on the same surface within the last second starts from that
//! box. It is drawn moved and scaled (about its centre, separately in
//! each axis) so its box covers the old one, then springs to its own box
//! along the `morph` entry's `~` (`$motion.spatial` by default). It
//! plays that in place of its `enter` pose, and its subtree moves and
//! scales with it. The node it came from leaves as it would anyway.
//!
//! Render knows nothing of where surfaces sit on the outputs, so a name
//! last drawn on another surface gives no box to start from. The node
//! plays its `enter` pose instead. `reduced_motion` and frames with no
//! clock show it in place at once.

use std::collections::HashMap;
use std::time::Duration;

use strand_scene::{Curve, LogicalRect, Motion, NodeId};

use crate::shapes::morph::Frame;

/// Settling tolerance (pixels and scale).
const EPS: f32 = 0.01;

/// How long a box is remembered after it was last drawn.
const STALE: Duration = Duration::from_secs(1);

/// The largest scale a morph starts at, either way.
const MAX_SCALE: f32 = 100.0;

#[derive(Debug)]
struct Seen {
    id: NodeId,
    root: NodeId,
    rect: LogicalRect,
    at: Duration,
}

/// Where each `morph` name was last drawn, and the morphs in flight.
#[derive(Debug, Default)]
pub(crate) struct SharedMorphs {
    seen: HashMap<String, Seen>,
    /// Per node: `[dx, dy, sx, sy]` springing to `[0, 0, 1, 1]`.
    flights: HashMap<NodeId, Motion<4>>,
}

impl SharedMorphs {
    /// Node `id` named `key`, laid out at `rect` on the surface of
    /// `root`, in `frame`: how far it is drawn from its box (`[dx, dy,
    /// sx, sy]`, `None` at its box), whether it is still moving and
    /// whether its morph started now. `entering`: it is about to play its
    /// enter pose.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn morph(
        &mut self,
        id: NodeId,
        key: &str,
        root: NodeId,
        rect: LogicalRect,
        entering: bool,
        curve: Curve,
        frame: Frame,
    ) -> (Option<[f32; 4]>, bool, bool) {
        let mut started = false;
        if entering
            && !frame.snap
            && let Some(seen) = self.seen.get(key)
            && seen.id != id
            && seen.root == root
            && frame.at.saturating_sub(seen.at) <= STALE
            && rect.w > 0.0
            && rect.h > 0.0
            && seen.rect.w > 0.0
            && seen.rect.h > 0.0
        {
            let c = |r: &LogicalRect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
            let ((ox, oy), (nx, ny)) = (c(&seen.rect), c(&rect));
            let scale = |a: f32, b: f32| (a / b).clamp(1.0 / MAX_SCALE, MAX_SCALE);
            let from = [
                ox - nx,
                oy - ny,
                scale(seen.rect.w, rect.w),
                scale(seen.rect.h, rect.h),
            ];
            if from.iter().all(|v| v.is_finite()) {
                let mut m = Motion::rest(from, EPS).sampled_at(frame.prev);
                m.retarget([0.0, 0.0, 1.0, 1.0], curve);
                self.flights.insert(id, m);
                started = true;
            }
        }
        if frame.commit {
            self.seen.insert(
                key.to_string(),
                Seen {
                    id,
                    root,
                    rect,
                    at: frame.at,
                },
            );
        }
        if frame.snap {
            if frame.commit {
                self.flights.remove(&id);
            }
            return (None, false, started);
        }
        let Some(m) = self.flights.get_mut(&id) else {
            return (None, false, started);
        };
        let v = if frame.commit {
            m.sample(frame.at)
        } else {
            m.peek(frame.at)
        };
        let moving = !m.is_settled(frame.at);
        if !moving {
            if frame.commit {
                self.flights.remove(&id);
            }
            return (None, false, started);
        }
        (Some(v), true, started)
    }

    /// Anything `under` a surface morphing.
    pub(crate) fn busy(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.flights.keys().any(|id| under(*id))
    }

    /// `id` morphs no more (its box stays remembered, so a node replacing
    /// it can start there).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.flights.remove(&id);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.flights.retain(|id, _| keep(*id));
    }
}

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

use std::collections::{HashMap, HashSet};
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
    /// The latest committed frame's time per surface root: a box older
    /// than [`STALE`] by its own surface's clock can start no morph and
    /// is dropped ([`SharedMorphs::prune`]).
    latest: HashMap<NodeId, Duration>,
    /// How many boxes were remembered after the last prune.
    kept: usize,
    /// Per node: `[dx, dy, sx, sy]` springing to `[0, 0, 1, 1]`.
    flights: HashMap<NodeId, Motion<4>>,
    /// Nodes a preview saw start a morph: the next painted frame starts
    /// it, from the boxes and offsets of its own time.
    pending: HashSet<NodeId>,
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
        // A morph a preview saw start begins on the painted frame.
        let entering = if frame.commit {
            self.pending.remove(&id) || entering
        } else {
            entering
        };
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
            if from.iter().all(|v| v.is_finite()) && !frame.commit {
                // A preview is drawn at the last frame's time, with its
                // ancestors' offsets of then: starting here would carry
                // their one-frame-old offsets into the whole morph. The
                // painted frame starts it (`busy` asks for a fresh
                // flatten), and the node plays no enter pose meanwhile.
                self.pending.insert(id);
                return (None, false, true);
            }
            if from.iter().all(|v| v.is_finite()) {
                let mut m = Motion::rest(from, EPS).sampled_at(frame.prev);
                m.retarget([0.0, 0.0, 1.0, 1.0], curve);
                self.flights.insert(id, m);
                started = true;
            }
        }
        if frame.commit {
            let latest = self.latest.entry(root).or_default();
            *latest = (*latest).max(frame.at);
            self.seen.insert(
                key.to_string(),
                Seen {
                    id,
                    root,
                    rect,
                    at: frame.at,
                },
            );
            // Computed names (`morph: "note-" + n.id`) come and go: drop
            // the stale ones whenever the map has doubled since the last
            // prune, so it stays bounded at little cost per frame.
            if self.seen.len() > 2 * self.kept + 16 {
                self.prune();
            }
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
        self.flights
            .keys()
            .chain(&self.pending)
            .any(|id| under(*id))
    }

    /// `id` morphs no more (its box stays remembered, so a node replacing
    /// it can start there).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.flights.remove(&id);
        self.pending.remove(&id);
    }

    /// Drops the morphs a preview saw start for nodes `under` a surface
    /// whose painted frame did not draw them.
    pub(crate) fn drop_pending(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        self.pending.retain(|id| !under(*id));
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.flights.retain(|id, _| keep(*id));
        self.pending.retain(|id| keep(*id));
        self.latest.retain(|root, _| keep(*root));
        let latest = &self.latest;
        self.seen.retain(|_, s| latest.contains_key(&s.root));
        self.prune();
    }

    /// Drops the boxes no morph can start from: older than [`STALE`] by
    /// their surface's latest frame.
    fn prune(&mut self) {
        let latest = &self.latest;
        self.seen.retain(|_, s| {
            latest
                .get(&s.root)
                .is_some_and(|t| t.saturating_sub(s.at) <= STALE)
        });
        self.kept = self.seen.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One computed `morph` name per notification, for a long session:
    /// the remembered boxes stay bounded, and a box within [`STALE`]
    /// still starts a morph.
    #[test]
    fn stale_names_are_dropped() {
        let root = NodeId::new(1, 0);
        let rect = LogicalRect::new(0.0, 0.0, 10.0, 10.0);
        let mut m = SharedMorphs::default();
        let frame = |ms: u64| Frame {
            at: Duration::from_millis(ms),
            commit: true,
            prev: Some(Duration::from_millis(ms.saturating_sub(16))),
            snap: false,
        };
        let mut most = 0;
        for i in 0..10_000u32 {
            let ms = 1000 + u64::from(i) * 100;
            let key = format!("note-{i}");
            m.morph(
                NodeId::new(10 + i, 0),
                &key,
                root,
                rect,
                true,
                Curve::Instant,
                frame(ms),
            );
            most = most.max(m.seen.len());
        }
        // A box lives 1 s, one name each 100 ms: about ten fresh ones.
        assert!(most < 64, "{most} boxes remembered");
        // A fresh box still starts a morph (from 10 px to the left).
        let ms = 1000 + 10_000 * 100;
        let r = LogicalRect::new(10.0, 0.0, 10.0, 10.0);
        let key = "note-9999";
        let curve = Curve::Spring(strand_scene::Spring::new(300.0, 1.0).unwrap());
        let (_, moving, started) = m.morph(NodeId::new(1, 1), key, root, r, true, curve, frame(ms));
        assert!(started && moving);
        // Its surface gone: everything it remembered goes.
        m.retain(|id| id != root);
        assert!(m.seen.is_empty() && m.latest.is_empty());
    }
}

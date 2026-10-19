//! (M4) Shared-element morph (design.md, "Motion and time": `morph:
//! "media"` on both nodes, "so the bar's media pill becomes the media
//! panel"; m4-plan: "Morph across surfaces needs surface origins, else
//! falls back to the enter pose").
//!
//! Render remembers where each `morph` name was last drawn (the node's
//! box with its paint offsets, logical pixels on its surface), as each
//! painted frame shows it, whether flattened for that frame or reusing a
//! scene flattened before (shared-morph-on). A node with a `morph` name
//! that enters while another node of that name is on screen, or left it
//! within the last second, starts from that box: a box is on screen
//! from the painted frame that draws it until one of its surface's
//! painted frames no longer does, or the surface goes. It is drawn
//! moved and scaled (about its centre, separately in each axis) so its
//! box covers the old one, then springs to its own box along the
//! `morph` entry's `~` (`$motion.spatial` by default). It plays that in
//! place of its `enter` pose (or, if a preview saw it start and its
//! painted frame finds the box gone stale, its pose after all), and
//! its subtree moves and scales with it; the props it was created with
//! show at their values, as any new node's do (none springs from unset
//! under the flight). The node it came from leaves as it would anyway.
//!
//! A name last drawn on another surface starts the morph too when both
//! surfaces lie on the same output and the host told render where
//! (`Renderer::set_surface_origin`, from the surface manager's
//! placement): the old box is moved by the difference between the two
//! surfaces' origins, so the bar's pill grows into the media panel. With
//! either origin unknown, or the surfaces on two outputs, there is no box
//! to start from and the node plays its `enter` pose. `reduced_motion` and
//! frames with no clock show it in place at once.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use strand_scene::{Curve, LogicalRect, Motion, NodeId};

use crate::flatten::SurfaceOrigin;

/// Where each surface lies on its output, by its root.
pub(crate) type Origins = HashMap<NodeId, SurfaceOrigin>;
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
    /// The time of the painted frame that last drew it, or once it is off
    /// screen, of the one that no longer did.
    at: Duration,
    /// Drawn by the latest painted frame of its surface: on screen
    /// however long ago that was.
    shown: bool,
}

/// Whether a node's morph starts ([`SharedMorphs::morph`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// None starts.
    No,
    /// It starts now (in a preview: on the painted frame, the node
    /// playing no enter pose meanwhile).
    Now,
    /// A preview saw it start, but the painted frame has no box to start
    /// from (the box left the screen over a second before it): the node
    /// plays its enter pose after all.
    Missed,
}

/// A `morph` name a flattened scene draws, where (`Seen::rect`): what
/// its surface shows once a frame paints that scene
/// ([`SharedMorphs::painted`]).
#[derive(Clone, Debug)]
pub(crate) struct MorphBox {
    pub key: String,
    pub id: NodeId,
    pub rect: LogicalRect,
}

impl Seen {
    /// Its box on the surface of `root`: as seen on its own surface, or
    /// moved from another surface on the same output (`origins`: where
    /// each surface lies, by root).
    fn rect_on(&self, root: NodeId, origins: &Origins) -> Option<LogicalRect> {
        if self.root == root {
            return Some(self.rect);
        }
        let (from, to) = (origins.get(&self.root)?, origins.get(&root)?);
        (from.output == to.output).then(|| {
            LogicalRect::new(
                self.rect.x + from.at.x - to.at.x,
                self.rect.y + from.at.y - to.at.y,
                self.rect.w,
                self.rect.h,
            )
        })
    }
}

/// Where each `morph` name was last drawn, and the morphs in flight.
#[derive(Debug, Default)]
pub(crate) struct SharedMorphs {
    /// Per name, where each surface last drew it (one box per surface
    /// root: the last of the name in its paint order).
    seen: HashMap<String, Vec<Seen>>,
    /// The latest painted frame's time per surface root: a box off
    /// screen for longer than [`STALE`] by its own surface's clock can
    /// start no morph and is dropped ([`SharedMorphs::prune`]).
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
    /// `root` (`place`: the root, and where each surface lies on its
    /// output, by root), in `frame`: how far it is drawn from its box (`[dx, dy,
    /// sx, sy]`, `None` at its box), whether it is still moving and
    /// whether its morph starts. `entering`: it is about to play its
    /// enter pose. Where it is drawn is remembered once a frame paints it
    /// ([`SharedMorphs::painted`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn morph(
        &mut self,
        id: NodeId,
        key: &str,
        (root, origins): (NodeId, &Origins),
        rect: LogicalRect,
        entering: bool,
        curve: Curve,
        frame: Frame,
    ) -> (Option<[f32; 4]>, bool, Start) {
        let mut started = Start::No;
        // A morph a preview saw start begins on the painted frame.
        let pending = frame.commit && self.pending.remove(&id);
        if pending {
            started = Start::Missed;
        }
        let entering = pending || entering;
        // (cross-surface-morph) Where another node of the name is: on
        // screen before off it, and the latest drawn of either. The bar's
        // pill stays on screen while the panel it grew into closes, so
        // the panel opened again grows from the pill, not from where its
        // own card was last drawn.
        let from = || {
            self.seen
                .get(key)?
                .iter()
                .filter(|s| s.id != id && (s.shown || frame.at.saturating_sub(s.at) <= STALE))
                .filter_map(|s| Some(((s.shown, s.at), s.rect_on(root, origins)?)))
                .max_by_key(|(order, _)| *order)
                .map(|(_, r)| r)
        };
        if entering
            && !frame.snap
            && let Some(old) = from()
            && rect.w > 0.0
            && rect.h > 0.0
            && old.w > 0.0
            && old.h > 0.0
        {
            let c = |r: &LogicalRect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
            let ((ox, oy), (nx, ny)) = (c(&old), c(&rect));
            let scale = |a: f32, b: f32| (a / b).clamp(1.0 / MAX_SCALE, MAX_SCALE);
            let from = [ox - nx, oy - ny, scale(old.w, rect.w), scale(old.h, rect.h)];
            if from.iter().all(|v| v.is_finite()) && !frame.commit {
                // A preview is drawn at the last frame's time, with its
                // ancestors' offsets of then: starting here would carry
                // their one-frame-old offsets into the whole morph. The
                // painted frame starts it (`busy` asks for a fresh
                // flatten), and the node plays no enter pose meanwhile.
                self.pending.insert(id);
                return (None, false, Start::Now);
            }
            if from.iter().all(|v| v.is_finite()) {
                let mut m = Motion::rest(from, EPS).sampled_at(frame.prev);
                m.retarget([0.0, 0.0, 1.0, 1.0], curve);
                self.flights.insert(id, m);
                started = Start::Now;
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
            frame.sample(m)
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

    /// A frame of the surface of `root` painted at `at` shows `drawn`
    /// (its scene's [`MorphBox`]es, in paint order: the last of a name
    /// wins), whether that scene was flattened for it or reused. A box of
    /// that surface it no longer draws is off screen from `at` on.
    pub(crate) fn painted(&mut self, root: NodeId, at: Duration, drawn: &[MorphBox]) {
        let latest = self.latest.entry(root).or_default();
        *latest = (*latest).max(at);
        for s in self.seen.values_mut().flatten() {
            if s.root == root && s.shown {
                s.shown = false;
                s.at = s.at.max(at);
            }
        }
        for b in drawn {
            let seen = Seen {
                id: b.id,
                root,
                rect: b.rect,
                at,
                shown: true,
            };
            let boxes = self.seen.entry(b.key.clone()).or_default();
            match boxes.iter_mut().find(|s| s.root == root) {
                Some(s) => *s = seen,
                None => boxes.push(seen),
            }
        }
        // Computed names (`morph: "note-" + n.id`) come and go: drop the
        // stale ones whenever the map has doubled since the last prune,
        // so it stays bounded at little cost per frame.
        if self.seen.len() > 2 * self.kept + 16 {
            self.prune();
        }
    }

    /// The surface of `root` is gone (no surface shows it): its boxes
    /// left the screen with its last painted frame.
    pub(crate) fn hidden(&mut self, root: NodeId) {
        for s in self.seen.values_mut().flatten() {
            if s.root == root {
                s.shown = false;
            }
        }
    }

    /// Anything `under` a surface morphing.
    pub(crate) fn busy(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.flights
            .keys()
            .chain(&self.pending)
            .any(|id| under(*id))
    }

    /// A preview saw a node `under` a surface start a morph
    /// (roll-preview).
    pub(crate) fn pending(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.pending.iter().any(|id| under(*id))
    }

    /// `id` morphs no more (its box stays remembered, so a node replacing
    /// it can start there).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.flights.remove(&id);
        self.pending.remove(&id);
    }

    /// Nodes `under` a surface that closed morph no more; the boxes
    /// their names were drawn at stay remembered.
    pub(crate) fn stop(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        self.flights.retain(|id, _| !under(*id));
        self.pending.retain(|id| !under(*id));
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
        for boxes in self.seen.values_mut() {
            boxes.retain(|s| latest.contains_key(&s.root));
        }
        self.prune();
    }

    /// Drops the boxes no morph can start from: off screen for longer
    /// than [`STALE`] by their surface's latest frame.
    fn prune(&mut self) {
        let latest = &self.latest;
        for boxes in self.seen.values_mut() {
            boxes.retain(|s| {
                latest
                    .get(&s.root)
                    .is_some_and(|t| s.shown || t.saturating_sub(s.at) <= STALE)
            });
        }
        self.seen.retain(|_, boxes| !boxes.is_empty());
        self.kept = self.seen.len();
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
            lead: strand_scene::motion::START_LEAD,
            snap: false,
        }
    }

    fn shown(key: &str, id: NodeId) -> MorphBox {
        MorphBox {
            key: key.into(),
            id,
            rect: LogicalRect::new(0.0, 0.0, 10.0, 10.0),
        }
    }

    /// Whether node `id` named `key`, entering at `ms` on `root`'s
    /// surface 10 px right of the box `shown` gives, starts a morph.
    fn starts(m: &mut SharedMorphs, root: NodeId, id: NodeId, key: &str, ms: u64) -> bool {
        let r = LogicalRect::new(10.0, 0.0, 10.0, 10.0);
        let curve = Curve::Spring(strand_scene::Spring::new(300.0, 1.0).unwrap());
        let (_, moving, started) =
            m.morph(id, key, (root, &Origins::new()), r, true, curve, frame(ms));
        assert_eq!(started == Start::Now, moving);
        started == Start::Now
    }

    /// One computed `morph` name per notification, each shown for one
    /// frame, for a long session: the remembered boxes stay bounded, and
    /// a box on screen still starts a morph.
    #[test]
    fn stale_names_are_dropped() {
        let root = NodeId::new(1, 0);
        let mut m = SharedMorphs::default();
        let mut most = 0;
        for i in 0..10_000u32 {
            let ms = 1000 + u64::from(i) * 100;
            let key = format!("note-{i}");
            m.painted(
                root,
                Duration::from_millis(ms),
                &[shown(&key, NodeId::new(10 + i, 0))],
            );
            most = most.max(m.seen.len());
        }
        // A box lives 1 s off screen, one name each 100 ms: about ten.
        assert!(most < 64, "{most} boxes remembered");
        // The box on screen still starts a morph (from 10 px to the left).
        let ms = 1000 + 10_000 * 100;
        assert!(starts(&mut m, root, NodeId::new(1, 1), "note-9999", ms));
        // Its surface gone: everything it remembered goes.
        m.retain(|id| id != root);
        assert!(m.seen.is_empty() && m.latest.is_empty());
    }

    /// (popup-reopen review) A surface that closes stops the morphs of
    /// its nodes: nothing of them keeps its root busy, and the boxes
    /// names were drawn at stay for a node elsewhere to start from.
    #[test]
    fn a_closed_surface_stops_its_morphs_and_keeps_its_boxes() {
        let root = NodeId::new(1, 0);
        let (pill, card) = (NodeId::new(10, 0), NodeId::new(20, 0));
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        assert!(starts(&mut m, root, card, "m", 1016));
        let closed = |id: NodeId| id == card;
        assert!(m.busy(closed), "the card morphs");
        m.stop(closed);
        assert!(!m.busy(closed), "the closed surface's morph is stopped");
        // The pill's box is still where a morph can start.
        assert!(starts(&mut m, root, NodeId::new(21, 0), "m", 1032));
    }

    /// (cross-surface-morph) The pill stays on screen on the bar while
    /// the panel its card grew into closes: the card is the name's
    /// latest box, but off screen, so the card entering again (the panel
    /// opened again) starts from the pill, on screen, and the pill's own
    /// surface keeps its box whatever the panel's frames draw.
    #[test]
    fn a_box_on_screen_is_taken_before_one_that_left() {
        let (bar, panel) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let (pill, card) = (NodeId::new(10, 0), NodeId::new(20, 0));
        // Both surfaces at the output's origin.
        let origins: Origins = [bar, panel]
            .into_iter()
            .map(|r| {
                let at = strand_scene::LogicalPoint::new(0.0, 0.0);
                (
                    r,
                    SurfaceOrigin {
                        output: "DP-1".into(),
                        at,
                    },
                )
            })
            .collect();
        let starts = |m: &mut SharedMorphs, id: NodeId, ms: u64| {
            let r = LogicalRect::new(10.0, 0.0, 10.0, 10.0);
            let curve = Curve::Spring(strand_scene::Spring::new(300.0, 1.0).unwrap());
            let (_, _, started) = m.morph(id, "m", (panel, &origins), r, true, curve, frame(ms));
            started == Start::Now
        };
        let pill_box = MorphBox {
            key: "m".into(),
            id: pill,
            rect: LogicalRect::new(0.0, 0.0, 10.0, 10.0),
        };
        let mut m = SharedMorphs::default();
        m.painted(
            bar,
            Duration::from_millis(1000),
            std::slice::from_ref(&pill_box),
        );
        m.painted(panel, Duration::from_millis(1016), &[shown("m", card)]);
        m.hidden(panel);
        assert_eq!(m.seen.get("m").map_or(0, Vec::len), 2, "a box per surface");
        // Long after the card left: the pill, on screen, starts it.
        assert!(starts(&mut m, card, 60_000), "from the pill");
        // The pill gone too: the latest off screen, within a second.
        let mut m = SharedMorphs::default();
        m.painted(
            bar,
            Duration::from_millis(1000),
            std::slice::from_ref(&pill_box),
        );
        m.painted(panel, Duration::from_millis(1016), &[shown("m", card)]);
        m.hidden(bar);
        m.hidden(panel);
        assert!(starts(&mut m, NodeId::new(21, 0), 1500));
        assert!(!starts(&mut m, NodeId::new(22, 0), 2100), "stale");
        // Its own box is no place to start from.
        let mut m = SharedMorphs::default();
        m.painted(panel, Duration::from_millis(1016), &[shown("m", card)]);
        m.hidden(panel);
        assert!(!starts(&mut m, card, 1500), "not from itself");
    }

    /// (shared-morph-on) A box drawn by its surface's latest painted
    /// frame starts a morph however long ago that frame was; once a
    /// painted frame no longer draws it, or its surface goes, for a
    /// second only.
    #[test]
    fn a_box_on_screen_starts_a_morph_until_it_left_a_second_ago() {
        let (root, pill) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        // On screen, idle 60 s: still the place to start from.
        assert!(starts(&mut m, root, NodeId::new(3, 0), "m", 61_000));
        // A frame at 61 s no longer draws it: a second from then.
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        m.painted(root, Duration::from_millis(61_000), &[]);
        assert!(starts(&mut m, root, NodeId::new(3, 0), "m", 61_900));
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        m.painted(root, Duration::from_millis(61_000), &[]);
        assert!(!starts(&mut m, root, NodeId::new(3, 0), "m", 62_100));
        // Another surface's frames leave it on screen.
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        m.painted(NodeId::new(9, 0), Duration::from_millis(61_000), &[]);
        assert!(starts(&mut m, root, NodeId::new(3, 0), "m", 61_900));
        // Its surface gone: off screen since its last frame.
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        m.hidden(root);
        assert!(starts(&mut m, root, NodeId::new(3, 0), "m", 1900));
        let mut m = SharedMorphs::default();
        m.painted(root, Duration::from_millis(1000), &[shown("m", pill)]);
        m.hidden(root);
        assert!(!starts(&mut m, root, NodeId::new(3, 0), "m", 2100));
    }
}

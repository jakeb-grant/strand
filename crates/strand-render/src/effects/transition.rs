//! (M4) Transition masks (design.md, "Motion and time": `transition:
//! wipe(left) | disc | dissolve | pixelate` on `if`, `pages` and image
//! swaps; decisions.md 2026-10-05: a branch's transition is written on
//! the branch's root node, a `pages`' on the `pages` element).
//!
//! A node with `transition:` that enters is revealed by its mask, which
//! grows from nothing to the whole box. When it leaves, its ghost is
//! hidden the same way, the mask shrinking back. A `pages` with
//! `transition:` swaps its pages through the mask: the page drawn on top
//! (the later in the `pages`' children) carries it. If that is the new
//! page, it is revealed over the old. If it is the old page, it is
//! hidden by the complement of the same mask over the new. The old page
//! plays out until the new one is in. Progress springs along the
//! `transition` entry's `~` (`$motion.spatial` by default, through
//! `Prop::X`'s default since the prop itself is a snap). `reduced_motion`
//! and frames with no clock swap at once.
//!
//! The masks at progress `p` (0 hidden, 1 shown), over the node's box:
//!
//! - `wipe(edge)`: the part of the box within `p` of its width (or
//!   height) from `edge` (`left` by default): it grows from that edge.
//! - `disc`: a disc about the box's centre whose radius reaches the
//!   corners at `p = 1`.
//! - `dissolve`: the box's 4 px cells (logical) whose random rank is
//!   below `p`.
//! - `pixelate`: the subtree drawn at low resolution and sampled back up,
//!   a mosaic of square cells `(1 − p) · 12` px wide (logical, aligned to
//!   the box's corner; each cell its pixels' average) that refines to the
//!   plain drawing as the node fades in at opacity `p`.
//!
//! wipe, disc and dissolve are clip paths, so they cost a clip. pixelate
//! is an offscreen group ([`crate::layers::Layer::mosaic`]), so it costs
//! one pass over the group's pixels.

use std::collections::{HashMap, HashSet};

use strand_scene::{Curve, Edge, Motion, NodeId, PropValue};
use vello_cpu::kurbo::{self, BezPath, Shape};

use crate::shapes::morph::Frame;

/// Settling tolerance of a mask's progress.
const EPS: f32 = 0.002;

/// `dissolve`'s cell, logical pixels.
const CELL: f64 = 4.0;

/// `pixelate`'s cell at the start, logical pixels.
const PIXELATE_CELL: f32 = 12.0;

/// A transition mask.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    Wipe(Edge),
    Disc,
    Dissolve,
    Pixelate,
}

impl Kind {
    /// `transition:`'s value: `wipe(left)`, `disc`, `dissolve`,
    /// `pixelate` (keywords or calls).
    pub(crate) fn of(v: &PropValue) -> Option<Kind> {
        let (name, args): (&str, &[PropValue]) = match v {
            PropValue::Keyword(k) | PropValue::Text(k) => (k, &[]),
            PropValue::Call { name, args } => (name, args),
            _ => return None,
        };
        Some(match name {
            "wipe" => Kind::Wipe(match args.first() {
                Some(PropValue::Keyword(e)) => match e.as_str() {
                    "right" => Edge::Right,
                    "top" => Edge::Top,
                    "bottom" => Edge::Bottom,
                    _ => Edge::Left,
                },
                _ => Edge::Left,
            }),
            "disc" => Kind::Disc,
            "dissolve" => Kind::Dissolve,
            "pixelate" => Kind::Pixelate,
            _ => return None,
        })
    }
}

/// True if `node` leaves through its own transition mask: removed, it
/// plays out as a ghost.
pub(crate) fn leaves_masked(node: &crate::tree::Node) -> bool {
    node.kind != strand_scene::NodeKind::Pages
        && node
            .get(strand_scene::Prop::Transition)
            .is_some_and(|v| Kind::of(v).is_some() || v.has_tokens())
}

/// What a node draws under its mask this frame.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Masked {
    pub kind: Kind,
    /// Progress, 0 (hidden) to 1 (shown).
    pub p: f32,
    /// The complement of the mask at `p` (an old page on top of the new).
    pub invert: bool,
}

/// What a mask draws as.
pub(crate) enum Drawn {
    /// A clip path over the subtree.
    Clip(BezPath),
    /// An offscreen group: a mosaic of square cells this many physical
    /// pixels wide (1: none) at this opacity.
    Mosaic { cell: u16, opacity: f32 },
}

impl Masked {
    /// The mask over a box `frame` (physical pixels) on a surface at
    /// `scale`.
    pub(crate) fn drawn(&self, frame: kurbo::Rect, scale: f64) -> Drawn {
        let p = self.p.clamp(0.0, 1.0) as f64;
        // How much of the node shows: `p`, or the rest of it.
        let shown = if self.invert { 1.0 - p } else { p };
        match self.kind {
            Kind::Pixelate => Drawn::Mosaic {
                cell: ((1.0 - shown) * PIXELATE_CELL as f64 * scale)
                    .round()
                    .clamp(1.0, 1024.0) as u16,
                opacity: shown as f32,
            },
            Kind::Wipe(edge) => {
                let (w, h) = (frame.width(), frame.height());
                // The part within `p` of `edge`, or (inverted) the part
                // beyond it.
                let r = match (edge, self.invert) {
                    (Edge::Left, false) => frame.with_size((w * p, h)),
                    (Edge::Left, true) => {
                        kurbo::Rect::new(frame.x0 + w * p, frame.y0, frame.x1, frame.y1)
                    }
                    (Edge::Right, false) => {
                        kurbo::Rect::new(frame.x1 - w * p, frame.y0, frame.x1, frame.y1)
                    }
                    (Edge::Right, true) => frame.with_size((w * (1.0 - p), h)),
                    (Edge::Top, false) => frame.with_size((w, h * p)),
                    (Edge::Top, true) => {
                        kurbo::Rect::new(frame.x0, frame.y0 + h * p, frame.x1, frame.y1)
                    }
                    (Edge::Bottom, false) => {
                        kurbo::Rect::new(frame.x0, frame.y1 - h * p, frame.x1, frame.y1)
                    }
                    (Edge::Bottom, true) => frame.with_size((w, h * (1.0 - p))),
                };
                Drawn::Clip(r.to_path(0.1))
            }
            Kind::Disc => {
                let c = frame.center();
                let reach = (frame.width().powi(2) + frame.height().powi(2)).sqrt() / 2.0;
                let radius = reach * p;
                let mut path = BezPath::new();
                if self.invert {
                    // The box with the disc cut out: the disc runs the
                    // other way round, so non-zero winding leaves it
                    // empty.
                    path.extend(frame.to_path(0.1));
                }
                const N: usize = 96;
                for i in 0..N {
                    let a = std::f64::consts::TAU * i as f64 / N as f64;
                    let a = if self.invert { -a } else { a };
                    let pt = kurbo::Point::new(c.x + radius * a.cos(), c.y + radius * a.sin());
                    if i == 0 {
                        path.move_to(pt);
                    } else {
                        path.line_to(pt);
                    }
                }
                path.close_path();
                Drawn::Clip(path)
            }
            Kind::Dissolve => {
                let cell = (CELL * scale).max(1.0);
                let cols = (frame.width() / cell).ceil().clamp(0.0, 4096.0) as i64;
                let rows = (frame.height() / cell).ceil().clamp(0.0, 4096.0) as i64;
                let mut path = BezPath::new();
                for y in 0..rows {
                    for x in 0..cols {
                        let rank = super::builtin::rand(y * 4096 + x, 7, 3) as f64;
                        if (rank < p) != self.invert {
                            let x0 = frame.x0 + x as f64 * cell;
                            let y0 = frame.y0 + y as f64 * cell;
                            let r = kurbo::Rect::new(
                                x0,
                                y0,
                                (x0 + cell).min(frame.x1),
                                (y0 + cell).min(frame.y1),
                            );
                            path.extend(r.to_path(0.1));
                        }
                    }
                }
                Drawn::Clip(path)
            }
        }
    }
}

impl Masked {
    /// The items that open and close the mask's group over a box `frame`
    /// (physical pixels) on a surface at `scale`, under the transform
    /// `xform`: a clip, or pixelate's offscreen mosaic.
    pub(crate) fn group(
        &self,
        frame: kurbo::Rect,
        scale: f32,
        xform: kurbo::Affine,
    ) -> (crate::flatten::Item, crate::flatten::Item) {
        use crate::flatten::Item;
        match self.drawn(frame, scale as f64) {
            Drawn::Clip(path) => (Item::PushClip(path), Item::PopClip),
            Drawn::Mosaic { cell, opacity } => {
                let layer = crate::layers::Layer {
                    effects: std::sync::Arc::from([strand_scene::Effect::Opacity(opacity)]),
                    frame,
                    scale,
                    xform,
                    mosaic: (cell > 1).then_some(cell),
                    gpu: None,
                };
                (Item::PushLayer(std::sync::Arc::new(layer)), Item::PopLayer)
            }
        }
    }
}

/// Every mask in flight, and the ghosts they hold.
#[derive(Debug, Default)]
pub(crate) struct Reveals {
    nodes: HashMap<NodeId, Motion<1>>,
    /// Ghosts playing out until a mask is done (their exit waits).
    holds: HashSet<NodeId>,
}

impl Reveals {
    /// Starts (or turns round) node `id`'s mask towards `to` (0 or 1),
    /// from `from` if it has none in flight.
    pub(crate) fn aim(&mut self, id: NodeId, from: f32, to: f32, curve: Curve, frame: Frame) {
        let m = self
            .nodes
            .entry(id)
            .or_insert_with(|| Motion::rest([from], EPS).sampled_at(frame.prev));
        if m.target() != [to] {
            m.retarget([to], curve);
        }
    }

    /// Node `id`'s mask in `frame` (`None`: it has none), and whether it
    /// is still moving. A mask settled at 1 is done and dropped.
    pub(crate) fn progress(&mut self, id: NodeId, frame: Frame) -> Option<(f32, bool)> {
        let m = self.nodes.get_mut(&id)?;
        let p = if frame.commit {
            m.sample(frame.at)[0]
        } else {
            m.peek(frame.at)[0]
        };
        let moving = !m.is_settled(frame.at);
        if !moving && m.target() == [1.0] && frame.commit {
            self.nodes.remove(&id);
        }
        Some((p.clamp(0.0, 1.0), moving))
    }

    /// Node `id`'s mask at `at` without starting anything.
    pub(crate) fn peek(&self, id: NodeId, at: std::time::Duration) -> Option<f32> {
        self.nodes.get(&id).map(|m| m.peek(at)[0].clamp(0.0, 1.0))
    }

    /// True while node `id`'s mask moves.
    pub(crate) fn moving(&self, id: NodeId, at: std::time::Duration) -> bool {
        self.nodes.get(&id).is_some_and(|m| !m.is_settled(at))
    }

    /// Ghost `id` waits (or not) for a mask.
    pub(crate) fn hold(&mut self, id: NodeId, on: bool) {
        if on {
            self.holds.insert(id);
        } else {
            self.holds.remove(&id);
        }
    }

    pub(crate) fn holds(&self, id: NodeId) -> bool {
        self.holds.contains(&id)
    }

    /// Anything `under` a surface masked and moving.
    pub(crate) fn busy(
        &self,
        at: std::time::Duration,
        mut under: impl FnMut(NodeId) -> bool,
    ) -> bool {
        self.nodes
            .iter()
            .any(|(id, m)| !m.is_settled(at) && under(*id))
            || self.holds.iter().any(|id| under(*id))
    }

    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.holds.remove(&id);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
        self.holds.retain(|id| keep(*id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(d: Drawn) -> f64 {
        match d {
            Drawn::Clip(p) => p.area().abs(),
            Drawn::Mosaic { .. } => f64::NAN,
        }
    }

    #[test]
    fn masks_grow_with_progress_and_complements_cover_the_rest() {
        let frame = kurbo::Rect::new(10.0, 10.0, 110.0, 60.0);
        let full = frame.area();
        for kind in [
            Kind::Wipe(Edge::Left),
            Kind::Wipe(Edge::Bottom),
            Kind::Disc,
            Kind::Dissolve,
        ] {
            let at = |p: f32, invert: bool| area(Masked { kind, p, invert }.drawn(frame, 1.0));
            assert!(at(0.0, false) < 1.0, "{kind:?} hidden at 0");
            assert!(at(0.3, false) < at(0.7, false), "{kind:?} grows");
            // Inverted: the rest of the box (the disc's polygon and the
            // dissolve's cells are a little off a true split).
            let sum = at(0.4, false) + at(0.4, true);
            assert!(
                (sum - full).abs() < full * 0.02,
                "{kind:?}: {sum} vs {full}"
            );
        }
        // The disc reaches the corners at 1.
        assert!(
            area(
                Masked {
                    kind: Kind::Disc,
                    p: 1.0,
                    invert: false
                }
                .drawn(frame, 1.0)
            ) > full
        );
        assert_eq!(
            Kind::of(&PropValue::Call {
                name: "wipe".into(),
                args: vec![PropValue::Keyword("top".into())]
            }),
            Some(Kind::Wipe(Edge::Top))
        );
        assert_eq!(
            Kind::of(&PropValue::Keyword("pixelate".into())),
            Some(Kind::Pixelate)
        );
    }

    /// A swap waiting for its decode holds the old image without asking
    /// for frames or keeping the surface busy; a decode that fails ends
    /// it; one that arrives wipes the new image in.
    #[test]
    fn an_image_swap_waits_without_frames_and_ends_on_a_failed_decode() {
        use std::time::Duration;
        let frame = |ms: u64| Frame {
            at: Duration::from_millis(ms),
            commit: true,
            prev: None,
            snap: false,
        };
        let curve = Curve::Timed {
            duration: Duration::from_millis(200),
            easing: strand_scene::Easing::Linear,
        };
        let id = NodeId::new(3, 0);
        let all = |_| true;
        for end in [Decode::Failed, Decode::Ready] {
            let mut swaps = ImageSwaps::default();
            swaps.swap(id, "a.png", Decode::Ready, curve, frame(1000));
            assert_eq!(
                swaps.swap(id, "b.png", Decode::Waiting, curve, frame(1016)),
                (Some(("a.png".to_string(), 0.0)), false),
                "waiting: the old image, no frames"
            );
            assert!(!swaps.busy(all));
            assert_eq!(
                swaps.swap(id, "b.png", Decode::Waiting, curve, frame(2000)),
                (Some(("a.png".to_string(), 0.0)), false)
            );
            let (swap, moving) = swaps.swap(id, "b.png", end, curve, frame(2016));
            if end == Decode::Failed {
                assert_eq!((swap, moving), (None, false), "failed: ended");
                assert!(!swaps.busy(all));
            } else {
                assert!(moving && swap.is_some(), "arrived: it wipes");
                assert!(swaps.busy(all));
            }
        }
    }
}

/// One image node's swap: the source it shows and, while it swaps, the
/// one it swaps from with the mask's progress (`None` until the new
/// source is decoded).
#[derive(Debug)]
struct ImageSwap {
    shown: String,
    from: Option<(String, Option<Motion<1>>)>,
}

/// Where an image's new source is in decoding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decode {
    /// Decoded (or a decode at another size stands in).
    Ready,
    /// On its way: the old image stays whole, and no frames are wanted
    /// meanwhile (the arrival repaints).
    Waiting,
    /// It failed (a missing or corrupt file): the swap ends at once.
    Failed,
}

/// `image`s with `transition:` swapping sources (design.md: transition
/// masks on "image swaps"): the new image comes in over the old through
/// the mask, starting once it is decoded (the old one stays whole until
/// then, without asking for frames); a source that fails to decode ends
/// the swap, drawing what an image without `transition:` would.
#[derive(Debug, Default)]
pub(crate) struct ImageSwaps {
    nodes: HashMap<NodeId, ImageSwap>,
}

impl ImageSwaps {
    /// Image `id` now showing `source` (`decode`: how far its decode is)
    /// in `frame`: the source it swaps from and the mask's progress, while
    /// it swaps; also whether it still moves.
    pub(crate) fn swap(
        &mut self,
        id: NodeId,
        source: &str,
        decode: Decode,
        curve: Curve,
        frame: Frame,
    ) -> (Option<(String, f32)>, bool) {
        let Some(s) = self.nodes.get_mut(&id) else {
            if frame.commit {
                self.nodes.insert(
                    id,
                    ImageSwap {
                        shown: source.to_string(),
                        from: None,
                    },
                );
            }
            return (None, false);
        };
        if s.shown != source {
            if !frame.commit {
                return (Some((s.shown.clone(), 0.0)), true);
            }
            let old = std::mem::replace(&mut s.shown, source.to_string());
            s.from = (!frame.snap).then_some((old, None));
        }
        let Some((old, motion)) = &mut s.from else {
            return (None, false);
        };
        if frame.snap || decode == Decode::Failed {
            if frame.commit {
                s.from = None;
            }
            return (None, false);
        }
        if motion.is_none() && decode == Decode::Ready {
            let mut m = Motion::rest([0.0], EPS).sampled_at(frame.prev);
            m.retarget([1.0], curve);
            *motion = Some(m);
        }
        let p = match motion {
            Some(m) if frame.commit => m.sample(frame.at)[0],
            Some(m) => m.peek(frame.at)[0],
            // Waiting for the decode: the old image, still.
            None => return (Some((old.clone(), 0.0)), false),
        };
        let settled = motion.as_ref().is_some_and(|m| m.is_settled(frame.at));
        if settled {
            if frame.commit {
                s.from = None;
            }
            return (None, false);
        }
        (Some((old.clone(), p.clamp(0.0, 1.0))), true)
    }

    /// Anything `under` a surface wiping in (not waiting for a decode).
    pub(crate) fn busy(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.nodes
            .iter()
            .any(|(id, s)| matches!(s.from, Some((_, Some(_)))) && under(*id))
    }

    /// Ends the swaps of images `gone` names (not drawn in the frame just
    /// painted): they show their new source at rest when they come back.
    pub(crate) fn end_undrawn(&mut self, mut gone: impl FnMut(NodeId) -> bool) {
        for (id, s) in self.nodes.iter_mut() {
            if s.from.is_some() && gone(*id) {
                s.from = None;
            }
        }
    }

    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
    }
}

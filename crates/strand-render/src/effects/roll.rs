//! (M4) Rolling numbers (design.md, "Motion and time": `text pct(level)
//! { roll: true }`, "Trivial").
//!
//! A text with `roll: true` remembers the layout it drew. When its glyphs
//! change, each letter (glyph, in visual order) that differs rolls: the
//! old one slides up out of the text's box as the new one slides up into
//! it from below, along a one-channel spring from 0 to 1 on the `roll`
//! prop's transition (`$motion.spatial` by default; `~ instant`,
//! `reduced_motion` and frames with no clock show the new text at once).
//! Letters that are the same stay put, so `41%` → `42%` rolls only the
//! `2`. When the count of letters changes (`9` → `10`) every letter
//! rolls. A change during a roll rolls on from the text on screen.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use strand_scene::{Color, Curve, Motion, NodeId};
use strand_text::{PlacedGlyph, TextLayout};

use crate::shapes::morph::Frame;

/// Settling tolerance of a roll's progress.
const EPS: f32 = 0.001;

#[derive(Debug)]
struct Roll {
    /// The layout it shows (or rolls to).
    shown: Arc<TextLayout>,
    /// The layout it rolls from, and the roll's progress.
    from: Option<(Arc<TextLayout>, Motion<1>)>,
}

/// A roll in flight: the layout it rolls from, the one it rolls to and
/// its progress.
pub(crate) type Rolling = (Arc<TextLayout>, Arc<TextLayout>, f32);

/// Every rolling text's last layout and roll.
#[derive(Debug, Default)]
pub(crate) struct Rolls {
    nodes: HashMap<NodeId, Roll>,
    /// Texts a preview saw change: the next painted frame starts the roll.
    pending: HashSet<NodeId>,
}

/// The glyphs of `l` in visual order.
fn glyphs(l: &TextLayout) -> Vec<PlacedGlyph> {
    l.runs
        .iter()
        .flat_map(|r| r.glyphs.iter().copied())
        .collect()
}

/// The same letter drawn the same way.
fn same(a: &PlacedGlyph, b: &PlacedGlyph) -> bool {
    (a.x, a.y, a.slot.page, a.slot.x, a.slot.y) == (b.x, b.y, b.slot.page, b.slot.x, b.slot.y)
}

/// True if `a` and `b` draw the same glyphs (a re-shape of the same
/// text for another width or key is no change).
fn same_text(a: &TextLayout, b: &TextLayout) -> bool {
    let (ga, gb) = (glyphs(a), glyphs(b));
    ga.len() == gb.len() && ga.iter().zip(&gb).all(|(x, y)| same(x, y))
}

impl Rolls {
    /// What text `id`, now laid out as `layout`, draws in `frame`: `None`
    /// to draw `layout` as it is, or the layout it rolls from, the one it
    /// rolls to and the progress. Also whether it is still moving.
    pub(crate) fn roll(
        &mut self,
        id: NodeId,
        layout: &Arc<TextLayout>,
        curve: Curve,
        frame: Frame,
    ) -> (Option<Rolling>, bool) {
        let Some(r) = self.nodes.get_mut(&id) else {
            if frame.commit {
                self.nodes.insert(
                    id,
                    Roll {
                        shown: layout.clone(),
                        from: None,
                    },
                );
            }
            return (None, false);
        };
        if !same_text(&r.shown, layout) {
            if !frame.commit {
                self.pending.insert(id);
                return (None, false);
            }
            self.pending.remove(&id);
            let old = std::mem::replace(&mut r.shown, layout.clone());
            if frame.snap || curve == Curve::Instant {
                r.from = None;
                return (None, false);
            }
            // A roll in flight rolls on from what is on screen: the
            // nearer of its two texts.
            let from = match r.from.take() {
                Some((before, m)) if m.peek(frame.at)[0] < 0.5 => before,
                _ => old,
            };
            let mut motion = Motion::rest([0.0], EPS).sampled_at(frame.prev);
            motion.retarget([1.0], curve);
            r.from = Some((from, motion));
        } else if !Arc::ptr_eq(&r.shown, layout) {
            r.shown = layout.clone();
        }
        let Some((from, motion)) = &mut r.from else {
            return (None, false);
        };
        if frame.snap {
            if frame.commit {
                r.from = None;
            }
            return (None, false);
        }
        let p = if frame.commit {
            motion.sample(frame.at)[0]
        } else {
            motion.peek(frame.at)[0]
        };
        if motion.is_settled(frame.at) {
            if frame.commit {
                r.from = None;
            }
            return (None, false);
        }
        (Some((from.clone(), layout.clone(), p)), true)
    }

    /// A preview saw a text under `under` change.
    pub(crate) fn pending(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.pending.iter().any(|id| under(*id))
    }

    /// Drops texts `keep` rejects.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
        self.pending.retain(|id| keep(*id));
    }

    /// Forgets `id` (another node now, or it stopped rolling).
    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.pending.remove(&id);
    }
}

/// The display items of a roll from `old` to `new` at progress `p`
/// (placed at `origin`, physical pixels, line height `h`): each changed
/// letter's old glyph moved up by `p · h` and new one by `(1 − p) · h`
/// below, unchanged letters in place, all clipped to `clip` by the
/// caller. Glyph items carry `color` and `spans`.
pub(crate) fn items(
    old: &TextLayout,
    new: &TextLayout,
    p: f32,
    origin: (i32, i32),
    h: f64,
    color: Color,
    spans: &[Color],
) -> Vec<(crate::flatten::Item, strand_scene::Rect)> {
    use crate::flatten::Item;
    let (go, gn) = (glyphs(old), glyphs(new));
    let all = go.len() != gn.len();
    let old_parts = super::letters::split(old);
    let new_parts = super::letters::split(new);
    let up = (p as f64 * h).round() as i32;
    let down = ((1.0 - p as f64) * h).round() as i32;
    let mut out = Vec::new();
    let mut push = |layout: Arc<TextLayout>, g: &PlacedGlyph, dy: i32| {
        let (x, y) = (origin.0, origin.1 + dy);
        let bounds = strand_scene::Rect::new(
            x + g.x - 1,
            y + g.y - 1,
            u32::from(g.slot.w) + 3,
            u32::from(g.slot.h) + 3,
        );
        out.push((
            Item::Glyphs {
                x,
                y,
                layout,
                color,
                spans: spans.to_vec(),
                fill: None,
            },
            bounds,
        ));
    };
    for (i, (part, g)) in new_parts.into_iter().zip(&gn).enumerate() {
        let changed = all || go.get(i).is_none_or(|o| !same(o, g));
        if changed {
            if let (Some(o), Some(op)) = (go.get(i), old_parts.get(i)) {
                push(op.clone(), o, -up);
            }
            push(part, g, down);
        } else {
            push(part, g, 0);
        }
    }
    // Old letters past the new text's end roll out.
    for (o, op) in go.iter().zip(&old_parts).skip(gn.len()) {
        push(op.clone(), o, -up);
    }
    out
}

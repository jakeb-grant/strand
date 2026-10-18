//! `ink`: what a text draws fits the box it is laid out in. It reads the
//! glyphs' ink each frame traces (the layout drawn, where it was drawn,
//! clipped where the frame clips it), so it sees what no box does: a
//! stand-in layout spilling past its box, or one cut far shorter than
//! its box holds (owner-testing.md bug 13, flatten/text.rs `place_text`:
//! "a layout of another width stands in: the widest shaped for no more
//! than the box (last frame's, while it grows), else the nearest wider
//! one clipped to the box; the unbounded one only where nothing else
//! was ever shaped, also clipped").
//!
//! - **spill**: a text's ink stays across its own box, give or take
//!   [`OVERHANG`] (a glyph's overhang, and the pixel a text may be wider
//!   than its box before it is cut).
//! - **over-cut**: a text cut with an ellipsis draws no less of itself
//!   as its box widens: from one frame to the next its line box grows
//!   and its ink does not narrow (an ellipsis cuts at a word or a glyph,
//!   so more room never shows less; a stand-in from a narrower box is
//!   the one the frame before drew, kept while it stands in). The frame
//!   before counts only where its ink stopped short of its box's edge:
//!   ink that reaches the edge may be a wider layout clipped there,
//!   which a wider box cannot draw again. A frame logic's diff or input
//!   reached is left out: its text may have changed.

use std::collections::HashMap;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;

/// How far ink may reach past its box, logical px.
pub const OVERHANG: f64 = 2.0;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let seg = |k: u32| {
        cx.trace
            .segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.s.is_none_or(|s| s < k) && k <= s.e)
    };
    // Per text and surface, the frame before: its line box and ink
    // width, and whether its ink stopped short of its box's edge.
    let mut before: HashMap<(&str, u32), (f64, f64, bool)> = HashMap::new();
    for f in &cx.trace.frames {
        let mut now: HashMap<(&str, u32), (f64, f64, bool)> = HashMap::new();
        for r in &f.n {
            let Some(ink) = r.ink else { continue };
            let key = r.key.as_str();
            let at = seg(f.k);
            if at.is_some_and(|(si, _)| {
                cx.exempt(
                    CheckId::Ink,
                    si,
                    cx.trace.nodes.get_key_value(key).map_or(key, |(k, _)| k),
                    strand_render::probe::Axes::W,
                )
            }) {
                continue;
            }
            let name = at.map(|(_, s)| s.name.as_str()).unwrap_or("");
            let (left, right) = (f64::from(ink[0]), f64::from(ink[0] + ink[2]));
            let (bl, br) = (f64::from(r.drawn[0]), f64::from(r.drawn[0] + r.drawn[2]));
            let give = OVERHANG + cx.tol;
            if left < bl - give || right > br + give {
                let past = (bl - left).max(right - br);
                out.push(
                    Finding::fail(
                        CheckId::Ink,
                        format!("spill: its glyphs reach {past:.1} px past its box"),
                    )
                    .at(f.k)
                    .node(key)
                    .axis("w")
                    .got(past, [0.0, give])
                    .in_segment(name),
                );
            }
            let width = right - left;
            let ellipsis = cx.trace.nodes.get(key).is_some_and(|m| m.ellipsis);
            if let (Some(fit), true) = (r.fit, ellipsis) {
                let fit = f64::from(fit);
                if let Some((was_fit, was_ink, short)) = before.get(&(key, r.surface))
                    && *short
                    && fit > was_fit + cx.tol
                    && width < was_ink - cx.tol
                    && !f.driven
                {
                    out.push(
                        Finding::fail(
                            CheckId::Ink,
                            format!(
                                "over-cut: its line box widened {was_fit:.0} -> {fit:.0} px and \
                                 its ellipsised text drew less of itself ({was_ink:.1} -> \
                                 {width:.1} px)"
                            ),
                        )
                        .at(f.k)
                        .node(key)
                        .axis("w")
                        .got(width, [was_ink - cx.tol, fit + give])
                        .in_segment(name),
                    );
                }
                now.insert((key, r.surface), (fit, width, right < br - cx.tol));
            }
        }
        before.retain(|(_, s), _| !f.painted.contains(s));
        before.extend(now);
    }
    first_per_node(out)
}

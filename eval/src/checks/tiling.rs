//! (eval-rules) `tiling`: two edges at one place in layout are drawn on
//! one physical pixel.
//!
//! decisions.md wave3-pixels ("boxes snap from absolute positions"): a
//! box's left edge is `round(x)` and its width `round(x + w) - round(x)`,
//! so "a box's edges depend only on where they fall". Boxes that touch
//! in layout (a row's children, a child flush with its parent's edge)
//! therefore touch on screen at every scale: no seam of background
//! between them, no pixel drawn twice.
//!
//! On every painted frame, among the nodes shown on one surface that are
//! drawn where they are laid (no motion of their own or inherited, no
//! offset, scale or turn, and (eval-rules-5) no ancestor scaled or
//! turned, at rest or not), every two edges on one axis that lie at the same
//! logical place in layout are drawn at the same place, within a
//! quantum.

use std::collections::BTreeMap;

use strand_render::probe::Motions;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::Row;

/// Laid edges this close are one place (the trace's quantum, 1/64, halved).
const SAME: f32 = 1.0 / 128.0;
/// Edges drawn further apart than this are on different physical pixels
/// (two quanta; a physical pixel is at least 1/4 logical px at scale 4).
/// A row's `visual` box is the one drawn, snapped to physical pixels and
/// written in logical px.
const APART: f32 = 1.0 / 32.0;

/// An edge: where it is laid, where it is drawn, its node and which
/// edge of it.
type Edge<'a> = (f32, f32, &'a str, &'static str);

/// Drawn where it is laid: nothing moves it or anything it is drawn
/// with (no motion of its own or inherited: an entering row's layer is
/// placed by its spring's whole pixels), and nothing offsets, scales or
/// turns it. A scroll's offset in flight (`SCROLL`, a fling's `FLING`)
/// is not such motion: it lays its content out at the offset each frame,
/// laid where drawn.
fn in_place(r: &Row) -> bool {
    let zero = |p: [f32; 2]| p == [0.0, 0.0];
    let laid_out = !(Motions::SCROLL.0 | Motions::FLING.0);
    r.mot & laid_out == 0
        && r.emot & laid_out == 0
        && zero(r.inherited)
        && zero(r.written)
        && zero(r.spring)
        && zero(r.glide)
        && zero(r.morph)
        && zero(r.flight)
        && r.scale == [1.0, 1.0]
        && r.rotate == 0.0
}

/// (eval-rules-5) No ancestor of `key` is scaled or turned at frame `i`,
/// moving or at rest: a row's own `scale` and `rotate` are its own, and
/// a box held pressed at `scale: 0.88` draws everything in it scaled
/// about its centre, off the places layout gave them, by design.
fn untransformed_above<'a>(cx: &Cx<'a>, i: usize, key: &'a str) -> bool {
    cx.lineage(key).iter().skip(1).all(|k| {
        cx.tl
            .row(i, k)
            .is_none_or(|a| a.scale == [1.0, 1.0] && a.rotate == 0.0)
    })
}

pub fn run(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let mut out = Vec::new();
    let mut under_transform = 0u64;
    for (i, f) in tl.frames.iter().enumerate() {
        if i > 0 && std::ptr::eq(*f, tl.frames[i - 1]) {
            continue;
        }
        // Per surface and axis: (laid edge, drawn edge, key, which edge).
        let mut edges: BTreeMap<(u32, usize), Vec<Edge>> = BTreeMap::new();
        for r in &f.n {
            if in_place(r) && !untransformed_above(cx, i, &r.key) {
                under_transform += 1;
            }
            if !in_place(r)
                || !untransformed_above(cx, i, &r.key)
                || !tl.visible(i, r)
                || r.visual[2] < 1.0
                || r.visual[3] < 1.0
            {
                continue;
            }
            for (axis, (lo, hi)) in [(0usize, ("left", "right")), (1, ("top", "bottom"))] {
                let e = edges.entry((r.surface, axis)).or_default();
                let (l, d) = (r.laid[axis], r.visual[axis]);
                let (lw, dw) = (r.laid[axis + 2], r.visual[axis + 2]);
                e.push((l, d, r.key.as_str(), lo));
                e.push((l + lw, d + dw, r.key.as_str(), hi));
            }
        }
        for ((_, axis), mut list) in edges {
            list.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut start = 0;
            while start < list.len() {
                let mut end = start + 1;
                while end < list.len() && list[end].0 - list[start].0 <= SAME {
                    end += 1;
                }
                let group = &list[start..end];
                let (lo, hi) = group
                    .iter()
                    .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), e| {
                        (a.min(e.1), b.max(e.1))
                    });
                if hi - lo > APART {
                    let a = group.iter().find(|e| e.1 == lo).unwrap_or(&group[0]);
                    let b = group.iter().find(|e| e.1 == hi).unwrap_or(&group[0]);
                    let name = |k: &str| {
                        cx.trace
                            .nodes
                            .get(k)
                            .map_or(k.to_string(), |m| m.label.clone())
                    };
                    out.push(
                        Finding::fail(
                            CheckId::Tiling,
                            format!(
                                "apart: laid at {:.2} on {}, its {} edge is drawn at {:.3} and {}'s \
                                 {} edge at {:.3}: {} px apart on screen",
                                group[0].0,
                                if axis == 0 { "x" } else { "y" },
                                a.3,
                                lo,
                                name(b.2),
                                b.3,
                                hi,
                                (f64::from(hi - lo) * cx.trace.scale_at(f)).round()
                            ),
                        )
                        .at(f.k)
                        .node(a.2)
                        .axis(if axis == 0 { "x" } else { "y" })
                        .got(f64::from(hi - lo), [0.0, f64::from(APART)]),
                    );
                }
                start = end;
            }
        }
    }
    super::stats("tiling.ancestor-transform", cx, &[("rows", under_transform)]);
    first_per_node(out)
}

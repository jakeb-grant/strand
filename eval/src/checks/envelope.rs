//! `envelope`: derived motion (a glide, layout, text arriving) takes a
//! node from where it was to where it ends, give or take an overshoot:
//! on an axis its laid box changes over a segment and nothing authored
//! moves, its visual value stays within `[min(s,e) - ov, max(s,e) + ov]`,
//! `ov = max(3 px, overshoot * |e - s|)` (3 px: morph.rs's bug-24 test).
//! Keyframes, lean, jelly and time-driven motion are out of it.

use strand_render::probe::Motions;

use super::{Cx, Finding, GEOMETRY, authored, first_per_node, laid, motions, value};
use crate::scenario::CheckId;

/// The overshoot every glide may have, logical px.
pub const MIN_OVERSHOOT: f64 = 3.0;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let free = Motions::KEYFRAMES | Motions::LEAN | Motions::JELLY | Motions::TIME;
    let mut out = Vec::new();
    for (si, seg, s) in cx.segments() {
        let (Some(bi), Some(ei)) = (cx.tl.at(s), cx.tl.at(seg.e)) else {
            continue;
        };
        for key in cx.visible_keys(bi) {
            if !cx.tl.visible_key(ei, key) {
                continue;
            }
            let (Some(b), Some(e)) = (cx.tl.row(bi, key), cx.tl.row(ei, key)) else {
                continue;
            };
            for a in GEOMETRY {
                if (laid(b, a) - laid(e, a)).abs() <= cx.tol {
                    continue;
                }
                let (vs, ve) = (value(b, a), value(e, a));
                let ov = MIN_OVERSHOOT.max(cx.sc.tolerance.overshoot * (ve - vs).abs());
                let (lo, hi) = (vs.min(ve) - ov, vs.max(ve) + ov);
                for i in cx.tl.within(Some(s), seg.e) {
                    let Some(r) = cx.tl.row(i, key) else { continue };
                    if !cx.tl.visible(i, r)
                        || r.hidden.as_deref() == Some("inert")
                        || authored(r, a)
                        || motions(r).intersects(free)
                        || cx.exempt(CheckId::Envelope, si, key, a.bits())
                    {
                        continue;
                    }
                    let v = value(r, a);
                    if v < lo || v > hi {
                        out.push(
                            Finding::fail(
                                CheckId::Envelope,
                                format!(
                                    "derived motion took {} outside its way from {vs:.1} to {ve:.1}",
                                    a.name()
                                ),
                            )
                            .at(cx.tl.ks[i])
                            .node(key)
                            .axis(a.name())
                            .got(v, [lo, hi])
                            .in_segment(&seg.name),
                        );
                    }
                }
            }
        }
    }
    first_per_node(out)
}

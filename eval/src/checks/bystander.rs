//! `bystander`: a node drawn at the same place on an axis at a segment's
//! start and at its rest, laid at the same place at both and elsewhere
//! between for no reason ([`super::way`]), is not drawn elsewhere on that
//! axis while it runs, unless authored motion moves it there; its opacity
//! and scale, when they end where they started, do not stray unless
//! authored. Bugs 24 and 25: the bars' laid `y` never changed, yet they
//! fell out of the island and came back. (eval-rules-4) A frame under
//! the node's own authored `scale` or `rotate` is judged by its box
//! before transforms ([`super::transformed`]): the turn grows its
//! visual box, and only that change is the transform's.

use super::{
    Cx, Finding, GEOMETRY, authored, drawn, first_per_node, laid, transformed, value, way,
};
use crate::scenario::CheckId;
use crate::steps::Axis;

/// How far opacity and scale may stray.
pub const OPACITY_GIVE: f64 = 0.01;
pub const SCALE_GIVE: f64 = 0.005;

pub fn run(cx: &Cx) -> Vec<Finding> {
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
            let mut axes: Vec<(Axis, f64)> = GEOMETRY
                .into_iter()
                .filter(|a| way(cx, key, s, seg.e, b, e, *a).bystander)
                .map(|a| (a, cx.tol))
                .collect();
            if (value(b, Axis::Opacity) - value(e, Axis::Opacity)).abs() <= OPACITY_GIVE {
                axes.push((Axis::Opacity, OPACITY_GIVE));
            }
            if (value(b, Axis::Scale) - value(e, Axis::Scale)).abs() <= SCALE_GIVE {
                axes.push((Axis::Scale, SCALE_GIVE));
            }
            for i in cx.tl.within(Some(s), seg.e) {
                let Some(r) = cx.tl.row(i, key) else { continue };
                if !cx.tl.visible(i, r) || r.hidden.as_deref() == Some("inert") {
                    continue;
                }
                let k = cx.tl.ks[i];
                for (a, give) in &axes {
                    if authored(r, *a) || cx.exempt(CheckId::Bystander, si, key, a.bits()) {
                        continue;
                    }
                    let val = if transformed(r) { drawn } else { value };
                    let (v0, v) = (val(b, *a), val(r, *a));
                    if (v - v0).abs() > *give {
                        let what = if matches!(a, Axis::Opacity | Axis::Scale) {
                            format!("its {} strayed while nothing authored moves it", a.name())
                        } else if (laid(r, *a) - laid(b, *a)).abs() > cx.tol {
                            format!(
                                "drawn off its level on {}: laid out elsewhere and back with \
                                 nothing to move it (no change from logic or input, no exit, \
                                 size spring, keyframes or late text)",
                                a.name()
                            )
                        } else {
                            format!(
                                "drawn off its level on {}: its laid {} is unchanged and nothing \
                                 authored moves it",
                                a.name(),
                                a.name()
                            )
                        };
                        out.push(
                            Finding::fail(CheckId::Bystander, what)
                                .at(k)
                                .node(key)
                                .axis(a.name())
                                .got(v, [v0 - give, v0 + give])
                                .in_segment(&seg.name),
                        );
                    }
                }
            }
        }
    }
    first_per_node(out)
}

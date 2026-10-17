//! `twin`: the scenario re-run with motion reduced from the start
//! (everything snaps) ends each settled segment as the run does: the
//! same nodes shown (by key), their visual boxes within `tol` and their
//! opacity within 1/255. A stuck offset, or a layout left on an estimate
//! at rest, shows as a difference. Keyframed and time-driven nodes are
//! left out (reduced motion drops loops).

use std::collections::BTreeSet;

use strand_render::probe::Motions;

use super::{Cx, Finding, Timeline, motions};
use crate::scenario::CheckId;
use crate::trace::RunTrace;

pub const OPACITY_GIVE: f64 = 1.0 / 255.0;

pub fn run(cx: &Cx, twin: &RunTrace) -> Vec<Finding> {
    let tw = Timeline::new(twin);
    let mut out = Vec::new();
    if twin.segments.len() != cx.trace.segments.len() {
        out.push(Finding::fail(
            CheckId::Twin,
            format!(
                "the reduced-motion run has {} segments, the run {}",
                twin.segments.len(),
                cx.trace.segments.len()
            ),
        ));
        return out;
    }
    let free = Motions::KEYFRAMES | Motions::TIME;
    for (a, b) in cx.trace.segments.iter().zip(&twin.segments) {
        if !a.settled || !b.settled {
            continue;
        }
        let (Some(ia), Some(ib)) = (cx.tl.at(a.e), tw.at(b.e)) else {
            continue;
        };
        let shown = |tl: &Timeline, i: usize| -> BTreeSet<String> {
            tl.screens[i]
                .iter()
                .filter(|(_, r)| tl.visible(i, r) && !motions(r).intersects(free))
                .map(|(k, _)| k.to_string())
                .collect()
        };
        let (sa, sb) = (shown(&cx.tl, ia), shown(&tw, ib));
        for k in sa.difference(&sb) {
            out.push(
                Finding::fail(CheckId::Twin, "shown at rest, but not with motion reduced")
                    .at(a.e)
                    .node(k)
                    .in_segment(&a.name),
            );
        }
        for k in sb.difference(&sa) {
            out.push(
                Finding::fail(
                    CheckId::Twin,
                    "not shown at rest, but shown with motion reduced",
                )
                .at(a.e)
                .node(k)
                .in_segment(&a.name),
            );
        }
        for k in sa.intersection(&sb) {
            let (Some(ra), Some(rb)) = (cx.tl.row(ia, k), tw.row(ib, k)) else {
                continue;
            };
            for (i, axis) in ["x", "y", "w", "h"].iter().enumerate() {
                let (va, vb) = (f64::from(ra.visual[i]), f64::from(rb.visual[i]));
                if (va - vb).abs() > cx.tol {
                    out.push(
                        Finding::fail(
                            CheckId::Twin,
                            "at rest it is not where it is with motion reduced",
                        )
                        .at(a.e)
                        .node(k)
                        .axis(axis)
                        .got(va, [vb - cx.tol, vb + cx.tol])
                        .in_segment(&a.name),
                    );
                }
            }
            let (oa, ob) = (f64::from(ra.op), f64::from(rb.op));
            if (oa - ob).abs() > OPACITY_GIVE {
                out.push(
                    Finding::fail(
                        CheckId::Twin,
                        "at rest its opacity is not the reduced run's",
                    )
                    .at(a.e)
                    .node(k)
                    .axis("opacity")
                    .got(oa, [ob - OPACITY_GIVE, ob + OPACITY_GIVE])
                    .in_segment(&a.name),
                );
            }
        }
    }
    out
}

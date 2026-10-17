//! `teleport`: a moving node does not jump. Between two frames in which
//! it shows, with any motion on it or an ancestor at either, each edge
//! moves at most `max(teleport_px * 60 / hz, 0.75 * |e - s|) + tol` per
//! tick between them. A glide retargeted in flight starts from where the
//! node was drawn: its drawn box does not jump with its layout. A jump
//! with no motion at either frame (layout snapping) is noted, not failed.
//! Skipped under reduced motion, where everything snaps.

use super::{Cx, Finding, GEOMETRY, Sev, first_per_node, laid, value};
use crate::scenario::CheckId;
use strand_render::probe::Motions;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (si, seg, s) in cx.segments() {
        if seg.reduced_motion {
            continue;
        }
        let per_tick = cx.sc.tolerance.teleport_px * 60.0 / f64::from(seg.hz.max(1));
        let (bi, ei) = (cx.tl.at(s), cx.tl.at(seg.e));
        let range = cx.tl.within(Some(s), seg.e);
        let start = bi.unwrap_or(range.start);
        for i in range {
            if i <= start {
                continue;
            }
            let p = i - 1;
            let ticks = f64::from(cx.tl.ks[i] - cx.tl.ks[p]).max(1.0);
            for (key, r) in &cx.tl.screens[i] {
                let Some(q) = cx.tl.row(p, key) else { continue };
                if !cx.tl.visible(i, r) || !cx.tl.visible(p, q) || q.surface != r.surface {
                    continue;
                }
                let moving = r.emot != 0 || q.emot != 0;
                let glides = Motions(r.mot).contains(Motions::GLIDE)
                    && Motions(q.mot).contains(Motions::GLIDE);
                for a in GEOMETRY {
                    if cx.exempt(CheckId::Teleport, si, key, a.bits()) {
                        continue;
                    }
                    let travel = match (
                        bi.and_then(|b| cx.tl.row(b, key)),
                        ei.and_then(|e| cx.tl.row(e, key)),
                    ) {
                        (Some(b), Some(e)) => (value(e, a) - value(b, a)).abs(),
                        _ => 0.0,
                    };
                    let bound = (per_tick * ticks).max(0.75 * travel) + cx.tol;
                    let d = value(r, a) - value(q, a);
                    let k = cx.tl.ks[i];
                    if d.abs() > bound {
                        let f = Finding::fail(
                            CheckId::Teleport,
                            if moving {
                                format!("jumped {:.1} px on {} in one frame", d.abs(), a.name())
                            } else {
                                format!(
                                    "jump: layout moved it {:.1} px on {} with no motion",
                                    d.abs(),
                                    a.name()
                                )
                            },
                        )
                        .at(k)
                        .node(key)
                        .axis(a.name())
                        .got(d.abs(), [0.0, bound])
                        .in_segment(&seg.name);
                        out.push(if moving { f } else { f.sev(Sev::Info) });
                        continue;
                    }
                    let dl = laid(r, a) - laid(q, a);
                    if glides && dl.abs() > 1.0f64.max(cx.tol) && (d - dl).abs() <= cx.tol {
                        out.push(
                            Finding::fail(
                                CheckId::Teleport,
                                format!(
                                    "retarget: its glide jumped {:.1} px on {} with its layout \
                                     instead of starting where it was drawn",
                                    dl.abs(),
                                    a.name()
                                ),
                            )
                            .at(k)
                            .node(key)
                            .axis(a.name())
                            .got(d.abs(), [0.0, cx.tol])
                            .in_segment(&seg.name),
                        );
                    }
                }
            }
        }
    }
    first_per_node(out)
}

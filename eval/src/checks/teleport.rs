//! `teleport`: a moving node does not jump. Between two frames in which
//! it shows, with any motion on it or an ancestor at either, each edge
//! moves at most `max(teleport_px * 60 / hz, 0.75 * d) + tol` per tick
//! between them. `d` is the segment's travel `|e - s|` or, for a node
//! gliding (or carried by a gliding ancestor) at the second frame and
//! gliding or lifted by a drag at the first, the glide (or drag) offset,
//! its own and inherited, it had left at the first, if larger: a
//! drag released far from home has no net travel, but its glide back
//! closes the whole distance, and a jump of all that is left in one tick
//! still exceeds 0.75 of it. A glide retargeted in flight starts from
//! where the node was drawn: its drawn box does not jump with its layout.
//! A jump with no motion at either frame (layout snapping) is noted, not
//! failed; on w and h only motion that sizes a box counts (a size spring
//! or a morph, its own or an ancestor's, lean, jelly, or authored motion
//! on w, h, scale or rotate: keyframes or `t` on those axes, a scale
//! spring): a glide, a drag, an offset or opacity spring, a reveal, a
//! stagger and a pose of opacity or offset move or fade a box, never size
//! it. Skipped under reduced motion, where everything snaps.

use super::{Cx, Finding, GEOMETRY, Sev, first_per_node, laid, value};
use crate::scenario::CheckId;
use strand_render::probe::{Axes, Motions};

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
                let any = Motions(r.emot | q.emot);
                let glides = Motions(r.mot).contains(Motions::GLIDE)
                    && Motions(q.mot).contains(Motions::GLIDE);
                // Gliding itself or carried by a gliding ancestor, from a
                // glide or from a drag's lift (the release: what the drag
                // left is what the glide back closes).
                let carried = Motions(r.emot).contains(Motions::GLIDE)
                    && Motions(q.emot).intersects(Motions::GLIDE | Motions::LIFT);
                for a in GEOMETRY {
                    if cx.exempt(CheckId::Teleport, si, key, a.bits()) {
                        continue;
                    }
                    // On w and h only motion that sizes a box counts: a
                    // size spring or a morph (its own or an ancestor's),
                    // lean or jelly, or authored motion on w, h, scale or
                    // rotate (keyframes, `t`, a scale spring). A glide, a
                    // drag, an offset or opacity spring, a reveal, a
                    // stagger and a pose of opacity or offset move or fade
                    // a box, never size it: a size that layout snaps under
                    // one is layout's, as with no motion (a late text's
                    // estimate corrected while its row glides or shakes).
                    let moving = match a {
                        super::Axis::W | super::Axis::H => {
                            any.intersects(
                                Motions::SIZE_SPRING
                                    | Motions::SHARED_MORPH
                                    | Motions::LEAN
                                    | Motions::JELLY,
                            ) || Axes(r.auth | q.auth)
                                .intersects(a.bits() | Axes::SCALE | Axes::ROTATE)
                        }
                        _ => !any.is_empty(),
                    };
                    let travel = match (
                        bi.and_then(|b| cx.tl.row(b, key)),
                        ei.and_then(|e| cx.tl.row(e, key)),
                    ) {
                        (Some(b), Some(e)) => (value(e, a) - value(b, a)).abs(),
                        _ => 0.0,
                    };
                    let left = match (carried, a) {
                        (true, super::Axis::X) => f64::from(q.glide[0] + q.inherited[0]).abs(),
                        (true, super::Axis::Y) => f64::from(q.glide[1] + q.inherited[1]).abs(),
                        _ => 0.0,
                    };
                    let bound = (per_tick * ticks).max(0.75 * travel.max(left)) + cx.tol;
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

//! `teleport`: a moving node does not jump. Between two frames in which
//! it shows, with any motion on it or an ancestor at either, each edge
//! moves at most `max(teleport_px * 60 / hz, 0.75 * d) + tol` per tick
//! between them. `d` is the segment's travel (for a node that enters or
//! leaves in the segment, from where it first shows or to where it last
//! shows), the span of its way
//! ([`super::way`]: where it starts, where it rests and every place a
//! frame counted laid it out, so a size spring sent out and back within
//! one segment travels its whole excursion, not the nothing between its
//! ends) or, for a node
//! gliding (or carried by a gliding ancestor) at the second frame and
//! gliding or lifted by a drag at the first, the glide (or drag) offset
//! it had left at the first, its own and its ancestors' in flight (what
//! they have written at rest is no way left to go), if larger: a
//! drag released far from home has no net travel, but its glide back
//! closes the whole distance, and a jump of all that is left in one tick
//! still exceeds 0.75 of it. A glide retargeted in flight starts from
//! where the node was drawn: its drawn box does not jump with its layout.
//! A jump with no motion at either frame (layout snapping) is noted, not
//! failed; the retarget arm holds x and y only, since a glide moves a
//! box and never sizes it (layout lengths snap, design.md "What can't
//! interpolate"); on w and h only motion that sizes a box counts (a size spring
//! or a morph, its own or an ancestor's, lean, jelly, or authored motion
//! on w, h, scale or rotate: keyframes or `t` on those axes, a scale
//! spring): a glide, a drag, an offset or opacity spring, a reveal, a
//! stagger and a pose of opacity or offset move or fade a box, never size
//! it. Skipped under reduced motion, where everything snaps.

use std::collections::HashMap;

use super::{Cx, Finding, GEOMETRY, Sev, first_per_node, laid, value, way};
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
        // Per node and axis, the segment's travel: the span of its way.
        let mut travels: HashMap<(&str, usize), f64> = HashMap::new();
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
                    let travel = *travels.entry((*key, a as usize)).or_insert_with(|| {
                        // (eval-coverage) A node that enters in the
                        // segment travels from where it first shows, and
                        // one that leaves to where it last shows: an
                        // enter pose's slide is held as the same slide of
                        // a node already there is.
                        let mut shown = cx
                            .tl
                            .within(Some(s), seg.e)
                            .filter_map(|j| cx.tl.row(j, key));
                        let first = shown.next();
                        let last = shown.next_back().or(first);
                        match (
                            bi.and_then(|b| cx.tl.row(b, key)).or(first),
                            ei.and_then(|e| cx.tl.row(e, key)).or(last),
                        ) {
                            (Some(b), Some(e)) => {
                                let w = way(cx, key, s, seg.e, b, e, a);
                                (value(e, a) - value(b, a)).abs().max(w.hi - w.lo)
                            }
                            _ => 0.0,
                        }
                    });
                    let left = match (carried, a) {
                        (true, super::Axis::X) => f64::from(q.glide[0] + q.flight[0]).abs(),
                        (true, super::Axis::Y) => f64::from(q.glide[1] + q.flight[1]).abs(),
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
                    let offset_axis = matches!(a, super::Axis::X | super::Axis::Y);
                    // (eval-coverage) A glide on the other axis only, while
                    // something on the surface lays out anew each frame
                    // (a sibling's size spring): the node follows its
                    // layout there, as a size spring's neighbours do, and
                    // no glide was retargeted on this axis.
                    let n = if a == super::Axis::X { 0 } else { 1 };
                    let follows = offset_axis
                        && q.glide[n] == 0.0
                        && r.glide[n] == 0.0
                        && cx.lays_out(i, r.surface);
                    if glides
                        && offset_axis
                        && !follows
                        && dl.abs() > 1.0f64.max(cx.tol)
                        && (d - dl).abs() <= cx.tol
                    {
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

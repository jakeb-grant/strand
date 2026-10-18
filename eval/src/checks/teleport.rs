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

use super::{Cx, Finding, GEOMETRY, Sev, authored, first_per_node, laid, value, way};
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
        let mut travels: HashMap<(&str, usize, usize), f64> = HashMap::new();
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
                // A scroll's offset in flight (a spring, or a fling, as
                // fast as the fingers were) lays its content out at the
                // offset each frame (laid where drawn): a jump it makes
                // is layout's, noted as with no motion (eval-coverage-2).
                let any = Motions((r.emot | q.emot) & !(Motions::SCROLL.0 | Motions::FLING.0));
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
                    // (eval-coverage-2) The stretch of frames the node
                    // shows in without a break that holds frame `i`: one
                    // that leaves and comes back within the segment (a
                    // branch shown again, morphing from where another was
                    // drawn) travels from where it comes back.
                    let (lo_i, hi_i) = stretch(cx, key, r.surface, i, start, seg.e);
                    let travel = *travels.entry((*key, a as usize, lo_i)).or_insert_with(|| {
                        // Where it starts and ends: at `s` and the rest, or
                        // for a node that came onto its surface in the
                        // segment (a popup's content, a branch, an enter
                        // pose's slide, held as the same slide of a node
                        // already there is) or left it (an exit's ghost),
                        // its first or last frame shown there
                        // (eval-coverage, eval-local), and for one that
                        // left and came back, the first frame of the
                        // stretch it shows in since (eval-coverage-2).
                        let at = |j: usize| cx.tl.row(j, key);
                        let from = if lo_i == start + 1 {
                            bi.and_then(at).or_else(|| at(lo_i))
                        } else {
                            at(lo_i)
                        };
                        let shown = |j: usize| {
                            at(j).filter(|q| q.surface == r.surface && cx.tl.visible(j, q))
                        };
                        let to = ei
                            .and_then(at)
                            .or_else(|| cx.tl.within(Some(s), seg.e).rev().find_map(shown));
                        let (Some(b), Some(e)) = (from, to) else {
                            return 0.0;
                        };
                        let w = way(cx, key, s, seg.e, b, e, a);
                        // (eval-coverage-2) And the way authored motion
                        // takes it in the stretch: an offset or a pose
                        // springs out and back (a page sliding in and,
                        // switched away mid-flight, out again) travels
                        // its excursion, not the little between its ends.
                        let mut ex: Option<(f64, f64)> = None;
                        for j in lo_i..=hi_i {
                            if let Some(x) = at(j).filter(|x| authored(x, a)) {
                                let v = value(x, a);
                                ex = Some(ex.map_or((v, v), |(l, h)| (l.min(v), h.max(v))));
                            }
                        }
                        let ex = ex.map_or(0.0, |(l, h)| h - l);
                        (value(e, a) - value(b, a)).abs().max(w.hi - w.lo).max(ex)
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
                    // (eval-coverage-2) A box whose own size snapped this
                    // tick and that its parent keeps by its centre or an
                    // edge (a centred card whose content shrank, placed
                    // in its held surface): its place moved with its
                    // size, which a glide never carries (it moves a box,
                    // never sizes it), so its glide was not retargeted.
                    let resized = offset_axis && {
                        let (o, sz) = (n, n + 2);
                        let (l0, l1) = (f64::from(q.laid[o]), f64::from(r.laid[o]));
                        let (s0, s1) = (f64::from(q.laid[sz]), f64::from(r.laid[sz]));
                        (s1 - s0).abs() > cx.tol
                            && ((l1 + s1 / 2.0 - l0 - s0 / 2.0).abs() <= cx.tol
                                || (l1 + s1 - l0 - s0).abs() <= cx.tol)
                    };
                    if glides
                        && offset_axis
                        && !follows
                        && !resized
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

/// (eval-coverage-2) The frames around frame `i` that `key` shows in on
/// `surface` without a break, within the segment's frames after `start`
/// (the frame at its `s`) up to its rest `e`: the first and the last.
fn stretch(cx: &Cx, key: &str, surface: u32, i: usize, start: usize, e: u32) -> (usize, usize) {
    let shown = |j: usize| {
        cx.tl
            .row(j, key)
            .is_some_and(|q| q.surface == surface && cx.tl.visible(j, q))
    };
    let last = cx.tl.ks.partition_point(|x| *x <= e).saturating_sub(1);
    let mut lo = i;
    while lo > start + 1 && shown(lo - 1) {
        lo -= 1;
    }
    let mut hi = i;
    while hi < last && shown(hi + 1) {
        hi += 1;
    }
    (lo, hi)
}

//! `teleport`: a moving node does not jump. Between two frames in which
//! it shows, with any motion on it or an ancestor at either, each edge
//! moves at most `max(teleport_px * 60 / hz, 0.75 * d) + tol` per tick
//! between them. `d` is the segment's travel (for a node that enters or
//! leaves in the segment, from where it first shows or to where it last
//! shows, and (eval-false-alarms) every place it comes back onto its
//! surface or leaves it again, and where each later change of the
//! segment found it: an offset sent out and turned back travels its
//! excursion; and (integration-r4) for a node under a scroll that moved
//! by itself, on y, how far the scroll's offset carried its content,
//! though a windowed list shows the node only inside its viewport), the
//! span of its way
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
//! failed (a row a scroll moves by itself moves: `SCROLL` and `FLING` are
//! motion, held to the bound), and on x and y a fade or a colour spring (opacity, fill, ink,
//! border, a meter's level: paint inside a box) is no motion
//! (eval-false-alarms); the retarget arm holds x and y only, on the
//! node's own move in its parent (what its parent's layout carried it by
//! is the parent's), since a glide moves a box and never sizes it
//! (layout lengths snap, design.md "What can't interpolate"); on w and h
//! only motion that sizes a box counts (a size spring or a morph, its own or an ancestor's, lean, jelly, or authored motion
//! on w, h, scale or rotate: keyframes or `t` on those axes, a scale
//! spring): a glide, a drag, an offset or opacity spring, a reveal, a
//! stagger and a pose of opacity or offset move or fade a box, never size
//! it. Skipped under reduced motion, where everything snaps.

use std::collections::HashMap;

use super::{Cx, Finding, GEOMETRY, Sev, first_per_node, laid, value, way};
use crate::scenario::CheckId;
use crate::trace::Row;
use strand_render::probe::{Axes, Motions};

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (si, seg, s) in cx.segments() {
        if seg.reduced_motion {
            continue;
        }
        let per_tick = cx.sc.tolerance.teleport_px * 60.0 / f64::from(seg.hz.max(1));
        // The frames showing just before each later change of the
        // segment: where that change found its nodes.
        let restarts: Vec<usize> = cx
            .restarts(si)
            .into_iter()
            .filter_map(|k| cx.tl.at(k.saturating_sub(1)))
            .collect();
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
                        // (eval-false-alarms) On x and y only motion that
                        // moves a box counts: a fade and a colour spring
                        // (its fill, ink or border, a meter's level) paint
                        // inside a box and never move it, so a place that
                        // layout snaps under one is layout's, as with no
                        // motion; a prop spring counts when what it
                        // springs places the box (x, y, scale, rotate).
                        _ => {
                            let paint = Motions::OPACITY_SPRING.0
                                | Motions::FILL_SPRING.0
                                | Motions::INK_SPRING.0
                                | Motions::LINE_SPRING.0
                                | Motions::PROP_SPRING.0;
                            any.0 & !paint != 0
                                || (any.contains(Motions::PROP_SPRING)
                                    && Axes(r.auth | q.auth)
                                        .intersects(a.bits() | Axes::SCALE | Axes::ROTATE))
                        }
                    };
                    let travel = *travels.entry((*key, a as usize)).or_insert_with(|| {
                        // Where it starts and ends: at `s` and the rest, or
                        // for a node that came onto its surface in the
                        // segment (a popup's content, a branch, an enter
                        // pose's slide, held as the same slide of a node
                        // already there is) or left it (an exit's ghost),
                        // its first or last frame shown there
                        // (eval-coverage, eval-local).
                        let shown = |j: usize| {
                            cx.tl
                                .row(j, key)
                                .filter(|q| q.surface == r.surface && cx.tl.visible(j, q))
                        };
                        let span = cx.tl.within(Some(s), seg.e);
                        let from = bi
                            .and_then(|b| cx.tl.row(b, key))
                            .or_else(|| span.clone().find_map(shown));
                        let to = ei
                            .and_then(|e| cx.tl.row(e, key))
                            .or_else(|| span.rev().find_map(shown));
                        let (Some(b), Some(e)) = (from, to) else {
                            return 0.0;
                        };
                        let w = way(cx, key, s, seg.e, b, e, a);
                        let (mut lo, mut hi) = (w.lo, w.hi);
                        // (eval-false-alarms) Its way starts again where a
                        // later change found it, and each time it comes
                        // back onto its surface (a morph reversed home,
                        // a branch shown again) or leaves it: an offset
                        // or pose sent out and turned back by a second
                        // change travels its whole excursion, as a size
                        // spring's laid places make it travel its own.
                        let mut put = |r: &Row| {
                            lo = lo.min(value(r, a));
                            hi = hi.max(value(r, a));
                        };
                        for j in &restarts {
                            if let Some(r) = shown(*j) {
                                put(r);
                            }
                        }
                        let span = cx.tl.within(Some(s), seg.e);
                        let mut was = bi.and_then(shown).is_some();
                        for j in span.clone() {
                            let now = shown(j);
                            let next = j + 1 < span.end && shown(j + 1).is_some();
                            if let Some(r) = now
                                && (!was || !next)
                            {
                                put(r);
                            }
                            was = now.is_some();
                        }
                        // (integration-r4) A row under a scroll that moved
                        // by itself travels as far as the scroll carried
                        // its content, though it shows only while inside
                        // the viewport: its per-tick bound is the
                        // scroll's, so a wheel spring that snaps its way
                        // in one tick still fails.
                        let carried = if a == super::Axis::Y {
                            cx.scroll_travel(key, s, seg.e)
                        } else {
                            0.0
                        };
                        (value(e, a) - value(b, a)).abs().max(hi - lo).max(carried)
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
                    // (eval-false-alarms) Its own move in its parent: what
                    // its parent's layout carried it by is the parent's
                    // move, judged on the parent (FLIP glides a node that
                    // moved with its parent by nothing more,
                    // `layout_pass::glide_moved`), so a glide that is
                    // ending when an ancestor's layout snaps (a held
                    // surface's root placed where its smaller box will
                    // be) has nothing to retarget.
                    let carried_by = cx
                        .parent(key)
                        .and_then(|pk| Some((cx.tl.row(p, pk)?, cx.tl.screens[i].get(pk)?)))
                        .map_or(0.0, |(pq, pr)| laid(pr, a) - laid(pq, a));
                    let own = dl - carried_by;
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
                        && (cx.lays_out(i, r.surface) || cx.scrolled(i, key));
                    if glides
                        && offset_axis
                        && !follows
                        && dl.abs() > 1.0f64.max(cx.tol)
                        && own.abs() > 1.0f64.max(cx.tol)
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

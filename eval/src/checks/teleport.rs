//! `teleport`: a moving node does not jump. Between two frames in which
//! it shows, with any motion on it or an ancestor at either, each edge
//! moves at most `max(teleport_px * 60 / hz, 0.75 * d) + tol` per tick
//! between them. `d` is the segment's travel (for a node that enters or
//! leaves in the segment, from where it first shows or to where it last
//! shows, and (eval-false-alarms) every place it comes back onto its
//! surface or leaves it again, and where each later change of the
//! segment found it: an offset sent out and turned back travels its
//! excursion; and (evalfix) every place its written offsets, its own and
//! its ancestors', put it while a spring of them is in flight, and where
//! each such spring started, both read from the probe's parts and never
//! from the drawn box, so an offset sprung out and called back by
//! logic's own timer travels its excursion though no frame lays it out
//! elsewhere and no step restarts it, while a frame drawn off its parts
//! does not widen its own bound; and (integration-r4) for a node under a scroll that moved
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
//! is the parent's) and, while its surface lays out anew by itself
//! (eval-coverage-4), beyond the gap to the sibling laid before it (slots
//! folding before it carry it there), since a glide moves a box and never sizes it
//! (layout lengths snap, design.md "What can't interpolate"); on w and h
//! only motion that sizes a box counts (a size spring or a morph, its own or an ancestor's, lean, jelly, or authored motion
//! on w, h, scale or rotate: keyframes or `t` on those axes, a scale
//! spring): a glide, a drag, an offset or opacity spring, a reveal, a
//! stagger and a pose of opacity or offset move or fade a box, never size
//! it.
//!
//! (eval-rules-5) A node carried by a motion that is not its own (a pose
//! the compositor applies to its surface, its parent's slide in flight)
//! travels that motion's way, though it shows on only some of its
//! frames: one that comes onto the screen as a panel slides in, or
//! leaves through a clip a page slides out of, is carried, not jumping.
//! A text whose content changed steps to its new width (and its left
//! edge with it, by the share its parent's declared alignment gives,
//! integration-r7): that step is no jump. Under
//! reduced motion, where every spring snaps, it holds no jump; instead
//! [`reduced_scrolls`] holds each scroll to its input: no fling at the
//! lift.

use std::collections::HashMap;

use super::{Axis, Cx, Finding, GEOMETRY, Sev, first_per_node, laid, value, way};
use crate::scenario::CheckId;
use crate::trace::Row;
use strand_render::probe::{Axes, Motions};

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = reduced_scrolls(cx.sc, cx.trace, cx.tol, CheckId::Teleport);
    // (eval-rules-5) How often a carrier's way or a content step decided.
    let (mut carried_by, mut steps) = (0u64, 0u64);
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
                        // (evalfix) Every place its written offsets put
                        // it while a spring of them is in flight: an
                        // offset (`x:`, `y:`, a pose's) springs between
                        // where it was when the spring was sent and where
                        // it is written to go, and both are on its way,
                        // as each laid place is on a size spring's. An
                        // offset sent out and called back by logic's own
                        // timer within the segment travels its excursion
                        // though no frame lays it out elsewhere and no
                        // step restarts it; one that snaps has no spring
                        // in flight and travels nothing more. Both places
                        // are read from the probe's declared parts, never
                        // from the drawn box (evalfix review): a frame
                        // drawn off its parts (a stray displacement, a
                        // spring drawn twice) cannot widen its own bound.
                        if let Some(n) = match a {
                            Axis::X => Some(0),
                            Axis::Y => Some(1),
                            _ => None,
                        } {
                            for j in span.clone() {
                                let Some(r) = shown(j) else { continue };
                                if !sprung(cx, j, key, r, n) {
                                    continue;
                                }
                                let at = written_place(r, n);
                                lo = lo.min(at);
                                hi = hi.max(at);
                                // Where the spring started: the frame
                                // before it first came into flight.
                                if let Some(q) = j.checked_sub(1).and_then(shown)
                                    && !sprung(cx, j - 1, key, q, n)
                                {
                                    let at = placed(q, n);
                                    lo = lo.min(at);
                                    hi = hi.max(at);
                                }
                            }
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
                        // (eval-rules-5) The motion that carries it: a
                        // node that comes onto the screen under a pose
                        // the compositor applies (a panel sliding in) or
                        // leaves through a clip (a page sliding out of
                        // its `pages`) shows on only some of the frames
                        // its carrier moves it on, and its way is the
                        // carrier's, not the part of it that shows.
                        let carrier =
                            carrier_travel(cx, key, r.surface, start, span.end, &restarts, a);
                        let own = (value(e, a) - value(b, a)).abs().max(hi - lo).max(carried);
                        if carrier > own + cx.tol {
                            carried_by += 1;
                        }
                        own.max(carrier)
                    });
                    let left = match (carried, a) {
                        (true, super::Axis::X) => f64::from(q.glide[0] + q.flight[0]).abs(),
                        (true, super::Axis::Y) => f64::from(q.glide[1] + q.flight[1]).abs(),
                        _ => 0.0,
                    };
                    let bound = (per_tick * ticks).max(0.75 * travel.max(left)) + cx.tol;
                    let d = value(r, a) - value(q, a);
                    // (eval-rules-5) A text whose content changed at this
                    // frame steps to its new width (layout lengths snap,
                    // design.md "What can't interpolate"): that part of
                    // the change is no jump of a motion.
                    // (integration-r7) Its left edge moves with that step
                    // by the share its parent's declared alignment gives
                    // (`NodeMeta::x_share`: none at the start, half
                    // centred, all at the end), the other way: a centred
                    // text that narrows moves right by half, a text at
                    // the start not at all. Where its parent's props do
                    // not fix the share, its left edge is given nothing.
                    let d = match a {
                        super::Axis::W => {
                            let step = content_step(cx, i, p, q, r, d);
                            if step != 0.0 {
                                steps += 1;
                            }
                            d - step
                        }
                        super::Axis::X => {
                            let share = cx.trace.nodes.get(*key).and_then(|m| m.x_share);
                            let dw = value(r, super::Axis::W) - value(q, super::Axis::W);
                            let step = content_step(cx, i, p, q, r, dw);
                            match share {
                                Some(k) if step != 0.0 => {
                                    steps += 1;
                                    d + f64::from(k) * step + carried_step(cx, key, r.surface, p, i)
                                }
                                _ => d,
                            }
                        }
                        _ => d,
                    };
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
                    // (eval-coverage-4) A node stacked after a sibling
                    // moves with that sibling's far edge while the surface
                    // lays out anew by itself: slots folding before it (an
                    // exit's height spring) carry it as a parent's move
                    // does, glide in flight or not. Its glide answers for
                    // its move beyond the gap to the sibling before it.
                    let beside = offset_axis
                        && cx.lays_out(i, r.surface)
                        && carried_beside(cx, p, i, key, a)
                            .is_some_and(|c| (dl - c).abs() <= 1.0f64.max(cx.tol));
                    if glides
                        && offset_axis
                        && !follows
                        && !beside
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
    super::stats(
        "teleport.carried",
        cx,
        &[("carried", carried_by), ("content-steps", steps)],
    );
    first_per_node(out)
}

/// (evalfix) Whether a spring is in flight at timeline frame `j` on
/// axis `n` (0 x, 1 y) on the offset of `key` (row `r`) or of an
/// ancestor drawn on its surface there (`Row::spring`: how far an
/// offset's spring, or a pose's, has it from what it is written to),
/// past a tolerance.
fn sprung<'a>(cx: &Cx<'a>, j: usize, key: &'a str, r: &Row, n: usize) -> bool {
    f64::from(r.spring[n].abs()) > cx.tol
        || cx.lineage(key).into_iter().skip(1).any(|anc| {
            cx.tl
                .row(j, anc)
                .filter(|p| p.surface == r.surface)
                .is_some_and(|p| f64::from(p.spring[n].abs()) > cx.tol)
        })
}

/// What the transforms over `r` (its own and its ancestors' scale and
/// turn, the compositor's pose) add to its place on axis `n`: its
/// visual box less its box before them. A translation of the box moves
/// both alike and is not in it.
fn transforms(r: &Row, n: usize) -> f64 {
    f64::from(r.visual[n]) - f64::from(r.drawn[n])
}

/// (evalfix) Where the written offsets of row `r` and its ancestors put
/// it on axis `n` (0 x, 1 y): its laid place, plus what its ancestors
/// have written (`inherited` less its part in flight, `flight`) and its
/// own written offset, through its transforms. Read from the probe's
/// declared parts, not from where it was drawn.
fn written_place(r: &Row, n: usize) -> f64 {
    f64::from(r.laid[n]) + f64::from(r.inherited[n]) - f64::from(r.flight[n])
        + f64::from(r.written[n])
        + transforms(r, n)
}

/// (evalfix review) Where the probe's declared parts put row `r` on axis
/// `n` (0 x, 1 y): its laid place, its ancestors' offsets, its own written
/// offset, its springs', glide's and morph's parts, through its
/// transforms. On the frame before a spring comes into flight, where that
/// spring starts from.
fn placed(r: &Row, n: usize) -> f64 {
    f64::from(r.laid[n])
        + f64::from(r.inherited[n])
        + f64::from(r.written[n])
        + f64::from(r.spring[n])
        + f64::from(r.glide[n])
        + f64::from(r.morph[n])
        + transforms(r, n)
}

/// (eval-coverage-4) How far the sibling laid just before `key` on axis
/// `a` (x or y) moved its far edge between timeline frames `p` and `i`:
/// of the siblings that end before `key` starts at both frames, the one
/// ending nearest it at `i`. `None` when `key` has no parent or no such
/// sibling shows at both frames on its surface, and (integration-r7)
/// when no sibling before it sizes on `a` between the two frames (its
/// laid length changes: a slot folding, growing or springing its size):
/// only a slot that sizes before it carries it as a parent's move does.
/// A follower whose siblings before it are all carried too (a size
/// spring elsewhere moving the whole row) keeps its glide held.
fn carried_beside(cx: &Cx, p: usize, i: usize, key: &str, a: super::Axis) -> Option<f64> {
    let parent = cx.parent(key)?;
    let (r, q) = (cx.tl.row(i, key)?, cx.tl.row(p, key)?);
    let len = |s: &Row| match a {
        super::Axis::X => f64::from(s.laid[2]),
        _ => f64::from(s.laid[3]),
    };
    let end = |s: &Row| laid(s, a) + len(s);
    let mut best: Option<(f64, f64)> = None;
    let mut sizes = false;
    for (k, s) in &cx.tl.screens[i] {
        if *k == key || cx.parent(k) != Some(parent) || s.surface != r.surface {
            continue;
        }
        let Some(t) = cx.tl.row(p, k).filter(|t| t.surface == q.surface) else {
            continue;
        };
        if end(s) > laid(r, a) + cx.tol || end(t) > laid(q, a) + cx.tol {
            continue;
        }
        // Any change of its laid length (layout snaps lengths to whole
        // pixels: a fold's last steps are 1 px each).
        sizes |= (len(s) - len(t)).abs() > crate::trace::QUANTUM / 2.0;
        if best.is_none_or(|(e, _)| end(s) > e) {
            best = Some((end(s), end(s) - end(t)));
        }
    }
    best.filter(|_| sizes).map(|(_, moved)| moved)
}

/// (eval-rules-5) How far the motion carrying `key` moved it on `a` over
/// the frames `[from, to)` (indexes of the timeline), whether it showed
/// on them or not: what carries it rigidly, its ancestors' offsets in
/// flight (`Row::flight`: a parent's glide, spring or exit pose) and the
/// pose the compositor applies to its surface (its offset, and what its
/// scale does to the node's place), taken where its way starts and ends
/// as the node's own travel is (its first and last frame there, and the
/// frame before each later change of the segment, `restarts`), never at
/// every frame between: a carrier that overshoots or kicks travels no
/// further for it, so a jump of all of it is still a jump. Only frames
/// that painted its surface count (its rows are that frame's own).
fn carrier_travel(
    cx: &Cx,
    key: &str,
    surface: u32,
    from: usize,
    to: usize,
    restarts: &[usize],
    a: Axis,
) -> f64 {
    let at = |i: usize| {
        let r = cx.tl.row(i, key)?;
        (r.surface == surface && cx.tl.frames[i].painted.contains(&surface))
            .then(|| carried(cx, i, r, a))
    };
    let present: Vec<usize> = (from..to).filter(|i| at(*i).is_some()).collect();
    let (Some(first), Some(last)) = (present.first(), present.last()) else {
        return 0.0;
    };
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for i in [*first, *last].iter().chain(restarts) {
        if let Some(c) = at(*i) {
            lo = lo.min(c);
            hi = hi.max(c);
        }
    }
    if hi >= lo { hi - lo } else { 0.0 }
}

/// What carries `r` rigidly at frame `i` on `a` (see [`carrier_travel`]).
fn carried(cx: &Cx, i: usize, r: &Row, a: Axis) -> f64 {
    let n = match a {
        Axis::X => 0,
        Axis::Y => 1,
        Axis::W => 2,
        Axis::H => 3,
        _ => return 0.0,
    };
    let flight = if n < 2 { f64::from(r.flight[n]) } else { 0.0 };
    let pose = cx.tl.frames[i].posed(r.surface).map_or(0.0, |p| {
        if p.scale.abs() < 1e-6 {
            return 0.0;
        }
        // The posed value on its surface and the unposed value it was
        // drawn from (integration-r7: the one model of a pose on screen,
        // `crate::trace::Posed`): what the pose added. A length is only
        // scaled.
        let moved = cx.tl.moved(i, r.surface);
        match n {
            0 | 1 => {
                let v = value(r, a) - f64::from(moved[n]);
                v - f64::from(p.unshow(n, v as f32))
            }
            _ => {
                let v = value(r, a);
                v - v / f64::from(p.scale)
            }
        }
    });
    flight + pose
}

/// (eval-rules-5) Of a text's change in width `d` between frames `p`
/// (row `q`) and `i` (row `r`), the part its content changing explains:
/// none unless the text it drew changed (`PaintRec::gl`). Then the step
/// is from the old content to the new laid in the same room, each at
/// its unbounded width (its cut line's `natural` stops when it is cut,
/// else its glyphs' ink) and no wider than its parent's box at `i`; a
/// part of `d` in the same direction and no larger than it. The rest,
/// what its room's motion did, is held to the bound as before.
fn content_step(cx: &Cx, i: usize, p: usize, q: &Row, r: &Row, d: f64) -> f64 {
    let gl = |row: &Row| row.paint.as_ref().map(|x| x.gl.clone()).unwrap_or_default();
    let (was, now) = (gl(q), gl(r));
    if was.is_empty() || now.is_empty() || was == now {
        return 0.0;
    }
    let natural = |j: usize, row: &Row| -> Option<f64> {
        let f = cx.tl.frames.get(j)?;
        if let Some(l) = f
            .lines
            .iter()
            .find(|l| l.key == row.key && l.s == row.surface)
            && let Some(last) = l.natural.last()
        {
            return Some(f64::from(last.1));
        }
        row.ink.map(|ink| f64::from(ink[2]))
    };
    let (Some(old), Some(new)) = (natural(p, q), natural(i, r)) else {
        return 0.0;
    };
    // Each no wider than its parent's box at its own frame
    // (integration-r7 part 2: a parent sized by its content narrows with
    // the text, so the old width is held to the parent it had then).
    let room = |j: usize| {
        cx.parent(&r.key)
            .and_then(|pk| cx.tl.row(j, pk))
            .map_or(f64::INFINITY, |pr| value(pr, Axis::W))
    };
    let step = new.min(room(i)) - old.min(room(p));
    if step * d <= 0.0 {
        return 0.0;
    }
    step.signum() * step.abs().min(d.abs())
}

/// (integration-r7 part 2) How far the ancestors a text's content step
/// resizes move it on x, the other way: each ancestor up from its
/// parent that is sized by what it holds (no width of its own,
/// [`crate::trace::Flow::SIZED`] unset) and placed in its own parent by
/// a declared share ([`crate::trace::NodeMeta::x_share`]) moves its left
/// edge by that share of the change in its own width between timeline
/// frames `p` and `i`. A content-sized row centred in a box re-centres
/// as its text widens, so the text inside it moves by half the row's
/// change though its own slot is the row's start. The chain stops at the
/// first ancestor whose width is its own, or whose place the declared
/// tree does not fix (stretched, spread, out of flow), or that is not
/// drawn on `surface` at both frames.
fn carried_step(cx: &Cx, key: &str, surface: u32, p: usize, i: usize) -> f64 {
    use crate::trace::Flow;
    let mut out = 0.0;
    let mut at = cx.trace.nodes.get(key).and_then(|m| m.parent.clone());
    let mut n = 0;
    while let Some(k) = at
        && n < 256
    {
        n += 1;
        let Some(m) = cx.trace.nodes.get(&k) else {
            break;
        };
        if m.flow & Flow::SIZED != 0 {
            break;
        }
        let Some(share) = m.x_share else { break };
        let (Some(was), Some(now)) = (
            cx.tl.row(p, &k).filter(|r| r.surface == surface),
            cx.tl.row(i, &k).filter(|r| r.surface == surface),
        ) else {
            break;
        };
        out += f64::from(share) * (value(now, Axis::W) - value(was, Axis::W));
        at = m.parent.clone();
    }
    out
}

/// (integration-r7) How far one wheel notch scrolls a `scroll` or
/// `list`, logical px: decisions.md wave3-pixels ("one with detents but
/// no pixel value scrolls 15 px per detent", libinput's legacy wheel
/// step). An eval constant with that citation, not the product's
/// `WHEEL_STEP`, so a change to the notch moves the product, not the
/// rule's bound.
pub const NOTCH_PX: f64 = 15.0;

/// (eval-rules-5) Under reduced motion a scroll moves only with its
/// input. decisions.md m4-lists-w1: touchpad motion moves the offset
/// with the fingers, "`reduced_motion` lands a step at once and does not
/// fling" (design.md "Runtime changes these need" 7: springs snap). So
/// in a segment whose motion is reduced throughout (no `reduce-motion`
/// step or `system.reduced_motion` set inside it) and that turns a wheel
/// or swipes a touchpad, no `scroll` or `list` is ever further from
/// where the segment found it than the input delivered so far has moved
/// the fingers: each `swipe DY over MS` spread evenly over its ticks,
/// each `scroll DY` its notches of [`NOTCH_PX`] at its tick, the amounts
/// read from the scenario's steps, never from the renderer. An offset
/// may lag (an end stops it, a window waits for its rows), never lead:
/// a fling, or a spring that overshoots, lands past the fingers. One
/// tick's input more is given (a tick counted differently), and the
/// edge tolerance. (integration-r7 part 2) Another step in the segment
/// that acts (a click, a key, a `set`, a pointer drag) may move an
/// offset at once (a row revealed, a list filtered shorter clamping its
/// offset): the frames it acts on are not judged, and the bound starts
/// again from where they leave each offset, counting only the input
/// after it. `check` names who reports it: `teleport` for the
/// run's own reduced segments, `twin` for its reduced-motion re-run.
pub fn reduced_scrolls(
    sc: &crate::scenario::Scenario,
    trace: &crate::trace::RunTrace,
    tol: f64,
    check: CheckId,
) -> Vec<Finding> {
    use crate::steps::StepKind;
    let mut out = Vec::new();
    let (mut judged_segments, mut judged_offsets, mut rebased) = (0u64, 0u64, 0u64);
    for (si, seg) in trace.segments.iter().enumerate() {
        let Some(s) = seg.s else { continue };
        if !seg.reduced_motion || seg.e <= s {
            continue;
        }
        let events: Vec<_> = trace
            .events
            .iter()
            .filter(|e| e.segment == Some(si))
            .collect();
        // Reduced only from part of the way: a motion in flight when it
        // turned on snaps to where it was going.
        if events.iter().any(|e| {
            e.text.starts_with("reduce-motion") || e.text.contains("system.reduced_motion")
        }) {
            continue;
        }
        let period = 1_000_000_000u64 / u64::from(seg.hz.max(1));
        // (first tick, ticks, px) of each input.
        let mut inputs: Vec<(u32, u32, f64)> = Vec::new();
        // (integration-r7 part 2) The ticks over which another step acts
        // (first, last): a click, a key or a `set` that reveals a row or
        // filters the list moves an offset at once by design (m4-lists-w1
        // `reveal_index`; a list that shrinks clamps its offset to its new
        // end), so the bound starts again after it.
        let mut others: Vec<(u32, u32)> = Vec::new();
        let ticks = |ms: u32| (u64::from(ms) * 1_000_000).div_ceil(period).max(1) as u32;
        for e in &events {
            let Some(step) = sc.steps.iter().find(|st| st.line == e.line) else {
                continue;
            };
            match &step.kind {
                StepKind::Swipe { dy, ms } => {
                    inputs.push((e.k, ticks(*ms), dy.abs()));
                }
                StepKind::Scroll { dy } => {
                    inputs.push((e.k, 1, (dy * NOTCH_PX).abs()));
                }
                StepKind::Glide { ms, .. } | StepKind::Drag { ms, .. } => {
                    others.push((e.k, e.k + ticks(*ms)));
                }
                StepKind::Mark(_)
                | StepKind::Shot(_)
                | StepKind::Log(_)
                | StepKind::Intend { .. }
                | StepKind::Expect { .. }
                | StepKind::Frames(_)
                | StepKind::Wait(_)
                | StepKind::Rest { .. }
                | StepKind::Jitter(_)
                | StepKind::Hz(_)
                | StepKind::Screen(_)
                | StepKind::At(_) => {}
                _ => others.push((e.k, e.k)),
            }
        }
        if inputs.is_empty() {
            continue;
        }
        let slack = tol
            + inputs
                .iter()
                .map(|(_, n, px)| px / f64::from(*n))
                .fold(0.0, f64::max);
        let delivered = |k: u32| -> f64 {
            inputs
                .iter()
                .filter(|(k0, _, _)| *k0 <= k)
                .map(|(k0, n, px)| px * f64::from((k - k0 + 1).min(*n)) / f64::from(*n))
                .sum()
        };
        // Where the segment found each scroll: its offset at or before `s`.
        let mut found: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
        for f in trace.frames.iter().filter(|f| f.k <= s) {
            for (key, o) in &f.offsets {
                found.insert(key.as_str(), f64::from(*o));
            }
        }
        let mut told: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        judged_segments += 1;
        // The tick the bound counts input from (none: the segment's
        // start, every input in it).
        let mut base: Option<u32> = None;
        let mut prev = s;
        for f in trace.frames.iter().filter(|f| f.k > s && f.k <= seg.e) {
            // Another step acted since the frame before: where this frame
            // leaves each offset is where the bound starts again.
            if others.iter().any(|(a, b)| *a <= f.k && *b > prev) {
                found = f
                    .offsets
                    .iter()
                    .map(|(key, o)| (key.as_str(), f64::from(*o)))
                    .collect();
                base = Some(f.k);
                prev = f.k;
                rebased += 1;
                continue;
            }
            prev = f.k;
            for (key, o) in &f.offsets {
                let Some(o0) = found.get(key.as_str()) else {
                    continue;
                };
                judged_offsets += 1;
                let moved = (f64::from(*o) - o0).abs();
                let may = delivered(f.k) - base.map_or(0.0, &delivered);
                if moved > may + slack && told.insert(key.as_str()) {
                    out.push(
                        Finding::fail(
                            check,
                            format!(
                                "reduced fling: with motion reduced its offset is {moved:.1} px \
                                 from where the segment (or the last other step in it) found \
                                 it, past the {may:.1} px its \
                                 input has moved (reduced motion does not fling, and a step \
                                 lands at once)"
                            ),
                        )
                        .at(f.k)
                        .node(key)
                        .axis("y")
                        .got(moved, [0.0, may + slack])
                        .in_segment(&seg.name),
                    );
                }
            }
        }
    }
    super::stats_of(
        &format!("{}.reduced-scroll", check.name()),
        &trace.scenario.id,
        &[
            ("segments", judged_segments),
            ("offsets", judged_offsets),
            ("rebased", rebased),
        ],
    );
    out
}

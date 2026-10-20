//! `vanish`: what shows does not blink.
//!
//! - **vanish**: shown at one frame, not at the next, shown again within
//!   three ticks or before its segment rests (eval-rules-3: a node gone
//!   for seven frames with nothing taking it out blinked as surely, set
//!   E's module swap), with no exit, enter or authored opacity or scale on it
//!   or an ancestor around it, and no step bringing it back (none acts
//!   between the frame it is gone in and the one it is back in). One
//!   still drawn whole while gone, only past its surface's edge where a
//!   scroll's spring, a glide, a drag or an authored offset moved it, left
//!   view by moving and did not blink (eval-false-alarms);
//! - **flash**: shown at one frame only, between frames it does not
//!   show in, gone again within three ticks (eval-rules-7: a frame its
//!   surface keeps up longer, idle, shows it that long), with no
//!   keyframes or time-driven motion, no scroll moving it by itself (a
//!   spring or fling that moves more than the viewport and a row in one
//!   frame carries a row across it between two frames), and no step
//!   taking it out (none acts between that frame and the next);
//! - **pop-in**: an element with an `enter` pose, mounted after boot,
//!   first shows near its pose: the pose's opacity and offset start its
//!   springs, and so do its width and height (its laid box; unshaped-
//!   enter), and its first frame shown has covered at most half the way
//!   to rest (a pose skipped shows it at rest at once). It holds each
//!   time the element comes onto a surface: when it mounts, and when the
//!   surface it is on is attached anew (a popup opened again). A shared
//!   morph plays in place of the pose (design.md, "Motion and time";
//!   anim/morph.rs), so a first frame that morphs is no pop-in: where it
//!   starts is `coherence`'s morph start (decisions.md shared-morph-on);
//! - **blank**: a frame that damaged pixels and drew no item while
//!   something inside its surface's root that draws (eval-rules-3: a
//!   paint of its own, an image, icon or shader; a bare container shows
//!   nothing) shows (a popup's first frame at
//!   a pose's `opacity: 0`, or its last as its exit ends, shows nothing
//!   and draws nothing);
//! - **pose back**: with the compositor applying surface poses, a
//!   surface logic closed (integration-r7 part 2: its diff removed its
//!   root or set its `open` false) commits, frame after frame until it
//!   goes or is opened again (eval-rules-5; integration-r7: logic's diff
//!   creates its root again), a pose that never comes back toward rest
//!   while short of its exit pose (an overshoot settling onto the pose is
//!   no pose back; decisions.md exit-overshoot) (owner-testing.md
//!   bug 7: the frame that ended a closing popup's exit committed the
//!   rest pose, and the compositor faded the whole menu out from full
//!   opacity).

use std::collections::BTreeSet;

use strand_render::probe::{Axes, Motions};

use super::{Cx, Finding, first_per_node, motions};
use crate::scenario::CheckId;
use crate::steps::StepKind;
use crate::trace::Row;

/// How far an entering node's first frame shown may be from its pose,
/// as a share of its way to rest.
pub const POP_IN_SHARE: f64 = 0.5;
/// Ticks within which a node shown again counts as a blink.
pub const BLINK_TICKS: u32 = 3;

fn leaving(r: &Row) -> bool {
    motions(r).intersects(Motions::EXIT | Motions::ENTER)
        || Axes(r.auth).intersects(Axes::OPACITY | Axes::SCALE)
}

pub fn run(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let mut out = Vec::new();
    let keys: BTreeSet<&str> = tl.screens.iter().flat_map(|s| s.keys().copied()).collect();
    let boot_end = cx.trace.segments.first().map_or(0, |s| s.e);
    let seg_of = |k: u32| {
        cx.trace
            .segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.s.is_none_or(|s| s < k) && k <= s.e)
    };
    // A step acted after tick `from` and by tick `to`: a segment starts
    // there (its `s` is the tick before its first change), or a later
    // step of one acts at a tick in between (eval-coverage-2: a second
    // `scroll` in one segment scrolls rows back into view as surely as
    // the first; an event's `k` is the tick it acts at).
    let mut acts: Vec<u32> = Vec::new();
    // (eval-coverage-2, integration-r4) A `swipe` acts on every tick of
    // its span (one axis frame a tick), but only on what lies under the
    // scrolls under the pointer as it starts, which its axis frames
    // move: a swipe faster than the viewport carries those rows across
    // it between two frames as the fingers do. Nothing else on screen is
    // moved by it, and a `glide` or `drag` moves a pointer, not rows.
    let mut swipes: Vec<(u32, u32, BTreeSet<&str>)> = Vec::new();
    for e in &cx.trace.events {
        let Some(st) = cx
            .sc
            .steps
            .iter()
            .find(|st| st.line == e.line && st.kind.opens_segment())
        else {
            continue;
        };
        acts.push(e.k);
        if let StepKind::Swipe { ms, .. } = st.kind {
            let hz = e
                .segment
                .and_then(|i| cx.trace.segments.get(i))
                .map_or(cx.sc.hz, |s| s.hz)
                .max(1);
            let ticks = (u64::from(ms) * u64::from(hz)).div_ceil(1000) as u32;
            swipes.push((e.k, e.k.saturating_add(ticks), swiped(cx, e.k)));
        }
    }
    let under = |key: &str, scrolls: &BTreeSet<&str>| {
        let mut at = cx.parent(key);
        for _ in 0..256 {
            let Some(p) = at else { return false };
            if scrolls.contains(p) {
                return true;
            }
            at = cx.parent(p);
        }
        false
    };
    let stepped = |key: &str, from: u32, to: u32| {
        cx.trace
            .segments
            .iter()
            .any(|s| s.s.is_some_and(|s| from <= s && s < to))
            || acts.iter().any(|&a| from < a && a <= to)
            || swipes
                .iter()
                .any(|(a, b, sc)| *a <= to && from < *b && under(key, sc))
    };
    for key in keys {
        let vis: Vec<bool> = (0..tl.ks.len()).map(|i| tl.visible_key(i, key)).collect();
        for i in 1..vis.len() {
            let k = tl.ks[i];
            let seg = seg_of(k);
            let exempt = |c| seg.is_some_and(|(si, _)| cx.exempt(c, si, key, Axes::ALL));
            let name = seg.map(|(_, s)| s.name.as_str()).unwrap_or("");
            // vanish: back within three ticks, or (eval-rules-3) by the
            // rest of the segment it went in. Nothing but a step brings
            // a node back once its segment's change has played out of
            // it, so one gone for longer and back before that rest, with
            // no step and nothing leaving, blinked as surely (set E's
            // module swap: a bar's every node laid out past its bottom
            // for seven frames while a label's exit wrapped it).
            if vis[i - 1] && !vis[i] && !exempt(CheckId::Vanish) {
                let until = seg.map_or(0, |(_, s)| s.e).max(k + BLINK_TICKS);
                let back = (i + 1..vis.len())
                    .take_while(|j| tl.ks[*j] <= until)
                    .find(|j| vis[*j])
                    .filter(|j| !stepped(key, k, tl.ks[*j]));
                if let Some(j) = back {
                    let around = (i - 1..=j).filter_map(|x| tl.row(x, key)).any(leaving);
                    if !around && !carried_off(cx, i, key) {
                        out.push(
                            Finding::fail(
                                CheckId::Vanish,
                                format!(
                                    "vanish: gone for {} tick(s) and back, with nothing \
                                     taking it out",
                                    tl.ks[j] - k
                                ),
                            )
                            .at(k)
                            .node(key)
                            .in_segment(name),
                        );
                    }
                }
            }
            // flash
            // (eval-rules-7) Gone again within three ticks, as a blink
            // is back within them: a frame its surface keeps on screen
            // longer, painting nothing new (practice set 8's OSD opened
            // under reduced motion, drawn once and left up for a second
            // until it closed), showed it for that long, not for a frame.
            if i + 1 < vis.len()
                && !vis[i - 1]
                && vis[i]
                && !vis[i + 1]
                && tl.ks[i + 1] - k <= BLINK_TICKS
                && !stepped(key, k, tl.ks[i + 1])
                && !exempt(CheckId::Vanish)
            {
                // (eval-coverage-2) A scroll moving by itself carries its
                // rows past its viewport at its own speed: one that
                // moves more than the viewport and a row in a frame
                // shows a row in one frame only.
                let free = tl.row(i, key).is_some_and(|r| {
                    motions(r).intersects(Motions::KEYFRAMES | Motions::TIME | Motions::SCROLL)
                });
                if !free {
                    out.push(
                        Finding::fail(CheckId::Vanish, "flash: shown for one frame only")
                            .at(k)
                            .node(key)
                            .in_segment(name),
                    );
                }
            }
        }
        // pop-in, each time it comes onto a surface: its first frame on
        // each surface it is drawn on (a surface attached anew is another
        // surface).
        let Some(pose) = cx.trace.nodes.get(key).and_then(|m| m.enter) else {
            continue;
        };
        let mut onto: Vec<usize> = Vec::new();
        let mut surfaces: BTreeSet<u32> = BTreeSet::new();
        for i in 0..tl.ks.len() {
            if let Some(r) = tl.row(i, key)
                && surfaces.insert(r.surface)
            {
                onto.push(i);
            }
        }
        for first in onto {
            if let Some(f) = pop_in(cx, key, &pose, first, boot_end, &vis, &seg_of) {
                out.push(f);
            }
        }
    }
    // blank: something inside the surface's root shows (the root is the
    // surface itself, whose own paint the trace cannot tell) that draws
    // something of its own. (eval-rules-3) A container that paints
    // nothing shows nothing: a panel's first frame, its one toast at its
    // pose's `opacity: 0` inside a bare `col`, draws no item and is
    // right to (set E's toast stack). What draws is read from the run: a
    // node that painted a fill, border, shadow or text on some frame, or
    // an image, icon or shader, whose pixels the paint record leaves out.
    let roots: BTreeSet<&str> = cx.trace.surfaces.iter().map(|s| s.root.as_str()).collect();
    for f in &cx.trace.frames {
        let Some(i) = tl.at(f.k) else { continue };
        for (s, c) in &f.cost {
            let shows = tl.screens[i].values().any(|r| {
                r.surface == *s
                    && !roots.contains(r.key.as_str())
                    && cx.draws(&r.key)
                    && tl.visible(i, r)
            });
            if c.damage_px > 0 && c.items == 0 && shows {
                let mut b = Finding::fail(
                    CheckId::Vanish,
                    format!(
                        "blank on surface {s}: damaged {} px and drew nothing, though what \
                         it holds shows",
                        c.damage_px
                    ),
                )
                .at(f.k);
                if let Some((_, seg)) = seg_of(f.k) {
                    b = b.in_segment(&seg.name);
                }
                out.push(b);
            }
        }
    }
    out.extend(pose_back(cx));
    out.extend(pop_out(cx));
    first_per_node(out)
}

/// How far from its exit pose an exiting node's last frame may show it:
/// one opacity quantum (1/255, the step a recording holds), a scale step
/// of 1/200, and `tol` on a place or size.
pub const POP_OUT_OPACITY: f64 = 1.0 / 255.0;
pub const POP_OUT_SCALE: f64 = 0.005;

/// (eval-rules) `pop-out`: a node that plays its `exit` pose and then
/// unmounts (design.md "Poses": "A removed subtree plays `exit`, then
/// unmounts") is, at the last frame it is drawn in, at its pose: its own
/// opacity, offset, scale and laid size where the pose puts them, or it
/// shows nothing (its effective opacity at most a quantum: an ancestor
/// faded it out; eval-rules-7: or it does not show at all, cut out by a
/// clip or carried off its surface, `Timeline::visible`, or a clipping
/// ancestor shows no area the frame after). An exit cut short pops what
/// is left of the node out in one frame. Not held under reduced motion,
/// where a pose snaps.
fn pop_out(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let mut out = Vec::new();
    for (key, meta) in &cx.trace.nodes {
        let Some(pose) = meta.exit else { continue };
        // The last frame each run of its rows is in, followed by a frame
        // without it.
        for i in 0..tl.ks.len() {
            let Some(r) = tl.row(i, key) else { continue };
            if i + 1 >= tl.ks.len() || tl.row(i + 1, key).is_some() {
                continue;
            }
            if !Motions(r.mot).contains(Motions::EXIT) {
                continue;
            }
            let k = tl.ks[i];
            let Some((_, seg)) = cx
                .trace
                .segments
                .iter()
                .enumerate()
                .rev()
                .find(|(_, s)| s.s.is_none_or(|s| s < k) && k <= s.e)
            else {
                continue;
            };
            // (eval-rules-7) What does not show on its last frame has
            // nothing to pop: faded out by an ancestor (its effective
            // opacity at most a quantum), or cut out by a clip or its
            // surface's edge (set J's network pages: a page's rows
            // unmount at their full pose, carried wholly past the pages'
            // clip by their page's slide, `Timeline::visible`).
            if seg.reduced_motion
                || cx.trace.reduced_throughout()
                || cx.allowed(CheckId::Vanish, key)
                || f64::from(r.op) <= POP_OUT_OPACITY
                || !tl.visible(i, r)
                || shut_after(cx, key, i + 1)
            {
                continue;
            }
            let mut wrong = Vec::new();
            if let Some(po) = pose.opacity
                && (f64::from(r.own_op) - f64::from(po)).abs() > POP_OUT_OPACITY
            {
                wrong.push(format!("opacity {:.3} (its pose's {po})", r.own_op));
            }
            for (axis, p, at) in [
                ("x", pose.x, r.written[0] + r.spring[0]),
                ("y", pose.y, r.written[1] + r.spring[1]),
            ] {
                if let Some(p) = p
                    && (f64::from(at) - f64::from(p)).abs() > cx.tol
                {
                    wrong.push(format!("offset {at:.2} on {axis} (its pose's {p})"));
                }
            }
            if let Some(ps) = pose.scale {
                let s = f64::from(r.scale[0].max(r.scale[1]));
                if (s - f64::from(ps)).abs() > POP_OUT_SCALE {
                    wrong.push(format!("scale {s:.3} (its pose's {ps})"));
                }
            }
            for (axis, p, at) in [("w", pose.w, r.laid[2]), ("h", pose.h, r.laid[3])] {
                if let Some(p) = p
                    && (f64::from(at) - f64::from(p)).abs() > cx.tol
                {
                    wrong.push(format!("laid {axis} {at:.2} (its pose's {p})"));
                }
            }
            if !wrong.is_empty() {
                out.push(
                    Finding::fail(
                        CheckId::Vanish,
                        format!(
                            "pop-out: unmounted after this frame short of its exit pose: {}",
                            wrong.join("; ")
                        ),
                    )
                    .at(k)
                    .node(key)
                    .in_segment(&seg.name),
                );
            }
        }
    }
    out
}

/// (eval-rules-7) Whether an ancestor of `key` that clips its content
/// shows no area at timeline frame `j` (under half a pixel across): what
/// lies inside it shows nothing there, wherever its own pose is, so a
/// node gone from that frame left by its clip, not by a pop. Practice
/// set 8: a filtered list's rows exit to `height: 0` and the list sized
/// by them is 0 tall the frame they reach their pose; a chip exiting to
/// `width: 0` is 0 wide while a child of it with its own exit is gone.
/// The renderer draws nothing of them there and ends their exits.
fn shut_after(cx: &Cx, key: &str, j: usize) -> bool {
    let tl = &cx.tl;
    cx.lineage(key).into_iter().skip(1).any(|a| {
        let Some(p) = tl.row(j, a) else {
            return false;
        };
        let meta = cx.trace.nodes.get(a);
        let own = Motions(p.mot);
        // It clips: `clip: true`, a `scroll` or a `list`, or (contain's
        // `held`) a box whose size springs or that exits to a size.
        let clips = meta.is_some_and(|m| m.clips)
            || own.contains(Motions::SIZE_SPRING)
            || (own.contains(Motions::EXIT)
                && meta
                    .and_then(|m| m.exit)
                    .is_some_and(|e| e.w.is_some() || e.h.is_some()));
        clips && (p.visual[2] < 0.5 || p.visual[3] < 0.5)
    })
}

/// The segment tick `k` falls in.
type SegOf<'a> = dyn Fn(u32) -> Option<(usize, &'a crate::trace::Segment)> + 'a;

/// The pop-in finding of `key` coming onto a surface at timeline frame
/// `first`, if its first frame shown there skipped its `pose`.
fn pop_in<'a>(
    cx: &Cx<'a>,
    key: &'a str,
    pose: &crate::trace::Pose,
    first: usize,
    boot_end: u32,
    vis: &[bool],
    seg_of: &SegOf<'a>,
) -> Option<Finding> {
    let tl = &cx.tl;
    {
        let k0 = tl.ks[first];
        let (si, seg) = seg_of(k0)?;
        if k0 <= boot_end || seg.reduced_motion || cx.exempt(CheckId::Vanish, si, key, Axes::ALL) {
            return None;
        }
        let surface = tl.row(first, key)?.surface;
        let shown = (first..tl.ks.len())
            .take_while(|i| tl.row(*i, key).is_none_or(|r| r.surface == surface))
            .find(|i| vis[*i])?;
        let (r0, r) = (tl.row(first, key)?, tl.row(shown, key)?);
        if Motions(r0.mot).contains(Motions::SHARED_MORPH) {
            return None;
        }
        // Where it rests: at the segment's rest, else (gone by then, or
        // not drawn) its last frame in the segment at rest, neither
        // entering nor leaving; with neither, how far its first frame is
        // along its pose cannot be told.
        let at_rest = |q: &&crate::trace::Row| {
            !Motions(q.mot).intersects(Motions::ENTER | Motions::EXIT)
                && q.hidden.as_deref() != Some("inert")
        };
        let rest = tl.at(seg.e).and_then(|e| tl.row(e, key)).or_else(|| {
            tl.within(Some(tl.ks[shown]), seg.e).rev().find_map(|j| {
                tl.row(j, key)
                    .filter(|q| q.surface == surface)
                    .filter(at_rest)
            })
        });
        let entered = Motions(r0.mot).contains(Motions::ENTER);
        // Not drawn until an ancestor drawn transparent came into view:
        // its pose was dropped unseen and it shows at rest by design
        // (decisions.md 2026-10-06 wave3-pixels (p2), "Enter poses of
        // nodes a frame did not draw (… under a transparent parent) are
        // dropped: they show at rest when they come into view").
        if !entered && dropped_unseen(cx, key, first, surface) {
            return None;
        }
        let mut wrong = Vec::new();
        // A pose that moves nothing the trace reads (each axis it holds
        // is where the node rests) has nothing to play, and a pose of its
        // size alone plays as a size spring, which `ENTER` (the paint
        // props' springs) does not report.
        let sized = pose.opacity.is_none()
            && pose.x.is_none()
            && pose.y.is_none()
            && pose.scale.is_none()
            && Motions(r0.mot).contains(Motions::SIZE_SPRING);
        if !entered && !sized && !rest.is_some_and(|rest| still_pose(pose, rest, cx.tol)) {
            wrong.push("it mounted without playing its pose".to_string());
        }
        let share = |at: f64, from: f64, to: f64| -> Option<f64> {
            ((to - from).abs() > 0.05).then(|| (at - from) / (to - from))
        };
        if let Some(po) = pose.opacity
            && let Some(rest) = rest
            && let Some(sh) = share(f64::from(r.own_op), f64::from(po), f64::from(rest.own_op))
            && sh > POP_IN_SHARE
        {
            wrong.push(format!(
                "its first frame shown is at opacity {:.2}, {:.0}% of the way from its pose's {po}",
                r.own_op,
                sh * 100.0
            ));
        }
        for (axis, p, at) in [("x", pose.x, r.spring[0]), ("y", pose.y, r.spring[1])] {
            if let Some(p) = p
                && f64::from(p).abs() > cx.tol
                && let Some(sh) = share(f64::from(at), f64::from(p), 0.0)
                && sh > POP_IN_SHARE
            {
                wrong.push(format!(
                    "its first frame shown is {at:.1} px off on {axis}, {:.0}% of the way from \
                     its pose's {p}",
                    sh * 100.0
                ));
            }
        }
        // (unshaped-enter) A pose's width or height is where its size
        // springs from (design.md "Poses": `enter {}` is where a node
        // animates in from, `enter { width: 0; opacity: 0 }`): its laid
        // box on its first frame shown has covered at most as much of
        // the way to its box at rest.
        for (axis, p, ax) in [("width", pose.w, 2), ("height", pose.h, 3)] {
            if let Some(p) = p
                && let Some(rest) = rest
                && let Some(sh) = share(
                    f64::from(r.laid[ax]),
                    f64::from(p),
                    f64::from(rest.laid[ax]),
                )
                && sh > POP_IN_SHARE
            {
                wrong.push(format!(
                    "its first frame shown is laid {:.1} px in {axis}, {:.0}% of the way from \
                     its pose's {p} to {:.1} at rest",
                    r.laid[ax],
                    sh * 100.0,
                    rest.laid[ax]
                ));
            }
        }
        (!wrong.is_empty()).then(|| {
            Finding::fail(CheckId::Vanish, format!("pop-in: {}", wrong.join("; ")))
                .at(tl.ks[shown])
                .node(key)
                .in_segment(&seg.name)
        })
    }
}

/// (eval-false-alarms) True if `key`, not shown at timeline frame `i`,
/// is still drawn there whole (its opacity, scale and size), hidden for
/// no reason the renderer gives (never `clipped`: an ancestor's clip
/// cutting it out is a blink, integration-r4), and lies wholly past its
/// surface's edge, moved there by motion that moves a box: a scroll's
/// spring under it, a glide or drag, or an authored offset. It left
/// view by moving, as a list scrolled past its rows takes them out, and
/// came back the same way; `teleport` and `bystander` judge how it
/// moved.
fn carried_off(cx: &Cx, i: usize, key: &str) -> bool {
    let Some(r) = cx.tl.row(i, key) else {
        return false;
    };
    let drawn = r.hidden.is_none() && r.op >= 0.02 && r.visual[2] * r.visual[3] >= 1.0;
    // Not shown though drawn whole and not hidden: only its place off
    // its surface can have taken it out of view.
    let off = drawn && !cx.tl.visible(i, r);
    let moved = cx.scrolled(i, key)
        || (i > 0 && cx.scrolled(i - 1, key))
        || Motions(r.emot).intersects(Motions::GLIDE | Motions::LIFT)
        || Axes(r.auth).intersects(Axes::X | Axes::Y);
    off && moved
}

/// (integration-r4) The `scroll` and `list` nodes under the pointer at
/// tick `k`: those a wheel or touchpad axis frame sent then scrolls (the
/// router gives it to the scrolls under the pointer).
fn swiped<'a>(cx: &Cx<'a>, k: u32) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    let Some(i) = cx.tl.at(k) else {
        return out;
    };
    // The pointer as the swipe starts: the frame before its first input.
    let ptr = cx
        .tl
        .frames
        .get(i.saturating_sub(1))
        .and_then(|f| f.ptr)
        .or_else(|| cx.tl.frames.get(i).and_then(|f| f.ptr));
    let Some(p) = ptr else {
        return out;
    };
    for (key, r) in &cx.tl.screens[i] {
        let scrolls = cx
            .trace
            .nodes
            .get(*key)
            .is_some_and(|m| matches!(m.kind.as_str(), "scroll" | "list"));
        let [x, y, w, h] = r.visual;
        if scrolls
            && r.surface == p.s
            && p.at[0] >= x
            && p.at[0] < x + w
            && p.at[1] >= y
            && p.at[1] < y + h
        {
            out.insert(*key);
        }
    }
    out
}

/// True if `key`, first drawn on `surface` at timeline frame `first`,
/// was kept from view the frame before by an ancestor drawn there: one
/// that did not show (drawn transparent or at scale 0), or one that
/// clips what it holds (`clip: true`, a `scroll`, a `list`) or is the
/// surface's root, whose box did not reach where the node first shows (a
/// clip it was outside of, a row out of view). (eval-r3) An ancestor that
/// does not clip draws its children past its box, so being outside it
/// kept nothing from view.
pub(super) fn dropped_unseen(cx: &Cx, key: &str, first: usize, surface: u32) -> bool {
    let (Some(p), Some(me)) = (first.checked_sub(1), cx.tl.row(first, key)) else {
        return false;
    };
    let [x, y, w, h] = me.visual.map(f64::from);
    let parent = |k: &str| cx.trace.nodes.get(k).and_then(|m| m.parent.as_deref());
    let mut at = parent(key);
    while let Some(k) = at {
        if let Some(r) = cx.tl.row(p, k).filter(|r| r.surface == surface) {
            let [ax, ay, aw, ah] = r.visual.map(f64::from);
            // It clips, or it is its surface's root (no parent drawn on
            // that surface), whose surface ends at its box.
            let meta = cx.trace.nodes.get(k);
            let bounds = meta.is_some_and(|m| {
                m.clips
                    || m.parent
                        .as_deref()
                        .is_none_or(|pk| cx.tl.row(p, pk).is_none_or(|pr| pr.surface != surface))
            });
            let apart = bounds && (x + w <= ax || ax + aw <= x || y + h <= ay || ay + ah <= y);
            if !cx.tl.visible(p, r) || apart {
                return true;
            }
        }
        at = parent(k);
    }
    false
}

/// True if `pose` holds an axis the trace reads and each it holds is
/// where `rest` is: its opacity, no offset, its scale.
fn still_pose(pose: &crate::trace::Pose, rest: &crate::trace::Row, tol: f64) -> bool {
    let near = |a: Option<f32>, b: f32, give: f64| {
        a.is_none_or(|a| (f64::from(a) - f64::from(b)).abs() <= give)
    };
    let any =
        pose.opacity.is_some() || pose.x.is_some() || pose.y.is_some() || pose.scale.is_some();
    any && near(pose.opacity, rest.own_op, POSE_OPACITY_GIVE)
        && near(pose.x, 0.0, tol)
        && near(pose.y, 0.0, tol)
        && near(pose.scale, rest.scale[0], POSE_SCALE_GIVE)
}

/// How much more a pose shows than rest's may come back per frame.
pub const POSE_OPACITY_GIVE: f64 = 0.01;
pub const POSE_SCALE_GIVE: f64 = 0.005;

/// (integration-r7 part 2) How far a recorded pose's scale may come back
/// per frame in `pose back`: the trace records a pose's scale to
/// [`crate::trace::QUANTUM`] ([`crate::trace::q`]), so two frames of a
/// scale that only settles may read a whole quantum apart.
pub const POSE_BACK_SCALE_GIVE: f64 = if POSE_SCALE_GIVE > crate::trace::QUANTUM {
    POSE_SCALE_GIVE
} else {
    crate::trace::QUANTUM
};

/// `pose back`: from the first frame a surface paints after logic closed
/// it (integration-r7 part 2: its diff removed its root, or a node above
/// it, or set its `open` false, `RunTrace::closed`; never the renderer's
/// motion bits, so a renderer that drops the exit is still judged) to
/// the last it paints, the pose it commits never comes back toward rest
/// (a frame with no pose commits rest's) while it is short of its exit
/// pose: an exit's spatial spring that overshoots its pose settles back
/// onto it (design.md: `$motion.spatial`'s overshoot shows on scale as on
/// a move; decisions.md exit-overshoot), and only coming back past the
/// exit pose toward rest is a pose back. Unless (eval-rules-5) it is
/// opened again first: (integration-r7) logic's diff creates its root
/// again after the close began (`RunTrace::opened`).
fn pose_back(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut reopened = 0u64;
    let mut closings = 0u64;
    for sf in &cx.trace.surfaces {
        let mut closing = false;
        let mut since = 0u32;
        // Logic's closes up to here are spent (by a reopening after them).
        let mut spent: Option<u32> = None;
        let mut last: Option<(f64, f64)> = None;
        let opens = cx.trace.opened.get(&sf.root);
        let closes = cx.trace.closed.get(&sf.root);
        // How far from rest its exit pose is, on each axis it sets (its
        // root's `exit`, its `enter` mirrored when it has none).
        let exit = cx.trace.nodes.get(&sf.root).and_then(|m| m.exit.as_ref());
        let goal = (
            exit.and_then(|e| e.opacity).map(|o| 1.0 - f64::from(o)),
            exit.and_then(|e| e.scale)
                .map(|s| (1.0 - f64::from(s)).abs()),
        );
        for f in &cx.trace.frames {
            if !f.painted.contains(&sf.id) {
                continue;
            }
            // (eval-rules-5) Opened again while it closed: it is not
            // closing any more, and its pose comes back toward rest by
            // design. (integration-r7) Read from logic: its diff created
            // the surface's root again after the close began
            // (`RunTrace::opened`).
            // A root logic created again that a new surface took is that
            // surface's opening, not this one's.
            let elsewhere = |t: u32| {
                cx.trace
                    .surfaces
                    .iter()
                    .any(|o| o.id != sf.id && o.root == sf.root && o.attached >= t)
            };
            if closing
                && opens
                    .is_some_and(|o| o.iter().any(|t| *t > since && *t <= f.k && !elsewhere(*t)))
            {
                reopened += 1;
                closing = false;
                spent = Some(f.k);
                last = None;
            }
            // A close before this surface was attached closed an earlier
            // surface of the root (each opening of a popup is a new one).
            if !closing
                && closes.is_some_and(|c| {
                    c.iter()
                        .any(|t| *t >= sf.attached && spent.is_none_or(|s| *t > s) && *t <= f.k)
                })
            {
                closing = true;
                closings += 1;
                since = f.k;
            }
            if !closing {
                continue;
            }
            let p = f.poses.get(&sf.id);
            let away = (
                1.0 - f64::from(p.map_or(1.0, |p| p.opacity)),
                (1.0 - f64::from(p.map_or(1.0, |p| p.scale))).abs(),
            );
            // Back toward rest by more than the give, and short of the
            // exit pose by more than it.
            let short =
                |now: f64, goal: Option<f64>, give: f64| goal.is_none_or(|g| now + give < g);
            if let Some((o, sc)) = last {
                let what =
                    if away.0 + POSE_OPACITY_GIVE < o && short(away.0, goal.0, POSE_OPACITY_GIVE) {
                        Some(("opacity", "opacity", 1.0 - o, 1.0 - away.0))
                    } else if away.1 + POSE_BACK_SCALE_GIVE < sc
                        && short(away.1, goal.1, POSE_BACK_SCALE_GIVE)
                    {
                        Some(("scale", "scale's distance from 1", sc, away.1))
                    } else {
                        None
                    };
                if let Some((axis, what, was, got)) = what {
                    let mut b = Finding::fail(
                        CheckId::Vanish,
                        format!(
                            "pose back: closing, surface {} committed a pose back toward \
                             rest ({what} {was:.3} -> {got:.3})",
                            sf.id
                        ),
                    )
                    .at(f.k)
                    .node(&sf.root)
                    .axis(axis);
                    if let Some(seg) = cx
                        .trace
                        .segments
                        .iter()
                        .rev()
                        .find(|s| s.s.is_none_or(|s| s < f.k) && f.k <= s.e)
                    {
                        b = b.in_segment(&seg.name);
                    }
                    out.push(b);
                }
            }
            last = Some(away);
        }
    }
    super::stats(
        "vanish.pose-back",
        cx,
        &[("closings", closings), ("reopened", reopened)],
    );
    out
}

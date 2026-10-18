//! `vanish`: what shows does not blink.
//!
//! - **vanish**: shown at one frame, not at the next, shown again within
//!   three ticks, with no exit, enter or authored opacity or scale on it
//!   or an ancestor around it, and no step bringing it back (none acts
//!   between the frame it is gone in and the one it is back in);
//! - **flash**: shown at one frame only, between frames it does not
//!   show in, with no keyframes or time-driven motion, and no step
//!   taking it out (none acts between that frame and the next);
//! - **pop-in**: an element with an `enter` pose, mounted after boot,
//!   first shows near its pose: the pose's opacity and offset start its
//!   springs, and its first frame shown has covered at most half the way
//!   to rest (a pose skipped shows it at rest at once). It holds each
//!   time the element comes onto a surface: when it mounts, and when the
//!   surface it is on is attached anew (a popup opened again). A shared
//!   morph plays in place of the pose (design.md, "Motion and time";
//!   anim/morph.rs), so a first frame that morphs is no pop-in: where it
//!   starts is `coherence`'s morph start (decisions.md shared-morph-on);
//! - **blank**: a frame that damaged pixels and drew no item while
//!   something inside its surface's root shows (a popup's first frame at
//!   a pose's `opacity: 0`, or its last as its exit ends, shows nothing
//!   and draws nothing);
//! - **pose back**: with the compositor applying surface poses, a
//!   surface whose root plays its exit commits, frame after frame until
//!   it goes, a pose that never comes back toward rest (owner-testing.md
//!   bug 7: the frame that ended a closing popup's exit committed the
//!   rest pose, and the compositor faded the whole menu out from full
//!   opacity).

use std::collections::BTreeSet;

use strand_render::probe::{Axes, Motions};

use super::{Cx, Finding, first_per_node, motions};
use crate::scenario::CheckId;
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
    // there (its `s` is the tick before its first change).
    let stepped = |from: u32, to: u32| {
        cx.trace
            .segments
            .iter()
            .any(|s| s.s.is_some_and(|s| from <= s && s < to))
    };
    for key in keys {
        let vis: Vec<bool> = (0..tl.ks.len()).map(|i| tl.visible_key(i, key)).collect();
        for i in 1..vis.len() {
            let k = tl.ks[i];
            let seg = seg_of(k);
            let exempt = |c| seg.is_some_and(|(si, _)| cx.exempt(c, si, key, Axes::ALL));
            let name = seg.map(|(_, s)| s.name.as_str()).unwrap_or("");
            // vanish
            if vis[i - 1] && !vis[i] && !exempt(CheckId::Vanish) {
                let back = (i + 1..vis.len())
                    .take_while(|j| tl.ks[*j] <= k + BLINK_TICKS)
                    .find(|j| vis[*j])
                    .filter(|j| !stepped(k, tl.ks[*j]));
                if let Some(j) = back {
                    let around = [i - 1, i, j]
                        .iter()
                        .filter_map(|x| tl.row(*x, key))
                        .any(leaving);
                    if !around {
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
            if i + 1 < vis.len()
                && !vis[i - 1]
                && vis[i]
                && !vis[i + 1]
                && !stepped(k, tl.ks[i + 1])
                && !exempt(CheckId::Vanish)
            {
                let free = tl
                    .row(i, key)
                    .is_some_and(|r| motions(r).intersects(Motions::KEYFRAMES | Motions::TIME));
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
    // surface itself, whose own paint the trace cannot tell).
    let roots: BTreeSet<&str> = cx.trace.surfaces.iter().map(|s| s.root.as_str()).collect();
    for f in &cx.trace.frames {
        let Some(i) = tl.at(f.k) else { continue };
        for (s, c) in &f.cost {
            let shows = tl.screens[i]
                .values()
                .any(|r| r.surface == *s && !roots.contains(r.key.as_str()) && tl.visible(i, r));
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
/// faded it out). An exit cut short pops what is left of the node out in
/// one frame. Not held under reduced motion, where a pose snaps.
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
            if seg.reduced_motion
                || cx.trace.scenario.reduced_motion
                || cx.allowed(CheckId::Vanish, key)
                || f64::from(r.op) <= POP_OUT_OPACITY
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
        let rest = tl.at(seg.e).and_then(|e| tl.row(e, key)).unwrap_or(r);
        let mut wrong = Vec::new();
        if !Motions(r0.mot).contains(Motions::ENTER) {
            wrong.push("it mounted without playing its pose".to_string());
        }
        let share = |at: f64, from: f64, to: f64| -> Option<f64> {
            ((to - from).abs() > 0.05).then(|| (at - from) / (to - from))
        };
        if let Some(po) = pose.opacity
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
        (!wrong.is_empty()).then(|| {
            Finding::fail(CheckId::Vanish, format!("pop-in: {}", wrong.join("; ")))
                .at(tl.ks[shown])
                .node(key)
                .in_segment(&seg.name)
        })
    }
}

/// How much more a pose shows than rest's may come back per frame.
pub const POSE_OPACITY_GIVE: f64 = 0.01;
pub const POSE_SCALE_GIVE: f64 = 0.005;

/// `pose back`: from the first frame a surface's root plays its exit to
/// the last it paints, the pose it commits never comes back toward rest
/// (a frame with no pose commits rest's).
fn pose_back(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    for sf in &cx.trace.surfaces {
        let mut closing = false;
        let mut last: Option<(f64, f64)> = None;
        for f in &cx.trace.frames {
            if !f.painted.contains(&sf.id) {
                continue;
            }
            let root = f.n.iter().find(|r| r.surface == sf.id && r.key == sf.root);
            closing |= root.is_some_and(|r| Motions(r.mot).contains(Motions::EXIT));
            if !closing {
                continue;
            }
            let p = f.poses.get(&sf.id);
            let away = (
                1.0 - f64::from(p.map_or(1.0, |p| p.opacity)),
                (1.0 - f64::from(p.map_or(1.0, |p| p.scale))).abs(),
            );
            if let Some((o, sc)) = last {
                let what = if away.0 + POSE_OPACITY_GIVE < o {
                    Some(("opacity", "opacity", 1.0 - o, 1.0 - away.0))
                } else if away.1 + POSE_SCALE_GIVE < sc {
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
    out
}

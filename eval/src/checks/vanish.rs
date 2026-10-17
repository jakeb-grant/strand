//! `vanish`: what shows does not blink.
//!
//! - **vanish**: shown at one frame, not at the next, shown again within
//!   three ticks, with no exit, enter or authored opacity or scale on it
//!   or an ancestor around it;
//! - **flash**: shown at one frame only, between frames it does not
//!   show in, with no keyframes or time-driven motion;
//! - **pop-in**: an element with an `enter` pose, mounted after boot,
//!   first shows near its pose: the pose's opacity and offset start its
//!   springs, and its first frame shown has covered at most half the way
//!   to rest (a pose skipped shows it at rest at once);
//! - **blank**: a frame that damaged pixels and drew no item.

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
                    .find(|j| vis[*j]);
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
            if i + 1 < vis.len() && !vis[i - 1] && vis[i] && !vis[i + 1] && !exempt(CheckId::Vanish)
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
        // pop-in
        let Some(pose) = cx.trace.nodes.get(key).and_then(|m| m.enter) else {
            continue;
        };
        let Some(first) = (0..tl.ks.len()).find(|i| tl.row(*i, key).is_some()) else {
            continue;
        };
        let k0 = tl.ks[first];
        let Some((si, seg)) = seg_of(k0) else {
            continue;
        };
        if k0 <= boot_end || seg.reduced_motion || cx.exempt(CheckId::Vanish, si, key, Axes::ALL) {
            continue;
        }
        let Some(shown) = (first..tl.ks.len()).find(|i| vis[*i]) else {
            continue;
        };
        let (Some(r0), Some(r)) = (tl.row(first, key), tl.row(shown, key)) else {
            continue;
        };
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
        if !wrong.is_empty() {
            out.push(
                Finding::fail(CheckId::Vanish, format!("pop-in: {}", wrong.join("; ")))
                    .at(tl.ks[shown])
                    .node(key)
                    .in_segment(&seg.name),
            );
        }
    }
    // blank
    for f in &cx.trace.frames {
        for (s, c) in &f.cost {
            if c.damage_px > 0 && c.items == 0 {
                out.push(
                    Finding::fail(
                        CheckId::Vanish,
                        format!(
                            "blank: surface {s} damaged {} px and drew nothing",
                            c.damage_px
                        ),
                    )
                    .at(f.k),
                );
            }
        }
    }
    first_per_node(out)
}

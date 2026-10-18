//! `twin`: the scenario re-run with motion reduced from the start
//! (everything snaps) ends each settled segment as the run does: the
//! same nodes shown (by identity), their visual boxes on the output
//! within `tol` and their
//! opacity within 1/255. A stuck offset, or a layout left on an estimate
//! at rest, shows as a difference. Keyframed, leaning and jelly nodes are
//! left out: reduced motion drops loops and effects (design.md "Runtime
//! changes these need" 7; effects/lean.rs: "`reduced_motion` turns both
//! off"), so a tilt held toward the pointer at rest is the run's and not
//! the twin's by design. Time-driven nodes are compared: a run rests only
//! once nothing shown follows a running clock, so at a settled rest their
//! clocks stand at t = 0 in both runs (time.rs `TimeContext::frozen`), and
//! one that stopped elsewhere is a difference.
//!
//! (eval-coverage-2, integration-r4) What lies under a scroll that flung
//! ([`Motions::FLING`]) is left out from that frame on: under
//! `reduced_motion` a touchpad lift flings nothing (renderer/lists.rs
//! `scroll_input`), so that scroll's content rests at another offset by
//! design. The scroll's own box, and everything else on its surface, is
//! still compared.

use std::collections::{BTreeMap, BTreeSet};

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
    let free = Motions::KEYFRAMES | Motions::LEAN | Motions::JELLY;
    // (eval-false-alarms) Where each run's surfaces were first placed: a
    // row read on screen is moved by how far its surface has moved since
    // then, so its place on the output is that plus where the surface
    // started. Two runs can place a surface first at different places (a
    // content-sized OSD held at a larger size while its enter pose plays,
    // configured at its own size at once with motion reduced) and still
    // rest at one.
    let (fa, fb) = (cx.trace.first_origins(), twin.first_origins());
    // (eval-coverage-2, integration-r4) Where each scroll first flung,
    // by its key: from there on its content is not the twin's.
    let mut flung: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, screen) in cx.tl.screens.iter().enumerate() {
        for (k, r) in screen {
            if Motions(r.mot).intersects(Motions::FLING) {
                flung.entry(*k).or_insert(i);
            }
        }
    }
    let under_fling = |key: &str, ia: usize| -> bool {
        if flung.is_empty() {
            return false;
        }
        let mut at = key;
        for _ in 0..256 {
            let Some(p) = cx.parent(at) else {
                return false;
            };
            if flung.get(p).is_some_and(|&f| f <= ia) {
                return true;
            }
            if p == at {
                return false;
            }
            at = p;
        }
        false
    };
    let at = |first: &BTreeMap<u32, [f32; 2]>, r: &crate::trace::Row, i: usize| -> f64 {
        let o = first.get(&r.surface).copied().unwrap_or([0.0, 0.0]);
        f64::from(r.visual[i]) + if i < 2 { f64::from(o[i]) } else { 0.0 }
    };
    for (a, b) in cx.trace.segments.iter().zip(&twin.segments) {
        if !a.settled || !b.settled {
            continue;
        }
        let (Some(ia), Some(ib)) = (cx.tl.at(a.e), tw.at(b.e)) else {
            continue;
        };
        // A node left out in either run is left out of both (the twin
        // draws a leaning node upright, without LEAN).
        let freed: BTreeSet<String> = [(&cx.tl, ia), (&tw, ib)]
            .iter()
            .flat_map(|(tl, i)| {
                tl.screens[*i]
                    .iter()
                    .filter(|(_, r)| motions(r).intersects(free))
                    .map(|(k, _)| k.to_string())
            })
            .collect();
        // (eval-false-alarms) By identity: the element written at that
        // place, without the `~n` a node re-created while its
        // predecessor still exited is given (how many generations
        // overlapped is the motion's, not the rest's). Two shown at
        // rest with one identity are told apart by their order.
        let shown = |tl: &Timeline, i: usize| -> BTreeMap<String, String> {
            let mut by: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for (k, r) in &tl.screens[i] {
                if tl.visible(i, r) && !freed.contains(*k) && !under_fling(k, ia) {
                    by.entry(identity(k)).or_default().push(k);
                }
            }
            let mut out = BTreeMap::new();
            for (id, mut keys) in by {
                keys.sort();
                for (n, k) in keys.into_iter().enumerate() {
                    let name = if n == 0 {
                        id.clone()
                    } else {
                        format!("{id} ({})", n + 1)
                    };
                    out.insert(name, k.to_string());
                }
            }
            out
        };
        let (ma, mb) = (shown(&cx.tl, ia), shown(&tw, ib));
        let sa: BTreeSet<&String> = ma.keys().collect();
        let sb: BTreeSet<&String> = mb.keys().collect();
        for id in sa.difference(&sb) {
            let k = &ma[*id];
            out.push(
                Finding::fail(CheckId::Twin, "shown at rest, but not with motion reduced")
                    .at(a.e)
                    .node(k)
                    .in_segment(&a.name),
            );
        }
        for id in sb.difference(&sa) {
            let k = &mb[*id];
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
        for id in sa.intersection(&sb) {
            let k = &ma[*id];
            let (Some(ra), Some(rb)) = (cx.tl.row(ia, k), tw.row(ib, &mb[*id])) else {
                continue;
            };
            for (i, axis) in ["x", "y", "w", "h"].iter().enumerate() {
                let (va, vb) = (at(&fa, ra, i), at(&fb, rb, i));
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

/// (eval-false-alarms) A node's key without the `~n` instance suffixes
/// (labels.rs: a second live node with the same key gets `~2`), its own
/// and its parent's inside it (a node with no source is keyed under its
/// parent's key): the element instance it is, whichever generation.
pub fn identity(key: &str) -> String {
    let c: Vec<char> = key.chars().collect();
    let mut out = String::with_capacity(key.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] == '~' && i > 0 && c[i - 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < c.len() && c[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && (j == c.len() || c[j] == '>') {
                i = j;
                continue;
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

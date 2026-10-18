//! `twin`: the scenario re-run with motion reduced from the start
//! (everything snaps) ends each settled segment as the run does: the
//! same nodes shown (by key), their visual boxes within `tol` and their
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
//! (eval-coverage-2) A surface whose scroll flung ([`Motions::FLING`]) is
//! left out from that frame on: under `reduced_motion` a touchpad lift
//! flings nothing (renderer/lists.rs `scroll_input`), so its offsets, and
//! every rest after, are not the twin's by design.

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
    // (eval-coverage-2) Where the run first flung, per surface: from
    // there on its scroll offsets are not the twin's by design.
    let mut flung: BTreeMap<u32, usize> = BTreeMap::new();
    for (i, screen) in cx.tl.screens.iter().enumerate() {
        for r in screen.values() {
            if motions(r).intersects(Motions::FLING) {
                flung.entry(r.surface).or_insert(i);
            }
        }
    }
    for (a, b) in cx.trace.segments.iter().zip(&twin.segments) {
        if !a.settled || !b.settled {
            continue;
        }
        let (Some(ia), Some(ib)) = (cx.tl.at(a.e), tw.at(b.e)) else {
            continue;
        };
        let left_out = |r: &crate::trace::Row| flung.get(&r.surface).is_some_and(|&f| f <= ia);
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
        let shown = |tl: &Timeline, i: usize| -> BTreeSet<String> {
            tl.screens[i]
                .iter()
                .filter(|(k, r)| tl.visible(i, r) && !freed.contains(**k) && !left_out(r))
                .map(|(k, _)| k.to_string())
                .collect()
        };
        let (sa, sb) = (shown(&cx.tl, ia), shown(&tw, ib));
        // (eval-coverage-2) A node made again while the one it replaces
        // still played its exit took the key with `~2` (labels.rs: a
        // second live node with one key); with motion reduced the exit
        // was over at once and it took the plain key. At a settled rest
        // the two are the same node: each key shown alone under its base
        // in both runs is compared by it.
        let (ma, mb) = (by_base(&sa), by_base(&sb));
        let sa: BTreeSet<String> = ma.keys().cloned().collect();
        let sb: BTreeSet<String> = mb.keys().cloned().collect();
        for k in sa.difference(&sb) {
            let k = &ma[k];
            out.push(
                Finding::fail(CheckId::Twin, "shown at rest, but not with motion reduced")
                    .at(a.e)
                    .node(k)
                    .in_segment(&a.name),
            );
        }
        for k in sb.difference(&sa) {
            let k = &mb[k];
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
        for k in sa.intersection(&sb) {
            let (ka, kb) = (&ma[k], &mb[k]);
            let (Some(ra), Some(rb)) = (cx.tl.row(ia, ka), tw.row(ib, kb)) else {
                continue;
            };
            let k = ka;
            for (i, axis) in ["x", "y", "w", "h"].iter().enumerate() {
                let (va, vb) = (f64::from(ra.visual[i]), f64::from(rb.visual[i]));
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

/// A node key without the `~n` a second live node with the same key
/// takes (labels.rs).
fn base(k: &str) -> &str {
    match k.rsplit_once('~') {
        Some((b, n)) if !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) => b,
        _ => k,
    }
}

/// The keys shown, by the name they are compared under: a key's base
/// when no other key shown has that base, else the key itself.
fn by_base(shown: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for k in shown {
        *count.entry(base(k)).or_default() += 1;
    }
    shown
        .iter()
        .map(|k| {
            let b = base(k);
            let name = if count[b] == 1 { b } else { k.as_str() };
            (name.to_string(), k.clone())
        })
        .collect()
}

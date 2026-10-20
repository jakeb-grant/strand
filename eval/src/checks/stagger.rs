//! (eval-rules) `stagger`: children entering together under `stagger:`
//! start their enter one step apart, in child order.
//!
//! design.md ("Motion and time": `stagger: 30ms` on a container) and the
//! schema ("Delays each child's `enter` this much after the one before"):
//! the children that mount together under a container with a stagger are
//! numbered in child order, the first starting at once and child `n`
//! holding at its enter pose for `n` steps (anim/stagger.rs; a step is
//! capped at one second there). Its start is the first frame it no
//! longer waits for its turn (`probe::Motions::STAGGER` cleared, as
//! anim/stagger.rs `held` lets it go), and what is drawn must agree: it
//! shows nothing of its enter before its turn (no frame off its pose
//! before it is due). The start is not read off the first frame that
//! shows it moving: an enter on a curve that starts slowly (`ease(in,
//! …)`, `in_back`, `emphasized_accelerate`, a `bezier`) stays within a
//! quantum of its pose for frames after it has begun.
//!
//! The batch is the container's children first drawn at one frame after
//! boot (with an `enter` pose or not: every child mounting counts in the
//! numbering), and (eval-coverage-4) those drawn again there after frames
//! without them (a result filtered out and typed back mounts anew;
//! integration-r7: read from what logic declared, a row a `list`'s
//! window brings back at rest does not), in paint order. Child `n`'s start is due `n` steps after
//! the batch's change: the tick before the batch's first frame, since
//! its first frame shows the first child one frame on (a retarget starts
//! one period before the frame that shows it, strand-scene motion.rs).
//! It is let go on the first frame at or after that, so its start is
//! held to lie between the moment due and one period after it: within a
//! frame; and no frame before the moment due shows it off its pose. A child whose text is still on its way waits for it
//! instead (decisions.md late-text-dip) and is not held, nor is one
//! without a pose to hold at, one whose pose was dropped unseen (kept
//! from view by an ancestor until its first frame, it shows at rest and
//! plays no enter, as `vanish` reads it: eval-false-alarms), or a run
//! under reduced motion (every child shows at once).

use std::collections::BTreeMap;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{Pose, Row};
use strand_render::probe::Motions;

/// The longest step render plays (anim/stagger.rs `MAX_STEP`).
pub const MAX_STEP_MS: f64 = 1000.0;
/// Slack on each side, for whole-nanosecond tick times.
const SLACK_NS: f64 = 1.0e6;

/// The row is off its pose: by an opacity quantum, 1/64 px of offset or
/// a scale step of 1/1000.
fn off_pose(r: &Row, p: &Pose) -> Option<bool> {
    let mut any = false;
    let mut off = false;
    if let Some(o) = p.opacity {
        any = true;
        off |= (f64::from(r.own_op) - f64::from(o)).abs() > 1.0 / 255.0;
    }
    for (v, at) in [
        (p.x, r.written[0] + r.spring[0]),
        (p.y, r.written[1] + r.spring[1]),
    ] {
        if let Some(v) = v {
            any = true;
            off |= (f64::from(at) - f64::from(v)).abs() > 1.0 / 64.0;
        }
    }
    if let Some(s) = p.scale {
        any = true;
        off |= (f64::from(r.scale[0].max(r.scale[1])) - f64::from(s)).abs() > 0.001;
    }
    any.then_some(off)
}

pub fn run(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let trace = cx.trace;
    let mut out = Vec::new();
    let boot_end = trace.segments.first().map_or(0, |s| s.e);
    for (parent, meta) in &trace.nodes {
        let Some(step) = meta.stagger else { continue };
        let step_ns = f64::from(step).min(MAX_STEP_MS) * 1.0e6;
        // Its children, by the timeline frame each mounted in: first
        // drawn, or (eval-coverage-4) drawn again after frames without it
        // (a result filtered out and typed back), which enters with the
        // children mounting beside it. (integration-r7) Whether a child
        // drawn again mounted anew is read from what logic declared,
        // never from the renderer's motion bits: spec.md 6.6 gives a pose
        // to every `if` branch, `for` copy, list item and page that
        // mounts, and only a row a `list`'s window mounts shows at rest
        // (logic's `Create { window: true }`, `RunTrace::windowed`;
        // decisions.md m4-lists-w1), so such a row is in no batch, the
        // first time it is drawn or again; a
        // remount that skips its enter fails its turn instead of leaving
        // the numbering.
        let mut batches: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
        for (key, m) in &trace.nodes {
            if m.parent.as_deref() != Some(parent.as_str()) {
                continue;
            }
            let windows = trace.windowed.get(key);
            let mut was = false;
            let mut gone_at = 0u32;
            for i in 0..tl.ks.len() {
                let row = tl.row(i, key);
                let by_window =
                    windows.is_some_and(|w| w.iter().any(|t| *t >= gone_at && *t <= tl.ks[i]));
                if row.is_some() && !was && !by_window {
                    batches.entry(i).or_default().push(key.as_str());
                }
                if row.is_none() && was {
                    gone_at = tl.ks[i];
                }
                was = row.is_some();
            }
        }
        for (i0, mut batch) in batches {
            let k0 = tl.ks[i0];
            let Some((_, seg)) = trace
                .segments
                .iter()
                .enumerate()
                .rev()
                .find(|(_, s)| s.s.is_none_or(|s| s < k0) && k0 <= s.e)
            else {
                continue;
            };
            if k0 <= boot_end || seg.reduced_motion {
                continue;
            }
            // Child order: paint order in the frame they were first drawn.
            let order = |k: &str| {
                tl.frames[i0]
                    .n
                    .iter()
                    .position(|r| r.key == k)
                    .unwrap_or(usize::MAX)
            };
            batch.sort_by_key(|k| order(k));
            let period_ns = 1.0e9 / f64::from(seg.hz.max(1));
            let t0 = tl.frames[i0].t_ns as f64 - period_ns;
            for (n, key) in batch.iter().enumerate() {
                let Some(pose) = trace.nodes.get(*key).and_then(|m| m.enter) else {
                    continue;
                };
                if cx.allowed(CheckId::Stagger, key) {
                    continue;
                }
                // (eval-false-alarms) Its pose dropped unseen: kept from
                // view by an ancestor until its first frame, it shows at
                // rest by design and plays no enter, so it has no turn to
                // wait for (`vanish`'s own reading of that frame,
                // decisions.md wave3-pixels (p2)).
                if let Some(r0) = tl.row(i0, key)
                    && !Motions(r0.mot).intersects(Motions::ENTER | Motions::STAGGER)
                    && super::vanish::dropped_unseen(cx, key, i0, r0.surface)
                {
                    continue;
                }
                // Within the segment: the first frame it no longer waits
                // for its turn, and the first that shows it off its pose.
                let mut released = None;
                let mut departed = None;
                let mut waits_for_text = false;
                for i in i0..tl.ks.len() {
                    if tl.ks[i] > seg.e {
                        break;
                    }
                    let Some(r) = tl.row(i, key) else { break };
                    waits_for_text |= !tl.frames[i].held_text.is_empty()
                        || tl.screens[i].iter().any(|(k, row)| {
                            matches!(row.text.as_deref(), Some("E" | "I"))
                                && cx.lineage(k).contains(key)
                        });
                    if released.is_none() && !Motions(r.mot).contains(Motions::STAGGER) {
                        released = Some(i);
                    }
                    if departed.is_none() {
                        match off_pose(r, &pose) {
                            None => break,
                            Some(true) => departed = Some(i),
                            Some(false) => {}
                        }
                    }
                    if released.is_some() && departed.is_some() {
                        break;
                    }
                }
                // One let go that never left its pose within the segment
                // showed no start to judge.
                let Some(d) = departed else { continue };
                if waits_for_text {
                    continue;
                }
                let due = t0 + n as f64 * step_ns;
                // Its start: the frame it was let go of its turn, unless
                // it was drawn moving before that.
                let j = released.map_or(d, |r| r.min(d));
                let at = tl.frames[j].t_ns as f64;
                let late = at - due;
                if late < -SLACK_NS || late > period_ns + SLACK_NS {
                    let ms = |v: f64| v / 1.0e6;
                    out.push(
                        Finding::fail(
                            CheckId::Stagger,
                            format!(
                                "start: child {} of {} entering together under {} ({step} ms \
                                 apart) left its pose {:.1} ms after their change, due at {:.1} \
                                 ms (within a frame)",
                                n + 1,
                                batch.len(),
                                trace
                                    .nodes
                                    .get(parent)
                                    .map_or(parent.as_str(), |m| m.label.as_str()),
                                ms(at - t0),
                                ms(due - t0),
                            ),
                        )
                        .at(tl.ks[j])
                        .node(key)
                        .got(ms(at - t0), [ms(due - t0), ms(due - t0 + period_ns)])
                        .in_segment(&seg.name),
                    );
                }
            }
        }
    }
    first_per_node(out)
}

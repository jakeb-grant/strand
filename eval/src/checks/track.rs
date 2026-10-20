//! (eval-rules-8) What time drives, held to what was declared:
//!
//! - `track` (in `continuity`): a `keyframes` block played on a node
//!   draws its track. design.md "Motion and time" (`keyframes shake { … }`
//!   with `play shake`) and spec.md 6.6 (`keyframes name { 0%, 100% { … };
//!   50% { … }; duration: T }`, "declares a timed animation") give a
//!   track as CSS keyframes do: at each stop's fraction of a run the prop
//!   is that stop's value, between two stops that set it it moves from one
//!   to the other along the block's easing (applied per segment, as CSS's
//!   `animation-timing-function` is), and a prop the first (or last) stop
//!   leaves out starts (or ends) at the node's own value. The block
//!   starts on the first painted frame that draws it, waits its `delay`,
//!   plays `repeat` runs of `duration` (every other one backwards with
//!   `alternate`), and then draws the node as it is. `x`, `y` and
//!   `rotate` add to the node's own value and `opacity` and `scale`
//!   multiply it (architecture.md: "Keyframes are offsets composed with
//!   the node's springs"). The expected value is worked out here from the
//!   block as logic declared it (`RunTrace::keyframes`), with an easing
//!   of the check's own (a cubic bézier solved by bisection, never
//!   `strand_scene::motion::ease`), and compared with what the frame drew:
//!   on `x` and `y` the node's drawn offset less its other parts (its
//!   ancestors', written, spring, glide and morph offsets), on `opacity`
//!   its own opacity, on `scale` and `rotate` its own. The node's own
//!   value under a multiplying or turning prop is fitted (the median over
//!   the play's frames), so only the track's shape is judged there. A
//!   block on an elastic or bounce easing is not judged (their constants
//!   are the product's), nor a play once motion was reduced in it (the
//!   `still` arm holds those frames), nor a frame where something else
//!   moves the prop (its own spring of it, an enter or exit, a morph, a
//!   lean, a jelly, a drag, a time signal).
//! - `still` (in `continuity` for the run's own reduced frames, in `twin`
//!   for its reduced-motion re-run): "`reduced_motion` turns off loops,
//!   time signals and effects; springs snap" (design.md "Runtime changes
//!   these need" 7), and spec.md 6.6: "with the desktop's reduced-motion
//!   setting on, every transition snaps and loops, time signals and
//!   effects stop". So a node time drives as declared (a `play`, a prop
//!   that reads `t`, `wave(…)` or `noise(…)`) is drawn the same on every
//!   frame motion is reduced at, unless something was driven since the
//!   frame before (logic's diff, input: a change snaps there) or its
//!   surface lays out anew by itself (a text landing, an exit's ghost):
//!   its offset from its laid box, its laid size, its own opacity, scale
//!   and turn, and the colours and shadows it paints. Held during waits,
//!   not only at a settled rest, which is all `twin` compared before
//!   (held-out set J's looping pulse went on through a reduced `wait`).

use std::collections::BTreeMap;

use strand_render::probe::Motions;

use super::{Cx, Finding};
use crate::scenario::CheckId;
use crate::trace::{EaseRec, Frame, KeyframesRec, QUANTUM, Row, RunTrace};

/// The props `track` reads, and how each meets the node's own value.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Compose {
    /// `x`, `y`: the keyframe offset is read off the drawn box exactly.
    Offset,
    /// `rotate`: adds to the node's own (fitted).
    Add,
    /// `opacity`, `scale`: multiplies the node's own (fitted).
    Multiply,
}

const PROPS: [(&str, Compose); 5] = [
    ("x", Compose::Offset),
    ("y", Compose::Offset),
    ("rotate", Compose::Add),
    ("opacity", Compose::Multiply),
    ("scale", Compose::Multiply),
];

/// A frame closer than this to a run's boundary (or the play's end) may
/// be read on either side of it.
const EDGE_NS: u64 = 1_000;

/// CSS `cubic-bezier(x1, y1, x2, y2)` at `u` in `0..=1`: the curve's
/// `x(t) = u` solved by bisection, then `y(t)`.
pub fn bezier(c: [f32; 4], u: f64) -> f64 {
    let [x1, y1, x2, y2] = c.map(f64::from);
    let u = u.clamp(0.0, 1.0);
    let at = |a: f64, b: f64, t: f64| {
        let m = 1.0 - t;
        3.0 * m * m * t * a + 3.0 * m * t * t * b + t * t * t
    };
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if at(x1, x2, mid) < u {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    at(y1, y2, 0.5 * (lo + hi))
}

/// The share of a segment's way at `u` along `ease`; `None` where the
/// check has no easing of its own (elastic, bounce).
pub fn eased(ease: EaseRec, u: f64) -> Option<f64> {
    match ease {
        EaseRec::Linear => Some(u.clamp(0.0, 1.0)),
        EaseRec::Bezier(c) => Some(bezier(c, u)),
        EaseRec::Elastic | EaseRec::Bounce => None,
    }
}

/// The value the block gives `prop` at progress `p` of a run (the
/// identity where its stops leave it to the node's own value).
pub fn value_at(k: &KeyframesRec, prop: &str, p: f64, identity: f64) -> Option<f64> {
    let mut before: (f64, Option<f64>) = (0.0, None);
    let mut after: (f64, Option<f64>) = (1.0, None);
    for (at, set) in &k.stops {
        let Some(v) = set.get(prop) else { continue };
        let at = f64::from(*at);
        if at <= p {
            before = (at, Some(f64::from(*v)));
        } else {
            after = (at, Some(f64::from(*v)));
            break;
        }
    }
    let (a, b) = (before.1.unwrap_or(identity), after.1.unwrap_or(identity));
    let span = after.0 - before.0;
    let u = if span > 0.0 {
        eased(k.ease, ((p - before.0) / span).clamp(0.0, 1.0))?
    } else {
        1.0
    };
    Some(a + (b - a) * u)
}

/// Where a play started `t0` stands at `t` (ns): its progress through
/// the run it is in, or `None` while it waits its delay and once it has
/// played (it draws the node as it is). Near a boundary both readings.
pub fn progress(k: &KeyframesRec, t0: u64, t: u64) -> Vec<Option<f64>> {
    let mut out = Vec::new();
    for t in [t.saturating_sub(EDGE_NS), t, t + EDGE_NS] {
        let p = progress_one(k, t0, t);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn progress_one(k: &KeyframesRec, t0: u64, t: u64) -> Option<f64> {
    let ran = t.saturating_sub(t0);
    if ran < k.delay || k.d == 0 {
        return None;
    }
    let ran = ran - k.delay;
    if let Some(n) = k.repeat
        && ran >= k.d.saturating_mul(u64::from(n))
    {
        return None;
    }
    let runs = ran as f64 / k.d as f64;
    let run = runs.floor();
    let mut p = runs - run;
    if k.alternate && (run as u64) % 2 == 1 {
        p = 1.0 - p;
    }
    Some(p.clamp(0.0, 1.0))
}

/// What moves `prop` on `r` this frame besides its play: such a frame
/// is not judged. (The renderer marks a playing node `TIME` too, so a
/// time signal is read from what logic declared, `TimedRec::t`.)
fn moved_otherwise(r: &Row, c: Compose, reads_time: bool) -> bool {
    let own = Motions(r.mot);
    let all = Motions(r.emot);
    let carried = Motions::LEAN | Motions::JELLY | Motions::LIFT | Motions::SHARED_MORPH;
    if reads_time || r.hidden.is_some() {
        return true;
    }
    match c {
        Compose::Offset => all.intersects(carried),
        Compose::Add | Compose::Multiply => own.intersects(
            carried
                | Motions::PROP_SPRING
                | Motions::OPACITY_SPRING
                | Motions::ENTER
                | Motions::EXIT,
        ),
    }
}

/// What `prop` reads on `r`: the keyframe offset on `x` and `y`, the
/// node's own opacity, scale or turn.
fn observed(r: &Row, prop: &str) -> f64 {
    let off = |i: usize| {
        f64::from(r.drawn[i])
            - f64::from(r.laid[i])
            - f64::from(r.inherited[i])
            - f64::from(r.written[i])
            - f64::from(r.spring[i])
            - f64::from(r.glide[i])
            - f64::from(r.morph[i])
    };
    match prop {
        "x" => off(0),
        "y" => off(1),
        "opacity" => f64::from(r.own_op),
        "scale" => f64::from(r.scale[0].max(r.scale[1])),
        _ => f64::from(r.rotate),
    }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

/// One play of a block on a node: its frames from its start.
struct Play<'a> {
    key: &'a str,
    name: &'a str,
    seq: u32,
    t0: u64,
    frames: Vec<(&'a Frame, &'a Row)>,
}

/// The run's plays, from the frames' declared `play`s: a play is one
/// node's block and `seq`, from the first painted frame that draws it.
fn plays(trace: &RunTrace) -> Vec<Play<'_>> {
    let mut open: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out: Vec<Play> = Vec::new();
    for f in &trace.frames {
        for (key, t) in &f.timed {
            let Some((name, seq)) = &t.play else { continue };
            let Some(row) = f.n.iter().find(|r| r.key == *key) else {
                continue;
            };
            let same = open
                .get(key.as_str())
                .is_some_and(|&i| out[i].name == name && out[i].seq == *seq);
            if !same {
                open.insert(key.as_str(), out.len());
                out.push(Play {
                    key,
                    name,
                    seq: *seq,
                    t0: f.t_ns,
                    frames: Vec::new(),
                });
            }
            if let Some(&i) = open.get(key.as_str()) {
                out[i].frames.push((f, row));
            }
        }
    }
    out
}

/// `track`: each play draws its block's track (see the module's docs).
pub fn track(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let (mut judged_plays, mut judged_frames) = (0u64, 0u64);
    let seg_name = |k: u32| {
        cx.trace
            .segments
            .iter()
            .rev()
            .find(|s| s.s.is_none_or(|s| s < k) && k <= s.e)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    };
    for play in plays(cx.trace) {
        let Some(block) = cx.trace.keyframes.get(play.name) else {
            continue;
        };
        if matches!(block.ease, EaseRec::Elastic | EaseRec::Bounce) {
            continue;
        }
        if cx.allowed(CheckId::Continuity, play.key) {
            continue;
        }
        // Up to the first frame with motion reduced: from there `still`
        // holds it.
        let frames: Vec<_> = play.frames.iter().take_while(|(f, _)| !f.reduced).collect();
        let mut judged_any = false;
        for (prop, c) in PROPS {
            if !block.stops.iter().any(|(_, s)| s.contains_key(prop)) {
                continue;
            }
            let identity = if c == Compose::Multiply { 1.0 } else { 0.0 };
            // (frame, row, the readings it may have, what it drew)
            let mut rows: Vec<(&Frame, &Row, Vec<f64>, f64)> = Vec::new();
            for (f, r) in &frames {
                let reads_time = f.timed.get(play.key).is_some_and(|t| t.t);
                if moved_otherwise(r, c, reads_time) {
                    continue;
                }
                let want: Option<Vec<f64>> = progress(block, play.t0, f.t_ns)
                    .into_iter()
                    .map(|p| match p {
                        None => Some(identity),
                        Some(p) => value_at(block, prop, p, identity),
                    })
                    .collect();
                let Some(want) = want else { continue };
                rows.push((f, r, want, observed(r, prop)));
            }
            if rows.len() < 2 {
                continue;
            }
            // The node's own value: exact on `x` and `y` (read off), else
            // the one that best explains the frames.
            let base = match c {
                Compose::Offset => 0.0,
                Compose::Add => {
                    median(rows.iter().map(|(_, _, w, o)| o - w[0]).collect()).unwrap_or(0.0)
                }
                Compose::Multiply => median(
                    rows.iter()
                        .filter(|(_, _, w, _)| w[0].abs() > 0.05)
                        .map(|(_, _, w, o)| o / w[0])
                        .collect(),
                )
                .unwrap_or(1.0),
            };
            let give = match prop {
                "x" | "y" => cx.tol + 4.0 * QUANTUM,
                "rotate" => 0.5,
                "opacity" => 2.5 / 255.0,
                _ => 0.005,
            };
            judged_any = true;
            for (f, r, want, got) in &rows {
                judged_frames += 1;
                let drawn_as = |w: f64| match c {
                    Compose::Offset => w,
                    Compose::Add => base + w,
                    Compose::Multiply => {
                        let v = base * w;
                        if prop == "opacity" {
                            v.clamp(0.0, 1.0)
                        } else {
                            v.max(0.0)
                        }
                    }
                };
                let near = want
                    .iter()
                    .map(|w| drawn_as(*w))
                    .min_by(|a, b| (a - got).abs().total_cmp(&(b - got).abs()))
                    .unwrap_or(identity);
                if (got - near).abs() > give {
                    let at = progress(block, play.t0, f.t_ns)
                        .into_iter()
                        .flatten()
                        .next()
                        .map_or("before or after its runs".to_string(), |p| {
                            format!("{p:.2} of its run")
                        });
                    let what = match c {
                        Compose::Offset => format!("{prop} offset"),
                        _ => prop.to_string(),
                    };
                    out.push(
                        Finding::fail(
                            CheckId::Continuity,
                            format!(
                                "track: playing `{}` ({at}, {} ms from its start), its {what} \
                                 is drawn {got:.3}, where its stops and easing put it at \
                                 {near:.3} (keyframes pass through their stops at their times \
                                 and move between them along their easing; spec.md 6.6, as CSS \
                                 keyframes)",
                                play.name,
                                (f.t_ns - play.t0) / 1_000_000,
                            ),
                        )
                        .at(f.k)
                        .node(&r.key)
                        .axis(prop)
                        .got(*got, [near - give, near + give])
                        .in_segment(&seg_name(f.k)),
                    );
                }
            }
        }
        if judged_any {
            judged_plays += 1;
        }
    }
    super::stats(
        "continuity.track",
        cx,
        &[("plays", judged_plays), ("frames", judged_frames)],
    );
    out
}

/// `still`: under reduced motion what time drives holds still (see the
/// module's docs). `check` names who reports it: `continuity` for the
/// run's own reduced frames, `twin` for its reduced-motion re-run.
pub fn still(trace: &RunTrace, check: CheckId) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let (mut judged_nodes, mut judged_frames) = (0u64, 0u64);
    let seg_name = |k: u32| {
        trace
            .segments
            .iter()
            .rev()
            .find(|s| s.s.is_none_or(|s| s < k) && k <= s.e)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    };
    // What lays a surface out anew by itself (its own causes, not a
    // play's or a time signal's, which are what is judged here).
    let causes = Motions::EXIT | Motions::SIZE_SPRING | Motions::LIFT;
    let unsettled = |f: &Frame, s: u32| {
        f.n.iter().any(|r| {
            r.surface == s
                && (Motions(r.mot).intersects(causes)
                    || r.hidden.as_deref() == Some("inert")
                    || matches!(r.text.as_deref(), Some("E" | "I")))
        })
    };
    fn texts(f: &Frame, s: u32) -> Vec<(&str, Option<&str>)> {
        f.n.iter()
            .filter(|r| r.surface == s)
            .map(|r| (r.key.as_str(), r.text.as_deref()))
            .collect()
    }
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut told: std::collections::BTreeMap<(&str, String), usize> = BTreeMap::new();
    for (i, f) in trace.frames.iter().enumerate() {
        if !f.reduced {
            continue;
        }
        for key in f.timed.keys() {
            let Some(r) = f.n.iter().find(|r| r.key == *key) else {
                continue;
            };
            // The last frame before that drew it.
            let Some((j, p)) = trace.frames[..i]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, g)| g.n.iter().any(|q| q.key == *key))
            else {
                continue;
            };
            let Some(q) = p.n.iter().find(|q| q.key == *key) else {
                continue;
            };
            // Both frames reduced, nothing driven since, nothing laying
            // its surface out anew, its surface its size, its texts as
            // they were.
            if !p.reduced
                || trace.frames[j + 1..=i]
                    .iter()
                    .any(|g| g.driven || !g.reduced)
                || unsettled(f, r.surface)
                || unsettled(p, r.surface)
                || f.sizes.get(&r.surface) != p.sizes.get(&r.surface)
                || texts(f, r.surface) != texts(p, r.surface)
                || r.surface != q.surface
            {
                continue;
            }
            judged_frames += 1;
            if seen.insert(key.as_str()) {
                judged_nodes += 1;
            }
            let geo = 2.0 * QUANTUM;
            let mut moved: Vec<(&str, f64, f64)> = Vec::new();
            for (n, axis) in ["x", "y"].iter().enumerate() {
                let (a, b) = (
                    f64::from(r.drawn[n] - r.laid[n]),
                    f64::from(q.drawn[n] - q.laid[n]),
                );
                if (a - b).abs() > geo {
                    moved.push((axis, a, b));
                }
            }
            for (n, axis) in ["w", "h"].iter().enumerate() {
                let (a, b) = (f64::from(r.laid[n + 2]), f64::from(q.laid[n + 2]));
                if (a - b).abs() > geo {
                    moved.push((axis, a, b));
                }
            }
            for (axis, a, b, give) in [
                ("opacity", r.own_op, q.own_op, 1.0 / 255.0),
                (
                    "scale",
                    r.scale[0].max(r.scale[1]),
                    q.scale[0].max(q.scale[1]),
                    1e-3,
                ),
                ("rotate", r.rotate, q.rotate, 0.1),
            ] {
                if f64::from((a - b).abs()) > give {
                    moved.push((axis, f64::from(a), f64::from(b)));
                }
            }
            let paint_of = |row: &Row| {
                row.paint
                    .as_ref()
                    .map(|p| (p.bg.clone(), p.bd.clone(), p.sh.clone(), p.fg.clone()))
            };
            let paint_moved = paint_of(r) != paint_of(q);
            if moved.is_empty() && !paint_moved {
                continue;
            }
            let what = match f.timed.get(key).and_then(|t| t.play.as_ref()) {
                Some((name, _)) => format!("its `play {name}`"),
                None => "a prop that reads time".to_string(),
            };
            let (axis, msg, got) = match moved.first() {
                Some((axis, a, b)) => (
                    axis.to_string(),
                    format!(
                        "{axis} {a:.3}, where the frame before (k{}) drew {b:.3}",
                        p.k
                    ),
                    Some((*a, *b)),
                ),
                None => (
                    "paint".to_string(),
                    format!("its paint changed since the frame before (k{})", p.k),
                    None,
                ),
            };
            let seg = seg_name(f.k);
            if let Some(&n) = told.get(&(key.as_str(), seg.clone())) {
                out[n].more += 1;
                continue;
            }
            let mut finding = Finding::fail(
                check,
                format!(
                    "still: with motion reduced, {what} still moves it: {msg}, with nothing \
                     driven between (design.md: reduced_motion turns off loops, time signals \
                     and effects)"
                ),
            )
            .at(f.k)
            .node(key)
            .axis(&axis)
            .in_segment(&seg);
            if let Some((a, b)) = got {
                finding = finding.got(a, [b, b]);
            }
            told.insert((key.as_str(), seg), out.len());
            out.push(finding);
        }
    }
    super::stats_of(
        &format!("{}.still", check.name()),
        &trace.scenario.id,
        &[("nodes", judged_nodes), ("frames", judged_frames)],
    );
    out
}

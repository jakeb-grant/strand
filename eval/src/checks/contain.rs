//! (eval-rules) `contain`: what layout puts on a surface stays where the
//! surface shows it.
//!
//! - **cut**: a surface sized by its content (a `popup` or `panel` with
//!   no `width`, or no `height`) holds it. It grows to its content at
//!   once and, while its content springs smaller, is held at the larger
//!   size until it settles (decisions.md popup-shrink, eval-coverage), so
//!   nothing it holds is drawn past its edge on that axis, mid-motion
//!   included. A spring's overshoot past both where it started and where
//!   it rests is the one exception: the surface takes its content's
//!   resting size, never the overshoot (popup-height-spring's card). Not
//!   held for a node moved by authored motion on that axis (an offset, a
//!   pose, a scale or a turn: an `enter: slide(top)` leaves the surface on
//!   purpose) or held inside a clipping ancestor (`clip: true`, a
//!   `scroll`, a `list`).
//! - **give way**: an ellipsised text gives way before it is cut
//!   (design.md "Layout": in a full row children give way by `shrink`;
//!   decisions.md wave3-pixels "text in its box": "Text with `ellipsis`
//!   or `max_lines` may be narrower than its text (min-content 0)", and
//!   "content never widens" a surface root). A text that cuts itself
//!   with an ellipsis, drawn whole, is never laid past its surface's
//!   edge unless something that cannot give way puts it there: the text
//!   itself or an ancestor laid past the edge with a width of its own,
//!   `shrink: 0` or a `min_width` (`labels::rigid_of`), something else
//!   rigid inside the outermost such ancestor laid past the edge too, or
//!   a clipping ancestor (a `scroll` lays its content wider on purpose).

use strand_render::probe::Axes;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{Row, RunTrace};

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = cut(cx);
    out.extend(give_way(cx));
    first_per_node(out)
}

/// The segment tick `k` falls in.
fn segment_of(trace: &RunTrace, k: u32) -> Option<(usize, &crate::trace::Segment)> {
    trace
        .segments
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.s.is_none_or(|s| s < k) && k <= s.e)
}

/// A row's box on its surface (`visual` or `laid`), wherever the surface
/// has moved to on the output.
fn on_surface(cx: &Cx, i: usize, r: &Row, b: [f32; 4]) -> [f64; 4] {
    let [dx, dy] = cx
        .tl
        .frames
        .get(i)
        .map_or([0.0, 0.0], |f| RunTrace::moved(&cx.tl.first, f, r.surface));
    [
        f64::from(b[0] - dx),
        f64::from(b[1] - dy),
        f64::from(b[2]),
        f64::from(b[3]),
    ]
}

/// Some ancestor of `key` (itself left out) clips what it holds.
fn clipped(cx: &Cx, key: &str) -> bool {
    cx.lineage(key)
        .iter()
        .skip(1)
        .any(|k| cx.trace.nodes.get(*k).is_some_and(|m| m.clips))
}

fn cut(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let trace = cx.trace;
    let mut out = Vec::new();
    for sf in &trace.surfaces {
        for (axis, sized) in sf.content.iter().enumerate() {
            if !*sized {
                continue;
            }
            let moves = [Axes::X, Axes::Y][axis] | Axes::SCALE | Axes::ROTATE;
            let name = ["x", "y"][axis];
            for i in 0..tl.ks.len() {
                let Some(size) = tl.size(i, sf.id) else {
                    continue;
                };
                let k = tl.ks[i];
                let Some((_, seg)) = segment_of(trace, k) else {
                    continue;
                };
                for (key, r) in &tl.screens[i] {
                    if r.surface != sf.id
                        || *key == sf.root.as_str()
                        || !tl.visible(i, r)
                        || Axes(r.auth).intersects(moves)
                        || clipped(cx, key)
                        || cx.allowed(CheckId::Contain, key)
                    {
                        continue;
                    }
                    let b = on_surface(cx, i, r, r.visual);
                    let (near, far) = (b[axis], b[axis] + b[axis + 2]);
                    // Where it started the segment and where it rests:
                    // a spring's overshoot past both is not the surface's.
                    let ends: Vec<[f64; 4]> = [seg.s.and_then(|s| tl.at(s)), tl.at(seg.e)]
                        .into_iter()
                        .flatten()
                        .filter_map(|j| tl.row(j, key).map(|q| on_surface(cx, j, q, q.visual)))
                        .collect();
                    if ends.is_empty() {
                        continue;
                    }
                    let far_end = ends
                        .iter()
                        .map(|e| e[axis] + e[axis + 2])
                        .fold(f64::MIN, f64::max);
                    let near_end = ends.iter().map(|e| e[axis]).fold(f64::MAX, f64::min);
                    let over_far = far - f64::from(size[axis]) - (far - far_end).max(0.0);
                    let over_near = -near - (near_end - near).max(0.0);
                    let over = over_far.max(over_near);
                    if over > cx.tol + 1.0 / 64.0 {
                        out.push(
                            Finding::fail(
                                CheckId::Contain,
                                format!(
                                    "cut: {over:.1} px of it is past the edge of surface {} ({}), \
                                     which is sized by its content on {name}",
                                    sf.id, sf.label
                                ),
                            )
                            .at(k)
                            .node(key)
                            .axis(name)
                            .got(over, [0.0, cx.tol])
                            .in_segment(&seg.name),
                        );
                    }
                }
            }
        }
    }
    out
}

fn give_way(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let trace = cx.trace;
    let mut out = Vec::new();
    for i in 0..tl.ks.len() {
        let k = tl.ks[i];
        for (key, r) in &tl.screens[i] {
            let Some(meta) = trace.nodes.get(*key) else {
                continue;
            };
            if !meta.ellipsis
                || r.text.as_deref() != Some("S")
                || r.fit.is_some()
                || !tl.visible(i, r)
                || Axes(r.auth).intersects(Axes::X | Axes::SCALE | Axes::ROTATE)
            {
                continue;
            }
            let Some(size) = tl.size(i, r.surface) else {
                continue;
            };
            let w = f64::from(size[0]);
            let past = |q: &Row| {
                let b = on_surface(cx, i, q, q.laid);
                b[0] + b[2] > w + cx.tol || b[0] < -cx.tol
            };
            if !past(r) || clipped(cx, key) {
                continue;
            }
            let Some((_, seg)) = segment_of(trace, k) else {
                continue;
            };
            if cx.allowed(CheckId::Contain, key) {
                continue;
            }
            // Outward from the text while past the edge: excused by one
            // that cannot give way.
            let line = cx.lineage(key);
            let mut outer = *key;
            let mut rigid = meta.rigid;
            for a in &line[1..] {
                let Some(q) = tl.row(i, a) else { break };
                if !past(q) {
                    break;
                }
                outer = a;
                rigid |= trace.nodes.get(*a).is_some_and(|m| m.rigid);
            }
            // Something else rigid inside the outermost, past the edge too.
            rigid |= tl.screens[i].iter().any(|(k2, q)| {
                !line.contains(k2)
                    && cx.lineage(k2).contains(&outer)
                    && trace.nodes.get(*k2).is_some_and(|m| m.rigid)
                    && past(q)
            });
            if rigid {
                continue;
            }
            let b = on_surface(cx, i, r, r.laid);
            out.push(
                Finding::fail(
                    CheckId::Contain,
                    format!(
                        "give way: its ellipsis cut nothing, and it is laid from {:.1} to {:.1}, \
                         past the {:.0} px of surface {}",
                        b[0],
                        b[0] + b[2],
                        w,
                        r.surface
                    ),
                )
                .at(k)
                .node(key)
                .axis("w")
                .in_segment(&seg.name),
            );
        }
    }
    out
}

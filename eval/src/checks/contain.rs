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
//! - **held** (eval-rules-2): a box whose size springs holds what it
//!   lays out inside it. design.md's toast exits to `height: 0` while its
//!   "siblings slide up to fill the gap", and "a node whose size springs
//!   clips its content while it does (a collapsing toast hides what no
//!   longer fits)" (decisions.md wave3-pixels p2), as does one exiting to
//!   a pose that sets its size until it unmounts (exit-height0). So what
//!   a descendant shows (its drawn box, cut by the clip its ancestors
//!   set) lies inside the sizing box's drawn box, give or take `tol` (a
//!   clip is a whole number of physical pixels). A box drawn turned (a
//!   `rotate`, a `tilt`'s lean, on it or an ancestor) is left out: the
//!   trace has its clip as the bounding box, which holds its corners.
//! - **shadow** (eval-rules-2): "Shadows enlarge the buffer but not the
//!   input region" (design.md "Layout"): a surface's buffer keeps an
//!   overhang for the reach of its shadows at rest (blur reach 1.5 ×
//!   blur + 1 + spread, offsets included; decisions.md wave3-pixels), so
//!   a shadow drawn by a box at rest (no paint offset, transform or
//!   motion on it or its ancestors, nothing on its surface springing a
//!   size, no ancestor clipping it) lies inside its buffer, but on a side
//!   of the buffer that lies on or past the output's edge where the
//!   driver placed it (past it nothing shows: render keeps an anchored
//!   side's overhang within its margin, m6-w2-surface), or its `attach:`
//!   side (m4-surface-w1), `trace::Frame::cuts` (integration-r4: from the
//!   modelled buffer, not render's caps).

use strand_render::probe::{Axes, Motions};

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{Row, RunTrace};

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = cut(cx);
    out.extend(give_way(cx));
    out.extend(held(cx));
    out.extend(shadow(cx));
    first_per_node(out)
}

/// What of `r` shows: its drawn box, cut by its ancestors' clip; `None`
/// when nothing does.
fn shown(r: &Row) -> Option<[f64; 4]> {
    let v = r.visual.map(f64::from);
    let [x0, y0, x1, y1] = match r.clip {
        Some(c) => {
            let c = c.map(f64::from);
            [
                v[0].max(c[0]),
                v[1].max(c[1]),
                (v[0] + v[2]).min(c[0] + c[2]),
                (v[1] + v[3]).min(c[1] + c[3]),
            ]
        }
        None => [v[0], v[1], v[0] + v[2], v[1] + v[3]],
    };
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// `held`: what a box whose size springs (or that exits to a size) lays
/// out shows inside its drawn box.
fn held(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let trace = cx.trace;
    let mut out = Vec::new();
    let mut boxes = 0u64;
    for i in 0..tl.ks.len() {
        let k = tl.ks[i];
        let rows = &tl.screens[i];
        for (key, n) in rows {
            let own = Motions(n.mot);
            let exits_to_size = own.contains(Motions::EXIT)
                && trace
                    .nodes
                    .get(*key)
                    .and_then(|m| m.exit)
                    .is_some_and(|p| p.w.is_some() || p.h.is_some())
                && trace
                    .nodes
                    .get(*key)
                    .and_then(|m| m.parent.as_deref())
                    .and_then(|p| rows.get(p))
                    .is_none_or(|p| !Motions(p.mot).contains(Motions::EXIT));
            // Collapsed to nothing it is not visible itself, and what it
            // holds shows only if not clipped to it: held to it however
            // small it is.
            if !(own.contains(Motions::SIZE_SPRING) || exits_to_size)
                || cx.allowed(CheckId::Contain, key)
            {
                continue;
            }
            let b = n.visual.map(f64::from);
            let tol = cx.tol;
            for (k2, d) in rows {
                if k2 == key
                    || d.surface != n.surface
                    || !tl.visible(i, d)
                    || !cx.lineage(k2).contains(key)
                    || cx.allowed(CheckId::Contain, k2)
                {
                    continue;
                }
                // The clip a rotated box sets reaches the trace as its
                // bounding box, which holds more than it shows: the
                // check holds boxes drawn square to the screen.
                if cx
                    .lineage(k2)
                    .iter()
                    .any(|a| rows.get(*a).is_some_and(|r| r.rotate.abs() > 1e-3))
                {
                    continue;
                }
                let Some(e) = shown(d) else { continue };
                boxes += 1;
                let over = [
                    b[0] - e[0],
                    b[1] - e[1],
                    e[0] + e[2] - (b[0] + b[2]),
                    e[1] + e[3] - (b[1] + b[3]),
                ];
                let (side, by) = over
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .map(|(s, v)| (s, *v))
                    .unwrap_or((0, 0.0));
                if by <= tol + 1.0 / 64.0 {
                    continue;
                }
                let label = trace.nodes.get(*key).map_or(*key, |m| m.label.as_str());
                let what = if exits_to_size && !own.contains(Motions::SIZE_SPRING) {
                    "exits to a size"
                } else {
                    "springs its size"
                };
                out.push(
                    Finding::fail(
                        CheckId::Contain,
                        format!(
                            "held: it shows {by:.1} px past the {} edge of {label}, which {what} \
                             and so clips what it holds",
                            ["left", "top", "right", "bottom"][side]
                        ),
                    )
                    .at(k)
                    .node(k2)
                    .axis(["x", "y", "w", "h"][side])
                    .got(by, [0.0, tol]),
                );
            }
        }
    }
    super::stats("contain.held", cx, &[("boxes", boxes)]);
    out
}

/// A shadow's colour's alpha and extent (`rrggbbaa@x,y,w,h`).
fn shadow_of(s: &str) -> Option<(u8, [f64; 4])> {
    let (c, e) = s.split_once('@')?;
    let a = u8::from_str_radix(c.get(6..8)?, 16).ok()?;
    let v: Vec<f64> = e.split(',').filter_map(|x| x.parse().ok()).collect();
    match v.as_slice() {
        [x, y, w, h] => Some((a, [*x, *y, *w, *h])),
        _ => None,
    }
}

/// What moves a box off where layout put it, or its layout by itself.
const MOVES: Motions = Motions(
    Motions::GLIDE.0
        | Motions::SIZE_SPRING.0
        | Motions::ENTER.0
        | Motions::EXIT.0
        | Motions::KEYFRAMES.0
        | Motions::SHARED_MORPH.0
        | Motions::REVEAL.0
        | Motions::STAGGER.0
        | Motions::LEAN.0
        | Motions::JELLY.0
        | Motions::LIFT.0
        | Motions::TIME.0
        | Motions::SCROLL.0,
);

/// `shadow`: a box at rest draws its shadows inside its buffer.
fn shadow(cx: &Cx) -> Vec<Finding> {
    let trace = cx.trace;
    let mut out = Vec::new();
    let mut shadows = 0u64;
    let tol = cx.tol + 1.0 / 64.0;
    for f in &trace.frames {
        for s in &f.painted {
            let Some(size) = f.sizes.get(s).map(|v| v.map(f64::from)) else {
                continue;
            };
            let rows: Vec<&Row> = f.n.iter().filter(|r| r.surface == *s).collect();
            // A size springing on the surface holds its overhang where
            // the spring started or rests (decisions.md m6-owner-fixes,
            // bug 9): an overshoot past both is the buffer's to clip.
            if rows
                .iter()
                .any(|r| Motions(r.mot).contains(Motions::SIZE_SPRING))
            {
                continue;
            }
            let cuts = f.cuts.get(s).copied().unwrap_or(0);
            for r in &rows {
                let Some(p) = &r.paint else { continue };
                if p.sh.is_empty()
                    || r.hidden.is_some()
                    || r.op < 0.02
                    || r.clip.is_some()
                    || Motions(r.emot).intersects(MOVES)
                    || Axes(r.auth).intersects(Axes::GEOMETRY)
                    || (0..4).any(|j| (r.drawn[j] - r.laid[j]).abs() > 1.0 / 64.0)
                    || (0..4).any(|j| (r.visual[j] - r.drawn[j]).abs() > 1.0)
                    || r.scale != [1.0, 1.0]
                    || r.rotate != 0.0
                    || cx.allowed(CheckId::Contain, &r.key)
                {
                    continue;
                }
                for sh in &p.sh {
                    let Some((a, e)) = shadow_of(sh) else {
                        continue;
                    };
                    if a == 0 {
                        continue;
                    }
                    shadows += 1;
                    let past = [
                        (1u8, -e[1]),
                        (2, e[0] + e[2] - size[0]),
                        (4, e[1] + e[3] - size[1]),
                        (8, -e[0]),
                    ];
                    let Some((side, by)) = past
                        .iter()
                        .filter(|(bit, _)| cuts & bit == 0)
                        .max_by(|a, b| a.1.total_cmp(&b.1))
                        .copied()
                    else {
                        continue;
                    };
                    if by <= tol {
                        continue;
                    }
                    let name = match side {
                        1 => "top",
                        2 => "right",
                        4 => "bottom",
                        _ => "left",
                    };
                    out.push(
                        Finding::fail(
                            CheckId::Contain,
                            format!(
                                "shadow: its shadow reaches {by:.1} px past the {name} edge of \
                                 surface {s}'s buffer, which its overhang should hold"
                            ),
                        )
                        .at(f.k)
                        .node(&r.key)
                        .axis(name)
                        .got(by, [0.0, tol]),
                    );
                }
            }
        }
    }
    super::stats("contain.shadow", cx, &[("shadows", shadows)]);
    out
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

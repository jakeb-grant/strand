//! `coherence`: what one frame draws hangs together, whatever moves it.
//! Four rules, each read from the drawn and visual boxes the frame
//! traced (never from the offsets the renderer reports it used):
//!
//! - **inherit**: a node is drawn at its laid offset from its parent.
//!   Its drawn box less its laid box, less its own offsets (written,
//!   spring, glide, morph), is its parent's drawn box less the parent's
//!   laid box: a parent's paint offset (a glide, a spring, a lift, a
//!   morph's translate) carries its whole subtree (design.md: FLIP and
//!   paint offsets move what is drawn, never what is laid out).
//! - **pivot**: `scale`, `rotate`, a morph's scale and jelly turn a node
//!   about its box's centre (flatten's transform group), so its visual
//!   box is centred where its ancestors' transforms take its drawn
//!   box's centre: a transform never moves the box it turns.
//! - **morph start**: a shared-element morph starts from the box the
//!   node of its name was last drawn at (anim/morph.rs: "drawn moved and
//!   scaled, about its centre, separately in each axis, so its box covers
//!   the old one, then springs to its own box" as one 4-channel motion).
//!   On the first frame it plays, every edge of its box is the same
//!   share of the way from that old box to its own, a share of at most
//!   [`START_SHARE`].
//! - **stacking**: a node in flight (lifted, gliding, springing or
//!   morphing) over a sibling never goes below it while their layout
//!   keeps its order: it keeps its stacking until it lands (a dragged
//!   item dropped under its neighbours mid-flight pops behind them). A
//!   node rising above what it flies over (a drag lifting it) and a
//!   reorder that swaps the siblings' laid places (FLIP) are not pops.
//! - **drag** (eval-rules-2): a slider held in a drag draws its knob
//!   under the pointer, on the frame the pointer gets there. "A dragged
//!   slider draws the drag until the release", and "a slider's drag and
//!   its drawing share one span (the knob centre runs between the knob
//!   radii), so the knob stays under the pointer" (decisions.md
//!   wave3-pixels p3; design.md's render-never-waits rule): the knob's
//!   centre (its last part, `trace::Frame::drags` naming the sliders a
//!   drag holds) is the pointer's x, held between a knob radius in from
//!   either end of the slider's box.
//!
//! **inherit** leaves out a node moved by its own keyframes,
//! time-driven props, lean or jelly, whose offsets the trace's parts do
//! not carry.
//!
//! The tolerance is `tol` plus one physical pixel: both boxes compared
//! are snapped to physical pixels on their own.

use std::collections::{BTreeMap, HashMap};

use strand_render::probe::Motions;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{Frame, Rect, Row};

/// The most of its way a morph's first frame may have covered (as
/// `vanish`'s pop-in allows an enter pose's first frame).
pub const START_SHARE: f64 = 0.5;

fn centre(r: &Rect) -> [f64; 2] {
    [
        f64::from(r[0]) + f64::from(r[2]) / 2.0,
        f64::from(r[1]) + f64::from(r[3]) / 2.0,
    ]
}

fn own_offset(r: &Row) -> [f64; 2] {
    let p = |i: usize| {
        f64::from(r.written[i])
            + f64::from(r.spring[i])
            + f64::from(r.glide[i])
            + f64::from(r.morph[i])
    };
    [p(0), p(1)]
}

fn transformed(scale: [f32; 2], r: &Row) -> bool {
    scale != [1.0, 1.0] || r.rotate != 0.0
}

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let seg_name = |k: u32| {
        cx.trace
            .segments
            .iter()
            .rev()
            .find(|s| s.s.is_none_or(|s| s < k) && k <= s.e)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    };
    let seg_index = |k: u32| {
        cx.trace
            .segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.s.is_none_or(|s| s < k) && k <= s.e)
            .map(|(i, _)| i)
    };
    let exempt = |k: u32, key: &str| {
        seg_index(k).is_some_and(|si| {
            cx.exempt(
                CheckId::Coherence,
                si,
                // The key lives as long as the trace.
                cx.trace.nodes.get_key_value(key).map_or(key, |(k, _)| k),
                strand_render::probe::Axes::GEOMETRY,
            )
        })
    };
    for f in &cx.trace.frames {
        let scale = cx.trace.scale_at(f).max(0.25);
        let give = cx.tol + 1.0 / scale;
        let rows: HashMap<(&str, u32), &Row> =
            f.n.iter()
                .map(|r| ((r.key.as_str(), r.surface), r))
                .collect();
        let parent_of = |r: &Row| -> Option<&Row> {
            let p = cx.trace.nodes.get(&r.key)?.parent.as_deref()?;
            rows.get(&(p, r.surface)).copied()
        };
        let pose = |s: u32| f.poses.get(&s).filter(|_| cx.trace.on_screen);
        // A node's own scale: a surface root moved on screen carries its
        // surface's pose's scale (`RunTrace::on_screen`), which turns
        // about the surface's top-left, not the root's centre, and is
        // applied last.
        let own_scale = |r: &Row| -> [f32; 2] {
            match pose(r.surface) {
                Some(p) if p.scale != 0.0 && cx.trace.surfaces.iter().any(|s| s.root == r.key) => {
                    [r.scale[0] / p.scale, r.scale[1] / p.scale]
                }
                _ => r.scale,
            }
        };
        for r in &f.n {
            if exempt(f.k, &r.key) {
                continue;
            }
            // inherit (a node whose own keyframes, time-driven props or
            // lean place it is offset by more than its parts report)
            let unreported = Motions::KEYFRAMES | Motions::TIME | Motions::LEAN | Motions::JELLY;
            if let Some(p) = parent_of(r)
                && !Motions(r.mot).intersects(unreported)
            {
                let own = own_offset(r);
                for (i, axis) in ["x", "y"].iter().enumerate() {
                    let carried = f64::from(r.drawn[i]) - f64::from(r.laid[i]) - own[i];
                    let parent = f64::from(p.drawn[i]) - f64::from(p.laid[i]);
                    if (carried - parent).abs() > give {
                        out.push(
                            Finding::fail(
                                CheckId::Coherence,
                                format!(
                                    "inherit: drawn {:.1} px off its laid place on {axis} \
                                     beyond its own offsets, where its parent is drawn {:.1} px \
                                     off its own",
                                    carried, parent
                                ),
                            )
                            .at(f.k)
                            .node(&r.key)
                            .axis(axis)
                            .got(carried, [parent - give, parent + give])
                            .in_segment(&seg_name(f.k)),
                        );
                    }
                }
            }
            // pivot
            if !Motions(r.emot).intersects(Motions::JELLY) {
                let mut c = centre(&r.drawn);
                let mut chain = true;
                let mut posed = transformed(own_scale(r), r);
                let mut at = r;
                while let Some(p) = parent_of(at) {
                    let ps = own_scale(p);
                    if transformed(ps, p) {
                        posed = true;
                        let o = centre(&p.drawn);
                        let (sx, sy) = (f64::from(ps[0]), f64::from(ps[1]));
                        let th = f64::from(p.rotate).to_radians();
                        let (v0, v1) = ((c[0] - o[0]) * sx, (c[1] - o[1]) * sy);
                        c = [
                            o[0] + v0 * th.cos() - v1 * th.sin(),
                            o[1] + v0 * th.sin() + v1 * th.cos(),
                        ];
                    }
                    at = p;
                }
                if cx
                    .trace
                    .nodes
                    .get(&at.key)
                    .and_then(|m| m.parent.as_ref())
                    .is_some()
                    && !cx.trace.surfaces.iter().any(|s| s.root == at.key)
                {
                    // An ancestor is not in the frame: its transform is
                    // unknown.
                    chain = false;
                }
                if let Some(p) = pose(r.surface) {
                    posed |= p.scale != 1.0;
                    let k = f64::from(p.scale);
                    c = [f64::from(p.x) + k * c[0], f64::from(p.y) + k * c[1]];
                }
                let v = centre(&r.visual);
                if posed && chain {
                    for (i, axis) in ["x", "y"].iter().enumerate() {
                        if (v[i] - c[i]).abs() > give {
                            out.push(
                                Finding::fail(
                                    CheckId::Coherence,
                                    format!(
                                        "pivot: its transform moved its centre {:.1} px on \
                                         {axis} from where its box and its ancestors' \
                                         transforms put it",
                                        v[i] - c[i]
                                    ),
                                )
                                .at(f.k)
                                .node(&r.key)
                                .axis(axis)
                                .got(v[i], [c[i] - give, c[i] + give])
                                .in_segment(&seg_name(f.k)),
                            );
                        }
                    }
                }
            }
        }
    }
    out.extend(morph_start(cx));
    out.extend(stacking(cx));
    out.extend(drag(cx));
    first_per_node(out)
}

/// A part's box (`x,y,w,h`, from its node's drawn box's top-left).
fn part_of(s: &str) -> Option<[f64; 4]> {
    let v: Vec<f64> = s.split(',').filter_map(|x| x.parse().ok()).collect();
    match v.as_slice() {
        [x, y, w, h] => Some([*x, *y, *w, *h]),
        _ => None,
    }
}

/// `drag`: a dragged slider's knob is under the pointer.
fn drag(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut knobs = 0u64;
    for f in &cx.trace.frames {
        let Some(ptr) = f.ptr else { continue };
        let s = cx.trace.scale_at(f);
        let snap = |v: f64| (v * s).round() / s;
        let give = cx.tol + 1.0 / s;
        for key in f.drags.keys() {
            let Some(r) = f.n.iter().find(|r| &r.key == key) else {
                continue;
            };
            // Its part is in its own box: one drawn turned or scaled is
            // not held.
            if r.surface != ptr.s
                || cx.allowed(CheckId::Coherence, key)
                || cx.lineage(key).iter().any(|a| {
                    f.n.iter()
                        .find(|n| n.key == *a)
                        .is_some_and(|n| n.rotate.abs() > 1e-3 || n.scale != [1.0, 1.0])
                })
            {
                continue;
            }
            let Some(knob) = r
                .paint
                .as_ref()
                .and_then(|p| p.pt.last())
                .and_then(|p| part_of(p))
            else {
                continue;
            };
            knobs += 1;
            let x0 = snap(f64::from(r.drawn[0]));
            let x1 = snap(f64::from(r.drawn[0]) + f64::from(r.drawn[2]));
            let rad = knob[2] / 2.0;
            let at = x0 + knob[0] + rad;
            let want = f64::from(ptr.at[0]).clamp(x0 + rad, (x1 - rad).max(x0 + rad));
            if (at - want).abs() <= give {
                continue;
            }
            out.push(
                Finding::fail(
                    CheckId::Coherence,
                    format!(
                        "drag: held in a drag with the pointer at x {:.1}, it draws its knob \
                         centred at {at:.1}, {:.1} px from where the pointer holds it ({want:.1})",
                        ptr.at[0],
                        (at - want).abs()
                    ),
                )
                .at(f.k)
                .node(key)
                .axis("x")
                .got(at, [want - give, want + give]),
            );
        }
    }
    super::stats("coherence.drag", cx, &[("knobs", knobs)]);
    out
}

/// The latest frame before `before` (an index of `trace.frames`) that
/// drew another node named `name` on `surface`, and that node's row.
fn partner<'a>(cx: &Cx<'a>, before: usize, key: &str, name: &str, surface: u32) -> Option<&'a Row> {
    cx.trace.frames[..before].iter().rev().find_map(|f| {
        f.n.iter().find(|r| {
            r.surface == surface
                && r.key != key
                && cx.trace.nodes.get(&r.key).and_then(|m| m.morph.as_deref()) == Some(name)
        })
    })
}

fn morph_start(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let named: BTreeMap<&str, &str> = cx
        .trace
        .nodes
        .iter()
        .filter_map(|(k, m)| Some((k.as_str(), m.morph.as_deref()?)))
        .collect();
    if named.is_empty() {
        return out;
    }
    // Per node and surface: it morphed in the surface's last frame.
    let mut was: HashMap<(&str, u32), bool> = HashMap::new();
    for (fi, f) in cx.trace.frames.iter().enumerate() {
        let scale = cx.trace.scale_at(f).max(0.25);
        let give = cx.tol + 1.0 / scale;
        let mut now: HashMap<(&str, u32), bool> = HashMap::new();
        for r in &f.n {
            let Some(name) = named.get(r.key.as_str()) else {
                continue;
            };
            let morphs = Motions(r.mot).contains(Motions::SHARED_MORPH);
            now.insert((r.key.as_str(), r.surface), morphs);
            let before = was
                .get(&(r.key.as_str(), r.surface))
                .copied()
                .unwrap_or(false);
            if !morphs || before {
                continue;
            }
            let Some(p) = partner(cx, fi, &r.key, name, r.surface) else {
                continue;
            };
            // Its own box (the morph's translate taken out) and the box
            // the morph draws (scaled about its drawn centre).
            let own = [
                f64::from(r.drawn[0]) - f64::from(r.morph[0]),
                f64::from(r.drawn[1]) - f64::from(r.morph[1]),
                f64::from(r.drawn[2]),
                f64::from(r.drawn[3]),
            ];
            let c = centre(&r.drawn);
            let (w, h) = (
                f64::from(r.drawn[2]) * f64::from(r.scale[0]),
                f64::from(r.drawn[3]) * f64::from(r.scale[1]),
            );
            let drawn = [
                c[0] - w / 2.0,
                c[1] - h / 2.0,
                c[0] + w / 2.0,
                c[1] + h / 2.0,
            ];
            let to = [own[0], own[1], own[0] + own[2], own[1] + own[3]];
            let from = [
                f64::from(p.drawn[0]),
                f64::from(p.drawn[1]),
                f64::from(p.drawn[0]) + f64::from(p.drawn[2]),
                f64::from(p.drawn[1]) + f64::from(p.drawn[3]),
            ];
            let (mut lo, mut hi) = (-0.05f64, START_SHARE);
            let mut off = Vec::new();
            for e in 0..4 {
                let d = to[e] - from[e];
                let at = drawn[e] - from[e];
                if d.abs() <= give {
                    if at.abs() > give + d.abs() {
                        off.push(e);
                    }
                    continue;
                }
                let (a, b) = ((at - give) / d, (at + give) / d);
                lo = lo.max(a.min(b));
                hi = hi.min(a.max(b));
            }
            if lo > hi || !off.is_empty() {
                let shares: Vec<String> = (0..4)
                    .map(|e| {
                        let d = to[e] - from[e];
                        if d.abs() <= give {
                            "-".to_string()
                        } else {
                            format!("{:.2}", (drawn[e] - from[e]) / d)
                        }
                    })
                    .collect();
                out.push(
                    Finding::fail(
                        CheckId::Coherence,
                        format!(
                            "morph start: its first frame is not on its way from the box \
                             `{name}` was last drawn at to its own (left, top, right, bottom \
                             at shares {} of that way; one share, at most {START_SHARE}, \
                             moves it as one)",
                            shares.join(", ")
                        ),
                    )
                    .at(f.k)
                    .node(&r.key),
                );
            }
        }
        // A surface painted shows only what this frame drew on it; one
        // not painted keeps what it showed.
        was.retain(|(_, s), _| !f.painted.contains(s));
        was.extend(now);
    }
    out
}

/// Each surface's painted frames, in order.
fn painted<'a>(cx: &Cx<'a>) -> BTreeMap<u32, Vec<&'a Frame>> {
    let mut m: BTreeMap<u32, Vec<&Frame>> = BTreeMap::new();
    for f in &cx.trace.frames {
        for s in &f.painted {
            m.entry(*s).or_default().push(f);
        }
    }
    m
}

fn shows(r: &Row) -> bool {
    r.op >= 0.02
        && !matches!(r.hidden.as_deref(), Some("opacity0" | "scale0" | "clipped"))
        && r.visual[2] * r.visual[3] >= 1.0
}

fn in_flight(r: &Row, tol: f64) -> bool {
    (0..2).any(|i| {
        (f64::from(r.spring[i]) + f64::from(r.glide[i]) + f64::from(r.morph[i])).abs() > tol
    })
}

fn overlap(a: &Rect, b: &Rect) -> f64 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    if w > 0.0 && h > 0.0 {
        f64::from(w) * f64::from(h)
    } else {
        0.0
    }
}

fn stacking(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    for (s, frames) in painted(cx) {
        for pair in frames.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            fn order(f: &Frame, s: u32) -> HashMap<&str, usize> {
                f.n.iter()
                    .filter(|r| r.surface == s)
                    .enumerate()
                    .map(|(i, r)| (r.key.as_str(), i))
                    .collect()
            }
            let (oa, ob) = (order(a, s), order(b, s));
            // Siblings by parent, in the later frame's order.
            let mut kids: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
            for r in b.n.iter().filter(|r| r.surface == s && shows(r)) {
                if let Some(p) = cx.trace.nodes.get(&r.key).and_then(|m| m.parent.as_deref()) {
                    kids.entry(p).or_default().push(r);
                }
            }
            let rows_a: HashMap<&str, &Row> =
                a.n.iter()
                    .filter(|r| r.surface == s)
                    .map(|r| (r.key.as_str(), r))
                    .collect();
            for list in kids.values() {
                for (i, x) in list.iter().enumerate() {
                    for y in &list[i + 1..] {
                        let (Some(xa), Some(ya)) = (oa.get(x.key.as_str()), oa.get(y.key.as_str()))
                        else {
                            continue;
                        };
                        let (Some(rxa), Some(rya)) =
                            (rows_a.get(x.key.as_str()), rows_a.get(y.key.as_str()))
                        else {
                            continue;
                        };
                        if !shows(rxa) || !shows(rya) {
                            continue;
                        }
                        let (xb, yb) = (ob[x.key.as_str()], ob[y.key.as_str()]);
                        if (xa < ya) == (xb < yb) {
                            continue;
                        }
                        // The one that went below, and the one now over it.
                        let (low, high, low_a) = if xb < yb { (x, y, rxa) } else { (y, x, rya) };
                        let laid_kept = (0..2).all(|i| {
                            let before = rxa.laid[i] - rya.laid[i];
                            let after = x.laid[i] - y.laid[i];
                            before.abs() <= cx.tol as f32
                                || after.abs() <= cx.tol as f32
                                || (before > 0.0) == (after > 0.0)
                        });
                        let flying = in_flight(low, cx.tol) || in_flight(low_a, cx.tol);
                        let over = overlap(&x.visual, &y.visual) >= 1.0
                            || overlap(&rxa.visual, &rya.visual) >= 1.0;
                        if !(laid_kept && flying && over) {
                            continue;
                        }
                        let other = cx
                            .trace
                            .nodes
                            .get(&high.key)
                            .map_or(high.key.as_str(), |m| m.label.as_str());
                        out.push(
                            Finding::fail(
                                CheckId::Coherence,
                                format!(
                                    "stacking: in flight over {other}, it went below it with \
                                     their layout in the same order"
                                ),
                            )
                            .at(b.k)
                            .node(&low.key),
                        );
                    }
                }
            }
        }
    }
    out
}

//! `coherence`: what one frame draws hangs together, whatever moves it.
//! Its rules, each read from the drawn and visual boxes the frame
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
//! - **morph arrival** (eval-rules-5, decisions.md cross-surface-morph):
//!   a node with a `morph` name that comes on screen while a node of that
//!   name was drawn the frame before, or within the second a box is
//!   remembered (anim/morph.rs), starts on that box on the output (its
//!   surface's origin plus its visual box), at most half its way from it,
//!   and reaches its own box with no step larger than a spring's; with no
//!   partner, or under reduced motion, it shows at its own box.
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
//! - **ring** (eval-rules-6): a `border:` is a ring of one width, its
//!   inner edge its rounded box inset by the border's width, each
//!   radius reduced by that width and never below 0, as CSS draws a
//!   border ("Corner Shaping"), the width rounded to whole device pixels,
//!   at least one (decisions.md 2026-10-05 render), and the outer radii
//!   the ones asked for, shrunk as CSS shrinks them. Measured on the
//!   outline the border filled along each corner's bisector and each
//!   edge's midpoint (`crate::ring`), against that shape worked out from
//!   the box, the `radius:` and the `border:` width, at every frame
//!   (fractional scales and sizes in flight included): each crossing
//!   within [`RING_TOL`] device px. A border along a `shape:` outline or
//!   squircle corners is not judged.
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
            // (fidelity) Its surface's scale: with several outputs, its
            // own output's, so each surface keeps the tolerance it has
            // alone.
            let scale = cx.trace.scale_on(f, r.surface).max(0.25);
            let give = cx.tol + 1.0 / scale;
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
                // (integration-r7) How much the ancestors' scales magnify
                // on each axis what was snapped before them.
                let mut mag = [1.0f64, 1.0];
                let mut at = r;
                while let Some(p) = parent_of(at) {
                    let ps = own_scale(p);
                    if transformed(ps, p) {
                        posed = true;
                        let o = centre(&p.drawn);
                        let (sx, sy) = (f64::from(ps[0]), f64::from(ps[1]));
                        mag = [mag[0] * sx.abs(), mag[1] * sy.abs()];
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
                // (integration-r7) Where the compositor shows the pose,
                // moved by the margins sent (`crate::trace::Posed`), as
                // `on_screen` placed the row.
                if let Some(p) = pose(r.surface).and_then(|_| f.posed(r.surface)) {
                    posed |= p.scale != 1.0;
                    c = [
                        f64::from(p.show(0, c[0] as f32)),
                        f64::from(p.show(1, c[1] as f32)),
                    ];
                }
                let v = centre(&r.visual);
                if posed && chain {
                    for (i, axis) in ["x", "y"].iter().enumerate() {
                        // (integration-r7) A box is snapped to physical
                        // pixels where it is drawn, before its ancestors'
                        // transforms (decisions.md wave3-pixels), so the
                        // physical pixel of give between its centre and
                        // its parent's is drawn as large as they magnify
                        // it: a pill morphing back from a card seven
                        // times its height draws its art 2 px off the
                        // pill's centre for a third of a pixel snapped.
                        let give = cx.tol + mag[i].max(1.0) / scale;
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
    out.extend(morph_arrival(cx));
    out.extend(stacking(cx));
    out.extend(drag(cx));
    out.extend(ring(cx));
    // (eval-rules-8) A shadow covers what its props ask at the scale.
    out.extend(super::reach::run(cx));
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
        // (fidelity) The knob is on the pointer's surface: its scale.
        let s = cx.trace.scale_on(f, ptr.s);
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
        let mut now: HashMap<(&str, u32), bool> = HashMap::new();
        for r in &f.n {
            let Some(name) = named.get(r.key.as_str()) else {
                continue;
            };
            // (fidelity) Its surface's scale.
            let scale = cx.trace.scale_on(f, r.surface).max(0.25);
            let give = cx.tol + 1.0 / scale;
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

/// How long a name's box is a morph's start after it left the screen
/// (anim/morph.rs: "or left it within the last second").
const MORPH_REMEMBERED_NS: u64 = 1_000_000_000;

/// (eval-rules-5) `r`'s visual box at timeline frame `i` on the output:
/// its surface's origin there plus its box on the surface (the edges
/// left, top, right, bottom). The origin is the driver's placement, what
/// render was given (`Frame::origins`), never render's morph state.
fn on_output(cx: &Cx, i: usize, r: &Row) -> [f64; 4] {
    let moved = cx.tl.moved(i, r.surface);
    let o = cx.tl.frames[i]
        .origins
        .get(&r.surface)
        .copied()
        .unwrap_or([0.0, 0.0]);
    let x = f64::from(r.visual[0]) - f64::from(moved[0]) + f64::from(o[0]);
    let y = f64::from(r.visual[1]) - f64::from(moved[1]) + f64::from(o[1]);
    [x, y, x + f64::from(r.visual[2]), y + f64::from(r.visual[3])]
}

/// (integration-r7) Where `r` rests, as laid out at timeline frame `i`,
/// on the output: its laid box plus what is written at rest on it and
/// its ancestors (`written`, and of `inherited` what is not in flight),
/// through the pose the compositor shows its surface at (when the rows
/// are read on screen, `crate::trace::Posed`), plus its surface's
/// origin. The box a morph that starts at `i` springs toward: the
/// layout it was given then, not where the segment's last frame shows
/// it (a later change of layout in the segment is another motion).
fn rest_on_output(cx: &Cx, i: usize, r: &Row) -> [f64; 4] {
    let f = cx.tl.frames[i];
    let moved = cx.tl.moved(i, r.surface);
    let o = f.origins.get(&r.surface).copied().unwrap_or([0.0, 0.0]);
    let at = |n: usize| r.laid[n] - moved[n] + r.inherited[n] - r.flight[n] + r.written[n];
    let (mut x, mut y, mut w, mut h) = (at(0), at(1), r.laid[2], r.laid[3]);
    if cx.trace.on_screen
        && let Some(p) = f.posed(r.surface)
    {
        (x, y, w, h) = (p.show(0, x), p.show(1, y), p.scale * w, p.scale * h);
    }
    let (x, y) = (f64::from(x + o[0]), f64::from(y + o[1]));
    [x, y, x + f64::from(w), y + f64::from(h)]
}

/// (eval-rules-5, cross-surface-morph) **morph arrival**: a node with a
/// `morph` name (as written, `NodeMeta::morph`) that comes on screen at
/// timeline frame `i` (its row there, on a surface it had no row on at
/// `i - 1`: created, its surface opened, a new surface) while another
/// node of that name showed at `i - 1` on a surface of the output (the
/// sandbox has one), or showed until less than a second before, starts
/// from that node's box (design.md: "the bar's media pill becomes the
/// media panel"): on its first frame every edge of its box on the output
/// is one share of the way from the partner's last box to its own box
/// at rest (integration-r7: as laid out at that first frame,
/// [`rest_on_output`]), a share of at most
/// [`START_SHARE`] (as `morph start`), the boxes compared on the output
/// (each surface's origin plus the box on it), so its scale is the two
/// boxes' ratio. It then reaches its own box with no step larger than a
/// motion's (`teleport`'s bound: `teleport_px` a 60 Hz tick, or 0.75 of
/// that edge's way). With no partner (none shown, or gone over a second
/// before) or with motion reduced, it shows at its own box at once,
/// unless something declared moves it (its or an ancestor's enter pose,
/// an ancestor's motion in flight, the compositor's pose; integration-r7
/// part 2: never the renderer's own motion bits). None of it reads render's morph state: the
/// partner, its box and the time are the trace's.
fn morph_arrival(cx: &Cx) -> Vec<Finding> {
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
    let tl = &cx.tl;
    let (mut partnered, mut alone, mut unjudged) = (0u64, 0u64, 0u64);
    let segment = |k: u32| {
        cx.trace
            .segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.s.is_some_and(|s| s < k) && k <= s.e)
    };
    for i in 1..tl.ks.len() {
        let k = tl.ks[i];
        let Some((_, seg)) = segment(k) else { continue };
        let t = tl.frames[i].t_ns;
        for (key, r) in &tl.screens[i] {
            let Some(name) = named.get(key) else { continue };
            // (fidelity) Its surface's scale.
            let scale = cx.trace.scale_on(tl.frames[i], r.surface).max(0.25);
            let give = cx.tol + 1.0 / scale;
            // Drawn, wherever on its surface: a morph from a box on
            // another surface starts off its own (the pill above the
            // panel), where its buffer cuts it.
            if tl.row(i - 1, key).is_some_and(|q| q.surface == r.surface)
                || !shows(r)
                || cx.allowed(CheckId::Coherence, key)
            {
                continue;
            }
            // Its last frame shown in the segment (this one, when the
            // segment rests at once): how far its way is followed.
            let end = i..tl.within(Some(k), seg.e).end.max(i + 1);
            let Some(last) = end.clone().rev().find(|j| {
                tl.row(*j, key)
                    .is_some_and(|q| q.surface == r.surface && shows(q))
            }) else {
                continue;
            };
            // (integration-r7) Its own box at rest: as laid out at its
            // first frame, the box the morph springs toward.
            let to = rest_on_output(cx, i, r);
            let first = on_output(cx, i, r);
            // (integration-r8) Each edge's give: a box is snapped to
            // physical pixels where it is drawn, before its ancestors'
            // transforms (decisions.md wave3-pixels; as `pivot` reads
            // it), so a pixel of give is drawn as large as they magnify
            // it on that axis: a title morphing out of a card while the
            // pill holding it is drawn 8.6 times its height.
            let mut mag = [1.0f64, 1.0];
            let mut up = cx.trace.nodes.get(*key).and_then(|m| m.parent.clone());
            while let Some(p) = up {
                if let Some(q) = tl.row(i, &p).filter(|q| q.surface == r.surface) {
                    mag = [
                        mag[0] * f64::from(q.scale[0]).abs().max(1.0),
                        mag[1] * f64::from(q.scale[1]).abs().max(1.0),
                    ];
                }
                up = cx.trace.nodes.get(&p).and_then(|m| m.parent.clone());
            }
            let give_on = |e: usize| cx.tol + mag[e % 2] / scale;
            // The partner: shown at i - 1, else the latest shown within
            // the second (ambiguous near the second's end: not judged).
            let others = |j: usize| -> Vec<&Row> {
                tl.screens[j]
                    .iter()
                    .filter(|(k2, q)| {
                        *k2 != key && named.get(*k2) == Some(name) && tl.visible(j, q)
                    })
                    .map(|(_, q)| *q)
                    .collect()
            };
            let mut partner: Option<(usize, Vec<&Row>)> = None;
            let mut none = true;
            let mut j = i - 1;
            loop {
                let p = others(j);
                if !p.is_empty() {
                    // It stopped showing at the frame after j.
                    let left = if j + 1 == i { t } else { tl.frames[j + 1].t_ns };
                    let ago = t.saturating_sub(left);
                    let period = 1_000_000_000 / u64::from(seg.hz.max(1));
                    if ago + 2 * period < MORPH_REMEMBERED_NS {
                        partner = Some((j, p));
                    } else if ago < MORPH_REMEMBERED_NS + 2 * period {
                        none = false;
                    }
                    break;
                }
                if j == 0 || t.saturating_sub(tl.frames[j].t_ns) > 2 * MORPH_REMEMBERED_NS {
                    break;
                }
                j -= 1;
            }
            let reduced = seg.reduced_motion;
            let ambiguous = partner.is_none() && !none;
            match partner {
                Some((j, ps)) if !reduced => {
                    partnered += 1;
                    // On its way from one of them (the nearest judged).
                    let mut best: Option<(f64, String)> = None;
                    for p in ps {
                        let from = on_output(cx, j, p);
                        let (mut lo, mut hi) = (-0.05f64, START_SHARE);
                        let mut off = false;
                        for e in 0..4 {
                            let d = to[e] - from[e];
                            let at = first[e] - from[e];
                            let give = give_on(e);
                            if d.abs() <= give {
                                off |= at.abs() > give + d.abs();
                                continue;
                            }
                            let (a, b) = ((at - give) / d, (at + give) / d);
                            lo = lo.max(a.min(b));
                            hi = hi.min(a.max(b));
                        }
                        if lo <= hi && !off {
                            best = None;
                            break;
                        }
                        let shares: Vec<String> = (0..4)
                            .map(|e| {
                                let d = to[e] - from[e];
                                if d.abs() <= give {
                                    format!("{:+.1} px", first[e] - from[e])
                                } else {
                                    format!("{:.2}", (first[e] - from[e]) / d)
                                }
                            })
                            .collect();
                        let worst = (0..4)
                            .map(|e| (first[e] - from[e]).abs())
                            .fold(0.0, f64::max);
                        if best.as_ref().is_none_or(|(w, _)| worst < *w) {
                            best = Some((
                                worst,
                                format!(
                                    "morph arrival: it came on screen while `{name}` showed \
                                     on the output, and its first frame is not on its way \
                                     from that box to its own (left, top, right, bottom at \
                                     shares {} of that way; at most {START_SHARE}, as one)",
                                    shares.join(", ")
                                ),
                            ));
                        }
                    }
                    if let Some((worst, msg)) = best {
                        out.push(
                            Finding::fail(CheckId::Coherence, msg)
                                .at(k)
                                .node(key)
                                .got(worst, [0.0, give])
                                .in_segment(&seg.name),
                        );
                        continue;
                    }
                    // Then to its own box with no step larger than a
                    // motion's.
                    let from = on_output(cx, j, others(j)[0]);
                    let per_tick = cx.sc.tolerance.teleport_px * 60.0 / f64::from(seg.hz.max(1));
                    let mut prev = (i, first);
                    for n in (i + 1)..=last {
                        let Some(q) = tl.row(n, key).filter(|q| q.surface == r.surface) else {
                            break;
                        };
                        if !shows(q) {
                            break;
                        }
                        let now = on_output(cx, n, q);
                        let ticks = f64::from(tl.ks[n] - tl.ks[prev.0]).max(1.0);
                        let jump = (0..4).find_map(|e| {
                            let bound =
                                (per_tick * ticks).max(0.75 * (to[e] - from[e]).abs()) + give_on(e);
                            let d = (now[e] - prev.1[e]).abs();
                            (d > bound).then_some((e, d, bound))
                        });
                        if let Some((e, d, bound)) = jump {
                            out.push(
                                Finding::fail(
                                    CheckId::Coherence,
                                    format!(
                                        "morph arrival: on its way from `{name}`'s box its {} \
                                         edge jumped {d:.1} px in one frame",
                                        ["left", "top", "right", "bottom"][e]
                                    ),
                                )
                                .at(tl.ks[n])
                                .node(key)
                                .got(d, [0.0, bound])
                                .in_segment(&seg.name),
                            );
                            break;
                        }
                        prev = (n, now);
                    }
                }
                _ if ambiguous && !reduced => unjudged += 1,
                _ => {
                    // No partner, or motion reduced: at its own box,
                    // unless something declared moves it. (integration-r7
                    // part 2) Read from the declared tree and the trace,
                    // never the renderer's motion bits: its own `enter`
                    // pose or an ancestor's, an ancestor's motion in
                    // flight carrying it (`flight`), or the compositor's
                    // pose. A renderer that flies a named node in with a
                    // motion of its own and no partner is judged.
                    let declared = |start: &str| {
                        let mut at = Some(start.to_string());
                        let mut n = 0;
                        while let Some(k) = at
                            && n < 256
                        {
                            let Some(m) = cx.trace.nodes.get(&k) else {
                                break;
                            };
                            if m.enter.is_some() {
                                return true;
                            }
                            at = m.parent.clone();
                            n += 1;
                        }
                        false
                    };
                    let moved = declared(key)
                        || r.flight.iter().any(|v| f64::from(v.abs()) > cx.tol)
                        || tl.frames[i]
                            .poses
                            .get(&r.surface)
                            .is_some_and(|p| p.x != 0.0 || p.y != 0.0 || p.scale != 1.0)
                        || tl.frames[i].posed(r.surface).is_some_and(|p| p.moves());
                    if moved && !reduced {
                        unjudged += 1;
                        continue;
                    }
                    alone += 1;
                    let worst = (0..4).map(|e| (first[e] - to[e]).abs()).fold(0.0, f64::max);
                    if worst > give {
                        out.push(
                            Finding::fail(
                                CheckId::Coherence,
                                format!(
                                    "morph arrival: {} it shows at its own box, and its first \
                                     frame is {worst:.1} px off it",
                                    if reduced {
                                        "with motion reduced".to_string()
                                    } else {
                                        format!("with no other `{name}` on screen")
                                    }
                                ),
                            )
                            .at(k)
                            .node(key)
                            .got(worst, [0.0, give])
                            .in_segment(&seg.name),
                        );
                    }
                }
            }
        }
    }
    super::stats(
        "coherence.morph-arrival",
        cx,
        &[
            ("partnered", partnered),
            ("at-box", alone),
            ("unjudged", unjudged),
        ],
    );
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

/// (eval-rules-6) How far a crossing of a drawn ring may lie from the
/// shape asked for, device px: an arc is drawn as cubic curves within
/// 0.1 px of it (the flattener's tolerance), and the rest is f32 slack.
/// A ring whose inner radius is not reduced by a 1 px border lies
/// `√2 − 1` px off on each corner's bisector.
pub const RING_TOL: f64 = 0.25;

/// `ring`: each border's outline, as filled, is the CSS ring its props
/// ask for.
fn ring(cx: &Cx) -> Vec<Finding> {
    const RAYS: [&str; 8] = [
        "top-left corner's bisector",
        "top-right corner's bisector",
        "bottom-right corner's bisector",
        "bottom-left corner's bisector",
        "top edge's middle",
        "right edge's middle",
        "bottom edge's middle",
        "left edge's middle",
    ];
    let mut out = Vec::new();
    let mut judged = 0u64;
    for f in &cx.trace.frames {
        for r in &f.rings {
            let [x0, y0, x1, y1] = r.frame;
            if x1 - x0 < 1.0 || y1 - y0 < 1.0 || cx.allowed(CheckId::Coherence, &r.key) {
                continue;
            }
            judged += 1;
            let want = crate::ring::expected(r);
            for (i, (got, want)) in r.drawn.iter().zip(&want).enumerate() {
                let off = if got.len() == want.len() {
                    got.iter()
                        .zip(want)
                        .map(|(g, w)| f64::from(*g) - w)
                        .fold(0.0f64, |m, d| if d.abs() > m.abs() { d } else { m })
                } else {
                    f64::INFINITY
                };
                if off.abs() <= RING_TOL {
                    continue;
                }
                let label = cx
                    .trace
                    .nodes
                    .get(&r.key)
                    .map_or(r.key.as_str(), |m| m.label.as_str());
                let list = |v: &mut dyn Iterator<Item = f64>| {
                    v.map(|t| format!("{t:.2}")).collect::<Vec<_>>().join(", ")
                };
                let msg = format!(
                    "ring: {label}'s border, {} px wide on a {:.0}x{:.0} px box with radii {}, is \
                     drawn crossing its {} at [{}] px where its CSS ring (its box inset by its \
                     width, each radius less the width) crosses it at [{}]",
                    crate::ring::border_px(r.width, r.scale),
                    x1 - x0,
                    y1 - y0,
                    crate::ring::outer_radii(r.radius, r.scale, x1 - x0, y1 - y0)
                        .map(|v| format!("{v:.1}"))
                        .join("/"),
                    RAYS.get(i).copied().unwrap_or("ray"),
                    list(&mut got.iter().map(|v| f64::from(*v))),
                    list(&mut want.iter().copied()),
                );
                let got_off = if off.is_finite() { off.abs() } else { 0.0 };
                out.push(
                    Finding::fail(CheckId::Coherence, msg)
                        .at(f.k)
                        .node(&r.key)
                        .got(got_off, [0.0, RING_TOL]),
                );
                break;
            }
        }
    }
    super::stats("coherence.ring", cx, &[("rings", judged)]);
    out
}

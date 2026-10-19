//! (eval-rules-4) `placement`: each surface's card (its root's laid box,
//! where its surface lies on the output) is where its declared props
//! and the protocols put it, at every frame it is painted at a settled
//! size and place; and a `split`'s `center` is centred on its split.
//! The expectation is worked out here from the declared placement
//! (`SurfaceInfo::places`, `SurfaceSpec`'s props as written), design.md
//! and spec.md 5.1, xdg_positioner's and layer-shell's semantics, never
//! by strand-surface's placement code (which the driver runs as the
//! compositor's model, so a bug there moves the buffer the trace sees).
//!
//! - `anchor`: a popup (not a tooltip, whose place nothing documents)
//!   opens away from a bar's edge, across the whole bar, below its
//!   anchor otherwise, and beside its row for a popup in a popup (on the
//!   side its `anchor:` names, level with the row's top); `attach:`
//!   names its side that touches its anchor. The gap on the side facing
//!   the anchor is its `margin` there when set, else 6 px (0 with
//!   `attach:`). Across, it is centred on its anchor element. A shadow's
//!   overhang lies outside the card (spec.md 5.1 `name.box`: a popup's
//!   own box is where it sits on the surface it opens from). On an axis
//!   where that place does not fit on the output the compositor may
//!   slide or flip it (xdg_positioner's constraint adjustment): that axis
//!   is not judged. A frame is judged once its anchor is where it was
//!   when the popup was shown (an anchor that moves keeps its popup where
//!   it was, render's `place_waiting_popups`), and the popup's size and
//!   place and its anchor's are what they were the frame before.
//! - `margin`: a bar, panel or OSD lies in the output's area left by the
//!   exclusive zones of the bars arranged before it (layer-shell: a bar
//!   reserves its thickness and its margin on its edge; the compositor
//!   arranges exclusive surfaces first, from the overlay layer down, in
//!   the order they were made, then the rest in what is left): on each
//!   side it is anchored to (a bar's edge and both ends; `anchor:`; a
//!   panel's `attach:`), its card lies its margin from that area's edge.
//!   Shadows enlarge the buffer, not the box (design.md "Hit testing",
//!   spec.md 5.1): the overhang lies outside the margin. A side
//!   anchored with its opposite is judged only for a bar along its edge
//!   (stretched, no declared length); an unanchored axis is centred by
//!   the compositor's buffer arithmetic and not judged. A frame is
//!   judged when no layer surface changed size or place since the
//!   frame before.
//! - `pose` (eval-rules-6): a surface whose pose the compositor applies
//!   (`compositor_poses`) lies, for each frame, where its pose's offset
//!   moves it from where it lies at rest: its offset is in output
//!   coordinates (design.md "Poses": `y: 20` moves the surface 20 px
//!   down, whatever edge it is anchored to), and layer shell's margins
//!   are distances from the edges a surface is anchored to, so a pose on
//!   a bottom-anchored OSD shrinks its bottom margin. Where the
//!   compositor puts it is the margins the surface manager sent for the
//!   frame (`Frame::shifts`); render delegates an offset only on an
//!   axis margins can move (anchored on one side) and never on a popup,
//!   so each frame's place is its rest place plus the offset, within
//!   half a pixel (margins are whole pixels).
//! - `centre`: a `split`'s `center` is centred on the split's content
//!   box along the split's axis "whatever the other two hold" (spec.md
//!   6, `split`); a side bar's split runs down.
//! - `fit`: an `image`'s or `icon`'s source shows where its `fit:` puts
//!   it in its box at every frame (spec.md: `contain` whole, as large as
//!   fits, centred; `cover` covering the box, centred; `fill` stretched
//!   to it), a decode made for another box size standing in while its
//!   box springs included: the source's place in the drawn pixmap (by
//!   the fit the pixmap was decoded by, at its own size) scaled to where
//!   the pixmap is drawn, against the source fitted to the box. `none`
//!   (its own size, which an SVG or icon takes at the box's scale) is
//!   not judged.

use std::collections::BTreeMap;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::{Frame, PlaceRec, Row, SurfaceInfo};

/// A popup's gap on the side facing its anchor while `margin` is unset
/// (spec.md 5.1).
pub const POPUP_GAP: f64 = 6.0;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = anchor(cx);
    out.extend(margin(cx));
    out.extend(pose(cx));
    out.extend(centre(cx));
    out.extend(fit(cx));
    first_per_node(out)
}

/// The tolerance of a placement: the compositor places whole logical
/// pixels (anchor rects, margins and centring halves rounded), and an
/// edge's give.
fn give(cx: &Cx) -> f64 {
    1.0 + cx.tol
}

/// A side of a box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    fn of(name: &str) -> Option<Side> {
        Some(match name {
            "top" => Side::Top,
            "right" => Side::Right,
            "bottom" => Side::Bottom,
            "left" => Side::Left,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Side::Top => "top",
            Side::Right => "right",
            Side::Bottom => "bottom",
            Side::Left => "left",
        }
    }

    /// Its index in a `margin` (top, right, bottom, left).
    fn at(self) -> usize {
        match self {
            Side::Top => 0,
            Side::Right => 1,
            Side::Bottom => 2,
            Side::Left => 3,
        }
    }

    fn opposite(self) -> Side {
        match self {
            Side::Top => Side::Bottom,
            Side::Right => Side::Left,
            Side::Bottom => Side::Top,
            Side::Left => Side::Right,
        }
    }

    fn vertical(self) -> bool {
        matches!(self, Side::Top | Side::Bottom)
    }
}

/// A box on the output, `[x, y, w, h]`.
type Rect = [f64; 4];

fn edge(r: Rect, s: Side) -> f64 {
    match s {
        Side::Top => r[1],
        Side::Right => r[0] + r[2],
        Side::Bottom => r[1] + r[3],
        Side::Left => r[0],
    }
}

/// The declared placement of `s` at tick `k`.
fn place_at(s: &SurfaceInfo, k: u32) -> Option<&PlaceRec> {
    s.places.iter().rev().find(|p| p.k <= k)
}

/// Where row `r` of surface `s` is laid on the output: its laid box (the
/// rows of the checks' trace are moved with their surface from where it
/// was first placed) at that first place.
fn on_output(first: &BTreeMap<u32, [f32; 2]>, r: &Row) -> Rect {
    let o = first.get(&r.surface).copied().unwrap_or([0.0, 0.0]);
    [
        f64::from(r.laid[0] + o[0]),
        f64::from(r.laid[1] + o[1]),
        f64::from(r.laid[2]),
        f64::from(r.laid[3]),
    ]
}

fn close(a: Rect, b: Rect) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 0.5)
}

/// A surface's size and its place on the output.
type Sized = ([f32; 2], [f32; 2]);

/// The surface's size and place at frame `f`.
fn sized(f: &Frame, s: u32) -> Option<Sized> {
    Some((
        *f.sizes.get(&s)?,
        f.origins.get(&s).copied().unwrap_or([0.0, 0.0]),
    ))
}

/// The way a popup opens: the side of its anchor it lies beyond.
fn opens(p: &PlaceRec, parent: &SurfaceInfo, parent_place: Option<&PlaceRec>) -> Option<Side> {
    // `attach:` names the popup's side that touches its anchor.
    if let Some(a) = p.attach.as_deref() {
        return Some(Side::of(a)?.opposite());
    }
    Some(match parent.kind.as_str() {
        // Away from the bar's edge.
        "bar" => {
            let e = parent_place
                .and_then(|pp| pp.edge.as_deref())
                .unwrap_or("top");
            Side::of(e)?.opposite()
        }
        // Beside its row, on the side its `anchor:` names.
        "popup" => match p.anchor.as_str() {
            "left" | "top_left" | "bottom_left" => Side::Left,
            "top" => Side::Top,
            "bottom" => Side::Bottom,
            _ => Side::Right,
        },
        _ => Side::Bottom,
    })
}

/// `anchor`: popups by their anchors.
fn anchor(cx: &Cx) -> Vec<Finding> {
    let trace = cx.trace;
    let first = trace.first_origins();
    let tol = give(cx);
    let (sw, sh) = (
        f64::from(cx.sc.screen.width.max(1)),
        f64::from(cx.sc.screen.height.max(1)),
    );
    let mut out = Vec::new();
    let mut judged = 0u64;
    for s in trace.surfaces.iter().filter(|s| s.kind == "popup") {
        if cx.allowed(CheckId::Placement, &s.root) {
            continue;
        }
        // Where its anchor was when it was first painted.
        let mut shown_at: Option<Rect> = None;
        let mut prev: Option<(Sized, Rect)> = None;
        for i in 0..cx.tl.ks.len() {
            let f = cx.tl.frames[i];
            let k = cx.tl.ks[i];
            if k < s.attached || s.detached.is_some_and(|d| k >= d) || f.k != k {
                continue;
            }
            let Some(p) = place_at(s, k) else { continue };
            let (Some(pid), Some(from)) = (p.parent, p.from.as_deref()) else {
                continue;
            };
            let Some(parent) = trace
                .surfaces
                .iter()
                .rev()
                .find(|x| x.id == pid && x.attached <= k)
            else {
                continue;
            };
            let (Some(a), Some(root)) = (cx.tl.row(i, from), cx.tl.row(i, &s.root)) else {
                continue;
            };
            if a.surface != pid || root.surface != s.id {
                continue;
            }
            let Some(now) = sized(f, s.id) else { continue };
            let ar = on_output(&first, a);
            let painted = f.painted.contains(&s.id);
            let was = prev.replace((now, ar));
            let first_seen = *shown_at.get_or_insert(ar);
            if p.tooltip || !painted {
                continue;
            }
            // Settled: its anchor where it was when it was shown, its own
            // and its anchor's size and place as at the frame before.
            if !close(ar, first_seen) || was.is_none_or(|(w, r)| w != now || !close(r, ar)) {
                continue;
            }
            let card = on_output(&first, root);
            let parent_place = place_at(parent, k);
            let Some(side) = opens(p, parent, parent_place) else {
                continue;
            };
            // The gap on the side facing the anchor.
            let facing = side.opposite();
            let gap = if p.margin_set {
                f64::from(p.margin[facing.at()])
            } else if p.attach.is_some() {
                0.0
            } else {
                POPUP_GAP
            };
            // What it opens away from: the whole bar across a bar.
            let main = if parent.kind == "bar" {
                match cx.tl.row(i, &parent.root) {
                    Some(b) => on_output(&first, b),
                    None => continue,
                }
            } else {
                ar
            };
            judged += 1;
            let size = |v: bool| if v { card[3] } else { card[2] };
            let lim = |v: bool| if v { sh } else { sw };
            // Its facing side's place, on the main axis.
            let want = match side {
                Side::Bottom | Side::Right => edge(main, side) + gap,
                Side::Top | Side::Left => edge(main, side) - gap,
            };
            let start = match side {
                Side::Bottom | Side::Right => want,
                Side::Top | Side::Left => want - size(side.vertical()),
            };
            let fits = |at: f64, v: bool| at >= -0.5 && at + size(v) <= lim(v) + 0.5;
            let got = edge(card, facing);
            if fits(start, side.vertical()) && (got - want).abs() > tol {
                out.push(
                    Finding::fail(
                        CheckId::Placement,
                        format!(
                            "anchor: the {} edge of {} lies {:.1} px from where it opens ({:.1}: \
                             {gap:.0} px {} its anchor{})",
                            facing.name(),
                            s.label,
                            got - want,
                            want,
                            side.name(),
                            if parent.kind == "bar" {
                                ", across the bar"
                            } else {
                                ""
                            },
                        ),
                    )
                    .at(k)
                    .node(&s.root)
                    .axis(if side.vertical() { "y" } else { "x" })
                    .got(got, [want - tol, want + tol]),
                );
            }
            // Across: level with its row beside it, else centred on its
            // anchor.
            let across_v = !side.vertical();
            let level = parent.kind == "popup" && p.attach.is_none() && across_v;
            let (want, got) = if level {
                (ar[1], card[1])
            } else if across_v {
                (ar[1] + ar[3] / 2.0 - card[3] / 2.0, card[1])
            } else {
                (ar[0] + ar[2] / 2.0 - card[2] / 2.0, card[0])
            };
            if fits(want, across_v) && (got - want).abs() > tol {
                out.push(
                    Finding::fail(
                        CheckId::Placement,
                        format!(
                            "anchor: {} lies {:.1} px {} from where it opens ({:.1}: {})",
                            s.label,
                            got - want,
                            if across_v { "down" } else { "across" },
                            want,
                            if level {
                                "level with its row's top"
                            } else {
                                "centred on its anchor"
                            }
                        ),
                    )
                    .at(k)
                    .node(&s.root)
                    .axis(if across_v { "y" } else { "x" })
                    .got(got, [want - tol, want + tol]),
                );
            }
        }
    }
    super::stats("placement.anchor", cx, &[("frames", judged)]);
    out
}

/// The sides a layer surface is anchored to, and whether it stretches
/// along both ends of its edge (a bar of no declared length).
fn anchored(kind: &str, p: &PlaceRec) -> (Vec<Side>, bool) {
    if kind == "bar" {
        let e = p.edge.as_deref().and_then(Side::of).unwrap_or(Side::Top);
        let (a, b) = if e.vertical() {
            (Side::Left, Side::Right)
        } else {
            (Side::Top, Side::Bottom)
        };
        let along = if e.vertical() { p.size[0] } else { p.size[1] };
        return (vec![e, a, b], along.is_none());
    }
    let mut sides: Vec<Side> = match p.anchor.as_str() {
        "top" => vec![Side::Top],
        "bottom" => vec![Side::Bottom],
        "left" => vec![Side::Left],
        "right" => vec![Side::Right],
        "top_left" => vec![Side::Top, Side::Left],
        "top_right" => vec![Side::Top, Side::Right],
        "bottom_left" => vec![Side::Bottom, Side::Left],
        "bottom_right" => vec![Side::Bottom, Side::Right],
        _ => Vec::new(),
    };
    // `attach:` anchors a panel to that edge, and not to the opposite
    // one (spec.md 5.1).
    if kind == "panel"
        && let Some(a) = p.attach.as_deref().and_then(Side::of)
    {
        sides.retain(|s| *s != a.opposite());
        if !sides.contains(&a) {
            sides.push(a);
        }
    }
    (sides, false)
}

/// The order the compositor arranges a layer surface in: higher layers
/// first, then the order they were made.
fn rank(p: &PlaceRec) -> u8 {
    match p.layer.as_deref() {
        Some("overlay") => 0,
        Some("bottom") => 2,
        Some("background") => 3,
        _ => 1,
    }
}

/// `margin`: layer surfaces by their margins.
fn margin(cx: &Cx) -> Vec<Finding> {
    let trace = cx.trace;
    let first = trace.first_origins();
    let tol = give(cx);
    let (sw, sh) = (
        f64::from(cx.sc.screen.width.max(1)),
        f64::from(cx.sc.screen.height.max(1)),
    );
    let layer = |s: &SurfaceInfo| matches!(s.kind.as_str(), "bar" | "panel" | "osd");
    let mut out = Vec::new();
    let mut judged = 0u64;
    let mut last: Option<BTreeMap<u32, Sized>> = None;
    for i in 0..cx.tl.ks.len() {
        let f = cx.tl.frames[i];
        let k = cx.tl.ks[i];
        if f.k != k {
            continue;
        }
        // Every layer surface shown, its size and place.
        let shown: Vec<&SurfaceInfo> = trace
            .surfaces
            .iter()
            .filter(|s| {
                layer(s)
                    && s.attached <= k
                    && s.detached.is_none_or(|d| d > k)
                    && f.sizes.contains_key(&s.id)
            })
            .collect();
        let now: BTreeMap<u32, Sized> = shown
            .iter()
            .filter_map(|s| Some((s.id, sized(f, s.id)?)))
            .collect();
        let settled = last.as_ref() == Some(&now);
        last = Some(now);
        if !settled {
            continue;
        }
        // The bars' cards and declared placements, in arranging order.
        let mut bars: Vec<(u8, u32, u32, &SurfaceInfo, &PlaceRec, Rect)> = shown
            .iter()
            .filter(|s| s.kind == "bar")
            .filter_map(|s| {
                let p = place_at(s, k)?;
                let r = cx.tl.row(i, &s.root)?;
                Some((rank(p), s.attached, s.id, *s, p, on_output(&first, r)))
            })
            .collect();
        bars.sort_by_key(|b| (b.0, b.1, b.2));
        for s in &shown {
            if !f.painted.contains(&s.id) || cx.allowed(CheckId::Placement, &s.root) {
                continue;
            }
            let (Some(p), Some(r)) = (place_at(s, k), cx.tl.row(i, &s.root)) else {
                continue;
            };
            if p.k >= k {
                continue;
            }
            let card = on_output(&first, r);
            // The area the zones of the bars before it leave (every bar
            // for a surface with no zone of its own).
            let mut area = [0.0, 0.0, sw, sh];
            let me = (rank(p), s.attached, s.id);
            for b in &bars {
                if b.2 == s.id || (s.kind == "bar" && (b.0, b.1, b.2) > me) {
                    continue;
                }
                let e = b.4.edge.as_deref().and_then(Side::of).unwrap_or(Side::Top);
                let thick = if e.vertical() { b.5[3] } else { b.5[2] };
                let zone = thick + f64::from(b.4.margin[e.at()]);
                if zone <= 0.0 {
                    continue;
                }
                match e {
                    Side::Top => {
                        area[1] += zone;
                        area[3] -= zone;
                    }
                    Side::Bottom => area[3] -= zone,
                    Side::Left => {
                        area[0] += zone;
                        area[2] -= zone;
                    }
                    Side::Right => area[2] -= zone,
                }
            }
            let (sides, stretch) = anchored(&s.kind, p);
            judged += 1;
            let along = |side: Side| {
                s.kind == "bar"
                    && side.vertical()
                        != p.edge
                            .as_deref()
                            .and_then(Side::of)
                            .unwrap_or(Side::Top)
                            .vertical()
            };
            for side in &sides {
                let both = sides.contains(&side.opposite());
                // A box anchored to both ends of an axis is judged only
                // where it stretches: a bar along its edge.
                if both && !(stretch && along(*side)) {
                    continue;
                }
                let m = f64::from(p.margin[side.at()]);
                let want = match side {
                    Side::Top | Side::Left => edge(area, *side) + m,
                    Side::Bottom | Side::Right => edge(area, *side) - m,
                };
                let got = edge(card, *side);
                if (got - want).abs() > tol {
                    out.push(
                        Finding::fail(
                            CheckId::Placement,
                            format!(
                                "margin: the {} edge of {} lies {:.1} px from its {m:.0} px \
                                 margin to the {} edge of the area it is arranged in ({:.1})",
                                side.name(),
                                s.label,
                                got - want,
                                side.name(),
                                edge(area, *side)
                            ),
                        )
                        .at(k)
                        .node(&s.root)
                        .axis(if side.vertical() { "y" } else { "x" })
                        .got(got, [want - tol, want + tol]),
                    );
                }
            }
        }
    }
    super::stats("placement.margin", cx, &[("frames", judged)]);
    out
}

/// A bar runs down its edge (a side bar).
fn bar_vertical(p: &PlaceRec, kind: &str) -> bool {
    kind == "bar" && matches!(p.edge.as_deref(), Some("left" | "right"))
}

/// `centre`: a split's `center` on its split.
fn centre(cx: &Cx) -> Vec<Finding> {
    let trace = cx.trace;
    let tol = give(cx);
    let mut out = Vec::new();
    let mut judged = 0u64;
    let centres: Vec<(&str, &str)> = trace
        .nodes
        .iter()
        .filter(|(_, m)| m.kind == "center")
        .filter_map(|(k, m)| Some((k.as_str(), m.parent.as_deref()?)))
        .filter(|(_, p)| trace.nodes.get(*p).is_some_and(|m| m.kind == "split"))
        .collect();
    for (key, split) in centres {
        if cx.allowed(CheckId::Placement, key) {
            continue;
        }
        let Some(pad) = trace.nodes.get(split).and_then(|m| m.inset) else {
            continue;
        };
        if trace.nodes.get(key).is_some_and(|m| m.inset.is_none()) {
            // Its margin is not known: where its box lies in its track is
            // not either.
            continue;
        }
        let margin = trace
            .nodes
            .get(key)
            .and_then(|m| m.inset)
            .unwrap_or_default();
        for i in 0..cx.tl.ks.len() {
            let (Some(c), Some(sp)) = (cx.tl.row(i, key), cx.tl.row(i, split)) else {
                continue;
            };
            if c.surface != sp.surface || !cx.tl.visible(i, c) {
                continue;
            }
            // A side bar's split runs down.
            let k = cx.tl.ks[i];
            let down = trace
                .surfaces
                .iter()
                .rev()
                .find(|s| s.id == sp.surface && s.attached <= k)
                .and_then(|s| Some((s, place_at(s, k)?)))
                .is_some_and(|(s, p)| bar_vertical(p, &s.kind));
            let l = sp.laid.map(f64::from);
            let b = c.laid.map(f64::from);
            let pad = pad.map(f64::from);
            let m = margin.map(f64::from);
            // Its box with its margin, centred on the content box.
            let (want, got) = if down {
                let (top, bottom) = (l[1] + pad[0], l[1] + l[3] - pad[2]);
                (
                    (top + bottom) / 2.0,
                    (b[1] - m[0] + b[1] + b[3] + m[2]) / 2.0,
                )
            } else {
                let (left, right) = (l[0] + pad[3], l[0] + l[2] - pad[1]);
                (
                    (left + right) / 2.0,
                    (b[0] - m[3] + b[0] + b[2] + m[1]) / 2.0,
                )
            };
            judged += 1;
            if (got - want).abs() > tol {
                out.push(
                    Finding::fail(
                        CheckId::Placement,
                        format!(
                            "centre: the centre of `center` lies {:.1} px off its split's centre \
                             ({:.1}), which it keeps whatever the other two hold",
                            got - want,
                            want
                        ),
                    )
                    .at(k)
                    .node(key)
                    .axis(if down { "y" } else { "x" })
                    .got(got, [want - tol, want + tol]),
                );
            }
        }
    }
    super::stats("placement.centre", cx, &[("frames", judged)]);
    out
}

/// Where `fit` puts a `sw × sh` source in a `w × h` box: its offset and
/// size (spec.md, `fit:`). `None` for `none` or a fit not known.
fn fitted(sw: f64, sh: f64, w: f64, h: f64, fit: &str) -> Option<[f64; 4]> {
    if !(sw > 0.0 && sh > 0.0) {
        return None;
    }
    let (dw, dh) = match fit {
        "fill" => (w, h),
        "contain" | "cover" => {
            let (kx, ky) = (w / sw, h / sh);
            let k = if fit == "contain" {
                kx.min(ky)
            } else {
                kx.max(ky)
            };
            (sw * k, sh * k)
        }
        _ => return None,
    };
    Some([(w - dw) / 2.0, (h - dh) / 2.0, dw, dh])
}

/// `fit`: each image's source where its fit puts it.
fn fit(cx: &Cx) -> Vec<Finding> {
    let tol = give(cx);
    let mut out = Vec::new();
    let mut judged = 0u64;
    for f in &cx.trace.frames {
        for im in &f.imgs {
            if cx.allowed(CheckId::Placement, &im.key) {
                continue;
            }
            let [rx, ry, rw, rh] = im.rect.map(f64::from);
            let [dx, dy, dw, dh] = im.dest.map(f64::from);
            let [sw, sh] = im.src;
            let (pw, ph) = (f64::from(im.px[0]), f64::from(im.px[1]));
            if !(pw > 0.0 && ph > 0.0 && rw > 0.0 && rh > 0.0) {
                continue;
            }
            let (Some(want), Some(inpx)) = (
                fitted(sw, sh, rw, rh, &im.fit),
                fitted(sw, sh, pw, ph, &im.pfit),
            ) else {
                continue;
            };
            // The source as drawn: its place in the pixmap, scaled as the
            // pixmap is.
            let (kx, ky) = (dw / pw, dh / ph);
            let got = [
                dx + inpx[0] * kx - rx,
                dy + inpx[1] * ky - ry,
                inpx[2] * kx,
                inpx[3] * ky,
            ];
            judged += 1;
            let names = ["x", "y", "w", "h"];
            if let Some(a) = (0..4).find(|a| (got[*a] - want[*a]).abs() > tol) {
                out.push(
                    Finding::fail(
                        CheckId::Placement,
                        format!(
                            "fit: its source shows at [{:.1}, {:.1}, {:.1}, {:.1}] in its box, \
                             where `fit: {}` puts it at [{:.1}, {:.1}, {:.1}, {:.1}]{}",
                            got[0],
                            got[1],
                            got[2],
                            got[3],
                            im.fit,
                            want[0],
                            want[1],
                            want[2],
                            want[3],
                            if im.stand_in {
                                " (a decode for another size stands in)"
                            } else {
                                ""
                            }
                        ),
                    )
                    .at(f.k)
                    .node(&im.key)
                    .axis(names[a])
                    .got(got[a], [want[a] - tol, want[a] + tol]),
                );
            }
        }
    }
    super::stats("placement.fit", cx, &[("images", judged)]);
    out
}

/// (eval-rules-6) `pose`: each posed surface lies where its pose's
/// offset moves it from its rest place (see the module's `pose`).
fn pose(cx: &Cx) -> Vec<Finding> {
    // Margins are whole logical pixels: an offset rounded to them.
    const HALF: f64 = 0.5 + 1e-3;
    let trace = cx.trace;
    let mut out = Vec::new();
    let mut judged = 0u64;
    for f in &trace.frames {
        for (s, p) in &f.poses {
            if !f.painted.contains(s) {
                continue;
            }
            let Some(info) = trace.surfaces.iter().find(|i| i.id == *s) else {
                continue;
            };
            if cx.allowed(CheckId::Placement, &info.root) {
                continue;
            }
            judged += 1;
            let got = f.shifts.get(s).copied().unwrap_or([0.0, 0.0]);
            for (a, (g, w)) in got.iter().zip([p.x, p.y]).enumerate() {
                let (g, w) = (f64::from(*g), f64::from(w));
                if (g - w).abs() <= HALF {
                    continue;
                }
                let axis = ["x", "y"][a];
                out.push(
                    Finding::fail(
                        CheckId::Placement,
                        format!(
                            "pose: {}'s pose moves it {w:.1} px on {axis} from where it lies at \
                             rest, but the margins sent for it put it {g:.1} px from there",
                            info.label
                        ),
                    )
                    .at(f.k)
                    .node(&info.root)
                    .axis(axis)
                    .got(g, [w - HALF, w + HALF]),
                );
            }
        }
    }
    super::stats("placement.pose", cx, &[("frames", judged)]);
    out
}

//! (eval-rules) `colour`: a colour that springs stays on its way.
//!
//! design.md springs colours in OKLab ("Tokens": "A theme swap springs the
//! palette in OKLab"; "Springs everywhere": `$motion.effects` for colour
//! and opacity), with alpha premultiplied so a fade to or from transparent
//! keeps its colour's hue (decisions.md 2026-10-04 render, "lerp_oklab
//! interpolates premultiplied OKLab", and wave3-theme t2; strand-scene
//! motion.rs `color_channels`). A spring moves every channel of a value
//! along one curve, so a colour sent from one value to another is drawn,
//! frame by frame, on the straight line between the two in premultiplied
//! OKLab: never through a colour neither end has (a fade from transparent
//! to yellow that passes through a dark olive, a blue-to-yellow change
//! through sRGB grey).
//!
//! It holds a node's own solid fill (`bg`), border and text colour, each
//! where the node's own colour of that kind springs (`probe::Motions`
//! `FILL_SPRING`, `LINE_SPRING`, `INK_SPRING`), over a segment, from the
//! colour drawn at its start to the one it rests at. A colour derived from
//! the token graph each frame (a theme swap, the contrast guard solving a
//! text's lightness) has no spring of its own and is not held; nor is one
//! retargeted in flight (logic's diff or input reaching the renderer after
//! the colour left its start, or a colour still springing from an earlier
//! segment when this one's change reaches it: a spring keeps its
//! velocity, so its way curves), one that keyframes or time drive, or a
//! gradient. A frame's
//! colour, as the recording writes it (straight-alpha sRGB to 1/255), is
//! compared with the nearest point of that line drawn as the raster
//! draws it (each channel clamped), premultiplied: within
//! [`STEPS`] steps of 1/255 on every channel.
//!
//! (eval-rules-3) Along that line it goes no further past either end
//! than its spring overshoots (`CurveRec::overshoot` of the curve its
//! own spring's samples name; half the way when no sample names one, or
//! for an elastic easing): a colour on the line but past its end is a
//! colour neither end has as surely as one off it.
//!
//! (eval-finish) A colour whose own spring is sent to a token a theme
//! swap re-resolves in the same change (a tile lit with `$accent` by the
//! click that swaps the palette) chases a moving target: design.md
//! re-evaluates every derived token from the springing roots each frame,
//! and a spring retargeted keeps its velocity, so its way bends toward
//! where the target has gone. A spring is a linear filter of its target:
//! from rest, what it draws is a weighted sum of where it started and
//! where its target has been, the weights summing to one and none
//! negative at or above critical damping. So such a colour is held to
//! the fan from its start to the target's way so far (the targets its
//! own spring's samples record, `SpringRec::to`, from the segment's start
//! to the frame), with the same overshoot bound along each ray, and only
//! when that way ends where the colour comes to rest; a target that does
//! not move leaves the straight way as it was. (eval-finish review) The
//! targets are held to a way of their own first: a token the swap moves
//! goes on one straight way in premultiplied OKLab, so targets that leave
//! the line from the first to the last by more than [`STEPS`] are not
//! followed and the straight way holds the colour. And along each ray
//! the share is at most what its own spring reaches from rest by the
//! frame's time ([`reach`]): a spring from rest has given its targets so
//! far exactly the weight its way to a still target has covered, so a
//! colour drawn at its target from the first frame is ahead of its
//! spring.
//!
//! - **palette**: a theme swap springs each palette root along the curve
//!   a colour takes in the table it sent ("Springs everywhere": colour
//!   and opacity use `$motion.effects`, unless the swap names its own
//!   transition), which the trace records with each root's samples
//!   (`SpringRec::ask`); a root sprung along another curve (set E's
//!   theme-swap mutant: a spatial spring that rings past the new
//!   palette) fails, once per root. `continuity` holds the roots'
//!   samples to their way as it does any spring's.

use strand_render::probe::Motions;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;
use crate::trace::Row;

/// How far, in steps of 1/255 per premultiplied channel, a drawn colour
/// may be from its way: the recording's quantum on both ends and the
/// frame, with room for sRGB rounding.
pub const STEPS: f64 = 3.0;

/// The paint fields held and the spring that moves each.
const FIELDS: [(&str, Motions); 3] = [
    ("bg", Motions::FILL_SPRING),
    ("bd", Motions::LINE_SPRING),
    ("fg", Motions::INK_SPRING),
];

fn field<'a>(r: &'a Row, name: &str) -> &'a [String] {
    match (&r.paint, name) {
        (Some(p), "bg") => &p.bg,
        (Some(p), "bd") => &p.bd,
        (Some(p), "fg") => &p.fg,
        _ => &[],
    }
}

/// `rrggbbaa` as straight-alpha channels in `0..=1`.
pub fn rgba(s: &str) -> Option<[f64; 4]> {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let ch = |i: usize| {
        u8::from_str_radix(&s[i..i + 2], 16)
            .ok()
            .map(|v| f64::from(v) / 255.0)
    };
    Some([ch(0)?, ch(2)?, ch(4)?, ch(6)?])
}

fn to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn to_srgb(c: f64) -> f64 {
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB to OKLab (Björn Ottosson's matrices).
pub fn oklab([r, g, b]: [f64; 3]) -> [f64; 3] {
    let (r, g, b) = (to_linear(r), to_linear(g), to_linear(b));
    let l = 0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b;
    let m = 0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b;
    let s = 0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b;
    let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
    [
        0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s,
    ]
}

/// OKLab to sRGB, each channel clamped as the raster draws it.
pub fn srgb([l, a, b]: [f64; 3]) -> [f64; 3] {
    let l_ = l + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = l - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = l - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let r = 4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s;
    let g = -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s;
    let b = -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s;
    [r, g, b].map(|c| to_srgb(c.clamp(0.0, 1.0)).clamp(0.0, 1.0))
}

/// A straight-alpha colour as premultiplied OKLab plus alpha, the space
/// a colour springs in.
pub fn premultiplied(c: [f64; 4]) -> [f64; 4] {
    let [l, a, b] = oklab([c[0], c[1], c[2]]);
    [l * c[3], a * c[3], b * c[3], c[3]]
}

/// A point of premultiplied OKLab drawn as the raster draws it:
/// premultiplied sRGB, alpha last.
fn drawn(p: [f64; 4]) -> [f64; 4] {
    let a = p[3].clamp(0.0, 1.0);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let [r, g, b] = srgb([p[0] / a, p[1] / a, p[2] / a]);
    [r * a, g * a, b * a, a]
}

/// How far `c` (straight-alpha) is from the way between `from` and `to`
/// in premultiplied OKLab, in steps of 1/255 of a premultiplied sRGB
/// channel, at the nearest point of the line through them (a share from
/// -0.5 to 1.5 of the way: a bouncy spring overshoots its end).
pub fn off_way(c: [f64; 4], from: [f64; 4], to: [f64; 4]) -> f64 {
    off_way_in(c, from, to, (-0.5, 1.5))
}

/// [`off_way`] with the share of the way bounded to `lo..=hi`.
pub fn off_way_in(c: [f64; 4], from: [f64; 4], to: [f64; 4], (lo, hi): (f64, f64)) -> f64 {
    let (p0, p1) = (premultiplied(from), premultiplied(to));
    let at = |s: f64| -> f64 {
        let p: [f64; 4] = std::array::from_fn(|i| p0[i] + s * (p1[i] - p0[i]));
        let d = drawn(p);
        let e = [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
        (0..4).map(|i| (d[i] - e[i]).abs()).fold(0.0, f64::max) * 255.0
    };
    // Coarse (the bounds among the points), then fine around the nearest.
    let mut best = (f64::INFINITY, lo);
    let n = ((hi - lo) * 100.0).ceil().max(1.0) as i32;
    for i in 0..=n {
        let s = lo + (hi - lo) * f64::from(i) / f64::from(n);
        let e = at(s);
        if e < best.0 {
            best = (e, s);
        }
    }
    let centre = best.1;
    for i in -40..=40 {
        let s = (centre + f64::from(i) / 4000.0).clamp(lo, hi);
        let e = at(s);
        if e < best.0 {
            best = (e, s);
        }
    }
    best.0
}

/// (eval-finish) How far `c` (straight-alpha) is from the fan between
/// `from` and the way its target took, `targets` (premultiplied OKLab
/// plus alpha, as a colour spring's channels are, in order): the nearest
/// point `from + s · (P − from)` with `P` on the line through the targets
/// one after another and `s` within `lo..=hi`, in steps of 1/255 of a
/// premultiplied sRGB channel as [`off_way`] measures. A spring from rest
/// chasing a moving target draws such a point (a weighted sum of where it
/// started and where its target has been); with one target it is
/// [`off_way_in`]'s line. Infinite with no target.
pub fn off_fan(c: [f64; 4], from: [f64; 4], targets: &[[f64; 4]], (lo, hi): (f64, f64)) -> f64 {
    let Some(first) = targets.first() else {
        return f64::INFINITY;
    };
    let p0 = premultiplied(from);
    let e = [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
    let segs: Vec<([f64; 4], [f64; 4])> = if targets.len() == 1 {
        vec![(*first, *first)]
    } else {
        targets.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let at = |j: usize, u: f64, s: f64| -> f64 {
        let (a, b) = segs[j];
        let p: [f64; 4] = std::array::from_fn(|i| a[i] + u * (b[i] - a[i]));
        let d = drawn(std::array::from_fn(|i| p0[i] + s * (p[i] - p0[i])));
        (0..4).map(|i| (d[i] - e[i]).abs()).fold(0.0, f64::max) * 255.0
    };
    // Coarse (a fiftieth of the way along each ray, an eighth of each
    // step of the target's way), then fine around the nearest few.
    const NU: i32 = 8;
    let ns = ((hi - lo) * 50.0).ceil().max(1.0) as i32;
    let mut near: Vec<(f64, usize, f64, f64)> = Vec::new();
    for j in 0..segs.len() {
        for ui in 0..=NU {
            let u = f64::from(ui) / f64::from(NU);
            for si in 0..=ns {
                let s = lo + (hi - lo) * f64::from(si) / f64::from(ns);
                let d = at(j, u, s);
                if near.len() < 4 || d < near[3].0 {
                    let i = near.partition_point(|n| n.0 <= d);
                    near.insert(i, (d, j, u, s));
                    near.truncate(4);
                }
            }
        }
    }
    let mut best = f64::INFINITY;
    for (d, j, u, s) in near {
        best = best.min(d);
        for du in -8..=8 {
            let u = (u + f64::from(du) / 64.0).clamp(0.0, 1.0);
            for ds in -20..=20 {
                let s = (s + f64::from(ds) / 1000.0).clamp(lo, hi);
                best = best.min(at(j, u, s));
            }
        }
    }
    best
}

/// (eval-finish) Which way a colour left: off every way, ahead of
/// where its spring can be by then (the time since the change, s, and
/// the furthest share of the way its spring reaches by then), or past
/// its end.
#[derive(Clone, Copy, Debug)]
enum Way {
    Off,
    Ahead(f64, f64),
    Past,
}

/// (eval-finish) How far a point of premultiplied OKLab `p` is from the
/// line between `a` and `b` (a share `lo..=hi` of the way), each drawn
/// as the raster draws it, in steps of 1/255.
pub fn off_line(p: [f64; 4], a: [f64; 4], b: [f64; 4], (lo, hi): (f64, f64)) -> f64 {
    let e = drawn(p);
    let at = |s: f64| -> f64 {
        let q: [f64; 4] = std::array::from_fn(|i| a[i] + s * (b[i] - a[i]));
        let d = drawn(q);
        (0..4).map(|i| (d[i] - e[i]).abs()).fold(0.0, f64::max) * 255.0
    };
    let mut best = (f64::INFINITY, lo);
    let n = ((hi - lo) * 100.0).ceil().max(1.0) as i32;
    for i in 0..=n {
        let s = lo + (hi - lo) * f64::from(i) / f64::from(n);
        let d = at(s);
        if d < best.0 {
            best = (d, s);
        }
    }
    let centre = best.1;
    for i in -40..=40 {
        let s = (centre + f64::from(i) / 4000.0).clamp(lo, hi);
        best.0 = best.0.min(at(s));
    }
    best.0
}

/// (eval-finish) The furthest share of its way a motion along `c`
/// reaches from rest within `secs` s of starting toward a still target:
/// a unit-mass damped oscillator released one way short of it
/// (`continuity::spring`, written from the oscillator), up to its first
/// overshoot past the end; a timed curve's easing, sampled.
pub fn reach(c: crate::trace::CurveRec, secs: f64) -> f64 {
    use crate::trace::CurveRec;
    let secs = secs.max(0.0);
    match c {
        CurveRec::Instant => 1.0,
        CurveRec::Spring { k, z } => {
            let (k, z) = (f64::from(k), f64::from(z));
            let at = |t: f64| 1.0 - super::continuity::spring(k, z, 1.0, 0.0, t).0;
            if z >= 1.0 {
                return at(secs);
            }
            // Rising until its first peak, then never further.
            let wd = k.max(1e-6).sqrt() * (1.0 - z * z).sqrt();
            at(secs.min(std::f64::consts::PI / wd))
        }
        CurveRec::Timed { d, ease } => {
            let d = (d as f64 / 1e9).max(1e-6);
            let easing = ease.easing();
            const N: u32 = 256;
            (0..=N)
                .map(|i| {
                    let u = (secs * f64::from(i) / f64::from(N) / d).min(1.0) as f32;
                    f64::from(strand_scene::motion::ease(easing, u))
                })
                .fold(0.0, f64::max)
        }
    }
}

/// A colour spring's target as its sample records it (`SpringRec::to`:
/// premultiplied OKLab and alpha last, after a border's width).
fn target_of(to: &[f32]) -> Option<[f64; 4]> {
    let t = to.get(to.len().checked_sub(4)?..)?;
    let t: [f64; 4] = std::array::from_fn(|i| f64::from(t[i]));
    t.iter().all(|v| v.is_finite()).then_some(t)
}

/// How far a point of premultiplied OKLab, drawn as the raster draws it,
/// is from `c` (straight-alpha), in steps of 1/255.
fn steps_to(p: [f64; 4], c: [f64; 4]) -> f64 {
    let d = drawn(p);
    let e = [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
    (0..4).map(|i| (d[i] - e[i]).abs()).fold(0.0, f64::max) * 255.0
}

/// How far, on some premultiplied OKLab channel, a colour spring's
/// target moves within its way before it counts as moving (about a third
/// of a step of 1/255 in lightness: below that the straight way is
/// within the tolerance of the bent one).
const TARGET_EPS: f64 = 1.0e-3;

/// The spring prop that moves each paint field.
fn prop_of(name: &str) -> &str {
    match name {
        "bg" => "bg",
        "bd" => "border",
        _ => "color",
    }
}

pub fn run(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let mut out = palette(cx);
    // (eval-rules-3) Ways held, and of those bounded by their curve's
    // overshoot.
    let counts = std::cell::Cell::new([0u64; 3]);
    for (_, seg, s) in cx.segments() {
        let (Some(bi), Some(ei)) = (tl.at(s), tl.at(seg.e)) else {
            continue;
        };
        for (key, b) in &tl.screens[bi] {
            let Some(e) = tl.row(ei, key) else { continue };
            if cx.allowed(CheckId::Colour, key) {
                continue;
            }
            for (name, bit) in FIELDS {
                // (eval-rules-7) Fills that come and go on the way (a
                // default fill an authored one takes the place of, a
                // state layer) are not one fill a slot at a time: what is
                // drawn is their composite.
                if name == "bg" && layers_change(cx, key, (bi, ei)) {
                    if let Some(f) = composite(cx, key, (bi, ei), &counts) {
                        out.push(f.in_segment(&seg.name));
                    }
                    continue;
                }
                let (fb, fe) = (field(b, name), field(e, name));
                for (slot, (cb, ce)) in fb.iter().zip(fe).enumerate() {
                    if cb == ce {
                        continue;
                    }
                    let (Some(from), Some(to)) = (rgba(cb), rgba(ce)) else {
                        continue;
                    };
                    if let Some(f) = transition(
                        cx,
                        key,
                        name,
                        bit,
                        slot,
                        (bi, ei),
                        (cb, ce),
                        (from, to),
                        &counts,
                    ) {
                        out.push(f.in_segment(&seg.name));
                    }
                }
            }
        }
    }
    let [ways, bounded, composites] = counts.get();
    super::stats(
        "colour.way",
        cx,
        &[
            ("ways", ways),
            ("bounded", bounded),
            ("composites", composites),
        ],
    );
    first_per_node(out)
}

/// The finding of `key`'s colour in `name[slot]`, sent from `cb` to `ce`
/// over the frames `(bi, ei]`, if it leaves its way.
#[allow(clippy::too_many_arguments)]
fn transition(
    cx: &Cx,
    key: &str,
    name: &str,
    bit: Motions,
    slot: usize,
    (bi, ei): (usize, usize),
    (cb, ce): (&str, &str),
    (from, to): ([f64; 4], [f64; 4]),
    counts: &std::cell::Cell<[u64; 3]>,
) -> Option<Finding> {
    let tl = &cx.tl;
    // Held only from rest: a colour still springing when the segment
    // starts (an earlier segment set it moving) has a velocity, so a
    // change now (to a third colour, or back) curves off the line from
    // where it was drawn, and the segment's first change is then itself
    // a retarget.
    if tl.row(bi, key).is_none_or(|r| Motions(r.mot).contains(bit)) {
        return None;
    }
    // The frames it is on its way: from the first that left `cb` to the
    // one before it arrives at `ce`.
    let mut way: Vec<(usize, &Row, &str)> = Vec::new();
    let mut left = false;
    for i in bi + 1..=ei {
        let r = tl.row(i, key)?;
        let c = field(r, name).get(slot)?.as_str();
        if !left && c == cb {
            continue;
        }
        left = true;
        if c == ce {
            break;
        }
        way.push((i, r, c));
    }
    // Held only when its own spring moves it the whole way, nothing
    // drives it by time, and no step retargets it in flight.
    let own = way.iter().all(|(_, r, _)| {
        let m = Motions(r.mot);
        m.contains(bit) && !m.intersects(Motions::KEYFRAMES | Motions::TIME)
    });
    let retargeted = way
        .iter()
        .skip(1)
        .any(|(i, _, _)| tl.frames.get(*i).is_some_and(|f| f.driven));
    if way.is_empty() || !own || retargeted {
        return None;
    }
    // (eval-rules-3) How far past either end its spring takes it: the
    // overshoot of the curve its samples over the way name.
    let prop = prop_of(name);
    let ov = way
        .iter()
        .find_map(|(i, _, _)| {
            tl.frames[*i]
                .sp
                .iter()
                .find(|r| r.key == key && r.of == prop)
                .map(|r| r.c.overshoot())
        })
        .flatten();
    let bound = ov.map_or((-0.5, 1.5), |o| (-o, 1.0 + o));
    let [ways, bounded, composites] = counts.get();
    counts.set([ways + 1, bounded + u64::from(ov.is_some()), composites]);
    // (eval-finish) Where its own spring was sent at each frame from the
    // segment's start: a target a theme swap re-resolves moves while it
    // springs. Held to the fan toward the target's way when that way
    // moves and ends where the colour rests, else to the straight way.
    let sent: Vec<Option<[f64; 4]>> = (0..=ei)
        .map(|i| {
            (i > bi)
                .then(|| {
                    tl.frames[i]
                        .sp
                        .iter()
                        .find(|r| r.key == key && r.of == prop)
                        .and_then(|r| target_of(&r.to))
                })
                .flatten()
        })
        .collect();
    let last_on_way = way.last().map_or(ei, |(i, _, _)| *i);
    let targets_to = |i: usize| -> Vec<[f64; 4]> {
        let mut t: Vec<[f64; 4]> = Vec::new();
        for p in sent[..=i].iter().flatten() {
            if t.last()
                .is_none_or(|q| (0..4).any(|c| (p[c] - q[c]).abs() > TARGET_EPS / 10.0))
            {
                t.push(*p);
            }
        }
        t
    };
    let all = targets_to(last_on_way);
    let moving = all.iter().any(|p| {
        all.first()
            .is_some_and(|q| (0..4).any(|c| (p[c] - q[c]).abs() > TARGET_EPS))
    });
    let leads = moving && all.last().is_some_and(|p| steps_to(*p, to) <= STEPS);
    // (eval-finish) The targets are a token's value as the swap moves it,
    // which goes from where it was to where it rests on one straight way
    // in premultiplied OKLab, as any colour a spring sends does (a palette
    // root's, held by `palette` and `continuity`; a node drawn with the
    // token is held to it here). Recorded targets that leave that way by
    // more than the tolerance are not a way the fan may follow: the
    // straight way holds the colour, and the finding says so.
    let wander = match (all.first(), all.last()) {
        (Some(a), Some(b)) if leads => all
            .iter()
            .map(|p| off_line(*p, *a, *b, (-0.5, 1.5)))
            .fold(0.0, f64::max),
        _ => 0.0,
    };
    let chases = leads && wander <= STEPS;
    // (eval-finish) How far along each ray its own spring can be by a
    // frame: a spring from rest chasing any target draws `from + s·(P̄ −
    // from)` with `s` its own way from rest to a still target by then
    // (the weights it has given its targets so far), so `s` is at most
    // the furthest that way reaches by the frame's time from the
    // segment's start (a change starts no earlier than that frame).
    let curve = way.iter().find_map(|(i, _, _)| {
        tl.frames[*i]
            .sp
            .iter()
            .find(|r| r.key == key && r.of == prop)
            .map(|r| r.c)
    });
    let t_b = tl.frames[bi].t_ns;
    let mut worst: Option<(f64, u32, &str, Way)> = None;
    for (i, _, c) in &way {
        let Some(at) = rgba(c) else { continue };
        let (e, kind) = if chases {
            let t = targets_to(*i);
            let secs = tl.frames[*i].t_ns.saturating_sub(t_b) as f64 / 1e9;
            let reach = curve.map_or(bound.1, |cv| reach(cv, secs).min(bound.1));
            let e = off_fan(at, from, &t, (bound.0, reach.max(bound.0)));
            let kind = if off_fan(at, from, &t, (-0.5, 1.5)) > STEPS {
                Way::Off
            } else if off_fan(at, from, &t, bound) <= STEPS {
                Way::Ahead(secs, reach)
            } else {
                Way::Past
            };
            (e, kind)
        } else {
            let e = off_way_in(at, from, to, bound);
            let kind = if off_way(at, from, to) > STEPS {
                Way::Off
            } else {
                Way::Past
            };
            (e, kind)
        };
        if e > STEPS && worst.is_none_or(|w| e > w.0) {
            worst = Some((e, tl.ks[*i], c, kind));
        }
    }
    let (e, k, c, kind) = worst?;
    let ovp = ov.unwrap_or(0.0) * 100.0;
    let msg = match (kind, chases) {
        (Way::Off, false) => format!(
            "off its way: its {name} springing from {cb} to {ce} was drawn {c}, {e:.1} steps \
             of 1/255 from the way between them in premultiplied OKLab"
        ),
        (Way::Past, false) if ov.is_none() => format!(
            "off its way: its {name} springing from {cb} to {ce} was drawn {c}, {e:.1} steps \
             of 1/255 from the way between them in premultiplied OKLab"
        ),
        (_, false) => format!(
            "past its end: its {name} springing from {cb} to {ce} was drawn {c}, on the way \
             between them in premultiplied OKLab but {e:.1} steps of 1/255 past where its \
             spring's overshoot ({ovp:.1}% of the way) takes it"
        ),
        (Way::Off, true) => format!(
            "off its way: its {name} springing from {cb} toward a moving target that rests at \
             {ce} was drawn {c}, {e:.1} steps of 1/255 from every way between where it started \
             and where its target had been, in premultiplied OKLab"
        ),
        (Way::Past, true) if ov.is_none() => format!(
            "off its way: its {name} springing from {cb} toward a moving target that rests at \
             {ce} was drawn {c}, {e:.1} steps of 1/255 from every way between where it started \
             and where its target had been, in premultiplied OKLab"
        ),
        (Way::Past, true) => format!(
            "past its end: its {name} springing from {cb} toward a moving target that rests at \
             {ce} was drawn {c}, between where it started and where its target had been in \
             premultiplied OKLab but {e:.1} steps of 1/255 past where its spring's overshoot \
             ({ovp:.1}% of the way) takes it"
        ),
        (Way::Ahead(secs, reach), _) => format!(
            "ahead of its spring: its {name} springing from {cb} toward a moving target that \
             rests at {ce} was drawn {c} {:.1} ms after the change, {e:.1} steps of 1/255 from \
             every way between where it started and where its target had been that its spring \
             reaches by then in premultiplied OKLab (at most {:.1}% of the way from rest)",
            secs * 1e3,
            reach * 100.0
        ),
    };
    let msg = if wander > STEPS {
        format!(
            "{msg}; its target, recorded moving to where it rests, left its own straight way \
             by {wander:.1} steps, so the straight way holds it"
        )
    } else {
        msg
    };
    Some(
        Finding::fail(CheckId::Colour, msg)
            .at(k)
            .node(key)
            .axis(name)
            .got(e, [0.0, STEPS]),
    )
}

/// (eval-rules-7) `key`'s fills change in number between the frames
/// `bi..=ei`: a layer comes or goes on the way.
fn layers_change(cx: &Cx, key: &str, (bi, ei): (usize, usize)) -> bool {
    let n = |i: usize| cx.tl.row(i, key).map(|r| field(r, "bg").len());
    let first = n(bi);
    (bi..=ei).any(|i| n(i).is_some_and(|m| Some(m) != first))
}

/// `c` (straight-alpha) over `under` (premultiplied), as the raster
/// composites a fill: source over, in premultiplied sRGB.
fn over(under: [f64; 4], c: [f64; 4]) -> [f64; 4] {
    let a = c[3];
    [
        c[0] * a + under[0] * (1.0 - a),
        c[1] * a + under[1] * (1.0 - a),
        c[2] * a + under[2] * (1.0 - a),
        a + under[3] * (1.0 - a),
    ]
}

/// (eval-rules-7) What `key`'s fills draw at timeline frame `i`, in
/// premultiplied sRGB: its own fills in paint order over its ancestors'
/// (each one's fills whose drawn box holds its centre, outermost first),
/// over nothing. `None` when it has no row there or a fill is not a solid
/// colour (a gradient).
fn composite_at(cx: &Cx, key: &str, i: usize) -> Option<[f64; 4]> {
    let tl = &cx.tl;
    let r = tl.row(i, key)?;
    let [x, y, w, h] = r.drawn;
    let (mx, my) = (x + w / 2.0, y + h / 2.0);
    let mut chain = Vec::new();
    let mut at = cx.parent(key);
    while let Some(p) = at {
        if chain.len() > 256 {
            break;
        }
        if let Some(pr) = tl.row(i, p).filter(|pr| pr.surface == r.surface) {
            let [px, py, pw, ph] = pr.drawn;
            if mx >= px && mx <= px + pw && my >= py && my <= py + ph {
                chain.push(pr);
            }
        }
        at = cx.parent(p);
    }
    let mut c = [0.0; 4];
    for row in chain.iter().rev().copied().chain(std::iter::once(r)) {
        for f in field(row, "bg") {
            c = over(c, rgba(f)?);
        }
    }
    Some(c)
}

/// (eval-rules-7) `composite`: where a node's fills come and go while one
/// of them springs (set J's drawer button: its default fill gave way to
/// `$accent.container` springing in from clear, with a hover state layer
/// over it), no one fill's way is what the eye sees; their composite over
/// what is drawn under the node is. It goes from where the segment found
/// it to where it rests on every channel without turning back: never
/// further from its end than the frame before (while short of its end),
/// and never outside its two ends by more than the spring overshoots, by
/// [`STEPS`]. A default fill that vanishes at once while its replacement
/// springs in from clear jumps away from both ends and fails.
fn composite(
    cx: &Cx,
    key: &str,
    (bi, ei): (usize, usize),
    counts: &std::cell::Cell<[u64; 3]>,
) -> Option<Finding> {
    let tl = &cx.tl;
    // As a fill's way: from rest, its own fill spring moving it the
    // whole way, nothing driving it by time, no step retargeting it, its
    // opacity the same throughout (a fade is `continuity`'s).
    if tl
        .row(bi, key)
        .is_none_or(|r| Motions(r.mot).contains(Motions::FILL_SPRING))
    {
        return None;
    }
    let (b, e) = (tl.row(bi, key)?, tl.row(ei, key)?);
    let (lb, le) = (field(b, "bg"), field(e, "bg"));
    let mut way: Vec<usize> = Vec::new();
    let mut left = false;
    for i in bi + 1..=ei {
        let r = tl.row(i, key)?;
        let l = field(r, "bg");
        if !left && l == lb {
            continue;
        }
        left = true;
        if l == le {
            break;
        }
        way.push(i);
    }
    let own = way.iter().all(|i| {
        tl.row(*i, key).is_some_and(|r| {
            let m = Motions(r.mot);
            m.contains(Motions::FILL_SPRING)
                && !m.intersects(Motions::KEYFRAMES | Motions::TIME)
                && (r.op - b.op).abs() < 1e-3
        })
    });
    let retargeted = way
        .iter()
        .skip(1)
        .any(|i| tl.frames.get(*i).is_some_and(|f| f.driven));
    if way.is_empty() || !own || retargeted || (e.op - b.op).abs() >= 1e-3 {
        return None;
    }
    let ov = way
        .iter()
        .find_map(|i| {
            tl.frames[*i]
                .sp
                .iter()
                .find(|r| r.key == key && r.of == "bg")
                .map(|r| r.c.overshoot())
        })
        .flatten()
        .unwrap_or(0.5);
    let (c0, c1) = (composite_at(cx, key, bi)?, composite_at(cx, key, ei)?);
    let [ways, bounded, composites] = counts.get();
    counts.set([ways, bounded, composites + 1]);
    let step = STEPS / 255.0;
    let mut prev = c0;
    for i in way.iter().copied().chain(std::iter::once(ei)) {
        let c = composite_at(cx, key, i)?;
        for ch in 0..4 {
            let (a, z) = (c0[ch], c1[ch]);
            let give = step + ov * (z - a).abs();
            let outside = c[ch] < a.min(z) - give || c[ch] > a.max(z) + give;
            // Short of its end (on the start's side of it), it never
            // moves further from it.
            let short = (z - prev[ch]) * (z - a) > 0.0;
            let back = short && (z - c[ch]).abs() > (z - prev[ch]).abs() + step;
            if outside || back {
                let hex = |c: [f64; 4]| {
                    let a = c[3];
                    let st = |v: f64| if a > 0.0 { v / a } else { 0.0 };
                    format!(
                        "{:02x}{:02x}{:02x}{:02x}",
                        (st(c[0]) * 255.0).round() as u8,
                        (st(c[1]) * 255.0).round() as u8,
                        (st(c[2]) * 255.0).round() as u8,
                        (a * 255.0).round() as u8
                    )
                };
                let how = if outside {
                    "outside both its ends"
                } else {
                    "turning back from its end"
                };
                return Some(
                    Finding::fail(
                        CheckId::Colour,
                        format!(
                            "composite: its fills ({} at its start, {} at its end) are drawn \
                             {} from {} toward {}, {how} (channel {ch} by {:.1} steps of 1/255)",
                            lb.join("+"),
                            le.join("+"),
                            hex(c),
                            hex(c0),
                            hex(c1),
                            if outside {
                                ((c[ch] - a.min(z)).min(0.0).abs() + (c[ch] - a.max(z)).max(0.0))
                                    * 255.0
                            } else {
                                ((z - c[ch]).abs() - (z - prev[ch]).abs()) * 255.0
                            }
                        ),
                    )
                    .at(tl.ks[i])
                    .node(key)
                    .axis("bg"),
                );
            }
        }
        prev = c;
    }
    None
}

/// `palette`: each theme-swap root springs along the curve its table
/// asks a colour to take. One finding per swap (the frame its roots
/// start on), naming how many roots took another curve.
fn palette(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut held = 0u64;
    for f in &cx.trace.frames {
        let wrong: Vec<_> =
            f.sp.iter()
                .filter(|r| r.of == "palette" && r.start.is_some())
                .filter_map(|r| r.ask.map(|a| (r, a)))
                .inspect(|_| held += 1)
                .filter(|(r, a)| r.c != *a && !cx.allowed(CheckId::Colour, &r.key))
                .collect();
        let Some((r, ask)) = wrong.first() else {
            continue;
        };
        out.push(
            Finding::fail(
                CheckId::Colour,
                format!(
                    "palette: the theme swap springs {} (and {} more palette roots) along {}, \
                     where its table asks a colour to take {}: colour springs with \
                     `$motion.effects` unless the swap names its own transition",
                    r.key,
                    wrong.len() - 1,
                    curve(r.c),
                    curve(*ask)
                ),
            )
            .at(f.k)
            .node(&r.key)
            .axis("palette"),
        );
    }
    super::stats("colour.palette", cx, &[("changes", held)]);
    out
}

fn curve(c: crate::trace::CurveRec) -> String {
    use crate::trace::CurveRec;
    match c {
        CurveRec::Instant => "no curve (a snap)".into(),
        CurveRec::Spring { k, z } => format!("spring({k}, {z})"),
        CurveRec::Timed { d, ease } => format!("{} ms {ease:?}", d / 1_000_000),
    }
}

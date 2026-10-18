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
//! the colour left its start: a spring keeps its velocity, so its way
//! curves), one that keyframes or time drive, or a gradient. A frame's
//! colour, as the recording writes it (straight-alpha sRGB to 1/255), is
//! compared with the nearest point of that line drawn as the raster
//! draws it (each channel clamped), premultiplied: within
//! [`STEPS`] steps of 1/255 on every channel.

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
    let (p0, p1) = (premultiplied(from), premultiplied(to));
    let at = |s: f64| -> f64 {
        let p: [f64; 4] = std::array::from_fn(|i| p0[i] + s * (p1[i] - p0[i]));
        let d = drawn(p);
        let e = [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
        (0..4).map(|i| (d[i] - e[i]).abs()).fold(0.0, f64::max) * 255.0
    };
    // Coarse, then fine around the nearest.
    let mut best = (f64::INFINITY, 0.0);
    for i in -50..=150 {
        let s = f64::from(i) / 100.0;
        let e = at(s);
        if e < best.0 {
            best = (e, s);
        }
    }
    let centre = best.1;
    for i in -40..=40 {
        let s = centre + f64::from(i) / 4000.0;
        let e = at(s);
        if e < best.0 {
            best = (e, s);
        }
    }
    best.0
}

pub fn run(cx: &Cx) -> Vec<Finding> {
    let tl = &cx.tl;
    let mut out = Vec::new();
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
                let (fb, fe) = (field(b, name), field(e, name));
                for (slot, (cb, ce)) in fb.iter().zip(fe).enumerate() {
                    if cb == ce {
                        continue;
                    }
                    let (Some(from), Some(to)) = (rgba(cb), rgba(ce)) else {
                        continue;
                    };
                    if let Some(f) =
                        transition(cx, key, name, bit, slot, (bi, ei), (cb, ce), (from, to))
                    {
                        out.push(f.in_segment(&seg.name));
                    }
                }
            }
        }
    }
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
) -> Option<Finding> {
    let tl = &cx.tl;
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
    let mut worst: Option<(f64, u32, &str)> = None;
    for (i, _, c) in &way {
        let Some(at) = rgba(c) else { continue };
        let e = off_way(at, from, to);
        if e > STEPS && worst.is_none_or(|w| e > w.0) {
            worst = Some((e, tl.ks[*i], c));
        }
    }
    let (e, k, c) = worst?;
    Some(
        Finding::fail(
            CheckId::Colour,
            format!(
                "off its way: its {name} springing from {cb} to {ce} was drawn {c}, {e:.1} steps \
                 of 1/255 from the way between them in premultiplied OKLab"
            ),
        )
        .at(k)
        .node(key)
        .axis(name)
        .got(e, [0.0, STEPS]),
    )
}

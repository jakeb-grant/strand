//! Scene paints as vello paints: BGRA colours, gradients and their stops.

use strand_scene::{Color, GradientStop, Paint};
use vello_cpu::PaintType;
use vello_cpu::color::{AlphaColor, ColorSpaceTag, Srgb};
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{Extend, Gradient};

/// A colour for vello with red and blue swapped (see the module docs).
pub(super) fn bgra(c: Color) -> AlphaColor<Srgb> {
    let c = c.clamped();
    AlphaColor::new([c.b, c.g, c.r, c.a])
}

/// How many times a piece of a segment between two stops may be halved:
/// down to 1/1024 of it (vello's ramps hold 512 samples over the whole
/// gradient, so finer pieces draw nothing new).
const RAMP_DEPTH: u32 = 10;

/// How far, in levels of an 8-bit channel, the straight line between two
/// samples may leave the OKLab ramp: under half a level, so the samples
/// round to the levels the ramp itself does.
const RAMP_LEVELS: f32 = 0.5;

/// A gradient's stops as samples of design.md's ramp (premultiplied
/// OKLab between stops) for a renderer that interpolates per channel in
/// sRGB between the stops it is given (vello on the CPU and the GPU).
/// Each segment is halved where the straight line between its samples
/// leaves the ramp by more than [`RAMP_LEVELS`] in a channel, straight or
/// premultiplied, down to [`RAMP_DEPTH`]: a few samples for colours near
/// in hue, more where colours far apart curve (red to blue left the ramp
/// by 29 levels with 8 even samples; decisions.md, gradient-cpu-gpu).
/// Offsets are clamped to 0..1 and colours to their range, as the CPU's
/// dithered ramp (`cache::lut`) takes them.
pub(crate) fn ramp(stops: &[GradientStop]) -> Vec<(f32, Color)> {
    let clean: Vec<GradientStop> = stops
        .iter()
        .map(|s| GradientStop {
            offset: if s.offset.is_finite() {
                s.offset.clamp(0.0, 1.0)
            } else {
                0.0
            },
            color: s.color.clamped(),
        })
        .collect();
    let mut out = Vec::with_capacity(clean.len() * 8);
    for (i, s) in clean.iter().enumerate() {
        out.push((s.offset, s.color));
        let Some(next) = clean.get(i + 1) else { break };
        if next.offset <= s.offset {
            continue;
        }
        for t in cuts(s.color, next.color).iter() {
            let offset = s.offset + (next.offset - s.offset) * t;
            out.push((offset, s.color.lerp_oklab(next.color, *t).clamped()));
        }
    }
    out
}

/// Segments' cuts worked out so far, by their two colours: a frame
/// lowers or paints its gradients every frame.
const CUTS_KEPT: usize = 1024;

type Cuts = std::collections::HashMap<[u32; 8], std::sync::Arc<[f32]>>;

/// Shared by every thread (perf-cpu-raster: the raster's cells are drawn
/// on several, `raster::draw_parallel`).
static CUTS: std::sync::Mutex<Option<Cuts>> = std::sync::Mutex::new(None);

fn cuts_kept() -> std::sync::MutexGuard<'static, Option<Cuts>> {
    CUTS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where [`ramp`] cuts the segment from `a` to `b`, strictly inside it
/// (0..1, in order).
fn cuts(a: Color, b: Color) -> std::sync::Arc<[f32]> {
    let key = [a.r, a.g, a.b, a.a, b.r, b.g, b.b, b.a].map(f32::to_bits);
    if let Some(c) = cuts_kept().as_ref().and_then(|c| c.get(&key).cloned()) {
        return c;
    }
    let at = |t: f32| a.lerp_oklab(b, t).clamped();
    // Straight and premultiplied channels, in levels.
    let levels =
        |c: Color| [c.r, c.g, c.b, c.a, c.r * c.a, c.g * c.a, c.b * c.a].map(|v| v * 255.0);
    // Whether the line over `t0..t1` stays within the levels at seven
    // points inside it.
    let fits = |t0: f32, t1: f32| {
        let (l0, l1) = (levels(at(t0)), levels(at(t1)));
        (1..8).all(|k| {
            let q = k as f32 / 8.0;
            let exact = levels(at(t0 + (t1 - t0) * q));
            (0..7).all(|c| (l0[c] + (l1[c] - l0[c]) * q - exact[c]).abs() <= RAMP_LEVELS)
        })
    };
    let mut out = Vec::new();
    // Pieces still to look at, the leftmost last.
    let mut todo = vec![(0.0f32, 1.0f32, 0u32)];
    while let Some((t0, t1, depth)) = todo.pop() {
        if depth >= RAMP_DEPTH || fits(t0, t1) {
            if t1 < 1.0 {
                out.push(t1);
            }
            continue;
        }
        let mid = (t0 + t1) / 2.0;
        todo.push((mid, t1, depth + 1));
        todo.push((t0, mid, depth + 1));
    }
    let out: std::sync::Arc<[f32]> = out.into();
    let mut c = cuts_kept();
    let c = c.get_or_insert_with(Default::default);
    if c.len() >= CUTS_KEPT {
        c.clear();
    }
    c.insert(key, out.clone());
    out
}

/// Gradient stops for vello: [`ramp`]'s samples with red and blue
/// swapped; vello interpolates per channel in sRGB between them, which
/// is symmetric in red and blue, so the swap stays valid.
pub(super) fn stops(stops: &[GradientStop]) -> Vec<(f32, AlphaColor<Srgb>)> {
    ramp(stops).into_iter().map(|(o, c)| (o, bgra(c))).collect()
}

pub(super) fn gradient(mut g: Gradient, s: &[GradientStop]) -> PaintType {
    g.interpolation_cs = ColorSpaceTag::Srgb;
    g.extend = Extend::Pad;
    PaintType::Gradient(g.with_stops(stops(s).as_slice()))
}

pub(super) fn finite_angle(deg: f32) -> f64 {
    if deg.is_finite() {
        (deg % 360.0) as f64
    } else {
        0.0
    }
}

/// The vello paint for `p` filling `frame`, and the paint transform it
/// needs (relative to the current transform).
pub(super) fn paint_type(p: &Paint, frame: kurbo::Rect) -> (PaintType, Affine) {
    let center = frame.center();
    let paint = match p {
        Paint::Solid(c) => PaintType::Solid(bgra(*c)),
        Paint::Linear { stops: s, .. }
        | Paint::Radial { stops: s }
        | Paint::Conic { stops: s, .. }
            if s.len() < 2 =>
        {
            PaintType::Solid(
                s.first()
                    .map_or(AlphaColor::TRANSPARENT, |st| bgra(st.color)),
            )
        }
        Paint::Linear { angle, stops: s } => {
            // CSS: 0deg points up, angles run clockwise, and the gradient
            // line is long enough for the corners to hit the end colours.
            let a = finite_angle(*angle).to_radians();
            let (sin, cos) = a.sin_cos();
            let len = (frame.width() * sin).abs() + (frame.height() * cos).abs();
            let d = kurbo::Vec2::new(sin, -cos) * (len / 2.0);
            gradient(Gradient::new_linear(center - d, center + d), s)
        }
        Paint::Radial { stops: s } => {
            let r = (frame.width() / 2.0).hypot(frame.height() / 2.0);
            gradient(Gradient::new_radial(center, r as f32), s)
        }
        Paint::Conic { from, stops: s } => {
            // A full turn starting at +x; the paint transform turns that
            // start to CSS's `from`, measured clockwise from the top (y
            // grows down, so a positive rotation is clockwise on screen).
            let g = gradient(Gradient::new_sweep(center, 0.0, std::f32::consts::TAU), s);
            let turn = (finite_angle(*from) - 90.0).to_radians();
            return (g, Affine::rotate_about(turn, center));
        }
    };
    (paint, Affine::IDENTITY)
}

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

/// Samples between adjacent stops, so vello's per-channel sRGB
/// interpolation follows the OKLab ramp design asks for.
pub(super) const GRADIENT_SAMPLES: usize = 8;

/// Gradient stops for vello. Colours are interpolated in premultiplied
/// OKLab by sampling each segment, then handed over with red and blue
/// swapped; vello interpolates per channel in sRGB between the samples,
/// which is symmetric in red and blue, so the swap stays valid.
pub(super) fn stops(stops: &[GradientStop]) -> Vec<(f32, AlphaColor<Srgb>)> {
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
    let mut out = Vec::with_capacity(clean.len() * GRADIENT_SAMPLES);
    for (i, s) in clean.iter().enumerate() {
        out.push((s.offset, bgra(s.color)));
        let Some(next) = clean.get(i + 1) else { break };
        if next.offset <= s.offset {
            continue;
        }
        for k in 1..GRADIENT_SAMPLES {
            let t = k as f32 / GRADIENT_SAMPLES as f32;
            let offset = s.offset + (next.offset - s.offset) * t;
            out.push((offset, bgra(s.color.lerp_oklab(next.color, t))));
        }
    }
    out
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

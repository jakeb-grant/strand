//! (M4) The effects catalogue's paint half (design.md, "Motion and visual
//! effects"), built from props.
//!
//! - [`group`]: a node's group effects from `filter:`, `mask:` and
//!   `blend:` (`strand_scene::Effect`, drawn as a layer by
//!   [`crate::layers`] and [`crate::offscreen`]). The bundled GPU filters
//!   (`bloom`, `crt`, `chromatic`, `wobble`) are shader passes; on the CPU
//!   `bloom` draws as a glow of the subtree's own pixels and the others
//!   draw unfiltered (design.md, "Bundled GPU effects").
//! - [`filter`]: the colour functions as colour matrices.

pub(crate) mod builtin;
pub(crate) mod filter;
pub(crate) mod glow;
pub(crate) mod goo;
pub(crate) mod jelly;
#[cfg(feature = "gpu")]
pub(crate) mod gpu;
pub(crate) mod lean;
pub(crate) mod letters;
pub(crate) mod light;
pub(crate) mod particles;
pub(crate) mod raster;
pub(crate) mod roll;
pub(crate) mod transition;

use std::sync::Arc;

use strand_scene::effect::compose_matrices;
use strand_scene::{
    Anchor, BlendMode, Bundled, Color, Edge, Effect, Length, Mask, Paint, Prop, PropValue,
    ShaderInput, ShaderPass, ShaderRef,
};

/// Largest blur radius (standard deviation) or bundled knob, logical
/// pixels: a group past it is no more blurred to the eye, and the blur's
/// cost grows with it.
pub(crate) const MAX_RADIUS: f32 = 250.0;

/// A finite number, from a number or a pixel length.
pub(crate) fn number(v: &PropValue) -> Option<f32> {
    match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) => n.is_finite().then_some(*n),
        _ => None,
    }
}

/// An angle in degrees (a bare number is degrees).
pub(crate) fn degrees(v: &PropValue) -> Option<f32> {
    match v {
        PropValue::Angle(a) => a.is_finite().then_some(*a),
        other => number(other),
    }
}

/// A colour, from a colour or a solid paint.
pub(crate) fn color(v: &PropValue) -> Option<Color> {
    match v {
        PropValue::Color(c) => Some(*c),
        PropValue::Paint(Paint::Solid(c)) => Some(*c),
        _ => None,
    }
}

/// A keyword or text's name.
pub(crate) fn keyword(v: &PropValue) -> Option<&str> {
    match v {
        PropValue::Keyword(k) | PropValue::Text(k) => Some(k),
        _ => None,
    }
}

/// The calls a value holds: itself, or each item of a list.
fn calls(v: &PropValue) -> Vec<(&str, &[PropValue])> {
    match v {
        PropValue::Call { name, args } => vec![(name.as_str(), args.as_slice())],
        PropValue::List(items) => items.iter().flat_map(calls).collect(),
        _ => Vec::new(),
    }
}

/// A radius knob, clamped to `0..=MAX_RADIUS`.
fn radius(args: &[PropValue]) -> Option<f32> {
    args.first()
        .and_then(number)
        .map(|r| r.clamp(0.0, MAX_RADIUS))
}

/// The group effects of a node from its resolved props (`get`), on a
/// surface at `scale`, for its box of `w × h` logical pixels (a radial
/// mask's percentage is of the distance to the box's farthest corner).
/// `None` when it has none.
pub(crate) fn group<'v>(
    get: impl Fn(Prop) -> Option<&'v PropValue>,
    w: f32,
    h: f32,
    scale: f32,
) -> Option<Arc<[Effect]>> {
    let mut out: Vec<Effect> = Vec::new();
    if let Some(v) = get(Prop::Filter) {
        for (name, args) in calls(v) {
            if let Some(e) = filter_effect(name, args, scale) {
                push_effect(&mut out, e);
            }
        }
    }
    if let Some(m) = get(Prop::Mask).and_then(|v| mask(v, w, h)) {
        out.push(Effect::Mask(m));
    }
    if let Some(b) = get(Prop::Blend)
        .and_then(keyword)
        .and_then(BlendMode::from_name)
    {
        out.push(Effect::Blend(b));
    }
    (!out.is_empty()).then(|| out.into())
}

/// Adds `e` to `out`, composing a colour matrix with one just before it
/// (a chain of colour functions is one pass).
fn push_effect(out: &mut Vec<Effect>, e: Effect) {
    if let (Effect::ColorMatrix(m), Some(Effect::ColorMatrix(prev))) = (&e, out.last_mut()) {
        *prev = compose_matrices(m, prev);
        return;
    }
    out.push(e);
}

/// One `filter:` function as an effect (an unknown name, or a misplaced
/// `glass()`, is none: the checker reports it).
fn filter_effect(name: &str, args: &[PropValue], scale: f32) -> Option<Effect> {
    let amount = || args.first().and_then(number);
    let matrix = match name {
        "blur" => {
            return radius(args)
                .filter(|r| *r > 0.0)
                .map(|radius| Effect::Blur { radius });
        }
        "bloom" | "chromatic" | "wobble" | "crt" => {
            let bundled = Bundled::from_name(name)?;
            // Uniforms in buffer units, the length first (its reach,
            // `Bundled::reach`); decisions.md, m4-gpu-effects.
            let length = radius(args).unwrap_or(0.0) * scale;
            let uniforms: Vec<f32> = match name {
                "crt" => Vec::new(),
                "bloom" => {
                    let strength = args.get(1).and_then(number).unwrap_or(1.0);
                    vec![length, strength.clamp(0.0, 10.0)]
                }
                "wobble" => vec![length, WOBBLE_WAVE * scale, WOBBLE_PERIOD],
                _ => vec![length],
            };
            return Some(Effect::Shader(ShaderPass {
                code: ShaderRef::Bundled(bundled),
                uniforms: uniforms.into(),
                input: ShaderInput::Content,
            }));
        }
        "grayscale" => filter::grayscale(amount()?),
        "saturate" => filter::saturate(amount()?.max(0.0)),
        "hue" => filter::hue(args.first().and_then(degrees)?),
        "brightness" => filter::brightness(amount()?),
        "contrast" => filter::contrast(amount()?),
        "invert" => filter::invert(amount()?),
        "tint" => filter::tint(args.first().and_then(color)?),
        _ => return None,
    };
    matrix
        .iter()
        .all(|v| v.is_finite())
        .then_some(Effect::ColorMatrix(matrix))
}

/// `wobble()`'s wavelength, logical pixels, and period, seconds.
const WOBBLE_WAVE: f32 = 40.0;
const WOBBLE_PERIOD: f32 = 2.0;

/// (M4) `effects` with the GPU's 3-D tilt last: `turn` is the lean's
/// `[across, down]` in degrees (`crate::effects::lean`), the pass's
/// uniforms its pitch and yaw in radians (`Bundled::Tilt`: the side the
/// pointer is on pressed away).
#[cfg(feature = "gpu")]
pub(crate) fn with_tilt(effects: Option<Arc<[Effect]>>, turn: [f32; 2]) -> Arc<[Effect]> {
    let mut out: Vec<Effect> = effects
        .as_deref()
        .map(<[Effect]>::to_vec)
        .unwrap_or_default();
    let [across, down] = turn.map(|d| {
        if d.is_finite() {
            d.clamp(-80.0, 80.0)
        } else {
            0.0
        }
    });
    out.push(Effect::Shader(ShaderPass {
        code: ShaderRef::Bundled(Bundled::Tilt),
        uniforms: vec![-down.to_radians(), across.to_radians()].into(),
        input: ShaderInput::Content,
    }));
    out.into()
}

/// `mask: fade(edge, len) | radial(at, size) | shape(name)`.
fn mask(v: &PropValue, w: f32, h: f32) -> Option<Mask> {
    let PropValue::Call { name, args } = v else {
        return None;
    };
    match name.as_str() {
        "fade" => {
            let edge = args.first().and_then(keyword).and_then(Edge::from_name)?;
            let len = args.get(1).and_then(number).unwrap_or(0.0).max(0.0);
            Some(Mask::Fade { edge, len })
        }
        "radial" => {
            let at = args
                .first()
                .and_then(keyword)
                .and_then(Anchor::from_name)
                .unwrap_or(Anchor::Center);
            let size = match args.get(1)? {
                PropValue::Length(Length::Percent(p)) if p.is_finite() => {
                    farthest_corner(at, w, h) * p / 100.0
                }
                other => number(other)?,
            };
            Some(Mask::Radial {
                at,
                size: size.max(0.0),
            })
        }
        "shape" => args
            .first()
            .and_then(keyword)
            .map(|s| Mask::Shape(s.to_string())),
        _ => None,
    }
}

/// The distance from the point `at` names in a `w × h` box to the box's
/// farthest corner: a radial reveal of 100% shows the whole box.
fn farthest_corner(at: Anchor, w: f32, h: f32) -> f32 {
    let (x, y) = match at {
        Anchor::Center => (0.5, 0.5),
        Anchor::Top => (0.5, 0.0),
        Anchor::Bottom => (0.5, 1.0),
        Anchor::Left => (0.0, 0.5),
        Anchor::Right => (1.0, 0.5),
        Anchor::TopLeft => (0.0, 0.0),
        Anchor::TopRight => (1.0, 0.0),
        Anchor::BottomLeft => (0.0, 1.0),
        Anchor::BottomRight => (1.0, 1.0),
    };
    let dx = w * f32::max(x, 1.0 - x);
    let dy = h * f32::max(y, 1.0 - y);
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Vec<PropValue>) -> PropValue {
        PropValue::Call {
            name: name.into(),
            args,
        }
    }

    fn effects(props: &[(Prop, PropValue)]) -> Vec<Effect> {
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v);
        group(get, 100.0, 40.0, 2.0).map_or(Vec::new(), |e| e.to_vec())
    }

    #[test]
    fn props_build_group_effects_in_order() {
        assert!(effects(&[]).is_empty());
        // A chain of colour functions is one matrix; a blur between
        // them splits it.
        let chain = PropValue::List(vec![
            call("grayscale", vec![PropValue::Number(1.0)]),
            call("brightness", vec![PropValue::Number(0.5)]),
            call("blur", vec![PropValue::Number(3.0)]),
            call("invert", vec![PropValue::Number(1.0)]),
        ]);
        let got = effects(&[
            (Prop::Filter, chain),
            (
                Prop::Mask,
                call(
                    "fade",
                    vec![PropValue::Keyword("bottom".into()), PropValue::Number(24.0)],
                ),
            ),
            (Prop::Blend, PropValue::Keyword("screen".into())),
        ]);
        let one = compose_matrices(&filter::brightness(0.5), &filter::grayscale(1.0));
        assert_eq!(
            got,
            vec![
                Effect::ColorMatrix(one),
                Effect::Blur { radius: 3.0 },
                Effect::ColorMatrix(filter::invert(1.0)),
                Effect::Mask(Mask::Fade {
                    edge: Edge::Bottom,
                    len: 24.0
                }),
                Effect::Blend(BlendMode::Screen),
            ]
        );
        // `blend: normal` is no effect; a radial mask's percentage is of
        // the farthest corner (from the centre of 100 × 40: √(50² + 20²)).
        let got = effects(&[
            (Prop::Blend, PropValue::Keyword("normal".into())),
            (
                Prop::Mask,
                call(
                    "radial",
                    vec![
                        PropValue::Keyword("center".into()),
                        PropValue::Length(Length::Percent(100.0)),
                    ],
                ),
            ),
        ]);
        let Effect::Mask(Mask::Radial { at, size }) = &got[0] else {
            panic!("{got:?}");
        };
        assert_eq!((*at, got.len()), (Anchor::Center, 1));
        assert!((size - 2900f32.sqrt()).abs() < 1e-3);
        assert_eq!(
            effects(&[(
                Prop::Mask,
                call("shape", vec![PropValue::Keyword("cookie".into())])
            )]),
            vec![Effect::Mask(Mask::Shape("cookie".into()))]
        );
    }

    /// The bundled filters are shader passes whose first uniform is their
    /// length knob in buffer pixels; a misplaced `glass()` builds nothing.
    #[test]
    fn bundled_filters_are_shader_passes() {
        let got = effects(&[(
            Prop::Filter,
            PropValue::List(vec![
                call("bloom", vec![PropValue::Number(12.0)]),
                call("crt", vec![]),
                call("chromatic", vec![PropValue::Number(2.0)]),
                call("glass", vec![]),
                call(
                    "bloom",
                    vec![PropValue::Number(4.0), PropValue::Number(1.5)],
                ),
                call("wobble", vec![PropValue::Number(3.0)]),
            ]),
        )]);
        let passes: Vec<(Bundled, Vec<f32>)> = got
            .iter()
            .map(|e| match e {
                Effect::Shader(ShaderPass {
                    code: ShaderRef::Bundled(b),
                    uniforms,
                    input: ShaderInput::Content,
                }) => (*b, uniforms.to_vec()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            passes,
            vec![
                (Bundled::Bloom, vec![24.0, 1.0]),
                (Bundled::Crt, vec![]),
                (Bundled::Chromatic, vec![4.0]),
                (Bundled::Bloom, vec![8.0, 1.5]),
                (Bundled::Wobble, vec![6.0, 80.0, 2.0]),
            ]
        );
        // Out-of-range knobs are clamped, bad ones dropped.
        assert_eq!(
            effects(&[(Prop::Filter, call("blur", vec![PropValue::Number(-4.0)]))]),
            vec![]
        );
        assert_eq!(
            effects(&[(Prop::Filter, call("grayscale", vec![]))]),
            vec![]
        );
    }

    /// The GPU's 3-D tilt goes last, in radians, pitch then yaw.
    #[cfg(feature = "gpu")]
    #[test]
    fn a_tilt_is_a_pass_in_radians() {
        let tilted = with_tilt(None, [10.0, -5.0]);
        let Effect::Shader(ShaderPass { code, uniforms, .. }) = &tilted[0] else {
            panic!("{tilted:?}");
        };
        assert_eq!(code, &ShaderRef::Bundled(Bundled::Tilt));
        assert!((uniforms[0] - 5f32.to_radians()).abs() < 1e-6);
        assert!((uniforms[1] - 10f32.to_radians()).abs() < 1e-6);
    }
}

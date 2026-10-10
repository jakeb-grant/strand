//! `filter:` colour functions as colour matrices (design.md, "Filters and
//! compositing": "Strand implements these itself, since vello's versions
//! are unimplemented").
//!
//! Each function is a 4×5 matrix over straight-alpha sRGB values (rows r,
//! g, b, a; the fifth column an offset in 0..1), the Filter Effects
//! spec's shorthand matrices with Rec. 709 luma weights; a chain composes
//! into one matrix ([`strand_scene::effect::compose_matrices`]), so
//! `[grayscale(1), brightness(0.9)]` is one pass over the group.

use strand_scene::Color;
use strand_scene::effect::IDENTITY_MATRIX;

/// Rec. 709 luma weights.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// `saturate(s)`: 0 is grey, 1 unchanged, above 1 more saturated.
pub fn saturate(s: f32) -> [f32; 20] {
    let [r, g, b] = LUMA;
    [
        r + (1.0 - r) * s,
        g - g * s,
        b - b * s,
        0.0,
        0.0, //
        r - r * s,
        g + (1.0 - g) * s,
        b - b * s,
        0.0,
        0.0, //
        r - r * s,
        g - g * s,
        b + (1.0 - b) * s,
        0.0,
        0.0, //
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// `grayscale(a)`: `a` of the way to grey (clamped to 0..1).
pub fn grayscale(a: f32) -> [f32; 20] {
    saturate(1.0 - a.clamp(0.0, 1.0))
}

/// `hue(deg)`: turns hues by `deg`, keeping luma.
pub fn hue(deg: f32) -> [f32; 20] {
    let (sin, cos) = deg.to_radians().sin_cos();
    let [r, g, b] = LUMA;
    [
        r + cos * (1.0 - r) - sin * r,
        g - cos * g - sin * g,
        b - cos * b + sin * (1.0 - b),
        0.0,
        0.0, //
        r - cos * r + sin * 0.143,
        g + cos * (1.0 - g) + sin * 0.140,
        b - cos * b - sin * 0.283,
        0.0,
        0.0, //
        r - cos * r - sin * (1.0 - r),
        g - cos * g + sin * g,
        b + cos * (1.0 - b) + sin * b,
        0.0,
        0.0, //
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

/// A matrix scaling r, g and b by `k` and adding `offset`.
fn linear(k: f32, offset: f32) -> [f32; 20] {
    let mut m = IDENTITY_MATRIX;
    for row in 0..3 {
        m[row * 6] = k;
        m[row * 5 + 4] = offset;
    }
    m
}

/// `brightness(b)`: scales every channel; 0 is black.
pub fn brightness(b: f32) -> [f32; 20] {
    linear(b.max(0.0), 0.0)
}

/// `contrast(c)`: about mid grey; 0 is grey, 1 unchanged.
pub fn contrast(c: f32) -> [f32; 20] {
    let c = c.max(0.0);
    linear(c, 0.5 - 0.5 * c)
}

/// `invert(a)`: `a` of the way to the negative (clamped to 0..1).
pub fn invert(a: f32) -> [f32; 20] {
    let a = a.clamp(0.0, 1.0);
    linear(1.0 - 2.0 * a, a)
}

/// `tint(c)`: maps each pixel's luma onto `c` (white becomes `c`, black
/// stays black), as strongly as `c`'s alpha.
pub fn tint(c: Color) -> [f32; 20] {
    let c = c.clamped();
    let k = c.a;
    let mut m = IDENTITY_MATRIX;
    for (row, channel) in [c.r, c.g, c.b].into_iter().enumerate() {
        for (col, w) in LUMA.into_iter().enumerate() {
            let id = if row == col { 1.0 } else { 0.0 };
            m[row * 5 + col] = id * (1.0 - k) + w * channel * k;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matrix applied to a straight-alpha colour.
    fn apply(m: &[f32; 20], c: [f32; 4]) -> [f32; 4] {
        let row = |i: usize| {
            let r = &m[i * 5..i * 5 + 5];
            r[0] * c[0] + r[1] * c[1] + r[2] * c[2] + r[3] * c[3] + r[4]
        };
        [row(0), row(1), row(2), row(3)]
    }

    fn near(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn each_function_does_what_its_name_says() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let white = [1.0, 1.0, 1.0, 1.0];
        let half = [0.5, 0.5, 0.5, 0.5];
        // Grey keeps luma, and identity at the neutral amount.
        let grey = apply(&grayscale(1.0), red);
        assert!(near(grey, [0.2126, 0.2126, 0.2126, 1.0]), "{grey:?}");
        assert!(near(apply(&grayscale(0.0), red), red));
        assert!(near(apply(&saturate(1.0), red), red));
        assert!(near(apply(&hue(0.0), red), red));
        assert!(near(apply(&brightness(1.0), red), red));
        assert!(near(apply(&contrast(1.0), red), red));
        assert!(near(apply(&invert(0.0), red), red));
        // A full turn is the identity; luma survives any turn.
        assert!(near(apply(&hue(360.0), red), red));
        let turned = apply(&hue(120.0), red);
        let luma = |c: [f32; 4]| LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2];
        assert!((luma(turned) - luma(red)).abs() < 1e-3, "{turned:?}");
        assert!(turned[1] > turned[0], "red turns towards green: {turned:?}");
        // Brightness, contrast and invert.
        assert!(near(apply(&brightness(0.5), white), [0.5, 0.5, 0.5, 1.0]));
        assert!(near(apply(&contrast(0.0), red), [0.5, 0.5, 0.5, 1.0]));
        assert!(near(apply(&invert(1.0), red), [0.0, 1.0, 1.0, 1.0]));
        assert!(near(apply(&invert(0.5), red), [0.5, 0.5, 0.5, 1.0]));
        // Alpha passes through.
        assert_eq!(apply(&grayscale(1.0), half)[3], 0.5);
        // Tint: white becomes the colour, black stays black, and a
        // half-transparent tint goes half way.
        let accent = Color::from_hex("#7aa2f7").unwrap();
        let w = apply(&tint(accent), white);
        assert!(near(w, [accent.r, accent.g, accent.b, 1.0]), "{w:?}");
        assert!(near(
            apply(&tint(accent), [0.0, 0.0, 0.0, 1.0]),
            [0.0, 0.0, 0.0, 1.0]
        ));
        let soft = apply(&tint(accent.with_alpha(0.5)), white);
        assert!(near(
            soft,
            [
                0.5 + accent.r / 2.0,
                0.5 + accent.g / 2.0,
                0.5 + accent.b / 2.0,
                1.0
            ]
        ));
    }
}

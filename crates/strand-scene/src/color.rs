//! Colour: straight-alpha sRGB storage with exact conversions to linear sRGB,
//! OKLab and OKLCH, and interpolation in OKLab.
//!
//! Conversions run in `f64`. The OKLab matrices are Björn Ottosson's
//! published linear-sRGB → LMS → Lab matrices; their inverses were computed
//! exactly (rational arithmetic) from those, so round trips are accurate to
//! `f64` rounding rather than to the 10 digits the inverse matrices are
//! usually quoted with. Out-of-gamut values are not clamped: the sRGB
//! transfer function is extended sign-symmetrically (as CSS Color 4 does)
//! so colours outside the sRGB gamut survive a round trip.

/// A colour in the sRGB colour space with straight (non-premultiplied) alpha.
/// Components are nominally `0.0..=1.0`; values outside that range are
/// out-of-gamut colours (see [`Color::clamped`]).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Linear-light sRGB with straight alpha.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LinearRgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub alpha: f64,
}

/// OKLab: perceptual lightness `l` (0 black to 1 white) and opponent axes
/// `a` (green–red) and `b` (blue–yellow), with straight alpha.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Oklab {
    pub l: f64,
    pub a: f64,
    pub b: f64,
    pub alpha: f64,
}

/// OKLCH: OKLab in polar form. `h` is the hue in degrees, `0.0..360.0`.
/// Achromatic colours (chroma ≈ 0) report hue 0.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
    pub alpha: f64,
}

// Linear sRGB → LMS (Ottosson).
const M1: [[f64; 3]; 3] = [
    [0.4122214708, 0.5363325363, 0.0514459929],
    [0.2119034982, 0.6806995451, 0.1073969566],
    [0.0883024619, 0.2817188376, 0.6299787005],
];
// Cube-rooted LMS → Lab (Ottosson).
const M2: [[f64; 3]; 3] = [
    [0.2104542553, 0.7936177850, -0.0040720468],
    [1.9779984951, -2.4285922050, 0.4505937099],
    [0.0259040371, 0.7827717662, -0.8086757660],
];
// Exact inverses of M1 and M2.
const M1_INV: [[f64; 3]; 3] = [
    [4.076741661347994, -3.3077115904081933, 0.2309699287294279],
    [-1.268438004092176, 2.6097574006633715, -0.3413193963102196],
    [
        -0.004196086541837109,
        -0.7034186144594496,
        1.7076147009309448,
    ],
];
const M2_INV: [[f64; 3]; 3] = [
    [0.9999999984505198, 0.39633779217376786, 0.2158037580607588],
    [
        1.0000000088817609,
        -0.10556134232365635,
        -0.06385417477170591,
    ],
    [
        1.0000000546724108,
        -0.08948418209496575,
        -1.2914855378640917,
    ],
];

fn mul(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// The sRGB electro-optical transfer function (encoded → linear), extended
/// sign-symmetrically to negative values.
pub fn srgb_to_linear(v: f64) -> f64 {
    let a = v.abs();
    let lin = if a <= 0.04045 {
        a / 12.92
    } else {
        ((a + 0.055) / 1.055).powf(2.4)
    };
    lin.copysign(v)
}

/// The inverse of [`srgb_to_linear`].
pub fn linear_to_srgb(v: f64) -> f64 {
    let a = v.abs();
    let enc = if a <= 0.0031308 {
        a * 12.92
    } else {
        1.055 * a.powf(1.0 / 2.4) - 0.055
    };
    enc.copysign(v)
}

impl Color {
    pub const TRANSPARENT: Color = Color::new(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Color = Color::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Color = Color::new(1.0, 1.0, 1.0, 1.0);

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self::new(r, g, b, 1.0)
    }

    pub fn from_rgba8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::new(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a as f32 / 255.0,
        )
    }

    /// Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` (the `#` is optional).
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if !s.is_ascii() {
            return None;
        }
        let nib = |i: usize| u8::from_str_radix(&s[i..i + 1], 16).ok().map(|v| v * 17);
        let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        match s.len() {
            3 => Some(Self::from_rgba8(nib(0)?, nib(1)?, nib(2)?, 255)),
            4 => Some(Self::from_rgba8(nib(0)?, nib(1)?, nib(2)?, nib(3)?)),
            6 => Some(Self::from_rgba8(byte(0)?, byte(2)?, byte(4)?, 255)),
            8 => Some(Self::from_rgba8(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    }

    /// Straight-alpha 8-bit components, clamped and rounded.
    pub fn to_rgba8(self) -> [u8; 4] {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [q(self.r), q(self.g), q(self.b), q(self.a)]
    }

    /// Premultiplied 8-bit pixel in `wl_shm` ARGB8888 byte order
    /// (little-endian: blue, green, red, alpha).
    pub fn to_argb8888_premul(self) -> [u8; 4] {
        let c = self.clamped();
        let q = |v: f32| (v * c.a * 255.0).round() as u8;
        [q(c.b), q(c.g), q(c.r), (c.a * 255.0).round() as u8]
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    /// Multiplies alpha, like the token method `$fg.alpha(0.65)` applied to
    /// an already translucent colour.
    pub fn alpha(self, factor: f32) -> Self {
        Self {
            a: self.a * factor,
            ..self
        }
    }

    /// True if every colour component lies in `0..=1` (within `eps`).
    pub fn in_gamut(self, eps: f32) -> bool {
        [self.r, self.g, self.b]
            .iter()
            .all(|v| (-eps..=1.0 + eps).contains(v))
    }

    /// All components clamped to `0..=1`.
    pub fn clamped(self) -> Self {
        let c = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
        Self::new(c(self.r), c(self.g), c(self.b), c(self.a))
    }

    pub fn to_linear(self) -> LinearRgb {
        LinearRgb {
            r: srgb_to_linear(self.r as f64),
            g: srgb_to_linear(self.g as f64),
            b: srgb_to_linear(self.b as f64),
            alpha: self.a as f64,
        }
    }

    pub fn from_linear(c: LinearRgb) -> Self {
        Self::new(
            linear_to_srgb(c.r) as f32,
            linear_to_srgb(c.g) as f32,
            linear_to_srgb(c.b) as f32,
            c.alpha as f32,
        )
    }

    pub fn to_oklab(self) -> Oklab {
        self.to_linear().to_oklab()
    }

    pub fn from_oklab(c: Oklab) -> Self {
        Self::from_linear(c.to_linear())
    }

    pub fn to_oklch(self) -> Oklch {
        self.to_oklab().to_oklch()
    }

    pub fn from_oklch(c: Oklch) -> Self {
        Self::from_oklab(c.to_oklab())
    }

    /// Interpolates from `self` (t = 0) to `other` (t = 1) in OKLab with
    /// premultiplied alpha, as CSS Color 4 does, so a fade to transparent
    /// does not pass through the transparent colour's hue. `t` is not
    /// clamped: a spring overshooting past 1 extrapolates.
    pub fn lerp_oklab(self, other: Color, t: f32) -> Color {
        if t == 0.0 {
            return self;
        }
        if t == 1.0 {
            return other;
        }
        let t = t as f64;
        let a = self.to_oklab();
        let b = other.to_oklab();
        let alpha = a.alpha + (b.alpha - a.alpha) * t;
        let pm = |x: f64, y: f64| x * a.alpha + (y * b.alpha - x * a.alpha) * t;
        let (l, ca, cb) = if alpha.abs() <= f64::EPSILON {
            // Fully transparent: premultiplied values carry no colour, so
            // fall back to a straight interpolation of the components.
            (
                a.l + (b.l - a.l) * t,
                a.a + (b.a - a.a) * t,
                a.b + (b.b - a.b) * t,
            )
        } else {
            (
                pm(a.l, b.l) / alpha,
                pm(a.a, b.a) / alpha,
                pm(a.b, b.b) / alpha,
            )
        };
        Color::from_oklab(Oklab {
            l,
            a: ca,
            b: cb,
            alpha,
        })
    }
}

impl LinearRgb {
    pub fn to_oklab(self) -> Oklab {
        let lms = mul(&M1, [self.r, self.g, self.b]);
        let lab = mul(&M2, lms.map(f64::cbrt));
        Oklab {
            l: lab[0],
            a: lab[1],
            b: lab[2],
            alpha: self.alpha,
        }
    }
}

impl Oklab {
    pub fn to_linear(self) -> LinearRgb {
        let lms_ = mul(&M2_INV, [self.l, self.a, self.b]);
        let rgb = mul(&M1_INV, lms_.map(|v| v * v * v));
        LinearRgb {
            r: rgb[0],
            g: rgb[1],
            b: rgb[2],
            alpha: self.alpha,
        }
    }

    pub fn to_oklch(self) -> Oklch {
        let c = (self.a * self.a + self.b * self.b).sqrt();
        let h = if c < 1e-12 {
            0.0
        } else {
            self.b.atan2(self.a).to_degrees().rem_euclid(360.0)
        };
        Oklch {
            l: self.l,
            c,
            h,
            alpha: self.alpha,
        }
    }
}

impl Oklch {
    pub fn to_oklab(self) -> Oklab {
        let (s, c) = self.h.to_radians().sin_cos();
        Oklab {
            l: self.l,
            a: self.c * c,
            b: self.c * s,
            alpha: self.alpha,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn close(a: Color, b: Color, eps: f32) -> bool {
        (a.r - b.r).abs() <= eps
            && (a.g - b.g).abs() <= eps
            && (a.b - b.b).abs() <= eps
            && (a.a - b.a).abs() <= eps
    }

    #[test]
    fn known_oklab_values() {
        // Reference values from Ottosson's post / CSS Color 4.
        let w = Color::WHITE.to_oklab();
        assert!((w.l - 1.0).abs() < 1e-6 && w.a.abs() < 1e-6 && w.b.abs() < 1e-6);
        let k = Color::BLACK.to_oklab();
        assert!(k.l.abs() < 1e-12);
        let red = Color::rgb(1.0, 0.0, 0.0).to_oklab();
        assert!((red.l - 0.627955).abs() < 1e-5, "{red:?}");
        assert!((red.a - 0.224863).abs() < 1e-5);
        assert!((red.b - 0.125846).abs() < 1e-5);
        let lch = Color::rgb(0.0, 0.0, 1.0).to_oklch();
        assert!((lch.h - 264.052).abs() < 1e-2, "{lch:?}");
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(
            Color::from_hex("#7aa2f7").unwrap().to_rgba8(),
            [0x7a, 0xa2, 0xf7, 255]
        );
        assert_eq!(
            Color::from_hex("f0f8").unwrap().to_rgba8(),
            [255, 0, 255, 136]
        );
        assert_eq!(Color::from_hex("#12345"), None);
        assert_eq!(Color::from_hex("#ééé"), None);
    }

    #[test]
    fn argb8888_premul_byte_order() {
        let c = Color::from_rgba8(255, 128, 0, 128);
        assert_eq!(c.to_argb8888_premul(), [0, 64, 128, 128]);
    }

    #[test]
    fn lerp_endpoints_and_midpoint() {
        let a = Color::from_hex("#ff0000").unwrap();
        let b = Color::from_hex("#0000ff").unwrap();
        assert_eq!(a.lerp_oklab(b, 0.0), a);
        assert_eq!(a.lerp_oklab(b, 1.0), b);
        let mid = a.lerp_oklab(b, 0.5).to_oklab();
        let (oa, ob) = (a.to_oklab(), b.to_oklab());
        assert!((mid.l - (oa.l + ob.l) / 2.0).abs() < 1e-6);
    }

    #[test]
    fn fade_to_transparent_keeps_hue() {
        let red = Color::rgb(1.0, 0.0, 0.0);
        let mid = red.lerp_oklab(Color::TRANSPARENT, 0.5);
        assert!((mid.a - 0.5).abs() < 1e-6);
        assert!(close(mid.with_alpha(1.0), red, 1e-4), "{mid:?}");
    }

    fn unit() -> impl Strategy<Value = f32> {
        0.0f32..=1.0
    }

    proptest! {
        #[test]
        fn srgb_linear_round_trip(r in unit(), g in unit(), b in unit(), a in unit()) {
            let c = Color::new(r, g, b, a);
            prop_assert!(close(Color::from_linear(c.to_linear()), c, 1e-6));
        }

        #[test]
        fn srgb_oklab_round_trip(r in unit(), g in unit(), b in unit(), a in unit()) {
            let c = Color::new(r, g, b, a);
            let back = Color::from_oklab(c.to_oklab());
            prop_assert!(close(back, c, 1e-6), "{c:?} -> {back:?}");
        }

        #[test]
        fn srgb_oklch_round_trip(r in unit(), g in unit(), b in unit(), a in unit()) {
            let c = Color::new(r, g, b, a);
            let back = Color::from_oklch(c.to_oklch());
            prop_assert!(close(back, c, 1e-6), "{c:?} -> {back:?}");
        }

        #[test]
        fn out_of_gamut_round_trip(r in -0.5f32..1.5, g in -0.5f32..1.5, b in -0.5f32..1.5) {
            let c = Color::rgb(r, g, b);
            prop_assert!(close(Color::from_oklab(c.to_oklab()), c, 1e-5));
        }

        #[test]
        fn oklch_hue_in_range(r in unit(), g in unit(), b in unit()) {
            let lch = Color::rgb(r, g, b).to_oklch();
            prop_assert!((0.0..360.0).contains(&lch.h));
            prop_assert!(lch.c >= 0.0);
        }

        #[test]
        fn lerp_stays_between_in_lightness(
            r in unit(), g in unit(), b in unit(),
            r2 in unit(), g2 in unit(), b2 in unit(),
            t in unit(),
        ) {
            let x = Color::rgb(r, g, b);
            let y = Color::rgb(r2, g2, b2);
            let m = x.lerp_oklab(y, t).to_oklab();
            let (lx, ly) = (x.to_oklab().l, y.to_oklab().l);
            prop_assert!(m.l >= lx.min(ly) - 1e-5 && m.l <= lx.max(ly) + 1e-5);
        }
    }
}

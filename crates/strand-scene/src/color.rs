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
        // `from_str_radix` alone would accept a leading `+`.
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
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

/// The highest text luminance [`Color::contrast_reachable`] counts on.
pub const REACH_MAX: f64 = 0.94;

/// The smallest WCAG 2 contrast ratio Strand keeps between a declared
/// text colour and its background (design.md, "Contrast guard").
pub const MIN_CONTRAST: f64 = 3.0;

/// Colour difference below which a clipped colour counts as the same
/// colour (CSS Color 4 gamut mapping's just-noticeable difference, in
/// OKLab ΔE).
const GAMUT_JND: f64 = 0.02;
const GAMUT_EPSILON: f64 = 0.0001;

fn delta_eok(a: Oklab, b: Oklab) -> f64 {
    ((a.l - b.l).powi(2) + (a.a - b.a).powi(2) + (a.b - b.b).powi(2)).sqrt()
}

impl Color {
    /// Brings an out-of-gamut colour into sRGB the way CSS Color 4 does:
    /// keep OKLCH lightness and hue and lower chroma until clipping the
    /// colour changes it by less than a just-noticeable difference
    /// (ΔEOK 0.02). Lightness at or beyond white or black gives white or
    /// black. In-gamut colours are returned as they are; alpha is clamped
    /// to `0..=1` and NaN components read as 0.
    pub fn gamut_mapped(self) -> Color {
        let nan0 = |v: f32| if v.is_nan() { 0.0 } else { v };
        let c = Color::new(nan0(self.r), nan0(self.g), nan0(self.b), nan0(self.a));
        let alpha = c.a.clamp(0.0, 1.0);
        if c.in_gamut(1e-6) {
            return c.clamped();
        }
        let origin = c.to_oklch();
        if !origin.l.is_finite() || !origin.c.is_finite() {
            return c.clamped();
        }
        if origin.l >= 1.0 {
            return Color::WHITE.with_alpha(alpha);
        }
        if origin.l <= 0.0 {
            return Color::BLACK.with_alpha(alpha);
        }
        let at = |chroma: f64| Oklch {
            c: chroma,
            ..origin
        };
        let clip = |lch: Oklch| Color::from_oklch(lch).clamped();
        let mut current = origin;
        let mut clipped = clip(current);
        if delta_eok(clipped.to_oklab(), current.to_oklab()) < GAMUT_JND {
            return clipped.with_alpha(alpha);
        }
        let (mut min, mut max) = (0.0, origin.c);
        let mut min_in_gamut = true;
        while max - min > GAMUT_EPSILON {
            let chroma = (min + max) / 2.0;
            current = at(chroma);
            if min_in_gamut && Color::from_oklch(current).in_gamut(1e-6) {
                min = chroma;
                continue;
            }
            clipped = clip(current);
            let e = delta_eok(clipped.to_oklab(), current.to_oklab());
            if e < GAMUT_JND {
                if GAMUT_JND - e < GAMUT_EPSILON {
                    break;
                }
                min_in_gamut = false;
                min = chroma;
            } else {
                max = chroma;
            }
        }
        clip(current).with_alpha(alpha)
    }

    /// WCAG 2 relative luminance of the (clamped, opaque) colour.
    pub fn relative_luminance(self) -> f64 {
        let l = self.clamped().to_linear();
        0.2126 * l.r + 0.7152 * l.g + 0.0722 * l.b
    }

    /// `self` drawn over the opaque colour `bg` (straight alpha, sRGB
    /// blending as compositors do).
    pub fn over(self, bg: Color) -> Color {
        let a = self.a.clamp(0.0, 1.0);
        let mix = |f: f32, b: f32| f * a + b * (1.0 - a);
        Color::new(mix(self.r, bg.r), mix(self.g, bg.g), mix(self.b, bg.b), 1.0)
    }

    /// WCAG 2 contrast ratio of `self` over `bg` (1 to 21); a translucent
    /// `self` is first drawn over `bg`.
    pub fn contrast(self, bg: Color) -> f64 {
        let fg = if self.a < 1.0 { self.over(bg) } else { self };
        let (a, b) = (fg.relative_luminance(), bg.relative_luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The text colour closest to `self` in OKLCH lightness that keeps at
    /// least `min` contrast over every colour in `bgs` (hue and chroma
    /// kept, gamut-mapped). `self` when it already does. The lightness
    /// moves away from the backgrounds (towards white over dark ones,
    /// towards black over light ones), or the other way if that is the
    /// only way to reach `min`; failing both (backgrounds on both sides),
    /// the reaching lightness nearest the original. When no lightness
    /// reaches `min` over all of them, `bgs[0]` is the one that must be
    /// met (the text's own background) and the rest are let go.
    pub fn with_contrast(self, bgs: &[Color], min: f64) -> Color {
        let all = self.with_contrast_all(bgs, min);
        if bgs.len() <= 1 || bgs.iter().all(|b| all.contrast(*b) >= min) {
            return all;
        }
        self.with_contrast_all(&bgs[..1], min)
    }

    fn with_contrast_all(self, bgs: &[Color], min: f64) -> Color {
        let worst = |c: Color| {
            bgs.iter()
                .map(|b| c.contrast(*b))
                .fold(f64::INFINITY, f64::min)
        };
        if bgs.is_empty() || worst(self) >= min {
            return self;
        }
        let lch = self.to_oklch();
        let l0 = lch.l.clamp(0.0, 1.0);
        let with_l = |l: f64| {
            #[cfg(test)]
            SOLVER_STEPS.with(|n| n.set(n.get() + 1));
            Color::from_oklch(Oklch {
                l: l.clamp(0.0, 1.0),
                ..lch
            })
            .gamut_mapped()
            .with_alpha(self.a)
        };
        let mean_lum = bgs.iter().map(|b| b.relative_luminance()).sum::<f64>() / bgs.len() as f64;
        // Towards white over dark backgrounds, towards black over light.
        let up_first = mean_lum < 0.18;
        // Binary search for the lightness between `l0` and `end` nearest
        // `l0` where `ok` starts to hold (`ok(end)` holds).
        let search = |end: f64, ok: &dyn Fn(Color) -> bool| {
            let (mut near, mut far) = (l0, end);
            for _ in 0..40 {
                let mid = (near + far) / 2.0;
                if ok(with_l(mid)) {
                    far = mid;
                } else {
                    near = mid;
                }
            }
            with_l(far)
        };
        if self.a < 1.0 {
            // Translucent text is judged as drawn over each background,
            // so its luminance depends on the background: move towards
            // the end that reaches `min` (if one does).
            let ends = if up_first { [1.0, 0.0] } else { [0.0, 1.0] };
            for end in ends {
                if worst(with_l(end)) >= min {
                    return search(end, &|c| worst(c) >= min);
                }
            }
            return self;
        }
        // Opaque text of luminance Y reaches `min` over a background of
        // luminance B unless Y falls in the open gap
        // ((B + 0.05) / min - 0.05, min (B + 0.05) - 0.05). The text's
        // luminance rises with its lightness (black at 0, white at 1), so
        // the reachable lightnesses are the complement of the merged
        // gaps: the nearest one above or below is a single search, and
        // gaps covering all of 0..=1 mean nothing reaches `min`.
        let merged = luminance_gaps(bgs, min);
        let y0 = self.relative_luminance();
        let Some(&(lo, hi)) = merged.iter().find(|(lo, hi)| y0 > *lo && y0 < *hi) else {
            // Outside every gap, short of `min` only by rounding.
            return self;
        };
        // A small margin keeps the answer on the reaching side after the
        // colour is rounded to f32.
        const MARGIN: f64 = 1e-6;
        let up = (hi < 1.0).then(|| (hi + MARGIN).min(1.0));
        let down = (lo > 0.0).then(|| (lo - MARGIN).max(0.0));
        let tries: [(f64, Option<f64>); 2] = if up_first {
            [(1.0, up), (0.0, down)]
        } else {
            [(0.0, down), (1.0, up)]
        };
        for (end, target) in tries {
            let Some(y) = target else { continue };
            let c = if end > 0.5 {
                search(end, &|c| c.relative_luminance() >= y)
            } else {
                search(end, &|c| c.relative_luminance() <= y)
            };
            if worst(c) >= min {
                return c;
            }
        }
        if up.is_none() && down.is_none() {
            // No lightness reaches `min` over every background.
            return self;
        }
        // Luminance not monotone in lightness here (a gamut-mapping
        // step): fall back to scanning.
        self.with_contrast_scan(bgs, min)
    }

    /// Whether some opaque text colour reaches `min` over every one of
    /// the opaque `bgs` at once: the luminances that fall short over
    /// each background (see [`Color::with_contrast`]) do not cover all of
    /// black to white. With backgrounds both darker than luminance 0.1
    /// and lighter than 0.3 (at 3:1) nothing does, whatever the text's
    /// hue: the case where a theme swap crossfades instead of springing
    /// (design.md, "How a swap animates").
    ///
    /// Conservative at the light end: a tinted text keeps its chroma, and
    /// gamut mapping leaves its lightest colour a little short of white
    /// (luminance about 0.986 for a saturated blue), so only luminances
    /// up to [`REACH_MAX`] count as reachable. Whenever this says yes,
    /// [`Color::with_contrast`] meets every background.
    pub fn contrast_reachable(bgs: &[Color], min: f64) -> bool {
        let lums: Vec<f64> = bgs.iter().map(|b| b.relative_luminance()).collect();
        luminance_reachable(&lums, min)
    }

    /// The reaching lightness nearest the original found by scanning
    /// 0..=1 (a fallback for colours the luminance model above misjudges;
    /// not reached by ordinary colours), else `self`.
    fn with_contrast_scan(self, bgs: &[Color], min: f64) -> Color {
        let lch = self.to_oklch();
        let mut best: Option<(f64, Color)> = None;
        for i in 0..=SCAN_STEPS {
            let l = i as f64 / SCAN_STEPS as f64;
            #[cfg(test)]
            SOLVER_STEPS.with(|n| n.set(n.get() + 1));
            let c = Color::from_oklch(Oklch { l, ..lch })
                .gamut_mapped()
                .with_alpha(self.a);
            if bgs.iter().all(|b| c.contrast(*b) >= min) {
                let d = (l - lch.l).abs();
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, c));
                }
            }
        }
        best.map_or(self, |(_, c)| c)
    }
}

const SCAN_STEPS: u32 = 400;

/// [`Color::contrast_reachable`] for backgrounds given by their relative
/// luminances (no allocation for up to 16).
pub fn luminance_reachable(lums: &[f64], min: f64) -> bool {
    let mut inline = [(0.0, 0.0); 16];
    let mut heap = Vec::new();
    let gaps: &mut [(f64, f64)] = if lums.len() <= inline.len() {
        &mut inline[..lums.len()]
    } else {
        heap.resize(lums.len(), (0.0, 0.0));
        &mut heap
    };
    for (g, b) in gaps.iter_mut().zip(lums) {
        *g = gap(*b, min);
    }
    gaps.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    // The lowest luminance no gap so far covers.
    let mut y = 0.0;
    for (lo, hi) in gaps.iter() {
        if y <= *lo {
            return true;
        }
        y = f64::max(y, *hi);
    }
    y <= REACH_MAX
}

fn gap(lum: f64, min: f64) -> (f64, f64) {
    let k = lum + 0.05;
    (k / min - 0.05, k * min - 0.05)
}

/// The open luminance intervals where opaque text falls short of `min`
/// over one of `bgs` (over a background of luminance B: between
/// `(B + 0.05) / min − 0.05` and `min (B + 0.05) − 0.05`), merged and
/// sorted.
fn luminance_gaps(bgs: &[Color], min: f64) -> Vec<(f64, f64)> {
    let mut gaps: Vec<(f64, f64)> = bgs
        .iter()
        .map(|b| gap(b.relative_luminance(), min))
        .collect();
    gaps.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(gaps.len());
    for g in gaps {
        match merged.last_mut() {
            Some(last) if g.0 <= last.1 => last.1 = last.1.max(g.1),
            _ => merged.push(g),
        }
    }
    merged
}

#[cfg(test)]
thread_local! {
    /// Lightnesses the solver tried (gamut maps), for cost tests.
    static SOLVER_STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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

    /// Black and white backgrounds leave mid-grey text; a dark and a
    /// mid-light one leave nothing.
    #[test]
    fn reachability_of_split_backgrounds() {
        let bgs = [Color::BLACK, Color::WHITE];
        assert!(Color::contrast_reachable(&bgs, MIN_CONTRAST));
        let split = [
            Color::from_hex("#202020").unwrap(),
            Color::from_hex("#a0a0a0").unwrap(),
        ];
        assert!(!Color::contrast_reachable(&split, MIN_CONTRAST));
        assert!(Color::contrast_reachable(&split[..1], MIN_CONTRAST));
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
        assert_eq!(Color::from_hex("#+f+f+f"), None);
        assert_eq!(Color::from_hex("+f+f+f"), None);
        assert_eq!(Color::from_hex("+ff"), None);
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

    #[test]
    fn gamut_mapping_keeps_lightness_and_hue() {
        // A vivid OKLCH colour far outside sRGB.
        let lch = Oklch {
            l: 0.7,
            c: 0.4,
            h: 150.0,
            alpha: 1.0,
        };
        let raw = Color::from_oklch(lch);
        assert!(!raw.in_gamut(1e-6));
        let m = raw.gamut_mapped();
        assert!(m.in_gamut(1e-6), "{m:?}");
        let back = m.to_oklch();
        assert!((back.l - 0.7).abs() < 0.02, "{back:?}");
        assert!((back.h - 150.0).abs() < 3.0, "{back:?}");
        assert!(back.c < 0.4);
        // Clipping per channel would move the hue further.
        assert_eq!(
            Color::rgb(0.2, 0.4, 0.6).gamut_mapped(),
            Color::rgb(0.2, 0.4, 0.6)
        );
        assert_eq!(Color::rgb(1.5, 1.2, 1.1).gamut_mapped().a, 1.0);
        assert_eq!(
            Color::from_oklch(Oklch {
                l: 1.2,
                c: 0.1,
                h: 30.0,
                alpha: 0.5
            })
            .gamut_mapped(),
            Color::WHITE.with_alpha(0.5)
        );
    }

    #[test]
    fn wcag_contrast_reference_values() {
        assert!((Color::BLACK.contrast(Color::WHITE) - 21.0).abs() < 1e-9);
        assert!((Color::WHITE.contrast(Color::WHITE) - 1.0).abs() < 1e-9);
        // #777 on white is about 4.48:1.
        let grey = Color::from_hex("#777777").unwrap();
        assert!((grey.contrast(Color::WHITE) - 4.48).abs() < 0.01);
        // Translucent text is judged as drawn.
        assert!(Color::BLACK.with_alpha(0.0).contrast(Color::WHITE) < 1.0001);
    }

    fn steps() -> u64 {
        SOLVER_STEPS.with(|n| n.replace(0))
    }

    #[test]
    fn the_solver_is_cheap_even_when_nothing_reaches_the_minimum() {
        let hex = |h| Color::from_hex(h).unwrap();
        // Backgrounds on both sides of the text: no lightness reaches 3:1
        // over all of them, so only the first is met.
        let bgs = [
            hex("#7a7a7a"),
            hex("#000000"),
            hex("#ffffff"),
            hex("#303030"),
        ];
        steps();
        let solved = hex("#808080").with_contrast(&bgs, MIN_CONTRAST);
        let n = steps();
        assert!(solved.contrast(bgs[0]) >= MIN_CONTRAST - 1e-6);
        assert!(n <= 50, "{n} lightnesses tried");
        // A reachable pair over eight backgrounds: one search.
        let surfaces: Vec<Color> = (0..8)
            .map(|i| {
                Color::from_oklch(Oklch {
                    l: 0.12 + i as f64 * 0.02,
                    c: 0.02,
                    h: 270.0,
                    alpha: 1.0,
                })
            })
            .collect();
        let fg = hex("#3a3a50").with_contrast(&surfaces, MIN_CONTRAST);
        let n = steps();
        assert!(
            surfaces
                .iter()
                .all(|b| fg.contrast(*b) >= MIN_CONTRAST - 1e-6)
        );
        assert!(n <= 50, "{n} lightnesses tried");
        // A pair that already passes costs nothing.
        Color::WHITE.with_contrast(&surfaces, MIN_CONTRAST);
        assert_eq!(steps(), 0);
    }

    fn unit() -> impl Strategy<Value = f32> {
        0.0f32..=1.0
    }

    proptest! {
        #[test]
        fn mapped_colours_are_in_gamut(l in 0.0f64..1.0, c in 0.0f64..0.5, h in 0.0f64..360.0) {
            let m = Color::from_oklch(Oklch { l, c, h, alpha: 1.0 }).gamut_mapped();
            prop_assert!(m.in_gamut(1e-6), "{m:?}");
        }

        #[test]
        fn solved_text_reaches_the_minimum(
            t in (unit(), unit(), unit()),
            b in (unit(), unit(), unit()),
        ) {
            let text = Color::rgb(t.0, t.1, t.2);
            let bg = Color::rgb(b.0, b.1, b.2);
            let solved = text.with_contrast(&[bg], MIN_CONTRAST);
            prop_assert!(solved.contrast(bg) >= MIN_CONTRAST - 1e-6, "{text:?} on {bg:?} -> {solved:?}");
            if text.contrast(bg) >= MIN_CONTRAST {
                prop_assert_eq!(solved, text);
            }
        }

        /// Several backgrounds: whenever some lightness reaches the
        /// minimum over all of them (a fine scan finds one), the solver
        /// does too, and the first background is always met.
        #[test]
        fn solved_text_reaches_the_minimum_over_many(
            t in (unit(), unit(), unit()),
            b in proptest::collection::vec((unit(), unit(), unit()), 1..9),
        ) {
            let text = Color::rgb(t.0, t.1, t.2);
            let bgs: Vec<Color> = b.iter().map(|b| Color::rgb(b.0, b.1, b.2)).collect();
            let solved = text.with_contrast(&bgs, MIN_CONTRAST);
            prop_assert!(solved.contrast(bgs[0]) >= MIN_CONTRAST - 1e-6);
            let lch = text.to_oklch();
            let feasible = (0..=400).any(|i| {
                let c = Color::from_oklch(Oklch { l: i as f64 / 400.0, ..lch }).gamut_mapped();
                bgs.iter().all(|b| c.contrast(*b) >= MIN_CONTRAST)
            });
            if feasible {
                for bg in &bgs {
                    prop_assert!(solved.contrast(*bg) >= MIN_CONTRAST - 1e-6, "{text:?} over {bgs:?} -> {solved:?}");
                }
            }
        }

        /// `contrast_reachable` says exactly when the solver can meet
        /// every background: reachable, the solved text does; not, a
        /// fine scan of lightness finds nothing either.
        #[test]
        fn reachability_matches_the_solver(
            t in (unit(), unit(), unit()),
            b in proptest::collection::vec((unit(), unit(), unit()), 1..9),
        ) {
            let text = Color::rgb(t.0, t.1, t.2);
            let bgs: Vec<Color> = b.iter().map(|b| Color::rgb(b.0, b.1, b.2)).collect();
            let solved = text.with_contrast(&bgs, MIN_CONTRAST);
            let all = bgs.iter().all(|b| solved.contrast(*b) >= MIN_CONTRAST - 1e-6);
            if Color::contrast_reachable(&bgs, MIN_CONTRAST) {
                prop_assert!(all, "{text:?} over {bgs:?} -> {solved:?}");
            }
            // Not reachable: no luminance up to REACH_MAX is, so a grey
            // text that dark or darker misses some background.
            if !Color::contrast_reachable(&bgs, MIN_CONTRAST) {
                let any = (0..=400).any(|i| {
                    let c = Color::from_oklch(Oklch { l: i as f64 / 400.0, c: 0.0, h: 0.0, alpha: 1.0 });
                    c.relative_luminance() <= REACH_MAX
                        && bgs.iter().all(|b| c.contrast(*b) >= MIN_CONTRAST + 1e-6)
                });
                prop_assert!(!any, "{bgs:?}: reachable after all");
            }
        }

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

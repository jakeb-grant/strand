//! (M4) The built-in effects on the CPU (design.md, "Generative,
//! data-driven and media": `effect lightning | sparks | shimmer | ripple
//! | aurora { … }` "with a few named knobs", "Trivial–ok on CPU; aurora is
//! GPU"; this is aurora's CPU fallback), and the canvas and sprites the
//! raster sources draw with.
//!
//! Every effect is a pure function of its knobs, its box and `t`: the
//! same tick draws the same pixels, a frame needs no state from the one
//! before, and `reduced_motion`'s frozen clock (`t = 0`) shows one still
//! frame. Each is already running at `t = 0` (a bolt has just struck,
//! a burst is in flight), so that still frame shows the effect. `speed`
//! scales time. Lengths are logical pixels times the surface's scale.

use std::f32::consts::TAU;

use strand_scene::Color;
use vello_cpu::color::PremulRgba8;

/// A built-in effect's kind (`EffectKind` in builtin.schema).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Lightning,
    Sparks,
    Shimmer,
    Ripple,
    Aurora,
}

impl Kind {
    pub(crate) fn from_name(name: &str) -> Option<Kind> {
        match name {
            "lightning" => Some(Kind::Lightning),
            "sparks" => Some(Kind::Sparks),
            "shimmer" => Some(Kind::Shimmer),
            "ripple" => Some(Kind::Ripple),
            "aurora" => Some(Kind::Aurora),
            _ => None,
        }
    }
}

/// A built-in effect with its knobs.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Builtin {
    pub kind: Kind,
    pub speed: f32,
    pub color: Color,
}

/// A pixel buffer drawn into source-over (premultiplied RGBA).
pub(crate) struct Canvas<'a> {
    pub px: &'a mut [PremulRgba8],
    pub w: u32,
    pub h: u32,
}

impl Canvas<'_> {
    /// Draws `c` (straight alpha) at `alpha` over the pixel at `(x, y)`.
    pub(crate) fn over(&mut self, x: i32, y: i32, c: Color, alpha: f32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let a = (c.a * alpha).clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let p = &mut self.px[y as usize * self.w as usize + x as usize];
        let k = 1.0 - a;
        let mix =
            |src: f32, dst: u8| (src * a * 255.0 + dst as f32 * k).round().clamp(0.0, 255.0) as u8;
        *p = PremulRgba8 {
            r: mix(c.r, p.r),
            g: mix(c.g, p.g),
            b: mix(c.b, p.b),
            a: (a * 255.0 + p.a as f32 * k).round().clamp(0.0, 255.0) as u8,
        };
    }

    /// Stamps `sprite` centred at `(cx, cy)` in `c` at `alpha`.
    pub(crate) fn stamp(&mut self, sprite: &Sprite, cx: f32, cy: f32, c: Color, alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let r = sprite.half as i32;
        let (x0, y0) = (cx.round() as i32 - r, cy.round() as i32 - r);
        for j in 0..sprite.size {
            for i in 0..sprite.size {
                let cov = sprite.alpha[j * sprite.size + i];
                if cov > 0.0 {
                    self.over(x0 + i as i32, y0 + j as i32, c, cov * alpha);
                }
            }
        }
    }
}

/// A round sprite: a disc with a soft glow round it.
#[derive(Clone, Debug)]
pub(crate) struct Sprite {
    pub size: usize,
    pub half: usize,
    pub alpha: Vec<f32>,
}

impl Sprite {
    /// A disc of `radius` with a glow reaching `glow` past it (both in
    /// device pixels).
    pub(crate) fn dot(radius: f32, glow: f32) -> Sprite {
        let radius = radius.clamp(0.25, 256.0);
        let glow = glow.clamp(0.0, 256.0);
        let half = (radius + glow).ceil() as usize + 1;
        let size = 2 * half + 1;
        let sigma = (glow / 2.5).max(0.01);
        let alpha = (0..size * size)
            .map(|k| {
                let (x, y) = (
                    (k % size) as f32 - half as f32,
                    (k / size) as f32 - half as f32,
                );
                let d = (x * x + y * y).sqrt();
                let disc = (radius + 0.5 - d).clamp(0.0, 1.0);
                let halo = if glow > 0.0 && d > radius {
                    0.6 * (-(d - radius).powi(2) / (2.0 * sigma * sigma)).exp()
                } else {
                    0.0
                };
                // (A halo under 1/255 is none: the sprite's corners stay
                // clear.)
                let halo = if halo < 1.0 / 255.0 { 0.0 } else { halo };
                disc.max(halo)
            })
            .collect();
        Sprite { size, half, alpha }
    }
}

/// A hash of three integers to `0..1`.
pub(crate) fn rand(a: i64, b: u32, c: u32) -> f32 {
    let mut v = (a as u64)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add((b as u64) << 32 | c as u64);
    v ^= v >> 30;
    v = v.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    v ^= v >> 27;
    v = v.wrapping_mul(0x94d0_49bb_1331_11eb);
    v ^= v >> 31;
    (v >> 40) as f32 / (1u64 << 24) as f32
}

impl Builtin {
    /// Draws the effect into `canvas` at `t` seconds, `scale` device
    /// pixels a logical one.
    pub(crate) fn draw(&self, canvas: &mut Canvas, scale: f32, t: f32) {
        let t = if t.is_finite() { t.max(0.0) } else { 0.0 } * self.speed.clamp(0.0, 100.0);
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        match self.kind {
            Kind::Lightning => lightning(canvas, self.color, s, t),
            Kind::Sparks => sparks(canvas, self.color, s, t),
            Kind::Shimmer => shimmer(canvas, self.color, t),
            Kind::Ripple => ripple(canvas, self.color, s, t),
            Kind::Aurora => aurora(canvas, self.color, t),
        }
    }
}

/// A bolt strikes every 1.6 s, from a random point along the top to one
/// along the bottom, in jagged steps with one branch, flashing and fading
/// over its first 0.3 s.
fn lightning(c: &mut Canvas, color: Color, s: f32, t: f32) {
    const PERIOD: f32 = 1.6;
    const FLASH: f32 = 0.3;
    let k = (t / PERIOD).floor() as i64;
    let phase = t - k as f32 * PERIOD;
    if phase > FLASH {
        return;
    }
    // Bright at the strike, a flicker at a third, then gone.
    let fade = 1.0 - phase / FLASH;
    let flicker = if (phase / FLASH - 0.33).abs() < 0.08 {
        0.5
    } else {
        1.0
    };
    let alpha = fade * flicker;
    let (w, h) = (c.w as f32, c.h as f32);
    let from = (w * (0.3 + 0.4 * rand(k, 1, 0)), 0.0);
    let to = (w * (0.3 + 0.4 * rand(k, 2, 0)), h);
    let mut bolt = vec![from, to];
    // Midpoint displacement: each level halves the jag.
    let mut jag = 0.18 * h.max(w * 0.5);
    for level in 0..6u32 {
        let mut next = Vec::with_capacity(bolt.len() * 2);
        for (i, pair) in bolt.windows(2).enumerate() {
            let (a, b) = (pair[0], pair[1]);
            let m = (
                (a.0 + b.0) / 2.0 + (rand(k, 10 + level, i as u32) - 0.5) * jag,
                (a.1 + b.1) / 2.0,
            );
            next.push(a);
            next.push(m);
        }
        next.push(*bolt.last().unwrap_or(&to));
        bolt = next;
        jag *= 0.5;
    }
    // A branch from a third of the way down, half as bright.
    let at = bolt.len() / 3;
    let start = bolt[at];
    let dir = if rand(k, 3, 0) < 0.5 { -1.0 } else { 1.0 };
    let mut branch = vec![start];
    let mut p = start;
    for i in 0..12u32 {
        p = (
            p.0 + dir * (2.0 + 4.0 * rand(k, 20, i)) * s,
            p.1 + (3.0 + 3.0 * rand(k, 21, i)) * s,
        );
        branch.push(p);
    }
    let core = Sprite::dot(0.8 * s, 4.0 * s);
    let white = Color::WHITE;
    for (line, a) in [(&branch, alpha * 0.5), (&bolt, alpha)] {
        for pair in line.windows(2) {
            let (p, q) = (pair[0], pair[1]);
            let n = ((q.0 - p.0).hypot(q.1 - p.1) / (0.75 * s)).ceil().max(1.0) as u32;
            for i in 0..n {
                let f = i as f32 / n as f32;
                let (x, y) = (p.0 + (q.0 - p.0) * f, p.1 + (q.1 - p.1) * f);
                c.stamp(&core, x, y, color, a * 0.35);
            }
        }
        // The hot core, white.
        for pair in line.windows(2) {
            let (p, q) = (pair[0], pair[1]);
            let n = ((q.0 - p.0).hypot(q.1 - p.1) / 0.5).ceil().max(1.0) as u32;
            for i in 0..n {
                let f = i as f32 / n as f32;
                let (x, y) = (p.0 + (q.0 - p.0) * f, p.1 + (q.1 - p.1) * f);
                c.over(x.round() as i32, y.round() as i32, white, a);
            }
        }
    }
}

/// Bursts of 14 sparks every 1.2 s from the middle, flying out and
/// falling, fading over 0.9 s.
fn sparks(c: &mut Canvas, color: Color, s: f32, t: f32) {
    const PERIOD: f32 = 1.2;
    const LIFE: f32 = 0.9;
    const N: u32 = 14;
    let (cx, cy) = (c.w as f32 / 2.0, c.h as f32 / 2.0);
    let reach = (c.w.min(c.h) as f32 / 2.0).max(4.0);
    let sprite = Sprite::dot(1.0 * s, 3.0 * s);
    let k = (t / PERIOD).floor() as i64;
    // This burst and the one before (still falling).
    for burst in [k - 1, k] {
        let age = t - burst as f32 * PERIOD + 0.15;
        if !(0.0..LIFE).contains(&age) {
            continue;
        }
        for i in 0..N {
            let a = TAU * (i as f32 + rand(burst, 1, i)) / N as f32;
            let v = reach * (1.2 + 1.2 * rand(burst, 2, i));
            let g = reach * 2.5;
            let x = cx + a.cos() * v * age;
            let y = cy + a.sin() * v * age + 0.5 * g * age * age;
            c.stamp(&sprite, x, y, color, 1.0 - age / LIFE);
        }
    }
}

/// A soft highlight band sweeping diagonally across every 1.5 s (half
/// way across at `t = 0`).
fn shimmer(c: &mut Canvas, color: Color, t: f32) {
    const PERIOD: f32 = 1.5;
    let (w, h) = (c.w as f32, c.h as f32);
    let band = (w * 0.25).max(8.0);
    let span = w + 0.5 * h + 2.0 * band;
    // Half way across at `t = 0`, so a still frame shows it.
    let pos = (t / PERIOD + 0.5).fract() * span - band;
    for y in 0..c.h {
        for x in 0..c.w {
            let u = x as f32 + 0.5 * (h - y as f32);
            let d = (u - pos) / band;
            let a = 0.45 * (-d * d * 4.0).exp();
            if a > 0.004 {
                c.over(x as i32, y as i32, color, a);
            }
        }
    }
}

/// Three rings growing from the middle every 2 s, thinning and fading
/// as they go.
fn ripple(c: &mut Canvas, color: Color, s: f32, t: f32) {
    const PERIOD: f32 = 2.0;
    let (cx, cy) = (c.w as f32 / 2.0, c.h as f32 / 2.0);
    let max = (cx * cx + cy * cy).sqrt().max(1.0);
    let rings: Vec<(f32, f32, f32)> = (0..3)
        .map(|j| {
            let ph = (t / PERIOD + j as f32 / 3.0).fract();
            (ph * max, (1.0 + 4.0 * ph) * s, 0.8 * (1.0 - ph))
        })
        .collect();
    for y in 0..c.h {
        for x in 0..c.w {
            let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
            let a = rings
                .iter()
                .map(|(r, w, a)| a * (-((d - r) / w).powi(2)).exp())
                .fold(0.0f32, f32::max);
            if a > 0.004 {
                c.over(x as i32, y as i32, color, a);
            }
        }
    }
}

/// Aurora's CPU fallback: three curtains of light in the colour and its
/// neighbours on the colour wheel across the box. The flattener draws it
/// still, at `t = 0` (`super::raster`); `t` moves the curtains for a
/// caller that animates it.
fn aurora(c: &mut Canvas, color: Color, t: f32) {
    let hues = [color, rotate_hue(color, 50.0), rotate_hue(color, -50.0)];
    let (w, h) = (c.w.max(1) as f32, c.h.max(1) as f32);
    for x in 0..c.w {
        let nx = x as f32 / w;
        // Each curtain's centre line and brightness at this column.
        let curtains: [(f32, f32); 3] = std::array::from_fn(|j| {
            let j = j as f32;
            let centre = 0.3
                + 0.2 * j
                + 0.12 * (TAU * (nx * (1.0 + 0.5 * j) + t * 0.05 * (j + 1.0))).sin()
                + 0.05 * (TAU * (nx * 3.1 - t * 0.08)).sin();
            let light = 0.55 + 0.45 * (TAU * (nx * 2.0 + t * 0.1 * (j + 1.0)) + j).sin();
            (centre, light)
        });
        for y in 0..c.h {
            let ny = y as f32 / h;
            for (j, (centre, light)) in curtains.iter().enumerate() {
                let d = (ny - centre) / 0.13;
                // Curtains hang: brighter at the top edge, a long tail down.
                let fall = if d < 0.0 {
                    (-d * d * 3.0).exp()
                } else {
                    (-d).exp()
                };
                let a = 0.5 * light * fall;
                if a > 0.004 {
                    c.over(x as i32, y as i32, hues[j], a);
                }
            }
        }
    }
}

/// `c` turned `deg` round the colour wheel (a luma-keeping rotation).
pub(crate) fn rotate_hue(c: Color, deg: f32) -> Color {
    let m = super::filter::hue(deg);
    let v = [c.r, c.g, c.b];
    let row =
        |i: usize| (m[i * 5] * v[0] + m[i * 5 + 1] * v[1] + m[i * 5 + 2] * v[2]).clamp(0.0, 1.0);
    Color {
        r: row(0),
        g: row(1),
        b: row(2),
        a: c.a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(kind: Kind, t: f32) -> Vec<PremulRgba8> {
        let mut px = vec![PremulRgba8::from_u8_array([0; 4]); 80 * 40];
        let mut c = Canvas {
            px: &mut px,
            w: 80,
            h: 40,
        };
        Builtin {
            kind,
            speed: 1.0,
            color: Color::from_hex("#89b4fa").unwrap(),
        }
        .draw(&mut c, 1.0, t);
        px
    }

    fn ink(px: &[PremulRgba8]) -> usize {
        px.iter().filter(|p| p.a > 8).count()
    }

    #[test]
    fn every_effect_draws_at_t_zero_and_moves() {
        for kind in [
            Kind::Lightning,
            Kind::Sparks,
            Kind::Shimmer,
            Kind::Ripple,
            Kind::Aurora,
        ] {
            let a = draw(kind, 0.0);
            assert!(ink(&a) > 10, "{kind:?} shows at t = 0");
            assert_eq!(a, draw(kind, 0.0), "{kind:?} is a function of t");
            assert_ne!(a, draw(kind, 0.1), "{kind:?} moves");
        }
        // Lightning is dark between strikes.
        assert_eq!(ink(&draw(Kind::Lightning, 1.0)), 0);
    }

    #[test]
    fn a_sprite_is_a_disc_with_a_glow() {
        let s = Sprite::dot(2.0, 3.0);
        let at = |x: usize, y: usize| s.alpha[y * s.size + x];
        let c = s.half;
        assert_eq!(at(c, c), 1.0);
        assert!(at(c + 3, c) > 0.0 && at(c + 3, c) < 0.7, "halo");
        assert_eq!(at(0, 0), 0.0);
        assert_eq!(Kind::from_name("ripple"), Some(Kind::Ripple));
        assert_eq!(Kind::from_name("rain"), None);
    }
}

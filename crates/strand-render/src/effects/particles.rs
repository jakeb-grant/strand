//! (M4) `particles { rate: 20; life: 1.2s; sprite: dot(3); glow: 6 }`
//! (design.md, "Generative, data-driven and media": "Under 1,000: CPU
//! sprite blits; above: GPU").
//!
//! A particle is born every `1 / rate` seconds at a random point of the
//! box, drifts in a random direction at `speed` × 20 logical pixels a
//! second, and fades in and out over its `life`. The field is a pure
//! function of `t` and already running at `t = 0` (particles born before
//! it are alive), so `reduced_motion`'s still frame shows a field. Each
//! particle is one blit of a precomputed sprite (`dot(size)`: a disc of
//! that diameter, with a glow of `glow` round it). At most 1,000 are
//! alive at once on the CPU (the rate is capped so `rate · life` stays
//! within it); the GPU version above that is wave 3's.

use std::f32::consts::TAU;

use strand_scene::Color;

use super::builtin::{Canvas, Sprite, rand};

/// Most particles alive at once on the CPU.
pub(crate) const MAX_ALIVE: f32 = 1000.0;

/// Drift at `speed: 1`, logical pixels a second.
const DRIFT: f32 = 20.0;

/// A particle field's knobs.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Particles {
    /// Births a second.
    pub rate: f32,
    /// Seconds each lives.
    pub life: f32,
    /// The sprite's diameter and glow, logical pixels.
    pub size: f32,
    pub glow: f32,
    pub speed: f32,
    pub color: Color,
}

impl Particles {
    /// The rate actually drawn: at most [`MAX_ALIVE`] alive at once.
    pub(crate) fn rate(&self) -> f32 {
        let rate = self.rate.clamp(0.0, 10_000.0);
        let life = self.life.max(0.0);
        if rate * life > MAX_ALIVE {
            MAX_ALIVE / life
        } else {
            rate
        }
    }

    /// Draws the field at `t` seconds into `c`, `scale` device pixels a
    /// logical one.
    pub(crate) fn draw(&self, c: &mut Canvas, scale: f32, t: f32) {
        let rate = self.rate();
        let life = self.life;
        if !(rate > 0.0 && life > 0.0 && life.is_finite()) {
            return;
        }
        let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let sprite = Sprite::dot(
            (self.size.clamp(0.5, 200.0) * s) / 2.0,
            self.glow.clamp(0.0, 200.0) * s,
        );
        let (w, h) = (c.w as f32, c.h as f32);
        let v = DRIFT * self.speed.clamp(0.0, 100.0) * s;
        // Every particle born in (t - life, t].
        let last = (t * rate).floor() as i64;
        let first = ((t - life) * rate).floor() as i64 + 1;
        for i in first..=last {
            let born = i as f32 / rate;
            let age = (t - born) / life;
            if !(0.0..1.0).contains(&age) {
                continue;
            }
            let a = TAU * rand(i, 1, 0);
            let x = w * rand(i, 2, 0) + a.cos() * v * age * life;
            let y = h * rand(i, 3, 0) + a.sin() * v * age * life;
            let alpha = (age * std::f32::consts::PI).sin();
            c.stamp(&sprite, x, y, self.color, alpha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vello_cpu::color::PremulRgba8;

    fn field(rate: f32, life: f32) -> Particles {
        Particles {
            rate,
            life,
            size: 3.0,
            glow: 0.0,
            speed: 1.0,
            color: Color::WHITE,
        }
    }

    fn dots(p: Particles, t: f32) -> usize {
        let mut px = vec![PremulRgba8::from_u8_array([0; 4]); 200 * 200];
        let mut c = Canvas {
            px: &mut px,
            w: 200,
            h: 200,
        };
        p.draw(&mut c, 1.0, t);
        // Centres: fully covered pixels with all four neighbours covered.
        let on = |x: usize, y: usize| px[y * 200 + x].a > 200;
        (1..199)
            .flat_map(|y| (1..199).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                on(x, y) && on(x - 1, y) && on(x + 1, y) && on(x, y - 1) && on(x, y + 1)
            })
            .count()
    }

    #[test]
    fn the_field_is_running_at_t_zero_and_moves() {
        let p = field(20.0, 1.2);
        let lit = dots(p, 0.0);
        assert!(lit > 5, "{lit}");
        let mut a = vec![PremulRgba8::from_u8_array([0; 4]); 64 * 64];
        let mut b = a.clone();
        p.draw(
            &mut Canvas {
                px: &mut a,
                w: 64,
                h: 64,
            },
            1.0,
            0.5,
        );
        p.draw(
            &mut Canvas {
                px: &mut b,
                w: 64,
                h: 64,
            },
            1.0,
            0.6,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn at_most_a_thousand_are_alive() {
        assert_eq!(field(20.0, 1.2).rate(), 20.0);
        assert_eq!(field(5000.0, 2.0).rate(), 500.0);
        assert_eq!(field(0.0, 1.0).rate(), 0.0);
    }
}

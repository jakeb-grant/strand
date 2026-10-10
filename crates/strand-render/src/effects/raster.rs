//! (M4) CPU raster sources built from props (design.md, "Runtime changes
//! these need", item 3): `grain:`, `particles` ([`super::particles`]) and
//! the built-in `effect`s ([`super::builtin`]). Each frame the flattener builds a node's source from its
//! resolved props ([`built`]) and draws it through
//! [`crate::offscreen::RasterNodes::pixmap_from`], which keeps the last
//! pixmap while the props, size, scale and tick are unchanged. Before the
//! props resolve, [`rate`] gives the node's clock from its raw props.
//!
//! `grain: amount` is film grain over the box's background: every device
//! pixel a grey of random lightness at `amount` opacity, new at 12 fps
//! (design.md: "grain at 12 fps"). Under `reduced_motion` its clock stands
//! still and the grain is static (design.md: "Trivial when static").

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use strand_scene::{Color, NodeKind, Prop, PropValue, TimeContext};
use vello_cpu::color::PremulRgba8;

use super::builtin::{Builtin, Canvas, Kind};
use super::particles::Particles;
use super::{keyword, number};
use crate::clock::Rate;
use crate::offscreen::RasterSource;
use crate::tree::Node;

/// `grain:`'s clock: 12 fps.
pub(crate) const GRAIN: Duration = Duration::from_nanos(1_000_000_000 / 12);

/// The clock rate of the raster source `node`'s props build, read from its
/// raw props (before they resolve): any `grain:` that is not a literal
/// zero or less.
pub(crate) fn rate(node: &Node) -> Option<Rate> {
    match node.kind {
        NodeKind::Particles => return Some(Rate::Refresh),
        NodeKind::Effect => {
            let kind = match node.get(Prop::Style) {
                Some(PropValue::Keyword(k) | PropValue::Text(k)) => Kind::from_name(k),
                _ => None,
            };
            match kind {
                Some(Kind::Shimmer) => return Some(Rate::Every(crate::clock::SHIMMER)),
                // Aurora's CPU fallback is one still frame: no clock.
                Some(Kind::Aurora) => {}
                Some(_) => return Some(Rate::Refresh),
                None => {}
            }
        }
        _ => {}
    }
    let grain = node.get(Prop::Grain).is_some_and(|v| match v {
        PropValue::Number(n) => *n > 0.0,
        _ => true,
    });
    grain.then_some(Rate::Every(GRAIN))
}

/// A raster source built from a node's resolved props.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Built {
    Grain(Grain),
    Effect(Builtin),
    Particles(Particles),
}

/// The source the resolved props (`get`) of a node of `kind` build, if
/// any; `color` is what it draws in (its own `color`, else `$accent`).
pub(crate) fn built<'v>(
    kind: NodeKind,
    get: impl Fn(Prop) -> Option<&'v PropValue>,
    color: Color,
) -> Option<Built> {
    let speed = get(Prop::Speed).and_then(number).unwrap_or(1.0).max(0.0);
    match kind {
        NodeKind::Effect => {
            let kind = get(Prop::Style)
                .and_then(keyword)
                .and_then(Kind::from_name)?;
            return Some(Built::Effect(Builtin { kind, speed, color }));
        }
        NodeKind::Particles => {
            let size = match get(Prop::Sprite) {
                Some(PropValue::Call { name, args }) if name == "dot" => {
                    args.first().and_then(number).unwrap_or(3.0)
                }
                _ => 3.0,
            };
            let life = match get(Prop::Life) {
                Some(PropValue::Duration(d)) => d.as_secs_f32(),
                Some(v) => number(v).unwrap_or(1.0),
                None => 1.0,
            };
            return Some(Built::Particles(Particles {
                rate: get(Prop::Rate).and_then(number).unwrap_or(10.0),
                life,
                size,
                // `glow: r` or `glow: r, colour` (as it springs, a list).
                glow: get(Prop::Glow)
                    .and_then(|v| match v {
                        PropValue::List(items) => items.first().and_then(number),
                        v => number(v),
                    })
                    .unwrap_or(0.0)
                    .max(0.0),
                speed,
                color,
            }));
        }
        _ => {}
    }
    let amount = get(Prop::Grain).and_then(number)?.clamp(0.0, 1.0);
    (amount > 0.0).then_some(Built::Grain(Grain { amount }))
}

impl Built {
    /// What it was built from, for the pixmap's key.
    pub(crate) fn config(&self) -> u64 {
        let mut h = DefaultHasher::new();
        match self {
            Built::Grain(g) => (0u8, g.amount.to_bits()).hash(&mut h),
            Built::Effect(b) => (1u8, b.kind, b.speed.to_bits(), color_bits(b.color)).hash(&mut h),
            Built::Particles(p) => (
                2u8,
                [p.rate, p.life, p.size, p.glow, p.speed].map(f32::to_bits),
                color_bits(p.color),
            )
                .hash(&mut h),
        }
        h.finish()
    }

    /// `time` as the source sees it: grain's tick (so a node whose props
    /// also read time, and so runs at refresh, keeps its 12 fps grain and
    /// its pixmap between ticks); aurora's `t = 0` (its CPU fallback is
    /// still, so its pixmap is drawn once per size).
    pub(crate) fn time(&self, time: TimeContext) -> TimeContext {
        match self {
            Built::Grain(_) => TimeContext {
                t: Grain::tick(time.t) as f32,
                ..time
            },
            Built::Effect(Builtin {
                kind: Kind::Aurora, ..
            }) => TimeContext { t: 0.0, ..time },
            Built::Effect(_) | Built::Particles(_) => time,
        }
    }

    /// What it draws in place of the GPU's version, if anything.
    fn fallback(&self) -> Option<Fallback> {
        match self {
            Built::Effect(Builtin {
                kind: Kind::Aurora, ..
            }) => Some(Fallback::Aurora),
            Built::Particles(p) if p.capped() => Some(Fallback::Particles),
            _ => None,
        }
    }
}

/// An effect the CPU draws in place of the GPU's version (m4-plan: "Before
/// the GPU path, particles cap at 1,000 and aurora is static, with a
/// notice").
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Fallback {
    Aurora,
    Particles,
}

impl Fallback {
    const ALL: [Fallback; 2] = [Fallback::Aurora, Fallback::Particles];

    fn bit(self) -> u8 {
        match self {
            Fallback::Aurora => 1,
            Fallback::Particles => 2,
        }
    }

    fn notice(self) -> &'static str {
        match self {
            Fallback::Aurora => {
                "effect aurora: drawn as a still CPU frame (its animated version needs the GPU)"
            }
            Fallback::Particles => {
                "particles: at most 1,000 alive on the CPU (rate × life above that needs the GPU)"
            }
        }
    }
}

/// (M4) The CPU fallbacks drawn so far, each said once a run: the
/// flattener notes every source it draws, and the host takes the notices
/// to log and send to `strand watch` (`Renderer::take_effect_notices`).
#[derive(Debug, Default)]
pub struct Fallbacks {
    drawn: std::cell::Cell<u8>,
    said: u8,
}

impl Fallbacks {
    /// `built` is drawn this frame.
    pub(crate) fn note(&self, built: &Built) {
        if let Some(f) = built.fallback() {
            self.drawn.set(self.drawn.get() | f.bit());
        }
    }

    /// The notices of fallbacks drawn and not said yet.
    pub(crate) fn take(&mut self) -> Vec<String> {
        let new = self.drawn.get() & !self.said;
        self.said |= new;
        Fallback::ALL
            .into_iter()
            .filter(|f| new & f.bit() != 0)
            .map(|f| f.notice().to_string())
            .collect()
    }
}

fn color_bits(c: Color) -> [u32; 4] {
    [c.r, c.g, c.b, c.a].map(f32::to_bits)
}

impl RasterSource for Built {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, time: TimeContext) {
        match self {
            Built::Grain(g) => g.draw(pixels, w, h, scale, time),
            Built::Effect(b) => b.draw(&mut Canvas { px: pixels, w, h }, scale, time.t),
            Built::Particles(p) => p.draw(&mut Canvas { px: pixels, w, h }, scale, time.t),
        }
    }

    fn rate(&self) -> Rate {
        match self {
            Built::Grain(_) => Rate::Every(GRAIN),
            Built::Effect(Builtin {
                kind: Kind::Shimmer,
                ..
            }) => Rate::Every(crate::clock::SHIMMER),
            Built::Effect(_) | Built::Particles(_) => Rate::Refresh,
        }
    }
}

/// `grain: amount`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Grain {
    pub amount: f32,
}

impl Grain {
    /// The 12 fps tick `t` seconds into the node's clock (the nearest:
    /// a capped clock's `t` is a whole number of periods, which rounding
    /// errors can leave a hair under the tick).
    fn tick(t: f32) -> u32 {
        if t.is_finite() && t > 0.0 {
            (t as f64 * 12.0).round().min(u32::MAX as f64) as u32
        } else {
            0
        }
    }

    /// Each pixel a grey of hashed lightness at `amount` opacity; `time`
    /// carries the tick ([`Built::time`]).
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, _h: u32, _scale: f32, time: TimeContext) {
        let a = self.amount.clamp(0.0, 1.0);
        let alpha = (a * 255.0).round() as u8;
        let seed = (time.t.max(0.0) as u32).wrapping_mul(0x9e37_79b9);
        for (i, p) in pixels.iter_mut().enumerate() {
            let x = (i as u32) % w.max(1);
            let y = (i as u32) / w.max(1);
            let l = noise(x, y, seed) as f32 / u32::MAX as f32;
            let v = (l * a * 255.0).round() as u8;
            *p = PremulRgba8 {
                r: v,
                g: v,
                b: v,
                a: alpha,
            };
        }
    }
}

/// A well-mixed hash of a pixel and a seed (lowbias32).
fn noise(x: u32, y: u32, seed: u32) -> u32 {
    let mut v = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ seed;
    v ^= v >> 16;
    v = v.wrapping_mul(0x7feb_352d);
    v ^= v >> 15;
    v = v.wrapping_mul(0x846c_a68b);
    v ^= v >> 16;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(g: Grain, t: f32) -> Vec<PremulRgba8> {
        let mut px = vec![PremulRgba8::from_u8_array([0; 4]); 64 * 16];
        let b = Built::Grain(g);
        b.draw(&mut px, 64, 16, 1.0, b.time(TimeContext::at(t)));
        px
    }

    /// A particle field's sprite glow is its radius, whether `glow:` is a
    /// lone radius or (springing, or with a colour) a list.
    #[test]
    fn particles_glow_by_radius_alone_or_in_a_list() {
        for v in [
            PropValue::Number(6.0),
            PropValue::List(vec![PropValue::Number(6.0), PropValue::Color(Color::WHITE)]),
        ] {
            let get = |p: Prop| (p == Prop::Glow).then_some(&v);
            match built(NodeKind::Particles, get, Color::WHITE) {
                Some(Built::Particles(p)) => assert_eq!(p.glow, 6.0, "{v:?}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn grain_is_noise_at_its_amount_that_changes_per_tick() {
        let g = Grain { amount: 0.25 };
        let a = draw(g, 0.0);
        assert!(
            a.iter()
                .all(|p| p.a == 64 && p.r == p.g && p.g == p.b && p.r <= 64)
        );
        // Its mean lightness is half way, and it varies.
        let mean = a.iter().map(|p| p.r as f32).sum::<f32>() / a.len() as f32;
        assert!((mean - 32.0).abs() < 3.0, "{mean}");
        assert!(a.iter().any(|p| p.r < 8) && a.iter().any(|p| p.r > 56));
        // The same tick draws the same grain; the next draws another.
        assert_eq!(a, draw(g, 0.04));
        assert_ne!(a, draw(g, 1.0 / 12.0));
        // A capped clock's t a hair under a tick is that tick.
        assert_eq!(Grain::tick(GRAIN.as_secs_f32() * 5.0), 5);
    }

    #[test]
    fn grain_builds_from_a_positive_amount() {
        let v = PropValue::Number(0.04);
        let get = |p: Prop| (p == Prop::Grain).then_some(&v);
        assert_eq!(
            built(NodeKind::Box, get, Color::WHITE),
            Some(Built::Grain(Grain { amount: 0.04 }))
        );
        let zero = PropValue::Number(0.0);
        assert_eq!(
            built(
                NodeKind::Box,
                |p| (p == Prop::Grain).then_some(&zero),
                Color::WHITE
            ),
            None
        );
        assert_eq!(built(NodeKind::Box, |_| None, Color::WHITE), None);
    }
}

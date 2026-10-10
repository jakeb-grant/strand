//! (M4) CPU raster sources built from props (design.md, "Runtime changes
//! these need", item 3): `grain:` now, particles and the built-in effects
//! as they land. Each frame the flattener builds a node's source from its
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

use strand_scene::{NodeKind, Prop, PropValue, TimeContext};
use vello_cpu::color::PremulRgba8;

use super::number;
use crate::clock::Rate;
use crate::offscreen::RasterSource;
use crate::tree::Node;

/// `grain:`'s clock: 12 fps.
pub(crate) const GRAIN: Duration = Duration::from_nanos(1_000_000_000 / 12);

/// The clock rate of the raster source `node`'s props build, read from its
/// raw props (before they resolve): any `grain:` that is not a literal
/// zero or less.
pub(crate) fn rate(node: &Node) -> Option<Rate> {
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
}

/// The source the resolved props (`get`) of a node of `kind` build, if
/// any.
pub(crate) fn built<'v>(
    _kind: NodeKind,
    get: impl Fn(Prop) -> Option<&'v PropValue>,
) -> Option<Built> {
    let amount = get(Prop::Grain).and_then(number)?.clamp(0.0, 1.0);
    (amount > 0.0).then_some(Built::Grain(Grain { amount }))
}

impl Built {
    /// What it was built from, for the pixmap's key.
    pub(crate) fn config(&self) -> u64 {
        let mut h = DefaultHasher::new();
        match self {
            Built::Grain(g) => (0u8, g.amount.to_bits()).hash(&mut h),
        }
        h.finish()
    }

    /// `time` as the source sees it: grain's tick (so a node whose props
    /// also read time, and so runs at refresh, keeps its 12 fps grain and
    /// its pixmap between ticks).
    pub(crate) fn time(&self, time: TimeContext) -> TimeContext {
        match self {
            Built::Grain(_) => TimeContext {
                t: Grain::tick(time.t) as f32,
                ..time
            },
        }
    }
}

impl RasterSource for Built {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, time: TimeContext) {
        match self {
            Built::Grain(g) => g.draw(pixels, w, h, scale, time),
        }
    }

    fn rate(&self) -> Rate {
        match self {
            Built::Grain(_) => Rate::Every(GRAIN),
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
            built(NodeKind::Box, get),
            Some(Built::Grain(Grain { amount: 0.04 }))
        );
        let zero = PropValue::Number(0.0);
        assert_eq!(
            built(NodeKind::Box, |p| (p == Prop::Grain).then_some(&zero)),
            None
        );
        assert_eq!(built(NodeKind::Box, |_| None), None);
    }
}

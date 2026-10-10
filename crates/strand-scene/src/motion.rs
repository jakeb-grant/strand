//! Motion math: analytic damped springs, timed curves and the animated
//! values the render thread samples every frame (design.md, "Layout,
//! animation and input"; "Testing": deterministic springs).
//!
//! Everything here is a pure function of timestamps: a [`Motion`] sampled
//! at the same times always yields the same values, so animation frames
//! are testable as images. Times are [`Duration`]s on the presentation
//! clock ([`crate::PaintTarget::time`]).
//!
//! - [`Spring`] is a unit-mass damped harmonic oscillator, `spring(700,
//!   0.9)` in the language: stiffness and damping *ratio* (1 is critical,
//!   below 1 overshoots). Its position and velocity at any time are closed
//!   form ([`Spring::step`]); there is no integration and so no drift.
//! - [`Curve`] is how a value moves to a new target: a spring, a timed
//!   cubic bézier (`~ 200ms`, `~ ease(out_back, 300ms)`, `~ bezier(…)`) or
//!   [`Curve::Instant`].
//! - [`Motion<N>`] is an `N`-channel value in flight (one channel for a
//!   length, four for a colour in premultiplied OKLab). [`Motion::retarget`]
//!   starts a new segment from wherever the value is at the next sample,
//!   *keeping its velocity*, so interrupted animations never jolt. A
//!   spring starts with that velocity; a timed curve adds it as a term
//!   that fades out over its duration (`v0·t·(1 − t/d)²`), on top of
//!   the curve's own start velocity (zero for the `~ 200ms` default).
//!
//! The theme's palette springs and the renderer's prop springs share this
//! module; see `docs/architecture.md` ("`strand-scene`", Motion).

use std::time::Duration;

use crate::color::{Color, Oklab};
use crate::protocol::{Easing, Transition};

/// A damped spring: `spring(stiffness, damping)` with unit mass, so the
/// natural frequency is `sqrt(stiffness)` rad/s and `damping` is the
/// damping ratio.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Spring {
    pub stiffness: f32,
    pub damping: f32,
}

/// `$motion.spatial`: movement and size (design.md, theme file).
pub const SPATIAL: Spring = Spring {
    stiffness: 700.0,
    damping: 0.9,
};

/// `$motion.effects`: colour and opacity.
pub const EFFECTS: Spring = Spring {
    stiffness: 1600.0,
    damping: 1.0,
};

/// `$motion.bouncy`.
pub const BOUNCY: Spring = Spring {
    stiffness: 380.0,
    damping: 0.75,
};

/// The longest a timed curve may run, and the time after which any
/// spring counts as settled whatever its residue: a malformed transition
/// (`spring(0.001, 0)`) never keeps frames coming for ever.
pub const MAX_MOTION: Duration = Duration::from_secs(10);

impl Spring {
    /// A spring with usable parameters: stiffness and damping finite,
    /// stiffness positive and damping not negative. Stiffness is capped
    /// at 1e6 (a millisecond-scale spring), damping at 100.
    pub fn new(stiffness: f32, damping: f32) -> Option<Spring> {
        (stiffness.is_finite() && damping.is_finite() && stiffness > 0.0 && damping >= 0.0).then(
            || Spring {
                stiffness: stiffness.min(1e6),
                damping: damping.min(100.0),
            },
        )
    }

    /// Displacement from the target and velocity (units per second) `t`
    /// seconds after release at displacement `x0` with velocity `v0`.
    pub fn step(self, x0: f64, v0: f64, t: f64) -> (f64, f64) {
        if t <= 0.0 {
            return (x0, v0);
        }
        let w0 = (self.stiffness.max(1e-6) as f64).sqrt();
        let z = self.damping.max(0.0) as f64;
        if (z - 1.0).abs() < 1e-6 {
            // Critically damped.
            let c = v0 + w0 * x0;
            let e = (-w0 * t).exp();
            let x = (x0 + c * t) * e;
            let v = (c - w0 * (x0 + c * t)) * e;
            (x, v)
        } else if z < 1.0 {
            // Underdamped: overshoots and rings down.
            let wd = w0 * (1.0 - z * z).sqrt();
            let b = (v0 + z * w0 * x0) / wd;
            let e = (-z * w0 * t).exp();
            let (s, c) = (wd * t).sin_cos();
            let x = e * (x0 * c + b * s);
            let v = e * ((b * wd - z * w0 * x0) * c - (x0 * wd + z * w0 * b) * s);
            (x, v)
        } else {
            // Overdamped: two decaying exponentials.
            let r = (z * z - 1.0).sqrt();
            let r1 = -w0 * (z - r);
            let r2 = -w0 * (z + r);
            let c2 = (v0 - r1 * x0) / (r2 - r1);
            let c1 = x0 - c2;
            let (e1, e2) = ((r1 * t).exp(), (r2 * t).exp());
            (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
        }
    }
}

/// How a value moves to a new target.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Curve {
    /// Snaps.
    Instant,
    Spring(Spring),
    /// A timed curve: `~ 200ms`, `~ ease(out_back, 300ms)`.
    Timed {
        duration: Duration,
        easing: Easing,
    },
}

impl Curve {
    /// The curve of a resolved transition (see
    /// [`crate::TokenScope::transition`]). `Default` and `Token`, which
    /// only a token scope resolves, and malformed springs snap.
    pub fn of(t: &Transition) -> Curve {
        match t {
            Transition::Spring { stiffness, damping } => {
                Spring::new(*stiffness, *damping).map_or(Curve::Instant, Curve::Spring)
            }
            Transition::Duration { duration, easing } if !duration.is_zero() => Curve::Timed {
                duration: (*duration).min(MAX_MOTION),
                easing: *easing,
            },
            _ => Curve::Instant,
        }
    }
}

impl Transition {
    /// The design's spring for a class's `Default` when no `$motion.*`
    /// token says otherwise (`$motion.spatial` and `$motion.effects`).
    pub fn of_spring(s: Spring) -> Transition {
        Transition::Spring {
            stiffness: s.stiffness,
            damping: s.damping,
        }
    }
}

/// `easing` at progress `p` (`0..=1`); curves like `out_back` overshoot
/// past 1.
pub fn ease(easing: Easing, p: f32) -> f32 {
    let p = if p.is_nan() { 1.0 } else { p.clamp(0.0, 1.0) };
    match easing {
        Easing::Linear => p,
        Easing::OutElastic => {
            if p <= 0.0 || p >= 1.0 {
                return p;
            }
            let c4 = std::f32::consts::TAU / 3.0;
            2f32.powf(-10.0 * p) * ((p * 10.0 - 0.75) * c4).sin() + 1.0
        }
        Easing::OutBounce => {
            const N1: f32 = 7.5625;
            const D1: f32 = 2.75;
            if p < 1.0 / D1 {
                N1 * p * p
            } else if p < 2.0 / D1 {
                let q = p - 1.5 / D1;
                N1 * q * q + 0.75
            } else if p < 2.5 / D1 {
                let q = p - 2.25 / D1;
                N1 * q * q + 0.9375
            } else {
                let q = p - 2.625 / D1;
                N1 * q * q + 0.984375
            }
        }
        Easing::Bezier { x1, y1, x2, y2 } => {
            if p <= 0.0 || p >= 1.0 {
                return p;
            }
            let fin = |v: f32| if v.is_finite() { v as f64 } else { 0.0 };
            // CSS clamps the x control points to 0..=1 so x(s) is monotonic.
            let (x1, x2) = (fin(x1).clamp(0.0, 1.0), fin(x2).clamp(0.0, 1.0));
            let (y1, y2) = (fin(y1), fin(y2));
            let bez = |a: f64, b: f64, s: f64| {
                let u = 1.0 - s;
                3.0 * u * u * s * a + 3.0 * u * s * s * b + s * s * s
            };
            let dbez = |a: f64, b: f64, s: f64| {
                let u = 1.0 - s;
                3.0 * u * u * a + 6.0 * u * s * (b - a) + 3.0 * s * s * (1.0 - b)
            };
            let x = p as f64;
            // Newton from s = x, then bisection if it wanders.
            let mut s = x;
            let mut ok = false;
            for _ in 0..8 {
                let err = bez(x1, x2, s) - x;
                if err.abs() < 1e-7 {
                    ok = true;
                    break;
                }
                let d = dbez(x1, x2, s);
                if d.abs() < 1e-9 {
                    break;
                }
                s -= err / d;
                if !(0.0..=1.0).contains(&s) {
                    break;
                }
            }
            if !ok {
                let (mut lo, mut hi) = (0.0, 1.0);
                s = x;
                for _ in 0..40 {
                    s = (lo + hi) / 2.0;
                    if bez(x1, x2, s) < x {
                        lo = s;
                    } else {
                        hi = s;
                    }
                }
            }
            bez(y1, y2, s) as f32
        }
    }
}

/// One segment of a [`Motion`]: from where it started towards its
/// target.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Segment<const N: usize> {
    start: Duration,
    from: [f32; N],
    vel: [f32; N],
    target: [f32; N],
    curve: Curve,
}

impl<const N: usize> Segment<N> {
    /// Position and velocity at `at`.
    fn at(&self, at: Duration) -> ([f32; N], [f32; N]) {
        let t = at.saturating_sub(self.start).as_secs_f64();
        let mut pos = self.target;
        let mut vel = [0.0; N];
        match self.curve {
            Curve::Instant => {}
            Curve::Spring(s) => {
                for i in 0..N {
                    let x0 = (self.from[i] - self.target[i]) as f64;
                    let (x, v) = s.step(x0, self.vel[i] as f64, t);
                    pos[i] = self.target[i] + x as f32;
                    vel[i] = v as f32;
                }
            }
            Curve::Timed { duration, easing } => {
                let d = duration.as_secs_f64().max(1e-6);
                let p = (t / d).min(1.0) as f32;
                let e = ease(easing, p);
                // Velocity by a small finite difference of the curve.
                let h = 1e-3f32;
                let e2 = ease(easing, (p + h).min(1.0));
                let de = if p < 1.0 {
                    (e2 - e) / h / d as f32
                } else {
                    0.0
                };
                // The velocity it was moving with when retargeted fades
                // out over the duration: `v0·t·(1 − t/d)²` starts at v0
                // and ends at rest exactly on the target, so a timed curve
                // interrupting a moving value never jolts.
                let u = (t / d).min(1.0);
                let carry = (t.min(d) * (1.0 - u) * (1.0 - u)) as f32;
                let dcarry = ((1.0 - u) * (1.0 - 3.0 * u)) as f32;
                for i in 0..N {
                    let span = self.target[i] - self.from[i];
                    pos[i] = self.from[i] + span * e + self.vel[i] * carry;
                    vel[i] = span * de + if p < 1.0 { self.vel[i] * dcarry } else { 0.0 };
                }
            }
        }
        (pos, vel)
    }

    /// Position and velocity at `at`, or `None` once settled there (as
    /// [`Segment::settled`]), the curve evaluated at most once.
    fn moving(&self, at: Duration, eps: f32) -> Option<([f32; N], [f32; N])> {
        let t = at.saturating_sub(self.start);
        match self.curve {
            Curve::Instant => None,
            Curve::Timed { duration, .. } if t >= duration => None,
            Curve::Spring(_) if t >= MAX_MOTION => None,
            Curve::Timed { .. } => Some(self.at(at)),
            Curve::Spring(_) => {
                let (p, v) = self.at(at);
                let rest = (0..N)
                    .all(|i| (p[i] - self.target[i]).abs() <= eps && v[i].abs() <= eps * 10.0);
                (!rest).then_some((p, v))
            }
        }
    }

    /// At rest at its target by `at`, within `eps` of position (and
    /// `eps` × 10 per second of velocity).
    fn settled(&self, at: Duration, eps: f32) -> bool {
        let t = at.saturating_sub(self.start);
        match self.curve {
            Curve::Instant => true,
            Curve::Timed { duration, .. } => t >= duration,
            Curve::Spring(_) if t >= MAX_MOTION => true,
            Curve::Spring(_) => {
                let (p, v) = self.at(at);
                (0..N).all(|i| (p[i] - self.target[i]).abs() <= eps && v[i].abs() <= eps * 10.0)
            }
        }
    }
}

/// A retarget waiting for its first sample, which fixes its start time.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Pending<const N: usize> {
    target: [f32; N],
    curve: Curve,
    /// Added to the position at the start (a FLIP: the value jumps by
    /// this, then springs back).
    shift: [f32; N],
}

/// An `N`-channel animated value.
///
/// [`Motion::retarget`] and [`Motion::shift`] do not take a time: the
/// render thread learns the time of the frame that will show a change
/// only when it paints. The change starts at the first
/// [`Motion::sample`] after it, at that frame's time less one frame lead
/// (at most [`START_LEAD`], and never before the previous sample), so
/// the first frame after a change already shows it moving.
#[derive(Clone, Debug, PartialEq)]
pub struct Motion<const N: usize> {
    seg: Segment<N>,
    pending: Option<Pending<N>>,
    /// Settling tolerance, in the channels' units.
    eps: f32,
    last: Option<Duration>,
}

/// How far before the first frame that samples it a change starts (one
/// refresh at 60 Hz), so that frame already shows progress.
pub const START_LEAD: Duration = Duration::from_micros(16_667);

impl<const N: usize> Motion<N> {
    /// A value at rest; `eps` is how close to its target (in its own
    /// units) it must come to count as settled.
    pub fn rest(value: [f32; N], eps: f32) -> Self {
        Motion {
            seg: Segment {
                start: Duration::ZERO,
                from: value,
                vel: [0.0; N],
                target: value,
                curve: Curve::Instant,
            },
            pending: None,
            eps: if eps.is_finite() && eps > 0.0 {
                eps
            } else {
                1e-3
            },
            last: None,
        }
    }

    /// As if last sampled at `at` (a frame that showed it at rest): a
    /// retarget starts at most one frame before the next sample, as it
    /// would after a sample (see [`START_LEAD`]).
    pub fn sampled_at(mut self, at: Option<Duration>) -> Self {
        self.last = at.or(self.last);
        self
    }

    /// Where it is heading (a pending retarget's target included).
    pub fn target(&self) -> [f32; N] {
        self.pending.map_or(self.seg.target, |p| p.target)
    }

    /// Moves towards `target` along `curve` from wherever it is at the
    /// next sample, keeping its velocity there.
    pub fn retarget(&mut self, target: [f32; N], curve: Curve) {
        let target = target.map(|v| if v.is_finite() { v } else { 0.0 });
        let shift = self.pending.map_or([0.0; N], |p| p.shift);
        self.pending = Some(Pending {
            target,
            curve,
            shift,
        });
    }

    /// Like [`Motion::retarget`], starting at `at` rather than at the next
    /// sample.
    pub fn retarget_at(&mut self, target: [f32; N], curve: Curve, at: Duration) {
        self.retarget(target, curve);
        self.resolve(at, Some(at));
    }

    /// Jumps by `delta` at the next sample (velocity kept), then moves
    /// back to its target along `curve`: what a FLIP glide does.
    pub fn shift(&mut self, delta: [f32; N], curve: Curve) {
        let p = self.pending.get_or_insert(Pending {
            target: self.seg.target,
            curve,
            shift: [0.0; N],
        });
        p.curve = curve;
        for (s, d) in p.shift.iter_mut().zip(delta) {
            if d.is_finite() {
                *s += d;
            }
        }
    }

    /// Snaps to `value`, at rest.
    pub fn snap(&mut self, value: [f32; N]) {
        *self = Motion {
            last: self.last,
            ..Motion::rest(value, self.eps)
        };
    }

    /// True while a retarget waits for its first sample.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn resolve(&mut self, at: Duration, start: Option<Duration>) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let start = start.unwrap_or_else(|| {
            let lead = match self.last {
                Some(last) => at.saturating_sub(last).min(START_LEAD),
                None => Duration::ZERO,
            };
            at.saturating_sub(lead)
        });
        let (mut pos, vel) = self.seg.at(start.max(self.seg.start));
        for (x, s) in pos.iter_mut().zip(p.shift) {
            *x += s;
        }
        self.seg = Segment {
            start,
            from: pos,
            vel,
            target: p.target,
            curve: p.curve,
        };
        if p.curve == Curve::Instant {
            self.seg.from = p.target;
            self.seg.vel = [0.0; N];
        }
    }

    /// The value at `at`, starting a pending retarget there. Once
    /// settled it is exactly its target.
    pub fn sample(&mut self, at: Duration) -> [f32; N] {
        self.resolve(at, None);
        self.last = Some(self.last.map_or(at, |l| l.max(at)));
        if self.seg.settled(at, self.eps) {
            self.seg.from = self.seg.target;
            self.seg.vel = [0.0; N];
            self.seg.curve = Curve::Instant;
            return self.seg.target;
        }
        self.seg.at(at).0
    }

    /// The value at `at` without starting anything: a pending retarget
    /// shows where the value would start (its current one, shifted).
    pub fn peek(&self, at: Duration) -> [f32; N] {
        let at = at.max(self.seg.start);
        let mut pos = if self.seg.settled(at, self.eps) {
            self.seg.target
        } else {
            self.seg.at(at).0
        };
        if let Some(p) = &self.pending {
            for (x, s) in pos.iter_mut().zip(p.shift) {
                *x += s;
            }
        }
        pos
    }

    /// Velocity at `at` (zero once settled), in units per second.
    pub fn velocity(&self, at: Duration) -> [f32; N] {
        if self.seg.settled(at, self.eps) {
            return [0.0; N];
        }
        self.seg.at(at).1
    }

    /// [`Motion::peek`], [`Motion::velocity`] and [`Motion::is_settled`]
    /// at `at` in one go, the curve evaluated once (twice only for a
    /// time before the segment starts): (position, velocity, settled).
    pub fn probe(&self, at: Duration) -> ([f32; N], [f32; N], bool) {
        let state = self.seg.moving(at, self.eps);
        let vel = state.map_or([0.0; N], |(_, v)| v);
        let settled = self.pending.is_none() && state.is_none();
        let pos = if at >= self.seg.start {
            let mut pos = state.map_or(self.seg.target, |(p, _)| p);
            if let Some(p) = &self.pending {
                for (x, s) in pos.iter_mut().zip(p.shift) {
                    *x += s;
                }
            }
            pos
        } else {
            self.peek(at)
        };
        (pos, vel, settled)
    }

    /// At rest at its target, with nothing pending, at `at`.
    pub fn is_settled(&self, at: Duration) -> bool {
        self.pending.is_none() && self.seg.settled(at, self.eps)
    }
}

/// A colour as springs move it: premultiplied OKLab plus alpha, so a fade
/// to transparent keeps its hue (as [`Color::lerp_oklab`]).
pub fn color_channels(c: Color) -> [f32; 4] {
    let o = c.to_oklab();
    let a = o.alpha.clamp(0.0, 1.0);
    [
        (o.l * a) as f32,
        (o.a * a) as f32,
        (o.b * a) as f32,
        a as f32,
    ]
}

/// The colour of [`color_channels`]; alpha is clamped to `0..=1`
/// (a spring overshooting transparent stays transparent).
pub fn channels_color(ch: [f32; 4]) -> Color {
    let alpha = ch[3].clamp(0.0, 1.0) as f64;
    if alpha <= 1e-6 {
        return Color::TRANSPARENT;
    }
    Color::from_oklab(Oklab {
        l: ch[0] as f64 / alpha,
        a: ch[1] as f64 / alpha,
        b: ch[2] as f64 / alpha,
        alpha,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_is_peek_velocity_and_settled_at_once() {
        // Springs (bouncy and critical), a timed curve, a pending
        // retarget with a shift, times before the segment starts and
        // long after it settles: `probe` answers what the three calls do,
        // bit for bit.
        let curves = [
            Curve::Spring(Spring::new(1600.0, 1.0).unwrap()),
            Curve::Spring(Spring::new(380.0, 0.75).unwrap()),
            Curve::Timed {
                duration: ms(200),
                easing: Easing::STANDARD,
            },
            Curve::Instant,
        ];
        for curve in curves {
            let mut m = Motion::rest([0.0, 1.0, -2.0, 0.5], 0.0005).sampled_at(Some(ms(1000)));
            m.retarget([1.0, -1.0, 2.0, 0.5], curve);
            let mut shifted = m.clone();
            shifted.shift([0.25, 0.0, 0.0, 0.0], curve);
            let probes = |m: &Motion<4>| {
                for t in (0..3000).step_by(7) {
                    let at = ms(t);
                    assert_eq!(
                        m.probe(at),
                        (m.peek(at), m.velocity(at), m.is_settled(at)),
                        "{curve:?} at {t} ms"
                    );
                }
            };
            probes(&m);
            probes(&shifted);
            m.sample(ms(1010));
            probes(&m);
            m.retarget([0.0; 4], curve);
            probes(&m);
            m.sample(ms(1100));
            probes(&m);
        }
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn springs_match_numeric_integration() {
        for s in [SPATIAL, EFFECTS, BOUNCY, Spring::new(300.0, 2.5).unwrap()] {
            // Semi-implicit Euler at 10 µs against the closed form.
            let (mut x, mut v) = (1.0f64, -3.0f64);
            let dt = 1e-5;
            let k = s.stiffness as f64;
            let c = 2.0 * s.damping as f64 * k.sqrt();
            for step in 1..=30_000 {
                v += (-k * x - c * v) * dt;
                x += v * dt;
                if step % 5000 == 0 {
                    let (ex, ev) = s.step(1.0, -3.0, step as f64 * dt);
                    assert!((ex - x).abs() < 2e-3, "{s:?} x {ex} vs {x}");
                    assert!((ev - v).abs() < 5e-2, "{s:?} v {ev} vs {v}");
                }
            }
        }
    }

    #[test]
    fn critically_damped_never_overshoots_and_bouncy_does() {
        let mut min = f64::MAX;
        for i in 0..200 {
            min = min.min(EFFECTS.step(1.0, 0.0, i as f64 * 0.005).0);
        }
        assert!(min >= -1e-9);
        let mut min = f64::MAX;
        for i in 0..200 {
            min = min.min(BOUNCY.step(1.0, 0.0, i as f64 * 0.005).0);
        }
        assert!(min < -0.01, "bouncy overshoots: {min}");
    }

    #[test]
    fn motion_is_deterministic_and_settles_exactly() {
        let mut a = Motion::<1>::rest([0.0], 0.01);
        a.retarget([100.0], Curve::Spring(SPATIAL));
        let mut b = a.clone();
        let t0 = Duration::from_secs(5);
        let mut c = a.clone();
        c.sample(t0);
        assert!(!c.is_settled(t0 + ms(100)));
        let xs: Vec<f32> = (0..60).map(|i| a.sample(t0 + ms(16 * i))[0]).collect();
        let ys: Vec<f32> = (0..60).map(|i| b.sample(t0 + ms(16 * i))[0]).collect();
        assert_eq!(xs, ys);
        // No previous sample: the first frame shows the start.
        assert_eq!(xs[0], 0.0);
        assert!(xs[1] > 0.0 && xs[10] > 50.0);
        assert!(a.is_settled(t0 + ms(2000)));
        assert_eq!(a.sample(t0 + ms(2000)), [100.0]);
    }

    #[test]
    fn a_change_after_a_frame_starts_one_frame_early() {
        let mut m = Motion::<1>::rest([0.0], 0.01);
        let t0 = Duration::from_secs(1);
        m.sample(t0);
        m.retarget([10.0], Curve::Spring(EFFECTS));
        // A long idle gap: the lead is one refresh, not the gap.
        let t1 = t0 + Duration::from_secs(3);
        let first = m.sample(t1)[0];
        let mut fresh = Motion::<1>::rest([0.0], 0.01);
        fresh.retarget_at([10.0], Curve::Spring(EFFECTS), t1 - START_LEAD);
        assert_eq!(first, fresh.sample(t1)[0]);
        assert!(first > 0.0);
    }

    #[test]
    fn retargeting_keeps_velocity() {
        let t0 = Duration::from_secs(2);
        let mut m = Motion::<1>::rest([0.0], 0.01);
        m.retarget_at([100.0], Curve::Spring(SPATIAL), t0);
        let mid = t0 + ms(60);
        let v_before = m.velocity(mid)[0];
        let x_before = m.peek(mid)[0];
        assert!(v_before > 100.0);
        // Interrupted mid-flight: same place, same velocity, new target.
        m.retarget_at([-50.0], Curve::Spring(SPATIAL), mid);
        assert_eq!(m.peek(mid)[0], x_before);
        assert!((m.velocity(mid)[0] - v_before).abs() < 1e-3);
        // It keeps going the old way for a moment before turning.
        assert!(m.peek(mid + ms(5))[0] > x_before);
        assert!(m.peek(mid + ms(1500))[0] < -49.0);
    }

    #[test]
    fn a_timed_retarget_keeps_velocity() {
        let t0 = Duration::from_secs(2);
        let mut m = Motion::<1>::rest([0.0], 0.01);
        m.retarget_at([100.0], Curve::Spring(SPATIAL), t0);
        let mid = t0 + ms(60);
        let (x0, v0) = (m.peek(mid)[0], m.velocity(mid)[0]);
        assert!(v0 > 100.0);
        // `~ 200ms` (the standard curve, which starts at rest) back the
        // other way, mid-flight.
        let timed = Curve::of(&Transition::Duration {
            duration: ms(200),
            easing: Easing::STANDARD,
        });
        m.retarget_at([-50.0], timed, mid);
        assert_eq!(m.peek(mid)[0], x0);
        let v = m.velocity(mid)[0];
        assert!((v - v0).abs() < v0 * 0.02, "{v} vs {v0}");
        // Still moving the old way just after, with no step in position
        // or speed between 1 ms samples.
        let xs: Vec<f32> = (0..=200).map(|i| m.peek(mid + ms(i))[0]).collect();
        assert!(xs[5] > x0);
        for w in xs.windows(3) {
            let (d1, d2) = (w[1] - w[0], w[2] - w[1]);
            assert!((d2 - d1).abs() < 0.6, "{w:?}");
        }
        // Ends exactly on target, at rest.
        assert_eq!(m.sample(mid + ms(200)), [-50.0]);
        assert!(m.is_settled(mid + ms(200)));
        // From rest the curve is unchanged.
        let mut r = Motion::<1>::rest([0.0], 0.01);
        r.retarget_at(
            [10.0],
            Curve::Timed {
                duration: ms(200),
                easing: Easing::Linear,
            },
            t0,
        );
        assert!((r.peek(t0 + ms(100))[0] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn shift_jumps_then_returns() {
        let t0 = Duration::from_secs(1);
        let mut m = Motion::<2>::rest([0.0, 0.0], 0.01);
        m.shift([0.0, 40.0], Curve::Spring(SPATIAL));
        m.shift([0.0, 4.0], Curve::Spring(SPATIAL));
        assert_eq!(m.peek(t0), [0.0, 44.0]);
        assert_eq!(m.sample(t0), [0.0, 44.0]);
        assert!(m.sample(t0 + ms(50))[1] < 44.0);
        assert_eq!(m.sample(t0 + ms(3000)), [0.0, 0.0]);
    }

    #[test]
    fn timed_curves_and_instant() {
        let t0 = Duration::from_secs(1);
        let mut m = Motion::<1>::rest([0.0], 0.01);
        let curve = Curve::of(&Transition::Duration {
            duration: ms(200),
            easing: Easing::Linear,
        });
        m.retarget_at([10.0], curve, t0);
        assert!((m.sample(t0 + ms(100))[0] - 5.0).abs() < 1e-4);
        assert_eq!(m.sample(t0 + ms(200)), [10.0]);
        let back = Curve::of(&Transition::Duration {
            duration: ms(300),
            easing: Easing::named("out_back").unwrap(),
        });
        m.retarget_at([20.0], back, t0 + ms(200));
        let peak = (0..30)
            .map(|i| m.peek(t0 + ms(200 + 10 * i))[0])
            .fold(f32::MIN, f32::max);
        assert!(peak > 20.0, "out_back overshoots: {peak}");
        m.retarget([3.0], Curve::Instant);
        assert_eq!(m.sample(t0 + ms(250)), [3.0]);
        assert!(m.is_settled(t0 + ms(250)));
    }

    #[test]
    fn bezier_easing_matches_css_reference_points() {
        // cubic-bezier(0.25, 0.1, 0.25, 1) ("ease") at x = 0.5 is ~0.8024.
        let e = ease(Easing::named("ease").unwrap(), 0.5);
        assert!((e - 0.8024).abs() < 2e-3, "{e}");
        assert_eq!(ease(Easing::Linear, 0.3), 0.3);
        assert_eq!(ease(Easing::STANDARD, 0.0), 0.0);
        assert_eq!(ease(Easing::STANDARD, 1.0), 1.0);
        let mut last = 0.0;
        for i in 0..=100 {
            let v = ease(Easing::STANDARD, i as f32 / 100.0);
            assert!(v >= last - 1e-6);
            last = v;
        }
    }

    #[test]
    fn elastic_and_bounce_are_not_beziers() {
        let bounce = Easing::named("out_bounce").unwrap();
        let elastic = Easing::named("out_elastic").unwrap();
        let samples = |e: Easing| {
            (0..=1000)
                .map(|i| ease(e, i as f32 / 1000.0))
                .collect::<Vec<_>>()
        };
        // Bounce: never past 1, touches it at each of three bounces and
        // falls back in between.
        let b = samples(bounce);
        assert!(b.iter().all(|v| *v <= 1.0 + 1e-6 && *v >= 0.0));
        let touches = b
            .windows(3)
            .filter(|w| w[1] > 0.97 && w[1] >= w[0] && w[1] >= w[2] && (w[0] < w[1] || w[2] < w[1]))
            .count();
        assert!(touches >= 3, "{touches} touches");
        assert!(b[500] < 0.8 && b[364] > 0.99, "{} {}", b[500], b[364]);
        // Elastic: overshoots 1, then undershoots, then settles there.
        let e = samples(elastic);
        let peak = e.iter().copied().fold(f32::MIN, f32::max);
        assert!(peak > 1.3, "{peak}");
        let crossings = (1..e.len())
            .filter(|&i| (e[i - 1] < 1.0) != (e[i] < 1.0))
            .count();
        assert!(crossings >= 4, "{crossings} crossings");
        assert_eq!((e[0], e[1000]), (0.0, 1.0));
        assert_eq!((b[0], b[1000]), (0.0, 1.0));
        // A timed motion along them ends exactly on its target.
        let t0 = Duration::from_secs(1);
        let mut m = Motion::<1>::rest([0.0], 0.01);
        m.retarget_at(
            [10.0],
            Curve::Timed {
                duration: ms(300),
                easing: bounce,
            },
            t0,
        );
        assert_eq!(m.sample(t0 + ms(300)), [10.0]);
    }

    #[test]
    fn malformed_transitions_snap_or_end() {
        assert_eq!(
            Curve::of(&Transition::Spring {
                stiffness: f32::NAN,
                damping: 1.0
            }),
            Curve::Instant
        );
        assert_eq!(Curve::of(&Transition::Default), Curve::Instant);
        let t0 = Duration::from_secs(1);
        let mut m = Motion::<1>::rest([0.0], 0.01);
        m.retarget_at([1.0], Curve::Spring(Spring::new(1e-3, 0.0).unwrap()), t0);
        assert!(m.is_settled(t0 + MAX_MOTION));
    }

    #[test]
    fn colours_spring_in_premultiplied_oklab() {
        let red = Color::from_hex("#ff0000").unwrap();
        let back = channels_color(color_channels(red));
        assert!((back.r - 1.0).abs() < 1e-4 && back.g.abs() < 1e-4);
        // Fading to transparent keeps the hue.
        let half = color_channels(red)
            .iter()
            .zip(color_channels(Color::TRANSPARENT))
            .map(|(a, b)| (a + b) / 2.0)
            .collect::<Vec<_>>();
        let c = channels_color([half[0], half[1], half[2], half[3]]);
        assert!((c.a - 0.5).abs() < 1e-4);
        assert!((c.r - 1.0).abs() < 1e-3 && c.g.abs() < 1e-3, "{c:?}");
        assert_eq!(channels_color([0.5, 0.0, 0.0, -0.2]), Color::TRANSPARENT);
    }
}

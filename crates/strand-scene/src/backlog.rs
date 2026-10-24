//! (history) A `graph`'s backlog: what logic kept of the graph's value
//! before the graph showed it (design.md, "Generative, data-driven and
//! media": `graph X { history: D }` shows the last D of X as soon as it
//! mounts), sent as [`crate::Prop::Backlog`] when the graph mounts and
//! again as the kept history grows.
//!
//! Each point is a value with the time it was taken on the boot clock
//! ([`boot_now`]: `CLOCK_BOOTTIME`, which logic and render both read, and
//! which counts time suspended), so render works out each point's age
//! when it draws, and how it got there: a point whose time is exact (a
//! shown sample, a value its service reports when it changes) steps to
//! it (`ramp` 0: the value held until then); a sample a hidden service
//! polled ramps linearly from the point before over the interval it
//! could have changed in (its polling period), so a hidden span reads as
//! a line between its samples. A point whose value is not finite starts
//! a gap: nothing is known from then until the next point (the machine
//! slept).

use std::time::Duration;

/// A `graph`'s `history` when none is set: what logic keeps of its value
/// and what render spans its box with.
pub const DEFAULT_HISTORY: Duration = Duration::from_secs(60);

/// See the module docs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Backlog {
    /// Oldest first.
    pub points: Vec<BacklogPoint>,
}

/// One kept value: 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BacklogPoint {
    /// When it was taken: seconds on the boot clock ([`boot_now`]).
    pub at: f64,
    /// The value; not finite: a gap starts here.
    pub value: f32,
    /// Seconds, ending at this point, over which the value moved here
    /// from the point before: 0 steps to it.
    pub ramp: f32,
}

/// The two clocks history reads: the boot clock, which counts time
/// suspended, and the awake clock, which does not. Between two readings,
/// what the boot clock ran ahead of the awake one is time the machine
/// slept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clocks {
    /// `CLOCK_BOOTTIME`.
    pub boot: Duration,
    /// `CLOCK_MONOTONIC`.
    pub awake: Duration,
}

/// Reads [`Clocks`] (a test passes its own).
pub type ClockFn = fn() -> Clocks;

/// The clocks now.
pub fn clocks() -> Clocks {
    Clocks {
        boot: read(rustix::time::ClockId::Boottime),
        awake: read(rustix::time::ClockId::Monotonic),
    }
}

/// The boot clock now (`CLOCK_BOOTTIME`): what backlog times are on.
pub fn boot_now() -> Duration {
    read(rustix::time::ClockId::Boottime)
}

fn read(id: rustix::time::ClockId) -> Duration {
    let t = rustix::time::clock_gettime(id);
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}

impl Backlog {
    /// The value at `at` (boot clock seconds): the newest point at or
    /// before then, or on its way to the next one while that one ramps.
    /// `None` before the oldest point and inside a gap.
    pub fn value_at(&self, at: f64) -> Option<f32> {
        if !at.is_finite() {
            return None;
        }
        // Points at or before that moment.
        let k = self.points.partition_point(|p| p.at <= at);
        let prev = self.points.get(k.checked_sub(1)?)?;
        if !prev.value.is_finite() {
            return None;
        }
        let Some(next) = self.points.get(k) else {
            return Some(prev.value);
        };
        let ramp = f64::from(next.ramp).min(next.at - prev.at);
        let start = next.at - ramp;
        if ramp > 0.0 && at > start && next.value.is_finite() {
            let f = ((at - start) / ramp).clamp(0.0, 1.0) as f32;
            Some(prev.value + (next.value - prev.value) * f)
        } else {
            Some(prev.value)
        }
    }

    /// When the oldest point was taken.
    pub fn oldest(&self) -> Option<f64> {
        self.points.first().map(|p| p.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(at: f64, value: f32, ramp: f32) -> BacklogPoint {
        BacklogPoint { at, value, ramp }
    }

    #[test]
    fn exact_points_step_and_polled_ones_ramp() {
        let b = Backlog {
            points: vec![
                p(100.0, 0.2, 0.0),
                p(101.0, 0.4, 0.0),
                // Polled hidden every 3 s: each a ramp.
                p(104.0, 0.7, 3.0),
                p(107.0, 0.1, 3.0),
                // Shown again: a step.
                p(109.0, 0.5, 0.0),
            ],
        };
        assert_eq!(b.value_at(99.0), None, "before the oldest point");
        assert_eq!(b.value_at(100.0), Some(0.2));
        assert_eq!(b.value_at(100.5), Some(0.2), "held until the next step");
        assert_eq!(b.value_at(101.0), Some(0.4));
        assert!(
            (b.value_at(102.5).unwrap() - 0.55).abs() < 1e-6,
            "halfway up"
        );
        assert!(
            (b.value_at(105.5).unwrap() - 0.4).abs() < 1e-6,
            "halfway down"
        );
        assert_eq!(b.value_at(108.0), Some(0.1), "held before a step");
        assert_eq!(b.value_at(200.0), Some(0.5), "the newest holds on");
        assert_eq!(b.oldest(), Some(100.0));
    }

    #[test]
    fn a_ramp_never_reaches_past_the_point_before() {
        let b = Backlog {
            points: vec![p(1.0, 0.0, 0.0), p(2.0, 1.0, 5.0)],
        };
        assert_eq!(b.value_at(1.0), Some(0.0));
        assert!((b.value_at(1.5).unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(Backlog::default().value_at(0.0), None);
    }

    /// A gap (the machine slept) knows nothing until the next point, which
    /// steps.
    #[test]
    fn a_gap_holds_nothing_until_the_next_point() {
        let b = Backlog {
            points: vec![p(10.0, 0.3, 0.0), p(12.0, f32::NAN, 0.0), p(70.0, 0.6, 3.0)],
        };
        assert_eq!(b.value_at(11.0), Some(0.3));
        assert_eq!(b.value_at(12.0), None);
        assert_eq!(b.value_at(69.0), None, "no ramp out of a gap");
        assert_eq!(b.value_at(70.0), Some(0.6));
    }

    #[test]
    fn a_point_is_16_bytes_and_the_clocks_run() {
        assert_eq!(std::mem::size_of::<BacklogPoint>(), 16);
        let c = clocks();
        assert!(!c.boot.is_zero() && !c.awake.is_zero());
        assert!(boot_now() >= c.boot, "it runs forward");
    }
}

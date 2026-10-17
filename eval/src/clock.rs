//! The virtual clock (docs/eval.md, "Clock"): tick `k` is presented at
//! `t(k) = T0 + Σ period · jitter`, in whole nanoseconds, so 144 Hz does
//! not drift. `T0` is one second: at time zero every motion would snap.

use std::time::Duration;

/// The first tick's presentation time.
pub const T0: Duration = Duration::from_secs(1);

/// The scenario's clock.
#[derive(Clone, Debug)]
pub struct Clock {
    hz: u32,
    /// Per-tick period factors, cycled (`jitter 1,1,2,1`); empty: none.
    jitter: Vec<f64>,
    /// The tick about to run and its time.
    k: u32,
    t: Duration,
}

impl Clock {
    pub fn new(hz: u32) -> Clock {
        Clock {
            hz: hz.max(1),
            jitter: Vec::new(),
            k: 0,
            t: T0,
        }
    }

    /// The frame period at the current rate.
    pub fn period(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.hz))
    }

    pub fn hz(&self) -> u32 {
        self.hz
    }

    pub fn set_hz(&mut self, hz: u32) {
        self.hz = hz.max(1);
    }

    pub fn set_jitter(&mut self, factors: Vec<f64>) {
        self.jitter = factors;
    }

    /// The tick about to run.
    pub fn k(&self) -> u32 {
        self.k
    }

    /// Its presentation time.
    pub fn now(&self) -> Duration {
        self.t
    }

    /// Ends the current tick: the next one is a period later (times its
    /// jitter factor).
    pub fn advance(&mut self) {
        let f = match self.jitter.len() {
            0 => 1.0,
            n => self.jitter[self.k as usize % n],
        };
        let ns = (self.period().as_nanos() as f64 * f).round() as u64;
        self.t += Duration::from_nanos(ns.max(1));
        self.k += 1;
    }

    /// Ticks `ms` milliseconds take at the current rate (at least one).
    pub fn ticks_for(&self, ms: u32) -> u32 {
        let p = self.period().as_nanos();
        (u128::from(ms) * 1_000_000).div_ceil(p).max(1) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_nanoseconds_do_not_drift() {
        let mut c = Clock::new(144);
        for _ in 0..144 {
            c.advance();
        }
        assert_eq!(c.now() - T0, Duration::from_nanos(6_944_444 * 144));
        assert_eq!(c.k(), 144);
    }

    #[test]
    fn jitter_stretches_periods_in_turn() {
        let mut c = Clock::new(60);
        c.set_jitter(vec![1.0, 2.0]);
        c.advance();
        c.advance();
        assert_eq!(c.now() - T0, Duration::from_nanos(16_666_666 * 3));
    }

    #[test]
    fn waits_round_up_to_ticks() {
        let c = Clock::new(60);
        assert_eq!(c.ticks_for(100), 7);
        assert_eq!(c.ticks_for(0), 1);
        assert_eq!(c.ticks_for(16), 1);
        assert_eq!(c.ticks_for(17), 2);
    }
}

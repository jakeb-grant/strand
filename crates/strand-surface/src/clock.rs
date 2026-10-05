//! Frame timing: `wp_presentation` feedback feeds a [`FrameClock`], which
//! predicts when the next frame of a surface reaches the screen. The
//! prediction is what [`strand_scene::PaintTarget::time`] carries, so springs
//! sample the moment the frame is seen, locked to the real refresh rate.
//!
//! Tests inject a [`FakeClock`] whose time and presentation timestamps they
//! script (60 Hz, 144 Hz, jittery), standing in for monitors nobody owns.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use strand_scene::SurfaceId;

/// One `wp_presentation_feedback.presented` event, on the presentation
/// clock.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Presentation {
    /// When the frame turned into light.
    pub time: Duration,
    /// The output's refresh period; `None` when the compositor reports 0
    /// (unknown, or variable refresh).
    pub refresh: Option<Duration>,
    /// The output's vertical retrace counter, when the compositor has one.
    pub seq: u64,
}

/// Source of frame timing for the surface manager.
///
/// All times are on the `wp_presentation` clock (`CLOCK_MONOTONIC` unless
/// the compositor announces another one), as durations since its epoch.
pub trait FrameClock {
    /// The current time on the presentation clock.
    fn now(&self) -> Duration;
    /// The compositor announced its presentation clock (`clk_id`, a POSIX
    /// clock id). Clocks that read the system time switch to it.
    fn set_clock_id(&mut self, clk_id: u32) {
        let _ = clk_id;
    }
    /// A committed frame of `surface` was presented.
    fn presented(&mut self, surface: SurfaceId, presentation: Presentation);
    /// A committed frame of `surface` was never shown.
    fn discarded(&mut self, surface: SurfaceId) {
        let _ = surface;
    }
    /// When a frame of `surface` painted now is expected on screen: the
    /// first refresh boundary after [`FrameClock::now`] on the surface's
    /// output, or `now` when the refresh rate is not known yet.
    fn predict(&self, surface: SurfaceId) -> Duration;
    /// `surface` is gone; drop what is kept for it.
    fn forget(&mut self, surface: SurfaceId) {
        let _ = surface;
    }
}

/// The prediction shared by the real and the fake clock: the last
/// presentation of each surface plus its refresh period.
#[derive(Clone, Debug, Default)]
struct Predictor {
    last: HashMap<SurfaceId, Presentation>,
}

impl Predictor {
    fn presented(&mut self, surface: SurfaceId, p: Presentation) {
        // Feedback for an older frame can arrive after a newer one when
        // frames were queued; keep the latest.
        let keep = self.last.get(&surface).is_none_or(|old| old.time <= p.time);
        if keep {
            self.last.insert(surface, p);
        }
    }

    fn predict(&self, surface: SurfaceId, now: Duration) -> Duration {
        let Some(last) = self.last.get(&surface) else {
            return now;
        };
        let Some(refresh) = last.refresh.filter(|r| !r.is_zero()) else {
            return now;
        };
        if now < last.time {
            // Clock skew between feedback and our reading: the next vblank
            // after the last presentation is the best guess.
            return last.time.checked_add(refresh).unwrap_or(now);
        }
        let elapsed = (now - last.time).as_nanos();
        let period = refresh.as_nanos();
        let periods = elapsed / period + 1;
        let offset = periods.saturating_mul(period);
        // Timestamps come from the compositor: a bogus one must not panic.
        u64::try_from(offset)
            .ok()
            .and_then(|o| last.time.checked_add(Duration::from_nanos(o)))
            .unwrap_or(now)
    }
}

/// The real clock: reads the clock the compositor named in
/// `wp_presentation.clock_id` and predicts from presentation feedback.
#[derive(Clone, Debug)]
pub struct PresentationClock {
    clock: rustix::time::ClockId,
    predictor: Predictor,
}

impl Default for PresentationClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationClock {
    /// Starts on `CLOCK_MONOTONIC`, which is what compositors announce.
    pub fn new() -> Self {
        Self {
            clock: rustix::time::ClockId::Monotonic,
            predictor: Predictor::default(),
        }
    }
}

impl FrameClock for PresentationClock {
    fn now(&self) -> Duration {
        let t = rustix::time::clock_gettime(self.clock);
        Duration::new(
            u64::try_from(t.tv_sec).unwrap_or(0),
            u32::try_from(t.tv_nsec).unwrap_or(0),
        )
    }

    fn set_clock_id(&mut self, clk_id: u32) {
        use rustix::time::ClockId;
        self.clock = match clk_id {
            1 => ClockId::Monotonic,
            4 => ClockId::MonotonicRaw,
            7 => ClockId::Boottime,
            other => {
                log::warn!("unknown presentation clock {other}; using CLOCK_MONOTONIC");
                ClockId::Monotonic
            }
        };
    }

    fn presented(&mut self, surface: SurfaceId, presentation: Presentation) {
        self.predictor.presented(surface, presentation);
    }

    fn predict(&self, surface: SurfaceId) -> Duration {
        self.predictor.predict(surface, self.now())
    }

    fn forget(&mut self, surface: SurfaceId) {
        self.predictor.last.remove(&surface);
    }
}

/// A clock whose time tests set. Clones share the same time, so a test
/// keeps one handle and gives the other to the surface manager.
///
/// Presentation feedback from a real compositor still flows in (and is
/// recorded in [`FakeClock::presentations`]); tests can also feed scripted
/// timestamps with [`FrameClock::presented`].
#[derive(Clone, Debug, Default)]
pub struct FakeClock {
    now: Arc<AtomicU64>,
    shared: Arc<std::sync::Mutex<FakeShared>>,
}

#[derive(Debug, Default)]
struct FakeShared {
    predictor: Predictor,
    log: Vec<(SurfaceId, Presentation)>,
    discarded: u64,
}

impl FakeClock {
    /// A clock reading `now`.
    pub fn new(now: Duration) -> Self {
        let clock = Self::default();
        clock.set(now);
        clock
    }

    /// Sets the time every clone reads.
    pub fn set(&self, now: Duration) {
        let nanos = u64::try_from(now.as_nanos()).unwrap_or(u64::MAX);
        self.now.store(nanos, Ordering::SeqCst);
    }

    /// Moves the time forward.
    pub fn advance(&self, by: Duration) {
        let by = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let _ = self
            .now
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |t| {
                Some(t.saturating_add(by))
            });
    }

    /// Every presentation recorded so far, oldest first.
    pub fn presentations(&self) -> Vec<(SurfaceId, Presentation)> {
        self.lock().log.clone()
    }

    /// How many frames were reported discarded.
    pub fn discarded_count(&self) -> u64 {
        self.lock().discarded
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeShared> {
        // A test that panicked while holding the lock leaves usable data.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl FrameClock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.now.load(Ordering::SeqCst))
    }

    fn presented(&mut self, surface: SurfaceId, presentation: Presentation) {
        let mut shared = self.lock();
        shared.predictor.presented(surface, presentation);
        shared.log.push((surface, presentation));
    }

    fn discarded(&mut self, _surface: SurfaceId) {
        self.lock().discarded += 1;
    }

    fn predict(&self, surface: SurfaceId) -> Duration {
        let now = self.now();
        self.lock().predictor.predict(surface, now)
    }

    fn forget(&mut self, surface: SurfaceId) {
        self.lock().predictor.last.remove(&surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: SurfaceId = SurfaceId(1);

    fn ms(v: f64) -> Duration {
        Duration::from_secs_f64(v / 1000.0)
    }

    fn present(clock: &mut impl FrameClock, at: Duration, refresh: Duration) {
        clock.presented(
            S,
            Presentation {
                time: at,
                refresh: Some(refresh),
                seq: 0,
            },
        );
    }

    #[test]
    fn predicts_now_without_feedback() {
        let clock = FakeClock::new(Duration::from_secs(5));
        assert_eq!(clock.predict(S), Duration::from_secs(5));
    }

    #[test]
    fn locks_to_60hz_refresh() {
        let refresh = Duration::from_nanos(16_666_667);
        let mut clock = FakeClock::new(Duration::from_secs(10));
        present(&mut clock, Duration::from_secs(10), refresh);
        // Painting right after a vblank targets the next one.
        clock.advance(ms(1.0));
        assert_eq!(clock.predict(S), Duration::from_secs(10) + refresh);
        // Three and a half periods later: the fourth boundary.
        clock.set(Duration::from_secs(10) + refresh * 3 + refresh / 2);
        assert_eq!(clock.predict(S), Duration::from_secs(10) + refresh * 4);
        // Exactly on a boundary: the following one (that one is gone).
        clock.set(Duration::from_secs(10) + refresh * 2);
        assert_eq!(clock.predict(S), Duration::from_secs(10) + refresh * 3);
    }

    #[test]
    fn locks_to_144hz_refresh() {
        let refresh = Duration::from_nanos(6_944_444);
        let mut clock = FakeClock::new(Duration::ZERO);
        present(&mut clock, ms(100.0), refresh);
        clock.set(ms(110.0));
        let p = clock.predict(S);
        assert_eq!(p, ms(100.0) + refresh * 2);
        assert!(p > clock.now() && p - clock.now() <= refresh);
    }

    #[test]
    fn jittery_feedback_still_predicts_the_future() {
        // Presentations jitter ±0.5 ms around 60 Hz; every prediction must
        // land after `now` and within one period of it.
        let refresh = Duration::from_nanos(16_666_667);
        let mut clock = FakeClock::new(Duration::ZERO);
        let jitter = [0.0, 0.4, -0.3, 0.5, -0.5, 0.1, 0.2, -0.4];
        for (i, j) in jitter.iter().enumerate() {
            let t = ms(1000.0 + i as f64 * 16.666_667 + j);
            present(&mut clock, t, refresh);
            for paint_at in [0.5, 8.0, 16.0, 30.0] {
                clock.set(t + ms(paint_at));
                let p = clock.predict(S);
                assert!(p > clock.now(), "frame {i} at +{paint_at} ms");
                assert!(p - clock.now() <= refresh, "frame {i} at +{paint_at} ms");
            }
        }
        assert_eq!(clock.presentations().len(), jitter.len());
    }

    #[test]
    fn stale_feedback_does_not_move_backwards() {
        let refresh = Duration::from_nanos(16_666_667);
        let mut clock = FakeClock::new(ms(50.0));
        present(&mut clock, ms(40.0), refresh);
        present(&mut clock, ms(10.0), refresh);
        assert_eq!(clock.predict(S), ms(40.0) + refresh);
    }

    #[test]
    fn bogus_timestamps_do_not_panic() {
        // A presentation time near the end of the clock (a compositor bug)
        // cannot be extended by a period: predict "now".
        let refresh = Duration::from_nanos(16_666_667);
        let mut clock = FakeClock::new(ms(5.0));
        present(&mut clock, Duration::new(u64::MAX, 999_999_990), refresh);
        assert_eq!(clock.predict(S), ms(5.0));
        // A next boundary past the end of `Duration` (beyond what the
        // fake clock can hold, so on the predictor itself).
        let mut p = Predictor::default();
        p.presented(
            S,
            Presentation {
                time: Duration::new(u64::MAX, 990_000_000),
                refresh: Some(refresh),
                seq: 0,
            },
        );
        let now = Duration::MAX;
        assert_eq!(p.predict(S, now), now);
    }

    #[test]
    fn unknown_refresh_predicts_now() {
        let mut clock = FakeClock::new(ms(50.0));
        clock.presented(
            S,
            Presentation {
                time: ms(45.0),
                refresh: None,
                seq: 0,
            },
        );
        assert_eq!(clock.predict(S), ms(50.0));
        clock.forget(S);
        assert_eq!(clock.predict(S), ms(50.0));
    }

    #[test]
    fn real_clock_is_monotonic() {
        let mut clock = PresentationClock::new();
        clock.set_clock_id(1);
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a && a > Duration::ZERO);
        // No feedback yet: the prediction is the time of the call.
        let p = clock.predict(S);
        assert!(p >= b && p <= clock.now());
    }
}

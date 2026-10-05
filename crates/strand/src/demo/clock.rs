//! Wall-clock minute ticks from a `CLOCK_REALTIME` timerfd.
//!
//! The timer is armed at the absolute time of the next minute boundary
//! (`TFD_TIMER_ABSTIME`), so the process sleeps until exactly then: no
//! polling and no drift from a monotonic countdown. `TFD_TIMER_CANCEL_ON_SET`
//! makes a clock change (NTP step, manual `date -s`, resume with a new
//! time) wake the timer early with `ECANCELED`, after which the caller
//! re-reads the time and re-arms.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::time::{
    ClockId, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, clock_gettime,
    timerfd_create, timerfd_settime,
};

/// One minute, the clock's tick period.
pub const MINUTE: Duration = Duration::from_secs(60);

fn nanos(t: Timespec) -> i128 {
    t.tv_sec as i128 * 1_000_000_000 + t.tv_nsec as i128
}

fn timespec(ns: i128) -> Timespec {
    Timespec {
        tv_sec: ns.div_euclid(1_000_000_000) as i64,
        tv_nsec: ns.rem_euclid(1_000_000_000) as _,
    }
}

/// Whole periods since the Unix epoch of a wall-clock time (floored, so
/// times before 1970 still land in the right period).
pub fn period_of(now: Timespec, period: Duration) -> i64 {
    nanos(now).div_euclid(period.as_nanos().max(1) as i128) as i64
}

/// The first period boundary strictly after `now`.
pub fn next_boundary(now: Timespec, period: Duration) -> Timespec {
    let p = period.as_nanos().max(1) as i128;
    timespec((period_of(now, period) as i128 + 1) * p)
}

/// The current wall-clock time.
pub fn realtime_now() -> Timespec {
    clock_gettime(ClockId::Realtime)
}

/// What a readable timer reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fired {
    /// The armed time passed.
    Expired,
    /// The wall clock was set; the armed time may now be wrong.
    ClockChanged,
    /// Spurious readiness: nothing to read (the fd is non-blocking).
    Nothing,
}

/// A `CLOCK_REALTIME` timerfd armed at absolute wall-clock times.
#[derive(Debug)]
pub struct WallTimer {
    fd: OwnedFd,
}

impl WallTimer {
    pub fn new() -> io::Result<Self> {
        let fd = timerfd_create(
            TimerfdClockId::Realtime,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )?;
        Ok(Self { fd })
    }

    /// Fire once at the absolute wall-clock time `at`.
    pub fn arm_at(&self, at: Timespec) -> io::Result<()> {
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: at,
        };
        timerfd_settime(
            &self.fd,
            TimerfdTimerFlags::ABSTIME | TimerfdTimerFlags::CANCEL_ON_SET,
            &spec,
        )?;
        Ok(())
    }

    /// Arm for the next `period` boundary after the current time and
    /// return the current period ([`period_of`]).
    ///
    /// `CANCEL_ON_SET` only reports clock changes made after the
    /// `timerfd_settime`, so a step between reading the time and arming
    /// would go unseen: the time is read again after arming, and if it
    /// moved backwards or into another period, the timer is re-armed.
    pub fn arm_next(&self, period: Duration) -> io::Result<i64> {
        let mut now = realtime_now();
        for _ in 0..4 {
            self.arm_at(next_boundary(now, period))?;
            let after = realtime_now();
            if nanos(after) >= nanos(now) && period_of(after, period) == period_of(now, period) {
                break;
            }
            now = after;
        }
        Ok(period_of(now, period))
    }

    /// Consume the readiness of the fd.
    pub fn read(&self) -> io::Result<Fired> {
        let mut buf = [0u8; 8];
        match rustix::io::read(&self.fd, &mut buf) {
            Ok(_) => Ok(Fired::Expired),
            Err(rustix::io::Errno::CANCELED) => Ok(Fired::ClockChanged),
            Err(rustix::io::Errno::AGAIN) => Ok(Fired::Nothing),
            Err(e) => Err(e.into()),
        }
    }
}

impl AsFd for WallTimer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn ts(sec: i64, nsec: i64) -> Timespec {
        Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    fn minute_of(t: Timespec) -> i64 {
        period_of(t, MINUTE)
    }

    fn next_minute_boundary(t: Timespec) -> Timespec {
        next_boundary(t, MINUTE)
    }

    #[test]
    fn boundary_is_the_next_whole_minute() {
        assert_eq!(next_minute_boundary(ts(120, 0)), ts(180, 0));
        assert_eq!(next_minute_boundary(ts(179, 999_999_999)), ts(180, 0));
        assert_eq!(next_minute_boundary(ts(121, 5)), ts(180, 0));
        assert_eq!(minute_of(ts(179, 999_999_999)), 2);
        // Before the epoch, floored.
        assert_eq!(minute_of(ts(-1, 0)), -1);
        assert_eq!(next_minute_boundary(ts(-1, 0)), ts(0, 0));
        // Sub-second periods (tests run the logic thread on them).
        let p = Duration::from_millis(250);
        assert_eq!(period_of(ts(1, 300_000_000), p), 5);
        assert_eq!(next_boundary(ts(1, 300_000_000), p), ts(1, 500_000_000));
        assert_eq!(next_boundary(ts(1, 750_000_000), p), ts(2, 0));
    }

    #[test]
    fn timer_sleeps_until_its_absolute_time() {
        let timer = WallTimer::new().unwrap();
        assert_eq!(timer.read().unwrap(), Fired::Nothing);
        // Monotonic start first: a preemption before reading the wall
        // clock can only lengthen the measured wait.
        let start = Instant::now();
        let now = realtime_now();
        let mut at = ts(now.tv_sec, now.tv_nsec + 80_000_000);
        if at.tv_nsec >= 1_000_000_000 {
            at.tv_sec += 1;
            at.tv_nsec -= 1_000_000_000;
        }
        timer.arm_at(at).unwrap();
        let mut fds = [rustix::event::PollFd::new(
            &timer,
            rustix::event::PollFlags::IN,
        )];
        let n = rustix::event::poll(&mut fds, None).unwrap();
        assert_eq!(n, 1);
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(70), "{waited:?}");
        assert!(nanos(realtime_now()) >= nanos(at));
        assert_eq!(timer.read().unwrap(), Fired::Expired);
        assert_eq!(timer.read().unwrap(), Fired::Nothing);
    }
}

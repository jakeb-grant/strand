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

use rustix::time::{
    ClockId, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, clock_gettime,
    timerfd_create, timerfd_settime,
};

/// Whole minutes since the Unix epoch of a wall-clock time (floored, so
/// times before 1970 still land in the right minute).
pub fn minute_of(now: Timespec) -> i64 {
    now.tv_sec.div_euclid(60)
}

/// The first minute boundary strictly after `now`.
pub fn next_minute_boundary(now: Timespec) -> Timespec {
    Timespec {
        tv_sec: (minute_of(now) + 1) * 60,
        tv_nsec: 0,
    }
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

    /// Arm for the next minute boundary after the current time and return
    /// the current minute ([`minute_of`]).
    pub fn arm_next_minute(&self) -> io::Result<i64> {
        let now = realtime_now();
        self.arm_at(next_minute_boundary(now))?;
        Ok(minute_of(now))
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
    use std::time::{Duration, Instant};

    fn ts(sec: i64, nsec: i64) -> Timespec {
        Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
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
    }

    #[test]
    fn timer_sleeps_until_its_absolute_time() {
        let timer = WallTimer::new().unwrap();
        assert_eq!(timer.read().unwrap(), Fired::Nothing);
        let now = realtime_now();
        let mut at = ts(now.tv_sec, now.tv_nsec + 80_000_000);
        if at.tv_nsec >= 1_000_000_000 {
            at.tv_sec += 1;
            at.tv_nsec -= 1_000_000_000;
        }
        let start = Instant::now();
        timer.arm_at(at).unwrap();
        let mut fds = [rustix::event::PollFd::new(
            &timer,
            rustix::event::PollFlags::IN,
        )];
        let n = rustix::event::poll(&mut fds, None).unwrap();
        assert_eq!(n, 1);
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(70), "{waited:?}");
        assert_eq!(timer.read().unwrap(), Fired::Expired);
        assert_eq!(timer.read().unwrap(), Fired::Nothing);
    }
}

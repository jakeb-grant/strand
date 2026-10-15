//! `system`: the desktop's appearance settings from the XDG portal
//! (`org.freedesktop.portal.Settings`: `color-scheme`, `accent-color`,
//! `contrast`, `reduced-motion`), followed live, and the host name.
//!
//! The portal client is `strand_watch::follow`, run on the shared
//! services runtime with the runtime's session connection (it used to
//! run on a thread of its own). Its boot read is the service's first
//! report: boot values, so `on change system.dark` does not fire at
//! boot; then [`Cx::ready`]. With no portal, the values the service
//! started from are kept (a host seeds them with the last values it saw:
//! `Client::seed`) and the portal is followed when it appears. When the
//! session bus cannot be reached, or the connection dies under the
//! follow (a session-bus restart), the body fails after `ready`: the
//! client starts it again with its backoff, and the bus cache connects
//! afresh. Only a bus disabled outright (`Bus::Disabled`) is not retried.
//!
//! The machine's facts sit beside the host name (gap 34, decisions.md
//! m6-service-gaps): `kernel`, read once at start from
//! `/proc/sys/kernel/osrelease`, and `uptime`, a `#[store(stream)]` field
//! from `/proc/uptime` (CLOCK_BOOTTIME, so suspend counts) floored to
//! whole minutes. It is read only while a visible reader shows it: at
//! once when one starts to, then by a timer set to the next whole minute
//! of uptime (a little after, so it reads the new minute), so the logic
//! thread is woken at most once a minute and never while nobody shows
//! it. The timer runs on the monotonic clock, which stops in suspend:
//! after a resume the value may lag by up to a minute.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use strand_watch::{ChangeEvent, Contrast, SystemSetting};

use crate::{Cx, Msg, Rgba, ServiceError, Store, procfs, service};

/// The schema the `system` service serves.
pub const SCHEMA: &str = strand_services_schema::SYSTEM;

/// See the module docs.
#[service(name = "system")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct System {
    /// The desktop prefers a dark style.
    pub dark: bool,
    /// The desktop's accent colour, if it sets one.
    pub accent: Option<Rgba>,
    /// The contrast preference, as `material(contrast:)` takes it: 0 for
    /// normal, 1 for high.
    pub contrast: f64,
    /// Motion should be reduced: springs snap, and loops, time signals and
    /// effects stop.
    pub reduced_motion: bool,
    /// The machine's host name.
    pub hostname: String,
    /// The running kernel's release, as `uname -r` gives it:
    /// `6.11.2-arch1-1`.
    pub kernel: String,
    /// Time since boot (suspend included), to the minute:
    /// `dur(system.uptime)` reads `3 h 12 min`. It changes once a minute,
    /// and only while a visible reader shows it.
    #[store(stream)]
    pub uptime: Duration,
}

impl System {
    /// Take in one portal setting.
    pub fn apply_setting(&mut self, s: &SystemSetting) {
        match s {
            SystemSetting::Dark { dark, .. } => self.dark = *dark,
            SystemSetting::Accent(c) => {
                self.accent = c.map(|[r, g, b]| Rgba::rgb(r as f32, g as f32, b as f32));
            }
            SystemSetting::Contrast(Contrast::High) => self.contrast = 1.0,
            SystemSetting::Contrast(Contrast::Normal) => self.contrast = 0.0,
            SystemSetting::ReducedMotion(on) => self.reduced_motion = *on,
        }
    }

    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let (host, kernel) = (hostname(), kernel());
        cx.update(|s| {
            if let Some(h) = host {
                s.hostname = h;
            }
            if let Some(k) = kernel {
                s.kernel = k;
            }
        });
        let conn = match cx.session().await {
            Ok(c) => Some(c),
            Err(e) if cx.buses().session == crate::Bus::Disabled => {
                log::debug!("not following the portal's appearance settings: {e}");
                // Keep serving the values it has until stopped.
                cx.ready();
                None
            }
            Err(e) => {
                cx.ready();
                return Err(ServiceError(format!("no session bus: {e}")));
            }
        };
        let (sink, rx) = strand_watch::channel();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let notify = arrived.clone();
        let sink = sink.with_waker(move || notify.notify_one());
        let follow = async {
            match &conn {
                Some(conn) => strand_watch::follow(conn, sink).await,
                // No bus: nothing to follow, the rest still runs.
                None => std::future::pending().await,
            }
        };
        tokio::pin!(follow);
        let mut uptime = Uptime::new();
        if !uptime.arm(&mut cx) {
            return Ok(());
        }
        loop {
            tokio::select! {
                r = &mut follow => {
                    // Batches it sent before ending.
                    if !drain(&mut cx, &rx) {
                        return Ok(());
                    }
                    cx.ready();
                    // It follows until the connection goes: the bus died
                    // or restarted. Fail, so the client starts it again
                    // (backing off) on a fresh connection.
                    return Err(ServiceError(match r {
                        Ok(()) => "the session bus connection ended".to_string(),
                        Err(e) => format!("following the portal failed: {e}"),
                    }));
                }
                () = arrived.notified() => {
                    if !drain(&mut cx, &rx) {
                        return Ok(());
                    }
                }
                () = &mut uptime.tick, if uptime.armed => {
                    UPTIME_TICKS.fetch_add(1, Ordering::Relaxed);
                    uptime.armed = false;
                    if !uptime.arm(&mut cx) {
                        return Ok(());
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Visible(true)) | Some(Msg::Watch { field: "uptime", on: true }) => {
                        // Shown again: the value now, then each minute.
                        uptime.armed = false;
                        if !uptime.arm(&mut cx) {
                            return Ok(());
                        }
                    }
                    Some(_) => {
                        if !uptime.arm(&mut cx) {
                            return Ok(());
                        }
                    }
                },
            }
        }
    }
}

/// How late after the minute turns `uptime` is read again: a timer
/// waking a little early would read the old minute and sleep again.
const SLACK: Duration = Duration::from_millis(50);

static UPTIME_TICKS: AtomicU64 = AtomicU64::new(0);

/// How many times a `system` service of this process woke to read the
/// uptime (tests: nothing ticks while nobody shows it, and at most once
/// a minute while someone does).
pub fn uptime_ticks() -> u64 {
    UPTIME_TICKS.load(Ordering::Relaxed)
}

/// `system.uptime`'s minute timer: armed only while a visible reader
/// reads `uptime`, to fire as the next whole minute of uptime turns.
struct Uptime {
    tick: std::pin::Pin<Box<tokio::time::Sleep>>,
    armed: bool,
}

impl Uptime {
    fn new() -> Uptime {
        Uptime {
            tick: Box::pin(tokio::time::sleep(Duration::ZERO)),
            armed: false,
        }
    }

    /// Arm or disarm the timer by whether a visible reader reads
    /// `uptime`; arming publishes the value now and sets the timer to
    /// the next minute. `false` once the service was stopped.
    fn arm(&mut self, cx: &mut Cx<System>) -> bool {
        if !(cx.watched("uptime") && cx.visible()) {
            self.armed = false;
            return true;
        }
        if self.armed {
            return true;
        }
        let Some(up) = read_uptime() else {
            // No `/proc/uptime`: nothing to count, nothing to wake for.
            return true;
        };
        self.tick
            .as_mut()
            .reset(tokio::time::Instant::now() + until_next_minute(up) + SLACK);
        self.armed = true;
        cx.update(|s| s.uptime = procfs::whole_minutes(up))
    }
}

/// The time since boot, from `/proc/uptime`.
fn read_uptime() -> Option<Duration> {
    procfs::parse_uptime(&std::fs::read_to_string("/proc/uptime").ok()?)
}

/// How long from an uptime of `up` until its next whole minute (a full
/// minute at a whole minute).
fn until_next_minute(up: Duration) -> Duration {
    let into = Duration::from_nanos((up.as_nanos() % 60_000_000_000) as u64);
    Duration::from_secs(60) - into
}

/// Apply every batch waiting; the boot batch makes the service ready.
/// `false` once the service was stopped.
fn drain(cx: &mut Cx<System>, rx: &std::sync::mpsc::Receiver<ChangeEvent>) -> bool {
    while let Ok(ev) = rx.try_recv() {
        let ChangeEvent::System(batch) = ev else {
            continue;
        };
        let sent = cx.update(|s| {
            for setting in &batch.settings {
                s.apply_setting(setting);
            }
        });
        if !sent {
            return false;
        }
        if batch.at_boot {
            cx.ready();
        }
    }
    true
}

/// The running kernel's release, as `uname -r` gives it.
fn kernel() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The host name, from the kernel.
fn hostname() -> Option<String> {
    let read = |p: &str| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    read("/proc/sys/kernel/hostname").or_else(|| read("/etc/hostname"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (gap 34) The uptime timer is set to the next whole minute: 54.7 s
    /// from 125.3 s, a whole minute from an exact one.
    #[test]
    fn the_uptime_timer_waits_for_the_next_whole_minute() {
        let d = until_next_minute(Duration::from_secs_f64(125.3));
        assert!((d.as_secs_f64() - 54.7).abs() < 1e-6, "{d:?}");
        assert_eq!(until_next_minute(Duration::ZERO), Duration::from_secs(60));
        assert_eq!(
            until_next_minute(Duration::from_secs(180)),
            Duration::from_secs(60)
        );
        assert_eq!(
            until_next_minute(Duration::from_millis(59_999)),
            Duration::from_millis(1)
        );
    }
}

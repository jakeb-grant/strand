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

use std::sync::Arc;

use strand_watch::{ChangeEvent, Contrast, SystemSetting};

use crate::{Cx, Rgba, ServiceError, Store, service};

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
        if let Some(h) = hostname() {
            cx.update(|s| s.hostname = h);
        }
        let conn = match cx.session().await {
            Ok(c) => c,
            Err(e) if cx.buses().session == crate::Bus::Disabled => {
                log::debug!("not following the portal's appearance settings: {e}");
                cx.ready();
                // Keep serving the values it has until stopped.
                while cx.recv().await.is_some() {}
                return Ok(());
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
        let follow = strand_watch::follow(&conn, sink);
        tokio::pin!(follow);
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
                m = cx.recv() => {
                    if m.is_none() {
                        return Ok(());
                    }
                }
            }
        }
    }
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

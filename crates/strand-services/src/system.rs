//! `system`: the desktop's appearance settings from the XDG portal
//! (`org.freedesktop.portal.Settings`: `color-scheme`, `accent-color`,
//! `contrast`, `reduced-motion`), followed live, and the host name.
//!
//! The portal client is `strand_watch::follow`, run on the shared
//! services runtime with the runtime's session connection (it used to
//! run on a thread of its own). Its boot read is the service's first
//! report: boot values, so `on change system.dark` does not fire at
//! boot; then [`Cx::ready`]. With no session bus or no portal, the
//! values the service started from are kept (a host seeds them with the
//! last values it saw: `Client::seed`).

use std::sync::Arc;

use strand_watch::{ChangeEvent, Contrast, SystemSetting};

use crate::{Cx, Rgba, ServiceError, Store, service};

/// The schema the `system` service serves.
pub const SCHEMA: &str = r#"
/// The desktop's appearance settings, from the XDG desktop portal and
/// followed live, and the machine's host name.
service system {
  /// The desktop prefers a dark style.
  dark: bool
  /// The desktop's accent colour, if it sets one.
  accent: color?
  /// The contrast preference, as `material(contrast:)` takes it: 0 for
  /// normal, 1 for high.
  contrast: float
  /// Motion should be reduced: springs snap, and loops, time signals and
  /// effects stop.
  reduced_motion: bool
  /// The machine's host name.
  hostname: text
}
"#;

/// See the module docs.
#[service(name = "system", schema = SCHEMA)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct System {
    /// The desktop prefers a dark style.
    pub dark: bool,
    /// The desktop's accent colour.
    pub accent: Option<Rgba>,
    /// 0 normal, 1 high.
    pub contrast: f64,
    /// Motion should be reduced.
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
            Err(e) => {
                log::warn!("not following the portal's appearance settings: {e}");
                cx.ready();
                // Keep serving the values it has until stopped.
                while cx.recv().await.is_some() {}
                return Ok(());
            }
        };
        let (sink, rx) = strand_watch::channel();
        let arrived = Arc::new(tokio::sync::Notify::new());
        let notify = arrived.clone();
        let sink = sink.with_waker(move || notify.notify_one());
        let follow = strand_watch::follow(&conn, sink);
        tokio::pin!(follow);
        let mut following = true;
        loop {
            tokio::select! {
                r = &mut follow, if following => {
                    following = false;
                    if let Err(e) = r {
                        log::warn!("not following the portal's appearance settings: {e}");
                    }
                    // Batches it sent before ending.
                    if !drain(&mut cx, &rx) {
                        return Ok(());
                    }
                    cx.ready();
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

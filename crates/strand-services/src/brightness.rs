//! `brightness`: the screen backlight.
//!
//! Read from `/sys/class/backlight/<device>` (`actual_brightness` over
//! `max_brightness`) and watched with inotify: the kernel notifies the
//! attribute when the level changes (a brightness key, another tool), so
//! the level follows with no polling. Written through logind's
//! `Session.SetBrightness("backlight", device, raw)` on the caller's
//! session (`/org/freedesktop/login1/session/auto`, logind-zbus's
//! proxy), which needs no root and no udev rules (design.md's table).
//!
//! With several backlights the firmware one wins over a platform one,
//! and that over a raw one (the kernel's own advice for user space).
//! Tests point the service at a directory of their own
//! ([`set_backlight_root`], or `STRAND_BACKLIGHT_DIR` for a child
//! process).

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::io::unix::AsyncFd;

use crate::{Cx, Msg, ServiceError, Store, service};

/// The schema the `brightness` service serves.
pub const SCHEMA: &str = strand_services_schema::BRIGHTNESS;

/// Where the kernel lists backlights.
pub const SYSFS: &str = "/sys/class/backlight";

/// The session logind resolves to the caller's.
pub const SESSION: &str = "/org/freedesktop/login1/session/auto";

static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Read backlights from `root` instead of [`SYSFS`] (tests: a directory
/// of fake backlights). `None` restores the default.
pub fn set_backlight_root(root: Option<PathBuf>) {
    if let Ok(mut r) = ROOT.lock() {
        *r = root;
    }
}

fn root() -> PathBuf {
    if let Some(r) = ROOT.lock().ok().and_then(|r| r.clone()) {
        return r;
    }
    std::env::var_os("STRAND_BACKLIGHT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SYSFS))
}

/// See the module docs.
#[service(name = "brightness")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Brightness {
    /// Backlight level, 0 to 1. Writable.
    #[store(rw)]
    pub level: f64,
    /// A backlight was found.
    pub available: bool,
}

/// One backlight device.
#[derive(Clone, Debug, PartialEq)]
pub struct Backlight {
    /// Its directory.
    pub dir: PathBuf,
    /// Its name (what logind's `SetBrightness` takes).
    pub name: String,
    /// Its `max_brightness`.
    pub max: u64,
}

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

impl Backlight {
    /// The backlight to use under `root`, if any.
    pub fn find(root: &Path) -> Option<Backlight> {
        let rank = |dir: &Path| match std::fs::read_to_string(dir.join("type"))
            .unwrap_or_default()
            .trim()
        {
            "firmware" => 0,
            "platform" => 1,
            "raw" => 2,
            _ => 3,
        };
        let mut found: Vec<(u8, Backlight)> = std::fs::read_dir(root)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let dir = e.path();
                let max = read_u64(&dir.join("max_brightness")).filter(|m| *m > 0)?;
                Some((
                    rank(&dir),
                    Backlight {
                        name: e.file_name().to_string_lossy().into_owned(),
                        dir,
                        max,
                    },
                ))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        found.into_iter().next().map(|(_, b)| b)
    }

    /// The level now, 0 to 1 (`actual_brightness`, else `brightness`).
    pub fn level(&self) -> Option<f64> {
        let raw = read_u64(&self.dir.join("actual_brightness"))
            .or_else(|| read_u64(&self.dir.join("brightness")))?;
        Some((raw as f64 / self.max as f64).clamp(0.0, 1.0))
    }

    /// The raw value for `level`.
    pub fn raw(&self, level: f64) -> u32 {
        let l = if level.is_finite() { level } else { 0.0 };
        (l.clamp(0.0, 1.0) * self.max as f64).round() as u32
    }

    /// An inotify watch on its level files (`MODIFY`: the kernel's
    /// notification of a change; `CLOSE_WRITE`: a file written whole, as
    /// tests do).
    fn watch(&self) -> std::io::Result<AsyncFd<OwnedFd>> {
        use rustix::fs::inotify;
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        let mut any = false;
        for f in ["actual_brightness", "brightness"] {
            let p = self.dir.join(f);
            if p.exists() {
                inotify::add_watch(
                    &fd,
                    &p,
                    inotify::WatchFlags::MODIFY | inotify::WatchFlags::CLOSE_WRITE,
                )?;
                any = true;
            }
        }
        if !any {
            return Err(std::io::Error::other("no level file to watch"));
        }
        AsyncFd::new(fd)
    }
}

/// Read everything waiting on the inotify descriptor. `false`: it failed.
fn drain(fd: &OwnedFd) -> bool {
    let mut buf = [0u8; 1024];
    loop {
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(rustix::io::Errno::AGAIN) => return true,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

impl Brightness {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let Some(light) = Backlight::find(&root()) else {
            // No backlight (a desktop): unavailable until restarted.
            cx.update(|s| *s = Brightness::default());
            cx.ready();
            while let Some(m) = cx.recv().await {
                if let Msg::Write(w) = m {
                    cx.report(&w, |s| s.level = 0.0);
                }
            }
            return Ok(());
        };
        let level = light.level().unwrap_or(0.0);
        cx.update(|s| {
            s.level = level;
            s.available = true;
        });
        cx.ready();
        let watch = match light.watch() {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("brightness: not watching {}: {e}", light.dir.display());
                None
            }
        };
        loop {
            tokio::select! {
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Write(w)) => {
                        let wanted: f64 = w.value().unwrap_or(cx.state().level);
                        let raw = light.raw(wanted);
                        let now = match set_brightness(&cx, &light.name, raw).await {
                            Ok(()) => raw as f64 / light.max as f64,
                            Err(e) => {
                                log::warn!("brightness: logind refused {raw} for {}: {e}", light.name);
                                light.level().unwrap_or(cx.state().level)
                            }
                        };
                        if !cx.report(&w, |s| s.level = now) {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                },
                r = async {
                    match &watch {
                        Some(w) => w.readable().await.map(|mut g| { g.clear_ready(); }),
                        None => std::future::pending().await,
                    }
                } => {
                    let ok = r.is_ok() && watch.as_ref().is_some_and(|w| drain(w.get_ref()));
                    if !ok {
                        return Err(ServiceError("the backlight watch failed".into()));
                    }
                    if let Some(l) = light.level()
                        && !cx.update(|s| s.level = l)
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// logind's `SetBrightness` on the caller's session.
async fn set_brightness(cx: &Cx<Brightness>, name: &str, raw: u32) -> zbus::Result<()> {
    let conn = cx.system().await?;
    let session = logind_zbus::session::SessionProxy::builder(&conn)
        .path(SESSION)?
        .build()
        .await?;
    session.set_brightness("backlight", name, raw).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(root: &Path, name: &str, ty: &str, max: u64, now: u64) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("type"), ty).unwrap();
        std::fs::write(d.join("max_brightness"), max.to_string()).unwrap();
        std::fs::write(d.join("brightness"), now.to_string()).unwrap();
        std::fs::write(d.join("actual_brightness"), now.to_string()).unwrap();
    }

    #[test]
    fn the_firmware_backlight_wins_and_levels_are_fractions() {
        let root = std::env::temp_dir().join(format!("strand-bl-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        fake(&root, "acpi_video0", "firmware", 15, 3);
        fake(&root, "intel_backlight", "raw", 1000, 500);
        let b = Backlight::find(&root).unwrap();
        assert_eq!(b.name, "acpi_video0");
        assert_eq!(b.level(), Some(0.2));
        assert_eq!(b.raw(0.5), 8);
        assert_eq!(b.raw(2.0), 15);
        assert_eq!(b.raw(f64::NAN), 0);
        assert_eq!(Backlight::find(&root.join("none")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}

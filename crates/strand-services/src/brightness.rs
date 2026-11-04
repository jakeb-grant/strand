//! `brightness`: the screen's and the keyboard's backlights.
//!
//! The screen's is read from `/sys/class/backlight/<device>`
//! (`actual_brightness` over `max_brightness`), the keyboard's from
//! `/sys/class/leds/<name>::kbd_backlight` (`brightness` over
//! `max_brightness`), and both are watched with inotify: the kernel
//! notifies the attribute when the level changes (a brightness key,
//! another tool, logind's own write), so the levels follow with no
//! polling. A keyboard light the firmware changes on its own (an Fn key,
//! a timeout) is seen where the driver reports it on
//! `brightness_hw_changed` (`LED_BRIGHT_HW_CHANGED`: dell-laptop,
//! thinkpad_acpi and most laptop drivers), which is watched too; on a
//! driver without it, such a change shows at the next write or start
//! (decisions.md power-media-kbd). Both are written through logind's
//! `Session.SetBrightness(subsystem, device, raw)` on the caller's session
//! (`/org/freedesktop/login1/session/auto`, logind-zbus's proxy), with
//! `backlight` or `leds`, which needs no root and no udev rules
//! (design.md's table); the call is bounded by [`dbus::READ_TIMEOUT`].
//!
//! With several backlights the firmware one wins over a platform one,
//! and that over a raw one (the kernel's own advice for user space); with
//! several keyboard lights, the first by name. A light that appears later
//! (a keyboard plugged in) is found at the next start. The two are
//! independent: a desktop with a lit keyboard has `keyboard_present` and
//! no `available`. Tests point the service at directories of their own
//! ([`set_backlight_root`] and [`set_leds_root`], or `STRAND_BACKLIGHT_DIR`
//! and `STRAND_LEDS_DIR` for a child process).

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::io::unix::AsyncFd;

use crate::{Cx, Msg, ServiceError, Store, dbus, service};

/// The schema the `brightness` service serves.
pub const SCHEMA: &str = strand_services_schema::BRIGHTNESS;

/// Where the kernel lists backlights.
pub const SYSFS: &str = "/sys/class/backlight";

/// Where the kernel lists lights (LEDs), the keyboard's among them.
pub const LEDS: &str = "/sys/class/leds";

/// The session logind resolves to the caller's.
pub const SESSION: &str = "/org/freedesktop/login1/session/auto";

static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);
static LEDS_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Read backlights from `root` instead of [`SYSFS`] (tests: a directory
/// of fake backlights). `None` restores the default.
pub fn set_backlight_root(root: Option<PathBuf>) {
    if let Ok(mut r) = ROOT.lock() {
        *r = root;
    }
}

/// Read lights from `root` instead of [`LEDS`] (tests: a directory of
/// fake lights). `None` restores the default.
pub fn set_leds_root(root: Option<PathBuf>) {
    if let Ok(mut r) = LEDS_ROOT.lock() {
        *r = root;
    }
}

fn root_of(set: &Mutex<Option<PathBuf>>, env: &str, default: &str) -> PathBuf {
    if let Some(r) = set.lock().ok().and_then(|r| r.clone()) {
        return r;
    }
    std::env::var_os(env)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

/// See the module docs.
#[service(name = "brightness")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Brightness {
    /// Backlight level, 0 to 1. Writable.
    #[store(rw)]
    pub level: f64,
    /// A screen backlight was found.
    pub available: bool,
    /// The keyboard's backlight level, 0 to 1, in the steps it has (most
    /// have two or three). Use `slider { value: <-> brightness.keyboard }`
    /// or `strand set brightness.keyboard +50%`. Writable: a level between
    /// two steps goes to the nearest. A change the firmware makes on its own
    /// (an Fn key, or the light timing out) shows where the driver reports it
    /// (`brightness_hw_changed`), as most laptop drivers do. 0 without a
    /// keyboard backlight.
    #[store(rw)]
    pub keyboard: f64,
    /// The keyboard has a backlight (a `*::kbd_backlight` light), as
    /// `battery.present` says a battery exists.
    pub keyboard_present: bool,
}

/// One light: a screen backlight or a keyboard's.
#[derive(Clone, Debug, PartialEq)]
pub struct Light {
    /// Its directory.
    pub dir: PathBuf,
    /// Its name (what logind's `SetBrightness` takes).
    pub name: String,
    /// Its `max_brightness`.
    pub max: u64,
    /// Its kernel subsystem, as `SetBrightness` takes it: `backlight` or
    /// `leds`.
    pub subsystem: &'static str,
}

fn read_u64(p: &Path) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// The lights under `root` with a `max_brightness` above 0, each with its
/// name.
fn lights(root: &Path, subsystem: &'static str) -> Vec<Light> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let dir = e.path();
            let max = read_u64(&dir.join("max_brightness")).filter(|m| *m > 0)?;
            Some(Light {
                name: e.file_name().to_string_lossy().into_owned(),
                dir,
                max,
                subsystem,
            })
        })
        .collect()
}

impl Light {
    /// The screen backlight to use under `root` (a `/sys/class/backlight`),
    /// if any.
    pub fn screen(root: &Path) -> Option<Light> {
        let rank = |dir: &Path| match std::fs::read_to_string(dir.join("type"))
            .unwrap_or_default()
            .trim()
        {
            "firmware" => 0,
            "platform" => 1,
            "raw" => 2,
            _ => 3,
        };
        let mut found: Vec<(u8, Light)> = lights(root, "backlight")
            .into_iter()
            .map(|l| (rank(&l.dir), l))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        found.into_iter().next().map(|(_, l)| l)
    }

    /// The keyboard's backlight under `root` (a `/sys/class/leds`): the
    /// first `*::kbd_backlight` by name, if any.
    pub fn keyboard(root: &Path) -> Option<Light> {
        let mut found: Vec<Light> = lights(root, "leds")
            .into_iter()
            .filter(|l| l.name.ends_with("::kbd_backlight"))
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found.into_iter().next()
    }

    /// The level now, 0 to 1 (a backlight's `actual_brightness`, else
    /// `brightness`).
    pub fn level(&self) -> Option<f64> {
        let actual = (self.subsystem == "backlight")
            .then(|| read_u64(&self.dir.join("actual_brightness")))
            .flatten();
        let raw = actual.or_else(|| read_u64(&self.dir.join("brightness")))?;
        Some((raw as f64 / self.max as f64).clamp(0.0, 1.0))
    }

    /// The raw value for `level`: the nearest step.
    pub fn raw(&self, level: f64) -> u32 {
        let l = if level.is_finite() { level } else { 0.0 };
        (l.clamp(0.0, 1.0) * self.max as f64).round() as u32
    }

    /// The files whose changes say its level changed: a backlight's
    /// `actual_brightness` and `brightness`; a light's `brightness` (any
    /// write, logind's included) and `brightness_hw_changed` (the
    /// firmware's own change, where the driver reports it).
    fn watched(&self) -> [&'static str; 2] {
        if self.subsystem == "backlight" {
            ["actual_brightness", "brightness"]
        } else {
            ["brightness", "brightness_hw_changed"]
        }
    }

    /// An inotify watch on its level files (`MODIFY`: the kernel's
    /// notification of a change; `CLOSE_WRITE`: a file written whole, as
    /// tests do).
    fn watch(&self) -> std::io::Result<AsyncFd<OwnedFd>> {
        use rustix::fs::inotify;
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        let mut any = false;
        for f in self.watched() {
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

    /// [`Light::watch`], or none (logged): the level is then read at the
    /// start and after each write only.
    fn watch_or_say(&self) -> Option<AsyncFd<OwnedFd>> {
        match self.watch() {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("brightness: not watching {}: {e}", self.dir.display());
                None
            }
        }
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

/// `watch` has something to read, emptied: `false` when it failed. Never
/// ready without a watch.
async fn woke(watch: &Option<AsyncFd<OwnedFd>>) -> bool {
    match watch {
        Some(w) => match w.readable().await {
            Ok(mut g) => {
                g.clear_ready();
                drain(w.get_ref())
            }
            Err(_) => false,
        },
        None => std::future::pending().await,
    }
}

impl Brightness {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let screen = Light::screen(&root_of(&ROOT, "STRAND_BACKLIGHT_DIR", SYSFS));
        let keyboard = Light::keyboard(&root_of(&LEDS_ROOT, "STRAND_LEDS_DIR", LEDS));
        if screen.is_none() && keyboard.is_none() {
            // No light (a desktop): unavailable until restarted.
            cx.update(|s| *s = Brightness::default());
            cx.ready();
            while let Some(m) = cx.recv().await {
                if let Msg::Write(w) = m {
                    cx.report(&w, |s| *s = Brightness::default());
                }
            }
            return Ok(());
        }
        // Watch before the first read (subscribe, then read, as
        // `dbus::Daemon` does): a change between the two is then either in
        // the read or queued on the watch, never lost.
        let screen_watch = screen.as_ref().and_then(Light::watch_or_say);
        let keyboard_watch = keyboard.as_ref().and_then(Light::watch_or_say);
        let level = screen.as_ref().and_then(Light::level).unwrap_or(0.0);
        let lit = keyboard.as_ref().and_then(Light::level).unwrap_or(0.0);
        cx.update(|s| {
            s.level = level;
            s.available = screen.is_some();
            s.keyboard = lit;
            s.keyboard_present = keyboard.is_some();
        });
        cx.ready();
        loop {
            tokio::select! {
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Write(w)) if w.field == "level" || w.field == "keyboard" => {
                        let on_keyboard = w.field == "keyboard";
                        let (light, current) = if on_keyboard {
                            (keyboard.as_ref(), cx.state().keyboard)
                        } else {
                            (screen.as_ref(), cx.state().level)
                        };
                        // No such light: nothing to set, the value kept.
                        let now = match light {
                            None => current,
                            Some(light) => {
                                let wanted: f64 = w.value().unwrap_or(current);
                                let raw = light.raw(wanted);
                                match set_brightness(&cx, light, raw).await {
                                    Ok(()) => raw as f64 / light.max as f64,
                                    Err(e) => {
                                        log::warn!("brightness: logind refused {raw} for {}: {e}", light.name);
                                        light.level().unwrap_or(current)
                                    }
                                }
                            }
                        };
                        let reported = cx.report(&w, |s| {
                            if on_keyboard {
                                s.keyboard = now;
                            } else {
                                s.level = now;
                            }
                        });
                        if !reported {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                },
                ok = woke(&screen_watch) => {
                    if !ok {
                        return Err(ServiceError("the backlight watch failed".into()));
                    }
                    if let Some(l) = screen.as_ref().and_then(Light::level)
                        && !cx.update(|s| s.level = l)
                    {
                        return Ok(());
                    }
                }
                ok = woke(&keyboard_watch) => {
                    if !ok {
                        return Err(ServiceError("the keyboard light's watch failed".into()));
                    }
                    if let Some(l) = keyboard.as_ref().and_then(Light::level)
                        && !cx.update(|s| s.keyboard = l)
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// logind's `SetBrightness` of `light` on the caller's session, bounded
/// by [`dbus::READ_TIMEOUT`] (a hung logind holds the body that long at
/// most).
async fn set_brightness(cx: &Cx<Brightness>, light: &Light, raw: u32) -> zbus::Result<()> {
    let conn = cx.system().await?;
    dbus::timed_for(dbus::READ_TIMEOUT, async {
        let session = logind_zbus::session::SessionProxy::builder(&conn)
            .path(SESSION)?
            .build()
            .await?;
        session
            .set_brightness(light.subsystem, &light.name, raw)
            .await
    })
    .await
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

    fn led(root: &Path, name: &str, max: u64, now: u64) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("max_brightness"), max.to_string()).unwrap();
        std::fs::write(d.join("brightness"), now.to_string()).unwrap();
    }

    #[test]
    fn the_firmware_backlight_wins_and_levels_are_fractions() {
        let root = std::env::temp_dir().join(format!("strand-bl-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        fake(&root, "acpi_video0", "firmware", 15, 3);
        fake(&root, "intel_backlight", "raw", 1000, 500);
        let b = Light::screen(&root).unwrap();
        assert_eq!(b.name, "acpi_video0");
        assert_eq!(b.subsystem, "backlight");
        assert_eq!(b.level(), Some(0.2));
        assert_eq!(b.raw(0.5), 8);
        assert_eq!(b.raw(2.0), 15);
        assert_eq!(b.raw(f64::NAN), 0);
        assert_eq!(Light::screen(&root.join("none")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// (power-media-kbd) The keyboard's light is the first
    /// `*::kbd_backlight` by name with a maximum above 0; the lock keys'
    /// and other lights are not it; its few steps round to the nearest.
    #[test]
    fn the_keyboard_light_is_the_first_kbd_backlight() {
        let root = std::env::temp_dir().join(format!("strand-leds-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        led(&root, "input3::capslock", 1, 1);
        led(&root, "platform::micmute", 1, 0);
        assert_eq!(Light::keyboard(&root), None, "no keyboard light");
        led(&root, "tpacpi::kbd_backlight", 0, 0);
        assert_eq!(Light::keyboard(&root), None, "a maximum of 0");
        led(&root, "zz::kbd_backlight", 3, 3);
        led(&root, "dell::kbd_backlight", 2, 1);
        let k = Light::keyboard(&root).unwrap();
        assert_eq!(k.name, "dell::kbd_backlight", "first by name");
        assert_eq!(k.subsystem, "leds");
        assert_eq!(k.level(), Some(0.5));
        // A light's `actual_brightness` is no level of its own.
        std::fs::write(k.dir.join("actual_brightness"), "2").unwrap();
        assert_eq!(k.level(), Some(0.5));
        assert_eq!((k.raw(0.2), k.raw(0.3), k.raw(0.8)), (0, 1, 2));
        assert_eq!((k.raw(1.0), k.raw(-1.0)), (2, 0));
        assert_eq!(Light::keyboard(&root.join("none")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}

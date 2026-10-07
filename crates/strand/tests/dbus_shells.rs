//! design.md's shells on the real D-Bus services (M3), end to end:
//! `strand run` without `STRAND_MOCK` on a headless sway, with a private
//! `dbus-daemon` carrying python-dbusmock's `upower` and `logind`, an
//! app's tray item (a small zbus StatusNotifierItem) and the notifications
//! an app sends to the shell's own server. The bar (a) shows the battery
//! UPower reports (in `$error` once low, gone when UPower says there is
//! none) and the tray item's icon; the toasts (c) show a notification
//! sent over D-Bus and leave when the sender closes it; `strand set
//! brightness.level +10%` goes through logind (the mock writes a fake
//! backlight) and pops the OSD (d) up.
//!
//! Shots of each state are written to `$STRAND_SHOTS` when it is set.
//! Skipped, loudly, without sway, grim, dbus-daemon or python-dbusmock
//! (CI sets `STRAND_REQUIRE_SWAY` and `STRAND_REQUIRE_DBUS`).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use strand_services::testing::{DbusMock, PrivateBus};
use zbus::zvariant::{ObjectPath, Value};

const FILES: [(&str, &str); 4] = [
    (
        "theme.strand",
        include_str!("../../strand-compiler/tests/fixtures/theme.strand"),
    ),
    (
        "bar.strand",
        include_str!("../../strand-compiler/tests/fixtures/bar.strand"),
    ),
    (
        "toasts.strand",
        include_str!("../../strand-compiler/tests/fixtures/toasts.strand"),
    ),
    (
        "osd.strand",
        include_str!("../../strand-compiler/tests/fixtures/osd.strand"),
    ),
];

const UPOWER: &str = "org.freedesktop.UPower";
const LOGIND: &str = "org.freedesktop.login1";
const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const W: usize = 1280;
const H: usize = 720;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, PartialEq)]
struct Img {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Img {
    fn ppm(ppm: &[u8]) -> Img {
        let mut nl = ppm.iter().enumerate().filter(|(_, b)| **b == b'\n');
        let (a, b, c) = (
            nl.next().unwrap().0,
            nl.next().unwrap().0,
            nl.next().unwrap().0,
        );
        let dims = std::str::from_utf8(&ppm[a + 1..b]).unwrap();
        let mut it = dims.split_whitespace().map(|v| v.parse::<usize>().unwrap());
        let (w, h) = (it.next().unwrap(), it.next().unwrap());
        Img {
            w,
            h,
            rgb: ppm[c + 1..c + 1 + w * h * 3].to_vec(),
        }
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// Pixels in `x0..x1` × `y0..y1` that `f` accepts.
    fn count(
        &self,
        xs: std::ops::Range<usize>,
        ys: std::ops::Range<usize>,
        f: impl Fn([u8; 3]) -> bool,
    ) -> usize {
        ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
            .filter(|&(x, y)| x < self.w && y < self.h && f(self.px(x, y)))
            .count()
    }
}

/// The app's tray icon: pure magenta, found by colour in the bar.
fn magenta(p: [u8; 3]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] > 200
}

/// Clearly red (`$error` text), not grey, white or the accent.
fn reddish(p: [u8; 3]) -> bool {
    p[0] > 120 && p[0] as i32 - p[1] as i32 > 60 && p[0] as i32 - p[2] as i32 > 60
}

/// A tray item serving a 16×16 magenta pixmap.
struct Item;

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {}
    #[zbus(property)]
    fn id(&self) -> String {
        "e2e".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        "E2E".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let px = [255u8, 255, 0, 255];
        vec![(16, 16, px.repeat(16 * 16))]
    }
}

struct Shell {
    dir: PathBuf,
    display: String,
    log: PathBuf,
    shots: Option<PathBuf>,
    n: std::cell::Cell<u32>,
    strand: Option<Proc>,
    _sway: Proc,
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Shell {
    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
        ]
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn shot(&self) -> Img {
        let path = self.dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", "HEADLESS-1"])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim");
        Img::ppm(&std::fs::read(&path).unwrap())
    }

    /// A PNG of the output under `$STRAND_SHOTS`, numbered and named.
    fn keep(&self, name: &str) {
        let Some(dir) = &self.shots else {
            return;
        };
        let n = self.n.get() + 1;
        self.n.set(n);
        let _ = std::fs::create_dir_all(dir);
        let _ = Command::new("grim")
            .args(["-t", "png", "-o", "HEADLESS-1"])
            .arg(dir.join(format!("dbus-{n:02}-{name}.png")))
            .envs(self.env())
            .status();
    }

    fn wait(&mut self, what: &str, done: impl Fn(&Shell) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(self) {
            let exited = self
                .strand
                .as_mut()
                .is_some_and(|p| p.0.try_wait().unwrap().is_some());
            assert!(!exited, "strand exited: {}", self.log_text());
            assert!(
                Instant::now() < deadline,
                "never: {what}\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Distinct surfaces painted so far (`STRAND_LOG=damage`).
    fn surfaces(&self) -> usize {
        let mut s: Vec<String> = self
            .log_text()
            .lines()
            .filter(|l| l.starts_with("strand: damage"))
            .filter_map(|l| {
                l.split_whitespace()
                    .find(|w| w.starts_with("surface="))
                    .map(String::from)
            })
            .collect();
        s.sort();
        s.dedup();
        s.len()
    }

    fn cli(&self, args: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .envs(self.env())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "strand {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!(
                "\n*** SKIPPED: {tool} is not installed; the D-Bus shells test did not run ***\n"
            );
            return false;
        }
    }
    true
}

fn sway(dir: &Path) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        format!("xwayland disable\noutput HEADLESS-1 resolution {W}x{H} position 0 0 scale 1\n"),
    )
    .unwrap();
    let log = std::fs::File::create(dir.join("sway.log")).unwrap();
    let mut cmd = Command::new("sway");
    // SAFETY: the hook runs between fork and exec and makes one
    // async-signal-safe syscall.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd
        .arg("-c")
        .arg(&cfg)
        .env("XDG_RUNTIME_DIR", dir)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("SWAYSOCK")
        .env_remove("DISPLAY")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let sway = Proc(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let display = names
            .iter()
            .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
        let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
        if let (Some(d), Some(i)) = (display, ipc) {
            let ok = Command::new("swaymsg")
                .args(["-t", "get_version"])
                .env("SWAYSOCK", dir.join(i))
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                return (sway, d.clone());
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn call<B>(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Message
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    tokio
        .block_on(conn.call_method(Some(dest), path, Some(iface), method, body))
        .unwrap_or_else(|e| panic!("{dest} {iface}.{method}: {e}"))
}

#[test]
fn design_shells_on_the_real_dbus_services() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    // UPower: a battery at 42 %, discharging, 1 h 30 m left.
    let Some(_upower) = DbusMock::start(&bus, "upower", true, None, UPOWER) else {
        return;
    };
    call(
        &tokio,
        &conn,
        UPOWER,
        "/org/freedesktop/UPower",
        "org.freedesktop.DBus.Mock",
        "SetupDisplayDevice",
        &(
            2u32, 2u32, 42.0f64, 42.0f64, 100.0f64, 9.0f64, 5400i64, 0i64, true, "", 1u32,
        ),
    );
    // logind with the caller's session, whose SetBrightness writes a fake
    // backlight as the kernel would.
    let Some(_logind) = DbusMock::start(&bus, "logind", true, None, LOGIND) else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("strand-dbus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let backlight = dir.join("backlight");
    let light = backlight.join("intel_backlight");
    std::fs::create_dir_all(&light).unwrap();
    for (f, v) in [
        ("max_brightness", "1000"),
        ("brightness", "500"),
        ("actual_brightness", "500"),
        ("type", "raw"),
    ] {
        std::fs::write(light.join(f), format!("{v}\n")).unwrap();
    }
    let session: String = call(
        &tokio,
        &conn,
        LOGIND,
        "/org/freedesktop/login1",
        "org.freedesktop.DBus.Mock",
        "AddSession",
        &("auto", "seat0", 1000u32, "user", true),
    )
    .body()
    .deserialize()
    .unwrap();
    let code = format!(
        "for f in ('brightness', 'actual_brightness'):\n    \
         open('{}/' + args[1] + '/' + f, 'w').write('%d\\n' % args[2])",
        backlight.display()
    );
    call(
        &tokio,
        &conn,
        LOGIND,
        &session,
        "org.freedesktop.DBus.Mock",
        "AddMethod",
        &(
            "org.freedesktop.login1.Session",
            "SetBrightness",
            "ssu",
            "",
            code.as_str(),
        ),
    );

    let (sway, display) = sway(&dir);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    for (name, text) in FILES {
        std::fs::write(config.join(name), text).unwrap();
    }
    let log = dir.join("strand.log");
    let strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("STRAND_LOG", "damage")
            .env("STRAND_BACKLIGHT_DIR", &backlight)
            .envs(bus.env())
            .env_remove("STRAND_MOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let mut sh = Shell {
        dir: dir.clone(),
        display,
        log,
        shots: std::env::var_os("STRAND_SHOTS").map(PathBuf::from),
        n: std::cell::Cell::new(0),
        strand: Some(strand),
        _sway: sway,
    };
    sh.wait("the bar", |s| s.surfaces() >= 1);
    // The bar's end, right of the clock: the battery (and later the
    // tray icon) are there.
    let end = |_: &Img| (W / 2 + 120..W - 8, 8..44);
    sh.wait("the battery in the bar", |s| {
        let img = s.shot();
        let (xs, ys) = end(&img);
        // Text and icons in the end part: more than the volume icon.
        img.count(xs, ys, |p| {
            p.iter().map(|&c| u32::from(c)).sum::<u32>() < 300
        }) > 60
    });
    std::thread::sleep(Duration::from_millis(600));
    sh.keep("battery-42");
    let at_42 = sh.shot();

    // The tray: the session has no watcher, so the shell is it; an app
    // registers its item, whose magenta icon joins the bar.
    let item_name = "org.kde.StatusNotifierItem-777-1";
    let item = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(item_name)
            .unwrap()
            .serve_at("/StatusNotifierItem", Item)
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    assert!(bus.wait_for_name("org.kde.StatusNotifierWatcher", Duration::from_secs(10)));
    call(
        &tokio,
        &item,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
        "RegisterStatusNotifierItem",
        &(item_name,),
    );
    sh.wait("the tray icon", |s| {
        let img = s.shot();
        let (xs, ys) = end(&img);
        img.count(xs, ys, magenta) >= 100
    });
    sh.keep("tray-icon");

    // The battery runs low: `$error`.
    call(
        &tokio,
        &conn,
        UPOWER,
        "/org/freedesktop/UPower",
        "org.freedesktop.DBus.Mock",
        "SetDeviceProperties",
        &(
            ObjectPath::try_from(DISPLAY_DEVICE).unwrap(),
            HashMap::from([("Percentage", Value::from(9.0f64))]),
        ),
    );
    sh.wait("the low battery in red", |s| {
        let img = s.shot();
        let (xs, ys) = end(&img);
        img.count(xs, ys, reddish) > 20
    });
    assert_eq!(
        at_42.count(end(&at_42).0, end(&at_42).1, reddish),
        0,
        "not red at 42 %"
    );
    sh.keep("battery-9-low");

    // A notification over D-Bus, as an app sends it: a toast at the top
    // right.
    let id: u32 = call(
        &tokio,
        &conn,
        NOTIFICATIONS,
        "/org/freedesktop/Notifications",
        NOTIFICATIONS,
        "Notify",
        &(
            "Mail",
            0u32,
            "mail-unread",
            "Hello from D-Bus",
            "The <b>real</b> notification server",
            Vec::<&str>::new(),
            HashMap::<&str, Value<'_>>::new(),
            600_000i32,
        ),
    )
    .body()
    .deserialize()
    .unwrap();
    let toasts = |img: &Img| img.count(W - 420..W - 12, 60..200, |p| p != img.px(W - 2, H / 2));
    let before = sh.surfaces();
    sh.wait("the toast", |s| {
        let img = s.shot();
        s.surfaces() > before && toasts(&img) > 2000
    });
    std::thread::sleep(Duration::from_millis(800));
    sh.keep("toast");
    // The sender closes it: the toast leaves.
    call(
        &tokio,
        &conn,
        NOTIFICATIONS,
        "/org/freedesktop/Notifications",
        NOTIFICATIONS,
        "CloseNotification",
        &(id,),
    );
    sh.wait("the toast gone", |s| toasts(&s.shot()) < 50);

    // `strand set brightness.level +10%`: through logind (the fake
    // backlight now reads 600 of 1000), and the OSD shows it.
    sh.cli(&["set", "brightness.level", "+10%"]);
    sh.wait("logind set 600", |_| {
        std::fs::read_to_string(light.join("brightness"))
            .unwrap_or_default()
            .trim()
            == "600"
    });
    let osd = |img: &Img| {
        img.count(W / 2 - 140..W / 2 + 140, H - 140..H - 60, |p| {
            p != img.px(8, H / 2)
        })
    };
    sh.wait("the OSD", |s| osd(&s.shot()) > 1000);
    sh.keep("osd-brightness");

    // UPower says there is no battery: the bar's Battery goes.
    call(
        &tokio,
        &conn,
        UPOWER,
        "/org/freedesktop/UPower",
        "org.freedesktop.DBus.Mock",
        "SetDeviceProperties",
        &(
            ObjectPath::try_from(DISPLAY_DEVICE).unwrap(),
            HashMap::from([("IsPresent", Value::from(false))]),
        ),
    );
    sh.wait("no battery", |s| {
        let img = s.shot();
        let (xs, ys) = end(&img);
        img.count(xs, ys, reddish) == 0
    });
    sh.keep("no-battery");
    drop(item);
    let errors: Vec<String> = sh
        .log_text()
        .lines()
        .filter(|l| l.contains("ERROR") || l.contains("panicked"))
        .map(String::from)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

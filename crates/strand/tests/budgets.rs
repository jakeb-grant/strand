//! M3 exit, "memory verified", and the idle budget on the real services
//! (design.md, "Memory budget" and "Testing": the build fails above 34 MB
//! for the two-monitor bar; a running service with nothing changing wakes
//! nothing).
//!
//! `strand run` without `STRAND_MOCK` on a headless sway with two
//! 2560×1440 outputs (scale 1 and 1.25), beside python-dbusmock's UPower
//! (a battery at 42 %), NetworkManager (a Wi-Fi network joined) and
//! logind and a mock portal (dark) on a private `dbus-daemon`; the shell's
//! own notifications server and tray host there; sway's IPC (workspaces,
//! windows, a window open); a private PipeWire with WirePlumber (audio,
//! the sink at 50 %); a backlight directory (brightness).
//!
//! - `the_design_bar_on_the_real_services_keeps_the_budget`, design.md's
//!   bar (`bar.strand` with `theme.strand`) with one component added to
//!   its end section that reads `network` and `notifications` too (the
//!   bar itself reads workspaces, windows, audio, battery, tray, clock):
//!   once every value is on screen and the boot work is done, PSS stays
//!   within the 34 MB gate (release; a debug build has its own ceiling),
//!   no page of it is a transparent huge page, and over 10 s between two
//!   minute ticks no thread of strand's wakes and no frame is drawn.
//! - `the_full_shell_on_the_real_services_is_measured`, the five design
//!   files (bar, launcher, toasts, OSD, theme) with the machine's own
//!   desktop entries and icons (`/usr/share`), `HEADLESS-1` at scale 2
//!   (design.md budgets the launcher's buffers at 2×), measured against
//!   design.md's 59–64 MB estimate for the full shell with the launcher
//!   open: the bar alone, then the launcher open, two notifications from
//!   an app as toasts and the OSD up (a `wpctl` volume change) at once,
//!   then the launcher closed. The open figure is held to the estimate's
//!   top, 64 MB, in release.
//!
//! Each prints its figures (`--nocapture`). Skipped, loudly, without sway,
//! grim, dbus-daemon, python-dbusmock, PipeWire or WirePlumber (CI sets
//! `STRAND_REQUIRE_SWAY`, `STRAND_REQUIRE_DBUS` and
//! `STRAND_REQUIRE_PIPEWIRE`).

#[path = "../../strand-services/tests/pipewire/mod.rs"]
mod pipewire;
#[allow(dead_code)]
#[path = "../../strand-services/tests/common/window.rs"]
mod window;

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pipewire::PipeWire;
use strand_services::testing::{DbusMock, PrivateBus};
use window::TestWindow;
use zbus::zvariant::{OwnedValue, Value as ZValue};

/// design.md: "the build fails above 34 MB for the two-monitor bar".
const PSS_GATE_KB: u64 = 34 * 1024;
/// A debug build's code is about three times release's (65 MB against
/// 32 MB here): a ceiling of its own that only catches gross growth; the
/// gate is checked in release (CI).
const DEBUG_PSS_CEILING_KB: u64 = 96 * 1024;
/// design.md's estimate for the full shell with the launcher open:
/// 59–64 MB. Its top is held in release.
const FULL_SHELL_KB: u64 = 64 * 1024;
/// The idle window: no wakeup, no frame.
const IDLE: Duration = Duration::from_secs(10);
/// How long a child command or a D-Bus call of the test's may take.
const STEP_LIMIT: Duration = Duration::from_secs(10);

const UPOWER: &str = "org.freedesktop.UPower";
const LOGIND: &str = "org.freedesktop.login1";
const NM: &str = "org.freedesktop.NetworkManager";
const NM_ROOT: &str = "/org/freedesktop/NetworkManager";

/// What the added component checks: every service the idle clause names
/// at the value the test gave it (one green box; red until then).
const CHECKS: &str = "battery.present && battery.percent > 0.41 && battery.percent < 0.43 \
    && network.connected && network.ssid == \"Home\" \
    && audio.sink.volume > 0.49 && audio.sink.volume < 0.51 \
    && notifications.count == 1 \
    && workspaces.all.count(w => w.focused && w.occupied) == 1 \
    && windows.focused?.title == \"a window\"";

/// The component added to the bar's end section, after `Volume`.
const STATUS: &str = r#"
component Status {
  row { gap: $space.1
    icon network.connected ? "network-wireless-symbolic" : "network-offline-symbolic"
    text network.ssid ?? "" { color: $fg.muted }
    box { width: 10; height: 10; bg: @CHECKS@ ? #00ff00 : #ff0000 }
  }
}
"#;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A directory removed when the test ends (after the processes using it:
/// declared last in [`Desktop`]).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(path: PathBuf) -> TmpDir {
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TmpDir(path)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        if std::env::var_os("STRAND_KEEP_TMP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Runs `cmd` to completion within [`STEP_LIMIT`]; `None` when it cannot
/// start. Panics, killing it, when it takes longer.
fn bounded(cmd: &mut Command, what: &str) -> Option<std::process::Output> {
    use std::io::Read;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let pipe = |r: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut v = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut v);
            }
            let _ = tx.send(v);
        });
        rx
    };
    let out = pipe(child.stdout.take().map(|r| Box::new(r) as _));
    let err = pipe(child.stderr.take().map(|r| Box::new(r) as _));
    let deadline = Instant::now() + STEP_LIMIT;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} took over {STEP_LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let read = |rx: std::sync::mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
    };
    Some(std::process::Output {
        status,
        stdout: read(out),
        stderr: read(err),
    })
}

fn tools(test: &str) -> bool {
    for (tool, tier) in [
        ("sway", "STRAND_REQUIRE_SWAY"),
        ("swaymsg", "STRAND_REQUIRE_SWAY"),
        ("grim", "STRAND_REQUIRE_SWAY"),
        ("pw-dump", "STRAND_REQUIRE_PIPEWIRE"),
    ] {
        if bounded(Command::new(tool).arg("--version"), tool).is_none() {
            assert!(
                std::env::var_os(tier).is_none(),
                "{tool} is not installed but {tier} is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed; {test} did not run ***\n");
            return false;
        }
    }
    true
}

/// The portal's settings: dark.
struct MockPortal;

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl MockPortal {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        portal_value(namespace, key)
    }

    async fn read(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        let v = portal_value(namespace, key)?;
        OwnedValue::try_from(ZValue::new(v)).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn read_all(
        &self,
        namespaces: Vec<String>,
    ) -> HashMap<String, HashMap<String, OwnedValue>> {
        let ns = "org.freedesktop.appearance";
        if !namespaces.is_empty() && !namespaces.iter().any(|n| n == ns) {
            return HashMap::new();
        }
        let dark = OwnedValue::try_from(ZValue::from(1u32)).unwrap();
        HashMap::from([(
            ns.to_string(),
            HashMap::from([("color-scheme".to_string(), dark)]),
        )])
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

fn portal_value(namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
    if namespace == "org.freedesktop.appearance" && key == "color-scheme" {
        return OwnedValue::try_from(ZValue::from(1u32))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()));
    }
    Err(zbus::fdo::Error::Failed("not found".into()))
}

/// The machine around strand: every real backend, a headless sway with
/// two outputs and a window, and `strand run` on `files`. Fields drop in
/// order: strand first, the directories last.
struct Desktop {
    strand: Proc,
    _window: TestWindow,
    _sway: Proc,
    _mocks: Vec<DbusMock>,
    _portal: zbus::Connection,
    conn: zbus::Connection,
    tokio: tokio::runtime::Runtime,
    bus: PrivateBus,
    pw: PipeWire,
    dir: PathBuf,
    display: String,
    log: PathBuf,
    _tmp: TmpDir,
    _home: TmpDir,
}

impl Desktop {
    /// `None` (after saying why) when a tool is missing and its tier is
    /// not required. `data_dirs` is strand's `XDG_DATA_DIRS`.
    fn start(
        name: &str,
        files: &[(&str, String)],
        data_dirs: Option<&str>,
        scale: u32,
    ) -> Option<Desktop> {
        if !tools(name) {
            return None;
        }
        let pw = PipeWire::start(name)?;
        pw.wait_for_defaults();
        let out = bounded(
            pw.command("wpctl")
                .args(["set-volume", "@DEFAULT_AUDIO_SINK@", "0.5"]),
            "wpctl",
        )
        .expect("wpctl runs");
        assert!(out.status.success(), "wpctl set-volume failed");
        let bus = PrivateBus::start()?;
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let conn = tokio.block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .method_timeout(STEP_LIMIT)
                .build()
                .await
                .unwrap()
        });
        let call = |dest: &str, path: &str, iface: &str, method: &str, body: &dyn Body| {
            body.call(&tokio, &conn, dest, path, iface, method)
        };
        // The system daemons.
        let upower = DbusMock::start(&bus, "upower", true, None, UPOWER)?;
        call(
            UPOWER,
            "/org/freedesktop/UPower",
            "org.freedesktop.DBus.Mock",
            "SetupDisplayDevice",
            &(
                2u32, 2u32, 42.0f64, 42.0f64, 100.0f64, 9.0f64, 5400i64, 0i64, true, "", 1u32,
            ),
        );
        let logind = DbusMock::start(&bus, "logind", true, None, LOGIND)?;
        let nm = DbusMock::start(&bus, "networkmanager", true, None, NM)?;
        let nm_call = |method: &str, body: &dyn Body| -> String {
            call(NM, NM_ROOT, "org.freedesktop.DBus.Mock", method, body)
                .body()
                .deserialize::<String>()
                .unwrap_or_default()
        };
        // A Wi-Fi device joined to `Home`.
        let dev = nm_call("AddWiFiDevice", &("mock_WiFi", "wlan0", 100i32));
        let ap = nm_call(
            "AddAccessPoint",
            &(
                dev.as_str(),
                "Mock_AP1",
                "Home",
                "00:23:F8:7E:12:BA",
                2u32,
                2425u32,
                5400u32,
                82u8,
                // NM_802_11_AP_SEC_KEY_MGMT_PSK.
                0x100u32,
            ),
        );
        let saved = nm_call(
            "AddWiFiConnection",
            &(dev.as_str(), "Mock_Con1", "Home", "wpa-psk"),
        );
        nm_call(
            "AddActiveConnection",
            &(
                vec![dev.as_str()],
                saved.as_str(),
                ap.as_str(),
                "Mock_Active1",
                2u32,
            ),
        );
        let portal = tokio.block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .method_timeout(STEP_LIMIT)
                .name("org.freedesktop.portal.Desktop")
                .unwrap()
                .serve_at("/org/freedesktop/portal/desktop", MockPortal)
                .unwrap()
                .build()
                .await
                .unwrap()
        });

        // The runtime directory under /tmp (a short socket path); the
        // config and data outside it (the watcher's ancestor watches
        // would wake for other tests' files there).
        let tmp = TmpDir::new(
            std::env::temp_dir().join(format!("strand-budgets-{name}-{}", std::process::id())),
        );
        let dir = tmp.0.clone();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = TmpDir::new(
            Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("budgets-{name}-{}", std::process::id())),
        );
        let backlight = home.0.join("backlight/intel_backlight");
        std::fs::create_dir_all(&backlight).unwrap();
        for (f, v) in [
            ("max_brightness", "1000"),
            ("brightness", "500"),
            ("actual_brightness", "500"),
            ("type", "raw"),
        ] {
            std::fs::write(backlight.join(f), format!("{v}\n")).unwrap();
        }
        std::fs::create_dir_all(home.0.join("share/applications")).unwrap();
        let config = home.0.join(".config/strand");
        std::fs::create_dir_all(&config).unwrap();
        for (file, text) in files {
            std::fs::write(config.join(file), text).unwrap();
        }

        let (sway, display, ipc) = sway(&dir, scale);
        let window = TestWindow::open(&dir.join(&display), "strand-budgets", "a window");
        let log = dir.join("strand.log");
        let data_dirs = data_dirs.map_or_else(|| home.0.join("share"), PathBuf::from);
        let strand = Proc(
            Command::new(env!("CARGO_BIN_EXE_strand"))
                .arg("run")
                .arg(&config)
                .env("XDG_RUNTIME_DIR", &dir)
                .env("WAYLAND_DISPLAY", &display)
                .env("SWAYSOCK", &ipc)
                .env("PIPEWIRE_RUNTIME_DIR", pw.dir.path())
                .env_remove("PIPEWIRE_REMOTE")
                .env("HOME", &home.0)
                .env("XDG_DATA_DIRS", data_dirs)
                .env("XDG_DATA_HOME", home.0.join("home-share"))
                .env("XDG_CONFIG_HOME", home.0.join(".config"))
                .env("XDG_CACHE_HOME", dir.join("cache"))
                .env("XDG_STATE_HOME", dir.join("state"))
                .env("STRAND_BACKLIGHT_DIR", home.0.join("backlight"))
                .env("STRAND_LOG", "info,damage")
                .envs(bus.env())
                .env_remove("STRAND_MOCK")
                .env_remove("STRAND_SOCKET")
                .stdin(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let desktop = Desktop {
            strand,
            _window: window,
            _sway: sway,
            _mocks: vec![upower, logind, nm],
            _portal: portal,
            conn,
            tokio,
            bus,
            pw,
            dir,
            display,
            log,
            _tmp: tmp,
            _home: home,
        };
        // The shell's tray is the watcher; its notifications server owns
        // the name.
        for name in [
            "org.kde.StatusNotifierWatcher",
            "org.freedesktop.Notifications",
        ] {
            assert!(
                desktop.bus.wait_for_name(name, Duration::from_secs(30)),
                "strand never owned {name}:\n{}",
                desktop.log_text()
            );
        }
        Some(desktop)
    }

    fn pid(&self) -> u32 {
        self.strand.0.id()
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn alive(&mut self, what: &str) {
        let exited = self.strand.0.try_wait().unwrap();
        assert!(
            exited.is_none(),
            "strand exited {what}:\n{}",
            self.log_text()
        );
    }

    /// An app's notification to the shell's server.
    fn notify(&self, summary: &str, body: &str) {
        let hints: HashMap<&str, ZValue> = HashMap::new();
        (
            "budgets",
            0u32,
            "dialog-information",
            summary,
            body,
            Vec::<&str>::new(),
            hints,
            0i32,
        )
            .call(
                &self.tokio,
                &self.conn,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "Notify",
            );
    }

    fn strand_set(&self, field: &str, value: &str) {
        let out = bounded(
            Command::new(env!("CARGO_BIN_EXE_strand"))
                .args(["set", field, value])
                .env_remove("STRAND_SOCKET")
                .env("XDG_RUNTIME_DIR", &self.dir)
                .env("WAYLAND_DISPLAY", &self.display),
            "strand set",
        )
        .expect("strand set runs");
        assert!(
            out.status.success(),
            "strand set {field} {value}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The `strand: damage` lines so far.
    fn frames(&self) -> Vec<String> {
        self.log_text()
            .lines()
            .filter(|l| l.starts_with("strand: damage"))
            .map(String::from)
            .collect()
    }

    /// The surfaces that have drawn a frame so far.
    fn surfaces(&self) -> usize {
        let mut ids: Vec<String> = self
            .frames()
            .iter()
            .filter_map(|l| {
                l.split_whitespace()
                    .find(|w| w.starts_with("surface="))
                    .map(String::from)
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids.len()
    }

    /// Waits (up to 10 s) for a surface beyond `before` to draw.
    fn wait_surfaces(&mut self, before: usize, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.surfaces() <= before {
            self.alive(what);
            assert!(
                Instant::now() < deadline,
                "{what}: no new surface drew\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for `quiet` without a frame (up to `limit`).
    fn wait_quiet(&self, quiet: Duration, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let n = self.frames().len();
            std::thread::sleep(quiet);
            if self.frames().len() == n || Instant::now() >= deadline {
                return;
            }
        }
    }

    /// One output's screenshot as RGB rows.
    fn shot(&self, output: &str) -> Option<Shot> {
        let path = self.dir.join(format!("{output}.ppm"));
        let ok = bounded(
            Command::new("grim")
                .args(["-t", "ppm", "-o", output])
                .arg(&path)
                .env("XDG_RUNTIME_DIR", &self.dir)
                .env("WAYLAND_DISPLAY", &self.display),
            "grim",
        )
        .is_some_and(|o| o.status.success());
        ok.then(|| Shot::ppm(&std::fs::read(&path).unwrap()))
    }
}

/// A D-Bus method body the test sends.
trait Body {
    fn call(
        &self,
        tokio: &tokio::runtime::Runtime,
        conn: &zbus::Connection,
        dest: &str,
        path: &str,
        iface: &str,
        method: &str,
    ) -> zbus::Message;
}

impl<B> Body for B
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    fn call(
        &self,
        tokio: &tokio::runtime::Runtime,
        conn: &zbus::Connection,
        dest: &str,
        path: &str,
        iface: &str,
        method: &str,
    ) -> zbus::Message {
        tokio
            .block_on(conn.call_method(Some(dest), path, Some(iface), method, self))
            .unwrap_or_else(|e| panic!("{dest} {iface}.{method}: {e}"))
    }
}

/// A headless sway in `dir` with `HEADLESS-1` (2560×1440 at `scale`) and
/// `HEADLESS-2` (2560×1440 at 1.25, right of it): the process, its
/// display and its IPC socket.
fn sway(dir: &Path, scale: u32) -> (Proc, String, PathBuf) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        format!(
            "xwayland disable\noutput HEADLESS-1 resolution 2560x1440 position 0 0 scale {scale}\n"
        ),
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
    let (display, ipc) = loop {
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
            let ok = bounded(
                Command::new("swaymsg")
                    .args(["-t", "get_version"])
                    .env("SWAYSOCK", dir.join(i)),
                "swaymsg",
            )
            .is_some_and(|o| o.status.success());
            if ok {
                break (d.clone(), dir.join(i));
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    };
    for args in [
        &["create_output"][..],
        &[
            "output",
            "HEADLESS-2",
            "resolution",
            "2560x1440",
            "position",
            "2560",
            "0",
            "scale",
            "1.25",
        ],
        // The window goes to the first output's workspace.
        &["focus", "output", "HEADLESS-1"],
    ] {
        let out = bounded(
            Command::new("swaymsg").args(args).env("SWAYSOCK", &ipc),
            "swaymsg",
        )
        .expect("swaymsg runs");
        assert!(out.status.success(), "swaymsg {args:?} failed");
    }
    (sway, display, ipc)
}

/// A screenshot as RGB rows.
struct Shot {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Shot {
    fn ppm(bytes: &[u8]) -> Shot {
        // Binary PPM: "P6\n<w> <h>\n255\n" then RGB.
        let mut fields = Vec::new();
        let mut i = 0;
        while fields.len() < 4 {
            while bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let start = i;
            while !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            fields.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
        }
        let (w, h) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
        Shot {
            w,
            h,
            rgb: bytes[i + 1..].to_vec(),
        }
    }

    /// The pixels in the top `rows` that are `color`, within 8 a channel.
    fn count(&self, rows: usize, color: [u8; 3]) -> usize {
        self.rgb
            .chunks_exact(3)
            .take(self.w * rows.min(self.h))
            .filter(|p| (0..3).all(|c| p[c].abs_diff(color[c]) <= 8))
            .count()
    }
}

fn status_of(pid: u32, key: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

fn pss_kb(pid: u32) -> u64 {
    status_of(pid, "Pss:").unwrap_or(0)
}

/// The rollup's split and the ten mappings with the most PSS.
fn memory_report(pid: u32) -> String {
    let mut out: String = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            ["Rss:", "Pss", "AnonHugePages:"]
                .iter()
                .any(|k| l.starts_with(k))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap_or_default();
    let mut by_name: BTreeMap<String, u64> = BTreeMap::new();
    let mut head = String::new();
    for l in smaps.lines() {
        let first = l.split_whitespace().next().unwrap_or("");
        if first.contains('-') && !first.ends_with(':') {
            head = l
                .split_whitespace()
                .nth(5)
                .unwrap_or("[anon]")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
        } else if let Some(v) = l.strip_prefix("Pss:") {
            let kb: u64 = v
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            *by_name.entry(head.clone()).or_default() += kb;
        }
    }
    let mut maps: Vec<(u64, String)> = by_name.into_iter().map(|(n, kb)| (kb, n)).collect();
    maps.sort_by_key(|m| std::cmp::Reverse(m.0));
    for (kb, name) in maps.iter().take(10) {
        out.push_str(&format!("{kb:>7} kB  {name}\n"));
    }
    out
}

/// Context switches per thread of `pid`, by `tid comm`.
fn per_thread(pid: u32) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return out;
    };
    for task in tasks.flatten() {
        let path = task.path();
        let name = format!(
            "{} {}",
            path.file_name().unwrap().to_string_lossy(),
            std::fs::read_to_string(path.join("comm"))
                .unwrap_or_default()
                .trim()
        );
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        let n = status
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum();
        out.insert(name, n);
    }
    out
}

fn switches(pid: u32) -> u64 {
    per_thread(pid).values().sum()
}

fn seconds_into_minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        % 60
}

/// The tests run one at a time: a test's directories removed beside the
/// other's config wake that one's watcher (its ancestors are watched for
/// children going), which the idle window would count.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The limit and its name for the build under test.
fn pss_limit() -> (u64, &'static str) {
    if cfg!(debug_assertions) {
        (DEBUG_PSS_CEILING_KB, "debug ceiling")
    } else {
        (PSS_GATE_KB, "34 MB gate")
    }
}

/// The two-monitor design bar on the real services: within the 34 MB
/// gate, no huge page, and 10 s between minute ticks without a wakeup or
/// a frame.
#[test]
fn the_design_bar_on_the_real_services_keeps_the_budget() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bar = include_str!("../../strand-compiler/tests/fixtures/bar.strand");
    assert!(bar.contains("      Volume\n"));
    let bar = format!(
        "{}{}",
        bar.replace("      Volume\n", "      Volume\n      Status\n"),
        STATUS.replace("@CHECKS@", CHECKS)
    );
    let files = [
        (
            "theme.strand",
            include_str!("../../strand-compiler/tests/fixtures/theme.strand").to_string(),
        ),
        ("bar.strand", bar),
    ];
    let Some(mut desk) = Desktop::start("bar", &files, None, 1) else {
        return;
    };
    desk.notify("budgets", "one kept");
    // Every value on screen: the added box green on both outputs.
    let deadline = Instant::now() + Duration::from_secs(30);
    for output in ["HEADLESS-1", "HEADLESS-2"] {
        loop {
            desk.alive("at boot");
            let ok = desk
                .shot(output)
                .is_some_and(|s| s.count(60, [0, 255, 0]) >= 64 && s.count(60, [255, 0, 0]) == 0);
            if ok {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{output}: the services' values never all showed (the box stayed red)\n{}",
                desk.log_text()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let pid = desk.pid();
    // Boot work done: a whole second without a wakeup or a frame, early
    // enough in the minute that the window below ends before the tick.
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        let (f, w) = (desk.frames().len(), switches(pid));
        std::thread::sleep(Duration::from_secs(1));
        if desk.frames().len() == f && switches(pid) == w {
            if seconds_into_minute() <= 45 {
                break;
            }
            // Past the tick, then settle again.
            std::thread::sleep(Duration::from_secs(63 - seconds_into_minute()));
        }
        desk.alive("settling");
        assert!(
            Instant::now() < deadline,
            "boot never settled: {:?}\n{}",
            per_thread(pid),
            desk.log_text()
        );
    }
    let pss = pss_kb(pid);
    let huge = status_of(pid, "AnonHugePages:").unwrap_or(0);
    let (limit, what) = pss_limit();
    eprintln!(
        "design bar on the real services, two 2560x1440 outputs: PSS {pss} kB ({what} {limit} kB)\n{}",
        memory_report(pid)
    );
    assert_eq!(huge, 0, "huge pages resident\n{}", memory_report(pid));
    assert!(
        pss <= limit,
        "PSS {pss} kB over the {limit} kB {what}\n{}",
        memory_report(pid)
    );
    // Idle: nothing changes, nothing wakes.
    let frames = desk.frames().len();
    let before = per_thread(pid);
    std::thread::sleep(IDLE);
    let after = per_thread(pid);
    let woke: Vec<String> = after
        .iter()
        .filter(|(t, n)| before.get(*t) != Some(n))
        .map(|(t, n)| format!("{t}: {} -> {n}", before.get(t).copied().unwrap_or(0)))
        .collect();
    assert!(
        woke.is_empty(),
        "woke while idle over {IDLE:?}: {woke:?}\n{}",
        desk.log_text()
    );
    assert_eq!(desk.frames().len(), frames, "painted while idle");
    desk.alive("after the idle window");
}

/// The full shell on the real services, measured against design.md's
/// 59–64 MB: the bar, then the launcher open with the machine's apps,
/// two toasts and the OSD up, then the launcher closed.
#[test]
fn the_full_shell_on_the_real_services_is_measured() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = |name: &str| -> (String, String) {
        let text = match name {
            "theme.strand" => include_str!("../../strand-compiler/tests/fixtures/theme.strand"),
            "bar.strand" => include_str!("../../strand-compiler/tests/fixtures/bar.strand"),
            "launcher.strand" => {
                include_str!("../../strand-compiler/tests/fixtures/launcher.strand")
            }
            "toasts.strand" => include_str!("../../strand-compiler/tests/fixtures/toasts.strand"),
            "osd.strand" => include_str!("../../strand-compiler/tests/fixtures/osd.strand"),
            _ => unreachable!(),
        };
        (name.to_string(), text.to_string())
    };
    let files: Vec<(String, String)> = [
        "theme.strand",
        "bar.strand",
        "launcher.strand",
        "toasts.strand",
        "osd.strand",
    ]
    .into_iter()
    .map(fixture)
    .collect();
    let files: Vec<(&str, String)> = files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    // The machine's own apps and icons.
    let data_dirs = "/usr/local/share:/usr/share";
    let apps = std::fs::read_dir("/usr/share/applications")
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "desktop"))
                .count()
        })
        .unwrap_or(0);
    let Some(mut desk) = Desktop::start("full", &files, Some(data_dirs), 2) else {
        return;
    };
    let pid = desk.pid();
    // The bars drawn and the boot work done.
    let deadline = Instant::now() + Duration::from_secs(30);
    while desk.surfaces() < 2 {
        desk.alive("at boot");
        assert!(
            Instant::now() < deadline,
            "the bars never drew\n{}",
            desk.log_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    desk.wait_quiet(Duration::from_secs(1), Duration::from_secs(30));
    let bar = pss_kb(pid);
    // The launcher, on the focused output (HEADLESS-1, scale 2).
    let before = desk.surfaces();
    desk.strand_set("launcher.open", "true");
    desk.wait_surfaces(before, "the launcher");
    desk.wait_quiet(Duration::from_millis(500), Duration::from_secs(10));
    let launcher = pss_kb(pid);
    // Two toasts from an app.
    let before = desk.surfaces();
    desk.notify("Build finished", "strand: all <b>green</b>");
    desk.notify("Meeting", "Stand-up in 5 minutes");
    desk.wait_surfaces(before, "the toasts");
    // The OSD: a volume change made outside the shell.
    let before = desk.surfaces();
    let out = bounded(
        desk.pw
            .command("wpctl")
            .args(["set-volume", "@DEFAULT_AUDIO_SINK@", "0.6"]),
        "wpctl",
    )
    .expect("wpctl runs");
    assert!(out.status.success(), "wpctl set-volume failed");
    desk.wait_surfaces(before, "the OSD");
    // Their enter animations done (the OSD stays up 1.2 s).
    std::thread::sleep(Duration::from_millis(700));
    let full = pss_kb(pid);
    let full_report = memory_report(pid);
    let full_huge = status_of(pid, "AnonHugePages:").unwrap_or(0);
    desk.strand_set("launcher.open", "false");
    std::thread::sleep(Duration::from_secs(2));
    desk.wait_quiet(Duration::from_millis(500), Duration::from_secs(10));
    let closed = pss_kb(pid);
    desk.alive("at the end");
    eprintln!(
        "full shell on the real services ({apps} desktop entries in /usr/share/applications), \
         HEADLESS-1 2560x1440@2 (the launcher's buffers at 2x, as design.md budgets them), \
         HEADLESS-2 2560x1440@1.25:\n\
         \x20 bar alone: {bar} kB\n\
         \x20 launcher open: {launcher} kB\n\
         \x20 launcher open, two toasts, OSD up: {full} kB (design.md: 59-64 MB)\n\
         \x20 launcher closed (toasts up): {closed} kB\n{full_report}"
    );
    assert_eq!(full_huge, 0, "huge pages resident\n{full_report}");
    if !cfg!(debug_assertions) {
        assert!(
            full <= FULL_SHELL_KB,
            "the full shell's PSS {full} kB is over design.md's {FULL_SHELL_KB} kB\n{full_report}"
        );
    }
}

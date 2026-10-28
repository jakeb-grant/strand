//! M3 exit, "memory verified", and the idle budget on the real services
//! (design.md, "Memory budget" and "Testing": the two-monitor bar with no
//! GPU device aims at 42 MB, a warning above it, and the build fails above
//! 48 MB; with the warm device on CI's lavapipe tier 242 / 252 MiB; a
//! running service with nothing changing wakes nothing).
//!
//! `strand run` without `STRAND_MOCK` on a headless sway with two
//! 2560×1440 outputs (scale 1 and 1.25), beside python-dbusmock's UPower
//! (a battery at 42 %), NetworkManager (a Wi-Fi network joined) and
//! logind and a mock portal (dark) on a private `dbus-daemon`; the shell's
//! own notifications server, tray host and (M5) D-Bus front end
//! (`io.github.jakeb_grant.Strand`) there; sway's IPC (workspaces,
//! windows, a window open); a private PipeWire with WirePlumber (audio,
//! the sink at 50 %); a backlight directory (brightness).
//!
//! - `the_design_bar_on_the_real_services_keeps_the_budget`, design.md's
//!   bar (`bar.strand` with `theme.strand`) with one component added to
//!   its end section that reads `network` and `notifications` too (the
//!   bar itself reads workspaces, windows, audio, battery, tray, clock),
//!   on the machine's icon themes (`/usr/share`; CI installs Adwaita):
//!   once every value is on screen, its volume, network and battery
//!   icons drawn, and the boot work is done, PSS stays
//!   within the 252 MiB ceiling (fails above it; above the 242 MiB target
//!   it warns; both hold the warm GPU device, lavapipe on CI's tier:
//!   decisions.md gpu-first) in release (a debug build has its own ceiling),
//!   no page of it is a transparent huge page, over 10 s between two
//!   minute ticks no thread of strand's wakes (or comes or goes) and no
//!   frame is drawn, and the next minute tick wakes it in one burst.
//! - `the_design_bar_with_no_gpu_keeps_the_cold_budget` (gpu-first
//!   review): the same bar where no device can exist (no software
//!   adapter accepted, no render node: the container, CI): nothing Vulkan
//!   mapped, and within 48 MB (a warning above 42 MB) in release.
//! - `the_full_shell_on_the_real_services_is_measured`, the five design
//!   files (bar, launcher, toasts, OSD, theme) with the machine's own
//!   desktop entries and icons (`/usr/share`) and three of the test's,
//!   `HEADLESS-1` at scale 2 (design.md budgets the launcher's buffers
//!   at 2×), measured against design.md's estimate for the full
//!   shell with the launcher open: the bar alone, then the launcher open
//!   (the test's apps' icons found on screen), two notifications from an
//!   app as toasts (on screen), the OSD up too (a `wpctl` volume change;
//!   on screen), then the launcher closed. The larger of the settled
//!   launcher-and-toasts figure and the OSD's aims at the 296 MiB target (a
//!   warning above it), and fails above the 324 MiB ceiling, in release
//!   (with the warm device: decisions.md gpu-first).
//! - `the_full_shell_with_a_desktop_of_apps_is_measured`, the same with
//!   160 more apps (a desktop's worth), half with their own PNG icons of
//!   mixed sizes, half naming icons of the machine's themes.
//! - `the_full_shell_with_no_gpu_keeps_the_cold_budget` (gpu-first
//!   review): the desktop of apps where no device can exist: nothing
//!   Vulkan mapped, aiming at 76 MB and failing above 104 MB in release.
//! - `the_release_binary_code_stays_within_its_gate`: the release
//!   binary's `.text` (most of it resident on a large-folio page cache):
//!   28,311,552 B with the GPU backend (the default build), 24,379,392 B
//!   for the CPU core (`--no-default-features`); both re-based for the
//!   speed profile and since by the measured-plus-reserve rule, last for
//!   services-round's services (decisions.md m5-speed, growth,
//!   services-round; docs/architecture.md,
//!   "`strand-gpu`", "Budgets and tests").
//! - `the_binary_holds_no_fault_code`: no `STRAND_FAULT` point in a
//!   default build (the lock's `faults` feature).
//! - `the_replay_rings_cost_at_most_their_size` (report): a bar on the
//!   same sway, clicked until both rings have wrapped with
//!   `system.replay` off and then again with it on: anonymous PSS
//!   (`Pss_Anon`) on minus off is at most 288 KiB (the rings' 256 KiB
//!   and 32 KiB of slack), and the dumps a report then saves are full.
//! - `an_open_window_costs_what_a_panel_does` (window): on a bar with no
//!   clock, a 640×400 window open costs at most a 640×400 panel's PSS
//!   plus 1 MiB and within the 14 MB a panel's buffers are budgeted,
//!   wakes nothing while it rests, and is let go when closed.
//!
//! The idle window and the minute tick's one burst are timing claims:
//! run this binary on its own (CI runs it as a step of its own, with
//! `--test-threads=1`). A concurrent build or test run on the machine can
//! split the tick's burst with a scheduling gap over 400 ms.
//!
//! Each prints its figures (`--nocapture`). Skipped, loudly, without sway,
//! grim, dbus-daemon, python-dbusmock, PipeWire or WirePlumber (CI sets
//! `STRAND_REQUIRE_SWAY`, `STRAND_REQUIRE_DBUS` and
//! `STRAND_REQUIRE_PIPEWIRE`).

#[path = "../../strand-services/tests/pipewire/mod.rs"]
mod pipewire;
mod support;
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

/// The cold gates (`Gpu::Cold`: no device can exist, nothing Vulkan
/// mapped), design.md's: the two-monitor bar aims at 42 MB (CI warns
/// above it) and the build fails above 48 MB. Re-based from 39 / 44 MB
/// (and the owner-confirmed 34 / 38 MB before that) with headroom for
/// M5's waves 1-2 resident code: the highest run, GitHub's 37,817 kB,
/// plus about 14%, and the ceiling a further 14% (decisions.md
/// m5-open-budgets). Tripwires for accidental growth, never a cap on a
/// user's shell. Kept as they were by gpu-first's review, so the shell's
/// own growth trips them (41,294 kB in the container then).
const COLD_PSS_TARGET_KB: u64 = 42 * 1024;
const COLD_PSS_GATE_KB: u64 = 48 * 1024;
/// A debug build's code is about three times release's (65 MB against
/// 32 MB here): a ceiling of its own that only catches gross growth; the
/// gate is checked in release (CI).
const COLD_DEBUG_PSS_CEILING_KB: u64 = 96 * 1024;
/// The cold full shell with the launcher open: the target (CI warns
/// above it) is 76 MB and the build fails above 104 MB, held in release.
/// Re-based from 67 / 74 MB (and the owner-confirmed 64 / 70 MB before
/// that): the highest run, GitHub's 68,040 kB with 172 entries (run
/// 38130824504, at the old target), plus about 14% for M5's waves 1-2,
/// and the ceiling a further 13% (decisions.md m5-open-budgets); the
/// ceiling then from 86 MB to 104 MB, as GitHub's runners read the same
/// build anywhere from 80,715 to 97,875 kB (its heap alone 12 MB apart)
/// while the container reads it within 3 MB (decisions.md seam-snap).
/// 75,034 kB in the container with a desktop of apps at gpu-first's
/// review.
const COLD_FULL_SHELL_TARGET_KB: u64 = 76 * 1024;
const COLD_FULL_SHELL_KB: u64 = 104 * 1024;
/// (gpu-first review) The warm gates (`Gpu::Warm`, CI's lavapipe tier:
/// the device started with the shell, lavapipe, LLVM and the device's
/// memfd-backed memory about 190 MB of each figure). Each is the
/// container's measurement plus the cold gates' absolute headroom, not a
/// percentage of LLVM, so a regression the size the cold gates catch
/// trips these too: the bar measured 232,089 kB, so a target 5.5 MB over
/// it (232 MiB) and a ceiling 15.7 MB over it (242 MiB); the full shell's
/// larger figure (a desktop of apps) 296,351 kB, so 296 MiB (6.7 MB over)
/// and 324 MiB (35.4 MB over, as 104 MB is over the cold shell for
/// GitHub's variance). decisions.md gpu-first. The bar's re-based by the
/// same rule (decisions.md perf, warm bar) when the warm start began
/// building the pipelines of an animation's shadows, backdrops and
/// crossfade (lavapipe's compiled code: 233,340 and 233,876 kB before,
/// 242,085 and 241,679 kB after in the container; 63,601 → 65,172 kB on
/// the owner's Intel GPU): a target 5.6 MB over 242,085 kB (242 MiB) and
/// a ceiling 16 MB over it (252 MiB).
const PSS_TARGET_KB: u64 = 242 * 1024;
const PSS_GATE_KB: u64 = 252 * 1024;
const FULL_SHELL_TARGET_KB: u64 = 296 * 1024;
const FULL_SHELL_KB: u64 = 324 * 1024;
/// The warm debug bar measured 269,462 kB (the cold one 75,166 kB under
/// its 96 MiB): a ceiling 25 MB over it, the cold one's headroom.
const DEBUG_PSS_CEILING_KB: u64 = 288 * 1024;
/// The environment requires a GPU device (CI's lavapipe tier): the warm
/// device must be up.
fn gpu_required() -> bool {
    cfg!(feature = "gpu") && std::env::var("STRAND_REQUIRE_GPU").as_deref() == Ok("1")
}

/// (gpu-first review) Whether the shell under test can have a GPU device.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Gpu {
    /// As CI's lavapipe tier runs it: the device warm from boot, its
    /// cost in every figure.
    Warm,
    /// No device can exist (no software adapter accepted, no render
    /// node): nothing Vulkan is loaded, and the shell is gated as it was
    /// before gpu-first, so growth in the shell itself is not hidden by
    /// lavapipe's and LLVM's 190 MB.
    Cold,
}

/// A DRM render node is here (`gpu.sh`): a cold leg cannot be cold.
fn render_node() -> bool {
    std::fs::read_dir("/dev/dri").is_ok_and(|d| {
        d.flatten().any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("renderD"))
        })
    })
}

/// A cold shell maps nothing Vulkan, says why there is no device, and
/// never starts one.
fn assert_cold(pid: u32, log: &str) {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
    let mapped: Vec<&str> = maps
        .lines()
        .filter(|l| l.contains("libvulkan") || l.contains("libLLVM"))
        .collect();
    assert!(
        mapped.is_empty(),
        "a Vulkan library is mapped by a shell that can have no GPU: {mapped:?}"
    );
    if cfg!(feature = "gpu") {
        assert!(
            log.contains("GPU: no GPU render node"),
            "the cold shell did not say why it has no device\n{log}"
        );
    }
    assert!(
        !log.contains("GPU: the device is up"),
        "the cold shell started a device\n{log}"
    );
}

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

/// The network component `the_m3_screenshots` adds to the bar's end
/// section: `STATUS` without its check box.
const NETWORK: &str = r#"
component Network {
  row { gap: $space.1
    icon network.connected ? "network-wireless-symbolic" : "network-offline-symbolic"
    text network.ssid ?? "" { color: $fg.muted }
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
    home: TmpDir,
}

impl Desktop {
    /// `None` (after saying why) when a tool is missing and its tier is
    /// not required. `data_dirs` is strand's `XDG_DATA_DIRS`; `apps` the
    /// test's own desktop entries in its `XDG_DATA_HOME` (see
    /// [`write_apps`]).
    fn start(
        name: &str,
        files: &[(&str, String)],
        data_dirs: Option<&str>,
        scale: u32,
        apps: Apps,
        gpu: Gpu,
    ) -> Option<Desktop> {
        if !tools(name) {
            return None;
        }
        if gpu == Gpu::Cold && render_node() {
            eprintln!(
                "{name}: skipped: a GPU render node is here, so no shell is cold (the cold \
                 leg is the container's and CI's)"
            );
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
        // config and data outside it. The watcher watches the ancestors of
        // what it watches for children moved or deleted (below HOME only,
        // decisions.md laptop-decisions; up to the mount root elsewhere),
        // so no ancestor of HOME should be a directory other tests make
        // and remove their own in: not /tmp, nor the target's `tmp`
        // (`CARGO_TARGET_TMPDIR`), which every test binary of the
        // workspace shares, but a directory beside it that only this
        // binary (its tests serialised) uses.
        let tmp = TmpDir::new(
            std::env::temp_dir().join(format!("strand-budgets-{name}-{}", std::process::id())),
        );
        let dir = tmp.0.clone();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let home = TmpDir::new(
            target_tmp
                .parent()
                .unwrap_or(target_tmp)
                .join("strand-budgets")
                .join(format!("{name}-{}", std::process::id())),
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
        // The font and icon directories the cache sources name, made, so
        // that the watcher watches each itself and not, while it is
        // missing, its nearest existing ancestor for creations: HOME
        // would be one, and any name made in it would wake the watcher.
        for d in [
            ".fonts",
            ".icons",
            ".config/fontconfig",
            ".config/gtk-3.0",
            ".config/gtk-4.0",
            "home-share/fonts",
            "home-share/icons",
            "share/fonts",
            "share/icons",
        ] {
            std::fs::create_dir_all(home.0.join(d)).unwrap();
        }
        write_apps(&home.0.join("home-share"), apps);
        let config = home.0.join(".config/strand");
        std::fs::create_dir_all(&config).unwrap();
        for (file, text) in files {
            std::fs::write(config.join(file), text).unwrap();
        }

        let (sway, display, ipc) = sway(&dir, scale);
        let window = TestWindow::open(&dir.join(&display), "strand-budgets", "a window");
        let log = dir.join("strand.log");
        let data_dirs = data_dirs.map_or_else(|| home.0.join("share"), PathBuf::from);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
        if gpu == Gpu::Cold {
            // No software adapter accepted and no render node: no device
            // can exist, so none is warmed or probed.
            cmd.env_remove("STRAND_GPU_SOFTWARE")
                .env_remove("STRAND_REQUIRE_GPU");
        }
        let strand = Proc(
            cmd.arg("run")
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
            home,
        };
        // The shell's tray is the watcher; its notifications server owns
        // the name; (M5) its D-Bus front end owns the shell's own, so
        // every PSS and idle window below is measured with it served.
        for name in [
            "org.kde.StatusNotifierWatcher",
            "org.freedesktop.Notifications",
            strand_core::ipc::DBUS_NAME,
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

    /// The directory trees strand's inotify watches can be in: its HOME
    /// (config, data, backlight), its runtime directory and the system
    /// data directories, each with its ancestors.
    /// What strand itself writes: its log, and its state (persist,
    /// settings, palettes, frecency) and cache directories.
    fn own_writes(&self) -> Vec<PathBuf> {
        vec![
            self.log.clone(),
            self.dir.join("state"),
            self.dir.join("cache"),
        ]
    }

    fn watched_roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.0.clone(),
            self.dir.clone(),
            PathBuf::from("/usr/share"),
            PathBuf::from("/usr/local/share"),
        ]
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
    /// `critical`: urgency 2, a toast that does not expire (toasts.strand
    /// expires the others after 6 s, which a slow debug run can outlast).
    fn notify(&self, summary: &str, body: &str, critical: bool) {
        let mut hints: HashMap<&str, ZValue> = HashMap::new();
        if critical {
            hints.insert("urgency", ZValue::U8(2));
        }
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

    /// `swaymsg` on the desktop's sway; its output.
    fn swaymsg(&self, args: &[&str]) -> String {
        let ipc = std::fs::read_dir(&self.dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("sway-ipc."))
            })
            .expect("sway's IPC socket");
        let out = bounded(
            Command::new("swaymsg").args(args).env("SWAYSOCK", ipc),
            "swaymsg",
        )
        .expect("swaymsg runs");
        assert!(out.status.success(), "swaymsg {args:?} failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A second window, on HEADLESS-1's workspace 3, focus back on
    /// workspace 1 once it is there: the bar's dots then show a focused
    /// and an occupied workspace (sway keeps no empty workspace that is
    /// not shown).
    fn second_workspace(&self) -> TestWindow {
        self.swaymsg(&["workspace", "3"]);
        let window = TestWindow::open(
            &self.dir.join(&self.display),
            "strand-budgets-2",
            "another window",
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self
            .swaymsg(&["-t", "get_tree"])
            .contains("\"strand-budgets-2\"")
        {
            assert!(Instant::now() < deadline, "the second window never mapped");
            std::thread::sleep(Duration::from_millis(20));
        }
        self.swaymsg(&["workspace", "1"]);
        window
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

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// The design bar's end section drew its three icons around the
    /// status box (the green 10 px box in the top 60 rows): the battery
    /// icon right after it, and left of the network name ("Home") the
    /// network icon, then the volume icon. Each is a run of ink (a pixel
    /// 40 off the bar's background 14 rows up) next to its neighbour; a
    /// missing icon leaves its 16 px slot blank, which puts the next run
    /// out of reach.
    fn status_icons(&self) -> Result<(), String> {
        let green = |p: [u8; 3]| p[0] <= 8 && p[1] >= 247 && p[2] <= 8;
        let mut box_x = (usize::MAX, 0);
        let mut box_y = (usize::MAX, 0);
        for y in 0..60.min(self.h) {
            for x in 0..self.w {
                if green(self.px(x, y)) {
                    box_x = (box_x.0.min(x), box_x.1.max(x));
                    box_y = (box_y.0.min(y), box_y.1.max(y));
                }
            }
        }
        if box_x.0 == usize::MAX {
            return Err("no status box".into());
        }
        let cy = (box_y.0 + box_y.1) / 2;
        if cy < 14 || cy + 10 >= self.h {
            return Err(format!("the status box at row {cy}"));
        }
        let ink = |x: usize| {
            let bg = self.px(x, cy - 14);
            (cy - 10..=cy + 10).any(|y| {
                let p = self.px(x, y);
                (0..3).any(|c| p[c].abs_diff(bg[c]) > 40)
            })
        };
        // Runs of ink columns (letters 3 px apart join) from mid-screen.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for x in self.w / 2..self.w {
            if ink(x) {
                match runs.last_mut() {
                    Some(r) if x - r.1 <= 3 => r.1 = x,
                    _ => runs.push((x, x)),
                }
            }
        }
        let at = runs
            .iter()
            .position(|r| r.0 <= box_x.0 && box_x.0 <= r.1)
            .ok_or("the status box is no run")?;
        // An icon's run: 8 to 21 px wide, its near edge within `slot`.
        let icon = |r: Option<&(usize, usize)>, slot: (usize, usize), left: bool, what: &str| {
            match r {
                Some(&(a, b)) if (7..=20).contains(&(b - a)) => {
                    let edge = if left { b } else { a };
                    if (slot.0..=slot.1).contains(&edge) {
                        return Ok(a);
                    }
                }
                _ => {}
            }
            Err(format!(
                "the {what} icon did not draw: no 8-21 px run reaching columns {slot:?} \
                 (found {r:?}; runs {runs:?})"
            ))
        };
        let right = box_x.1;
        icon(runs.get(at + 1), (right + 1, right + 24), false, "battery")?;
        let text = runs.get(at.wrapping_sub(1)).ok_or("no network name")?;
        let net = icon(
            runs.get(at.wrapping_sub(2)),
            (text.0.saturating_sub(12), text.0 - 1),
            true,
            "network",
        )?;
        icon(
            runs.get(at.wrapping_sub(3)),
            (net.saturating_sub(20), net - 1),
            true,
            "volume",
        )?;
        Ok(())
    }

    /// The bands of rows (each at least `tall` rows) with at least
    /// `wide` pixels of `color` (within 24 a channel): one per marked
    /// app's icon in the launcher's list.
    fn bands(&self, color: [u8; 3], wide: usize, tall: usize) -> usize {
        let mut bands = 0;
        let mut run = 0;
        for row in self.rgb.chunks_exact(self.w * 3) {
            let n = row
                .chunks_exact(3)
                .filter(|p| (0..3).all(|c| p[c].abs_diff(color[c]) <= 24))
                .count();
            if n >= wide {
                run += 1;
            } else {
                bands += usize::from(run >= tall);
                run = 0;
            }
        }
        bands + usize::from(run >= tall)
    }

    /// The pixels in `xs` × `ys` that differ from `other`'s.
    fn changed(
        &self,
        other: &Shot,
        xs: std::ops::Range<usize>,
        ys: std::ops::Range<usize>,
    ) -> usize {
        ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
            .filter(|&(x, y)| x < self.w && y < self.h && self.px(x, y) != other.px(x, y))
            .count()
    }

    /// The smallest box (`x0, y0, x1, y1`, exclusive ends) in `xs` × `ys`
    /// holding every pixel that differs from `other`'s.
    fn changed_box(
        &self,
        other: &Shot,
        xs: std::ops::Range<usize>,
        ys: std::ops::Range<usize>,
    ) -> Option<(usize, usize, usize, usize)> {
        let mut b: Option<(usize, usize, usize, usize)> = None;
        for y in ys.start..ys.end.min(self.h) {
            for x in xs.start..xs.end.min(self.w) {
                if self.px(x, y) != other.px(x, y) {
                    b = Some(match b {
                        None => (x, y, x + 1, y + 1),
                        Some((x0, y0, x1, y1)) => {
                            (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1))
                        }
                    });
                }
            }
        }
        b
    }

    /// Saves `x0..x1` × `y0..y1` (grown by `pad`, clamped to the shot) as
    /// a PNG.
    fn save_png(&self, (x0, y0, x1, y1): (usize, usize, usize, usize), pad: usize, path: &Path) {
        let (x0, y0) = (x0.saturating_sub(pad), y0.saturating_sub(pad));
        let (x1, y1) = ((x1 + pad).min(self.w), (y1 + pad).min(self.h));
        let mut img = image::RgbImage::new((x1 - x0) as u32, (y1 - y0) as u32);
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel((x - x0) as u32, (y - y0) as u32, image::Rgb(self.px(x, y)));
            }
        }
        img.save(path)
            .unwrap_or_else(|e| panic!("saving {}: {e}", path.display()));
    }

    /// Where `color` (within 24 a channel, as [`Shot::bands`] reads it)
    /// is on screen: how many pixels, their bounding box and the rows
    /// holding any, and the widest run in one row; what a missing band
    /// prints (nothing drawn, drawn smaller, or drawn elsewhere).
    fn color_report(&self, color: [u8; 3]) -> String {
        let (mut n, mut rows, mut widest) = (0, 0, 0);
        let mut bbox: Option<(usize, usize, usize, usize)> = None;
        for (y, row) in self.rgb.chunks_exact(self.w * 3).enumerate() {
            let mut in_row = 0;
            for (x, p) in row.chunks_exact(3).enumerate() {
                if (0..3).all(|c| p[c].abs_diff(color[c]) <= 24) {
                    in_row += 1;
                    let b = bbox.get_or_insert((x, y, x, y));
                    *b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
                }
            }
            n += in_row;
            rows += usize::from(in_row > 0);
            widest = widest.max(in_row);
        }
        format!(
            "{n} px of {color:?} in {rows} rows (at most {widest} in a row), box {bbox:?} \
             of {}x{}",
            self.w, self.h
        )
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

/// The test's own desktop entries (in strand's `XDG_DATA_HOME`).
#[derive(Clone, Copy, Default)]
struct Apps {
    /// "App NNN": each with its own solid magenta PNG (by path; sizes
    /// 16 to 256 px in turn), first in the launcher's list (by name), so
    /// its rows can be found on screen.
    marked: usize,
    /// "Tool NNN": each naming an icon of the machine's themes (looked
    /// up and decoded as a desktop's apps are).
    themed: usize,
}

/// The magenta the marked apps' icons are.
const MARK: [u8; 3] = [255, 0, 255];

/// Writes `apps` under `share` (`applications/`, `test-icons/`).
fn write_apps(share: &Path, apps: Apps) {
    let entries = share.join("applications");
    let icons = share.join("test-icons");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::create_dir_all(&icons).unwrap();
    let entry = |file: String, name: &str, icon: &str| {
        let text = format!(
            "[Desktop Entry]\nType=Application\nName={name}\nComment=A {name} the budgets test wrote\n\
             Exec=true\nIcon={icon}\nCategories=Utility;\n"
        );
        std::fs::write(entries.join(file), text).unwrap();
    };
    const SIZES: [u32; 6] = [16, 32, 48, 64, 128, 256];
    for i in 0..apps.marked {
        let size = SIZES[i % SIZES.len()];
        let png = icons.join(format!("mark-{i:03}.png"));
        image::RgbaImage::from_pixel(size, size, image::Rgba([MARK[0], MARK[1], MARK[2], 255]))
            .save(&png)
            .unwrap();
        entry(
            format!("strand-test-app-{i:03}.desktop"),
            &format!("App {i:03}"),
            &png.to_string_lossy(),
        );
    }
    let names = theme_icons();
    assert!(
        apps.themed == 0 || !names.is_empty(),
        "no icon in /usr/share/icons/{{hicolor,Adwaita}}"
    );
    for i in 0..apps.themed {
        entry(
            format!("strand-test-tool-{i:03}.desktop"),
            &format!("Tool {i:03}"),
            &names[i % names.len()],
        );
    }
}

/// The icon names of the machine's hicolor and Adwaita themes, sorted.
fn theme_icons() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    let mut stack: Vec<PathBuf> = ["hicolor", "Adwaita"]
        .iter()
        .map(|t| Path::new("/usr/share/icons").join(t))
        .collect();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "png" || x == "svg")
                && let Some(stem) = path.file_stem()
            {
                names.insert(stem.to_string_lossy().into_owned());
            }
        }
    }
    names.into_iter().collect()
}

/// The size of the release binary's `.text` (ELF64, little endian).
fn text_size(elf: &[u8]) -> Option<u64> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(elf.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(elf.get(o..o + 4)?.try_into().ok()?));
    let u64_at = |o: usize| Some(u64::from_le_bytes(elf.get(o..o + 8)?.try_into().ok()?));
    if elf.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let shoff = usize::try_from(u64_at(0x28)?).ok()?;
    let (shentsize, shnum, shstrndx) = (
        usize::from(u16_at(0x3a)?),
        usize::from(u16_at(0x3c)?),
        usize::from(u16_at(0x3e)?),
    );
    let section = |i: usize| shoff + i * shentsize;
    let strings = usize::try_from(u64_at(section(shstrndx) + 0x18)?).ok()?;
    (0..shnum).find_map(|i| {
        let name = strings + usize::try_from(u32_at(section(i))?).ok()?;
        let end = name + elf.get(name..)?.iter().position(|&b| b == 0)?;
        (&elf[name..end] == b".text").then(|| u64_at(section(i) + 0x20))?
    })
}

/// Waits for one whole second without a context switch in any of
/// `pid`'s threads and without a frame (up to `limit`): a burst's
/// settling done, the allocator's trim (500 ms after it) included.
fn settled(desk: &mut Desktop, what: &str, limit: Duration) {
    let pid = desk.pid();
    let deadline = Instant::now() + limit;
    loop {
        let (f, w) = (desk.frames().len(), switches(pid));
        std::thread::sleep(Duration::from_secs(1));
        if desk.frames().len() == f && switches(pid) == w {
            return;
        }
        desk.alive(what);
        assert!(
            Instant::now() < deadline,
            "{what}: never settled: {:?}\n{}",
            per_thread(pid),
            desk.log_text()
        );
    }
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

fn seconds_since_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn seconds_into_minute() -> u64 {
    seconds_since_epoch() % 60
}

/// Every inotify watch `pid` holds, from `/proc/<pid>/fdinfo`: the
/// watched directory's inode and the watch's mask (all its instances').
fn inotify_watches(pid: u32) -> HashMap<u64, u32> {
    let mut out = HashMap::new();
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
        return out;
    };
    for fd in fds.flatten() {
        let text = std::fs::read_to_string(fd.path()).unwrap_or_default();
        for line in text.lines().filter(|l| l.starts_with("inotify ")) {
            let field = |k: &str| line.split_whitespace().find_map(|w| w.strip_prefix(k));
            let (Some(ino), Some(mask)) = (field("ino:"), field("mask:")) else {
                continue;
            };
            if let (Ok(ino), Ok(mask)) =
                (u64::from_str_radix(ino, 16), u32::from_str_radix(mask, 16))
            {
                *out.entry(ino).or_default() |= mask;
            }
        }
    }
    out
}

/// Sees what strand's watches see: each directory strand watches (found
/// by inode among `roots`, their ancestors and the directories below
/// them) watched here with strand's own mask. A wake of strand's
/// `strand-watch` thread in the idle window is then told apart: an event
/// here means something outside the shell changed a watched directory
/// (another process making or removing a name beside an ancestor of the
/// config, which the watcher watches so that an ancestor moved or
/// deleted is heard), not that the shell woke on its own.
struct DirMonitor {
    fd: rustix::fd::OwnedFd,
    dirs: HashMap<i32, PathBuf>,
    /// strand's watches no candidate directory had the inode of.
    unmatched: usize,
}

impl DirMonitor {
    fn new(pid: u32, roots: &[PathBuf]) -> DirMonitor {
        use rustix::fs::inotify;
        use std::os::unix::fs::MetadataExt;
        let watches = inotify_watches(pid);
        let mut candidates: Vec<PathBuf> = Vec::new();
        for root in roots {
            candidates.extend(root.ancestors().map(Path::to_path_buf));
            let mut stack = vec![(root.clone(), 0)];
            while let Some((d, depth)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in rd.flatten() {
                    if depth < 4 && e.file_type().is_ok_and(|t| t.is_dir()) {
                        candidates.push(e.path());
                        stack.push((e.path(), depth + 1));
                    }
                }
            }
        }
        let fd =
            inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC).unwrap();
        let mut dirs = HashMap::new();
        let mut found = std::collections::HashSet::new();
        for d in candidates {
            let Ok(meta) = std::fs::metadata(&d) else {
                continue;
            };
            let Some(&mask) = watches.get(&meta.ino()) else {
                continue;
            };
            if !found.insert(meta.ino()) {
                continue;
            }
            if let Ok(wd) = inotify::add_watch(&fd, &d, inotify::WatchFlags::from_bits_retain(mask))
            {
                dirs.insert(wd, d);
            }
        }
        DirMonitor {
            fd,
            dirs,
            unmatched: watches.len() - found.len(),
        }
    }

    /// The events queued since the last call, as `dir/name FLAGS`.
    fn events(&mut self) -> Vec<String> {
        let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
        let mut reader = rustix::fs::inotify::Reader::new(&self.fd, &mut buf);
        let mut out = Vec::new();
        while let Ok(e) = reader.next() {
            let dir = self
                .dirs
                .get(&e.wd())
                .map_or_else(|| format!("wd {}", e.wd()), |d| d.display().to_string());
            let name = e
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(format!("{dir}/{name} {:?}", e.events()));
        }
        out
    }

    fn describe(&self) -> String {
        format!(
            "{} of strand's watched directories mirrored, {} not found under the roots",
            self.dirs.len(),
            self.unmatched
        )
    }
}

/// Whether a [`DirMonitor::events`] line (`dir/name FLAGS`) names `path`
/// or a name under it.
fn event_under(event: &str, path: &Path) -> bool {
    let p = path.display().to_string();
    event
        .strip_prefix(&p)
        .is_some_and(|rest| rest.starts_with('/') || rest.starts_with(' '))
}

#[test]
fn an_event_is_under_its_own_path_only() {
    let state = Path::new("/run/t/state");
    assert!(event_under("/run/t/state/strand/persist/x CREATE", state));
    assert!(event_under("/run/t/state CREATE | ISDIR", state));
    assert!(!event_under("/run/t/statement CREATE", state));
    assert!(!event_under("/run/t/cache/x CREATE", state));
}

/// A whole second without a wakeup or a frame, early enough in the
/// minute (at most 45 s into it) that a 10 s window after it ends before
/// the next minute tick.
fn settle_early_in_minute(desk: &mut Desktop, what: &str) {
    let pid = desk.pid();
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
        desk.alive(what);
        assert!(
            Instant::now() < deadline,
            "{what}: never settled: {:?}\n{}",
            per_thread(pid),
            desk.log_text()
        );
    }
}

/// The tests run one at a time: a test's directories removed beside the
/// other's config wake that one's watcher (its ancestors are watched for
/// children going), which the idle window would count.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A figure CI shows on the run's page (a workflow `notice` annotation),
/// so the measured numbers are read without the job's log.
fn notice(text: &str) {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        // On a line of its own: the harness has printed `test name ... `.
        println!(
            "\n::notice title=M3 memory ({})::{text}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
    }
}

/// The limit and its name for the build under test.
fn pss_limit(gpu: Gpu) -> (u64, &'static str) {
    match (gpu, cfg!(debug_assertions)) {
        (Gpu::Warm, true) => (DEBUG_PSS_CEILING_KB, "debug ceiling"),
        (Gpu::Warm, false) => (PSS_GATE_KB, "warm ceiling"),
        (Gpu::Cold, true) => (COLD_DEBUG_PSS_CEILING_KB, "cold debug ceiling"),
        (Gpu::Cold, false) => (COLD_PSS_GATE_KB, "cold 48 MB ceiling"),
    }
}

/// A release measurement above its design.md target: a warning (a CI
/// annotation), not a failure; the ceiling is what fails the build.
fn warn_over_target(what: &str, pss: u64, target: u64) {
    if !cfg!(debug_assertions) && pss > target {
        eprintln!("{what}: PSS {pss} kB is over the {target} kB target");
        if std::env::var_os("GITHUB_ACTIONS").is_some() {
            println!(
                "\n::warning title=M3 memory over target::{what}: PSS {pss} kB is over the {target} kB target (the build fails above the ceiling)"
            );
        }
    }
}

/// The two-monitor design bar on the real services: within the 252 MiB
/// ceiling (a warning above the 242 MiB target; the warm device's), no huge page, and 10 s between minute ticks without a wakeup or
/// a frame.
#[test]
fn the_design_bar_on_the_real_services_keeps_the_budget() {
    design_bar(Gpu::Warm);
}

/// (gpu-first review) The same bar where no device can exist: nothing
/// Vulkan mapped, within the 48 MB ceiling (a warning above 42 MB) in
/// release, as before gpu-first. Its idle window is the warm bar's.
#[test]
fn the_design_bar_with_no_gpu_keeps_the_cold_budget() {
    design_bar(Gpu::Cold);
}

fn design_bar(gpu: Gpu) {
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
    // The machine's icon themes (CI installs Adwaita): the bar's volume,
    // network and battery icons are looked up, their theme indexed and
    // their SVGs decoded, as on a desktop.
    let data_dirs = "/usr/local/share:/usr/share";
    let name = match gpu {
        Gpu::Warm => "bar",
        Gpu::Cold => "bar-cold",
    };
    let Some(mut desk) = Desktop::start(name, &files, Some(data_dirs), 1, Apps::default(), gpu)
    else {
        return;
    };
    desk.notify("budgets", "one kept", false);
    // Every value on screen: the added box green on both outputs.
    let deadline = Instant::now() + Duration::from_secs(30);
    for output in ["HEADLESS-1", "HEADLESS-2"] {
        loop {
            desk.alive("at boot");
            let shot = desk.shot(output);
            let ok = shot
                .as_ref()
                .is_some_and(|s| s.count(60, [0, 255, 0]) >= 64 && s.count(60, [255, 0, 0]) == 0);
            if ok {
                // And the icons drew beside it.
                if let Some(Err(e)) = shot.as_ref().map(Shot::status_icons) {
                    assert!(
                        Instant::now() < deadline,
                        "{output}: {e}\n{}",
                        desk.log_text()
                    );
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
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
    // The minute tick is watched in release only (a debug build's tick
    // burst has gaps of its own); so is the first tick waited for.
    let watch_tick = !cfg!(debug_assertions);
    // One minute tick past first: the first tick after boot can commit
    // fresh memory (a new heap segment for a thread), which the
    // allocator's trim answers once, half a second on; the tick watched
    // below is a steady one.
    let booted = seconds_since_epoch() / 60;
    while watch_tick && seconds_since_epoch() / 60 == booted {
        desk.alive("waiting for the first minute tick");
        std::thread::sleep(Duration::from_millis(500));
    }
    // The idle window's mirror of strand's inotify watches (below) is
    // built first: it walks HOME and /usr/share, which can take seconds
    // on a loaded runner, and between the settle and the window it once
    // pushed the window over the minute tick (CI run 37764216530).
    let mut monitor = DirMonitor::new(pid, &desk.watched_roots());
    eprintln!("idle monitor: {}", monitor.describe());
    // Boot work done: a whole second without a wakeup or a frame, early
    // enough in the minute that the window below ends before the tick.
    settle_early_in_minute(&mut desk, "settling");
    let pss = pss_kb(pid);
    let huge = status_of(pid, "AnonHugePages:").unwrap_or(0);
    let (limit, what) = pss_limit(gpu);
    eprintln!(
        "design bar on the real services ({gpu:?} GPU), two 2560x1440 outputs: PSS {pss} kB \
         ({what} {limit} kB)\n{}",
        memory_report(pid)
    );
    notice(&format!(
        "design bar on the real services ({gpu:?} GPU): PSS {pss} kB ({what} {limit} kB)"
    ));
    assert_eq!(huge, 0, "huge pages resident\n{}", memory_report(pid));
    if gpu == Gpu::Cold {
        assert_cold(pid, &desk.log_text());
        warn_over_target("design bar with no GPU", pss, COLD_PSS_TARGET_KB);
        assert!(
            pss <= limit,
            "PSS {pss} kB over the {limit} kB {what}\n{}",
            memory_report(pid)
        );
        return;
    }
    // (gpu-first) The device is warm wherever one can start: started
    // with the shell, kept for its first animation, and counted in the
    // PSS above (lavapipe and LLVM on CI's tier; docs/architecture.md,
    // "`strand-gpu`", "Budgets and tests"; decisions.md gpu-first).
    if gpu_required() {
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default();
        assert!(
            maps.contains("libvulkan") && desk.log_text().contains("GPU: the device is up"),
            "STRAND_REQUIRE_GPU is set, but the bar has no warm device\n{}",
            desk.log_text()
        );
    }
    warn_over_target("design bar on the real services", pss, PSS_TARGET_KB);
    assert!(
        pss <= limit,
        "PSS {pss} kB over the {limit} kB {what}\n{}",
        memory_report(pid)
    );
    // Idle: nothing changes, nothing wakes. A wake of the watcher thread
    // alone that a change in one of its watched directories explains
    // (another process making or removing a name beside an ancestor of
    // the config) is the environment, not the shell: the window is
    // tried again, at most twice, after settling again.
    // The PSS readout above takes time too: a window that would start
    // more than 45 s into the minute settles again first, so it never
    // covers the minute tick.
    for attempt in 1..=3 {
        if attempt > 1 || seconds_into_minute() > 45 {
            settle_early_in_minute(&mut desk, "settling again");
        }
        let _ = monitor.events();
        let frames = desk.frames().len();
        let before = per_thread(pid);
        std::thread::sleep(IDLE);
        let after = per_thread(pid);
        let seen = monitor.events();
        let woke: Vec<&String> = after
            .keys()
            .filter(|t| before.get(*t) != after.get(*t))
            .collect();
        let watcher_only = woke.iter().all(|t| {
            t.split_once(' ')
                .is_some_and(|(_, comm)| comm == "strand-watch")
        });
        let shown: Vec<String> = woke
            .iter()
            .map(|t| {
                format!(
                    "{t}: {} -> {}",
                    before.get(*t).copied().unwrap_or(0),
                    after[*t]
                )
            })
            .collect();
        // Only a change strand did not make explains the wake: one of
        // its own writes (its log, state, settings or cache files) is the
        // shell waking itself, and is shown, not tried again.
        let own = desk.own_writes();
        let (mine, outside): (Vec<&String>, Vec<&String>) = seen
            .iter()
            .partition(|e| own.iter().any(|p| event_under(e, p)));
        if !mine.is_empty() {
            eprintln!("idle window {attempt}: events in strand's own files: {mine:?}");
            notice(&format!(
                "idle window {attempt}: events in strand's own files: {mine:?}"
            ));
        }
        if !woke.is_empty() && watcher_only && mine.is_empty() && !outside.is_empty() && attempt < 3
        {
            eprintln!(
                "idle window {attempt}: the watcher woke on a change outside the shell, \
                 trying again: {shown:?}, events {seen:?}"
            );
            notice(&format!(
                "idle window {attempt}: the watcher woke on a change outside the shell: {seen:?}"
            ));
            continue;
        }
        assert!(
            woke.is_empty(),
            "woke while idle over {IDLE:?}: {shown:?}\nevents in strand's watched \
             directories: {seen:?} ({})\n{}",
            monitor.describe(),
            desk.log_text()
        );
        // No thread came or went either (one that ended inside the window
        // ran inside it).
        assert!(
            before.keys().eq(after.keys()),
            "threads changed while idle over {IDLE:?}: {:?} -> {:?}",
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>()
        );
        assert_eq!(desk.frames().len(), frames, "painted while idle");
        break;
    }
    desk.alive("after the idle window");
    // The retry's premise, shown: a directory made and removed in
    // `~/.config`, the config's parent, which the watcher watches for
    // names, wakes the watcher thread, and the monitor sees it.
    let beside = desk
        .home
        .0
        .join(format!(".config/beside-{}", std::process::id()));
    let _ = monitor.events();
    let before = per_thread(pid);
    std::fs::create_dir(&beside).unwrap();
    std::fs::remove_dir(&beside).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let after = per_thread(pid);
    let seen = monitor.events();
    assert!(
        seen.iter().any(|e| e.contains("/beside-")),
        "the monitor missed a name removed in ~/.config: {seen:?} ({})",
        monitor.describe()
    );
    assert!(
        after
            .iter()
            .any(|(t, n)| t.ends_with(" strand-watch") && before.get(t) != Some(n)),
        "the watcher slept through a name removed in ~/.config: {before:?} -> {after:?}"
    );
    // Ancestor watches stop below HOME (decisions.md, laptop-decisions):
    // a name made beside HOME, in its parent, is in no directory strand
    // watches, so the mirror hears nothing.
    let above = desk
        .home
        .0
        .with_file_name(format!("beside-{}", std::process::id()));
    let _ = monitor.events();
    std::fs::create_dir(&above).unwrap();
    std::fs::remove_dir(&above).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let seen = monitor.events();
    assert!(
        !seen.iter().any(|e| e.contains("/beside-")),
        "strand watches HOME's parent: {seen:?} ({})",
        monitor.describe()
    );
    settled(
        &mut desk,
        "after the watcher's control",
        Duration::from_secs(30),
    );
    if !watch_tick {
        return;
    }
    // The minute tick: design.md's "wakes once a minute" is one burst
    // (the clock's timer, its frame), with no second wake after it (the
    // allocator's delayed trim arms only on a structural diff,
    // `run::structural`, which a tick's prop set is not, and comes 500 ms
    // after the burst's last wake: a gap over 400 ms; the tick trims
    // inline, inside its own wakes).
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let until = Instant::now()
        + Duration::from_millis(u64::try_from(60_000 - millis % 60_000).unwrap() + 3_000);
    let mut last = per_thread(pid);
    let mut wakes: Vec<(Instant, Vec<String>)> = Vec::new();
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
        let now = per_thread(pid);
        let woke: Vec<String> = now
            .iter()
            .filter(|(t, n)| last.get(*t) != Some(n))
            .map(|(t, _)| t.clone())
            .collect();
        if !woke.is_empty() {
            wakes.push((Instant::now(), woke));
        }
        last = now;
    }
    let bursts = wakes
        .windows(2)
        .filter(|w| w[1].0 - w[0].0 > Duration::from_millis(400))
        .count()
        + usize::from(!wakes.is_empty());
    let t0 = wakes.first().map(|w| w.0);
    let shown: Vec<String> = wakes
        .iter()
        .map(|(t, w)| format!("+{:?} {w:?}", t0.map(|t0| *t - t0).unwrap_or_default()))
        .collect();
    assert_eq!(
        bursts, 1,
        "the minute tick woke strand in {bursts} bursts, not one: {shown:#?}"
    );
    desk.alive("after the minute tick");
}

/// The full shell on the real services with the machine's own apps
/// (and three marked ones the test finds on screen).
#[test]
fn the_full_shell_on_the_real_services_is_measured() {
    full_shell(
        "full",
        Apps {
            marked: 3,
            themed: 0,
        },
        None,
        Gpu::Warm,
    );
}

/// `scripts/m3-shots.sh`'s screenshots: the full shell on the
/// real services with only the machine's apps, every check of
/// `full_shell` made, and HEADLESS-1 saved at each step into
/// `$STRAND_SHOTS_DIR`: the bar, the launcher, the toasts, the OSD and
/// the whole output with all four up.
#[test]
#[ignore = "writes the full shell's screenshots: scripts/m3-shots.sh"]
fn the_m3_screenshots() {
    let dir = PathBuf::from(
        std::env::var_os("STRAND_SHOTS_DIR").expect("STRAND_SHOTS_DIR names where the shots go"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    full_shell("shots", Apps::default(), Some(&dir), Gpu::Warm);
}

/// The same with a desktop's worth of apps: 160 more, half with their
/// own PNG icons of mixed sizes and half naming icons of the machine's
/// themes.
#[test]
fn the_full_shell_with_a_desktop_of_apps_is_measured() {
    full_shell(
        "apps",
        Apps {
            marked: 80,
            themed: 80,
        },
        None,
        Gpu::Warm,
    );
}

/// (gpu-first review) The desktop of apps where no device can exist:
/// nothing Vulkan mapped, its larger figure aiming at the 76 MB target
/// and failing above the 104 MB ceiling in release, as before gpu-first.
#[test]
fn the_full_shell_with_no_gpu_keeps_the_cold_budget() {
    full_shell(
        "apps-cold",
        Apps {
            marked: 80,
            themed: 80,
        },
        None,
        Gpu::Cold,
    );
}

/// The full shell on the real services, measured against design.md's
/// 59–64 MB: the bar, then the launcher open (its marked apps' icons on
/// screen), two toasts (on screen), then the OSD up too (on screen),
/// then the launcher closed. Each figure but the OSD's is read once the
/// shell has settled (a whole second without a switch or a frame, the
/// allocator's trim done); the OSD is up 1.2 s, so its figure is read
/// 0.7 s after its surface draws.
fn full_shell(name: &str, apps: Apps, shots: Option<&Path>, gpu: Gpu) {
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
    .map(|(n, t)| {
        // design.md's bar shows no network; the screenshots add the
        // network icon and name after the volume (`NETWORK`).
        if shots.is_some() && n == "bar.strand" {
            assert!(t.contains("      Volume\n"));
            let t = t.replace("      Volume\n", "      Volume\n      Network\n");
            (n, format!("{t}{NETWORK}"))
        } else {
            (n, t)
        }
    })
    .collect();
    let files: Vec<(&str, String)> = files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    // The machine's own apps and icons, and the test's.
    let data_dirs = "/usr/local/share:/usr/share";
    let machine = std::fs::read_dir("/usr/share/applications")
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "desktop"))
                .count()
        })
        .unwrap_or(0);
    let entries = machine + apps.marked + apps.themed;
    let Some(mut desk) = Desktop::start(name, &files, Some(data_dirs), 2, apps, gpu) else {
        return;
    };
    let pid = desk.pid();
    let limit = Duration::from_secs(60);
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
    // The screenshots' bar shows more than one workspace (the measured
    // runs keep the one).
    let _second = shots.map(|_| desk.second_workspace());
    settled(&mut desk, "the bars", limit);
    let bar = pss_kb(pid);
    let shot = |desk: &Desktop, what: &str| {
        desk.shot("HEADLESS-1")
            .unwrap_or_else(|| panic!("{what}: no screenshot of HEADLESS-1"))
    };
    let before_launcher = shot(&desk, "the bars");
    // The bar's band on HEADLESS-1 (36 px and its 8 px margin, at 2x).
    let keep = |s: &Shot, area: Option<(usize, usize, usize, usize)>, file: &str| {
        if let Some(dir) = shots {
            let area = area.unwrap_or_else(|| panic!("{file}: nothing changed to keep"));
            s.save_png(area, 24, &dir.join(file));
        }
    };
    let bar_rows = 104;
    keep(
        &before_launcher,
        // Down to the bar's bottom edge (88 rows with the pad).
        Some((0, 0, before_launcher.w, 64)),
        "m3-bar.png",
    );
    // The launcher, on the focused output (HEADLESS-1, scale 2), listing
    // the apps: the marked ones first (by name), one 64 px magenta icon
    // (32 at 2x) a row.
    let before = desk.surfaces();
    desk.strand_set("launcher.open", "true");
    desk.wait_surfaces(before, "the launcher");
    // The icons are looked up and decoded off the frame path, so a quiet
    // second can pass before they draw (CI run 37895260781: 1.08 s after
    // it). Wait for them, bounded, so the launcher is measured with its
    // icons; the check below still names the cause if they never come.
    let icons_by = Instant::now() + Duration::from_secs(10);
    while Instant::now() < icons_by
        && shot(&desk, "the launcher").bands(MARK, 48, 48) < apps.marked.min(3)
    {
        desk.alive("waiting for the launcher's icons");
        std::thread::sleep(Duration::from_millis(100));
    }
    settled(&mut desk, "the launcher", limit);
    let launcher = pss_kb(pid);
    let open = shot(&desk, "the launcher");
    let rows = open.bands(MARK, 48, 48);
    if rows < apps.marked.min(3) {
        // CI runs 37796294304 and 37895260781 failed here (icons late, the
        // second showed; the wait above is the fix): name the cause.
        // What is on screen now, then whether the icons come late (a
        // screenshot every 250 ms for 5 s, and the frames meanwhile) or
        // never.
        let now = open.color_report(MARK);
        let frames = desk.frames().len();
        let start = Instant::now();
        let mut late = None;
        while start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(250));
            let s = shot(&desk, "the launcher, again");
            if s.bands(MARK, 48, 48) >= apps.marked.min(3) {
                late = Some(start.elapsed());
                break;
            }
        }
        let after = desk.frames();
        let verdict = match late {
            Some(t) => format!("the icons drew {t:?} after the settle second"),
            None => format!(
                "no icons 5 s later either: {}",
                shot(&desk, "the launcher, at last").color_report(MARK)
            ),
        };
        panic!(
            "the launcher shows {rows} rows with a marked app's icon, not {}: {now}; {verdict}; \
             {} frames meanwhile: {:?}\n{}",
            apps.marked.min(3),
            after.len() - frames,
            &after[frames..],
            desk.log_text()
        );
    }
    assert_eq!(
        before_launcher.bands(MARK, 48, 48),
        0,
        "marked icons before the launcher opened"
    );
    keep(
        &open,
        open.changed_box(&before_launcher, 0..open.w, bar_rows..open.h),
        "m3-launcher.png",
    );
    // Two toasts from an app, top right of HEADLESS-1 (380 wide at 2x,
    // under the bar).
    let before = desk.surfaces();
    desk.notify("Build finished", "strand: all <b>green</b>", true);
    desk.notify("Meeting", "Stand-up in 5 minutes", true);
    desk.wait_surfaces(before, "the toasts");
    settled(&mut desk, "the toasts", limit);
    let toasts = pss_kb(pid);
    let toasted = shot(&desk, "the toasts");
    let w = toasted.w;
    let top_right = toasted.changed(&open, w - 780..w, 120..600);
    assert!(
        top_right >= 40_000,
        "the toasts changed {top_right} pixels top right of HEADLESS-1\n{}",
        desk.log_text()
    );
    keep(
        &toasted,
        toasted.changed_box(&open, w - 780..w, bar_rows..600),
        "m3-toasts.png",
    );
    // The OSD: volume changes made outside the shell, one every 400 ms
    // until its surface draws (each keeps it up 1.2 s longer; a slow
    // debug run can hide it again before its first frame).
    let before = desk.surfaces();
    let deadline = Instant::now() + Duration::from_secs(15);
    for step in 0.. {
        let volume = if step % 2 == 0 { "0.6" } else { "0.62" };
        let out = bounded(
            desk.pw
                .command("wpctl")
                .args(["set-volume", "@DEFAULT_AUDIO_SINK@", volume]),
            "wpctl",
        )
        .expect("wpctl runs");
        assert!(out.status.success(), "wpctl set-volume failed");
        let sent = Instant::now();
        while desk.surfaces() <= before && sent.elapsed() < Duration::from_millis(400) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if desk.surfaces() > before {
            break;
        }
        desk.alive("raising the OSD");
        assert!(
            Instant::now() < deadline,
            "the OSD never drew\n{}",
            desk.log_text()
        );
    }
    // On screen bottom centre of HEADLESS-1 (260 wide at 2x, 96 up):
    // shots until it shows, a change before each keeping it up 1.2 s
    // more (a slow debug run puts it on screen half a second after its
    // first frame).
    let deadline = Instant::now() + Duration::from_secs(10);
    let (w, h) = (toasted.w, toasted.h);
    for step in 0.. {
        let volume = if step % 2 == 0 { "0.64" } else { "0.66" };
        let out = bounded(
            desk.pw
                .command("wpctl")
                .args(["set-volume", "@DEFAULT_AUDIO_SINK@", volume]),
            "wpctl",
        )
        .expect("wpctl runs");
        assert!(out.status.success(), "wpctl set-volume failed");
        std::thread::sleep(Duration::from_millis(200));
        let shot = shot(&desk, "the OSD");
        let bottom = shot.changed(&toasted, w / 2 - 300..w / 2 + 300, h - 360..h - 160);
        if bottom >= 15_000 {
            if shots.is_some() {
                // Its enter pose done, then the OSD and the whole output.
                std::thread::sleep(Duration::from_millis(300));
                let up = desk.shot("HEADLESS-1").expect("a screenshot of the OSD");
                keep(
                    &up,
                    up.changed_box(&toasted, w / 2 - 300..w / 2 + 300, h - 360..h - 160),
                    "m3-osd.png",
                );
                keep(&up, Some((0, 0, up.w, up.h)), "m3-shell.png");
            }
            break;
        }
        desk.alive("the OSD on screen");
        assert!(
            Instant::now() < deadline,
            "the OSD changed {bottom} pixels bottom centre of HEADLESS-1\n{}",
            desk.log_text()
        );
    }
    // Its enter animation done (it stays up 1.2 s past the last change).
    std::thread::sleep(Duration::from_millis(300));
    let full = pss_kb(pid);
    let full_report = memory_report(pid);
    let full_huge = status_of(pid, "AnonHugePages:").unwrap_or(0);
    desk.strand_set("launcher.open", "false");
    std::thread::sleep(Duration::from_secs(2));
    settled(&mut desk, "the launcher closed", limit);
    let closed = pss_kb(pid);
    desk.alive("at the end");
    // (gpu-first) The device started with the shell and is kept: the
    // launcher's opening and the toasts' entrances draw on it, and its
    // cost is in every figure above.
    let log = desk.log_text();
    if gpu == Gpu::Cold {
        assert_cold(pid, &log);
    } else if gpu_required() {
        assert!(
            log.contains("GPU: the device is up"),
            "STRAND_REQUIRE_GPU is set, but the shell has no warm device\n{log}"
        );
    }
    assert!(
        !log.contains("GPU: the thread ended"),
        "the warm device was dropped\n{log}"
    );
    // The launcher closed is gated too (it was only printed).
    let peak = full.max(toasts).max(closed);
    eprintln!(
        "full shell on the real services ({gpu:?} GPU; {entries} desktop entries: {machine} in \
         /usr/share/applications, {} the test's), \
         HEADLESS-1 2560x1440@2 (the launcher's buffers at 2x, as design.md budgets them), \
         HEADLESS-2 2560x1440@1.25:\n\
         \x20 bar alone: {bar} kB\n\
         \x20 launcher open: {launcher} kB\n\
         \x20 launcher open, two toasts: {toasts} kB\n\
         \x20 launcher open, two toasts, OSD up: {full} kB (design.md: 59-64 MB)\n\
         \x20 launcher closed (toasts up): {closed} kB\n{full_report}",
        apps.marked + apps.themed
    );
    notice(&format!(
        "full shell on the real services ({gpu:?} GPU; {entries} desktop entries, {machine} the machine's): \
         bar {bar} kB, launcher open {launcher} kB, with two toasts {toasts} kB, \
         and the OSD {full} kB, launcher closed {closed} kB (design.md: 59-64 MB)"
    ));
    assert_eq!(full_huge, 0, "huge pages resident\n{full_report}");
    let (target, ceiling) = match gpu {
        Gpu::Warm => (FULL_SHELL_TARGET_KB, FULL_SHELL_KB),
        Gpu::Cold => (COLD_FULL_SHELL_TARGET_KB, COLD_FULL_SHELL_KB),
    };
    warn_over_target("full shell", peak, target);
    if !cfg!(debug_assertions) {
        assert!(
            peak <= ceiling,
            "the full shell's PSS {peak} kB ({gpu:?} GPU) is over design.md's {ceiling} kB \
             ceiling\n{full_report}"
        );
    }
}

/// The release profile's per-package size opt-levels the `.text` and
/// memory budgets rest on (decisions.md, wave4-exitMemory: "the budget
/// rests on the workspace's release profile"; m5-speed-profile moved
/// every crate a person waits on back to opt 3 and kept these, on code
/// no one waits on). Checked in every build, debug too, so a cleanup of
/// `Cargo.toml` fails `cargo test --workspace` locally rather than only
/// the release budget in CI.
const RELEASE_OPT_LEVELS: &[(&str, &str)] = &[
    ("strand-services", "z"),
    ("zbus", "z"),
    ("zvariant", "z"),
    ("zbus_names", "z"),
    ("tokio", "z"),
    ("swayipc-types", "z"),
    ("logind-zbus", "z"),
    ("strand-icons", "s"),
    ("strand-introspect", "z"),
    ("toml_edit", "s"),
    ("toml_parser", "s"),
    ("fontique", "s"),
    ("quick-xml", "s"),
    ("calloop", "s"),
    ("xkeysym", "s"),
    ("miette", "z"),
    ("chrono", "s"),
    ("freedesktop-desktop-entry", "z"),
    ("pipewire", "z"),
    ("libspa", "z"),
];

/// Crates the release profile leaves at the default (opt 3) because a
/// person waits on them (decisions.md, m5-speed-profile): `strand`
/// above all, whose level the fat-LTO pass runs the whole binary at.
/// A size override on any of them fails here, as a dropped one above
/// does.
const RELEASE_SPEED_CRATES: &[&str] = &[
    "strand",
    "serde_json",
    "wayland-client",
    "wayland-backend",
    "smithay-client-toolkit",
    "material-colors",
    "roxmltree",
    "velato",
    "image-webp",
    "jpeg-decoder",
    "png",
    "gif",
    "fdeflate",
    "miniz_oxide",
    "weezl",
    "imagesize",
    "usvg",
    "svgtypes",
    "simplecss",
    "data-url",
    "resvg",
    "tiny-skia",
    "tiny-skia-path",
];

/// The key [`release_opt_levels`] gives `[profile.release]`'s own
/// `opt-level`, the level of every crate without an override.
const RELEASE_WIDE: &str = "[profile.release]";

/// `[profile.release.package.<name>]` → its `opt-level`, read from a
/// Cargo.toml's text (section headers and `key = value` lines only);
/// `[profile.release]`'s own is under [`RELEASE_WIDE`] and the wildcard
/// `[profile.release.package."*"]`'s under `*`.
fn release_opt_levels(manifest: &str) -> std::collections::BTreeMap<String, String> {
    let unquote = |s: &str| s.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
    let mut levels = std::collections::BTreeMap::new();
    let mut package: Option<String> = None;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            package = if line == RELEASE_WIDE {
                Some(RELEASE_WIDE.to_string())
            } else {
                line.strip_prefix("[profile.release.package.")
                    .and_then(|rest| rest.strip_suffix(']'))
                    .map(unquote)
            };
        } else if let (Some(name), Some((key, value))) = (&package, line.split_once('='))
            && key.trim() == "opt-level"
        {
            levels.insert(name.clone(), unquote(value));
        }
    }
    levels
}

#[test]
fn the_release_profile_keeps_the_size_opt_levels_the_budget_rests_on() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let manifest = std::fs::read_to_string(path).unwrap();
    let levels = release_opt_levels(&manifest);
    let changed: Vec<String> = RELEASE_OPT_LEVELS
        .iter()
        .filter(|(name, level)| levels.get(*name).map(String::as_str) != Some(*level))
        .map(|(name, level)| {
            format!(
                "{name}: opt-level {level} expected, found {:?}",
                levels.get(*name)
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "the workspace's [profile.release.package] opt-levels changed; the bar's memory budget \
         rests on them (decisions.md, wave4-exitMemory): {changed:#?}"
    );
    let sized = sized_speed_crates(&levels);
    assert!(
        sized.is_empty(),
        "crates a person waits on are built for size; they stay at the release default, 3 \
         (decisions.md, m5-speed-profile): {sized:#?}"
    );
}

/// What builds a speed crate for size: an override of its own, a
/// wildcard (`"*"`) level other than 3, which reaches every dependency
/// without an override, or a profile-wide one, which reaches `strand`
/// too and so the whole binary's fat-LTO pass (decisions.md,
/// m5-speed-strand).
fn sized_speed_crates(levels: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut sized: Vec<String> = [RELEASE_WIDE, "*"]
        .iter()
        .filter_map(|key| {
            levels
                .get(*key)
                .filter(|level| level.as_str() != "3")
                .map(|level| format!("{key}: opt-level {level}"))
        })
        .collect();
    sized.extend(RELEASE_SPEED_CRATES.iter().filter_map(|name| {
        levels
            .get(*name)
            .filter(|level| level.as_str() != "3")
            .map(|level| format!("{name}: opt-level {level}"))
    }));
    sized
}

#[test]
fn a_size_level_reaching_a_speed_crate_is_caught() {
    let clean =
        "[profile.release]\nlto = \"fat\"\n[profile.release.package.zbus]\nopt-level = \"z\"\n";
    assert!(sized_speed_crates(&release_opt_levels(clean)).is_empty());
    for manifest in [
        "[profile.release]\nopt-level = \"s\"\n",
        "[profile.release]\nopt-level = 2 # a comment\n",
        "[profile.release.package.\"*\"]\nopt-level = \"z\"\n",
        "[profile.release.package.strand]\nopt-level = \"s\"\n",
        "[profile.release.package.velato]\nopt-level = 'z'\n",
    ] {
        assert_eq!(
            sized_speed_crates(&release_opt_levels(manifest)).len(),
            1,
            "{manifest}"
        );
    }
    let three =
        "[profile.release]\nopt-level = 3\n[profile.release.package.strand]\nopt-level = 3\n";
    assert!(sized_speed_crates(&release_opt_levels(three)).is_empty());
}

#[test]
fn release_opt_levels_reads_release_sections_only() {
    // `[profile.release]`'s own level is read too (under RELEASE_WIDE,
    // for the speed-crate guard); other sections are not.
    let manifest = "[profile.release]\nopt-level = 3\n\
                    [profile.release.package.zbus]\nopt-level = \"z\" # size\n\
                    [profile.release.package.\"quoted\"]\nopt-level = 's'\n\
                    [profile.dev]\nopt-level = 1\n\
                    [dependencies]\nopt-level = \"z\"\n";
    let levels = release_opt_levels(manifest);
    assert_eq!(levels.len(), 3, "{levels:?}");
    assert_eq!(levels[RELEASE_WIDE], "3");
    assert_eq!(levels["zbus"], "z");
    assert_eq!(levels["quoted"], "s");
}

/// design.md budgets 10–14 MB for "code and libraries touched", and the
/// two-monitor bar's code is resident close to its whole `.text` where
/// the page cache maps large folios (decisions.md, wave4-exitMemory):
/// the release binary's `.text` stays within its gate whatever the
/// runner's page cache does. It rests on the workspace's release
/// profile (`[profile.release.package]`: services and glue built for
/// size).
#[test]
fn the_release_binary_code_stays_within_its_gate() {
    if cfg!(debug_assertions) {
        eprintln!("skipped: a debug build's code is not the release binary's");
        return;
    }
    let elf = std::fs::read(env!("CARGO_BIN_EXE_strand")).unwrap();
    let text = text_size(&elf).expect("an ELF64 binary with a .text");
    // Both gates are a measured tree's size (CI's budgets job, last on
    // `main` at 091c7f64) plus a 512 KiB reserve, rounded up to 256 KiB
    // (`TEXT_GATE_*`; decisions.md growth).
    let (gate, build) = if cfg!(feature = "gpu") {
        (TEXT_GATE_GPU, "the default build (GPU backend)")
    } else {
        (TEXT_GATE_CPU, "the CPU-only build (--no-default-features)")
    };
    eprintln!("strand's .text: {text} bytes, {build}");
    notice(&format!(
        "strand's .text: {text} bytes (gate {gate}, {build})"
    ));
    // The M5 ledger: each build
    // adds its half to STRAND_LEDGER_FILE, and scripts/container/ledger.sh
    // prints the line once both builds ran (CI's budgets job, run.sh ci).
    let gpu = cfg!(feature = "gpu");
    let half = ledger_half(gpu, text, gate);
    eprintln!("ledger: {half}");
    if let Some(file) = std::env::var_os("STRAND_LEDGER_FILE") {
        use std::io::Write;
        let key = if gpu { "gpu" } else { "cpu" };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "{key} {half}").unwrap();
    }
    assert!(
        text <= gate,
        "strand's .text is {text} bytes, over {gate} for {build}: the gate catches accidental \
         growth; code no one waits on belongs at opt-level \"s\" or \"z\" (Cargo.toml, \
         [profile.release.package]), and a deliberate speed choice re-bases the gate with its \
         measured reason (decisions.md, m5-speed-profile)"
    );
}

/// The M5 ledger's baselines: M4's merged release `.text`
/// (decisions.md m4-owner-3), CPU-only and GPU builds. Every M5 merge
/// is read against them.
const LEDGER_BASELINE_CPU: u64 = 15_657_671;
const LEDGER_BASELINE_GPU: u64 = 19_041_095;

/// One build's half of the ledger line, e.g. `CPU-only 15,700,000 (Δ
/// +42,329 vs 15,657,671; gate 15,925,248)`.
fn ledger_half(gpu: bool, text: u64, gate: u64) -> String {
    let (name, base) = if gpu {
        ("GPU", LEDGER_BASELINE_GPU)
    } else {
        ("CPU-only", LEDGER_BASELINE_CPU)
    };
    let delta = text as i128 - base as i128;
    let sign = if delta < 0 { "-" } else { "+" };
    format!(
        "{name} {} (Δ {sign}{} vs {}; gate {})",
        grouped(text as u128),
        grouped(delta.unsigned_abs()),
        grouped(base as u128),
        grouped(gate as u128)
    )
}

/// `n` with thousands separators: 15657671 → "15,657,671".
fn grouped(n: u128) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[test]
fn the_ledger_half_reads_size_delta_baseline_and_gate() {
    // Literal gates, not TEXT_GATE_*: the integrator lowers the gates as
    // M5's reclaims land, and that must not break this formatter test.
    assert_eq!(
        ledger_half(false, 15_700_000, 15_925_248),
        "CPU-only 15,700,000 (Δ +42,329 vs 15,657,671; gate 15,925,248)"
    );
    assert_eq!(
        ledger_half(true, 19_000_000, 19_398_656),
        "GPU 19,000,000 (Δ -41,095 vs 19,041,095; gate 19,398,656)"
    );
    assert_eq!(
        ledger_half(false, LEDGER_BASELINE_CPU, 15_400_000),
        "CPU-only 15,657,671 (Δ +0 vs 15,657,671; gate 15,400,000)"
    );
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(999), "999");
    assert_eq!(grouped(1_000), "1,000");
}

/// `.text` of the default (GPU) release build: 28,311,552 B (108 × 256
/// KiB): services-round's tree (`laptop/services` at its S3 commit)
/// measured 27,620,503 B in the container suite (`main` at 294970c8
/// 26,539,863 B: S1 and S2's service members +814,912 B, S3's polkit
/// agent and greetd greeter +265,728 B), plus a 512 KiB reserve over
/// CI's runner (about 48 KB above the container), rounded up to 256 KiB
/// (decisions.md services-round). It was 26,738,688 B: `main` at
/// 091c7f64 measured 26,207,698 B in GitHub CI's
/// budgets job (run 39691736543; 26,159,895 B in the container suite),
/// 6,702 B under the gate before; the owner raised it (2026-10-22,
/// "Raise the code size limit."), by the same rule: CI's measurement
/// plus a 512 KiB reserve, rounded up to 256 KiB (decisions.md growth).
/// It was 26,214,400 B from
/// the m6-service-gaps merged tree, which measured 25,465,431 B (the
/// clipboard, disk and keyboard services and the network, Bluetooth,
/// media, system and notification additions, +617,856 B over `main`);
/// the gate adds a 512 KiB reserve, rounded up to 256 KiB (decisions.md
/// m6-service-gaps, integration); before it 25,165,824 B from M6 wave 2's
/// merged tree (24,451,607 B, the surface stream's box graph; 512 KiB
/// reserve, decisions.md m6-owner, 2026-10-14), 24,379,392 B from the re-base on M6 wave 1's `main` (512 KiB reserve,
/// decisions.md m6-owner, 2026-10-12), 23,855,104 B from remote
/// media art's re-base (ureq over rustls/ring, about 778 KB; 64 KiB
/// reserve, decisions.md m6-integration-w1), 22,806,528 B from M5 wave 2's re-base (decisions.md
/// m5-integration-w2), 22,282,240 B after the speed review (decisions.md
/// m5-speed) and 18.5 MiB before it.
const TEXT_GATE_GPU: u64 = 28_311_552;
/// `.text` of the CPU-only release build: 24,379,392 B (93 × 256 KiB):
/// services-round's tree measured 23,581,207 B in the container suite
/// (`main` at 294970c8 22,508,247 B; S1 and S2 +807,168 B, S3 +265,792
/// B), plus a 512 KiB reserve over CI's runner, rounded up to 256 KiB
/// (decisions.md services-round). It was 22,806,528 B: `main` at
/// 091c7f64 measured 22,218,514 B (CI run 39691736543;
/// 22,170,775 B in the container suite); the owner raised the gate
/// (2026-10-22) by the same rule, CI's measurement plus a 512 KiB
/// reserve, rounded up to 256 KiB (decisions.md growth). It was
/// 22,282,240 B from the m6-service-gaps merged tree (21,512,215 B,
/// +595,648 B over `main`, plus the same reserve; decisions.md
/// m6-service-gaps, integration), 21,233,664 B from
/// M6 wave 2's merged tree (20,533,655 B; 512 KiB reserve, decisions.md
/// m6-owner, 2026-10-14), 20,447,232 B from the re-base on M6 wave 1's `main` (512 KiB
/// reserve, decisions.md m6-owner, 2026-10-12), 19,922,944 B from remote media art's re-base (64 KiB reserve,
/// decisions.md m6-integration-w1), 18,874,368 B from
/// M5 wave 2's re-base (decisions.md m5-integration-w2), 18,350,080 B
/// after the speed review (decisions.md m5-speed) and 15,859,712 B
/// before it. The gate catches accidental growth, never caps a feature:
/// a deliberate choice re-bases it with its measured reason.
const TEXT_GATE_CPU: u64 = 24_379_392;
/// The lock's fault injection never ships (decisions.md, m4-lock-w2):
/// the `faults` feature's `STRAND_FAULT` points are not in the binary a
/// default build makes, in the dev profile here and in release in CI's
/// budgets job (which runs this file with `--release`). Built with
/// `--features faults` (the lock VM's build) the same check is the
/// positive control: the strings must be there.
#[test]
fn the_binary_holds_no_fault_code() {
    let bin = std::fs::read(env!("CARGO_BIN_EXE_strand")).unwrap();
    for needle in [
        b"STRAND_FAULT".as_slice(),
        b"logic_panic",
        b"lock_no_frame",
        b"auth_missing",
    ] {
        let holds = bin.windows(needle.len()).any(|w| w == needle);
        assert_eq!(
            holds,
            cfg!(feature = "faults"),
            "{:?} in strand's binary (faults: {})",
            String::from_utf8_lossy(needle),
            cfg!(feature = "faults")
        );
    }
}

/// (report) What the replay rings may add to the process's anonymous
/// PSS (`Pss_Anon`, where the heap and so the rings are): their 256 KiB
/// and 32 KiB of slack (docs/architecture.md, "Glitch reports"). The
/// whole PSS is not compared: its shared libraries' share moves by a
/// few hundred kB as other processes map and unmap them.
const RINGS_KB: u64 = 288;

/// `pid`'s anonymous PSS in kB.
fn anon_kb(pid: u32) -> u64 {
    status_of(pid, "Pss_Anon:").unwrap_or(0)
}

/// (report) The replay rings, filled, cost at most their size: a bar
/// clicked until both rings wrap, first with `system.replay` off (the
/// same input warms every cache), then on; anonymous PSS on minus off.
#[test]
fn the_replay_rings_cost_at_most_their_size() {
    const CLICKS: u32 = 2400;
    // Three cells a click: a slow (debug) shell's tick takes several
    // clicks, and one tick's writes of a cell are one record.
    const BAR: &str = "export state clicks = 0
state a = 0
state b = 0
export let recording = system.replay
bar Top {
  edge: top; height: 40
  row { box { width: 80; height: 40; bg: clicks % 2 == 0 ? #00ff00 : #0000ff
    on click { clicks = clicks + 1; a = a + 2; b = b + 3 } } }
}
";
    let Some(shell) = Bare::start("rings", BAR) else {
        return;
    };
    let mut pointer = support::pointer::Pointer::new(&shell.dir.join(&shell.display));
    let pid = shell.strand.0.id();
    let log_text = || shell.log_text();
    let cli = |args: &[&str]| shell.cli(args);
    let wait = |what: &str, ok: &dyn Fn() -> bool| shell.wait(what, ok);
    wait("strand answers", &|| cli(&["get", "b.clicks"]).is_some());
    // The layout: 2560 + 2560 / 1.25 logical pixels wide.
    let (lw, lh) = (2560 + 2048, 1440);
    // Clicks in small batches until `clicks` reaches `upto`: a slow
    // (debug) shell takes a batch's presses as fewer clicks.
    let clicks = || -> u32 {
        cli(&["get", "b.clicks"])
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    };
    let round = |pointer: &mut support::pointer::Pointer, upto: u32| {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut sent = 0u32;
        loop {
            let got = clicks();
            if got >= upto {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{got} of {upto} clicks\n{}",
                log_text()
            );
            for _ in 0..(upto - got).min(10) {
                pointer.click(10 + sent % 60, 20, lw, lh);
                sent += 1;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    assert!(cli(&["set", "system.replay", "false"]).is_some());
    wait("recording off", &|| {
        cli(&["get", "b.recording"]).as_deref() == Some("false")
    });
    // Twice off: the second round's PSS is the steady figure (the
    // first grows what the input needs, within a few kB of the second).
    // Twice off: the second round's figure is the steady one.
    round(&mut pointer, CLICKS);
    round(&mut pointer, 2 * CLICKS);
    let off = anon_kb(pid);
    assert!(cli(&["set", "system.replay", "true"]).is_some());
    wait("recording on", &|| {
        cli(&["get", "b.recording"]).as_deref() == Some("true")
    });
    round(&mut pointer, 3 * CLICKS);
    let on = anon_kb(pid);
    let added = on.saturating_sub(off);
    eprintln!(
        "replay rings: anonymous PSS {off} kB off, {on} kB on, {added} kB added (at most {RINGS_KB})"
    );
    // Both rings wrapped: a report's dumps are full.
    let ans = cli(&["report", "--json", "rings"]).expect("a report");
    let ans: serde_json::Value = serde_json::from_str(&ans).unwrap();
    let bundle = PathBuf::from(ans["bundle"].as_str().unwrap());
    for (file, spec) in [
        ("input.flight", strand_core::flight::INPUT),
        ("changes.flight", strand_core::flight::CHANGES),
    ] {
        let d = strand_core::flight::read(&std::fs::read(bundle.join(file)).unwrap()).unwrap();
        assert!(
            d.wrapped && d.records.len() == spec.records,
            "{file}: {} records, wrapped {}",
            d.records.len(),
            d.wrapped
        );
    }
    assert!(
        added <= RINGS_KB,
        "the replay rings added {added} kB of anonymous PSS, over {RINGS_KB} kB\n{}",
        memory_report(pid)
    );
    drop(shell);
}

/// A `strand run` of one config file on the test's headless sway
/// (`sway`, at scale 1) and a private bus, without the real services'
/// mocks: the rings' and the window's measurements.
struct Bare {
    // Dropped first: strand before its compositor.
    strand: Proc,
    _sway: Proc,
    _bus: PrivateBus,
    dir: PathBuf,
    display: String,
    socket: PathBuf,
    log: PathBuf,
    _tmp: TmpDir,
}

impl Bare {
    /// `None` (skipped, loudly) without sway or a bus.
    fn start(tag: &str, src: &str) -> Option<Bare> {
        for tool in ["sway", "swaymsg"] {
            if bounded(Command::new(tool).arg("--version"), tool).is_none() {
                assert!(
                    std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                    "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
                );
                eprintln!("\n*** SKIPPED: {tool} is not installed ***\n");
                return None;
            }
        }
        let bus = PrivateBus::start()?;
        let tmp =
            TmpDir::new(std::env::temp_dir().join(format!("strand-{tag}-{}", std::process::id())));
        let dir = tmp.0.join("run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (sway, display, _ipc) = sway(&dir, 1);
        let config = tmp.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("b.strand"), src).unwrap();
        let socket = dir.join("s.sock");
        let log = tmp.0.join("strand.log");
        let strand = Proc(
            Command::new(env!("CARGO_BIN_EXE_strand"))
                .arg("run")
                .arg(&config)
                .envs(bus.env())
                .env("XDG_RUNTIME_DIR", &dir)
                .env("WAYLAND_DISPLAY", &display)
                .env("HOME", tmp.0.join("home"))
                .env("XDG_CONFIG_HOME", tmp.0.join("xdg-config"))
                .env("XDG_CACHE_HOME", tmp.0.join("cache"))
                .env("XDG_STATE_HOME", tmp.0.join("state"))
                .env("STRAND_SOCKET", &socket)
                .env_remove("STRAND_MOCK")
                .env_remove("SWAYSOCK")
                .stdin(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        Some(Bare {
            strand,
            _sway: sway,
            _bus: bus,
            dir,
            display,
            socket,
            log,
            _tmp: tmp,
        })
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// A `strand` command's stdout, `None` when it failed.
    fn cli(&self, args: &[&str]) -> Option<String> {
        let out = bounded(
            Command::new(env!("CARGO_BIN_EXE_strand"))
                .args(args)
                .env("STRAND_SOCKET", &self.socket),
            "strand",
        )?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Waits up to a minute for `ok`.
    fn wait(&self, what: &str, ok: &dyn Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !ok() {
            assert!(Instant::now() < deadline, "{what}\n{}", self.log_text());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits up to 30 s for a whole second without a context switch in
    /// any of strand's threads.
    fn settle(&self, what: &str) {
        let pid = self.strand.0.id();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let w = switches(pid);
            std::thread::sleep(Duration::from_secs(1));
            if switches(pid) == w {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: never settled: {:?}\n{}",
                per_thread(pid),
                self.log_text()
            );
        }
    }
}

/// (window) `pid`'s own PSS in kB: anonymous and shared memory (its
/// heap, its buffers and the device's), not its libraries' share, which
/// moves by megabytes as other processes (a concurrent test's shell)
/// map and unmap them.
fn own_kb(pid: u32) -> u64 {
    status_of(pid, "Pss_Anon:").unwrap_or(0) + status_of(pid, "Pss_Shmem:").unwrap_or(0)
}

/// (window) What one open window costs over the shell without it, at
/// most what a panel of its size costs plus 1 MiB (its keyboard focus,
/// its toplevel and decoration objects), and well within the 14 MB
/// design.md's budget gives a panel (the launcher's buffers at 2×).
const WINDOW_OVER_PANEL_KB: u64 = 1024;
const PANEL_BUDGET_KB: u64 = 14 * 1024;

/// (window) One open window costs what a panel of its size does: a
/// 640×400 window (fixed-size, so sway floats it at that size) and a
/// 640×400 panel, each opened alone from `strand set` after both were
/// opened and closed once (the caches their first draw grows warm), on
/// a bar with no clock. Strand's own PSS ([`own_kb`]) with the window
/// open over the bar alone is within the panel's plus
/// [`WINDOW_OVER_PANEL_KB`] and within [`PANEL_BUDGET_KB`]; the open
/// window, drawn, wakes no thread of strand's for 5 s; closed, its
/// buffers are let go (within 1 MiB of the bar alone). A config with no
/// window is the design bar's, gated above.
#[test]
fn an_open_window_costs_what_a_panel_does() {
    const SRC: &str = "export state win = false
export state card = false
bar Top { edge: top; height: 30; text \"bar\" }
window Probe {
  open: <-> win; title: \"Probe\"; bg: #1e1e2e
  width: 640; height: 400; min_width: 640; max_width: 640; min_height: 400; max_height: 400
  col { pad: 24; text \"Settings\"; box { width: 200; height: 100; bg: #89b4fa } }
}
panel Card {
  open: <-> card; bg: #1e1e2e; width: 640; height: 400
  col { pad: 24; text \"Settings\"; box { width: 200; height: 100; bg: #89b4fa } }
}
";
    let Some(shell) = Bare::start("window-cost", SRC) else {
        return;
    };
    let pid = shell.strand.0.id();
    shell.wait("strand answers", &|| shell.cli(&["get", "b.win"]).is_some());
    let shown = |ns: &str| {
        shell
            .cli(&["report"])
            .is_some_and(|r| r.contains(&format!("{ns} (")))
    };
    let open = |cell: &str, ns: &str| {
        assert!(shell.cli(&["set", cell, "true"]).is_some());
        shell.wait(&format!("{ns} open"), &|| shown(ns));
        shell.settle(&format!("{ns} drawn"));
    };
    let close = |cell: &str, ns: &str| {
        assert!(shell.cli(&["set", cell, "false"]).is_some());
        shell.wait(&format!("{ns} closed"), &|| !shown(ns));
        shell.settle(&format!("{ns} let go"));
    };
    shell.settle("the bar");
    // Once each: the glyphs, the keymap and the pools they grow.
    open("b.win", "strand-Probe");
    close("b.win", "strand-Probe");
    open("b.card", "strand-Card");
    close("b.card", "strand-Card");
    let bar = own_kb(pid);
    open("b.win", "strand-Probe");
    let report = shell.cli(&["report"]).unwrap_or_default();
    assert!(
        report.contains("    window: floating"),
        "sway floats a fixed-size window: {report}"
    );
    let with_window = own_kb(pid);
    // The report's round trip ends after its answer (the connection's
    // hangup), so the quiet seconds start once that has settled. The
    // watcher also watches the config's ancestors (`/tmp` among them), so
    // a wake of `strand-watch` alone while another test makes a name
    // beside one is tried again, as the bar's idle window does.
    let root = shell.log.parent().unwrap().to_path_buf();
    let mut monitor = DirMonitor::new(
        pid,
        &[
            root.clone(),
            PathBuf::from("/usr/share"),
            PathBuf::from("/usr/local/share"),
        ],
    );
    let own = [shell.log.clone(), root.join("state"), root.join("cache")];
    let mut woke = Vec::new();
    for attempt in 1..=3 {
        shell.settle("the report answered");
        let _ = monitor.events();
        let before = per_thread(pid);
        std::thread::sleep(Duration::from_secs(5));
        let after = per_thread(pid);
        let seen = monitor.events();
        let threads: Vec<&String> = after
            .keys()
            .filter(|t| before.get(*t) != after.get(*t))
            .collect();
        woke = threads
            .iter()
            .map(|t| {
                format!(
                    "{t}: {} -> {}",
                    before.get(*t).copied().unwrap_or(0),
                    after[*t]
                )
            })
            .collect();
        let watcher_only = threads.iter().all(|t| {
            t.split_once(' ')
                .is_some_and(|(_, comm)| comm == "strand-watch")
        });
        let outside = seen.iter().any(|e| !own.iter().any(|p| event_under(e, p)));
        if !woke.is_empty() && watcher_only && outside && attempt < 3 {
            eprintln!(
                "window quiet {attempt}: the watcher woke on a change outside the shell, \
                 trying again: {woke:?}, events {seen:?}"
            );
            continue;
        }
        if !woke.is_empty() {
            woke.push(format!("events: {seen:?} ({})", monitor.describe()));
        }
        break;
    }
    close("b.win", "strand-Probe");
    let after_window = own_kb(pid);
    open("b.card", "strand-Card");
    let with_panel = own_kb(pid);
    close("b.card", "strand-Card");
    let window = with_window.saturating_sub(bar);
    let panel = with_panel.saturating_sub(bar);
    eprintln!(
        "window cost: own PSS {bar} kB the bar, {with_window} kB with the window ({window} kB), \
         {with_panel} kB with the panel ({panel} kB), {after_window} kB the window closed; \
         threads woken over 5 s with the window open: {woke:?}"
    );
    assert!(
        woke.is_empty(),
        "an open window woke strand: {woke:?}\n{}",
        shell.log_text()
    );
    assert!(
        window <= panel + WINDOW_OVER_PANEL_KB && window <= PANEL_BUDGET_KB,
        "the window costs {window} kB, the panel {panel} kB\n{}",
        memory_report(pid)
    );
    assert!(
        after_window <= bar + 1024,
        "the closed window kept {} kB\n{}",
        after_window.saturating_sub(bar),
        memory_report(pid)
    );
}

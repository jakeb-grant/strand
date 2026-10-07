//! M3 exit, "100 reloads with no reconnects" (design.md, "Live reload":
//! a reload keeps services running; "System services": a service starts
//! on its first reader and stops 5 s after its last one leaves).
//!
//! `strand run` without `STRAND_MOCK` runs a shell that reads every real
//! service: python-dbusmock's UPower, NetworkManager (a Wi-Fi network
//! joined), BlueZ (a powered adapter, a paired device), logind and
//! power-profiles-daemon (a `from dbus` service) and a mock portal on a
//! private `dbus-daemon`; the shell's own notifications server (one
//! notification sent) and tray host there, with an app's tray item and
//! its DBusMenu and an MPRIS player playing (zbus mocks); sway's IPC (workspaces, windows, wm) on a headless sway; a
//! private PipeWire with WirePlumber (audio); a backlight directory
//! (brightness), desktop entries (apps), a `from file` service, procfs
//! (cpu, memory), the clock and the calendar. Then 100 reloads through
//! `strand run`'s watcher, saved in place: token edits, markup edits (a
//! node added and removed) and binding edits of an expression that reads
//! services. Nothing restarts or reconnects:
//!
//! - no service starts or stops again (`STRAND_LOG=info`: one
//!   `service `x` started` line per run, the client's start counter);
//! - no connection is made to the bus at all (dbus-daemon numbers its
//!   connections in order: a probe connection before and after the
//!   reloads takes consecutive unique names), and the mocks answer no
//!   method call (python-dbusmock's logs: every `Get`, `GetAll` and
//!   method call they answer is a line; the zbus mocks count each
//!   property read and method call);
//! - strand's sockets are the same ones after every reload (sway's IPC,
//!   PipeWire, the bus, Wayland: the socket inodes of its open files);
//! - PipeWire's clients of strand are the same objects (`object.serial`);
//! - every reload keeps its state (`strand watch` lists no reset), and
//!   the state set before the reloads and every service's value are on
//!   screen after them, and still 6 s later (past the 5 s stop grace).
//!
//! Skipped, loudly, without sway, grim, dbus-daemon, python-dbusmock,
//! PipeWire or WirePlumber (CI sets `STRAND_REQUIRE_SWAY`,
//! `STRAND_REQUIRE_DBUS` and `STRAND_REQUIRE_PIPEWIRE`).

#[path = "../../strand-services/tests/pipewire/mod.rs"]
mod pipewire;
#[allow(dead_code)]
#[path = "../../strand-services/tests/common/window.rs"]
mod window;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pipewire::PipeWire;
use strand_services::testing::{DbusMock, PrivateBus};
use window::TestWindow;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

const W: usize = 1280;
const H: usize = 720;
const RELOADS: usize = 100;
const UPOWER: &str = "org.freedesktop.UPower";
const LOGIND: &str = "org.freedesktop.login1";
const NM: &str = "org.freedesktop.NetworkManager";
const NM_ROOT: &str = "/org/freedesktop/NetworkManager";
const BLUEZ: &str = "org.bluez";
/// The paired Bluetooth device.
const HEADPHONES: &str = "11:22:33:44:55:66";

/// What each service reads at the values the test gives it: one box
/// each, green when it holds.
const CHECKS: [(&str, &str); 15] = [
    (
        "battery",
        "battery.present && battery.percent > 0.41 && battery.percent < 0.43",
    ),
    (
        "audio",
        "audio.sink.volume > 0.49 && audio.sink.volume < 0.51",
    ),
    (
        "brightness",
        "brightness.level > 0.49 && brightness.level < 0.51",
    ),
    (
        "network",
        "network.connected && network.ssid == \"Home\" && network.strength > 0.81 \
         && network.access_points.count(a => a.active && a.ssid == \"Home\") == 1",
    ),
    (
        "bluetooth",
        "bluetooth.powered && bluetooth.devices.count(d => d.name == \"Headphones\") == 1",
    ),
    (
        "tray",
        "tray.items.count(t => t.title == \"Reloads\" \
         && t.menu.items.count(m => m.label == \"Open\") == 1) == 1",
    ),
    (
        "notifications",
        "notifications.count == 1 \
         && notifications.all.count(n => n.summary == \"reloads\") == 1",
    ),
    ("media", "media.playing && media.title == \"Track\""),
    ("workspaces", "workspaces.all.count(w => true) >= 1"),
    ("windows", "windows.all.count(w => true) >= 1"),
    ("system", "system.dark"),
    ("apps", "apps.all.count(a => true) == 1"),
    ("mood (from file)", "mood.level == 7"),
    ("ppd (from dbus)", "ppd.profile == \"balanced\""),
    ("wm", "wm.name == \"sway\""),
];

/// The edited file: `@COLOR@` (a token edit), `@EXTRA@` (a markup edit)
/// and `@CMP@` (a binding edit of an expression reading services).
const SHELL: &str = r#"service mood from file "@MOOD@" { level: int }
service ppd from dbus system "net.hadess.PowerProfiles" { profile: text = ActiveProfile }
tokens base { probe.c: @COLOR@ }
export state n = 0
bar Top {
  edge: top; height: 40
  bg: $probe.c
  row {
    gap: 4
    box { width: 20; height: 40; bg: n == 7 ? #00ff00 : #ff0000 }
@CHECKS@    text join(" ", network.connected, network.access_points.count(a => true),
      bluetooth.powered, tray.items.count(t => true), notifications.count,
      wm.name, media.playing, cpu.usage @CMP@ 0, memory.usage > 0,
      clock.format("%H:%M"), calendar.days(clock.today).count(d => true))
@EXTRA@  }
}
"#;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A mock `org.freedesktop.portal.Settings`: dark.
struct MockPortal {
    values: std::collections::HashMap<String, OwnedValue>,
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl MockPortal {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if namespace != "org.freedesktop.appearance" {
            return Err(zbus::fdo::Error::Failed("not found".into()));
        }
        self.values
            .get(key)
            .and_then(|v| v.try_clone().ok())
            .ok_or_else(|| zbus::fdo::Error::Failed("not found".into()))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

/// The calls a zbus mock answered: each property read (one per property
/// of a `GetAll`) and method call is one.
#[derive(Clone, Default)]
struct Calls(Arc<AtomicUsize>);

impl Calls {
    fn tick(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// An app's tray item (`org.kde.StatusNotifierItem`) with a menu.
struct TrayItemMock(Calls);

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl TrayItemMock {
    fn activate(&self, _x: i32, _y: i32) {
        self.0.tick();
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        self.0.tick();
    }
    fn scroll(&self, _delta: i32, _orientation: &str) {
        self.0.tick();
    }
    fn context_menu(&self, _x: i32, _y: i32) {
        self.0.tick();
    }
    #[zbus(property)]
    fn id(&self) -> String {
        self.0.tick();
        "reloads".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        self.0.tick();
        "Reloads".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        self.0.tick();
        "Active".into()
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.0.tick();
        "reloads-icon".into()
    }
    #[zbus(property)]
    fn tool_tip(&self) -> Tip {
        self.0.tick();
        (String::new(), Vec::new(), "Reloads".into(), String::new())
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        self.0.tick();
        OwnedObjectPath::try_from("/Menu").unwrap()
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        self.0.tick();
        false
    }
}

/// The tray item's menu (`com.canonical.dbusmenu`): Open, Quit.
struct TrayMenuMock(Calls);

/// A tray item's tooltip: icon name, pixmaps, title, text.
type Tip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

type Layout = (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>));

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl TrayMenuMock {
    #[zbus(out_args("revision", "layout"))]
    fn get_layout(&self, _parent: i32, _depth: i32, _props: Vec<String>) -> Layout {
        self.0.tick();
        let node = |id: i32, label: &str| {
            let props = HashMap::from([("label".to_string(), ZValue::from(label.to_string()))]);
            OwnedValue::try_from(ZValue::from((id, props, Vec::<ZValue>::new()))).unwrap()
        };
        (
            1,
            (0, HashMap::new(), vec![node(1, "Open"), node(2, "Quit")]),
        )
    }
    fn event(&self, _id: i32, _event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.0.tick();
    }
    fn about_to_show(&self, _id: i32) -> bool {
        self.0.tick();
        false
    }
}

/// An MPRIS player playing `Track`.
struct PlayerMock(Calls);

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl PlayerMock {
    fn play_pause(&self) {
        self.0.tick();
    }
    fn next(&self) {
        self.0.tick();
    }
    fn previous(&self) {
        self.0.tick();
    }
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.0.tick();
        "Playing".into()
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.0.tick();
        HashMap::from([
            (
                "xesam:title".to_string(),
                OwnedValue::try_from(ZValue::from("Track")).unwrap(),
            ),
            ("mpris:length".to_string(), OwnedValue::from(200_000_000i64)),
        ])
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.0.tick();
        1_000_000
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.0.tick();
        1.0
    }
}

/// The player's `org.mpris.MediaPlayer2`.
struct PlayerRootMock(Calls);

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl PlayerRootMock {
    #[zbus(property)]
    fn identity(&self) -> String {
        self.0.tick();
        "Reloads".into()
    }
}

struct Img {
    w: usize,
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
            rgb: ppm[c + 1..c + 1 + w * h * 3].to_vec(),
        }
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// The probe boxes along the bar, green or not: the state set before
    /// the reloads, then [`CHECKS`] (`None` until all are drawn).
    fn boxes(&self) -> Option<Vec<bool>> {
        let g = |p: [u8; 3]| p[1] > 200 && p[0] < 60 && p[2] < 60;
        let r = |p: [u8; 3]| p[0] > 200 && p[1] < 60 && p[2] < 60;
        let mut runs = Vec::new();
        let mut last = None;
        for x in 0..600.min(self.w) {
            let p = self.px(x, 20);
            let now = if g(p) {
                Some(true)
            } else if r(p) {
                Some(false)
            } else {
                None
            };
            if now.is_some() && now != last {
                runs.push(now == Some(true));
            }
            last = now;
        }
        (runs.len() == 1 + CHECKS.len()).then_some(runs)
    }
}

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim", "pw-dump"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed; the 100 reloads did not run ***\n");
            return false;
        }
    }
    true
}

/// A headless sway in `dir`: the process, its display and its IPC socket.
fn sway(dir: &Path) -> (Proc, String, PathBuf) {
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
                return (sway, d.clone(), dir.join(i));
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

/// The bus's next connection number: a probe connection's unique name
/// (`:1.N`), dropped at once. dbus-daemon numbers connections in order,
/// so two probes `N` and `M` apart saw `M - N - 1` other connections
/// made between them.
fn next_connection(tokio: &tokio::runtime::Runtime, bus: &PrivateBus) -> u64 {
    tokio.block_on(async {
        let conn = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap();
        let name = conn.unique_name().unwrap().to_string();
        name.rsplit('.').next().unwrap().parse().unwrap()
    })
}

/// The socket inodes among a process's open files.
fn sockets(pid: u32) -> BTreeSet<String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| std::fs::read_link(e.path()).ok())
                .map(|l| l.display().to_string())
                .filter(|l| l.starts_with("socket:"))
                .collect()
        })
        .unwrap_or_default()
}

/// PipeWire's client objects of a process, by `object.serial` (a new
/// connection is a new serial).
fn pipewire_clients(pw: &PipeWire, pid: u32) -> BTreeSet<u64> {
    let dump: Vec<serde_json::Value> =
        serde_json::from_str(&pw.run("pw-dump", &[])).unwrap_or_default();
    dump.iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Client")
        .filter(|o| {
            let p = &o["info"]["props"]["application.process.id"];
            p.as_u64() == Some(pid as u64) || p.as_str() == Some(pid.to_string().as_str())
        })
        .filter_map(|o| o["info"]["props"]["object.serial"].as_u64())
        .collect()
}

/// The method calls each mock answered so far (lines of its log that
/// are not signals it emitted).
fn mock_calls(mocks: &[(&str, &DbusMock)]) -> BTreeMap<String, Vec<String>> {
    mocks
        .iter()
        .map(|(name, m)| {
            let text = std::fs::read_to_string(m.log()).unwrap_or_default();
            let calls = text
                .lines()
                .filter_map(|l| l.split_once(' ').map(|(_, rest)| rest))
                .filter(|l| !l.starts_with("emit "))
                .map(str::to_string)
                .collect();
            (name.to_string(), calls)
        })
        .collect()
}

/// The service runs `strand run` logged (`STRAND_LOG=info`): starts and
/// stops, in order.
fn lifecycle(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.contains("service `") && (l.ends_with(" stopped") || l.contains("started")))
        .map(str::to_string)
        .collect()
}

/// `strand watch --json`'s events, read on a thread of their own.
struct Watch {
    _child: Proc,
    events: std::sync::mpsc::Receiver<serde_json::Value>,
}

impl Watch {
    fn start(env: &[(&str, PathBuf)]) -> Watch {
        let mut child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["watch", "--json"])
            .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if let Ok(v) = serde_json::from_str(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        let w = Watch {
            _child: Proc(child),
            events,
        };
        w.until("subscribed", |ev| ev["event"] == "watching");
        w
    }

    /// The first event `f` accepts (30 s at most).
    fn until(&self, what: &str, f: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = self
                .events
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("strand watch never said: {what}"));
            if f(&ev) {
                return ev;
            }
        }
    }
}

#[test]
fn a_hundred_reloads_reconnect_and_restart_nothing() {
    if !tools() {
        return;
    }
    let Some(pw) = PipeWire::start("a_hundred_reloads_reconnect_and_restart_nothing") else {
        return;
    };
    pw.wait_for_defaults();
    pw.wpctl(&["set-volume", "@DEFAULT_AUDIO_SINK@", "0.5"]);
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
    // The system daemons.
    let Some(upower) = DbusMock::start(&bus, "upower", true, None, UPOWER) else {
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
    let Some(logind) = DbusMock::start(&bus, "logind", true, None, LOGIND) else {
        return;
    };
    let Some(nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    // A Wi-Fi device joined to `Home` (its saved connection active).
    macro_rules! nm {
        ($method:expr, $body:expr) => {
            call(
                &tokio,
                &conn,
                NM,
                NM_ROOT,
                "org.freedesktop.DBus.Mock",
                $method,
                &$body,
            )
            .body()
            .deserialize::<String>()
            .unwrap_or_default()
        };
    }
    let dev = nm!("AddWiFiDevice", ("mock_WiFi", "wlan0", 100i32));
    let ap = nm!(
        "AddAccessPoint",
        (
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
        )
    );
    let saved = nm!(
        "AddWiFiConnection",
        (dev.as_str(), "Mock_Con1", "Home", "wpa-psk")
    );
    nm!(
        "AddActiveConnection",
        (
            vec![dev.as_str()],
            saved.as_str(),
            ap.as_str(),
            "Mock_Active1",
            2u32
        )
    );
    let Some(bluez) = DbusMock::start(&bus, "bluez5", true, None, BLUEZ) else {
        return;
    };
    // An adapter (powered) with one paired device.
    macro_rules! bluez {
        ($method:expr, $body:expr) => {
            call(&tokio, &conn, BLUEZ, "/", "org.bluez.Mock", $method, &$body)
        };
    }
    bluez!("AddAdapter", ("hci0", "my-laptop"));
    bluez!("AddDevice", ("hci0", HEADPHONES, "Headphones"));
    bluez!("PairDevice", ("hci0", HEADPHONES));
    let Some(ppd) = DbusMock::start(
        &bus,
        "power_profiles_daemon",
        true,
        None,
        "net.hadess.PowerProfiles",
    ) else {
        return;
    };
    // The portal: dark.
    let values = std::collections::HashMap::from([(
        "color-scheme".to_string(),
        OwnedValue::try_from(ZValue::from(1u32)).unwrap(),
    )]);
    let _portal = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at("/org/freedesktop/portal/desktop", MockPortal { values })
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    // A media player, playing; an app's tray item, registered once the
    // shell's tray is the watcher. Each counts the calls it answers.
    let (player_calls, tray_calls) = (Calls::default(), Calls::default());
    let tray_name = "org.kde.StatusNotifierItem-4242-1";
    let (_player, tray_item) = tokio.block_on(async {
        let player = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.reloads")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", PlayerMock(player_calls.clone()))
            .unwrap()
            .serve_at(
                "/org/mpris/MediaPlayer2",
                PlayerRootMock(player_calls.clone()),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let item = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(tray_name)
            .unwrap()
            .serve_at("/StatusNotifierItem", TrayItemMock(tray_calls.clone()))
            .unwrap()
            .serve_at("/Menu", TrayMenuMock(tray_calls.clone()))
            .unwrap()
            .build()
            .await
            .unwrap();
        (player, item)
    });

    let dir = std::env::temp_dir().join(format!("strand-reloads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A backlight at half (brightness), one desktop entry (apps), the
    // `from file` service's document.
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
    // The config and data outside /tmp: the watcher's light watches on
    // the config's ancestors would wake for other tests' files there.
    let home =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("reloads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let apps = home.join("share/applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("one.desktop"),
        "[Desktop Entry]\nType=Application\nName=One\nExec=true\n",
    )
    .unwrap();
    let mood = home.join("mood.json");
    std::fs::write(&mood, r#"{"level": 7}"#).unwrap();
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    let file = config.join("reload.strand");
    let checks: String = CHECKS
        .iter()
        .map(|(_, c)| format!("    box {{ width: 12; height: 40; bg: {c} ? #00ff00 : #ff0000 }}\n"))
        .collect();
    let shell = |color: &str, extra: bool, cmp: &str| {
        SHELL
            .replace("@MOOD@", &mood.display().to_string())
            .replace("@CHECKS@", &checks)
            .replace("@COLOR@", color)
            .replace("@CMP@", cmp)
            .replace("@EXTRA@", if extra { "    text \"extra\"\n" } else { "" })
    };
    std::fs::write(&file, shell("#204080", false, ">")).unwrap();

    let (_sway, display, ipc) = sway(&dir);
    // A window: the windows store has one, its workspace is occupied.
    let _window = TestWindow::open(&dir.join(&display), "strand-reloads", "a window");
    let log = dir.join("strand.log");
    let mut strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("SWAYSOCK", &ipc)
            .env("PIPEWIRE_RUNTIME_DIR", pw.dir.path())
            .env_remove("PIPEWIRE_REMOTE")
            .env("HOME", &home)
            .env("XDG_DATA_DIRS", home.join("share"))
            .env("XDG_DATA_HOME", home.join("home-share"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("STRAND_BACKLIGHT_DIR", &backlight)
            .env("STRAND_LOG", "info")
            .envs(bus.env())
            .env_remove("STRAND_MOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let pid = strand.0.id();
    let log_text = || std::fs::read_to_string(&log).unwrap_or_default();
    let env: Vec<(&str, PathBuf)> = vec![
        ("XDG_RUNTIME_DIR", dir.clone()),
        ("WAYLAND_DISPLAY", PathBuf::from(&display)),
        ("HOME", home.clone()),
    ];
    let shot = || -> Option<Img> {
        let path = dir.join("shot.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", "HEADLESS-1"])
            .arg(&path)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .status()
            .is_ok_and(|s| s.success());
        ok.then(|| Img::ppm(&std::fs::read(&path).unwrap()))
    };
    let mut alive = |what: &str| {
        let exited = strand.0.try_wait().unwrap();
        assert!(exited.is_none(), "strand exited {what}:\n{}", log_text());
    };
    // Screenshots until every service's box is green and the state's is
    // `state`; the services not showing their value named otherwise.
    let wait_shot = |what: &str, state: bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let boxes = shot().and_then(|img| img.boxes());
            if let Some(b) = &boxes
                && b[0] == state
                && b[1..].iter().all(|g| *g)
            {
                return;
            }
            if Instant::now() >= deadline {
                let wrong: Vec<&str> = match &boxes {
                    Some(b) => CHECKS
                        .iter()
                        .zip(&b[1..])
                        .filter(|(_, g)| !**g)
                        .map(|((name, _), _)| *name)
                        .collect(),
                    None => vec!["(the boxes are not drawn)"],
                };
                panic!(
                    "never: {what}; not shown: {wrong:?}, the state's box {:?}\n{}",
                    boxes.as_ref().map(|b| b[0]),
                    log_text()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };

    // The shell's tray is the watcher, its notifications server owns the
    // name: the item registers, a notification arrives.
    for name in [
        "org.kde.StatusNotifierWatcher",
        "org.freedesktop.Notifications",
    ] {
        assert!(
            bus.wait_for_name(name, Duration::from_secs(30)),
            "strand never owned {name}:\n{}",
            log_text()
        );
    }
    call(
        &tokio,
        &tray_item,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
        "RegisterStatusNotifierItem",
        &(tray_name,),
    );
    call(
        &tokio,
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        &(
            "reloads",
            0u32,
            "",
            "reloads",
            "kept across them",
            Vec::<&str>::new(),
            HashMap::<&str, ZValue>::new(),
            0i32,
        ),
    );
    // Every service read: its box green.
    wait_shot("every service's value", false);
    alive("at boot");
    // The state the reloads must keep.
    let out = Command::new(env!("CARGO_BIN_EXE_strand"))
        .args(["set", "reload.n", "7"])
        .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_shot("the state set", true);
    // Settled: no service started for 2 s.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let before = lifecycle(&log_text()).len();
        std::thread::sleep(Duration::from_secs(2));
        if lifecycle(&log_text()).len() == before {
            break;
        }
        assert!(Instant::now() < deadline, "the services never settled");
    }
    let watch = Watch::start(&env);
    let runs = lifecycle(&log_text());
    let started: BTreeSet<&str> = runs.iter().filter_map(|l| l.split('`').nth(1)).collect();
    eprintln!("services running: {started:?}");
    for s in [
        "audio",
        "battery",
        "bluetooth",
        "brightness",
        "network",
        "tray",
        "notifications",
        "workspaces",
        "windows",
        "wm",
        "apps",
        "system",
        "media",
        "cpu",
        "memory",
        "mood",
        "ppd",
    ] {
        assert!(
            started.contains(s),
            "`{s}` never started:\n{}",
            runs.join("\n")
        );
    }
    assert!(
        runs.iter().all(|l| l.contains("(run 1)")),
        "a service ran twice before the reloads:\n{}",
        runs.join("\n")
    );
    let mocks = [
        ("upower", &upower),
        ("logind", &logind),
        ("networkmanager", &nm),
        ("bluez5", &bluez),
        ("power_profiles_daemon", &ppd),
    ];
    let calls = mock_calls(&mocks);
    let zbus_calls = [("tray item", &tray_calls), ("media player", &player_calls)];
    let zbus_before: Vec<usize> = zbus_calls.iter().map(|(_, c)| c.get()).collect();
    assert!(
        zbus_before.iter().all(|n| *n > 0),
        "a zbus mock was never read: {zbus_before:?}"
    );
    let socks = sockets(pid);
    let clients = pipewire_clients(&pw, pid);
    assert!(!clients.is_empty(), "strand is no PipeWire client");
    let first = next_connection(&tokio, &bus);
    eprintln!(
        "before the reloads: calls answered {:?} {:?}; {} sockets; PipeWire clients {clients:?}; bus connection {first}",
        calls
            .iter()
            .map(|(k, v)| (k.as_str(), v.len()))
            .collect::<Vec<_>>(),
        zbus_calls
            .iter()
            .zip(&zbus_before)
            .map(|((k, _), n)| (k, n))
            .collect::<Vec<_>>(),
        socks.len(),
    );
    // (logind is only written to: brightness reads the backlight.)
    assert!(
        calls.iter().all(|(k, c)| k == "logind" || !c.is_empty()),
        "a mock was never read: {calls:?}"
    );

    // 100 reloads, saved in place: a token edit, a markup edit, a binding
    // edit of an expression that reads services, in turn.
    let (mut color, mut extra, mut cmp) = ("#204080".to_string(), false, ">");
    let t0 = Instant::now();
    for i in 0..RELOADS {
        match i % 3 {
            0 => color = format!("#{:02x}4080", (i * 7) % 256),
            1 => extra = !extra,
            _ => cmp = if cmp == ">" { ">=" } else { ">" },
        }
        std::fs::write(&file, shell(&color, extra, cmp)).unwrap();
        let ev = watch.until(&format!("reload {i}"), |ev| {
            ev["event"] == "reload" && ev["committed"].as_array().is_some_and(|c| !c.is_empty())
        });
        assert_eq!(
            ev["reset"],
            serde_json::json!([]),
            "reload {i} reset state: {ev}"
        );
        let errors: Vec<_> = ev["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["severity"] == "error")
            .collect();
        assert!(errors.is_empty(), "reload {i}: {errors:?}");
        alive(&format!("after reload {i}"));
        assert_eq!(
            sockets(pid),
            socks,
            "strand's sockets changed by reload {i}"
        );
    }
    eprintln!("{RELOADS} reloads in {:.1} s", t0.elapsed().as_secs_f64());
    // Past the 5 s stop grace: a reader released by a reload and not
    // taken again would stop its service now.
    std::thread::sleep(Duration::from_secs(6));
    alive("after the reloads");
    let now = lifecycle(&log_text());
    assert!(
        now == runs,
        "a service started or stopped during the reloads:\n{}",
        now[runs.len().min(now.len())..].join("\n")
    );
    let last = next_connection(&tokio, &bus);
    assert_eq!(
        last,
        first + 1,
        "{} connections were made to the bus during the reloads",
        last - first - 1
    );
    let after = mock_calls(&mocks);
    for (name, before) in &calls {
        let now = &after[name];
        assert_eq!(
            now.len(),
            before.len(),
            "{name} answered calls during the reloads: {:?}",
            &now[before.len().min(now.len())..]
        );
    }
    for ((name, c), before) in zbus_calls.iter().zip(&zbus_before) {
        assert_eq!(
            c.get(),
            *before,
            "the {name} answered calls during the reloads"
        );
    }
    assert_eq!(sockets(pid), socks, "strand's sockets changed");
    assert_eq!(
        pipewire_clients(&pw, pid),
        clients,
        "strand's PipeWire clients changed"
    );
    // The state and every service's value, on screen.
    wait_shot("the state and every service after the reloads", true);
    drop(watch);
    drop(strand);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

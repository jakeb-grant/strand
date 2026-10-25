//! The builtin services on the contract: `system` against a mock portal
//! on a private bus, `cpu` and `memory` from procfs, `network`'s byte
//! rates from fake kernel counters, and `disk` from
//! `statvfs` (sampled only while a
//! reader is visible), and the test harness itself (private bus,
//! python-dbusmock).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Builtin, Buses, Rgba, Services, Store, cpu, disk, memory, network, system};
use zbus::zvariant::{OwnedValue, Value};

const PATH: &str = "/org/freedesktop/portal/desktop";

struct Portal {
    values: HashMap<String, OwnedValue>,
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl Portal {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if namespace != "org.freedesktop.appearance" {
            return Err(zbus::fdo::Error::Failed("not found".into()));
        }
        self.values
            .get(key)
            .map(|v| v.try_clone().unwrap())
            .ok_or_else(|| zbus::fdo::Error::Failed("not found".into()))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

fn owned(v: Value<'_>) -> OwnedValue {
    v.try_into().unwrap()
}

fn tokio() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn services(rt: &Runtime, buses: Buses) -> (Services, Builtin) {
    let s = Services::new(rt, buses, || {});
    let b = Builtin::register(&s, rt);
    (s, b)
}

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn system_follows_the_portal_on_the_shared_runtime() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let values = HashMap::from([
        ("color-scheme".to_string(), owned(Value::from(1u32))),
        (
            "accent-color".to_string(),
            owned(Value::from((0.5f64, 0.25f64, 1.0f64))),
        ),
        ("contrast".to_string(), owned(Value::from(1u32))),
    ]);
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(PATH, Portal { values })
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    // `on change system.dark` must not fire for the boot read.
    let changes = Arc::new(Mutex::new(Vec::new()));
    let c = changes.clone();
    let dark = b.system.cells().dark;
    let _watch = rt.on_change(
        move |rt| dark.get(rt),
        move |_, v: &bool| {
            c.lock().unwrap().push(*v);
            Ok(())
        },
    );
    rt.flush();
    b.system.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the boot read");
    rt.flush();
    let cells = b.system.cells();
    assert_eq!(cells.dark.get_untracked(&rt), Ok(true));
    assert_eq!(
        cells.accent.get_untracked(&rt),
        Ok(Some(Rgba::rgb(0.5, 0.25, 1.0)))
    );
    assert_eq!(cells.contrast.get_untracked(&rt), Ok(1.0));
    assert!(!cells.hostname.get_untracked(&rt).unwrap().is_empty());
    assert!(
        changes.lock().unwrap().is_empty(),
        "boot values are not changes"
    );
    // A change: an ordinary write.
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.portal.Settings",
            "SettingChanged",
            &(
                "org.freedesktop.appearance",
                "color-scheme",
                Value::from(2u32),
            ),
        ))
        .unwrap();
    until(&rt, &s, "light", || {
        cells.dark.get_untracked(&rt) == Ok(false)
    });
    rt.flush();
    assert_eq!(*changes.lock().unwrap(), [false]);
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.portal.Settings",
            "SettingChanged",
            &(
                "org.freedesktop.appearance",
                "reduced-motion",
                Value::from(1u32),
            ),
        ))
        .unwrap();
    until(&rt, &s, "reduced motion", || {
        cells.reduced_motion.get_untracked(&rt) == Ok(true)
    });
    s.shutdown();
    drop(conn);
}

/// A portal serving `values` on `bus`.
fn serve_portal(
    tokio: &tokio::runtime::Runtime,
    bus: &PrivateBus,
    values: HashMap<String, OwnedValue>,
) -> zbus::Connection {
    tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(PATH, Portal { values })
            .unwrap()
            .build()
            .await
            .unwrap()
    })
}

/// The session bus restarts under `system` (its connection dies under
/// the portal follow): the body fails, the client starts it again after
/// its backoff, and the new run connects afresh and follows the portal
/// on the new bus.
#[test]
fn system_follows_the_portal_again_after_the_bus_restarts() {
    let Some(mut bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let scheme = |v: u32| HashMap::from([("color-scheme".to_string(), owned(Value::from(v)))]);
    let conn = serve_portal(&tokio, &bus, scheme(1));
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.system.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the boot read");
    let cells = b.system.cells();
    assert_eq!(cells.dark.get_untracked(&rt), Ok(true));
    // The bus restarts; a portal comes back on it, now light.
    assert!(bus.restart());
    drop(conn);
    until(&rt, &s, "the follow to fail", || !b.system.running());
    let _conn = serve_portal(&tokio, &bus, scheme(2));
    // The retry (1 s on the logic clock) follows it again.
    let mut now = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(20);
    while cells.dark.get_untracked(&rt) != Ok(false) {
        assert!(Instant::now() < deadline, "never followed the new bus");
        now += Duration::from_millis(250);
        rt.tick(now);
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(b.system.running());
    assert!(b.system.starts() >= 2);
    s.shutdown();
}

/// A python-dbusmock template that cannot load says why: its output is
/// in the assertion message, not thrown away.
#[test]
fn a_dbusmock_that_fails_shows_its_output() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    if strand_services::testing::dbusmock_python().is_none() {
        return;
    }
    let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        DbusMock::start(&bus, "no_such_template", true, None, "org.example.Nobody")
    }));
    let Err(payload) = started else {
        panic!("a missing template started");
    };
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(msg.contains("never owned org.example.Nobody"), "{msg}");
    assert!(msg.contains("no_such_template"), "{msg}");
    assert!(
        msg.contains("its output:\n") && msg.lines().count() > 2,
        "{msg}"
    );
}

#[test]
fn without_a_bus_system_keeps_its_seeded_values() {
    let rt = Runtime::new();
    let (s, b) = services(&rt, Buses::none());
    b.system
        .seed(&rt, |sys| {
            sys.dark = true;
            sys.accent = Some(Rgba::rgb(1.0, 0.0, 0.0));
        })
        .unwrap();
    b.system.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let cells = b.system.cells();
    assert_eq!(cells.dark.get_untracked(&rt), Ok(true));
    assert_eq!(
        cells.accent.get_untracked(&rt),
        Ok(Some(Rgba::rgb(1.0, 0.0, 0.0)))
    );
    assert!(b.system.running(), "it keeps serving what it has");
    s.shutdown();
}

#[test]
fn cpu_and_memory_sample_only_while_a_reader_is_visible() {
    let rt = Runtime::new();
    let (s, b) = services(&rt, Buses::none());
    b.cpu.acquire(&rt);
    b.memory.acquire(&rt);
    // The first values are there before the first frame.
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let usage = b.cpu.cells().usage.get_untracked(&rt).unwrap();
    assert!((0.0..=1.0).contains(&usage), "{usage}");
    let cores = b.cpu.cells().cores.get_untracked(&rt).unwrap();
    assert!(!cores.is_empty());
    assert!(cores.iter().all(|c| (0.0..=1.0).contains(c)), "{cores:?}");
    let total = b.memory.cells().total.get_untracked(&rt).unwrap();
    let used = b.memory.cells().used.get_untracked(&rt).unwrap();
    assert!(
        total > 0.0 && used > 0.0 && used <= total,
        "{used} of {total}"
    );
    let fraction = b.memory.cells().usage.get_untracked(&rt).unwrap();
    assert!((used / total - fraction).abs() < 1e-9);
    // (services-round) The breakdown, from the same reading.
    let m = b.memory.cells();
    let available = m.available.get_untracked(&rt).unwrap();
    let free = m.free.get_untracked(&rt).unwrap();
    let cached = m.cached.get_untracked(&rt).unwrap();
    assert!(
        (total - available - used).abs() < 1.0,
        "used is what is not available"
    );
    assert!(
        free > 0.0 && free <= available,
        "{free} free of {available}"
    );
    assert!(cached > 0.0 && cached < total, "{cached}");
    let swap_total = m.swap_total.get_untracked(&rt).unwrap();
    let swap_used = m.swap_used.get_untracked(&rt).unwrap();
    assert!(
        (0.0..=swap_total).contains(&swap_used),
        "{swap_used} of {swap_total}"
    );
    // Visible: once a second.
    let start = (cpu::samples(), memory::samples());
    std::thread::sleep(cpu::PERIOD * 2 + Duration::from_millis(500));
    assert!(cpu::samples() >= start.0 + 2, "cpu sampled while visible");
    assert!(memory::samples() >= start.1 + 2);
    // Hidden (the last reader released): no more samples, though the
    // service runs on through its 5 s grace.
    b.cpu.release(&rt);
    b.memory.release(&rt);
    std::thread::sleep(Duration::from_millis(100));
    let hidden = (cpu::samples(), memory::samples());
    std::thread::sleep(cpu::PERIOD * 2 + Duration::from_millis(500));
    assert_eq!((cpu::samples(), memory::samples()), hidden);
    assert!(b.cpu.running() && b.memory.running());
    // Shown again: sampling resumes.
    b.cpu.acquire(&rt);
    std::thread::sleep(cpu::PERIOD + Duration::from_millis(500));
    assert!(cpu::samples() > hidden.0);
    s.shutdown();
}

#[test]
fn python_dbusmock_runs_on_a_private_bus() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(_upower) = DbusMock::start(&bus, "upower", true, None, "org.freedesktop.UPower")
    else {
        return;
    };
    assert!(bus.wait_for_name("org.freedesktop.UPower", Duration::from_secs(1)));
    assert!(!bus.wait_for_name("org.example.Nobody", Duration::from_millis(50)));
}

#[test]
fn the_buses_are_an_error_without_a_tokio_runtime() {
    // A body on a thread of its own that runs no tokio runtime gets an
    // error from `cx.session()`, never a panic.
    let none = Buses::none();
    let fut = strand_services::bus::session(&none);
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Err(e)) => assert!(e.to_string().contains("tokio"), "{e}"),
        other => panic!("{:?}", other.map(|r| r.map(|_| ()))),
    }
}

#[test]
fn a_restarted_bus_is_connected_afresh_and_nothing_installed_activates() {
    let Some(mut bus) = PrivateBus::start() else {
        return;
    };
    let buses = bus.buses();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let a = strand_services::bus::session(&buses).await.unwrap();
        let b = strand_services::bus::session(&buses).await.unwrap();
        assert_eq!(a.unique_name(), b.unique_name(), "one shared connection");
        // The private bus activates nothing the machine has installed.
        let dbus = zbus::fdo::DBusProxy::new(&a).await.unwrap();
        let name = zbus::names::WellKnownName::try_from("ca.desrt.dconf").unwrap();
        assert!(dbus.start_service_by_name(name, 0).await.is_err());
    });
    assert!(bus.restart());
    rt.block_on(async {
        let c = strand_services::bus::session(&buses).await.unwrap();
        // The new connection works (the old one died with its daemon).
        let dbus = zbus::fdo::DBusProxy::new(&c).await.unwrap();
        assert!(dbus.get_id().await.is_ok());
    });
}

/// A portal exposing GSettings' `org.gnome.desktop.interface`.
struct GnomePortal {
    theme: String,
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl GnomePortal {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if namespace != "org.gnome.desktop.interface" || key != "icon-theme" {
            return Err(zbus::fdo::Error::Failed("not found".into()));
        }
        Ok(owned(Value::from(self.theme.as_str())))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

/// The icon theme a GNOME desktop sets through GSettings reaches the
/// icon lookup through the portal: read at start, followed on
/// `SettingChanged` (another key changes nothing), each switch reported
/// once; `$STRAND_ICON_THEME` still wins over it.
#[test]
fn the_portal_icon_theme_is_followed_live() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(
                PATH,
                GnomePortal {
                    theme: "Papirus".into(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let pinned = std::env::var("STRAND_ICON_THEME").is_ok_and(|t| !t.trim().is_empty());
    let switches = Arc::new(Mutex::new(0u32));
    let n = switches.clone();
    let rt = Runtime::new();
    let (services, builtin) = services(&rt, bus.buses());
    // `system` is held for the whole of a run, on the same shared
    // runtime and session connection as the follower.
    builtin.system.acquire(&rt);
    let follower = strand_services::icon_theme::spawn(&services, move || {
        *n.lock().unwrap() += 1;
    });
    assert!(services.runtime_started(), "a task of the shared runtime");
    let threads = || -> Vec<String> {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
            .map(|c| c.trim().to_string())
            .collect()
    };
    assert!(
        !threads().iter().any(|t| t.contains("icon-the")),
        "no thread of its own: {:?}",
        threads()
    );
    let wait = |what: &str, count: u32| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while *switches.lock().unwrap() < count {
            assert!(Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    wait("the boot read", 1);
    if !pinned {
        assert_eq!(strand_icons::system_theme(), "Papirus");
    }
    let emit = |key: &str, v: &str| {
        tokio
            .block_on(conn.emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.portal.Settings",
                "SettingChanged",
                &("org.gnome.desktop.interface", key, Value::from(v)),
            ))
            .unwrap();
    };
    // Another key of the namespace is not the theme.
    emit("gtk-theme", "Other");
    emit("icon-theme", "Breeze");
    wait("the switch", 2);
    if !pinned {
        assert_eq!(strand_icons::system_theme(), "Breeze");
    }
    // The same name again is no switch.
    emit("icon-theme", "Breeze");
    emit("icon-theme", "Adwaita");
    wait("the second switch", 3);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(*switches.lock().unwrap(), 3, "one call per switch");
    drop(follower);
    builtin.system.release(&rt);
    services.shutdown();
    strand_icons::set_desktop_theme(None);
}

/// (gap 34, m6-service-gaps) `system` reads the kernel's release once and
/// counts the uptime in whole minutes, read only while a visible reader
/// shows it, and never more than once a minute: nothing ticks per second,
/// and nothing at all once the reader is gone.
#[test]
fn system_reads_the_kernel_and_counts_uptime_by_the_minute() {
    let rt = Runtime::new();
    let (s, b) = services(&rt, Buses::none());
    let uptime = system::System::FIELDS
        .iter()
        .position(|f| f.name == "uptime")
        .unwrap();
    b.system.acquire(&rt);
    b.system.acquire_field(uptime);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let cells = b.system.cells();
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
    assert_eq!(
        cells.kernel.get_untracked(&rt),
        Ok(release.trim().to_string())
    );
    until(&rt, &s, "the uptime", || {
        cells
            .uptime
            .get_untracked(&rt)
            .is_ok_and(|u| u > Duration::ZERO)
    });
    let up = cells.uptime.get_untracked(&rt).unwrap();
    assert_eq!(up.as_secs() % 60, 0, "whole minutes: {up:?}");
    assert_eq!(up.subsec_nanos(), 0, "whole minutes: {up:?}");
    let booted = std::fs::read_to_string("/proc/uptime").unwrap();
    let secs: f64 = booted.split_whitespace().next().unwrap().parse().unwrap();
    assert!(
        up.as_secs_f64() <= secs && secs - up.as_secs_f64() < 61.0,
        "{up:?} of {secs} s"
    );
    // Shown: at most one wake (a minute turning) in 3 s, and at most one
    // report to the logic thread.
    let (ticks, reports) = (system::uptime_ticks(), b.system.reports());
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        system::uptime_ticks() <= ticks + 1,
        "{} wakes in 3 s",
        system::uptime_ticks() - ticks
    );
    assert!(
        b.system.reports() <= reports + 1,
        "{} reports in 3 s",
        b.system.reports() - reports
    );
    // Nobody shows it: the timer is off, though `system` still runs.
    b.system.release_field(uptime);
    s.pump(&rt);
    std::thread::sleep(Duration::from_millis(100));
    let ticks = system::uptime_ticks();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(system::uptime_ticks(), ticks, "no wakes while unread");
    assert!(b.system.running());
    s.shutdown();
}

/// (gap 33, m6-service-gaps) `disk` measures the real root filesystem
/// before its first frame, lists it first in `all`, and samples only
/// while a reader is visible: none once the reader is gone, though the
/// service runs on through its grace.
#[test]
fn disk_reads_the_root_filesystem_only_while_visible() {
    disk::configure(Some(disk::Config {
        period: Duration::from_millis(100),
        ..disk::Config::default()
    }));
    let rt = Runtime::new();
    let (s, b) = services(&rt, Buses::none());
    b.disk.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let cells = b.disk.cells();
    let total = cells.total.get_untracked(&rt).unwrap();
    let used = cells.used.get_untracked(&rt).unwrap();
    let free = cells.free.get_untracked(&rt).unwrap();
    let usage = cells.usage.get_untracked(&rt).unwrap();
    assert!(total > 0.0, "{total}");
    assert!(used <= total && free <= total, "{used} {free} of {total}");
    assert!((0.0..=1.0).contains(&usage), "{usage}");
    let all: Vec<disk::Filesystem> = cells
        .all
        .get_untracked(&rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, f)| f.clone())
        .collect();
    assert_eq!(all.first().map(|f| f.path.as_str()), Some("/"), "{all:?}");
    assert!(all.iter().all(|f| f.total > 0.0), "{all:?}");
    assert_eq!(all[0].total, total);
    // Visible: sampled every period.
    let start = disk::samples();
    std::thread::sleep(Duration::from_millis(700));
    assert!(disk::samples() >= start + 3, "sampled while visible");
    // Released: no more samples, though the service runs on (5 s grace).
    b.disk.release(&rt);
    s.pump(&rt);
    std::thread::sleep(Duration::from_millis(300));
    let hidden = disk::samples();
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(disk::samples(), hidden, "no samples while hidden");
    assert!(b.disk.running());
    // Shown again: sampled at once, and on.
    b.disk.acquire(&rt);
    std::thread::sleep(Duration::from_millis(500));
    assert!(disk::samples() > hidden, "sampling resumes");
    disk::configure(None);
    s.shutdown();
}

/// Write fake kernel counters under `root`: `eth0` (hardware) and
/// `veth0` (a container's link), and loopback.
fn counters(root: &std::path::Path, eth: (u64, u64), veth: (u64, u64)) {
    let dev = format!(
        "Inter-|   Receive |  Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes\n\
             lo: 5000 1 0 0 0 0 0 0 5000 1 0 0 0 0 0 0\n\
           eth0: {} 1 0 0 0 0 0 0 {} 1 0 0 0 0 0 0\n\
          veth0: {} 1 0 0 0 0 0 0 {} 1 0 0 0 0 0 0\n",
        eth.0, eth.1, veth.0, veth.1
    );
    let tmp = root.join("proc/net/dev.tmp");
    std::fs::write(&tmp, dev).unwrap();
    std::fs::rename(&tmp, root.join("proc/net/dev")).unwrap();
}

/// (services-round) `network.rx` and `network.tx` are the bytes a second
/// through the machine's hardware, from the kernel's counters with no
/// daemon or bus at all: a container's link and loopback never count.
/// Read only while a visible reader reads them, once a second; nothing
/// is read while only the rest of `network` is shown.
#[test]
fn network_rates_count_the_hardware_only_while_read() {
    let root = std::env::temp_dir().join(format!("strand-rates-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("proc/net")).unwrap();
    std::fs::create_dir_all(root.join("sys/class/net/eth0/device")).unwrap();
    std::fs::create_dir_all(root.join("sys/class/net/veth0")).unwrap();
    std::fs::create_dir_all(root.join("sys/class/net/lo")).unwrap();
    counters(&root, (1_000, 2_000), (0, 0));
    network::rates::configure_root(Some(root.clone()));
    let rt = Runtime::new();
    let (s, b) = services(&rt, Buses::none());
    let field = |name: &str| {
        network::Network::FIELDS
            .iter()
            .position(|f| f.name == name)
            .unwrap()
    };
    // Shown, but nobody reads the rates: nothing is read.
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let before = network::rates::samples();
    std::thread::sleep(network::rates::PERIOD * 2 + Duration::from_millis(300));
    s.pump(&rt);
    assert_eq!(network::rates::samples(), before, "no reader of the rates");
    // Read: a baseline at once, then once a second.
    b.network.acquire_field(field("rx"));
    b.network.acquire_field(field("tx"));
    let started = Instant::now();
    until(&rt, &s, "the baseline", || {
        network::rates::samples() > before
    });
    // 1 MB in, 0.5 MB out through eth0, and 50 MB through the
    // container's link, before the first sample.
    counters(&root, (1_001_000, 502_000), (50_000_000, 50_000_000));
    let cells = b.network.cells();
    until(&rt, &s, "a rate", || {
        cells.rx.get_untracked(&rt).is_ok_and(|r| r > 0.0)
    });
    let secs = started.elapsed().as_secs_f64();
    let rx = cells.rx.get_untracked(&rt).unwrap();
    let tx = cells.tx.get_untracked(&rt).unwrap();
    // One second's sample (or a little more on a loaded runner).
    assert!(
        rx <= 1_000_000.0 * 1.05 && rx >= 1_000_000.0 / secs.max(1.0) * 0.9,
        "rx {rx} after {secs} s"
    );
    assert!((tx / rx - 0.5).abs() < 1e-9, "tx {tx} of rx {rx}");
    // No more bytes: the next sample says 0.
    until(&rt, &s, "an idle second", || {
        cells.rx.get_untracked(&rt) == Ok(0.0) && cells.tx.get_untracked(&rt) == Ok(0.0)
    });
    // Unread again: no more reads.
    b.network.release_field(field("rx"));
    b.network.release_field(field("tx"));
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    let unread = network::rates::samples();
    std::thread::sleep(network::rates::PERIOD * 2 + Duration::from_millis(300));
    assert_eq!(network::rates::samples(), unread, "nobody reads the rates");
    // Hidden but kept by a graph's history: sampled at the kept period,
    // and said to be polled.
    b.network.release(&rt);
    b.network.keep(&rt, Some(Duration::from_secs(2)));
    std::thread::sleep(Duration::from_millis(100));
    s.pump(&rt);
    let kept = network::rates::samples();
    std::thread::sleep(Duration::from_millis(4_600));
    s.pump(&rt);
    let n = network::rates::samples() - kept;
    assert!((1..=3).contains(&n), "{n} samples in 4.6 s kept at 2 s");
    assert_eq!(b.network.sampled(), Some(Duration::from_secs(2)));
    b.network.keep(&rt, None);
    s.shutdown();
    network::rates::configure_root(None);
    let _ = std::fs::remove_dir_all(&root);
}

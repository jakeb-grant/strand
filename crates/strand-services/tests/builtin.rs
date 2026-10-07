//! The builtin services on the contract: `system` against a mock portal
//! on a private bus, `cpu` and `memory` from procfs (sampled only while a
//! reader is visible), and the test harness itself (private bus,
//! python-dbusmock).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Builtin, Buses, Rgba, Services, cpu, memory};
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

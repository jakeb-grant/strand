//! `battery` against python-dbusmock's `upower` template on a private
//! bus: the display device and devices read at start, changes on the
//! mock followed, and UPower restarting followed without the service
//! restarting.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::battery::UPOWER;
use strand_services::testing::{DbusMock, PrivateBus};
use support::*;
use zbus::zvariant::Value;

const ROOT: &str = "/org/freedesktop/UPower";
const DISPLAY: &str = "/org/freedesktop/UPower/devices/DisplayDevice";

/// Start the upower mock with a discharging display device at
/// `percent` and one battery.
fn upower(
    tokio: &tokio::runtime::Runtime,
    bus: &PrivateBus,
    conn: &zbus::Connection,
    percent: f64,
) -> Option<DbusMock> {
    let m = DbusMock::start(bus, "upower", true, None, UPOWER)?;
    // Type battery, state discharging, percent, energy, full, rate,
    // to empty, to full, present, icon, warning level.
    mock(
        tokio,
        conn,
        UPOWER,
        ROOT,
        "SetupDisplayDevice",
        &(
            2u32, 2u32, percent, percent, 100.0f64, 12.5f64, 5400i64, 0i64, true, "", 1u32,
        ),
    );
    mock(
        tokio,
        conn,
        UPOWER,
        ROOT,
        "AddDischargingBattery",
        &("mock_BAT", "Mock Battery", percent, 5400i64),
    );
    Some(m)
}

#[test]
fn battery_follows_upower_and_its_restarts() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(mock1) = upower(&tokio, &bus, &conn, 50.0) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.battery.cells();
    let changes = Arc::new(Mutex::new(Vec::new()));
    let c = changes.clone();
    let percent = cells.percent;
    let _watch = rt.on_change(
        move |rt| percent.get(rt),
        move |_, v: &f64| {
            c.lock().unwrap().push(*v);
            Ok(())
        },
    );
    rt.flush();
    b.battery.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the boot read");
    rt.flush();
    assert_eq!(cells.present.get_untracked(&rt), Ok(true));
    assert_eq!(cells.percent.get_untracked(&rt), Ok(0.5));
    assert_eq!(cells.charging.get_untracked(&rt), Ok(false));
    assert_eq!(
        cells.time_left.get_untracked(&rt),
        Ok(Some(Duration::from_secs(5400)))
    );
    assert_eq!(cells.power.get_untracked(&rt), Ok(12.5));
    assert_eq!(
        cells.icon.get_untracked(&rt).unwrap(),
        "battery-level-50-symbolic"
    );
    let devices = b.battery.cells().devices.get_untracked(&rt).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(
        devices.items()[0].1.id,
        "/org/freedesktop/UPower/devices/mock_BAT"
    );
    assert_eq!(devices.items()[0].1.name, "Mock Battery");
    assert_eq!(devices.items()[0].1.kind, "battery");
    assert!(
        changes.lock().unwrap().is_empty(),
        "boot values are not changes"
    );

    // The charge drops on the mock: a change.
    mock(
        &tokio,
        &conn,
        UPOWER,
        ROOT,
        "SetDeviceProperties",
        &(
            zbus::zvariant::ObjectPath::try_from(DISPLAY).unwrap(),
            HashMap::from([("Percentage", Value::from(30.0f64))]),
        ),
    );
    until(&rt, &s, "30%", || {
        cells.percent.get_untracked(&rt) == Ok(0.3)
    });
    assert_eq!(*changes.lock().unwrap(), [0.3]);
    // Plugged in.
    mock(
        &tokio,
        &conn,
        UPOWER,
        ROOT,
        "SetDeviceProperties",
        &(
            zbus::zvariant::ObjectPath::try_from(DISPLAY).unwrap(),
            HashMap::from([
                ("State", Value::from(1u32)),
                ("TimeToFull", Value::from(1800i64)),
            ]),
        ),
    );
    until(&rt, &s, "charging", || {
        cells.charging.get_untracked(&rt) == Ok(true)
            && cells.time_left.get_untracked(&rt) == Ok(Some(Duration::from_secs(1800)))
    });
    assert_eq!(
        cells.icon.get_untracked(&rt).unwrap(),
        "battery-level-30-charging-symbolic"
    );
    // A peripheral appears, then goes.
    let path = mock(
        &tokio,
        &conn,
        UPOWER,
        ROOT,
        "AddChargingBattery",
        &("mouse_dev", "Mouse", 80.0f64, 600i64),
    );
    let path: String = path.body().deserialize().unwrap();
    until(&rt, &s, "a second device", || {
        b.battery.cells().devices.get_untracked(&rt).unwrap().len() == 2
    });
    mock(
        &tokio,
        &conn,
        UPOWER,
        ROOT,
        "RemoveDevice",
        &(zbus::zvariant::ObjectPath::try_from(path.as_str()).unwrap(),),
    );
    until(&rt, &s, "the device gone", || {
        b.battery.cells().devices.get_untracked(&rt).unwrap().len() == 1
    });

    // UPower goes away: no battery, the service still running.
    drop(mock1);
    until(&rt, &s, "no battery", || {
        cells.present.get_untracked(&rt) == Ok(false)
    });
    assert!(b.battery.running());
    // It comes back (a restart): read afresh, no shell reload, no
    // service restart.
    let Some(_mock2) = upower(&tokio, &bus, &conn, 80.0) else {
        return;
    };
    until(&rt, &s, "the new UPower", || {
        cells.present.get_untracked(&rt) == Ok(true) && cells.percent.get_untracked(&rt) == Ok(0.8)
    });
    assert_eq!(b.battery.starts(), 1, "followed, not restarted");
    s.shutdown();
}

#[test]
fn without_upower_there_is_no_battery() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.battery.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(b.battery.cells().present.get_untracked(&rt), Ok(false));
    assert!(b.battery.running(), "it waits for UPower to appear");
    s.shutdown();
}

/// UPower started by D-Bus activation, as distributions often ship it
/// (`SystemdService=upower.service`): nothing runs it until someone
/// calls; the battery service asks for it once and follows it in.
#[test]
fn battery_starts_an_activatable_upower() {
    let Some(bus) = PrivateBus::start_activating(&[("upower", true, UPOWER)]) else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    assert!(
        !bus.wait_for_name(UPOWER, Duration::from_millis(200)),
        "not running before it is asked for"
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.battery.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(
        bus.wait_for_name(UPOWER, Duration::from_secs(15)),
        "the battery service had UPower started"
    );
    // The activated UPower's display device appears: followed in.
    mock(
        &tokio,
        &conn,
        UPOWER,
        ROOT,
        "SetupDisplayDevice",
        &(
            2u32, 2u32, 77.0f64, 77.0f64, 100.0f64, 12.5f64, 5400i64, 0i64, true, "", 1u32,
        ),
    );
    until(&rt, &s, "the activated UPower's battery", || {
        b.battery.cells().percent.get_untracked(&rt) == Ok(0.77)
            && b.battery.cells().present.get_untracked(&rt) == Ok(true)
    });
    assert_eq!(b.battery.starts(), 1);
    s.shutdown();
    // The activated mock is the bus's child: stop it with the test.
    let dbus = tokio.block_on(zbus::fdo::DBusProxy::new(&conn)).unwrap();
    if let Ok(pid) = tokio.block_on(dbus.get_connection_unix_process_id(UPOWER.try_into().unwrap()))
    {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }
}

/// The shared system bus connection lives only while a running body uses
/// it: `battery` stopping closes it although `cpu` (on the same services
/// thread, using no bus) runs on.
#[test]
fn a_shared_connection_closes_with_its_last_user() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_mock) = upower(&tokio, &bus, &conn, 50.0) else {
        return;
    };
    let dbus = tokio.block_on(zbus::fdo::DBusProxy::new(&conn)).unwrap();
    let peers = || {
        tokio
            .block_on(dbus.list_names())
            .unwrap()
            .iter()
            .filter(|n| n.starts_with(':'))
            .count()
    };
    let before = peers();
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.cpu.acquire(&rt);
    b.battery.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(b.battery.cells().present.get_untracked(&rt).unwrap());
    assert_eq!(peers(), before + 1, "one shared connection");
    // Nobody reads the battery: it stops after the grace, and its
    // connection goes with it; cpu goes on.
    b.battery.release(&rt);
    rt.tick(strand_services::STOP_GRACE + Duration::from_millis(1));
    until(&rt, &s, "battery stopped", || !b.battery.running());
    assert!(b.cpu.running());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while peers() != before {
        assert!(
            std::time::Instant::now() < deadline,
            "the system bus connection stayed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Read again: connected afresh.
    b.battery.acquire(&rt);
    until(&rt, &s, "battery again", || {
        b.battery.running() && peers() == before + 1
    });
    s.shutdown();
}

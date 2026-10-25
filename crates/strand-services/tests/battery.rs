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

/// (services-round) `battery.history(span, points)`: UPower's
/// `GetHistory("charge", …)` of the laptop battery, asked only when
/// called, resampled to even points oldest first; with no battery that
/// keeps one, an error.
#[test]
fn history_asks_upower_only_when_called() {
    use strand_services::{Data, FromData};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_mock) = upower(&tokio, &bus, &conn, 50.0) else {
        return;
    };
    let bat = "/org/freedesktop/UPower/devices/mock_BAT";
    // Charge 80 % an hour ago, 60 % half an hour ago, 50 % a minute ago,
    // newest first as UPower answers; one record of unknown state.
    call(
        &tokio,
        &conn,
        UPOWER,
        bat,
        "org.freedesktop.DBus.Mock",
        "AddMethod",
        &(
            "org.freedesktop.UPower.Device",
            "GetHistory",
            "suu",
            "a(udu)",
            "t = int(__import__('time').time()); \
             ret = [(t - 60, 50.0, 2), (t - 1700, 10.0, 0), (t - 1800, 60.0, 2), (t - 3600, 80.0, 2)]",
        ),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.battery.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(
        mock_calls(&tokio, &conn, UPOWER, bat, "GetHistory").is_empty(),
        "nothing asked before a call"
    );
    let d = b.battery.dynamic();
    let ask = |args: &[Data]| -> Result<Vec<f64>, String> {
        let fut = d.fetch(&rt, "history", args);
        let r = tokio.block_on(async {
            let mut fut = fut;
            loop {
                tokio::select! {
                    r = &mut fut => return r,
                    () = tokio::time::sleep(Duration::from_millis(5)) => {
                        s.pump(&rt);
                    }
                }
            }
        });
        r.map(|v| Vec::<f64>::from_data(&v).unwrap())
    };
    let hour = Data::Duration(Duration::from_secs(3600));
    let got = ask(&[hour.clone(), Data::Int(3)]).unwrap();
    // An hour ago, half an hour ago, now (within the second the mock
    // answers in).
    assert_eq!(got, [0.8, 0.6, 0.5]);
    let calls = mock_calls(&tokio, &conn, UPOWER, bat, "GetHistory");
    assert_eq!(calls.len(), 1);
    assert_eq!(<&str>::try_from(&calls[0][0]).unwrap(), "charge");
    assert_eq!(u32::try_from(&calls[0][1]).unwrap(), 3600);
    // `points` left out: 60 points, the earliest an hour ago.
    let got = ask(&[hour.clone(), Data::Null]).unwrap();
    assert_eq!(got.len(), 60);
    assert_eq!((got[0], got[59]), (0.8, 0.5));
    // Two hours: the hour before the first record is left out.
    let got = ask(&[Data::Duration(Duration::from_secs(7200)), Data::Int(5)]).unwrap();
    assert_eq!(got, [0.8, 0.6, 0.5]);

    // No battery keeps a history: an error, not a hang.
    mock(
        &tokio,
        &conn,
        UPOWER,
        ROOT,
        "RemoveDevice",
        &(zbus::zvariant::ObjectPath::try_from(bat).unwrap(),),
    );
    until(&rt, &s, "the battery gone", || {
        b.battery
            .cells()
            .devices
            .get_untracked(&rt)
            .unwrap()
            .is_empty()
    });
    let err = ask(&[hour, Data::Int(3)]).unwrap_err();
    assert!(err.contains("no battery keeps a history"), "{err}");
    s.shutdown();
}

/// (services-round) With its bus disabled, `history` answers why.
#[test]
fn without_a_bus_history_says_why() {
    use strand_services::Data;
    let rt = Runtime::new();
    let (s, b) = services(&rt, strand_services::Buses::none());
    b.battery.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let tokio = tokio();
    let fut = b.battery.dynamic().fetch(
        &rt,
        "history",
        &[Data::Duration(Duration::from_secs(60)), Data::Null],
    );
    let r = tokio.block_on(async {
        let mut fut = fut;
        loop {
            tokio::select! {
                r = &mut fut => return r,
                () = tokio::time::sleep(Duration::from_millis(5)) => {
                    s.pump(&rt);
                }
            }
        }
    });
    assert_eq!(r, Err("no system bus".to_string()));
    s.shutdown();
}

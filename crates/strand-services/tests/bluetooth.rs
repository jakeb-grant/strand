//! `bluetooth` against python-dbusmock's `bluez5` template on a private
//! bus: the adapter's power (read and written), paired devices, a device
//! connecting from the mock and through the service's actions, and BlueZ
//! restarting.

mod support;

use std::time::Duration;

use strand_core::Runtime;
use strand_services::bluetooth::{BLUEZ, BluetoothDevice};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Data, ToData};
use support::*;

const ADDRESS: &str = "11:22:33:44:55:66";

fn bluez(
    tokio: &tokio::runtime::Runtime,
    bus: &PrivateBus,
    conn: &zbus::Connection,
) -> Option<(DbusMock, String)> {
    let m = DbusMock::start(bus, "bluez5", true, None, BLUEZ)?;
    let mock_iface = |method: &str, body: &(&str, &str)| {
        call(tokio, conn, BLUEZ, "/", "org.bluez.Mock", method, body)
    };
    mock_iface("AddAdapter", &("hci0", "my-laptop"));
    let dev = call(
        tokio,
        conn,
        BLUEZ,
        "/",
        "org.bluez.Mock",
        "AddDevice",
        &("hci0", ADDRESS, "Headphones"),
    );
    let dev: String = dev.body().deserialize().unwrap();
    // An unpaired device in range is not listed.
    call(
        tokio,
        conn,
        BLUEZ,
        "/",
        "org.bluez.Mock",
        "AddDevice",
        &("hci0", "AA:BB:CC:DD:EE:FF", "Stranger"),
    );
    mock_iface("PairDevice", &("hci0", ADDRESS));
    Some((m, dev))
}

fn devices(b: &strand_services::Builtin, rt: &Runtime) -> Vec<BluetoothDevice> {
    b.bluetooth
        .cells()
        .devices
        .get_untracked(rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, d)| d.clone())
        .collect()
}

#[test]
fn bluetooth_follows_bluez() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some((mock1, dev)) = bluez(&tokio, &bus, &conn) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.bluetooth.cells();
    b.bluetooth.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.powered.get_untracked(&rt), Ok(true));
    let d = devices(&b, &rt);
    assert_eq!(d.len(), 1, "only the paired device: {d:?}");
    assert_eq!(d[0].address, ADDRESS);
    assert_eq!(d[0].name, "Headphones");
    assert!(!d[0].connected);
    assert_eq!(d[0].battery, None);

    // The shell connects it: BlueZ's `Connect` (the mock reports it).
    b.bluetooth
        .dynamic()
        .action(&rt, "connect", Some(&d[0].to_data()), &[])
        .unwrap();
    until(&rt, &s, "connected", || devices(&b, &rt)[0].connected);
    b.bluetooth
        .dynamic()
        .action(&rt, "disconnect", Some(&d[0].to_data()), &[])
        .unwrap();
    until(&rt, &s, "disconnected", || !devices(&b, &rt)[0].connected);

    // BlueZ refuses a connect (the headphones are off): `failed` says so.
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let f = failures.clone();
    b.bluetooth.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a {
            f.lock().unwrap().push(args.clone());
        }
    }));
    call(
        &tokio,
        &conn,
        BLUEZ,
        &dev,
        "org.freedesktop.DBus.Mock",
        "AddMethod",
        &(
            "org.bluez.Device1",
            "Connect",
            "",
            "",
            "raise dbus.exceptions.DBusException('Page Timeout', name='org.bluez.Error.Failed')",
        ),
    );
    b.bluetooth
        .dynamic()
        .action(&rt, "connect", Some(&d[0].to_data()), &[])
        .unwrap();
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        [
            Data::Text(ADDRESS.into()),
            Data::Text("Page Timeout".into())
        ]
    );
    assert!(!devices(&b, &rt)[0].connected);

    // It reports a battery (an interface added).
    let battery =
        std::collections::HashMap::from([("Percentage", zbus::zvariant::Value::from(70u8))]);
    call(
        &tokio,
        &conn,
        BLUEZ,
        &dev,
        "org.freedesktop.DBus.Mock",
        "AddProperties",
        &("org.bluez.Battery1", battery),
    );
    let ifaces = std::collections::HashMap::from([(
        "org.bluez.Battery1",
        std::collections::HashMap::from([("Percentage", zbus::zvariant::Value::from(70u8))]),
    )]);
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(
                zbus::zvariant::ObjectPath::try_from(dev.as_str()).unwrap(),
                ifaces,
            ),
        ))
        .unwrap();
    // A signal from someone other than BlueZ is ignored.
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(devices(&b, &rt)[0].battery, None, "not BlueZ's signal");
    // BlueZ's own.
    let ifaces = std::collections::HashMap::from([(
        "org.bluez.Battery1",
        std::collections::HashMap::from([("Percentage", zbus::zvariant::Value::from(70u8))]),
    )]);
    call(
        &tokio,
        &conn,
        BLUEZ,
        "/",
        "org.freedesktop.DBus.Mock",
        "EmitSignal",
        &(
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            "oa{sa{sv}}",
            vec![
                zbus::zvariant::Value::from(
                    zbus::zvariant::ObjectPath::try_from(dev.as_str()).unwrap(),
                ),
                zbus::zvariant::Value::from(ifaces),
            ],
        ),
    );
    until(&rt, &s, "its battery", || {
        devices(&b, &rt)[0].battery == Some(0.7)
    });

    // The shell powers the adapter off.
    b.bluetooth
        .dynamic()
        .write(&rt, 0, &[], Data::Bool(false))
        .unwrap();
    rt.flush();
    until(&rt, &s, "powered off on the mock", || {
        let v: zbus::zvariant::OwnedValue = call(
            &tokio,
            &conn,
            BLUEZ,
            "/org/bluez/hci0",
            "org.freedesktop.DBus.Properties",
            "Get",
            &("org.bluez.Adapter1", "Powered"),
        )
        .body()
        .deserialize()
        .unwrap();
        v == zbus::zvariant::OwnedValue::from(false)
    });
    assert_eq!(cells.powered.get_untracked(&rt), Ok(false));

    // BlueZ goes and comes back: read afresh, no service restart.
    drop(mock1);
    until(&rt, &s, "no adapter", || devices(&b, &rt).is_empty());
    let Some((_mock2, _)) = bluez(&tokio, &bus, &conn) else {
        return;
    };
    until(&rt, &s, "BlueZ back", || {
        devices(&b, &rt).len() == 1 && cells.powered.get_untracked(&rt) == Ok(true)
    });
    assert_eq!(b.bluetooth.starts(), 1);
    s.shutdown();
}

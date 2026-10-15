//! `bluetooth` against python-dbusmock's `bluez5` template on a private
//! bus: the adapter's power (read and written), paired devices, a device
//! connecting from the mock and through the service's actions, and BlueZ
//! restarting.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::bluetooth::{BLUEZ, BluetoothDevice};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Data, Store, ToData};
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
    assert_eq!(cells.available.get_untracked(&rt), Ok(true));
    let d = devices(&b, &rt);
    assert_eq!(d.len(), 1, "only the paired device: {d:?}");
    assert_eq!(d[0].address, ADDRESS);
    assert_eq!(d[0].name, "Headphones");
    assert!(!d[0].connected);
    assert_eq!(d[0].battery, None);

    // The shell connects it: BlueZ's `Connect` (the mock reports it).
    b.bluetooth
        .dynamic()
        .action(
            &rt,
            "connect",
            Some(&d[0].to_data()),
            &[],
            Default::default(),
        )
        .unwrap();
    until(&rt, &s, "connected", || devices(&b, &rt)[0].connected);
    b.bluetooth
        .dynamic()
        .action(
            &rt,
            "disconnect",
            Some(&d[0].to_data()),
            &[],
            Default::default(),
        )
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
        .action(
            &rt,
            "connect",
            Some(&d[0].to_data()),
            &[],
            Default::default(),
        )
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
    // (M6, owner-testing gap 20) Without BlueZ it is unavailable.
    until(&rt, &s, "unavailable", || {
        cells.available.get_untracked(&rt) == Ok(false)
    });
    let Some((_mock2, _)) = bluez(&tokio, &bus, &conn) else {
        return;
    };
    until(&rt, &s, "BlueZ back", || {
        devices(&b, &rt).len() == 1 && cells.powered.get_untracked(&rt) == Ok(true)
    });
    assert_eq!(cells.available.get_untracked(&rt), Ok(true));
    assert_eq!(b.bluetooth.starts(), 1);
    s.shutdown();
}

const ADAPTER_PATH: &str = "/org/bluez/hci0";
const STRANGER: &str = "AA:BB:CC:DD:EE:FF";

fn nearby(b: &strand_services::Builtin, rt: &Runtime) -> Vec<BluetoothDevice> {
    b.bluetooth
        .cells()
        .nearby
        .get_untracked(rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, d)| d.clone())
        .collect()
}

fn names(list: &[BluetoothDevice]) -> Vec<String> {
    list.iter().map(|d| d.name.clone()).collect()
}

fn field(name: &str) -> usize {
    strand_services::bluetooth::Bluetooth::FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap()
}

/// `d.<name>()` of the device at `address`.
fn act(rt: &Runtime, b: &strand_services::Builtin, name: &str, address: &str) {
    let item = BluetoothDevice {
        address: address.into(),
        ..BluetoothDevice::default()
    };
    b.bluetooth
        .dynamic()
        .action(rt, name, Some(&item.to_data()), &[], Default::default())
        .unwrap();
}

/// The `failed(address, error)`s the service emits.
fn failures_of(b: &strand_services::Builtin) -> Arc<Mutex<Vec<(String, String)>>> {
    let failures = Arc::new(Mutex::new(Vec::new()));
    let f = failures.clone();
    b.bluetooth.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a
            && let [Data::Text(address), Data::Text(error)] = args.as_slice()
        {
            f.lock()
                .unwrap()
                .push((address.to_string(), error.to_string()));
        }
    }));
    failures
}

/// Add a device to the mock's adapter: its path.
fn add_device(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    address: &str,
    name: &str,
) -> String {
    call(
        tokio,
        conn,
        BLUEZ,
        "/",
        "org.bluez.Mock",
        "AddDevice",
        &("hci0", address, name),
    )
    .body()
    .deserialize()
    .unwrap()
}

/// Replace the mock's `method` on `path` with python `code`.
fn override_method(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    path: &str,
    method: &str,
    code: &str,
) {
    mock(
        tokio,
        conn,
        BLUEZ,
        path,
        "AddMethod",
        &("org.bluez.Device1", method, "", "", code),
    );
}

/// A device method that takes `secs` and then says `Connected` is `on`.
fn slow_connection(secs: f64, on: bool) -> String {
    let on = if on { "True" } else { "False" };
    format!(
        "time.sleep({secs})\n\
         self.props['org.bluez.Device1']['Connected'] = dbus.Boolean({on}, variant_level=1)\n\
         self.EmitSignal('org.freedesktop.DBus.Properties', 'PropertiesChanged', 'sa{{sv}}as', \
         ['org.bluez.Device1', {{'Connected': dbus.Boolean({on}, variant_level=1)}}, []])"
    )
}

fn adapter_calls(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    method: &str,
) -> Vec<Vec<zbus::zvariant::OwnedValue>> {
    mock_calls(tokio, conn, BLUEZ, ADAPTER_PATH, method)
}

/// (m6-service-gaps, gap 29) `nearby` is the named unpaired devices in
/// range, found by a discovery that runs only while it is read (a filter
/// without duplicate data, then `StartDiscovery`; `StopDiscovery` when the
/// reader goes); the adapter powering on again starts it anew.
#[test]
fn bluetooth_finds_nearby_devices_only_while_read() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some((_mock, _)) = bluez(&tokio, &bus, &conn) else {
        return;
    };
    // An advertiser that gives no name is not listed.
    add_device(&tokio, &conn, "12:34:56:78:9A:BC", "");
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.bluetooth.cells();
    b.bluetooth.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert!(nearby(&b, &rt).is_empty(), "nobody reads it");
    assert_eq!(cells.scanning.get_untracked(&rt), Ok(false));
    assert!(adapter_calls(&tokio, &conn, "StartDiscovery").is_empty());

    b.bluetooth.acquire_field(field("nearby"));
    until(&rt, &s, "Stranger nearby", || {
        names(&nearby(&b, &rt)) == ["Stranger"] && cells.scanning.get_untracked(&rt) == Ok(true)
    });
    let filters = adapter_calls(&tokio, &conn, "SetDiscoveryFilter");
    assert_eq!(filters.len(), 1);
    let filter: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        filters[0][0].try_clone().unwrap().try_into().unwrap();
    assert_eq!(
        filter
            .get("DuplicateData")
            .map(|v| bool::try_from(v).unwrap()),
        Some(false)
    );
    assert_eq!(adapter_calls(&tokio, &conn, "StartDiscovery").len(), 1);
    assert_eq!(
        names(&devices(&b, &rt)),
        ["Headphones"],
        "paired, not nearby"
    );

    // A device comes into range.
    add_device(&tokio, &conn, "DE:AD:BE:EF:00:01", "Speaker");
    until(&rt, &s, "Speaker nearby", || {
        names(&nearby(&b, &rt)) == ["Speaker", "Stranger"]
    });

    // The adapter off and on again: discovery again.
    let powered = field("powered");
    b.bluetooth
        .dynamic()
        .write(&rt, powered, &[], Data::Bool(false))
        .unwrap();
    until(&rt, &s, "powered off", || {
        cells.powered.get_untracked(&rt) == Ok(false)
    });
    b.bluetooth
        .dynamic()
        .write(&rt, powered, &[], Data::Bool(true))
        .unwrap();
    until(&rt, &s, "discovery again", || {
        adapter_calls(&tokio, &conn, "StartDiscovery").len() == 2
    });

    // The reader goes: the discovery stops, nothing is listed.
    b.bluetooth.release_field(field("nearby"));
    until(&rt, &s, "nothing nearby", || {
        nearby(&b, &rt).is_empty() && cells.scanning.get_untracked(&rt) == Ok(false)
    });
    assert_eq!(adapter_calls(&tokio, &conn, "StopDiscovery").len(), 1);
    assert_eq!(adapter_calls(&tokio, &conn, "StartDiscovery").len(), 2);
    s.shutdown();
}

/// (gap 29) `d.pair()` pairs, trusts and connects a nearby device, which
/// is `connecting` throughout and paired after; a pairing BlueZ refuses
/// fails saying why, and the device stays nearby.
#[test]
fn pairing_a_nearby_device_pairs_trusts_and_connects_it() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some((_mock, _)) = bluez(&tokio, &bus, &conn) else {
        return;
    };
    let stranger = format!("{ADAPTER_PATH}/dev_AA_BB_CC_DD_EE_FF");
    override_method(
        &tokio,
        &conn,
        &stranger,
        "Connect",
        &slow_connection(1.5, true),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.bluetooth.acquire(&rt);
    b.bluetooth.acquire_field(field("nearby"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "Stranger nearby", || {
        names(&nearby(&b, &rt)) == ["Stranger"]
    });

    act(&rt, &b, "pair", STRANGER);
    until(&rt, &s, "Stranger paired, connecting", || {
        devices(&b, &rt)
            .iter()
            .any(|d| d.address == STRANGER && d.connecting && !d.connected)
    });
    assert!(nearby(&b, &rt).is_empty(), "paired, no longer nearby");
    until(&rt, &s, "Stranger connected", || {
        devices(&b, &rt)
            .iter()
            .any(|d| d.address == STRANGER && d.connected && !d.connecting)
    });
    let trusted: zbus::zvariant::OwnedValue = call(
        &tokio,
        &conn,
        BLUEZ,
        &stranger,
        "org.freedesktop.DBus.Properties",
        "Get",
        &("org.bluez.Device1", "Trusted"),
    )
    .body()
    .deserialize()
    .unwrap();
    assert_eq!(trusted, zbus::zvariant::OwnedValue::from(true));
    assert_eq!(mock_calls(&tokio, &conn, BLUEZ, &stranger, "Pair").len(), 1);
    assert_eq!(
        mock_calls(&tokio, &conn, BLUEZ, &stranger, "Connect").len(),
        1
    );
    assert!(failures.lock().unwrap().is_empty());

    // Pairing refused: it fails, saying why; the device stays nearby.
    let other = add_device(&tokio, &conn, "DE:AD:BE:EF:00:02", "Keyboard");
    override_method(
        &tokio,
        &conn,
        &other,
        "Pair",
        "raise dbus.exceptions.DBusException('Authentication Failed', \
         name='org.bluez.Error.AuthenticationFailed')",
    );
    until(&rt, &s, "Keyboard nearby", || {
        names(&nearby(&b, &rt)) == ["Keyboard"]
    });
    act(&rt, &b, "pair", "DE:AD:BE:EF:00:02");
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        (
            "DE:AD:BE:EF:00:02".to_string(),
            "Authentication Failed".to_string()
        )
    );
    until(&rt, &s, "Keyboard idle", || {
        nearby(&b, &rt)
            .iter()
            .any(|d| d.name == "Keyboard" && !d.connecting)
    });
    assert!(mock_calls(&tokio, &conn, BLUEZ, &other, "Connect").is_empty());
    s.shutdown();
}

/// (gap 29) `d.disconnect()` shows the device `disconnecting` until
/// BlueZ answers; `d.forget()` removes it from the adapter
/// (`RemoveDevice`), and it is gone; a device BlueZ does not have fails.
#[test]
fn disconnecting_and_forgetting_a_paired_device() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some((_mock, dev)) = bluez(&tokio, &bus, &conn) else {
        return;
    };
    override_method(
        &tokio,
        &conn,
        &dev,
        "Disconnect",
        &slow_connection(1.0, false),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.bluetooth.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    act(&rt, &b, "connect", ADDRESS);
    until(&rt, &s, "connected", || devices(&b, &rt)[0].connected);
    act(&rt, &b, "disconnect", ADDRESS);
    until(&rt, &s, "disconnecting", || {
        let d = &devices(&b, &rt)[0];
        d.disconnecting && d.connected && !d.connecting
    });
    until(&rt, &s, "disconnected", || {
        let d = &devices(&b, &rt)[0];
        !d.connected && !d.disconnecting
    });

    act(&rt, &b, "forget", ADDRESS);
    until(&rt, &s, "forgotten", || devices(&b, &rt).is_empty());
    let removed = adapter_calls(&tokio, &conn, "RemoveDevice");
    assert_eq!(removed.len(), 1);
    let path: zbus::zvariant::ObjectPath<'_> = removed[0][0].downcast_ref().unwrap();
    assert_eq!(path.as_str(), dev);
    assert!(failures.lock().unwrap().is_empty());

    // Gone: forgetting or connecting it again fails.
    act(&rt, &b, "forget", ADDRESS);
    act(&rt, &b, "connect", ADDRESS);
    until(&rt, &s, "the failures", || {
        failures.lock().unwrap().len() == 2
    });
    for f in failures.lock().unwrap().iter() {
        assert_eq!(f, &(ADDRESS.to_string(), "no such device".to_string()));
    }
    assert_eq!(adapter_calls(&tokio, &conn, "RemoveDevice").len(), 1);
    s.shutdown();
}

/// (gap 29) Without a system bus the service backs off, and a pair or a
/// forget is refused as a `failed` naming the device.
#[test]
fn without_bluez_pair_and_forget_fail_as_events() {
    let gone = strand_services::Bus::Address("unix:path=/nonexistent/strand-test-bus".into());
    let rt = Runtime::new();
    let (s, b) = services(
        &rt,
        strand_services::Buses {
            session: gone.clone(),
            system: gone,
        },
    );
    let failures = failures_of(&b);
    b.bluetooth.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the failed body", || !b.bluetooth.running());
    assert_eq!(b.bluetooth.cells().available.get_untracked(&rt), Ok(false));
    act(&rt, &b, "pair", STRANGER);
    act(&rt, &b, "forget", ADDRESS);
    until(&rt, &s, "the refusals", || {
        failures.lock().unwrap().len() == 2
    });
    let f = failures.lock().unwrap().clone();
    assert_eq!(f[0].0, STRANGER);
    assert_eq!(f[1].0, ADDRESS);
    for (_, error) in &f {
        assert!(error.contains("not running"), "{f:?}");
    }
    s.shutdown();
}

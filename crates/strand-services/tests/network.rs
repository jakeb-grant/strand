//! `network` against python-dbusmock's `networkmanager` template on a
//! private bus: the joined Wi-Fi network, the radio written through
//! `WirelessEnabled`, access points scanned only while a visible reader
//! reads them, and joining a network.

mod support;

use std::time::Duration;

use strand_core::Runtime;
use strand_services::network::{AccessPoint, NM};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Data, Store, ToData, network};
use support::*;

const ROOT: &str = "/org/freedesktop/NetworkManager";

fn add_ap(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    dev: &str,
    name: &str,
    ssid: &str,
    strength: u8,
) -> String {
    mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddAccessPoint",
        &(
            dev,
            name,
            ssid,
            "00:23:F8:7E:12:BA",
            2u32,
            2425u32,
            5400u32,
            strength,
            // NM_802_11_AP_SEC_KEY_MGMT_PSK.
            0x100u32,
        ),
    )
    .body()
    .deserialize()
    .unwrap()
}

fn names(b: &strand_services::Builtin, rt: &Runtime) -> Vec<(String, bool)> {
    b.network
        .cells()
        .access_points
        .get_untracked(rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, a)| (a.ssid.clone(), a.active))
        .collect()
}

/// A Wi-Fi device with the network `Home` joined (its saved connection
/// active): the device's and the access point's paths.
fn wifi_home(tokio: &tokio::runtime::Runtime, conn: &zbus::Connection) -> (String, String) {
    let dev: String = mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddWiFiDevice",
        &("mock_WiFi", "wlan0", 100i32),
    )
    .body()
    .deserialize()
    .unwrap();
    let home = add_ap(tokio, conn, &dev, "Mock_AP1", "Home", 82);
    let saved: String = mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddWiFiConnection",
        &(dev.as_str(), "Mock_Con1", "Home", "wpa-psk"),
    )
    .body()
    .deserialize()
    .unwrap();
    mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddActiveConnection",
        &(
            vec![dev.as_str()],
            saved.as_str(),
            home.as_str(),
            "Mock_Active1",
            2u32,
        ),
    );
    (dev, home)
}

#[test]
fn network_follows_networkmanager_and_scans_only_while_watched() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(nm1) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (dev, home) = wifi_home(&tokio, &conn);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let f = failures.clone();
    b.network.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a {
            f.lock().unwrap().push(args.clone());
        }
    }));
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.connected.get_untracked(&rt), Ok(true));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(Some("Home".to_string())));
    assert_eq!(cells.strength.get_untracked(&rt), Ok(0.82));
    assert_eq!(cells.wifi.get_untracked(&rt), Ok(true));
    assert!(names(&b, &rt).is_empty(), "nobody reads the access points");
    let scans = || mock_calls(&tokio, &conn, NM, &dev, "RequestScan").len();
    assert_eq!(scans(), 0, "no scan while nobody reads the access points");

    // The joined network's strength moves.
    mock(
        &tokio,
        &conn,
        NM,
        &home,
        "UpdateProperties",
        &(
            "org.freedesktop.NetworkManager.AccessPoint",
            std::collections::HashMap::from([("Strength", zbus::zvariant::Value::from(40u8))]),
        ),
    );
    until(&rt, &s, "the new strength", || {
        cells.strength.get_untracked(&rt) == Ok(0.4)
    });

    // A network menu opens: access points are read and a scan asked for.
    let field = network::Network::FIELDS
        .iter()
        .position(|f| f.name == "access_points")
        .unwrap();
    b.network.acquire_field(field);
    until(&rt, &s, "the access points", || {
        names(&b, &rt) == [("Home".to_string(), true)]
    });
    assert_eq!(scans(), 1);
    let cafe = add_ap(&tokio, &conn, &dev, "Mock_AP2", "Cafe", 60);
    until(&rt, &s, "a new access point", || {
        names(&b, &rt) == [("Home".to_string(), true), ("Cafe".to_string(), false)]
    });
    let _ = cafe;

    // Joining Cafe (no saved connection): a new one is added.
    let item = AccessPoint {
        ssid: "Cafe".into(),
        strength: 0.6,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(&rt, "connect", Some(&item.to_data()), &[])
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mock_calls(&tokio, &conn, NM, ROOT, "AddAndActivateConnection").is_empty() {
        assert!(std::time::Instant::now() < deadline, "never joined Cafe");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Joining Home (saved): its connection is activated.
    let home_item = AccessPoint {
        ssid: "Home".into(),
        ..item
    };
    b.network
        .dynamic()
        .action(&rt, "connect", Some(&home_item.to_data()), &[])
        .unwrap();
    while mock_calls(&tokio, &conn, NM, ROOT, "ActivateConnection").is_empty() {
        assert!(std::time::Instant::now() < deadline, "never joined Home");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The menu closes: the list goes, and new access points are not
    // followed.
    b.network.release_field(field);
    until(&rt, &s, "no access points", || names(&b, &rt).is_empty());
    add_ap(&tokio, &conn, &dev, "Mock_AP3", "Library", 30);
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert!(names(&b, &rt).is_empty());
    assert_eq!(scans(), 1);

    // The radio is switched off from the shell.
    b.network
        .dynamic()
        .write(&rt, 3, &[], Data::Bool(false))
        .unwrap();
    rt.flush();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(&rt);
        rt.flush();
        let on = tokio
            .block_on(conn.call_method(
                Some(NM),
                ROOT,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(NM, "WirelessEnabled"),
            ))
            .unwrap();
        let on: zbus::zvariant::OwnedValue = on.body().deserialize().unwrap();
        if on == zbus::zvariant::OwnedValue::from(false) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the radio stayed on");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(cells.wifi.get_untracked(&rt), Ok(false));
    // NetworkManager says it went offline.
    mock(
        &tokio,
        &conn,
        NM,
        ROOT,
        "SetGlobalConnectionState",
        &(20u32,),
    );
    until(&rt, &s, "offline", || {
        cells.connected.get_untracked(&rt) == Ok(false)
    });

    // Joining a network out of range fails: `failed` says so.
    let gone = AccessPoint {
        ssid: "Elsewhere".into(),
        strength: 0.0,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(&rt, "connect", Some(&gone.to_data()), &[])
        .unwrap();
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0][0],
        Data::Text("Elsewhere".into())
    );

    // NetworkManager restarts while a network menu is open: offline while
    // it is gone, then read afresh (and one new scan asked for) without
    // the service restarting.
    b.network.acquire_field(field);
    until(&rt, &s, "the menu's list", || !names(&b, &rt).is_empty());
    drop(nm1);
    until(&rt, &s, "NetworkManager gone", || {
        cells.ssid.get_untracked(&rt) == Ok(None)
            && cells.connected.get_untracked(&rt) == Ok(false)
            && names(&b, &rt).is_empty()
    });
    let Some(_nm2) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (dev2, _) = wifi_home(&tokio, &conn);
    until(&rt, &s, "NetworkManager back", || {
        cells.ssid.get_untracked(&rt) == Ok(Some("Home".to_string()))
            && cells.connected.get_untracked(&rt) == Ok(true)
            && names(&b, &rt) == [("Home".to_string(), true)]
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mock_calls(&tokio, &conn, NM, &dev2, "RequestScan").is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no scan on the new one"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(
        mock_calls(&tokio, &conn, NM, &dev2, "RequestScan").len(),
        1,
        "one scan"
    );
    assert_eq!(b.network.starts(), 1);
    s.shutdown();
}

/// Joining with a password (`ap.connect_with(password)`) hands
/// NetworkManager the WPA settings; a join NetworkManager accepts and
/// later gives up on (the active connection deactivated for want of a
/// password) is `failed`, with the reason.
#[test]
fn a_join_that_fails_later_is_reported() {
    use zbus::zvariant::{OwnedValue, Value};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (dev, _) = wifi_home(&tokio, &conn);
    add_ap(&tokio, &conn, &dev, "Mock_AP2", "Cafe", 60);
    add_ap(&tokio, &conn, &dev, "Mock_AP3", "Airport", 50);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let f = failures.clone();
    b.network.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a {
            f.lock().unwrap().push(args.clone());
        }
    }));
    b.network.acquire(&rt);
    let field = network::Network::FIELDS
        .iter()
        .position(|f| f.name == "access_points")
        .unwrap();
    b.network.acquire_field(field);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the access points", || names(&b, &rt).len() == 3);
    let ap = |ssid: &str| AccessPoint {
        ssid: ssid.into(),
        strength: 0.5,
        secure: true,
        active: false,
    };

    // The menu closes as the shell joins (the access points are not
    // followed any more): the join reads them afresh.
    b.network.release_field(field);
    until(&rt, &s, "no access points", || names(&b, &rt).is_empty());
    // A password from the shell: a WPA personal connection with it.
    b.network
        .dynamic()
        .action(
            &rt,
            "connect_with",
            Some(&ap("Cafe").to_data()),
            &[Data::Text("hunter22".into())],
        )
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let added = loop {
        let calls = mock_calls(&tokio, &conn, NM, ROOT, "AddAndActivateConnection");
        if let Some(c) = calls.into_iter().next() {
            break c;
        }
        assert!(std::time::Instant::now() < deadline, "never joined Cafe");
        std::thread::sleep(Duration::from_millis(20));
    };
    type Settings =
        std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>>;
    let settings: Settings = added[0].try_clone().unwrap().try_into().unwrap();
    let sec = &settings["802-11-wireless-security"];
    assert_eq!(
        sec["key-mgmt"],
        OwnedValue::try_from(Value::from("wpa-psk")).unwrap()
    );
    assert_eq!(
        sec["psk"],
        OwnedValue::try_from(Value::from("hunter22")).unwrap()
    );
    // The template activates it at once: no failure.
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );

    // NetworkManager accepts a join, then deactivates it: no secrets.
    let active = "/org/freedesktop/NetworkManager/ActiveConnection/77";
    let iface = "org.freedesktop.NetworkManager.Connection.Active";
    mock(
        &tokio,
        &conn,
        NM,
        ROOT,
        "AddMethod",
        &(
            NM,
            "AddAndActivateConnection",
            "a{sa{sv}}oo",
            "oo",
            format!(
                "self.AddObject('{active}', '{iface}', {{'State': dbus.UInt32(1)}}, [])\n\
                 ret = (dbus.ObjectPath('/org/freedesktop/NetworkManager/Settings/77'), \
                 dbus.ObjectPath('{active}'))"
            ),
        ),
    );
    b.network
        .dynamic()
        .action(&rt, "connect", Some(&ap("Airport").to_data()), &[])
        .unwrap();
    while mock_calls(&tokio, &conn, NM, ROOT, "AddAndActivateConnection").len() < 2 {
        assert!(std::time::Instant::now() < deadline, "never joined Airport");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    s.pump(&rt);
    assert!(
        failures.lock().unwrap().is_empty(),
        "activating is not failing"
    );
    // NetworkManager says DEACTIVATED twice: as a property change (no
    // reason) and then as StateChanged with the reason. The join is
    // settled on the second, so the reason is kept.
    mock(
        &tokio,
        &conn,
        NM,
        active,
        "EmitSignal",
        &(
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            "sa{sv}as",
            vec![
                Value::from(iface),
                Value::from(std::collections::HashMap::from([(
                    "State".to_string(),
                    Value::from(4u32),
                )])),
                Value::from(Vec::<String>::new()),
            ],
        ),
    );
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert!(
        failures.lock().unwrap().is_empty(),
        "not settled without a reason: {:?}",
        failures.lock().unwrap()
    );
    // NM_ACTIVE_CONNECTION_STATE_DEACTIVATED, _REASON_NO_SECRETS.
    mock(
        &tokio,
        &conn,
        NM,
        active,
        "EmitSignal",
        &(
            iface,
            "StateChanged",
            "uu",
            vec![Value::from(4u32), Value::from(9u32)],
        ),
    );
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        [
            Data::Text("Airport".into()),
            Data::Text(network::reason_text(9).into())
        ]
    );
    s.shutdown();
}

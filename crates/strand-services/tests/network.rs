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
    assert_eq!(cells.available.get_untracked(&rt), Ok(true));
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
        .action(
            &rt,
            "connect",
            Some(&item.to_data()),
            &[],
            Default::default(),
        )
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
        .action(
            &rt,
            "connect",
            Some(&home_item.to_data()),
            &[],
            Default::default(),
        )
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
        .action(
            &rt,
            "connect",
            Some(&gone.to_data()),
            &[],
            Default::default(),
        )
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
            Default::default(),
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
        .action(
            &rt,
            "connect",
            Some(&ap("Airport").to_data()),
            &[],
            Default::default(),
        )
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

/// The failures `failed` has reported, as (ssid, error).
fn failures_of(
    b: &strand_services::Builtin,
) -> std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> {
    let failures = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let f = failures.clone();
    b.network.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a
            && let [Data::Text(ssid), Data::Text(error)] = args.as_slice()
        {
            f.lock()
                .unwrap()
                .push((ssid.to_string(), error.to_string()));
        }
    }));
    failures
}

/// (m6-services) An enterprise (802.1X) network with a profile saved in
/// NetworkManager joins with `connect()`, through that profile; one
/// without a saved profile is refused with an error that says why, and
/// NetworkManager is not asked to add anything for it. (services-r2-1) A
/// WEP network is `secure` and joins like a personal one: unsaved,
/// `connect()` asks NetworkManager (whose secret agent asks for the key)
/// and `connect_with(key)` carries the key.
#[test]
fn enterprise_networks_join_only_through_a_saved_profile() {
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (dev, _) = wifi_home(&tokio, &conn);
    // The template saves only personal networks: Corp is saved as one,
    // then its access point says it is 802.1X (as the profile the
    // system's settings made would be).
    let corp = add_ap(&tokio, &conn, &dev, "Mock_Corp", "Corp", 70);
    let corp_saved: String = mock(
        &tokio,
        &conn,
        NM,
        ROOT,
        "AddWiFiConnection",
        &(dev.as_str(), "Mock_CorpCon", "Corp", "wpa-eap"),
    )
    .body()
    .deserialize()
    .unwrap();
    // NM_802_11_AP_SEC_KEY_MGMT_802_1X with CCMP.
    let eap = 0x200u32 | 0x8 | 0x80;
    mock(
        &tokio,
        &conn,
        NM,
        &corp,
        "UpdateProperties",
        &(
            "org.freedesktop.NetworkManager.AccessPoint",
            std::collections::HashMap::from([
                ("WpaFlags", Value::from(0u32)),
                ("RsnFlags", Value::from(eap)),
            ]),
        ),
    );
    // Office: 802.1X, never saved. Legacy: WEP (privacy, no WPA).
    let office = add_ap(&tokio, &conn, &dev, "Mock_Office", "Office", 60);
    mock(
        &tokio,
        &conn,
        NM,
        &office,
        "UpdateProperties",
        &(
            "org.freedesktop.NetworkManager.AccessPoint",
            std::collections::HashMap::from([
                ("WpaFlags", Value::from(0u32)),
                ("RsnFlags", Value::from(eap)),
            ]),
        ),
    );
    let legacy = add_ap(&tokio, &conn, &dev, "Mock_Legacy", "Legacy", 50);
    mock(
        &tokio,
        &conn,
        NM,
        &legacy,
        "UpdateProperties",
        &(
            "org.freedesktop.NetworkManager.AccessPoint",
            std::collections::HashMap::from([
                ("Flags", Value::from(1u32)),
                ("WpaFlags", Value::from(0u32)),
                ("RsnFlags", Value::from(0u32)),
            ]),
        ),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    // `secure` is where a password joins: Home (WPA2) and Legacy (WEP),
    // not the 802.1X networks.
    let field = network::Network::FIELDS
        .iter()
        .position(|f| f.name == "access_points")
        .unwrap();
    b.network.acquire_field(field);
    until(&rt, &s, "the access points", || names(&b, &rt).len() == 4);
    let mut secure: Vec<(String, bool)> = b
        .network
        .cells()
        .access_points
        .get_untracked(&rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, a)| (a.ssid.clone(), a.secure))
        .collect();
    secure.sort();
    assert_eq!(
        secure,
        [
            ("Corp".to_string(), false),
            ("Home".to_string(), true),
            ("Legacy".to_string(), true),
            ("Office".to_string(), false),
        ]
    );
    b.network.release_field(field);
    let join = |ssid: &str, password: Option<&str>| {
        let item = AccessPoint {
            ssid: ssid.into(),
            strength: 0.5,
            secure: true,
            active: false,
        };
        let (name, args) = match password {
            Some(p) => ("connect_with", vec![Data::Text(p.into())]),
            None => ("connect", vec![]),
        };
        b.network
            .dynamic()
            .action(&rt, name, Some(&item.to_data()), &args, Default::default())
            .unwrap();
    };
    let failed = |n: usize| {
        until(&rt, &s, "a failure", || failures.lock().unwrap().len() >= n);
        failures.lock().unwrap()[n - 1].clone()
    };

    // Saved Corp: its profile is activated.
    join("Corp", None);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let activated = loop {
        if let Some(c) = mock_calls(&tokio, &conn, NM, ROOT, "ActivateConnection")
            .into_iter()
            .next()
        {
            break c;
        }
        assert!(std::time::Instant::now() < deadline, "Corp never joined");
        std::thread::sleep(Duration::from_millis(20));
    };
    let path: OwnedObjectPath = activated[0].try_clone().unwrap().try_into().unwrap();
    assert_eq!(path.as_str(), corp_saved);
    let ap: OwnedObjectPath = activated[2].try_clone().unwrap().try_into().unwrap();
    assert_eq!(ap.as_str(), corp);
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    // A password for it: refused (the profile holds the login).
    join("Corp", Some("hunter22"));
    let (ssid, error) = failed(1);
    assert_eq!(ssid, "Corp");
    assert!(
        error.contains("enterprise (802.1X)") && error.contains("`connect()`"),
        "{error}"
    );

    // Unsaved Office, with and without a password: refused.
    join("Office", None);
    let (ssid, error) = failed(2);
    assert_eq!(ssid, "Office");
    assert!(
        error.contains("enterprise (802.1X)") && error.contains("no saved profile"),
        "{error}"
    );
    join("Office", Some("hunter22"));
    assert!(failed(3).1.contains("no saved profile"));
    // NetworkManager was asked to add nothing, and Corp's profile was not
    // rewritten.
    std::thread::sleep(Duration::from_millis(200));
    assert!(mock_calls(&tokio, &conn, NM, ROOT, "AddAndActivateConnection").is_empty());
    assert!(mock_calls(&tokio, &conn, NM, &corp_saved, "Update").is_empty());

    // Unsaved Legacy (WEP), with its key: a new connection carrying it.
    join("Legacy", Some("abcde"));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let added = loop {
        if let Some(c) = mock_calls(&tokio, &conn, NM, ROOT, "AddAndActivateConnection")
            .into_iter()
            .next()
        {
            break c;
        }
        assert!(std::time::Instant::now() < deadline, "Legacy never joined");
        std::thread::sleep(Duration::from_millis(20));
    };
    type Settings =
        std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>>;
    let settings: Settings = added[0].try_clone().unwrap().try_into().unwrap();
    let sec = &settings["802-11-wireless-security"];
    assert_eq!(
        sec["key-mgmt"],
        OwnedValue::try_from(Value::from("none")).unwrap()
    );
    assert_eq!(
        sec["wep-key0"],
        OwnedValue::try_from(Value::from("abcde")).unwrap()
    );
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(failures.lock().unwrap().len(), 3, "Legacy joined");
    assert_eq!(
        mock_calls(&tokio, &conn, NM, ROOT, "ActivateConnection").len(),
        1
    );
    assert_eq!(b.network.starts(), 1);
    s.shutdown();
}

/// (m6-services, services-r3-3) A key retyped for a saved shared-key WEP
/// network is saved into its profile's security section, which keeps
/// what else it said: its `auth-alg` and transmit key (the key written at
/// that index), not a section of the key alone.
#[test]
fn a_new_key_keeps_the_saved_profile_s_security_settings() {
    use std::collections::HashMap;
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
    let legacy = add_ap(&tokio, &conn, &dev, "Mock_Legacy", "Legacy", 50);
    mock(
        &tokio,
        &conn,
        NM,
        &legacy,
        "UpdateProperties",
        &(
            "org.freedesktop.NetworkManager.AccessPoint",
            HashMap::from([
                ("Flags", Value::from(1u32)),
                ("WpaFlags", Value::from(0u32)),
                ("RsnFlags", Value::from(0u32)),
            ]),
        ),
    );
    // Saved in the system's settings: shared-key auth, transmitting with
    // key 1.
    let profile: HashMap<&str, HashMap<&str, Value>> = HashMap::from([
        (
            "connection",
            HashMap::from([
                ("id", Value::from("Legacy")),
                ("type", Value::from("802-11-wireless")),
            ]),
        ),
        (
            "802-11-wireless",
            HashMap::from([("ssid", Value::from(b"Legacy".to_vec()))]),
        ),
        (
            "802-11-wireless-security",
            HashMap::from([
                ("key-mgmt", Value::from("none")),
                ("auth-alg", Value::from("shared")),
                ("wep-tx-keyidx", Value::from(1u32)),
            ]),
        ),
    ]);
    let saved: zbus::zvariant::OwnedObjectPath = call(
        &tokio,
        &conn,
        NM,
        "/org/freedesktop/NetworkManager/Settings",
        "org.freedesktop.NetworkManager.Settings",
        "AddConnection",
        &(profile,),
    )
    .body()
    .deserialize()
    .unwrap();
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let item = AccessPoint {
        ssid: "Legacy".into(),
        strength: 0.5,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(
            &rt,
            "connect_with",
            Some(&item.to_data()),
            &[Data::Text("abcde".into())],
            Default::default(),
        )
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let updated = loop {
        if let Some(c) = mock_calls(&tokio, &conn, NM, saved.as_str(), "Update")
            .into_iter()
            .next()
        {
            break c;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the profile was never updated"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    type Settings = HashMap<String, HashMap<String, OwnedValue>>;
    let settings: Settings = updated[0].try_clone().unwrap().try_into().unwrap();
    let sec = &settings["802-11-wireless-security"];
    let text = |s: &str| OwnedValue::try_from(Value::from(s)).unwrap();
    assert_eq!(sec["auth-alg"], text("shared"), "{sec:?}");
    assert_eq!(sec["wep-tx-keyidx"], OwnedValue::from(1u32));
    assert_eq!(sec["wep-key1"], text("abcde"));
    assert_eq!(sec["key-mgmt"], text("none"));
    s.shutdown();
}

/// A NetworkManager of the test's own, for what python-dbusmock cannot
/// do (it answers one call at a time): take its time over the saved
/// connections while answering everything else at once.
mod slow_nm {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

    pub const DEV: &str = "/org/freedesktop/NetworkManager/Devices/1";
    pub const AP: &str = "/org/freedesktop/NetworkManager/AccessPoint/1";

    fn path(p: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(p).unwrap()
    }

    pub struct Manager {
        pub wifi: bool,
        /// Home is joined ([`JOINED`]).
        pub joined: bool,
        pub added: Arc<AtomicUsize>,
        pub deactivated: Arc<AtomicUsize>,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl Manager {
        #[zbus(property)]
        fn state(&self) -> u32 {
            70
        }
        #[zbus(property)]
        fn wireless_enabled(&self) -> bool {
            self.wifi
        }
        #[zbus(property)]
        fn set_wireless_enabled(&mut self, on: bool) {
            self.wifi = on;
        }
        #[zbus(property)]
        fn active_connections(&self) -> Vec<OwnedObjectPath> {
            if self.joined {
                vec![path(JOINED)]
            } else {
                Vec::new()
            }
        }
        #[zbus(property)]
        fn devices(&self) -> Vec<OwnedObjectPath> {
            vec![path(DEV)]
        }
        fn add_and_activate_connection(
            &self,
            _settings: HashMap<String, HashMap<String, OwnedValue>>,
            _dev: ObjectPath<'_>,
            _ap: ObjectPath<'_>,
        ) -> (OwnedObjectPath, OwnedObjectPath) {
            self.added.fetch_add(1, Ordering::SeqCst);
            (
                path("/org/freedesktop/NetworkManager/Settings/1"),
                path(ACTIVE),
            )
        }
        fn deactivate_connection(&self, _active: ObjectPath<'_>) {
            self.deactivated.fetch_add(1, Ordering::SeqCst);
        }
        /// (services-round) A saved connection's `up()`: answered after
        /// [`ACTIVATE_DELAY`].
        async fn activate_connection(
            &self,
            _conn: ObjectPath<'_>,
            _dev: ObjectPath<'_>,
            _specific: ObjectPath<'_>,
        ) -> OwnedObjectPath {
            tokio::time::sleep(ACTIVATE_DELAY).await;
            path(ACTIVE)
        }
    }

    /// How long a saved connection's activation takes.
    pub const ACTIVATE_DELAY: Duration = Duration::from_secs(3);

    /// The active connection every join is answered with.
    pub const ACTIVE: &str = "/org/freedesktop/NetworkManager/ActiveConnection/1";
    /// Home's active connection, activated, when it is joined.
    pub const JOINED: &str = "/org/freedesktop/NetworkManager/ActiveConnection/0";
    /// Home's access point.
    pub const HOME_AP: &str = "/org/freedesktop/NetworkManager/AccessPoint/0";

    /// Home joined on the device.
    pub struct Joined;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
    impl Joined {
        #[zbus(property, name = "Type")]
        fn kind(&self) -> String {
            "802-11-wireless".into()
        }
        #[zbus(property)]
        fn state(&self) -> u32 {
            2
        }
        #[zbus(property)]
        fn devices(&self) -> Vec<OwnedObjectPath> {
            vec![path(DEV)]
        }
        #[zbus(property)]
        fn specific_object(&self) -> OwnedObjectPath {
            path(HOME_AP)
        }
        #[zbus(property)]
        fn default(&self) -> bool {
            true
        }
    }

    /// The device's refusals, as NetworkManager names them.
    #[derive(Debug, zbus::DBusError)]
    #[zbus(prefix = "org.freedesktop.NetworkManager.Device")]
    pub enum DeviceError {
        #[zbus(error)]
        ZBus(zbus::Error),
        NotActive(String),
        PermissionDenied(String),
    }

    /// How the device's `Disconnect` answers, and what it and
    /// `DeactivateConnection` were asked.
    #[derive(Clone, Default)]
    pub struct Leave {
        /// `Disconnect` answers after this.
        pub delay: Duration,
        /// `Disconnect` is refused ("Not authorized").
        pub refuse: bool,
        /// Home is joined; without it `Disconnect` is refused as
        /// NetworkManager refuses it on a device with nothing active
        /// (`NotActive`, "This device is not active").
        pub joined: bool,
        pub disconnects: Arc<AtomicUsize>,
        pub deactivated: Arc<AtomicUsize>,
    }

    pub struct Device(Leave);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
    impl Device {
        #[zbus(property)]
        fn device_type(&self) -> u32 {
            2
        }
        async fn disconnect(&self) -> Result<(), DeviceError> {
            self.0.disconnects.fetch_add(1, Ordering::SeqCst);
            if !self.0.joined {
                return Err(DeviceError::NotActive("This device is not active".into()));
            }
            tokio::time::sleep(self.0.delay).await;
            if self.0.refuse {
                return Err(DeviceError::PermissionDenied("Not authorized".into()));
            }
            Ok(())
        }
    }

    pub struct Wireless;

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
    impl Wireless {
        #[zbus(property)]
        fn access_points(&self) -> Vec<OwnedObjectPath> {
            vec![path(AP)]
        }
        fn request_scan(&self, _options: HashMap<String, OwnedValue>) {}
    }

    /// An access point of the network it names.
    pub struct AccessPoint(&'static str);

    #[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
    impl AccessPoint {
        #[zbus(property)]
        fn ssid(&self) -> Vec<u8> {
            self.0.as_bytes().to_vec()
        }
        #[zbus(property)]
        fn strength(&self) -> u8 {
            60
        }
        #[zbus(property)]
        fn flags(&self) -> u32 {
            1
        }
        #[zbus(property)]
        fn wpa_flags(&self) -> u32 {
            0
        }
        #[zbus(property)]
        fn rsn_flags(&self) -> u32 {
            0x188
        }
    }

    pub struct Settings {
        pub delay: Duration,
        pub asked: Arc<AtomicUsize>,
    }

    /// One served on the bus at `address`, its saved connections listed
    /// after `delay`; counting the `ListConnections` (`asked`) and
    /// `AddAndActivateConnection` (`added`) calls. Named NetworkManager
    /// if `own`, else without a name (it asks for it when it wants).
    pub fn serve(
        tokio: &tokio::runtime::Runtime,
        address: &str,
        delay: Duration,
        asked: &Arc<AtomicUsize>,
        added: &Arc<AtomicUsize>,
        own: bool,
    ) -> zbus::Connection {
        serve_leaving(tokio, address, delay, asked, added, own, Leave::default())
    }

    /// [`serve`], its device's `Disconnect` answering as `leave` says.
    pub fn serve_leaving(
        tokio: &tokio::runtime::Runtime,
        address: &str,
        delay: Duration,
        asked: &Arc<AtomicUsize>,
        added: &Arc<AtomicUsize>,
        own: bool,
        leave: Leave,
    ) -> zbus::Connection {
        tokio.block_on(async {
            let mut b = zbus::connection::Builder::address(address).unwrap();
            if own {
                b = b.name(super::NM).unwrap();
            }
            b.serve_at(
                super::ROOT,
                Manager {
                    wifi: true,
                    joined: leave.joined,
                    added: added.clone(),
                    deactivated: leave.deactivated.clone(),
                },
            )
            .unwrap()
            .serve_at(DEV, Device(leave))
            .unwrap()
            .serve_at(DEV, Wireless)
            .unwrap()
            .serve_at(AP, AccessPoint("Slow"))
            .unwrap()
            .serve_at(HOME_AP, AccessPoint("Home"))
            .unwrap()
            .serve_at(JOINED, Joined)
            .unwrap()
            .serve_at(
                "/org/freedesktop/NetworkManager/Settings",
                Settings {
                    delay,
                    asked: asked.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap()
        })
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
    impl Settings {
        fn get_connection_by_uuid(&self, _uuid: &str) -> OwnedObjectPath {
            path("/org/freedesktop/NetworkManager/Settings/9")
        }
        async fn list_connections(&self) -> Vec<OwnedObjectPath> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Vec::new()
        }
    }
}

/// (m6-services) A join is asked in a task of its own: while
/// NetworkManager takes its time over it (here 4 s over the saved
/// connections, within the 5 s bound), the radio is switched at once.
/// Awaited inline, the switch waited for the join.
#[test]
fn a_slow_join_does_not_hold_up_the_radio() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let _nm = slow_nm::serve(
        &tokio,
        &bus.address,
        Duration::from_secs(4),
        &asked,
        &added,
        true,
    );
    let conn = connect(&tokio, &bus.address);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.wifi.get_untracked(&rt), Ok(true));

    let item = AccessPoint {
        ssid: "Slow".into(),
        strength: 0.6,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(
            &rt,
            "connect_with",
            Some(&item.to_data()),
            &[Data::Text("hunter22".into())],
            Default::default(),
        )
        .unwrap();
    until(&rt, &s, "the join asking for the saved connections", || {
        asked.load(Ordering::SeqCst) == 1
    });

    // The radio, switched off while NetworkManager is still on the join.
    let asked_at = std::time::Instant::now();
    b.network
        .dynamic()
        .write(&rt, 3, &[], Data::Bool(false))
        .unwrap();
    rt.flush();
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
        assert!(
            asked_at.elapsed() < Duration::from_millis(2500),
            "the radio waited for the join"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(added.load(Ordering::SeqCst), 0, "the join is still asking");
    // The join goes on and is asked of NetworkManager in the end.
    until(&rt, &s, "the join", || added.load(Ordering::SeqCst) == 1);
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    s.shutdown();
}

/// `ap.connect_with(password)` on "Slow" (the slow NetworkManager's one
/// network).
fn join_slow(rt: &Runtime, b: &strand_services::Builtin, password: &str) {
    let item = AccessPoint {
        ssid: "Slow".into(),
        strength: 0.6,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(
            rt,
            "connect_with",
            Some(&item.to_data()),
            &[Data::Text(password.into())],
            Default::default(),
        )
        .unwrap();
}

/// (m6-services, services-r1-1) Joins are tasks, but one network is
/// asked one join at a time: a double click on an unsaved network makes
/// one profile, not two (both would find none saved and add one, and the
/// second activation would end the first), and a join with another
/// password waits for the first's answer.
#[test]
fn two_joins_of_one_network_add_one_profile() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let _nm = slow_nm::serve(
        &tokio,
        &bus.address,
        Duration::from_secs(2),
        &asked,
        &added,
        true,
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    join_slow(&rt, &b, "hunter22");
    join_slow(&rt, &b, "hunter22");
    join_slow(&rt, &b, "other-password");
    until(&rt, &s, "the first join asking", || {
        asked.load(Ordering::SeqCst) >= 1
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(800) {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "one join of Slow at a time"
    );
    assert_eq!(added.load(Ordering::SeqCst), 0);
    // The first answers; then the other password is asked, once.
    until(&rt, &s, "the waiting join", || {
        added.load(Ordering::SeqCst) == 2
    });
    assert_eq!(asked.load(Ordering::SeqCst), 2, "the repeat was not asked");
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(500) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(added.load(Ordering::SeqCst), 2);
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    s.shutdown();
}

/// (m6-services, services-r1-4) A join still asking when NetworkManager
/// is replaced stops there: its next call would reach the new
/// NetworkManager, which would activate a network the service no longer
/// follows while it reported the join as failed.
#[test]
fn a_join_stops_when_networkmanager_is_replaced() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let old = slow_nm::serve(
        &tokio,
        &bus.address,
        Duration::from_secs(2),
        &asked,
        &added,
        true,
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    join_slow(&rt, &b, "hunter22");
    until(&rt, &s, "the join asking", || {
        asked.load(Ordering::SeqCst) == 1
    });
    // A new NetworkManager queues for the name and takes it from the old
    // one, which still answers the join's call (with no saved profile).
    let new_asked = Arc::new(AtomicUsize::new(0));
    let new_added = Arc::new(AtomicUsize::new(0));
    let new = slow_nm::serve(
        &tokio,
        &bus.address,
        Duration::ZERO,
        &new_asked,
        &new_added,
        false,
    );
    tokio.block_on(async {
        new.request_name_with_flags(NM, zbus::fdo::RequestNameFlags::AllowReplacement.into())
            .await
            .unwrap();
        old.release_name(NM).await.unwrap();
    });
    until(&rt, &s, "the join failing", || {
        !failures.lock().unwrap().is_empty()
    });
    // Past the old one's answer: nothing more is asked of either.
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        new_added.load(Ordering::SeqCst),
        0,
        "the new NetworkManager was asked to join"
    );
    assert_eq!(added.load(Ordering::SeqCst), 0);
    assert_eq!(
        *failures.lock().unwrap(),
        vec![("Slow".to_string(), "NetworkManager stopped".to_string())]
    );
    s.shutdown();
}

/// (services-round) A saved connection's `up()` still in flight when
/// NetworkManager is replaced is cut short, and says so:
/// `network.failed(name, "NetworkManager stopped")`, as a join does.
#[test]
fn an_up_in_flight_fails_when_networkmanager_is_replaced() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (asked, added) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let old = slow_nm::serve(&tokio, &bus.address, Duration::ZERO, &asked, &added, true);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let vpn = network::Connection {
        id: "u-vpn".into(),
        name: "Work VPN".into(),
        kind: network::ConnectionKind::Vpn,
        active: false,
    };
    b.network
        .dynamic()
        .action(&rt, "up", Some(&vpn.to_data()), &[], Default::default())
        .unwrap();
    // Asked, and not answered yet.
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(500) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    let (new_asked, new_added) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let new = slow_nm::serve(
        &tokio,
        &bus.address,
        Duration::ZERO,
        &new_asked,
        &new_added,
        false,
    );
    tokio.block_on(async {
        new.request_name_with_flags(NM, zbus::fdo::RequestNameFlags::AllowReplacement.into())
            .await
            .unwrap();
        old.release_name(NM).await.unwrap();
    });
    until(&rt, &s, "the up failing", || {
        !failures.lock().unwrap().is_empty()
    });
    // Past the old one's answer: no second failure, no success taken.
    let t = std::time::Instant::now();
    while t.elapsed() < slow_nm::ACTIVATE_DELAY {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        *failures.lock().unwrap(),
        vec![("Work VPN".to_string(), "NetworkManager stopped".to_string())]
    );
    s.shutdown();
}

/// (gap 28, connectivity-r1-6) A join NetworkManager answers while a
/// `disconnect()` is in flight is not followed, and once the device has
/// left its network that activation is deactivated (it may have started
/// after the leave): neither the join nor its end is a failure.
#[test]
fn a_join_answered_during_a_disconnect_is_deactivated_once_left() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let leave = slow_nm::Leave {
        delay: Duration::from_millis(2500),
        joined: true,
        ..slow_nm::Leave::default()
    };
    let _nm = slow_nm::serve_leaving(
        &tokio,
        &bus.address,
        Duration::from_millis(600),
        &asked,
        &added,
        true,
        leave.clone(),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    join_slow(&rt, &b, "hunter22");
    until(&rt, &s, "the join asking", || {
        asked.load(Ordering::SeqCst) == 1
    });
    b.network
        .dynamic()
        .action(&rt, "disconnect", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "the disconnect in flight", || {
        leave.disconnects.load(Ordering::SeqCst) == 1
            && cells.disconnecting.get_untracked(&rt) == Ok(Some("Home".into()))
            && cells.connecting.get_untracked(&rt) == Ok(None)
    });
    // NetworkManager answers the join while the device is being left.
    until(&rt, &s, "the join answered", || {
        added.load(Ordering::SeqCst) == 1
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        leave.deactivated.load(Ordering::SeqCst),
        0,
        "deactivated before the disconnect answered"
    );
    until(&rt, &s, "the activation ended", || {
        leave.deactivated.load(Ordering::SeqCst) == 1
            && cells.disconnecting.get_untracked(&rt) == Ok(None)
    });
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    s.shutdown();
}

/// (gap 28, connectivity-r2-1) With nothing active on the Wi-Fi device
/// (a join of an unsaved network still asking, before NetworkManager has
/// an activation for it), `disconnect()` stops the join itself: the
/// device is not asked (NetworkManager answers `NotActive`, "This device
/// is not active", which surfaced as a failure and let the join go on),
/// nothing is reported, and the activation NetworkManager answers the
/// join with is deactivated at once.
#[test]
fn a_disconnect_with_nothing_active_stops_the_join_itself() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let leave = slow_nm::Leave::default();
    let _nm = slow_nm::serve_leaving(
        &tokio,
        &bus.address,
        Duration::from_millis(1000),
        &asked,
        &added,
        true,
        leave.clone(),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    join_slow(&rt, &b, "hunter22");
    until(&rt, &s, "the join asking", || {
        asked.load(Ordering::SeqCst) == 1
            && cells.connecting.get_untracked(&rt) == Ok(Some("Slow".into()))
    });
    b.network
        .dynamic()
        .action(&rt, "disconnect", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "the join stopped", || {
        cells.connecting.get_untracked(&rt) == Ok(None)
    });
    assert_eq!(cells.disconnecting.get_untracked(&rt), Ok(None));
    // NetworkManager answers the join: its activation is ended.
    until(&rt, &s, "the activation ended", || {
        added.load(Ordering::SeqCst) == 1 && leave.deactivated.load(Ordering::SeqCst) == 1
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        leave.disconnects.load(Ordering::SeqCst),
        0,
        "the device was asked"
    );
    assert_eq!(cells.connecting.get_untracked(&rt), Ok(None));
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    s.shutdown();
}

/// (gap 28, connectivity-r2-2) A `disconnect()` while one is in flight (a
/// double click) is dropped: the device is asked once, and the first
/// one's answer settles the join it stopped. The second overwrote the
/// first, whose answer then matched nothing, so only the second's
/// outcome counted (a refusal resumed joins the first had left).
#[test]
fn a_disconnect_while_one_is_in_flight_is_dropped() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let leave = slow_nm::Leave {
        delay: Duration::from_millis(1500),
        joined: true,
        ..slow_nm::Leave::default()
    };
    let _nm = slow_nm::serve_leaving(
        &tokio,
        &bus.address,
        Duration::from_millis(400),
        &asked,
        &added,
        true,
        leave.clone(),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    join_slow(&rt, &b, "hunter22");
    until(&rt, &s, "the join asking", || {
        asked.load(Ordering::SeqCst) == 1
    });
    for _ in 0..2 {
        b.network
            .dynamic()
            .action(&rt, "disconnect", None, &[], Default::default())
            .unwrap();
    }
    until(&rt, &s, "the disconnect in flight", || {
        cells.disconnecting.get_untracked(&rt) == Ok(Some("Home".into()))
    });
    until(&rt, &s, "the disconnect answered", || {
        cells.disconnecting.get_untracked(&rt) == Ok(None)
            && leave.deactivated.load(Ordering::SeqCst) == 1
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(500) {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(leave.disconnects.load(Ordering::SeqCst), 1, "asked twice");
    assert_eq!(cells.connecting.get_untracked(&rt), Ok(None));
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    s.shutdown();
}

/// (gap 28, connectivity-r1-6) A disconnect NetworkManager refuses (here
/// `Not authorized`, as polkit may answer) leaves the join it stopped
/// going on: it is followed again, and its later failure reported. It was
/// dropped as stopped before, so a wrong password after a refused
/// disconnect was never told.
#[test]
fn a_join_a_refused_disconnect_stopped_is_still_followed() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let asked = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let leave = slow_nm::Leave {
        refuse: true,
        joined: true,
        ..slow_nm::Leave::default()
    };
    let nm = slow_nm::serve_leaving(
        &tokio,
        &bus.address,
        Duration::from_millis(1500),
        &asked,
        &added,
        true,
        leave.clone(),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    join_slow(&rt, &b, "hunter22");
    until(&rt, &s, "the join asking", || {
        asked.load(Ordering::SeqCst) == 1
    });
    b.network
        .dynamic()
        .action(&rt, "disconnect", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "the refusal", || {
        !failures.lock().unwrap().is_empty()
    });
    // The disconnect names the network the device has joined.
    assert_eq!(
        *failures.lock().unwrap(),
        vec![("Home".to_string(), "Not authorized".to_string())]
    );
    assert_eq!(
        cells.connecting.get_untracked(&rt),
        Ok(Some("Slow".into())),
        "still joining"
    );
    until(&rt, &s, "the join answered", || {
        added.load(Ordering::SeqCst) == 1
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        s.pump(&rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(leave.deactivated.load(Ordering::SeqCst), 0, "not ended");
    // The activation fails: a password is needed.
    tokio.block_on(async {
        nm.emit_signal(
            None::<zbus::names::BusName<'_>>,
            slow_nm::ACTIVE,
            "org.freedesktop.NetworkManager.Connection.Active",
            "StateChanged",
            &(4u32, 9u32),
        )
        .await
        .unwrap();
    });
    until(&rt, &s, "the join's failure", || {
        failures.lock().unwrap().len() == 2
    });
    assert_eq!(
        failures.lock().unwrap()[1],
        ("Slow".to_string(), network::reason_text(9))
    );
    s.shutdown();
}

/// (m6-services, services-r2-2) A join asked again once NetworkManager
/// has answered the first (its activation under way) is a repeat too: a
/// second click does not activate the network again, which would end the
/// first activation and report it as failed while the network came up.
/// A join with another password is asked, and the activation it
/// replaces ending is not a failure; the new one is still followed.
#[test]
fn a_repeat_while_the_join_is_activating_is_dropped() {
    use zbus::zvariant::Value;
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
    // Each join makes an active connection that stays activating.
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
                "self.joins = getattr(self, 'joins', 0) + 1\n\
                 p = '/org/freedesktop/NetworkManager/ActiveConnection/8%d' % self.joins\n\
                 self.AddObject(p, '{iface}', {{'State': dbus.UInt32(1)}}, [])\n\
                 ret = (dbus.ObjectPath('/org/freedesktop/NetworkManager/Settings/88'), \
                 dbus.ObjectPath(p))"
            ),
        ),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let join = |password: &str| {
        let item = AccessPoint {
            ssid: "Cafe".into(),
            strength: 0.6,
            secure: true,
            active: false,
        };
        b.network
            .dynamic()
            .action(
                &rt,
                "connect_with",
                Some(&item.to_data()),
                &[Data::Text(password.into())],
                Default::default(),
            )
            .unwrap();
    };
    let added = || mock_calls(&tokio, &conn, NM, ROOT, "AddAndActivateConnection").len();
    let settle = |ms: u64| {
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            s.pump(&rt);
            rt.flush();
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    join("hunter22");
    until(&rt, &s, "the first join", || added() == 1);
    // Its answer is in: the join is activating, no task asks it.
    settle(300);
    join("hunter22");
    settle(500);
    assert_eq!(added(), 1, "the repeat activated Cafe again");

    // Another password: asked, replacing the first activation.
    join("other-password");
    until(&rt, &s, "the second join", || added() == 2);
    settle(300);
    let state_changed = |path: &str, reason: u32| {
        mock(
            &tokio,
            &conn,
            NM,
            path,
            "EmitSignal",
            &(
                iface,
                "StateChanged",
                "uu",
                vec![Value::from(4u32), Value::from(reason)],
            ),
        );
    };
    // NetworkManager ends the replaced activation (the device
    // disconnected): not a failure.
    state_changed("/org/freedesktop/NetworkManager/ActiveConnection/81", 3);
    settle(300);
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    // The new one is followed: its end is reported.
    state_changed("/org/freedesktop/NetworkManager/ActiveConnection/82", 9);
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        *failures.lock().unwrap(),
        vec![("Cafe".to_string(), network::reason_text(9))]
    );
    s.shutdown();
}

/// (M6, owner-testing gap 20) Where the network cannot be reached (here
/// a system bus that does not answer, so the body fails and backs off)
/// `available` is false, a join is `failed(ssid, error)` and the radio's
/// write is dropped: neither is the caller's error, which a handler
/// faulted on before (`'network' is not running`).
#[test]
fn an_unreachable_network_is_unavailable_and_fails_as_values() {
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
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the failed body", || !b.network.running());
    let cells = b.network.cells();
    assert_eq!(cells.available.get_untracked(&rt), Ok(false));
    let wifi = network::Network::FIELDS
        .iter()
        .position(|f| f.name == "wifi")
        .unwrap();
    b.network
        .dynamic()
        .write(&rt, wifi, &[], Data::Bool(true))
        .unwrap();
    assert_eq!(cells.wifi.get_untracked(&rt), Ok(false), "dropped");
    let ap = AccessPoint {
        ssid: "Home".into(),
        ..AccessPoint::default()
    };
    b.network
        .dynamic()
        .action(&rt, "connect", Some(&ap.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "the refusal", || {
        !failures.lock().unwrap().is_empty()
    });
    let f = failures.lock().unwrap().clone();
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, "Home");
    assert!(f[0].1.contains("not running"), "{f:?}");
    // (m6-service-gaps) The service's own actions, naming no network.
    for name in ["disconnect", "scan"] {
        b.network
            .dynamic()
            .action(&rt, name, None, &[], Default::default())
            .unwrap();
    }
    until(&rt, &s, "the refusals", || {
        failures.lock().unwrap().len() == 3
    });
    let f = failures.lock().unwrap().clone();
    for (ssid, error) in &f[1..] {
        assert_eq!(ssid, "", "{f:?}");
        assert!(error.contains("not running"), "{f:?}");
    }
    s.shutdown();
}

/// An access point at `freq` MHz with `security` as its WPA and RSN flags
/// (python-dbusmock writes both alike).
#[allow(clippy::too_many_arguments)]
fn add_ap_at(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    dev: &str,
    name: &str,
    ssid: &str,
    strength: u8,
    freq: u32,
    security: u32,
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
            "00:23:F8:7E:12:BB",
            2u32,
            freq,
            5400u32,
            strength,
            security,
        ),
    )
    .body()
    .deserialize()
    .unwrap()
}

/// A Wi-Fi device on interface `iface` with network `ssid` in range at
/// `freq`, saved, and an active connection for it in `state`: the
/// device's, the access point's and the active connection's paths.
fn wifi_with(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    iface: &str,
    ssid: &str,
    freq: u32,
    state: u32,
) -> (String, String, String) {
    let dev: String = mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddWiFiDevice",
        &("mock_WiFi", iface, 100i32),
    )
    .body()
    .deserialize()
    .unwrap();
    let ap = add_ap_at(tokio, conn, &dev, "Mock_AP1", ssid, 70, freq, 0x100);
    let saved: String = mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddWiFiConnection",
        &(dev.as_str(), "Mock_Con1", ssid, "wpa-psk"),
    )
    .body()
    .deserialize()
    .unwrap();
    let active: String = mock(
        tokio,
        conn,
        NM,
        ROOT,
        "AddActiveConnection",
        &(
            vec![dev.as_str()],
            saved.as_str(),
            ap.as_str(),
            "Mock_Active1",
            state,
        ),
    )
    .body()
    .deserialize()
    .unwrap();
    (dev, ap, active)
}

fn field(name: &str) -> usize {
    network::Network::FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap()
}

const ACTIVE_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";

/// The mock's `SetProperty`: a property set and its change signalled.
fn set_property(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    path: &str,
    iface: &str,
    name: &str,
    value: zbus::zvariant::Value<'_>,
) {
    mock(
        tokio,
        conn,
        NM,
        ROOT,
        "SetProperty",
        &(path, iface, name, value),
    );
}

/// (m6-service-gaps, gap 28) The joined network's frequency and security
/// (from its access point's flags, followed as they change), and its
/// address, read from the kernel only while a reader reads it: here the
/// device's interface is `lo`, whose address is 127.0.0.1.
#[test]
fn network_tells_the_joined_networks_details() {
    use strand_services::network::WifiSecurity;
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (_dev, ap, _) = wifi_with(&tokio, &conn, "lo", "Home", 5180, 2);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(Some("Home".to_string())));
    assert_eq!(cells.frequency.get_untracked(&rt), Ok(Some(5180)));
    assert_eq!(
        cells.security.get_untracked(&rt),
        Ok(Some(WifiSecurity::Wpa2))
    );
    // Nobody reads the address: it is not followed.
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(cells.address.get_untracked(&rt), Ok(None));
    b.network.acquire_field(field("address"));
    until(&rt, &s, "the address", || {
        cells.address.get_untracked(&rt) == Ok(Some("127.0.0.1".to_string()))
    });
    b.network.release_field(field("address"));
    until(&rt, &s, "the address let go", || {
        cells.address.get_untracked(&rt) == Ok(None)
    });
    // The access point now offers SAE alone: WPA3.
    mock(
        &tokio,
        &conn,
        NM,
        &ap,
        "UpdateProperties",
        &(
            "org.freedesktop.NetworkManager.AccessPoint",
            std::collections::HashMap::from([
                ("WpaFlags", zbus::zvariant::Value::from(0u32)),
                ("RsnFlags", zbus::zvariant::Value::from(0x400u32)),
            ]),
        ),
    );
    until(&rt, &s, "WPA3", || {
        cells.security.get_untracked(&rt) == Ok(Some(WifiSecurity::Wpa3))
    });
    s.shutdown();
}

/// (gap 28) A network NetworkManager is activating is `connecting`, not
/// yet `ssid` (it was joined at once before); activated, it is joined.
#[test]
fn network_says_what_it_is_joining_and_leaving() {
    use zbus::zvariant::Value;
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (_, _, active) = wifi_with(&tokio, &conn, "wlan0", "Cafe", 2412, 1);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.connecting.get_untracked(&rt), Ok(Some("Cafe".into())));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(None));
    assert_eq!(cells.frequency.get_untracked(&rt), Ok(None));
    set_property(
        &tokio,
        &conn,
        &active,
        ACTIVE_IFACE,
        "State",
        Value::from(2u32),
    );
    mock(
        &tokio,
        &conn,
        NM,
        &active,
        "EmitSignal",
        &(
            ACTIVE_IFACE,
            "StateChanged",
            "uu",
            vec![Value::from(2u32), Value::from(0u32)],
        ),
    );
    until(&rt, &s, "Cafe joined", || {
        cells.ssid.get_untracked(&rt) == Ok(Some("Cafe".into()))
            && cells.connecting.get_untracked(&rt) == Ok(None)
    });
    assert_eq!(cells.frequency.get_untracked(&rt), Ok(Some(2412)));
    // NetworkManager leaving it on its own: `disconnecting`.
    set_property(
        &tokio,
        &conn,
        &active,
        ACTIVE_IFACE,
        "State",
        Value::from(3u32),
    );
    until(&rt, &s, "Cafe being left", || {
        cells.disconnecting.get_untracked(&rt) == Ok(Some("Cafe".into()))
            && cells.ssid.get_untracked(&rt) == Ok(None)
    });
    s.shutdown();
}

/// (gap 28) `network.disconnect()` is the device's `Disconnect`; the
/// network being left is `disconnecting` until NetworkManager has left
/// it, and then nothing of it is told. With nothing joined it fails.
#[test]
fn disconnect_leaves_the_joined_network() {
    use zbus::zvariant::Value;
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (dev, _, active) = wifi_with(&tokio, &conn, "lo", "Home", 5180, 2);
    // The template's device has no `Disconnect`.
    mock(
        &tokio,
        &conn,
        NM,
        &dev,
        "AddMethod",
        &(
            "org.freedesktop.NetworkManager.Device",
            "Disconnect",
            "",
            "",
            "",
        ),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    b.network.acquire_field(field("address"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the address", || {
        cells.address.get_untracked(&rt) == Ok(Some("127.0.0.1".to_string()))
    });
    let disconnect = || {
        b.network
            .dynamic()
            .action(&rt, "disconnect", None, &[], Default::default())
            .unwrap();
    };
    disconnect();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mock_calls(&tokio, &conn, NM, &dev, "Disconnect").is_empty() {
        assert!(std::time::Instant::now() < deadline, "never disconnected");
        std::thread::sleep(Duration::from_millis(20));
    }
    // NetworkManager leaves it.
    set_property(
        &tokio,
        &conn,
        &active,
        ACTIVE_IFACE,
        "State",
        Value::from(3u32),
    );
    until(&rt, &s, "Home being left", || {
        cells.disconnecting.get_untracked(&rt) == Ok(Some("Home".into()))
    });
    mock(
        &tokio,
        &conn,
        NM,
        ROOT,
        "RemoveActiveConnection",
        &(dev.as_str(), active.as_str()),
    );
    mock(
        &tokio,
        &conn,
        NM,
        ROOT,
        "SetGlobalConnectionState",
        &(20u32,),
    );
    until(&rt, &s, "Home left", || {
        cells.ssid.get_untracked(&rt) == Ok(None)
            && cells.disconnecting.get_untracked(&rt) == Ok(None)
            && cells.connected.get_untracked(&rt) == Ok(false)
    });
    assert_eq!(cells.address.get_untracked(&rt), Ok(None));
    assert_eq!(cells.frequency.get_untracked(&rt), Ok(None));
    assert_eq!(cells.security.get_untracked(&rt), Ok(None));
    assert_eq!(mock_calls(&tokio, &conn, NM, &dev, "Disconnect").len(), 1);
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    // Nothing joined: it fails, naming no network.
    disconnect();
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        (String::new(), "no Wi-Fi network is joined".to_string())
    );
    assert_eq!(mock_calls(&tokio, &conn, NM, &dev, "Disconnect").len(), 1);
    s.shutdown();
}

/// (gap 28) `network.scan()` asks a scan and raises `scanning` until the
/// device's `LastScan` says it ended; a menu opening raises it too. With
/// the radio off the scan fails.
#[test]
fn an_explicit_scan_raises_scanning_until_it_ends() {
    use zbus::zvariant::Value;
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    let (dev, _) = wifi_home(&tokio, &conn);
    let wireless = "org.freedesktop.NetworkManager.Device.Wireless";
    mock(
        &tokio,
        &conn,
        NM,
        &dev,
        "AddProperty",
        &(wireless, "LastScan", Value::from(1i64)),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.scanning.get_untracked(&rt), Ok(false));
    let scans = || mock_calls(&tokio, &conn, NM, &dev, "RequestScan").len();
    let scan = || {
        b.network
            .dynamic()
            .action(&rt, "scan", None, &[], Default::default())
            .unwrap();
    };
    scan();
    until(&rt, &s, "scanning", || {
        cells.scanning.get_untracked(&rt) == Ok(true)
    });
    assert_eq!(scans(), 1);
    set_property(&tokio, &conn, &dev, wireless, "LastScan", Value::from(2i64));
    until(&rt, &s, "the scan's end", || {
        cells.scanning.get_untracked(&rt) == Ok(false)
    });
    // A menu opening asks a scan too.
    b.network.acquire_field(field("access_points"));
    until(&rt, &s, "the menu's scan", || {
        cells.scanning.get_untracked(&rt) == Ok(true)
    });
    assert_eq!(scans(), 2);
    set_property(&tokio, &conn, &dev, wireless, "LastScan", Value::from(3i64));
    until(&rt, &s, "the menu's scan ended", || {
        cells.scanning.get_untracked(&rt) == Ok(false)
    });
    b.network.release_field(field("access_points"));
    // The radio off: the scan fails, naming no network.
    b.network
        .dynamic()
        .write(&rt, field("wifi"), &[], Data::Bool(false))
        .unwrap();
    until(&rt, &s, "the radio off", || {
        cells.wifi.get_untracked(&rt) == Ok(false)
    });
    scan();
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        (String::new(), "Wi-Fi is off".to_string())
    );
    assert_eq!(scans(), 2);
    assert_eq!(cells.scanning.get_untracked(&rt), Ok(false));
    s.shutdown();
}

/// (services-round) A connection saved in NetworkManager's settings.
fn save_connection(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    id: &str,
    uuid: &str,
    kind: &str,
) -> String {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    let settings: HashMap<&str, HashMap<&str, Value<'_>>> = HashMap::from([(
        "connection",
        HashMap::from([
            ("id", Value::from(id)),
            ("uuid", Value::from(uuid)),
            ("type", Value::from(kind)),
        ]),
    )]);
    call(
        tokio,
        conn,
        NM,
        "/org/freedesktop/NetworkManager/Settings",
        "org.freedesktop.NetworkManager.Settings",
        "AddConnection",
        &(settings,),
    )
    .body()
    .deserialize::<zbus::zvariant::OwnedObjectPath>()
    .unwrap()
    .to_string()
}

/// (services-round) `network.connections` lists the saved connections
/// only while a visible reader reads them (loopback left out), follows
/// one saved meanwhile, `c.up()` and `c.down()` switch a VPN, `kind`
/// names the connection carrying the traffic (the VPN while it is up),
/// and `down()` of one that is not up fails as `failed(name, error)`.
#[test]
fn saved_connections_are_listed_while_read_and_switched() {
    use strand_services::network::{Connection, ConnectionKind};
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_nm) = DbusMock::start(&bus, "networkmanager", true, None, NM) else {
        return;
    };
    mock(
        &tokio,
        &conn,
        NM,
        ROOT,
        "AddEthernetDevice",
        &("mock_Eth", "eth0", 30i32),
    );
    wifi_home(&tokio, &conn);
    save_connection(&tokio, &conn, "Wired", "u-wired", "802-3-ethernet");
    save_connection(&tokio, &conn, "Work VPN", "u-vpn", "vpn");
    save_connection(&tokio, &conn, "lo", "u-lo", "loopback");
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let list = || -> Vec<Connection> {
        cells
            .connections
            .get_untracked(&rt)
            .unwrap()
            .items()
            .iter()
            .map(|(_, c)| c.clone())
            .collect()
    };
    assert_eq!(
        cells.kind.get_untracked(&rt),
        Ok(Some(ConnectionKind::Wifi))
    );
    assert!(list().is_empty(), "not read while nobody reads it");

    b.network.acquire_field(field("connections"));
    until(&rt, &s, "the saved connections", || list().len() == 3);
    let shown: Vec<(String, ConnectionKind, bool)> = list()
        .into_iter()
        .map(|c| (c.name, c.kind, c.active))
        .collect();
    assert_eq!(
        shown,
        [
            ("Home".to_string(), ConnectionKind::Wifi, true),
            ("Wired".to_string(), ConnectionKind::Ethernet, false),
            ("Work VPN".to_string(), ConnectionKind::Vpn, false),
        ]
    );
    // One saved while it is read: listed.
    save_connection(&tokio, &conn, "wg0", "u-wg", "wireguard");
    until(&rt, &s, "the new connection", || {
        list()
            .iter()
            .any(|c| c.id == "u-wg" && c.kind == ConnectionKind::Wireguard)
    });

    // A connection's state changing (below) lists none of them again.
    let settings = "/org/freedesktop/NetworkManager/Settings";
    let listings = || mock_calls(&tokio, &conn, NM, settings, "ListConnections").len();
    let listed = listings();
    // Up: the VPN is up, and carries the traffic.
    let vpn = list().into_iter().find(|c| c.id == "u-vpn").unwrap();
    b.network
        .dynamic()
        .action(&rt, "up", Some(&vpn.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "the VPN up", || {
        list().iter().any(|c| c.id == "u-vpn" && c.active)
            && cells.kind.get_untracked(&rt) == Ok(Some(ConnectionKind::Vpn))
    });
    let activated = mock_calls(&tokio, &conn, NM, ROOT, "ActivateConnection");
    assert_eq!(activated.len(), 1);
    // Down: it goes.
    b.network
        .dynamic()
        .action(&rt, "down", Some(&vpn.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "the VPN down", || {
        list().iter().any(|c| c.id == "u-vpn" && !c.active)
    });
    assert_eq!(
        mock_calls(&tokio, &conn, NM, ROOT, "DeactivateConnection").len(),
        1
    );
    assert_eq!(listings(), listed, "the saved connections were kept");
    // Down of one that is not up: a failure naming it.
    let wired = list().into_iter().find(|c| c.id == "u-wired").unwrap();
    b.network
        .dynamic()
        .action(&rt, "down", Some(&wired.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "the refusal", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        ("Wired".to_string(), "it is not active".to_string())
    );
    // Up of one NetworkManager does not know: its error.
    let gone = Connection {
        id: "u-gone".into(),
        name: "Gone".into(),
        ..Connection::default()
    };
    b.network
        .dynamic()
        .action(&rt, "up", Some(&gone.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "the unknown one's failure", || {
        failures.lock().unwrap().len() == 2
    });
    assert_eq!(failures.lock().unwrap()[1].0, "Gone");

    // Nobody reads them: the list goes.
    b.network.release_field(field("connections"));
    until(&rt, &s, "the list dropped", || list().is_empty());
    s.shutdown();
}

/// (services-round) With no system bus, `up()` is refused as
/// `failed(name, why)`.
#[test]
fn without_a_bus_up_fails_naming_the_connection() {
    use strand_services::network::Connection;
    let rt = Runtime::new();
    let (s, b) = services(&rt, strand_services::Buses::none());
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let vpn = Connection {
        id: "u-vpn".into(),
        name: "Work VPN".into(),
        ..Connection::default()
    };
    b.network
        .dynamic()
        .action(&rt, "up", Some(&vpn.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "the refusal", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        ("Work VPN".to_string(), "no system bus".to_string())
    );
    s.shutdown();
}

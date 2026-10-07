//! `network`: NetworkManager on the system bus, with our own small zbus
//! client (decisions.md, wave4-a2: nmrs always connects to the machine's
//! system bus, so it cannot run on the `Buses` a registry is given).
//!
//! Followed live ([`Daemon`]): the manager object (`State`,
//! `WirelessEnabled`, `ActiveConnections`, `Devices`), the active
//! connections and the access point each Wi-Fi connection uses. The
//! access points in range (`access_points`, a `#[store(stream)]` field)
//! are tracked, and a scan requested (`RequestScan`), only while a
//! visible reader reads them; otherwise their strength changes do not
//! even reach this process (no match rule for them). NetworkManager
//! restarting is read afresh; missing, it is "offline".

use std::collections::BTreeMap;

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Call, Cx, Msg, ServiceError, Store, service};

/// The schema the `network` service serves.
pub const SCHEMA: &str = strand_services_schema::NETWORK;

/// NetworkManager's bus name.
pub const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const AP_NAMESPACE: &str = "/org/freedesktop/NetworkManager/AccessPoint";

/// A Wi-Fi network in range.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "AccessPoint", key = ssid)]
pub struct AccessPoint {
    pub ssid: String,
    pub strength: f64,
    pub secure: bool,
    pub active: bool,
}

/// `network`'s actions.
#[derive(Call, Debug)]
pub enum NetworkAction {
    /// `ap.connect()`.
    Connect { item: AccessPoint },
}

/// See the module docs.
#[service(name = "network", action = NetworkAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Network {
    /// A network connection is up.
    pub connected: bool,
    /// The Wi-Fi network joined, if any.
    pub ssid: Option<String>,
    /// The Wi-Fi signal strength, 0 to 1.
    pub strength: f64,
    /// The Wi-Fi radio is on. Writable.
    #[store(rw)]
    pub wifi: bool,
    /// The Wi-Fi networks in range, keyed by `ssid`. Scanned only while a
    /// visible reader reads them (a network menu that is open).
    #[store(keyed, stream)]
    pub access_points: Vec<AccessPoint>,
}

/// An access point's SSID (bytes; shown lossily as UTF-8).
fn ssid(p: &Props) -> Option<String> {
    ssid_at(p, "Ssid")
}

/// The SSID bytes under `key` (`Ssid` on an access point, `ssid` in a
/// connection's settings).
fn ssid_at(p: &Props, key: &str) -> Option<String> {
    let bytes: Vec<u8> = p.get(key)?.try_clone().ok()?.try_into().ok()?;
    let s = String::from_utf8_lossy(&bytes)
        .trim_end_matches('\0')
        .to_string();
    (!s.is_empty()).then_some(s)
}

fn strength(p: &Props) -> f64 {
    (dbus::number(p, "Strength").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0)
}

fn secure(p: &Props) -> bool {
    ["Flags", "WpaFlags", "RsnFlags"]
        .iter()
        .any(|k| dbus::number(p, k).unwrap_or(0.0) != 0.0)
}

fn object_paths(p: &Props, name: &str) -> Vec<String> {
    p.get(name)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<zbus::zvariant::OwnedObjectPath>::try_from(v).ok())
        .map(dbus::paths)
        .unwrap_or_default()
}

fn object_path(p: &Props, name: &str) -> Option<String> {
    let path: zbus::zvariant::ObjectPath<'_> = p.get(name)?.downcast_ref().ok()?;
    let s = path.to_string();
    (s != "/").then_some(s)
}

/// NetworkManager's objects as last read.
#[derive(Debug, Default)]
pub struct Nm {
    manager: Props,
    active: BTreeMap<String, Props>,
    /// Wi-Fi devices: their `Wireless` properties.
    wifi_devices: BTreeMap<String, Props>,
    /// Access points read: those Wi-Fi connections use, and while
    /// scanning every one in range.
    aps: BTreeMap<String, Props>,
}

impl Nm {
    /// The access points the Wi-Fi connections use.
    fn used_aps(&self) -> Vec<String> {
        self.active
            .values()
            .filter(|c| dbus::text(c, "Type").as_deref() == Some("802-11-wireless"))
            .filter_map(|c| object_path(c, "SpecificObject"))
            .collect()
    }

    /// The state these objects make.
    pub fn state(&self, scanning: bool) -> Network {
        let state = dbus::number(&self.manager, "State").unwrap_or(0.0);
        let used = self.used_aps();
        // The Wi-Fi connection that is the default route wins.
        let mut wifi: Vec<&Props> = self
            .active
            .values()
            .filter(|c| dbus::text(c, "Type").as_deref() == Some("802-11-wireless"))
            .collect();
        wifi.sort_by_key(|c| !dbus::boolean(c, "Default").unwrap_or(false));
        let joined = wifi
            .first()
            .and_then(|c| object_path(c, "SpecificObject"))
            .and_then(|p| self.aps.get(&p));
        let mut access_points: Vec<AccessPoint> = Vec::new();
        if scanning {
            let mut by_ssid: BTreeMap<String, AccessPoint> = BTreeMap::new();
            for (path, p) in &self.aps {
                let Some(name) = ssid(p) else {
                    continue;
                };
                let ap = AccessPoint {
                    ssid: name.clone(),
                    strength: strength(p),
                    secure: secure(p),
                    active: used.contains(path),
                };
                by_ssid
                    .entry(name)
                    .and_modify(|e| {
                        e.active |= ap.active;
                        if ap.strength > e.strength {
                            e.strength = ap.strength;
                        }
                    })
                    .or_insert(ap);
            }
            access_points = by_ssid.into_values().collect();
            // Strongest first; the joined one leads.
            access_points.sort_by(|a, b| {
                b.active
                    .cmp(&a.active)
                    .then(b.strength.total_cmp(&a.strength))
                    .then(a.ssid.cmp(&b.ssid))
            });
        }
        Network {
            // NM_STATE_CONNECTED_LOCAL (50) and up.
            connected: state >= 50.0,
            ssid: joined.and_then(ssid),
            strength: joined.map(strength).unwrap_or(0.0),
            wifi: dbus::boolean(&self.manager, "WirelessEnabled").unwrap_or(false),
            access_points,
        }
    }

    /// Read everything from the current owner (`scanning`: every access
    /// point in range too).
    async fn read(&mut self, daemon: &Daemon, scanning: bool) {
        *self = Nm::default();
        if daemon.owner().is_none() {
            return;
        }
        let conn = daemon.conn();
        self.manager = dbus::get_all(conn, NM, ROOT, NM).await.unwrap_or_default();
        for path in object_paths(&self.manager, "ActiveConnections") {
            if let Ok(p) = dbus::get_all(conn, NM, &path, ACTIVE).await {
                self.active.insert(path, p);
            }
        }
        for path in object_paths(&self.manager, "Devices") {
            let Ok(dev) = dbus::get_all(conn, NM, &path, DEVICE).await else {
                continue;
            };
            // NM_DEVICE_TYPE_WIFI.
            if dbus::number(&dev, "DeviceType") != Some(2.0) {
                continue;
            }
            if let Ok(w) = dbus::get_all(conn, NM, &path, WIRELESS).await {
                self.wifi_devices.insert(path, w);
            }
        }
        let mut wanted = self.used_aps();
        if scanning {
            for w in self.wifi_devices.values() {
                wanted.extend(object_paths(w, "AccessPoints"));
            }
        }
        for path in wanted {
            if let Ok(p) = dbus::get_all(conn, NM, &path, AP).await {
                self.aps.insert(path, p);
            }
        }
    }

    /// Apply a signal: `Some(true)` it changed something, `Some(false)` it
    /// did not, `None` read everything again (the set of objects moved).
    async fn signal(&mut self, daemon: &Daemon, m: &zbus::Message) -> Option<bool> {
        let c = dbus::properties_changed(m)?;
        match c.iface.as_str() {
            AP => match self.aps.get_mut(&c.path) {
                Some(p) => {
                    dbus::apply_changed(daemon.conn(), NM, p, c).await;
                    Some(true)
                }
                None => Some(false),
            },
            // The manager's state or radio: applied in place unless its
            // objects changed.
            NM if c.path == ROOT => {
                let moves = c.changed.contains_key("ActiveConnections")
                    || c.changed.contains_key("Devices");
                if moves {
                    return None;
                }
                dbus::apply_changed(daemon.conn(), NM, &mut self.manager, c).await;
                Some(true)
            }
            ACTIVE => match self.active.get_mut(&c.path) {
                Some(p) if !c.changed.contains_key("SpecificObject") => {
                    dbus::apply_changed(daemon.conn(), NM, p, c).await;
                    Some(true)
                }
                _ => None,
            },
            WIRELESS if c.changed.contains_key("AccessPoints") => None,
            _ => Some(false),
        }
    }
}

impl Network {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return crate::battery::idle_without_bus(&mut cx, "system", e).await,
        };
        let mut daemon = Daemon::new(&conn, NM).await?;
        // The manager, the active connections and the devices' Wi-Fi
        // interface (access points coming and going); access points only
        // while scanning.
        daemon.subscribe("manager", dbus::path_rule(ROOT)?).await?;
        daemon
            .subscribe(
                "active",
                dbus::namespace_rule("/org/freedesktop/NetworkManager/ActiveConnection")?,
            )
            .await?;
        daemon
            .subscribe(
                "devices",
                dbus::namespace_rule("/org/freedesktop/NetworkManager/Devices")?,
            )
            .await?;
        let mut nm = Nm::default();
        let mut scanning = cx.watched("access_points");
        let mut used = None;
        // A scan is asked for when scanning starts and when a new
        // NetworkManager appears, not on every re-read.
        let mut want_scan = scanning;
        loop {
            if scanning {
                sync_subscriptions(&mut daemon, &nm, scanning, &mut used).await;
            }
            nm.read(&daemon, scanning).await;
            sync_subscriptions(&mut daemon, &nm, scanning, &mut used).await;
            if scanning && want_scan {
                request_scan(&daemon, &nm).await;
                want_scan = false;
            }
            if !cx.update(|s| *s = nm.state(scanning)) {
                return Ok(());
            }
            cx.ready();
            'follow: loop {
                tokio::select! {
                    ev = daemon.next() => match ev {
                        None => return Err(ServiceError("the system bus connection ended".into())),
                        Some(DaemonEvent::Owner) => {
                            want_scan = scanning;
                            break 'follow;
                        }
                        Some(DaemonEvent::Signal(m)) => match nm.signal(&daemon, &m).await {
                            None => break 'follow,
                            Some(false) => {}
                            Some(true) => {
                                if !cx.update(|s| *s = nm.state(scanning)) {
                                    return Ok(());
                                }
                            }
                        },
                    },
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Watch { field: "access_points", on }) => {
                            scanning = on;
                            want_scan = on;
                            break 'follow;
                        }
                        Some(Msg::Write(w)) if w.field == "wifi" => {
                            let on: bool = w.value().unwrap_or(nm.state(scanning).wifi);
                            let set = dbus::set(&conn, NM, ROOT, NM, "WirelessEnabled", on.into()).await;
                            let now = match set {
                                Ok(()) => on,
                                Err(e) => {
                                    log::warn!("network: Wi-Fi not switched: {e}");
                                    !on
                                }
                            };
                            if !cx.report(&w, |s| s.wifi = now) {
                                return Ok(());
                            }
                        }
                        Some(Msg::Action(NetworkAction::Connect { item })) => {
                            if let Err(e) = connect(&daemon, &nm, &item.ssid).await {
                                log::warn!("network: not joining {}: {e}", item.ssid);
                            }
                        }
                        Some(_) => {}
                    },
                }
            }
        }
    }
}

/// The access points' signals only while scanning; otherwise the signals
/// of the access point in use (its strength), one rule replaced when the
/// connection moves.
async fn sync_subscriptions(
    daemon: &mut Daemon,
    nm: &Nm,
    scanning: bool,
    used: &mut Option<String>,
) {
    if scanning {
        daemon.unsubscribe("used");
        *used = None;
        if !daemon.subscribed("aps")
            && let Ok(r) = dbus::namespace_rule(AP_NAMESPACE)
            && let Err(e) = daemon.subscribe("aps", r).await
        {
            log::debug!("network: no access point signals: {e}");
        }
        return;
    }
    daemon.unsubscribe("aps");
    let now = nm.used_aps().into_iter().next();
    if *used == now {
        return;
    }
    daemon.unsubscribe("used");
    *used = None;
    if let Some(path) = now
        && let Ok(r) = dbus::path_rule(&path)
    {
        match daemon.subscribe("used", r).await {
            Ok(()) => *used = Some(path),
            Err(e) => log::debug!("network: no signals of {path}: {e}"),
        }
    }
}

/// Ask every Wi-Fi device for a fresh scan (NetworkManager rate-limits
/// these itself).
async fn request_scan(daemon: &Daemon, nm: &Nm) {
    for dev in nm.wifi_devices.keys() {
        let opts: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = Default::default();
        let r = daemon
            .conn()
            .call_method(
                Some(NM),
                dev.as_str(),
                Some(WIRELESS),
                "RequestScan",
                &(opts,),
            )
            .await;
        if let Err(e) = r {
            log::debug!("network: no scan on {dev}: {e}");
        }
    }
}

/// Join the network named `ssid`: its strongest access point, with a
/// saved connection for that name if there is one.
async fn connect(daemon: &Daemon, nm: &Nm, name: &str) -> zbus::Result<()> {
    use zbus::zvariant::ObjectPath;
    let conn = daemon.conn();
    let mut best: Option<(&String, &String, f64)> = None;
    for (dev, w) in &nm.wifi_devices {
        for ap in object_paths(w, "AccessPoints") {
            let Some(p) = nm.aps.get(&ap) else {
                continue;
            };
            if ssid(p).as_deref() != Some(name) {
                continue;
            }
            let s = strength(p);
            if best.is_none_or(|b| s > b.2) {
                let ap_key = nm.aps.get_key_value(&ap).map(|(k, _)| k);
                if let Some(k) = ap_key {
                    best = Some((dev, k, s));
                }
            }
        }
    }
    let Some((dev, ap, _)) = best else {
        return Err(zbus::Error::Failure(format!("`{name}` is not in range")));
    };
    let dev = ObjectPath::try_from(dev.as_str())?;
    let ap = ObjectPath::try_from(ap.as_str())?;
    // A saved connection for the name?
    let saved: Vec<zbus::zvariant::OwnedObjectPath> = conn
        .call_method(
            Some(NM),
            SETTINGS,
            Some(SETTINGS_IFACE),
            "ListConnections",
            &(),
        )
        .await?
        .body()
        .deserialize()?;
    type Settings = std::collections::HashMap<
        String,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    >;
    for path in saved {
        let Ok(reply) = conn
            .call_method(
                Some(NM),
                path.as_str(),
                Some(CONNECTION_IFACE),
                "GetSettings",
                &(),
            )
            .await
        else {
            continue;
        };
        let Ok(settings) = reply.body().deserialize::<Settings>() else {
            continue;
        };
        let matches = settings
            .get("802-11-wireless")
            .is_some_and(|w| ssid_at(w, "ssid").as_deref() == Some(name));
        if matches {
            conn.call_method(
                Some(NM),
                ROOT,
                Some(NM),
                "ActivateConnection",
                &(path.as_ref(), &dev, &ap),
            )
            .await?;
            return Ok(());
        }
    }
    let empty: std::collections::HashMap<
        &str,
        std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    > = Default::default();
    conn.call_method(
        Some(NM),
        ROOT,
        Some(NM),
        "AddAndActivateConnection",
        &(empty, &dev, &ap),
    )
    .await?;
    Ok(())
}

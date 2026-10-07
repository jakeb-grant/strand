//! `network`: NetworkManager on the system bus, with our own small zbus
//! client (decisions.md, wave4-a2: nmrs always connects to the machine's
//! system bus, so it cannot run on the `Buses` a registry is given).
//!
//! Followed live ([`Daemon`]): the manager object (`State`,
//! `WirelessEnabled`, `ActiveConnections`, `Devices`), the active
//! connections and the access point each Wi-Fi connection uses. The
//! access points in range (`access_points`, a `#[store(stream)]` field)
//! are tracked, and a scan requested (`RequestScan`), only while a
//! visible reader reads them; otherwise their strength changes, and the
//! Wi-Fi devices' access point lists, do not even reach this process (no
//! match rule for them). NetworkManager restarting is read afresh;
//! missing, it is "offline".
//!
//! Joining (`ap.connect()`, `ap.connect_with(password)`) follows the
//! active connection NetworkManager answers with until it is up; if it
//! is deactivated first (`StateChanged` to DEACTIVATED, a missing or
//! wrong password, a timeout) or the call itself fails, `failed(ssid,
//! error)` says why.

use std::collections::{BTreeMap, HashMap};

use zbus::zvariant::OwnedValue;

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

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
    /// `ap.connect_with(password)`.
    ConnectWith { item: AccessPoint, password: String },
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
    /// Joining a network failed (no password and no secret agent to ask
    /// for one, a wrong one, out of range, timed out): `on
    /// network.failed(ssid, error) { … }`.
    pub failed: Event<(String, String)>,
}

/// How long NetworkManager may take to accept an activation (its own
/// D-Bus timeout is 25 s); the outcome comes later, as `StateChanged`.
const ACTIVATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// `NMActiveConnectionState`: activated.
const ACTIVATED: u32 = 2;
/// `NMActiveConnectionState`: deactivated.
const DEACTIVATED: u32 = 4;

/// Why an activation ended, for `failed` (`NMActiveConnectionStateReason`).
pub fn reason_text(reason: u32) -> String {
    match reason {
        2 => "disconnected".into(),
        3 => "the Wi-Fi device disconnected".into(),
        4 | 7 | 8 => "a NetworkManager service failed".into(),
        5 => "no IP address was given".into(),
        6 => "it timed out".into(),
        9 => "a password is needed (none was given, or it was wrong)".into(),
        10 => "the login failed (a wrong password?)".into(),
        11 => "the connection was removed".into(),
        12 => "a connection it depends on failed".into(),
        13 | 14 => "the Wi-Fi device is gone".into(),
        r => format!("it could not be joined (reason {r})"),
    }
}

/// A network being joined: the active connection NetworkManager made
/// for it, followed until it is up or deactivated.
#[derive(Debug)]
struct Joining {
    ssid: String,
    /// It was listed among the active connections (once gone from them
    /// before it came up, it failed).
    seen: bool,
}

/// A state change of an active connection: its path, state and reason
/// (`StateChanged`, sent since NetworkManager 1.8). The `State` property
/// change NetworkManager sends with it carries no reason, so a join is
/// not settled on it: the reason (a password needed, say) would be lost.
fn active_state(m: &zbus::Message) -> Option<(String, u32, u32)> {
    let path = dbus::path(m)?;
    if dbus::interface(m).as_deref() == Some(ACTIVE)
        && dbus::member(m).as_deref() == Some("StateChanged")
    {
        let (state, reason): (u32, u32) = m.body().deserialize().ok()?;
        return Some((path, state, reason));
    }
    None
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

/// It asks for a key: `Flags` has PRIVACY (WEP), or it has WPA or RSN
/// flags. `Flags`' WPS bits (0x2, 0x4, 0x8) say nothing about a key: an
/// open network may offer WPS.
fn secure(p: &Props) -> bool {
    let n = |k| dbus::number(p, k).map_or(0, |n| n as u32);
    n("Flags") & 0x1 != 0 || n("WpaFlags") != 0 || n("RsnFlags") != 0
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
            failed: Event::default(),
        }
    }

    /// Read everything from the current owner (`scanning`: every access
    /// point in range too). Each read is bounded ([`dbus::READ_TIMEOUT`]):
    /// a hung NetworkManager is an error (the run fails and is retried),
    /// an object gone meanwhile is left out.
    async fn read(&mut self, daemon: &Daemon, scanning: bool) -> zbus::Result<()> {
        *self = Nm::default();
        if daemon.owner().is_none() {
            return Ok(());
        }
        let conn = daemon.conn();
        let read = |path: String, iface: &'static str| async move {
            match dbus::get_all(conn, NM, &path, iface).await {
                Ok(p) => Ok(Some((path, p))),
                Err(e) if dbus::is_timeout(&e) => Err(e),
                Err(_) => Ok(None),
            }
        };
        self.manager = match dbus::get_all(conn, NM, ROOT, NM).await {
            Ok(p) => p,
            Err(e) if dbus::is_timeout(&e) => return Err(e),
            Err(_) => Props::new(),
        };
        for path in object_paths(&self.manager, "ActiveConnections") {
            if let Some((path, p)) = read(path, ACTIVE).await? {
                self.active.insert(path, p);
            }
        }
        for path in object_paths(&self.manager, "Devices") {
            let Some((path, dev)) = read(path, DEVICE).await? else {
                continue;
            };
            // NM_DEVICE_TYPE_WIFI.
            if dbus::number(&dev, "DeviceType") != Some(2.0) {
                continue;
            }
            if let Some((path, w)) = read(path, WIRELESS).await? {
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
            if let Some((path, p)) = read(path, AP).await? {
                self.aps.insert(path, p);
            }
        }
        Ok(())
    }

    /// Apply a signal: `Some(true)` it changed something, `Some(false)` it
    /// did not, `None` read everything again (the set of objects moved).
    /// A device's access points coming and going matter only while
    /// `scanning` (the one in use is followed through its connection's
    /// `SpecificObject`); then only the new ones are read.
    async fn signal(&mut self, daemon: &Daemon, m: &zbus::Message, scanning: bool) -> Option<bool> {
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
                Some(_) => None,
                // Not one of ours (yet): the manager's `ActiveConnections`
                // says when it is.
                None => Some(false),
            },
            WIRELESS if c.changed.contains_key("AccessPoints") => {
                if !scanning {
                    return Some(false);
                }
                let Some(dev) = self.wifi_devices.get_mut(&c.path) else {
                    return Some(false);
                };
                dev.extend(c.changed);
                let listed: Vec<String> = self
                    .wifi_devices
                    .values()
                    .flat_map(|w| object_paths(w, "AccessPoints"))
                    .chain(self.used_aps())
                    .collect();
                self.aps.retain(|p, _| listed.contains(p));
                for path in listed {
                    if self.aps.contains_key(&path) {
                        continue;
                    }
                    if let Ok(p) = dbus::get_all(daemon.conn(), NM, &path, AP).await {
                        self.aps.insert(path, p);
                    }
                }
                Some(true)
            }
            _ => Some(false),
        }
    }
}

impl Network {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        let mut daemon = Daemon::new(&conn, NM).await?;
        // The manager and the active connections; the devices' Wi-Fi
        // interface (access points coming and going) and the access
        // points only while scanning.
        // The active connections first: their `StateChanged` (a join's
        // outcome and reason) is taken before the manager's list change
        // that drops a failed one.
        daemon
            .subscribe(
                "active",
                dbus::namespace_rule("/org/freedesktop/NetworkManager/ActiveConnection")?,
            )
            .await?;
        daemon.subscribe("manager", dbus::path_rule(ROOT)?).await?;
        let mut nm = Nm::default();
        let mut joining: BTreeMap<String, Joining> = BTreeMap::new();
        let mut scanning = cx.watched("access_points");
        let mut used = None;
        // A scan is asked for when scanning starts and when a new
        // NetworkManager appears, not on every re-read.
        let mut want_scan = scanning;
        loop {
            if scanning {
                sync_subscriptions(&mut daemon, &nm, scanning, &mut used).await;
            }
            if let Err(e) = nm.read(&daemon, scanning).await {
                return Err(ServiceError(format!("NetworkManager did not answer: {e}")));
            }
            sync_subscriptions(&mut daemon, &nm, scanning, &mut used).await;
            if !settle_joins(&mut cx, &nm, &mut joining) {
                return Ok(());
            }
            // Asked once there is a Wi-Fi device to ask (a NetworkManager
            // that just appeared may not list its devices yet).
            if scanning && want_scan && request_scan(&daemon, &nm).await {
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
                            // Its activations went with it.
                            for (_, j) in std::mem::take(&mut joining) {
                                if !cx.emit(NetworkEvent::Failed((j.ssid, "NetworkManager stopped".into()))) {
                                    return Ok(());
                                }
                            }
                            break 'follow;
                        }
                        Some(DaemonEvent::Signal(m)) => {
                            if let Some((path, state, reason)) = active_state(&m)
                                && let Some(j) = joining.get(&path)
                            {
                                let ssid = j.ssid.clone();
                                if state == ACTIVATED {
                                    joining.remove(&path);
                                } else if state == DEACTIVATED {
                                    joining.remove(&path);
                                    log::info!("network: joining {ssid} failed: reason {reason}");
                                    if !cx.emit(NetworkEvent::Failed((ssid, reason_text(reason)))) {
                                        return Ok(());
                                    }
                                }
                            }
                            match nm.signal(&daemon, &m, scanning).await {
                            None => break 'follow,
                            Some(false) => {}
                            Some(true) => {
                                if !cx.update(|s| *s = nm.state(scanning)) {
                                    return Ok(());
                                }
                            }
                            }
                        }
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
                                    // The radio as last read.
                                    nm.state(scanning).wifi
                                }
                            };
                            if !cx.report(&w, |s| s.wifi = now) {
                                return Ok(());
                            }
                        }
                        Some(Msg::Action(a)) => {
                            let (item, password) = match a {
                                NetworkAction::Connect { item } => (item, None),
                                NetworkAction::ConnectWith { item, password } => (item, Some(password)),
                            };
                            match connect(&daemon, &nm, &item.ssid, password.as_deref()).await {
                                Ok(active) => {
                                    // Followed until it is up (or not).
                                    joining.insert(active, Joining { ssid: item.ssid, seen: false });
                                }
                                Err(e) => {
                                    log::warn!("network: not joining {}: {e}", item.ssid);
                                    let error = match e {
                                        zbus::Error::MethodError(_, Some(text), _) => text,
                                        e => e.to_string(),
                                    };
                                    if !cx.emit(NetworkEvent::Failed((item.ssid.clone(), error))) {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        Some(_) => {}
                    },
                }
            }
        }
    }
}

/// The Wi-Fi devices' access point lists: their `PropertiesChanged` on
/// the `Wireless` interface only.
fn wireless_rule() -> zbus::Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path_namespace("/org/freedesktop/NetworkManager/Devices")?
        .interface(dbus::PROPERTIES)?
        .member("PropertiesChanged")?
        .arg(0, WIRELESS)?
        .build())
}

/// The access points' signals and the devices' access point lists only
/// while scanning; otherwise the signals of the access point in use (its
/// strength), one rule replaced when the connection moves.
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
        if !daemon.subscribed("devices")
            && let Ok(r) = wireless_rule()
            && let Err(e) = daemon.subscribe("devices", r).await
        {
            log::debug!("network: no device signals: {e}");
        }
        return;
    }
    daemon.unsubscribe("aps");
    daemon.unsubscribe("devices");
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

/// After a read: joins whose connection is up are done; one deactivated,
/// or gone from the active connections once listed, failed. `false` once
/// the service was stopped.
fn settle_joins(cx: &mut Cx<Network>, nm: &Nm, joining: &mut BTreeMap<String, Joining>) -> bool {
    let mut failed = Vec::new();
    joining.retain(|path, j| match nm.active.get(path) {
        Some(p) => {
            j.seen = true;
            match dbus::number(p, "State").map(|n| n as u32) {
                Some(ACTIVATED) => false,
                // Seen deactivated on a re-read: its `StateChanged`, with
                // the reason, was not heard.
                Some(DEACTIVATED) => {
                    failed.push((j.ssid.clone(), "the connection was deactivated".into()));
                    false
                }
                _ => true,
            }
        }
        None if j.seen => {
            failed.push((j.ssid.clone(), "the connection was deactivated".into()));
            false
        }
        None => true,
    });
    failed.into_iter().all(|f| cx.emit(NetworkEvent::Failed(f)))
}

/// The `802-11-wireless-security` settings for joining access point `p`
/// with `password`: WPA/WPA2 personal (`wpa-psk`) or WPA3 personal
/// (`sae`). Enterprise and WEP networks are refused.
fn security(p: &Props, password: &str) -> zbus::Result<HashMap<&'static str, OwnedValue>> {
    // NM_802_11_AP_SEC_KEY_MGMT_PSK, _802_1X, _SAE.
    let mgmt = |k| dbus::number(p, k).map_or(0, |n| n as u32);
    let flags = mgmt("WpaFlags") | mgmt("RsnFlags");
    let key_mgmt = if flags & 0x100 != 0 {
        "wpa-psk"
    } else if flags & 0x400 != 0 {
        "sae"
    } else if flags & 0x200 != 0 {
        return Err(zbus::Error::Failure(
            "an enterprise network: join it from the system's network settings".into(),
        ));
    } else {
        return Err(zbus::Error::Failure(
            "not a WPA network with a password".into(),
        ));
    };
    let text = |s: &str| OwnedValue::try_from(zbus::zvariant::Value::from(s.to_string()));
    Ok(HashMap::from([
        ("key-mgmt", text(key_mgmt)?),
        ("psk", text(password)?),
    ]))
}

/// Ask every Wi-Fi device for a fresh scan (NetworkManager rate-limits
/// these itself); whether there was one to ask.
async fn request_scan(daemon: &Daemon, nm: &Nm) -> bool {
    for dev in nm.wifi_devices.keys() {
        let opts: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = Default::default();
        let r = dbus::timed_for(
            dbus::READ_TIMEOUT,
            daemon.conn().call_method(
                Some(NM),
                dev.as_str(),
                Some(WIRELESS),
                "RequestScan",
                &(opts,),
            ),
        )
        .await;
        if let Err(e) = r {
            log::debug!("network: no scan on {dev}: {e}");
        }
    }
    !nm.wifi_devices.is_empty()
}

/// Join the network named `ssid`: its strongest access point, with a
/// saved connection for that name if there is one (given `password`, it
/// is saved in it first), else a new one. The active connection
/// NetworkManager made for it.
async fn connect(
    daemon: &Daemon,
    nm: &Nm,
    name: &str,
    password: Option<&str>,
) -> zbus::Result<String> {
    use zbus::zvariant::ObjectPath;
    let conn = daemon.conn();
    // The devices' access point lists and the access points read now:
    // outside a scan neither is followed (the menu may just have closed).
    let mut best: Option<(String, String, Props, f64)> = None;
    let devices: Vec<String> = nm.wifi_devices.keys().cloned().collect();
    for dev in devices {
        let listed = match dbus::get(conn, NM, &dev, WIRELESS, "AccessPoints").await {
            Ok(v) => v.try_into().map(dbus::paths).unwrap_or_default(),
            Err(e) if dbus::is_timeout(&e) => return Err(e),
            Err(_) => continue,
        };
        for ap in listed {
            let p = match nm.aps.get(&ap) {
                Some(p) => p.clone(),
                None => match dbus::get_all(conn, NM, &ap, AP).await {
                    Ok(p) => p,
                    Err(e) if dbus::is_timeout(&e) => return Err(e),
                    Err(_) => continue,
                },
            };
            if ssid(&p).as_deref() != Some(name) {
                continue;
            }
            let s = strength(&p);
            if best.as_ref().is_none_or(|b| s > b.3) {
                best = Some((dev.clone(), ap, p, s));
            }
        }
    }
    let Some((dev, ap, ap_props, _)) = best else {
        return Err(zbus::Error::Failure(format!("`{name}` is not in range")));
    };
    let secret = match password {
        Some(pw) => Some(security(&ap_props, pw)?),
        None => None,
    };
    let dev = ObjectPath::try_from(dev.as_str())?;
    let ap = ObjectPath::try_from(ap.as_str())?;
    // A saved connection for the name?
    let saved: Vec<zbus::zvariant::OwnedObjectPath> = dbus::timed_for(
        dbus::READ_TIMEOUT,
        conn.call_method(
            Some(NM),
            SETTINGS,
            Some(SETTINGS_IFACE),
            "ListConnections",
            &(),
        ),
    )
    .await?
    .body()
    .deserialize()?;
    type Settings = HashMap<String, HashMap<String, OwnedValue>>;
    for path in saved {
        let reply = dbus::timed_for(
            dbus::READ_TIMEOUT,
            conn.call_method(
                Some(NM),
                path.as_str(),
                Some(CONNECTION_IFACE),
                "GetSettings",
                &(),
            ),
        )
        .await;
        let reply = match reply {
            Ok(r) => r,
            Err(e) if dbus::is_timeout(&e) => return Err(e),
            Err(_) => continue,
        };
        let Ok(mut settings) = reply.body().deserialize::<Settings>() else {
            continue;
        };
        let matches = settings
            .get("802-11-wireless")
            .is_some_and(|w| ssid_at(w, "ssid").as_deref() == Some(name));
        if matches {
            if let Some(secret) = &secret {
                // The new password, saved in it.
                let mut sec = HashMap::new();
                for (k, v) in secret {
                    sec.insert(k.to_string(), v.try_clone()?);
                }
                settings.insert("802-11-wireless-security".into(), sec);
                dbus::timed_for(
                    dbus::READ_TIMEOUT,
                    conn.call_method(
                        Some(NM),
                        path.as_str(),
                        Some(CONNECTION_IFACE),
                        "Update",
                        &(settings,),
                    ),
                )
                .await?;
            }
            let active: zbus::zvariant::OwnedObjectPath = dbus::timed_for(
                ACTIVATE_TIMEOUT,
                conn.call_method(
                    Some(NM),
                    ROOT,
                    Some(NM),
                    "ActivateConnection",
                    &(path.as_ref(), &dev, &ap),
                ),
            )
            .await?
            .body()
            .deserialize()?;
            return Ok(active.to_string());
        }
    }
    let mut new: HashMap<&str, HashMap<&str, OwnedValue>> = HashMap::new();
    if let Some(secret) = secret {
        new.insert("802-11-wireless-security", secret);
    }
    let (_, active): (
        zbus::zvariant::OwnedObjectPath,
        zbus::zvariant::OwnedObjectPath,
    ) = dbus::timed_for(
        ACTIVATE_TIMEOUT,
        conn.call_method(
            Some(NM),
            ROOT,
            Some(NM),
            "AddAndActivateConnection",
            &(new, &dev, &ap),
        ),
    )
    .await?
    .body()
    .deserialize()?;
    Ok(active.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{OwnedValue, Value};

    fn ap(flags: u32, wpa: u32, rsn: u32) -> Props {
        let n = |v: u32| OwnedValue::try_from(Value::from(v)).unwrap();
        Props::from([
            ("Flags".to_string(), n(flags)),
            ("WpaFlags".to_string(), n(wpa)),
            ("RsnFlags".to_string(), n(rsn)),
        ])
    }

    #[test]
    fn only_a_key_makes_a_network_secure() {
        assert!(!secure(&ap(0, 0, 0)), "open");
        // Open, offering WPS (NM_802_11_AP_FLAGS_WPS, _WPS_PBC, _WPS_PIN).
        assert!(!secure(&ap(0x2 | 0x4 | 0x8, 0, 0)), "open with WPS");
        assert!(secure(&ap(0x1, 0, 0)), "WEP (privacy)");
        assert!(secure(&ap(0x1 | 0x2, 0, 0x188)), "WPA2 with WPS");
        assert!(secure(&ap(0, 0x108, 0)), "WPA");
        assert!(secure(&ap(0, 0, 0x400)), "WPA3 (SAE)");
    }
}

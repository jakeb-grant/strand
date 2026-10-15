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
//! Joining (`ap.connect()`, `ap.connect_with(password)`) is asked in a
//! task of its own, so the radio, NetworkManager's signals and other
//! joins go on while NetworkManager takes its time; the active connection
//! it answers with is followed until it is up. If it is deactivated first
//! (`StateChanged` to DEACTIVATED, a missing or wrong password, a
//! timeout) or the call itself fails, `failed(ssid, error)` says why.
//! A personal network (WPA/WPA2/WPA3) or a WEP one joins with a password
//! (`secure`), its saved profile, or NetworkManager's secret agent; an
//! enterprise (802.1X) network joins only through a profile saved in
//! NetworkManager (`connect()`), and without one is refused with an
//! error saying so; an open one takes no password (decisions.md,
//! m6-services). A join of a network already being joined with the same
//! password is a repeat, dropped, whether it is still being asked or
//! already activating.
//!
//! Where NetworkManager is not running and iwd is, the network is iwd's
//! ([`iwd`], owner-testing gaps 15): NetworkManager is chosen whenever it
//! runs (it may run iwd as its own Wi-Fi backend), iwd otherwise; either
//! appearing or going chooses again.

use std::collections::{BTreeMap, HashMap};

use futures_lite::StreamExt;
use tokio::task::JoinSet;

use zbus::zvariant::OwnedValue;

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

pub mod address;
pub mod iwd;

/// How a backend's body ended.
#[derive(Debug, PartialEq)]
enum Next {
    /// The service stopped.
    Stop,
    /// The daemons changed: choose the backend again.
    Again,
}

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

/// How a Wi-Fi network is secured (`network.security`).
#[derive(crate::Data, Clone, Copy, Debug, PartialEq, Eq)]
#[data(name = "WifiSecurity")]
pub enum WifiSecurity {
    /// No key.
    Open,
    /// OWE: open, but encrypted.
    EnhancedOpen,
    Wep,
    Wpa,
    Wpa2,
    Wpa3,
    /// 802.1X.
    Enterprise,
}

/// `network`'s actions.
#[derive(Call, Debug)]
pub enum NetworkAction {
    /// `ap.connect()`.
    Connect { item: AccessPoint },
    /// `ap.connect_with(password)`.
    ConnectWith { item: AccessPoint, password: String },
    /// `network.disconnect()`.
    Disconnect,
    /// `network.scan()`.
    Scan,
}

/// See the module docs.
#[service(name = "network", action = NetworkAction, refused = refused)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Network {
    /// A network connection is up. Over iwd, which knows only Wi-Fi: the
    /// Wi-Fi network is joined.
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
    /// NetworkManager or iwd answers: false without either (or without a
    /// system bus), so a menu can hide what cannot work.
    pub available: bool,
    /// The Wi-Fi network being joined, by name: from the moment
    /// `ap.connect()` or `ap.connect_with()` is asked, or the daemon starts
    /// joining one on its own, until it is joined or fails; null otherwise
    /// (`if network.connecting == ap.ssid { … }`).
    pub connecting: Option<String>,
    /// The Wi-Fi network being left (`network.disconnect()`, or the daemon
    /// leaving it), until it is left; null otherwise.
    pub disconnecting: Option<String>,
    /// The joined Wi-Fi network's frequency in MHz (2412, 5180, 5955): below
    /// 3000 is the 2.4 GHz band, 5150 to 5895 the 5 GHz band, from 5925 the
    /// 6 GHz band. Null while no Wi-Fi network is joined, or when iwd does
    /// not say.
    pub frequency: Option<i64>,
    /// How the joined Wi-Fi network is secured; null while none is joined.
    pub security: Option<WifiSecurity>,
    /// The connection's IP address, as text without a prefix: its IPv4
    /// address, else its global IPv6 address. Null while not connected.
    /// Over NetworkManager this is the default-route connection's interface
    /// (wired too); over iwd it is the Wi-Fi interface. Read from the
    /// kernel, and followed only while a visible reader reads it.
    #[store(stream)]
    pub address: Option<String>,
    /// A Wi-Fi scan runs: one `scan()` asked, or the scan a network menu
    /// asks when it opens; over iwd, iwd's own scans too.
    pub scanning: bool,
    /// Joining a network failed (no password and no secret agent to ask
    /// for one, a wrong one, out of range, timed out, or the service not
    /// running): `on network.failed(ssid, error) { … }`. `ssid` is the
    /// network the action was about, "" for one that names none (`scan()`,
    /// or a `disconnect()` the service could not take).
    pub failed: Event<(String, String)>,
}

/// An action the service could not take (no system bus, its run backing
/// off): `failed(ssid, error)`, as one that fails is; "" for one that
/// names no network.
fn refused(action: &NetworkAction, error: &str) -> Option<NetworkEvent> {
    let ssid = match action {
        NetworkAction::Connect { item } | NetworkAction::ConnectWith { item, .. } => {
            item.ssid.clone()
        }
        NetworkAction::Disconnect | NetworkAction::Scan => String::new(),
    };
    Some(NetworkEvent::Failed((ssid, error.to_string())))
}

/// What the shell itself asked and the daemon has not said yet, laid over
/// what the daemon says ([`overlay`]) the same way on both backends: a
/// click shows at once.
#[derive(Debug, Default)]
struct Pending {
    /// The latest join asked and not answered yet: its network.
    join: Option<String>,
    /// A `disconnect()` in flight: the network it leaves.
    disconnect: Option<String>,
    /// A `scan()` (or a menu's scan) asked over NetworkManager, which has
    /// no scanning property: until when it counts as running.
    scan_until: Option<tokio::time::Instant>,
    /// The address follower's value ([`address::Follower`]).
    address: Option<String>,
}

/// `s`, as a backend's state, with the shell's own requests laid over it:
/// the daemon's join or leave first, else the one asked; a scan asked;
/// the address while connected.
fn overlay(p: &Pending, s: &mut Network) {
    if s.connecting.is_none() {
        s.connecting = p.join.clone();
    }
    if s.disconnecting.is_none() {
        s.disconnecting = p.disconnect.clone();
    }
    s.scanning |= p.scan_until.is_some();
    s.address = if s.connected { p.address.clone() } else { None };
}

/// How long a scan NetworkManager accepted counts as running when its
/// `LastScan` does not say it ended (NetworkManager before 1.12).
const SCAN_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);

/// A body's address follower ([`address::Follower`]): open only while a
/// visible reader reads `address` and the backend names an interface.
#[derive(Debug, Default)]
struct Addresses {
    follower: Option<address::Follower>,
    /// The socket failed: not opened again until `address` is watched
    /// anew.
    broken: bool,
}

impl Addresses {
    /// Open or close the follower for `iface` while `watched`; when it
    /// closes, the value goes. Whether the value changed.
    fn sync(&mut self, pending: &mut Pending, watched: bool, iface: Option<&str>) -> bool {
        if !watched {
            self.broken = false;
        }
        if watched && iface.is_some() && !self.broken {
            if self.follower.is_none() {
                match address::Follower::open() {
                    Ok(f) => self.follower = Some(f),
                    Err(e) => {
                        log::warn!("network: the address is not followed: {e}");
                        self.broken = true;
                    }
                }
            }
            return false;
        }
        self.follower = None;
        pending.address.take().is_some()
    }

    /// The next address of `iface` (pending while closed). A follower
    /// that fails is closed, and its value is null.
    async fn next(&mut self, iface: Option<&str>) -> Option<String> {
        let Some(f) = self.follower.as_mut() else {
            return std::future::pending().await;
        };
        match f.changed(iface).await {
            Ok(a) => a,
            Err(e) => {
                log::warn!("network: the address is not followed: {e}");
                self.follower = None;
                self.broken = true;
                None
            }
        }
    }
}

/// How long NetworkManager may take to accept an activation (its own
/// D-Bus timeout is 25 s); the outcome comes later, as `StateChanged`.
const ACTIVATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

/// `NMActiveConnectionState`: activating.
const ACTIVATING: u32 = 1;
/// `NMActiveConnectionState`: activated.
const ACTIVATED: u32 = 2;
/// `NMActiveConnectionState`: deactivating.
const DEACTIVATING: u32 = 3;
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
    /// The fingerprint of the password it was asked with ([`fingerprint`]).
    asked_with: u64,
}

/// A join of `ssid` with the password whose fingerprint is `asked_with`
/// is wanted while no task asks one (services-r2-2): whether it repeats
/// the join already activating (dropped: asked again, NetworkManager
/// would end that activation and start another, and its end would be
/// reported as a failure while the network came up). A join with another
/// password is asked, and the activation it replaces is followed no
/// more, so its end is not a failure.
fn repeats_joining(joining: &mut BTreeMap<String, Joining>, ssid: &str, asked_with: u64) -> bool {
    let Some(path) = joining
        .iter()
        .find(|(_, j)| j.ssid == ssid)
        .map(|(path, _)| path.clone())
    else {
        return false;
    };
    if joining
        .get(&path)
        .is_some_and(|j| j.asked_with == asked_with)
    {
        log::debug!("network: already joining {ssid}");
        return true;
    }
    joining.remove(&path);
    false
}

/// How the active connections no join follows yet ended (activated or
/// deactivated, with the reason), the last [`Ended::KEEP`]: a join's
/// answer can come after its connection's `StateChanged`.
#[derive(Debug, Default)]
struct Ended(std::collections::VecDeque<(String, u32, u32)>);

impl Ended {
    const KEEP: usize = 32;

    fn note(&mut self, path: String, state: u32, reason: u32) {
        if state != ACTIVATED && state != DEACTIVATED {
            return;
        }
        self.0.retain(|(p, ..)| *p != path);
        if self.0.len() == Self::KEEP {
            self.0.pop_front();
        }
        self.0.push_back((path, state, reason));
    }

    /// How `path` ended, if it did.
    fn take(&mut self, path: &str) -> Option<(u32, u32)> {
        let i = self.0.iter().position(|(p, ..)| p == path)?;
        self.0.remove(i).map(|(_, state, reason)| (state, reason))
    }

    fn clear(&mut self) {
        self.0.clear();
    }
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

/// It needs a password, which `connect_with` takes: a personal
/// (WPA/WPA2/WPA3) or WEP network ([`kind`]). An open or Enhanced Open
/// (OWE) one has none to give, and an enterprise (802.1X) one a login
/// its saved profile holds (services-r2-1). `Flags`' WPS bits (0x2, 0x4,
/// 0x8) say nothing about a key: an open network may offer WPS.
fn secure(p: &Props) -> bool {
    matches!(kind(p), Kind::Personal(_) | Kind::Wep)
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
    /// Every device's interface name (`IpInterface`, else `Interface`).
    interfaces: BTreeMap<String, String>,
    /// A Wi-Fi device's `LastScan` changed since this was last taken: a
    /// scan ended.
    scanned: bool,
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

    /// The Wi-Fi active connections in `state` (an
    /// `NMActiveConnectionState`), the default route's first.
    fn wifi_in(&self, state: u32) -> Vec<&Props> {
        let mut wifi: Vec<&Props> = self
            .active
            .values()
            .filter(|c| dbus::text(c, "Type").as_deref() == Some("802-11-wireless"))
            .filter(|c| dbus::number(c, "State").map(|n| n as u32) == Some(state))
            .collect();
        wifi.sort_by_key(|c| !dbus::boolean(c, "Default").unwrap_or(false));
        wifi
    }

    /// The access point the first Wi-Fi active connection in `state` uses.
    fn ap_in(&self, state: u32) -> Option<&Props> {
        self.wifi_in(state)
            .first()
            .and_then(|c| object_path(c, "SpecificObject"))
            .and_then(|p| self.aps.get(&p))
    }

    /// The interface whose address is the connection's: the device of the
    /// default route's active connection (IPv4, else IPv6), else of the
    /// joined Wi-Fi network.
    fn interface(&self) -> Option<String> {
        let device_of = |c: &Props| {
            object_paths(c, "Devices")
                .into_iter()
                .next()
                .and_then(|d| self.interfaces.get(&d).cloned())
        };
        let default = |key: &str| {
            self.active
                .values()
                .find(|c| dbus::boolean(c, key).unwrap_or(false))
                .and_then(device_of)
        };
        default("Default")
            .or_else(|| default("Default6"))
            .or_else(|| self.wifi_in(ACTIVATED).first().and_then(|c| device_of(c)))
    }

    /// The Wi-Fi device a `disconnect()` asks, and the network it leaves:
    /// the device of the joined Wi-Fi network, else of the one being
    /// joined.
    fn wifi_device(&self) -> Option<(String, Option<String>)> {
        [ACTIVATED, ACTIVATING].into_iter().find_map(|state| {
            let c = *self.wifi_in(state).first()?;
            let dev = object_paths(c, "Devices").into_iter().next()?;
            let name = object_path(c, "SpecificObject")
                .and_then(|p| self.aps.get(&p))
                .and_then(ssid);
            Some((dev, name))
        })
    }

    /// The state these objects make.
    pub fn state(&self, scanning: bool) -> Network {
        let state = dbus::number(&self.manager, "State").unwrap_or(0.0);
        // Only an activated connection is joined (an activating one is
        // `connecting`, a deactivating one `disconnecting`), in the list
        // as in `ssid`; the default route's wins.
        let joined_aps: Vec<String> = self
            .wifi_in(ACTIVATED)
            .into_iter()
            .filter_map(|c| object_path(c, "SpecificObject"))
            .collect();
        let joined = self.ap_in(ACTIVATED);
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
                    active: joined_aps.contains(path),
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
            // Its manager object answered the last read.
            available: !self.manager.is_empty(),
            connecting: self.ap_in(ACTIVATING).and_then(ssid),
            disconnecting: self.ap_in(DEACTIVATING).and_then(ssid),
            frequency: joined
                .and_then(|p| dbus::number(p, "Frequency"))
                .map(|f| f as i64),
            security: joined.map(security_of),
            address: None,
            scanning: false,
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
            if let Some(name) = dbus::text(&dev, "IpInterface")
                .filter(|n| !n.is_empty())
                .or_else(|| dbus::text(&dev, "Interface"))
            {
                self.interfaces.insert(path.clone(), name);
            }
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
            WIRELESS => {
                // A scan ended (NetworkManager 1.12 and later).
                if c.changed.contains_key("LastScan") {
                    self.scanned = true;
                }
                if !c.changed.contains_key("AccessPoints") || !scanning {
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
        loop {
            // NetworkManager when it runs, or when nothing else does (it
            // may be started for us); iwd when only iwd runs.
            let next = if !iwd::owned(&conn, NM).await && iwd::owned(&conn, iwd::IWD).await {
                iwd::run(&mut cx, &conn).await?
            } else {
                with_nm(&mut cx, &conn).await?
            };
            if next == Next::Stop {
                return Ok(());
            }
        }
    }
}

/// The `network` body over NetworkManager, until it goes while iwd runs,
/// or iwd appears while it is not running ([`Next::Again`]), or the
/// service stops.
async fn with_nm(cx: &mut Cx<Network>, conn: &zbus::Connection) -> Result<Next, ServiceError> {
    let dbus_proxy = zbus::fdo::DBusProxy::new(conn).await?;
    let mut iwd_owners = dbus_proxy
        .receive_name_owner_changed_with_args(&[(0, iwd::IWD)])
        .await?;
    let mut daemon = Daemon::new(conn, NM).await?;
    // iwd may have come up between the choice and the subscription to its
    // name (a boot race, review minor 1): choose again rather than follow
    // an absent NetworkManager until a daemon changes.
    if daemon.owner().is_none() && iwd::owned(conn, iwd::IWD).await {
        return Ok(Next::Again);
    }
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
    // Joins asked for and not answered yet (each a task of its own,
    // one per network at a time): the active connection each made, or
    // why not.
    let mut asking: JoinSet<zbus::Result<String>> = JoinSet::new();
    let mut asked = Asked::default();
    // How active connections no join followed yet ended, while joins
    // are asked: an answer naming one settles at once.
    let mut ended = Ended::default();
    // `disconnect()`s in flight (and the deactivations of joins they
    // stopped): the one whose answer ends `pending.disconnect` is
    // `leave`.
    let mut leaving: JoinSet<Result<(), String>> = JoinSet::new();
    let mut leave: Option<(tokio::task::Id, String)> = None;
    // The joins `leave` stopped, until it answers.
    let mut stopped = Stopped::default();
    let mut pending = Pending::default();
    let mut addresses = Addresses::default();
    // The access points are listed (a reader reads them).
    let mut listing = cx.watched("access_points");
    let mut used = None;
    // A scan is asked for when listing starts and when a new
    // NetworkManager appears, not on every re-read.
    let mut want_scan = listing;
    let publish = |cx: &mut Cx<Network>, nm: &Nm, pending: &Pending, listing: bool| {
        cx.update(|s| {
            *s = nm.state(listing);
            overlay(pending, s);
        })
    };
    loop {
        if listing {
            sync_subscriptions(&mut daemon, &nm, listing, false, &mut used).await;
        }
        if let Err(e) = nm.read(&daemon, listing).await {
            return Err(ServiceError(format!("NetworkManager did not answer: {e}")));
        }
        if !settle_joins(cx, &nm, &mut joining) {
            return Ok(Next::Stop);
        }
        // Asked once there is a Wi-Fi device to ask (a NetworkManager
        // that just appeared may not list its devices yet).
        if listing && want_scan {
            match request_scan(&daemon, &nm).await {
                Scan::NoDevice => {}
                outcome => {
                    want_scan = false;
                    if outcome == Scan::Accepted {
                        pending.scan_until = Some(tokio::time::Instant::now() + SCAN_LIMIT);
                    }
                }
            }
        }
        let scan_running = pending.scan_until.is_some();
        sync_subscriptions(&mut daemon, &nm, listing, scan_running, &mut used).await;
        if !publish(cx, &nm, &pending, listing) {
            return Ok(Next::Stop);
        }
        cx.ready();
        'follow: loop {
            let iface = nm.interface();
            if addresses.sync(&mut pending, cx.watched("address"), iface.as_deref())
                && !publish(cx, &nm, &pending, listing)
            {
                return Ok(Next::Stop);
            }
            let scan_until = pending.scan_until;
            tokio::select! {
                ev = daemon.next() => match ev {
                    None => return Err(ServiceError("the system bus connection ended".into())),
                    Some(DaemonEvent::Owner) => {
                        want_scan = listing;
                        ended.clear();
                        // Its activations went with it, and the joins
                        // still asking stop here: their next call would
                        // reach the new NetworkManager, whose answer
                        // this would not follow.
                        asking.shutdown().await;
                        leaving.shutdown().await;
                        leave = None;
                        stopped = Stopped::default();
                        pending = Pending {
                            address: pending.address.take(),
                            ..Pending::default()
                        };
                        let stopped = asked.clear().into_iter().chain(
                            std::mem::take(&mut joining).into_values().map(|j| j.ssid),
                        );
                        for ssid in stopped {
                            if !cx.emit(NetworkEvent::Failed((ssid, "NetworkManager stopped".into()))) {
                                return Ok(Next::Stop);
                            }
                        }
                        if daemon.owner().is_none() && iwd::owned(conn, iwd::IWD).await {
                            return Ok(Next::Again);
                        }
                        break 'follow;
                    }
                    Some(DaemonEvent::Signal(m)) => {
                        if let Some((path, state, reason)) = active_state(&m) {
                            if let Some(j) = joining.get(&path) {
                                let ssid = j.ssid.clone();
                                if state == ACTIVATED {
                                    joining.remove(&path);
                                } else if state == DEACTIVATED {
                                    joining.remove(&path);
                                    log::info!("network: joining {ssid} failed: reason {reason}");
                                    if !cx.emit(NetworkEvent::Failed((ssid, reason_text(reason)))) {
                                        return Ok(Next::Stop);
                                    }
                                }
                            } else if stopped.state_changed(&path, state, reason) {
                                // A join the disconnect in flight stopped:
                                // settled when it answers.
                            } else if !asking.is_empty() {
                                // Perhaps a join whose answer is still
                                // on its way.
                                ended.note(path, state, reason);
                            }
                        }
                        let changed = nm.signal(&daemon, &m, listing).await;
                        // A scan ended: its flag goes, and the devices'
                        // signals with it unless the list is open.
                        let scanned = std::mem::take(&mut nm.scanned) && pending.scan_until.take().is_some();
                        if scanned {
                            sync_subscriptions(&mut daemon, &nm, listing, false, &mut used).await;
                        }
                        match changed {
                            None => break 'follow,
                            Some(changed) => {
                                if (changed || scanned) && !publish(cx, &nm, &pending, listing) {
                                    return Ok(Next::Stop);
                                }
                            }
                        }
                    }
                },
                Some(o) = iwd_owners.next() => {
                    if daemon.owner().is_none() && o.args().ok().is_some_and(|a| a.new_owner().is_some()) {
                        return Ok(Next::Again);
                    }
                }
                () = until(scan_until) => {
                    // NetworkManager did not say the scan ended.
                    pending.scan_until = None;
                    sync_subscriptions(&mut daemon, &nm, listing, false, &mut used).await;
                    if !publish(cx, &nm, &pending, listing) {
                        return Ok(Next::Stop);
                    }
                }
                a = addresses.next(iface.as_deref()) => {
                    pending.address = a;
                    if !publish(cx, &nm, &pending, listing) {
                        return Ok(Next::Stop);
                    }
                }
                Some(done) = leaving.join_next_with_id(), if !leaving.is_empty() => {
                    let (id, r) = match done {
                        Ok((id, r)) => (id, r),
                        Err(e) => (e.id(), Err("the disconnect stopped".to_string())),
                    };
                    let Some((_, ssid)) = leave.take_if(|(l, _)| *l == id) else {
                        // A stopped join's deactivation.
                        if let Err(e) = r {
                            log::debug!("network: a stopped join stays: {e}");
                        }
                        continue;
                    };
                    pending.disconnect = None;
                    match r {
                        Ok(()) => {
                            // Left: the joins it stopped are dropped, and
                            // an activation NetworkManager answered
                            // meanwhile is ended.
                            asked.left();
                            for active in stopped.left() {
                                let conn = daemon.conn().clone();
                                leaving.spawn(async move { deactivate(&conn, &active).await });
                            }
                        }
                        Err(error) => {
                            log::info!("network: not leaving {ssid}: {error}");
                            if !cx.emit(NetworkEvent::Failed((ssid, error))) {
                                return Ok(Next::Stop);
                            }
                            // Not left: the joins it stopped go on, and are
                            // followed and reported again.
                            if let Some(join) = asked.resume().pop() {
                                pending.join = Some(join);
                            }
                            for f in stopped.resume(&mut joining) {
                                if !cx.emit(NetworkEvent::Failed(f)) {
                                    return Ok(Next::Stop);
                                }
                            }
                            if !settle_joins(cx, &nm, &mut joining) {
                                return Ok(Next::Stop);
                            }
                        }
                    }
                    if !publish(cx, &nm, &pending, listing) {
                        return Ok(Next::Stop);
                    }
                }
                Some(done) = asking.join_next_with_id(), if !asking.is_empty() => {
                    let (id, answer) = match done {
                        Ok((id, answer)) => (id, answer),
                        Err(e) => (e.id(), Err(zbus::Error::Failure("the join stopped".into()))),
                    };
                    match asked.stopped(id) {
                        None => {}
                        // A join a `disconnect()` stopped that has left:
                        // not followed nor reported, and an activation it
                        // made is ended.
                        Some(Stop::Cancelled) => {
                            if let Ok(active) = answer {
                                let conn = daemon.conn().clone();
                                leaving.spawn(async move { deactivate(&conn, &active).await });
                            }
                            if asking.is_empty() {
                                ended.clear();
                            }
                            continue;
                        }
                        // One the disconnect in flight stopped: kept
                        // until it answers.
                        Some(Stop::InFlight(ssid, asked_with)) => {
                            match answer {
                                Ok(active) => {
                                    let ended_with = match ended.take(&active) {
                                        Some((DEACTIVATED, reason)) => Some(reason),
                                        _ => None,
                                    };
                                    let seen = nm.active.contains_key(&active);
                                    stopped.joins.insert(active, StoppedJoin {
                                        join: Joining { ssid, seen, asked_with },
                                        late: true,
                                        ended: ended_with,
                                    });
                                }
                                Err(e) => stopped.failures.push((ssid, error_text(e))),
                            }
                            if asking.is_empty() {
                                ended.clear();
                            }
                            continue;
                        }
                    }
                    let Some((ssid, asked_with, next)) = asked.answered(id) else {
                        continue;
                    };
                    if pending.join.as_deref() == Some(ssid.as_str()) {
                        pending.join = None;
                    }
                    let failure = match answer {
                        Ok(active) => match ended.take(&active) {
                            Some((ACTIVATED, _)) => None,
                            Some((_, reason)) => {
                                log::info!("network: joining {ssid} failed: reason {reason}");
                                Some(reason_text(reason))
                            }
                            None => {
                                // Followed until it is up (or not).
                                let seen = nm.active.contains_key(&active);
                                joining.insert(active, Joining { ssid: ssid.clone(), seen, asked_with });
                                if !settle_joins(cx, &nm, &mut joining) {
                                    return Ok(Next::Stop);
                                }
                                None
                            }
                        },
                        Err(e) => {
                            log::warn!("network: not joining {ssid}: {e}");
                            Some(error_text(e))
                        }
                    };
                    if asking.is_empty() {
                        ended.clear();
                    }
                    if let Some(error) = failure
                        && !cx.emit(NetworkEvent::Failed((ssid.clone(), error)))
                    {
                        return Ok(Next::Stop);
                    }
                    // A join of this network asked meanwhile (another
                    // password): asked now, so it finds the profile
                    // this one may have saved, and the activation it
                    // replaces is followed no more.
                    if let Some(password) = next
                        && !repeats_joining(&mut joining, &ssid, fingerprint(password.as_deref()))
                    {
                        let id = ask(&mut asking, daemon.conn(), &nm, ssid.clone(), password.clone());
                        asked.started(ssid.clone(), id, password.as_deref());
                        pending.join = Some(ssid);
                    }
                    if !publish(cx, &nm, &pending, listing) {
                        return Ok(Next::Stop);
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(Next::Stop),
                    Some(Msg::Watch { field: "access_points", on }) => {
                        listing = on;
                        want_scan = on;
                        break 'follow;
                    }
                    Some(Msg::Write(w)) if w.field == "wifi" => {
                        let on: bool = w.value().unwrap_or(nm.state(listing).wifi);
                        let set = dbus::set(conn, NM, ROOT, NM, "WirelessEnabled", on.into()).await;
                        let now = match set {
                            Ok(()) => {
                                // As NetworkManager's signal will say (a
                                // scan asked before it comes is refused).
                                nm.manager.insert("WirelessEnabled".into(), on.into());
                                on
                            }
                            Err(e) => {
                                log::warn!("network: Wi-Fi not switched: {e}");
                                // The radio as last read.
                                nm.state(listing).wifi
                            }
                        };
                        if !cx.report(&w, |s| s.wifi = now) {
                            return Ok(Next::Stop);
                        }
                    }
                    Some(Msg::Action(NetworkAction::Scan)) => {
                        let failure = if !nm.state(listing).wifi {
                            Some("Wi-Fi is off".to_string())
                        } else {
                            match request_scan(&daemon, &nm).await {
                                Scan::NoDevice => Some("there is no Wi-Fi device".to_string()),
                                Scan::Accepted => {
                                    nm.scanned = false;
                                    pending.scan_until = Some(tokio::time::Instant::now() + SCAN_LIMIT);
                                    sync_subscriptions(&mut daemon, &nm, listing, true, &mut used).await;
                                    None
                                }
                                Scan::Busy => None,
                                Scan::Failed(error) => Some(error),
                            }
                        };
                        if let Some(error) = failure {
                            log::info!("network: no scan: {error}");
                            if !cx.emit(NetworkEvent::Failed((String::new(), error))) {
                                return Ok(Next::Stop);
                            }
                        }
                        if !publish(cx, &nm, &pending, listing) {
                            return Ok(Next::Stop);
                        }
                    }
                    Some(Msg::Action(NetworkAction::Disconnect)) => {
                        // The device of the network joined or being
                        // joined, else (a join still asked) the first
                        // Wi-Fi device.
                        let target = nm
                            .wifi_device()
                            .map(|(dev, name)| (dev, name.or_else(|| pending.join.clone())))
                            .or_else(|| {
                                let join = pending.join.clone()?;
                                let dev = nm.wifi_devices.keys().next()?.clone();
                                Some((dev, Some(join)))
                            });
                        let Some((dev, name)) = target else {
                            if !cx.emit(NetworkEvent::Failed((String::new(), "no Wi-Fi network is joined".into()))) {
                                return Ok(Next::Stop);
                            }
                            continue;
                        };
                        let name = name.unwrap_or_default();
                        // The joins it stops: neither followed nor
                        // reported, unless it fails.
                        stop_joins(&mut asked, &mut joining, &mut stopped);
                        pending.join = None;
                        let conn = daemon.conn().clone();
                        let id = leaving
                            .spawn(async move { leave_device(&conn, &dev).await })
                            .id();
                        leave = Some((id, name.clone()));
                        pending.disconnect = Some(name);
                        if !publish(cx, &nm, &pending, listing) {
                            return Ok(Next::Stop);
                        }
                    }
                    Some(Msg::Action(a)) => {
                        let (item, password) = match a {
                            NetworkAction::Connect { item } => (item, None),
                            NetworkAction::ConnectWith { item, password } => (item, Some(password)),
                            NetworkAction::Disconnect | NetworkAction::Scan => continue,
                        };
                        // Asked in a task (up to 25 s for the
                        // activation): NetworkManager's signals, the
                        // radio and other joins go on meanwhile. One
                        // network is asked one join at a time, and a
                        // repeat of one activating is dropped.
                        if asked.ask(&item.ssid, password.as_deref())
                            && !repeats_joining(&mut joining, &item.ssid, fingerprint(password.as_deref()))
                        {
                            let id = ask(&mut asking, daemon.conn(), &nm, item.ssid.clone(), password.clone());
                            asked.started(item.ssid.clone(), id, password.as_deref());
                            pending.join = Some(item.ssid);
                            if !publish(cx, &nm, &pending, listing) {
                                return Ok(Next::Stop);
                            }
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

/// Wait until `deadline`; never without one.
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// What a failed call says: the daemon's words when it gave some.
fn error_text(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(text), _) => text,
        zbus::Error::Failure(text) => text,
        e => e.to_string(),
    }
}

/// The joins a `disconnect()` stops: those still asked
/// ([`Asked::cancel`]) and those activating ([`Stopped::stop`]) are
/// neither followed nor reported while it is in flight; once it has left
/// they are dropped, and if it fails they are followed again.
fn stop_joins(asked: &mut Asked, joining: &mut BTreeMap<String, Joining>, stopped: &mut Stopped) {
    asked.cancel();
    stopped.stop(joining);
}

/// `Device.Disconnect` on Wi-Fi device `dev`: NetworkManager leaves its
/// network and joins nothing on its own until one is joined again.
async fn leave_device(conn: &zbus::Connection, dev: &str) -> Result<(), String> {
    dbus::timed_for(
        dbus::READ_TIMEOUT,
        conn.call_method(Some(NM), dev, Some(DEVICE), "Disconnect", &()),
    )
    .await
    .map(drop)
    .map_err(error_text)
}

/// Deactivate active connection `active` (a join `disconnect()` stopped,
/// which NetworkManager answered afterwards).
async fn deactivate(conn: &zbus::Connection, active: &str) -> Result<(), String> {
    let path = zbus::zvariant::ObjectPath::try_from(active).map_err(|e| e.to_string())?;
    dbus::timed_for(
        dbus::READ_TIMEOUT,
        conn.call_method(Some(NM), ROOT, Some(NM), "DeactivateConnection", &(path,)),
    )
    .await
    .map(drop)
    .map_err(error_text)
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
/// while listing; otherwise the signals of the access point in use (its
/// strength), one rule replaced when the connection moves, and the
/// devices' signals while a scan runs (its end, `LastScan`).
async fn sync_subscriptions(
    daemon: &mut Daemon,
    nm: &Nm,
    listing: bool,
    scan_running: bool,
    used: &mut Option<String>,
) {
    async fn devices(daemon: &mut Daemon) {
        if !daemon.subscribed("devices")
            && let Ok(r) = wireless_rule()
            && let Err(e) = daemon.subscribe("devices", r).await
        {
            log::debug!("network: no device signals: {e}");
        }
    }
    if listing {
        daemon.unsubscribe("used");
        *used = None;
        if !daemon.subscribed("aps")
            && let Ok(r) = dbus::namespace_rule(AP_NAMESPACE)
            && let Err(e) = daemon.subscribe("aps", r).await
        {
            log::debug!("network: no access point signals: {e}");
        }
        devices(daemon).await;
        return;
    }
    daemon.unsubscribe("aps");
    if scan_running {
        devices(daemon).await;
    } else {
        daemon.unsubscribe("devices");
    }
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

/// How an access point is joined: its key management.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// No key.
    Open,
    /// WPA/WPA2 personal (`wpa-psk`) or WPA3 personal (`sae`): a password.
    Personal(&'static str),
    /// WPA enterprise (802.1X): a login NetworkManager's saved profile
    /// holds (an EAP method, identity, certificates).
    Enterprise,
    /// WEP: `Flags` has PRIVACY and there is no WPA or RSN key management.
    /// A key (`wep-key0`).
    Wep,
}

/// How access point `p` is joined. A personal key wins over 802.1X when
/// both are offered (as NetworkManager's own choice for a password does).
fn kind(p: &Props) -> Kind {
    let n = |k| dbus::number(p, k).map_or(0, |n| n as u32);
    // NM_802_11_AP_SEC_KEY_MGMT_PSK (0x100), _802_1X (0x200), _SAE
    // (0x400), _OWE (0x800), _OWE_TM (0x1000), _EAP_SUITE_B_192 (0x2000).
    let mgmt = n("WpaFlags") | n("RsnFlags");
    if mgmt & 0x100 != 0 {
        Kind::Personal("wpa-psk")
    } else if mgmt & 0x400 != 0 {
        Kind::Personal("sae")
    } else if mgmt & (0x200 | 0x2000) != 0 {
        Kind::Enterprise
    } else if mgmt & (0x800 | 0x1000) == 0 && n("Flags") & 0x1 != 0 {
        // NM_802_11_AP_FLAGS_PRIVACY without WPA key management.
        Kind::Wep
    } else {
        // Open, or Enhanced Open (OWE): no key to give.
        Kind::Open
    }
}

/// How access point `p` is secured, from the flags [`kind`] reads too. A
/// WPA3 transition network (PSK and SAE) reads `wpa2`: the flags do not
/// say which one was used.
fn security_of(p: &Props) -> WifiSecurity {
    let n = |k| dbus::number(p, k).map_or(0, |n| n as u32);
    let (wpa, rsn) = (n("WpaFlags"), n("RsnFlags"));
    let mgmt = wpa | rsn;
    let (psk, sae) = (mgmt & 0x100 != 0, mgmt & 0x400 != 0);
    if mgmt & (0x800 | 0x1000) != 0 && !psk && !sae {
        WifiSecurity::EnhancedOpen
    } else if mgmt & (0x200 | 0x2000) != 0 && !psk && !sae {
        WifiSecurity::Enterprise
    } else if rsn & 0x400 != 0 && rsn & 0x100 == 0 {
        WifiSecurity::Wpa3
    } else if rsn & 0x100 != 0 {
        WifiSecurity::Wpa2
    } else if wpa & 0x100 != 0 {
        WifiSecurity::Wpa
    } else if sae {
        WifiSecurity::Wpa3
    } else if n("Flags") & 0x1 != 0 && mgmt & 0x3f00 == 0 {
        // PRIVACY without key management.
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
    }
}

/// The `802-11-wireless-security` settings for joining a network of
/// `kind` with `password`: a WPA/WPA2 or WPA3 personal `psk`, or a WEP
/// key (`wep-key0`), none if `kind` takes no password.
fn security(kind: Kind, password: &str) -> zbus::Result<Option<HashMap<&'static str, OwnedValue>>> {
    let text = |s: &str| OwnedValue::try_from(zbus::zvariant::Value::from(s.to_string()));
    Ok(match kind {
        Kind::Personal(key_mgmt) => Some(HashMap::from([
            ("key-mgmt", text(key_mgmt)?),
            ("psk", text(password)?),
        ])),
        Kind::Wep => Some(HashMap::from([
            ("key-mgmt", text("none")?),
            ("wep-key0", text(password)?),
            ("wep-key-type", OwnedValue::from(wep_key_type(password))),
        ])),
        Kind::Open | Kind::Enterprise => None,
    })
}

/// A saved profile's `802-11-wireless-security` section (`had`) with the
/// new password's `secret` ([`security`]) saved in it (services-r3-3):
/// the profile's other keys stay (a shared-key WEP `auth-alg`, its
/// `wep-tx-keyidx`, a WPA `proto` or `pmf`, the secret's flags), and only
/// `key-mgmt`, `psk` or the WEP key and its type are written. A WEP key
/// goes where the profile transmits with (`wep-key<wep-tx-keyidx>`). A
/// section of the other kind (a WEP profile now asked a WPA password, or
/// the other way) has nothing to keep and is replaced.
fn merged_security(
    had: Option<HashMap<String, OwnedValue>>,
    secret: &HashMap<&'static str, OwnedValue>,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    // WEP's `key-mgmt` is `none`; a personal network's `wpa-psk` or `sae`.
    let wep = |m: &HashMap<_, OwnedValue>| {
        m.get("key-mgmt")
            .and_then(|v| <&str>::try_from(v).ok())
            .is_some_and(|k| k == "none")
    };
    let secret_wep = secret.contains_key("wep-key0");
    let mut sec = had
        .filter(|h| h.contains_key("key-mgmt") && wep(h) == secret_wep)
        .unwrap_or_default();
    let tx = sec
        .get("wep-tx-keyidx")
        .and_then(|v| u32::try_from(v).ok())
        .filter(|i| *i <= 3)
        .unwrap_or(0);
    for (k, v) in secret {
        let k = if *k == "wep-key0" {
            format!("wep-key{tx}")
        } else {
            k.to_string()
        };
        sec.insert(k, v.try_clone()?);
    }
    Ok(sec)
}

/// NetworkManager's `wep-key-type` for a WEP `key`: 1 (the key itself,
/// 5 or 13 characters, or 10 or 26 hex digits) or 2 (a passphrase it
/// hashes into one).
fn wep_key_type(key: &str) -> u32 {
    let hex = key.chars().all(|c| c.is_ascii_hexdigit());
    match key.len() {
        5 | 13 if key.is_ascii() => 1,
        10 | 26 if hex => 1,
        _ => 2,
    }
}

/// Why `name` (an access point of `kind`) cannot be joined the way it was
/// asked, before anything is sent: `saved` it has a saved connection,
/// `password` one was given. An enterprise (802.1X) network joins only
/// through a profile saved in NetworkManager (made in the system's
/// network settings), which holds its login; a password is for a
/// personal or WEP network (`secure`), and without one NetworkManager's
/// secret agent is asked for it.
fn refusal(name: &str, kind: Kind, saved: bool, password: bool) -> Option<String> {
    match (kind, saved, password) {
        (Kind::Enterprise, true, true) => Some(format!(
            "`{name}` is an enterprise (802.1X) network: its saved profile holds the login, \
             so join it with `connect()`, not a password"
        )),
        (Kind::Enterprise, false, _) => Some(format!(
            "`{name}` is an enterprise (802.1X) network with no saved profile: \
             add it in the system's network settings first"
        )),
        (Kind::Open, _, true) => Some(format!("`{name}` is an open network: it takes no password")),
        _ => None,
    }
}

/// How a scan asked of NetworkManager went.
#[derive(Debug, PartialEq)]
enum Scan {
    /// There is no Wi-Fi device to ask.
    NoDevice,
    /// A device scans.
    Accepted,
    /// Refused as one runs or has just run, or while a network is being
    /// joined: no failure.
    Busy,
    /// Refused otherwise, saying why.
    Failed(String),
}

/// NetworkManager's refusal of a scan: one it will not ask now, or one
/// the device cannot make.
const NOT_ALLOWED: &str = "org.freedesktop.NetworkManager.Device.NotAllowed";

/// Why a refused `RequestScan` is a failure; `None` for a refusal that
/// only says "not now", which is none (connectivity-r1-2). NetworkManager
/// says `NotAllowed` for both kinds and tells them apart only in its words:
/// before 1.22 it refused a scan running ("while already scanning"), just
/// run ("immediately following previous scan") or not wanted while its
/// supplicant was busy ("at this time"), and a device activating ("while
/// unavailable or activating"; `activating`: a Wi-Fi network is being
/// joined); since, it refuses only a device that cannot scan ("while
/// unavailable": the radio switched off by its hardware key, the device
/// not ready), which is a failure, as the radio off is.
fn scan_failure(e: zbus::Error, activating: bool) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, text, _) if name.as_str() == NOT_ALLOWED => {
            let text = text.unwrap_or_default();
            let busy = ["already scanning", "immediately following", "at this time"]
                .iter()
                .any(|w| text.contains(w))
                || (activating && text.contains("activating"));
            match (busy, text.contains("unavailable")) {
                (true, _) => None,
                (false, true) => Some("the Wi-Fi device is unavailable".to_string()),
                (false, false) if text.is_empty() => Some("the scan was refused".to_string()),
                (false, false) => Some(text),
            }
        }
        e => Some(error_text(e)),
    }
}

/// Ask every Wi-Fi device for a fresh scan (NetworkManager rate-limits
/// these itself).
async fn request_scan(daemon: &Daemon, nm: &Nm) -> Scan {
    let mut outcome = Scan::NoDevice;
    let activating = !nm.wifi_in(ACTIVATING).is_empty();
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
        let this = match r {
            Ok(_) => Scan::Accepted,
            Err(e) => {
                log::debug!("network: no scan on {dev}: {e}");
                scan_failure(e, activating).map_or(Scan::Busy, Scan::Failed)
            }
        };
        // One device scanning is a scan; one busy is no failure.
        outcome = match (outcome, this) {
            (Scan::Accepted, _) | (_, Scan::Accepted) => Scan::Accepted,
            (Scan::Busy, _) | (_, Scan::Busy) => Scan::Busy,
            (_, this) => this,
        };
    }
    outcome
}

/// What a join needs from the last read, taken along by its task.
#[derive(Debug, Clone)]
struct Snapshot {
    /// The Wi-Fi devices.
    devices: Vec<String>,
    /// The access points read.
    aps: BTreeMap<String, Props>,
}

impl Snapshot {
    fn of(nm: &Nm) -> Snapshot {
        Snapshot {
            devices: nm.wifi_devices.keys().cloned().collect(),
            aps: nm.aps.clone(),
        }
    }
}

/// Ask NetworkManager to join `ssid` in a task of `asking` with what the
/// last read knew; the task's id.
fn ask(
    asking: &mut JoinSet<zbus::Result<String>>,
    conn: &zbus::Connection,
    nm: &Nm,
    ssid: String,
    password: Option<String>,
) -> tokio::task::Id {
    let conn = conn.clone();
    let known = Snapshot::of(nm);
    asking
        .spawn(async move { connect(&conn, &known, &ssid, password.as_deref()).await })
        .id()
}

/// The joins being asked, one per network at a time (decisions.md,
/// m6-services). Each join is a task, so without this two joins of one
/// unsaved network (a double click) would both find no saved profile and
/// both add one: NetworkManager would keep two, and the second activation
/// would end the first. A join asked again with the same password while
/// it is asked is a repeat and dropped; with another password it waits
/// for the answer (the last one asked), then is asked, and finds the
/// profile the first saved. So there is at most one task per network
/// name, and one waiting join.
#[derive(Debug, Default)]
struct Asked {
    /// By network name: the task asking and its password's fingerprint.
    asking: BTreeMap<String, (tokio::task::Id, u64)>,
    /// By network name: the join to ask when its answer comes.
    waiting: BTreeMap<String, Option<String>>,
    /// Tasks asking joins the `disconnect()` in flight stopped, with their
    /// network and password's fingerprint: cancelled once it has left,
    /// asked again if it fails ([`Asked::left`], [`Asked::resume`]).
    stopping: HashMap<tokio::task::Id, (String, u64)>,
    /// Tasks asking joins that were cancelled.
    cancelled: std::collections::HashSet<tokio::task::Id>,
}

/// A join a `disconnect()` stopped, as its task answers
/// ([`Asked::stopped`]).
#[derive(Debug, PartialEq)]
enum Stop {
    /// Stopped by the disconnect still in flight: its network and its
    /// password's fingerprint.
    InFlight(String, u64),
    /// Cancelled: the disconnect left the network.
    Cancelled,
}

impl Asked {
    /// A join of `ssid` with `password` is wanted: whether to ask it now
    /// (then [`Asked::started`] records it). If not, it is a repeat of
    /// the join being asked (dropped, and any join waiting with it) or
    /// waits for its answer.
    fn ask(&mut self, ssid: &str, password: Option<&str>) -> bool {
        let Some((_, asked_with)) = self.asking.get(ssid) else {
            return true;
        };
        if *asked_with == fingerprint(password) {
            log::debug!("network: already joining {ssid}");
            self.waiting.remove(ssid);
        } else {
            self.waiting
                .insert(ssid.to_string(), password.map(str::to_string));
        }
        false
    }

    /// The join of `ssid` with `password` is asked by task `id`.
    fn started(&mut self, ssid: String, id: tokio::task::Id, password: Option<&str>) {
        self.asking.insert(ssid, (id, fingerprint(password)));
    }

    /// Task `id` answered: its network, its password's fingerprint, and
    /// the join of it to ask now, if one waited.
    #[allow(clippy::type_complexity)]
    fn answered(&mut self, id: tokio::task::Id) -> Option<(String, u64, Option<Option<String>>)> {
        let ssid = self
            .asking
            .iter()
            .find(|(_, (task, _))| *task == id)
            .map(|(ssid, _)| ssid.clone())?;
        let (_, asked_with) = self.asking.remove(&ssid)?;
        let next = self.waiting.remove(&ssid);
        Some((ssid, asked_with, next))
    }

    /// Everything forgotten (its tasks are stopped by the caller): the
    /// networks that were being asked.
    fn clear(&mut self) -> Vec<String> {
        self.waiting.clear();
        self.cancelled.clear();
        self.stopping.clear();
        std::mem::take(&mut self.asking).into_keys().collect()
    }

    /// Every join being asked is stopped by a `disconnect()`, and none
    /// waits: their answers are not followed while it is in flight
    /// ([`Asked::stopped`]).
    fn cancel(&mut self) {
        self.waiting.clear();
        let asking = std::mem::take(&mut self.asking);
        self.stopping.extend(
            asking
                .into_iter()
                .map(|(ssid, (task, fp))| (task, (ssid, fp))),
        );
    }

    /// The disconnect left the network: the joins it stopped are
    /// cancelled for good.
    fn left(&mut self) {
        self.cancelled
            .extend(std::mem::take(&mut self.stopping).into_keys());
    }

    /// The disconnect failed: the joins it stopped are asked again, so
    /// their answers are followed (one of a network asked anew meanwhile
    /// stays cancelled). Their networks.
    fn resume(&mut self) -> Vec<String> {
        let mut resumed = Vec::new();
        for (task, (ssid, fp)) in std::mem::take(&mut self.stopping) {
            if self.asking.contains_key(&ssid) {
                self.cancelled.insert(task);
            } else {
                self.asking.insert(ssid.clone(), (task, fp));
                resumed.push(ssid);
            }
        }
        resumed
    }

    /// Task `id` asked a join a disconnect stopped (said once).
    fn stopped(&mut self, id: tokio::task::Id) -> Option<Stop> {
        if let Some((ssid, fp)) = self.stopping.remove(&id) {
            return Some(Stop::InFlight(ssid, fp));
        }
        self.cancelled.remove(&id).then_some(Stop::Cancelled)
    }
}

/// A join a `disconnect()` in flight stopped: neither followed nor
/// reported until the disconnect answers (connectivity-r1-6).
#[derive(Debug)]
struct StoppedJoin {
    join: Joining,
    /// NetworkManager answered it after the disconnect was asked: its
    /// activation may have started after the device was left, so it is
    /// deactivated once the disconnect has left.
    late: bool,
    /// It was deactivated meanwhile, with the reason.
    ended: Option<u32>,
}

/// The joins the `disconnect()` in flight stopped, by active connection,
/// and the failures of joins it stopped that NetworkManager answered
/// meanwhile: dropped once it has left the network, followed again (and
/// the failures reported) if it fails, so a join that goes on is never
/// lost.
#[derive(Debug, Default)]
struct Stopped {
    joins: BTreeMap<String, StoppedJoin>,
    failures: Vec<(String, String)>,
}

impl Stopped {
    /// The activating joins `joining` follows are stopped.
    fn stop(&mut self, joining: &mut BTreeMap<String, Joining>) {
        for (path, join) in std::mem::take(joining) {
            self.joins.insert(
                path,
                StoppedJoin {
                    join,
                    late: false,
                    ended: None,
                },
            );
        }
    }

    /// A state change of active connection `path`: whether it is a
    /// stopped join's (then its end is kept, not reported).
    fn state_changed(&mut self, path: &str, state: u32, reason: u32) -> bool {
        let Some(j) = self.joins.get_mut(path) else {
            return false;
        };
        match state {
            DEACTIVATED => j.ended = Some(reason),
            ACTIVATED => j.ended = None,
            _ => {}
        }
        true
    }

    /// The disconnect left the network: the late joins' activations to
    /// end.
    fn left(&mut self) -> Vec<String> {
        self.failures.clear();
        std::mem::take(&mut self.joins)
            .into_iter()
            .filter(|(_, j)| j.late && j.ended.is_none())
            .map(|(path, _)| path)
            .collect()
    }

    /// The disconnect failed: the joins still activating are followed
    /// again (into `joining`); the failures (those deactivated meanwhile,
    /// and those answered with an error) to report.
    fn resume(&mut self, joining: &mut BTreeMap<String, Joining>) -> Vec<(String, String)> {
        let mut failures = std::mem::take(&mut self.failures);
        for (path, j) in std::mem::take(&mut self.joins) {
            match j.ended {
                Some(reason) => failures.push((j.join.ssid, reason_text(reason))),
                None => {
                    joining.insert(path, j.join);
                }
            }
        }
        failures
    }
}

/// A password's fingerprint, to tell a repeated join from a new one
/// without keeping the password.
fn fingerprint(password: Option<&str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    password.hash(&mut h);
    h.finish()
}

/// Join the network named `ssid`: its strongest access point, with a
/// saved connection for that name if there is one (given `password`, it
/// is saved in it first), else a new one. The active connection
/// NetworkManager made for it. Runs as a task of its own (every call
/// bounded: [`dbus::READ_TIMEOUT`] each, [`ACTIVATE_TIMEOUT`] for the
/// activation), so the service goes on following NetworkManager and
/// answering the shell meanwhile.
async fn connect(
    conn: &zbus::Connection,
    known: &Snapshot,
    name: &str,
    password: Option<&str>,
) -> zbus::Result<String> {
    use zbus::zvariant::ObjectPath;
    // The devices' access point lists and the access points read now:
    // outside a scan neither is followed (the menu may just have closed).
    let mut best: Option<(String, String, Props, f64)> = None;
    for dev in &known.devices {
        let listed = match dbus::get(conn, NM, dev, WIRELESS, "AccessPoints").await {
            Ok(v) => v.try_into().map(dbus::paths).unwrap_or_default(),
            Err(e) if dbus::is_timeout(&e) => return Err(e),
            Err(_) => continue,
        };
        for ap in listed {
            let p = match known.aps.get(&ap) {
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
    let kind = kind(&ap_props);
    let dev = ObjectPath::try_from(dev.as_str())?;
    let ap = ObjectPath::try_from(ap.as_str())?;
    let saved = saved_connection(conn, name).await?;
    if let Some(why) = refusal(name, kind, saved.is_some(), password.is_some()) {
        return Err(zbus::Error::Failure(why));
    }
    let secret = match password {
        Some(pw) => security(kind, pw)?,
        None => None,
    };
    if let Some((path, mut settings)) = saved {
        if let Some(secret) = &secret {
            // The new password, saved in it: merged into the section the
            // profile has, so what else it says (a shared-key WEP
            // `auth-alg`, its transmit key, `proto`, `pmf`) stays.
            let had = settings.remove("802-11-wireless-security");
            let sec = merged_security(had, secret)?;
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

/// A connection's settings, as `GetSettings` answers them.
type Settings = HashMap<String, HashMap<String, OwnedValue>>;

/// The saved connection for the Wi-Fi network `name`, with its settings
/// (no secrets), if there is one.
async fn saved_connection(
    conn: &zbus::Connection,
    name: &str,
) -> zbus::Result<Option<(zbus::zvariant::OwnedObjectPath, Settings)>> {
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
        let Ok(settings) = reply.body().deserialize::<Settings>() else {
            continue;
        };
        if settings
            .get("802-11-wireless")
            .is_some_and(|w| ssid_at(w, "ssid").as_deref() == Some(name))
        {
            return Ok(Some((path, settings)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{OwnedValue, Value};

    /// (m6-services, services-r3-3) A new password is merged into a saved
    /// profile's security section: a shared-key WEP profile keeps its
    /// `auth-alg` and transmit key (the key written at that index), a
    /// WPA profile its `proto` and `pmf`; a section of the other kind is
    /// replaced.
    #[test]
    fn a_new_password_keeps_the_saved_profile_s_security_settings() {
        let text = |s: &str| OwnedValue::try_from(Value::from(s.to_string())).unwrap();
        let wep_profile = HashMap::from([
            ("key-mgmt".to_string(), text("none")),
            ("auth-alg".to_string(), text("shared")),
            ("wep-tx-keyidx".to_string(), OwnedValue::from(1u32)),
            ("wep-key-type".to_string(), OwnedValue::from(1u32)),
        ]);
        let sec = merged_security(
            Some(wep_profile.clone()),
            &security(Kind::Wep, "abcde").unwrap().unwrap(),
        )
        .unwrap();
        assert_eq!(sec["auth-alg"], text("shared"));
        assert_eq!(sec["wep-tx-keyidx"], OwnedValue::from(1u32));
        assert_eq!(sec["wep-key1"], text("abcde"));
        assert!(!sec.contains_key("wep-key0"));
        assert_eq!(sec["key-mgmt"], text("none"));

        let wpa_profile = HashMap::from([
            ("key-mgmt".to_string(), text("wpa-psk")),
            (
                "proto".to_string(),
                OwnedValue::try_from(Value::from(vec!["rsn"])).unwrap(),
            ),
            ("pmf".to_string(), OwnedValue::from(3i32)),
        ]);
        let personal = security(Kind::Personal("wpa-psk"), "hunter22")
            .unwrap()
            .unwrap();
        let sec = merged_security(Some(wpa_profile.clone()), &personal).unwrap();
        assert_eq!(sec["pmf"], OwnedValue::from(3i32));
        assert!(sec.contains_key("proto"));
        assert_eq!(sec["psk"], text("hunter22"));

        // The other kind: nothing of the old section is kept.
        let sec = merged_security(Some(wep_profile), &personal).unwrap();
        assert!(!sec.contains_key("auth-alg"));
        assert_eq!(sec.len(), 2);
        let sec = merged_security(None, &personal).unwrap();
        assert_eq!(sec.len(), 2);
    }

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

    /// (m6-services, services-r2-1) `secure` is what the schema says: the
    /// network needs a password, so a shell's password field
    /// (`connect_with`) is offered exactly where it joins. Enhanced Open
    /// and 802.1X have RSN flags but take no password.
    #[test]
    fn secure_is_exactly_the_networks_a_password_joins() {
        for (flags, wpa, rsn) in [
            (0, 0, 0),
            (0x1, 0, 0x800),
            (0x1, 0, 0x1000),
            (0x1, 0, 0x288),
            (0x1, 0x288, 0),
            (0x1, 0, 0x2000),
            (0x1, 0, 0x188),
            (0x1, 0, 0x400),
            (0x1, 0, 0x300),
            (0x1, 0, 0),
        ] {
            let p = ap(flags, wpa, rsn);
            let takes = refusal("n", kind(&p), false, true).is_none();
            assert_eq!(secure(&p), takes, "{flags:#x} {wpa:#x} {rsn:#x}");
        }
        assert!(!secure(&ap(0x1, 0, 0x800)), "Enhanced Open");
        assert!(!secure(&ap(0x1, 0, 0x288)), "802.1X");
        // A WEP key, as NetworkManager takes it: the key itself (5 or 13
        // characters, 10 or 26 hex digits), else a passphrase.
        assert_eq!(wep_key_type("abcde"), 1);
        assert_eq!(wep_key_type("0123456789"), 1);
        assert_eq!(wep_key_type("0123456789abcdef0123456789"), 1);
        assert_eq!(wep_key_type("0123456789abcdef012345678g"), 2);
        assert_eq!(wep_key_type("a longer passphrase"), 2);
        let wep = security(Kind::Wep, "abcde").unwrap().unwrap();
        assert_eq!(
            wep["key-mgmt"],
            OwnedValue::try_from(Value::from("none")).unwrap()
        );
        assert_eq!(wep["wep-key-type"], OwnedValue::from(1u32));
        assert!(security(Kind::Open, "x").unwrap().is_none());
        assert!(security(Kind::Enterprise, "x").unwrap().is_none());
    }

    /// (m6-services, services-r2-2) A join of a network whose activation
    /// is under way (its task has answered) with the same password is a
    /// repeat and dropped; with another it is asked, and the activation
    /// it replaces is followed no more (its end is not a failure).
    #[test]
    fn a_repeat_of_a_join_activating_is_dropped() {
        let mut joining = BTreeMap::new();
        let pw = fingerprint(Some("pw"));
        joining.insert(
            "/a/5".to_string(),
            Joining {
                ssid: "Home".into(),
                seen: false,
                asked_with: pw,
            },
        );
        assert!(repeats_joining(&mut joining, "Home", pw));
        assert!(joining.contains_key("/a/5"), "still followed");
        assert!(
            !repeats_joining(&mut joining, "Cafe", pw),
            "another network"
        );
        assert!(!repeats_joining(&mut joining, "Home", fingerprint(None)));
        assert!(joining.is_empty(), "the replaced activation is let go");
    }

    /// (m6-services) How each network is joined, and what is refused
    /// before NetworkManager is asked to activate anything.
    #[test]
    fn enterprise_networks_join_only_through_a_saved_profile() {
        assert_eq!(kind(&ap(0, 0, 0)), Kind::Open);
        assert_eq!(kind(&ap(0x1, 0, 0x188)), Kind::Personal("wpa-psk"));
        assert_eq!(kind(&ap(0x1, 0, 0x400)), Kind::Personal("sae"));
        assert_eq!(kind(&ap(0x1, 0, 0x288)), Kind::Enterprise);
        assert_eq!(kind(&ap(0x1, 0, 0x2000)), Kind::Enterprise, "Suite B");
        // A transition network offering both: the password.
        assert_eq!(kind(&ap(0x1, 0, 0x300)), Kind::Personal("wpa-psk"));
        assert_eq!(kind(&ap(0x1, 0, 0)), Kind::Wep);
        assert_eq!(kind(&ap(0x1, 0x1, 0x1)), Kind::Wep, "pairwise WEP only");
        assert_eq!(kind(&ap(0x1, 0, 0x800)), Kind::Open, "Enhanced Open");
        // Saved: joined with connect(); a password is for a personal or
        // WEP one.
        for k in [
            Kind::Enterprise,
            Kind::Wep,
            Kind::Personal("sae"),
            Kind::Open,
        ] {
            assert_eq!(refusal("n", k, true, false), None, "{k:?}");
        }
        assert!(
            refusal("n", Kind::Enterprise, true, true).is_some_and(|e| e.contains("connect()"))
        );
        assert_eq!(refusal("n", Kind::Wep, true, true), None);
        assert_eq!(refusal("n", Kind::Personal("wpa-psk"), true, true), None);
        // Not saved: enterprise is refused, with or without one; WEP is
        // asked of NetworkManager (its secret agent asks for the key).
        for pw in [false, true] {
            let e = refusal("Corp", Kind::Enterprise, false, pw).unwrap();
            assert!(
                e.contains("enterprise (802.1X)") && e.contains("no saved profile"),
                "{e}"
            );
            assert_eq!(refusal("Old", Kind::Wep, false, pw), None);
        }
        assert_eq!(refusal("n", Kind::Personal("wpa-psk"), false, true), None);
        assert_eq!(refusal("n", Kind::Personal("wpa-psk"), false, false), None);
        assert_eq!(refusal("n", Kind::Open, false, false), None);
        assert!(refusal("n", Kind::Open, false, true).is_some());
    }

    /// (m6-services, services-r1-1) One join per network at a time: a
    /// repeat is dropped, another password waits for the answer (the last
    /// one asked), and other networks are asked at once.
    #[test]
    fn a_network_is_asked_one_join_at_a_time() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _in = rt.enter();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut id = || tasks.spawn(async {}).id();
        let mut a = Asked::default();
        assert!(a.ask("Home", Some("pw")));
        let home = id();
        a.started("Home".into(), home, Some("pw"));
        // A double click: the same join again is dropped.
        assert!(!a.ask("Home", Some("pw")));
        assert!(a.waiting.is_empty());
        // Another network is asked at once.
        assert!(a.ask("Cafe", None));
        let cafe = id();
        a.started("Cafe".into(), cafe, None);
        // Another password waits; the last one asked wins.
        assert!(!a.ask("Home", Some("typo")));
        assert!(!a.ask("Home", Some("fixed")));
        assert_eq!(a.asking.len(), 2, "one task per network");
        assert_eq!(
            a.answered(home),
            Some((
                "Home".into(),
                fingerprint(Some("pw")),
                Some(Some("fixed".into()))
            ))
        );
        assert!(a.ask("Home", Some("again")), "answered: asked at once");
        // A repeat of the join being asked drops the one waiting.
        assert!(!a.ask("Cafe", Some("pw")));
        assert!(!a.ask("Cafe", None));
        assert_eq!(
            a.answered(cafe),
            Some(("Cafe".into(), fingerprint(None), None))
        );
        assert_eq!(a.answered(cafe), None, "answered once");
        let x = id();
        a.started("X".into(), x, None);
        assert!(!a.ask("X", Some("pw")));
        assert_eq!(a.clear(), vec!["X".to_string()]);
        assert!(a.waiting.is_empty() && a.ask("X", Some("pw")));
    }

    /// (m6-service-gaps, gap 28) The joined network's security, from its
    /// access point's flags.
    #[test]
    fn the_joined_networks_security_is_named_from_its_flags() {
        use WifiSecurity::*;
        for (flags, wpa, rsn, want) in [
            (0, 0, 0, Open),
            (0x1, 0, 0x800, EnhancedOpen),
            (0x1, 0, 0x1000, EnhancedOpen),
            (0x1, 0, 0, Wep),
            (0x1, 0x1, 0x1, Wep),
            (0x1, 0x108, 0, Wpa),
            (0x1, 0, 0x188, Wpa2),
            // python-dbusmock writes both flags alike.
            (0x1, 0x100, 0x100, Wpa2),
            (0x1, 0, 0x400, Wpa3),
            // A WPA3 transition network: the flags do not say which.
            (0x1, 0, 0x500, Wpa2),
            (0x1, 0, 0x288, Enterprise),
            (0x1, 0x288, 0, Enterprise),
            (0x1, 0, 0x2000, Enterprise),
        ] {
            assert_eq!(
                security_of(&ap(flags, wpa, rsn)),
                want,
                "{flags:#x} {wpa:#x} {rsn:#x}"
            );
        }
    }

    fn active(state: u32, ap: &str, default: bool) -> Props {
        let v = |x: Value<'_>| OwnedValue::try_from(x).unwrap();
        let path = |p: &str| {
            v(Value::from(
                zbus::zvariant::ObjectPath::try_from(p).unwrap(),
            ))
        };
        Props::from([
            ("Type".to_string(), v(Value::from("802-11-wireless"))),
            ("State".to_string(), v(Value::from(state))),
            ("SpecificObject".to_string(), path(ap)),
            ("Default".to_string(), v(Value::from(default))),
            (
                "Devices".to_string(),
                v(Value::from(vec![
                    zbus::zvariant::ObjectPath::try_from("/d/1").unwrap(),
                ])),
            ),
        ])
    }

    fn named_ap(name: &str, freq: u32) -> Props {
        let mut p = ap(0x1, 0x100, 0x100);
        p.insert(
            "Ssid".into(),
            OwnedValue::try_from(Value::from(name.as_bytes().to_vec())).unwrap(),
        );
        p.insert("Frequency".into(), OwnedValue::from(freq));
        p
    }

    /// (gap 28) Only an activated connection is joined: an activating one
    /// is `connecting` (it showed as joined before, unlike iwd), a
    /// deactivating one `disconnecting`.
    #[test]
    fn only_an_activated_connection_is_joined() {
        let mut nm = Nm::default();
        nm.aps.insert("/ap/1".into(), named_ap("Cafe", 5180));
        nm.aps.insert("/ap/2".into(), named_ap("Home", 2412));
        nm.interfaces.insert("/d/1".into(), "wlan0".into());
        nm.active
            .insert("/a/1".into(), active(ACTIVATING, "/ap/1", false));
        let s = nm.state(false);
        assert_eq!(s.connecting.as_deref(), Some("Cafe"));
        assert_eq!((s.ssid, s.frequency, s.security), (None, None, None));
        let listed = |nm: &Nm| -> Vec<(String, bool)> {
            nm.state(true)
                .access_points
                .into_iter()
                .map(|a| (a.ssid, a.active))
                .collect()
        };
        // (connectivity-r1-1) The list agrees with `ssid`: a network being joined is
        // not the one joined, as over iwd.
        assert_eq!(
            listed(&nm),
            [("Cafe".to_string(), false), ("Home".to_string(), false)]
        );
        assert_eq!(nm.interface(), None, "nothing joined, no default route");
        assert_eq!(
            nm.wifi_device(),
            Some(("/d/1".to_string(), Some("Cafe".to_string()))),
            "a disconnect stops the join"
        );
        nm.active
            .insert("/a/1".into(), active(ACTIVATED, "/ap/1", true));
        let s = nm.state(false);
        assert_eq!(s.ssid.as_deref(), Some("Cafe"));
        assert_eq!(s.connecting, None);
        assert_eq!(s.frequency, Some(5180));
        assert_eq!(s.security, Some(WifiSecurity::Wpa2));
        assert_eq!(nm.interface().as_deref(), Some("wlan0"));
        assert_eq!(
            listed(&nm),
            [("Cafe".to_string(), true), ("Home".to_string(), false)]
        );
        nm.active
            .insert("/a/1".into(), active(DEACTIVATING, "/ap/1", false));
        let s = nm.state(false);
        assert_eq!(s.disconnecting.as_deref(), Some("Cafe"));
        assert_eq!(s.ssid, None);
        assert_eq!(
            listed(&nm),
            [("Cafe".to_string(), false), ("Home".to_string(), false)]
        );
        assert_eq!(nm.wifi_device(), None);
    }

    /// (gap 28) The shell's own requests over the daemon's state: a join
    /// asked shows before NetworkManager knows it, the daemon's own value
    /// wins, a scan asked raises `scanning`, and the address goes while
    /// not connected.
    #[test]
    fn the_overlay_merges_the_shells_requests() {
        let pending = Pending {
            join: Some("Cafe".into()),
            disconnect: Some("Home".into()),
            scan_until: Some(tokio::time::Instant::now()),
            address: Some("192.168.1.9".into()),
        };
        let mut s = Network::default();
        overlay(&pending, &mut s);
        assert_eq!(s.connecting.as_deref(), Some("Cafe"));
        assert_eq!(s.disconnecting.as_deref(), Some("Home"));
        assert!(s.scanning);
        assert_eq!(s.address, None, "not connected");
        let mut s = Network {
            connected: true,
            connecting: Some("Attic".into()),
            ..Network::default()
        };
        overlay(&pending, &mut s);
        assert_eq!(s.connecting.as_deref(), Some("Attic"), "the daemon's");
        assert_eq!(s.address.as_deref(), Some("192.168.1.9"));
        let mut s = Network {
            scanning: true,
            ..Network::default()
        };
        overlay(&Pending::default(), &mut s);
        assert!(s.scanning, "iwd's own scan");
        assert_eq!((s.connecting, s.disconnecting), (None, None));
    }

    /// (gap 28, connectivity-r1-2) NetworkManager refusing a scan because
    /// one runs or has just run (before 1.22), or while a network is being
    /// joined, is no failure; a device that cannot scan (the hardware
    /// switch, not ready: NetworkManager's only `NotAllowed` since) is,
    /// as is any other refusal.
    #[test]
    fn a_scan_refusal_is_no_failure() {
        let refusal = |name: &str, text: &str| {
            let call = zbus::Message::method_call("/", "RequestScan")
                .unwrap()
                .build(&())
                .unwrap();
            zbus::Error::MethodError(
                zbus::names::OwnedErrorName::try_from(name).unwrap(),
                Some(text.to_string()),
                call,
            )
        };
        for activating in [false, true] {
            for text in [
                "Scanning not allowed while already scanning",
                "Scanning not allowed immediately following previous scan",
                "Scanning not allowed at this time",
            ] {
                assert_eq!(
                    scan_failure(refusal(NOT_ALLOWED, text), activating),
                    None,
                    "{text}"
                );
            }
        }
        // NetworkManager 1.22 and later: the device cannot scan.
        for activating in [false, true] {
            assert_eq!(
                scan_failure(
                    refusal(NOT_ALLOWED, "Scanning not allowed while unavailable"),
                    activating
                ),
                Some("the Wi-Fi device is unavailable".to_string())
            );
        }
        // Before: the device unavailable, or a network being joined.
        let older = "Scanning not allowed while unavailable or activating";
        assert_eq!(scan_failure(refusal(NOT_ALLOWED, older), true), None);
        assert_eq!(
            scan_failure(refusal(NOT_ALLOWED, older), false),
            Some("the Wi-Fi device is unavailable".to_string())
        );
        assert_eq!(
            scan_failure(refusal(NOT_ALLOWED, "something new"), false),
            Some("something new".to_string())
        );
        assert_eq!(
            scan_failure(
                refusal(
                    "org.freedesktop.NetworkManager.Device.NotActive",
                    "the device is unavailable"
                ),
                false
            ),
            Some("the device is unavailable".to_string())
        );
        assert_eq!(
            scan_failure(zbus::Error::Failure("no answer".into()), false),
            Some("no answer".to_string())
        );
    }

    /// (gap 28, connectivity-r1-6) A disconnect stops the joins: one
    /// still asked and one activating are neither followed nor reported
    /// while it is in flight (nor a join waiting with them asked). Once it
    /// has left they are dropped, and an activation answered meanwhile is
    /// ended; if it fails, they are followed again and a join that ended
    /// meanwhile is reported.
    #[test]
    fn a_join_cancelled_by_disconnect_is_not_followed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _in = rt.enter();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let home_join = || {
            BTreeMap::from([(
                "/a/5".to_string(),
                Joining {
                    ssid: "Home".into(),
                    seen: true,
                    asked_with: fingerprint(None),
                },
            )])
        };

        // The disconnect leaves.
        let mut a = Asked::default();
        assert!(a.ask("Cafe", None));
        let cafe = tasks.spawn(async {}).id();
        a.started("Cafe".into(), cafe, None);
        assert!(!a.ask("Cafe", Some("pw")), "waits");
        let mut joining = home_join();
        let mut stopped = Stopped::default();
        stop_joins(&mut a, &mut joining, &mut stopped);
        assert!(joining.is_empty(), "not followed");
        assert!(a.waiting.is_empty(), "the waiting join goes too");
        assert_eq!(a.answered(cafe), None, "not followed");
        assert!(a.ask("Cafe", None), "asked afresh");
        assert!(
            stopped.state_changed("/a/5", DEACTIVATED, 2),
            "not reported"
        );
        let attic = tasks.spawn(async {}).id();
        a.started("Attic".into(), attic, None);
        stop_joins(&mut a, &mut joining, &mut stopped);
        // Attic's activation, answered while the disconnect is in flight.
        assert_eq!(
            a.stopped(attic),
            Some(Stop::InFlight("Attic".into(), fingerprint(None)))
        );
        stopped.joins.insert(
            "/a/6".into(),
            StoppedJoin {
                join: Joining {
                    ssid: "Attic".into(),
                    seen: false,
                    asked_with: fingerprint(None),
                },
                late: true,
                ended: None,
            },
        );
        a.left();
        assert_eq!(stopped.left(), ["/a/6"], "the late activation is ended");
        assert!(stopped.joins.is_empty());
        assert_eq!(a.stopped(cafe), Some(Stop::Cancelled));
        assert_eq!(a.stopped(cafe), None, "said once");

        // The disconnect fails: the joins go on, followed and reported.
        let mut a = Asked::default();
        let cafe = tasks.spawn(async {}).id();
        a.started("Cafe".into(), cafe, Some("pw"));
        let mut joining = home_join();
        let mut stopped = Stopped::default();
        stop_joins(&mut a, &mut joining, &mut stopped);
        assert!(
            !stopped.state_changed("/a/9", DEACTIVATED, 9),
            "not stopped"
        );
        assert_eq!(stopped.resume(&mut joining), Vec::new());
        assert!(joining.contains_key("/a/5"), "Home followed again");
        assert_eq!(a.resume(), ["Cafe"]);
        assert_eq!(
            a.answered(cafe),
            Some(("Cafe".into(), fingerprint(Some("pw")), None)),
            "Cafe's answer followed again"
        );
        // A join that ended while the disconnect was in flight, and one
        // whose answer was an error, are reported once it fails.
        stop_joins(&mut a, &mut joining, &mut stopped);
        assert!(stopped.state_changed("/a/5", DEACTIVATED, 9));
        stopped
            .failures
            .push(("Attic".into(), "`Attic` is not in range".into()));
        assert_eq!(
            stopped.resume(&mut joining),
            [
                ("Attic".to_string(), "`Attic` is not in range".to_string()),
                ("Home".to_string(), reason_text(9)),
            ]
        );
        assert!(joining.is_empty());
        // A network asked anew while the disconnect was in flight keeps
        // its new join; the stopped one stays cancelled.
        let old = tasks.spawn(async {}).id();
        a.started("Cafe".into(), old, None);
        a.cancel();
        let new = tasks.spawn(async {}).id();
        assert!(a.ask("Cafe", None));
        a.started("Cafe".into(), new, None);
        assert_eq!(a.resume(), Vec::<String>::new());
        assert_eq!(a.stopped(old), Some(Stop::Cancelled));
        assert_eq!(a.answered(new).map(|(s, ..)| s), Some("Cafe".into()));
    }

    #[test]
    fn a_join_answered_after_its_connection_ended_is_settled_from_the_signal() {
        let mut e = Ended::default();
        e.note("/a/1".into(), 1, 0);
        assert_eq!(e.take("/a/1"), None, "still activating: not an end");
        e.note("/a/1".into(), DEACTIVATED, 9);
        e.note("/a/2".into(), ACTIVATED, 0);
        assert_eq!(e.take("/a/1"), Some((DEACTIVATED, 9)));
        assert_eq!(e.take("/a/1"), None, "taken once");
        for i in 0..100 {
            e.note(format!("/b/{i}"), ACTIVATED, 0);
        }
        assert_eq!(e.0.len(), Ended::KEEP, "bounded");
        assert_eq!(e.take("/a/2"), None, "the oldest went");
    }
}

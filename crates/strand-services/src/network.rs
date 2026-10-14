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
        if !settle_joins(cx, &nm, &mut joining) {
            return Ok(Next::Stop);
        }
        // Asked once there is a Wi-Fi device to ask (a NetworkManager
        // that just appeared may not list its devices yet).
        if scanning && want_scan && request_scan(&daemon, &nm).await {
            want_scan = false;
        }
        if !cx.update(|s| *s = nm.state(scanning)) {
            return Ok(Next::Stop);
        }
        cx.ready();
        'follow: loop {
            tokio::select! {
                ev = daemon.next() => match ev {
                    None => return Err(ServiceError("the system bus connection ended".into())),
                    Some(DaemonEvent::Owner) => {
                        want_scan = scanning;
                        ended.clear();
                        // Its activations went with it, and the joins
                        // still asking stop here: their next call would
                        // reach the new NetworkManager, whose answer
                        // this would not follow.
                        asking.shutdown().await;
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
                            } else if !asking.is_empty() {
                                // Perhaps a join whose answer is still
                                // on its way.
                                ended.note(path, state, reason);
                            }
                        }
                        match nm.signal(&daemon, &m, scanning).await {
                        None => break 'follow,
                        Some(false) => {}
                        Some(true) => {
                            if !cx.update(|s| *s = nm.state(scanning)) {
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
                Some(done) = asking.join_next_with_id(), if !asking.is_empty() => {
                    let (id, answer) = match done {
                        Ok((id, answer)) => (id, answer),
                        Err(e) => (e.id(), Err(zbus::Error::Failure("the join stopped".into()))),
                    };
                    let Some((ssid, asked_with, next)) = asked.answered(id) else {
                        continue;
                    };
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
                            Some(match e {
                                zbus::Error::MethodError(_, Some(text), _) => text,
                                zbus::Error::Failure(text) => text,
                                e => e.to_string(),
                            })
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
                        asked.started(ssid, id, password.as_deref());
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(Next::Stop),
                    Some(Msg::Watch { field: "access_points", on }) => {
                        scanning = on;
                        want_scan = on;
                        break 'follow;
                    }
                    Some(Msg::Write(w)) if w.field == "wifi" => {
                        let on: bool = w.value().unwrap_or(nm.state(scanning).wifi);
                        let set = dbus::set(conn, NM, ROOT, NM, "WirelessEnabled", on.into()).await;
                        let now = match set {
                            Ok(()) => on,
                            Err(e) => {
                                log::warn!("network: Wi-Fi not switched: {e}");
                                // The radio as last read.
                                nm.state(scanning).wifi
                            }
                        };
                        if !cx.report(&w, |s| s.wifi = now) {
                            return Ok(Next::Stop);
                        }
                    }
                    Some(Msg::Action(a)) => {
                        let (item, password) = match a {
                            NetworkAction::Connect { item } => (item, None),
                            NetworkAction::ConnectWith { item, password } => (item, Some(password)),
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
                            asked.started(item.ssid, id, password.as_deref());
                        }
                    }
                    Some(_) => {}
                },
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
        std::mem::take(&mut self.asking).into_keys().collect()
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

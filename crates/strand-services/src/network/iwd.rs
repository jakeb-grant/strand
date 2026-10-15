//! `network` over iwd (`net.connman.iwd`) on the system bus, where
//! NetworkManager is absent (owner-testing gaps 15, decisions.md
//! m6-w2-services).
//!
//! iwd's objects come from its `ObjectManager` (`GetManagedObjects`, then
//! `InterfacesAdded` and `InterfacesRemoved`) and are followed through
//! their `PropertiesChanged`, all from one match rule on iwd's name. The
//! Wi-Fi device is the first station-mode `Device` (its `Powered` is
//! `wifi`); its `Station` says whether it is connected (`State`) and to
//! which `Network` (`ConnectedNetwork`, whose `Name` is `ssid`). The
//! joined network's strength comes from a signal level agent this
//! process serves (`RegisterSignalLevelAgent`: iwd says when the signal
//! crosses a 5 dBm step, nothing is polled), read first from
//! `GetOrderedNetworks` when the network is joined. The networks in range
//! (`access_points`) are `GetOrderedNetworks`, asked again when a scan
//! ends, and a scan (`Scan`) is asked only while a visible reader reads
//! them.
//!
//! Joining is `Network.Connect`, in a task of its own (iwd answers once
//! the network is joined or not); `connect_with(password)` holds the
//! password for the agent this process registers
//! (`AgentManager.RegisterAgent`), which iwd asks for the passphrase of
//! a network it does not know. iwd keeps a passphrase that worked, as
//! NetworkManager keeps a profile. The refusals are NetworkManager's: an
//! open network takes no password, an enterprise one joins only once
//! iwd knows it (its provisioning file).
//!
//! (m6-service-gaps) The station's `State` says what is in progress
//! (`connecting`, `disconnecting`, with `ConnectedNetwork` naming the
//! network); `scanning` is its `Scanning`. The joined network's frequency
//! and security come from `StationDiagnostic.GetDiagnostics`, read when
//! the station becomes connected (a join, a roam's end), else its
//! security from the network's `Type`. `disconnect()` is
//! `Station.Disconnect` in a task; the joins it stops are not reported
//! (iwd answers them `Aborted`) unless it fails, when they go on.
//! `scan()` is `Station.Scan`, iwd's `Busy` being no failure.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use super::{
    AccessPoint, Addresses, Kind, Network, NetworkAction, NetworkEvent, Next, Pending,
    WifiSecurity, error_text, object_path, overlay, refusal,
};
use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Cx, Event, Msg, ServiceError};

/// iwd's bus name.
pub const IWD: &str = "net.connman.iwd";
/// iwd's agent manager.
pub const MANAGER: &str = "/net/connman/iwd";
const DEVICE: &str = "net.connman.iwd.Device";
const STATION: &str = "net.connman.iwd.Station";
const STATION_DIAGNOSTIC: &str = "net.connman.iwd.StationDiagnostic";
const NETWORK: &str = "net.connman.iwd.Network";
const KNOWN_NETWORK: &str = "net.connman.iwd.KnownNetwork";
const AGENT_MANAGER: &str = "net.connman.iwd.AgentManager";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
/// Where this process serves its agents (the passphrase agent and the
/// signal level agent).
pub const AGENT_PATH: &str = "/dev/strand/network/iwd";

/// The signal level steps the agent is told of, dBm, strongest first:
/// level 0 is stronger than -50 dBm, level 9 weaker than -90.
pub const LEVELS: [i16; 9] = [-50, -55, -60, -65, -70, -75, -80, -85, -90];

/// A signal of `dbm` as a strength, 0 to 1: -100 dBm and below is 0,
/// -50 and above 1 (NetworkManager's own scale).
pub fn quality(dbm: f64) -> f64 {
    ((dbm + 100.0) / 50.0).clamp(0.0, 1.0)
}

/// The strength a signal level agent's `level` stands for: the middle
/// of its step.
pub fn level_strength(level: u8) -> f64 {
    let i = usize::from(level);
    let last = LEVELS.len() - 1;
    let dbm = match i {
        0 => f64::from(LEVELS[0]) + 2.5,
        i if i > last => f64::from(LEVELS[last]) - 2.5,
        i => (f64::from(LEVELS[i - 1]) + f64::from(LEVELS[i])) / 2.0,
    };
    quality(dbm)
}

/// Calls iwd's `method` of `iface` on `path`, bounded by `limit`, with
/// `NO_AUTO_START`: a call that reaches the bus while iwd is gone (it
/// exited and its owner change is not handled yet) is refused rather than
/// having the bus start iwd beside a NetworkManager that runs
/// wpa_supplicant (review minor 5; `Daemon::watch` follows iwd without
/// starting it for the same reason).
pub async fn call<B, R>(
    conn: &zbus::Connection,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
    limit: Duration,
) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    dbus::timed_for(limit, async {
        let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(conn)
            .destination(IWD)?
            .path(path.to_string())?
            .interface(iface.to_string())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        proxy
            .call_with_flags(method, zbus::proxy::MethodFlags::NoAutoStart.into(), body)
            .await?
            .ok_or_else(|| zbus::Error::Failure(format!("iwd's {method} did not answer")))
    })
    .await
}

/// iwd's objects: path, interface, properties.
type Objects = BTreeMap<String, BTreeMap<String, Props>>;

/// What is read from iwd.
#[derive(Debug, Default)]
pub struct Iwd {
    objects: Objects,
    /// The networks in range, strongest first, from the last
    /// `GetOrderedNetworks`: path and strength.
    ordered: Vec<(String, f64)>,
    /// The joined network's strength from the signal level agent (newer
    /// than `ordered`'s while set).
    level: Option<f64>,
    /// What `GetDiagnostics` said of the joined network.
    diagnostics: Option<Diagnostics>,
    /// The network the station last named while joined or joining, by
    /// name: the one a `disconnecting` station leaves once it no longer
    /// names it. Forgotten once the station is disconnected.
    last_joined: Option<String>,
}

/// The joined network as `StationDiagnostic.GetDiagnostics` tells it.
#[derive(Debug, Default, Clone, PartialEq)]
struct Diagnostics {
    /// MHz.
    frequency: Option<u32>,
    /// iwd's words (`WPA2-Personal`, `WPA3-Personal`, `OWE`).
    security: Option<String>,
}

/// How a network is secured, from iwd's diagnostic words
/// (`WPA2-Personal`, `WPA3-Personal + FT`, `WPA1-Enterprise`, `OWE`).
pub fn security_named(text: &str) -> Option<WifiSecurity> {
    Some(if text.contains("OWE") {
        WifiSecurity::EnhancedOpen
    } else if ["Enterprise", "FILS", "8021X"]
        .iter()
        .any(|w| text.contains(w))
    {
        WifiSecurity::Enterprise
    } else if text.contains("WPA3") {
        WifiSecurity::Wpa3
    } else if text.contains("WPA1") || text.starts_with("WPA-") {
        WifiSecurity::Wpa
    } else if text.contains("WPA2") {
        WifiSecurity::Wpa2
    } else if text.contains("WEP") {
        WifiSecurity::Wep
    } else if text.contains("Open") {
        WifiSecurity::Open
    } else {
        return None;
    })
}

/// How a network of iwd's `Type` is secured, when its diagnostics do not
/// say: a personal one is taken as WPA2.
fn security_of_type(ty: &str) -> Option<WifiSecurity> {
    Some(match ty {
        "open" => WifiSecurity::Open,
        "wep" => WifiSecurity::Wep,
        "psk" => WifiSecurity::Wpa2,
        "8021x" => WifiSecurity::Enterprise,
        _ => return None,
    })
}

/// What a signal changed.
#[derive(Debug, Default, PartialEq)]
struct Change {
    /// The state may have changed.
    changed: bool,
    /// Ask `GetOrderedNetworks` again (a scan ended, another network was
    /// joined).
    reorder: bool,
    /// The station became connected (a join, a roam's end): read its
    /// diagnostics.
    diagnose: bool,
}

impl Iwd {
    fn props(&self, path: &str, iface: &str) -> Option<&Props> {
        self.objects.get(path)?.get(iface)
    }

    /// The Wi-Fi device: the first in station mode.
    fn device(&self) -> Option<(&String, &Props)> {
        self.objects.iter().find_map(|(path, ifaces)| {
            let d = ifaces.get(DEVICE)?;
            let mode = dbus::text(d, "Mode");
            (mode.is_none() || mode.as_deref() == Some("station")).then_some((path, d))
        })
    }

    /// The device's station (none while it is off).
    fn station(&self) -> Option<(&String, &Props)> {
        let (path, _) = self.device()?;
        Some((path, self.props(path, STATION)?))
    }

    /// The network joined (or being joined or left: iwd names it from
    /// the start of a join).
    fn joined(&self) -> Option<String> {
        let (_, s) = self.station()?;
        object_path(s, "ConnectedNetwork").filter(|p| p != "/")
    }

    /// The station's `State` (`connected`, `connecting`, `roaming`).
    fn station_state(&self) -> Option<String> {
        self.station().and_then(|(_, s)| dbus::text(s, "State"))
    }

    /// The station is connected (roaming included).
    fn connected(&self) -> bool {
        matches!(
            self.station_state().as_deref(),
            Some("connected" | "roaming")
        )
    }

    /// Network `path`'s name.
    fn name(&self, path: &str) -> Option<String> {
        self.props(path, NETWORK)
            .and_then(|n| dbus::text(n, "Name"))
    }

    /// The network joined or being joined, by name.
    fn in_use(&self) -> Option<String> {
        let state = self.station_state()?;
        let busy =
            matches!(state.as_str(), "connected" | "roaming") || state.starts_with("connecting");
        self.joined().filter(|_| busy).and_then(|p| self.name(&p))
    }

    /// The Wi-Fi interface whose address is the connection's, while
    /// connected.
    fn interface(&self) -> Option<String> {
        if !self.connected() {
            return None;
        }
        self.device().and_then(|(_, d)| dbus::text(d, "Name"))
    }

    /// The name of the network joined or being joined, kept for
    /// `disconnecting` (connectivity-r1-4): iwd stops a join or leaves a
    /// network by naming none (`ConnectedNetwork` "/") and then saying
    /// `disconnecting`, so the network left is the one named last, a join
    /// (the shell's or iwd's own) included. Once the station is
    /// disconnected (or gone) no network is left, and none is kept.
    fn remember(&mut self) {
        match self.station_state().as_deref() {
            None | Some("disconnected") => self.last_joined = None,
            Some("disconnecting") => {}
            Some(_) => {
                if let Some(name) = self.in_use() {
                    self.last_joined = Some(name);
                }
            }
        }
    }

    /// Read the joined network's diagnostics (none if iwd has no
    /// `StationDiagnostic`, or it fails).
    async fn diagnose(&mut self, conn: &zbus::Connection) {
        self.diagnostics = None;
        let Some((station, _)) = self.station() else {
            return;
        };
        let station = station.clone();
        let r: zbus::Result<Props> = call(
            conn,
            &station,
            STATION_DIAGNOSTIC,
            "GetDiagnostics",
            &(),
            dbus::CALL_TIMEOUT,
        )
        .await;
        match r {
            Ok(d) => {
                self.diagnostics = Some(Diagnostics {
                    frequency: dbus::number(&d, "Frequency").map(|f| f as u32),
                    security: dbus::text(&d, "Security"),
                });
            }
            Err(e) => log::debug!("network: no iwd diagnostics: {e}"),
        }
    }

    /// The state these objects make.
    pub fn state(&self, scanning: bool) -> Network {
        let wifi = self
            .device()
            .and_then(|(_, d)| dbus::boolean(d, "Powered"))
            .unwrap_or(false);
        let state = self.station_state();
        let connected = self.connected();
        let joined = self.joined().filter(|_| connected);
        let name = |path: &str| self.name(path);
        let in_flight = |what: &str| {
            state
                .as_deref()
                .is_some_and(|s| s.starts_with(what))
                .then(|| self.joined().and_then(|p| self.name(&p)))
                .flatten()
        };
        let connecting = in_flight("connecting");
        let disconnecting = state
            .as_deref()
            .filter(|s| *s == "disconnecting")
            .and_then(|_| in_flight("disconnecting").or_else(|| self.last_joined.clone()));
        let type_security = |path: &str| {
            self.props(path, NETWORK)
                .and_then(|n| dbus::text(n, "Type"))
                .and_then(|t| security_of_type(&t))
        };
        let (frequency, security) = match joined.as_deref() {
            None => (None, None),
            Some(p) => {
                let d = self.diagnostics.clone().unwrap_or_default();
                (
                    d.frequency.map(i64::from),
                    d.security
                        .as_deref()
                        .and_then(security_named)
                        .or_else(|| type_security(p)),
                )
            }
        };
        let station_scans = self
            .station()
            .and_then(|(_, s)| dbus::boolean(s, "Scanning"))
            .unwrap_or(false);
        let ordered = |path: &str| {
            self.ordered
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, s)| *s)
        };
        let strength = joined
            .as_deref()
            .and_then(|p| self.level.or_else(|| ordered(p)))
            .unwrap_or(0.0);
        let mut access_points: Vec<AccessPoint> = Vec::new();
        if scanning {
            let mut by_ssid: BTreeMap<String, AccessPoint> = BTreeMap::new();
            for (path, s) in &self.ordered {
                let Some(ssid) = name(path) else {
                    continue;
                };
                let active = joined.as_deref() == Some(path.as_str());
                let ap = AccessPoint {
                    ssid: ssid.clone(),
                    strength: if active { strength } else { *s },
                    secure: matches!(self.kind(path), Some(Kind::Personal(_) | Kind::Wep)),
                    active,
                };
                by_ssid
                    .entry(ssid)
                    .and_modify(|e| {
                        e.active |= ap.active;
                        if ap.strength > e.strength {
                            e.strength = ap.strength;
                        }
                    })
                    .or_insert(ap);
            }
            access_points = by_ssid.into_values().collect();
            access_points.sort_by(|a, b| {
                b.active
                    .cmp(&a.active)
                    .then(b.strength.total_cmp(&a.strength))
                    .then(a.ssid.cmp(&b.ssid))
            });
        }
        Network {
            connected,
            ssid: joined.as_deref().and_then(name),
            strength,
            wifi,
            access_points,
            // This body runs only while iwd owns its name.
            available: true,
            connecting,
            disconnecting,
            frequency,
            security,
            address: None,
            scanning: station_scans,
            failed: Event::default(),
        }
    }

    /// How network `path` is joined (iwd's `Type`).
    fn kind(&self, path: &str) -> Option<Kind> {
        let n = self.props(path, NETWORK)?;
        Some(match dbus::text(n, "Type").as_deref() {
            Some("open") => Kind::Open,
            Some("wep") => Kind::Wep,
            Some("8021x") => Kind::Enterprise,
            _ => Kind::Personal("psk"),
        })
    }

    /// The network named `ssid` to join (on the Wi-Fi device when it
    /// lists one there), and the known network to forget first: iwd uses
    /// a passphrase it keeps without asking, so a password given for a
    /// network it knows replaces the kept one (review minor 6). Or why
    /// not.
    fn to_join(&self, ssid: &str, password: bool) -> Result<(String, Option<String>), String> {
        let device = self.device().map(|(p, _)| p.clone());
        let mut named = self.objects.iter().filter_map(|(path, ifaces)| {
            let n = ifaces.get(NETWORK)?;
            (dbus::text(n, "Name").as_deref() == Some(ssid)).then_some((path, n))
        });
        let first = named.next();
        let (path, n) = std::iter::once(first)
            .flatten()
            .chain(named)
            .find(|(_, n)| object_path(n, "Device") == device)
            .or(first)
            .ok_or_else(|| format!("`{ssid}` is not in range"))?;
        let known = object_path(n, "KnownNetwork").filter(|k| k != "/");
        let kind = self.kind(path).unwrap_or(Kind::Personal("psk"));
        match refusal(ssid, kind, known.is_some(), password) {
            Some(why) => Err(why),
            None => Ok((path.clone(), known.filter(|_| password))),
        }
    }

    /// Read every object from iwd.
    async fn read(&mut self, daemon: &Daemon) -> zbus::Result<()> {
        *self = Iwd::default();
        if daemon.owner().is_none() {
            return Ok(());
        }
        let conn = daemon.conn();
        let objects: HashMap<OwnedObjectPath, HashMap<String, Props>> = call(
            conn,
            "/",
            OBJECT_MANAGER,
            "GetManagedObjects",
            &(),
            dbus::READ_TIMEOUT,
        )
        .await?;
        self.objects = objects
            .into_iter()
            .map(|(path, ifaces)| (path.to_string(), ifaces.into_iter().collect()))
            .collect();
        self.reorder(conn).await;
        self.remember();
        if self.station_state().as_deref() == Some("connected") {
            self.diagnose(conn).await;
        }
        Ok(())
    }

    /// Ask the station for the networks in range again.
    async fn reorder(&mut self, conn: &zbus::Connection) {
        self.ordered.clear();
        let Some((station, _)) = self.station() else {
            return;
        };
        let station = station.clone();
        let reply: zbus::Result<Vec<(OwnedObjectPath, i16)>> = call(
            conn,
            &station,
            STATION,
            "GetOrderedNetworks",
            &(),
            dbus::READ_TIMEOUT,
        )
        .await;
        match reply {
            // Hundredths of a dBm.
            Ok(nets) => {
                self.ordered = nets
                    .into_iter()
                    .map(|(p, s)| (p.to_string(), quality(f64::from(s) / 100.0)))
                    .collect();
            }
            Err(e) => log::debug!("network: iwd's networks not read: {e}"),
        }
    }

    /// Apply a signal from iwd.
    fn signal(&mut self, m: &zbus::Message) -> Change {
        if let Some(c) = dbus::properties_changed(m) {
            let Some(props) = self
                .objects
                .get_mut(&c.path)
                .and_then(|i| i.get_mut(&c.iface))
            else {
                return Change::default();
            };
            let moved = c.iface == STATION && c.changed.contains_key("ConnectedNetwork");
            let scanned = c.iface == STATION
                && c.changed
                    .get("Scanning")
                    .is_some_and(|v| bool::try_from(v).ok() == Some(false));
            let new_state = (c.iface == STATION)
                .then(|| c.changed.get("State"))
                .flatten()
                .and_then(|v| <&str>::try_from(v).ok().map(str::to_string));
            for name in &c.invalidated {
                props.remove(name);
            }
            props.extend(c.changed);
            if moved {
                self.level = None;
                self.diagnostics = None;
            }
            let connected = new_state.as_deref() == Some("connected");
            if new_state
                .as_deref()
                .is_some_and(|s| !matches!(s, "connected" | "roaming"))
            {
                self.diagnostics = None;
            }
            self.remember();
            // Connected, or a network joined while connected (iwd may
            // name it after saying `connected`): its diagnostics anew.
            let diagnose =
                (connected || moved) && self.station_state().as_deref() == Some("connected");
            return Change {
                changed: true,
                reorder: moved || scanned,
                diagnose,
            };
        }
        if dbus::interface(m).as_deref() != Some(OBJECT_MANAGER) {
            return Change::default();
        }
        match dbus::member(m).as_deref() {
            Some("InterfacesAdded") => {
                let Ok((path, ifaces)) = m
                    .body()
                    .deserialize::<(OwnedObjectPath, HashMap<String, Props>)>()
                else {
                    return Change::default();
                };
                let station = ifaces.contains_key(STATION);
                if station {
                    // A level told before the radio went off is no
                    // longer the signal (review minor 4).
                    self.level = None;
                    self.diagnostics = None;
                }
                self.objects
                    .entry(path.to_string())
                    .or_default()
                    .extend(ifaces);
                self.remember();
                Change {
                    changed: true,
                    reorder: station,
                    diagnose: station && self.station_state().as_deref() == Some("connected"),
                }
            }
            Some("InterfacesRemoved") => {
                let Ok((path, ifaces)) = m.body().deserialize::<(OwnedObjectPath, Vec<String>)>()
                else {
                    return Change::default();
                };
                let path = path.to_string();
                if ifaces.iter().any(|i| i == STATION) {
                    self.level = None;
                    self.diagnostics = None;
                }
                if let Some(o) = self.objects.get_mut(&path) {
                    o.retain(|i, _| !ifaces.contains(i));
                    if o.is_empty() {
                        self.objects.remove(&path);
                    }
                }
                self.ordered.retain(|(p, _)| *p != path);
                Change {
                    changed: true,
                    reorder: false,
                    diagnose: false,
                }
            }
            _ => Change::default(),
        }
    }
}

/// The passwords `connect_with` gives, by network path, for the agent.
type Passwords = Arc<Mutex<HashMap<String, String>>>;

/// iwd's unique bus name: the only peer the agents answer (review minor
/// 7: a password goes to iwd, never to another process on the bus).
type Owner = Arc<Mutex<Option<String>>>;

/// Whether the call `h` heads came from iwd.
fn from_iwd(iwd: &Owner, h: &zbus::message::Header<'_>) -> bool {
    let owner = iwd.lock().ok().and_then(|o| o.clone());
    match (owner, h.sender()) {
        (Some(o), Some(s)) => o == s.as_str(),
        _ => false,
    }
}

/// The passphrase agent (`net.connman.iwd.Agent`).
struct Agent {
    passwords: Passwords,
    iwd: Owner,
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "net.connman.iwd.Agent.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Canceled(String),
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl Agent {
    fn release(&self) {}

    /// The password `connect_with` gave for `network`; refused when none
    /// was (a `connect()` of a network iwd does not know), and to anyone
    /// but iwd.
    fn request_passphrase(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        network: ObjectPath<'_>,
    ) -> Result<String, AgentError> {
        if !from_iwd(&self.iwd, &h) {
            return Err(AgentError::Canceled("only iwd is answered".into()));
        }
        let given = self
            .passwords
            .lock()
            .ok()
            .and_then(|p| p.get(network.as_str()).cloned());
        given.ok_or_else(|| AgentError::Canceled("no password was given".into()))
    }

    fn cancel(&self, reason: String) {
        log::debug!("network: iwd cancelled its question: {reason}");
    }
}

/// The signal level agent (`net.connman.iwd.SignalLevelAgent`).
struct Levels {
    levels: mpsc::UnboundedSender<u8>,
    iwd: Owner,
}

#[zbus::interface(name = "net.connman.iwd.SignalLevelAgent")]
impl Levels {
    fn release(&self, _device: ObjectPath<'_>) {}

    /// A level from iwd; anyone else's is ignored.
    fn changed(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        _device: ObjectPath<'_>,
        level: u8,
    ) {
        if from_iwd(&self.iwd, &h) {
            let _ = self.levels.send(level);
        }
    }
}

/// The agents served, registered with iwd, and taken down again. Each
/// registration is tried once per iwd (a body follows one owner) and,
/// for the signal level agent, once per station: one refused (another
/// tool holds the station's one signal agent) is not asked again on
/// every signal (review minor 3).
struct Agents {
    conn: zbus::Connection,
    /// The station the signal level agent is registered with.
    station: Option<String>,
    /// The station last tried (`Some(None)`: tried with none).
    tried_station: Option<Option<String>>,
    registered: bool,
    tried_agent: bool,
}

impl Agents {
    async fn serve(
        conn: &zbus::Connection,
        passwords: Passwords,
        levels: mpsc::UnboundedSender<u8>,
        iwd: Owner,
    ) -> zbus::Result<Agents> {
        let server = conn.object_server();
        // A body that ended without taking them down.
        let _ = server.remove::<Agent, _>(AGENT_PATH).await;
        let _ = server.remove::<Levels, _>(AGENT_PATH).await;
        let agent = Agent {
            passwords,
            iwd: iwd.clone(),
        };
        server.at(AGENT_PATH, agent).await?;
        server.at(AGENT_PATH, Levels { levels, iwd }).await?;
        Ok(Agents {
            conn: conn.clone(),
            station: None,
            tried_station: None,
            registered: false,
            tried_agent: false,
        })
    }

    /// Register the passphrase agent once, and the signal level agent
    /// with the station iwd has now (once per station).
    async fn register(&mut self, iwd: &Iwd) {
        let Ok(path) = ObjectPath::try_from(AGENT_PATH) else {
            return;
        };
        if !std::mem::replace(&mut self.tried_agent, true) {
            let r: zbus::Result<()> = call(
                &self.conn,
                MANAGER,
                AGENT_MANAGER,
                "RegisterAgent",
                &(path.clone(),),
                dbus::CALL_TIMEOUT,
            )
            .await;
            match r {
                Ok(()) => self.registered = true,
                Err(e) => log::warn!("network: no iwd agent (joining asks no password): {e}"),
            }
        }
        let station = iwd.station().map(|(p, _)| p.clone());
        if self.tried_station.as_ref() == Some(&station) {
            return;
        }
        self.tried_station = Some(station.clone());
        self.station = None;
        let Some(station) = station else {
            return;
        };
        let r: zbus::Result<()> = call(
            &self.conn,
            &station,
            STATION,
            "RegisterSignalLevelAgent",
            &(path, LEVELS.to_vec()),
            dbus::CALL_TIMEOUT,
        )
        .await;
        match r {
            Ok(()) => self.station = Some(station),
            Err(e) => log::warn!("network: iwd tells no signal levels: {e}"),
        }
    }

    /// Unregister (while iwd runs) and stop serving.
    async fn down(self, iwd_runs: bool) {
        if iwd_runs && let Ok(p) = ObjectPath::try_from(AGENT_PATH) {
            if self.registered {
                let _: zbus::Result<()> = call(
                    &self.conn,
                    MANAGER,
                    AGENT_MANAGER,
                    "UnregisterAgent",
                    &(p.clone(),),
                    dbus::CALL_TIMEOUT,
                )
                .await;
            }
            if let Some(station) = &self.station {
                let _: zbus::Result<()> = call(
                    &self.conn,
                    station,
                    STATION,
                    "UnregisterSignalLevelAgent",
                    &(p,),
                    dbus::CALL_TIMEOUT,
                )
                .await;
            }
        }
        let server = self.conn.object_server();
        let _ = server.remove::<Agent, _>(AGENT_PATH).await;
        let _ = server.remove::<Levels, _>(AGENT_PATH).await;
    }
}

/// Ask the station for a scan: none without a station (the radio off),
/// else how iwd answered.
async fn scan(conn: &zbus::Connection, iwd: &Iwd) -> Option<zbus::Result<()>> {
    let (station, _) = iwd.station()?;
    let r: zbus::Result<()> = call(conn, station, STATION, "Scan", &(), dbus::CALL_TIMEOUT).await;
    if let Err(e) = &r {
        log::debug!("network: iwd scan not started: {e}");
    }
    Some(r)
}

/// Why a refused `Scan` is a failure; `None` for iwd's `Busy` (a scan
/// runs, which ends as well).
fn scan_failure(e: zbus::Error) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, _, _) if name.as_str() == "net.connman.iwd.Busy" => None,
        e => Some(error_text(e)),
    }
}

/// What a join's task ending reports: nothing for one that worked, or
/// that `disconnect()` stopped (iwd answers it `Aborted`, which is no
/// failure of the join's own).
fn join_failure(cancelled: bool, r: Result<(), String>) -> Option<String> {
    match r {
        Err(e) if !cancelled => Some(e),
        _ => None,
    }
}

/// What a join's failure says.
fn join_error(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, text, _) => match name.as_str() {
            "net.connman.iwd.Aborted" | "net.connman.iwd.NoAgent" => {
                "a password is needed (none was given, or it was wrong)".into()
            }
            "net.connman.iwd.Failed" if text.is_none() => "it could not be joined".into(),
            _ => text.unwrap_or_else(|| name.to_string()),
        },
        zbus::Error::Failure(text) => text,
        e => e.to_string(),
    }
}

/// Join network `path`, forgetting the known network `forget` first (a
/// new password for it): iwd answers once it is joined or not. Once
/// `stop` is raised (a `disconnect()` before iwd was asked to join,
/// [`drop_joins`]) nothing more is asked.
async fn join(
    conn: zbus::Connection,
    path: String,
    forget: Option<String>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let stopped = || stop.load(Ordering::SeqCst);
    if stopped() {
        return Err("the join was stopped".into());
    }
    if let Some(known) = forget {
        let r: zbus::Result<()> = call(
            &conn,
            &known,
            KNOWN_NETWORK,
            "Forget",
            &(),
            dbus::CALL_TIMEOUT,
        )
        .await;
        r.map_err(join_error)?;
    }
    if stopped() {
        return Err("the join was stopped".into());
    }
    // iwd's own handshake and DHCP limits are well under this.
    let r: zbus::Result<()> = call(
        &conn,
        &path,
        NETWORK,
        "Connect",
        &(),
        super::ACTIVATE_TIMEOUT,
    )
    .await;
    r.map_err(join_error)
}

/// A join asked and not answered.
#[derive(Debug)]
struct Join {
    /// Its network's name.
    ssid: String,
    /// Its network's path.
    path: String,
    /// The `disconnect()` (its task) that stopped it: its end is not
    /// reported, unless that disconnect fails (connectivity-r1-6).
    stopped_by: Option<tokio::task::Id>,
    /// Stopped by a `disconnect()` that asked iwd nothing (the station
    /// named no network, [`drop_joins`]): its end is never reported, and
    /// if iwd joins it all the same the station is disconnected.
    dropped: bool,
    /// Raised with `dropped`: the task asks iwd nothing more.
    stop: Arc<AtomicBool>,
}

impl Join {
    fn new(ssid: String, path: String, stop: Arc<AtomicBool>) -> Join {
        Join {
            ssid,
            path,
            stopped_by: None,
            dropped: false,
            stop,
        }
    }

    fn cancelled(&self) -> bool {
        self.stopped_by.is_some() || self.dropped
    }
}

/// A `disconnect()` while the station names no network (a join still
/// forgetting the old password, or not yet asked of iwd): iwd would
/// answer `Station.Disconnect` `NotConnected`, so the joins are stopped
/// here instead, their tasks asking iwd nothing more; whether there were
/// any (connectivity-r2-1).
fn drop_joins(joining: &mut Joining) -> bool {
    let mut any = false;
    for j in joining.values_mut().filter(|j| !j.cancelled()) {
        j.dropped = true;
        j.stop.store(true, Ordering::SeqCst);
        any = true;
    }
    any
}

/// `Station.Disconnect` on `station`: iwd leaves its network, or stops
/// the join in progress, and joins nothing on its own until one is
/// joined again.
async fn leave_station(conn: &zbus::Connection, station: &str) -> Result<(), String> {
    let r: zbus::Result<()> = call(
        conn,
        station,
        STATION,
        "Disconnect",
        &(),
        dbus::READ_TIMEOUT,
    )
    .await;
    r.map_err(error_text)
}

/// Disconnect `leave` failed: the joins it stopped go on, and are
/// reported as any join is. Their networks.
fn resume_joins(joining: &mut Joining, leave: tokio::task::Id) -> Vec<String> {
    joining
        .values_mut()
        .filter(|j| j.stopped_by == Some(leave))
        .map(|j| {
            j.stopped_by = None;
            j.ssid.clone()
        })
        .collect()
}

/// Joins asked and not answered, by task.
type Joining = HashMap<tokio::task::Id, Join>;

/// Every join in flight fails with `why` (its task is dropped with the
/// body), but those `disconnect()` stopped; `false` once the service was
/// stopped.
fn fail_joins(cx: &mut Cx<Network>, joining: &mut Joining, why: &str) -> bool {
    for j in std::mem::take(joining).into_values() {
        if !j.cancelled() && !cx.emit(NetworkEvent::Failed((j.ssid, why.to_string()))) {
            return false;
        }
    }
    true
}

/// Whether `name` has an owner on `conn` now.
pub(super) async fn owned(conn: &zbus::Connection, name: &str) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(name) else {
        return false;
    };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// The `network` body over iwd, until NetworkManager appears or iwd goes
/// ([`Next::Again`]: the backend is chosen again) or the service stops.
pub(super) async fn run(
    cx: &mut Cx<Network>,
    conn: &zbus::Connection,
) -> Result<Next, ServiceError> {
    // NetworkManager appearing takes over.
    let dbus_proxy = zbus::fdo::DBusProxy::new(conn).await?;
    let mut nm_owners = dbus_proxy
        .receive_name_owner_changed_with_args(&[(0, super::NM)])
        .await?;
    let mut daemon = Daemon::watch(conn, IWD).await?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(IWD)?
        .build();
    daemon.subscribe("iwd", rule).await?;
    if owned(conn, super::NM).await || daemon.owner().is_none() {
        return Ok(Next::Again);
    }
    let owner: Owner = Arc::new(Mutex::new(daemon.owner().map(|o| o.to_string())));
    let passwords = Passwords::default();
    let (levels_tx, mut levels) = mpsc::unbounded_channel();
    let mut agents = Agents::serve(conn, passwords.clone(), levels_tx, owner).await?;
    let mut iwd = Iwd::default();
    let mut joins: JoinSet<Result<(), String>> = JoinSet::new();
    let mut joining = Joining::new();
    // `disconnect()`s in flight: the latest is `leave`.
    let mut leaving: JoinSet<Result<(), String>> = JoinSet::new();
    let mut leave: Option<(tokio::task::Id, String)> = None;
    // Failures of joins `leave` stopped, while it is in flight.
    let mut held: Vec<(tokio::task::Id, String, String)> = Vec::new();
    let mut pending = Pending::default();
    let mut addresses = Addresses::default();
    // The networks in range are listed (a reader reads them).
    let mut listing = cx.watched("access_points");
    let mut want_scan = listing;
    let publish = |cx: &mut Cx<Network>, iwd: &Iwd, pending: &Pending, listing: bool| {
        cx.update(|s| {
            *s = iwd.state(listing);
            overlay(pending, s);
        })
    };
    let next = 'body: {
        if let Err(e) = iwd.read(&daemon).await {
            agents.down(true).await;
            return Err(ServiceError(format!("iwd did not answer: {e}")));
        }
        agents.register(&iwd).await;
        if listing && want_scan && scan(conn, &iwd).await.is_some() {
            want_scan = false;
        }
        if !publish(cx, &iwd, &pending, listing) {
            break 'body Next::Stop;
        }
        cx.ready();
        loop {
            let iface = iwd.interface();
            if addresses.sync(&mut pending, cx.watched("address"), iface.as_deref())
                && !publish(cx, &iwd, &pending, listing)
            {
                break 'body Next::Stop;
            }
            tokio::select! {
                ev = daemon.next() => match ev {
                    None => {
                        agents.down(false).await;
                        return Err(ServiceError("the system bus connection ended".into()));
                    }
                    Some(DaemonEvent::Owner) => {
                        if !fail_joins(cx, &mut joining, "iwd stopped") {
                            break 'body Next::Stop;
                        }
                        break 'body Next::Again;
                    }
                    Some(DaemonEvent::Signal(m)) => {
                        let c = iwd.signal(&m);
                        if c.reorder {
                            iwd.reorder(conn).await;
                        }
                        if c.diagnose {
                            iwd.diagnose(conn).await;
                        }
                        // The station comes with the device switched on.
                        agents.register(&iwd).await;
                        if listing && want_scan && scan(conn, &iwd).await.is_some() {
                            want_scan = false;
                        }
                        if (c.changed || c.reorder) && !publish(cx, &iwd, &pending, listing) {
                            break 'body Next::Stop;
                        }
                    }
                },
                Some(o) = nm_owners.next() => {
                    if o.args().ok().is_some_and(|a| a.new_owner().is_some()) {
                        // Its joins are dropped with this body (review
                        // minor 2).
                        if !fail_joins(cx, &mut joining, "NetworkManager took over") {
                            break 'body Next::Stop;
                        }
                        break 'body Next::Again;
                    }
                }
                Some(level) = levels.recv() => {
                    iwd.level = Some(level_strength(level));
                    if !publish(cx, &iwd, &pending, listing) {
                        break 'body Next::Stop;
                    }
                }
                a = addresses.next(iface.as_deref()) => {
                    pending.address = a;
                    if !publish(cx, &iwd, &pending, listing) {
                        break 'body Next::Stop;
                    }
                }
                Some(done) = leaving.join_next_with_id(), if !leaving.is_empty() => {
                    let (id, r) = match done {
                        Ok((id, r)) => (id, r),
                        Err(e) => (e.id(), Err("the disconnect stopped".to_string())),
                    };
                    let Some((_, ssid)) = leave.take_if(|(l, _)| *l == id) else {
                        continue;
                    };
                    pending.disconnect = None;
                    if r.is_ok() {
                        // Left: the joins it stopped that iwd has not
                        // started (still forgetting the old password)
                        // ask nothing more, and are left if iwd joins
                        // them all the same.
                        for j in joining.values_mut().filter(|j| j.stopped_by == Some(id)) {
                            j.dropped = true;
                            j.stop.store(true, Ordering::SeqCst);
                        }
                    }
                    if let Err(error) = r {
                        log::info!("network: not leaving {ssid}: {error}");
                        if !cx.emit(NetworkEvent::Failed((ssid, error))) {
                            break 'body Next::Stop;
                        }
                        // Not left: the joins it stopped go on, and those
                        // that failed meanwhile are reported.
                        if let Some(join) = resume_joins(&mut joining, id).pop() {
                            pending.join = Some(join);
                        }
                        for (_, ssid, error) in held.extract_if(.., |(l, ..)| *l == id) {
                            if !cx.emit(NetworkEvent::Failed((ssid, error))) {
                                break 'body Next::Stop;
                            }
                        }
                    }
                    held.clear();
                    if !publish(cx, &iwd, &pending, listing) {
                        break 'body Next::Stop;
                    }
                }
                Some(done) = joins.join_next_with_id(), if !joins.is_empty() => {
                    let (id, r) = match done {
                        Ok((id, r)) => (id, r),
                        Err(e) => (e.id(), Err("the join stopped".to_string())),
                    };
                    let Some(j) = joining.remove(&id) else {
                        continue;
                    };
                    if let Ok(mut p) = passwords.lock() {
                        p.remove(&j.path);
                    }
                    if !j.cancelled() && pending.join.as_deref() == Some(j.ssid.as_str()) {
                        pending.join = None;
                    }
                    // A join stopped by the disconnect in flight that
                    // failed: reported only if that disconnect fails.
                    if let (Some(by), Err(error)) = (j.stopped_by, &r)
                        && leave.as_ref().is_some_and(|(l, _)| *l == by)
                    {
                        held.push((by, j.ssid.clone(), error.clone()));
                    }
                    // One dropped that iwd joined all the same (its
                    // `Connect` already asked): left again.
                    if j.dropped
                        && r.is_ok()
                        && let Some((station, _)) = iwd.station()
                    {
                        let (conn, station) = (conn.clone(), station.clone());
                        leaving.spawn(async move { leave_station(&conn, &station).await });
                    }
                    if let Some(error) = join_failure(j.cancelled(), r) {
                        log::info!("network: joining {} failed: {error}", j.ssid);
                        if !cx.emit(NetworkEvent::Failed((j.ssid, error))) {
                            break 'body Next::Stop;
                        }
                    }
                    if !publish(cx, &iwd, &pending, listing) {
                        break 'body Next::Stop;
                    }
                }
                m = cx.recv() => match m {
                    None => break 'body Next::Stop,
                    Some(Msg::Watch { field: "access_points", on }) => {
                        listing = on;
                        want_scan = on;
                        if on {
                            iwd.reorder(conn).await;
                            if scan(conn, &iwd).await.is_some() {
                                want_scan = false;
                            }
                        }
                        if !publish(cx, &iwd, &pending, listing) {
                            break 'body Next::Stop;
                        }
                    }
                    Some(Msg::Write(w)) if w.field == "wifi" => {
                        let was = iwd.state(listing).wifi;
                        let on: bool = w.value().unwrap_or(was);
                        let now = match iwd.device().map(|(p, _)| p.clone()) {
                            Some(dev) => {
                                let set: zbus::Result<()> = call(
                                    conn,
                                    &dev,
                                    dbus::PROPERTIES,
                                    "Set",
                                    &(DEVICE, "Powered", Value::from(on)),
                                    dbus::READ_TIMEOUT,
                                )
                                .await;
                                match set {
                                    Ok(()) => on,
                                    Err(e) => {
                                        log::warn!("network: Wi-Fi not switched: {e}");
                                        was
                                    }
                                }
                            }
                            None => was,
                        };
                        if !cx.report(&w, |s| s.wifi = now) {
                            break 'body Next::Stop;
                        }
                    }
                    Some(Msg::Action(NetworkAction::Scan)) => {
                        let failure = match scan(conn, &iwd).await {
                            None => Some("Wi-Fi is off".to_string()),
                            Some(Ok(())) => None,
                            Some(Err(e)) => scan_failure(e),
                        };
                        if let Some(error) = failure {
                            log::info!("network: no scan: {error}");
                            if !cx.emit(NetworkEvent::Failed((String::new(), error))) {
                                break 'body Next::Stop;
                            }
                        }
                    }
                    Some(Msg::Action(NetworkAction::Disconnect)) => {
                        // A repeat while one is in flight (a double
                        // click) is dropped: that one's answer decides
                        // (connectivity-r2-2).
                        if leave.is_some() {
                            log::debug!("network: already disconnecting");
                            continue;
                        }
                        // The network joined or being joined, iwd's own
                        // included. With none named, the shell's joins
                        // iwd has not started are stopped here.
                        let target = iwd.station().map(|(p, _)| p.clone()).zip(iwd.in_use());
                        let Some((station, name)) = target else {
                            if drop_joins(&mut joining) {
                                pending.join = None;
                                if !publish(cx, &iwd, &pending, listing) {
                                    break 'body Next::Stop;
                                }
                            } else if !cx.emit(NetworkEvent::Failed((String::new(), "no Wi-Fi network is joined".into()))) {
                                break 'body Next::Stop;
                            }
                            continue;
                        };
                        // The joins it stops are not reported, unless it
                        // fails.
                        pending.join = None;
                        let conn = conn.clone();
                        let id = leaving
                            .spawn(async move { leave_station(&conn, &station).await })
                            .id();
                        // Those an earlier disconnect stopped stay its.
                        for j in joining.values_mut().filter(|j| !j.cancelled()) {
                            j.stopped_by = Some(id);
                        }
                        leave = Some((id, name.clone()));
                        pending.disconnect = Some(name);
                        if !publish(cx, &iwd, &pending, listing) {
                            break 'body Next::Stop;
                        }
                    }
                    Some(Msg::Action(a)) => {
                        let (item, password) = match a {
                            NetworkAction::Connect { item } => (item, None),
                            NetworkAction::ConnectWith { item, password } => (item, Some(password)),
                            NetworkAction::Disconnect | NetworkAction::Scan => continue,
                        };
                        // One join of a network at a time; a repeat is
                        // dropped.
                        if joining.values().any(|j| j.ssid == item.ssid && !j.cancelled()) {
                            continue;
                        }
                        match iwd.to_join(&item.ssid, password.is_some()) {
                            Err(error) => {
                                log::info!("network: not joining {}: {error}", item.ssid);
                                if !cx.emit(NetworkEvent::Failed((item.ssid, error))) {
                                    break 'body Next::Stop;
                                }
                            }
                            Ok((path, forget)) => {
                                if let Some(password) = password
                                    && let Ok(mut p) = passwords.lock()
                                {
                                    p.insert(path.clone(), password);
                                }
                                let stop = Arc::new(AtomicBool::new(false));
                                let task = joins.spawn(join(conn.clone(), path.clone(), forget, stop.clone()));
                                pending.join = Some(item.ssid.clone());
                                joining.insert(task.id(), Join::new(item.ssid, path, stop));
                                if !publish(cx, &iwd, &pending, listing) {
                                    break 'body Next::Stop;
                                }
                            }
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    };
    agents.down(daemon.owner().is_some()).await;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedValue;

    fn props(pairs: &[(&str, Value<'_>)]) -> Props {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    OwnedValue::try_from(v.try_clone().unwrap()).unwrap(),
                )
            })
            .collect()
    }

    fn path(p: &str) -> Value<'static> {
        Value::from(ObjectPath::try_from(p.to_string()).unwrap())
    }

    /// The owner's wlan0, joined to `Home`, `Cafe` (open) and `Work`
    /// (802.1X, unknown) in range.
    fn wlan0() -> Iwd {
        let dev = "/net/connman/iwd/0/4";
        let net = |name: &str, ty: &str, connected: bool| {
            BTreeMap::from([(
                NETWORK.to_string(),
                props(&[
                    ("Name", Value::from(name)),
                    ("Type", Value::from(ty)),
                    ("Connected", Value::from(connected)),
                    ("Device", path(dev)),
                ]),
            )])
        };
        let mut objects = Objects::new();
        objects.insert(
            dev.to_string(),
            BTreeMap::from([
                (
                    DEVICE.to_string(),
                    props(&[
                        ("Name", Value::from("wlan0")),
                        ("Powered", Value::from(true)),
                        ("Mode", Value::from("station")),
                    ]),
                ),
                (
                    STATION.to_string(),
                    props(&[
                        ("State", Value::from("connected")),
                        ("ConnectedNetwork", path(&format!("{dev}/486f6d65_psk"))),
                        ("Scanning", Value::from(false)),
                    ]),
                ),
            ]),
        );
        objects.insert(format!("{dev}/486f6d65_psk"), net("Home", "psk", true));
        objects.insert(format!("{dev}/43616665_open"), net("Cafe", "open", false));
        objects.insert(format!("{dev}/576f726b_8021x"), net("Work", "8021x", false));
        Iwd {
            objects,
            ordered: vec![
                (format!("{dev}/43616665_open"), quality(-45.0)),
                (format!("{dev}/486f6d65_psk"), quality(-60.0)),
                (format!("{dev}/576f726b_8021x"), quality(-80.0)),
            ],
            ..Iwd::default()
        }
    }

    #[test]
    fn the_station_makes_the_state() {
        let iwd = wlan0();
        let s = iwd.state(false);
        assert!(s.connected && s.wifi);
        assert_eq!(s.ssid.as_deref(), Some("Home"));
        assert!((s.strength - 0.8).abs() < 1e-9);
        assert!(s.access_points.is_empty(), "listed only while scanning");
        let s = iwd.state(true);
        let aps: Vec<(&str, bool, bool)> = s
            .access_points
            .iter()
            .map(|a| (a.ssid.as_str(), a.secure, a.active))
            .collect();
        assert_eq!(
            aps,
            [
                ("Home", true, true),
                ("Cafe", false, false),
                ("Work", false, false)
            ]
        );
    }

    /// (m6-service-gaps, gap 28) iwd's diagnostic words name the
    /// security; without diagnostics the network's `Type` does, and no
    /// frequency is known.
    #[test]
    fn diagnostics_name_the_security() {
        use WifiSecurity::*;
        for (text, want) in [
            ("Open", Some(Open)),
            ("OWE", Some(EnhancedOpen)),
            ("WEP", Some(Wep)),
            ("WPA1-Personal", Some(Wpa)),
            ("WPA-PSK", Some(Wpa)),
            ("WPA2-Personal", Some(Wpa2)),
            ("WPA2-Personal + FT", Some(Wpa2)),
            ("WPA3-Personal", Some(Wpa3)),
            ("WPA2-Enterprise", Some(Enterprise)),
            ("WPA1-Enterprise", Some(Enterprise)),
            ("FILS-SHA256", Some(Enterprise)),
            ("8021X", Some(Enterprise)),
            ("Something new", None),
        ] {
            assert_eq!(security_named(text), want, "{text}");
        }
        let mut iwd = wlan0();
        let s = iwd.state(false);
        assert_eq!((s.frequency, s.security), (None, Some(Wpa2)), "psk");
        iwd.diagnostics = Some(Diagnostics {
            frequency: Some(5180),
            security: Some("WPA3-Personal".into()),
        });
        let s = iwd.state(false);
        assert_eq!((s.frequency, s.security), (Some(5180), Some(Wpa3)));
        assert_eq!(iwd.interface().as_deref(), Some("wlan0"));
        assert_eq!(security_of_type("open"), Some(Open));
        assert_eq!(security_of_type("8021x"), Some(Enterprise));
        assert_eq!(security_of_type("wep"), Some(Wep));
    }

    /// (gap 28) The station's `State` says what is in progress: a join
    /// names its network as `connecting` (not yet `ssid`), a leave names
    /// it as `disconnecting`, from the network named last (joined or
    /// being joined) once the station no longer names it.
    #[test]
    fn the_station_says_what_is_in_progress() {
        let dev = "/net/connman/iwd/0/4";
        // As a `PropertiesChanged` of the station's is applied.
        let set = |iwd: &mut Iwd, state: &str, net: &str| {
            let st = iwd.objects.get_mut(dev).unwrap().get_mut(STATION).unwrap();
            st.insert(
                "State".into(),
                OwnedValue::try_from(Value::from(state)).unwrap(),
            );
            st.insert(
                "ConnectedNetwork".into(),
                OwnedValue::try_from(path(net)).unwrap(),
            );
            iwd.remember();
        };
        let mut iwd = wlan0();
        iwd.remember();
        set(&mut iwd, "disconnecting", "/");
        let s = iwd.state(false);
        assert_eq!(s.disconnecting.as_deref(), Some("Home"), "the last joined");
        assert_eq!((s.connecting, s.ssid), (None, None));
        set(&mut iwd, "connecting", &format!("{dev}/43616665_open"));
        let s = iwd.state(false);
        assert_eq!(s.connecting.as_deref(), Some("Cafe"));
        assert_eq!((s.ssid, s.connected, s.disconnecting), (None, false, None));
        assert_eq!(iwd.in_use().as_deref(), Some("Cafe"));
        assert_eq!(iwd.interface(), None, "not connected");
        // (connectivity-r1-4) A join stopped: iwd names no network, then
        // leaves; the network left is the one being joined, not the one
        // joined before.
        set(&mut iwd, "disconnecting", "/");
        let s = iwd.state(false);
        assert_eq!(s.disconnecting.as_deref(), Some("Cafe"), "the join stopped");
        assert_eq!((s.connecting, s.ssid), (None, None));
        set(&mut iwd, "disconnecting", &format!("{dev}/43616665_open"));
        assert_eq!(iwd.state(false).disconnecting.as_deref(), Some("Cafe"));
        set(&mut iwd, "disconnected", "/");
        let s = iwd.state(false);
        assert_eq!(
            (s.connecting, s.disconnecting, s.security),
            (None, None, None)
        );
        assert_eq!(iwd.in_use(), None);
        // Disconnected, nothing is kept: a leave that names no network
        // later (iwd's own, read afresh) names none rather than an old one.
        set(&mut iwd, "disconnecting", "/");
        assert_eq!(iwd.state(false).disconnecting, None, "not Home nor Cafe");
        // Joined Home, left, then joining Attic (here Work) stopped.
        set(&mut iwd, "connected", &format!("{dev}/486f6d65_psk"));
        set(&mut iwd, "disconnected", "/");
        set(&mut iwd, "connecting", &format!("{dev}/576f726b_8021x"));
        set(&mut iwd, "disconnecting", "/");
        assert_eq!(iwd.state(false).disconnecting.as_deref(), Some("Work"));
    }

    /// (connectivity-r1-6) A disconnect that fails lets the joins it
    /// stopped go on (reported as any join is); one another disconnect
    /// stopped stays stopped.
    #[test]
    fn a_failed_disconnect_resumes_the_joins_it_stopped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _in = rt.enter();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut id = || tasks.spawn(async {}).id();
        let (first, second) = (id(), id());
        let join = |ssid: &str, by| Join {
            stopped_by: by,
            ..Join::new(ssid.into(), format!("/n/{ssid}"), Arc::default())
        };
        let mut joining = Joining::from([
            (id(), join("Cafe", Some(second))),
            (id(), join("Home", Some(first))),
            (id(), join("Work", None)),
        ]);
        assert_eq!(resume_joins(&mut joining, second), ["Cafe"]);
        assert!(joining.values().all(|j| j.ssid == "Home" || !j.cancelled()));
        assert!(joining.values().any(|j| j.ssid == "Home" && j.cancelled()));
        assert_eq!(resume_joins(&mut joining, second), Vec::<String>::new());
    }

    /// (connectivity-r2-1) A `disconnect()` while the station names no
    /// network stops the shell's joins itself: each is cancelled and its
    /// task told to ask iwd nothing more; one an earlier disconnect
    /// stopped stays that one's; with no live join there is nothing to
    /// stop.
    #[test]
    fn with_no_network_named_disconnect_stops_the_joins_itself() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _in = rt.enter();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut id = || tasks.spawn(async {}).id();
        let earlier = id();
        let mut joining = Joining::new();
        assert!(!drop_joins(&mut joining), "no join");
        let attic = Arc::new(AtomicBool::new(false));
        let old = Arc::new(AtomicBool::new(false));
        joining.insert(
            id(),
            Join::new("Attic".into(), "/n/Attic".into(), attic.clone()),
        );
        joining.insert(
            id(),
            Join {
                stopped_by: Some(earlier),
                ..Join::new("Home".into(), "/n/Home".into(), old.clone())
            },
        );
        assert!(drop_joins(&mut joining));
        assert!(joining.values().all(Join::cancelled));
        assert!(attic.load(Ordering::SeqCst), "Attic's task asks no more");
        assert!(
            !old.load(Ordering::SeqCst),
            "Home's stays the earlier one's"
        );
        assert!(!drop_joins(&mut joining), "nothing more to stop");
        assert_eq!(resume_joins(&mut joining, earlier), ["Home"]);
        assert!(
            joining.values().any(|j| j.ssid == "Attic" && j.cancelled()),
            "a dropped join is never resumed"
        );
    }

    /// (gap 28) A join `disconnect()` stopped is no failure: iwd answers
    /// it `Aborted`, which reads "a password is needed".
    #[test]
    fn an_aborted_join_after_disconnect_is_no_failure() {
        let aborted = Err("a password is needed (none was given, or it was wrong)".to_string());
        assert_eq!(join_failure(true, aborted.clone()), None);
        assert!(join_failure(false, aborted).is_some());
        assert_eq!(join_failure(false, Ok(())), None);
        assert_eq!(join_failure(true, Ok(())), None);
    }

    #[test]
    fn signal_levels_are_strengths() {
        assert_eq!(level_strength(0), 1.0);
        assert!((level_strength(2) - quality(-57.5)).abs() < 1e-9);
        assert!((level_strength(9) - quality(-92.5)).abs() < 1e-9);
        assert!((level_strength(200) - quality(-92.5)).abs() < 1e-9);
        let mut iwd = wlan0();
        iwd.level = Some(level_strength(5));
        assert!((iwd.state(false).strength - quality(-72.5)).abs() < 1e-9);
    }

    #[test]
    fn joins_are_refused_as_networkmanager_refuses_them() {
        let iwd = wlan0();
        assert_eq!(
            iwd.to_join("Home", true),
            Ok(("/net/connman/iwd/0/4/486f6d65_psk".to_string(), None))
        );
        assert!(iwd.to_join("Cafe", false).is_ok());
        assert!(
            iwd.to_join("Cafe", true)
                .unwrap_err()
                .contains("open network")
        );
        assert!(
            iwd.to_join("Work", false)
                .unwrap_err()
                .contains("no saved profile")
        );
        assert!(
            iwd.to_join("Gone", false)
                .unwrap_err()
                .contains("not in range")
        );
    }

    #[test]
    fn the_device_switched_off_is_offline() {
        let mut iwd = wlan0();
        let dev = "/net/connman/iwd/0/4";
        let o = iwd.objects.get_mut(dev).unwrap();
        o.remove(STATION);
        o.get_mut(DEVICE)
            .unwrap()
            .insert("Powered".into(), OwnedValue::from(false));
        let s = iwd.state(true);
        assert!(!s.connected && !s.wifi);
        assert_eq!((s.ssid, s.strength), (None, 0.0));
    }

    /// Review minor 6: a password for a network iwd knows comes with the
    /// known network to forget first; without one, or for an unknown
    /// network, nothing is forgotten.
    #[test]
    fn a_new_password_for_a_known_network_forgets_it_first() {
        let mut iwd = wlan0();
        let home = "/net/connman/iwd/0/4/486f6d65_psk";
        iwd.objects
            .get_mut(home)
            .unwrap()
            .get_mut(NETWORK)
            .unwrap()
            .insert(
                "KnownNetwork".into(),
                OwnedValue::try_from(path("/net/connman/iwd/486f6d65_psk")).unwrap(),
            );
        assert_eq!(
            iwd.to_join("Home", true),
            Ok((
                home.to_string(),
                Some("/net/connman/iwd/486f6d65_psk".to_string())
            ))
        );
        assert_eq!(iwd.to_join("Home", false), Ok((home.to_string(), None)));
    }

    /// Review minor 4: the station going (the radio off) or coming back
    /// drops the level iwd told, which may be another network's.
    #[test]
    fn a_station_coming_or_going_drops_the_level() {
        let dev = "/net/connman/iwd/0/4";
        let removed = zbus::Message::signal("/", OBJECT_MANAGER, "InterfacesRemoved")
            .unwrap()
            .build(&(
                ObjectPath::try_from(dev).unwrap(),
                vec![STATION.to_string()],
            ))
            .unwrap();
        let mut iwd = wlan0();
        iwd.level = Some(level_strength(2));
        iwd.signal(&removed);
        assert_eq!(iwd.level, None);
        let station: HashMap<String, Props> = HashMap::from([(
            STATION.to_string(),
            props(&[("State", Value::from("connected"))]),
        )]);
        let added = zbus::Message::signal("/", OBJECT_MANAGER, "InterfacesAdded")
            .unwrap()
            .build(&(ObjectPath::try_from(dev).unwrap(), station))
            .unwrap();
        iwd.level = Some(level_strength(2));
        iwd.signal(&added);
        assert_eq!(iwd.level, None);
    }

    /// Review minor 5: a call to iwd while iwd is not running is refused
    /// at once, never D-Bus-activated, even where a service file could
    /// start it (here one whose process never takes the name, so an
    /// activation would hold the call until the limit).
    #[test]
    fn iwd_is_never_started_by_a_call() {
        let Some(bus) = crate::testing::PrivateBus::start_activating(&[("logind", true, IWD)])
        else {
            return;
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let conn = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let started = std::time::Instant::now();
            let r: zbus::Result<()> = call(
                &conn,
                MANAGER,
                AGENT_MANAGER,
                "RegisterAgent",
                &(ObjectPath::try_from(AGENT_PATH).unwrap(),),
                Duration::from_secs(8),
            )
            .await;
            assert!(r.is_err());
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "the call waited {:?}: iwd was being started",
                started.elapsed()
            );
        });
    }
}

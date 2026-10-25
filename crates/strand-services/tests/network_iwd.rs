//! `network` over iwd where NetworkManager is absent (owner-testing gaps
//! 15): a small zbus iwd on a private bus (python-dbusmock has no iwd
//! template), with the owner's `wlan0`. The joined network and its
//! strength (from the signal level agent), the networks in range scanned
//! while a visible reader reads them, joining through the passphrase
//! agent, the radio, and NetworkManager appearing and going.

mod support;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::network::{AccessPoint, NM, iwd};
use strand_services::{Data, Store, ToData, network};
use support::*;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const DEV: &str = "/net/connman/iwd/0/4";
const HOME: &str = "/net/connman/iwd/0/4/486f6d65_psk";
const ATTIC: &str = "/net/connman/iwd/0/4/4174746963_psk";
const LOFT: &str = "/net/connman/iwd/0/4/4c6f6674_psk";
const CAFE: &str = "/net/connman/iwd/0/4/43616665_open";

#[derive(Default)]
struct World {
    powered: bool,
    /// The network joined.
    connected: Option<String>,
    /// The networks iwd knows (a passphrase that worked).
    known: HashSet<String>,
    /// The passphrase agent registered: its sender and path.
    agent: Option<(String, String)>,
    /// The signal level agent registered: sender, path, levels.
    levels: Option<(String, String, Vec<i16>)>,
    calls: Vec<String>,
    /// The networks in range, strongest first: path, hundredths of a dBm.
    ordered: Vec<(String, i16)>,
    /// `Connect` waits while this is set (a join in flight).
    stall: bool,
    /// `RegisterSignalLevelAgent` is refused (another tool holds it).
    refuse_levels: bool,
    /// How often `RegisterSignalLevelAgent` was asked.
    level_registrations: u32,
    /// The device's interface name ("wlan0" when empty).
    iface: String,
    /// The station scans.
    scanning: bool,
    /// A `Scan` keeps scanning until the test ends it ([`end_scan`]).
    hold_scan: bool,
    /// The station's `State` while a join or a leave is in progress.
    state: Option<String>,
    /// The network being joined (iwd names it as `ConnectedNetwork`).
    joining: Option<String>,
    /// `StationDiagnostic` is served, saying (MHz, security).
    diagnostics: Option<(u32, String)>,
    /// `Disconnect` waits while this is set (a leave in flight).
    stall_disconnect: bool,
    /// A `Disconnect` came during a join: the join is aborted.
    abort: bool,
    /// `Disconnect` is refused (as a policy that denies it would).
    refuse_disconnect: bool,
    /// A `Disconnect` is in progress: another is refused `Busy`, as iwd
    /// refuses one while the station is disconnecting.
    leaving: bool,
    /// `KnownNetwork.Forget` waits while this is set.
    stall_forget: bool,
    /// How often `GetOrderedNetworks` was asked.
    ordered_reads: u32,
}

type W = Arc<Mutex<World>>;

fn path(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p.to_string()).unwrap()
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "net.connman.iwd")]
enum IwdError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Aborted(String),
    Busy(String),
    NotConnected(String),
}

struct Device(W);

#[zbus::interface(name = "net.connman.iwd.Device")]
impl Device {
    #[zbus(property)]
    fn name(&self) -> String {
        let w = self.0.lock().unwrap();
        if w.iface.is_empty() {
            "wlan0".into()
        } else {
            w.iface.clone()
        }
    }

    #[zbus(property)]
    fn mode(&self) -> String {
        "station".into()
    }

    #[zbus(property)]
    fn powered(&self) -> bool {
        self.0.lock().unwrap().powered
    }

    #[zbus(property)]
    fn set_powered(&mut self, on: bool) {
        let mut w = self.0.lock().unwrap();
        w.powered = on;
        w.calls.push(format!("Powered {on}"));
    }
}

struct Station(W);

#[zbus::interface(name = "net.connman.iwd.Station")]
impl Station {
    #[zbus(property)]
    fn state(&self) -> String {
        let w = self.0.lock().unwrap();
        if let Some(s) = &w.state {
            return s.clone();
        }
        match w.connected {
            Some(_) => "connected".into(),
            None => "disconnected".into(),
        }
    }

    #[zbus(property)]
    fn connected_network(&self) -> OwnedObjectPath {
        let w = self.0.lock().unwrap();
        path(
            w.joining
                .as_deref()
                .or(w.connected.as_deref())
                .unwrap_or("/"),
        )
    }

    #[zbus(property)]
    fn scanning(&self) -> bool {
        self.0.lock().unwrap().scanning
    }

    /// Leaves the network joined, or aborts the join in flight; as iwd,
    /// refused `NotConnected` with neither, `Busy` while leaving.
    async fn disconnect(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> Result<(), IwdError> {
        let aborting = {
            let mut w = self.0.lock().unwrap();
            w.calls.push("Disconnect".into());
            if w.refuse_disconnect {
                return Err(IwdError::Failed("Not authorized".into()));
            }
            if w.leaving {
                return Err(IwdError::Busy("Operation already in progress".into()));
            }
            if w.joining.is_none() && w.connected.is_none() {
                return Err(IwdError::NotConnected("Not connected".into()));
            }
            w.leaving = true;
            if w.joining.is_some() {
                w.abort = true;
                true
            } else {
                w.state = Some("disconnecting".into());
                false
            }
        };
        if !aborting {
            self.state_changed(&e).await?;
        }
        while self.0.lock().unwrap().stall_disconnect {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if aborting {
            self.0.lock().unwrap().leaving = false;
            return Ok(());
        }
        let old = {
            let mut w = self.0.lock().unwrap();
            w.state = None;
            w.leaving = false;
            w.connected.take()
        };
        self.state_changed(&e).await?;
        self.connected_network_changed(&e).await?;
        if let Some(p) = old {
            let n = server.interface::<_, Net>(p.as_str()).await?;
            n.get().await.connected_changed(n.signal_emitter()).await?;
        }
        Ok(())
    }

    /// A scan: Cafe comes into range.
    async fn scan(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) {
        let first = {
            let mut w = self.0.lock().unwrap();
            w.calls.push("Scan".into());
            let first = !w.ordered.iter().any(|(p, _)| p == CAFE);
            if first {
                w.ordered.insert(0, (CAFE.into(), -4500));
            }
            first
        };
        if first {
            server
                .at(CAFE, Net::new(&self.0, CAFE, "Cafe", "open"))
                .await
                .unwrap();
        }
        let hold = {
            let mut w = self.0.lock().unwrap();
            w.scanning = true;
            w.hold_scan
        };
        self.scanning_changed(&e).await.unwrap();
        if !hold {
            self.0.lock().unwrap().scanning = false;
            self.scanning_changed(&e).await.unwrap();
        }
    }

    fn get_ordered_networks(&self) -> Vec<(OwnedObjectPath, i16)> {
        let mut w = self.0.lock().unwrap();
        w.ordered_reads += 1;
        w.ordered.iter().map(|(p, s)| (path(p), *s)).collect()
    }

    fn register_signal_level_agent(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        agent: ObjectPath<'_>,
        levels: Vec<i16>,
    ) -> Result<(), IwdError> {
        let mut w = self.0.lock().unwrap();
        w.level_registrations += 1;
        if w.refuse_levels {
            return Err(IwdError::Failed("Operation failed".into()));
        }
        let sender = h.sender().unwrap().to_string();
        w.levels = Some((sender, agent.to_string(), levels));
        Ok(())
    }

    fn unregister_signal_level_agent(&self, _agent: ObjectPath<'_>) {
        self.0.lock().unwrap().levels = None;
    }
}

struct Net {
    w: W,
    path: String,
    name: String,
    kind: String,
}

impl Net {
    fn new(w: &W, path: &str, name: &str, kind: &str) -> Net {
        Net {
            w: w.clone(),
            path: path.into(),
            name: name.into(),
            kind: kind.into(),
        }
    }
}

#[zbus::interface(name = "net.connman.iwd.Network")]
impl Net {
    #[zbus(property)]
    fn name(&self) -> String {
        self.name.clone()
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        self.kind.clone()
    }

    #[zbus(property)]
    fn connected(&self) -> bool {
        self.w.lock().unwrap().connected.as_deref() == Some(self.path.as_str())
    }

    #[zbus(property)]
    fn device(&self) -> OwnedObjectPath {
        path(DEV)
    }

    #[zbus(property)]
    fn known_network(&self) -> OwnedObjectPath {
        let known = self.w.lock().unwrap().known.contains(&self.path);
        if known {
            path(&format!("{}_known", self.path))
        } else {
            path("/")
        }
    }

    /// Joins: a personal network iwd does not know asks the agent for
    /// its passphrase (`hunter22` is right).
    async fn connect(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> Result<(), IwdError> {
        self.w
            .lock()
            .unwrap()
            .calls
            .push(format!("Connect {}", self.name));
        let station = server.interface::<_, Station>(DEV).await?;
        let stalled = self.w.lock().unwrap().stall;
        if stalled {
            // iwd names the network from the start of the join.
            {
                let mut w = self.w.lock().unwrap();
                w.state = Some("connecting".into());
                w.joining = Some(self.path.clone());
            }
            let e = station.signal_emitter();
            station.get().await.state_changed(e).await?;
            station.get().await.connected_network_changed(e).await?;
        }
        loop {
            let (stall, abort) = {
                let w = self.w.lock().unwrap();
                (w.stall, w.abort)
            };
            if abort {
                {
                    let mut w = self.w.lock().unwrap();
                    w.abort = false;
                    w.state = None;
                    w.joining = None;
                }
                let e = station.signal_emitter();
                station.get().await.state_changed(e).await?;
                station.get().await.connected_network_changed(e).await?;
                return Err(IwdError::Aborted("Operation aborted".into()));
            }
            if !stall {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        {
            let mut w = self.w.lock().unwrap();
            w.state = None;
            w.joining = None;
        }
        if stalled {
            // Back to the network joined before, until this one is.
            let e = station.signal_emitter();
            station.get().await.state_changed(e).await?;
            station.get().await.connected_network_changed(e).await?;
        }
        let (ask, agent) = {
            let w = self.w.lock().unwrap();
            (
                self.kind == "psk" && !w.known.contains(&self.path),
                w.agent.clone(),
            )
        };
        if ask {
            let Some((sender, agent)) = agent else {
                return Err(IwdError::Aborted("no agent".into()));
            };
            let reply = conn
                .call_method(
                    Some(sender.as_str()),
                    agent.as_str(),
                    Some("net.connman.iwd.Agent"),
                    "RequestPassphrase",
                    &(ObjectPath::try_from(self.path.as_str()).unwrap(),),
                )
                .await
                .map_err(|_| IwdError::Aborted("Operation aborted".into()))?;
            let passphrase: String = reply.body().deserialize()?;
            if passphrase != "hunter22" {
                return Err(IwdError::Failed("Operation failed".into()));
            }
            self.w.lock().unwrap().known.insert(self.path.clone());
        }
        let old = self.w.lock().unwrap().connected.replace(self.path.clone());
        let e = station.signal_emitter();
        station.get().await.state_changed(e).await?;
        station.get().await.connected_network_changed(e).await?;
        for p in old.into_iter().chain([self.path.clone()]) {
            let n = server.interface::<_, Net>(p.as_str()).await?;
            n.get().await.connected_changed(n.signal_emitter()).await?;
        }
        Ok(())
    }
}

/// A network iwd knows: forgetting it drops its passphrase.
struct Known(W, String, String);

#[zbus::interface(name = "net.connman.iwd.KnownNetwork")]
impl Known {
    async fn forget(&self) {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(format!("Forget {}", self.2));
        while self.0.lock().unwrap().stall_forget {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.0.lock().unwrap().known.remove(&self.1);
    }
}

struct AgentManager(W);

#[zbus::interface(name = "net.connman.iwd.AgentManager")]
impl AgentManager {
    fn register_agent(&self, #[zbus(header)] h: zbus::message::Header<'_>, agent: ObjectPath<'_>) {
        let sender = h.sender().unwrap().to_string();
        self.0.lock().unwrap().agent = Some((sender, agent.to_string()));
    }

    fn unregister_agent(&self, _agent: ObjectPath<'_>) {
        self.0.lock().unwrap().agent = None;
    }
}

/// `net.connman.iwd.StationDiagnostic`: the joined network's frequency
/// and security.
struct Diagnostic(W);

#[zbus::interface(name = "net.connman.iwd.StationDiagnostic")]
impl Diagnostic {
    fn get_diagnostics(&self) -> std::collections::HashMap<String, zbus::zvariant::OwnedValue> {
        let w = self.0.lock().unwrap();
        let (freq, security) = w.diagnostics.clone().unwrap_or_default();
        std::collections::HashMap::from([
            (
                "Frequency".to_string(),
                zbus::zvariant::OwnedValue::from(freq),
            ),
            (
                "Security".to_string(),
                zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from(security))
                    .unwrap(),
            ),
        ])
    }
}

/// The fake's scan ends: `Scanning` falls.
fn end_scan(tokio: &tokio::runtime::Runtime, conn: &zbus::Connection, w: &W) {
    w.lock().unwrap().scanning = false;
    tokio.block_on(async {
        let st = conn
            .object_server()
            .interface::<_, Station>(DEV)
            .await
            .unwrap();
        st.get()
            .await
            .scanning_changed(st.signal_emitter())
            .await
            .unwrap();
    });
}

/// iwd with `wlan0` joined to Home (known); Attic and Loft (personal,
/// unknown) in range; its diagnostics served when the world has some.
fn fake_iwd(tokio: &tokio::runtime::Runtime, address: &str, w: &W) -> zbus::Connection {
    let diagnostics = w.lock().unwrap().diagnostics.is_some();
    tokio.block_on(async {
        let mut b = zbus::connection::Builder::address(address)
            .unwrap()
            .name(iwd::IWD)
            .unwrap()
            .serve_at("/", zbus::fdo::ObjectManager)
            .unwrap()
            .serve_at(iwd::MANAGER, AgentManager(w.clone()))
            .unwrap()
            .serve_at(DEV, Device(w.clone()))
            .unwrap();
        if diagnostics {
            b = b.serve_at(DEV, Diagnostic(w.clone())).unwrap();
        }
        b.serve_at(DEV, Station(w.clone()))
            .unwrap()
            .serve_at(HOME, Net::new(w, HOME, "Home", "psk"))
            .unwrap()
            .serve_at(
                format!("{HOME}_known"),
                Known(w.clone(), HOME.into(), "Home".into()),
            )
            .unwrap()
            .serve_at(ATTIC, Net::new(w, ATTIC, "Attic", "psk"))
            .unwrap()
            .serve_at(LOFT, Net::new(w, LOFT, "Loft", "psk"))
            .unwrap()
            .build()
            .await
            .unwrap()
    })
}

/// The failures `failed` has reported, as (ssid, error).
fn failures_of(b: &strand_services::Builtin) -> Arc<Mutex<Vec<(String, String)>>> {
    let failures = Arc::new(Mutex::new(Vec::new()));
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

#[test]
fn network_follows_iwd_where_networkmanager_is_absent() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w: W = Arc::new(Mutex::new(World {
        powered: true,
        connected: Some(HOME.into()),
        known: HashSet::from([HOME.to_string()]),
        ordered: vec![
            (HOME.into(), -6000),
            (ATTIC.into(), -7000),
            (LOFT.into(), -8500),
        ],
        ..World::default()
    }));
    let iwd_conn = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.connected.get_untracked(&rt), Ok(true));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(Some("Home".into())));
    assert_eq!(cells.wifi.get_untracked(&rt), Ok(true));
    assert_eq!(cells.available.get_untracked(&rt), Ok(true), "iwd answers");
    // -60 dBm, from `GetOrderedNetworks`.
    assert_eq!(cells.strength.get_untracked(&rt), Ok(iwd::quality(-60.0)));

    // The signal falls: iwd tells the level agent, nothing is polled.
    let (sender, agent, levels) = w.lock().unwrap().levels.clone().unwrap();
    assert_eq!(levels, iwd::LEVELS);
    tokio
        .block_on(iwd_conn.call_method(
            Some(sender.as_str()),
            agent.as_str(),
            Some("net.connman.iwd.SignalLevelAgent"),
            "Changed",
            &(ObjectPath::try_from(DEV).unwrap(), 5u8),
        ))
        .unwrap();
    until(&rt, &s, "the new level", || {
        cells.strength.get_untracked(&rt) == Ok(iwd::level_strength(5))
    });
    // Review minor 7: another peer on the bus is not iwd.
    let other = connect(&tokio, &bus.address);
    tokio
        .block_on(other.call_method(
            Some(sender.as_str()),
            agent.as_str(),
            Some("net.connman.iwd.SignalLevelAgent"),
            "Changed",
            &(ObjectPath::try_from(DEV).unwrap(), 0u8),
        ))
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    assert_eq!(
        cells.strength.get_untracked(&rt),
        Ok(iwd::level_strength(5)),
        "a level from another peer"
    );
    assert!(
        !w.lock().unwrap().calls.contains(&"Scan".to_string()),
        "no scan while nobody reads the networks"
    );

    // A network menu opens: a scan, and Cafe comes into range.
    let field = network::Network::FIELDS
        .iter()
        .position(|f| f.name == "access_points")
        .unwrap();
    b.network.acquire_field(field);
    let names = || -> Vec<(String, bool, bool)> {
        cells
            .access_points
            .get_untracked(&rt)
            .unwrap()
            .items()
            .iter()
            .map(|(_, a)| (a.ssid.clone(), a.secure, a.active))
            .collect()
    };
    let want = |v: &[(&str, bool, bool)]| -> Vec<(String, bool, bool)> {
        v.iter().map(|(n, s, a)| (n.to_string(), *s, *a)).collect()
    };
    until(&rt, &s, "the networks in range", || {
        names()
            == want(&[
                ("Home", true, true),
                ("Cafe", false, false),
                ("Attic", true, false),
                ("Loft", true, false),
            ])
    });
    assert!(w.lock().unwrap().calls.contains(&"Scan".to_string()));

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

    // Attic with its password: iwd asks the agent, and it is joined.
    // While the join is in flight, the agent answers iwd alone (review
    // minor 7).
    w.lock().unwrap().stall = true;
    join("Attic", Some("hunter22"));
    let in_flight = || {
        w.lock()
            .unwrap()
            .calls
            .contains(&"Connect Attic".to_string())
    };
    until(&rt, &s, "the Attic join in flight", in_flight);
    let (agent_owner, agent_path) = w.lock().unwrap().agent.clone().unwrap();
    let ask = |conn: &zbus::Connection| {
        tokio.block_on(conn.call_method(
            Some(agent_owner.as_str()),
            agent_path.as_str(),
            Some("net.connman.iwd.Agent"),
            "RequestPassphrase",
            &(ObjectPath::try_from(ATTIC).unwrap(),),
        ))
    };
    assert!(ask(&other).is_err(), "another peer gets no password");
    let given: String = ask(&iwd_conn).unwrap().body().deserialize().unwrap();
    assert_eq!(given, "hunter22");
    w.lock().unwrap().stall = false;
    until(&rt, &s, "Attic joined", || {
        cells.ssid.get_untracked(&rt) == Ok(Some("Attic".into()))
    });
    assert!(w.lock().unwrap().known.contains(ATTIC));
    until(&rt, &s, "Attic active in the list", || {
        names().first() == Some(&("Attic".to_string(), true, true))
    });
    // Loft with a wrong password, then with none: each fails, saying why.
    join("Loft", Some("nope"));
    assert_eq!(
        failed(1),
        ("Loft".to_string(), "Operation failed".to_string())
    );
    join("Loft", None);
    let (ssid, error) = failed(2);
    assert_eq!(ssid, "Loft");
    assert!(error.contains("password is needed"), "{error}");
    // Cafe is open: a password is refused before iwd is asked.
    join("Cafe", Some("x"));
    let (ssid, error) = failed(3);
    assert_eq!(ssid, "Cafe");
    assert!(error.contains("open network"), "{error}");
    assert!(
        !w.lock()
            .unwrap()
            .calls
            .contains(&"Connect Cafe".to_string())
    );

    // Review minor 6: a password for Home, which iwd knows, replaces the
    // kept one: Home is forgotten, then joined asking the agent.
    join("Home", Some("hunter22"));
    until(&rt, &s, "Home joined again", || {
        cells.ssid.get_untracked(&rt) == Ok(Some("Home".into()))
    });
    {
        let w = w.lock().unwrap();
        let forget = w.calls.iter().position(|c| c == "Forget Home");
        let connect = w.calls.iter().rposition(|c| c == "Connect Home");
        assert!(forget.is_some() && forget < connect, "{:?}", w.calls);
        assert!(w.known.contains(HOME), "the new passphrase is kept");
    }
    b.network.release_field(field);

    // The radio is switched off from the shell: the device's `Powered`.
    let wifi = network::Network::FIELDS
        .iter()
        .position(|f| f.name == "wifi")
        .unwrap();
    b.network
        .dynamic()
        .write(&rt, wifi, &[], Data::Bool(false))
        .unwrap();
    until(&rt, &s, "the radio off", || {
        !w.lock().unwrap().powered && cells.wifi.get_untracked(&rt) == Ok(false)
    });

    // NetworkManager appears while Loft is being joined: the network is
    // its now (this one has no objects: offline), the agents are taken
    // down, and the join is reported failed (review minor 2).
    w.lock().unwrap().stall = true;
    let loft_joins = |w: &W| {
        w.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| *c == "Connect Loft")
            .count()
    };
    let before = loft_joins(&w);
    join("Loft", Some("hunter22"));
    until(&rt, &s, "the Loft join in flight", || {
        loft_joins(&w) > before
    });
    let nm = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(NM)
            .unwrap()
            // Its reads answered at once (no such interface).
            .serve_at("/", zbus::fdo::ObjectManager)
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    assert_eq!(
        failed(4),
        ("Loft".to_string(), "NetworkManager took over".to_string())
    );
    w.lock().unwrap().stall = false;
    until(&rt, &s, "NetworkManager's", || {
        cells.ssid.get_untracked(&rt) == Ok(None) && cells.connected.get_untracked(&rt) == Ok(false)
    });
    until(&rt, &s, "the agents unregistered", || {
        let w = w.lock().unwrap();
        w.agent.is_none() && w.levels.is_none()
    });
    // It goes: iwd's again.
    drop(nm);
    until(&rt, &s, "iwd's again", || {
        cells.ssid.get_untracked(&rt) == Ok(Some("Home".into()))
    });
    assert!(w.lock().unwrap().agent.is_some());
    assert_eq!(b.network.starts(), 1, "one body throughout");
    s.shutdown();
}

/// Review minor 3: a signal level agent iwd refuses (another tool holds
/// the station's one) is asked once, not again on every signal.
#[test]
fn a_refused_signal_agent_is_asked_once() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w: W = Arc::new(Mutex::new(World {
        powered: true,
        connected: Some(HOME.into()),
        known: HashSet::from([HOME.to_string()]),
        ordered: vec![(HOME.into(), -6000)],
        refuse_levels: true,
        ..World::default()
    }));
    let iwd_conn = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(Some("Home".into())));
    // Signals from iwd: the radio toggled three times.
    for on in [false, true, false] {
        w.lock().unwrap().powered = on;
        tokio.block_on(async {
            let d = iwd_conn
                .object_server()
                .interface::<_, Device>(DEV)
                .await
                .unwrap();
            d.get()
                .await
                .powered_changed(d.signal_emitter())
                .await
                .unwrap();
        });
        until(&rt, &s, "the radio", || {
            cells.wifi.get_untracked(&rt) == Ok(on)
        });
    }
    assert_eq!(w.lock().unwrap().level_registrations, 1);
    s.shutdown();
}

fn field(name: &str) -> usize {
    network::Network::FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap()
}

/// `network.disconnect()` or `network.scan()`.
fn act(rt: &Runtime, b: &strand_services::Builtin, name: &str) {
    b.network
        .dynamic()
        .action(rt, name, None, &[], Default::default())
        .unwrap();
}

/// `ap.connect()` of `ssid`.
fn join(rt: &Runtime, b: &strand_services::Builtin, ssid: &str) {
    let item = AccessPoint {
        ssid: ssid.into(),
        strength: 0.5,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(
            rt,
            "connect",
            Some(&item.to_data()),
            &[],
            Default::default(),
        )
        .unwrap();
}

/// Home joined on `iface`, its diagnostics served when given.
fn home_world(iface: &str, diagnostics: Option<(u32, &str)>) -> W {
    Arc::new(Mutex::new(World {
        powered: true,
        connected: Some(HOME.into()),
        known: HashSet::from([HOME.to_string(), ATTIC.to_string()]),
        ordered: vec![(HOME.into(), -6000), (ATTIC.into(), -7000)],
        iface: iface.into(),
        diagnostics: diagnostics.map(|(f, s)| (f, s.to_string())),
        ..World::default()
    }))
}

/// (m6-service-gaps, gap 28) The joined network's frequency and security
/// from iwd's `StationDiagnostic`, else its security from the network's
/// `Type` and no frequency; its address from the kernel while a reader
/// reads it (the device's interface here is `lo`).
#[test]
fn iwd_tells_the_joined_networks_details() {
    use strand_services::network::WifiSecurity;
    for (diagnostics, frequency, security) in [
        (
            Some((5180, "WPA3-Personal")),
            Some(5180),
            WifiSecurity::Wpa3,
        ),
        (None, None, WifiSecurity::Wpa2),
    ] {
        let Some(bus) = strand_services::testing::PrivateBus::start() else {
            return;
        };
        let tokio = tokio();
        let w = home_world("lo", diagnostics);
        let _iwd = fake_iwd(&tokio, &bus.address, &w);
        let rt = Runtime::new();
        let (s, b) = services(&rt, bus.buses());
        let cells = b.network.cells();
        b.network.acquire(&rt);
        assert!(s.wait_ready(&rt, Duration::from_secs(10)));
        assert_eq!(cells.ssid.get_untracked(&rt), Ok(Some("Home".into())));
        assert_eq!(cells.frequency.get_untracked(&rt), Ok(frequency));
        assert_eq!(cells.security.get_untracked(&rt), Ok(Some(security)));
        assert_eq!(cells.address.get_untracked(&rt), Ok(None), "not read");
        b.network.acquire_field(field("address"));
        until(&rt, &s, "the address", || {
            cells.address.get_untracked(&rt) == Ok(Some("127.0.0.1".into()))
        });
        s.shutdown();
    }
}

/// (gap 28) A join in flight is `connecting` (the shell's at once, then
/// iwd's), a leave `disconnecting`; once left nothing of the network is
/// told. A disconnect during a join stops it with no failure (iwd's
/// `Aborted`); with nothing joined it fails.
#[test]
fn iwd_says_what_it_is_joining_and_leaving_and_disconnects() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = home_world("lo", Some((2412, "WPA2-Personal")));
    w.lock().unwrap().connected = None;
    let _iwd = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    b.network.acquire_field(field("address"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(None));
    let calls = |what: &str| {
        w.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| *c == what)
            .count()
    };

    // A join, held by iwd.
    w.lock().unwrap().stall = true;
    join(&rt, &b, "Attic");
    until(&rt, &s, "Attic being joined", || {
        cells.connecting.get_untracked(&rt) == Ok(Some("Attic".into()))
            && calls("Connect Attic") == 1
            && w.lock().unwrap().joining.is_some()
    });
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(None));
    w.lock().unwrap().stall = false;
    until(&rt, &s, "Attic joined", || {
        cells.ssid.get_untracked(&rt) == Ok(Some("Attic".into()))
            && cells.connecting.get_untracked(&rt) == Ok(None)
            && cells.address.get_untracked(&rt) == Ok(Some("127.0.0.1".into()))
    });
    assert_eq!(cells.frequency.get_untracked(&rt), Ok(Some(2412)));

    // Leaving it, held by iwd.
    w.lock().unwrap().stall_disconnect = true;
    act(&rt, &b, "disconnect");
    until(&rt, &s, "Attic being left", || {
        cells.disconnecting.get_untracked(&rt) == Ok(Some("Attic".into()))
            && w.lock().unwrap().state.as_deref() == Some("disconnecting")
    });
    w.lock().unwrap().stall_disconnect = false;
    until(&rt, &s, "Attic left", || {
        cells.ssid.get_untracked(&rt) == Ok(None)
            && cells.disconnecting.get_untracked(&rt) == Ok(None)
            && cells.connected.get_untracked(&rt) == Ok(false)
    });
    assert_eq!(cells.address.get_untracked(&rt), Ok(None));
    assert_eq!(cells.frequency.get_untracked(&rt), Ok(None));
    assert_eq!(cells.security.get_untracked(&rt), Ok(None));

    // A disconnect during a join stops it: no failure.
    w.lock().unwrap().stall = true;
    join(&rt, &b, "Attic");
    until(&rt, &s, "the second join in flight", || {
        calls("Connect Attic") == 2 && w.lock().unwrap().joining.is_some()
    });
    act(&rt, &b, "disconnect");
    until(&rt, &s, "the join stopped", || {
        calls("Disconnect") == 2
            && w.lock().unwrap().joining.is_none()
            && cells.connecting.get_untracked(&rt) == Ok(None)
            && cells.disconnecting.get_untracked(&rt) == Ok(None)
    });
    w.lock().unwrap().stall = false;
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(None));

    // Nothing joined: it fails, naming no network.
    act(&rt, &b, "disconnect");
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        (String::new(), "no Wi-Fi network is joined".to_string())
    );
    assert_eq!(calls("Disconnect"), 2);
    s.shutdown();
}

/// (gap 28) `network.scan()` over iwd: `Scan`, `scanning` while the
/// station's `Scanning` says so, the networks read again when it ends;
/// with the radio off (no station) it fails.
#[test]
fn an_explicit_scan_on_iwd_raises_scanning() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = home_world("wlan0", None);
    w.lock().unwrap().hold_scan = true;
    let iwd_conn = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.scanning.get_untracked(&rt), Ok(false));
    act(&rt, &b, "scan");
    until(&rt, &s, "scanning", || {
        cells.scanning.get_untracked(&rt) == Ok(true)
    });
    assert!(w.lock().unwrap().calls.contains(&"Scan".to_string()));
    let reads = w.lock().unwrap().ordered_reads;
    end_scan(&tokio, &iwd_conn, &w);
    until(&rt, &s, "the scan's end", || {
        cells.scanning.get_untracked(&rt) == Ok(false)
    });
    until(&rt, &s, "the networks read again", || {
        w.lock().unwrap().ordered_reads > reads
    });
    // The radio off: iwd takes the station away.
    tokio.block_on(async {
        iwd_conn
            .object_server()
            .remove::<Station, _>(DEV)
            .await
            .unwrap();
    });
    until(&rt, &s, "offline", || {
        cells.connected.get_untracked(&rt) == Ok(false)
    });
    act(&rt, &b, "scan");
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        (String::new(), "Wi-Fi is off".to_string())
    );
    s.shutdown();
}

/// (gap 28, connectivity-r1-6) A disconnect iwd refuses leaves the join it
/// stopped going on: that join is followed again, and its failure is
/// reported as any join's is (it was dropped as stopped before).
#[test]
fn a_join_a_refused_disconnect_stopped_is_still_reported() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = home_world("wlan0", None);
    w.lock().unwrap().connected = None;
    let _iwd = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));

    // Loft (unknown) joined with no password, held by iwd.
    w.lock().unwrap().stall = true;
    w.lock().unwrap().refuse_disconnect = true;
    join(&rt, &b, "Loft");
    until(&rt, &s, "Loft being joined", || {
        w.lock().unwrap().joining.is_some()
            && cells.connecting.get_untracked(&rt) == Ok(Some("Loft".into()))
    });
    act(&rt, &b, "disconnect");
    until(&rt, &s, "the refusal", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        failures.lock().unwrap()[0],
        ("Loft".to_string(), "Not authorized".to_string())
    );
    until(&rt, &s, "still joining Loft", || {
        cells.connecting.get_untracked(&rt) == Ok(Some("Loft".into()))
            && cells.disconnecting.get_untracked(&rt) == Ok(None)
    });
    // The join goes on, and fails: iwd's agent has no password for Loft.
    w.lock().unwrap().stall = false;
    until(&rt, &s, "the join's failure", || {
        failures.lock().unwrap().len() == 2
    });
    assert_eq!(
        failures.lock().unwrap()[1],
        (
            "Loft".to_string(),
            "a password is needed (none was given, or it was wrong)".to_string()
        )
    );
    s.shutdown();
}

/// (gap 28, connectivity-r2-1) A `disconnect()` while the station names
/// no network (a `connect_with` join still forgetting the old password)
/// stops the join itself: iwd is not asked (it answers `NotConnected`,
/// which surfaced as a failure and let the join go on to `Connect`),
/// nothing is reported, and the join asks iwd nothing more.
#[test]
fn a_disconnect_before_iwd_joins_stops_the_join_itself() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = home_world("wlan0", None);
    {
        let mut w = w.lock().unwrap();
        w.connected = None;
        w.stall_forget = true;
    }
    let _iwd = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let calls = |what: &str| {
        w.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| *c == what)
            .count()
    };

    // Home with a new password: its old one is forgotten first, held.
    let home = AccessPoint {
        ssid: "Home".into(),
        strength: 0.6,
        secure: true,
        active: false,
    };
    b.network
        .dynamic()
        .action(
            &rt,
            "connect_with",
            Some(&home.to_data()),
            &[Data::Text("hunter22".into())],
            Default::default(),
        )
        .unwrap();
    until(&rt, &s, "Home's old password being forgotten", || {
        calls("Forget Home") == 1 && cells.connecting.get_untracked(&rt) == Ok(Some("Home".into()))
    });
    act(&rt, &b, "disconnect");
    until(&rt, &s, "the join stopped", || {
        cells.connecting.get_untracked(&rt) == Ok(None)
    });
    w.lock().unwrap().stall_forget = false;
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(500) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(calls("Disconnect"), 0, "iwd was asked to disconnect");
    assert_eq!(calls("Connect Home"), 0, "the join went on");
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(None));
    assert_eq!(cells.connecting.get_untracked(&rt), Ok(None));
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    s.shutdown();
}

/// (gap 28, connectivity-r2-2) A `disconnect()` while one is in flight (a
/// double click) is dropped: iwd is asked once, and the join the first
/// stopped is not reported. The second was asked too, re-tagging the
/// join; iwd refused it `Busy`, which was reported along with the
/// stopped join's `Aborted` ("a password is needed").
#[test]
fn a_disconnect_while_one_is_in_flight_is_dropped_on_iwd() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = home_world("wlan0", None);
    w.lock().unwrap().connected = None;
    let _iwd = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.network.cells();
    let failures = failures_of(&b);
    b.network.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let calls = |what: &str| {
        w.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| *c == what)
            .count()
    };

    // Attic being joined, held by iwd; the leave held too.
    w.lock().unwrap().stall = true;
    join(&rt, &b, "Attic");
    until(&rt, &s, "Attic being joined", || {
        w.lock().unwrap().joining.is_some()
            && cells.connecting.get_untracked(&rt) == Ok(Some("Attic".into()))
    });
    w.lock().unwrap().stall_disconnect = true;
    act(&rt, &b, "disconnect");
    act(&rt, &b, "disconnect");
    until(&rt, &s, "the join aborted", || {
        calls("Disconnect") >= 1
            && w.lock().unwrap().joining.is_none()
            && cells.connecting.get_untracked(&rt) == Ok(None)
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    w.lock().unwrap().stall = false;
    w.lock().unwrap().stall_disconnect = false;
    until(&rt, &s, "the leave answered", || {
        !w.lock().unwrap().leaving && cells.disconnecting.get_untracked(&rt) == Ok(None)
    });
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(calls("Disconnect"), 1, "iwd was asked twice");
    assert!(
        failures.lock().unwrap().is_empty(),
        "{:?}",
        failures.lock().unwrap()
    );
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(None));
    s.shutdown();
}

/// (services-round) Over iwd, `kind` is `wifi` while a network is
/// joined, `connections` is empty even while read, and `up()`/`down()`
/// fail saying saved connections need NetworkManager.
#[test]
fn iwd_has_no_saved_connections_and_says_so() {
    use strand_services::ToData;
    use strand_services::network::{Connection, ConnectionKind, IWD_NO_CONNECTIONS};
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = home_world("lo", None);
    let _iwd = fake_iwd(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    let cells = b.network.cells();
    b.network.acquire(&rt);
    b.network.acquire_field(field("connections"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.ssid.get_untracked(&rt), Ok(Some("Home".into())));
    assert_eq!(
        cells.kind.get_untracked(&rt),
        Ok(Some(ConnectionKind::Wifi))
    );
    assert!(
        cells
            .connections
            .get_untracked(&rt)
            .unwrap()
            .items()
            .is_empty()
    );
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
        ("Work VPN".to_string(), IWD_NO_CONNECTIONS.to_string())
    );
    s.shutdown();
}

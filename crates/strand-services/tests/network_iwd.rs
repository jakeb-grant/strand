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
}

struct Device(W);

#[zbus::interface(name = "net.connman.iwd.Device")]
impl Device {
    #[zbus(property)]
    fn name(&self) -> String {
        "wlan0".into()
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
        match self.0.lock().unwrap().connected {
            Some(_) => "connected".into(),
            None => "disconnected".into(),
        }
    }

    #[zbus(property)]
    fn connected_network(&self) -> OwnedObjectPath {
        path(self.0.lock().unwrap().connected.as_deref().unwrap_or("/"))
    }

    #[zbus(property)]
    fn scanning(&self) -> bool {
        false
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
        self.scanning_changed(&e).await.unwrap();
    }

    fn get_ordered_networks(&self) -> Vec<(OwnedObjectPath, i16)> {
        let w = self.0.lock().unwrap();
        w.ordered.iter().map(|(p, s)| (path(p), *s)).collect()
    }

    fn register_signal_level_agent(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        agent: ObjectPath<'_>,
        levels: Vec<i16>,
    ) {
        let sender = h.sender().unwrap().to_string();
        self.0.lock().unwrap().levels = Some((sender, agent.to_string(), levels));
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
        path(if known { "/net/connman/iwd/known" } else { "/" })
    }

    /// Joins: a personal network iwd does not know asks the agent for
    /// its passphrase (`hunter22` is right).
    async fn connect(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> Result<(), IwdError> {
        let (ask, agent) = {
            let mut w = self.w.lock().unwrap();
            w.calls.push(format!("Connect {}", self.name));
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
        let station = server.interface::<_, Station>(DEV).await?;
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

/// iwd with `wlan0` joined to Home (known); Attic and Loft (personal,
/// unknown) in range.
fn fake_iwd(tokio: &tokio::runtime::Runtime, address: &str, w: &W) -> zbus::Connection {
    tokio.block_on(async {
        zbus::connection::Builder::address(address)
            .unwrap()
            .name(iwd::IWD)
            .unwrap()
            .serve_at("/", zbus::fdo::ObjectManager)
            .unwrap()
            .serve_at(iwd::MANAGER, AgentManager(w.clone()))
            .unwrap()
            .serve_at(DEV, Device(w.clone()))
            .unwrap()
            .serve_at(DEV, Station(w.clone()))
            .unwrap()
            .serve_at(HOME, Net::new(w, HOME, "Home", "psk"))
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
    join("Attic", Some("hunter22"));
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

    // NetworkManager appears: the network is its now (this one has no
    // objects: offline), and the agents are taken down.
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
        cells.ssid.get_untracked(&rt) == Ok(Some("Attic".into()))
    });
    assert!(w.lock().unwrap().agent.is_some());
    assert_eq!(b.network.starts(), 1, "one body throughout");
    s.shutdown();
}

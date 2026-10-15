//! `bluetooth`: BlueZ on the system bus, with our own small zbus client
//! (decisions.md, wave4-a2: bluer is built on libdbus, not zbus).
//!
//! BlueZ is an object manager: the service reads `GetManagedObjects` on
//! `/`, then follows `InterfacesAdded`/`InterfacesRemoved` and the
//! objects' `PropertiesChanged` ([`Daemon`]). The first adapter (by
//! path) gives `powered`, written through its `Powered` property; the
//! devices paired with it are `devices`, with their battery from
//! `org.bluez.Battery1` when they report one. BlueZ restarting is read
//! afresh; missing, there is no adapter (`powered` false, no devices).
//!
//! (m6-service-gaps) `nearby` (a `#[store(stream)]` field) is the
//! unpaired devices in range that have a name, found by discovery, which
//! runs only while a visible reader reads them (`SetDiscoveryFilter`
//! without duplicate data, then `StartDiscovery`; `StopDiscovery` when
//! the reader goes, only for a discovery this body started: BlueZ keeps
//! a discovery per client, so another program's is never stopped).
//! Signals that change only a device's signal strength or advertisement
//! data are dropped unread. `d.pair()` pairs, trusts and connects;
//! `d.forget()` removes the device from the adapter. Connects,
//! disconnects, pairs and forgets run as tasks, and a device's
//! `connecting` or `disconnecting` is the shell's own call in flight
//! (BlueZ has no such property), the one asked last when there are
//! several. A call repeating one still in flight on its device (a double
//! click; a connect while a pair runs, which connects too) is dropped:
//! BlueZ would refuse it `InProgress`. A pairing that times out is
//! cancelled (`CancelPairing`), so BlueZ does not go on with it. No
//! pairing agent is registered: BlueZ uses the system's if one runs,
//! else pairs without one.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

/// The schema the `bluetooth` service serves.
pub const SCHEMA: &str = strand_services_schema::BLUETOOTH;

/// BlueZ's bus name.
pub const BLUEZ: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const BATTERY: &str = "org.bluez.Battery1";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";

/// How long a connect may take (BlueZ's own page timeout is shorter).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long pairing may take: a person may be answering the system's
/// pairing agent.
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);

/// A Bluetooth device, paired or in range.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "BluetoothDevice", key = address)]
pub struct BluetoothDevice {
    pub address: String,
    pub name: String,
    pub connected: bool,
    pub battery: Option<f64>,
    pub icon: String,
    pub connecting: bool,
    pub disconnecting: bool,
}

/// `bluetooth`'s actions.
#[derive(Call, Debug)]
pub enum BluetoothAction {
    /// `d.connect()`.
    Connect { item: BluetoothDevice },
    /// `d.disconnect()`.
    Disconnect { item: BluetoothDevice },
    /// `d.pair()`.
    Pair { item: BluetoothDevice },
    /// `d.forget()`.
    Forget { item: BluetoothDevice },
}

impl BluetoothAction {
    fn item(self) -> BluetoothDevice {
        match self {
            BluetoothAction::Connect { item }
            | BluetoothAction::Disconnect { item }
            | BluetoothAction::Pair { item }
            | BluetoothAction::Forget { item } => item,
        }
    }
}

/// See the module docs.
#[service(name = "bluetooth", action = BluetoothAction, refused = refused)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Bluetooth {
    /// The adapter is on. Writable.
    #[store(rw)]
    pub powered: bool,
    /// Paired devices, keyed by `address`.
    #[store(keyed)]
    pub devices: Vec<BluetoothDevice>,
    /// BlueZ answers and has an adapter: false without them (or without a
    /// system bus), so a menu can hide what cannot work.
    pub available: bool,
    /// Unpaired devices in range that have a name, keyed by `address`, found
    /// by discovery. Discovery runs only while a visible reader reads them
    /// (a pairing page that is open).
    #[store(keyed, stream)]
    pub nearby: Vec<BluetoothDevice>,
    /// The adapter is discovering devices, for `nearby` or for another
    /// program.
    pub scanning: bool,
    /// Connecting, disconnecting, pairing or forgetting a device failed (out
    /// of range, turned off, timed out, or the service not running): `on
    /// bluetooth.failed(address, error) { … }`.
    pub failed: Event<(String, String)>,
}

/// An action the service could not take (no system bus, its run backing
/// off): `failed(address, error)`, as BlueZ refusing it is.
fn refused(action: &BluetoothAction, error: &str) -> Option<BluetoothEvent> {
    let (BluetoothAction::Connect { item }
    | BluetoothAction::Disconnect { item }
    | BluetoothAction::Pair { item }
    | BluetoothAction::Forget { item }) = action;
    Some(BluetoothEvent::Failed((
        item.address.clone(),
        error.to_string(),
    )))
}

/// BlueZ's objects: path, then interface, then properties.
type Objects = BTreeMap<String, HashMap<String, Props>>;

/// A call of the shell's own on a device, in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    /// `connect()` or `pair()`.
    Connecting,
    /// `disconnect()`.
    Disconnecting,
}

/// An icon name for BlueZ's `Icon` (`audio-headset`, `input-mouse`).
fn device_icon(p: &Props) -> String {
    match dbus::text(p, "Icon") {
        Some(i) if !i.is_empty() => format!("{i}-symbolic"),
        _ => "bluetooth-symbolic".to_string(),
    }
}

/// The properties that change with every advertisement a device sends
/// while discovering; a signal changing only these is dropped.
const ADVERTISED: [&str; 6] = [
    "RSSI",
    "TxPower",
    "ManufacturerData",
    "ServiceData",
    "AdvertisingFlags",
    "AdvertisingData",
];

/// A device's `PropertiesChanged` that changes only what its
/// advertisements carry (its signal strength, its data): nothing shown.
fn advertisement_only(c: &dbus::Changed) -> bool {
    c.iface == DEVICE
        && c.changed
            .keys()
            .map(String::as_str)
            .chain(c.invalidated.iter().map(String::as_str))
            .all(|k| ADVERTISED.contains(&k))
}

impl Bluetooth {
    /// The state BlueZ's objects make, with the shell's own calls in
    /// flight (`busy`, by address); the devices in range only while
    /// `listing`.
    pub fn from_objects(
        objects: &Objects,
        busy: &HashMap<String, Busy>,
        listing: bool,
    ) -> Bluetooth {
        let adapter = objects
            .iter()
            .find_map(|(path, ifaces)| ifaces.get(ADAPTER).map(|p| (path, p)));
        let Some((adapter_path, adapter)) = adapter else {
            return Bluetooth::default();
        };
        let mut devices = Vec::new();
        let mut nearby = Vec::new();
        for ifaces in objects.values() {
            let Some(d) = ifaces.get(DEVICE) else {
                continue;
            };
            let on_adapter = d
                .get("Adapter")
                .and_then(|v| v.downcast_ref::<zbus::zvariant::ObjectPath<'_>>().ok())
                .is_some_and(|a| a.as_str() == adapter_path);
            let paired = dbus::boolean(d, "Paired").unwrap_or(false);
            // A device in range is listed only by a name it gave (BlueZ's
            // `Alias` falls back to the address).
            let named = dbus::text(d, "Name").is_some_and(|n| !n.is_empty());
            if !on_adapter || !(paired || (listing && named)) {
                continue;
            }
            let Some(address) = dbus::text(d, "Address") else {
                continue;
            };
            let state = busy.get(&address).copied();
            let device = BluetoothDevice {
                name: dbus::text(d, "Alias")
                    .or_else(|| dbus::text(d, "Name"))
                    .unwrap_or_default(),
                connected: dbus::boolean(d, "Connected").unwrap_or(false),
                battery: ifaces
                    .get(BATTERY)
                    .and_then(|b| dbus::number(b, "Percentage"))
                    .map(|p| (p / 100.0).clamp(0.0, 1.0)),
                icon: device_icon(d),
                connecting: state == Some(Busy::Connecting),
                disconnecting: state == Some(Busy::Disconnecting),
                address,
            };
            if paired {
                devices.push(device);
            } else {
                nearby.push(device);
            }
        }
        let by_name = |a: &BluetoothDevice, b: &BluetoothDevice| {
            a.name.cmp(&b.name).then(a.address.cmp(&b.address))
        };
        devices.sort_by(by_name);
        nearby.sort_by(by_name);
        Bluetooth {
            powered: dbus::boolean(adapter, "Powered").unwrap_or(false),
            devices,
            available: true,
            nearby,
            scanning: dbus::boolean(adapter, "Discovering").unwrap_or(false),
            failed: Event::default(),
        }
    }

    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        let mut discovery = Discovery::default();
        let r = follow(&mut cx, &conn, &mut discovery).await;
        // A discovery this body started ends with it (BlueZ would end it
        // only with the connection, which other services share).
        discovery.stop(&conn).await;
        r
    }
}

/// The shell's calls on devices in flight, by task: each one's device
/// address and what it asks, numbered in the order asked
/// (connectivity-r1-3).
#[derive(Debug, Default)]
struct InFlight {
    calls: HashMap<tokio::task::Id, (String, Ask, u64)>,
    asked: u64,
}

impl InFlight {
    /// `ask` of `address` repeats a call in flight on it: the same one,
    /// or a connect while a pair runs (a pair connects too).
    fn repeats(&self, address: &str, ask: Ask) -> bool {
        self.calls.values().any(|(a, running, _)| {
            a == address && (*running == ask || (ask == Ask::Connect && *running == Ask::Pair))
        })
    }

    /// Task `id` asks `ask` of `address`.
    fn insert(&mut self, id: tokio::task::Id, address: String, ask: Ask) {
        self.asked += 1;
        self.calls.insert(id, (address, ask, self.asked));
    }

    /// Task `id` ended.
    fn remove(&mut self, id: tokio::task::Id) {
        self.calls.remove(&id);
    }

    /// What each device is busy with: the call asked last that shows (a
    /// forget does not).
    fn by_address(&self) -> HashMap<String, Busy> {
        let mut last: HashMap<&str, (u64, Busy)> = HashMap::new();
        for (address, ask, n) in self.calls.values() {
            let Some(busy) = ask.busy() else {
                continue;
            };
            let e = last.entry(address.as_str()).or_insert((*n, busy));
            if *n >= e.0 {
                *e = (*n, busy);
            }
        }
        last.into_iter()
            .map(|(a, (_, busy))| (a.to_string(), busy))
            .collect()
    }
}

/// The `bluetooth` body: BlueZ followed, read afresh when it restarts.
async fn follow(
    cx: &mut Cx<Bluetooth>,
    conn: &zbus::Connection,
    discovery: &mut Discovery,
) -> Result<(), ServiceError> {
    let mut daemon = Daemon::new(conn, BLUEZ).await?;
    daemon.subscribe("manager", dbus::path_rule("/")?).await?;
    daemon
        .subscribe("objects", dbus::namespace_rule("/org/bluez")?)
        .await?;
    // Connects, disconnects, pairs and forgets in flight: dropped
    // (cancelled) with the body, so a stopped service keeps no call, and
    // no connection. Each brings back its device's address and, if it
    // failed, why; `busy` says which device each is busy with.
    let mut calls: tokio::task::JoinSet<(String, Option<String>)> = tokio::task::JoinSet::new();
    let mut busy = InFlight::default();
    // The devices in range are listed (a reader reads them).
    let mut listing = cx.watched("nearby");
    let publish = |cx: &mut Cx<Bluetooth>, objects: &Objects, busy: &InFlight, listing: bool| {
        let busy = busy.by_address();
        cx.update(|s| *s = Bluetooth::from_objects(objects, &busy, listing))
    };
    loop {
        let mut objects = read(&daemon)
            .await
            .map_err(|e| ServiceError(format!("BlueZ did not answer: {e}")))?;
        // A discovery of the BlueZ before went with it.
        discovery.forget();
        discovery.sync(conn, &objects, listing).await;
        if !publish(cx, &objects, &busy, listing) {
            return Ok(());
        }
        cx.ready();
        'follow: loop {
            tokio::select! {
                Some(done) = calls.join_next_with_id(), if !calls.is_empty() => {
                    let failure = match done {
                        Ok((id, (address, error))) => {
                            busy.remove(id);
                            error.map(|e| (address, e))
                        }
                        Err(e) => {
                            busy.remove(e.id());
                            None
                        }
                    };
                    if !publish(cx, &objects, &busy, listing) {
                        return Ok(());
                    }
                    if let Some(failure) = failure
                        && !cx.emit(BluetoothEvent::Failed(failure))
                    {
                        return Ok(());
                    }
                }
                ev = daemon.next() => match ev {
                    None => return Err(ServiceError("the system bus connection ended".into())),
                    Some(DaemonEvent::Owner) => break 'follow,
                    Some(DaemonEvent::Signal(m)) => {
                        if signal(&daemon, &mut objects, &m).await {
                            // The adapter powered on: discovery again if
                            // it is wanted.
                            discovery.sync(conn, &objects, listing).await;
                            if !publish(cx, &objects, &busy, listing) {
                                return Ok(());
                            }
                        }
                    }
                },
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Watch { field: "nearby", on }) => {
                        listing = on;
                        discovery.sync(conn, &objects, listing).await;
                        if !publish(cx, &objects, &busy, listing) {
                            return Ok(());
                        }
                    }
                    Some(Msg::Write(w)) if w.field == "powered" => {
                        let on: bool = w.value().unwrap_or(cx.state().powered);
                        let adapter = objects
                            .iter()
                            .find(|(_, i)| i.contains_key(ADAPTER))
                            .map(|(p, _)| p.clone());
                        let ok = match adapter {
                            Some(path) => dbus::set(conn, BLUEZ, &path, ADAPTER, "Powered", on.into())
                                .await
                                .map_err(|e| e.to_string()),
                            None => Err("no adapter".to_string()),
                        };
                        let now = match ok {
                            Ok(()) => on,
                            Err(e) => {
                                log::warn!("bluetooth: not powered {}: {e}", if on { "on" } else { "off" });
                                // The adapter as last read.
                                Bluetooth::from_objects(&objects, &HashMap::new(), false).powered
                            }
                        };
                        if !cx.report(&w, |s| s.powered = now) {
                            return Ok(());
                        }
                    }
                    Some(Msg::Action(a)) => {
                        let what = match &a {
                            BluetoothAction::Connect { .. } => Ask::Connect,
                            BluetoothAction::Disconnect { .. } => Ask::Disconnect,
                            BluetoothAction::Pair { .. } => Ask::Pair,
                            BluetoothAction::Forget { .. } => Ask::Forget,
                        };
                        let address = a.item().address;
                        if busy.repeats(&address, what) {
                            log::debug!("bluetooth: {what:?} {address} already runs");
                            continue;
                        }
                        let found = objects.iter().find_map(|(p, i)| {
                            let d = i.get(DEVICE)?;
                            (dbus::text(d, "Address").as_deref() == Some(address.as_str()))
                                .then(|| (p.clone(), object_path(d, "Adapter")))
                        });
                        let Some((path, adapter)) = found else {
                            log::warn!("bluetooth: no device {address}");
                            if !cx.emit(BluetoothEvent::Failed((address, "no such device".into()))) {
                                return Ok(());
                            }
                            continue;
                        };
                        // Each can take seconds (pairing, a minute): it
                        // holds up nothing; BlueZ reports the outcome as
                        // property changes.
                        let conn = conn.clone();
                        let addr = address.clone();
                        let task = calls.spawn(async move {
                            let r = match what {
                                Ask::Connect => device_call(&conn, &path, "Connect", CONNECT_TIMEOUT).await,
                                Ask::Disconnect => device_call(&conn, &path, "Disconnect", CONNECT_TIMEOUT).await,
                                Ask::Pair => pair(&conn, &path, PAIR_TIMEOUT).await,
                                Ask::Forget => forget(&conn, adapter.as_deref(), &path).await,
                            };
                            if let Err(e) = &r {
                                log::warn!("bluetooth: {what:?} {path}: {e}");
                            }
                            (addr, r.err())
                        });
                        busy.insert(task.id(), address, what);
                        if what.busy().is_some() && !publish(cx, &objects, &busy, listing) {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

/// What an action asks of BlueZ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Connect,
    Disconnect,
    Pair,
    Forget,
}

impl Ask {
    /// What its device shows while it runs.
    fn busy(self) -> Option<Busy> {
        match self {
            Ask::Connect | Ask::Pair => Some(Busy::Connecting),
            Ask::Disconnect => Some(Busy::Disconnecting),
            Ask::Forget => None,
        }
    }
}

/// What a failed call to BlueZ says: its own words ("Page Timeout",
/// "Host is down", "Authentication Failed"), or that it timed out.
fn bluez_error(e: zbus::Error) -> String {
    match e {
        e if dbus::is_timeout(&e) => "timed out".to_string(),
        zbus::Error::MethodError(_, Some(text), _) => text,
        e => e.to_string(),
    }
}

/// The error BlueZ answered `e` with is `name`.
fn is_error(e: &zbus::Error, name: &str) -> bool {
    matches!(e, zbus::Error::MethodError(n, _, _) if n.as_str() == name)
}

/// `method` (`Connect`, `Disconnect`) of device `path`, bounded.
async fn device_call(
    conn: &zbus::Connection,
    path: &str,
    method: &str,
    limit: Duration,
) -> Result<(), String> {
    dbus::timed_for(
        limit,
        conn.call_method(Some(BLUEZ), path, Some(DEVICE), method, &()),
    )
    .await
    .map(drop)
    .map_err(bluez_error)
}

/// Pair with device `path` (one paired already is no failure), trust it
/// and connect to it (one connected already is none either). The first
/// step failing says why; a device paired whose connect failed stays
/// paired. A pairing not done within `limit` is cancelled
/// (`CancelPairing`): BlueZ would go on with it, and refuse the next
/// one as in progress.
async fn pair(conn: &zbus::Connection, path: &str, limit: Duration) -> Result<(), String> {
    let paired = dbus::timed_for(
        limit,
        conn.call_method(Some(BLUEZ), path, Some(DEVICE), "Pair", &()),
    )
    .await;
    match paired {
        Err(e) if dbus::is_timeout(&e) => {
            let cancel = dbus::timed_for(
                dbus::READ_TIMEOUT,
                conn.call_method(Some(BLUEZ), path, Some(DEVICE), "CancelPairing", &()),
            )
            .await;
            if let Err(c) = cancel {
                log::debug!("bluetooth: pairing {path} not cancelled: {c}");
            }
            return Err(bluez_error(e));
        }
        Err(e) if !is_error(&e, "org.bluez.Error.AlreadyExists") => return Err(bluez_error(e)),
        _ => {}
    }
    dbus::set(conn, BLUEZ, path, DEVICE, "Trusted", true.into())
        .await
        .map_err(bluez_error)?;
    let connected = dbus::timed_for(
        CONNECT_TIMEOUT,
        conn.call_method(Some(BLUEZ), path, Some(DEVICE), "Connect", &()),
    )
    .await;
    match connected {
        Err(e) if !is_error(&e, "org.bluez.Error.AlreadyConnected") => Err(bluez_error(e)),
        _ => Ok(()),
    }
}

/// Remove device `path` from its adapter: unpaired and forgotten.
async fn forget(conn: &zbus::Connection, adapter: Option<&str>, path: &str) -> Result<(), String> {
    let adapter = adapter.ok_or_else(|| "the device has no adapter".to_string())?;
    let device = zbus::zvariant::ObjectPath::try_from(path).map_err(|e| e.to_string())?;
    dbus::timed_for(
        dbus::READ_TIMEOUT,
        conn.call_method(
            Some(BLUEZ),
            adapter,
            Some(ADAPTER),
            "RemoveDevice",
            &(device,),
        ),
    )
    .await
    .map(drop)
    .map_err(bluez_error)
}

/// Discovery of this body's own (`nearby`): started while wanted on a
/// powered adapter, stopped when no longer wanted.
#[derive(Debug, Default)]
struct Discovery {
    /// The adapter this body started discovering on.
    started: Option<String>,
    /// The adapter discovery was last asked on (a refusal is not asked
    /// again on every signal; the adapter powering on again asks anew).
    tried: Option<String>,
}

impl Discovery {
    /// Start or stop discovery as `wanted` says, on the first adapter
    /// while it is powered.
    async fn sync(&mut self, conn: &zbus::Connection, objects: &Objects, wanted: bool) {
        let adapter = objects.iter().find_map(|(path, ifaces)| {
            let a = ifaces.get(ADAPTER)?;
            Some((path.clone(), dbus::boolean(a, "Powered").unwrap_or(false)))
        });
        let powered = adapter.and_then(|(p, on)| on.then_some(p));
        if powered.is_none() {
            // Powered off (or gone): BlueZ ended the discovery.
            self.forget();
            return;
        }
        if !wanted {
            self.tried = None;
            self.stop(conn).await;
            return;
        }
        if self.started.is_some() || self.tried == powered {
            return;
        }
        let Some(adapter) = powered else {
            return;
        };
        self.tried = Some(adapter.clone());
        match start_discovery(conn, &adapter).await {
            Ok(()) => self.started = Some(adapter),
            Err(e) => log::warn!("bluetooth: no discovery on {adapter}: {e}"),
        }
    }

    /// Stop the discovery this body started.
    async fn stop(&mut self, conn: &zbus::Connection) {
        let Some(adapter) = self.started.take() else {
            return;
        };
        let r = dbus::timed_for(
            dbus::READ_TIMEOUT,
            conn.call_method(
                Some(BLUEZ),
                adapter.as_str(),
                Some(ADAPTER),
                "StopDiscovery",
                &(),
            ),
        )
        .await;
        if let Err(e) = r {
            log::debug!("bluetooth: discovery on {adapter} not stopped: {e}");
        }
    }

    /// The adapter or BlueZ went: nothing of this body's runs.
    fn forget(&mut self) {
        self.started = None;
        self.tried = None;
    }
}

/// Discover devices on `adapter`: without duplicate data (fewer signals
/// for each device's advertisements), then `StartDiscovery`.
async fn start_discovery(conn: &zbus::Connection, adapter: &str) -> zbus::Result<()> {
    let filter: HashMap<&str, zbus::zvariant::Value<'_>> =
        HashMap::from([("DuplicateData", zbus::zvariant::Value::from(false))]);
    let r = dbus::timed_for(
        dbus::READ_TIMEOUT,
        conn.call_method(
            Some(BLUEZ),
            adapter,
            Some(ADAPTER),
            "SetDiscoveryFilter",
            &(filter,),
        ),
    )
    .await;
    if let Err(e) = r {
        log::debug!("bluetooth: no discovery filter on {adapter}: {e}");
    }
    dbus::timed_for(
        dbus::READ_TIMEOUT,
        conn.call_method(Some(BLUEZ), adapter, Some(ADAPTER), "StartDiscovery", &()),
    )
    .await
    .map(drop)
}

/// Every object BlueZ manages.
///
/// A hung BlueZ (no answer in [`dbus::READ_TIMEOUT`]) is an error: the
/// run fails and is retried.
async fn read(daemon: &Daemon) -> zbus::Result<Objects> {
    if daemon.owner().is_none() {
        return Ok(Objects::new());
    }
    let reply = dbus::timed_for(
        dbus::READ_TIMEOUT,
        daemon.conn().call_method(
            Some(BLUEZ),
            "/",
            Some(OBJECT_MANAGER),
            "GetManagedObjects",
            &(),
        ),
    )
    .await;
    let reply = match reply {
        Err(e) if dbus::is_timeout(&e) => return Err(e),
        r => r,
    };
    let objects = reply.and_then(|r| {
        r.body()
            .deserialize::<HashMap<zbus::zvariant::OwnedObjectPath, HashMap<String, Props>>>()
    });
    Ok(match objects {
        Ok(o) => o.into_iter().map(|(p, i)| (p.to_string(), i)).collect(),
        Err(e) => {
            log::debug!("bluetooth: no objects: {e}");
            Objects::new()
        }
    })
}

/// Apply a signal; whether anything changed.
async fn signal(daemon: &Daemon, objects: &mut Objects, m: &zbus::Message) -> bool {
    if let Some(c) = dbus::properties_changed(m) {
        // A device's advertisement while discovering: nothing shown.
        if advertisement_only(&c) {
            return false;
        }
        let Some(props) = objects.get_mut(&c.path).and_then(|i| i.get_mut(&c.iface)) else {
            return false;
        };
        dbus::apply_changed(daemon.conn(), BLUEZ, props, c).await;
        return true;
    }
    if dbus::interface(m).as_deref() != Some(OBJECT_MANAGER) {
        return false;
    }
    match dbus::member(m).as_deref() {
        Some("InterfacesAdded") => {
            let Ok((path, ifaces)) = m
                .body()
                .deserialize::<(zbus::zvariant::OwnedObjectPath, HashMap<String, Props>)>()
            else {
                return false;
            };
            objects.entry(path.to_string()).or_default().extend(ifaces);
            true
        }
        Some("InterfacesRemoved") => {
            let Ok((path, ifaces)) = m
                .body()
                .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
            else {
                return false;
            };
            let key = path.to_string();
            if let Some(o) = objects.get_mut(&key) {
                for i in ifaces {
                    o.remove(&i);
                }
                if o.is_empty() {
                    objects.remove(&key);
                }
            }
            true
        }
        _ => false,
    }
}

/// An object path property (`Adapter`), `None` for `/`.
fn object_path(p: &Props, name: &str) -> Option<String> {
    let path: zbus::zvariant::ObjectPath<'_> = p.get(name)?.downcast_ref().ok()?;
    let s = path.to_string();
    (s != "/").then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    fn v(x: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(x).unwrap()
    }

    /// An adapter with a paired device (Headphones), a named one in range
    /// (Stranger), and an anonymous advertiser.
    fn objects() -> Objects {
        let adapter = "/org/bluez/hci0";
        let device = |address: &str, name: Option<&str>, paired: bool| {
            let mut d = Props::from([
                ("Address".to_string(), v(Value::from(address))),
                ("Alias".to_string(), v(Value::from(name.unwrap_or(address)))),
                ("Paired".to_string(), v(Value::from(paired))),
                ("Connected".to_string(), v(Value::from(false))),
                (
                    "Adapter".to_string(),
                    v(Value::from(ObjectPath::try_from(adapter).unwrap())),
                ),
            ]);
            if let Some(n) = name {
                d.insert("Name".into(), v(Value::from(n)));
            }
            HashMap::from([(DEVICE.to_string(), d)])
        };
        Objects::from([
            (
                adapter.to_string(),
                HashMap::from([(
                    ADAPTER.to_string(),
                    Props::from([
                        ("Powered".to_string(), v(Value::from(true))),
                        ("Discovering".to_string(), v(Value::from(true))),
                    ]),
                )]),
            ),
            (
                format!("{adapter}/dev_1"),
                device("11:22:33:44:55:66", Some("Headphones"), true),
            ),
            (
                format!("{adapter}/dev_2"),
                device("AA:BB:CC:DD:EE:FF", Some("Stranger"), false),
            ),
            (
                format!("{adapter}/dev_3"),
                device("12:34:56:78:9A:BC", None, false),
            ),
        ])
    }

    #[test]
    fn nearby_lists_named_unpaired_devices_only_while_listing() {
        let o = objects();
        let none = HashMap::new();
        let b = Bluetooth::from_objects(&o, &none, false);
        assert_eq!(b.devices.len(), 1);
        assert_eq!(b.devices[0].name, "Headphones");
        assert!(b.nearby.is_empty(), "not listed while nobody reads it");
        assert!(b.scanning);
        let b = Bluetooth::from_objects(&o, &none, true);
        let nearby: Vec<&str> = b.nearby.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(nearby, ["Stranger"], "the anonymous one is left out");
        assert_eq!(b.devices.len(), 1, "a paired device is not nearby");
    }

    #[test]
    fn busy_devices_show_connecting_and_disconnecting() {
        let o = objects();
        let busy = HashMap::from([
            ("11:22:33:44:55:66".to_string(), Busy::Disconnecting),
            ("AA:BB:CC:DD:EE:FF".to_string(), Busy::Connecting),
        ]);
        let b = Bluetooth::from_objects(&o, &busy, true);
        assert!(b.devices[0].disconnecting && !b.devices[0].connecting);
        assert!(b.nearby[0].connecting && !b.nearby[0].disconnecting);
        let b = Bluetooth::from_objects(&o, &HashMap::new(), true);
        assert!(!b.devices[0].disconnecting && !b.nearby[0].connecting);
    }

    /// (gap 29, connectivity-r1-3) A call repeating one in flight on its
    /// device is dropped (a double click; a connect while a pair runs),
    /// and a device busy with several calls shows the one asked last,
    /// whatever order the tasks are kept in.
    #[test]
    fn a_device_shows_its_last_call_and_repeats_are_dropped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _in = rt.enter();
        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let mut id = || tasks.spawn(async {}).id();
        let (a, b) = ("11:22:33:44:55:66", "AA:BB:CC:DD:EE:FF");
        let mut f = InFlight::default();
        let pair = id();
        f.insert(pair, b.into(), Ask::Pair);
        assert!(f.repeats(b, Ask::Pair), "a double click");
        assert!(f.repeats(b, Ask::Connect), "the pair connects");
        assert!(!f.repeats(b, Ask::Disconnect));
        assert!(!f.repeats(a, Ask::Pair), "another device");
        assert!(!f.repeats(b, Ask::Forget));
        let connect = id();
        f.insert(connect, a.into(), Ask::Connect);
        assert!(!f.repeats(a, Ask::Pair), "a pair of a device connecting");
        for _ in 0..20 {
            let mut f2 = InFlight::default();
            let (c, d) = (id(), id());
            f2.insert(c, a.into(), Ask::Connect);
            f2.insert(d, a.into(), Ask::Disconnect);
            assert_eq!(f2.by_address()[a], Busy::Disconnecting, "the last asked");
            f2.insert(id(), a.into(), Ask::Forget);
            assert_eq!(
                f2.by_address()[a],
                Busy::Disconnecting,
                "a forget shows not"
            );
            f2.insert(id(), a.into(), Ask::Connect);
            assert_eq!(f2.by_address()[a], Busy::Connecting);
            f2.remove(d);
            f2.remove(c);
            assert_eq!(f2.by_address()[a], Busy::Connecting);
        }
        assert_eq!(f.by_address().len(), 2);
        f.remove(pair);
        assert!(!f.repeats(b, Ask::Pair), "ended");
        assert_eq!(f.by_address().get(b), None);
    }

    /// (gap 29, connectivity-r1-3) A pairing not done in time is
    /// cancelled, so BlueZ does not go on with it (and refuse the next
    /// pairing as in progress); it fails "timed out".
    #[test]
    fn a_pairing_that_times_out_is_cancelled() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Device(Arc<AtomicUsize>);
        #[zbus::interface(name = "org.bluez.Device1")]
        impl Device {
            async fn pair(&self) {
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
            fn cancel_pairing(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let Some(bus) = crate::testing::PrivateBus::start() else {
            return;
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let dev = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        rt.block_on(async {
            let _bluez = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .name(BLUEZ)
                .unwrap()
                .serve_at(dev, Device(cancelled.clone()))
                .unwrap()
                .build()
                .await
                .unwrap();
            let conn = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let r = pair(&conn, dev, Duration::from_millis(300)).await;
            assert_eq!(r, Err("timed out".to_string()));
        });
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn advertisement_only_changes_are_ignored() {
        let changed = |iface: &str, keys: &[&str], invalidated: &[&str]| dbus::Changed {
            path: "/org/bluez/hci0/dev_2".into(),
            iface: iface.into(),
            changed: keys
                .iter()
                .map(|k| (k.to_string(), v(Value::from(1i16))))
                .collect(),
            invalidated: invalidated.iter().map(|s| s.to_string()).collect(),
        };
        assert!(advertisement_only(&changed(DEVICE, &["RSSI"], &[])));
        assert!(advertisement_only(&changed(
            DEVICE,
            &["RSSI", "TxPower", "ManufacturerData"],
            &["ServiceData"]
        )));
        assert!(!advertisement_only(&changed(
            DEVICE,
            &["RSSI", "Name"],
            &[]
        )));
        assert!(!advertisement_only(&changed(DEVICE, &["Connected"], &[])));
        assert!(!advertisement_only(&changed(DEVICE, &[], &["Alias"])));
        assert!(!advertisement_only(&changed(ADAPTER, &["RSSI"], &[])));
    }
}

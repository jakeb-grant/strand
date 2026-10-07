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
//! Nothing is discovered (no scan): pairing is the system's settings.

use std::collections::{BTreeMap, HashMap};

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

/// A paired Bluetooth device.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "BluetoothDevice", key = address)]
pub struct BluetoothDevice {
    pub address: String,
    pub name: String,
    pub connected: bool,
    pub battery: Option<f64>,
    pub icon: String,
}

/// `bluetooth`'s actions.
#[derive(Call, Debug)]
pub enum BluetoothAction {
    /// `d.connect()`.
    Connect { item: BluetoothDevice },
    /// `d.disconnect()`.
    Disconnect { item: BluetoothDevice },
}

/// See the module docs.
#[service(name = "bluetooth", action = BluetoothAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Bluetooth {
    /// The adapter is on. Writable.
    #[store(rw)]
    pub powered: bool,
    /// Paired devices, keyed by `address`.
    #[store(keyed)]
    pub devices: Vec<BluetoothDevice>,
    /// Connecting or disconnecting a device failed (out of range, turned
    /// off, timed out after 30 s): `on bluetooth.failed(address, error)
    /// { … }`.
    pub failed: Event<(String, String)>,
}

/// BlueZ's objects: path, then interface, then properties.
type Objects = BTreeMap<String, HashMap<String, Props>>;

/// An icon name for BlueZ's `Icon` (`audio-headset`, `input-mouse`).
fn device_icon(p: &Props) -> String {
    match dbus::text(p, "Icon") {
        Some(i) if !i.is_empty() => format!("{i}-symbolic"),
        _ => "bluetooth-symbolic".to_string(),
    }
}

impl Bluetooth {
    /// The state BlueZ's objects make.
    pub fn from_objects(objects: &Objects) -> Bluetooth {
        let adapter = objects
            .iter()
            .find_map(|(path, ifaces)| ifaces.get(ADAPTER).map(|p| (path, p)));
        let Some((adapter_path, adapter)) = adapter else {
            return Bluetooth::default();
        };
        let mut devices: Vec<BluetoothDevice> = objects
            .values()
            .filter_map(|ifaces| {
                let d = ifaces.get(DEVICE)?;
                let on_adapter = d
                    .get("Adapter")
                    .and_then(|v| v.downcast_ref::<zbus::zvariant::ObjectPath<'_>>().ok())
                    .is_some_and(|a| a.as_str() == adapter_path);
                if !on_adapter || !dbus::boolean(d, "Paired").unwrap_or(false) {
                    return None;
                }
                Some(BluetoothDevice {
                    address: dbus::text(d, "Address")?,
                    name: dbus::text(d, "Alias")
                        .or_else(|| dbus::text(d, "Name"))
                        .unwrap_or_default(),
                    connected: dbus::boolean(d, "Connected").unwrap_or(false),
                    battery: ifaces
                        .get(BATTERY)
                        .and_then(|b| dbus::number(b, "Percentage"))
                        .map(|p| (p / 100.0).clamp(0.0, 1.0)),
                    icon: device_icon(d),
                })
            })
            .collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.address.cmp(&b.address)));
        Bluetooth {
            powered: dbus::boolean(adapter, "Powered").unwrap_or(false),
            devices,
            failed: Event::default(),
        }
    }

    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        let mut daemon = Daemon::new(&conn, BLUEZ).await?;
        daemon.subscribe("manager", dbus::path_rule("/")?).await?;
        daemon
            .subscribe("objects", dbus::namespace_rule("/org/bluez")?)
            .await?;
        // Connects and disconnects in flight: dropped (cancelled) with the
        // body, so a stopped service keeps no call, and no connection.
        // Each brings back a failure: the device's address and why.
        let mut calls: tokio::task::JoinSet<Option<(String, String)>> = tokio::task::JoinSet::new();
        loop {
            let mut objects = read(&daemon)
                .await
                .map_err(|e| ServiceError(format!("BlueZ did not answer: {e}")))?;
            if !cx.update(|s| *s = Bluetooth::from_objects(&objects)) {
                return Ok(());
            }
            cx.ready();
            'follow: loop {
                tokio::select! {
                    Some(done) = calls.join_next(), if !calls.is_empty() => {
                        if let Ok(Some(failure)) = done
                            && !cx.emit(BluetoothEvent::Failed(failure))
                        {
                            return Ok(());
                        }
                    }
                    ev = daemon.next() => match ev {
                        None => return Err(ServiceError("the system bus connection ended".into())),
                        Some(DaemonEvent::Owner) => break 'follow,
                        Some(DaemonEvent::Signal(m)) => {
                            if signal(&daemon, &mut objects, &m).await
                                && !cx.update(|s| *s = Bluetooth::from_objects(&objects))
                            {
                                return Ok(());
                            }
                        }
                    },
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Write(w)) if w.field == "powered" => {
                            let on: bool = w.value().unwrap_or(cx.state().powered);
                            let adapter = objects
                                .iter()
                                .find(|(_, i)| i.contains_key(ADAPTER))
                                .map(|(p, _)| p.clone());
                            let ok = match adapter {
                                Some(path) => dbus::set(&conn, BLUEZ, &path, ADAPTER, "Powered", on.into())
                                    .await
                                    .map_err(|e| e.to_string()),
                                None => Err("no adapter".to_string()),
                            };
                            let now = match ok {
                                Ok(()) => on,
                                Err(e) => {
                                    log::warn!("bluetooth: not powered {}: {e}", if on { "on" } else { "off" });
                                    // The adapter as last read.
                                    Bluetooth::from_objects(&objects).powered
                                }
                            };
                            if !cx.report(&w, |s| s.powered = now) {
                                return Ok(());
                            }
                        }
                        Some(Msg::Action(a)) => {
                            let (item, method) = match a {
                                BluetoothAction::Connect { item } => (item, "Connect"),
                                BluetoothAction::Disconnect { item } => (item, "Disconnect"),
                            };
                            let path = objects.iter().find_map(|(p, i)| {
                                let d = i.get(DEVICE)?;
                                (dbus::text(d, "Address").as_deref() == Some(item.address.as_str()))
                                    .then(|| p.clone())
                            });
                            let Some(path) = path else {
                                log::warn!("bluetooth: no device {}", item.address);
                                if !cx.emit(BluetoothEvent::Failed((item.address, "no such device".into()))) {
                                    return Ok(());
                                }
                                continue;
                            };
                            // Connecting can take seconds: do not hold up
                            // the service; BlueZ reports the outcome as
                            // `Connected` changes.
                            let conn = conn.clone();
                            let address = item.address;
                            calls.spawn(async move {
                                let call = conn.call_method(Some(BLUEZ), path.as_str(), Some(DEVICE), method, &());
                                let error = match tokio::time::timeout(std::time::Duration::from_secs(30), call).await {
                                    Ok(Ok(_)) => return None,
                                    // BlueZ's own words ("Page Timeout",
                                    // "Host is down").
                                    Ok(Err(zbus::Error::MethodError(_, Some(text), _))) => text,
                                    Ok(Err(e)) => e.to_string(),
                                    Err(_) => "timed out".to_string(),
                                };
                                log::warn!("bluetooth: {method} {path}: {error}");
                                Some((address, error))
                            });
                        }
                        Some(_) => {}
                    },
                }
            }
        }
    }
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

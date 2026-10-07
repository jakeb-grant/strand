//! What the D-Bus services share: following a daemon by its bus name
//! (its owner coming, going and restarting: `NameOwnerChanged`), reading
//! properties, and the signals a daemon sends, filtered by its current
//! owner.
//!
//! Every service following a daemon has the same shape ([`Daemon`]):
//! subscribe to the name's owner changes and to the daemon's signals
//! first, then read its state; a new owner (the daemon restarted) is read
//! afresh without the service restarting, and no owner is the state's
//! defaults. Nothing polls: an idle daemon wakes nothing. A daemon that
//! is not running is asked for once per start (D-Bus activation:
//! distributions often start UPower or bluetoothd only when first called),
//! without waiting: its arrival is an owner change like any other.

use std::collections::HashMap;

use futures_lite::StreamExt;
use zbus::message::Type as MessageType;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MatchRule, Message, MessageStream};

/// An object's properties on one interface, by name.
pub type Props = HashMap<String, OwnedValue>;

/// `org.freedesktop.DBus.Properties`.
pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// How long a call to an app on the bus may take (a tray item, a media
/// player): an app with its main loop blocked must not hold up the
/// service behind it.
pub const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `call`, given up after [`CALL_TIMEOUT`].
pub async fn timed<T>(call: impl std::future::Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("no answer in time".into())))
}

/// A property as `T`, if present and of that type.
pub fn prop<'a, T>(props: &'a Props, name: &str) -> Option<T>
where
    T: TryFrom<&'a Value<'a>>,
    <T as TryFrom<&'a Value<'a>>>::Error: Into<zbus::zvariant::Error>,
{
    props.get(name).and_then(|v| v.downcast_ref::<T>().ok())
}

/// A text property (owned).
pub fn text(props: &Props, name: &str) -> Option<String> {
    props
        .get(name)
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .map(str::to_string)
}

/// A number property of any numeric D-Bus type, as `f64`.
pub fn number(props: &Props, name: &str) -> Option<f64> {
    let v: &Value<'_> = props.get(name)?;
    let v = match v {
        Value::Value(inner) => &**inner,
        v => v,
    };
    Some(match v {
        Value::U8(n) => f64::from(*n),
        Value::I16(n) => f64::from(*n),
        Value::U16(n) => f64::from(*n),
        Value::I32(n) => f64::from(*n),
        Value::U32(n) => f64::from(*n),
        Value::I64(n) => *n as f64,
        Value::U64(n) => *n as f64,
        Value::F64(n) => *n,
        _ => return None,
    })
}

/// A boolean property.
pub fn boolean(props: &Props, name: &str) -> Option<bool> {
    prop::<bool>(props, name)
}

/// Every property of `iface` on `path` of `dest` (`GetAll`).
pub async fn get_all(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
) -> zbus::Result<Props> {
    let reply = conn
        .call_method(Some(dest), path, Some(PROPERTIES), "GetAll", &(iface,))
        .await?;
    reply.body().deserialize::<Props>()
}

/// One property (`Get`).
pub async fn get(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
) -> zbus::Result<OwnedValue> {
    let reply = conn
        .call_method(Some(dest), path, Some(PROPERTIES), "Get", &(iface, name))
        .await?;
    reply.body().deserialize::<OwnedValue>()
}

/// Set one property (`Set`).
pub async fn set(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
    value: Value<'_>,
) -> zbus::Result<()> {
    conn.call_method(
        Some(dest),
        path,
        Some(PROPERTIES),
        "Set",
        &(iface, name, value),
    )
    .await?;
    Ok(())
}

/// A `PropertiesChanged` signal, taken apart.
#[derive(Debug)]
pub struct Changed {
    /// The object.
    pub path: String,
    /// The interface whose properties changed.
    pub iface: String,
    /// The new values.
    pub changed: Props,
    /// Properties that changed without their values (read them again).
    pub invalidated: Vec<String>,
}

/// `msg` as a `PropertiesChanged` signal, if it is one.
pub fn properties_changed(msg: &Message) -> Option<Changed> {
    let h = msg.header();
    if h.interface().map(|i| i.as_str()) != Some(PROPERTIES)
        || h.member().map(|m| m.as_str()) != Some("PropertiesChanged")
    {
        return None;
    }
    let path = h.path()?.to_string();
    let (iface, changed, invalidated) = msg
        .body()
        .deserialize::<(String, Props, Vec<String>)>()
        .ok()?;
    Some(Changed {
        path,
        iface,
        changed,
        invalidated,
    })
}

/// Apply a `PropertiesChanged` to `props`, reading invalidated
/// properties again.
pub async fn apply_changed(conn: &Connection, dest: &str, props: &mut Props, c: Changed) {
    props.extend(c.changed);
    for name in c.invalidated {
        match get(conn, dest, &c.path, &c.iface, &name).await {
            Ok(v) => {
                props.insert(name, v);
            }
            Err(_) => {
                props.remove(&name);
            }
        }
    }
}

/// The signal's member name.
pub fn member(msg: &Message) -> Option<String> {
    msg.header().member().map(|m| m.to_string())
}

/// The signal's interface.
pub fn interface(msg: &Message) -> Option<String> {
    msg.header().interface().map(|m| m.to_string())
}

/// The message's object path.
pub fn path(msg: &Message) -> Option<String> {
    msg.header().path().map(|p| p.to_string())
}

/// A daemon followed by its well-known name: its owner changes, and the
/// signals it sends that the daemon's subscriptions match (each a match
/// rule of its own, added and dropped as the service needs them: a scan's
/// access points only while watched).
pub struct Daemon {
    conn: Connection,
    name: BusName<'static>,
    dbus: zbus::fdo::DBusProxy<'static>,
    owners: zbus::fdo::NameOwnerChangedStream,
    subscriptions: Vec<(String, MessageStream)>,
    owner: Option<OwnedUniqueName>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("name", &self.name)
            .field("owner", &self.owner)
            .field(
                "subscriptions",
                &self.subscriptions.iter().map(|s| &s.0).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// What [`Daemon::next`] saw.
#[derive(Debug)]
pub enum DaemonEvent {
    /// The name's owner changed (the daemon went, came or restarted):
    /// read everything again ([`Daemon::owner`] says who, if anyone).
    Owner,
    /// A signal from the current owner.
    Signal(Message),
}

/// A signal rule: the signals of objects under `namespace`.
pub fn namespace_rule(namespace: &str) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .path_namespace(namespace.to_string())?
        .build())
}

/// A signal rule: the signals of the object at `path`.
pub fn path_rule(path: &str) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .path(path.to_string())?
        .build())
}

impl Daemon {
    /// Follow `name` on `conn`, with the signals of the daemon's objects
    /// under `namespace` (`/` for all). Subscribes before the first read,
    /// so nothing is missed between them.
    pub async fn follow(conn: &Connection, name: &str, namespace: &str) -> zbus::Result<Daemon> {
        let mut d = Self::new(conn, name).await?;
        d.subscribe("main", namespace_rule(namespace)?).await?;
        Ok(d)
    }

    /// Follow `name` on `conn` with no signal subscription yet
    /// ([`Daemon::subscribe`]); its owner is asked for once the
    /// owner-change subscription is in place.
    pub async fn new(conn: &Connection, name: &str) -> zbus::Result<Daemon> {
        let name = BusName::try_from(name.to_string())?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        let owners = dbus
            .receive_name_owner_changed_with_args(&[(0, name.as_str())])
            .await?;
        let owner = dbus.get_name_owner(name.clone()).await.ok();
        if owner.is_none() {
            activate(conn, name.as_str()).await;
        }
        Ok(Daemon {
            conn: conn.clone(),
            name,
            dbus,
            owners,
            subscriptions: Vec::new(),
            owner,
        })
    }

    /// Add the signals `rule` matches, under `key` (replacing a
    /// subscription of the same key).
    pub async fn subscribe(&mut self, key: &str, rule: MatchRule<'_>) -> zbus::Result<()> {
        let stream = MessageStream::for_match_rule(rule.to_owned(), &self.conn, Some(256)).await?;
        self.unsubscribe(key);
        self.subscriptions.push((key.to_string(), stream));
        Ok(())
    }

    /// Drop the subscription `key` (its match rule goes with it).
    pub fn unsubscribe(&mut self, key: &str) {
        self.subscriptions.retain(|(k, _)| k != key);
    }

    /// Whether subscription `key` is in place.
    pub fn subscribed(&self, key: &str) -> bool {
        self.subscriptions.iter().any(|(k, _)| k == key)
    }

    /// The connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The name's current owner (`None`: the daemon is not running).
    pub fn owner(&self) -> Option<&OwnedUniqueName> {
        self.owner.as_ref()
    }

    /// The bus name followed.
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// `msg` was sent by the name's current owner.
    pub fn from_owner(&self, msg: &Message) -> bool {
        match (&self.owner, msg.header().sender()) {
            (Some(o), Some(s)) => o.as_str() == s.as_str(),
            _ => false,
        }
    }

    /// The next owner change or signal from the owner. `None`: the
    /// connection ended (the bus went away).
    pub async fn next(&mut self) -> Option<DaemonEvent> {
        use std::task::Poll;
        loop {
            let got = std::future::poll_fn(|cx| {
                match self.owners.poll_next(cx) {
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Ready(Some(o)) => {
                        let owner = o.args().ok().and_then(|a| {
                            a.new_owner()
                                .as_ref()
                                .map(|n| OwnedUniqueName::from(n.to_owned()))
                        });
                        return Poll::Ready(Some(Err(owner)));
                    }
                    Poll::Pending => {}
                }
                for (_, s) in &mut self.subscriptions {
                    loop {
                        match s.poll_next(cx) {
                            Poll::Ready(None) => return Poll::Ready(None),
                            Poll::Ready(Some(Ok(m))) => return Poll::Ready(Some(Ok(m))),
                            // A message that did not parse: skip it (and
                            // poll again, so the waker stays registered).
                            Poll::Ready(Some(Err(_))) => continue,
                            Poll::Pending => break,
                        }
                    }
                }
                Poll::Pending
            })
            .await?;
            match got {
                Err(owner) => {
                    self.owner = owner;
                    return Some(DaemonEvent::Owner);
                }
                Ok(m) if self.from_owner(&m) => return Some(DaemonEvent::Signal(m)),
                Ok(_) => {}
            }
        }
    }

    /// Ask the bus for the owner again (after a failed read, say).
    pub async fn refresh_owner(&mut self) {
        self.owner = self.dbus.get_name_owner(self.name.clone()).await.ok();
    }
}

/// Ask the bus to start `name`'s service (`StartServiceByName`, D-Bus
/// activation), without waiting for the reply: the service owning its
/// name is a `NameOwnerChanged` for whoever follows it. A name nothing
/// can start is ignored.
pub async fn activate(conn: &Connection, name: &str) {
    let msg = Message::method_call("/org/freedesktop/DBus", "StartServiceByName")
        .and_then(|b| b.destination("org.freedesktop.DBus"))
        .and_then(|b| b.interface("org.freedesktop.DBus"))
        .and_then(|b| b.with_flags(zbus::message::Flags::NoReplyExpected))
        .and_then(|b| b.build(&(name, 0u32)));
    match msg {
        Ok(m) => {
            if let Err(e) = conn.send(&m).await {
                log::debug!("{name} not started: {e}");
            }
        }
        Err(e) => log::debug!("{name} not started: {e}"),
    }
}

/// Every object path in a `ao` reply body.
pub fn paths(v: Vec<OwnedObjectPath>) -> Vec<String> {
    v.into_iter().map(|p| p.to_string()).collect()
}

/// The process owning `name` on `conn`, for a diagnostic: its pid and
/// command name (`/proc/<pid>/comm`), when the bus says.
pub async fn owner_process(conn: &Connection, name: &str) -> Option<(u32, Option<String>)> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
    let bus_name = BusName::try_from(name.to_string()).ok()?;
    let pid = dbus.get_connection_unix_process_id(bus_name).await.ok()?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some((pid, comm))
}

/// A service whose bus cannot be reached: ready at its defaults. A bus
/// disabled outright keeps it that way until stopped; any other failure
/// ends the run with an error (the client retries with its backoff).
pub(crate) async fn idle_without_bus<S: crate::Service>(
    cx: &mut crate::Cx<S>,
    which: &str,
    e: zbus::Error,
) -> Result<(), crate::ServiceError> {
    cx.ready();
    let disabled = match which {
        "system" => cx.buses().system == crate::Bus::Disabled,
        _ => cx.buses().session == crate::Bus::Disabled,
    };
    if disabled {
        log::debug!("{}: no {which} bus: {e}", cx.name());
        while cx.recv().await.is_some() {}
        return Ok(());
    }
    Err(crate::ServiceError(format!("no {which} bus: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_of_any_width_read_as_f64() {
        let mut p = Props::new();
        p.insert("a".into(), OwnedValue::from(3u32));
        p.insert("b".into(), OwnedValue::from(-2i64));
        p.insert("c".into(), OwnedValue::from(0.5f64));
        p.insert("d".into(), OwnedValue::from(7u8));
        p.insert("e".into(), OwnedValue::from(true));
        assert_eq!(number(&p, "a"), Some(3.0));
        assert_eq!(number(&p, "b"), Some(-2.0));
        assert_eq!(number(&p, "c"), Some(0.5));
        assert_eq!(number(&p, "d"), Some(7.0));
        assert_eq!(number(&p, "e"), None);
        assert_eq!(number(&p, "missing"), None);
        assert_eq!(boolean(&p, "e"), Some(true));
    }
}

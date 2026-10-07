//! What the D-Bus services share: following a daemon by its bus name
//! (its owner coming, going and restarting: `NameOwnerChanged`), reading
//! properties, and the signals a daemon sends, filtered by its current
//! owner.
//!
//! Every service following a daemon has the same shape ([`Daemon`]):
//! subscribe to the name's owner changes and to the daemon's signals
//! first, then read its state; a new owner (the daemon restarted) is read
//! afresh without the service restarting, and no owner is the state's
//! defaults. Nothing polls: an idle daemon wakes nothing.

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
/// signals it sends under a path namespace.
pub struct Daemon {
    conn: Connection,
    name: BusName<'static>,
    dbus: zbus::fdo::DBusProxy<'static>,
    owners: zbus::fdo::NameOwnerChangedStream,
    signals: MessageStream,
    owner: Option<OwnedUniqueName>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("name", &self.name)
            .field("owner", &self.owner)
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

impl Daemon {
    /// Follow `name` on `conn`, with the signals of the daemon's objects
    /// under `namespace` (`/` for all). Subscribes before the first read,
    /// so nothing is missed between them.
    pub async fn follow(conn: &Connection, name: &str, namespace: &str) -> zbus::Result<Daemon> {
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .path_namespace(namespace.to_string())?
            .build();
        Self::follow_rule(conn, name, rule).await
    }

    /// Follow `name` with the signals `rule` matches.
    pub async fn follow_rule(
        conn: &Connection,
        name: &str,
        rule: MatchRule<'_>,
    ) -> zbus::Result<Daemon> {
        let name = BusName::try_from(name.to_string())?;
        let dbus = zbus::fdo::DBusProxy::new(conn).await?;
        let owners = dbus
            .receive_name_owner_changed_with_args(&[(0, name.as_str())])
            .await?;
        let signals = MessageStream::for_match_rule(rule.to_owned(), conn, Some(256)).await?;
        let owner = dbus.get_name_owner(name.clone()).await.ok();
        Ok(Daemon {
            conn: conn.clone(),
            name,
            dbus,
            owners,
            signals,
            owner,
        })
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

    /// The next owner change or signal from the owner. `None`: the
    /// connection ended (the bus went away).
    pub async fn next(&mut self) -> Option<DaemonEvent> {
        loop {
            tokio::select! {
                o = self.owners.next() => {
                    let o = o?;
                    let owner = o.args().ok().and_then(|a| a.new_owner().as_ref().map(|n| OwnedUniqueName::from(n.to_owned())));
                    self.owner = owner;
                    return Some(DaemonEvent::Owner);
                }
                m = self.signals.next() => {
                    let m = m?;
                    let Ok(m) = m else { continue };
                    let from_owner = match (&self.owner, m.header().sender()) {
                        (Some(o), Some(s)) => o.as_str() == s.as_str(),
                        _ => false,
                    };
                    if from_owner {
                        return Some(DaemonEvent::Signal(m));
                    }
                }
            }
        }
    }

    /// Ask the bus for the owner again (after a failed read, say).
    pub async fn refresh_owner(&mut self) {
        self.owner = self.dbus.get_name_owner(self.name.clone()).await.ok();
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

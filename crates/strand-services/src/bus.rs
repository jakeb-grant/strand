//! Which D-Bus buses services talk to.
//!
//! `strand run` uses the session and system buses of its environment.
//! Tests hand the runtime private buses ([`Buses::private`], a
//! `dbus-daemon` they spawned: see [`crate::testing`]), so no test ever
//! talks to the machine's real buses.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// One bus.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Bus {
    /// The environment's (`DBUS_SESSION_BUS_ADDRESS`, the system bus
    /// socket).
    #[default]
    Default,
    /// An explicit address (a private test bus).
    Address(String),
    /// None at all: connecting fails (services keep their defaults).
    Disabled,
}

/// The session and system buses services use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Buses {
    pub session: Bus,
    pub system: Bus,
}

impl Buses {
    /// Both buses at one private address (a test's `dbus-daemon`).
    pub fn private(address: &str) -> Buses {
        Buses {
            session: Bus::Address(address.to_string()),
            system: Bus::Address(address.to_string()),
        }
    }

    /// No buses (unit tests of services that need none).
    pub fn none() -> Buses {
        Buses {
            session: Bus::Disabled,
            system: Bus::Disabled,
        }
    }
}

/// How long connecting to a bus may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Which {
    Session,
    System,
}

/// One bus's connection slot: its lock makes concurrent first users
/// share one connect.
type Slot = Rc<tokio::sync::Mutex<Option<zbus::Connection>>>;

thread_local! {
    /// The connections of this services thread, shared by its services
    /// (one session and one system connection per runtime). Dropped when
    /// the last body on the thread ends ([`forget`]).
    static CONNECTIONS: RefCell<Vec<((Which, Bus), Slot)>> = const { RefCell::new(Vec::new()) };
}

fn slot(which: Which, bus: &Bus) -> Slot {
    CONNECTIONS.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((_, s)) = c.iter().find(|(k, _)| k.0 == which && &k.1 == bus) {
            return s.clone();
        }
        let s = Slot::default();
        c.push(((which, bus.clone()), s.clone()));
        s
    })
}

/// A cached connection still answers (the daemon may have restarted).
async fn alive(conn: &zbus::Connection) -> bool {
    let ping = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus.Peer"),
        "Ping",
        &(),
    );
    matches!(tokio::time::timeout(CONNECT_TIMEOUT, ping).await, Ok(Ok(_)))
}

async fn connect(which: Which, bus: &Bus) -> zbus::Result<zbus::Connection> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(zbus::Error::Failure(
            "no tokio runtime on this thread: a service on a thread of its own runs one to use the buses".into(),
        ));
    }
    let slot = slot(which, bus);
    let mut held = slot.lock().await;
    if let Some(conn) = held.as_ref() {
        if alive(conn).await {
            return Ok(conn.clone());
        }
        // Dead (the bus restarted): connect afresh.
        *held = None;
    }
    let fresh = async {
        match (which, bus) {
            (_, Bus::Disabled) => Err(zbus::Error::Failure("no bus".into())),
            (Which::Session, Bus::Default) => zbus::Connection::session().await,
            (Which::System, Bus::Default) => zbus::Connection::system().await,
            (_, Bus::Address(a)) => {
                zbus::connection::Builder::address(a.as_str())?
                    .build()
                    .await
            }
        }
    };
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, fresh)
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("connecting timed out".into())))?;
    *held = Some(conn.clone());
    Ok(conn)
}

/// The session bus connection of this thread (connected on first use).
pub async fn session(buses: &Buses) -> zbus::Result<zbus::Connection> {
    connect(Which::Session, &buses.session).await
}

/// The system bus connection of this thread (connected on first use).
pub async fn system(buses: &Buses) -> zbus::Result<zbus::Connection> {
    connect(Which::System, &buses.system).await
}

/// A connection of its own to the session bus, not shared with other
/// services: what a service that owns a bus name uses (the notification
/// server, the tray's watcher), so the name goes with the connection when
/// the service stops.
pub async fn own_session(buses: &Buses) -> zbus::Result<zbus::Connection> {
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(zbus::Error::Failure(
            "no tokio runtime on this thread".into(),
        ));
    }
    let fresh = async {
        match &buses.session {
            Bus::Disabled => Err(zbus::Error::Failure("no bus".into())),
            Bus::Default => zbus::Connection::session().await,
            Bus::Address(a) => {
                zbus::connection::Builder::address(a.as_str())?
                    .build()
                    .await
            }
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT, fresh)
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("connecting timed out".into())))
}

/// Forget this thread's connections (the shared runtime's thread ending).
pub(crate) fn forget() {
    CONNECTIONS.with(|c| c.borrow_mut().clear());
}

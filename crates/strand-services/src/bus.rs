//! Which D-Bus buses services talk to.
//!
//! `strand run` uses the session and system buses of its environment.
//! Tests hand the runtime private buses ([`Buses::private`], a
//! `dbus-daemon` they spawned: see [`crate::testing`]), so no test ever
//! talks to the machine's real buses.

use std::cell::RefCell;
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

thread_local! {
    /// The connections of this services thread, shared by its services
    /// (one session and one system connection per runtime).
    static CONNECTIONS: RefCell<Vec<((Which, Bus), zbus::Connection)>> = const { RefCell::new(Vec::new()) };
}

async fn connect(which: Which, bus: &Bus) -> zbus::Result<zbus::Connection> {
    let cached = CONNECTIONS.with(|c| {
        c.borrow()
            .iter()
            .find(|(k, _)| k.0 == which && &k.1 == bus)
            .map(|(_, conn)| conn.clone())
    });
    if let Some(conn) = cached {
        return Ok(conn);
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
    CONNECTIONS.with(|c| c.borrow_mut().push(((which, bus.clone()), conn.clone())));
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

/// Forget this thread's connections (the shared runtime's thread ending).
pub(crate) fn forget() {
    CONNECTIONS.with(|c| c.borrow_mut().clear());
}

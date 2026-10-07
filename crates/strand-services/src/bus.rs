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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Which {
    Session,
    System,
}

/// One bus's connection slot: its lock makes concurrent first users
/// share one connect.
type Slot = Rc<tokio::sync::Mutex<Option<zbus::Connection>>>;

/// A shared connection: its bus, its slot, and how many running bodies
/// use it.
type Shared = ((Which, Bus), Slot, usize);

thread_local! {
    /// The connections of this services thread, shared by its services
    /// (one session and one system connection per runtime), each with
    /// how many running bodies use it: a connection is dropped when the
    /// last body that used it ends ([`User`]), and all of them when the
    /// thread's last body does ([`forget`]).
    static CONNECTIONS: RefCell<Vec<Shared>> = const { RefCell::new(Vec::new()) };
}

tokio::task_local! {
    /// The body running in this task (a service body on the shared
    /// runtime, [`with_user`]).
    static USER: Rc<User>;
}

/// One body's hold on the shared connections it used: dropped with the
/// body (stopped, returned or failed), it lets go of them, and a
/// connection no running body uses is closed. A bar reading only `cpu`
/// keeps no bus connection because `battery` once ran.
#[derive(Default)]
pub(crate) struct User {
    held: RefCell<Vec<(Which, Bus)>>,
}

impl User {
    fn hold(&self, which: Which, bus: &Bus) {
        let key = (which, bus.clone());
        if self.held.borrow().contains(&key) {
            return;
        }
        CONNECTIONS.with(|c| {
            if let Some(e) = c.borrow_mut().iter_mut().find(|e| e.0 == key) {
                e.2 += 1;
            }
        });
        self.held.borrow_mut().push(key);
    }
}

impl Drop for User {
    fn drop(&mut self) {
        let held = std::mem::take(&mut *self.held.borrow_mut());
        // Taken out under the borrow, dropped after it (dropping a
        // connection may run code that looks here again).
        let closed: Vec<Slot> = CONNECTIONS
            .try_with(|c| {
                let mut c = c.borrow_mut();
                for key in &held {
                    if let Some(e) = c.iter_mut().find(|e| &e.0 == key) {
                        e.2 = e.2.saturating_sub(1);
                    }
                }
                let mut closed = Vec::new();
                c.retain(|e| {
                    let unused = e.2 == 0 && held.contains(&e.0);
                    if unused {
                        closed.push(e.1.clone());
                    }
                    !unused
                });
                closed
            })
            .unwrap_or_default();
        drop(closed);
    }
}

/// Run `body` as one user of this thread's shared connections ([`User`]).
pub(crate) async fn with_user<F: std::future::Future>(body: F) -> F::Output {
    USER.scope(Rc::new(User::default()), body).await
}

/// The slot for `which`/`bus`, held by the body asking (before it
/// connects: a body ending meanwhile does not close it under this one).
fn slot(which: Which, bus: &Bus) -> Slot {
    let s = CONNECTIONS.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((_, s, _)) = c.iter().find(|(k, _, _)| k.0 == which && &k.1 == bus) {
            return s.clone();
        }
        let s = Slot::default();
        c.push(((which, bus.clone()), s.clone(), 0));
        s
    });
    let _ = USER.try_with(|u| u.hold(which, bus));
    s
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

//! (services-round) `idle`: whether the user has been away, from the
//! compositor's `ext_idle_notifier_v1` (ext-idle-notify-v1, which sway,
//! Hyprland, niri and labwc implement).
//!
//! `idle.after(timeout)` is true once the seat has had no input for
//! `timeout`, and false again at the next input. The protocol only tells
//! a client about timeouts it asked for, so each distinct timeout a
//! config reads is one `ext_idle_notification_v1`, made the first time
//! `after` is evaluated with it ([`want`]) and kept while some
//! evaluation still reads it: each evaluation counts a reader of its
//! timeout until it re-runs or its scope goes ([`unwant`]), and a
//! timeout no evaluation has read for [`LET_GO`] has its notification
//! destroyed (a reload that changes the timeout, a slider driving it,
//! leave no compositor timer behind). `timeouts` lists those followed
//! with their state, keyed by the timeout, and `after` reads it (a
//! tracked read, so a reader follows it). The
//! notifications are the plain idle ones, which an idle inhibitor (a
//! video playing, a surface's `inhibit_idle: true`) holds back, so a
//! screensaver never covers what inhibits idling. `available` is the
//! notifier and a seat being offered; without them `after` is false.
//!
//! The service runs on a thread of its own (`strand-idle`) with its own
//! Wayland connection, asleep in `poll(2)` on its socket and an eventfd
//! that [`want`], [`unwant`] and the service's messages poke (and, while
//! a timeout nobody reads waits out [`LET_GO`], until then), so it costs
//! no wakeup between the compositor's `idled` and `resumed`. It connects to
//! `$WAYLAND_DISPLAY` ([`configure`] points it elsewhere: tests). A lost
//! connection clears `available` and `timeouts` and ends the run with an
//! error, which the contract retries with its backoff.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags};
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

use crate::wm::WaylandTarget;
use crate::{Cx, Data, Msg, ServiceError, Store, ToData, service};

/// The schema the `idle` service serves.
pub const SCHEMA: &str = strand_services_schema::IDLE;

/// The longest timeout the protocol carries (milliseconds in a `u32`,
/// about 49 days); a longer one is asked as this.
pub const MAX_TIMEOUT: Duration = Duration::from_millis(u32::MAX as u64);

static TARGET: Mutex<Option<WaylandTarget>> = Mutex::new(None);

/// Which display `idle` connects to from its next start on (tests: a
/// headless sway's socket). `None` restores `$WAYLAND_DISPLAY`. A running
/// service keeps its connection until it stops.
pub fn configure(target: Option<WaylandTarget>) {
    *TARGET.lock().unwrap_or_else(PoisonError::into_inner) = target;
}

fn target() -> WaylandTarget {
    TARGET
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or(WaylandTarget::Env)
}

/// How long a timeout no evaluation reads keeps its notification: long
/// enough for a re-evaluation (its reader released, then counted again)
/// or a reload (the old tree goes, the new one mounts) to read it again
/// without the compositor's timer starting over.
pub const LET_GO: Duration = Duration::from_secs(1);

/// The readers of one timeout.
#[derive(Debug, Default)]
struct Readers {
    count: usize,
    /// When the last reader went (none while any reads it).
    unread_since: Option<Instant>,
}

/// The timeouts `after` is asked about, in this process.
static WANTED: Mutex<BTreeMap<Duration, Readers>> = Mutex::new(BTreeMap::new());

/// The running services' eventfds, poked when what is wanted changes.
static WAKERS: Mutex<Vec<Weak<OwnedFd>>> = Mutex::new(Vec::new());

/// `timeout` as the protocol carries it.
fn clamp(timeout: Duration) -> Duration {
    Duration::from_millis(timeout.min(MAX_TIMEOUT).as_millis() as u64)
}

fn poke() {
    let mut wakers = WAKERS.lock().unwrap_or_else(PoisonError::into_inner);
    wakers.retain(|w| match w.upgrade() {
        Some(fd) => {
            let _ = rustix::io::write(&*fd, &1u64.to_ne_bytes());
            true
        }
        None => false,
    });
}

/// `idle.after(timeout)` was evaluated: one more reader of `timeout`,
/// which a running service follows (one notification object each) until
/// its readers have all gone ([`unwant`]). A new one wakes the running
/// services to make it.
pub fn want(timeout: Duration) {
    let timeout = clamp(timeout);
    let new = {
        let mut wanted = WANTED.lock().unwrap_or_else(PoisonError::into_inner);
        let new = !wanted.contains_key(&timeout);
        let r = wanted.entry(timeout).or_default();
        r.count += 1;
        r.unread_since = None;
        new
    };
    if new {
        poke();
    }
}

/// A reader [`want`] counted went (the evaluation re-ran or its scope
/// was disposed). The last one starts [`LET_GO`].
pub fn unwant(timeout: Duration) {
    let timeout = clamp(timeout);
    let last = {
        let mut wanted = WANTED.lock().unwrap_or_else(PoisonError::into_inner);
        match wanted.get_mut(&timeout) {
            Some(r) if r.count > 0 => {
                r.count -= 1;
                if r.count == 0 {
                    r.unread_since = Some(Instant::now());
                }
                r.count == 0
            }
            _ => false,
        }
    };
    if last {
        poke();
    }
}

/// The timeouts to follow at `now`, and when the next one nobody reads
/// is let go. Those let go are forgotten.
fn wanted(now: Instant) -> (BTreeSet<Duration>, Option<Instant>) {
    let mut wanted = WANTED.lock().unwrap_or_else(PoisonError::into_inner);
    wanted.retain(|_, r| r.unread_since.is_none_or(|t| now < t + LET_GO));
    let next = wanted
        .values()
        .filter_map(|r| r.unread_since)
        .min()
        .map(|t| t + LET_GO);
    (wanted.keys().copied().collect(), next)
}

/// A timeout `idle.after` was asked about, and whether the seat has been
/// idle that long: the schema's `IdleTimeout`.
#[derive(crate::Data, Clone, Debug, Default, PartialEq, Eq)]
#[data(name = "IdleTimeout", key = timeout)]
pub struct IdleTimeout {
    /// How long without input it waits for.
    pub timeout: Duration,
    /// The seat has had no input for at least `timeout`.
    pub idle: bool,
}

/// `idle`: see the module docs.
#[service(name = "idle", fns = idle_fns, thread)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Idle {
    /// The compositor tells how long the user has been away
    /// (ext-idle-notify-v1); while false, `idle.after` is false.
    pub available: bool,
    /// The timeouts `idle.after` was asked about, each with whether the
    /// seat has been idle that long, by timeout.
    #[store(keyed)]
    pub timeouts: Vec<IdleTimeout>,
}

/// `idle.after(timeout)`: asks the service to follow `timeout` and reads
/// its state (false until the compositor says otherwise).
fn idle_fns(
    cells: &IdleCells,
    rt: &strand_core::Runtime,
    method: &str,
    args: &[Data],
) -> Option<Result<Data, strand_core::Error>> {
    match method {
        "after" => {
            let Some(Data::Duration(timeout)) = args.first() else {
                return Some(Err(strand_core::Error::failed(
                    "idle.after takes a duration".to_string(),
                )));
            };
            let timeout = clamp(*timeout);
            // Read until this evaluation re-runs or its scope goes.
            want(timeout);
            rt.on_cleanup(move || unwant(timeout));
            Some(
                cells
                    .timeouts
                    .with(rt, |v| v.get(&timeout).is_some_and(|t| t.idle).to_data()),
            )
        }
        _ => None,
    }
}

#[derive(Default)]
struct Client {
    seat: Option<wl_seat::WlSeat>,
    notifier: Option<ExtIdleNotifierV1>,
    /// The notifier's global name, to see it removed.
    notifier_global: Option<u32>,
    /// Each timeout followed, with its notification and its state.
    notifications: BTreeMap<Duration, (ExtIdleNotificationV1, bool)>,
    /// A state changed since it was last published.
    changed: bool,
    /// The registry's first sync is answered.
    synced: bool,
    /// The notifier went away: the run ends (and is retried).
    lost: bool,
}

impl Client {
    fn snapshot(&self) -> Vec<IdleTimeout> {
        self.notifications
            .iter()
            .map(|(t, (_, idle))| IdleTimeout {
                timeout: *t,
                idle: *idle,
            })
            .collect()
    }
}

fn socket(target: WaylandTarget) -> io::Result<std::path::PathBuf> {
    match target {
        WaylandTarget::Socket(p) => Ok(p),
        WaylandTarget::Env => crate::wm::protocol::display_socket(
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        ),
    }
}

impl Idle {
    fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let wake = Arc::new(
            rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .map_err(io::Error::from)?,
        );
        let poke = wake.clone();
        cx.set_notify(move || {
            let _ = rustix::io::write(&*poke, &1u64.to_ne_bytes());
        });
        WAKERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&wake));
        let r = Self::serve(&mut cx, &wake);
        if r.is_err() {
            // What it showed no longer holds.
            cx.update(|s| {
                s.available = false;
                s.timeouts.clear();
            });
        }
        r
    }

    fn serve(cx: &mut Cx<Self>, wake: &OwnedFd) -> Result<(), ServiceError> {
        let path = socket(target())?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| ServiceError(format!("{}: {e}", path.display())))?;
        let conn = Connection::from_socket(stream).map_err(|e| ServiceError(e.to_string()))?;
        let mut queue: EventQueue<Client> = conn.new_event_queue();
        let qh = queue.handle();
        let display = conn.display();
        let _registry = display.get_registry(&qh, ());
        display.sync(&qh, ());
        let mut client = Client::default();
        loop {
            queue
                .dispatch_pending(&mut client)
                .map_err(|e| ServiceError(e.to_string()))?;
            if client.lost {
                return Err(ServiceError(
                    "the compositor withdrew its idle notifier".into(),
                ));
            }
            loop {
                match cx.try_recv() {
                    Ok(Msg::Action(a)) => match a {},
                    Ok(_) => {}
                    Err(false) => break,
                    Err(true) => {
                        let _ = conn.flush();
                        return Ok(());
                    }
                }
            }
            let mut let_go = None;
            if client.synced {
                // Each timeout read: one notification; one nobody reads
                // any more: destroyed.
                let (want, next) = wanted(Instant::now());
                let_go = next;
                client.notifications.retain(|t, (n, _)| {
                    let keep = want.contains(t);
                    if !keep {
                        n.destroy();
                        client.changed = true;
                    }
                    keep
                });
                if let (Some(notifier), Some(seat)) = (&client.notifier, &client.seat) {
                    for t in want {
                        if client.notifications.contains_key(&t) {
                            continue;
                        }
                        let ms = u32::try_from(t.as_millis()).unwrap_or(u32::MAX);
                        let n = notifier.get_idle_notification(ms, seat, &qh, t);
                        client.notifications.insert(t, (n, false));
                        client.changed = true;
                    }
                }
                if std::mem::take(&mut client.changed) || !cx.is_ready() {
                    let available = client.notifier.is_some() && client.seat.is_some();
                    let timeouts = client.snapshot();
                    cx.update(|s| {
                        s.available = available;
                        s.timeouts = timeouts;
                    });
                    cx.ready();
                }
            }
            let unflushed = match queue.flush() {
                Ok(()) => false,
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock =>
                {
                    true
                }
                Err(e) => return Err(ServiceError(e.to_string())),
            };
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let (readable, woken) = {
                let fd = guard.connection_fd();
                let flags = if unflushed {
                    PollFlags::IN | PollFlags::OUT
                } else {
                    PollFlags::IN
                };
                let mut fds = [PollFd::new(&fd, flags), PollFd::new(wake, PollFlags::IN)];
                let timeout = let_go.map(|t| {
                    let d = t.saturating_duration_since(Instant::now());
                    rustix::event::Timespec {
                        tv_sec: d.as_secs().try_into().unwrap_or(i64::MAX),
                        tv_nsec: d.subsec_nanos().into(),
                    }
                });
                match rustix::event::poll(&mut fds, timeout.as_ref()) {
                    Ok(_) => (
                        fds[0]
                            .revents()
                            .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                        !fds[1].revents().is_empty(),
                    ),
                    Err(rustix::io::Errno::INTR) => (false, false),
                    Err(e) => return Err(io::Error::from(e).into()),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        return Err(ServiceError(format!("the Wayland connection ended: {e}")));
                    }
                }
            } else {
                drop(guard);
            }
            if woken {
                let mut buf = [0u8; 8];
                let _ = rustix::io::read(wake, &mut buf);
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == ExtIdleNotifierV1::interface().name && state.notifier.is_none() {
                    state.notifier = Some(registry.bind(name, version.min(1), qh, ()));
                    state.notifier_global = Some(name);
                    state.changed = true;
                } else if interface == wl_seat::WlSeat::interface().name && state.seat.is_none() {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                    state.changed = true;
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.notifier_global == Some(name) => {
                state.lost = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = true;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, Duration> for Client {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        timeout: &Duration,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = match event {
            ext_idle_notification_v1::Event::Idled => true,
            ext_idle_notification_v1::Event::Resumed => false,
            _ => return,
        };
        if let Some((_, was)) = state.notifications.get_mut(timeout)
            && *was != idle
        {
            *was = idle;
            state.changed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_whole_milliseconds_within_the_protocols_range() {
        assert_eq!(
            clamp(Duration::from_micros(300_400)),
            Duration::from_millis(300)
        );
        assert_eq!(clamp(Duration::from_secs(u64::MAX / 2)), MAX_TIMEOUT);
    }
}

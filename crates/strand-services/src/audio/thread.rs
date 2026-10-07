//! The `strand-pipewire` thread: one PipeWire main loop, iterated by hand.
//!
//! Every PipeWire callback only queues a [`Work`] item (owned data), and
//! the [`Driver`] handles the queue after each loop iteration with plain
//! `&mut self`: no shared mutable state between callbacks, no re-entrancy
//! (a stream's `connect` may call its listener at once; that only queues),
//! and one published batch per burst of events.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::device::{Device, DeviceListener};
use pipewire::loop_::{Timeout, TimerSource};
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::{Metadata, MetadataListener};
use pipewire::node::{Node, NodeListener};
use pipewire::properties::PropertiesBox;
use pipewire::registry::{GlobalObject, RegistryRc};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::Pod;
use pipewire::spa::support::system::IoFlags;
use pipewire::types::ObjectType;
use rustix::fs::inotify;

use super::meter::{Meter, MeterEvent, MeterTarget, retry_delay};
use super::model::{
    AudioChange, AudioDevice, AudioState, Direction, LevelTarget, Publisher, icon, linear,
    perceptual,
};
use super::pod::{Props, Route, default_name, default_value};
use super::{AudioAction, AudioConfig, AudioError, Cmd, DeviceRef, Host};

/// The first wait after a lost or refused connection.
const FIRST: Duration = Duration::from_millis(100);
/// The longest wait between attempts. Once the doubling passes it and the
/// socket's directory is watched, attempts wait for the socket to appear.
const MAX: Duration = Duration::from_secs(10);

/// The shortest time between two readings of one meter (60 per second):
/// readings in between are held, the loudest peak per channel kept.
pub const FRAME: Duration = Duration::from_micros(16_667);

/// How many of its last volume writes a device remembers, so the echo of
/// any of them reads back as written: as many as the store keeps pending
/// (`strand_core::echo::MAX_PENDING_ECHOES`). A volume on the 1/10 000
/// grid reads back as written also once forgotten ([`perceptual`]).
pub const ECHOES: usize = 64;

/// How long a new connection's first state waits for the session
/// manager (its `default` metadata, and the defaults shown before the
/// loss), and how long the last defaults stay shown after the metadata
/// leaves.
pub const SETTLE: Duration = Duration::from_secs(3);

/// How long a connection's first sync may take: a daemon that accepted
/// the connection but has not answered by then (a socket-activated
/// `pipewire.service` that is slow to start or keeps failing) counts as
/// lost, and the thread reconnects. Until its first sync is back a
/// connection publishes nothing, so the devices shown before stay.
pub const UNANSWERED: Duration = Duration::from_secs(6);

/// How long an action waits for a connection when there is none (the
/// thread just started, or the daemon went away): it runs once the next
/// connection's first state is out, or answers `NotConnected`.
pub const GRACE: Duration = Duration::from_secs(2);

/// The most actions that wait for a connection; more are refused.
const QUEUED: usize = 64;

/// How close (on the perceptual scale) a reported volume may be to a
/// written one and still read as it: a card's route quantizes to its
/// mixer's steps.
const ECHO_TOLERANCE: f64 = 0.005;

/// The callbacks' queue.
pub(crate) type Queue = Rc<RefCell<VecDeque<Work>>>;

/// What a callback hands the driver. `session` is the connection's number:
/// work queued by a connection that has since been dropped is ignored.
pub(crate) enum Work {
    Cmd(Cmd),
    Global {
        session: u64,
        global: GlobalObject<PropertiesBox>,
    },
    GlobalRemove {
        session: u64,
        id: u32,
    },
    NodeInfo {
        session: u64,
        id: u32,
        name: Option<String>,
        description: Option<String>,
        link: Option<RouteLink>,
    },
    /// A device's `Route` param info flags (they change, the serial bit
    /// toggling, whenever its routes change).
    DeviceInfo {
        session: u64,
        id: u32,
        route_flags: Option<u32>,
    },
    DeviceRoute {
        session: u64,
        id: u32,
        route: Route,
    },
    NodeProps {
        session: u64,
        id: u32,
        props: Props,
    },
    Meta {
        session: u64,
        subject: u32,
        key: Option<String>,
        value: Option<String>,
    },
    Done {
        session: u64,
        seq: i32,
    },
    CoreError {
        session: u64,
        id: u32,
        res: i32,
        message: String,
    },
    Retry,
    SocketAppeared,
    /// The socket directory's watch ended (the directory went away).
    WatchGone(i32),
    /// The meters' frame timer.
    Flush,
    /// A meter's data thread heard sound.
    MetersWake,
    /// The deadline timer (settling, kept defaults, waiting actions,
    /// meter retries).
    Deadline,
    Meter {
        meter: u64,
        event: MeterEvent,
    },
}

/// The thread's body: the PipeWire loop on the calling thread until a
/// [`Cmd::Stop`] (from the channel, or from the host's
/// [`Host::poll`]). No loop or context (PipeWire's library failed): the
/// host is given the defaults and the error is returned.
pub(crate) fn run(
    config: AudioConfig,
    host: Box<dyn Host>,
    rx: pipewire::channel::Receiver<Cmd>,
) -> Result<(), String> {
    pipewire::init();
    let mut host = host;
    let mainloop = match MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            host.changes(Publisher::new().publish(AudioState::default()));
            return Err(format!("cannot create a PipeWire loop: {e}"));
        }
    };
    let context = match ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            host.changes(Publisher::new().publish(AudioState::default()));
            return Err(format!("cannot create a PipeWire context: {e}"));
        }
    };
    let q: Queue = Rc::default();
    let lp = mainloop.loop_();
    let _cmds = rx.attach(lp, {
        let q = q.clone();
        move |cmd| q.borrow_mut().push_back(Work::Cmd(cmd))
    });
    let timer = lp.add_timer({
        let q = q.clone();
        move |_| q.borrow_mut().push_back(Work::Retry)
    });
    let flush = lp.add_timer({
        let q = q.clone();
        move |_| q.borrow_mut().push_back(Work::Flush)
    });
    let deadline = lp.add_timer({
        let q = q.clone();
        move |_| q.borrow_mut().push_back(Work::Deadline)
    });
    // The meters' data threads write it on the first sound after a read.
    let wake = rustix::event::eventfd(
        0,
        rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
    )
    .map_err(|e| log::warn!("audio: no eventfd, so no peak meters: {e}"))
    .ok()
    .map(Arc::new);
    let _wake_source = wake.as_ref().map(|fd| {
        let q = q.clone();
        lp.add_io(WakeFd(fd.clone()), IoFlags::IN, move |fd| {
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(&*fd.0, &mut buf);
            q.borrow_mut().push_back(Work::MetersWake);
        })
    });
    let watch = SocketWatch::new(config.remote.as_deref());
    let _watch_source = watch.as_ref().map(|w| {
        let q = q.clone();
        let name = w.name.clone();
        lp.add_io(WatchFd(w.fd.clone()), IoFlags::IN, move |fd| {
            let seen = drain_inotify(&fd.0, &name);
            let mut q = q.borrow_mut();
            q.extend(seen.gone.into_iter().map(Work::WatchGone));
            if seen.hit {
                q.push_back(Work::SocketAppeared);
            }
        })
    });

    let mut driver = Driver {
        config,
        q,
        context,
        timer: &timer,
        flush: &flush,
        flush_at: None,
        deadline: &deadline,
        deadline_at: None,
        wake,
        watch,
        host,
        session: None,
        sessions: 0,
        publisher: Publisher::new(),
        dirty: false,
        meters_dirty: false,
        out: Vec::new(),
        backoff: FIRST,
        wanted: BTreeSet::new(),
        meters: 0,
        queued: VecDeque::new(),
        grace_until: Instant::now() + GRACE,
        quit: false,
    };
    driver.connect();
    driver.drain();
    while !driver.quit {
        lp.iterate(Timeout::Infinite);
        driver.drain();
    }
    for (_, reply) in driver.queued.drain(..) {
        let _ = reply.send(Err(AudioError::NotConnected));
    }
    // The session (its streams, proxies and core) goes before the loop's
    // sources, the context and the loop.
    drop(driver);
    Ok(())
}

/// The inotify fd, shared by the loop's io source and the driver.
struct WatchFd(Rc<OwnedFd>);

impl AsRawFd for WatchFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// The meters' eventfd, shared with their data threads.
struct WakeFd(Arc<OwnedFd>);

impl AsRawFd for WakeFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Watches the socket's directory while disconnected, so a restarted
/// daemon is reached at once without polling.
struct SocketWatch {
    fd: Rc<OwnedFd>,
    dir: PathBuf,
    name: OsString,
    wd: Option<i32>,
}

impl SocketWatch {
    fn new(remote: Option<&str>) -> Option<SocketWatch> {
        let (dir, name) = socket_path(remote)?;
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
            .map_err(|e| log::warn!("audio: no inotify for the PipeWire socket: {e}"))
            .ok()?;
        Some(SocketWatch {
            fd: Rc::new(fd),
            dir,
            name,
            wd: None,
        })
    }

    fn watch(&mut self) -> bool {
        if self.wd.is_none() {
            match inotify::add_watch(
                &*self.fd,
                &self.dir,
                inotify::WatchFlags::CREATE | inotify::WatchFlags::MOVED_TO,
            ) {
                Ok(wd) => self.wd = Some(wd),
                Err(e) => log::debug!("audio: cannot watch {}: {e}", self.dir.display()),
            }
        }
        self.wd.is_some()
    }

    /// The socket is not there (so only its creation can bring the daemon
    /// back). Any other answer (it exists, or cannot be checked) is not.
    fn socket_missing(&self) -> bool {
        std::fs::symlink_metadata(self.dir.join(&self.name))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    }

    /// Watch `wd` ended; true if it was ours.
    fn gone(&mut self, wd: i32) -> bool {
        if self.wd == Some(wd) {
            self.wd = None;
            return true;
        }
        false
    }

    fn unwatch(&mut self) {
        if let Some(wd) = self.wd.take() {
            let _ = inotify::remove_watch(&*self.fd, wd);
        }
        // Events already queued are dropped with the watch's removal; any
        // left are read and ignored by the next callback.
    }
}

/// The directory and file name of the PipeWire socket, as PipeWire
/// resolves them: an absolute remote as is, else the remote (or
/// `$PIPEWIRE_REMOTE`, or `pipewire-0`) in `$PIPEWIRE_RUNTIME_DIR`, else
/// `$XDG_RUNTIME_DIR`.
fn socket_path(remote: Option<&str>) -> Option<(PathBuf, OsString)> {
    let name: OsString = match remote {
        Some(r) => r.into(),
        None => std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into()),
    };
    let path = PathBuf::from(&name);
    if path.is_absolute() {
        return Some((path.parent()?.to_path_buf(), path.file_name()?.to_owned()));
    }
    let dir = std::env::var_os("PIPEWIRE_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()))?;
    let full = PathBuf::from(dir).join(path);
    Some((full.parent()?.to_path_buf(), full.file_name()?.to_owned()))
}

/// What the queued inotify events said.
#[derive(Debug, Default, PartialEq)]
struct Seen {
    /// One named the socket, or the queue overflowed (events were lost:
    /// one may have been the socket's).
    hit: bool,
    /// Watches that ended (`IN_IGNORED`: the directory went away, or we
    /// removed it).
    gone: Vec<i32>,
}

/// Reads every queued inotify event.
fn drain_inotify(fd: &OwnedFd, name: &OsString) -> Seen {
    let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(fd.as_fd(), &mut buf);
    let mut seen = Seen::default();
    // Bounded: a reader that keeps failing never spins.
    for _ in 0..4096 {
        match reader.next() {
            Ok(ev) => seen.add(ev.wd(), ev.events(), ev.file_name(), name),
            Err(_) => break,
        }
    }
    seen
}

impl Seen {
    fn add(
        &mut self,
        wd: i32,
        events: inotify::ReadFlags,
        file: Option<&std::ffi::CStr>,
        name: &OsString,
    ) {
        use std::os::unix::ffi::OsStrExt;
        if events.contains(inotify::ReadFlags::QUEUE_OVERFLOW)
            || file.map(|n| n.to_bytes()) == Some(name.as_bytes())
        {
            self.hit = true;
        }
        if events.contains(inotify::ReadFlags::IGNORED) {
            self.gone.push(wd);
        }
    }
}

/// One connection to the daemon and what it follows.
struct Session {
    number: u64,
    // Drop order: meters (streams), proxies and listeners, then the
    // registry and the core.
    meters: Vec<Meter>,
    nodes: BTreeMap<u32, NodeEntry>,
    devices: BTreeMap<u32, DeviceEntry>,
    metadata: Option<MetaEntry>,
    _registry_listener: pipewire::registry::Listener,
    registry: RegistryRc,
    _core_listener: pipewire::core::Listener,
    core: CoreRc,
    defaults: Defaults,
    /// The defaults shown before (the last connection's, or this one's
    /// before its metadata left), used while the current ones name no
    /// device, until the deadline.
    held: Option<Held>,
    /// The connection's first sync has come back.
    initial_done: bool,
    /// The sync after the latest bind, until it comes back.
    awaiting: Option<i32>,
    /// The sync after binding the `default` metadata, until it comes back
    /// (its properties are read then).
    meta_sync: Option<i32>,
    meta_ready: bool,
    /// The first state of this connection has gone out.
    published: bool,
    /// The first state goes out once the session manager is back (the
    /// metadata read, and a default sink and source again if one was
    /// shown before), or at this deadline.
    settle_by: Instant,
    expect_sink: bool,
    expect_source: bool,
    /// The settling deadline passed.
    settled: bool,
    /// When it connected: only a connection that lasted [`MAX`] resets
    /// the backoff when lost (a daemon that dies at once keeps backing
    /// off).
    since: Instant,
}

struct NodeEntry {
    _listener: NodeListener,
    proxy: Node,
    device: AudioDevice,
    direction: Direction,
    /// How many channels its volume has (0 while unknown).
    channels: u32,
    serial: Option<String>,
    /// The card profile device it belongs to, if it is a card's node.
    link: Option<RouteLink>,
    /// The sync issued after binding it; it is shown once that returns
    /// (with its properties and volume read).
    sync: i32,
    ready: bool,
    /// Its volume writes and their echoes ([`Echoes`]).
    echoes: Echoes,
    /// It has reported `channelVolumes` (the `volume` prop is then
    /// ignored).
    has_channel_volumes: bool,
}

/// The last [`ECHOES`] volumes written to a device, with the linear
/// channel volumes each became: PipeWire echoes those, and any of them
/// reads back as the value written (not its cube root's float noise),
/// also when a slider sent many writes before the first echo. On a
/// node's `Props` the echo is exact; through a card's route it may come
/// back quantized to the mixer's steps, and still reads as the closest
/// write through a route within [`ECHO_TOLERANCE`]. An echo of a write already forgotten
/// reads as [`perceptual`] reads it: exactly as written for a volume on
/// its grid.
#[derive(Debug, Default)]
struct Echoes {
    /// Each write: the volume, its linear channel volumes, and whether it
    /// went through a card's route (its echo may be quantized).
    written: VecDeque<(f64, Vec<f32>, bool)>,
    /// The last write, until PipeWire has reported it (or another
    /// volume, after the write was applied): a step adds to this, not to
    /// a volume it is replacing.
    pending: Option<f64>,
    /// The core sync issued after the last write, until it comes back:
    /// a report before then that matches no write is an older state
    /// (the echo of a mute, say), not another program's volume.
    wait: Option<i32>,
}

impl Echoes {
    fn wrote(&mut self, volume: f64, linear: Vec<f32>, quantized: bool) {
        if self.written.len() == ECHOES {
            self.written.pop_front();
        }
        self.written.push_back((volume, linear, quantized));
        self.pending = Some(volume);
    }

    /// PipeWire has applied the last write (its sync came back).
    fn synced(&mut self, seq: i32) {
        if self.wait.is_some_and(|w| seq >= w) {
            self.wait = None;
        }
    }

    /// The volume to show for reported linear channel volumes.
    fn read(&mut self, reported: &[f32]) -> Option<f64> {
        let shown = perceptual(reported);
        let hit = self
            .written
            .iter()
            .rposition(|(_, lin, _)| lin == reported)
            .or_else(|| {
                // The closest write through a route (the latest of equally
                // close ones). A write to a node's `Props` echoes exactly:
                // a near miss there is another volume (or the echo of a
                // write already forgotten, which `perceptual` reads).
                let shown = shown?;
                self.written
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, _, quantized))| *quantized)
                    .map(|(i, (v, _, _))| (i, (v - shown).abs()))
                    .filter(|(_, d)| *d < ECHO_TOLERANCE)
                    .min_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
                    .map(|(i, _)| i)
            });
        let last = hit.is_some_and(|i| i + 1 == self.written.len());
        if last || (hit.is_none() && self.wait.is_none()) {
            // The last write landed, or another program set it after it.
            self.pending = None;
        }
        hit.map(|i| self.written[i].0).or(shown)
    }

    /// The volume a step starts from.
    fn base(&self, shown: f64) -> f64 {
        self.pending.unwrap_or(shown)
    }
}

/// A node's place on a card: the device (`device.id`) and the device's
/// profile device (`card.profile.device`) it plays or records through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RouteLink {
    pub device: u32,
    pub profile_device: i32,
}

impl RouteLink {
    fn of(device: Option<&str>, profile_device: Option<&str>) -> Option<RouteLink> {
        Some(RouteLink {
            device: device?.parse().ok()?,
            profile_device: profile_device?.parse().ok()?,
        })
    }
}

/// A bound audio device (a card): its active routes, by profile device.
struct DeviceEntry {
    _listener: DeviceListener,
    proxy: Device,
    routes: Routes,
}

/// A device's active routes.
#[derive(Debug, Default)]
struct Routes {
    /// The flags of its last `Route` param info.
    flags: Option<u32>,
    /// The routes changed and the new set has not come yet: until it
    /// does, none is used (a device without routes stays so, and its
    /// nodes are written directly).
    stale: bool,
    by_profile_device: BTreeMap<i32, Route>,
}

impl Routes {
    /// Its `Route` info flags: a change means a new set of routes comes.
    fn info(&mut self, flags: Option<u32>) {
        if flags != self.flags {
            self.flags = flags;
            self.stale = true;
        }
    }

    /// One route of the set that follows the info.
    fn route(&mut self, route: Route) {
        if self.stale {
            self.stale = false;
            self.by_profile_device.clear();
        }
        self.by_profile_device.insert(route.device, route);
    }

    fn active(&self, profile_device: i32) -> Option<Route> {
        if self.stale {
            return None;
        }
        self.by_profile_device.get(&profile_device).copied()
    }
}

/// Where a volume or mute write goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteVia {
    /// The node's own `Props`.
    Node,
    /// The card's active route for the node (`save: true`): what moves a
    /// hardware mixer and what the session manager saves and restores.
    Route { device: u32, route: Route },
}

/// The write path for a node `link`ed to a card: its active route when
/// the device has one for the node's profile device, else the node.
fn write_via(link: Option<RouteLink>, routes: impl Fn(u32) -> Option<Route>) -> WriteVia {
    let Some(link) = link else {
        return WriteVia::Node;
    };
    match routes(link.device) {
        Some(route) if route.device == link.profile_device => WriteVia::Route {
            device: link.device,
            route,
        },
        _ => WriteVia::Node,
    }
}

struct MetaEntry {
    global: u32,
    _listener: MetadataListener,
    proxy: Metadata,
}

impl Defaults {
    /// The effective and the configured default of `direction`.
    fn names(&self, direction: Direction) -> [&Option<String>; 2] {
        match direction {
            Direction::Sink => [&self.sink, &self.configured_sink],
            Direction::Source => [&self.source, &self.configured_source],
        }
    }
}

/// Defaults kept from before, until `until`.
struct Held {
    defaults: Defaults,
    until: Instant,
}

/// The `default` metadata's audio keys.
#[derive(Default)]
struct Defaults {
    sink: Option<String>,
    source: Option<String>,
    configured_sink: Option<String>,
    configured_source: Option<String>,
}

struct Driver<'l> {
    config: AudioConfig,
    q: Queue,
    context: ContextRc,
    timer: &'l TimerSource<'l>,
    /// The meters' frame timer, and when it fires (armed only while a
    /// meter holds a reading).
    flush: &'l TimerSource<'l>,
    flush_at: Option<Instant>,
    /// The deadline timer, and when it fires ([`Driver::next_deadline`]).
    deadline: &'l TimerSource<'l>,
    deadline_at: Option<Instant>,
    /// The meters' eventfd (`None`: no meters).
    wake: Option<Arc<OwnedFd>>,
    watch: Option<SocketWatch>,
    /// Who gets the changes, and gives commands of its own.
    host: Box<dyn Host>,
    session: Option<Session>,
    sessions: u64,
    publisher: Publisher,
    /// The state may have changed since the last publish.
    dirty: bool,
    /// The meters may need starting, stopping or retargeting.
    meters_dirty: bool,
    out: Vec<AudioChange>,
    backoff: Duration,
    wanted: BTreeSet<LevelTarget>,
    meters: u64,
    /// Actions that came while no connection had published its first
    /// state, in order; they run right after it.
    queued: VecDeque<(AudioAction, Reply)>,
    /// Until when actions wait while there is no connection at all.
    grace_until: Instant,
    quit: bool,
}

type Reply = tokio::sync::oneshot::Sender<Result<(), AudioError>>;

impl Driver<'_> {
    /// Handles every queued item (and what the host asks for), then
    /// publishes once.
    fn drain(&mut self) {
        loop {
            loop {
                let w = self.q.borrow_mut().pop_front();
                let Some(w) = w else { break };
                self.handle(w);
            }
            if !self.quit {
                let cmds = self.host.poll();
                if !cmds.is_empty() {
                    self.q.borrow_mut().extend(cmds.into_iter().map(Work::Cmd));
                    continue;
                }
            }
            if self.dirty {
                self.publish();
            }
            if self.session.as_ref().is_some_and(|s| s.published) && !self.queued.is_empty() {
                self.run_queued();
                // The host reads their replies (a write's answer clock
                // starts once PipeWire was asked).
                self.q.borrow_mut().push_back(Work::Cmd(Cmd::Poke));
            }
            if self.meters_dirty {
                self.meters_dirty = false;
                self.sync_meters();
            }
            // Starting a meter may queue its first events at once.
            if self.q.borrow().is_empty() {
                break;
            }
        }
        if !self.out.is_empty() {
            self.host.changes(std::mem::take(&mut self.out));
            // The host may answer what the batch settled.
            if !self.quit {
                let cmds = self.host.poll();
                if !cmds.is_empty() {
                    self.q.borrow_mut().extend(cmds.into_iter().map(Work::Cmd));
                    return self.drain();
                }
            }
        }
        self.arm_deadline();
    }

    fn live(&self, session: u64) -> bool {
        self.session.as_ref().is_some_and(|s| s.number == session)
    }

    fn handle(&mut self, w: Work) {
        match w {
            Work::Cmd(Cmd::Stop) => self.quit = true,
            // The host's own inbox: read by `Host::poll` after the queue.
            Work::Cmd(Cmd::Poke) => {}
            // After a stop, nothing more is done.
            Work::Cmd(Cmd::Levels(_)) if self.quit => {}
            Work::Cmd(Cmd::Levels(set)) => {
                self.wanted = set;
                self.meters_dirty = true;
            }
            Work::Cmd(Cmd::Action(_, reply)) if self.quit => {
                let _ = reply.send(Err(AudioError::NotConnected));
            }
            Work::Cmd(Cmd::Action(action, reply)) => self.request(action, reply),
            Work::Global { session, global } if self.live(session) => self.global(global),
            Work::GlobalRemove { session, id } if self.live(session) => self.global_remove(id),
            Work::NodeInfo {
                session,
                id,
                name,
                description,
                link,
            } if self.live(session) => {
                if let Some(n) = self.session.as_mut().and_then(|s| s.nodes.get_mut(&id)) {
                    if let Some(name) = name {
                        n.device.name = name;
                    }
                    if let Some(d) = description {
                        n.device.description = d;
                    }
                    n.link = link;
                    self.dirty = true;
                }
            }
            Work::DeviceInfo {
                session,
                id,
                route_flags,
            } if self.live(session) => {
                if let Some(d) = self.session.as_mut().and_then(|s| s.devices.get_mut(&id)) {
                    d.routes.info(route_flags);
                }
            }
            Work::DeviceRoute { session, id, route } if self.live(session) => {
                if let Some(d) = self.session.as_mut().and_then(|s| s.devices.get_mut(&id)) {
                    d.routes.route(route);
                }
            }
            Work::NodeProps { session, id, props } if self.live(session) => {
                if let Some(n) = self.session.as_mut().and_then(|s| s.nodes.get_mut(&id)) {
                    apply_props(n, &props);
                    self.dirty = true;
                }
            }
            Work::Meta {
                session,
                subject,
                key,
                value,
            } if self.live(session) => {
                if subject == 0
                    && let Some(s) = &mut self.session
                {
                    apply_meta(&mut s.defaults, key.as_deref(), value.as_deref());
                    self.dirty = true;
                    self.meters_dirty = true;
                }
            }
            Work::Done { session, seq } if self.live(session) => {
                if let Some(s) = &mut self.session {
                    s.initial_done = true;
                    if s.awaiting.is_some_and(|a| seq >= a) {
                        s.awaiting = None;
                    }
                    if s.meta_sync.is_some_and(|m| seq >= m) {
                        s.meta_sync = None;
                        s.meta_ready = true;
                    }
                    for n in s.nodes.values_mut() {
                        if !n.ready && seq >= n.sync {
                            n.ready = true;
                        }
                        n.echoes.synced(seq);
                    }
                    self.dirty = true;
                    self.meters_dirty = true;
                }
            }
            Work::CoreError {
                session,
                id,
                res,
                message,
            } if self.live(session) => {
                if id == PW_ID_CORE && disconnected(res) {
                    log::info!("audio: lost PipeWire ({res}: {message}); reconnecting");
                    self.lost();
                } else {
                    log::warn!("audio: PipeWire error on object {id} ({res}): {message}");
                }
            }
            Work::Retry => self.connect(),
            Work::SocketAppeared => {
                if self.session.is_none() {
                    self.backoff = FIRST;
                    self.connect();
                }
            }
            Work::WatchGone(wd) => {
                let ours = self.watch.as_mut().is_some_and(|w| w.gone(wd));
                if ours && self.session.is_none() {
                    // Watch again (the directory may be back), or fall
                    // back to the timer.
                    self.watching();
                    self.schedule_retry();
                }
            }
            Work::Flush => {
                self.flush_at = None;
                self.tick_meters(false);
            }
            Work::MetersWake => self.tick_meters(true),
            Work::Deadline => {
                self.deadline_at = None;
                self.deadline();
            }
            Work::Meter { meter, event } => self.meter_event(meter, event),
            // Work of a dropped connection.
            Work::Global { .. }
            | Work::GlobalRemove { .. }
            | Work::NodeInfo { .. }
            | Work::DeviceInfo { .. }
            | Work::DeviceRoute { .. }
            | Work::NodeProps { .. }
            | Work::Meta { .. }
            | Work::Done { .. }
            | Work::CoreError { .. } => {}
        }
    }

    // --- Connection --------------------------------------------------------

    fn connect(&mut self) {
        if self.session.is_some() {
            return;
        }
        let props = self.config.remote.as_ref().map(|r| {
            let mut p = PropertiesBox::new();
            p.insert("remote.name", r.as_str());
            p
        });
        let result = self
            .context
            .connect_rc(props)
            .map_err(|e| e.to_string())
            .and_then(|core| self.start(core));
        match result {
            Ok(mut session) => {
                // Until the session manager is back, what was shown before
                // stays: the first state waits for it (or `SETTLE`), and
                // the defaults shown resolve meanwhile.
                if let Some(shown) = self.publisher.state() {
                    session.expect_sink = shown.sink().is_some();
                    session.expect_source = shown.source().is_some();
                    session.held = held(shown, session.settle_by);
                }
                self.session = Some(session);
                let _ = self.timer.update_timer(None, None);
                if let Some(w) = &mut self.watch {
                    w.unwatch();
                }
            }
            Err(e) => {
                log::debug!("audio: cannot connect to PipeWire: {e}");
                if self.publisher.state().is_none() {
                    // The first batch: nothing to show, and not connected.
                    self.dirty = true;
                }
                self.watching();
                self.schedule_retry();
            }
        }
    }

    fn start(&mut self, core: CoreRc) -> Result<Session, String> {
        self.sessions += 1;
        let number = self.sessions;
        let (q1, q2) = (self.q.clone(), self.q.clone());
        let core_listener = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == PW_ID_CORE {
                    q1.borrow_mut().push_back(Work::Done {
                        session: number,
                        seq: seq.seq(),
                    });
                }
            })
            .error(move |id, _seq, res, message| {
                q2.borrow_mut().push_back(Work::CoreError {
                    session: number,
                    id,
                    res,
                    message: message.to_owned(),
                });
            })
            .register();
        let registry = core
            .get_registry_rc()
            .map_err(|e| format!("no registry: {e}"))?;
        let (q1, q2) = (self.q.clone(), self.q.clone());
        let registry_listener = registry
            .add_listener_local()
            .global(move |g| {
                // Only what the thread binds is copied.
                if matches!(
                    g.type_,
                    ObjectType::Node | ObjectType::Device | ObjectType::Metadata
                ) {
                    q1.borrow_mut().push_back(Work::Global {
                        session: number,
                        global: g.to_owned(),
                    });
                }
            })
            .global_remove(move |id| {
                q2.borrow_mut().push_back(Work::GlobalRemove {
                    session: number,
                    id,
                });
            })
            .register();
        let seq = core.sync(0).map_err(|e| format!("cannot sync: {e}"))?.seq();
        Ok(Session {
            number,
            meters: Vec::new(),
            nodes: BTreeMap::new(),
            devices: BTreeMap::new(),
            metadata: None,
            _registry_listener: registry_listener,
            registry,
            _core_listener: core_listener,
            core,
            defaults: Defaults::default(),
            held: None,
            initial_done: false,
            awaiting: Some(seq),
            meta_sync: None,
            meta_ready: false,
            published: false,
            settle_by: Instant::now() + SETTLE,
            expect_sink: false,
            expect_source: false,
            settled: false,
            since: Instant::now(),
        })
    }

    /// The daemon went away: keep the devices, say so, and reconnect.
    fn lost(&mut self) {
        if let Some(mut s) = self.session.take() {
            if s.since.elapsed() >= MAX {
                self.backoff = FIRST;
            }
            for mut m in s.meters.drain(..) {
                self.out.extend(m.pause().map(AudioChange::Levels));
            }
        }
        // Actions wait a little for the next connection.
        self.grace_until = Instant::now() + GRACE;
        self.dirty = true;
        self.watching();
        self.schedule_retry();
    }

    fn watching(&mut self) {
        if let Some(w) = &mut self.watch {
            w.watch();
        }
    }

    fn schedule_retry(&mut self) {
        // Only a missing socket is waited for; one that is there but
        // refuses (a stale socket, a permission change) is retried.
        let waiting = self
            .watch
            .as_ref()
            .is_some_and(|w| w.wd.is_some() && w.socket_missing());
        if self.backoff > MAX && waiting {
            // The socket's creation will wake us.
            let _ = self.timer.update_timer(None, None);
            return;
        }
        let delay = self.backoff.min(MAX);
        let _ = self.timer.update_timer(Some(delay), None);
        self.backoff = delay * 2;
    }

    // --- Registry ----------------------------------------------------------

    fn global(&mut self, global: GlobalObject<PropertiesBox>) {
        let q = self.q.clone();
        let Some(s) = &mut self.session else { return };
        let props = global.props.as_ref();
        let get = |k: &str| props.and_then(|p| p.get(k)).map(str::to_owned);
        match global.type_ {
            ObjectType::Node => {
                let Some(direction) = get("media.class")
                    .as_deref()
                    .and_then(Direction::of_media_class)
                else {
                    return;
                };
                let node: Node = match s.registry.bind(&global) {
                    Ok(n) => n,
                    Err(e) => {
                        log::warn!("audio: cannot bind node {}: {e}", global.id);
                        return;
                    }
                };
                let (number, id) = (s.number, global.id);
                let (q1, q2) = (q.clone(), q);
                let listener = node
                    .add_listener_local()
                    .info(move |info| {
                        let Some(p) = info.props() else { return };
                        let description = p
                            .get("node.description")
                            .or_else(|| p.get("node.nick"))
                            .map(str::to_owned);
                        q1.borrow_mut().push_back(Work::NodeInfo {
                            session: number,
                            id,
                            name: p.get("node.name").map(str::to_owned),
                            description,
                            link: RouteLink::of(p.get("device.id"), p.get("card.profile.device")),
                        });
                    })
                    .param(move |_, ty, _, _, pod| {
                        if ty != ParamType::Props {
                            return;
                        }
                        if let Some(props) = pod.and_then(|p| Props::parse(p.as_bytes())) {
                            q2.borrow_mut().push_back(Work::NodeProps {
                                session: number,
                                id,
                                props,
                            });
                        }
                    })
                    .register();
                node.subscribe_params(&[ParamType::Props]);
                let name = get("node.name").unwrap_or_else(|| format!("node-{id}"));
                let mut device = AudioDevice::new(id, name, direction);
                if let Some(d) = get("node.description").or_else(|| get("node.nick")) {
                    device.description = d;
                }
                let Some(sync) = sync(s) else { return };
                s.nodes.insert(
                    id,
                    NodeEntry {
                        _listener: listener,
                        proxy: node,
                        device,
                        direction,
                        channels: 0,
                        serial: get("object.serial"),
                        link: None,
                        sync,
                        ready: false,
                        echoes: Echoes::default(),
                        has_channel_volumes: false,
                    },
                );
            }
            ObjectType::Device => {
                // Cards (ALSA, Bluetooth): only their routes are read, for
                // writes; they show nothing themselves.
                if get("media.class").as_deref() != Some("Audio/Device") {
                    return;
                }
                let device: Device = match s.registry.bind(&global) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("audio: cannot bind device {}: {e}", global.id);
                        return;
                    }
                };
                let (number, id) = (s.number, global.id);
                let (q1, q2) = (q.clone(), q);
                let listener = device
                    .add_listener_local()
                    .info(move |info| {
                        let route_flags = info
                            .params()
                            .iter()
                            .find(|p| p.id() == ParamType::Route)
                            .map(|p| p.flags().bits());
                        q1.borrow_mut().push_back(Work::DeviceInfo {
                            session: number,
                            id,
                            route_flags,
                        });
                    })
                    .param(move |_, ty, _, _, pod| {
                        if ty != ParamType::Route {
                            return;
                        }
                        if let Some(route) = pod.and_then(|p| Route::parse(p.as_bytes())) {
                            q2.borrow_mut().push_back(Work::DeviceRoute {
                                session: number,
                                id,
                                route,
                            });
                        }
                    })
                    .register();
                device.subscribe_params(&[ParamType::Route]);
                // The first state (and the actions waiting for it) waits
                // for its routes, so no early write goes to a card node's
                // `Props`.
                if sync(s).is_none() {
                    return;
                }
                s.devices.insert(
                    id,
                    DeviceEntry {
                        _listener: listener,
                        proxy: device,
                        routes: Routes::default(),
                    },
                );
            }
            ObjectType::Metadata => {
                if s.metadata.is_some() || get("metadata.name").as_deref() != Some("default") {
                    return;
                }
                let meta: Metadata = match s.registry.bind(&global) {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("audio: cannot bind the default metadata: {e}");
                        return;
                    }
                };
                let number = s.number;
                let listener = meta
                    .add_listener_local()
                    .property(move |subject, key, _ty, value| {
                        q.borrow_mut().push_back(Work::Meta {
                            session: number,
                            subject,
                            key: key.map(str::to_owned),
                            value: value.map(str::to_owned),
                        });
                        0
                    })
                    .register();
                let Some(seq) = sync(s) else { return };
                s.meta_sync = Some(seq);
                s.meta_ready = false;
                s.metadata = Some(MetaEntry {
                    global: global.id,
                    _listener: listener,
                    proxy: meta,
                });
            }
            _ => {}
        }
    }

    fn global_remove(&mut self, id: u32) {
        let Some(s) = &mut self.session else { return };
        s.devices.remove(&id);
        if s.nodes.remove(&id).is_some() {
            self.dirty = true;
            self.meters_dirty = true;
        } else if s.metadata.as_ref().is_some_and(|m| m.global == id) {
            // The session manager restarted: its new metadata will come.
            // The defaults shown stay meanwhile (until `SETTLE`), so a
            // restart does not flash an empty `audio.sink`.
            let shown = state_of(s);
            s.held = held(&shown, Instant::now() + SETTLE);
            s.metadata = None;
            s.meta_sync = None;
            s.meta_ready = false;
            s.defaults = Defaults::default();
            self.dirty = true;
            self.meters_dirty = true;
        }
    }

    // --- State -------------------------------------------------------------

    fn publish(&mut self) {
        self.dirty = false;
        let state = match &mut self.session {
            Some(s) => {
                if !s.published && !(ready_to_publish(s) || s.settled && s.initial_done) {
                    // The first state of a connection waits for its sync
                    // (always: a daemon that never answers is not a
                    // connection, [`UNANSWERED`]) and for the session
                    // manager ([`SETTLE`]); until then the last state
                    // stays, not connected.
                    return;
                }
                s.published = true;
                state_of(s)
            }
            None => AudioState {
                connected: false,
                ..self.publisher.state().cloned().unwrap_or_default()
            },
        };
        let changes = self.publisher.publish(state);
        self.out.extend(changes);
    }

    // --- Actions -----------------------------------------------------------

    /// An action from the handle: run now when a connection has published
    /// its first state, else held (in order) until one has, or until the
    /// grace without a connection ends.
    fn request(&mut self, action: AudioAction, reply: Reply) {
        let published = self.session.as_ref().is_some_and(|s| s.published);
        if published && self.queued.is_empty() {
            self.answer(action, reply);
        } else if self.session.is_some() || Instant::now() < self.grace_until {
            if self.queued.len() >= QUEUED {
                log::warn!("audio: too many actions wait for PipeWire; refusing one");
                let _ = reply.send(Err(AudioError::NotConnected));
            } else {
                self.queued.push_back((action, reply));
            }
        } else {
            let _ = reply.send(Err(AudioError::NotConnected));
        }
    }

    fn answer(&mut self, action: AudioAction, reply: Reply) {
        let r = self.action(action);
        if let Err(e) = &r {
            log::debug!("audio: {e}");
        }
        // Nobody waits for this answer (the store's `make_default()`, a
        // dropped handle reply): a refusal is logged here, or nowhere.
        if let Err(Err(e)) = reply.send(r) {
            log::warn!("audio: {e}");
        }
    }

    /// Runs the actions that waited for this connection's first state.
    fn run_queued(&mut self) {
        while let Some((action, reply)) = self.queued.pop_front() {
            self.answer(action, reply);
        }
    }

    fn action(&mut self, action: AudioAction) -> Result<(), AudioError> {
        let s = self.session.as_mut().ok_or(AudioError::NotConnected)?;
        match action {
            AudioAction::SetVolume(d, v) | AudioAction::StepVolume(d, v) => {
                if !v.is_finite() {
                    return Err(AudioError::InvalidVolume(v));
                }
                let step = matches!(action, AudioAction::StepVolume(..));
                let n = node_mut(s, d)?;
                let current = n.device.volume;
                let v = if step { n.echoes.base(current) + v } else { v };
                // Never raised past 1 by us, nor past a volume another
                // program amplified it to.
                let v = v.clamp(0.0, current.max(1.0));
                let (props, lin) = if n.has_channel_volumes && n.channels > 0 {
                    let lin = vec![linear(v); n.channels as usize];
                    let props = Props {
                        channel_volumes: Some(lin.clone()),
                        ..Props::default()
                    };
                    (props, lin)
                } else {
                    let props = Props {
                        volume: Some(linear(v)),
                        ..Props::default()
                    };
                    (props, vec![linear(v)])
                };
                let id = n.device.id;
                let via = write(s, id, &props)?;
                if let Some(n) = s.nodes.get_mut(&id) {
                    n.echoes.wrote(v, lin, via == Written::Route);
                }
                // Its echo comes before this sync returns: a report until
                // then that matches no write is an older state.
                match s.core.sync(0) {
                    Ok(seq) => {
                        if let Some(n) = s.nodes.get_mut(&id) {
                            n.echoes.wait = Some(seq.seq());
                        }
                    }
                    Err(e) => log::debug!("audio: cannot sync after a write: {e}"),
                }
                Ok(())
            }
            AudioAction::SetMuted(d, m) => {
                let id = node_mut(s, d)?.device.id;
                write(
                    s,
                    id,
                    &Props {
                        mute: Some(m),
                        ..Props::default()
                    },
                )
                .map(|_| ())
            }
            AudioAction::MakeDefault(d) => {
                let (name, direction) = {
                    let n = node_mut(s, d)?;
                    (n.device.name.clone(), n.direction)
                };
                let meta = s.metadata.as_ref().ok_or(AudioError::NoDefaultMetadata)?;
                let kind = match direction {
                    Direction::Sink => "sink",
                    Direction::Source => "source",
                };
                let value = default_value(&name);
                for key in [
                    format!("default.configured.audio.{kind}"),
                    format!("default.audio.{kind}"),
                ] {
                    meta.proxy
                        .set_property(0, &key, Some("Spa:String:JSON"), Some(&value));
                }
                Ok(())
            }
        }
    }

    // --- Meters ------------------------------------------------------------

    fn sync_meters(&mut self) {
        let Some(s) = &mut self.session else { return };
        let now = Instant::now();
        let state = state_of(s);
        let mut keep = Vec::new();
        let mut old = std::mem::take(&mut s.meters);
        for &target in &self.wanted {
            let device = match target {
                LevelTarget::DefaultSink => state.sink(),
                LevelTarget::DefaultSource => state.source(),
                LevelTarget::Device(id) => state.device(id),
            };
            let Some(device) = device else { continue };
            let mut failures = 0;
            if let Some(i) = old
                .iter()
                .position(|m| m.target == target && m.device == device.id)
            {
                let m = &old[i];
                // A meter whose stream failed is started anew once its
                // retry is due (later after each failure in a row).
                if !m.failed || m.retry_at.is_some_and(|t| t > now) {
                    keep.push(old.swap_remove(i));
                    continue;
                }
                failures = m.failures;
            }
            let Some(node) = s.nodes.get(&device.id) else {
                continue;
            };
            let Some(wake) = &self.wake else { continue };
            self.meters += 1;
            let t = MeterTarget {
                id: device.id,
                name: &device.name,
                serial: node.serial.as_deref(),
                direction: node.direction,
            };
            match Meter::start(&s.core, self.meters, target, &t, &self.q, wake, failures) {
                Ok(m) => keep.push(m),
                Err(e) => log::warn!("audio: {e}"),
            }
        }
        // Meters no longer wanted, or retargeted, stop here; each that
        // last showed sound says it is quiet, so a reader never keeps a
        // stale level (hidden while the sound stopped, or a default that
        // moved to a silent device).
        for mut m in old {
            self.out.extend(m.pause().map(AudioChange::Levels));
        }
        s.meters = keep;
    }

    /// Reads the meters: those whose data thread heard sound (`wake`), or
    /// those due on the frame timer. Each sends at most one reading per
    /// [`FRAME`], the loudest peak per channel since its last; only the
    /// first silent reading after sound is sent.
    fn tick_meters(&mut self, wake: bool) {
        let now = Instant::now();
        let Some(s) = &mut self.session else { return };
        let mut next: Option<Instant> = None;
        for m in &mut s.meters {
            let due = match m.next_tick {
                Some(t) => !wake && t <= now,
                None => wake && m.woken(),
            };
            if due {
                self.out.extend(m.collect(now).map(AudioChange::Levels));
            }
            if let Some(t) = m.next_tick {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }
        if let Some(at) = next {
            self.arm_flush(at, now);
        }
    }

    fn arm_flush(&mut self, at: Instant, now: Instant) {
        if self.flush_at.is_some_and(|t| t <= at) {
            return;
        }
        self.flush_at = Some(at);
        // Never zero: a zero timeout disarms the timer.
        let delay = at
            .saturating_duration_since(now)
            .max(Duration::from_micros(100));
        let _ = self.flush.update_timer(Some(delay), None);
    }

    fn meter_event(&mut self, meter: u64, event: MeterEvent) {
        let Some(m) = self
            .session
            .as_mut()
            .and_then(|s| s.meters.iter_mut().find(|m| m.id == meter))
        else {
            return;
        };
        match event {
            MeterEvent::Running => m.failures = 0,
            MeterEvent::Idle | MeterEvent::Failed => {
                if event == MeterEvent::Failed && !m.failed {
                    m.failed = true;
                    m.failures = m.failures.saturating_add(1);
                    let wait = retry_delay(m.failures);
                    m.retry_at = Some(Instant::now() + wait);
                    log::debug!(
                        "audio: the meter on device {} stopped; again in {wait:?}",
                        m.device
                    );
                }
                // The device stopped: what it held is dropped, and a meter
                // that showed sound says it is quiet.
                self.out.extend(m.pause().map(AudioChange::Levels));
            }
        }
    }

    // --- Deadlines ---------------------------------------------------------

    /// The next deadline: a connection's settling, defaults kept from
    /// before, actions waiting without a connection, a failed meter's
    /// retry. `None` (the timer off) when nothing waits.
    fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut at = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        match &self.session {
            Some(s) => {
                if !s.published && !s.settled {
                    at(s.settle_by);
                }
                if !s.initial_done {
                    at(s.since + UNANSWERED);
                }
                if let Some(h) = &s.held {
                    at(h.until);
                }
                for m in s.meters.iter().filter(|m| m.failed) {
                    if let Some(t) = m.retry_at {
                        at(t);
                    }
                }
            }
            None if !self.queued.is_empty() => at(self.grace_until),
            None => {}
        }
        if let Some(t) = self.host.deadline() {
            at(t);
        }
        next
    }

    fn arm_deadline(&mut self) {
        let next = self.next_deadline();
        if next == self.deadline_at {
            return;
        }
        self.deadline_at = next;
        let delay = next.map(|t| {
            t.saturating_duration_since(Instant::now())
                .max(Duration::from_micros(100))
        });
        let _ = self.deadline.update_timer(delay, None);
    }

    fn deadline(&mut self) {
        // A timer never fires early; the margin covers clock rounding.
        let now = Instant::now() + Duration::from_millis(1);
        if self
            .session
            .as_ref()
            .is_some_and(|s| !s.initial_done && s.since + UNANSWERED <= now)
        {
            log::info!("audio: PipeWire accepted the connection but never answered; reconnecting");
            // What waited for this connection does not wait for the next.
            for (_, reply) in self.queued.drain(..) {
                let _ = reply.send(Err(AudioError::NotConnected));
            }
            self.lost();
            return;
        }
        match &mut self.session {
            Some(s) => {
                if !s.published && !s.settled && s.settle_by <= now {
                    log::debug!("audio: no session manager yet; showing what PipeWire has");
                    s.settled = true;
                    self.dirty = true;
                }
                if s.held.as_ref().is_some_and(|h| h.until <= now) {
                    s.held = None;
                    self.dirty = true;
                    self.meters_dirty = true;
                }
                if s.meters
                    .iter()
                    .any(|m| m.failed && m.retry_at.is_some_and(|t| t <= now))
                {
                    self.meters_dirty = true;
                }
            }
            None => {
                if self.grace_until <= now {
                    for (_, reply) in self.queued.drain(..) {
                        let _ = reply.send(Err(AudioError::NotConnected));
                    }
                }
            }
        }
    }
}

/// The defaults `shown`, kept until `until`.
fn held(shown: &AudioState, until: Instant) -> Option<Held> {
    let defaults = Defaults {
        sink: shown.sink().map(|d| d.name.clone()),
        source: shown.source().map(|d| d.name.clone()),
        ..Defaults::default()
    };
    (defaults.sink.is_some() || defaults.source.is_some()).then_some(Held { defaults, until })
}

/// Whether a connection's first state may go out before [`SETTLE`]: its
/// syncs are back, the session manager's `default` metadata is read, and
/// each default shown before the loss names a device again.
fn ready_to_publish(s: &Session) -> bool {
    s.initial_done
        && s.awaiting.is_none()
        && s.meta_ready
        && (!s.expect_sink || default_id(s, Direction::Sink).is_some())
        && (!s.expect_source || default_id(s, Direction::Source).is_some())
}

/// Whether a core error means the connection is gone (what pw-cli and
/// pw-mon treat so); any other error on the core is logged and kept.
fn disconnected(res: i32) -> bool {
    use rustix::io::Errno;
    [Errno::PIPE, Errno::CONNRESET, Errno::NOTCONN]
        .iter()
        .any(|e| res == -e.raw_os_error())
}

/// Issues a sync after a bind; the bound object is shown once it returns.
fn sync(s: &mut Session) -> Option<i32> {
    match s.core.sync(0) {
        Ok(seq) => {
            s.awaiting = Some(seq.seq());
            Some(seq.seq())
        }
        Err(e) => {
            log::warn!("audio: cannot sync: {e}");
            None
        }
    }
}

fn apply_props(n: &mut NodeEntry, props: &Props) {
    if let Some(cv) = &props.channel_volumes {
        n.has_channel_volumes = true;
        n.channels = cv.len() as u32;
        n.device.volume = n.echoes.read(cv).unwrap_or(1.0);
    } else if let Some(v) = props.volume
        && !n.has_channel_volumes
    {
        n.device.volume = n.echoes.read(&[v]).unwrap_or(1.0);
    }
    if let Some(m) = props.mute {
        n.device.muted = m;
    }
}

fn apply_meta(d: &mut Defaults, key: Option<&str>, value: Option<&str>) {
    let Some(key) = key else {
        *d = Defaults::default();
        return;
    };
    let slot = match key {
        "default.audio.sink" => &mut d.sink,
        "default.audio.source" => &mut d.source,
        "default.configured.audio.sink" => &mut d.configured_sink,
        "default.configured.audio.source" => &mut d.configured_source,
        _ => return,
    };
    *slot = value.and_then(default_name);
}

/// The default device of `direction`: the effective default when it names
/// a device, else the configured one, else (for a while after a restart)
/// the one shown before.
fn default_id(s: &Session, direction: Direction) -> Option<u32> {
    let find = |name: &Option<String>| {
        let name = name.as_deref()?;
        s.nodes
            .values()
            .find(|n| n.ready && n.direction == direction && n.device.name == name)
            .map(|n| n.device.id)
    };
    let held = s.held.as_ref().map(|h| h.defaults.names(direction));
    s.defaults
        .names(direction)
        .into_iter()
        .chain(held.into_iter().flatten())
        .find_map(find)
}

fn state_of(s: &Session) -> AudioState {
    let sink = default_id(s, Direction::Sink);
    let source = default_id(s, Direction::Source);
    let mut state = AudioState {
        connected: true,
        ..AudioState::default()
    };
    for n in s.nodes.values().filter(|n| n.ready) {
        let mut d = n.device.clone();
        d.default = Some(d.id) == sink || Some(d.id) == source;
        d.icon = icon(n.direction, d.volume, d.muted).to_owned();
        if let Some(serial) = n.serial.as_deref().and_then(|v| v.parse().ok()) {
            state.serials.insert(d.id, serial);
        }
        match n.direction {
            Direction::Sink => state.sinks.push(d),
            Direction::Source => state.sources.push(d),
        }
    }
    state
}

fn node_mut(s: &mut Session, d: DeviceRef) -> Result<&mut NodeEntry, AudioError> {
    let id = match d {
        DeviceRef::DefaultSink => default_id(s, Direction::Sink),
        DeviceRef::DefaultSource => default_id(s, Direction::Source),
        DeviceRef::Id(id) => Some(id),
    };
    id.and_then(|id| s.nodes.get_mut(&id))
        .filter(|n| n.ready)
        .ok_or(AudioError::UnknownDevice(d))
}

/// Writes volume or mute to node `id`: through its card's active route
/// when it has one (as `wpctl` and pipewire-pulse do, so the hardware
/// mixer moves and the session manager saves it), else to its `Props`.
/// Where a write went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Written {
    /// The node's `Props`: its echo is exact.
    Node,
    /// A card's `Route`: its echo is quantized to the mixer's steps.
    Route,
}

fn write(s: &Session, id: u32, props: &Props) -> Result<Written, AudioError> {
    let n = s
        .nodes
        .get(&id)
        .ok_or(AudioError::UnknownDevice(DeviceRef::Id(id)))?;
    let via = write_via(n.link, |device| {
        let profile_device = n.link?.profile_device;
        s.devices.get(&device)?.routes.active(profile_device)
    });
    match via {
        WriteVia::Route { device, route } => {
            let d = s.devices.get(&device).ok_or_else(|| {
                AudioError::Failed(format!("device {device} of node {id} is gone"))
            })?;
            let bytes = route.write_pod(props).map_err(AudioError::Failed)?;
            let pod = Pod::from_bytes(&bytes)
                .ok_or_else(|| AudioError::Failed("bad Route pod".into()))?;
            d.proxy.set_param(ParamType::Route, 0, pod);
            Ok(Written::Route)
        }
        WriteVia::Node => {
            let bytes = props.to_pod().map_err(AudioError::Failed)?;
            let pod = Pod::from_bytes(&bytes)
                .ok_or_else(|| AudioError::Failed("bad Props pod".into()))?;
            n.proxy.set_param(ParamType::Props, 0, pod);
            Ok(Written::Node)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_go_through_the_active_route_of_a_card_node() {
        let route = Route {
            index: 4,
            device: 1,
        };
        let routes = |device: u32| (device == 50).then_some(route);
        // A null sink, or any node of no card: its Props.
        assert_eq!(write_via(None, routes), WriteVia::Node);
        // A card's node whose profile device has an active route.
        let link = RouteLink {
            device: 50,
            profile_device: 1,
        };
        assert_eq!(
            write_via(Some(link), routes),
            WriteVia::Route { device: 50, route }
        );
        // A card without that route (another profile device, or no
        // routes at all, e.g. a pro-audio profile): its Props.
        let other = RouteLink {
            profile_device: 0,
            ..link
        };
        assert_eq!(write_via(Some(other), routes), WriteVia::Node);
        let elsewhere = RouteLink { device: 51, ..link };
        assert_eq!(write_via(Some(elsewhere), routes), WriteVia::Node);
    }

    #[test]
    fn node_links_come_from_its_properties() {
        assert_eq!(
            RouteLink::of(Some("48"), Some("1")),
            Some(RouteLink {
                device: 48,
                profile_device: 1
            })
        );
        assert_eq!(RouteLink::of(Some("48"), None), None);
        assert_eq!(RouteLink::of(None, Some("1")), None);
        assert_eq!(RouteLink::of(Some("x"), Some("1")), None);
    }

    #[test]
    fn a_device_uses_only_its_current_set_of_routes() {
        let r = |index, device| Route { index, device };
        let mut routes = Routes::default();
        // Before any info and set: nothing.
        assert_eq!(routes.active(0), None);
        routes.info(Some(0b111));
        assert_eq!(routes.active(0), None);
        routes.route(r(2, 0));
        routes.route(r(5, 1));
        assert_eq!(routes.active(0), Some(r(2, 0)));
        assert_eq!(routes.active(1), Some(r(5, 1)));
        // The same flags again (another param changed): kept.
        routes.info(Some(0b111));
        assert_eq!(routes.active(1), Some(r(5, 1)));
        // The routes changed (the serial bit toggled): none is used
        // until the new set comes, which replaces the old one.
        routes.info(Some(0b110));
        assert_eq!(routes.active(0), None);
        routes.route(r(3, 1));
        assert_eq!(routes.active(1), Some(r(3, 1)));
        assert_eq!(routes.active(0), None);
        // A set with no routes never comes: none is used.
        routes.info(Some(0b111));
        assert_eq!(routes.active(1), None);
    }

    #[test]
    fn echoes_of_recent_writes_read_back_as_written() {
        let mut e = Echoes::default();
        // Not ours: the cube root.
        assert_eq!(e.read(&[0.125, 0.125]), Some(0.5));
        // A slider sends three writes before the first echo.
        for v in [0.3, 0.31, 0.32] {
            e.wrote(v, vec![linear(v); 2], false);
        }
        assert_eq!(e.base(0.5), 0.32);
        // The echoes come back one by one, each as written.
        assert_eq!(e.read(&[linear(0.3); 2]), Some(0.3));
        assert_eq!(e.base(0.3), 0.32, "the last write is still pending");
        assert_eq!(e.read(&[linear(0.31); 2]), Some(0.31));
        assert_eq!(e.read(&[linear(0.32); 2]), Some(0.32));
        assert_eq!(e.base(0.32), 0.32);
        assert_eq!(e.pending, None);
        // Another program's volume ends the pending write.
        e.wrote(0.9, vec![linear(0.9)], false);
        assert_eq!(e.read(&[0.008]), perceptual(&[0.008]));
        assert_eq!(e.base(0.2), 0.2);
    }

    #[test]
    fn an_echo_of_a_forgotten_write_still_reads_as_written() {
        let mut e = Echoes::default();
        // A slider drag: more writes than the ring holds before the first
        // echo, on the grid (as `strand set` and a slider's steps are).
        let grid: Vec<f64> = (0..ECHOES as u32 + 10)
            .map(|i| f64::from(300 + i) / 1000.0)
            .collect();
        for v in &grid {
            e.wrote(*v, vec![linear(*v); 2], false);
        }
        assert_eq!(e.written.len(), ECHOES);
        // The sync issued after the last write is out.
        e.wait = Some(1);
        // The oldest writes are forgotten, and their echoes still read as
        // written (not 0.3000000025939058).
        for v in &grid[..10] {
            assert_eq!(e.read(&[linear(*v); 2]), Some(*v));
        }
        assert_eq!(e.pending, Some(grid[grid.len() - 1]));
        for v in &grid[10..] {
            assert_eq!(e.read(&[linear(*v); 2]), Some(*v));
        }
        assert_eq!(e.pending, None);
        // Off the grid: any of the last ECHOES reads as written.
        let off: Vec<f64> = (0..ECHOES as u32)
            .map(|i| 0.2 + f64::from(i) * 0.001_234_567)
            .collect();
        for v in &off {
            e.wrote(*v, vec![linear(*v); 2], false);
        }
        for v in &off {
            assert_eq!(e.read(&[linear(*v); 2]), Some(*v));
        }
    }

    #[test]
    fn an_older_report_before_a_write_is_applied_keeps_the_step_base() {
        let mut e = Echoes::default();
        // Mute, then +5% from 0.5: the volume write waits for sync 7.
        e.wrote(0.55, vec![linear(0.55); 2], false);
        e.wait = Some(7);
        // The mute's echo carries the old volume: shown, but a second
        // step still adds to the write, not to it.
        assert_eq!(e.read(&[0.125, 0.125]), Some(0.5));
        assert_eq!(e.base(0.5), 0.55);
        e.synced(6);
        assert_eq!(e.read(&[0.125, 0.125]), Some(0.5));
        assert_eq!(e.base(0.5), 0.55);
        // The write's echo, then its sync.
        assert_eq!(e.read(&[linear(0.55); 2]), Some(0.55));
        assert_eq!(e.pending, None);
        e.synced(7);
        assert_eq!(e.wait, None);
        // Once a write is applied, a volume matching none of the writes
        // is another program's, and ends a pending write.
        e.wrote(0.6, vec![linear(0.6); 2], false);
        e.wait = Some(9);
        e.synced(9);
        assert_eq!(e.read(&[0.008, 0.008]), perceptual(&[0.008]));
        assert_eq!(e.base(0.2), 0.2);
    }

    #[test]
    fn a_quantized_echo_reads_as_written() {
        // A card's route moves the mixer in steps: its echo is close to,
        // not exactly, the volume written.
        let mut e = Echoes::default();
        e.wrote(0.37, vec![linear(0.37); 2], true);
        assert_eq!(e.read(&[linear(0.3681); 2]), Some(0.37));
        assert_eq!(e.pending, None);
        // Farther than that is another volume.
        let other = e.read(&[linear(0.38); 2]).unwrap();
        assert!((other - 0.38).abs() < 1e-6, "{other}");
        // A write to a node's `Props` echoes exactly: a near miss is
        // another volume, read as it is.
        let mut e = Echoes::default();
        e.wrote(0.37, vec![linear(0.37); 2], false);
        assert_eq!(e.read(&[linear(0.368); 2]), Some(0.368));
    }

    #[test]
    fn inotify_events_that_end_or_overflow_the_watch() {
        let name = OsString::from("pipewire-0");
        let mut seen = Seen::default();
        let other = std::ffi::CString::new("other").unwrap();
        seen.add(3, inotify::ReadFlags::CREATE, Some(&other), &name);
        assert_eq!(seen, Seen::default());
        let ours = std::ffi::CString::new("pipewire-0").unwrap();
        seen.add(3, inotify::ReadFlags::CREATE, Some(&ours), &name);
        assert!(seen.hit);
        let mut seen = Seen::default();
        seen.add(-1, inotify::ReadFlags::QUEUE_OVERFLOW, None, &name);
        assert!(seen.hit, "an overflow may have lost the socket's event");
        let mut seen = Seen::default();
        seen.add(3, inotify::ReadFlags::IGNORED, None, &name);
        assert_eq!(seen.gone, [3]);
        assert!(!seen.hit);
    }

    #[test]
    fn only_a_missing_socket_is_waited_for() {
        let dir = std::env::temp_dir().join(format!("strand-pw-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("pipewire-0");
        let _ = std::fs::remove_file(&socket);
        let watch = SocketWatch::new(Some(&socket.display().to_string())).unwrap();
        assert!(watch.socket_missing());
        // A stale socket (a crashed daemon's), or one that refuses: the
        // timer keeps trying.
        std::fs::write(&socket, b"").unwrap();
        assert!(!watch.socket_missing());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_broken_connection_counts_as_lost() {
        assert!(disconnected(-32)); // EPIPE
        assert!(disconnected(-104)); // ECONNRESET
        assert!(!disconnected(-22)); // EINVAL: a bad request
        assert!(!disconnected(-2)); // ENOENT
        assert!(!disconnected(0));
    }
}

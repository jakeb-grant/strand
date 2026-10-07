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

use super::meter::{Meter, MeterEvent, MeterTarget};
use super::model::{
    AudioChange, AudioDevice, AudioState, Direction, LevelTarget, Publisher, icon, linear,
    perceptual,
};
use super::pod::{Props, Route, default_name, default_value};
use super::{AudioAction, AudioConfig, AudioError, Cmd, DeviceRef};

/// The first wait after a lost or refused connection.
const FIRST: Duration = Duration::from_millis(100);
/// The longest wait between attempts. Once the doubling passes it and the
/// socket's directory is watched, attempts wait for the socket to appear.
const MAX: Duration = Duration::from_secs(10);

/// The shortest time between two readings of one meter (60 per second):
/// readings in between are held, the loudest peak per channel kept.
pub const FRAME: Duration = Duration::from_micros(16_667);

/// How many of its last volume writes a device remembers, so the echo of
/// any of them reads back exactly as written.
pub const ECHOES: usize = 8;

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
    Reading {
        meter: u64,
        peaks: Vec<f32>,
    },
    Meter {
        meter: u64,
        event: MeterEvent,
    },
}

/// The thread's body.
pub(crate) fn run(
    config: AudioConfig,
    sink: Box<dyn FnMut(Vec<AudioChange>) + Send>,
    rx: pipewire::channel::Receiver<Cmd>,
) {
    pipewire::init();
    let mut sink = sink;
    let mainloop = match MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            log::error!("audio: cannot create a PipeWire loop: {e}");
            sink(Publisher::new().publish(AudioState::default()));
            return;
        }
    };
    let context = match ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("audio: cannot create a PipeWire context: {e}");
            sink(Publisher::new().publish(AudioState::default()));
            return;
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
        watch,
        sink,
        session: None,
        sessions: 0,
        publisher: Publisher::new(),
        dirty: false,
        meters_dirty: false,
        out: Vec::new(),
        backoff: FIRST,
        wanted: BTreeSet::new(),
        meters: 0,
        quit: false,
    };
    driver.connect();
    driver.drain();
    while !driver.quit {
        lp.iterate(Timeout::Infinite);
        driver.drain();
    }
    // The session (its streams, proxies and core) goes before the loop's
    // sources, the context and the loop.
    drop(driver);
}

/// The inotify fd, shared by the loop's io source and the driver.
struct WatchFd(Rc<OwnedFd>);

impl AsRawFd for WatchFd {
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
    /// The connection's first sync has come back.
    initial_done: bool,
    /// The sync after the latest bind, until it comes back.
    awaiting: Option<i32>,
    /// The first state of this connection has gone out.
    published: bool,
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
/// also when a slider sent several writes before the first echo.
#[derive(Debug, Default)]
struct Echoes {
    written: VecDeque<(f64, Vec<f32>)>,
    /// The last write, until PipeWire has reported it (or another
    /// volume): a step adds to this, not to a volume it is replacing.
    pending: Option<f64>,
}

impl Echoes {
    fn wrote(&mut self, volume: f64, linear: Vec<f32>) {
        if self.written.len() == ECHOES {
            self.written.pop_front();
        }
        self.written.push_back((volume, linear));
        self.pending = Some(volume);
    }

    /// The volume to show for reported linear channel volumes.
    fn read(&mut self, reported: &[f32]) -> Option<f64> {
        let hit = self.written.iter().rposition(|(_, lin)| lin == reported);
        if hit.is_none_or(|i| i + 1 == self.written.len()) {
            // The last write landed, or another program set it.
            self.pending = None;
        }
        hit.map(|i| self.written[i].0)
            .or_else(|| perceptual(reported))
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
    watch: Option<SocketWatch>,
    sink: Box<dyn FnMut(Vec<AudioChange>) + Send>,
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
    quit: bool,
}

impl Driver<'_> {
    /// Handles every queued item, then publishes once.
    fn drain(&mut self) {
        loop {
            loop {
                let w = self.q.borrow_mut().pop_front();
                let Some(w) = w else { break };
                self.handle(w);
            }
            if self.dirty {
                self.publish();
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
            (self.sink)(std::mem::take(&mut self.out));
        }
    }

    fn live(&self, session: u64) -> bool {
        self.session.as_ref().is_some_and(|s| s.number == session)
    }

    fn handle(&mut self, w: Work) {
        match w {
            Work::Cmd(Cmd::Stop) => self.quit = true,
            Work::Cmd(Cmd::Levels(set)) => {
                self.wanted = set;
                self.meters_dirty = true;
            }
            Work::Cmd(Cmd::Action(action, reply)) => {
                let r = self.action(action);
                if let Err(e) = &r {
                    log::debug!("audio: {e}");
                }
                let _ = reply.send(r);
            }
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
                    for n in s.nodes.values_mut() {
                        if !n.ready && seq >= n.sync {
                            n.ready = true;
                        }
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
                self.flush_meters();
            }
            Work::Reading { meter, peaks } => self.reading(meter, peaks),
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
            Ok(session) => {
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
            initial_done: false,
            awaiting: Some(seq),
            published: false,
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
                self.out.extend(m.quiet().map(AudioChange::Levels));
            }
        }
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
        let watched = self.watch.as_ref().is_some_and(|w| w.wd.is_some());
        if self.backoff > MAX && watched {
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
                if sync(s).is_none() {
                    return;
                }
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
            s.metadata = None;
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
                if !s.published && !(s.initial_done && s.awaiting.is_none()) {
                    // The first state of a connection waits for its sync.
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
                let props = if n.has_channel_volumes && n.channels > 0 {
                    let lin = vec![linear(v); n.channels as usize];
                    n.echoes.wrote(v, lin.clone());
                    Props {
                        channel_volumes: Some(lin),
                        ..Props::default()
                    }
                } else {
                    n.echoes.wrote(v, vec![linear(v)]);
                    Props {
                        volume: Some(linear(v)),
                        ..Props::default()
                    }
                };
                let id = n.device.id;
                write(s, id, &props)
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
            // A meter that failed (a stream error) is started anew.
            if let Some(i) = old
                .iter()
                .position(|m| m.target == target && m.device == device.id && !m.failed)
            {
                keep.push(old.swap_remove(i));
                continue;
            }
            let Some(node) = s.nodes.get(&device.id) else {
                continue;
            };
            self.meters += 1;
            let t = MeterTarget {
                id: device.id,
                name: &device.name,
                serial: node.serial.as_deref(),
                direction: node.direction,
            };
            match Meter::start(&s.core, self.meters, target, &t, &self.q) {
                Ok(m) => keep.push(m),
                Err(e) => log::warn!("audio: {e}"),
            }
        }
        // Meters no longer wanted, or retargeted, stop here; each that
        // last showed sound says it is quiet, so a reader never keeps a
        // stale level (hidden while the sound stopped, or a default that
        // moved to a silent device).
        for mut m in old {
            self.out.extend(m.quiet().map(AudioChange::Levels));
        }
        s.meters = keep;
    }

    /// A meter's reading: held, the loudest peak per channel kept, and
    /// sent at most once per [`FRAME`]. Only the first silent reading
    /// after sound is sent.
    fn reading(&mut self, meter: u64, peaks: Vec<f32>) {
        let now = Instant::now();
        let Some(m) = self
            .session
            .as_mut()
            .and_then(|s| s.meters.iter_mut().find(|m| m.id == meter))
        else {
            return;
        };
        if m.hold.add(peaks, now)
            && let Some(l) = m.take(now)
        {
            self.out.push(AudioChange::Levels(l));
        }
        if let Some(at) = m.hold.due(now) {
            self.arm_flush(at, now);
        }
    }

    /// The frame timer: sends what each meter held once it is due.
    fn flush_meters(&mut self) {
        let now = Instant::now();
        let Some(s) = &mut self.session else { return };
        let mut next: Option<Instant> = None;
        for m in &mut s.meters {
            match m.hold.due(now) {
                Some(at) if at <= now => self.out.extend(m.take(now).map(AudioChange::Levels)),
                Some(at) => next = Some(next.map_or(at, |n| n.min(at))),
                None => {}
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
        if event == MeterEvent::Failed && !m.failed {
            m.failed = true;
            log::debug!("audio: the meter on device {} stopped", m.device);
        }
        // The device stopped: what it held is dropped, and a meter that
        // showed sound says it is quiet.
        self.out.extend(m.quiet().map(AudioChange::Levels));
    }
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
/// a device, else the configured one.
fn default_id(s: &Session, direction: Direction) -> Option<u32> {
    let (effective, configured) = match direction {
        Direction::Sink => (&s.defaults.sink, &s.defaults.configured_sink),
        Direction::Source => (&s.defaults.source, &s.defaults.configured_source),
    };
    let find = |name: &Option<String>| {
        let name = name.as_deref()?;
        s.nodes
            .values()
            .find(|n| n.ready && n.direction == direction && n.device.name == name)
            .map(|n| n.device.id)
    };
    find(effective).or_else(|| find(configured))
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
fn write(s: &Session, id: u32, props: &Props) -> Result<(), AudioError> {
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
        }
        WriteVia::Node => {
            let bytes = props.to_pod().map_err(AudioError::Failed)?;
            let pod = Pod::from_bytes(&bytes)
                .ok_or_else(|| AudioError::Failed("bad Props pod".into()))?;
            n.proxy.set_param(ParamType::Props, 0, pod);
        }
    }
    Ok(())
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
            e.wrote(v, vec![linear(v); 2]);
        }
        assert_eq!(e.base(0.5), 0.32);
        // The echoes come back one by one, each as written.
        assert_eq!(e.read(&[linear(0.3); 2]), Some(0.3));
        assert_eq!(e.base(0.3), 0.32, "the last write is still pending");
        assert_eq!(e.read(&[linear(0.31); 2]), Some(0.31));
        assert_eq!(e.read(&[linear(0.32); 2]), Some(0.32));
        assert_eq!(e.base(0.32), 0.32);
        assert_eq!(e.pending, None);
        // Only the last ECHOES writes are remembered.
        for i in 0..=ECHOES {
            e.wrote(0.1 + i as f64 / 100.0, vec![linear(0.1 + i as f64 / 100.0)]);
        }
        assert_eq!(e.written.len(), ECHOES);
        assert_eq!(e.read(&[linear(0.1)]), perceptual(&[linear(0.1)]));
        // Another program's volume ends the pending write.
        e.wrote(0.9, vec![linear(0.9)]);
        assert_eq!(e.read(&[0.008]), perceptual(&[0.008]));
        assert_eq!(e.base(0.2), 0.2);
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
    fn only_a_broken_connection_counts_as_lost() {
        assert!(disconnected(-32)); // EPIPE
        assert!(disconnected(-104)); // ECONNRESET
        assert!(!disconnected(-22)); // EINVAL: a bad request
        assert!(!disconnected(-2)); // ENOENT
        assert!(!disconnected(0));
    }
}

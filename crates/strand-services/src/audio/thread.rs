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
    AudioChange, AudioDevice, AudioState, Direction, LevelTarget, Levels, Publisher, linear,
    perceptual,
};
use super::pod::{Props, default_name, default_value};
use super::{AudioAction, AudioConfig, AudioError, Cmd, DeviceRef};

/// The first wait after a lost or refused connection.
const FIRST: Duration = Duration::from_millis(100);
/// The longest wait between attempts. Once the doubling passes it and the
/// socket's directory is watched, attempts wait for the socket to appear.
const MAX: Duration = Duration::from_secs(10);

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
    let watch = SocketWatch::new(config.remote.as_deref());
    let _watch_source = watch.as_ref().map(|w| {
        let q = q.clone();
        let name = w.name.clone();
        lp.add_io(WatchFd(w.fd.clone()), IoFlags::IN, move |fd| {
            if drain_inotify(&fd.0, &name) {
                q.borrow_mut().push_back(Work::SocketAppeared);
            }
        })
    });

    let mut driver = Driver {
        config,
        q,
        context,
        timer: &timer,
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

/// Reads every queued inotify event; true if one named `name`.
fn drain_inotify(fd: &OwnedFd, name: &OsString) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(fd.as_fd(), &mut buf);
    let mut hit = false;
    // Bounded: a reader that keeps failing never spins.
    for _ in 0..4096 {
        match reader.next() {
            Ok(ev) => {
                if ev.file_name().map(|n| n.to_bytes()) == Some(name.as_bytes()) {
                    hit = true;
                }
            }
            Err(_) => break,
        }
    }
    hit
}

/// One connection to the daemon and what it follows.
struct Session {
    number: u64,
    // Drop order: meters (streams), proxies and listeners, then the
    // registry and the core.
    meters: Vec<Meter>,
    nodes: BTreeMap<u32, NodeEntry>,
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
    serial: Option<String>,
    /// The sync issued after binding it; it is shown once that returns
    /// (with its properties and volume read).
    sync: i32,
    ready: bool,
    /// The last volume we wrote: what we wrote and the channel volumes it
    /// became, so the echo reads back exactly as written.
    written: Option<(f64, Vec<f32>)>,
    /// It has reported `channelVolumes` (the `volume` prop is then
    /// ignored).
    has_channel_volumes: bool,
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
            } if self.live(session) => {
                if let Some(n) = self.session.as_mut().and_then(|s| s.nodes.get_mut(&id)) {
                    if let Some(name) = name {
                        n.device.name = name;
                    }
                    if let Some(d) = description {
                        n.device.description = d;
                    }
                    self.dirty = true;
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
                if id == PW_ID_CORE {
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
            Work::Reading { meter, peaks } => self.reading(meter, peaks),
            Work::Meter { meter, event } => self.meter_event(meter, event),
            // Work of a dropped connection.
            Work::Global { .. }
            | Work::GlobalRemove { .. }
            | Work::NodeInfo { .. }
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
                if matches!(g.type_, ObjectType::Node | ObjectType::Metadata) {
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
            for m in s.meters.drain(..) {
                if !m.silent {
                    self.out.push(AudioChange::Levels(Levels {
                        target: m.target,
                        device: m.device,
                        peaks: Vec::new(),
                    }));
                }
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
                        serial: get("object.serial"),
                        sync,
                        ready: false,
                        written: None,
                        has_channel_volumes: false,
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
            AudioAction::SetVolume(d, v) => {
                if !v.is_finite() {
                    return Err(AudioError::InvalidVolume(v));
                }
                let n = node_mut(s, d)?;
                let v = v.clamp(0.0, n.device.volume.max(1.0));
                let props = if n.has_channel_volumes && n.device.channels > 0 {
                    let lin = vec![linear(v); n.device.channels as usize];
                    n.written = Some((v, lin.clone()));
                    Props {
                        channel_volumes: Some(lin),
                        ..Props::default()
                    }
                } else {
                    Props {
                        volume: Some(linear(v)),
                        ..Props::default()
                    }
                };
                set_props(&n.proxy, &props)
            }
            AudioAction::SetMuted(d, m) => {
                let n = node_mut(s, d)?;
                set_props(
                    &n.proxy,
                    &Props {
                        mute: Some(m),
                        ..Props::default()
                    },
                )
            }
            AudioAction::MakeDefault(d) => {
                let (name, direction) = {
                    let n = node_mut(s, d)?;
                    (n.device.name.clone(), n.device.direction)
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
            if let Some(i) = old
                .iter()
                .position(|m| m.target == target && m.device == device.id)
            {
                keep.push(old.swap_remove(i));
                continue;
            }
            let serial = s.nodes.get(&device.id).and_then(|n| n.serial.as_deref());
            self.meters += 1;
            let t = MeterTarget {
                id: device.id,
                name: &device.name,
                serial,
                direction: device.direction,
            };
            match Meter::start(&s.core, self.meters, target, &t, &self.q) {
                Ok(m) => keep.push(m),
                Err(e) => log::warn!("audio: {e}"),
            }
        }
        // Meters no longer wanted, or retargeted, stop here.
        drop(old);
        s.meters = keep;
    }

    fn reading(&mut self, meter: u64, peaks: Vec<f32>) {
        let Some(m) = self
            .session
            .as_mut()
            .and_then(|s| s.meters.iter_mut().find(|m| m.id == meter))
        else {
            return;
        };
        let silent = peaks.iter().all(|p| *p == 0.0);
        if silent && m.silent {
            return;
        }
        m.silent = silent;
        self.out.push(AudioChange::Levels(Levels {
            target: m.target,
            device: m.device,
            peaks,
        }));
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
        if !m.silent {
            m.silent = true;
            self.out.push(AudioChange::Levels(Levels {
                target: m.target,
                device: m.device,
                peaks: Vec::new(),
            }));
        }
    }
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
        n.device.channels = cv.len() as u32;
        n.device.volume = match &n.written {
            Some((v, lin)) if lin == cv => *v,
            _ => perceptual(cv).unwrap_or(1.0),
        };
    } else if let Some(v) = props.volume
        && !n.has_channel_volumes
    {
        n.device.volume = perceptual(&[v]).unwrap_or(1.0);
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
            .find(|n| n.ready && n.device.direction == direction && n.device.name == name)
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
        match d.direction {
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

fn set_props(node: &Node, props: &Props) -> Result<(), AudioError> {
    let bytes = props.to_pod().map_err(AudioError::Failed)?;
    let pod = Pod::from_bytes(&bytes).ok_or_else(|| AudioError::Failed("bad Props pod".into()))?;
    node.set_param(ParamType::Props, 0, pod);
    Ok(())
}

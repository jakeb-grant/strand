//! `strand run [dir]`: the config in `dir` (default
//! `$XDG_CONFIG_HOME/strand`) compiled and run, wired as
//! `docs/architecture.md` describes ("Threads", and the host-loop recipe
//! under Instantiation).
//!
//! - The main thread runs the surface manager and the renderer, as the
//!   demo does. Its host forwards the surface layer's monitor hooks to the
//!   logic thread as the `screens` service (`Screen.id` is the
//!   `MonitorId`; `monitor_forgotten` is `Instance::forget_screen`).
//!   Pointer input goes to the node under the pointer
//!   (`Renderer::hit`'s chain, topmost in paint order): the chain is
//!   hovered and pressed, a release clicks the innermost node both the
//!   press and the release were over, a scroll goes to the innermost
//!   node, and logic bubbles events to the nearest handler. Layout facts
//!   (`self.width`) are still per surface, `set_size` on the surface's
//!   node: layout boxes, and hits on containers that paint nothing, are
//!   M2.
//! - The compiler worker (`live.rs`) compiles saves off the logic
//!   thread; [`Shell::commit`] reloads the instance with each result
//!   (a lock edit, or a hard reload, waits while a lock is shown), puts
//!   diagnostics and reload notices on the overlay and streams the event
//!   to `strand watch`.
//! - The logic thread owns the runtime, the real service host (the
//!   services registry and its stores behind a composite host,
//!   `services::Real`; `SchemaHost::real` answers the rest, the wall
//!   clock and calendar among them; `STRAND_MOCK` selects the mock host
//!   instead) and the `Instance`, and loops on `Instance::step(now, wall)`, sending one
//!   `SceneDiff` per tick. Between steps it sleeps in a calloop loop of
//!   its own on the main thread's messages, the runtime's wake hook (an
//!   IO reply), the logic clock's next deadline and a `CLOCK_REALTIME`
//!   timerfd armed at the wall-clock wake (`TFD_TIMER_ABSTIME`, with
//!   `TFD_TIMER_CANCEL_ON_SET`): after a suspend or a clock step the
//!   clock shows the new time at once, not when a monotonic countdown
//!   runs out.
//! - SIGINT, SIGTERM and the compositor going away end the run the same
//!   way: the main thread sends [`ToLogic::Shutdown`] and joins the logic
//!   thread, which unmounts the instance and drops its stores, so
//!   debounced `persist` and settings writes reach the disk.

use std::cell::Cell;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use calloop::channel::{Channel, Event, Sender};
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use rustix::time::Timespec;
use serde_json::{Value as Json, json};
use strand_compiler::diagnostic::{Style, render, render_short};
use strand_compiler::instantiate::{Instance, NodeFlag, Storage};
use strand_compiler::reconcile::loader::Outcome;
use strand_compiler::reconcile::{Build, EditClass, Report};
use strand_compiler::vm::Value;
use strand_compiler::vm::clock::{Clock, Zone};
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_render::{Renderer, TextBackend};
use strand_scene::{NodeId, SceneDiff};
use strand_services::Cells as _;
use strand_surface::{Config, SurfaceManager};
use strand_text::{FontConfig, TextWorker};

use crate::demo::clock::WallTimer;
use crate::demo::host::Host;
use crate::demo::{DemoError, FIRST_FRAME_TEXT_WAIT, apply, connection_closed};
use crate::ipc;
use crate::live::{self, FromWorker, Job, Loaded, Worker};
use crate::logging::LogConfig;
use crate::overlay::{self, Click, Overlay};
use crate::system;
use strand_watch::Role;

/// A monitor as the `screens` service shows it (plain data: it crosses
/// threads).
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenInfo {
    /// The `MonitorId` (make, model and description).
    pub id: String,
    /// The connector (`DP-1`), or the id when the compositor gives none.
    pub name: String,
    pub make: String,
    pub model: String,
    pub description: String,
    pub scale: f64,
    /// Logical size, when known.
    pub width: f64,
    pub height: f64,
}

/// An input event on a node, as render resolved it (the node under the
/// pointer). One variant per kind of event, each with its own payload:
/// M2 adds keyboard input (`Key`, `Text`, `Activate`, for
/// `keyboard: on_demand | exclusive` and the launcher) and M4 `Drop`
/// (`on drop(p: T, at: int)`) as new variants, so the render → logic
/// message stays the same.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeEvent {
    /// A left click (`on click`).
    Click,
    /// A right click (`on secondary`).
    Secondary,
    /// A scroll in logical pixels, positive down and right
    /// (`on scroll(dy, dx)`).
    Scroll { dy: f64, dx: f64 },
    /// A middle click (`on middle`).
    Middle,
    /// A list row chosen with Enter (`on activate`).
    Activate,
    /// A key pressed while the node has focus (`on key(k)`).
    Key {
        name: String,
        text: String,
        modifiers: strand_scene::Modifiers,
    },
    /// Escape or a click away closed a popup (`on dismiss`).
    Dismiss,
}

impl NodeEvent {
    /// The `on` event name it is delivered as.
    pub fn name(&self) -> &'static str {
        match self {
            NodeEvent::Click => "click",
            NodeEvent::Secondary => "secondary",
            NodeEvent::Scroll { .. } => "scroll",
            NodeEvent::Middle => "middle",
            NodeEvent::Activate => "activate",
            NodeEvent::Key { .. } => "key",
            NodeEvent::Dismiss => "dismiss",
        }
    }

    /// The handler's arguments (`Key` records are made by the service
    /// host: [`NodeEvent::args_with`]).
    pub fn args(&self) -> Vec<Value> {
        match self {
            NodeEvent::Scroll { dy, dx } => vec![Value::float(*dy), Value::float(*dx)],
            _ => Vec::new(),
        }
    }

    /// The handler's arguments, a `Key` record made by `host`.
    pub fn args_with(&self, host: &SchemaHost) -> Vec<Value> {
        match self {
            NodeEvent::Key {
                name,
                text,
                modifiers: m,
            } => vec![host.record(
                "Key",
                &[
                    ("name", Value::text(name.as_str())),
                    ("text", Value::text(text.as_str())),
                    ("ctrl", Value::Bool(m.ctrl)),
                    ("shift", Value::Bool(m.shift)),
                    ("alt", Value::Bool(m.alt)),
                    ("logo", Value::Bool(m.logo)),
                ],
            )],
            e => e.args(),
        }
    }
}

/// What the main thread tells the logic thread.
#[derive(Clone, Debug, PartialEq)]
pub enum ToLogic {
    /// The plugged-in monitors, in plug order (the first counts as
    /// focused until a compositor service says otherwise).
    Screens(Vec<ScreenInfo>),
    /// A monitor unplugged 30 s ago did not come back.
    Forget(String),
    /// An input event on a node (`on click`, `on scroll(dy, dx)`, …),
    /// delivered to its nearest handler.
    Event { node: NodeId, event: NodeEvent },
    /// A surface's node is hovered or pressed (`on`) or no longer.
    Flag {
        node: NodeId,
        flag: NodeFlag,
        on: bool,
    },
    /// A surface's logical size.
    Size {
        node: NodeId,
        width: f32,
        height: f32,
    },
    /// Laid-out sizes that changed (`self.width`, container queries):
    /// `(node, width, height)` in logical pixels, as fact batch `seq`
    /// (`Renderer::layout_seq`); the next diff echoes it as
    /// `SceneDiff::layout_seen`, even with no ops, which releases a frame
    /// render held for a container query's answer.
    Layout {
        seq: u64,
        sizes: Vec<(NodeId, f32, f32)>,
    },
    /// A widget or the surface wrote a two-way prop: an `input`'s
    /// `text`, a surface's `open` (Escape, click-away, focus loss).
    Write {
        node: NodeId,
        prop: strand_scene::Prop,
        value: strand_scene::PropValue,
    },
    /// The run is over (a signal, the compositor gone): unmount, flush
    /// what is kept and end.
    Shutdown,
}

/// The `screens` service fields for `screens`.
pub(crate) fn set_screens(rt: &Runtime, host: &SchemaHost, screens: &[ScreenInfo]) {
    let records: Vec<Value> = screens
        .iter()
        .enumerate()
        .map(|(i, s)| {
            host.record(
                "Screen",
                &[
                    ("id", Value::text(s.id.as_str())),
                    ("name", Value::text(s.name.as_str())),
                    ("make", Value::text(s.make.as_str())),
                    ("model", Value::text(s.model.as_str())),
                    ("description", Value::text(s.description.as_str())),
                    ("scale", Value::float(s.scale)),
                    ("width", Value::float(s.width)),
                    ("height", Value::float(s.height)),
                    ("focused", Value::Bool(i == 0)),
                ],
            )
        })
        .collect();
    let focused = records.first().cloned().unwrap_or(Value::Null);
    if let Err(e) = host.set(rt, "screens.all", Value::list(records)) {
        log::error!("screens.all: {e}");
    }
    if let Err(e) = host.set(rt, "screens.focused", focused) {
        log::error!("screens.focused: {e}");
    }
}

/// How long the first frame waits for the services it starts to send
/// their first reads (the portal's boot read has up to 500 ms; a desktop
/// portal answers in a few).
const BOOT_SERVICES_HOLD: Duration = Duration::from_millis(100);

/// The persist and settings stores under `$XDG_STATE_HOME/strand`. With
/// no state directory (neither `XDG_STATE_HOME` nor `HOME` absolute),
/// persisted state is not kept, but settings files are still read from
/// and written to the config directory, their overlays (for read-only
/// files) kept in a temporary directory for this run.
pub fn storage(dir: &Path) -> Storage {
    storage_or(dir, Storage::from_env(dir))
}

fn storage_or(dir: &Path, found: Result<Storage, strand_core::PersistError>) -> Storage {
    match found {
        Ok(s) => s,
        Err(e) => {
            let overlays = std::env::temp_dir().join(format!("strand-{}", std::process::id()));
            log::warn!(
                "persist store: {e}: persisted state is not kept across restarts; \
                 settings overlays go to {}",
                overlays.display()
            );
            Storage {
                persist: None,
                settings: Some(strand_core::SettingsStore::new(overlays.join("settings"))),
                config_dir: Some(dir.to_path_buf()),
            }
        }
    }
}

/// What the logic thread's sources collected while it slept.
#[derive(Debug, Default)]
struct Inbox {
    msgs: Vec<ToLogic>,
    /// Every sender is gone (the main thread ended without a word).
    closed: bool,
    /// Loads and settings changes from the compiler worker.
    worker: Vec<FromWorker>,
    /// IPC sockets with something to read.
    ipc: ipc::Ready,
}

impl ipc::HasReady for Inbox {
    fn ready(&mut self) -> &mut ipc::Ready {
        &mut self.ipc
    }
}

/// The logic thread's sleep: its own calloop loop over the main thread's
/// messages, the runtime's wake hook (a ping) and a `CLOCK_REALTIME`
/// timer for wall-clock wakes, with the logic clock's deadline as the
/// dispatch timeout.
struct Sleeper {
    event_loop: EventLoop<'static, Inbox>,
    timer: Rc<WallTimer>,
}

/// `t` as a `Timespec` since the epoch (before it: the epoch, already
/// past).
fn timespec(t: SystemTime) -> Timespec {
    let d = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_nanos(1));
    Timespec {
        tv_sec: d.as_secs() as i64,
        tv_nsec: d.subsec_nanos() as _,
    }
}

impl Sleeper {
    /// The sleeper and the ping that wakes it (the runtime's wake hook).
    #[cfg(test)]
    fn new(rx: Channel<ToLogic>) -> io::Result<(Self, calloop::ping::Ping)> {
        Self::with_worker(rx, None)
    }

    /// [`Sleeper::new`], also waking for the compiler worker's results.
    fn with_worker(
        rx: Channel<ToLogic>,
        worker: Option<Channel<FromWorker>>,
    ) -> io::Result<(Self, calloop::ping::Ping)> {
        let event_loop = EventLoop::<Inbox>::try_new().map_err(io::Error::other)?;
        let handle = event_loop.handle();
        handle
            .insert_source(rx, |event, _, inbox| match event {
                Event::Msg(m) => inbox.msgs.push(m),
                Event::Closed => inbox.closed = true,
            })
            .map_err(|e| io::Error::other(e.error))?;
        if let Some(w) = worker {
            handle
                .insert_source(w, |event, _, inbox| {
                    if let Event::Msg(m) = event {
                        inbox.worker.push(m);
                    }
                })
                .map_err(|e| io::Error::other(e.error))?;
        }
        let (ping, ping_source) = calloop::ping::make_ping().map_err(io::Error::other)?;
        handle
            .insert_source(ping_source, |_, _, _| {})
            .map_err(|e| io::Error::other(e.error))?;
        let timer = Rc::new(WallTimer::new()?);
        handle
            .insert_source(
                Generic::new(Rc::clone(&timer), Interest::READ, Mode::Level),
                |_, timer, _| {
                    // Expired, or the clock was set (`ECANCELED`): either
                    // way the loop steps with the current wall time.
                    timer.read()?;
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| io::Error::other(e.error))?;
        Ok((Self { event_loop, timer }, ping))
    }

    /// Sleep until a message, a ping, `timeout` (logic clock) or the wall
    /// time `wall` (or a clock step), whichever comes first. `stepped_at`
    /// is the wall time the last step used: a clock set back before the
    /// timer was armed is not reported by `CANCEL_ON_SET`, so it is
    /// checked here.
    fn sleep(
        &mut self,
        timeout: Option<Duration>,
        wall: Option<SystemTime>,
        stepped_at: SystemTime,
        inbox: &mut Inbox,
    ) -> io::Result<()> {
        let mut timeout = timeout;
        match wall {
            Some(at) => {
                self.timer.arm_at(timespec(at))?;
                if SystemTime::now() < stepped_at {
                    timeout = Some(Duration::ZERO);
                }
            }
            // Disarmed.
            None => self.timer.arm_at(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            })?,
        }
        self.event_loop
            .dispatch(timeout, inbox)
            .map_err(io::Error::other)
    }
}

/// What the logic thread is given besides the main thread's channel: the
/// compiler worker's results and job queue, and the IPC socket to serve.
#[derive(Debug, Default)]
pub struct Live {
    pub worker: Option<Channel<FromWorker>>,
    pub jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// Where to serve `strand reload` and `strand watch`.
    pub socket: Option<PathBuf>,
    /// The buses the real services use (`strand run`: the environment's;
    /// tests: a private one). `None`: no bus at all, so services that
    /// need one keep their seeded values (`system` its last values).
    /// Unused under `STRAND_MOCK`, whose mock host serves everything.
    pub buses: Option<strand_services::Buses>,
}

/// What a load attempt found wrong (held and unreadable files, its
/// diagnostics with their sources).
#[derive(Debug, Default)]
struct Problems {
    held: Vec<PathBuf>,
    unreadable: Vec<(PathBuf, String)>,
    diagnostics: Vec<strand_compiler::diagnostic::Diagnostic>,
    sources: std::sync::Arc<strand_compiler::source::SourceMap>,
}

impl Problems {
    fn of(o: &Outcome) -> Self {
        Problems {
            held: o.held.clone(),
            unreadable: o.unreadable.clone(),
            diagnostics: o.diagnostics.clone(),
            sources: o.sources.clone(),
        }
    }

    /// Put these on `o` in place of its own.
    fn onto(&self, o: &mut Outcome) {
        o.held = self.held.clone();
        o.unreadable = self.unreadable.clone();
        o.diagnostics = self.diagnostics.clone();
        o.sources = self.sources.clone();
    }
}

/// The logic thread's state between steps (see [`logic`]).
struct Shell {
    inst: Instance,
    /// The schema host: the mock under `STRAND_MOCK`, else the fallback
    /// of the composite host (the names no service serves yet: `screens`,
    /// the clock).
    host: Rc<SchemaHost>,
    /// The real services (not under `STRAND_MOCK`).
    real: Option<crate::services::Real>,
    build: Build,
    overlay: Overlay,
    server: Option<ipc::Server>,
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// A build that changes a lock while one is shown: committed after
    /// the unlock.
    deferred: Option<Box<Loaded>>,
    /// A hard reload asked for while a lock was shown, owed after the
    /// unlock (its load went stale under a newer commit).
    deferred_hard: bool,
    /// Reload events waiting for the step that draws them (their total
    /// time ends when its diff is sent), with the clients whose
    /// `strand reload` each answers.
    events: Vec<(Json, Instant, Vec<ipc::ClientId>)>,
    /// The newest load attempt's problems: a deferred load replayed after
    /// the unlock reports these, not its own older ones.
    latest: Problems,
    /// The referenced files last given to the watcher.
    watched: Vec<(PathBuf, Role)>,
    /// Cells kept over a changed default outside a reload while nobody
    /// watched (at boot): the next reload event lists them.
    unheard: Vec<strand_compiler::reconcile::KeptCell>,
    /// Settings files (and their runtime overlays) read again since the
    /// last step, as notices name them.
    settings_reread: Vec<String>,
    /// The last layout fact batch taken in since the last diff went out.
    layout_seen: Option<u64>,
}

impl Shell {
    /// Apply one message from the main thread.
    fn handle(&mut self, msg: ToLogic) {
        let inst = &self.inst;
        match msg {
            ToLogic::Screens(list) => set_screens(inst.runtime(), &self.host, &list),
            ToLogic::Forget(id) => {
                inst.forget_screen(&id);
            }
            ToLogic::Event { node, event } => {
                if event == NodeEvent::Click
                    && let Some(c) = self.overlay.click(node, inst)
                {
                    match c {
                        Click::Open(file, line, col) => overlay::open_editor(&file, line, col),
                        Click::Reset(path) => {
                            if let Err(e) = inst.reset(&path) {
                                log::warn!("[reset] {path}: {e}");
                            }
                        }
                        Click::Clear(file, field) => {
                            inst.clear_settings_overlay(&file, &field);
                        }
                        Click::Dismissed | Click::Nothing => {}
                    }
                    return;
                }
                inst.event(node, event.name(), event.args_with(&self.host));
            }
            ToLogic::Layout { seq, sizes } => {
                for (node, w, h) in sizes {
                    inst.set_size(node, w, h);
                }
                self.layout_seen = Some(seq);
            }
            ToLogic::Write { node, prop, value } => {
                if let Err(e) = inst.write(node, prop, value) {
                    log::debug!("write to {prop}: {e}");
                }
            }
            ToLogic::Flag { node, flag, on } => inst.set_flag(node, flag, on),
            ToLogic::Size {
                node,
                width,
                height,
            } => inst.set_size(node, width, height),
            ToLogic::Shutdown => {}
        }
    }

    /// What a service needs the user to act on (another notification
    /// server owns the name): overlay rows and `strand watch` notices, as
    /// lowering's notices are (each is logged where it is made).
    fn service_diagnostics(&mut self, diagnostics: Vec<strand_services::ServiceDiagnostic>) {
        let texts: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        let rows = texts.iter().map(|t| overlay::notice_line(t)).collect();
        self.overlay.note(rows, Instant::now(), &self.inst);
        if let Some(s) = &mut self.server {
            s.broadcast(&json!({
                "event": "notices",
                "kept_over_default": [],
                "notices": texts,
            }));
        }
    }

    /// A result from the compiler worker.
    fn worker(&mut self, msg: FromWorker) {
        match msg {
            FromWorker::Settings(changes) => {
                for c in changes {
                    let p = c.path;
                    if self.inst.reload_settings_with(&p, c.read) {
                        // The next step's notices say what is still wrong
                        // in them; the rest of their rows go.
                        self.settings_reread.push(p.to_string_lossy().into_owned());
                        for o in self.inst.settings_overlay_paths(&p) {
                            self.settings_reread.push(o.to_string_lossy().into_owned());
                        }
                    }
                }
            }
            FromWorker::Theme(paths) => {
                self.inst.theme_files_changed(&paths);
            }
            FromWorker::Loaded(l) => self.commit(l),
        }
    }

    /// Commit a load: the new build into the running instance (a hard
    /// reload recreates everything), its diagnostics and reload notices
    /// to the overlay, its event queued for the watchers.
    ///
    /// While a lock is shown, a build that changes a lock (or a hard
    /// reload) is not committed (decisions.md, wave2-runtime): it waits
    /// in [`Shell::deferred`], a newer deferred load absorbing it, and is
    /// committed after the unlock; its event goes out at once (classes
    /// `lock-deferred`, `"deferred": true`), answering `strand reload`.
    /// A build committed meanwhile makes the deferred one stale: it is
    /// dropped (a deferred hard reload is still owed).
    fn commit(&mut self, l: Box<Loaded>) {
        self.latest = Problems::of(&l.outcome);
        self.apply(l, true);
    }

    /// [`Shell::commit`]; `overlay`: the load's diagnostics replace the
    /// overlay's (not for a hard reload replayed after an unlock, whose
    /// load carries none).
    fn apply(&mut self, mut l: Box<Loaded>, overlay: bool) {
        let began = Instant::now();
        let build = l.outcome.build.clone();
        let report = match (&build, l.hard) {
            (Some(b), false) => Some(self.inst.reload(b)),
            (b, true) => {
                let b = b.clone().unwrap_or_else(|| self.build.clone());
                Some(self.inst.reload_hard(&b))
            }
            (None, false) => None,
        };
        let deferred = report
            .as_ref()
            .is_some_and(|r| r.classes == [EditClass::LockDeferred]);
        if deferred {
            // Said in the event, the log and the overlay: an edit synced
            // in while locked (over ssh, a home-manager switch) does not
            // land until the unlock, also when it does not touch the
            // lock itself (decisions.md, wave2-runtime).
            let what = if l.files.is_empty() {
                "the reload".to_string()
            } else {
                l.files
                    .iter()
                    .map(|f| f.file_name().unwrap_or(f.as_os_str()).to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let why = if self.deferred.is_some() {
                "a lock edit is waiting"
            } else {
                "the lock changed while it is shown"
            };
            let notice = format!("{what}: waits for the unlock ({why})");
            log::info!("{notice}");
            self.overlay.note(
                vec![overlay::Line {
                    cell: Some(overlay::WAITS_FOR_UNLOCK.to_string()),
                    ..overlay::notice_line(&notice)
                }],
                Instant::now(),
                &self.inst,
            );
            l.notices.push(notice);
            if let Some(old) = self.deferred.take() {
                absorb(&mut l, &old);
            }
        } else if report.is_some() {
            self.overlay
                .forget_cell(overlay::WAITS_FOR_UNLOCK, &self.inst);
            // Committed: an older deferred build is stale (this one is
            // newer and has everything it had, the lock edit aside,
            // which this one either reverted or kept).
            if let Some(old) = self.deferred.take()
                && old.hard
                && !l.hard
            {
                self.deferred_hard = true;
            }
            if l.hard {
                self.deferred_hard = false;
            }
            if let Some(b) = build.or_else(|| l.hard.then(|| self.build.clone())) {
                self.build = b;
                self.overlay.set_running(true);
            }
        }
        let commit = began.elapsed();
        if overlay {
            // The overlay: every diagnostic of the attempt (none when it
            // all committed).
            let lines = overlay::lines(
                &l.outcome.diagnostics,
                &l.outcome.sources,
                &l.outcome.unreadable,
            );
            let errors = l.outcome.errors();
            self.overlay.set(
                if errors > 0 || !l.outcome.unreadable.is_empty() {
                    lines
                } else {
                    Vec::new()
                },
                Instant::now(),
                &self.inst,
            );
        }
        if let Some(r) = report.as_ref().filter(|_| !deferred) {
            for n in &r.notices {
                log::info!("{n}");
            }
            // Kept-over-a-new-default cells (with their `[reset]`),
            // renamed or retyped cells reset, cancelled `await`s:
            // overlay rows, after the same quiet period.
            self.overlay
                .note(overlay::report_lines(r), Instant::now(), &self.inst);
        }
        if l.outcome.errors() > 0 {
            log::warn!(
                "{}",
                render(&l.outcome.diagnostics, &l.outcome.sources, Style::Plain)
            );
        }
        self.watch_settings();
        let mut ev = reload_event(&l, report.as_ref(), commit);
        ev["deferred"] = json!(deferred);
        if !self.unheard.is_empty()
            && let Some(k) = ev["kept_over_default"].as_array_mut()
        {
            // Kept when nobody was watching (the boot's persisted cells).
            let earlier = kept_json(&std::mem::take(&mut self.unheard));
            if let Json::Array(earlier) = earlier {
                k.splice(0..0, earlier);
            }
        }
        let clients = std::mem::take(&mut l.clients);
        self.events
            .push((ev, l.saved.unwrap_or(l.started), clients));
        if deferred {
            self.deferred = Some(l);
        }
    }

    /// After the unlock: the deferred load (or a hard reload still
    /// owed), committed now.
    fn unlocked(&mut self) {
        if self.inst.lock_shown() {
            return;
        }
        if let Some(mut l) = self.deferred.take() {
            l.hard |= std::mem::take(&mut self.deferred_hard);
            // Saves after it may have broken the config again (held back,
            // the overlay up): the replay shows the newest attempt's
            // problems, not the ones it had when it was deferred.
            self.latest.onto(&mut l.outcome);
            self.apply(l, true);
        } else if std::mem::take(&mut self.deferred_hard) {
            let now = Instant::now();
            let mut l = Loaded {
                outcome: Outcome::default(),
                requested: true,
                clients: Vec::new(),
                hard: true,
                files: Vec::new(),
                saved: None,
                started: now,
                notices: Vec::new(),
            };
            // Its event reports the newest attempt's problems (the
            // overlay already lists them).
            self.latest.onto(&mut l.outcome);
            self.apply(Box::new(l), false);
        }
    }

    /// Give the worker the files the program now reads: its settings
    /// files, and the wallpapers and palette files its theme read (a
    /// wallpaper's link target is watched too).
    fn watch_settings(&mut self) {
        let (images, imports) = self.inst.theme_files();
        let files: Vec<(PathBuf, Role)> = self
            .inst
            .settings_files()
            .into_iter()
            .map(|f| (f, Role::Settings))
            .chain(images.into_iter().map(|f| (f, Role::Wallpaper)))
            .chain(imports.into_iter().map(|f| (f, Role::Other)))
            .collect();
        if files != self.watched {
            self.watched = files.clone();
            if let Some(j) = &self.jobs {
                let _ = j.send(Job::Referenced {
                    files,
                    settings: self.inst.settings_sources(),
                });
            }
        }
    }

    /// A request from an IPC client.
    fn request(&mut self, id: ipc::ClientId, req: ipc::Request) {
        match req {
            ipc::Request::Reload { hard } => match &self.jobs {
                Some(j)
                    if j.send(Job::Reload {
                        hard,
                        client: Some(id),
                    })
                    .is_ok() => {}
                _ => {
                    if let Some(s) = &mut self.server {
                        s.answer(
                            id,
                            &json!({"ok": false, "error": "this shell does not reload"}),
                        );
                    }
                }
            },
            ipc::Request::Reset { path } => {
                let ans = match self.inst.reset(&path) {
                    Ok(()) => {
                        self.overlay.forget_cell(&path, &self.inst);
                        json!({"ok": true})
                    }
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                };
                if let Some(s) = &mut self.server {
                    s.answer(id, &ans);
                }
            }
            ipc::Request::Set { path, value } => {
                // A service's `rw` field (`brightness.level +5%`) goes
                // through the service host; everything else is state or
                // settings.
                let rt = self.inst.runtime();
                let exported = self.inst.get(&path).is_ok();
                let r = if !exported && crate::services::is_service_path(&path) {
                    match &self.real {
                        Some(real) => real.set_text(rt, &path, &value),
                        None => crate::services::set_text(
                            &*self.host,
                            rt,
                            &crate::services::schema().types,
                            &path,
                            &value,
                        ),
                    }
                } else {
                    self.inst.set_text(&path, &value).map_err(|e| e.to_string())
                };
                let ans = match r {
                    Ok(()) => json!({"ok": true}),
                    Err(e) => json!({"ok": false, "error": e}),
                };
                if let Some(s) = &mut self.server {
                    s.answer(id, &ans);
                }
            }
            ipc::Request::Mock(req) => {
                let ans = if crate::mock::requested().is_none() {
                    json!({"ok": false, "error": "`mock` needs a shell run with STRAND_MOCK"})
                } else {
                    match crate::mock::command(self.inst.runtime(), &self.host, &req) {
                        Ok(()) => json!({"ok": true}),
                        Err(e) => json!({"ok": false, "error": e}),
                    }
                };
                if let Some(s) = &mut self.server {
                    s.answer(id, &ans);
                }
            }
            // Answered by the server itself.
            ipc::Request::Watch => {}
        }
    }

    /// After a step: send the reload events it drew (and answer the
    /// clients waiting for a reload), report runtime faults.
    fn after_step(&mut self, update: &strand_compiler::instantiate::Update) {
        for e in &update.errors {
            log::error!("{e}");
            // A runtime fault freezes its component, outlined red.
            let frozen = self.inst.freeze(e);
            if let Some(s) = &mut self.server {
                let at = match (e.file, e.span) {
                    (Some(f), Some(sp)) => self.build.sources.get(f).map(|file| {
                        let (l, c) = overlay::line_col(&file.text, sp.start);
                        format!("{}:{l}:{c}", file.name)
                    }),
                    _ => None,
                };
                s.broadcast(&json!({
                    "event": "fault",
                    "message": e.to_string(),
                    "at": at,
                    "frozen": frozen,
                }));
            }
        }
        let mut settings = Vec::new();
        for d in &update.diagnostics {
            match d {
                strand_core::Diagnostic::Settings(n) => {
                    log::warn!("{n}");
                    settings.push(n);
                }
                d => log::warn!("{d:?}"),
            }
        }
        let reread = std::mem::take(&mut self.settings_reread);
        if !settings.is_empty() || !reread.is_empty() {
            // Settings files: a bad value kept, a syntax error, a
            // read-only file going to an overlay, a file change shadowed
            // by the runtime overlay (with its `[clear]`). A file read
            // again without its old problem loses its row.
            let rows: Vec<_> = settings.iter().map(|n| overlay::settings_line(n)).collect();
            self.overlay
                .settings_read(&reread, rows, Instant::now(), &self.inst);
        }
        if !settings.is_empty()
            && let Some(s) = &mut self.server
        {
            let texts: Vec<String> = settings.iter().map(|n| n.to_string()).collect();
            s.broadcast(&json!({
                "event": "notices",
                "kept_over_default": [],
                "notices": texts,
            }));
        }
        for n in &update.notices {
            log::info!("{n}");
        }
        if !update.notices.is_empty() || !update.kept.is_empty() {
            // Persisted cells kept over a changed default at boot (their
            // `[reset]` from the structured record), lowering's warnings.
            let rows = overlay::kept_and_notices(&update.kept, &update.notices);
            self.overlay.note(rows, Instant::now(), &self.inst);
            // `strand watch` hears them as they happen; with nobody
            // watching (at boot), the next reload event carries them.
            match &mut self.server {
                Some(s) if s.watchers() > 0 => s.broadcast(&json!({
                    "event": "notices",
                    "kept_over_default": kept_json(&update.kept),
                    "notices": update.notices,
                })),
                _ => self.unheard.extend(update.kept.iter().cloned()),
            }
        }
        let now = Instant::now();
        for (mut ev, since, clients) in self.events.drain(..) {
            ev["timing"]["total_ms"] = json!(ms(now.saturating_duration_since(since)));
            log::info!("{}", ipc::describe(&ev).trim_end());
            if let Some(s) = &mut self.server {
                s.broadcast(&ev);
                // Each `strand reload` gets the event of its own load.
                for id in clients {
                    s.answer(id, &json!({"ok": true, "event": ev.clone()}));
                }
            }
        }
    }
}

/// A newer deferred load `l` takes in an older one it replaces: the
/// files it named and committed, and whether it was asked for (hard).
fn absorb(l: &mut Loaded, old: &Loaded) {
    for f in &old.files {
        if !l.files.contains(f) {
            l.files.push(f.clone());
        }
    }
    for f in &old.outcome.committed {
        if !l.outcome.committed.contains(f) {
            l.outcome.committed.push(f.clone());
        }
    }
    // A deferred hard reload of the same sources has no build of its
    // own: it applies the deferred one.
    if l.outcome.build.is_none() {
        l.outcome.build = old.outcome.build.clone();
    }
    l.requested |= old.requested;
    l.hard |= old.hard;
    l.saved = match (l.saved, old.saved) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    for n in &old.notices {
        if !l.notices.contains(n) {
            l.notices.push(n.clone());
        }
    }
}

/// Cells kept over a changed default, as `strand watch` lists them.
fn kept_json(kept: &[strand_compiler::reconcile::KeptCell]) -> Json {
    kept.iter()
        .map(|k| json!({"path": k.path, "shown": k.shown}))
        .collect()
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e5).round() / 100.0
}

/// A load as `strand watch` streams it (`timing.total_ms` is filled in
/// when the step that draws it has sent its diff).
fn reload_event(l: &Loaded, report: Option<&Report>, commit: Duration) -> Json {
    let paths =
        |v: &[PathBuf]| -> Vec<String> { v.iter().map(|p| p.display().to_string()).collect() };
    let diagnostics: Vec<Json> = {
        l.outcome
            .diagnostics
            .iter()
            .map(|d| {
                // One rendering per diagnostic: a multi-line one cannot
                // shift the others.
                let short = render_short(std::slice::from_ref(d), &l.outcome.sources);
                let line = short.trim_end();
                let at = d.primary().and_then(|lab| {
                    l.outcome.sources.get(lab.file).map(|f| {
                        let (ln, col) = overlay::line_col(&f.text, lab.span.start);
                        json!({"file": f.name, "line": ln, "column": col})
                    })
                });
                json!({
                    "severity": if d.is_error() { "error" } else { "warning" },
                    "code": d.code,
                    "message": d.message,
                    "help": d.help,
                    "at": at,
                    "labels": d.labels.iter().map(|lab| {
                        let at = l.outcome.sources.get(lab.file).map(|f| {
                            let (ln, col) = overlay::line_col(&f.text, lab.span.start);
                            json!({"file": f.name, "line": ln, "column": col})
                        });
                        json!({"message": lab.message, "primary": lab.primary, "at": at})
                    }).collect::<Vec<_>>(),
                    "short": line,
                })
            })
            .collect()
    };
    let r = report.cloned().unwrap_or_default();
    json!({
        "event": "reload",
        "requested": l.requested,
        "hard": l.hard,
        "files": paths(&l.files),
        "committed": paths(&l.outcome.committed),
        "held": paths(&l.outcome.held),
        "unreadable": l.outcome.unreadable.iter().map(|(p, e)| json!({"file": p.display().to_string(), "error": e})).collect::<Vec<_>>(),
        "from_cache": l.outcome.from_cache,
        "classes": r.classes.iter().map(|c| c.name()).collect::<Vec<_>>(),
        "kept": r.kept,
        "kept_over_default": kept_json(&r.kept_over_default),
        "reset": r.reset.iter().map(|(c, w)| json!({"cell": c, "why": w})).collect::<Vec<_>>(),
        "ambiguous": r.ambiguous,
        "notices": r.notices.iter().chain(&l.notices).collect::<Vec<_>>(),
        "restarted": r.restarted,
        "cancelled": r.cancelled,
        "timing": {
            "watch_ms": l.saved.map(|s| ms(l.started.saturating_duration_since(s))),
            "compile_ms": ms(l.outcome.compile_time),
            "commit_ms": ms(commit),
            "total_ms": Json::Null,
        },
        "diagnostics": diagnostics,
    })
}

/// The logic thread: mount `boot` (the loader's first outcome: its build,
/// or nothing and its diagnostics on the overlay), then step until
/// [`ToLogic::Shutdown`] (or until the main thread is gone), committing
/// what the compiler worker sends and serving the IPC socket; then
/// unmount and drop the stores so pending writes reach the disk.
pub fn logic(
    boot: Outcome,
    storage: Storage,
    rx: Channel<ToLogic>,
    out: Sender<SceneDiff>,
    live: Live,
) -> Result<(), String> {
    let (mut sleeper, ping) =
        Sleeper::with_worker(rx, live.worker).map_err(|e| format!("logic loop: {e}"))?;
    let handle = sleeper.event_loop.handle();
    let server = live
        .socket
        .as_deref()
        .and_then(|p| match ipc::Server::bind(p, &handle) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!(
                    "no IPC socket (strand reload, strand watch): {}: {e}",
                    p.display()
                );
                None
            }
        });
    let rt = Runtime::new();
    let services_ping = ping.clone();
    rt.set_wake_hook(move || ping.ping());
    // With nothing to run yet (broken at boot, no last good version),
    // the host still serves the builtin services (`screens`, the clock):
    // the fixed config later mounts against this host.
    let host_types = match &boot.build {
        Some(b) => b.program.types.clone(),
        None => crate::services::schema().types.clone(),
    };
    let build = boot.build.clone().unwrap_or_else(Build::empty);
    let mock = crate::mock::requested();
    // The acceptance mock's clock stands still (UTC), so its screenshots
    // are the same every run.
    let frozen = crate::mock::frozen_time();
    let host = Rc::new(match frozen {
        Some(at) => {
            let utc = chrono::FixedOffset::east_opt(0).map_or(Zone::Local, Zone::Fixed);
            let clock = Clock::new(&rt, &host_types, utc, at);
            SchemaHost::new(&rt, &host_types, Some(clock))
        }
        None => SchemaHost::real(&rt, &host_types),
    });
    if let Some(m) = &mock {
        crate::mock::desktop(&rt, &host, m);
    }
    // The real services (none under the mock): registered now, each
    // started by its first reader.
    let real = mock.is_none().then(|| {
        let buses = live
            .buses
            .clone()
            .unwrap_or_else(strand_services::Buses::none);
        crate::services::Real::start(&rt, &host_types, buses, host.clone(), move || {
            services_ping.ping()
        })
    });
    // Monitors the main thread already knows about.
    let mut inbox = Inbox::default();
    sleeper
        .event_loop
        .dispatch(Some(Duration::ZERO), &mut inbox)
        .map_err(|e| format!("logic loop: {e}"))?;
    let mut stop = inbox.closed;
    for msg in inbox.msgs.drain(..) {
        match msg {
            ToLogic::Screens(list) => set_screens(&rt, &host, &list),
            ToLogic::Shutdown => stop = true,
            _ => {}
        }
    }
    // `system`'s last values before the first frame (the portal's boot
    // read may take up to 500 ms), kept off the logic thread whenever
    // the service reports new ones. The runtime itself reads
    // `system.reduced_motion` (render snaps every spring while it is
    // on), so it holds one reader of `system` for the whole run.
    let system_file = system::Last::file(storage.palette_dir());
    let saved = (
        system_file.clone(),
        strand_theme::FileWriter::new()
            .inspect_err(|e| log::warn!("not keeping the portal's settings: {e}"))
            .ok(),
    );
    let mut last = system_file
        .as_deref()
        .map(system::Last::load)
        .unwrap_or_default();
    if let Some(r) = &real {
        if let Err(e) = r.builtin.system.seed(&rt, |s| last.seed(s)) {
            log::warn!("system: {e}");
        }
        r.builtin.system.acquire(&rt);
    }
    let inst = Instance::from_build(&rt, &build, live_host(&host, &real), storage);
    // The first frame waits (at most BOOT_SERVICES_HOLD) for the first
    // reads of the services the config started, so a desktop whose
    // scheme or accent changed while Strand was not running does not show
    // the kept values first and then switch. A slower service keeps its
    // seeded or default values until its read arrives.
    if let Some(r) = &real {
        r.services.wait_ready(&rt, BOOT_SERVICES_HOLD);
        keep_system(&rt, r, &mut last, &saved);
    }
    let mut shell = Shell {
        inst,
        host,
        real,
        build,
        overlay: Overlay::default(),
        server,
        jobs: live.jobs,
        deferred: None,
        deferred_hard: false,
        events: Vec::new(),
        latest: Problems::of(&boot),
        watched: Vec::new(),
        unheard: Vec::new(),
        settings_reread: Vec::new(),
        layout_seen: None,
    };
    shell.overlay.set_running(boot.build.is_some());
    // The boot's diagnostics: a config broken at boot runs its last good
    // version (or nothing) with the overlay up.
    if boot.errors() > 0 || !boot.unreadable.is_empty() {
        log::warn!("{}", render(&boot.diagnostics, &boot.sources, Style::Plain));
        let lines = overlay::lines(&boot.diagnostics, &boot.sources, &boot.unreadable);
        shell.overlay.set(lines, Instant::now(), &shell.inst);
    }
    if boot.from_cache {
        log::warn!("the config does not compile: running its last good version");
    }
    shell.watch_settings();
    let start = Instant::now();
    // The reduced-motion preference render last heard of.
    let mut reduced_sent = false;
    while !stop {
        if shell.deferred.is_some() || shell.deferred_hard {
            shell.unlocked();
        }
        let wall = frozen.unwrap_or_else(SystemTime::now);
        let (mut update, wake) = shell.inst.step(start.elapsed(), wall);
        let mut diff = std::mem::take(&mut update.diff);
        diff.layout_seen = shell.layout_seen.take();
        // `system.reduced_motion` (the portal's, or its last value) goes
        // to render, which snaps every spring while it is on.
        let reduced = {
            let rt = shell.inst.runtime();
            let host = live_host(&shell.host, &shell.real);
            rt.untrack(|rt| host.read(rt, "system", "reduced_motion"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        if reduced != reduced_sent {
            diff.reduced_motion = Some(reduced);
            reduced_sent = reduced;
        }
        if !diff.is_empty() && out.send(diff).is_err() {
            break;
        }
        shell.after_step(&update);
        // Output a client's socket cannot take yet waits for its write
        // source to wake the loop (no polling).
        if let Some(s) = shell.server.as_mut() {
            s.flush(&handle);
        }
        let now = Instant::now();
        let mut timeout = wake.deadline.map(|d| d.saturating_sub(start.elapsed()));
        let mut also = |t: Option<Duration>| {
            if let Some(t) = t {
                timeout = Some(timeout.map_or(t, |x| x.min(t)));
            }
        };
        also(
            shell
                .overlay
                .deadline()
                .map(|d| d.saturating_duration_since(now)),
        );
        // A step that closed the lock: the deferred load commits now
        // (no polling while it stays shown: only a step can unlock).
        if (shell.deferred.is_some() || shell.deferred_hard) && !shell.inst.lock_shown() {
            also(Some(Duration::ZERO));
        }
        sleeper
            .sleep(
                timeout,
                wake.wall.filter(|_| frozen.is_none()),
                wall,
                &mut inbox,
            )
            .map_err(|e| format!("logic loop: {e}"))?;
        for m in inbox.msgs.drain(..) {
            if m == ToLogic::Shutdown {
                stop = true;
                break;
            }
            shell.handle(m);
        }
        stop |= inbox.closed;
        for w in inbox.worker.drain(..) {
            shell.worker(w);
        }
        // What the services sent while the loop slept, applied outside
        // handlers (reports are not handler writes) before the next step.
        if let Some(r) = &shell.real
            && r.services.pump(shell.inst.runtime())
        {
            keep_system(shell.inst.runtime(), r, &mut last, &saved);
        }
        let failed = shell
            .real
            .as_ref()
            .map(|r| r.services.take_diagnostics())
            .unwrap_or_default();
        if !failed.is_empty() {
            shell.service_diagnostics(failed);
        }
        if shell.inst.take_theme_files_changed() {
            shell.watch_settings();
        }
        let ready = std::mem::take(&mut inbox.ipc);
        if let Some(s) = &mut shell.server {
            if ready.accept {
                s.accept(&handle);
            }
            let mut reqs = Vec::new();
            for id in ready.readable {
                for r in s.read(id) {
                    reqs.push((id, r));
                }
            }
            for (id, r) in reqs {
                shell.request(id, r);
            }
        }
        shell.overlay.tick(Instant::now(), &shell.inst);
    }
    // Unmount: debounced `persist` writes and settings write-outs are
    // flushed as their cells go; the runtime's shutdown waits for the
    // persist queue (bounded), and the last store handle joins its IO
    // thread.
    let Shell {
        mut inst,
        host,
        real,
        ..
    } = shell;
    inst.shutdown();
    drop(inst);
    if let Some(r) = &real {
        r.shutdown(&rt);
    }
    rt.shutdown();
    drop(real);
    drop(host);
    Ok(())
}

/// The host the instance runs against: the real services' composite, or
/// the schema host alone (the mock).
fn live_host(
    host: &Rc<SchemaHost>,
    real: &Option<crate::services::Real>,
) -> Rc<dyn strand_compiler::vm::ServiceHost> {
    match real {
        Some(r) => r.host.clone(),
        None => host.clone(),
    }
}

/// `system`'s values, written to the disk (off the logic thread) when
/// they changed.
fn keep_system(
    rt: &Runtime,
    real: &crate::services::Real,
    last: &mut system::Last,
    saved: &(Option<PathBuf>, Option<strand_theme::FileWriter>),
) {
    let Ok(now) = real.builtin.system.cells().snapshot(rt) else {
        return;
    };
    let now = system::Last::of(&now);
    if now != *last {
        *last = now;
        if let (Some(f), Some(w)) = saved {
            w.write(f.clone(), last.to_text());
        }
    }
}

/// SIGINT and SIGTERM as a file descriptor: blocked in the calling thread
/// (and so in every thread it starts afterwards) and read from a
/// `signalfd`, so the main loop ends the run in order instead of the
/// process dying with writes in flight.
fn signal_fd() -> io::Result<OwnedFd> {
    // SAFETY: `sigset_t` is plain data; `sigemptyset` initialises it.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a valid `sigset_t`; the calls only write to it and
    // to this thread's signal mask, and `signalfd` returns a new fd or -1.
    let fd = unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new descriptor owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The signal numbers waiting on a signalfd.
fn read_signals(fd: BorrowedFd<'_>) -> Vec<i32> {
    let mut out = Vec::new();
    loop {
        let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        match rustix::io::read(fd, &mut info) {
            Ok(n) if n == info.len() => {
                // `ssi_signo` is the first field (a u32).
                out.push(u32::from_ne_bytes([info[0], info[1], info[2], info[3]]) as i32);
            }
            _ => return out,
        }
    }
}

/// Why the main loop ended.
enum End {
    /// The compositor went away, or a signal asked us to stop.
    Done,
    /// The logic thread hung up first.
    LogicEnded,
    Failed(DemoError),
}

/// Run the config in `dir` until the compositor goes away or SIGINT or
/// SIGTERM arrives (then `Ok`, after the logic thread has flushed and
/// ended), or until the logic thread fails.
pub fn run(dir: &Path, log: &LogConfig) -> Result<(), DemoError> {
    if !dir.is_dir() {
        return Err(DemoError::Logic(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    // Before any thread starts, so every thread has the signals blocked.
    let signals = signal_fd()?;
    // The watcher, then the first load (a config broken at boot runs its
    // last good version), then the compiler worker for every later save.
    let (worker_tx, worker_rx) = calloop::channel::channel::<FromWorker>();
    let (compiler, boot) = Worker::spawn(dir, live::cache_dir(), worker_tx)?;
    if boot.build.is_none() && boot.errors() == 0 {
        log::warn!("{}: nothing to run yet", dir.display());
    }
    let live = Live {
        worker: Some(worker_rx),
        jobs: Some(compiler.jobs()),
        socket: ipc::socket_path(),
        buses: Some(strand_services::Buses::default()),
    };
    let (ping, ping_source) = calloop::ping::make_ping()?;
    let wake = ping.clone();
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    renderer.set_first_frame_wait(FIRST_FRAME_TEXT_WAIT);
    let (to_logic, from_main) = calloop::channel::channel::<ToLogic>();
    let host = Host::new(renderer, log.damage)
        .forwarding(to_logic.clone())
        .waking(wake);
    let mut mgr = SurfaceManager::connect(host, Config::default())?;
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| crate::demo::text_ready(state))
        .map_err(|e| DemoError::Io(io::Error::other(e.error)))?;
    let signalled = Rc::new(Cell::new(false));
    let flag = Rc::clone(&signalled);
    handle
        .insert_source(
            Generic::new(signals, Interest::READ, Mode::Level),
            move |_, fd, _| {
                for sig in read_signals(fd.as_fd()) {
                    log::info!("signal {sig}: shutting down");
                    flag.set(true);
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| DemoError::Io(io::Error::other(e.error)))?;
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    let storage = storage(dir);
    compiler.register_own_writes(&storage);
    let logic = std::thread::Builder::new()
        .name("strand-logic".into())
        .spawn(move || logic(boot, storage, from_main, tx, live))?;
    let hung_up = Rc::new(Cell::new(false));
    let flag = Rc::clone(&hung_up);
    handle
        .insert_source(rx, move |event, _, state| match event {
            Event::Msg(diff) => apply(state, diff),
            Event::Closed => flag.set(true),
        })
        .map_err(|e| DemoError::Io(io::Error::other(e.error)))?;
    let end = loop {
        if hung_up.get() {
            break End::LogicEnded;
        }
        if signalled.get() {
            break End::Done;
        }
        match mgr.dispatch(None) {
            Ok(()) => {}
            Err(e) if connection_closed(&e) => {
                log::info!("the compositor went away: {e}");
                break End::Done;
            }
            Err(e) => break End::Failed(e.into()),
        }
    };
    let _ = to_logic.send(ToLogic::Shutdown);
    let joined = logic.join();
    if compiler.join().is_err() {
        log::error!("the compiler worker or the file watcher panicked");
    }
    match (end, joined) {
        (_, Err(_)) => Err(DemoError::Logic("panicked".into())),
        (End::Failed(e), _) => Err(e),
        (_, Ok(Err(e))) => Err(DemoError::Logic(e)),
        (End::LogicEnded, Ok(Ok(()))) => Err(DemoError::Logic("ended".into())),
        (End::Done, Ok(Ok(()))) => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use strand_compiler::instantiate::SceneMirror;
    use strand_compiler::reconcile::loader::Loader;
    use strand_scene::{Prop, PropValue};

    pub(crate) fn screen(id: &str, name: &str) -> ScreenInfo {
        ScreenInfo {
            id: id.into(),
            name: name.into(),
            make: "Make".into(),
            model: id.into(),
            description: String::new(),
            scale: 1.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    /// The config in `dir` loaded once (no watcher, no cache).
    fn load(dir: &Path) -> Outcome {
        let out = Loader::new(dir, crate::services::schema().clone(), None).boot();
        assert_eq!(out.errors(), 0, "{:?}", out.diagnostics);
        out
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("strand-run-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A calloop channel is read through a loop: one on a thread of its
    /// own hands the diffs to a plain receiver.
    pub(crate) fn inbox(rx: Channel<SceneDiff>) -> std::sync::mpsc::Receiver<SceneDiff> {
        let (tx, inbox) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut el = EventLoop::<bool>::try_new().unwrap();
            el.handle()
                .insert_source(rx, move |event, _, closed| match event {
                    Event::Msg(d) => {
                        let _ = tx.send(d);
                    }
                    Event::Closed => *closed = true,
                })
                .unwrap();
            let mut closed = false;
            while !closed {
                el.dispatch(None, &mut closed).unwrap();
            }
        });
        inbox
    }

    /// The scene as the main thread sees it, fed from the diff channel.
    struct Mirror {
        inbox: std::sync::mpsc::Receiver<SceneDiff>,
        scene: SceneMirror,
        /// Every diff must leave the bar up and the error overlay shut
        /// (saves that are valid once complete never flash it).
        steady: bool,
        /// The last `reduced_motion` a diff carried.
        reduced: Option<bool>,
    }

    impl Mirror {
        fn new(rx: Channel<SceneDiff>) -> Self {
            Self {
                inbox: inbox(rx),
                scene: SceneMirror::new(),
                steady: false,
                reduced: None,
            }
        }

        /// Apply one diff, checking what every diff must keep.
        fn apply(&mut self, what: &str, diff: &SceneDiff) {
            let had = !self.scene.roots().is_empty();
            if diff.reduced_motion.is_some() {
                self.reduced = diff.reduced_motion;
            }
            self.scene.apply(diff).unwrap();
            // No blank frame: once something shows, a diff never leaves
            // nothing.
            assert!(
                !had || !self.scene.roots().is_empty(),
                "{what}: a blank frame"
            );
            if self.steady {
                assert_eq!(
                    self.scene.of_kind(strand_scene::NodeKind::Bar).len(),
                    1,
                    "{what}: the bar went\n{}",
                    self.scene.render()
                );
                assert!(
                    self.scene.of_kind(strand_scene::NodeKind::Panel).is_empty(),
                    "{what}: the error overlay flashed\n{}",
                    self.scene.render()
                );
            }
        }

        /// Apply whatever diffs come for `d`.
        fn settle(&mut self, what: &str, d: Duration) {
            let deadline = Instant::now() + d;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return;
                }
                if let Ok(diff) = self.inbox.recv_timeout(left) {
                    self.apply(what, &diff);
                }
            }
        }

        /// Apply diffs until `done` holds (10 s at most).
        fn until(&mut self, what: &str, done: impl Fn(&SceneMirror) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(&self.scene) {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.inbox.recv_timeout(left) {
                    Ok(diff) => self.apply(what, &diff),
                    Err(_) => panic!("{what}:\n{}", self.scene.render()),
                }
            }
        }

        /// Until a diff tells render `reduced_motion` is `want`.
        fn until_reduced(&mut self, want: bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.reduced != Some(want) {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.inbox.recv_timeout(left) {
                    Ok(diff) => self.apply("reduced motion", &diff),
                    Err(_) => panic!("reduced_motion never {want}: {:?}", self.reduced),
                }
            }
        }

        fn texts(&self) -> Vec<String> {
            let mut t = self.scene.texts();
            t.sort();
            t
        }
    }

    /// The logic side of `strand run` without Wayland: monitors arrive as
    /// messages and each gets its bar; surface input (hover, pressed,
    /// click, secondary, scroll with its `dy, dx`) and sizes reach it;
    /// an unplugged monitor's bar is parked and comes back with its
    /// state, and once forgotten comes back fresh; `Shutdown` ends the
    /// thread after the persisted state reached the disk.
    #[test]
    fn the_logic_thread_follows_the_monitors() {
        let dir = temp_dir("monitors");
        std::fs::write(
            dir.join("bar.strand"),
            "state total = 0 persist\nbar Top {\n  state n = 0\n  state e = \"-\"\n  opacity: hover ? 0.5 : 1\n  height: self.width > 1000 ? 40 : 32\n  when pressed { opacity: 0.25 }\n  on click {\n    n += 1\n    total += 1\n  }\n  on secondary { e = \"secondary\" }\n  on scroll(dy, dx) { e = join(\",\", dy, dx) }\n  text join(\" \", screen.name, n, total, e)\n}\n",
        )
        .unwrap();
        let program = load(&dir);
        let storage = Storage::in_dirs(dir.join("state"), &dir);
        let persist = storage.persist.clone().unwrap();
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || logic(program, storage, from_main, tx, Live::default()));
        let mut m = Mirror::new(rx);
        m.until("A's bar", |s| s.texts() == ["DP-1 0 0 -"]);
        let a = m.scene.roots()[0];
        let send = |msg| to_logic.send(msg).unwrap();
        // Surface input and size reach the bar's node.
        send(ToLogic::Flag {
            node: a,
            flag: NodeFlag::Hover,
            on: true,
        });
        send(ToLogic::Size {
            node: a,
            width: 1920.0,
            height: 32.0,
        });
        m.until("hover and size", |s| {
            s.prop(a, Prop::Opacity) == Some(&PropValue::Number(0.5))
                && s.prop(a, Prop::Height) == Some(&PropValue::Number(40.0))
        });
        // Layout facts are answered with their batch number, even when
        // they change nothing (render holds a query's frame for it).
        send(ToLogic::Layout {
            seq: 7,
            sizes: vec![(a, 1920.0, 40.0)],
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let diff = m.inbox.recv_timeout(left).expect("an answer to the facts");
            m.apply("facts", &diff);
            if diff.layout_seen == Some(7) {
                break;
            }
        }
        send(ToLogic::Flag {
            node: a,
            flag: NodeFlag::Pressed,
            on: true,
        });
        m.until("pressed", |s| {
            s.prop(a, Prop::Opacity) == Some(&PropValue::Number(0.25))
        });
        send(ToLogic::Flag {
            node: a,
            flag: NodeFlag::Pressed,
            on: false,
        });
        // A right click runs `on secondary` (not `on click`), and a
        // scroll runs `on scroll(dy, dx)` with its deltas in that order.
        send(ToLogic::Event {
            node: a,
            event: NodeEvent::Secondary,
        });
        m.until("a right click", |s| s.texts() == ["DP-1 0 0 secondary"]);
        send(ToLogic::Event {
            node: a,
            event: NodeEvent::Scroll { dy: 1.5, dx: -2.5 },
        });
        m.until("a scroll", |s| s.texts() == ["DP-1 0 0 1.5,-2.5"]);
        send(ToLogic::Event {
            node: a,
            event: NodeEvent::Click,
        });
        m.until("a click", |s| s.texts() == ["DP-1 1 1 1.5,-2.5"]);
        let two = || vec![screen("A", "DP-1"), screen("B", "HDMI-A-1")];
        send(ToLogic::Screens(two()));
        m.until("B's bar", |s| s.roots().len() == 2);
        let b = *m.scene.roots().iter().find(|&&r| r != a).unwrap();
        send(ToLogic::Event {
            node: b,
            event: NodeEvent::Click,
        });
        m.until("a click on B", |s| {
            let mut t = s.texts();
            t.sort();
            t == ["DP-1 1 2 1.5,-2.5", "HDMI-A-1 1 2 -"]
        });
        // B unplugged: its bar is parked; back within 30 s, with its `n`.
        send(ToLogic::Screens(vec![screen("A", "DP-1")]));
        m.until("B parked", |s| s.roots().len() == 1);
        send(ToLogic::Screens(two()));
        m.until("B back", |s| s.roots().len() == 2);
        assert_eq!(m.texts(), ["DP-1 1 2 1.5,-2.5", "HDMI-A-1 1 2 -"]);
        // Unplugged and forgotten: B comes back fresh (the top-level
        // `total` stays).
        send(ToLogic::Screens(vec![screen("A", "DP-1")]));
        m.until("B parked again", |s| s.roots().len() == 1);
        send(ToLogic::Forget("B".into()));
        send(ToLogic::Screens(two()));
        m.until("B fresh", |s| s.roots().len() == 2);
        assert_eq!(m.texts(), ["DP-1 1 2 1.5,-2.5", "HDMI-A-1 0 2 -"]);
        // Shutdown: the thread ends, and the persisted `total`, written
        // less than the 250 ms debounce ago, is on disk.
        send(ToLogic::Shutdown);
        assert_eq!(t.join().unwrap(), Ok(()));
        let stored = std::fs::read(persist.file_of("bar.total").unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&stored).contains('2'), "{stored:?}");
        drop(persist);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Live reload without Wayland: the watcher and the compiler worker
    /// feed the logic thread. A prop edit and an added node keep the
    /// bar's state; a broken save keeps the last good tree and, after
    /// 250 ms, opens the overlay listing the error with its fix; the fix
    /// takes it away; `strand watch` streams each reload and `strand
    /// reload` answers with its event.
    #[test]
    fn saves_reload_live_with_state_kept() {
        let dir = temp_dir("live");
        let bar = |opacity: &str, extra: &str| {
            format!(
                "bar Top {{\n  state n = 0\n  on click {{ n += 1 }}\n  text join(\" \", \"n\", n) {{ opacity: {opacity} }}\n{extra}}}\n"
            )
        };
        let file = dir.join("bar.strand");
        std::fs::write(&file, bar("0.5", "")).unwrap();
        let socket = dir.join("ipc.sock");
        let (wtx, wrx) = calloop::channel::channel();
        let (compiler, boot) = Worker::spawn(&dir, None, wtx).unwrap();
        let live = Live {
            worker: Some(wrx),
            jobs: Some(compiler.jobs()),
            socket: Some(socket.clone()),
            buses: None,
        };
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
        let mut m = Mirror::new(rx);
        m.until("the bar", |s| s.texts() == ["n 0"]);
        let root = m.scene.roots()[0];
        to_logic
            .send(ToLogic::Event {
                node: root,
                event: NodeEvent::Click,
            })
            .unwrap();
        m.until("a click", |s| s.texts() == ["n 1"]);
        // A watcher.
        let mut events =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
        assert_eq!(ok["ok"], true);
        let mut next_event = || {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut events, &mut line).unwrap();
            serde_json::from_str::<Json>(&line).unwrap()
        };
        // A prop edit: patched in place, the state kept.
        let text = m.scene.of_kind(strand_scene::NodeKind::Text)[0];
        std::fs::write(&file, bar("0.75", "")).unwrap();
        m.until("the new opacity", |s| {
            s.prop(text, strand_scene::Prop::Opacity) == Some(&PropValue::Number(0.75))
        });
        assert_eq!(m.texts(), ["n 1"]);
        let ev = next_event();
        assert_eq!(ev["event"], "reload");
        assert_eq!(ev["classes"], json!(["prop"]), "{ev}");
        assert!(ev["timing"]["total_ms"].as_f64().is_some(), "{ev}");
        // A node added: created, the old ones kept.
        std::fs::write(&file, bar("0.75", "  text \"new\"\n")).unwrap();
        m.until("the new node", |s| s.texts().len() == 2);
        assert_eq!(m.texts(), ["n 1", "new"]);
        assert_eq!(next_event()["classes"], json!(["node-added"]));
        // Broken: the bar keeps running; after 250 ms the overlay.
        std::fs::write(&file, bar("0.75", "  txet \"new\"\n")).unwrap();
        let ev = next_event();
        assert!(ev["held"].as_array().is_some_and(|h| h.len() == 1), "{ev}");
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        assert_eq!(ev["diagnostics"][0]["help"], "did you mean `text`?", "{ev}");
        m.until("the overlay", |s| {
            s.of_kind(strand_scene::NodeKind::Panel).len() == 1
        });
        let listed = m.texts().join("\n");
        assert!(listed.contains("bar.strand:5:3: error"), "{listed}");
        assert!(listed.contains("did you mean `text`?"), "{listed}");
        assert!(
            m.texts().contains(&"n 1".to_string()),
            "the last good tree runs"
        );
        // Fixed: committed, the overlay gone, the state still kept.
        std::fs::write(&file, bar("0.75", "  text \"fixed\"\n")).unwrap();
        m.until("the fix", |s| {
            s.of_kind(strand_scene::NodeKind::Panel).is_empty() && s.texts().len() == 2
        });
        assert_eq!(m.texts(), ["fixed", "n 1"]);
        let ev = next_event();
        assert_eq!(ev["diagnostics"], json!([]), "{ev}");
        assert_eq!(ev["kept_over_default"], json!([]), "{ev}");
        assert_eq!(ev["ambiguous"], json!([]), "{ev}");
        // A new default while the cell holds another value: kept, and
        // `strand watch` names the cell (not only in prose).
        std::fs::write(
            &file,
            bar("0.75", "  text \"fixed\"\n").replace("state n = 0", "state n = 7"),
        )
        .unwrap();
        let ev = next_event();
        assert_eq!(ev["classes"], json!(["state-default"]), "{ev}");
        let kept = &ev["kept_over_default"];
        assert_eq!(kept.as_array().map(Vec::len), Some(1), "{ev}");
        assert!(
            kept[0]["path"].as_str().is_some_and(|p| p.ends_with(".n")),
            "{ev}"
        );
        assert_eq!(kept[0]["shown"], "1", "{ev}");
        assert_eq!(m.texts(), ["fixed", "n 1"]);
        // `strand reload`: answered with its event once done.
        let mut client =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ans = ipc::request(
            &mut client,
            &ipc::Request::Reload { hard: false },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["ok"], true, "{ans}");
        assert_eq!(ans["event"]["requested"], true, "{ans}");
        // A client that half-closes after its request (`nc -N`) still
        // gets its answer.
        {
            use std::io::{Read, Write};
            let mut raw = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            raw.write_all(ipc::encode(&ipc::Request::Reload { hard: false }).as_bytes())
                .unwrap();
            raw.shutdown(std::net::Shutdown::Write).unwrap();
            raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut answer = String::new();
            raw.read_to_string(&mut answer).unwrap();
            let ans: Json = serde_json::from_str(answer.trim()).unwrap();
            assert_eq!(ans["ok"], true, "{ans}");
        }
        // `--hard`: state dropped, every surface recreated.
        let ans = ipc::request(
            &mut client,
            &ipc::Request::Reload { hard: true },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["event"]["classes"], json!(["hard"]), "{ans}");
        m.until("a fresh bar", |s| s.texts().contains(&"n 7".to_string()));
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        assert!(!socket.exists(), "the socket is removed at exit");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The live pipeline on `dir` (no cache): the compiler worker, the
    /// logic thread with one monitor, a mirror of its scene.
    fn spawn_live(
        dir: &Path,
        socket: Option<PathBuf>,
    ) -> (
        Worker,
        calloop::channel::Sender<ToLogic>,
        std::thread::JoinHandle<Result<(), String>>,
        Mirror,
    ) {
        spawn_live_with(dir, socket, None, Storage::none())
    }

    /// [`spawn_live`] with the services on `buses`, with storage.
    fn spawn_live_with(
        dir: &Path,
        socket: Option<PathBuf>,
        buses: Option<strand_services::Buses>,
        storage: Storage,
    ) -> (
        Worker,
        calloop::channel::Sender<ToLogic>,
        std::thread::JoinHandle<Result<(), String>>,
        Mirror,
    ) {
        let (wtx, wrx) = calloop::channel::channel();
        let (compiler, boot) = Worker::spawn(dir, None, wtx).unwrap();
        compiler.register_own_writes(&storage);
        let live = Live {
            worker: Some(wrx),
            jobs: Some(compiler.jobs()),
            socket,
            buses,
        };
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || logic(boot, storage, from_main, tx, live));
        (compiler, to_logic, t, Mirror::new(rx))
    }

    /// A first config with a typo and no last good version: nothing runs
    /// but the overlay; the fix mounts the per-monitor bar with `screen`
    /// in scope (the builtin services are there from the start).
    #[test]
    fn a_config_broken_at_first_boot_runs_once_fixed() {
        let dir = temp_dir("first-boot");
        let file = dir.join("bar.strand");
        std::fs::write(&file, "bar Top {\n  txet screen.name\n}\n").unwrap();
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
        m.until("the overlay", |s| {
            s.of_kind(strand_scene::NodeKind::Panel).len() == 1
        });
        std::fs::write(&file, "bar Top {\n  text screen.name\n}\n").unwrap();
        m.until("the bar", |s| {
            s.of_kind(strand_scene::NodeKind::Panel).is_empty() && s.texts() == ["DP-1"]
        });
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// While the lock is shown, saves that change it wait: `strand watch`
    /// and `strand reload` hear `"deferred": true` at once, a second
    /// deferred save absorbs the first, and after the unlock both the
    /// lock edit and the bar edit land.
    #[test]
    fn lock_edits_wait_for_the_unlock_and_then_land() {
        let dir = temp_dir("lock");
        let src = |lock: &str, bar: &str| {
            format!(
                "export state locked = true\nlock L {{\n  open: locked\n  on click {{ locked = false }}\n  text \"{lock}\"\n}}\nbar Top {{\n  on click {{ locked = true }}\n  text \"{bar}\"\n}}\n"
            )
        };
        let file = dir.join("shell.strand");
        std::fs::write(&file, src("lock a", "bar a")).unwrap();
        let socket = dir.join("ipc.sock");
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
        m.until("lock and bar", |s| {
            let mut t = s.texts();
            t.sort();
            t == ["bar a", "lock a"]
        });
        let mut events =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
        assert_eq!(ok["ok"], true);
        let mut next_event = || {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut events, &mut line).unwrap();
            serde_json::from_str::<Json>(&line).unwrap()
        };
        // An edit that leaves the lock alone commits at once, lock shown.
        std::fs::write(&file, src("lock a", "bar a2")).unwrap();
        let ev = next_event();
        assert_eq!(ev["deferred"], false, "{ev}");
        m.until("the bar edit while locked", |s| {
            s.texts().contains(&"bar a2".to_string())
        });
        std::fs::write(&file, src("lock b", "bar b")).unwrap();
        let ev = next_event();
        assert_eq!(ev["deferred"], true, "{ev}");
        assert_eq!(ev["classes"], json!(["lock-deferred"]), "{ev}");
        assert_eq!(
            ev["notices"],
            json!(["shell.strand: waits for the unlock (the lock changed while it is shown)"]),
            "{ev}"
        );
        // Once a lock edit waits, a later save waits with it (the
        // loader's sources carry the lock edit; decisions.md).
        // `strand reload --hard` while locked: answered at once,
        // deferred (and it absorbs the deferred save).
        let mut client =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        std::fs::write(&file, src("lock b", "bar c")).unwrap();
        let ev = next_event();
        assert_eq!(ev["deferred"], true, "{ev}");
        // A save that waits only because a lock edit does is told so.
        assert!(
            ev["notices"].as_array().is_some_and(|n| n.contains(&json!(
                "shell.strand: waits for the unlock (a lock edit is waiting)"
            ))),
            "{ev}"
        );
        let ans = ipc::request(
            &mut client,
            &ipc::Request::Reload { hard: true },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["event"]["deferred"], true, "{ans}");
        let _ = next_event();
        assert_eq!(m.texts(), ["bar a2", "lock a"], "nothing committed yet");
        // Unlock: the newest deferred build lands, with both edits.
        let lock = m.scene.of_kind(strand_scene::NodeKind::Lock)[0];
        to_logic
            .send(ToLogic::Event {
                node: lock,
                event: NodeEvent::Click,
            })
            .unwrap();
        m.until("the bar edit", |s| s.texts().contains(&"bar c".to_string()));
        let ev = next_event();
        assert_eq!(ev["deferred"], false, "{ev}");
        assert!(
            ev["files"].as_array().is_some_and(|f| f
                .iter()
                .any(|p| p.as_str().is_some_and(|p| p.ends_with("shell.strand")))),
            "{ev}"
        );
        // Locked again: the lock shows its edit.
        let bar = m.scene.of_kind(strand_scene::NodeKind::Bar)[0];
        to_logic
            .send(ToLogic::Event {
                node: bar,
                event: NodeEvent::Click,
            })
            .unwrap();
        m.until("the lock edit", |s| {
            s.texts().contains(&"lock b".to_string())
        });
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A watcher on `socket`: the next event it hears.
    fn watch(socket: &Path) -> impl FnMut() -> Json {
        let mut events =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(socket).unwrap());
        let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
        assert_eq!(ok["ok"], true);
        move || {
            let mut line = String::new();
            std::io::BufRead::read_line(&mut events, &mut line).unwrap();
            serde_json::from_str::<Json>(&line).unwrap()
        }
    }

    fn panels(s: &SceneMirror) -> usize {
        s.of_kind(strand_scene::NodeKind::Panel).len()
    }

    /// design.md: "The fix commits and the overlay vanishes", also when
    /// the fix is an undo back to the last good text (nothing to commit).
    #[test]
    fn a_revert_to_the_last_good_text_closes_the_overlay() {
        let dir = temp_dir("revert");
        let file = dir.join("bar.strand");
        let good = "bar Top {\n  text \"a\"\n}\n";
        std::fs::write(&file, good).unwrap();
        let socket = dir.join("ipc.sock");
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
        m.until("the bar", |s| s.texts() == ["a"]);
        let mut next_event = watch(&socket);
        std::fs::write(&file, "bar Top {\n  txet \"a\"\n}\n").unwrap();
        let ev = next_event();
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        m.until("the overlay", |s| panels(s) == 1);
        std::fs::write(&file, good).unwrap();
        let ev = next_event();
        assert_eq!(ev["diagnostics"], json!([]), "{ev}");
        assert_eq!(ev["held"], json!([]), "{ev}");
        m.until("the overlay gone", |s| panels(s) == 0);
        assert_eq!(m.texts(), ["a"]);
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A deferred lock edit replayed after the unlock does not take away
    /// the overlay of a broken save made after it: the config on disk is
    /// still broken.
    #[test]
    fn an_unlock_replay_keeps_the_newer_errors() {
        let dir = temp_dir("lock-errors");
        let src = |lock: &str, bar: &str, el: &str| {
            format!(
                "export state locked = true\nlock L {{\n  open: locked\n  on click {{ locked = false }}\n  text \"{lock}\"\n}}\nbar Top {{\n  {el} \"{bar}\"\n}}\n"
            )
        };
        let file = dir.join("shell.strand");
        std::fs::write(&file, src("lock a", "bar a", "text")).unwrap();
        let socket = dir.join("ipc.sock");
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
        m.until("lock and bar", |s| s.texts().len() == 2);
        let mut next_event = watch(&socket);
        std::fs::write(&file, src("lock b", "bar b", "text")).unwrap();
        assert_eq!(next_event()["deferred"], true);
        std::fs::write(&file, src("lock b", "bar b", "txet")).unwrap();
        let ev = next_event();
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        m.until("the overlay", |s| panels(s) == 1);
        let lock = m.scene.of_kind(strand_scene::NodeKind::Lock)[0];
        to_logic
            .send(ToLogic::Event {
                node: lock,
                event: NodeEvent::Click,
            })
            .unwrap();
        m.until("the deferred edit", |s| {
            s.texts().contains(&"bar b".to_string())
        });
        let ev = next_event();
        assert_eq!(ev["deferred"], false, "{ev}");
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        assert_eq!(ev["held"].as_array().map(Vec::len), Some(1), "{ev}");
        // The replay's diff (the one with `bar b`) left the overlay up.
        assert_eq!(panels(&m.scene), 1, "the config is still broken");
        assert!(
            m.texts().join("\n").contains("unknown element"),
            "{:?}",
            m.texts()
        );
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A hard reload asked for while locked, made stale by a bar edit
    /// committed meanwhile, is still owed after the unlock; its replayed
    /// event reports the newest attempt's problems (a broken save after
    /// it), not a clean reload.
    #[test]
    fn a_replayed_hard_reload_reports_the_newer_errors() {
        let dir = temp_dir("hard-replay");
        let src = |bar: &str, el: &str| {
            format!(
                "export state locked = true\nlock L {{\n  open: locked\n  on click {{ locked = false }}\n  text \"lock\"\n}}\nbar Top {{\n  {el} \"{bar}\"\n}}\n"
            )
        };
        let file = dir.join("shell.strand");
        std::fs::write(&file, src("bar a", "text")).unwrap();
        let socket = dir.join("ipc.sock");
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
        m.until("lock and bar", |s| s.texts().len() == 2);
        let mut next_event = watch(&socket);
        let mut client =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ans = ipc::request(
            &mut client,
            &ipc::Request::Reload { hard: true },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["event"]["deferred"], true, "{ans}");
        assert_eq!(next_event()["deferred"], true);
        // A bar edit commits at once (the hard reload is still owed).
        std::fs::write(&file, src("bar b", "text")).unwrap();
        assert_eq!(next_event()["deferred"], false);
        m.until("the bar edit", |s| s.texts().contains(&"bar b".to_string()));
        // Broken.
        std::fs::write(&file, src("bar c", "txet")).unwrap();
        let ev = next_event();
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        let lock = m.scene.of_kind(strand_scene::NodeKind::Lock)[0];
        to_logic
            .send(ToLogic::Event {
                node: lock,
                event: NodeEvent::Click,
            })
            .unwrap();
        let ev = next_event();
        assert_eq!(ev["classes"], json!(["hard"]), "{ev}");
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        assert_eq!(ev["held"].as_array().map(Vec::len), Some(1), "{ev}");
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// design.md: "Format-on-save and delete-then-create saves never
    /// flash it": a broken text fixed within 100 ms (a formatter's second
    /// write) lands without the overlay ever opening.
    #[test]
    fn a_save_fixed_at_once_never_opens_the_overlay() {
        let dir = temp_dir("format-on-save");
        let file = dir.join("bar.strand");
        std::fs::write(&file, "bar Top {\n  text \"a\"\n}\n").unwrap();
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
        m.until("the bar", |s| s.texts() == ["a"]);
        m.steady = true;
        for (i, gap) in [5u64, 40, 90].into_iter().enumerate() {
            // The editor's write, then the formatter's.
            std::fs::write(&file, format!("bar Top {{\ntext \"b{i}\"\n")).unwrap();
            std::thread::sleep(Duration::from_millis(gap));
            std::fs::write(&file, format!("bar Top {{\n  text \"b{i}\"\n}}\n")).unwrap();
            let want = format!("b{i}");
            m.until("the formatted save", |s| s.texts() == [want.as_str()]);
        }
        m.settle("after the saves", Duration::from_millis(500));
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `strand reload` on a broken config: one compile and one event
    /// (the watcher's own re-listing that follows finds nothing new).
    #[test]
    fn a_reload_of_a_broken_config_is_one_event() {
        use std::io::BufRead;
        let dir = temp_dir("reload-broken");
        let file = dir.join("bar.strand");
        std::fs::write(&file, "bar Top {\n  text \"a\"\n}\n").unwrap();
        let socket = dir.join("ipc.sock");
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
        m.until("the bar", |s| s.texts() == ["a"]);
        let mut events =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
        assert_eq!(ok["ok"], true);
        let mut next = |wait: Duration| -> Option<Json> {
            events.get_ref().set_read_timeout(Some(wait)).unwrap();
            let mut line = String::new();
            match events.read_line(&mut line) {
                Ok(n) if n > 0 => Some(serde_json::from_str(&line).unwrap()),
                _ => None,
            }
        };
        std::fs::write(&file, "bar Top {\n  txet \"a\"\n}\n").unwrap();
        let ev = next(Duration::from_secs(10)).expect("the broken save");
        assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
        let mut client =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ans = ipc::request(
            &mut client,
            &ipc::Request::Reload { hard: false },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["event"]["requested"], true, "{ans}");
        assert_eq!(
            ans["event"]["diagnostics"][0]["severity"], "error",
            "the reload still reports the error: {ans}"
        );
        let ev = next(Duration::from_secs(10)).expect("the reload's event");
        assert_eq!(ev["requested"], true, "{ev}");
        if let Some(ev) = next(Duration::from_millis(500)) {
            panic!("a second event: {ev}");
        }
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// p95 of `v` (milliseconds).
    pub(crate) fn p95(v: &[f64]) -> f64 {
        let mut v = v.to_vec();
        v.sort_by(f64::total_cmp);
        let i = ((v.len() as f64 * 0.95).ceil() as usize).clamp(1, v.len()) - 1;
        v[i]
    }

    /// Save to pixels through the real pipeline: each edit is written
    /// in place, goes through the watcher (its 15 ms coalescing
    /// included), the compiler worker and the logic thread, and its diff
    /// is applied to a renderer and the bar painted (2560×40 at 1×).
    /// Returns the token edits' and the markup edits' times (ms).
    fn reload_latency(rounds: usize) -> (Vec<f64>, Vec<f64>) {
        use std::sync::Arc;
        use strand_scene::{NodeKind, PaintTarget, Painter, Scale, SceneOp, Size, SurfaceId};
        let dir = temp_dir("latency");
        let file = dir.join("bar.strand");
        let src = |bg: &str, extra: bool| {
            format!(
                "tokens base {{ bar.bg: {bg} }}\n\
                 bar Top {{\n\
                 \x20 state n = 0\n\
                 \x20 edge: top; height: 40\n\
                 \x20 bg: $bar.bg\n\
                 \x20 on click {{ n += 1 }}\n\
                 \x20 split {{\n\
                 \x20   start  {{ text \"start\" }}\n\
                 \x20   center {{ text clock.format(\"%H:%M\") }}\n\
                 \x20   end    {{ text join(\"\", n) }}\n\
                 {}\
                 \x20 }}\n\
                 }}\n",
                if extra { "    text \"extra\"\n" } else { "" }
            )
        };
        std::fs::write(&file, src("#204080", false)).unwrap();
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
        let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
        let size = Size::new(2560, 40);
        let mut px = vec![0u8; (size.w * size.h * 4) as usize];
        let mut attached = false;
        // Apply diffs until `done` holds for one, painting each; the
        // time the matching one is painted.
        let mut until =
            |m: &mut Mirror, done: &dyn Fn(&SceneDiff) -> bool| -> (Instant, SceneDiff) {
                loop {
                    let d = m
                        .inbox
                        .recv_timeout(Duration::from_secs(10))
                        .expect("a diff");
                    m.scene.apply(&d).unwrap();
                    let hit = done(&d);
                    let kept = hit.then(|| d.clone());
                    r.apply(d);
                    if !attached && let Some(&bar) = m.scene.of_kind(NodeKind::Bar).first() {
                        r.attach_surface(SurfaceId(1), bar);
                        attached = true;
                    }
                    let mut target =
                        PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, 1).unwrap();
                    r.paint(SurfaceId(1), &mut target);
                    if let Some(d) = kept {
                        return (Instant::now(), d);
                    }
                }
            };
        until(&mut m, &|_| true);
        // The clock's text may tick during a token edit; nothing else
        // may change with it.
        let clock = m
            .scene
            .of_kind(NodeKind::Text)
            .into_iter()
            .find(|&n| {
                matches!(m.scene.prop(n, strand_scene::Prop::Text),
                    Some(PropValue::Text(t)) if t.contains(':'))
            })
            .expect("the clock");
        let (mut tokens, mut markup) = (Vec::new(), Vec::new());
        let mut extra = false;
        for i in 1..=rounds {
            std::thread::sleep(Duration::from_millis(40));
            // A token edit: the bar's colour.
            let bg = format!("#{:02x}4080", (i * 7) % 256);
            let saved = Instant::now();
            std::fs::write(&file, src(&bg, extra)).unwrap();
            let (painted, d) = until(&mut m, &|d| {
                d.ops.iter().any(|o| matches!(o, SceneOp::SetTokens { .. }))
            });
            // A pure token edit: the token swap and nothing else.
            assert!(
                d.ops.iter().all(|o| match o {
                    SceneOp::SetTokens { .. } => true,
                    SceneOp::SetProp { id, .. } => *id == clock,
                    _ => false,
                }),
                "{:#?}",
                d.ops
            );
            tokens.push(painted.duration_since(saved).as_secs_f64() * 1e3);
            std::thread::sleep(Duration::from_millis(40));
            // A markup edit: a node added or removed.
            extra = !extra;
            let saved = Instant::now();
            std::fs::write(&file, src(&bg, extra)).unwrap();
            let (painted, _) = until(&mut m, &|d| {
                d.ops
                    .iter()
                    .any(|o| matches!(o, SceneOp::Create { .. } | SceneOp::Remove { .. }))
            });
            markup.push(painted.duration_since(saved).as_secs_f64() * 1e3);
        }
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
        (tokens, markup)
    }

    /// design.md, "Live reload": at p95 a token edit shows within 35 ms
    /// of save and a markup edit within 50 ms. Measured save → painted
    /// buffer through the real watcher, compiler worker, logic thread
    /// and renderer (the compositor's present is not in it). Checked on
    /// an optimised build only, against the budget itself: CI runs
    /// `cargo test --release -p strand --bin strand reload_latency`
    /// (an unoptimised repaint alone takes about half the token budget,
    /// so a debug run would measure the build, not the design).
    /// `STRAND_LATENCY_ROUNDS` sets the edits per kind (20).
    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "a budget test: run optimised (cargo test --release)"
    )]
    fn reload_latency_meets_its_budget() {
        let rounds = std::env::var("STRAND_LATENCY_ROUNDS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(20);
        let (tokens, markup) = reload_latency(rounds);
        let (pt, pm) = (p95(&tokens), p95(&markup));
        eprintln!(
            "reload latency over {rounds} edits each: token p95 {pt:.1} ms (max {:.1}), markup p95 {pm:.1} ms (max {:.1})",
            tokens.iter().copied().fold(0.0, f64::max),
            markup.iter().copied().fold(0.0, f64::max),
        );
        assert!(pt <= 35.0, "token edits: p95 {pt:.1} ms: {tokens:?}");
        assert!(pm <= 50.0, "markup edits: p95 {pm:.1} ms: {markup:?}");
    }

    /// design.md, "Live reload": monitor changes show on the next frame.
    /// The logic thread answers a plug, a scale change, an unplug and a
    /// replug each in the first diff it sends after hearing of it (no
    /// reload pipeline, no coalescing), with the whole change in it; the
    /// main thread paints that diff in its next frame
    /// (`bench.rs::reload_latency_to_the_presented_frame` times a plug on
    /// sway).
    #[test]
    fn a_monitor_change_is_in_the_next_diff() {
        let dir = temp_dir("next-frame");
        std::fs::write(
            dir.join("bar.strand"),
            "bar Top {\n  state n = 0\n  on click { n += 1 }\n  text join(\" \", screen.name, screen.scale, n)\n}\n",
        )
        .unwrap();
        let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
        m.until("the bar", |s| s.texts() == ["DP-1 1 0"]);
        let a = m.scene.roots()[0];
        to_logic
            .send(ToLogic::Event {
                node: a,
                event: NodeEvent::Click,
            })
            .unwrap();
        m.until("a click", |s| s.texts() == ["DP-1 1 1"]);
        // Nothing else is coming: no clock, no animation.
        m.settle("quiet", Duration::from_millis(100));
        let next = |m: &mut Mirror, what: &str, msg: ToLogic| {
            to_logic.send(msg).unwrap();
            let d = m
                .inbox
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("{what}: no diff"));
            m.apply(what, &d);
            m.texts()
        };
        let b = screen("B", "HDMI-A-1");
        let mut b2 = b.clone();
        b2.scale = 2.0;
        let a1 = screen("A", "DP-1");
        assert_eq!(
            next(
                &mut m,
                "a plug",
                ToLogic::Screens(vec![a1.clone(), b.clone()])
            ),
            ["DP-1 1 1", "HDMI-A-1 1 0"]
        );
        assert_eq!(
            next(
                &mut m,
                "a scale change",
                ToLogic::Screens(vec![a1.clone(), b2.clone()])
            ),
            ["DP-1 1 1", "HDMI-A-1 2 0"]
        );
        assert_eq!(
            next(&mut m, "an unplug", ToLogic::Screens(vec![a1.clone()])),
            ["DP-1 1 1"]
        );
        assert_eq!(
            next(&mut m, "a replug", ToLogic::Screens(vec![a1, b2])),
            ["DP-1 1 1", "HDMI-A-1 2 0"]
        );
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The scene as text with surfaces in a fixed order.
    pub(crate) fn canonical(scene: &SceneMirror) -> String {
        let mut blocks: Vec<String> = Vec::new();
        for line in scene.render().lines() {
            if !line.starts_with(' ') || blocks.is_empty() {
                blocks.push(String::new());
            }
            if let Some(b) = blocks.last_mut() {
                b.push_str(line);
                b.push('\n');
            }
        }
        blocks.sort();
        blocks.concat()
    }

    /// What a cold boot of `dir` shows (on one monitor `A`).
    fn cold_boot(dir: &Path) -> String {
        let out = load(dir);
        let build = out.build.unwrap();
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::real(&rt, &build.program.types));
        set_screens(&rt, &host, &[screen("A", "DP-1")]);
        let inst = Instance::from_build(&rt, &build, host, Storage::none());
        let mut m = SceneMirror::new();
        m.apply(&inst.flush().diff).unwrap();
        canonical(&m)
    }

    /// The reload fuzzer at the process level: random valid edits of a
    /// two-file config, each saved in one of five editor styles (in
    /// place, rename over, backup-then-rename, delete-then-create, a
    /// symlink swapped to a new target), go through the real watcher and
    /// compiler worker; after each the running scene equals a cold boot
    /// of the same files, and no diff ever blanks it.
    #[test]
    fn five_save_styles_land_on_a_cold_boot() {
        let dir = temp_dir("styles");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        // Valid variants of each file, chosen at random.
        let bar = |r: &mut dyn FnMut(u64) -> u64| -> String {
            let mut s = String::from("bar Top {\n  state n = ");
            s.push_str(&r(5).to_string());
            s.push_str("\n  height: ");
            s.push_str(&(24 + r(3) * 8).to_string());
            s.push('\n');
            if r(2) == 0 {
                s.push_str("  opacity: 0.");
                s.push_str(&(1 + r(9)).to_string());
                s.push('\n');
            }
            s.push_str("  row {\n");
            for i in 0..r(4) {
                s.push_str(&format!("    text join(\" \", theme.label, n, {i})\n"));
            }
            if r(2) == 0 {
                s.push_str("    Pill\n");
            }
            s.push_str("  }\n}\n");
            s
        };
        let theme = |r: &mut dyn FnMut(u64) -> u64| -> String {
            format!(
                "export let label = \"v{}\"\ntokens base {{ pill.gap: {}px }}\ncomponent Pill {{\n  state on = false\n  box {{ width: {}; on click {{ on = !on }} }}\n}}\n",
                r(7),
                r(9),
                10 + r(5)
            )
        };
        let bar_path = config.join("bar.strand");
        let theme_path = config.join("theme.strand");
        let first_bar = bar(&mut rnd);
        std::fs::write(&bar_path, &first_bar).unwrap();
        // theme.strand is a link into the store, as home-manager makes it.
        let mut version = 0;
        let target = store.join(format!("theme-{version}.strand"));
        std::fs::write(&target, theme(&mut rnd)).unwrap();
        std::os::unix::fs::symlink(&target, &theme_path).unwrap();
        let (wtx, wrx) = calloop::channel::channel();
        let (compiler, boot) = Worker::spawn(&config, None, wtx).unwrap();
        let live = Live {
            worker: Some(wrx),
            jobs: Some(compiler.jobs()),
            socket: None,
            buses: None,
        };
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
        let mut m = Mirror::new(rx);
        let expect = cold_boot(&config);
        m.until("the boot", |s| canonical(s) == expect);
        // From here on every diff keeps the bar and never opens the
        // error overlay: a delete-then-create or backup-then-rename save
        // is briefly a missing file, never shown as an error.
        m.steady = true;
        let rounds = std::env::var("STRAND_SAVE_FUZZ")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(25);
        for round in 0..rounds {
            let which = rnd(2);
            let style = round % 5;
            let (path, text) = if which == 0 {
                (bar_path.clone(), bar(&mut rnd))
            } else {
                (theme_path.clone(), theme(&mut rnd))
            };
            match style {
                // In place: truncate and write.
                0 => std::fs::write(&path, &text).unwrap(),
                // Rename over (Helix, VS Code's atomic save).
                1 => {
                    let tmp = config.join(".tmp-save");
                    std::fs::write(&tmp, &text).unwrap();
                    std::fs::rename(&tmp, &path).unwrap();
                }
                // Backup then rename (Vim's backupcopy=no).
                2 => {
                    let backup = path.with_extension("strand~");
                    std::fs::rename(&path, &backup).unwrap();
                    std::fs::write(&path, &text).unwrap();
                    std::fs::remove_file(&backup).unwrap();
                }
                // Delete, then create.
                3 => {
                    std::fs::remove_file(&path).unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                    std::fs::write(&path, &text).unwrap();
                }
                // A symlink swapped to a new target.
                _ => {
                    version += 1;
                    let target = store.join(format!("v{version}.strand"));
                    std::fs::write(&target, &text).unwrap();
                    let link = config.join(".link-tmp");
                    let _ = std::fs::remove_file(&link);
                    std::os::unix::fs::symlink(&target, &link).unwrap();
                    std::fs::rename(&link, &path).unwrap();
                }
            }
            let expect = cold_boot(&config);
            m.until(&format!("round {round} (style {style})"), |s| {
                canonical(s) == expect
            });
        }
        // An overlay a save had armed would open 250 ms after it.
        m.settle("after the saves", Duration::from_millis(400));
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The main thread gone without a word (every sender dropped) ends
    /// the logic thread too: the runtime's wake hook holds no sender.
    #[test]
    fn the_logic_thread_ends_with_the_main_thread() {
        let dir = temp_dir("orphan");
        std::fs::write(dir.join("bar.strand"), "bar Top { text \"hi\" }\n").unwrap();
        let program = load(&dir);
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || {
            logic(program, Storage::none(), from_main, tx, Live::default())
        });
        let mut m = Mirror::new(rx);
        m.until("the bar", |s| s.texts() == ["hi"]);
        drop(to_logic);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !t.is_finished() {
            assert!(Instant::now() < deadline, "the logic thread did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(t.join().unwrap(), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The sleep ends at the wall-clock wake even when the logic clock
    /// has nothing due for long: the wake is a `CLOCK_REALTIME` timer,
    /// so a deadline already past (a resume after a suspend, a clock set
    /// forward) steps at once instead of after a monotonic countdown.
    #[test]
    fn a_wall_clock_wake_in_the_past_steps_at_once() {
        let (_tx, rx) = calloop::channel::channel::<ToLogic>();
        let (mut sleeper, _ping) = Sleeper::new(rx).unwrap();
        let mut inbox = Inbox::default();
        let now = SystemTime::now();
        let started = Instant::now();
        // The clock jumped past the wake: due an hour ago.
        sleeper
            .sleep(
                Some(Duration::from_secs(60)),
                Some(now - Duration::from_secs(3600)),
                now,
                &mut inbox,
            )
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        // Due shortly: woken by the timer, not the 60 s timeout.
        let started = Instant::now();
        sleeper
            .sleep(
                Some(Duration::from_secs(60)),
                Some(SystemTime::now() + Duration::from_millis(50)),
                SystemTime::now(),
                &mut inbox,
            )
            .unwrap();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(40), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        // The clock set back after the step: no wait at all.
        let started = Instant::now();
        sleeper
            .sleep(
                Some(Duration::from_secs(60)),
                Some(SystemTime::now() + Duration::from_secs(60)),
                SystemTime::now() + Duration::from_secs(3600),
                &mut inbox,
            )
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(inbox.msgs.is_empty() && !inbox.closed);
    }

    /// No state directory: persisted state is not kept, but settings
    /// files still are.
    #[test]
    fn settings_survive_a_missing_state_directory() {
        let dir = Path::new("/etc/strand-config");
        let s = storage_or(dir, Err(strand_core::PersistError::NoStateDir));
        assert!(s.persist.is_none());
        assert!(s.settings.is_some());
        assert_eq!(s.config_dir.as_deref(), Some(dir));
    }

    /// A settings file edited by hand with a bad value: the field keeps
    /// its last good value and the overlay says so; the fix applies.
    /// (Saved whole, as editors do: a truncate-then-write save can be
    /// read empty in between, and an empty file is every default.)
    #[test]
    fn a_bad_settings_value_is_kept_and_shown() {
        fn save(path: &Path, text: &str) {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, path).unwrap();
        }
        let dir = temp_dir("settings-notice");
        std::fs::write(dir.join("prefs.toml"), "# mine\ngap = 6\n").unwrap();
        std::fs::write(
            dir.join("bar.strand"),
            "state prefs from \"prefs.toml\" { gap: int = 4 }\nbar Top { text join(\" \", \"gap\", prefs.gap) }\n",
        )
        .unwrap();
        let storage = Storage::in_dirs(dir.join("state"), &dir);
        let (compiler, to_logic, t, mut m) = spawn_live_with(&dir, None, None, storage);
        m.until("the file's value", |s| s.texts() == ["gap 6"]);
        save(&dir.join("prefs.toml"), "# mine\ngap = \"wide\"\n");
        m.until("the notice", |s| {
            s.texts()
                .iter()
                .any(|t| t.contains("gap") && t.contains("keeping its last good value"))
        });
        assert!(m.scene.texts().contains(&"gap 6".to_string()), "kept");
        // A settings-only overlay says so (no `[reset]` here).
        m.until("the settings header", |s| {
            s.texts()
                .iter()
                .any(|t| t.starts_with("strand: settings files"))
        });
        assert!(
            !m.scene.texts().iter().any(|t| t.contains("[reset]")),
            "{:?}",
            m.scene.texts()
        );
        save(&dir.join("prefs.toml"), "# mine\ngap = 8\n");
        m.until("the fix", |s| s.texts().contains(&"gap 8".to_string()));
        // Fixed: its notice goes, and the overlay with it.
        m.until("the notice gone", |s| {
            !s.texts()
                .iter()
                .any(|t| t.contains("keeping its last good value") || t.starts_with("strand:"))
        });
        // A syntax error, then the file parses again: same.
        save(&dir.join("prefs.toml"), "# mine\ngap = = 8\n");
        m.until("the syntax notice", |s| {
            s.texts()
                .iter()
                .any(|t| t.contains("keeping every last good value"))
        });
        save(&dir.join("prefs.toml"), "# mine\ngap = 9\n");
        m.until("parsed again", |s| {
            s.texts().contains(&"gap 9".to_string())
                && !s.texts().iter().any(|t| t.starts_with("strand:"))
        });
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A mock `org.freedesktop.portal.Settings` for the theme test.
    struct MockPortal {
        values: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl MockPortal {
        async fn read_one(
            &self,
            namespace: &str,
            key: &str,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedValue> {
            if namespace != "org.freedesktop.appearance" {
                return Err(zbus::fdo::Error::Failed("not found".into()));
            }
            self.values
                .get(key)
                .map(|v| v.try_clone().unwrap())
                .ok_or_else(|| zbus::fdo::Error::Failed("not found".into()))
        }

        #[zbus(property)]
        fn version(&self) -> u32 {
            2
        }
    }

    struct Bus {
        child: std::process::Child,
        address: String,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// A private session bus (skipped without `dbus-daemon` unless
    /// `STRAND_REQUIRE_DBUS` is set).
    fn private_bus(dir: &Path) -> Option<Bus> {
        use std::io::BufRead;
        let spawned = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}/bus", dir.display()))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) if std::env::var_os("STRAND_REQUIRE_DBUS").is_none() => {
                eprintln!("skipping: dbus-daemon unavailable ({e})");
                return None;
            }
            Err(e) => panic!("dbus-daemon: {e}"),
        };
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Some(Bus {
            child,
            address: line.trim().to_string(),
        })
    }

    fn png(major: [u8; 3], minor: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_fn(320, 180, |x, _| {
            image::Rgb(if x < 240 { major } else { minor })
        });
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn accent(s: &SceneMirror) -> Option<strand_scene::Color> {
        match s.tokens.lookup("accent") {
            Some(PropValue::Color(c)) => Some(c),
            _ => None,
        }
    }

    /// design.md's theme.strand in `strand run`, end to end: the portal's
    /// boot read and changes (`system.dark`, `system.accent`), `strand
    /// set theme.look …` over IPC, a wallpaper palette that follows the
    /// file, its symlink and the link's target, and a restart that boots
    /// straight into the last palette.
    #[test]
    fn the_theme_follows_the_portal_the_wallpaper_and_strand_set() {
        use strand_theme::{Options, Role, from_seed};
        use zbus::zvariant::{OwnedValue, Value as ZValue};
        let dir = temp_dir("theme");
        let config = dir.join("config");
        let walls = dir.join("walls");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&walls).unwrap();
        let Some(bus) = private_bus(&dir) else {
            return;
        };
        let theme = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../strand-compiler/tests/fixtures/theme.strand"
        ))
        .unwrap()
        .replace("~/.config/strand/prefs.toml", "prefs.toml");
        std::fs::write(config.join("theme.strand"), theme).unwrap();
        std::fs::write(
            config.join("bar.strand"),
            "bar Top {\n  bg: $surface\n  text \"themed\" { color: $fg }\n}\n",
        )
        .unwrap();
        // The wallpaper is a link into the wallpapers directory.
        let (blue, red, green) = (
            png([30, 90, 200], [240, 200, 40]),
            png([200, 40, 40], [20, 20, 20]),
            png([40, 170, 60], [230, 230, 230]),
        );
        std::fs::write(walls.join("blue.png"), &blue).unwrap();
        std::fs::write(walls.join("red.png"), &red).unwrap();
        let wall = dir.join("wall.png");
        std::os::unix::fs::symlink(walls.join("blue.png"), &wall).unwrap();
        std::fs::write(
            config.join("prefs.toml"),
            format!("# my prefs\nwallpaper = \"{}\"\n", wall.display()),
        )
        .unwrap();

        // The portal: dark, a red accent.
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let owned = |v: ZValue<'_>| -> OwnedValue { v.try_into().unwrap() };
        let values = std::collections::HashMap::from([
            ("color-scheme".to_string(), owned(ZValue::from(1u32))),
            (
                "accent-color".to_string(),
                owned(ZValue::from((0.88f64, 0.11f64, 0.14f64))),
            ),
            ("contrast".to_string(), owned(ZValue::from(0u32))),
            ("reduced-motion".to_string(), owned(ZValue::from(1u32))),
        ]);
        let conn = tokio.block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .name("org.freedesktop.portal.Desktop")
                .unwrap()
                .serve_at("/org/freedesktop/portal/desktop", MockPortal { values })
                .unwrap()
                .build()
                .await
                .unwrap()
        });
        let state = dir.join("state");
        let storage = Storage::in_dirs(&state, &config);
        let socket = dir.join("s.sock");
        let (compiler, to_logic, t, mut m) = spawn_live_with(
            &config,
            Some(socket.clone()),
            Some(strand_services::Buses::private(&bus.address)),
            storage,
        );
        let portal_accent = strand_scene::Color::rgb(0.88, 0.11, 0.14);
        let seeded = |dark| {
            from_seed(
                // The portal's accent arrives as f64 sRGB.
                portal_accent,
                Options {
                    dark,
                    ..Options::default()
                },
            )
            .get(Role::Accent)
        };
        // look: auto → the portal's dark and accent.
        m.until("the portal's boot read", |s| {
            accent(s) == Some(seeded(true))
        });
        // The portal's `reduced-motion` reaches render (it snaps every
        // spring), and its change too.
        m.until_reduced(true);
        tokio
            .block_on(conn.emit_signal(
                None::<&str>,
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
                "SettingChanged",
                &(
                    "org.freedesktop.appearance",
                    "reduced-motion",
                    ZValue::from(0u32),
                ),
            ))
            .unwrap();
        m.until_reduced(false);
        // The desktop switches to light.
        tokio
            .block_on(conn.emit_signal(
                None::<&str>,
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
                "SettingChanged",
                &(
                    "org.freedesktop.appearance",
                    "color-scheme",
                    ZValue::from(2u32),
                ),
            ))
            .unwrap();
        m.until("light", |s| accent(s) == Some(seeded(false)));

        let mut ipc_conn =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let mut ask = |look: &str| -> Json {
            ipc::request(
                &mut ipc_conn,
                &ipc::Request::Set {
                    path: "theme.look".into(),
                    value: look.into(),
                },
                Duration::from_secs(10),
            )
            .unwrap()
        };
        let mut set = |look: &str| {
            let ans = ask(look);
            assert_eq!(ans["ok"], json!(true), "{ans}");
            ans
        };
        // strand set theme.look mocha.
        set("mocha");
        m.until("mocha", |s| {
            accent(s) == strand_scene::Color::from_hex("#cba6f7")
        });
        // The wallpaper: quantised, then its palette.
        let image_accent = |bytes: &[u8]| {
            let seed = strand_theme::image::seed_from_bytes(bytes).unwrap();
            from_seed(
                seed,
                Options {
                    dark: true,
                    ..Options::default()
                },
            )
            .get(Role::Accent)
        };
        set("wallpaper");
        m.until("the blue wallpaper", |s| {
            accent(s) == Some(image_accent(&blue))
        });
        // swww-style: the link swapped to another file.
        let tmp = dir.join("wall.png.new");
        std::os::unix::fs::symlink(walls.join("red.png"), &tmp).unwrap();
        std::fs::rename(&tmp, &wall).unwrap();
        m.until("the link swapped to red", |s| {
            accent(s) == Some(image_accent(&red))
        });
        // The link's target replaced in its own directory.
        std::fs::write(walls.join("next.png"), &green).unwrap();
        std::fs::rename(walls.join("next.png"), walls.join("red.png")).unwrap();
        m.until("the target replaced by green", |s| {
            accent(s) == Some(image_accent(&green))
        });
        set("auto");
        m.until("auto again", |s| accent(s) == Some(seeded(false)));
        // Bad input is refused without closing anything.
        let ans = ask("sepia");
        assert_eq!(ans["ok"], json!(false), "{ans}");
        assert!(ans["error"].as_str().unwrap().contains("sepia"), "{ans}");
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        drop(conn);
        drop(bus);

        // A restart without the portal: the last values are in the boot
        // table itself (no default-colour frame while a portal answers).
        let storage = Storage::in_dirs(&state, &config);
        let (compiler, to_logic, t, mut m) = spawn_live_with(&config, None, None, storage);
        m.until("the first table", |s| accent(s).is_some());
        assert_eq!(accent(&m.scene), Some(seeded(false)));
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        let _ = std::fs::remove_dir_all(dir);
    }
}

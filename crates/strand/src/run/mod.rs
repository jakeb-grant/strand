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
use strand_scene::{NodeId, SceneDiff, SceneOp};
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
use strand_watch::{CacheKind, Role};

mod lists;
mod lock;
mod logic;
mod shell;
mod sleep;
mod trim;

pub use logic::{Live, Switched, logic};
pub(crate) use trim::trim;
use trim::{Trimmer, structural};

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
    /// The rows a virtualised `list` wants mounted: its view plus
    /// overscan, as global row indexes (`Renderer::take_list_windows`),
    /// applied with `Instance::set_list_window`.
    ListWindow {
        list: NodeId,
        first: u32,
        count: u32,
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

/// A cache source changed, on the watcher's thread: the shared icon
/// lookups and the apps service are told at once; the result is what the
/// renderer drops ([`caches_changed`]). An app installed with its icon
/// (`~/.local/share/icons/hicolor/256x256/apps/foo.png`, deeper than the
/// icon directories' one-level watch, `icon-theme.cache` untouched)
/// counts as an icon change too: a miss remembered for that name is
/// forgotten, by the apps service and the renderer alike.
fn cache_changed(kind: CacheKind) -> CacheKind {
    match kind {
        CacheKind::Apps | CacheKind::Icons => {
            // Before the apps service looks its icons up again.
            strand_icons::invalidate();
            strand_services::apps::changed();
            CacheKind::Icons
        }
        CacheKind::Fonts => CacheKind::Fonts,
    }
}

/// A cache source changed: the renderer drops what it no longer holds
/// true (icons looked up afresh, text shaped again) and its surfaces
/// repaint.
fn caches_changed(state: &mut strand_surface::State<Host>, kind: CacheKind) {
    let r = &mut state.host_mut().renderer;
    match kind {
        CacheKind::Icons => r.icons_changed(),
        CacheKind::Fonts => r.fonts_changed(),
        CacheKind::Apps => return,
    }
    crate::demo::text_ready(state);
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
    let mut live = Live {
        worker: Some(worker_rx),
        jobs: Some(compiler.jobs()),
        socket: ipc::socket_path(),
        buses: Some(strand_services::Buses::default()),
        icon_theme_switched: None,
    };
    let (ping, ping_source) = calloop::ping::make_ping()?;
    let wake = ping.clone();
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    renderer.set_first_frame_wait(FIRST_FRAME_TEXT_WAIT);
    // Apps, icons and fonts are caches their directories' changes
    // invalidate (design.md, "Change sources"): the watcher (on the
    // compiler worker) reports them; the renderer's caches are dropped on
    // this thread, the apps service is told directly.
    let (caches_tx, caches_rx) = calloop::channel::channel::<CacheKind>();
    let icons_tx = caches_tx.clone();
    let _ = compiler.jobs().send(Job::Caches {
        sources: live::cache_sources(),
        changed: live::CacheSink(Box::new(move |kind| {
            let _ = caches_tx.send(cache_changed(kind));
        })),
    });
    // The portal's icon theme switching (followed by the logic thread's
    // services) invalidates like an `index.theme` change.
    live.icon_theme_switched = Some(Switched(Box::new(move || {
        let _ = icons_tx.send(CacheKind::Icons);
    })));
    let (to_logic, from_main) = calloop::channel::channel::<ToLogic>();
    let host = Host::new(renderer, log.damage)
        .forwarding(to_logic.clone())
        .waking(wake);
    let mut mgr = SurfaceManager::connect(host, Config::default())?;
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| crate::demo::text_ready(state))
        .map_err(|e| DemoError::Io(io::Error::other(e.error)))?;
    handle
        .insert_source(caches_rx, |event, _, state| {
            if let Event::Msg(kind) = event {
                caches_changed(state, kind);
            }
        })
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
    // A structural diff applied: the main thread trims once quiet too.
    let shaped = Rc::new(Cell::new(false));
    let shape = Rc::clone(&shaped);
    handle
        .insert_source(rx, move |event, _, state| match event {
            Event::Msg(diff) => {
                if structural(&diff) {
                    shape.set(true);
                }
                apply(state, diff)
            }
            Event::Closed => flag.set(true),
        })
        .map_err(|e| DemoError::Io(io::Error::other(e.error)))?;
    // The main thread's frees (frames, layouts, surfaces) are returned
    // to the system once it has been quiet a moment too ([`trim`]).
    let mut trimmer = Trimmer::default();
    let end = loop {
        if hung_up.get() {
            break End::LogicEnded;
        }
        if signalled.get() {
            break End::Done;
        }
        let now = Instant::now();
        if shaped.take() {
            trimmer.arm(now);
        }
        trimmer.run(now);
        trimmer.settle(now, mgr.state().host().renderer.in_motion());
        match mgr.dispatch(trimmer.wait(now)) {
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
pub(crate) mod tests;

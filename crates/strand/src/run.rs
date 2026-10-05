//! `strand run [dir]`: the config in `dir` (default
//! `$XDG_CONFIG_HOME/strand`) compiled and run, wired as
//! `docs/architecture.md` describes ("Threads", and the host-loop recipe
//! under Instantiation).
//!
//! - The main thread runs the surface manager and the renderer, as the
//!   demo does. Its host forwards the surface layer's monitor hooks to the
//!   logic thread as the `screens` service (`Screen.id` is the
//!   `MonitorId`; `monitor_forgotten` is `Instance::forget_screen`), and
//!   surface-level input and layout facts as `event`/`set_flag`/
//!   `set_size` on the surface's node (hit testing inside a surface is
//!   M2).
//! - The logic thread owns the runtime, the real service host
//!   (`SchemaHost::real`: the wall clock and calendar) and the
//!   `Instance`, and loops on `Instance::step(now, wall)`, sending one
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
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_render::{Renderer, TextBackend};
use strand_scene::{NodeId, SceneDiff};
use strand_surface::{Config, SurfaceManager};
use strand_text::{FontConfig, TextWorker};

use crate::demo::clock::WallTimer;
use crate::demo::host::Host;
use crate::demo::{DemoError, FIRST_FRAME_TEXT_WAIT, apply, connection_closed};
use crate::ipc;
use crate::live::{self, FromWorker, Job, Loaded, Worker};
use crate::logging::LogConfig;
use crate::overlay::{self, Click, Overlay};
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

/// What the main thread tells the logic thread.
#[derive(Clone, Debug, PartialEq)]
pub enum ToLogic {
    /// The plugged-in monitors, in plug order (the first counts as
    /// focused until a compositor service says otherwise).
    Screens(Vec<ScreenInfo>),
    /// A monitor unplugged 30 s ago did not come back.
    Forget(String),
    /// `on <name>` on a surface's node, with numeric arguments (`scroll`:
    /// `dy, dx`).
    Event {
        node: NodeId,
        name: &'static str,
        args: Vec<f64>,
    },
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
    /// The run is over (a signal, the compositor gone): unmount, flush
    /// what is kept and end.
    Shutdown,
}

/// The `screens` service fields for `screens`.
fn set_screens(rt: &Runtime, host: &SchemaHost, screens: &[ScreenInfo]) {
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
}

/// The logic thread's state between steps (see [`logic`]).
struct Shell {
    inst: Instance,
    host: Rc<SchemaHost>,
    build: Build,
    overlay: Overlay,
    server: Option<ipc::Server>,
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// A build that changes a lock while one is shown: committed after
    /// the unlock.
    deferred: Option<Box<Loaded>>,
    /// Reload events waiting for the step that draws them (their total
    /// time ends when its diff is sent).
    events: Vec<(Json, Instant)>,
    /// Clients waiting for the reload they asked for.
    waiting: Vec<ipc::ClientId>,
    /// The settings files last given to the watcher.
    watched: Vec<PathBuf>,
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
            ToLogic::Event { node, name, args } => {
                if name == "click"
                    && let Some(c) = self.overlay.click(node, inst)
                {
                    if let Click::Open(file, line, col) = c {
                        overlay::open_editor(&file, line, col);
                    }
                    return;
                }
                inst.event(node, name, args.into_iter().map(Value::float).collect());
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

    /// A result from the compiler worker.
    fn worker(&mut self, msg: FromWorker) {
        match msg {
            FromWorker::Settings(paths) => {
                for p in paths {
                    self.inst.reload_settings(&p);
                }
            }
            FromWorker::Loaded(l) => self.commit(l),
        }
    }

    /// Commit a load: the new build into the running instance (a hard
    /// reload recreates everything), its diagnostics to the overlay, its
    /// event queued for the watchers.
    fn commit(&mut self, l: Box<Loaded>) {
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
        if let Some(r) = &report
            && r.classes == [EditClass::LockDeferred]
        {
            log::info!("a lock is shown: the reload waits for the unlock");
            self.deferred = Some(l);
            return;
        }
        if let Some(b) = build.or_else(|| l.hard.then(|| self.build.clone())) {
            self.build = b;
        }
        let commit = began.elapsed();
        // The overlay: every diagnostic of the attempt (none when it all
        // committed).
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
        for n in report.iter().flat_map(|r| &r.notices) {
            log::info!("{n}");
        }
        if errors > 0 {
            log::warn!(
                "{}",
                render(&l.outcome.diagnostics, &l.outcome.sources, Style::Plain)
            );
        }
        self.watch_settings();
        let ev = reload_event(&l, report.as_ref(), commit);
        self.events.push((ev, l.saved.unwrap_or(l.started)));
    }

    /// Give the worker the settings files the program now mounts.
    fn watch_settings(&mut self) {
        let files = self.inst.settings_files();
        if files != self.watched {
            self.watched = files.clone();
            if let Some(j) = &self.jobs {
                let _ = j.send(Job::Referenced(
                    files.into_iter().map(|f| (f, Role::Settings)).collect(),
                ));
            }
        }
    }

    /// A request from an IPC client.
    fn request(&mut self, id: ipc::ClientId, req: ipc::Request) {
        match req {
            ipc::Request::Reload { hard } => match &self.jobs {
                Some(j) if j.send(Job::Reload { hard }).is_ok() => self.waiting.push(id),
                _ => {
                    if let Some(s) = &mut self.server {
                        s.send(
                            id,
                            &json!({"ok": false, "error": "this shell does not reload"}),
                        );
                    }
                }
            },
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
        for d in &update.diagnostics {
            log::warn!("{d:?}");
        }
        for n in &update.notices {
            log::info!("{n}");
        }
        let now = Instant::now();
        for (mut ev, since) in self.events.drain(..) {
            ev["timing"]["total_ms"] = json!(ms(now.saturating_duration_since(since)));
            log::info!("{}", ipc::describe(&ev).trim_end());
            if let Some(s) = &mut self.server {
                s.broadcast(&ev);
                for id in self.waiting.drain(..) {
                    s.send(id, &json!({"ok": true, "event": ev.clone()}));
                }
            }
        }
    }
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
        let short = render_short(&l.outcome.diagnostics, &l.outcome.sources);
        l.outcome
            .diagnostics
            .iter()
            .zip(short.lines())
            .map(|(d, line)| {
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
        "reset": r.reset.iter().map(|(c, w)| json!({"cell": c, "why": w})).collect::<Vec<_>>(),
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
    rt.set_wake_hook(move || ping.ping());
    let build = boot.build.clone().unwrap_or_else(Build::empty);
    let host = Rc::new(SchemaHost::real(&rt, &build.program.types));
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
    let inst = Instance::from_build(&rt, &build, host.clone(), storage);
    let mut shell = Shell {
        inst,
        host,
        build,
        overlay: Overlay::default(),
        server,
        jobs: live.jobs,
        deferred: None,
        events: Vec::new(),
        waiting: Vec::new(),
        watched: Vec::new(),
    };
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
    while !stop {
        if let Some(l) = shell.deferred.take() {
            if shell.inst.lock_shown() {
                shell.deferred = Some(l);
            } else {
                shell.commit(l);
            }
        }
        let wall = SystemTime::now();
        let (mut update, wake) = shell.inst.step(start.elapsed(), wall);
        let diff = std::mem::take(&mut update.diff);
        if !diff.is_empty() && out.send(diff).is_err() {
            break;
        }
        shell.after_step(&update);
        let queued = shell.server.as_mut().is_some_and(|s| s.flush(&handle));
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
        also(queued.then_some(Duration::from_millis(50)));
        if shell.deferred.is_some() {
            also(Some(Duration::from_millis(250)));
        }
        sleeper
            .sleep(timeout, wake.wall, wall, &mut inbox)
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
    let Shell { mut inst, host, .. } = shell;
    inst.shutdown();
    drop(inst);
    rt.shutdown();
    drop(host);
    Ok(())
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
    };
    let (ping, ping_source) = calloop::ping::make_ping()?;
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    renderer.set_first_frame_wait(FIRST_FRAME_TEXT_WAIT);
    let (to_logic, from_main) = calloop::channel::channel::<ToLogic>();
    let host = Host::new(renderer, log.damage).forwarding(to_logic.clone());
    let mut mgr = SurfaceManager::connect(host, Config::default())?;
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| {
            state.host_mut().renderer.update();
            state.poll();
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
    drop(compiler);
    match (end, joined) {
        (_, Err(_)) => Err(DemoError::Logic("panicked".into())),
        (End::Failed(e), _) => Err(e),
        (_, Ok(Err(e))) => Err(DemoError::Logic(e)),
        (End::LogicEnded, Ok(Ok(()))) => Err(DemoError::Logic("ended".into())),
        (End::Done, Ok(Ok(()))) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_compiler::instantiate::SceneMirror;
    use strand_compiler::reconcile::loader::Loader;
    use strand_compiler::schema::Schema;
    use strand_scene::{Prop, PropValue};

    fn screen(id: &str, name: &str) -> ScreenInfo {
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
        let out = Loader::new(dir, Schema::builtin().clone(), None).boot();
        assert_eq!(out.errors(), 0, "{:?}", out.diagnostics);
        out
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("strand-run-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The scene as the main thread sees it, fed from the diff channel.
    struct Mirror {
        inbox: std::sync::mpsc::Receiver<SceneDiff>,
        scene: SceneMirror,
    }

    impl Mirror {
        /// A calloop channel is read through a loop: one on a thread of
        /// its own hands the diffs to a plain receiver.
        fn new(rx: Channel<SceneDiff>) -> Self {
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
            Self {
                inbox,
                scene: SceneMirror::new(),
            }
        }

        /// Apply diffs until `done` holds (10 s at most).
        fn until(&mut self, what: &str, done: impl Fn(&SceneMirror) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(&self.scene) {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.inbox.recv_timeout(left) {
                    Ok(diff) => {
                        let had = !self.scene.roots().is_empty();
                        self.scene.apply(&diff).unwrap();
                        // No blank frame: once something shows, a diff
                        // never leaves nothing.
                        assert!(
                            !had || !self.scene.roots().is_empty(),
                            "{what}: a blank frame"
                        );
                    }
                    Err(_) => panic!("{what}:\n{}", self.scene.render()),
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
    /// messages and each gets its bar; surface input and sizes reach it;
    /// an unplugged monitor's bar is parked and comes back with its
    /// state, and once forgotten comes back fresh; `Shutdown` ends the
    /// thread after the persisted state reached the disk.
    #[test]
    fn the_logic_thread_follows_the_monitors() {
        let dir = temp_dir("monitors");
        std::fs::write(
            dir.join("bar.strand"),
            "state total = 0 persist\nbar Top {\n  state n = 0\n  opacity: hover ? 0.5 : 1\n  height: self.width > 1000 ? 40 : 32\n  when pressed { opacity: 0.25 }\n  on click {\n    n += 1\n    total += 1\n  }\n  text join(\" \", screen.name, n, total)\n}\n",
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
        m.until("A's bar", |s| s.texts() == ["DP-1 0 0"]);
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
        send(ToLogic::Event {
            node: a,
            name: "click",
            args: Vec::new(),
        });
        m.until("a click", |s| s.texts() == ["DP-1 1 1"]);
        let two = || vec![screen("A", "DP-1"), screen("B", "HDMI-A-1")];
        send(ToLogic::Screens(two()));
        m.until("B's bar", |s| s.roots().len() == 2);
        let b = *m.scene.roots().iter().find(|&&r| r != a).unwrap();
        send(ToLogic::Event {
            node: b,
            name: "click",
            args: Vec::new(),
        });
        m.until("a click on B", |s| {
            let mut t = s.texts();
            t.sort();
            t == ["DP-1 1 2", "HDMI-A-1 1 2"]
        });
        // B unplugged: its bar is parked; back within 30 s, with its `n`.
        send(ToLogic::Screens(vec![screen("A", "DP-1")]));
        m.until("B parked", |s| s.roots().len() == 1);
        send(ToLogic::Screens(two()));
        m.until("B back", |s| s.roots().len() == 2);
        assert_eq!(m.texts(), ["DP-1 1 2", "HDMI-A-1 1 2"]);
        // Unplugged and forgotten: B comes back fresh (the top-level
        // `total` stays).
        send(ToLogic::Screens(vec![screen("A", "DP-1")]));
        m.until("B parked again", |s| s.roots().len() == 1);
        send(ToLogic::Forget("B".into()));
        send(ToLogic::Screens(two()));
        m.until("B fresh", |s| s.roots().len() == 2);
        assert_eq!(m.texts(), ["DP-1 1 2", "HDMI-A-1 0 2"]);
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
                name: "click",
                args: Vec::new(),
            })
            .unwrap();
        m.until("a click", |s| s.texts() == ["n 1"]);
        // A watcher.
        let watch = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let ok = ipc::request(&watch, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
        assert_eq!(ok["ok"], true);
        let mut events = std::io::BufReader::new(watch.try_clone().unwrap());
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
        // `strand reload`: answered with its event once done.
        let client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let ans = ipc::request(
            &client,
            &ipc::Request::Reload { hard: false },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["ok"], true, "{ans}");
        assert_eq!(ans["event"]["requested"], true, "{ans}");
        // `--hard`: state dropped, every surface recreated.
        let ans = ipc::request(
            &client,
            &ipc::Request::Reload { hard: true },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ans["event"]["classes"], json!(["hard"]), "{ans}");
        m.until("a fresh bar", |s| s.texts().contains(&"n 0".to_string()));
        to_logic.send(ToLogic::Shutdown).unwrap();
        assert_eq!(t.join().unwrap(), Ok(()));
        drop(compiler);
        assert!(!socket.exists(), "the socket is removed at exit");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The scene as text with surfaces in a fixed order.
    fn canonical(scene: &SceneMirror) -> String {
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
}

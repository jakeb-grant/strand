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
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use calloop::channel::{Channel, Event, Sender};
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use rustix::time::Timespec;
use strand_compiler::SourceMap;
use strand_compiler::diagnostic::{Style, render};
use strand_compiler::instantiate::{Instance, NodeFlag, Storage};
use strand_compiler::lower::{self, Program};
use strand_compiler::source::find_files;
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
use crate::logging::LogConfig;

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

/// Read and compile the config in `dir`: the program, or the rendered
/// diagnostics.
pub fn compile(dir: &Path, style: Style) -> Result<Program, String> {
    let found = find_files(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    if found.files.is_empty() {
        return Err(format!("no .strand files in {}", dir.display()));
    }
    let mut map = SourceMap::new();
    for path in &found.files {
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        map.add(path.display().to_string(), src);
    }
    let compiled = strand_compiler::compile(&map);
    if compiled.errors() > 0 {
        return Err(render(&compiled.diagnostics, &map, style));
    }
    if !compiled.diagnostics.is_empty() {
        log::warn!("{}", render(&compiled.diagnostics, &map, Style::Plain));
    }
    Ok(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ))
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

/// Apply one message from the main thread.
fn handle(inst: &Instance, host: &SchemaHost, msg: ToLogic) {
    match msg {
        ToLogic::Screens(list) => set_screens(inst.runtime(), host, &list),
        ToLogic::Forget(id) => {
            inst.forget_screen(&id);
        }
        ToLogic::Event { node, name, args } => {
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
    fn new(rx: Channel<ToLogic>) -> io::Result<(Self, calloop::ping::Ping)> {
        let event_loop = EventLoop::<Inbox>::try_new().map_err(io::Error::other)?;
        let handle = event_loop.handle();
        handle
            .insert_source(rx, |event, _, inbox| match event {
                Event::Msg(m) => inbox.msgs.push(m),
                Event::Closed => inbox.closed = true,
            })
            .map_err(|e| io::Error::other(e.error))?;
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

/// The logic thread: mount `program`, then step until
/// [`ToLogic::Shutdown`] (or until the main thread is gone), then
/// unmount and drop the stores so pending writes reach the disk.
pub fn logic(
    program: Arc<Program>,
    storage: Storage,
    rx: Channel<ToLogic>,
    out: Sender<SceneDiff>,
) -> Result<(), String> {
    let (mut sleeper, ping) = Sleeper::new(rx).map_err(|e| format!("logic loop: {e}"))?;
    let rt = Runtime::new();
    rt.set_wake_hook(move || ping.ping());
    let host = Rc::new(SchemaHost::real(&rt, &program.types));
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
    let mut inst = Instance::new(&rt, program, host.clone(), storage);
    let start = Instant::now();
    while !stop {
        let wall = SystemTime::now();
        let (update, wake) = inst.step(start.elapsed(), wall);
        for e in &update.errors {
            log::error!("{e}");
        }
        for d in &update.diagnostics {
            log::warn!("{d:?}");
        }
        for n in &update.notices {
            log::info!("{n}");
        }
        if !update.diff.is_empty() && out.send(update.diff).is_err() {
            break;
        }
        let timeout = wake.deadline.map(|d| d.saturating_sub(start.elapsed()));
        sleeper
            .sleep(timeout, wake.wall, wall, &mut inbox)
            .map_err(|e| format!("logic loop: {e}"))?;
        for m in inbox.msgs.drain(..) {
            if m == ToLogic::Shutdown {
                stop = true;
                break;
            }
            handle(&inst, &host, m);
        }
        stop |= inbox.closed;
    }
    // Unmount: debounced `persist` writes and settings write-outs are
    // flushed as their cells go; the runtime's shutdown waits for the
    // persist queue (bounded), and the last store handle joins its IO
    // thread.
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
    let program = Arc::new(compile(dir, Style::Plain).map_err(DemoError::Logic)?);
    // Before any thread starts, so every thread has the signals blocked.
    let signals = signal_fd()?;
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
    let logic = std::thread::Builder::new()
        .name("strand-logic".into())
        .spawn(move || logic(program, storage, from_main, tx))?;
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
    use std::path::PathBuf;
    use strand_compiler::instantiate::SceneMirror;
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
                    Ok(diff) => self.scene.apply(&diff).unwrap(),
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
        let program = Arc::new(compile(&dir, Style::Plain).unwrap());
        let storage = Storage::in_dirs(dir.join("state"), &dir);
        let persist = storage.persist.clone().unwrap();
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || logic(program, storage, from_main, tx));
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

    /// The main thread gone without a word (every sender dropped) ends
    /// the logic thread too: the runtime's wake hook holds no sender.
    #[test]
    fn the_logic_thread_ends_with_the_main_thread() {
        let dir = temp_dir("orphan");
        std::fs::write(dir.join("bar.strand"), "bar Top { text \"hi\" }\n").unwrap();
        let program = Arc::new(compile(&dir, Style::Plain).unwrap());
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let t = std::thread::spawn(move || logic(program, Storage::none(), from_main, tx));
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

//! Save → pixels on a real compositor (design.md, "Live reload": at p95
//! a token edit shows within 35 ms of save, a markup edit within 50 ms,
//! portal or monitor changes on the next frame; the M1 exit gate "under
//! 50 ms from save to pixels").
//!
//! `strand run`'s threads (watcher, compiler worker, logic thread, text
//! worker, renderer and surface manager) run against a headless sway at
//! 2560×1440, 60 Hz. Each edit is saved in place; the time runs from just
//! before the write to the `wp_presentation_feedback.presented`
//! timestamp of the first frame that carries it (painted with its text
//! shaped), both on `CLOCK_MONOTONIC`. A monitor plugged in is timed from
//! the moment the main thread hears of it (`wl_output.done`) to the
//! presentation of its bar's first frame, which must come within two
//! refresh intervals: the layer surface's configure round trip, then the
//! next frame (`run.rs::a_monitor_change_is_in_the_next_diff` shows the
//! logic thread answers in its first diff). Portal changes are not wired
//! into `strand run` yet (the `system` service, M2).
//!
//! A budget test: ignored in debug builds, run optimised in CI
//! (`cargo test --release -p strand --bin strand reload_latency`).
//! Skipped when sway is not installed, unless `STRAND_REQUIRE_SWAY` is
//! set (CI). `STRAND_LATENCY_ROUNDS` sets the edits per kind (20).

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::channel::Event;
use strand_compiler::instantiate::Storage;
use strand_render::{Renderer, TextBackend};
use strand_scene::{SceneDiff, SceneOp, SurfaceId};
use strand_surface::{Config, FrameClock, Presentation, PresentationClock, SurfaceManager};
use strand_text::{FontConfig, TextWorker};

use crate::demo::apply;
use crate::demo::host::{Host, Probe, ProbeHandle};
use crate::live::Worker;
use crate::run::tests::p95;
use crate::run::{Live, ToLogic, logic};

/// A headless sway of the test's own, killed on drop.
struct Sway {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
    ipc: PathBuf,
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Sway {
    fn start(tag: &str) -> Option<Sway> {
        for tool in ["sway", "swaymsg"] {
            if Command::new(tool).arg("--version").output().is_err() {
                assert!(
                    std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                    "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
                );
                eprintln!(
                    "\n*** SKIPPED: {tool} is not installed; the latency gate did not run ***\n"
                );
                return None;
            }
        }
        let dir = std::env::temp_dir().join(format!("strand-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 2560x1440 position 0 0 scale 1\n",
        )
        .unwrap();
        let log = std::fs::File::create(dir.join("sway.log")).unwrap();
        let child = Command::new("sway")
            .arg("-c")
            .arg(&cfg)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("SWAYSOCK")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut sway = Sway {
            child,
            dir: dir.clone(),
            socket: PathBuf::new(),
            ipc: PathBuf::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let display = names
                .iter()
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
            if let (Some(d), Some(i)) = (display, ipc) {
                sway.socket = dir.join(d);
                sway.ipc = dir.join(i);
                if sway.msg(&["-t", "get_version"]).is_some() {
                    return Some(sway);
                }
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn msg(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &self.ipc)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// `CLOCK_MONOTONIC`, the presentation clock sway announces.
fn monotonic() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

/// Which diff starts the clock's watch for the frame that carries it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Await {
    Nothing,
    /// The token table swapped.
    Tokens,
    /// A node created or removed.
    Markup,
}

/// The frame being waited for, shared by the diff handler, the painter
/// and the frame clock (all on the main thread).
#[derive(Debug)]
struct Watch {
    awaiting: Await,
    /// The next frame painted with damage and all text shaped counts.
    armed: bool,
    /// A monitor change arms it (set before the change is made).
    on_monitor: bool,
    /// Then only a surface never painted before counts.
    new_surface: bool,
    known: Vec<SurfaceId>,
    /// When the main thread heard of the monitor.
    monitor_at: Option<Duration>,
    /// The counted frame, painted: waiting for its presentation.
    painted: Option<SurfaceId>,
    presented: Option<Presentation>,
    discarded: u32,
}

#[derive(Clone)]
struct Shared(Rc<RefCell<Watch>>);

impl Probe for Shared {
    fn painted(&self, surface: SurfaceId, drew: bool, renderer: &Renderer) {
        let mut w = self.0.borrow_mut();
        let new = !w.known.contains(&surface);
        if new {
            w.known.push(surface);
        }
        if w.armed && drew && !renderer.text_pending() && (!w.new_surface || new) {
            w.armed = false;
            w.painted = Some(surface);
        }
    }

    fn monitor(&self) {
        let mut w = self.0.borrow_mut();
        if w.on_monitor {
            w.on_monitor = false;
            w.monitor_at = Some(monotonic());
            w.armed = true;
        }
    }
}

/// The real clock, reporting presentations to the watch.
struct Clock {
    inner: PresentationClock,
    watch: Shared,
}

impl FrameClock for Clock {
    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn set_clock_id(&mut self, clk_id: u32) {
        assert_eq!(clk_id, 1, "sway's presentation clock is CLOCK_MONOTONIC");
        self.inner.set_clock_id(clk_id);
    }

    fn presented(&mut self, surface: SurfaceId, presentation: Presentation) {
        self.inner.presented(surface, presentation);
        let mut w = self.watch.0.borrow_mut();
        if w.painted == Some(surface) {
            w.painted = None;
            w.presented = Some(presentation);
        }
    }

    fn discarded(&mut self, surface: SurfaceId) {
        self.inner.discarded(surface);
        let mut w = self.watch.0.borrow_mut();
        if w.painted == Some(surface) {
            // Never shown: the next frame of it carries the change.
            w.painted = None;
            w.armed = true;
            w.discarded += 1;
        }
    }

    fn predict(&self, surface: SurfaceId) -> Duration {
        self.inner.predict(surface)
    }

    fn forget(&mut self, surface: SurfaceId) {
        self.inner.forget(surface);
    }
}

/// The hello bar of the painted-buffer benchmark: a token for its
/// colour, a node to add and remove.
fn bar_src(bg: &str, extra: bool) -> String {
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
}

/// Milliseconds from `from` to the presentation.
fn since(p: &Presentation, from: Duration) -> f64 {
    p.time.saturating_sub(from).as_secs_f64() * 1e3
}

struct Measured {
    tokens: Vec<f64>,
    markup: Vec<f64>,
    monitors: Vec<f64>,
    refresh: Duration,
    discarded: u32,
}

fn measure(rounds: usize) -> Option<Measured> {
    let sway = Sway::start("latency")?;
    let shm = PathBuf::from("/dev/shm");
    let root = if shm.is_dir() {
        shm
    } else {
        std::env::temp_dir()
    }
    .join(format!("strand-latency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("bar.strand");
    std::fs::write(&file, bar_src("#204080", false)).unwrap();

    // `strand run`'s threads, as `run::run` starts them.
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(&root, None, wtx).unwrap();
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket: None,
    };
    let (ping, ping_source) = calloop::ping::make_ping().unwrap();
    let font = std::fs::read(strand_text::test_font_path()).unwrap();
    let worker = TextWorker::spawn_with_waker(
        FontConfig::isolated(vec![std::sync::Arc::new(font)]),
        Some(Box::new(move || ping.ping())),
    )
    .unwrap();
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    renderer.set_first_frame_wait(crate::demo::FIRST_FRAME_TEXT_WAIT);
    let watch = Shared(Rc::new(RefCell::new(Watch {
        awaiting: Await::Nothing,
        armed: true,
        on_monitor: false,
        new_surface: false,
        known: Vec::new(),
        monitor_at: None,
        painted: None,
        presented: None,
        discarded: 0,
    })));
    let (to_logic, from_main) = calloop::channel::channel::<ToLogic>();
    let mut host = Host::new(renderer, false).forwarding(to_logic.clone());
    host.probe = Some(ProbeHandle(Rc::new(watch.clone())));
    let conn = wayland_client::Connection::from_socket(UnixStream::connect(&sway.socket).unwrap())
        .unwrap();
    let config = Config {
        clock: Box::new(Clock {
            inner: PresentationClock::new(),
            watch: watch.clone(),
        }),
        ..Config::default()
    };
    let mut mgr = SurfaceManager::with_connection(conn, host, config).unwrap();
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| {
            state.host_mut().renderer.update();
            state.poll();
        })
        .unwrap();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    let logic = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
    let w = watch.clone();
    handle
        .insert_source(rx, move |event, _, state| {
            if let Event::Msg(diff) = event {
                {
                    let mut w = w.0.borrow_mut();
                    let hit = match w.awaiting {
                        Await::Nothing => false,
                        Await::Tokens => diff
                            .ops
                            .iter()
                            .any(|o| matches!(o, SceneOp::SetTokens { .. })),
                        Await::Markup => diff
                            .ops
                            .iter()
                            .any(|o| matches!(o, SceneOp::Create { .. } | SceneOp::Remove { .. })),
                    };
                    if hit {
                        w.awaiting = Await::Nothing;
                        w.armed = true;
                    }
                }
                apply(state, diff);
            }
        })
        .unwrap();
    // Dispatch until the watched frame is presented.
    let presented = |mgr: &mut SurfaceManager<Host>, what: &str| -> Presentation {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(p) = watch.0.borrow_mut().presented.take() {
                return p;
            }
            assert!(Instant::now() < deadline, "{what}: no frame presented");
            mgr.dispatch(Some(Duration::from_millis(5))).unwrap();
        }
    };
    let idle = |mgr: &mut SurfaceManager<Host>, d: Duration| {
        let until = Instant::now() + d;
        while Instant::now() < until {
            mgr.dispatch(Some(until.saturating_duration_since(Instant::now())))
                .unwrap();
        }
    };
    let first = presented(&mut mgr, "the boot");
    let refresh = first.refresh.unwrap_or(Duration::from_micros(16_667));
    idle(&mut mgr, Duration::from_millis(300));

    let (mut tokens, mut markup, mut monitors) = (Vec::new(), Vec::new(), Vec::new());
    let mut extra = false;
    for i in 1..=rounds {
        // A token edit: the bar's colour.
        let bg = format!("#{:02x}4080", (i * 7) % 256);
        watch.0.borrow_mut().awaiting = Await::Tokens;
        let saved = monotonic();
        std::fs::write(&file, bar_src(&bg, extra)).unwrap();
        let p = presented(&mut mgr, "a token edit");
        tokens.push(since(&p, saved));
        // Let the colour's transition finish before the next save.
        idle(&mut mgr, Duration::from_millis(250));
        // A markup edit: a node added or removed.
        extra = !extra;
        watch.0.borrow_mut().awaiting = Await::Markup;
        let saved = monotonic();
        std::fs::write(&file, bar_src(&bg, extra)).unwrap();
        let p = presented(&mut mgr, "a markup edit");
        markup.push(since(&p, saved));
        idle(&mut mgr, Duration::from_millis(250));
    }
    // Monitors plugged in: each gets its bar on its first frame.
    for k in 0..5 {
        {
            let mut w = watch.0.borrow_mut();
            w.on_monitor = true;
            w.new_surface = true;
            w.monitor_at = None;
        }
        sway.msg(&["create_output"]).unwrap();
        let p = presented(&mut mgr, &format!("monitor {k}"));
        let heard = watch.0.borrow().monitor_at.unwrap();
        monitors.push(since(&p, heard));
        idle(&mut mgr, Duration::from_millis(200));
    }
    let discarded = watch.0.borrow().discarded;
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(logic.join().unwrap(), Ok(()));
    drop(mgr);
    drop(compiler);
    drop(sway);
    let _ = std::fs::remove_dir_all(&root);
    Some(Measured {
        tokens,
        markup,
        monitors,
        refresh,
        discarded,
    })
}

/// The M1 exit gate on a compositor: p95 save → presented frame within
/// 35 ms for a token edit and 50 ms for a markup edit, and a monitor's
/// bar on the next frame.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "a budget test: run optimised (cargo test --release)"
)]
fn reload_latency_to_the_presented_frame() {
    let rounds = std::env::var("STRAND_LATENCY_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let Some(m) = measure(rounds) else {
        return;
    };
    let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
    let (pt, pm, pmon) = (p95(&m.tokens), p95(&m.markup), p95(&m.monitors));
    eprintln!(
        "save → presented over {rounds} edits each at {:.2} ms refresh: token p95 {pt:.1} ms (max {:.1}), markup p95 {pm:.1} ms (max {:.1}; a node added p95 {:.1}, removed {:.1}); monitor plugged → its bar presented {:?} ms; {} frames discarded",
        m.refresh.as_secs_f64() * 1e3,
        max(&m.tokens),
        max(&m.markup),
        p95(&m.markup.iter().copied().step_by(2).collect::<Vec<_>>()),
        p95(&m
            .markup
            .iter()
            .copied()
            .skip(1)
            .step_by(2)
            .collect::<Vec<_>>()),
        m.monitors
            .iter()
            .map(|x| (x * 10.0).round() / 10.0)
            .collect::<Vec<_>>(),
        m.discarded,
    );
    assert!(pt <= 35.0, "token edits: p95 {pt:.1} ms: {:?}", m.tokens);
    assert!(pm <= 50.0, "markup edits: p95 {pm:.1} ms: {:?}", m.markup);
    // The next frame: a new bar needs one configure round trip before it
    // may commit, then shows at the next refresh; two refresh intervals
    // (33.3 ms at 60 Hz) leave no room for a frame of its own lost.
    let frame = m.refresh.as_secs_f64() * 1e3;
    assert!(
        pmon <= 2.0 * frame,
        "monitors: p95 {pmon:.1} ms (two frames are {:.1} ms): {:?}",
        2.0 * frame,
        m.monitors
    );
}

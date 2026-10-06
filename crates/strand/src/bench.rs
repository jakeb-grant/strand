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
//! shaped, and painted before that presentation), both on
//! `CLOCK_MONOTONIC`. Headless sway presents a commit at once (the
//! painted → presented gap is printed); a monitor waits for its vblank,
//! so the gates apply to each sample plus a vblank wait drawn over one
//! refresh ([`on_a_monitor`]): a model (the exact p95 of the headless
//! sample plus a uniform vblank phase), not a measurement on hardware. The worst phase
//! (each sample plus a whole refresh) is printed beside it. The edits are
//! made on an idle surface (no frame callback pending). A busy surface
//! (an animation running) is not measured: M1's renderer animates
//! nothing (springs land in M2), and headless sway answers a frame
//! callback at once, so the wait a monitor adds there (the pending
//! callback's vblank, then the next) cannot be seen here
//! (`docs/m1-report.md`).
//!
//! Monitor changes: a scale change (`swaymsg output … scale`) is timed
//! from the main thread hearing of it (`wl_output.done`) to the
//! presentation of the bar's first frame at the new scale, within one
//! refresh; a monitor plugged in, from the main thread hearing of it
//! (`wl_output.done`) to its bar's first painted frame, within one
//! refresh: the logic thread's answer (the new bar's diff), the main
//! thread applying it, the layer surface's creation and its configure
//! round trip, and the paint (heard → configured is printed apart). The
//! first plug, an output of a width never seen, also waits for the bar's
//! text to be shaped for that width: it is timed and printed, and the
//! five plugs after it are gated.
//! Portal changes are not wired into `strand run` yet (the `system`
//! service, M2).
//!
//! A budget test: ignored in debug builds, run optimised in CI
//! (`cargo test --release -p strand --bin strand reload_latency --
//! --test-threads=1`). Skipped when sway is not installed, unless
//! `STRAND_REQUIRE_SWAY` is set (CI). `STRAND_LATENCY_ROUNDS` sets the
//! edits per kind (20).

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
use strand_scene::{Scale, SceneDiff, SceneOp, SurfaceId};
use strand_surface::{Config, FrameClock, Presentation, PresentationClock, SurfaceManager};
use strand_text::{FontConfig, TextWorker};

use crate::demo::apply;
use crate::demo::host::{Host, Probe, ProbeHandle};
use crate::live::Worker;
use crate::run::tests::p95;
use crate::run::{Live, ToLogic, logic};

/// A headless sway of the test's own, killed on drop (also the reload
/// fuzzer's).
pub(crate) struct Sway {
    child: Child,
    dir: PathBuf,
    pub(crate) socket: PathBuf,
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
    /// `None` (the test is skipped, and says so) when sway is not
    /// installed, unless `STRAND_REQUIRE_SWAY` is set.
    pub(crate) fn start(tag: &str) -> Option<Sway> {
        for tool in ["sway", "swaymsg"] {
            if Command::new(tool).arg("--version").output().is_err() {
                assert!(
                    std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                    "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
                );
                eprintln!(
                    "\n*** SKIPPED: {tool} is not installed; the {tag} test did not run ***\n"
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

    pub(crate) fn msg(&self, args: &[&str]) -> Option<String> {
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
    /// A monitor plugged in arms it (set before the plug).
    on_monitor: bool,
    /// Then only a surface never painted before counts.
    new_surface: bool,
    /// A scale change: only a frame painted at this scale counts (its
    /// text may still be a resampled stand-in: the change is shown).
    scale: Option<Scale>,
    known: Vec<SurfaceId>,
    /// When the main thread heard of the monitor.
    monitor_at: Option<Duration>,
    /// When the new surface was first configured (the layer surface's
    /// configure round trip over).
    configured_at: Option<Duration>,
    /// The counted frame and when it was painted: waiting for its
    /// presentation.
    painted: Option<(SurfaceId, Duration)>,
    /// Its presentation, and when it was painted.
    presented: Option<(Presentation, Duration)>,
    discarded: u32,
    /// A counted frame discarded: the surface to paint again (nothing
    /// else may dirty it while the bench idles).
    repaint: Option<SurfaceId>,
    /// A plugged monitor's new surface whose counted frame was discarded:
    /// its next frame counts, though it is no longer new.
    recount: Option<SurfaceId>,
    /// Presentations of the counted surface timed before its counted
    /// frame was painted (an older frame's), not taken for it.
    stale: u32,
}

#[derive(Clone)]
struct Shared(Rc<RefCell<Watch>>);

impl Probe for Shared {
    fn painted(&self, surface: SurfaceId, drew: bool, scale: Scale, renderer: &Renderer) {
        let mut w = self.0.borrow_mut();
        let new = !w.known.contains(&surface);
        if new {
            w.known.push(surface);
        }
        let shown = match w.scale {
            Some(s) => s == scale,
            None => !renderer.text_pending(),
        };
        let again = w.recount == Some(surface);
        if w.armed && drew && shown && (!w.new_surface || new || again) {
            w.armed = false;
            w.recount = None;
            w.painted = Some((surface, monotonic()));
        }
    }

    fn configured(&self, surface: SurfaceId) {
        let mut w = self.0.borrow_mut();
        if w.new_surface && !w.known.contains(&surface) && w.configured_at.is_none() {
            w.configured_at = Some(monotonic());
        }
    }

    fn monitor(&self) {
        let mut w = self.0.borrow_mut();
        if w.monitor_at.is_none() {
            w.monitor_at = Some(monotonic());
        }
        if w.on_monitor {
            w.on_monitor = false;
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
        if let Some((s, at)) = w.painted
            && s == surface
        {
            if presentation.time < at {
                // An older frame of the surface: the counted one is
                // still to come.
                w.stale += 1;
            } else {
                w.painted = None;
                w.presented = Some((presentation, at));
            }
        }
    }

    fn discarded(&mut self, surface: SurfaceId) {
        self.inner.discarded(surface);
        let mut w = self.watch.0.borrow_mut();
        if w.painted.is_some_and(|(s, _)| s == surface) {
            // Never shown: the next frame of it carries the change.
            w.painted = None;
            w.armed = true;
            w.discarded += 1;
            w.repaint = Some(surface);
            if w.new_surface {
                w.recount = Some(surface);
            }
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

/// A monitor change in two steps: until the shell painted its first
/// frame showing it, then until the compositor presented that frame.
#[derive(Clone, Copy, Debug)]
struct Steps {
    to_paint: f64,
    to_present: f64,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// The p95 the same samples would have on a real output: headless sway
/// presents a commit at once (the paint → presented gap is measured),
/// where a monitor shows it at its next vblank, 0 to one refresh later
/// with the save's phase uniform against the vblank. The p95 of that
/// mixture (each sample plus `U(0, refresh)`) is computed exactly: the
/// `x` at which the mean of the samples' uniform CDFs reaches 0.95,
/// found by bisection. It moves with the samples: `on_a_monitor(s + d)`
/// is `on_a_monitor(s) + d`.
fn on_a_monitor(samples: &[f64], refresh: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let below = |x: f64| {
        samples
            .iter()
            .map(|s| ((x - s) / refresh).clamp(0.0, 1.0))
            .sum::<f64>()
            / samples.len() as f64
    };
    let lo = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max) + refresh;
    let (mut lo, mut hi) = (lo, hi);
    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        if below(mid) >= 0.95 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

struct Measured {
    tokens: Vec<f64>,
    markup: Vec<f64>,
    /// Painted → presented, for each edit's counted frame.
    gaps: Vec<f64>,
    /// A monitor plugged in: heard → its new surface configured (the
    /// logic thread's answer, the surface's creation and its configure
    /// round trip), then heard → its bar painted → presented.
    configured: Vec<f64>,
    plugs: Vec<Steps>,
    /// The first plug, of a new output width (its text shaped anew).
    first_plug: Option<Steps>,
    /// A scale change: heard → the bar painted at the new scale →
    /// presented.
    scales: Vec<Steps>,
    refresh: Duration,
    discarded: u32,
    stale: u32,
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
        scale: None,
        known: Vec::new(),
        monitor_at: None,
        configured_at: None,
        painted: None,
        presented: None,
        discarded: 0,
        repaint: None,
        recount: None,
        stale: 0,
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
    // Dispatch until the watched frame is presented: the presentation
    // and when the frame was painted.
    let presented = |mgr: &mut SurfaceManager<Host>, what: &str| -> (Presentation, Duration) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(p) = watch.0.borrow_mut().presented.take() {
                return p;
            }
            // A counted frame the compositor discarded: paint it again.
            let again = watch.0.borrow_mut().repaint.take();
            if let Some(surface) = again {
                mgr.state_mut().repaint(surface);
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
    let (first, _) = presented(&mut mgr, "the boot");
    let refresh = first.refresh.unwrap_or(Duration::from_micros(16_667));
    idle(&mut mgr, Duration::from_millis(300));

    let (mut tokens, mut markup, mut gaps) = (Vec::new(), Vec::new(), Vec::new());
    let mut extra = false;
    for i in 1..=rounds {
        // A token edit: the bar's colour.
        let bg = format!("#{:02x}4080", (i * 7) % 256);
        watch.0.borrow_mut().awaiting = Await::Tokens;
        let saved = monotonic();
        std::fs::write(&file, bar_src(&bg, extra)).unwrap();
        let (p, painted) = presented(&mut mgr, "a token edit");
        tokens.push(since(&p, saved));
        gaps.push(since(&p, painted));
        // Idle before the next save: no frame callback pending.
        idle(&mut mgr, Duration::from_millis(250));
        // A markup edit: a node added or removed.
        extra = !extra;
        watch.0.borrow_mut().awaiting = Await::Markup;
        let saved = monotonic();
        std::fs::write(&file, bar_src(&bg, extra)).unwrap();
        let (p, painted) = presented(&mut mgr, "a markup edit");
        markup.push(since(&p, saved));
        gaps.push(since(&p, painted));
        idle(&mut mgr, Duration::from_millis(250));
    }
    // The output's scale changed: the bar is shown at the new scale.
    let mut scales: Vec<Steps> = Vec::new();
    for (k, s) in [180u32, 120, 180, 120].into_iter().enumerate() {
        {
            let mut w = watch.0.borrow_mut();
            w.scale = Scale::new(s);
            w.monitor_at = None;
            w.armed = true;
        }
        let factor = format!("{}", s as f64 / 120.0);
        sway.msg(&["output", "HEADLESS-1", "scale", &factor])
            .unwrap();
        let (p, painted) = presented(&mut mgr, &format!("scale change {k}"));
        let heard = watch.0.borrow().monitor_at.expect("the scale change heard");
        scales.push(Steps {
            to_paint: ms(painted.saturating_sub(heard)),
            to_present: since(&p, painted),
        });
        idle(&mut mgr, Duration::from_millis(200));
    }
    watch.0.borrow_mut().scale = None;
    // Monitors plugged in: each gets its bar on its first frame. The
    // first output of a width never seen (sway's new outputs are 1920
    // wide, the first 2560) also waits for its bar's text to be shaped
    // for that width: timed and printed, not gated (`first_plug`); the
    // five after it are.
    let (mut configured_after, mut plugs) = (Vec::new(), Vec::new());
    let mut first_plug = None;
    for k in 0..6 {
        {
            let mut w = watch.0.borrow_mut();
            w.on_monitor = true;
            w.new_surface = true;
            w.monitor_at = None;
            w.configured_at = None;
        }
        sway.msg(&["create_output"]).unwrap();
        let (p, painted) = presented(&mut mgr, &format!("monitor {k}"));
        let (heard, configured) = {
            let w = watch.0.borrow();
            (
                w.monitor_at.expect("the plug heard"),
                w.configured_at.expect("the new surface configured"),
            )
        };
        let step = Steps {
            to_paint: ms(painted.saturating_sub(heard)),
            to_present: since(&p, painted),
        };
        if k == 0 {
            first_plug = Some(step);
        } else {
            configured_after.push(ms(configured.saturating_sub(heard)));
            plugs.push(step);
        }
        idle(&mut mgr, Duration::from_millis(200));
    }
    let (discarded, stale) = {
        let w = watch.0.borrow();
        (w.discarded, w.stale)
    };
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(logic.join().unwrap(), Ok(()));
    drop(mgr);
    drop(compiler);
    drop(sway);
    let _ = std::fs::remove_dir_all(&root);
    Some(Measured {
        tokens,
        markup,
        gaps,
        configured: configured_after,
        plugs,
        first_plug,
        scales,
        refresh,
        discarded,
        stale,
    })
}

/// The M1 exit gate on a compositor: p95 save → presented frame within
/// 35 ms for a token edit and 50 ms for a markup edit, as a monitor
/// would show them (each sample plus the vblank wait headless sway does
/// not have), and a monitor change on the next frame.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "a budget test: run optimised (cargo test --release)"
)]
fn reload_latency_to_the_presented_frame() {
    let rounds = std::env::var("STRAND_LATENCY_ROUNDS")
        .ok()
        .map(|n| {
            n.parse()
                .unwrap_or_else(|e| panic!("STRAND_LATENCY_ROUNDS={n:?}: {e}"))
        })
        .unwrap_or(20);
    let Some(m) = measure(rounds) else {
        return;
    };
    let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
    let round = |v: &[f64]| {
        v.iter()
            .map(|x| (x * 10.0).round() / 10.0)
            .collect::<Vec<_>>()
    };
    let frame = ms(m.refresh);
    let (pt, pm) = (p95(&m.tokens), p95(&m.markup));
    let (ht, hm) = (
        on_a_monitor(&m.tokens, frame),
        on_a_monitor(&m.markup, frame),
    );
    // The headless p95 the token gate breaks at: the model moves with
    // the samples, so the gap to 35 ms is the headroom on either scale.
    let token_break = pt + (35.0 - ht);
    let added: Vec<f64> = m.markup.iter().copied().step_by(2).collect();
    let removed: Vec<f64> = m.markup.iter().copied().skip(1).step_by(2).collect();
    let steps = |v: &[Steps]| {
        v.iter()
            .map(|s| format!("{:.1} + {:.1}", s.to_paint, s.to_present))
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!(
        "save → presented over {rounds} edits each at {frame:.2} ms refresh:\n\
         \x20 token p95 {pt:.1} ms (max {:.1}; the gate breaks above {token_break:.1}); on a monitor (model: uniform vblank phase) p95 {ht:.1} ms, worst phase {:.1}\n\
         \x20 markup p95 {pm:.1} ms (max {:.1}; a node added p95 {:.1}, removed {:.1}); on a monitor p95 {hm:.1} ms, worst phase {:.1}\n\
         \x20 painted → presented p95 {:.2} ms (max {:.2}); {} frames discarded, {} stale presentations\n\
         \x20 scale change heard → painted at the new scale + → presented: {} ms\n\
         \x20 monitor plugged: heard → new surface configured (logic answer + surface creation + round trip) {:?} ms; heard → painted + → presented: {} ms\n\
         \x20 first monitor of a new width (not gated: its text shaped for the width): heard → painted + → presented {} ms",
        max(&m.tokens),
        pt + frame,
        max(&m.markup),
        p95(&added),
        p95(&removed),
        pm + frame,
        p95(&m.gaps),
        max(&m.gaps),
        m.discarded,
        m.stale,
        steps(&m.scales),
        round(&m.configured),
        steps(&m.plugs),
        steps(m.first_plug.as_slice()),
    );
    assert!(
        ht <= 35.0,
        "token edits: p95 on a monitor {ht:.1} ms (headless {pt:.1}, the gate breaks above {token_break:.1}: a slow runner shows as every sample a little high, a regression as a step): {:?}",
        m.tokens
    );
    assert!(
        hm <= 50.0,
        "markup edits: p95 on a monitor {hm:.1} ms (headless {pm:.1}): {:?}",
        m.markup
    );
    // The next frame: the first frame the shell paints once it hears of
    // a scale change or a plugged monitor (for the plug: the logic
    // thread's new bar, its layer surface created and configured, then
    // painted) shows the change, within one refresh; a frame lost on the
    // shell's side (painted without the change, or superseded before it
    // was shown) misses by a refresh. That frame is then the one
    // presented: headless sway presents an edit's frame at once, but the
    // first frame after an output change, or on a new output, at its
    // next frame timer (17 ms after the paint for a scale change), so the
    // presentation is held to the compositor's next frame, under two
    // refreshes.
    for (what, v) in [
        ("a scale change", &m.scales),
        ("a plugged monitor", &m.plugs),
    ] {
        let paint = p95(&v.iter().map(|s| s.to_paint).collect::<Vec<_>>());
        let present = p95(&v.iter().map(|s| s.to_present).collect::<Vec<_>>());
        assert!(
            paint <= frame && present < 2.0 * frame,
            "{what}: p95 {paint:.1} ms from hearing of it to the frame showing it (one frame is {frame:.1}), then {present:.1} ms to its presentation: {}",
            steps(v)
        );
    }
}

/// The vblank model is the exact p95 of each sample plus a uniform wait
/// over one refresh, and moves with the samples.
#[test]
fn the_monitor_model_is_the_exact_p95() {
    let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
    assert!(close(on_a_monitor(&[10.0], 16.0), 10.0 + 0.95 * 16.0));
    // Half the mass done by 10, the rest uniform over 100..110.
    assert!(close(on_a_monitor(&[0.0, 100.0], 10.0), 109.0));
    let s = [17.1, 17.9, 18.4, 16.2, 19.0];
    let shifted: Vec<f64> = s.iter().map(|x| x + 1.5).collect();
    assert!(close(
        on_a_monitor(&shifted, 16.667),
        on_a_monitor(&s, 16.667) + 1.5
    ));
}

//! `strand run --demo`: the M0 hello bar, wired the way
//! `docs/architecture.md` describes (logic thread → one `SceneDiff` per tick
//! over a channel → main render + surface thread), without the language.

pub mod clock;
pub mod host;
pub mod logic;
pub mod scene;

use std::cell::Cell;
use std::convert::Infallible;
use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use calloop::channel::Event;
use strand_render::host_loop::{self, Cause, LoopHost, LoopSync};
use strand_render::{Intent, Renderer, Router, TextBackend};
use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneDiff, SceneOp, SurfaceId};
use strand_surface::{Config, State, SurfaceError, SurfaceManager};
use strand_text::{FontConfig, TextError, TextWorker};

use crate::logging::LogConfig;
use host::Host;

#[derive(Debug)]
pub enum DemoError {
    Io(std::io::Error),
    Text(TextError),
    Surface(SurfaceError),
    /// The logic thread ended or failed.
    Logic(String),
    /// What the user asked for cannot run (a config directory that is
    /// not one), said as it is.
    Config(String),
}

impl fmt::Display for DemoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Text(e) => write!(f, "text worker: {e:?}"),
            Self::Surface(e) => write!(f, "{e}"),
            Self::Logic(e) => write!(f, "logic thread: {e}"),
            Self::Config(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for DemoError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<SurfaceError> for DemoError {
    fn from(e: SurfaceError) -> Self {
        Self::Surface(e)
    }
}

/// Apply one logic tick's diff and hand surface changes to the manager
/// (render loop steps 0 and 3; `host_loop::apply`: the router observes
/// it, the renderer applies it, routing settles, surfaces sync).
pub(crate) fn apply(state: &mut State<Host>, mut diff: SceneDiff) {
    // (M6) The popups this diff creates or opens were opened by its press,
    // if it has one: only those grab (`State::set_popup_cause`).
    for (node, press) in popup_causes(&diff) {
        state.set_popup_cause(node, press);
    }
    // (kept-size) A reload committed: what the old config measured goes
    // before the new one is laid out.
    if let Some(config) = diff.config {
        state.host_mut().config_committed(config);
    }
    // (window) What its handlers asked of windows, once the windows it
    // opens are there: a move or resize with its press.
    let (asks, press) = (std::mem::take(&mut diff.window_asks), diff.press);
    let Ok(()) = host_loop::apply(&mut Main(state), diff);
    for (node, ask) in asks {
        state.window_ask(node, ask, press);
    }
}

/// (M6) What `diff` says opened each popup it touches: a popup it creates
/// or sets `open: true` was opened by its press (`SceneDiff::press`,
/// none for a diff no press caused: a hover, a timer); one it closes or
/// removes is opened by nothing. Other surfaces' entries are never read.
pub(crate) fn popup_causes(diff: &SceneDiff) -> Vec<(NodeId, Option<u32>)> {
    diff.ops
        .iter()
        .filter_map(|op| match op {
            SceneOp::Create {
                id,
                kind: NodeKind::Popup,
                ..
            } => Some((*id, diff.press)),
            SceneOp::SetProp {
                id,
                prop: Prop::Open,
                value,
                ..
            } => Some((*id, diff.press.filter(|_| *value == PropValue::Bool(true)))),
            SceneOp::Remove { id, .. } => Some((*id, None)),
            _ => None,
        })
        .collect()
}

/// The text worker delivered layouts, or something else woke the loop:
/// (M6) a hover grace that ended (the renderer's timer woke for it) ticks
/// the router, `Renderer::update` collects and lays out, and what it
/// changed (a content-sized surface's size, laid-out sizes) is handed on
/// (`host_loop::woke`).
pub(crate) fn text_ready(state: &mut State<Host>) {
    let Ok(()) = host_loop::woke(&mut Main(state));
}

/// (eval-host-loop) The main thread's host with its surface manager: the
/// host loop's reactions that hand surface changes over run on it.
pub(crate) struct Main<'a>(pub(crate) &'a mut State<Host>);

impl LoopHost for Main<'_> {
    type Error = Infallible;

    fn parts(&mut self) -> (&mut Renderer, Option<&mut Router>) {
        self.0.host_mut().parts()
    }

    fn shown(&self, out: &mut Vec<SurfaceId>) {
        self.0.host().shown(out);
    }

    fn applied(&mut self, errors: Vec<strand_render::SceneError>) -> Result<(), Infallible> {
        self.0.host_mut().applied(errors)
    }

    fn intents(&mut self, cause: Cause<'_>, intents: Vec<Intent>) -> Result<(), Infallible> {
        self.0.host_mut().intents(cause, intents)
    }

    fn facts(&mut self) -> Result<(), Infallible> {
        self.0.host_mut().facts()
    }

    fn wake(&mut self) -> Result<(), Infallible> {
        LoopHost::wake(self.0.host_mut())
    }
}

impl LoopSync for Main<'_> {
    /// Hands the renderer's surface changes to the surface manager and
    /// its layout facts to logic, then asks every surface for a frame.
    fn sync(&mut self) -> Result<(), Infallible> {
        let state = &mut *self.0;
        let changes = state.host_mut().renderer.take_surface_changes();
        for (node, change) in changes {
            state.apply_surface_change(node, change);
        }
        state.host_mut().forward_facts();
        state.poll();
        Ok(())
    }
}

/// How long a new bar holds its first frame for text being shaped. The
/// renderer's default (50 ms) can run out at boot under load, while the
/// text worker loads fonts; the bar then shows a stand-in layout and
/// repaints its text a moment later. Half a second is still well before
/// anyone looks for the bar.
pub const FIRST_FRAME_TEXT_WAIT: Duration = Duration::from_millis(500);

/// The Wayland connection is gone (the compositor quit or crashed): the
/// normal end of a shell's life, not a failure.
pub(crate) fn connection_closed(e: &SurfaceError) -> bool {
    let SurfaceError::EventLoop(e) = e else {
        return false;
    };
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cause {
        if let Some(io) = c.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        {
            return true;
        }
        cause = c.source();
    }
    false
}

/// Run the demo bar until the compositor goes away (then `Ok`), or until
/// the logic thread fails (then its error).
pub fn run(log: &LogConfig) -> Result<(), DemoError> {
    // Text worker, waking the main loop when layouts arrive (step 1–2).
    let (ping, ping_source) = calloop::ping::make_ping()?;
    let wake = ping.clone();
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let mut renderer = Renderer::try_new(TextBackend::Worker(worker)).map_err(DemoError::Io)?;
    renderer.set_first_frame_wait(FIRST_FRAME_TEXT_WAIT);
    let host = Host::new(renderer, log.damage).waking(wake);
    let mut mgr = SurfaceManager::connect(host, Config::default())?;
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| text_ready(state))
        .map_err(|e| DemoError::Io(std::io::Error::other(e.error)))?;

    // Logic thread → main thread, one diff per tick.
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    let logic = std::thread::Builder::new()
        .name("strand-logic".into())
        .spawn(move || logic::run(tx))?;
    // The sender drops when the logic thread returns or panics, before
    // std marks the thread finished: the loop ends on that, not on
    // `is_finished`, so it never blocks on a loop nothing will wake.
    let hung_up = Rc::new(Cell::new(false));
    let flag = Rc::clone(&hung_up);
    handle
        .insert_source(rx, move |event, _, state| match event {
            Event::Msg(diff) => apply(state, diff),
            Event::Closed => flag.set(true),
        })
        .map_err(|e| DemoError::Io(std::io::Error::other(e.error)))?;

    while !hung_up.get() {
        match mgr.dispatch(None) {
            Ok(()) => {}
            Err(e) if connection_closed(&e) => {
                log::info!("the compositor went away: {e}");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        // (m6-owner-fixes, B-d) The batch's events are in.
        mgr.state_mut().host_mut().settle_rebase();
    }
    // The thread is past its last send: joining does not block for long.
    match logic.join() {
        Ok(Ok(())) => Err(DemoError::Logic("ended".into())),
        Ok(Err(e)) => Err(DemoError::Logic(e.to_string())),
        Err(_) => Err(DemoError::Logic("panicked".into())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use strand_scene::{Damage, PaintTarget, Painter, Scale, Size, SurfaceId};
    use strand_text::{TextEngine, test_font_path};

    use super::*;

    const GATE: u64 = 2000;

    /// The host loop's steps (`strand_render::host_loop`) a host must
    /// not take by hand: routing's, the input wake's, the surfaces' and
    /// the renderer's own.
    const STEPS: [&str; 12] = [
        "router.observe(",
        "router.settle(",
        "router.tick(",
        "router.handle(",
        "router.attached(",
        "router.detached(",
        "set_input_wake(",
        "attach_surface(",
        "detach_surface(",
        "configure_surface(",
        "renderer.apply(",
        "renderer.update()",
    ];

    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// (eval-host-loop) The shell's main thread takes logic's diffs,
    /// wakes, input and its surfaces' attach, configure, detach and
    /// paints through the host loop's reactions, which the evaluation
    /// sandbox's driver runs too (eval/tests/drift.rs holds its side):
    /// no step of them is taken by hand anywhere in the shell's code
    /// outside tests, so the sandbox cannot drift from the shell's order
    /// unseen (docs/decisions.md, eval-host-loop). (eval-r3) A tripwire
    /// on call spellings: an alias of the renderer or router gets past
    /// it, and it holds every non-test file of the crate, so a new
    /// product path that must take a step itself (a headless shot) goes
    /// through a reaction or changes this test with a reason.
    #[test]
    fn the_shell_takes_the_host_loops_steps_only_through_its_reactions() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&src, &mut files);
        files.sort();
        let tests_only = ["bench.rs", "fuzz.rs", "speed.rs", "run/tests.rs"];
        let mut taken = Vec::new();
        for f in &files {
            let rel = f.strip_prefix(&src).unwrap().display().to_string();
            if tests_only.contains(&rel.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(f).unwrap();
            let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
            // A file's code ends where its test module starts.
            let end = lines
                .windows(2)
                .position(|w| w[0] == "#[cfg(test)]" && w[1].starts_with("mod tests"))
                .unwrap_or(lines.len());
            for (i, line) in lines[..end].iter().enumerate() {
                if line.starts_with("//") {
                    continue;
                }
                for step in STEPS.iter().filter(|s| line.contains(*s)) {
                    taken.push(format!("{rel}:{}: {step}", i + 1));
                }
            }
        }
        assert!(
            taken.is_empty(),
            "host loop steps taken by hand (run them through strand_render::host_loop):\n{}",
            taken.join("\n")
        );
        let host = std::fs::read_to_string(src.join("demo/host.rs")).unwrap()
            + &std::fs::read_to_string(src.join("demo/mod.rs")).unwrap();
        for reaction in [
            "apply",
            "woke",
            "input",
            "attached",
            "configured",
            "detached",
            "painted",
        ] {
            assert!(
                host.contains(&format!("host_loop::{reaction}(")),
                "the shell runs no `host_loop::{reaction}`"
            );
        }
    }

    #[test]
    fn a_closed_connection_is_a_normal_end() {
        let gone =
            |kind| SurfaceError::EventLoop(calloop::Error::IoError(std::io::Error::from(kind)));
        assert!(connection_closed(&gone(std::io::ErrorKind::BrokenPipe)));
        assert!(connection_closed(&gone(
            std::io::ErrorKind::ConnectionReset
        )));
        assert!(!connection_closed(&gone(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!connection_closed(&SurfaceError::MissingGlobal("wl_shm")));
    }

    fn renderer() -> Renderer {
        let font = std::fs::read(test_font_path()).unwrap();
        let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
        Renderer::new(TextBackend::Inline(Box::new(engine)))
    }

    fn paint(
        r: &mut Renderer,
        id: SurfaceId,
        px: &mut [u8],
        size: Size,
        scale: Scale,
        age: u8,
    ) -> Damage {
        let mut t = PaintTarget::new(px, size, size.w * 4, scale, age).unwrap();
        r.paint(id, &mut t)
    }

    /// The M0 damage gate offline: on a 2560 px wide output at 1.0 and
    /// 1.25, a minute tick (even one changing every digit and the text
    /// width) repaints at most 2,000 px² once the buffer holds a previous
    /// frame (age 1 after copy-forward, age 2 when two buffers alternate).
    #[test]
    fn a_minute_tick_repaints_at_most_2000_px2() {
        for numerator in [120, 150] {
            let scale = Scale::new(numerator).unwrap();
            let size = Size::new(2560, (scene::HEIGHT * scale.as_f32()).round() as u32);
            let mut r = renderer();
            let mut boot = scene::bar();
            boot.ops.extend(scene::clock("09:58").ops);
            assert!(r.apply(boot).is_empty());
            let id = SurfaceId(1);
            r.attach_surface(id, scene::BAR);
            r.configure_surface(id, size, scale);
            let mut a = vec![0u8; (size.w * size.h * 4) as usize];
            let d = paint(&mut r, id, &mut a, size, scale, 0);
            assert_eq!(d, Damage::full(size));
            // Copy-forward into the second buffer, then a tick.
            let mut b = a.clone();
            assert!(r.apply(scene::clock("09:59")).is_empty());
            let d = paint(&mut r, id, &mut b, size, scale, 1);
            assert!(!d.is_empty());
            assert!(
                d.area() <= GATE,
                "scale {numerator}/120: {} {d:?}",
                d.area()
            );
            // Steady state: buffers alternate, age 2.
            for text in ["10:00", "10:01", "11:11"] {
                assert!(r.apply(scene::clock(text)).is_empty());
                let d = paint(&mut r, id, &mut a, size, scale, 2);
                assert!(
                    d.area() <= GATE,
                    "{text} at {numerator}/120: {} {d:?}",
                    d.area()
                );
                std::mem::swap(&mut a, &mut b);
            }
            // The copy and the alternation leave the same picture as a
            // full repaint of the final scene.
            let mut fresh = vec![0u8; a.len()];
            let mut r2 = renderer();
            let mut boot = scene::bar();
            boot.ops.extend(scene::clock("11:11").ops);
            assert!(r2.apply(boot).is_empty());
            r2.attach_surface(id, scene::BAR);
            r2.configure_surface(id, size, scale);
            paint(&mut r2, id, &mut fresh, size, scale, 0);
            assert!(b == fresh, "incremental frames differ from a full repaint");
        }
    }
}

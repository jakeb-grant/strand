//! `strand run --demo`: the M0 hello bar, wired the way
//! `docs/architecture.md` describes (logic thread → one `SceneDiff` per tick
//! over a channel → main render + surface thread), without the language.

pub mod clock;
pub mod host;
pub mod logic;
pub mod scene;

use std::cell::Cell;
use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use calloop::channel::Event;
use strand_render::{Renderer, TextBackend};
use strand_scene::SceneDiff;
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
}

impl fmt::Display for DemoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Text(e) => write!(f, "text worker: {e:?}"),
            Self::Surface(e) => write!(f, "{e}"),
            Self::Logic(e) => write!(f, "logic thread: {e}"),
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
/// (render loop steps 0 and 3).
pub(crate) fn apply(state: &mut State<Host>, diff: SceneDiff) {
    for error in state.host_mut().renderer.apply(diff) {
        log::error!("scene: {error:?}");
    }
    sync(state);
}

/// The text worker delivered layouts: collect them, and hand on what they
/// changed (a content-sized surface's size, laid-out sizes).
pub(crate) fn text_ready(state: &mut State<Host>) {
    state.host_mut().renderer.update();
    sync(state);
}

/// Hands the renderer's surface changes to the surface manager and its
/// layout facts to logic, then asks every surface for a frame.
fn sync(state: &mut State<Host>) {
    let changes = state.host_mut().renderer.take_surface_changes();
    for (node, change) in changes {
        state.apply_surface_change(node, change);
    }
    state.host_mut().forward_facts();
    state.poll();
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
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    renderer.set_first_frame_wait(FIRST_FRAME_TEXT_WAIT);
    let mut mgr = SurfaceManager::connect(Host::new(renderer, log.damage), Config::default())?;
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

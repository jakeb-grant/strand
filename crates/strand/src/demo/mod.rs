//! `strand run --demo`: the M0 hello bar, wired the way
//! `docs/architecture.md` describes (logic thread → one `SceneDiff` per tick
//! over a channel → main render + surface thread), without the language.

pub mod clock;
pub mod host;
pub mod logic;
pub mod scene;

use std::fmt;

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
fn apply(state: &mut State<Host>, diff: SceneDiff) {
    for error in state.host_mut().renderer.apply(diff) {
        log::error!("scene: {error:?}");
    }
    let changes = state.host_mut().renderer.take_surface_changes();
    for (node, change) in changes {
        state.apply_surface_change(node, change);
    }
    state.poll();
}

/// Run the demo bar until the compositor goes away.
pub fn run(log: &LogConfig) -> Result<(), DemoError> {
    // Text worker, waking the main loop when layouts arrive (step 1–2).
    let (ping, ping_source) = calloop::ping::make_ping()?;
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let renderer = Renderer::new(TextBackend::Worker(worker));
    let mut mgr = SurfaceManager::connect(Host::new(renderer, log.damage), Config::default())?;
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| {
            state.host_mut().renderer.update();
            state.poll();
        })
        .map_err(|e| DemoError::Io(std::io::Error::other(e.error)))?;

    // Logic thread → main thread, one diff per tick.
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    let logic = std::thread::Builder::new()
        .name("strand-logic".into())
        .spawn(move || logic::run(tx))?;
    handle
        .insert_source(rx, |event, _, state| match event {
            Event::Msg(diff) => apply(state, diff),
            Event::Closed => log::error!("logic thread hung up"),
        })
        .map_err(|e| DemoError::Io(std::io::Error::other(e.error)))?;

    loop {
        if logic.is_finished() {
            return match logic.join() {
                Ok(Ok(())) => Err(DemoError::Logic("ended".into())),
                Ok(Err(e)) => Err(DemoError::Logic(e.to_string())),
                Err(_) => Err(DemoError::Logic("panicked".into())),
            };
        }
        mgr.dispatch(None)?;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use strand_scene::{Damage, PaintTarget, Painter, Scale, Size, SurfaceId};
    use strand_text::{TextEngine, test_font_path};

    use super::*;

    const GATE: u64 = 2000;

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

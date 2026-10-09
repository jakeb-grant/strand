//! `reduced_motion` from the language: a theme's `motion { reduced: … }`
//! token, compiled and instantiated, reaches the renderer, which then
//! snaps every spring, glide and pose (design.md, "Layout, animation and
//! input": reduced_motion snaps everything).

mod common;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::Renderer;
use strand_scene::*;

const THEME: &str = "// theme.strand
tokens base {
  motion { spatial: spring(700, 0.9); effects: spring(1600, 1); bouncy: spring(380, 0.75)
           reduced: system.reduced_motion }
}
";

const SHELL: &str = "// shell.strand
panel P {
  width: 100; height: 40; open: true; bg: #1e1e2e
  row {
    box { width: 20; height: 20; bg: #ff0000; x: system.dark ? 50 : 0 }
    if system.dark { box { width: 20; height: 20; bg: #00ff00; enter { opacity: 0 } } }
  }
}
";

const T0: Duration = Duration::from_secs(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    r: Renderer,
    buf: Buffer,
}

impl Shell {
    fn new(reduced: bool) -> Shell {
        Self::with(SHELL, reduced)
    }

    fn with(shell: &str, reduced: bool) -> Shell {
        let mut map = SourceMap::new();
        map.add("theme.strand".to_string(), THEME.to_string());
        map.add("shell.strand".to_string(), shell.to_string());
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
        let program = Arc::new(lower::lower(
            &compiled.program,
            strand_compiler::schema::Schema::builtin(),
        ));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
        host.set(&rt, "system.reduced_motion", Value::Bool(reduced))
            .unwrap();
        host.set(&rt, "system.dark", Value::Bool(false)).unwrap();
        let inst = Instance::new(&rt, program, host.clone(), Storage::none());
        let mut r = renderer();
        assert!(r.apply(inst.flush().diff).is_empty());
        let root = r.tree().surface_nodes().next().unwrap();
        r.attach_surface(SurfaceId(1), root);
        let mut buf = Buffer::new(100, 40, Scale::ONE);
        buf.paint_at(&mut r, SurfaceId(1), 0, T0);
        Shell {
            rt,
            host,
            inst,
            r,
            buf,
        }
    }

    /// Flips `system.dark` (the box moves right, a green box enters) and
    /// returns how many frames it animates for.
    fn flip(&mut self) -> u32 {
        self.host
            .set(&self.rt, "system.dark", Value::Bool(true))
            .unwrap();
        assert!(self.r.apply(self.inst.flush().diff).is_empty());
        let mut k = 1;
        while self.r.wants_frame(SurfaceId(1)) {
            self.buf.paint_at(&mut self.r, SurfaceId(1), 1, frame(k));
            k += 1;
            assert!(k < 400, "never settled");
        }
        k - 1
    }
}

#[test]
fn the_reduced_token_from_the_theme_snaps_everything() {
    let mut moving = Shell::new(false);
    assert!(!moving.r.reduced_motion());
    let n = moving.flip();
    assert!(n > 5, "springs and poses animate: {n} frames");

    let mut still = Shell::new(true);
    assert!(still.r.reduced_motion(), "the token reached render");
    let n = still.flip();
    assert!(n <= 1, "everything snaps: {n} frames");
    // At rest at once: the red box at x 50, the green one fully drawn.
    let px = still.buf.px(55, 10);
    assert!(px[2] > 200 && px[1] < 50, "{px:?}");
    let green = (0..100).find(|x| still.buf.px(*x, 10)[1] > 200);
    assert!(green.is_some(), "the entering box is at rest");
}

/// The system setting changing while the shell runs: the token follows
/// `system.reduced_motion`, and the next change snaps.
#[test]
fn turning_reduced_motion_on_live_snaps_the_next_change() {
    let mut sh = Shell::new(false);
    sh.host
        .set(&sh.rt, "system.reduced_motion", Value::Bool(true))
        .unwrap();
    assert!(sh.r.apply(sh.inst.flush().diff).is_empty());
    assert!(sh.r.reduced_motion());
    let n = sh.flip();
    assert!(n <= 1, "snaps: {n} frames");
}

/// The desktop's preference (the portal's `reduced-motion`, which `strand
/// run` sends as `SceneDiff::reduced_motion`) reaches the renderer with no
/// theme token, and turns off again.
#[test]
fn a_diff_carries_the_desktops_reduced_motion() {
    let mut r = Renderer::new(strand_render::TextBackend::Inline(Box::new(
        strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![Arc::new(
            std::fs::read(strand_text::test_font_path()).unwrap(),
        )])),
    )));
    assert!(!r.reduced_motion());
    let diff = |on| SceneDiff {
        reduced_motion: Some(on),
        ..SceneDiff::default()
    };
    assert!(!diff(true).is_empty(), "a diff carrying only it is sent");
    r.apply(diff(true));
    assert!(r.reduced_motion());
    r.apply(diff(false));
    assert!(!r.reduced_motion());
}

/// A box whose `x` follows `40 * wave(1s)`.
const TIMED: &str = "// shell.strand
panel P {
  width: 100; height: 40; open: true; bg: #1e1e2e
  row {
    box { width: 20; height: 20; bg: #ff0000; x: 40 * wave(1s) }
  }
}
";

/// Where the red box's left edge is on row 10.
fn red_left(buf: &Buffer) -> Option<u32> {
    (0..100).find(|x| {
        let p = buf.px(*x, 10);
        p[2] > 200 && p[1] < 50
    })
}

/// design.md: `reduced_motion` turns off time signals. Every time leaf
/// reads `t = 0` (`TimeContext::frozen`): the box stays where `wave`
/// starts, the surface asks for no frames, and later frames repaint
/// nothing. Turned on while the signal runs, it stops at once at its
/// `t = 0` value; turned off, the clock runs again from where it would
/// be.
#[test]
fn time_signals_freeze() {
    // Moving: it wants a frame every refresh and half a period in the
    // box is 40 px right.
    let mut moving = Shell::with(TIMED, false);
    assert_eq!(red_left(&moving.buf), Some(0), "wave(1s) starts at 0");
    for k in 1..=30 {
        assert!(moving.r.wants_frame(SurfaceId(1)), "frame {k}");
        moving
            .buf
            .paint_at(&mut moving.r, SurfaceId(1), 1, frame(k));
    }
    let at = red_left(&moving.buf).expect("drawn");
    assert!((39..=41).contains(&at), "half a period in: {at}");

    // Reduced: frozen at `t = 0`.
    let mut still = Shell::with(TIMED, true);
    assert!(still.r.reduced_motion());
    assert!(!still.r.wants_frame(SurfaceId(1)), "no clock runs");
    let d = still.buf.paint_at(&mut still.r, SurfaceId(1), 1, frame(30));
    assert!(d.is_empty(), "nothing moves: {d:?}");
    assert_eq!(red_left(&still.buf), Some(0));

    // Turned on live: the next frame shows `t = 0` and the loop stops.
    moving
        .host
        .set(&moving.rt, "system.reduced_motion", Value::Bool(true))
        .unwrap();
    assert!(moving.r.apply(moving.inst.flush().diff).is_empty());
    assert!(moving.r.reduced_motion());
    assert!(moving.r.wants_frame(SurfaceId(1)), "one frame to stop");
    moving
        .buf
        .paint_at(&mut moving.r, SurfaceId(1), 1, frame(31));
    assert_eq!(red_left(&moving.buf), Some(0), "frozen at t = 0");
    assert!(!moving.r.wants_frame(SurfaceId(1)), "the loop stopped");
    assert_eq!(moving.r.next_wake(), None, "and nothing wakes it");

    // Off again: the clock was kept, so a quarter period later (t =
    // 0.75 s from the node's appearance) the box is 20 px right.
    moving
        .host
        .set(&moving.rt, "system.reduced_motion", Value::Bool(false))
        .unwrap();
    assert!(moving.r.apply(moving.inst.flush().diff).is_empty());
    assert!(moving.r.wants_frame(SurfaceId(1)));
    moving
        .buf
        .paint_at(&mut moving.r, SurfaceId(1), 1, frame(45));
    let at = red_left(&moving.buf).expect("drawn");
    assert!((19..=21).contains(&at), "t = 0.75 s: {at}");
    assert!(moving.r.wants_frame(SurfaceId(1)), "running again");
}

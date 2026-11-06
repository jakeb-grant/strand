//! (lock-int) Strand's own surfaces settle before a lock's stills are
//! taken (design.md, "Lock screen"): `Renderer::settle_for_lock` ends
//! every running `exit` outside the lock and gives every other surface
//! with a motion toward an end one settled frame, its next, where that
//! motion is at its end; loops and time signals run on through it, and a
//! lock's own surfaces are never touched. With nothing moving it does
//! nothing, so the lock is asked for at once.

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

/// An island whose green box fades with `fading`, whose red box loops a
/// slide and whose blue box follows `wave`; a launcher that fades out as
/// `open` turns false; a lock whose yellow box fades with `fading` too.
const SHELL: &str = "// shell.strand
state fading = false
state open = true
state locked = true
keyframes slide { 0% { x: 0 }; 100% { x: 100 }; duration: 1s; repeat: 0 }
panel Island {
  anchor: top; width: 200; height: 60; open: true; bg: #1e1e2e
  col {
    box { width: 20; height: 20; bg: #00ff00; opacity: (fading ? 0 : 1) ~ 240ms }
    box { width: 20; height: 20; bg: #ff0000; play slide }
    box { width: 20; height: 20; bg: #0000ff; x: 100 * wave(1s) }
  }
}
panel Launcher {
  anchor: bottom; width: 200; height: 60; open: open; bg: #e0e0e0
  exit { opacity: 0 }
}
lock Gate {
  open: locked; bg: #2050d0
  box { width: 20; height: 20; bg: #ffff00; opacity: (fading ? 0 : 1) ~ 240ms }
  on key(k) { auth.submit(k.name) }
}
";

const ISLAND: SurfaceId = SurfaceId(1);
const LAUNCHER: SurfaceId = SurfaceId(2);
const LOCK: SurfaceId = SurfaceId(3);
const T0: Duration = Duration::from_secs(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

struct Shell {
    _rt: Runtime,
    inst: Instance,
    r: Renderer,
    bufs: Vec<(SurfaceId, Buffer)>,
    nodes: [NodeId; 3],
}

impl Shell {
    fn new() -> Shell {
        let mut map = SourceMap::new();
        map.add("shell.strand".to_string(), SHELL.to_string());
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
        let program = Arc::new(lower::lower(
            &compiled.program,
            strand_compiler::schema::Schema::builtin(),
        ));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
        let inst = Instance::new(&rt, program, host, Storage::none());
        let mut r = renderer();
        assert!(r.apply(inst.flush().diff).is_empty());
        let kind = |r: &Renderer, k: NodeKind, nth: usize| {
            r.tree()
                .surface_nodes()
                .filter(|n| r.tree().get(*n).is_some_and(|n| n.kind == k))
                .nth(nth)
                .unwrap()
        };
        let island = kind(&r, NodeKind::Panel, 0);
        let launcher = kind(&r, NodeKind::Panel, 1);
        let lock = kind(&r, NodeKind::Lock, 0);
        let mut bufs = Vec::new();
        for (id, node) in [(ISLAND, island), (LAUNCHER, launcher), (LOCK, lock)] {
            r.attach_surface(id, node);
            r.configure_surface(id, Size::new(200, 60), Scale::ONE);
            let mut buf = Buffer::new(200, 60, Scale::ONE);
            buf.paint_at(&mut r, id, 0, T0);
            bufs.push((id, buf));
        }
        r.take_surface_changes();
        Shell {
            _rt: rt,
            inst,
            r,
            bufs,
            nodes: [island, launcher, lock],
        }
    }

    fn set(&mut self, name: &str, v: Value) {
        self.inst.set_value("shell", name, v).unwrap();
        assert!(self.r.apply(self.inst.flush().diff).is_empty());
    }

    /// Paints frame `k` of `id` if it wants one; whether it painted.
    fn paint(&mut self, id: SurfaceId, k: u32) -> bool {
        if !self.r.wants_frame(id) {
            return false;
        }
        let r = &mut self.r;
        let buf = &mut self.bufs.iter_mut().find(|(s, _)| *s == id).unwrap().1;
        buf.paint_at(r, id, 1, frame(k));
        true
    }

    fn buf(&self, id: SurfaceId) -> &Buffer {
        &self.bufs.iter().find(|(s, _)| *s == id).unwrap().1
    }

    /// The fade starts and the launcher starts leaving; `n` frames of
    /// each surface are painted.
    fn start_moving(&mut self, n: u32) {
        self.set("fading", Value::Bool(true));
        self.set("open", Value::Bool(false));
        for k in 1..=n {
            for id in [ISLAND, LAUNCHER, LOCK] {
                self.paint(id, k);
            }
        }
    }
}

/// The left edge of the first pixel on row `y` of `buf` that `is` picks.
fn left(buf: &Buffer, y: u32, is: impl Fn([u8; 4]) -> bool) -> Option<u32> {
    (0..200).find(|x| is(buf.px(*x, y)))
}

/// The buffer is BGRA: a green at least `g` strong.
fn green(px: [u8; 4]) -> u8 {
    px[1].saturating_sub(px[2].max(px[0]))
}

#[test]
fn a_lock_settles_strands_surfaces_and_leaves_loops_and_the_lock_alone() {
    let mut sh = Shell::new();
    let mut control = Shell::new();
    let [_, launcher, _] = sh.nodes;
    // Halfway through: the green box part faded, the launcher leaving.
    sh.start_moving(5);
    control.start_moving(5);
    let mid = green(sh.buf(ISLAND).px(10, 10));
    assert!(mid > 20 && mid < 250, "the fade is under way: {mid}");
    assert!(sh.r.surface_spec(launcher).is_some_and(|s| s.open));

    let settled = sh.r.settle_for_lock();
    // The launcher's exit ended: its spec reports it closed, so its
    // surface goes; the island owes a settled frame; the lock is not
    // touched.
    assert_eq!(settled.exits, 1, "{settled:?}");
    assert_eq!(settled.closed, vec![launcher], "{settled:?}");
    assert_eq!(settled.settled, vec![ISLAND], "{settled:?}");
    assert_eq!(settled.going, vec![LAUNCHER], "{settled:?}");
    assert!(!settled.surfaces().contains(&LOCK), "{settled:?}");
    assert!(
        sh.r.take_surface_changes()
            .iter()
            .any(|(n, c)| *n == launcher
                && matches!(c, SurfaceChange::Updated { spec, .. } if !spec.open)),
        "the launcher's spec closes now, not when its exit would have ended"
    );
    assert!(sh.r.lock_settle_owed(ISLAND), "the settled frame is owed");
    assert!(sh.r.lock_settle_owed(LAUNCHER), "the launcher must go");
    assert!(!sh.r.lock_settle_owed(LOCK), "the lock owes nothing");

    // The settled frame: the fade at its end (the green box gone to the
    // island's background) where the control is still part way, the
    // loop and the wave where the control has them.
    assert!(sh.paint(ISLAND, 6), "the settled frame is wanted");
    assert!(control.paint(ISLAND, 6));
    assert!(!sh.r.lock_settle_owed(ISLAND), "painted");
    let (a, b) = (sh.buf(ISLAND), control.buf(ISLAND));
    assert_eq!(green(a.px(10, 10)), 0, "faded out: {:?}", a.px(10, 10));
    assert!(green(b.px(10, 10)) > 0, "the control still fades");
    let red = |p: [u8; 4]| p[2] > 200 && p[1] < 50 && p[0] < 50;
    let blue = |p: [u8; 4]| p[0] > 200 && p[1] < 50 && p[2] < 50;
    let slid = left(a, 30, red);
    assert!(slid.is_some_and(|x| x > 0), "the loop runs: {slid:?}");
    assert_eq!(slid, left(b, 30, red), "the loop is where it would be");
    let waved = left(a, 50, blue);
    assert!(waved.is_some_and(|x| x > 0), "time runs: {waved:?}");
    assert_eq!(waved, left(b, 50, blue), "the wave is where it would be");
    // Time keeps the island asking for frames; the next ones draw on
    // with nothing left to settle.
    assert!(sh.r.wants_frame(ISLAND), "the loop and wave go on");
    assert!(sh.paint(ISLAND, 7));
    assert_eq!(green(sh.buf(ISLAND).px(10, 10)), 0, "and stays at its end");

    // The lock's own fade goes on as the control's.
    assert!(sh.paint(LOCK, 6));
    assert!(control.paint(LOCK, 6));
    // The yellow box part faded over the lock's blue: more red than the
    // blue has (BGRA).
    let yellow = |b: &Buffer| {
        (0..60)
            .flat_map(|y| (0..200).map(move |x| (x, y)))
            .filter(|(x, y)| b.px(*x, *y)[2] > 0x40)
            .count()
    };
    assert!(yellow(sh.buf(LOCK)) > 0, "the lock's box still shows");
    assert_eq!(
        sh.buf(LOCK).pixels,
        control.buf(LOCK).pixels,
        "the lock draws what it would have"
    );
    assert!(sh.r.wants_frame(LOCK), "its fade moves on");

    // The launcher's surface goes: nothing owed once it is detached.
    sh.r.detach_surface(LAUNCHER);
    assert!(!sh.r.lock_settle_owed(LAUNCHER));
}

/// Nothing moving toward an end (a loop and a wave are not): nothing to
/// settle, so the lock waits for nothing.
#[test]
fn a_lock_with_nothing_moving_settles_nothing() {
    let mut sh = Shell::new();
    for k in 1..=3 {
        sh.paint(ISLAND, k);
    }
    let settled = sh.r.settle_for_lock();
    assert!(settled.is_empty(), "{settled:?}");
    assert!(settled.surfaces().is_empty());
    assert!(sh.r.take_surface_changes().is_empty());
    for id in [ISLAND, LAUNCHER, LOCK] {
        assert!(!sh.r.lock_settle_owed(id), "{id:?}");
    }
}

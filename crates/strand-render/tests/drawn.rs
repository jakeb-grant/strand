//! (replay-owner) `Renderer::drawn_nodes` mid-motion, for a glitch
//! report's `drawn.json` (docs/architecture.md, "Glitch reports"): each
//! node's box and opacity as the last painted frame drew them, how far it
//! moves over the next frame period (what the replay's verdict bounds a
//! moving node by), and when that frame was drawn. Asking peeks; it
//! changes nothing the next frame draws.

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
  motion { spatial: spring(200, 1); effects: spring(200, 1); bouncy: spring(200, 1)
           reduced: false }
}
";

/// A mover that springs right, a fader that springs to 0.2 and a box
/// that stays.
const SHELL: &str = "// shell.strand
panel P {
  width: 200; height: 40; open: true; bg: #1e1e2e
  row {
    box { width: 20; height: 20; bg: #ff0000; x: system.dark ? 120 : 0 }
    box { width: 20; height: 20; bg: #00ff00; opacity: system.dark ? 0.2 : 1 }
    box { width: 20; height: 20; bg: #0000ff }
  }
}
";

const T0: Duration = Duration::from_secs(1);
const S: SurfaceId = SurfaceId(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    r: Renderer,
    buf: Buffer,
}

impl Shell {
    fn new() -> Shell {
        let mut map = SourceMap::new();
        map.add("theme.strand".to_string(), THEME.to_string());
        map.add("shell.strand".to_string(), SHELL.to_string());
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
        let program = Arc::new(lower::lower(
            &compiled.program,
            strand_compiler::schema::Schema::builtin(),
        ));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
        host.set(&rt, "system.dark", Value::Bool(false)).unwrap();
        let inst = Instance::new(&rt, program, host.clone(), Storage::none());
        let mut r = renderer();
        assert!(r.apply(inst.flush().diff).is_empty());
        let root = r.tree().surface_nodes().next().unwrap();
        r.attach_surface(S, root);
        let mut buf = Buffer::new(200, 40, Scale::ONE);
        buf.paint_at(&mut r, S, 0, T0);
        Shell {
            rt,
            host,
            inst,
            r,
            buf,
        }
    }

    /// Flips `system.dark` and paints frames 1 to `n`.
    fn flip_and_paint(&mut self, n: u32) {
        self.host
            .set(&self.rt, "system.dark", Value::Bool(true))
            .unwrap();
        assert!(self.r.apply(self.inst.flush().diff).is_empty());
        for k in 1..=n {
            self.buf.paint_at(&mut self.r, S, 1, frame(k));
        }
    }
}

/// Mid-flight, the mover travels and the fader fades over the next
/// period, the box that stays does neither; the travel is how far the
/// next frame does move it; asking twice gives the same frame and the
/// next frame draws as it would have unasked; at rest nothing travels.
#[test]
fn a_drawn_frame_mid_motion_says_how_far_each_node_moves() {
    let mut asked = Shell::new();
    let mut twin = Shell::new();
    asked.flip_and_paint(3);
    twin.flip_and_paint(3);
    let f = asked.r.drawn_nodes(S).expect("a painted surface");
    assert!(f.drawn_at.is_some(), "the frame's time is known");
    assert!(
        (f.period.as_secs_f64() - 0.016_667).abs() < 0.000_1,
        "{:?}",
        f.period
    );
    // Root, row, mover, fader, still.
    assert_eq!(f.nodes.len(), 5, "{:?}", f.nodes);
    let (mover, fader, still) = (f.nodes[2], f.nodes[3], f.nodes[4]);
    assert!(mover.travel > 1.0 && mover.travel < 120.0, "{mover:?}");
    assert_eq!(mover.fade, 0.0, "{mover:?}");
    assert!(fader.fade > 0.005, "{fader:?}");
    assert_eq!(fader.travel, 0.0, "{fader:?}");
    assert_eq!((still.travel, still.fade), (0.0, 0.0), "{still:?}");
    assert_eq!(asked.r.drawn_nodes(S).unwrap(), f, "asked again");

    // The next frame moves the mover as far as was said, and draws as
    // the twin's, which was never asked.
    asked.buf.paint_at(&mut asked.r, S, 1, frame(4));
    twin.buf.paint_at(&mut twin.r, S, 1, frame(4));
    assert!(
        asked.buf.pixels == twin.buf.pixels,
        "asking changed the next frame"
    );
    let g = asked.r.drawn_nodes(S).unwrap();
    let moved = (g.nodes[2].rect.x - mover.rect.x).abs();
    assert!(
        (moved - mover.travel).abs() < 0.05,
        "said {} px, moved {moved} px",
        mover.travel
    );
    let faded = (g.nodes[3].opacity - fader.opacity).abs();
    assert!(
        (faded - fader.fade).abs() < 0.002,
        "said {}, faded {faded}",
        fader.fade
    );

    // At rest: nothing travels.
    let mut k = 5;
    while asked.r.wants_frame(S) {
        asked.buf.paint_at(&mut asked.r, S, 1, frame(k));
        k += 1;
        assert!(k < 400, "never settled");
    }
    let rest = asked.r.drawn_nodes(S).unwrap();
    assert!(
        rest.nodes.iter().all(|n| n.travel == 0.0 && n.fade == 0.0),
        "{:?}",
        rest.nodes
    );
    assert_eq!(rest.nodes[2].rect.x, 120.0);
}

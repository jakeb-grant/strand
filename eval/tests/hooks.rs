//! strand-render's `eval-hooks` as the sandbox reads them (docs/decisions.md
//! eval-sandbox): the frame trace, the inline text hold and the router's
//! pinned clock, driven directly through `Renderer` and the logic VM
//! (the scenario driver builds on the same calls).
//!
//! The shell below is a bar-only copy of
//! crates/strand-render/tests/morph.rs's `Shell` (`boot_opts`, `apply`,
//! `paint`, `answer`, `set`), and `Buffer::paint_at` of
//! crates/strand-render/tests/common/mod.rs.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::probe::{Axes, FrameTrace, Hidden, Motions, NodeTrace, TextDrawn};
use strand_render::{InputScene, Intent, Renderer, Router, SceneTree, TextBackend};
use strand_scene::{
    InputEvent, LogicalPoint, NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, Scale,
    SceneDiff, Size, SurfaceId,
};
use strand_text::{FontConfig, TextEngine, test_font_path};

const BAR: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

/// The presentation time of frame `k` after `T0` at 60 Hz.
fn frame(k: u32) -> Duration {
    T0 + Duration::from_nanos(1_000_000_000 / 60 * u64::from(k))
}

/// A renderer shaping text inline with the bundled Liberation Sans.
fn renderer() -> Renderer {
    let data = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    Renderer::new(TextBackend::Inline(Box::new(engine)))
}

/// A compiled shell on one 300 px screen at scale 1, its bar painted.
struct Shell {
    inst: Instance,
    r: Renderer,
    pixels: Vec<u8>,
    k: u32,
}

impl Shell {
    fn boot(src: &str, hold: bool) -> Shell {
        let mut map = SourceMap::new();
        map.add("shell.strand".to_string(), src.to_string());
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
        let program = Arc::new(lower::lower(
            &compiled.program,
            strand_compiler::schema::Schema::builtin(),
        ));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
        let screen = host.record(
            "Screen",
            &[
                ("id", Value::text("Mock | DP-1")),
                ("name", Value::text("DP-1")),
                ("width", Value::float(300.0)),
                ("scale", Value::float(1.0)),
            ],
        );
        host.set(&rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
        let inst = Instance::new(&rt, program, host.clone(), Storage::none());
        let mut r = renderer();
        r.set_query_wait(Duration::from_secs(30));
        r.set_busy_window(Duration::ZERO);
        // No wall-clock holds: what text waits for is the hold's to say.
        r.set_first_frame_wait(Duration::ZERO);
        r.set_new_text_wait(Duration::ZERO);
        r.set_estimate_wait(Duration::ZERO);
        r.set_frame_trace(true);
        r.set_text_hold(hold);
        assert!(r.apply(inst.flush().diff).is_empty());
        let bar = r
            .tree()
            .surface_nodes()
            .find(|n| r.tree().get(*n).is_some_and(|n| n.kind == NodeKind::Bar))
            .expect("a bar");
        r.attach_surface(BAR, bar);
        r.configure_surface(BAR, Size::new(300, 30), Scale::ONE);
        let mut sh = Shell {
            inst,
            r,
            pixels: vec![0; 300 * 30 * 4],
            k: 0,
        };
        sh.paint();
        sh
    }

    /// Paints the next frame, handing layout facts to logic and its
    /// answer back; returns the frames traced.
    fn paint(&mut self) -> Vec<FrameTrace> {
        let k = self.k;
        self.k += 1;
        for _ in 0..8 {
            let t = PaintTarget::new(&mut self.pixels, Size::new(300, 30), 300 * 4, Scale::ONE, 1)
                .unwrap();
            self.r.paint(BAR, &mut t.at(frame(k)));
            let facts = self.r.take_layout_facts();
            if facts.is_empty() {
                break;
            }
            for (n, b) in facts {
                self.inst.set_box(n, b);
            }
            let mut diff = self.inst.flush().diff;
            diff.layout_seen = Some(self.r.layout_seq());
            assert!(self.r.apply(diff).is_empty());
        }
        self.r.take_frame_traces()
    }

    fn set(&mut self, name: &str, v: Value) {
        self.inst.set_value("shell", name, v).unwrap();
        let diff = self.inst.flush().diff;
        assert!(self.r.apply(diff).is_empty());
    }

    /// Paints while the bar wants frames; returns every frame traced.
    fn run(&mut self) -> Vec<FrameTrace> {
        let mut out = Vec::new();
        let mut n = 0;
        while self.r.wants_frame(BAR) {
            out.extend(self.paint());
            n += 1;
            assert!(n < 240, "never settled");
        }
        out
    }
}

/// `node`'s trace in `f`, if drawn or found hidden.
fn node(f: &FrameTrace, id: NodeId) -> Option<&NodeTrace> {
    f.nodes.iter().find(|n| n.node == id)
}

/// The first node of `kind` in `f` matching `pick`.
fn find(f: &FrameTrace, kind: NodeKind, pick: impl Fn(&NodeTrace) -> bool) -> Option<NodeId> {
    f.nodes
        .iter()
        .find(|n| n.kind == kind && pick(n))
        .map(|n| n.node)
}

/// crates/strand-render/tests/morph.rs's FOCUS_OFF (owner-testing.md bug
/// 25): the count re-enters with `enter { opacity: 0; x: -6 }`.
const FOCUS_OFF: &str = "
state dnd = true
bar Top {
  height: 30
  split { width: 300; height: 30; pad: 0, 14
    end { align: center
      row { gap: 6; align: center; shrink: 0
        box { width: 15; height: 12; bg: #00ff00 }
        if dnd { box { width: 14; height: 14; bg: #ff00ff } }
        if !dnd {
          row { gap: 3; align: center; shrink: 0; enter { opacity: 0; x: -6 }
            text \"iiii\" { width: 14; shrink: 0; color: #ff00ff }
          }
        }
        box { width: 20; height: 10; bg: #0000ff }
      }
    }
  }
}
";

#[test]
fn every_paint_is_traced_once_and_a_preview_never() {
    let mut sh = Shell::boot(FOCUS_OFF, false);
    // Boot painted the first frame (taken by `paint`); it reported the
    // whole bar.
    sh.r.update();
    assert!(sh.r.take_frame_traces().is_empty(), "update traces nothing");
    let at_rest = sh.paint();
    assert_eq!(at_rest.len(), 1, "{at_rest:?}");
    let f = &at_rest[0];
    assert_eq!(f.surface, BAR);
    assert_eq!(f.time, frame(1));
    assert_eq!(f.damage_px, 0, "nothing changed");
    assert!(!f.fresh, "the scene flattened at rest is reused");
    assert!(f.nodes.len() >= 6, "{:?}", f.nodes);
    let bar = &f.nodes[0];
    assert_eq!(bar.kind, NodeKind::Bar);
    assert_eq!(bar.parent, None);
    assert_eq!(bar.visual, bar.laid);
    // Paint order: a parent before its children.
    for (i, n) in f.nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            let at = f.nodes.iter().position(|m| m.node == p);
            assert!(at.is_some_and(|at| at < i), "{n:?} before its parent");
        }
    }
    // At rest nothing moves and every box is drawn where it is laid.
    for n in f.nodes.iter() {
        assert_eq!(n.motions, Motions::NONE, "{n:?}");
        assert_eq!(n.authored, Axes::NONE, "{n:?}");
        assert_eq!(n.drawn, n.laid, "{n:?}");
        assert_eq!(n.hidden, None, "{n:?}");
    }
}

#[test]
fn an_entering_row_reports_its_pose_then_authored_motion() {
    let mut sh = Shell::boot(FOCUS_OFF, false);
    sh.paint();
    sh.set("dnd", Value::Bool(false));
    let frames = sh.run();
    assert!(frames.len() > 3, "{}", frames.len());
    let green = |n: &NodeTrace| n.laid.w == 15.0;
    let bars = find(&frames[0], NodeKind::Box, green).expect("the bars");
    let count = frames
        .iter()
        .find_map(|f| find(f, NodeKind::Row, |n| n.motions.contains(Motions::ENTER)))
        .expect("the count enters");
    let first = frames
        .iter()
        .position(|f| node(f, count).is_some())
        .unwrap();
    let pose = node(&frames[first], count).unwrap();
    assert!(pose.motions.contains(Motions::ENTER), "{pose:?}");
    assert!(
        pose.authored.contains(Axes::X | Axes::OPACITY),
        "the pose is authored: {pose:?}"
    );
    assert!(
        pose.hidden == Some(Hidden::Opacity0) || pose.opacity < 0.25,
        "it starts faded out: {pose:?}"
    );
    assert!(pose.parts.spring[0] < -3.0, "it starts 6 px left: {pose:?}");
    // Its text is absent while it is hidden, and drawn from its shaped
    // layout once it shows.
    for f in &frames[first..] {
        let row = node(f, count).unwrap();
        let text = f
            .nodes
            .iter()
            .find(|n| n.parent == Some(count) && n.kind == NodeKind::Text);
        match row.hidden {
            Some(_) => assert!(text.is_none(), "{f:?}"),
            None => assert_eq!(text.and_then(|t| t.text), Some(TextDrawn::Shaped)),
        }
    }
    // The bars are a bystander: no authored motion, ever.
    for f in &frames {
        let b = node(f, bars).expect("the bars are drawn");
        assert_eq!(b.authored, Axes::NONE, "{b:?}");
    }
    // At rest: still, where laid out, and its own x is back to 0.
    let last = node(frames.last().unwrap(), count).unwrap();
    assert_eq!(last.motions, Motions::NONE, "{last:?}");
    assert_eq!(last.parts.spring, [0.0, 0.0]);
    assert_eq!(last.opacity, 1.0);
}

/// Three boxes entering under a 25 ms stagger with `enter { opacity: 0;
/// scale: 0.85 }` (blind set L's app grid, in a bar).
const STAGGERED: &str = "
state on = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 4; align: center; stagger: 25ms
    if on {
      box { width: 20; height: 10; bg: #0000ff; enter { opacity: 0; scale: 0.85 } }
      box { width: 20; height: 10; bg: #0000ff; enter { opacity: 0; scale: 0.85 } }
      box { width: 20; height: 10; bg: #0000ff; enter { opacity: 0; scale: 0.85 } }
    }
  }
}
";

/// (eval-finish) A child waiting for its stagger turn is held at its
/// enter pose, hidden at opacity 0: its trace reports the pose's scale,
/// not 1 (the hidden row's look was written as unscaled, which `stagger`
/// read as the child leaving its pose on its first frame).
#[test]
fn a_hidden_held_child_reports_its_poses_scale() {
    let mut sh = Shell::boot(STAGGERED, false);
    sh.paint();
    sh.set("on", Value::Bool(true));
    let frames = sh.run();
    let held: Vec<&NodeTrace> = frames
        .iter()
        .flat_map(|f| f.nodes.iter())
        .filter(|n| n.kind == NodeKind::Box && n.motions.contains(Motions::STAGGER))
        .collect();
    assert!(!held.is_empty(), "a child waits for its turn");
    for n in held {
        assert_eq!(n.hidden, Some(Hidden::Opacity0), "{n:?}");
        assert!(
            (n.scale[0] - 0.85).abs() < 1e-6 && (n.scale[1] - 0.85).abs() < 1e-6,
            "held at its pose's scale: {n:?}"
        );
    }
}

const LATE: &str = "
state show = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 4; align: center
    if show { text \"iiii\" { color: #ff00ff } }
    box { width: 20; height: 10; bg: #0000ff }
  }
}
";

#[test]
fn held_text_is_drawn_by_its_estimate_until_released() {
    let mut sh = Shell::boot(LATE, true);
    assert!(sh.r.held_text().is_empty(), "nothing to shape at boot");
    sh.set("show", Value::Bool(true));
    let f = sh.paint();
    let f = f.last().expect("a frame");
    let text = find(f, NodeKind::Text, |_| true).expect("the text is laid out");
    assert_eq!(node(f, text).unwrap().text, Some(TextDrawn::Estimate));
    let held = sh.r.held_text();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!((held[0].node, held[0].text.as_str()), (text, "iiii"));
    assert_eq!(held[0].paints, 1);
    assert_eq!((f.text_held, f.text_pending), (1, 1));
    // Held, it stays pending across frames.
    sh.paint();
    assert_eq!(sh.r.held_text()[0].paints, 2);
    assert!(sh.r.text_pending());
    assert_eq!(sh.r.release_text(&mut |h| h.text == "nothing"), 0);
    assert_eq!(sh.r.release_text(&mut |h| h.text == "iiii"), 1);
    assert!(sh.r.held_text().is_empty());
    assert!(sh.r.wants_frame(BAR), "its arrival repaints");
    let f = sh.run();
    let shaped = node(f.first().unwrap(), text).unwrap();
    assert_eq!(shaped.text, Some(TextDrawn::Shaped));
    assert_eq!(f.first().unwrap().text_held, 0);
    assert!(!sh.r.text_pending());
}

/// Two texts mounted at once, one held by name: the other is shaped in
/// the same frame (`set_text_hold_with`, the driver's `text hold where`).
const TWO: &str = "
state show = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 4; align: center
    if show { text \"iiii\" { color: #ff00ff } }
    if show { text \"mm\" { color: #00ff00 } }
  }
}
";

#[test]
fn a_hold_filter_holds_only_the_texts_it_picks() {
    let mut sh = Shell::boot(TWO, false);
    sh.r.set_text_hold_with(Some(Box::new(|_, text: &str| text == "iiii")));
    sh.set("show", Value::Bool(true));
    let f = sh.paint();
    let f = f.last().expect("a frame");
    let held = sh.r.held_text();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].text, "iiii");
    let late = find(f, NodeKind::Text, |n| n.node == held[0].node).unwrap();
    assert_eq!(node(f, late).unwrap().text, Some(TextDrawn::Estimate));
    let other = find(f, NodeKind::Text, |n| n.node != late).expect("the other text");
    assert_eq!(node(f, other).unwrap().text, Some(TextDrawn::Shaped));
    // No filter: nothing held, what was is released.
    sh.r.set_text_hold_with(None);
    assert!(sh.r.held_text().is_empty());
    let f = sh.run();
    let last = f.last().unwrap();
    assert_eq!(node(last, late).unwrap().text, Some(TextDrawn::Shaped));
    assert_eq!(last.text_held, 0);
}

/// At rest the trace's boxes are the renderer's own: every node's
/// visual box is its `node_rect`, the box hit testing reads.
#[test]
fn at_rest_the_traced_boxes_are_the_renderers() {
    let mut sh = Shell::boot(FOCUS_OFF, false);
    sh.set("dnd", Value::Bool(false));
    let f = sh.run();
    let f = f.last().expect("frames");
    let mut n = 0;
    for t in f.nodes.iter() {
        let Some(rect) = sh.r.node_rect(BAR, t.node) else {
            continue;
        };
        n += 1;
        assert_eq!(t.visual, rect, "{t:?}");
        assert_eq!(t.laid, rect, "{t:?}");
    }
    assert!(n >= 6, "{n} nodes with a box");
}

/// A bar with an item whose popup opens on hover after `delay`
/// (crates/strand-render/tests/input.rs's `Islands`, one island).
struct Island {
    tree: SceneTree,
}

const BAR_N: NodeId = NodeId::new(0, 0);
const ITEM: NodeId = NodeId::new(1, 0);
const POP: NodeId = NodeId::new(2, 0);

impl InputScene for Island {
    fn hit(&self, _: SurfaceId, at: LogicalPoint) -> Vec<NodeId> {
        if at.x < 10.0 {
            vec![ITEM, BAR_N]
        } else {
            vec![BAR_N]
        }
    }
    fn tree(&self) -> Option<&SceneTree> {
        Some(&self.tree)
    }
}

#[test]
fn the_router_reads_the_pinned_clock() {
    let delay = Duration::from_millis(80);
    let mut d = SceneDiff::new();
    d.create(BAR_N, NodeKind::Bar, None, 0)
        .create(ITEM, NodeKind::Box, Some(BAR_N), 0)
        .create(POP, NodeKind::Popup, Some(ITEM), 0);
    d.set(POP, Prop::Trigger, PropValue::Keyword("hover".into()));
    d.set(POP, Prop::Delay, PropValue::Duration(delay));
    let mut tree = SceneTree::new();
    assert!(tree.apply(d).is_empty());
    let mut scene = Island { tree };
    let opened = |out: Vec<Intent>| {
        out.into_iter().any(|i| {
            matches!(
                i,
                Intent::Write {
                    node: POP,
                    prop: Prop::Open,
                    value: PropValue::Bool(true)
                }
            )
        })
    };
    // An hour ago: the wall clock would have opened it long since.
    let i0 = Instant::now() - Duration::from_secs(3600);
    let mut router = Router::new();
    router.set_now(Some(i0));
    router.attached(BAR, BAR_N);
    let over = InputEvent::PointerMotion {
        surface: BAR,
        position: LogicalPoint::new(5.0, 1.0),
        time: 0,
    };
    assert!(!opened(router.handle(&over, &mut scene)));
    assert_eq!(
        router.next_wake(),
        Some(i0 + delay),
        "the delay runs from the pinned time"
    );
    router.set_now(Some(i0 + delay - Duration::from_millis(1)));
    assert!(!opened(router.tick(&mut scene)), "a millisecond early");
    router.set_now(Some(i0 + delay));
    assert!(opened(router.tick(&mut scene)), "opens when its delay ends");
}

/// Every layout inline shaping delivers at once is counted, as a text
/// worker would wake the loop for it; one the hold keeps back is not,
/// nor one `release_text` delivers.
#[test]
fn inline_shaping_counts_the_wakes_a_worker_would_make() {
    let mut sh = Shell::boot(TWO, false);
    sh.r.take_inline_shaped();
    sh.r.set_text_hold_with(Some(Box::new(|_, text: &str| text == "iiii")));
    sh.set("show", Value::Bool(true));
    sh.paint();
    assert_eq!(sh.r.take_inline_shaped(), 1, "\"mm\" alone");
    assert_eq!(sh.r.take_inline_shaped(), 0, "taken");
    assert_eq!(sh.r.release_text(&mut |_| true), 1);
    sh.run();
    assert_eq!(sh.r.take_inline_shaped(), 0, "a release is not shaping");
}

//! End to end: the real `strand-render` renderer as the surface host on a
//! headless sway, wired the way `docs/architecture.md` ("Render loop")
//! describes, checked with grim.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Sway, WAIT};
use strand_render::{Renderer, TextBackend};
use strand_scene::{
    Color, Damage, Font, NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, Scale, SceneDiff,
    Size, SurfaceId,
};
use strand_surface::{Config, Monitor, SurfaceHost, SurfaceManager};
use strand_text::{FontConfig, TEST_FONT_FAMILY, TextEngine, test_font_path};

/// What the binary will do: forward lifecycle hooks to the renderer.
struct Host {
    renderer: Renderer,
}

impl Painter for Host {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        self.renderer.paint(surface, target)
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.renderer.wants_frame(surface)
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.renderer.opaque_region(surface)
    }
}

impl SurfaceHost for Host {
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, _: Option<&Monitor>) {
        self.renderer.attach_surface(surface, node);
    }

    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        self.renderer.configure_surface(surface, size, scale);
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.renderer.detach_surface(surface);
    }

    fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        self.renderer.frame_deadline(surface)
    }

    fn frame_dropped(&mut self, surface: SurfaceId) {
        self.renderer.invalidate(surface);
    }
}

const ROOT: NodeId = NodeId::new(0, 0);
const CLOCK: NodeId = NodeId::new(1, 0);
const SQUARE: NodeId = NodeId::new(2, 0);

fn rgb(c: &str) -> [u8; 3] {
    let c = Color::from_hex(c).unwrap();
    let to = |v: f32| (v * 255.0).round() as u8;
    [to(c.r), to(c.g), to(c.b)]
}

fn scene() -> SceneDiff {
    let mut d = SceneDiff::default();
    d.create(ROOT, NodeKind::Bar, None, 0);
    d.set(ROOT, Prop::Name, PropValue::Text("Top".into()));
    d.set(ROOT, Prop::Edge, PropValue::Keyword("top".into()));
    d.set(ROOT, Prop::Height, PropValue::Number(36.0));
    d.set(
        ROOT,
        Prop::Bg,
        PropValue::Color(Color::from_hex("#1e1e2e").unwrap()),
    );
    d.set(
        ROOT,
        Prop::Color,
        PropValue::Color(Color::from_hex("#ffffff").unwrap()),
    );
    d.set(
        ROOT,
        Prop::Font,
        PropValue::Font(Font {
            family: TEST_FONT_FAMILY.into(),
            size: 16.0,
            weight: 400,
        }),
    );
    d.create(CLOCK, NodeKind::Text, Some(ROOT), 0);
    d.set(CLOCK, Prop::Text, PropValue::Text("12:34".into()));
    d.set(CLOCK, Prop::X, PropValue::Number(20.0));
    d.set(CLOCK, Prop::Y, PropValue::Number(8.0));
    d.create(SQUARE, NodeKind::Box, Some(ROOT), 1);
    d.set(SQUARE, Prop::X, PropValue::Number(300.0));
    d.set(SQUARE, Prop::Y, PropValue::Number(8.0));
    d.set(SQUARE, Prop::Size, PropValue::Number(20.0));
    d.set(
        SQUARE,
        Prop::Bg,
        PropValue::Color(Color::from_hex("#f38ba8").unwrap()),
    );
    d
}

/// Applies a diff the way the binary's diff-channel callback will.
fn apply(mgr: &mut SurfaceManager<Host>, diff: SceneDiff) {
    let state = mgr.state_mut();
    let errors = state.host_mut().renderer.apply(diff);
    assert!(errors.is_empty(), "{errors:?}");
    let changes = state.host_mut().renderer.take_surface_changes();
    for (node, change) in changes {
        state.apply_surface_change(node, change);
    }
    state.poll();
}

#[test]
fn renderer_paints_a_bar_through_the_surface_manager() {
    let Some(sway) = Sway::start("renderer_paints_a_bar_through_the_surface_manager") else {
        return;
    };
    let font = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
    let host = Host {
        renderer: Renderer::new(TextBackend::Inline(Box::new(engine))),
    };
    let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default()).unwrap();
    apply(&mut mgr, scene());
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let st = s.stats();
            st.commits > 0 && st.presented >= st.commits
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let info = mgr.state().surfaces()[0].clone();
    assert_eq!(info.namespace, "strand-Top");
    assert_eq!(info.buffer_size, Size::new(1920, 36));
    // The opaque background was declared opaque.
    assert_eq!(info.stats.opaque_updates, 1);

    let shot = sway.grim("HEADLESS-1");
    let bg = rgb("#1e1e2e");
    assert_eq!(shot.rgb(1000, 18), bg);
    assert_eq!(shot.rgb(310, 18), rgb("#f38ba8"));
    // The clock's glyphs are drawn: some pixel in its box is light.
    let lit = (20..80).any(|x| (8..30).any(|y| shot.rgb(x, y)[0] > 0xc0));
    assert!(lit, "no text drawn");

    // Clock-tick-sized changes repaint only what moved, once buffers are
    // reused (the second frame ever needs a fresh, fully painted buffer).
    for x in [500.0, 700.0, 900.0] {
        let mut d = SceneDiff::default();
        d.set(SQUARE, Prop::X, PropValue::Number(x));
        let commits = mgr.state().stats().commits;
        apply(&mut mgr, d);
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                let st = s.stats();
                st.commits > commits && st.presented >= st.commits
            })
            .unwrap();
        assert!(ok);
    }
    let damage = mgr.state().host().renderer.last_damage(info.id).unwrap();
    assert!(damage.area() <= 4 * 20 * 20, "{damage:?}");
    std::thread::sleep(Duration::from_millis(50));
    let shot = sway.grim("HEADLESS-1");
    for x in [310, 510, 710] {
        assert_eq!(shot.rgb(x, 18), bg, "old square at {x}");
    }
    assert_eq!(shot.rgb(910, 18), rgb("#f38ba8"));
    assert!((20..80).any(|x| (8..30).any(|y| shot.rgb(x, y)[0] > 0xc0)));

    // Then it is idle: no frame callbacks pending, nothing painted.
    let before = mgr.state().stats();
    let t = Instant::now();
    mgr.dispatch(Some(Duration::from_millis(700))).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(650), "woke while idle");
    assert_eq!(mgr.state().stats(), before);
}

//! The emitter's diffs as the renderer takes them: the design shells and
//! theme applied to a real `strand_render::Renderer`, surface specs
//! resolved through the token table, and a bar painted offline.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::instantiate::Instance;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::{Renderer, TextBackend};
use strand_scene::{
    Anchor, Edge, Keyboard, Layer, PaintTarget, Painter, Scale, Screens, Size, SurfaceChange,
    SurfaceId,
};
use strand_text::{FontConfig, TextEngine, test_font_path};

fn renderer() -> Renderer {
    let data = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    Renderer::new(TextBackend::Inline(Box::new(engine)))
}

fn fixture(name: &str) -> (String, String) {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    (name.to_string(), std::fs::read_to_string(path).unwrap())
}

#[test]
fn design_shells_reach_the_renderer() {
    let mut map = SourceMap::new();
    for f in [
        "theme.strand",
        "bar.strand",
        "launcher.strand",
        "toasts.strand",
        "osd.strand",
    ] {
        let (n, t) = fixture(f);
        map.add(n, t);
    }
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::new(
        &rt,
        program,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let mut r = renderer();
    let errors = r.apply(inst.flush().diff);
    assert!(errors.is_empty(), "{errors:?}");
    let specs: Vec<_> = r
        .take_surface_changes()
        .into_iter()
        .filter_map(|(id, c)| match c {
            SurfaceChange::Created(s) => Some((id, s)),
            _ => None,
        })
        .collect();
    let named = |n: &str| {
        specs
            .iter()
            .find(|(_, s)| s.name.as_deref() == Some(n))
            .unwrap_or_else(|| panic!("no surface {n}: {specs:?}"))
    };
    // The bar: on its monitor, its margin resolved through `$space.2`.
    let (bar, top) = named("Top");
    assert_eq!(top.screens, Screens::Named(vec!["DP-1".into()]));
    assert_eq!(top.edge, Some(Edge::Top));
    assert_eq!(top.height, Some(36.0));
    assert_eq!(top.exclusive_zone(), Some(36.0));
    assert_eq!(
        (top.margin.top, top.margin.right, top.margin.bottom),
        (8.0, 8.0, 0.0)
    );
    let (_, launcher) = named("Launcher");
    assert_eq!(launcher.screens, Screens::Focused);
    assert_eq!(launcher.layer, Some(Layer::Overlay));
    assert_eq!(launcher.keyboard, Keyboard::Exclusive);
    assert!(!launcher.open);
    let (_, toasts) = named("Toasts");
    assert_eq!(toasts.anchor, Anchor::TopRight);
    assert_eq!(toasts.margin.top, 12.0, "$space.3");
    let (_, level) = named("Level");
    assert_eq!(level.layer, Some(Layer::Overlay));
    assert!(!level.open);
    // The calendar popup is a surface of its own.
    assert!(
        specs
            .iter()
            .any(|(_, s)| s.kind == strand_scene::NodeKind::Popup)
    );

    // Paint the bar (flex layout is M2: the containers have no size yet,
    // the bar's own background draws); a minute later the tick is one
    // text prop the renderer takes.
    let surface = SurfaceId(1);
    r.attach_surface(surface, *bar);
    r.configure_surface(surface, Size::new(1920, 36), Scale::ONE);
    let mut pixels = vec![0u8; 1920 * 36 * 4];
    let damage = {
        let mut t =
            PaintTarget::new(&mut pixels, Size::new(1920, 36), 1920 * 4, Scale::ONE, 0).unwrap();
        r.paint(surface, &mut t)
    };
    assert!(damage.area() > 0);
    assert!(pixels.iter().any(|&b| b != 0), "the bar drew something");
    host.set_time(
        &rt,
        std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(strand_compiler::vm::schema_host::MOCK_TIME + 60),
    );
    let tick = inst.flush();
    assert_eq!(tick.diff.ops.len(), 1, "{:?}", tick.diff.ops);
    assert!(r.apply(tick.diff).is_empty());
    // A theme switch is one token table; the bar's spec is unchanged.
    let mocha = host.variant("Look", "mocha");
    inst.set("theme.look", mocha).unwrap();
    let errors = r.apply(inst.flush().diff);
    assert!(errors.is_empty(), "{errors:?}");
}

/// A container query settles inside the frame: render holds a frame
/// whose layout changed a size a query reads until logic has seen the
/// facts (`layout_seen`), so no painted frame shows the variant for the
/// stale size, at boot (where `self.width` starts at 0) or on a resize.
#[test]
fn container_queries_settle_before_the_frame_paints() {
    let mut map = SourceMap::new();
    map.add(
        "q.strand".to_string(),
        "bar Top {\n  height: 30\n  row {\n    opacity: 1\n    when self.width < 300 { opacity: 0.5 }\n    text \"x\"\n  }\n}\n"
            .to_string(),
    );
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::new(
        &rt,
        program,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let mut r = renderer();
    r.set_query_wait(std::time::Duration::from_secs(30));
    // Idle between the resizes however fast the test runs (a surface in
    // motion holds nothing for queries: render's own test).
    r.set_busy_window(std::time::Duration::ZERO);
    assert!(r.apply(inst.flush().diff).is_empty());
    let bar = r
        .take_surface_changes()
        .into_iter()
        .find_map(|(id, c)| matches!(c, SurfaceChange::Created(_)).then_some(id))
        .unwrap();
    let row = r.tree().get(bar).unwrap().children[0];
    assert_eq!(
        r.tree().get(row).unwrap().get(strand_scene::Prop::Watch),
        Some(&strand_scene::PropValue::Keyword("query".into()))
    );
    let opacity = |r: &Renderer| match r.tree().get(row).unwrap().get(strand_scene::Prop::Opacity) {
        Some(strand_scene::PropValue::Number(n)) => *n,
        p => panic!("{p:?}"),
    };
    // What logic would do: take the facts in and answer with its diff.
    let answer = |r: &mut Renderer| {
        let facts = r.take_layout_facts();
        assert!(facts.iter().any(|(n, _, _)| *n == row), "{facts:?}");
        for (n, w, h) in facts {
            inst.set_size(n, w, h);
        }
        let mut diff = inst.flush().diff;
        diff.layout_seen = Some(r.layout_seq());
        assert!(r.apply(diff).is_empty());
    };
    let surface = SurfaceId(1);
    r.attach_surface(surface, bar);
    let mut painted = Vec::new();
    for width in [400u32, 250, 400] {
        r.configure_surface(surface, Size::new(width, 30), Scale::ONE);
        assert!(!r.wants_frame(surface), "{width}: held for the query");
        answer(&mut r);
        assert!(r.wants_frame(surface), "{width}: released by the answer");
        let mut pixels = vec![0u8; width as usize * 30 * 4];
        let mut t =
            PaintTarget::new(&mut pixels, Size::new(width, 30), width * 4, Scale::ONE, 0).unwrap();
        r.paint(surface, &mut t);
        painted.push(opacity(&r));
        // Settled: the answer's own layout asks for nothing more.
        answer_if_any(&mut r, &inst);
        assert!(!r.wants_frame(surface), "{width}: settled");
    }
    assert_eq!(
        painted,
        [1.0, 0.5, 1.0],
        "each frame shows its width's variant"
    );
}

/// Hands logic any facts left (sizes the answer's own layout changed).
fn answer_if_any(r: &mut Renderer, inst: &Instance) {
    let facts = r.take_layout_facts();
    if facts.is_empty() {
        return;
    }
    for (n, w, h) in facts {
        inst.set_size(n, w, h);
    }
    let mut diff = inst.flush().diff;
    diff.layout_seen = Some(r.layout_seq());
    r.apply(diff);
}

/// The bar's `Clock`: a click on its text toggles `open`, and the
/// calendar `popup`'s spec opens nested in the bar, anchored to the
/// clock's box, sized by the calendar.
#[test]
fn a_click_on_the_clock_opens_the_calendar_popup() {
    let mut map = SourceMap::new();
    for f in ["theme.strand", "bar.strand"] {
        let (n, t) = fixture(f);
        map.add(n, t);
    }
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::new(
        &rt,
        program,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let mut r = renderer();
    assert!(r.apply(inst.flush().diff).is_empty());
    let changes = r.take_surface_changes();
    let bar = changes
        .iter()
        .find_map(|(id, c)| match c {
            SurfaceChange::Created(s) if s.kind == strand_scene::NodeKind::Bar => Some(*id),
            _ => None,
        })
        .unwrap();
    let popup = changes
        .iter()
        .find_map(|(id, c)| match c {
            SurfaceChange::Created(s) if s.kind == strand_scene::NodeKind::Popup => Some(*id),
            _ => None,
        })
        .unwrap();
    let clock = r.tree().get(popup).unwrap().parent.unwrap();
    let surface = SurfaceId(1);
    r.attach_surface(surface, bar);
    let size = Size::new(1920, 64);
    r.configure_surface(surface, size, Scale::ONE);
    let mut pixels = vec![0u8; (size.w * size.h * 4) as usize];
    {
        let mut t = PaintTarget::new(&mut pixels, size, size.w * 4, Scale::ONE, 0).unwrap();
        r.paint(surface, &mut t);
    }
    r.take_surface_changes();
    // The router's click on the clock's box.
    let b = r.boxes(surface).unwrap().rects[&clock];
    let at = strand_scene::LogicalPoint::new(b.x + b.w / 2.0, b.y + b.h / 2.0);
    assert!(
        r.hit(surface, at).contains(&clock),
        "{:?}",
        r.hit(surface, at)
    );
    let mut router = strand_render::Router::new();
    router.attached(surface, bar);
    let mut intents = Vec::new();
    for state in [
        strand_scene::ButtonState::Pressed,
        strand_scene::ButtonState::Released,
    ] {
        intents.extend(router.handle(
            &strand_scene::InputEvent::PointerButton {
                surface,
                position: at,
                button: strand_scene::input::button::LEFT,
                state,
                time: 0,
            },
            &mut r,
        ));
    }
    for i in intents {
        if let strand_render::Intent::Event { node, event } = i {
            if event == strand_render::NodeEvent::Click {
                inst.event(node, "click", Vec::new());
            }
        }
    }
    assert!(r.apply(inst.flush().diff).is_empty());
    let spec = r
        .take_surface_changes()
        .into_iter()
        .find_map(|(id, c)| match c {
            SurfaceChange::Updated { spec, .. } if id == popup => Some(spec),
            _ => None,
        })
        .expect("the popup's spec changed");
    assert!(spec.open);
    assert_eq!(spec.parent, Some(bar));
    assert_eq!(spec.anchor_rect, Some(b));
    assert!(
        spec.width.unwrap() > 150.0 && spec.height.unwrap() > 150.0,
        "{spec:?}"
    );
}

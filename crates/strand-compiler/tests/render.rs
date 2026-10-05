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
    let inst = Instance::new(&rt, program, host.clone(), None);
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

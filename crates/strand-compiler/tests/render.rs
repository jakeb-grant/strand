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
        if let strand_render::Intent::Event { node, event } = i
            && event == strand_render::NodeEvent::Click
        {
            inst.event(node, "click", Vec::new());
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

/// design.md's theme switcher end to end: `segmented { options: Look;
/// value: <-> theme.look }` compiled, a click on a segment routed, its
/// write applied to the instance (a keyword into the enum `state`), and
/// the chosen segment redrawn.
#[test]
fn the_theme_switcher_writes_the_look() {
    let mut map = SourceMap::new();
    let (n, t) = fixture("theme.strand");
    map.add(n, t);
    map.add(
        "switcher.strand",
        "bar Switcher { edge: top; height: 40\n  segmented { options: Look; value: <-> theme.look }\n}\n",
    );
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
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
    let bar = r
        .take_surface_changes()
        .iter()
        .find_map(|(id, c)| match c {
            SurfaceChange::Created(s) if s.kind == strand_scene::NodeKind::Bar => Some(*id),
            _ => None,
        })
        .unwrap();
    let surface = SurfaceId(1);
    r.attach_surface(surface, bar);
    let size = Size::new(800, 40);
    r.configure_surface(surface, size, Scale::ONE);
    let paint = |r: &mut Renderer| {
        let mut pixels = vec![0u8; (size.w * size.h * 4) as usize];
        let mut t = PaintTarget::new(&mut pixels, size, size.w * 4, Scale::ONE, 0).unwrap();
        r.paint(surface, &mut t);
        pixels
    };
    let before = paint(&mut r);
    let seg = *r
        .boxes(surface)
        .unwrap()
        .rects
        .keys()
        .find(|n| r.tree().get(**n).unwrap().kind == strand_scene::NodeKind::Segmented)
        .unwrap();
    let b = r.boxes(surface).unwrap().rects[&seg];
    // Five options (auto, light, dark, wallpaper, mocha): the second.
    let at = strand_scene::LogicalPoint::new(b.x + b.w * 0.3, b.y + b.h / 2.0);
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
    let mut wrote = 0;
    for i in intents {
        if let strand_render::Intent::Write { node, prop, value } = i {
            inst.write(node, prop, value).unwrap();
            wrote += 1;
        }
    }
    assert_eq!(wrote, 1);
    // `light`, the second variant of `enum Look { auto, light, … }`.
    let look = inst.get("theme.look").unwrap();
    assert!(matches!(look, Value::Enum(_, 1)), "{look:?}");
    assert!(r.apply(inst.flush().diff).is_empty());
    let after = paint(&mut r);
    // The second segment's pixels changed (it is the chosen one now), and
    // so did the first's (no longer chosen).
    let changed = |x0: f32, x1: f32| {
        (b.y as u32 + 2..(b.y + b.h) as u32 - 2).any(|y| {
            (x0 as u32..x1 as u32).any(|x| {
                let i = ((y * size.w + x) * 4) as usize;
                before[i..i + 4] != after[i..i + 4]
            })
        })
    };
    let w = b.w / 5.0;
    assert!(changed(b.x + w, b.x + 2.0 * w), "the chosen segment");
    assert!(changed(b.x, b.x + w), "the one chosen before");
}

/// `marks: h.ranges` from `apps.search` (a list of `Range` records)
/// reaches the renderer as `[start, end]` pairs, which it paints in
/// `mark_color` (the launcher's matched letters in `$accent`).
#[test]
fn search_ranges_reach_text_marks() {
    use strand_scene::PropValue::{List, Number};
    let pair = |a: f32, b: f32| vec![List(vec![List(vec![Number(a), Number(b)])])];
    assert_eq!(
        marks_of("oo", "Foot"),
        pair(1.0, 3.0),
        "\"Foot\" marked at 1..3"
    );
    // Characters, not bytes, and case folded per character: "É" is two
    // bytes.
    assert_eq!(
        marks_of("CR", "Écrire"),
        pair(1.0, 3.0),
        "\"Écrire\" at 1..3"
    );
    assert_eq!(marks_of("é", "Écrire"), pair(0.0, 1.0));
}

/// The `marks` a text drawing `apps.search(query)`'s one hit for an app
/// named `name` carries.
fn marks_of(query: &str, name: &str) -> Vec<strand_scene::PropValue> {
    let mut map = SourceMap::new();
    map.add(
        "marks.strand",
        format!(
            "let hits = apps.search(\"{query}\")\nbar B {{ edge: top; height: 40\n  row {{ for h in hits {{ text h.app.name {{ marks: h.ranges }} }} }}\n}}\n"
        ),
    );
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let app = host.record(
        "App",
        &[
            ("id", Value::text("app")),
            ("name", Value::text(name)),
            ("icon", Value::text("utilities-terminal")),
        ],
    );
    host.set(&rt, "apps.all", Value::list(vec![app])).unwrap();
    let inst = Instance::new(
        &rt,
        program,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let mut r = renderer();
    assert!(r.apply(inst.flush().diff).is_empty());
    let bar = r
        .take_surface_changes()
        .iter()
        .find_map(|(id, c)| match c {
            SurfaceChange::Created(s) if s.kind == strand_scene::NodeKind::Bar => Some(*id),
            _ => None,
        })
        .unwrap();
    let surface = SurfaceId(1);
    r.attach_surface(surface, bar);
    r.configure_surface(surface, Size::new(400, 40), Scale::ONE);
    r.boxes(surface)
        .unwrap()
        .rects
        .keys()
        .filter_map(|n| r.tree().get(*n)?.get(strand_scene::Prop::Marks).cloned())
        .collect()
}

/// Reads `refs/<name>.png` (RGBA) and compares it with `pixels`
/// (premultiplied BGRA) within `tolerance` per channel; `STRAND_BLESS=1`
/// writes it instead. On a mismatch the frame is written beside it as
/// `<name>.actual.png`.
fn assert_matches_ref(name: &str, size: Size, pixels: &[u8], tolerance: u8) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/refs");
    let path = dir.join(format!("{name}.png"));
    let rgba: Vec<u8> = pixels
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u32;
            let un = |c: u8| {
                (c as u32 * 255 + a / 2)
                    .checked_div(a)
                    .map_or(0, |v| v.min(255) as u8)
            };
            [un(p[2]), un(p[1]), un(p[0]), p[3]]
        })
        .collect();
    let save = |path: &std::path::Path| {
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbaImage::from_raw(size.w, size.h, rgba.clone())
            .unwrap()
            .save(path)
            .unwrap();
    };
    if std::env::var_os("STRAND_BLESS").is_some() {
        save(&path);
        return;
    }
    let want = image::open(&path)
        .unwrap_or_else(|e| panic!("{path:?}: {e}; run with STRAND_BLESS=1"))
        .to_rgba8();
    assert_eq!(want.dimensions(), (size.w, size.h), "{name}: size differs");
    let bad = want
        .as_raw()
        .chunks_exact(4)
        .zip(pixels.chunks_exact(4))
        .filter(|(w, got)| {
            let a = w[3] as u32;
            let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            let exp = [pm(w[2]), pm(w[1]), pm(w[0]), w[3]];
            got.iter().zip(exp).any(|(g, e)| g.abs_diff(e) > tolerance)
        })
        .count();
    if bad > 0 {
        save(&dir.join(format!("{name}.actual.png")));
        panic!("{name}: {bad} pixels differ by more than {tolerance}");
    }
}

/// design.md's rice ("What a rice looks like", `rice_now.strand`) on a
/// bar, compiled with the design theme and painted offline at fixed
/// times while music plays: a squircle pill with a rotating conic border,
/// a cookie-shaped album cover turning at 20°/s, a mirrored spectrum fed
/// by the audio tap and a breathing glow (refs `rice_now_0ms`,
/// `rice_now_500ms`, `rice_now_1000ms`). Only the pill's own pixels
/// repaint. Paused, the cover stops turning, the glow goes and the
/// spectrum rests; the border, whose `conic(from: t * 40deg, …)` reads
/// `t` whether or not music plays, keeps turning (decisions.md,
/// m4-effects-finish).
#[test]
fn rice_now_renders_at_fixed_times() {
    use std::time::Duration;
    // The album art: four 32 px quadrants.
    let dir = std::env::temp_dir().join(format!("strand-rice-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let art = dir.join("art.png");
    image::RgbaImage::from_fn(64, 64, |x, y| match (x < 32, y < 32) {
        (true, true) => image::Rgba([0xf3, 0x8b, 0xa8, 0xff]),
        (false, true) => image::Rgba([0xa6, 0xe3, 0xa1, 0xff]),
        (true, false) => image::Rgba([0x89, 0xb4, 0xfa, 0xff]),
        (false, false) => image::Rgba([0xf9, 0xe2, 0xaf, 0xff]),
    })
    .save(&art)
    .unwrap();

    let mut map = SourceMap::new();
    for f in ["theme.strand", "rice_now.strand"] {
        let (n, t) = fixture(f);
        map.add(n, t);
    }
    map.add(
        "rice_bar.strand".to_string(),
        "bar Rice { edge: top; height: 48\n  row { pad: 8; Now }\n}\n".to_string(),
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
    let sink = host.record("AudioDevice", &[("id", Value::int(42))]);
    host.set(&rt, "audio.sink", sink).unwrap();
    host.set(&rt, "media.playing", Value::Bool(true)).unwrap();
    host.set(
        &rt,
        "media.art",
        Value::text(art.to_string_lossy().as_ref()),
    )
    .unwrap();
    let inst = Instance::new(
        &rt,
        program,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let mut r = renderer();
    assert!(r.apply(inst.flush().diff).is_empty());
    let bar = r
        .take_surface_changes()
        .into_iter()
        .find_map(|(id, c)| match c {
            SurfaceChange::Created(s) if s.kind == strand_scene::NodeKind::Bar => Some(id),
            _ => None,
        })
        .unwrap();
    let surface = SurfaceId(1);
    let size = Size::new(220, 48);
    r.attach_surface(surface, bar);
    r.configure_surface(surface, size, Scale::ONE);
    let mut pixels = vec![0u8; (size.w * size.h * 4) as usize];
    let t0 = Duration::from_secs(1);
    let paint = |r: &mut Renderer, pixels: &mut Vec<u8>, at: Duration| {
        let t = PaintTarget::new(pixels, size, size.w * 4, Scale::ONE, 1).unwrap();
        r.paint(surface, &mut t.at(at))
    };
    paint(&mut r, &mut pixels, t0);
    // The spectrum is visible: the binary would tap the sink. A tone and
    // its overtones, the same every frame.
    let demand = r.take_feed_demand().expect("a spectrum to feed");
    assert_eq!(demand.len(), 1);
    assert_eq!(
        demand[0].kind,
        strand_render::FeedKind::Spectrum {
            device: "42".into()
        }
    );
    let spectrum = demand[0].node;
    let bands: Vec<f32> = (0..64)
        .map(|i| {
            let x = i as f32 / 63.0;
            (0.9 - x * 0.7) * (0.6 + 0.4 * (x * 19.0).sin().abs())
        })
        .collect();
    // The pill: the row holding the spectrum, which is 72 px wide as
    // written and 24 px tall by default.
    let pill = r.tree().get(spectrum).unwrap().parent.unwrap();
    let sb = r.boxes(surface).unwrap().rects[&spectrum];
    assert_eq!((sb.w, sb.h), (72.0, 24.0));
    let mut frames = 0u32;
    let mut at = t0;
    for (k, ms) in [(0u64, 0u64), (1, 500), (2, 1000)] {
        // Frames 16 ms apart up to the shot, fed each one.
        while at < t0 + Duration::from_millis(ms) {
            at = (at + Duration::from_millis(16)).min(t0 + Duration::from_millis(ms));
            r.feed(spectrum, &bands);
            let damage = paint(&mut r, &mut pixels, at);
            frames += 1;
            // Only the pill (and its glow's reach) repaints.
            let b = r.boxes(surface).unwrap().rects[&pill];
            let reach = 14.0;
            for d in damage.rects() {
                let (x, y) = (d.x as f32, d.y as f32);
                let (w, h) = (d.w as f32, d.h as f32);
                assert!(
                    x >= b.x - reach
                        && y >= b.y - reach
                        && x + w <= b.x + b.w + reach
                        && y + h <= b.y + b.h + reach,
                    "frame {frames}: damage {d:?} outside the pill {b:?}"
                );
            }
        }
        if k == 0 {
            r.feed(spectrum, &bands);
            paint(&mut r, &mut pixels, at);
        }
        assert!(r.wants_frame(surface), "playing: it animates");
        assert_matches_ref(&format!("rice_now_{ms}ms"), size, &pixels, 2);
    }

    // Paused: the cover and glow stop following time; the spectrum is
    // fed silence by the binary and rests.
    host.set(&rt, "media.playing", Value::Bool(false)).unwrap();
    assert!(r.apply(inst.flush().diff).is_empty());
    r.feed(spectrum, &[]);
    for _ in 0..120 {
        at += Duration::from_millis(16);
        paint(&mut r, &mut pixels, at);
    }
    let cover = |r: &Renderer| {
        r.tree()
            .get(pill)
            .unwrap()
            .children
            .iter()
            .copied()
            .find(|c| r.tree().get(*c).unwrap().kind == strand_scene::NodeKind::Image)
            .unwrap()
    };
    let c = r.boxes(surface).unwrap().rects[&cover(&r)];
    // The cover's middle 12 px, clear of the pill's border (which goes
    // on turning, and curves into the cover's box at the pill's end).
    let (cx, cy) = ((c.x + c.w / 2.0) as usize, (c.y + c.h / 2.0) as usize);
    let shot = |pixels: &[u8]| -> Vec<u8> {
        let mut out = Vec::new();
        for y in cy - 6..cy + 6 {
            let row = y * size.w as usize * 4;
            out.extend_from_slice(&pixels[row + (cx - 6) * 4..row + (cx + 6) * 4]);
        }
        out
    };
    let before = shot(&pixels);
    at += Duration::from_millis(500);
    paint(&mut r, &mut pixels, at);
    assert!(before == shot(&pixels), "paused: the cover holds still");
    assert!(r.wants_frame(surface), "the border's `t` keeps turning");
}

/// The glyph diff is linear: 50,000 cells (a quadratic diff is
/// 2.5 × 10^9 comparisons) with one changed at the end, in the
/// middle, and all changed, each in well under a frame even in a
/// debug build; the damage is the changed cells, or their box.
#[test]
fn glyph_damage_is_linear_in_the_glyphs() {
    use strand_scene::Rect;
    let cells: Vec<(Rect, u64)> = (0..50_000)
        .map(|i| (Rect::new((i % 200) * 10, (i / 200) * 16, 10, 16), i as u64))
        .collect();
    let start = Instant::now();
    // The last glyph.
    let mut b = cells.clone();
    b[49_999].1 = 7;
    let mut d = Damage::default();
    glyph_damage(&cells, &b, &mut d);
    assert_eq!(d.area(), 160, "{d:?}");
    // One in the middle.
    let mut b = cells.clone();
    b[25_000].1 = 7;
    let mut d = Damage::default();
    glyph_damage(&cells, &b, &mut d);
    assert_eq!(d.bounds(), Some(cells[25_000].0));
    // A glyph inserted near the start: everything after it moved.
    let mut b = cells.clone();
    b.insert(10, (Rect::new(1, 1, 10, 16), 9));
    for c in &mut b[11..] {
        c.0.x += 10;
    }
    let mut d = Damage::default();
    glyph_damage(&cells, &b, &mut d);
    assert!(d.bounds().is_some_and(|r| r.contains_rect(cells[30_000].0)));
    // Nothing changed.
    let mut d = Damage::default();
    glyph_damage(&cells, &cells, &mut d);
    assert!(d.is_empty());
    let took = start.elapsed();
    assert!(took < Duration::from_millis(250), "{took:?}");
}
use super::frame::glyph_damage;
use super::*;
use std::time::Duration;
use strand_scene::{Color, PropValue};
use strand_scene::{PaintTarget, Painter, SceneDiff, SceneOp};
use strand_text::{FontConfig, test_font_path};
use strand_text::{TextEngine, TextLayout};

fn engine() -> TextEngine {
    let data = std::fs::read(test_font_path()).unwrap();
    TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]))
}

fn renderer() -> Renderer {
    Renderer::new(TextBackend::Inline(Box::new(engine())))
}

/// The text state of `n` at scale 1 (tests show one width per node).
fn text_at(r: &Renderer, n: NodeId) -> &TextState {
    r.texts
        .iter()
        .find(|(s, _)| s.node == n && s.scale == Scale::ONE)
        .map(|(_, t)| t)
        .unwrap()
}

/// A bar with one full-width text aligned `align`.
fn aligned_text(text: &str, align: &str) -> (SceneDiff, NodeId, NodeId) {
    let (root, txt) = (NodeId::new(0, 0), NodeId::new(1, 0));
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .set(root, Prop::Color, PropValue::Color(Color::WHITE))
        .create(txt, NodeKind::Text, Some(root), 0)
        .set(txt, Prop::Text, PropValue::Text(text.into()))
        .set(
            txt,
            Prop::Width,
            PropValue::Length(strand_scene::Length::Percent(100.0)),
        )
        .set(txt, Prop::Align, PropValue::Keyword(align.into()));
    (d, root, txt)
}

fn paint_sized(r: &mut Renderer, id: u32, size: Size, age: u8) -> (Vec<u8>, Damage) {
    let mut px = vec![0u8; size.w as usize * size.h as usize * 4];
    let mut t = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, age).unwrap();
    let d = r.paint(SurfaceId(id), &mut t);
    (px, d)
}

/// The frame a renderer showing only this surface paints from scratch.
fn alone(diffs: &[SceneDiff], root: NodeId, size: Size) -> Vec<u8> {
    let mut r = renderer();
    for d in diffs {
        assert!(r.apply(d.clone()).is_empty());
    }
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), size, Scale::ONE);
    paint_sized(&mut r, 1, size, 0).0
}

/// One text node on two surfaces of the same scale but different widths
/// (a 2560 and a 1920 monitor at scale 1): alignment happens in the
/// line box, so each surface needs its own layout. Every frame on each
/// equals that surface painted alone, from the first and after a change.
#[test]
fn one_text_on_two_widths_at_one_scale_aligns_on_each() {
    for align in ["center", "end"] {
        let (wide, narrow) = (Size::new(200, 20), Size::new(120, 20));
        let (d, root, txt) = aligned_text("12:59", align);
        let mut r = renderer();
        assert!(r.apply(d.clone()).is_empty());
        r.attach_surface(SurfaceId(1), root);
        r.attach_surface(SurfaceId(2), root);
        r.configure_surface(SurfaceId(1), wide, Scale::ONE);
        r.configure_surface(SurfaceId(2), narrow, Scale::ONE);
        let (a, _) = paint_sized(&mut r, 1, wide, 0);
        let (b, _) = paint_sized(&mut r, 2, narrow, 0);
        assert!(
            a == alone(std::slice::from_ref(&d), root, wide),
            "{align}: wide boot"
        );
        assert!(
            b == alone(std::slice::from_ref(&d), root, narrow),
            "{align}: narrow boot"
        );
        assert_eq!(r.texts.len(), 1, "one unbounded layout for both widths");

        let mut tick = SceneDiff::new();
        tick.set(txt, Prop::Text, PropValue::Text("13:00".into()));
        assert!(r.apply(tick.clone()).is_empty());
        // Paint the narrow one first this time.
        let mut nb = b.clone();
        let mut t = PaintTarget::new(&mut nb, narrow, narrow.w * 4, Scale::ONE, 1).unwrap();
        let dn = r.paint(SurfaceId(2), &mut t);
        let mut na = a.clone();
        let mut t = PaintTarget::new(&mut na, wide, wide.w * 4, Scale::ONE, 1).unwrap();
        let dw = r.paint(SurfaceId(1), &mut t);
        let both = [d.clone(), tick];
        assert!(na == alone(&both, root, wide), "{align}: wide tick");
        assert!(nb == alone(&both, root, narrow), "{align}: narrow tick");
        assert!(dn.area() < narrow.w as u64 * 20 && dw.area() < wide.w as u64 * 20);
        assert_eq!(r.texts.len(), 1, "replaced layouts are not kept");
        r.detach_surface(SurfaceId(2));
        assert_eq!(r.texts.len(), 1);
    }
}

fn paint(r: &mut Renderer, px: &mut [u8], age: u8) -> Damage {
    let mut t = PaintTarget::new(px, Size::new(80, 20), 320, Scale::ONE, age).unwrap();
    r.paint(SurfaceId(1), &mut t)
}

/// After the worker restarts its engine, every mirrored page is stale:
/// the renderer drops them with all layouts, repaints in full and
/// re-requests its text, ending up with the same pixels.
#[test]
fn engine_reset_drops_layouts_and_reshapes() {
    let mut r = renderer();
    let (root, txt) = (NodeId::new(0, 0), NodeId::new(1, 0));
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .set(root, Prop::Color, PropValue::Color(Color::WHITE))
        .create(txt, NodeKind::Text, Some(root), 0)
        .set(txt, Prop::Text, PropValue::Text("12:59".into()));
    assert!(r.apply(d).is_empty());
    r.attach_surface(SurfaceId(1), root);
    let mut before = vec![0u8; 80 * 20 * 4];
    paint(&mut r, &mut before, 0);
    assert!(before.iter().any(|b| *b != 0));
    assert!(!r.wants_frame(SurfaceId(1)));

    // What the worker does: a fresh engine, then the reset marker.
    r.text = TextBackend::Inline(Box::new(engine()));
    r.deliver(TextLayout::reset(TextKey(0), Scale::ONE));
    assert!(r.texts.is_empty() && r.atlas.is_empty());
    assert!(r.wants_frame(SurfaceId(1)));
    let mut after = vec![0u8; 80 * 20 * 4];
    let d = paint(&mut r, &mut after, 1);
    assert_eq!(d, Damage::full(Size::new(80, 20)));
    assert!(before == after);
}

fn texts_diff(texts: &[(u32, &str)]) -> (SceneDiff, NodeId) {
    let root = NodeId::new(0, 0);
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .set(root, Prop::Color, PropValue::Color(Color::WHITE));
    for (i, t) in texts {
        let id = NodeId::new(*i, 0);
        d.create(id, NodeKind::Text, Some(root), u32::MAX).set(
            id,
            Prop::Text,
            PropValue::Text((*t).into()),
        );
    }
    (d, root)
}

fn worker() -> TextBackend {
    let data = std::fs::read(test_font_path()).unwrap();
    TextBackend::Worker(
        strand_text::TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap(),
    )
}

/// Text the worker delivers while a content-sized surface is being
/// painted resizes it: the paint refreshes its spec first and draws
/// nothing at the old size, holding the frame for the configure.
#[test]
fn text_collected_while_painting_holds_a_resized_surface() {
    let (root, txt) = (NodeId::new(0, 0), NodeId::new(1, 0));
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Panel, None, 0)
        .set(root, Prop::Color, PropValue::Color(Color::WHITE))
        .create(txt, NodeKind::Text, Some(root), 0)
        .set(txt, Prop::Text, PropValue::Text("short".into()));
    let mut r = Renderer::new(worker());
    r.set_resize_wait(Duration::from_secs(30));
    assert!(r.apply(d).is_empty());
    assert!(r.wait_for_text(Duration::from_secs(10)));
    r.update();
    let size = |r: &Renderer| {
        let s = r.surface_spec(root).unwrap();
        Size::new(s.width.unwrap() as u32, s.height.unwrap() as u32)
    };
    let first = size(&r);
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), first, Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    r.update();
    assert!(!paint_sized(&mut r, 1, first, 0).1.is_empty());
    r.take_surface_changes();
    let mut d = SceneDiff::new();
    d.set(
        txt,
        Prop::Text,
        PropValue::Text("a much longer line of text".into()),
    );
    r.apply(d);
    // Shaped meanwhile, collected by the paint.
    std::thread::sleep(Duration::from_millis(300));
    let (_, damage) = paint_sized(&mut r, 1, first, 1);
    assert!(damage.is_empty(), "painted at the old size");
    assert!(r.frame_deadline(SurfaceId(1)).is_some());
    assert!(size(&r).w > first.w);
    assert!(!r.take_surface_changes().is_empty());
}

/// A request that crashes the worker's engine every time is not asked
/// for again after the reset, so the worker does not restart its
/// engine (and every text reshape) in a loop.
#[test]
fn crashing_requests_are_not_retried() {
    let mut r = Renderer::new(worker());
    let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
    let (d, root) = texts_diff(&[(1, "crash"), (2, "fine")]);
    assert!(r.apply(d).is_empty());
    r.attach_surface(SurfaceId(1), root);
    // Sized without `configure_surface`: its `update` polls the worker,
    // which may already have answered, leaving no request in flight to
    // crash.
    r.surfaces
        .get_mut(&SurfaceId(1))
        .unwrap()
        .resize(Size::new(80, 20), Scale::ONE);
    let key_of = |r: &Renderer, n: NodeId| text_at(r, n).requested.as_ref().unwrap().0;
    // Flattening without polling the worker: what `update` would ask.
    let ask = |r: &mut Renderer| {
        r.surfaces.get_mut(&SurfaceId(1)).unwrap().cache = None;
        r.flatten_surface(SurfaceId(1));
    };
    ask(&mut r);
    let ka = key_of(&r, a);
    r.deliver(TextLayout::reset(ka, Scale::ONE));
    ask(&mut r);
    assert!(text_at(&r, a).poisoned);
    assert!(text_at(&r, a).requested.is_none(), "not asked again");
    // A second crash (here: the other text) keeps the first culprit
    // poisoned too.
    let kb = key_of(&r, b);
    r.deliver(TextLayout::reset(kb, Scale::ONE));
    ask(&mut r);
    assert!(r.pending.is_empty(), "{:?}", r.pending);
    assert!(text_at(&r, a).poisoned && text_at(&r, b).poisoned);
    // New text for the culprit is asked for.
    let mut d = SceneDiff::new();
    d.set(a, Prop::Text, PropValue::Text("other".into()));
    r.apply(d);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    assert!(text_at(&r, a).layout.is_some());
    assert!(!text_at(&r, a).poisoned);
}

/// A poisoned slot never gets a layout, so it does not hold on to the
/// node's layouts at other widths as stand-ins: when the other surface
/// goes, its layout goes too.
#[test]
fn a_poisoned_slot_keeps_no_stand_ins() {
    let mut r = Renderer::new(worker());
    let (mut d, root, txt) = aligned_text(
        "a window title much too long to fit on either of the two bars",
        "center",
    );
    d.set(txt, Prop::Ellipsis, PropValue::Keyword("end".into()));
    assert!(r.apply(d).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.attach_surface(SurfaceId(2), root);
    r.configure_surface(SurfaceId(1), Size::new(200, 20), Scale::ONE);
    r.configure_surface(SurfaceId(2), Size::new(120, 20), Scale::ONE);
    // The unbounded layout first: then each width asks for its own.
    let start = std::time::Instant::now();
    while r
        .texts
        .iter()
        .all(|(s, t)| s.width.is_some() || t.layout.is_none())
    {
        r.poll_text();
        std::thread::sleep(Duration::from_millis(1));
        assert!(start.elapsed() < Duration::from_secs(10));
    }
    for id in [1, 2] {
        r.surfaces.get_mut(&SurfaceId(id)).unwrap().cache = None;
        r.flatten_surface(SurfaceId(id));
    }
    // The engine crashes on the wide one's request.
    let wide = Some(200f32.to_bits());
    let key = r
        .texts
        .iter()
        .find(|(s, _)| s.node == txt && s.width == wide)
        .and_then(|(_, t)| t.requested.as_ref())
        .unwrap()
        .0;
    r.deliver(TextLayout::reset(key, Scale::ONE));
    for id in [1, 2] {
        r.surfaces.get_mut(&SurfaceId(id)).unwrap().cache = None;
        r.flatten_surface(SurfaceId(id));
    }
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let slot = |r: &Renderer, w: f32| {
        r.texts
            .iter()
            .find(|(s, _)| s.width == Some(w.to_bits()))
            .map(|(_, t)| (t.poisoned, t.layout.is_some()))
    };
    assert_eq!(slot(&r, 200.0), Some((true, false)));
    assert_eq!(slot(&r, 120.0), Some((false, true)));
    r.update();
    let s1 = &r.surfaces[&SurfaceId(1)];
    assert!(s1.cache.is_some());
    r.detach_surface(SurfaceId(2));
    assert_eq!(slot(&r, 120.0), None, "kept as a stand-in for nothing");
    assert_eq!(
        r.texts.len(),
        2,
        "the unbounded layout and the poisoned one"
    );
    // Surface 1 drew that layout as its stand-in: it must not keep
    // showing glyphs that are gone.
    let s1 = &r.surfaces[&SurfaceId(1)];
    assert!(s1.dirty && s1.cache.is_none(), "stale stand-in kept");
}

/// A layout missing glyphs for want of atlas room is retried a bounded
/// number of times, and again once other text frees pages.
#[test]
fn incomplete_text_retries_without_looping() {
    let data = std::fs::read(test_font_path()).unwrap();
    let mut cfg = FontConfig::isolated(vec![Arc::new(data)]);
    cfg.atlas = strand_text::AtlasConfig {
        page_size: 64,
        max_pages: 1,
        max_bytes: 64 * 64,
    };
    let mut r = Renderer::new(TextBackend::Inline(Box::new(TextEngine::new(cfg))));
    let (b, a) = (NodeId::new(1, 0), NodeId::new(2, 0));
    let (mut d, root) = texts_diff(&[(1, "WXYZ"), (2, "ABCD")]);
    let big = PropValue::Font(strand_scene::Font {
        size: 40.0,
        ..strand_scene::Font::default()
    });
    d.set(root, Prop::Font, big);
    assert!(r.apply(d).is_empty());
    r.attach_surface(SurfaceId(1), root);
    let mut px = vec![0u8; 200 * 40 * 4];
    let mut t = PaintTarget::new(&mut px, Size::new(200, 40), 800, Scale::ONE, 0).unwrap();
    r.paint(SurfaceId(1), &mut t);
    let state = |r: &Renderer, n| text_at(r, n).incomplete();
    let n = |r: &Renderer, x| text_at(r, x).layout.as_ref().unwrap().glyphs().count();
    assert!(
        !state(&r, b) && state(&r, a),
        "B fills the only page: {} {} {} {}",
        state(&r, b),
        state(&r, a),
        n(&r, b),
        n(&r, a)
    );
    // Two retries, then nothing more however often it is flattened.
    let keys = r.next_key;
    assert_eq!(keys, 1 + 2 + MAX_TEXT_RETRIES as u64);
    for _ in 0..3 {
        r.invalidate(SurfaceId(1));
        r.surfaces.get_mut(&SurfaceId(1)).unwrap().cache = None;
        r.update();
    }
    assert_eq!(r.next_key, keys, "no hot loop");
    // B goes: its page frees and A completes.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove { id: b });
    r.apply(d);
    assert!(!state(&r, a));
    assert_eq!(text_at(&r, a).layout.as_ref().unwrap().glyphs().count(), 4);
}

/// Several outputs of different sizes keep a context per cell size
/// instead of rebuilding edge-cell contexts every frame.
#[test]
fn contexts_cover_several_outputs() {
    let mut r = renderer();
    let mut d = SceneDiff::new();
    let sizes = [Size::new(300, 70), Size::new(280, 40), Size::new(270, 100)];
    for i in 0..3u32 {
        let id = NodeId::new(i, 0);
        d.create(id, NodeKind::Bar, None, i)
            .set(id, Prop::Bg, PropValue::Color(Color::WHITE));
        r.attach_surface(SurfaceId(i), id);
    }
    r.apply(d);
    for _ in 0..2 {
        for (i, size) in sizes.iter().enumerate() {
            let mut px = vec![0u8; (size.w * size.h * 4) as usize];
            let mut t = PaintTarget::new(&mut px, *size, size.w * 4, Scale::ONE, 0).unwrap();
            r.paint(SurfaceId(i as u32), &mut t);
        }
    }
    assert_eq!(r.raster.contexts(), 9, "every cell size is kept");
    r.detach_surface(SurfaceId(2));
    r.detach_surface(SurfaceId(1));
    assert!(r.raster.contexts() <= 8);
}

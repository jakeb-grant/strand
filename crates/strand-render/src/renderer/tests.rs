/// The glyph diff is linear: 50,000 cells (a quadratic diff is
/// 2.5 × 10^9 comparisons) with one changed at the end, in the
/// middle, and all changed, each compared a few times per cell at most
/// (counted, not timed: a debug build on a shared runner has no clock
/// to trust; m4-audit); the damage is the changed cells, or their box.
#[test]
fn glyph_damage_is_linear_in_the_glyphs() {
    use std::cell::Cell;
    use strand_scene::Rect;
    thread_local! {
        static COMPARED: Cell<usize> = const { Cell::new(0) };
    }
    /// A cell that counts how often it is compared.
    #[derive(Clone, Copy)]
    struct Counted(Rect, u64);
    impl PartialEq for Counted {
        fn eq(&self, o: &Self) -> bool {
            COMPARED.with(|c| c.set(c.get() + 1));
            self.0 == o.0 && self.1 == o.1
        }
    }
    impl super::frame::GlyphCell for Counted {
        fn rect(&self) -> Rect {
            self.0
        }
    }
    const N: usize = 50_000;
    // Linear: the prefix and the suffix each compare a cell once, and a
    // middle of at most GLYPH_CELLS_COMPARED cells per side is compared
    // pairwise both ways.
    let small = super::frame::GLYPH_CELLS_COMPARED;
    let bound = 2 * (N + 1) + 2 * small * small;
    let diff = |a: &[Counted], b: &[Counted]| {
        COMPARED.with(|c| c.set(0));
        let mut d = Damage::default();
        glyph_damage(a, b, &mut d);
        let n = COMPARED.with(Cell::get);
        assert!(n <= bound, "{n} comparisons for {N} cells (bound {bound})");
        d
    };
    let cells: Vec<Counted> = (0..N)
        .map(|i| {
            Counted(
                Rect::new((i % 200) as i32 * 10, (i / 200) as i32 * 16, 10, 16),
                i as u64,
            )
        })
        .collect();
    // The last glyph.
    let mut b = cells.clone();
    b[N - 1].1 = 7;
    let d = diff(&cells, &b);
    assert_eq!(d.area(), 160, "{d:?}");
    // One in the middle.
    let mut b = cells.clone();
    b[25_000].1 = 7;
    let d = diff(&cells, &b);
    assert_eq!(d.bounds(), Some(cells[25_000].0));
    // A glyph inserted near the start: everything after it moved.
    let mut b = cells.clone();
    b.insert(10, Counted(Rect::new(1, 1, 10, 16), 9));
    for c in &mut b[11..] {
        c.0.x += 10;
    }
    let d = diff(&cells, &b);
    assert!(d.bounds().is_some_and(|r| r.contains_rect(cells[30_000].0)));
    // Nothing changed.
    let d = diff(&cells, &cells);
    assert!(d.is_empty());
    // The real cells take the same path.
    let real: Vec<(Rect, u64)> = cells.iter().map(|c| (c.0, c.1)).collect();
    let mut b = real.clone();
    b[N - 1].1 = 7;
    let mut d = Damage::default();
    glyph_damage(&real, &b, &mut d);
    assert_eq!(d.area(), 160, "{d:?}");
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

/// (owner-testing.md bug 17) A surface's first-frame and size holds wait
/// for the text worker and the compositor's configure, which the loop
/// cannot take while it paints another surface: a paint moves on every
/// other surface's hold still running when it started by the time it
/// took, and leaves its own and one already over alone. Before, under
/// load, a toast panel's first frame was held 500 ms for its configure
/// at the size its shaped text gave it, the loop spent those painting
/// the bar and the launcher, and it showed one frame at the size its
/// estimated text had given it
/// (crates/strand/tests/demo.rs::the_design_launcher_is_centred_and_closes_on_click_away).
#[test]
fn painting_another_surface_does_not_run_out_a_hold() {
    let mut r = renderer();
    let (a, b, c) = (NodeId::new(0, 0), NodeId::new(1, 0), NodeId::new(2, 0));
    let mut d = SceneDiff::new();
    for id in [a, b, c] {
        d.create(id, NodeKind::Panel, None, 0);
    }
    assert!(r.apply(d).is_empty());
    for (i, root) in [a, b, c].into_iter().enumerate() {
        r.attach_surface(SurfaceId(i as u32 + 1), root);
    }
    let now = Instant::now();
    let (wait, spent) = (Duration::from_millis(40), Duration::from_millis(100));
    let size = LogicalSize::new(10.0, 10.0);
    for (id, until) in [(1, now + wait), (2, now + wait), (3, now - spent * 2)] {
        let s = r.surfaces.get_mut(&SurfaceId(id)).unwrap();
        s.wait_until = Some(until);
        s.size_hold = Some((size, until));
    }
    // Surface 1 painted for 100 ms, started before both holds ran out.
    let started = now - spent;
    r.pause_holds(SurfaceId(1), started);
    let hold = |r: &Renderer, id| {
        let s = &r.surfaces[&SurfaceId(id)];
        (s.wait_until.unwrap(), s.size_hold.unwrap().1)
    };
    let moved = hold(&r, 2);
    assert!(moved.0 >= now + wait + spent && moved.1 >= now + wait + spent);
    assert!(
        moved.0 < now + wait + spent * 2,
        "moved by the paint's time"
    );
    assert_eq!(hold(&r, 1), (now + wait, now + wait), "its own hold");
    assert_eq!(hold(&r, 3), (now - spent * 2, now - spent * 2), "one over");
    // A painted surface's first-frame wait is over: only its size hold
    // waits for a configure.
    r.surfaces.get_mut(&SurfaceId(2)).unwrap().painted = true;
    r.pause_holds(SurfaceId(1), Instant::now() - spent);
    let after = hold(&r, 2);
    assert_eq!(after.0, moved.0);
    assert!(after.1 > moved.1);
}

/// (m6-owner-fixes review) Other surfaces painting back to back move a
/// hold on by at most `MAX_HOLD_PAUSE` in all: a size hold whose
/// configure never matches (a compositor clamping the size) runs out
/// while another surface animates, rather than holding the panel blank
/// until the animation stops.
#[test]
fn back_to_back_paints_move_a_hold_on_at_most_max_hold_pause() {
    let mut r = renderer();
    let (a, b) = (NodeId::new(0, 0), NodeId::new(1, 0));
    let mut d = SceneDiff::new();
    for id in [a, b] {
        d.create(id, NodeKind::Panel, None, 0);
    }
    assert!(r.apply(d).is_empty());
    for (i, root) in [a, b].into_iter().enumerate() {
        r.attach_surface(SurfaceId(i as u32 + 1), root);
    }
    let size = LogicalSize::new(10.0, 10.0);
    let until = Instant::now() + Duration::from_millis(50);
    let first = until;
    {
        let s = r.surfaces.get_mut(&SurfaceId(2)).unwrap();
        s.wait_until = Some(until);
        s.size_hold = Some((size, until));
    }
    // Surface 1 paints for 100 ms at a time, one paint after another,
    // each starting before the hold (as moved) runs out.
    let spent = Duration::from_millis(100);
    for _ in 0..40 {
        r.pause_holds(SurfaceId(1), Instant::now() - spent);
    }
    let s = &r.surfaces[&SurfaceId(2)];
    let (wait, held) = (s.wait_until.unwrap(), s.size_hold.unwrap().1);
    assert!(
        held <= first + MAX_HOLD_PAUSE && wait <= first + MAX_HOLD_PAUSE,
        "moved on by {:?} and {:?}",
        held - first,
        wait - first
    );
    assert!(held >= first + MAX_HOLD_PAUSE - spent, "paused at all");
}

/// A request that crashes the worker's engine every time is not asked
/// for again after the reset, so the worker does not restart its
/// engine (and every text reshape) in a loop.
#[test]
fn crashing_requests_are_not_retried() {
    let mut r = Renderer::new(worker());
    // Flattening waits for no new text here (`ESTIMATE_WAIT`): the
    // requests must stay in flight for the test to answer them.
    r.set_estimate_wait(Duration::ZERO);
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
    // Flattening waits for no new text here (`ESTIMATE_WAIT`): the wide
    // request must stay in flight for the test to crash it, and an
    // optimised worker answers within the wait.
    r.set_estimate_wait(Duration::ZERO);
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
    d.push(SceneOp::Remove {
        id: b,
        window: false,
    });
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

/// A `list` 40 px tall with `stagger: 100ms` whose 40 entering rows run
/// past its viewport: the rows out of view are numbered with the rest
/// but never drawn, so once the rows in view have played nothing under
/// the surface is busy (its scene cache is reused, a content-sized
/// surface may shrink) long before the last row's turn at 4 s.
#[test]
fn a_stagger_past_the_viewport_leaves_nothing_busy() {
    let (root, list) = (NodeId::new(1, 0), NodeId::new(2, 0));
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .create(list, NodeKind::List, Some(root), 0)
        .set(list, Prop::Height, PropValue::Number(40.0))
        .set(
            list,
            Prop::Stagger,
            PropValue::Duration(Duration::from_millis(100)),
        );
    let mut r = renderer();
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    let size = Size::new(120, 40);
    r.attach_surface(s, root);
    r.configure_surface(s, size, Scale::ONE);
    let mut px = vec![0u8; 120 * 40 * 4];
    let mut paint = |r: &mut Renderer, ms: u64| {
        let t = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, 1).unwrap();
        let mut t = t.at(Duration::from_millis(ms));
        r.paint(s, &mut t);
    };
    paint(&mut r, 1000);
    let mut d = SceneDiff::new();
    let rows: Vec<NodeId> = (0..40).map(|i| NodeId::new(100 + i, 0)).collect();
    for (i, n) in rows.iter().enumerate() {
        d.create(*n, NodeKind::Row, Some(list), i as u32)
            .set(*n, Prop::Height, PropValue::Number(20.0))
            .set(*n, Prop::Bg, PropValue::Color(Color::WHITE))
            .set(
                *n,
                Prop::Enter,
                PropValue::Pose(vec![(Prop::Opacity, PropValue::Number(0.0))]),
            );
    }
    assert!(r.apply(d).is_empty());
    paint(&mut r, 1016);
    assert!(r.anim.busy(&r.tree, root), "the second row waits its turn");
    let mut t = 1016;
    while r.anim.busy(&r.tree, root) || r.animating(s) {
        t += 16;
        paint(&mut r, t);
        assert!(t < 3000, "still busy at {t} ms");
    }
}

/// The shell's renderer refuses to start when its image worker cannot:
/// decodes and file reads would otherwise run on the render thread with
/// jank as the only sign.
#[test]
fn an_image_worker_that_cannot_start_is_an_error_not_inline_decoding() {
    use crate::image::ImageBackend;
    let text = TextBackend::Worker(strand_text::TextWorker::spawn(Default::default()).unwrap());
    let (images, err) = image_backend(&text, |_| Err(std::io::Error::other("no threads")));
    assert!(matches!(images, ImageBackend::Inline(_)));
    assert_eq!(err.map(|e| e.to_string()).as_deref(), Some("no threads"));
    let (images, err) = image_backend(&text, spawn_images);
    assert!(matches!(images, ImageBackend::Worker(_)) && err.is_none());
    assert!(Renderer::try_new(text).is_ok());
    // An inline text backend decodes inline by design (offline renders).
    let inline = TextBackend::Inline(Box::new(strand_text::TextEngine::new(Default::default())));
    let (images, err) = image_backend(&inline, |_| Err(std::io::Error::other("unused")));
    assert!(matches!(images, ImageBackend::Inline(_)) && err.is_none());
}

/// (late-text-dip) The owner's crown status row, reduced (as
/// tests/morph.rs STATUS_ROW): a 300 × 30 bar whose `split` packs a row
/// (gap 6, centred) to its end, holding a 16 × 12 green box. Returns the
/// diff, the bar and the row.
fn status_row() -> (SceneDiff, NodeId, NodeId) {
    let n = |i| NodeId::new(i, 0);
    let (root, split, end, row, bars) = (n(0), n(1), n(2), n(3), n(4));
    let num = PropValue::Number;
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .set(root, Prop::Height, num(30.0))
        .create(split, NodeKind::Split, Some(root), 0)
        .set(split, Prop::Width, num(300.0))
        .set(split, Prop::Height, num(30.0))
        .create(end, NodeKind::End, Some(split), 0)
        .set(end, Prop::Align, PropValue::Keyword("center".into()))
        .create(row, NodeKind::Row, Some(end), 0)
        .set(row, Prop::Gap, num(6.0))
        .set(row, Prop::Align, PropValue::Keyword("center".into()))
        .set(row, Prop::Shrink, num(0.0))
        .create(bars, NodeKind::Box, Some(row), 0)
        .set(bars, Prop::Width, num(16.0))
        .set(bars, Prop::Height, num(12.0))
        .set(
            bars,
            Prop::Bg,
            PropValue::Color(Color::from_hex("#00ff00").unwrap()),
        );
    (d, root, row)
}

/// (late-text-dip) A text "iiii" in magenta (narrow letters: its old
/// estimate, 0.55 em a letter, is far from its shaped size), with
/// `props`, mounted at the end of `parent`.
fn late_text(d: &mut SceneDiff, id: NodeId, parent: NodeId, props: &[(Prop, PropValue)]) {
    d.create(id, NodeKind::Text, Some(parent), u32::MAX)
        .set(id, Prop::Text, PropValue::Text("iiii".into()))
        .set(
            id,
            Prop::Color,
            PropValue::Color(Color::from_hex("#ff00ff").unwrap()),
        );
    for (p, v) in props {
        d.set(id, *p, v.clone());
    }
}

/// A box of pixels: left, top, right, bottom (inclusive).
type Px4 = (u32, u32, u32, u32);

/// The box of the pixels close to `want` (straight RGB) with an alpha
/// over `least` in a 300 × 30 premultiplied BGRA frame, and the largest
/// alpha among them.
fn pixels_of(px: &[u8], want: [u8; 3], least: u8) -> Option<(Px4, u8)> {
    let close = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() < 60;
    let mut out: Option<(Px4, u8)> = None;
    for y in 0..30u32 {
        for x in 0..300u32 {
            let i = ((y * 300 + x) * 4) as usize;
            let (b, g, r, a) = (px[i], px[i + 1], px[i + 2], px[i + 3]);
            if a <= least {
                continue;
            }
            // Back to straight colour.
            let s = |c: u8| (u32::from(c) * 255 / u32::from(a)).min(255) as u8;
            if close(s(r), want[0]) && close(s(g), want[1]) && close(s(b), want[2]) {
                out = Some(match out {
                    None => ((x, y, x, y), a),
                    Some(((x0, y0, x1, y1), m)) => {
                        ((x0.min(x), y0.min(y), x1.max(x), y1.max(y)), m.max(a))
                    }
                });
            }
        }
    }
    out
}

/// The box of the pixels close to `want`, faint edges left out.
fn bbox_of(px: &[u8], want: [u8; 3]) -> Option<Px4> {
    pixels_of(px, want, 40).map(|(b, _)| b)
}

/// `inner` lies inside `outer`.
fn inside(inner: Px4, outer: Px4) -> bool {
    inner.0 >= outer.0 && inner.1 >= outer.1 && inner.2 <= outer.2 && inner.3 <= outer.3
}

/// (late-text-dip) A renderer on the text worker that never waits for
/// it, showing `diff` on a 300 × 30 bar, painted once at rest. Its
/// frames are 60 Hz from `frame`.
fn late_bar(diff: SceneDiff, root: NodeId) -> (Renderer, Vec<u8>, u32) {
    let mut r = Renderer::new(worker());
    r.set_first_frame_wait(Duration::ZERO);
    r.set_new_text_wait(Duration::ZERO);
    r.set_estimate_wait(Duration::ZERO);
    r.set_busy_window(Duration::ZERO);
    assert!(r.apply(diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(300, 30), Scale::ONE);
    let mut px = vec![0u8; 300 * 30 * 4];
    late_paint(&mut r, &mut px, 0);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut k = 1;
    while r.wants_frame(SurfaceId(1)) {
        late_paint(&mut r, &mut px, k);
        k += 1;
        assert!(k < 200, "never settled");
    }
    (r, px, k)
}

fn late_paint(r: &mut Renderer, px: &mut [u8], k: u32) {
    let at = Duration::from_secs(1) + Duration::from_micros(16_667 * u64::from(k));
    let mut t = PaintTarget::new(px, Size::new(300, 30), 1200, Scale::ONE, 1)
        .unwrap()
        .at(at);
    r.paint(SurfaceId(1), &mut t);
}

/// (late-text-dip) Applies `diff`, which mounts text, and paints its
/// frames with the text later than the first: the worker's answer is
/// taken before the renderer sees it (a worker slower than the frame's
/// waits, as a font family's first load is) and delivered after that
/// frame, as the runtime's `text_ready` would. Returns each frame's
/// green and magenta boxes and the magenta's largest alpha, from that
/// first frame to rest.
fn mount_late(
    r: &mut Renderer,
    px: &mut [u8],
    mut k: u32,
    diff: SceneDiff,
) -> Vec<(Option<Px4>, Option<Px4>, u8)> {
    assert!(r.apply(diff).is_empty());
    assert!(!r.pending.is_empty(), "the text is asked for");
    let TextBackend::Worker(w) = &r.text else {
        panic!("a worker");
    };
    let held = w
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .expect("the worker answers");
    let mut seen = Vec::new();
    let mut late = Some(held);
    loop {
        late_paint(r, px, k);
        k += 1;
        seen.push((
            bbox_of(px, [0, 255, 0]),
            bbox_of(px, [255, 0, 255]),
            pixels_of(px, [255, 0, 255], 4).map_or(0, |(_, a)| a),
        ));
        if let Some(l) = late.take() {
            r.deliver(l);
            assert!(r.wait_for_text(Duration::from_secs(10)));
        }
        if !r.wants_frame(SurfaceId(1)) {
            break;
        }
        assert!(seen.len() < 200, "never settled");
    }
    seen
}

/// (late-text-dip) Text mounted beside a box, with its layout later than
/// the frame that mounts it, moves the box once and never off its level.
/// That frame was laid out by the text's estimate (four narrow letters
/// as lines wrapped in its 14 px): the row grew to three lines, the box's
/// layout dropped 9 px for that frame and its glide dipped toward it and
/// back (eval/scenarios/regressions/bug24-flip-open-late1.scn). Text with
/// no layout takes no room of its own now: the box glides left on one
/// level, and the text appears where it rests, never sliding in from
/// where its box was before it was shaped.
#[test]
fn a_late_text_moves_its_siblings_once() {
    let (d, root, row) = status_row();
    let (mut r, mut px, k) = late_bar(d, root);
    let rest = bbox_of(&px, [0, 255, 0]).expect("the box is drawn");
    let mut d = SceneDiff::new();
    let num = PropValue::Number;
    late_text(
        &mut d,
        NodeId::new(5, 0),
        row,
        &[(Prop::Width, num(14.0)), (Prop::Shrink, num(0.0))],
    );
    let seen = mount_late(&mut r, &mut px, k, d);
    let end = bbox_of(&px, [0, 255, 0]).expect("the box is drawn");
    let glyph = bbox_of(&px, [255, 0, 255]).expect("the text is drawn");
    assert!(end.0 < rest.0, "no room made: {rest:?} to {end:?}");
    assert!(seen[0].2 == 0, "the first frame has no text: {seen:?}");
    for (k, (b, m, _)) in seen.iter().enumerate() {
        let b = b.unwrap_or_else(|| panic!("frame {k}: the box left the bar: {seen:?}"));
        assert!(
            (b.1, b.3) == (rest.1, rest.3),
            "frame {k}: the box left its level: {seen:?}"
        );
        assert!(
            b.0 + 3 >= end.0 && b.0 <= rest.0 + 3,
            "frame {k}: the box went past its glide: {seen:?}"
        );
        if let Some(m) = m {
            assert!(*m == glyph, "frame {k}: the text off its place: {seen:?}");
        }
    }
}

/// (late-text-dip) A late text whose box is nothing across until it is
/// shaped (no `width`) takes no room, not even the row's gap beside it:
/// on the frame laid out before it arrives the box stays where it was,
/// then glides left once, on its level.
#[test]
fn a_late_text_takes_no_room_until_it_arrives() {
    let (d, root, row) = status_row();
    let (mut r, mut px, k) = late_bar(d, root);
    let rest = bbox_of(&px, [0, 255, 0]).expect("the box is drawn");
    let mut d = SceneDiff::new();
    late_text(&mut d, NodeId::new(5, 0), row, &[]);
    let seen = mount_late(&mut r, &mut px, k, d);
    let end = bbox_of(&px, [0, 255, 0]).expect("the box is drawn");
    assert!(end.0 < rest.0, "no room made: {rest:?} to {end:?}");
    assert_eq!(
        seen[0].0,
        Some(rest),
        "the box moved before the text arrived: {seen:?}"
    );
    for (k, (b, _, _)) in seen.iter().enumerate() {
        let b = b.unwrap_or_else(|| panic!("frame {k}: the box left the bar: {seen:?}"));
        assert!(
            (b.1, b.3) == (rest.1, rest.3) && b.0 + 3 >= end.0 && b.0 <= rest.0,
            "frame {k}: the box left its glide: {seen:?}"
        );
    }
}

/// (late-text-dip) A row holding only a late text is laid out empty
/// until the text arrives: drawn as nothing there, it has no place to
/// glide from, and the text appears where it rests (it slid down 7 px
/// from the empty row's centre). An `enter` pose on it holds until its
/// text is drawn: the fade plays from its start then, not from a frame
/// that showed nothing.
#[test]
fn a_box_around_a_late_text_shows_it_whole() {
    for enter in [false, true] {
        let (d, root, row) = status_row();
        let (mut r, mut px, k) = late_bar(d, root);
        let (wrap, txt) = (NodeId::new(5, 0), NodeId::new(6, 0));
        let mut d = SceneDiff::new();
        d.create(wrap, NodeKind::Row, Some(row), u32::MAX)
            .set(wrap, Prop::Gap, PropValue::Number(3.0))
            .set(wrap, Prop::Align, PropValue::Keyword("center".into()));
        if enter {
            d.set(
                wrap,
                Prop::Enter,
                PropValue::Pose(vec![(Prop::Opacity, PropValue::Number(0.0))]),
            );
        }
        late_text(&mut d, txt, wrap, &[]);
        let seen = mount_late(&mut r, &mut px, k, d);
        let (glyph, full) = pixels_of(&px, [255, 0, 255], 40).expect("the text is drawn");
        // Faint edges fade below the cut while it fades in: what shows
        // lies inside where it rests.
        for (k, (_, m, _)) in seen.iter().enumerate() {
            if let Some(m) = m {
                assert!(
                    inside(*m, glyph),
                    "enter {enter}, frame {k}: text off its place: {seen:?}"
                );
            }
        }
        if enter {
            // The fade's first frame shows a sixth of the way at 60 Hz;
            // one that started on the frame before, past a third.
            let first = seen
                .iter()
                .find(|(_, _, a)| *a > 0)
                .map_or(0, |(_, _, a)| *a);
            assert!(
                first > 0 && f32::from(first) < 0.25 * f32::from(full),
                "the text showed {first} of {full} on its first frame: {seen:?}"
            );
        }
    }
}

/// (late-text-dip) A paint keeps as never drawn only what waits for a
/// late text: the text, and a box laid out empty around it. An empty box
/// mounted beside it with nothing to wait for was drawn as what it is,
/// and leaves the set with the paint, as a node does once unmounted.
#[test]
fn only_what_waits_for_late_text_stays_unpainted() {
    let (d, root, row) = status_row();
    let (mut r, mut px, k) = late_bar(d, root);
    let (wrap, txt, empty) = (NodeId::new(5, 0), NodeId::new(6, 0), NodeId::new(7, 0));
    let mut d = SceneDiff::new();
    d.create(wrap, NodeKind::Row, Some(row), u32::MAX).create(
        empty,
        NodeKind::Row,
        Some(row),
        u32::MAX,
    );
    late_text(&mut d, txt, wrap, &[]);
    assert!(r.apply(d).is_empty());
    let TextBackend::Worker(w) = &r.text else {
        panic!("a worker");
    };
    let held = w
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .expect("the worker answers");
    late_paint(&mut r, &mut px, k);
    let unpainted = &r.surfaces[&SurfaceId(1)].unpainted;
    assert!(
        unpainted.contains(&txt) && unpainted.contains(&wrap),
        "the late text and its empty box wait: {unpainted:?}"
    );
    assert!(
        !unpainted.contains(&empty),
        "an empty box with nothing to wait for was drawn: {unpainted:?}"
    );
    r.deliver(held);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut k = k + 1;
    while r.wants_frame(SurfaceId(1)) {
        late_paint(&mut r, &mut px, k);
        k += 1;
        assert!(k < 400, "never settled");
    }
    let unpainted = &r.surfaces[&SurfaceId(1)].unpainted;
    assert!(unpainted.is_empty(), "drawn at rest: {unpainted:?}");
}

/// (late-text-dip) A box that clips its content, holding only text, is
/// laid out empty until the text is shaped, so it clips everything
/// under it: its text is still asked for (as a hidden node's is), or it
/// would never be shaped and the box never show it.
#[test]
fn text_in_a_box_clipped_to_nothing_is_shaped() {
    let (d, root, row) = status_row();
    let mut r = renderer();
    assert!(r.apply(d).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(300, 30), Scale::ONE);
    let mut px = vec![0u8; 300 * 30 * 4];
    late_paint(&mut r, &mut px, 0);
    let (clip, txt) = (NodeId::new(5, 0), NodeId::new(6, 0));
    let mut d = SceneDiff::new();
    d.create(clip, NodeKind::Row, Some(row), u32::MAX)
        .set(clip, Prop::Clip, PropValue::Bool(true))
        .set(
            clip,
            Prop::Enter,
            PropValue::Pose(vec![(Prop::Opacity, PropValue::Number(0.0))]),
        );
    late_text(&mut d, txt, clip, &[]);
    // With motion reduced the pose is not played: the box is drawn, not
    // hidden (which asks for its texts), from its first frame.
    r.set_reduced_motion(true);
    assert!(r.apply(d).is_empty());
    let mut k = 1;
    while r.wants_frame(SurfaceId(1)) {
        late_paint(&mut r, &mut px, k);
        k += 1;
        assert!(k < 200, "never settled");
    }
    assert!(
        bbox_of(&px, [255, 0, 255]).is_some(),
        "the clipped box shows its text"
    );
}

/// (late-text-dip, exit-height0) The same box with its text later than
/// the frame that mounts it, on the worker: that frame lays the box out
/// empty and clips everything under it, so nothing visits the text. The
/// box asks for its texts as a hidden node does; without that, nothing
/// asks for the text (`mount_late` finds no request), it is never
/// shaped, and the box never shows it.
#[test]
fn a_late_text_in_a_box_clipped_to_nothing_is_shown() {
    let (d, root, row) = status_row();
    let (mut r, mut px, k) = late_bar(d, root);
    let (clip, txt) = (NodeId::new(5, 0), NodeId::new(6, 0));
    let mut d = SceneDiff::new();
    d.create(clip, NodeKind::Row, Some(row), u32::MAX)
        .set(clip, Prop::Clip, PropValue::Bool(true));
    late_text(&mut d, txt, clip, &[]);
    let seen = mount_late(&mut r, &mut px, k, d);
    assert!(
        seen[0].1.is_none(),
        "the first frame has no text to draw: {seen:?}"
    );
    assert!(
        bbox_of(&px, [255, 0, 255]).is_some(),
        "the clipped box shows its text at rest: {seen:?}"
    );
}

/// (late-text-dip) A content-sized surface (a popup) opened before its
/// text is shaped is sized by the text's estimate, not as if the text
/// took no room: its size is asked of the compositor, and a popup
/// resized after it is shown is made again (dropping its keyboard grab
/// where `xdg_wm_base` cannot reposition it).
#[test]
fn a_popup_opened_before_its_text_is_shaped_is_sized_by_its_estimate() {
    let (root, clock, popup, card, txt) = (
        NodeId::new(0, 0),
        NodeId::new(1, 0),
        NodeId::new(2, 0),
        NodeId::new(3, 0),
        NodeId::new(4, 0),
    );
    let num = PropValue::Number;
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .set(root, Prop::Height, num(30.0))
        .create(clock, NodeKind::Box, Some(root), 0)
        .set(clock, Prop::Width, num(40.0))
        .set(clock, Prop::Height, num(20.0))
        .create(popup, NodeKind::Popup, Some(clock), 0)
        .set(popup, Prop::Open, PropValue::Bool(false))
        .create(card, NodeKind::Col, Some(popup), 0)
        .set(card, Prop::Pad, num(12.0))
        .create(txt, NodeKind::Text, Some(card), 0)
        .set(txt, Prop::Text, PropValue::Text("October 2026".into()));
    let (mut r, _, _) = late_bar(d, root);
    r.take_surface_changes();
    let mut d = SceneDiff::new();
    d.set(popup, Prop::Open, PropValue::Bool(true));
    r.apply(d);
    assert!(!r.pending.is_empty(), "the text is asked for, not shaped");
    let first = r
        .take_surface_changes()
        .into_iter()
        .find_map(|(n, c)| match c {
            SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } if n == popup => {
                Some(s)
            }
            _ => None,
        })
        .expect("the popup's spec");
    // Twelve characters of the default 13 px font at 0.55 em, and the pad.
    let w = first.width.expect("content-sized");
    assert!(w > 24.0 + 60.0, "sized as if its text took no room: {w}");
}

/// (jelly-drag review) A drag whose move a preview saw (pending), hidden
/// by a diff before its frame (its parent at opacity 0 at once): the
/// frame does not draw it, so nothing measures the move. Its jelly is
/// dropped with the rest of the paint state of what the frame did not
/// draw, so the surface is not left moving (a content-sized surface is
/// held at its size while anything on it moves) until the node is drawn
/// again.
#[test]
fn a_jelly_hidden_before_its_frame_leaves_nothing_moving() {
    let n = |i| NodeId::new(i, 0);
    let (root, wrap, id) = (n(0), n(1), n(2));
    let num = PropValue::Number;
    let mut d = SceneDiff::new();
    d.create(root, NodeKind::Bar, None, 0)
        .set(root, Prop::Height, num(60.0))
        .create(wrap, NodeKind::Box, Some(root), 0)
        .set(wrap, Prop::Width, num(160.0))
        .set(wrap, Prop::Height, num(60.0))
        .create(id, NodeKind::Box, Some(wrap), 0)
        .set(id, Prop::Width, num(20.0))
        .set(id, Prop::Height, num(20.0))
        .set(id, Prop::Jelly, num(0.4))
        .set(
            id,
            Prop::Bg,
            PropValue::Color(Color::from_hex("#f9e2af").unwrap()),
        );
    let mut r = renderer();
    assert!(r.apply(d).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(160, 60), Scale::ONE);
    let mut px = vec![0u8; 160 * 60 * 4];
    let mut paint = |r: &mut Renderer, k: u64| {
        let at = Duration::from_secs(1) + Duration::from_millis(16 * k);
        let mut t = PaintTarget::new(&mut px, Size::new(160, 60), 640, Scale::ONE, 1)
            .unwrap()
            .at(at);
        r.paint(SurfaceId(1), &mut t);
    };
    paint(&mut r, 0);
    for k in 1..=3u64 {
        r.lift(id, Some(LogicalPoint::new(20.0 * k as f32, 0.0)));
        paint(&mut r, k);
    }
    r.lift(id, Some(LogicalPoint::new(80.0, 0.0)));
    r.update();
    assert!(r.anim.busy(&r.tree, root), "the previewed move is pending");
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: wrap,
        prop: Prop::Opacity,
        value: num(0.0),
        transition: strand_scene::Transition::Instant,
    });
    assert!(r.apply(d).is_empty());
    paint(&mut r, 4);
    assert!(
        !r.anim.busy(&r.tree, root),
        "the hidden jelly is left moving"
    );
}

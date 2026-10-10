//! (M4) Effect layers: group opacity, blend modes and masks drawn through
//! vello_cpu layers, partial repaints under them equal to full ones, and
//! damage grown by each effect's reach (`crate::layers`). The effects
//! come from each node's props (`filter:`, `mask:`, `blend:`), built by
//! render (`crate::effects`).

mod common;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

/// Props that give a node effects.
type Fx = Vec<(Prop, PropValue)>;

fn call(name: &str, args: Vec<PropValue>) -> PropValue {
    PropValue::Call {
        name: name.into(),
        args,
    }
}

/// `filter:` with these functions of one number each.
fn filter(fns: &[(&str, f32)]) -> (Prop, PropValue) {
    let calls: Vec<PropValue> = fns.iter().map(|(n, v)| call(n, vec![num(*v)])).collect();
    let v = if calls.len() == 1 {
        calls.into_iter().next().unwrap()
    } else {
        PropValue::List(calls)
    };
    (Prop::Filter, v)
}

/// `filter: blur(r)`.
fn blur(r: f32) -> (Prop, PropValue) {
    filter(&[("blur", r)])
}

/// `filter: grayscale(1)` (a Rec. 709 luma colour matrix).
fn grayscale() -> (Prop, PropValue) {
    filter(&[("grayscale", 1.0)])
}

/// Sets each node's effect props.
fn with_fx(r: &mut Renderer, fx: &[(NodeId, Fx)]) {
    let mut d = SceneDiff::new();
    for (id, props) in fx {
        for (p, v) in props {
            d.set(*id, *p, v.clone());
        }
    }
    assert!(r.apply(d).is_empty());
}

/// Four 40 px boxes in a 240×60 bar: two overlapping squares under one
/// group opacity, a square multiplied onto a yellow box, a green box
/// fading out toward its bottom and a mauve box revealed in a circle.
/// Returns the scene, the effects per node and the faded box.
fn scene(fade_bg: &str) -> (SceneDiff, Vec<(NodeId, Fx)>, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let place = |x: f32, more: Vec<(Prop, PropValue)>| {
        let mut p = vec![
            (Prop::X, num(x)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
        ];
        p.extend(more);
        p
    };
    let group = b.node(NodeKind::Box, Some(root), place(10.0, vec![]));
    let sq = |c: &str, x: f32, y: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(y)),
            (Prop::Size, num(24.0)),
            (Prop::Bg, color(c)),
        ]
    };
    b.node(NodeKind::Box, Some(group), sq("#f38ba8", 0.0, 0.0));
    b.node(NodeKind::Box, Some(group), sq("#89b4fa", 12.0, 12.0));
    let yellow = b.node(
        NodeKind::Box,
        Some(root),
        place(70.0, vec![(Prop::Bg, color("#f9e2af"))]),
    );
    let multiplied = b.node(NodeKind::Box, Some(yellow), sq("#89b4fa", 20.0, 20.0));
    let faded = b.node(
        NodeKind::Box,
        Some(root),
        place(130.0, vec![(Prop::Bg, color(fade_bg))]),
    );
    let revealed = b.node(
        NodeKind::Box,
        Some(root),
        place(190.0, vec![(Prop::Bg, color("#cba6f7"))]),
    );
    let kw = |k: &str| PropValue::Keyword(k.into());
    let effects = vec![
        (group, vec![(Prop::Opacity, num(0.5))]),
        (multiplied, vec![(Prop::Blend, kw("multiply"))]),
        (
            faded,
            vec![(Prop::Mask, call("fade", vec![kw("bottom"), num(20.0)]))],
        ),
        (
            revealed,
            vec![(Prop::Mask, call("radial", vec![kw("center"), num(16.0)]))],
        ),
    ];
    (b.diff, effects, faded)
}

fn drawn(diff: SceneDiff, effects: &[(NodeId, Fx)], scale: Scale) -> (Renderer, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    with_fx(&mut r, effects);
    r.attach_surface(S, r.tree().roots()[0]);
    let (w, h) = (
        (240.0 * scale.as_f32()).round() as u32,
        (60.0 * scale.as_f32()).round() as u32,
    );
    let mut buf = Buffer::new(w, h, scale);
    buf.paint(&mut r, S, 0);
    (r, buf)
}

/// design.md "Effects": group opacity (the overlap is no darker than
/// either square), `blend: multiply`, `mask: fade(bottom, 20)` and
/// `mask: radial(center, 16)`, at 1× and 2× (ref `layers.png`,
/// `layers_2x.png`).
#[test]
fn effect_layers_draw_opacity_blend_and_masks() {
    let (diff, effects, _) = scene("#a6e3a1");
    let (_, buf) = drawn(diff, &effects, Scale::ONE);
    assert_matches_ref("layers", &buf, 1);
    let (diff, effects, _) = scene("#a6e3a1");
    let (_, buf) = drawn(diff, &effects, Scale::new(240).unwrap());
    assert_matches_ref("layers_2x", &buf, 1);
}

/// A change under a mask layer repaints only its damage, and the result
/// equals a full repaint.
#[test]
fn partial_repaints_under_layers_match_full() {
    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        let (diff, effects, faded) = scene("#a6e3a1");
        let (mut r, mut buf) = drawn(diff, &effects, scale);
        let mut d = SceneDiff::new();
        d.set(faded, Prop::Bg, color("#fab387"));
        assert!(r.apply(d).is_empty());
        let damage = buf.paint(&mut r, S, 1);
        assert!(!damage.is_empty());
        assert!(
            damage.area() < (buf.size.w * buf.size.h / 4) as u64,
            "{scale:?}: only the faded box repaints: {damage:?}"
        );
        let (diff, effects, _) = scene("#fab387");
        let (_, full) = drawn(diff, &effects, scale);
        assert!(
            buf.pixels == full.pixels,
            "{scale:?}: partial differs from full"
        );
    }
}

/// design.md: a layer's damage grows by its effects' reach. A child of
/// a box under `blur(2)` (3σ = 6 px) that changes damages its box grown
/// by 6 px; the same box with no effect damages only itself.
#[test]
fn damage_grows_by_each_effects_reach() {
    for (effects, grow) in [
        (vec![], 0),
        (vec![blur(2.0)], 6),
        (vec![blur(2.0), (Prop::Opacity, num(0.5))], 6),
    ] {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let outer = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(60.0)),
                (Prop::Y, num(10.0)),
                (Prop::Size, num(40.0)),
            ],
        );
        let inner = b.node(
            NodeKind::Box,
            Some(outer),
            vec![
                (Prop::X, num(8.0)),
                (Prop::Y, num(8.0)),
                (Prop::Size, num(16.0)),
                (Prop::Bg, color("#f38ba8")),
            ],
        );
        let (mut r, mut buf) = drawn(b.diff, &[(outer, effects.clone())], Scale::ONE);
        let mut d = SceneDiff::new();
        d.set(inner, Prop::Bg, color("#89b4fa"));
        assert!(r.apply(d).is_empty());
        let damage = buf.paint(&mut r, S, 1);
        let rect = r.boxes(S).unwrap().rects[&inner];
        // Drawn at its box moved by its own and its parent's `x`, `y`.
        let x = (rect.x + 68.0) as i32;
        let y = (rect.y + 18.0) as i32;
        let want = Rect::new(
            x - grow,
            y - grow,
            16 + 2 * grow as u32,
            16 + 2 * grow as u32,
        );
        assert!(
            damage.covers(want),
            "{effects:?}: {damage:?} misses {want:?}"
        );
        assert!(
            damage.area() <= want.area() + 4 * (16 + 2 * grow as u64 + 2),
            "{effects:?}: {damage:?} is more than {want:?}"
        );
    }
}

// ---- (M4) Offscreen groups and raster nodes ---------------------------

/// Three 60 px groups of two squares in a 240×80 bar: blurred (σ 3),
/// grayscale, and blurred at half opacity. Returns the scene, the
/// effects and the blurred group's first square (in `first`).
fn filtered(first: &str) -> (SceneDiff, Vec<(NodeId, Fx)>, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut groups = Vec::new();
    let mut first_sq = None;
    for x in [10.0, 90.0, 170.0] {
        let g = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(x)),
                (Prop::Y, num(10.0)),
                (Prop::Size, num(60.0)),
            ],
        );
        let sq = |c: &str, dx: f32| {
            vec![
                (Prop::X, num(dx)),
                (Prop::Y, num(dx)),
                (Prop::Size, num(30.0)),
                (Prop::Bg, color(c)),
            ]
        };
        let c = if first_sq.is_none() { first } else { "#f38ba8" };
        let a = b.node(NodeKind::Box, Some(g), sq(c, 5.0));
        b.node(NodeKind::Box, Some(g), sq("#89b4fa", 25.0));
        first_sq.get_or_insert(a);
        groups.push(g);
    }
    let effects = vec![
        (groups[0], vec![blur(3.0)]),
        (groups[1], vec![grayscale()]),
        (groups[2], vec![blur(3.0), (Prop::Opacity, num(0.5))]),
    ];
    (b.diff, effects, first_sq.unwrap())
}

fn drawn_80(diff: SceneDiff, effects: &[(NodeId, Fx)], scale: Scale) -> (Renderer, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    with_fx(&mut r, effects);
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new((240.0 * k).round() as u32, (80.0 * k).round() as u32, scale);
    buf.paint(&mut r, S, 0);
    (r, buf)
}

/// design.md "Effects": `filter: blur(3)` and `grayscale(1)` drawn by
/// offscreen groups (refs `offscreen.png`, `offscreen_2x.png`); a change
/// under a blur repaints its spread, and the partial repaint equals a
/// full one.
#[test]
fn blur_and_color_matrix_draw_through_offscreen_groups() {
    let (diff, effects, _) = filtered("#f38ba8");
    let (_, buf) = drawn_80(diff, &effects, Scale::ONE);
    assert_matches_ref("offscreen", &buf, 1);
    // Grayscale: every pixel of the middle group's first square is grey.
    for y in 16..44 {
        for x in 96..124 {
            let [b, g, r, _] = buf.px(x, y);
            assert!(
                b.abs_diff(g) <= 1 && g.abs_diff(r) <= 1,
                "({x}, {y}): {b} {g} {r}"
            );
        }
    }
    let (diff, effects, _) = filtered("#f38ba8");
    let (_, buf) = drawn_80(diff, &effects, Scale::new(240).unwrap());
    assert_matches_ref("offscreen_2x", &buf, 1);

    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        let (diff, effects, sq) = filtered("#f38ba8");
        let (mut r, mut buf) = drawn_80(diff, &effects, scale);
        let mut d = SceneDiff::new();
        d.set(sq, Prop::Bg, color("#a6e3a1"));
        assert!(r.apply(d).is_empty());
        buf.paint(&mut r, S, 1);
        let (diff, effects, _) = filtered("#a6e3a1");
        let (_, full) = drawn_80(diff, &effects, scale);
        assert!(
            buf.pixels == full.pixels,
            "{scale:?}: partial differs from full"
        );
    }
}

/// A blurred (σ 4, reach 12) grayscale group in a 240×80 bar holding a
/// blurred (σ 2, reach 6) group of one red square, and a square 45 px to
/// its right (`right`'s colour). Returns the scene, the effects and that
/// square.
fn nested(right: &str) -> (SceneDiff, Vec<(NodeId, Fx)>, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let outer = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Width, num(220.0)),
            (Prop::Height, num(60.0)),
        ],
    );
    let inner = b.node(
        NodeKind::Box,
        Some(outer),
        vec![
            (Prop::X, num(5.0)),
            (Prop::Y, num(5.0)),
            (Prop::Size, num(50.0)),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(inner),
        vec![
            (Prop::X, num(20.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(30.0)),
            (Prop::Bg, color("#f38ba8")),
        ],
    );
    let sq = b.node(
        NodeKind::Box,
        Some(outer),
        vec![
            (Prop::X, num(100.0)),
            (Prop::Y, num(15.0)),
            (Prop::Size, num(30.0)),
            (Prop::Bg, color(right)),
        ],
    );
    let effects = vec![
        (outer, vec![filter(&[("blur", 4.0), ("grayscale", 1.0)])]),
        (inner, vec![blur(2.0)]),
    ];
    (b.diff, effects, sq)
}

/// Nested offscreen groups: damage that meets an outer group but not the
/// group inside it still draws the inner one filtered into the outer
/// group (which is drawn whole), so the partial repaint equals a full one
/// and a full repaint afterwards reuses the outer group it built.
#[test]
fn nested_offscreen_groups_keep_their_filters_on_partial_repaints() {
    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        let (diff, effects, sq) = nested("#89b4fa");
        let (mut r, mut buf) = drawn_80(diff, &effects, scale);
        let mut d = SceneDiff::new();
        d.set(sq, Prop::Bg, color("#a6e3a1"));
        assert!(r.apply(d).is_empty());
        let damage = buf.paint(&mut r, S, 1);
        let k = scale.as_f32();
        // The damage never meets the inner group's bounds (its square,
        // its own reach and the outer blur's end at x = 83).
        assert!(
            damage.rects().iter().all(|d| d.x as f32 >= 83.0 * k),
            "{scale:?}: {damage:?}"
        );
        let builds = r.offscreen_cache().1;
        // A full repaint of the same content: both groups come from the
        // cache, the outer one as the partial frame built it.
        let mut d = SceneDiff::new();
        d.set(r.tree().roots()[0], Prop::Bg, color("#1e1e2e"));
        assert!(r.apply(d).is_empty());
        buf.paint(&mut r, S, 0);
        assert_eq!(
            r.offscreen_cache().1,
            builds,
            "{scale:?}: the outer group was built without its inner one"
        );
        let (diff, effects, _) = nested("#a6e3a1");
        let (_, full) = drawn_80(diff, &effects, scale);
        assert!(
            buf.pixels == full.pixels,
            "{scale:?}: partial differs from full"
        );
    }
}

/// design.md: cached offscreen groups redraw only when their children
/// change, within a 4 MB budget, freed when idle. A repaint of the whole
/// surface reuses the group; a change inside it draws it again; four
/// 1.25 MB groups keep three (the frame's fourth is drawn uncached); an
/// idle cache empties.
#[test]
fn offscreen_groups_are_reused_bounded_and_freed_when_idle() {
    let (diff, effects, sq) = filtered("#f38ba8");
    let (mut r, mut buf) = drawn_80(diff, &effects, Scale::ONE);
    let (bytes, builds, kept) = r.offscreen_cache();
    assert_eq!((builds, kept), (3, 3));
    assert!(bytes > 0);
    // Repainted whole (buffer age 0): every group comes from the cache.
    let mut d = SceneDiff::new();
    d.set(r.tree().roots()[0], Prop::Bg, color("#11111b"));
    assert!(r.apply(d).is_empty());
    buf.paint(&mut r, S, 0);
    assert_eq!(r.offscreen_cache().1, 3, "reused");
    // A child changes: its group alone is drawn again.
    let mut d = SceneDiff::new();
    d.set(sq, Prop::Bg, color("#a6e3a1"));
    assert!(r.apply(d).is_empty());
    buf.paint(&mut r, S, 1);
    assert_eq!(r.offscreen_cache().1, 4, "the changed group redraws");

    // The budget: four 560 px grayscale groups (1.25 MB each).
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut big = Vec::new();
    for i in 0..4 {
        let g = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(i as f32 * 570.0)),
                (Prop::Size, num(560.0)),
                (Prop::Bg, color("#f38ba8")),
            ],
        );
        big.push((g, vec![grayscale()]));
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    with_fx(&mut r, &big);
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(2280, 560, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let (bytes, builds, kept) = r.offscreen_cache();
    assert!(bytes <= strand_render::OFFSCREEN_BYTES, "{bytes}");
    assert_eq!((builds, kept), (4, 3), "over the budget: three kept");
    // Every group drew, the uncached one too: all grey.
    for i in 0..4u32 {
        let [b, g, r, _] = buf.px(i * 570 + 280, 280);
        assert!(b.abs_diff(g) <= 1 && g.abs_diff(r) <= 1, "group {i}");
    }

    // Idle: nothing used them for the idle time; the next wake frees them.
    r.set_paint_cache_idle(std::time::Duration::from_millis(1));
    std::thread::sleep(std::time::Duration::from_millis(5));
    r.update();
    assert_eq!(r.offscreen_cache().0, 0, "freed when idle");
}

/// A raster node source: a solid red whose level steps with `t`.
#[derive(Debug)]
struct Steps(std::time::Duration);

impl strand_render::RasterSource for Steps {
    fn draw(
        &self,
        px: &mut [vello_cpu::color::PremulRgba8],
        _: u32,
        _: u32,
        _: f32,
        time: TimeContext,
    ) {
        let level = (40.0 + time.t * 400.0).min(255.0) as u8;
        for p in px {
            *p = vello_cpu::color::PremulRgba8 {
                r: level,
                g: 0,
                b: 0,
                a: 255,
            };
        }
    }

    fn rate(&self) -> strand_render::Rate {
        strand_render::Rate::Every(self.0)
    }
}

/// design.md: CPU raster nodes draw into a cached pixmap at their
/// clock's rate. A 10 fps node painted at 60 Hz draws its pixmap once
/// per tick, repaints only on ticks, and shows the tick's `t`.
#[test]
fn raster_nodes_draw_at_their_clock_rate() {
    use std::time::Duration;
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let node = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(20.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(20.0)),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.set_raster_source(
        node,
        Some(std::sync::Arc::new(Steps(Duration::from_millis(100)))),
    );
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    let t0 = Duration::from_secs(1);
    let at = |k: u64| t0 + Duration::from_nanos(1_000_000_000 * k / 60);
    buf.paint_at(&mut r, S, 0, t0);
    assert_eq!(r.raster_nodes().0, 1);
    let mut damaged = 0;
    for k in 1..=30 {
        if !buf.paint_at(&mut r, S, 1, at(k)).is_empty() {
            damaged += 1;
        }
    }
    // Half a second: ticks at 0.1 … 0.5 s.
    assert_eq!(r.raster_nodes().0, 6, "one pixmap per tick");
    assert_eq!(damaged, 5, "repainted only on ticks");
    let rect = r.boxes(S).unwrap().rects[&node];
    let [blue, _, red, _] = buf.px((rect.x + 30.0) as u32, (rect.y + 20.0) as u32);
    assert_eq!((red, blue), (240, 0), "t = 0.5 s: 40 + 200");
    assert!(r.raster_nodes().1 >= 20 * 20 * 4);
}

/// A capped clock's ticks do not stop the frame loop: an offscreen group
/// the ticks' damage never reaches stays cached between them (design.md:
/// groups "render once and redraw only when children change", freed
/// when idle), and goes once it has been idle.
#[test]
fn capped_ticks_keep_untouched_offscreen_groups() {
    use std::time::Duration;
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let group = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(group),
        vec![
            (Prop::X, num(5.0)),
            (Prop::Y, num(5.0)),
            (Prop::Size, num(30.0)),
            (Prop::Bg, color("#f38ba8")),
        ],
    );
    let raster = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(180.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(20.0)),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    with_fx(&mut r, &[(group, vec![blur(3.0)])]);
    r.set_raster_source(
        raster,
        Some(std::sync::Arc::new(Steps(Duration::from_millis(100)))),
    );
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    let t0 = Duration::from_secs(1);
    let at = |k: u64| t0 + Duration::from_nanos(1_000_000_000 * k / 60);
    buf.paint_at(&mut r, S, 0, t0);
    assert_eq!(r.offscreen_cache().1, 1, "the blurred group is built");
    let blurred = r.boxes(S).unwrap().rects[&group];
    let reach = Rect::new(
        blurred.x as i32 - 12,
        blurred.y as i32 - 12,
        blurred.w as u32 + 24,
        blurred.h as u32 + 24,
    );
    for tick in 1..=4 {
        let d = buf.paint_at(&mut r, S, 1, at(6 * tick));
        assert!(!d.is_empty(), "tick {tick} repaints the raster node");
        for rect in d.rects() {
            assert!(
                !rect.intersects(reach),
                "tick {tick}: damage {rect:?} reaches the group"
            );
        }
        assert!(!r.wants_frame(S), "between ticks: no frame");
        assert!(r.next_wake().is_some(), "but a wake for the next tick");
        assert_eq!(
            r.offscreen_cache().2,
            1,
            "tick {tick}: the untouched group stays cached"
        );
    }
    assert_eq!(r.offscreen_cache().1, 1, "never built again");

    // The clock goes and the loop stops. The burst's first frame used the
    // group, so it is kept until it has been idle for the idle time.
    r.set_raster_source(raster, None);
    let mut k = 30;
    while r.wants_frame(S) {
        buf.paint_at(&mut r, S, 1, at(k));
        k += 1;
        assert!(k < 40, "settles");
    }
    assert_eq!(r.next_wake(), None);
    assert_eq!(r.offscreen_cache().2, 1, "used in the burst");
    r.set_paint_cache_idle(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(5));
    r.update();
    assert_eq!(r.offscreen_cache().2, 0, "idle: freed");
}

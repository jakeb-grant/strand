//! (M4) The effects catalogue's paint half, drawn offline from props
//! (design.md, "Motion and visual effects"): colour filters, the bundled
//! filters' CPU fallbacks, blend modes and masks, each against a PNG
//! reference at fixed `PaintTarget::time` values.

mod common;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

/// Props that give a node effects.
type Fx = Vec<(Prop, PropValue)>;

/// Tile pitch and the sample's size, logical pixels.
const PITCH: f32 = 80.0;
const BOX: f32 = 60.0;

fn call(name: &str, args: Vec<PropValue>) -> PropValue {
    PropValue::Call {
        name: name.into(),
        args,
    }
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn filter(name: &str, args: Vec<PropValue>) -> (Prop, PropValue) {
    (Prop::Filter, call(name, args))
}

/// A row of tiles on a dark bar, one per entry of `fx`: a 60 px group of
/// four coloured squares (red, green, blue and yellow quarters) carrying
/// that entry's props. Returns the renderer and the groups.
fn tiles(fx: Vec<Fx>, scale: Scale) -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut groups = Vec::new();
    for (i, props) in fx.into_iter().enumerate() {
        let mut p = vec![
            (Prop::X, num(i as f32 * PITCH + 10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(BOX)),
            (Prop::Place, kw("absolute")),
        ];
        p.extend(props);
        let g = b.node(NodeKind::Box, Some(root), p);
        for (c, x, y) in [
            ("#f38ba8", 0.0, 0.0),
            ("#a6e3a1", 30.0, 0.0),
            ("#89b4fa", 0.0, 30.0),
            ("#f9e2af", 30.0, 30.0),
        ] {
            b.node(
                NodeKind::Box,
                Some(g),
                vec![
                    (Prop::X, num(x)),
                    (Prop::Y, num(y)),
                    (Prop::Size, num(30.0)),
                    (Prop::Place, kw("absolute")),
                    (Prop::Bg, color(c)),
                ],
            );
        }
        groups.push(g);
    }
    let n = groups.len() as f32;
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new(
        (n * PITCH * k).round() as u32,
        (PITCH * k).round() as u32,
        scale,
    );
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_secs(1));
    (r, buf, groups)
}

/// Pixel `(x, y)` of tile `i`, in the tile's logical pixels (1×).
fn at(buf: &Buffer, i: usize, x: f32, y: f32) -> [u8; 4] {
    buf.px((i as f32 * PITCH + x) as u32, y as u32)
}

/// design.md "Filters and compositing": `filter: grayscale(1)`,
/// `saturate(2)`, `hue(120deg)`, `brightness(0.6)`, `contrast(1.8)`,
/// `invert(1)`, `tint($accent)` and a chain, implemented by Strand as
/// colour matrices (ref `effects_filters.png`, 1× and 2×).
#[test]
fn colour_filters_draw_each_function() {
    let fx = || {
        vec![
            vec![],
            vec![filter("grayscale", vec![num(1.0)])],
            vec![filter("saturate", vec![num(2.0)])],
            vec![filter("hue", vec![PropValue::Angle(120.0)])],
            vec![filter("brightness", vec![num(0.6)])],
            vec![filter("contrast", vec![num(1.8)])],
            vec![filter("invert", vec![num(1.0)])],
            vec![filter("tint", vec![color("#7aa2f7")])],
            vec![(
                Prop::Filter,
                PropValue::List(vec![
                    call("grayscale", vec![num(1.0)]),
                    call("brightness", vec![num(1.3)]),
                ]),
            )],
        ]
    };
    let (_, buf, _) = tiles(fx(), Scale::ONE);
    // The red quarter's centre in each tile.
    let red = |i| at(&buf, i, 25.0, 25.0);
    let [b, g, r, _] = red(0);
    assert_eq!([r, g, b], [0xf3, 0x8b, 0xa8], "unfiltered");
    let [gb, gg, gr, _] = red(1);
    assert!(gb.abs_diff(gg) <= 1 && gg.abs_diff(gr) <= 1, "grey");
    let [ib, ig, ir, _] = red(6);
    assert_eq!([ir, ig, ib], [255 - r, 255 - g, 255 - b], "inverted");
    let [db, dg, dr, _] = red(4);
    assert!(dr < r && dg < g && db < b, "darker");
    let [hb, hg, hr, _] = red(3);
    assert!(hg > hr, "hue turns red towards green: {hr} {hg} {hb}");
    let [tb, tg, tr, _] = red(7);
    assert!(tb > tr && tb > tg, "tinted blue: {tr} {tg} {tb}");
    assert_matches_ref("effects_filters", &buf, 1);
    let (_, buf, _) = tiles(fx(), Scale::new(240).unwrap());
    assert_matches_ref("effects_filters_2x", &buf, 1);
}

/// design.md "Bundled GPU effects": without a GPU `bloom(r)` becomes a
/// glow (the subtree's own colours spread up to `r` around it), and
/// `crt()`, `chromatic(px)` and `wobble(amp)` draw the node unfiltered
/// (ref `effects_bundled.png`).
#[test]
fn bundled_filters_fall_back_on_the_cpu() {
    let (_, buf, _) = tiles(
        vec![
            vec![],
            vec![filter("bloom", vec![num(9.0)])],
            vec![filter("crt", vec![])],
            vec![filter("chromatic", vec![num(2.0)])],
            vec![filter("wobble", vec![num(4.0)])],
        ],
        Scale::ONE,
    );
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    // Bloom: just outside the box the red quarter glows; plain tiles
    // show the bar there.
    assert_eq!(at(&buf, 0, 7.0, 30.0), bg);
    let [b, g, r, _] = at(&buf, 1, 7.0, 25.0);
    assert!(r > 0x40 && r > b && r > g, "a red glow: {r} {g} {b}");
    assert_eq!(at(&buf, 1, 25.0, 25.0), at(&buf, 0, 25.0, 25.0), "inside");
    // Past 3σ = r (9 px) nothing reaches.
    assert_eq!(at(&buf, 1, 0.0, 30.0), bg);
    // The shader-only filters draw exactly as unfiltered.
    for i in 2..5 {
        for (x, y) in [(25.0, 25.0), (45.0, 45.0), (7.0, 30.0), (40.0, 69.0)] {
            assert_eq!(at(&buf, i, x, y), at(&buf, 0, x, y), "tile {i} ({x}, {y})");
        }
    }
    assert_matches_ref("effects_bundled", &buf, 1);
}

/// design.md: `blend: screen | add | multiply | overlay | difference`,
/// each square blended onto a yellow box (ref `effects_blend.png`).
#[test]
fn blend_modes_blend_with_what_is_below() {
    let (_, buf, _) = tiles(
        [
            "screen",
            "add",
            "multiply",
            "overlay",
            "difference",
            "normal",
        ]
        .into_iter()
        .map(|m| vec![(Prop::Bg, color("#808080")), (Prop::Blend, kw(m))])
        .collect(),
        Scale::ONE,
    );
    // The group's grey box blends onto the bar; read the bar-only edge
    // against the box's own colours: multiply darkens, screen and add
    // lighten, difference of grey on the dark bar is lighter than it.
    let bar = [0x1e, 0x1e, 0x2e];
    let mid = |i| {
        let [b, g, r, _] = at(&buf, i, 45.0, 15.0);
        [r, g, b]
    };
    let green = [0xa6, 0xe3, 0xa1];
    let (screen, add, multiply, diff, normal) = (mid(0), mid(1), mid(2), mid(4), mid(5));
    assert_eq!(normal, green, "normal is no effect");
    for c in 0..3 {
        assert!(screen[c] >= green[c].max(bar[c]), "screen lightens");
        assert!(add[c] >= screen[c], "add is at least screen");
        assert!(multiply[c] <= green[c].min(bar[c]) + 1, "multiply darkens");
        assert_eq!(diff[c], green[c].abs_diff(bar[c]), "difference");
    }
    assert_matches_ref("effects_blend", &buf, 1);
}

/// design.md: `mask: fade(bottom, 24)` and `radial(…)` with a pixel and
/// a percentage size (of the distance to the farthest corner).
#[test]
fn masks_fade_and_reveal() {
    let (_, buf, _) = tiles(
        vec![
            vec![(Prop::Mask, call("fade", vec![kw("bottom"), num(24.0)]))],
            vec![(Prop::Mask, call("fade", vec![kw("left"), num(30.0)]))],
            vec![(Prop::Mask, call("radial", vec![kw("center"), num(20.0)]))],
            vec![(
                Prop::Mask,
                call(
                    "radial",
                    vec![kw("top_left"), PropValue::Length(Length::Percent(100.0))],
                ),
            )],
        ],
        Scale::ONE,
    );
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    let edge = at(&buf, 0, 40.0, 69.0);
    assert!(
        edge.iter().zip(bg).all(|(a, b)| a.abs_diff(b) <= 4),
        "faded out at the bottom edge: {edge:?}"
    );
    assert_eq!(at(&buf, 0, 25.0, 25.0)[2], 0xf3, "opaque 24 px in");
    assert_eq!(at(&buf, 2, 12.0, 12.0), bg, "outside the 20 px disc");
    assert_eq!(at(&buf, 2, 38.0, 38.0)[0], 0xa8, "inside the disc (red)");
    assert_ne!(at(&buf, 3, 68.0, 68.0), bg, "100% reaches the far corner");
    assert_matches_ref("effects_masks", &buf, 1);
}

// ---- Shapes ---------------------------------------------------------

const SHAPES: [&str; 13] = [
    "rect", "circle", "pill", "cookie", "clover", "burst", "flower", "gem", "sunny", "triangle",
    "pentagon", "hexagon", "heart",
];

/// One 56 × 56 box per entry of `props` on a dark bar, 64 px apart (the
/// pill's box is wider). Returns the renderer, buffer and boxes.
fn boxes(props: Vec<Fx>, scale: Scale, time: u64) -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let n = props.len();
    let ids = props
        .into_iter()
        .enumerate()
        .map(|(i, more)| {
            let mut p = vec![
                (Prop::X, num(i as f32 * 64.0 + 4.0)),
                (Prop::Y, num(4.0)),
                (Prop::Width, num(56.0)),
                (Prop::Height, num(56.0)),
                (Prop::Place, kw("absolute")),
                (Prop::Bg, color("#cba6f7")),
            ];
            p.extend(more);
            b.node(NodeKind::Box, Some(root), p)
        })
        .collect();
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new(
        (n as f32 * 64.0 * k).round() as u32,
        (64.0 * k).round() as u32,
        scale,
    );
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(time));
    (r, buf, ids)
}

/// design.md "Shape and geometry": every shape of the library, filled
/// and with a border drawn inside its outline (refs `effects_shapes.png`
/// at 1× and 1.5×).
#[test]
fn the_shape_library_draws_every_shape() {
    let fx = || -> Vec<Fx> {
        SHAPES
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut p = vec![(Prop::Shape, kw(s))];
                if i % 2 == 1 {
                    p.push((
                        Prop::Border,
                        PropValue::Border(Border {
                            width: 3.0,
                            paint: Paint::Solid(hex("#f5e0dc")),
                        }),
                    ));
                }
                p
            })
            .collect()
    };
    let (_, buf, _) = boxes(fx(), Scale::ONE, 1000);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    let lilac = [0xf7, 0xa6, 0xcb, 0xff];
    let tile = |i: u32, x: u32, y: u32| buf.px(i * 64 + 4 + x, 4 + y);
    // A rect fills its corner, a circle does not; every shape fills its
    // centre.
    assert_eq!(tile(0, 1, 1), lilac);
    assert_eq!(tile(1, 2, 2), bg);
    for i in [0, 2, 3, 4, 5, 6, 8, 10, 12] {
        assert_eq!(
            tile(i, 28, 28),
            lilac,
            "{} at its centre",
            SHAPES[i as usize]
        );
    }
    // The heart's notch at the top middle is outside it.
    assert_eq!(tile(12, 28, 3), bg, "the heart's notch");
    assert_matches_ref("effects_shapes", &buf, 1);
    let (_, buf, _) = boxes(fx(), Scale::new(180).unwrap(), 1000);
    assert_matches_ref("effects_shapes_1_5x", &buf, 1);
}

/// design.md: "`shape: cookie` → `when loading { shape: burst }`; morphs
/// by spring". Mid-morph frames lie between the two outlines (ref
/// `effects_morph.png` at 80 ms), the morph settles on the new shape, and
/// `reduced_motion` snaps.
#[test]
fn a_shape_change_morphs_by_spring() {
    use std::time::Duration;
    let (mut r, mut buf, ids) = boxes(vec![vec![(Prop::Shape, kw("cookie"))]], Scale::ONE, 1000);
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Shape, kw("burst"));
    assert!(r.apply(d).is_empty());
    let ms = |m: u64| Duration::from_millis(1000 + m);
    buf.paint_at(&mut r, S, 1, ms(16));
    assert!(r.wants_frame(S), "the morph keeps frames coming");
    buf.paint_at(&mut r, S, 1, ms(80));
    assert_matches_ref("effects_morph", &buf, 1);
    let (_, cookie, _) = boxes(vec![vec![(Prop::Shape, kw("cookie"))]], Scale::ONE, 1000);
    let (_, burst, _) = boxes(vec![vec![(Prop::Shape, kw("burst"))]], Scale::ONE, 1000);
    assert!(buf.pixels != cookie.pixels && buf.pixels != burst.pixels);
    let mut t = 96;
    while r.wants_frame(S) {
        buf.paint_at(&mut r, S, 1, ms(t));
        t += 16;
        assert!(t < 3000, "settles");
    }
    assert!(buf.pixels == burst.pixels, "settled on the burst");
    // Reduced motion: the next change snaps.
    r.set_reduced_motion(true);
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Shape, kw("cookie"));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, ms(t + 16));
    assert!(buf.pixels == cookie.pixels, "snapped");
    assert!(!r.wants_frame(S));
}

/// design.md: `mask: shape(cookie)` masks a subtree with a shape of the
/// library (ref `effects_mask_shapes.png`).
#[test]
fn shape_masks_cut_a_subtree_to_its_outline() {
    let (_, buf, _) = tiles(
        ["cookie", "heart", "clover", "triangle"]
            .into_iter()
            .map(|s| vec![(Prop::Mask, call("shape", vec![kw(s)]))])
            .collect(),
        Scale::ONE,
    );
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    // The squares' corners are cut away; the centre stays.
    for i in 0..4 {
        assert_eq!(at(&buf, i, 11.0, 11.0), bg, "tile {i}'s corner");
        assert_ne!(at(&buf, i, 40.0, 45.0), bg, "tile {i}'s middle");
    }
    assert_matches_ref("effects_mask_shapes", &buf, 1);
}

/// One `w × h` box carrying `props`, alone on the dark bar at `scale`,
/// painted at rest: its pixels.
fn solo(props: Fx, w: f32, h: f32, scale: Scale) -> Vec<u8> {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = vec![
        (Prop::X, num(4.0)),
        (Prop::Y, num(4.0)),
        (Prop::Width, num(w)),
        (Prop::Height, num(h)),
        (Prop::Place, kw("absolute")),
        (Prop::Bg, color("#cba6f7")),
    ];
    p.extend(props);
    b.node(NodeKind::Box, Some(root), p);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new(
        ((w + 8.0) * k).round() as u32,
        ((h + 8.0) * k).round() as u32,
        scale,
    );
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    buf.pixels
}

/// Owner decision 2026-10-10 (shapes): a shape goes only if the others
/// draw it exactly. None does: every pair of the 13 differs in pixels in
/// every box tried (square, wide, tall, odd; 1× and 1.5×), except that
/// `circle` and `pill` coincide in a square box (and only there: an
/// ellipse is not a capsule). Outside the library, `rect` draws as a box
/// with no `shape:` and `pill` as `radius: full`, but neither of those
/// morphs (`only_named_shapes_morph`), so both stay. That keep rests on
/// the renderer, not on the pixels: if a box with no `shape:` ever
/// morphs from its own outline, `rect` and `pill` duplicate a plain box
/// and `radius: full` exactly and the owner's rule removes them.
#[test]
fn no_shape_duplicates_another() {
    let boxes = [(56.0, 56.0), (96.0, 40.0), (40.0, 96.0), (31.0, 57.0)];
    let scales = [Scale::ONE, Scale::new(180).unwrap()];
    let variants: Vec<(f32, f32, Scale)> = boxes
        .iter()
        .flat_map(|&(w, h)| scales.iter().map(move |&s| (w, h, s)))
        .collect();
    let draw = |props: Fx| -> Vec<Vec<u8>> {
        variants
            .iter()
            .map(|&(w, h, s)| solo(props.clone(), w, h, s))
            .collect()
    };
    let shaped: Vec<Vec<Vec<u8>>> = SHAPES
        .iter()
        .map(|s| draw(vec![(Prop::Shape, kw(s))]))
        .collect();
    for i in 0..SHAPES.len() {
        for j in i + 1..SHAPES.len() {
            for (v, &(w, h, _)) in variants.iter().enumerate() {
                let same = shaped[i][v] == shaped[j][v];
                let expected = (SHAPES[i], SHAPES[j]) == ("circle", "pill") && w == h;
                assert_eq!(
                    same, expected,
                    "{} and {} in a {w}×{h} box (variant {v})",
                    SHAPES[i], SHAPES[j]
                );
            }
        }
    }
    // The static spellings outside the library.
    let plain = draw(vec![]);
    let full = draw(vec![(Prop::Radius, kw("full"))]);
    assert!(plain == shaped[0], "`shape: rect` draws a plain box");
    assert!(full == shaped[2], "`shape: pill` draws `radius: full`");
    for (v, &(w, h, _)) in variants.iter().enumerate() {
        assert_eq!(full[v] == shaped[1][v], w == h, "circle vs radius: full");
    }
}

/// Why `rect`, `circle` and `pill` stay beside the plain-box spellings
/// they draw: a `shape:` change morphs by spring, but a box that gains
/// `shape:` snaps (`Morphs::outline` has no entry for it), so only a
/// named shape can start a morph at a plain rect, disc or capsule. The
/// snap is what the shapes decision (docs/decisions.md, 2026-10-10
/// m4-owner-shapes) keeps `rect` and `pill` for, not a fixed fact: if
/// this test fails because a plain box now morphs, revisit that decision
/// (both would then duplicate a plain box and `radius: full` exactly)
/// rather than only updating the assertion.
#[test]
fn only_named_shapes_morph() {
    use std::time::Duration;
    let ms = |m: u64| Duration::from_millis(1000 + m);
    for from in ["rect", "circle", "pill"] {
        let (mut r, mut buf, ids) = boxes(vec![vec![(Prop::Shape, kw(from))]], Scale::ONE, 1000);
        let mut d = SceneDiff::new();
        d.set(ids[0], Prop::Shape, kw("cookie"));
        assert!(r.apply(d).is_empty());
        buf.paint_at(&mut r, S, 1, ms(16));
        assert!(r.wants_frame(S), "{from} morphs to a cookie");
    }
    // No `shape:` (a plain box, or `radius: full`): gaining one snaps.
    let (_, cookie, _) = boxes(vec![vec![(Prop::Shape, kw("cookie"))]], Scale::ONE, 1000);
    for plain in [vec![], vec![(Prop::Radius, kw("full"))]] {
        let (mut r, mut buf, ids) = boxes(vec![plain], Scale::ONE, 1000);
        let mut d = SceneDiff::new();
        d.set(ids[0], Prop::Shape, kw("cookie"));
        assert!(r.apply(d).is_empty());
        buf.paint_at(&mut r, S, 1, ms(16));
        assert!(
            !r.wants_frame(S),
            "a box gaining `shape:` now morphs: the 2026-10-10 shapes decision \
             (docs/decisions.md, m4-owner-shapes) keeps `rect` and `pill` only \
             because this snapped; revisit it, since both now duplicate a plain \
             box and `radius: full` exactly"
        );
        assert!(buf.pixels == cookie.pixels, "snapped to the cookie");
    }
}

fn shadow(x: f32, y: f32, blur: f32, c: &str) -> PropValue {
    PropValue::Shadow(vec![Shadow {
        x,
        y,
        blur,
        spread: 0.0,
        color: hex(c),
    }])
}

fn pair(a: PropValue, b: PropValue) -> PropValue {
    PropValue::List(vec![a, b])
}

/// The light scene: a glowing box, a box with an inner shadow and a rim,
/// glowing text and a grained pill, on a dark bar 360×80 logical pixels.
fn light(scale: Scale, time: u64, reduced: bool) -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let place = |x: f32, w: f32, h: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num((80.0 - h) / 2.0)),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Place, kw("absolute")),
        ]
    };
    let mut glow = place(20.0, 50.0, 40.0);
    glow.extend([
        (Prop::Bg, color("#313244")),
        (Prop::Radius, num(10.0)),
        (
            Prop::Glow,
            pair(num(12.0), PropValue::Color(hex("#89b4fa").alpha(0.8))),
        ),
    ]);
    let mut inset = place(100.0, 60.0, 44.0);
    inset.extend([
        (Prop::Bg, color("#cba6f7")),
        (Prop::Radius, num(12.0)),
        (Prop::InnerShadow, shadow(0.0, 4.0, 8.0, "#11111b")),
        (
            Prop::Rim,
            pair(kw("top"), PropValue::Color(hex("#ffffff").alpha(0.7))),
        ),
    ]);
    let mut words = place(180.0, 80.0, 30.0);
    words.extend([
        (Prop::Text, text("Glow")),
        (Prop::Font, PropValue::Font(font(24.0))),
        (Prop::Color, color("#f5e0dc")),
        (Prop::Glow, pair(num(8.0), PropValue::Color(hex("#f38ba8")))),
    ]);
    let mut grain = place(270.0, 80.0, 36.0);
    grain.extend([
        (Prop::Bg, color("#a6e3a1")),
        (Prop::Radius, kw("full")),
        (Prop::Grain, num(0.35)),
    ]);
    let ids = vec![
        b.node(NodeKind::Box, Some(root), glow),
        b.node(NodeKind::Box, Some(root), inset),
        b.node(NodeKind::Text, Some(root), words),
        b.node(NodeKind::Box, Some(root), grain),
    ];
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new((360.0 * k).round() as u32, (80.0 * k).round() as u32, scale);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(time));
    (r, buf, ids)
}

/// `glow: r` alone glows in the node's own colour, at rest and while
/// its radius springs (not in the colour it inherits).
#[test]
fn a_lone_radius_glow_springs_in_the_nodes_colour() {
    use std::time::Duration;
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#000000")),
            (Prop::Color, color("#0000ff")),
        ],
    );
    let mut p = at_xy(30.0, 20.0, 40.0, 40.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Bg, color("#000000")),
        (Prop::Color, color("#ff0000")),
        (Prop::Glow, num(8.0)),
    ]);
    let n = b.node(NodeKind::Box, Some(root), p);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(100, 80, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
    // (BGRA.)
    let red = |px: [u8; 4]| px[2] > 40 && px[0] < px[2] / 4;
    assert!(red(buf.px(28, 40)), "at rest: {:?}", buf.px(28, 40));
    let mut d = SceneDiff::new();
    d.set(n, Prop::Glow, num(20.0));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1050));
    assert!(r.wants_frame(S), "springing");
    assert!(red(buf.px(28, 40)), "mid-spring: {:?}", buf.px(28, 40));
    settle(&mut r, &mut buf, 1050);
    assert!(red(buf.px(24, 40)), "settled: {:?}", buf.px(24, 40));
}

/// A text's solid `fill:` springs like the other drawn props (m4-plan:
/// newly drawn props join the springs).
#[test]
fn a_solid_text_fill_springs() {
    use std::time::Duration;
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#000000"))]);
    let mut words = at_xy(10.0, 10.0, 80.0, 40.0);
    words.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Text, text("\u{2588}\u{2588}")),
        (Prop::Font, PropValue::Font(font(32.0))),
        (Prop::Fill, PropValue::Paint(Paint::Solid(hex("#ff0000")))),
    ]);
    let t = b.node(NodeKind::Text, Some(root), words);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(100, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
    // (BGRA.)
    let ink = buf.px(30, 30);
    assert!(ink[2] > 200 && ink[0] < 40, "red letters {ink:?}");
    let mut d = SceneDiff::new();
    d.set(
        t,
        Prop::Fill,
        PropValue::Paint(Paint::Solid(hex("#0000ff"))),
    );
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1060));
    assert!(r.wants_frame(S), "springing");
    let mid = buf.px(30, 30);
    assert!(mid[0] > 20 && mid[2] > 20, "between red and blue: {mid:?}");
    settle(&mut r, &mut buf, 1060);
    let end = buf.px(30, 30);
    assert!(end[0] > 200 && end[2] < 40, "blue letters {end:?}");
}

/// design.md "Paint and light": `glow:` on a box (a shadow-like halo)
/// and on text (its letters' own halo), `inner_shadow:` with `rim: top`,
/// and `grain:` (refs `effects_light.png` at 1× and 2×).
#[test]
fn glow_inner_shadow_rim_and_grain_draw() {
    let (_, buf, _) = light(Scale::ONE, 1000, false);
    assert_matches_ref("effects_light", &buf, 2);
    let px = |x: u32, y: u32| buf.px(x, y);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    // The box (20, 20, 50×40) glows blue beside it, and not 12 px out.
    let beside = px(17, 40);
    assert!(beside[0] > beside[2] + 0x20, "blue halo {beside:?}");
    assert_eq!(px(7, 40), bg);
    // Inside the inset box (100, 18, 60×44): darker under its top edge
    // than in its middle, and the rim lights its top row.
    let (mid, under) = (px(130, 46), px(130, 22));
    assert!(under[1] + 20 < mid[1], "shadowed {under:?} {mid:?}");
    assert!(px(130, 18)[1] > under[1] + 20, "rim {:?}", px(130, 18));
    // The text (180, 25) glows red around its letters (which are
    // #f5e0dc: never 40 redder than green).
    let reddish = (175..265)
        .flat_map(|x| (20..60).map(move |y| (x, y)))
        .map(|(x, y)| buf.px(x, y))
        .filter(|p| p[2] as u32 > p[1] as u32 + 40)
        .count();
    assert!(reddish > 100, "a red halo around the letters: {reddish}");
    // The grain varies within the pill (270, 22, 80×36), and stays inside
    // its round ends.
    let row: Vec<[u8; 4]> = (290..330).map(|x| buf.px(x, 40)).collect();
    assert!(row.windows(2).any(|w| w[0] != w[1]), "grain varies");
    assert_eq!(px(271, 23), bg);

    let (_, buf, _) = light(Scale::new(240).unwrap(), 1000, false);
    assert_matches_ref("effects_light_2x", &buf, 2);
}

/// Grain is new on every 12 fps tick, and `reduced_motion` freezes it
/// (design.md: "reduced_motion turns off loops, time signals and
/// effects").
#[test]
fn grain_moves_at_12_fps_and_reduced_motion_freezes_it() {
    use std::time::Duration;
    let pill = |buf: &Buffer| -> Vec<[u8; 4]> { (290..330).map(|x| buf.px(x, 40)).collect() };
    let (mut r, mut buf, _) = light(Scale::ONE, 1000, false);
    let first = pill(&buf);
    assert!(
        r.wants_frame(S) || r.next_wake().is_some(),
        "its clock runs"
    );
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1000 + 84));
    assert_ne!(pill(&buf), first, "the next tick");

    let (mut r, mut buf, _) = light(Scale::ONE, 1000, true);
    let frozen = pill(&buf);
    assert!(!r.wants_frame(S) && r.next_wake().is_none(), "no clock");
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1500));
    assert_eq!(pill(&buf), frozen, "static grain");
}

/// A dark bar `w × h` logical pixels holding `nodes` (each a kind and its
/// props, placed absolutely by its own `x`/`y`), painted at `time` ms.
fn scene(
    nodes: Vec<(NodeKind, Fx)>,
    (w, h): (f32, f32),
    scale: Scale,
    time: u64,
) -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let ids = nodes
        .into_iter()
        .map(|(kind, mut p)| {
            p.push((Prop::Place, kw("absolute")));
            b.node(kind, Some(root), p)
        })
        .collect();
    let mut r = renderer();
    let mut tokens = TokenTable::default();
    tokens.insert("accent", PropValue::Color(hex("#89b4fa")));
    tokens.insert("fg", PropValue::Color(hex("#cdd6f4")));
    b.diff.set_tokens(tokens, Transition::Instant);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new((w * k).round() as u32, (h * k).round() as u32, scale);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(time));
    (r, buf, ids)
}

fn at_xy(x: f32, y: f32, w: f32, h: f32) -> Fx {
    vec![
        (Prop::X, num(x)),
        (Prop::Y, num(y)),
        (Prop::Width, num(w)),
        (Prop::Height, num(h)),
    ]
}

fn stroke(width: f32, c: &str) -> (Prop, PropValue) {
    (
        Prop::Stroke,
        PropValue::Border(Border {
            width,
            paint: Paint::Solid(hex(c)),
        }),
    )
}

/// The stroke scene: a plain stroke, a trimmed round-capped ring, a
/// dashed stroke and a wavy one on boxes; a 270° arc gauge and a full
/// ring; a wavy and a flat meter.
fn strokes(scale: Scale) -> (Renderer, Buffer, Vec<NodeId>) {
    let with = |mut base: Fx, more: Vec<(Prop, PropValue)>| {
        base.extend(more);
        base
    };
    let n = |a: f32, b: f32| PropValue::List(vec![num(a), num(b)]);
    let nodes = vec![
        (
            NodeKind::Box,
            with(
                at_xy(8.0, 8.0, 48.0, 48.0),
                vec![stroke(3.0, "#f38ba8"), (Prop::Radius, num(12.0))],
            ),
        ),
        (
            NodeKind::Box,
            with(
                at_xy(64.0, 8.0, 48.0, 48.0),
                vec![
                    stroke(5.0, "#a6e3a1"),
                    (Prop::Shape, kw("circle")),
                    (Prop::Trim, n(0.0, 0.6)),
                    (Prop::Cap, kw("round")),
                ],
            ),
        ),
        (
            NodeKind::Box,
            with(
                at_xy(120.0, 8.0, 48.0, 48.0),
                vec![
                    stroke(2.0, "#f9e2af"),
                    (Prop::Radius, num(6.0)),
                    (Prop::Dash, n(6.0, 4.0)),
                ],
            ),
        ),
        (
            NodeKind::Box,
            with(
                at_xy(176.0, 8.0, 48.0, 48.0),
                vec![stroke(2.0, "#cba6f7"), (Prop::Wave, n(2.0, 12.0))],
            ),
        ),
        (
            NodeKind::Arc,
            vec![
                (Prop::X, num(232.0)),
                (Prop::Y, num(8.0)),
                (Prop::Size, num(48.0)),
                (Prop::Value, num(0.6)),
                (Prop::Sweep, PropValue::Angle(270.0)),
                (Prop::Width, num(5.0)),
            ],
        ),
        (
            NodeKind::Arc,
            vec![
                (Prop::X, num(288.0)),
                (Prop::Y, num(8.0)),
                (Prop::Size, num(48.0)),
                (Prop::Value, num(0.25)),
                (Prop::Sweep, PropValue::Angle(360.0)),
                (Prop::Color, color("#fab387")),
            ],
        ),
        (
            NodeKind::Meter,
            with(
                at_xy(16.0, 72.0, 150.0, 4.0),
                vec![(Prop::Value, num(0.6)), (Prop::Wave, num(3.0))],
            ),
        ),
        (
            NodeKind::Meter,
            with(
                at_xy(184.0, 72.0, 150.0, 4.0),
                vec![(Prop::Value, num(0.6))],
            ),
        ),
    ];
    scene(nodes, (344.0, 88.0), scale, 1000)
}

/// design.md "Shape and geometry": stroke styles (dash, trim, caps,
/// wavy), arc and ring gauges, and the wavy media meter (refs
/// `effects_strokes.png` at 1× and 2×).
#[test]
fn strokes_arcs_and_wavy_meters_draw() {
    let (_, buf, _) = strokes(Scale::ONE);
    assert_matches_ref("effects_strokes", &buf, 2);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    let px = |x: u32, y: u32| buf.px(x, y);
    // The plain stroke is inside its box; the middle is empty.
    assert_ne!(px(9, 32), bg);
    assert_eq!(px(32, 32), bg);
    assert_eq!(px(7, 32), bg);
    // The trimmed ring (centre 88, 32) runs clockwise from 12 o'clock to
    // 0.6 of the way: the right side is drawn, the upper left is not.
    assert_ne!(px(88 + 21, 32), bg, "3 o'clock");
    assert_eq!(px(88 - 15, 32 - 15), bg, "10:30");
    // The dashed stroke has gaps along its top edge.
    let top: Vec<bool> = (128..160).map(|x| px(x, 9) != bg).collect();
    assert!(top.iter().any(|d| *d) && top.iter().any(|d| !*d));
    // The arc (centre 256, 32) has its gap at the bottom; its value
    // starts bottom left and ends past the top.
    assert_eq!(px(256, 32 + 20), bg, "the gap");
    assert_ne!(px(256, 32 - 20), bg, "the top");
    let [b, g, r, _] = px(256 - 20, 32);
    assert!(b > r && b > g, "the value is the accent at 9 o'clock");
    // The wavy meter's fill leaves its 4 px track; the flat one does not.
    let wavy = (68..86)
        .filter(|y| (16..106).any(|x| px(x, *y) != bg))
        .count();
    let flat = (68..86)
        .filter(|y| (184..334).any(|x| px(x, *y) != bg))
        .count();
    assert!(wavy >= 9, "{wavy}");
    assert_eq!(flat, 4);

    let (_, buf, _) = strokes(Scale::new(240).unwrap());
    assert_matches_ref("effects_strokes_2x", &buf, 2);
}

/// The outlines scene, on a dark bar 168 × 64: a box with a 6 px blue
/// `border:`, a 2 px pink `stroke:` over its outer edge and a white top
/// `rim`; and three nested boxes, each 4 px inside the last, with 2 px
/// pink, blue and green borders.
fn outlines(scale: Scale) -> Buffer {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let border = |w: f32, c: &str| {
        (
            Prop::Border,
            PropValue::Border(Border {
                width: w,
                paint: Paint::Solid(hex(c)),
            }),
        )
    };
    let mut one = at_xy(8.0, 8.0, 64.0, 48.0);
    one.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Radius, num(12.0)),
        border(6.0, "#89b4fa"),
        stroke(2.0, "#f38ba8"),
        (
            Prop::Rim,
            pair(kw("top"), PropValue::Color(hex("#ffffff").alpha(0.7))),
        ),
    ]);
    b.node(NodeKind::Box, Some(root), one);
    let mut parent = root;
    for (i, c) in ["#f38ba8", "#89b4fa", "#a6e3a1"].into_iter().enumerate() {
        let k = i as f32 * 4.0;
        let (x, y) = if i == 0 { (96.0, 8.0) } else { (4.0, 4.0) };
        let mut p = at_xy(x, y, 64.0 - 2.0 * k, 48.0 - 2.0 * k);
        p.extend([
            (Prop::Place, kw("absolute")),
            (Prop::Radius, num(12.0 - k)),
            border(2.0, c),
        ]);
        parent = b.node(NodeKind::Box, Some(parent), p);
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new((168.0 * k).round() as u32, (64.0 * k).round() as u32, scale);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_secs(1));
    buf
}

/// design.md "Paint and light": "multiple outlines", composed rather
/// than a list-valued `border:` (decisions.md, m4-effects-finish): a
/// `stroke:` over a wider `border:` is two concentric outlines with a
/// `rim` lighting their top, and nested bordered boxes are separate rings
/// (refs `effects_outlines.png` at 1× and 2×).
#[test]
fn multiple_outlines_compose_from_border_stroke_rim_and_nesting() {
    let buf = outlines(Scale::ONE);
    assert_matches_ref("effects_outlines", &buf, 2);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    let pink = [0xa8, 0x8b, 0xf3, 0xff];
    let blue = [0xfa, 0xb4, 0x89, 0xff];
    let green = [0xa1, 0xe3, 0xa6, 0xff];
    let px = |x: u32, y: u32| buf.px(x, y);
    // Along the first box's left edge: 2 px of stroke, then 4 px of the
    // border it does not cover, then the bar.
    assert_eq!([px(8, 32), px(9, 32)], [pink, pink]);
    assert_eq!([px(10, 32), px(13, 32)], [blue, blue]);
    assert_eq!(px(14, 32), bg);
    assert_eq!(px(7, 32), bg);
    // The rim lights the top: lighter than the same stroke at the side.
    let (top, side) = (px(40, 8), px(8, 32));
    assert!(top[1] > side[1] + 20, "rim {top:?} over {side:?}");
    // The nested boxes, from x = 96: pink, a gap, blue, a gap, green.
    let row: Vec<[u8; 4]> = (96..108).map(|x| px(x, 32)).collect();
    assert_eq!(
        row,
        vec![pink, pink, bg, bg, blue, blue, bg, bg, green, green, bg, bg]
    );

    let buf = outlines(Scale::new(240).unwrap());
    assert_matches_ref("effects_outlines_2x", &buf, 2);
}

/// design.md: a wavy meter "flattens when paused": `wave: 3` → `0`
/// springs, and `reduced_motion` snaps.
#[test]
fn a_wavy_meter_flattens_by_spring() {
    use std::time::Duration;
    let rows = |buf: &Buffer| {
        (8..36)
            .filter(|y| (16..166).any(|x| buf.px(x, *y) != [0x2e, 0x1e, 0x1e, 0xff]))
            .count()
    };
    for reduced in [false, true] {
        let (mut r, mut buf, ids) = scene(
            vec![(
                NodeKind::Meter,
                vec![
                    (Prop::X, num(16.0)),
                    (Prop::Y, num(20.0)),
                    (Prop::Width, num(150.0)),
                    (Prop::Height, num(4.0)),
                    (Prop::Value, num(0.8)),
                    (Prop::Wave, num(3.0)),
                ],
            )],
            (180.0, 44.0),
            Scale::ONE,
            1000,
        );
        let waving = rows(&buf);
        assert!(waving >= 9, "{waving}");
        r.set_reduced_motion(reduced);
        let mut d = SceneDiff::new();
        d.set(ids[0], Prop::Wave, num(0.0));
        assert!(r.apply(d).is_empty());
        buf.paint_at(&mut r, S, 1, Duration::from_millis(1040));
        let mid = rows(&buf);
        if reduced {
            assert_eq!(mid, 4, "reduced motion: flat at once");
            continue;
        }
        assert!(mid > 4, "not snapped: {mid}");
        let mut seen = vec![mid];
        let mut t = 1040;
        while r.wants_frame(S) {
            t += 16;
            buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
            seen.push(rows(&buf));
            assert!(t < 4000, "settles");
        }
        assert!(
            seen.iter().any(|n| *n > 4 && *n < waving),
            "flattens through lower waves: {seen:?}"
        );
        assert_eq!(rows(&buf), 4, "flat");
    }
}

/// Stripes of colour across a 240×64 bar with a `backdrop:` box over
/// them (`fx` its backdrop and more props), painted at 1 s.
fn backdrop_scene(fx: Vec<Fx>, scale: Scale) -> (Renderer, Buffer, Vec<NodeId>) {
    let colors = ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af"];
    let mut nodes: Vec<(NodeKind, Fx)> = (0..24)
        .map(|i| {
            let mut p = at_xy(i as f32 * 10.0, 0.0, 10.0, 64.0);
            p.push((Prop::Bg, color(colors[i % 4])));
            (NodeKind::Box, p)
        })
        .collect();
    let n = nodes.len();
    for (i, more) in fx.into_iter().enumerate() {
        let mut p = at_xy(12.0 + i as f32 * 76.0, 12.0, 64.0, 40.0);
        p.push((Prop::Radius, num(12.0)));
        p.extend(more);
        nodes.push((NodeKind::Box, p));
    }
    let (r, buf, ids) = scene(nodes, (240.0, 64.0), scale, 1000);
    (r, buf, ids[n..].to_vec())
}

/// design.md "Filters and compositing": `backdrop: blur(16)` blurs the
/// surface's own content behind a box (at quarter scale), in its
/// outline and under its background; `backdrop: glass()` falls back to
/// a blur and a tint (refs `effects_backdrop.png` at 1× and 2×). A
/// change behind it repaints it.
#[test]
fn backdrop_blurs_what_is_behind() {
    let fx = || {
        vec![
            vec![(Prop::Backdrop, call("blur", vec![num(16.0)]))],
            vec![
                (Prop::Backdrop, call("blur", vec![num(3.0)])),
                (Prop::Bg, PropValue::Color(hex("#1e1e2e").alpha(0.3))),
            ],
            vec![(Prop::Backdrop, call("glass", vec![]))],
        ]
    };
    let (mut r, mut buf, ids) = backdrop_scene(fx(), Scale::ONE);
    assert_matches_ref("effects_backdrop", &buf, 2);
    // Unblurred stripes are pure; under the strong blur each pixel mixes
    // its neighbours: no channel at a stripe's own extreme.
    let stripe = buf.px(5, 32);
    assert_eq!(stripe, [0xa8, 0x8b, 0xf3, 0xff]);
    let mixed = buf.px(45, 32);
    let pure = [buf.px(45, 2), buf.px(35, 2), buf.px(55, 2)];
    assert!(!pure.contains(&mixed), "{mixed:?} {pure:?}");
    // Outside its rounded corner, the stripes are untouched.
    assert_eq!(buf.px(12, 12), buf.px(12, 2));
    // Glass is lighter than the plain blur of similar content.
    let lum = |p: [u8; 4]| p[0] as u32 + p[1] as u32 + p[2] as u32;
    let glass = (170..220).map(|x| lum(buf.px(x, 32))).sum::<u32>();
    let plain = (180..230).map(|x| lum(buf.px(x, 2))).sum::<u32>();
    assert!(glass > plain * 9 / 10, "{glass} {plain}");
    // A stripe behind the blur changes colour: the box repaints with it.
    let before = buf.px(44, 32);
    let mut d = SceneDiff::new();
    let stripe_id = NodeId::new(ids[0].index - 20, ids[0].generation);
    d.set(stripe_id, Prop::Bg, color("#11111b"));
    assert!(r.apply(d).is_empty());
    let damage = buf.paint_at(&mut r, S, 1, std::time::Duration::from_millis(1100));
    assert!(!damage.is_empty());
    assert_ne!(
        buf.px(44, 32),
        before,
        "the backdrop follows what is behind"
    );

    let (_, buf, _) = backdrop_scene(fx(), Scale::new(240).unwrap());
    assert_matches_ref("effects_backdrop_2x", &buf, 2);
}

/// A `backdrop:` inside an offscreen ancestor (a `filter: grayscale(1)`
/// holder) shows what is behind it on the surface, blurred, and the
/// ancestor filters it with the rest of its content: grey and mixed
/// inside, the stripes untouched outside (ref `effects_backdrop_nested.png`).
#[test]
fn a_backdrop_inside_a_filtered_ancestor_shows_what_is_behind() {
    let colors = ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af"];
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    for i in 0..12 {
        let mut p = at_xy(i as f32 * 10.0, 0.0, 10.0, 64.0);
        p.push((Prop::Bg, color(colors[i % 4])));
        p.push((Prop::Place, kw("absolute")));
        b.node(NodeKind::Box, Some(root), p);
    }
    let mut p = at_xy(12.0, 12.0, 64.0, 40.0);
    p.push((Prop::Place, kw("absolute")));
    p.push((Prop::Filter, call("grayscale", vec![num(1.0)])));
    let holder = b.node(NodeKind::Box, Some(root), p);
    let mut p = at_xy(0.0, 0.0, 64.0, 40.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Radius, num(12.0)),
        (Prop::Backdrop, call("blur", vec![num(16.0)])),
    ]);
    b.node(NodeKind::Box, Some(holder), p);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(120, 64, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    assert_matches_ref("effects_backdrop_nested", &buf, 2);
    // Grey, but for the little of the sharp stripes that shows where the
    // blur reaches past the surface's edge (transparent there).
    let chroma = |p: [u8; 4]| p[..3].iter().max().unwrap() - p[..3].iter().min().unwrap();
    let grey = |p: [u8; 4]| chroma(p) <= 16;
    let pure = [buf.px(45, 2), buf.px(35, 2), buf.px(55, 2)];
    for x in [30, 45, 60] {
        let p = buf.px(x, 32);
        assert!(grey(p) && p[3] == 255, "({x}, 32) grey: {p:?}");
        assert!(!pure.contains(&p));
    }
    // Neighbouring stripes blend: not one stripe's own grey.
    assert_ne!(buf.px(31, 32), buf.px(39, 32));
    assert!(
        !grey(buf.px(45, 2)),
        "outside, the stripes keep their colour"
    );
}

/// The five built-in effects and a particle field, 72×48 each, on a
/// 432×64 bar; the first frame at 1 s (their clocks' `t = 0`).
fn generative(scale: Scale, reduced: bool) -> (Renderer, Buffer) {
    let mut nodes: Vec<(NodeKind, Fx)> = ["lightning", "sparks", "shimmer", "ripple", "aurora"]
        .iter()
        .enumerate()
        .map(|(i, style)| {
            let mut p = at_xy(i as f32 * 72.0, 8.0, 72.0, 48.0);
            p.push((Prop::Style, kw(style)));
            (NodeKind::Effect, p)
        })
        .collect();
    let mut p = at_xy(360.0, 8.0, 72.0, 48.0);
    p.extend([
        (Prop::Rate, num(20.0)),
        (
            Prop::Life,
            PropValue::Duration(std::time::Duration::from_millis(1200)),
        ),
        (Prop::Sprite, call("dot", vec![num(3.0)])),
        (Prop::Glow, num(6.0)),
        (Prop::Color, color("#f5c2e7")),
    ]);
    nodes.push((NodeKind::Particles, p));
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    for (kind, mut p) in nodes {
        p.push((Prop::Place, kw("absolute")));
        b.node(kind, Some(root), p);
    }
    let mut tokens = TokenTable::default();
    tokens.insert("accent", PropValue::Color(hex("#89b4fa")));
    b.diff.set_tokens(tokens, Transition::Instant);
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new((432.0 * k).round() as u32, (64.0 * k).round() as u32, scale);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    (r, buf)
}

/// design.md "Generative, data-driven and media": the built-in effects
/// (aurora as its CPU fallback) and particles as CPU sprite blits, drawn
/// at fixed `t` (refs `effects_generative_0.png` at `t = 0`,
/// `effects_generative_100ms.png` at 0.1 s and `effects_generative_2x.png`).
#[test]
fn builtin_effects_and_particles_draw_at_fixed_times() {
    use std::time::Duration;
    let (mut r, mut buf) = generative(Scale::ONE, false);
    assert_matches_ref("effects_generative_0", &buf, 2);
    assert!(r.wants_frame(S), "their clocks run");
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1100));
    assert_matches_ref("effects_generative_100ms", &buf, 2);
    // Each cell has ink.
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    for i in 0..6u32 {
        let lit = (i * 72..i * 72 + 72)
            .flat_map(|x| (8..56).map(move |y| (x, y)))
            .filter(|&(x, y)| buf.px(x, y) != bg)
            .count();
        assert!(lit > 20, "cell {i}: {lit}");
    }
    let (_, buf) = generative(Scale::new(240).unwrap(), false);
    assert_matches_ref("effects_generative_2x", &buf, 2);
}

/// A renderer with no GPU to draw on: in a `gpu` build, one whose device
/// was refused (the CPU fallbacks and their notices are for it).
fn no_gpu() -> Renderer {
    #[allow(unused_mut)]
    let mut r = renderer();
    #[cfg(feature = "gpu")]
    r.deliver_gpu(strand_gpu::GpuReply::Unavailable(strand_gpu::GpuError {
        kind: strand_gpu::GpuErrorKind::NoAdapter,
        message: "no adapter (test)".into(),
    }));
    r
}

/// m4-plan: "Before the GPU path, particles cap at 1,000 and aurora is
/// static, with a notice." Without a GPU, aurora alone runs no clock and
/// draws the same still frame at any time; the renderer says once that
/// aurora is still and, when `rate × life` passes 1,000, that particles
/// are capped. (With one they are the GPU's: `tests/gpu.rs`.)
#[test]
fn aurora_is_still_and_cpu_fallbacks_are_noticed_once() {
    use std::time::Duration;
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(0.0, 0.0, 72.0, 48.0);
    p.extend([(Prop::Place, kw("absolute")), (Prop::Style, kw("aurora"))]);
    b.node(NodeKind::Effect, Some(root), p);
    let mut r = no_gpu();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(72, 48, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
    let first = buf.pixels.clone();
    assert!(first.iter().any(|&p| p != first[0]), "the curtains drawn");
    assert!(!r.wants_frame(S) && r.next_wake().is_none(), "no clock");
    let notices = r.take_effect_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("aurora"), "{notices:?}");
    buf.paint_at(&mut r, S, 0, Duration::from_millis(3000));
    assert!(buf.pixels == first, "still");
    assert!(r.take_effect_notices().is_empty(), "said once");

    // Particles: capped above 1,000 alive, and said so; not below.
    for (rate, said) in [(20.0, false), (2000.0, true)] {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![]);
        let mut p = at_xy(0.0, 0.0, 72.0, 48.0);
        p.extend([
            (Prop::Place, kw("absolute")),
            (Prop::Rate, num(rate)),
            (Prop::Life, PropValue::Duration(Duration::from_secs(1))),
        ]);
        b.node(NodeKind::Particles, Some(root), p);
        let mut r = no_gpu();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(72, 48, Scale::ONE);
        buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
        let notices = r.take_effect_notices();
        assert_eq!(!notices.is_empty(), said, "rate {rate}: {notices:?}");
        assert!(notices.iter().all(|n| n.contains("1,000")));
    }
}

/// design.md: "`reduced_motion` turns off loops, time signals and
/// effects": every built-in effect and the particles hold their first
/// frame, and no clock runs.
#[test]
fn reduced_motion_freezes_effects_and_particles() {
    use std::time::Duration;
    let (mut r, mut buf) = generative(Scale::ONE, true);
    let first = buf.pixels.clone();
    assert!(!r.wants_frame(S) && r.next_wake().is_none(), "no clock");
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1500));
    assert!(buf.pixels == first, "still");
    // The still frame is their t = 0 frame.
    let (_, moving) = generative(Scale::ONE, false);
    assert!(moving.pixels == first);
}

fn mul(a: TokenExpr, b: TokenExpr) -> TokenExpr {
    TokenExpr::Binary {
        op: BinOp::Mul,
        lhs: Box::new(a),
        rhs: Box::new(b),
    }
}

fn val(n: f32) -> TokenExpr {
    TokenExpr::value(num(n))
}

/// The text scene on a 360×64 bar: an outlined, gradient-filled word,
/// and a word whose `letters` wave (`y: 6 * wave(1s, phase: index *
/// 0.15)`) and turn and fade by index; painted at 1 s, then at `t`.
fn words(scale: Scale, t_ms: u64) -> (Renderer, Buffer) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let font = |size| (Prop::Font, PropValue::Font(common::font(size)));
    let mut p = at_xy(12.0, 10.0, 160.0, 44.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Text, text("Strand")),
        font(32.0),
        (
            Prop::Fill,
            PropValue::Paint(Paint::Linear {
                angle: 90.0,
                stops: vec![
                    GradientStop {
                        color: hex("#f38ba8"),
                        offset: 0.0,
                    },
                    GradientStop {
                        color: hex("#89b4fa"),
                        offset: 1.0,
                    },
                ],
            }),
        ),
        (
            Prop::TextStroke,
            PropValue::Border(Border {
                width: 1.5,
                paint: Paint::Solid(hex("#f9e2af")),
            }),
        ),
    ]);
    b.node(NodeKind::Text, Some(root), p);
    let mut p = at_xy(190.0, 12.0, 160.0, 40.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Text, text("letters")),
        font(26.0),
        (Prop::Color, color("#a6e3a1")),
    ]);
    let word = b.node(NodeKind::Text, Some(root), p);
    b.node(
        NodeKind::Letters,
        Some(word),
        vec![
            (
                Prop::Y,
                PropValue::Token(mul(
                    val(6.0),
                    TokenExpr::Wave {
                        period: std::time::Duration::from_secs(1),
                        phase: Box::new(mul(TokenExpr::Index, val(0.15))),
                    },
                )),
            ),
            (
                Prop::Rotate,
                PropValue::Token(mul(TokenExpr::Index, val(4.0))),
            ),
            (
                Prop::Opacity,
                PropValue::Token(TokenExpr::Binary {
                    op: BinOp::Sub,
                    lhs: Box::new(val(1.0)),
                    rhs: Box::new(mul(TokenExpr::Index, val(0.1))),
                }),
            ),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let k = scale.as_f32();
    let mut buf = Buffer::new((360.0 * k).round() as u32, (64.0 * k).round() as u32, scale);
    let t0 = std::time::Duration::from_millis(1000);
    buf.paint_at(&mut r, S, 0, t0);
    if t_ms > 0 {
        buf.paint_at(&mut r, S, 1, t0 + std::time::Duration::from_millis(t_ms));
    }
    (r, buf)
}

/// design.md "Paint and light": text effects — `text_stroke:` outlines
/// the letters, `fill:` paints them with a gradient, and `letters`
/// animates each letter of its text by `index` (refs `effects_text.png`
/// at `t = 250 ms`, `effects_text_2x.png` at 0).
#[test]
fn text_stroke_fill_and_letters_draw() {
    let (r, buf) = words(Scale::ONE, 250);
    assert_matches_ref("effects_text", &buf, 2);
    assert!(r.wants_frame(S), "the letters' wave runs a clock");
    // The fill runs pink to blue across the word: the reddest glyph
    // pixel on the left, the bluest on the right.
    let glyph = |x0: u32, x1: u32| {
        (x0..x1)
            .flat_map(|x| (10..54).map(move |y| (x, y)))
            .map(|(x, y)| buf.px(x, y))
            .filter(|p| p[1] < 0xa0 && (p[2] > 0xc0 || p[0] > 0xc0))
            .map(|p| p[2] as i32 - p[0] as i32)
            .collect::<Vec<i32>>()
    };
    let left = glyph(12, 50);
    let right = glyph(75, 110);
    assert!(!left.is_empty() && !right.is_empty());
    let avg = |v: &[i32]| v.iter().sum::<i32>() / v.len() as i32;
    assert!(avg(&left) > avg(&right), "{} {}", avg(&left), avg(&right));
    // The stroke: yellow (#f9e2af) pixels around the letters.
    let yellow = (12..172)
        .flat_map(|x| (8..56).map(move |y| (x, y)))
        .filter(|&(x, y)| {
            let p = buf.px(x, y);
            p[2] > 0xd0 && p[1] > 0xb0 && p[0] < 0xc0
        })
        .count();
    assert!(yellow > 50, "outline pixels: {yellow}");

    // The letters: at t = 0 every wave reads its phase; later frames
    // differ (they move), and a frozen clock holds t = 0.
    let (_, still) = words(Scale::ONE, 0);
    assert!(still.pixels != buf.pixels);
    let (_, buf) = words(Scale::new(240).unwrap(), 0);
    assert_matches_ref("effects_text_2x", &buf, 2);
}

/// A `roll: true` text reading `value` in a 120×40 bar at 1 s.
fn rolling(value: &str, reduced: bool) -> (Renderer, Buffer, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(10.0, 4.0, 100.0, 32.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Text, text(value)),
        (Prop::Font, PropValue::Font(common::font(24.0))),
        (Prop::Color, color("#cdd6f4")),
        (Prop::Roll, PropValue::Bool(true)),
    ]);
    let id = b.node(NodeKind::Text, Some(root), p);
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(120, 40, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    (r, buf, id)
}

/// design.md "Motion and time": `text pct(level) { roll: true }` — a
/// changed digit rolls (the old one up and out, the new one up and in)
/// while the others stay; it settles to the new text, and
/// `reduced_motion` shows it at once (ref `effects_roll.png` mid-roll).
#[test]
fn rolling_numbers_roll_only_the_changed_digit() {
    use std::time::Duration;
    let (mut r, mut buf, id) = rolling("41%", false);
    let before = buf.pixels.clone();
    let mut d = SceneDiff::new();
    d.set(id, Prop::Text, text("42%"));
    assert!(r.apply(d).is_empty());
    let mut t = 1016;
    buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    t += 50;
    buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    assert_matches_ref("effects_roll", &buf, 2);
    let (_, settled, _) = rolling("42%", false);
    let column = |b: &Buffer, x0: u32, x1: u32| -> Vec<[u8; 4]> {
        (x0..x1)
            .flat_map(|x| (0..40).map(move |y| (x, y)))
            .map(|(x, y)| b.px(x, y))
            .collect()
    };
    // The `4` (the first ~15 px of the text) stays; the middle digit is
    // neither the old nor the new one.
    assert!(
        column(&buf, 10, 24) == column(&settled, 10, 24),
        "the 4 stays"
    );
    let mid = column(&buf, 26, 40);
    assert!(mid != column(&settled, 26, 40) && mid != column_of(&before, 26, 40));
    while r.wants_frame(S) {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
        assert!(t < 4000, "settles");
    }
    assert!(buf.pixels == settled.pixels, "settles on 42%");

    // Reduced motion: the new text at once.
    let (mut r, mut buf, id) = rolling("41%", true);
    let mut d = SceneDiff::new();
    d.set(id, Prop::Text, text("42%"));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
    assert!(buf.pixels == settled.pixels);
}

fn column_of(px: &[u8], x0: u32, x1: u32) -> Vec<[u8; 4]> {
    (x0..x1)
        .flat_map(|x| (0..40u32).map(move |y| (x, y)))
        .map(|(x, y)| {
            let i = ((y * 120 + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        })
        .collect()
}

/// A compiled keyframes block, as logic sends it with `play`.
fn keyframes(
    name: &str,
    seq: u32,
    ms: u64,
    repeat: Option<u32>,
    stops: Vec<(f32, Fx)>,
) -> (Prop, PropValue) {
    let mut k = Keyframes::new(name, seq, std::time::Duration::from_millis(ms));
    k.repeat = repeat;
    k.stops = stops;
    (Prop::Play, PropValue::Keyframes(std::sync::Arc::new(k)))
}

fn shake(seq: u32) -> (Prop, PropValue) {
    keyframes(
        "shake",
        seq,
        400,
        Some(1),
        vec![
            (0.0, vec![(Prop::X, num(0.0))]),
            (0.25, vec![(Prop::X, num(-8.0))]),
            (0.75, vec![(Prop::X, num(8.0))]),
            (1.0, vec![(Prop::X, num(0.0))]),
        ],
    )
}

/// Three 30 px boxes on a 200 × 50 bar, painted first at 1 s: `shake`
/// (x out and back), `flash` (bg and opacity at 50%, from and back to
/// the box's own) and `spin` (a quarter turn a second, forever). With
/// `play: false` the boxes play nothing.
fn keyframed(play: bool, reduced: bool) -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let plays = [
        shake(1),
        keyframes(
            "flash",
            1,
            400,
            Some(1),
            vec![(
                0.5,
                vec![(Prop::Bg, color("#f38ba8")), (Prop::Opacity, num(0.5))],
            )],
        ),
        keyframes(
            "spin",
            1,
            1000,
            None,
            vec![
                (0.0, vec![(Prop::Rotate, PropValue::Angle(0.0))]),
                (1.0, vec![(Prop::Rotate, PropValue::Angle(90.0))]),
            ],
        ),
    ];
    let ids = plays
        .into_iter()
        .enumerate()
        .map(|(i, kf)| {
            let mut p = at_xy(10.0 + 60.0 * i as f32, 10.0, 30.0, 30.0);
            p.extend([(Prop::Place, kw("absolute")), (Prop::Bg, color("#cba6f7"))]);
            if play {
                p.push(kf);
            }
            b.node(NodeKind::Box, Some(root), p)
        })
        .collect();
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(200, 50, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    (r, buf, ids)
}

/// The pixels of columns `x0..x1`.
fn columns(b: &Buffer, x0: u32, x1: u32) -> Vec<[u8; 4]> {
    (x0..x1)
        .flat_map(|x| (0..b.size.h).map(move |y| (x, y)))
        .map(|(x, y)| b.px(x, y))
        .collect()
}

/// design.md "Motion and time": `keyframes shake { … }` + `play shake`.
/// A block plays from the first frame that draws it, offsets composed
/// with the node's own values (x added, opacity multiplied, a colour
/// from the node's own and back), and draws the node as it is once
/// played; the same `seq` never replays and a new one restarts it (ref
/// `effects_keyframes.png` a quarter of the way in).
#[test]
fn keyframes_play_once_and_restart_on_a_new_seq() {
    use std::time::Duration;
    let (mut r, mut buf, ids) = keyframed(true, false);
    let (_, rest, _) = keyframed(false, false);
    let lilac = rest.px(25, 25);
    assert!(r.wants_frame(S));
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1100));
    assert_matches_ref("effects_keyframes", &buf, 2);
    // `shake` at 25%: 8 px left of its box.
    assert_eq!(buf.px(4, 25), lilac);
    assert_ne!(buf.px(36, 25), lilac);
    // `flash` halfway to pink at half opacity: neither lilac nor pink.
    let f = buf.px(85, 25);
    assert!(f != lilac && f != rest.px(85, 5), "{f:?}");
    let mut t = 1100;
    while t < 1500 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    }
    assert!(
        columns(&buf, 0, 120) == columns(&rest, 0, 120),
        "played: at rest"
    );
    assert!(r.wants_frame(S), "the loop keeps playing");
    let spin = columns(&buf, 120, 200);
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1750));
    assert!(columns(&buf, 120, 200) != spin, "the loop moves");
    assert!(columns(&buf, 0, 120) == columns(&rest, 0, 120), "no replay");

    // A new `seq` restarts it.
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Play, shake(2).1);
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(2000));
    buf.paint_at(&mut r, S, 1, Duration::from_millis(2100));
    assert_eq!(buf.px(4, 25), lilac, "playing again");
}

/// `reduced_motion`: a block that plays a fixed number of times is not
/// played, and a loop holds still at its start.
#[test]
fn reduced_motion_skips_keyframes_and_freezes_loops() {
    use std::time::Duration;
    let (mut r, mut buf, _) = keyframed(true, true);
    let (_, rest, _) = keyframed(false, true);
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1100));
    assert!(buf.pixels == rest.pixels, "nothing plays; the loop at 0deg");
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1600));
    assert!(buf.pixels == rest.pixels);
    assert!(!r.wants_frame(S));
}

/// A loop that pulses a box's background to pink and back each second.
fn pulse_loop() -> (Prop, PropValue) {
    keyframes(
        "pulse",
        1,
        1000,
        None,
        vec![(0.5, vec![(Prop::Bg, color("#f38ba8"))])],
    )
}

/// design.md: "the frame loop stops when every clock is idle". A looping
/// `play` runs only while its node is drawn: under `opacity: 0` (its own
/// or an ancestor's, set by logic) or outside its parent's clip it wants
/// no frames and no wake, and once shown it plays again. A loop that
/// fades its own opacity through 0 keeps playing (as a loop that moves a
/// node outside its clip would: what places it follows time, the rule
/// time signals keep).
#[test]
fn a_hidden_or_clipped_loop_wants_no_frames() {
    use std::time::Duration;
    // (hidden, own opacity, parent opacity, x inside a clipped parent)
    let cases = [
        ("drawn", true, 1.0, 1.0, 10.0),
        ("opacity 0", false, 0.0, 1.0, 10.0),
        ("hidden parent", false, 1.0, 0.0, 10.0),
        ("outside the clip", false, 1.0, 1.0, 500.0),
    ];
    for (name, drawn, own, parent, x) in cases {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let mut gp = at_xy(0.0, 0.0, 200.0, 50.0);
        gp.extend([
            (Prop::Place, kw("absolute")),
            (Prop::Clip, PropValue::Bool(true)),
            (Prop::Opacity, num(parent)),
        ]);
        let g = b.node(NodeKind::Box, Some(root), gp);
        let mut p = at_xy(x, 10.0, 30.0, 30.0);
        p.extend([
            (Prop::Place, kw("absolute")),
            (Prop::Bg, color("#cba6f7")),
            (Prop::Opacity, num(own)),
            pulse_loop(),
        ]);
        let n = b.node(NodeKind::Box, Some(g), p);
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(200, 50, Scale::ONE);
        buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
        buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
        assert_eq!(r.wants_frame(S), drawn, "{name}");
        if drawn {
            continue;
        }
        assert_eq!(r.next_wake(), None, "{name}: no wake either");
        // Shown again: it plays (in phase with its start).
        let mut d = SceneDiff::new();
        set_now(&mut d, g, Prop::Opacity, num(1.0));
        set_now(&mut d, n, Prop::Opacity, num(1.0));
        set_now(&mut d, n, Prop::X, num(10.0));
        assert!(r.apply(d).is_empty());
        buf.paint_at(&mut r, S, 1, Duration::from_millis(1250));
        assert!(r.wants_frame(S), "{name}: shown, it plays");
        let before = columns(&buf, 0, 60);
        buf.paint_at(&mut r, S, 1, Duration::from_millis(1500));
        assert!(columns(&buf, 0, 60) != before, "{name}: and moves");
    }

    // A loop that blinks its own opacity through 0 keeps playing.
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(10.0, 10.0, 30.0, 30.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Bg, color("#cba6f7")),
        keyframes(
            "blink",
            1,
            1000,
            None,
            vec![
                (0.0, vec![(Prop::Opacity, num(0.0))]),
                (1.0, vec![(Prop::Opacity, num(1.0))]),
            ],
        ),
    ]);
    b.node(NodeKind::Box, Some(root), p);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(200, 50, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
    assert!(r.wants_frame(S), "blinking at opacity 0: still playing");
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1500));
    assert!(r.wants_frame(S));
    assert_ne!(buf.px(25, 25), buf.px(100, 25), "half faded in");
}

/// A row with `stagger: 100ms` on a 120 × 40 bar, painted at 1 s, then
/// four 20 px boxes with `enter { opacity: 0 }` created in it at once and
/// painted at 1.016 s.
fn staggered(reduced: bool) -> (Renderer, Buffer) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(10.0, 10.0, 100.0, 20.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Gap, num(6.0)),
        (
            Prop::Stagger,
            PropValue::Duration(std::time::Duration::from_millis(100)),
        ),
    ]);
    let row = b.node(NodeKind::Row, Some(root), p);
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(120, 40, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    let mut d = SceneDiff::new();
    for i in 0..4u32 {
        let n = NodeId::new(100 + i, 0);
        d.create(n, NodeKind::Box, Some(row), i)
            .set(n, Prop::Size, num(20.0))
            .set(n, Prop::Bg, color("#a6e3a1"))
            .set(
                n,
                Prop::Enter,
                PropValue::Pose(vec![(Prop::Opacity, num(0.0))]),
            );
    }
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, std::time::Duration::from_millis(1016));
    (r, buf)
}

/// design.md "Motion and time": `stagger: 30ms` on a container delays
/// each child's `enter` that much after the one before; `reduced_motion`
/// shows them all at once (ref `effects_stagger.png` at 150 ms).
#[test]
fn stagger_delays_each_childs_enter() {
    use std::time::Duration;
    let (mut r, mut buf) = staggered(false);
    let bg = buf.px(1, 1);
    // Box i's centre: x = 10 + 26 i + 10.
    let alpha = |b: &Buffer, i: u32| b.px(20 + 26 * i, 20);
    assert_ne!(alpha(&buf, 0), bg, "the first starts at once");
    assert_eq!(alpha(&buf, 1), bg, "the second waits");
    let mut t = 1016;
    while t < 1150 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t.min(1150)));
    }
    assert_matches_ref("effects_stagger", &buf, 2);
    // The first further along than the second; the third (from 200 ms)
    // and fourth unseen.
    let g = |b: &Buffer, i: u32| alpha(b, i)[1];
    assert!(
        g(&buf, 0) > g(&buf, 1) && g(&buf, 1) > bg[1],
        "{:?}",
        (0..4).map(|i| g(&buf, i)).collect::<Vec<_>>()
    );
    assert_eq!((alpha(&buf, 2), alpha(&buf, 3)), (bg, bg));
    while r.wants_frame(S) {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
        assert!(t < 5000, "settles");
    }
    let shown = alpha(&buf, 0);
    assert!((1..4).all(|i| alpha(&buf, i) == shown), "all shown");

    let (r, buf) = staggered(true);
    assert!((0..4).all(|i| alpha(&buf, i) == shown), "reduced: at once");
    assert!(!r.wants_frame(S));
}

/// A 30 px box with `parallax: 6px` and one with `tilt: 12deg` on a
/// 160 × 60 bar, painted at 1 s (no `lean` props with `lean: false`).
fn leaning(lean: bool, reduced: bool) -> (Renderer, Buffer) {
    let mut p0 = at_xy(25.0, 15.0, 30.0, 30.0);
    let mut p1 = at_xy(105.0, 15.0, 30.0, 30.0);
    for p in [&mut p0, &mut p1] {
        p.push((Prop::Bg, color("#89b4fa")));
    }
    if lean {
        p0.push((Prop::Parallax, num(6.0)));
        p1.push((Prop::Tilt, PropValue::Angle(12.0)));
    }
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    for p in [p0, p1] {
        let mut p = p;
        p.push((Prop::Place, kw("absolute")));
        b.node(NodeKind::Box, Some(root), p);
    }
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(160, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    (r, buf)
}

/// Paints frames 16 ms apart from `t` until nothing moves; the last time.
fn settle(r: &mut Renderer, buf: &mut Buffer, mut t: u64) -> u64 {
    while r.wants_frame(S) {
        t += 16;
        buf.paint_at(r, S, 1, std::time::Duration::from_millis(t));
        assert!(t < 10_000, "settles");
    }
    t
}

/// design.md "Motion and time": `parallax: 6px` moves a node towards the
/// pointer (all the way with the pointer at the surface's corner) and
/// `tilt: 12deg` turns it in its plane towards the side of its box the
/// pointer is on (2D on the CPU); both spring back when the pointer
/// leaves, a surface with neither ignores the pointer, and
/// `reduced_motion` turns them off (ref `effects_lean.png`, settled with
/// the pointer at the top right).
#[test]
fn parallax_and_tilt_follow_the_pointer() {
    let (mut r, mut buf) = leaning(true, false);
    let (_, rest) = leaning(false, false);
    assert!(buf.pixels == rest.pixels, "no pointer: at rest");
    assert!(!r.wants_frame(S));
    r.set_pointer(S, Some(LogicalPoint { x: 160.0, y: 0.0 }));
    assert!(r.wants_frame(S), "the pointer moved: a frame");
    let t = settle(&mut r, &mut buf, 1000);
    assert_matches_ref("effects_lean", &buf, 2);
    let blue = rest.px(40, 30);
    // Parallax: 6 px right and up, so its left column (x 25..31) shows
    // the bar and x 56..61 the box; its top 6 rows moved up.
    assert_eq!(buf.px(58, 30), blue);
    assert_ne!(buf.px(27, 30), blue);
    assert_eq!(buf.px(40, 10), blue);
    // Tilt: turned, its corners off the axis-aligned square.
    assert_ne!(buf.px(106, 16), rest.px(106, 16));
    // The pointer leaves: back to rest.
    r.set_pointer(S, None);
    assert!(r.wants_frame(S));
    settle(&mut r, &mut buf, t);
    assert!(buf.pixels == rest.pixels, "back at rest");

    // Nothing leans: the pointer asks for no frame.
    let (mut r, _) = leaning(false, false);
    r.set_pointer(S, Some(LogicalPoint { x: 160.0, y: 0.0 }));
    assert!(!r.wants_frame(S));

    // Reduced motion: off.
    let (mut r, mut buf) = leaning(true, true);
    let (_, rest) = leaning(false, true);
    r.set_pointer(S, Some(LogicalPoint { x: 160.0, y: 0.0 }));
    settle(&mut r, &mut buf, 1000);
    buf.paint_at(&mut r, S, 1, std::time::Duration::from_millis(1100));
    assert!(buf.pixels == rest.pixels);
}

const TRANSITIONS: [(&str, &str); 4] = [
    ("wipe", "left"),
    ("disc", ""),
    ("dissolve", ""),
    ("pixelate", ""),
];

/// Sets `prop` with `~ instant` (a node created on a shown surface
/// would spring its props in from their defaults).
fn set_now(d: &mut SceneDiff, id: NodeId, prop: Prop, value: PropValue) {
    d.push(SceneOp::SetProp {
        id,
        prop,
        value,
        transition: Transition::Instant,
    });
}

/// The value of `transition:` for an entry of [`TRANSITIONS`].
fn transition_of((name, arg): (&str, &str)) -> PropValue {
    if arg.is_empty() {
        kw(name)
    } else {
        call(name, vec![kw(arg)])
    }
}

/// An empty 200 × 60 bar painted at 1 s, then (with `masks`) a 40 px
/// box per transition mask created on it and painted at 1.016 s. Returns
/// the boxes.
fn masked(masks: bool, reduced: bool) -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(200, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    let mut d = SceneDiff::new();
    let mut ids = Vec::new();
    for (i, t) in TRANSITIONS.into_iter().enumerate() {
        let n = NodeId::new(200 + i as u32, 0);
        d.create(n, NodeKind::Box, Some(root), i as u32);
        for (p, v) in at_xy(8.0 + 48.0 * i as f32, 10.0, 40.0, 40.0) {
            set_now(&mut d, n, p, v);
        }
        set_now(&mut d, n, Prop::Place, kw("absolute"));
        set_now(&mut d, n, Prop::Bg, color("#f9e2af"));
        if masks {
            d.set(n, Prop::Transition, transition_of(t));
        }
        ids.push(n);
    }
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, std::time::Duration::from_millis(1016));
    (r, buf, ids)
}

/// How many pixels of box `i` (of [`masked`]) are drawn at all.
fn shown(buf: &Buffer, i: u32) -> usize {
    let bg = buf.px(199, 59);
    let x0 = 8 + 48 * i;
    (x0..x0 + 40)
        .flat_map(|x| (10..50).map(move |y| (x, y)))
        .filter(|&(x, y)| buf.px(x, y) != bg)
        .count()
}

/// design.md "Motion and time": `transition: wipe(left) | disc |
/// dissolve | pixelate`. A node with one is revealed through its mask as
/// it enters (ref `effects_transitions.png` partway) and hidden by it as
/// it leaves, its ghost kept until the mask is done; `reduced_motion`
/// swaps at once.
#[test]
fn transition_masks_reveal_and_hide() {
    use std::time::Duration;
    let (mut r, mut buf, ids) = masked(true, false);
    let (_, plain, _) = masked(false, false);
    let full = shown(&plain, 0);
    assert_eq!(full, 1600);
    let mut t = 1016;
    while t < 1080 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    }
    assert_matches_ref("effects_transitions", &buf, 2);
    for i in 0..4 {
        let n = shown(&buf, i);
        assert!(n > 0 && (i == 3 || n < full), "mask {i} partway: {n}");
    }
    // pixelate: fading in, so not yet the box's colour.
    assert_ne!(buf.px(8 + 48 * 3 + 20, 30), plain.px(8 + 48 * 3 + 20, 30));
    // The wipe grows from the left: its left column is in, its right not.
    assert_eq!(buf.px(9, 30), plain.px(9, 30));
    assert_ne!(buf.px(46, 30), plain.px(46, 30));
    t = settle(&mut r, &mut buf, t);
    assert!(buf.pixels == plain.pixels, "revealed");

    // Removed: each hides behind its mask, then goes.
    let mut d = SceneDiff::new();
    for id in &ids {
        d.push(SceneOp::Remove {
            id: *id,
            window: false,
        });
    }
    assert!(r.apply(d).is_empty());
    t += 16;
    buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    t += 48;
    buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    for i in 0..4 {
        let n = shown(&buf, i);
        // (pixelate fades and blurs: every pixel is touched.)
        assert!(n > 0 && (i == 3 || n < full), "mask {i} closing: {n}");
    }
    settle(&mut r, &mut buf, t);
    assert!((0..4).all(|i| shown(&buf, i) == 0), "hidden");
    assert!(
        ids.iter().all(|id| r.tree().get(*id).is_none()),
        "the ghosts are gone"
    );

    // Reduced motion: at once.
    let (r, buf, _) = masked(true, true);
    assert!(buf.pixels == plain.pixels);
    assert!(!r.wants_frame(S));
}

/// design.md "Motion and time": `transition: pixelate` draws the subtree
/// at low resolution and samples it back up: partway, every pixel of a
/// cell (square, aligned to the box's corner) is the cell's average, so
/// 1 px stripes spread into blocks (ref `effects_pixelate.png`); the cells
/// shrink to the plain drawing as it fades in.
#[test]
fn pixelate_draws_a_mosaic_that_refines() {
    use std::time::Duration;
    let build = |masks: bool| {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(80, 64, Scale::ONE);
        buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
        let mut d = SceneDiff::new();
        let n = NodeId::new(200, 0);
        d.create(n, NodeKind::Box, Some(root), 0);
        for (p, v) in at_xy(10.0, 8.0, 48.0, 48.0) {
            set_now(&mut d, n, p, v);
        }
        set_now(&mut d, n, Prop::Place, kw("absolute"));
        set_now(&mut d, n, Prop::Bg, color("#89b4fa"));
        if masks {
            d.set(n, Prop::Transition, kw("pixelate"));
        }
        // 1 px stripes every 4 px.
        for i in 0..12u32 {
            let c = NodeId::new(300 + i, 0);
            d.create(c, NodeKind::Box, Some(n), i);
            for (p, v) in at_xy(1.0 + 4.0 * i as f32, 0.0, 1.0, 48.0) {
                set_now(&mut d, c, p, v);
            }
            set_now(&mut d, c, Prop::Place, kw("absolute"));
            set_now(&mut d, c, Prop::Bg, color("#f38ba8"));
        }
        // And a 13 px square (no cell size divides it) in the corner.
        let sq = NodeId::new(400, 0);
        d.create(sq, NodeKind::Box, Some(n), 12);
        for (p, v) in at_xy(0.0, 0.0, 13.0, 13.0) {
            set_now(&mut d, sq, p, v);
        }
        set_now(&mut d, sq, Prop::Place, kw("absolute"));
        set_now(&mut d, sq, Prop::Bg, color("#a6e3a1"));
        assert!(r.apply(d).is_empty());
        buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
        (r, buf)
    };
    let (mut r, mut buf) = build(true);
    let (_, plain) = build(false);
    let mut t = 1016;
    while t < 1064 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    }
    assert_matches_ref("effects_pixelate", &buf, 2);
    // Every pixel is its cell's (cells of `c` from the box's corner).
    let (x0, y0) = (10u32, 8u32);
    let cells_of = |buf: &Buffer, c: u32| {
        (y0..y0 + 48).all(|y| {
            (x0..x0 + 48)
                .all(|x| buf.px(x, y) == buf.px(x0 + (x - x0) / c * c, y0 + (y - y0) / c * c))
        })
    };
    let c = (2..=12).rev().find(|&c| cells_of(&buf, c));
    assert!(c.is_some_and(|c| c >= 3), "a mosaic: cells of {c:?} px");
    assert!(!cells_of(&plain, 2));
    settle(&mut r, &mut buf, t);
    assert!(buf.pixels == plain.pixels, "refined to the plain drawing");
}

/// A `pages` swap under `transition: wipe(left)`: the new page is
/// revealed from the left over the old (created after it) or, created
/// before it, the old page is hidden by the rest of the wipe over the new
/// one; the old page plays out until the new one is in.
#[test]
fn a_pages_transition_wipes_one_page_over_the_other() {
    use std::time::Duration;
    for first in [false, true] {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let mut p = at_xy(0.0, 0.0, 120.0, 40.0);
        p.extend([
            (Prop::Place, kw("absolute")),
            (Prop::RowFirst, num(0.0)),
            (Prop::Transition, call("wipe", vec![kw("left")])),
        ]);
        let pages = b.node(NodeKind::Pages, Some(root), p);
        let old = b.node(
            NodeKind::Page,
            Some(pages),
            vec![(Prop::Bg, color("#f38ba8"))],
        );
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(120, 40, Scale::ONE);
        buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
        let new = NodeId::new(300, 0);
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: old,
            window: false,
        });
        d.create(new, NodeKind::Page, Some(pages), if first { 0 } else { 1 });
        set_now(&mut d, new, Prop::Bg, color("#a6e3a1"));
        d.set(pages, Prop::RowFirst, num(1.0));
        assert!(r.apply(d).is_empty());
        let mut t = 1000;
        while t < 1064 {
            t += 16;
            buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
        }
        let (red, green) = (hex("#f38ba8"), hex("#a6e3a1"));
        let is = |px: [u8; 4], c: Color| {
            let [b, g, r, _] = px;
            let c = c.to_rgba8();
            (r as i32 - c[0] as i32).abs() < 3
                && (g as i32 - c[1] as i32).abs() < 3
                && (b as i32 - c[2] as i32).abs() < 3
        };
        assert!(is(buf.px(2, 20), green), "first {first}: new at the left");
        assert!(is(buf.px(117, 20), red), "first {first}: old at the right");
        assert!(r.tree().get(old).is_some(), "the old page plays out");
        settle(&mut r, &mut buf, t);
        assert!(is(buf.px(117, 20), green) && is(buf.px(2, 20), green));
        assert!(r.tree().get(old).is_none(), "then goes");
    }
}

const S2: SurfaceId = SurfaceId(2);

/// A 200 × 60 bar holding a 20 px box `morph: "m"` at (10, 20) and an
/// empty 100 × 60 panel, painted at 1 s. Returns the box and both roots.
fn morphing(reduced: bool) -> (Renderer, Buffer, Buffer, [NodeId; 3]) {
    let mut b = Builder::default();
    let bar = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let panel = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Width, num(100.0)),
            (Prop::Height, num(60.0)),
        ],
    );
    let mut p = at_xy(10.0, 20.0, 20.0, 20.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Bg, color("#fab387")),
        (Prop::Morph, text("m")),
    ]);
    let pill = b.node(NodeKind::Box, Some(bar), p);
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, bar);
    r.attach_surface(S2, panel);
    let mut buf = Buffer::new(200, 60, Scale::ONE);
    let mut buf2 = Buffer::new(100, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    buf2.paint_at(&mut r, S2, 0, std::time::Duration::from_millis(1000));
    (r, buf, buf2, [pill, bar, panel])
}

/// Creates a `morph: "m"` box `id` under `parent` at `(x, y)`, `w × h`,
/// with `enter { opacity: 0 }`.
fn morph_box(d: &mut SceneDiff, id: NodeId, parent: NodeId, (x, y, w, h): (f32, f32, f32, f32)) {
    d.create(id, NodeKind::Box, Some(parent), 0);
    for (p, v) in at_xy(x, y, w, h) {
        set_now(d, id, p, v);
    }
    set_now(d, id, Prop::Place, kw("absolute"));
    set_now(d, id, Prop::Bg, color("#fab387"));
    // (Its own `~` is the morph's: the default here.)
    d.set(id, Prop::Morph, text("m"));
    set_now(
        d,
        id,
        Prop::Enter,
        PropValue::Pose(vec![(Prop::Opacity, num(0.0))]),
    );
}

/// design.md "Motion and time": `morph: "media"` on both nodes. A node
/// that enters with the name of one drawn on its surface starts from
/// that box and springs to its own, moved and scaled, in place of its
/// enter pose (ref `effects_morph_shared.png` on its way); another
/// surface's box is unknown while the surfaces' origins are, so there it
/// plays its enter pose; `reduced_motion` shows it in place.
#[test]
fn a_shared_morph_starts_from_the_named_box() {
    use std::time::Duration;
    let (mut r, mut buf, _, [pill, bar, _]) = morphing(false);
    let orange = buf.px(20, 30);
    let bg = buf.px(100, 5);
    let big = NodeId::new(400, 0);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: pill,
        window: false,
    });
    morph_box(&mut d, big, bar, (120.0, 5.0, 60.0, 50.0));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
    // Over the old box, at full opacity (no fade: the morph replaces it),
    // not yet at its own.
    assert_eq!(buf.px(20, 30), orange, "starts at the pill");
    assert_eq!(buf.px(175, 30), bg, "not yet at its box");
    let mut t = 1016;
    while t < 1064 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    }
    assert_matches_ref("effects_morph_shared", &buf, 2);
    settle(&mut r, &mut buf, t);
    assert_eq!(buf.px(150, 30), orange);
    assert_eq!(buf.px(122, 7), orange);
    assert_eq!(buf.px(20, 30), bg);

    // On another surface: its enter pose, in place.
    let (mut r, _, mut buf2, [_, _, panel]) = morphing(false);
    let other = NodeId::new(401, 0);
    let mut d = SceneDiff::new();
    morph_box(&mut d, other, panel, (40.0, 20.0, 20.0, 20.0));
    assert!(r.apply(d).is_empty());
    buf2.paint_at(&mut r, S2, 1, Duration::from_millis(1016));
    let p = buf2.px(50, 30);
    assert!(
        p != orange && p != buf2.px(5, 5),
        "fading in in place: {p:?}"
    );

    // Reduced motion: in place at once.
    let (mut r, mut buf, _, [pill, bar, _]) = morphing(true);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: pill,
        window: false,
    });
    morph_box(&mut d, big, bar, (120.0, 5.0, 60.0, 50.0));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
    assert_eq!(buf.px(150, 30), orange);
    assert_eq!(buf.px(20, 30), bg);
}

/// design.md: "Shared-element morph across surfaces, so the bar's media
/// pill becomes the media panel". With both surfaces placed on the same
/// output (`Renderer::set_surface_origin`), a node entering on the panel
/// starts over the bar's pill, moved by the difference of the origins
/// (ref `effects_morph_across.png` on its way), and springs to its own
/// box; on another output it plays its enter pose in place.
#[test]
fn a_shared_morph_crosses_surfaces_on_one_output() {
    use std::time::Duration;
    let at = |x: f32, y: f32| strand_scene::LogicalPoint::new(x, y);
    for same in [true, false] {
        let (mut r, buf, mut buf2, [_, _, panel]) = morphing(false);
        let orange = buf.px(20, 30);
        // The panel lies 30 px below the bar's top, on DP-1 or another.
        r.set_surface_origin(S, Some((Some("DP-1".into()), at(0.0, 0.0))));
        let other = if same { "DP-1" } else { "DP-2" };
        r.set_surface_origin(S2, Some((Some(other.into()), at(0.0, 30.0))));
        // The pill is drawn again (its box remembered with its place).
        buf2.paint_at(&mut r, S2, 1, Duration::from_millis(1000));
        let mut bar = Buffer::new(200, 60, Scale::ONE);
        bar.paint_at(&mut r, S, 1, Duration::from_millis(1008));
        let big = NodeId::new(401, 0);
        let mut d = SceneDiff::new();
        morph_box(&mut d, big, panel, (40.0, 20.0, 40.0, 30.0));
        assert!(r.apply(d).is_empty());
        buf2.paint_at(&mut r, S2, 1, Duration::from_millis(1016));
        if same {
            // The pill's box (10..30 × 20..40 on the bar) is 10..30 ×
            // -10..10 on the panel: its lower half shows, in full colour.
            assert_eq!(buf2.px(20, 5), orange, "starts at the pill");
            assert_ne!(buf2.px(70, 40), orange, "not yet at its box");
            let mut t = 1016;
            while t < 1064 {
                t += 16;
                buf2.paint_at(&mut r, S2, 1, Duration::from_millis(t));
            }
            assert_matches_ref("effects_morph_across", &buf2, 2);
        } else {
            let p = buf2.px(60, 35);
            assert!(
                p != orange && p != buf2.px(5, 5),
                "another output: fading in in place: {p:?}"
            );
            assert_ne!(buf2.px(20, 5), orange);
        }
        let mut t = 1100;
        while r.wants_frame(S2) {
            t += 16;
            buf2.paint_at(&mut r, S2, 1, Duration::from_millis(t));
            assert!(t < 5000, "settles");
        }
        assert_eq!(buf2.px(60, 35), orange, "at its own box");
    }
}

/// A shared morph that a preview (the flatten at the last frame's time
/// before a paint) sees start begins on the painted frame, from the
/// offsets of that frame: inside a parent whose `x` springs, the morph
/// (a slow linear 10 s one, so its first frame is all but its start)
/// still starts over the pill's box, not where the parent was a frame
/// before.
#[test]
fn a_preview_leaves_a_shared_morph_to_the_painted_frame() {
    use std::time::Duration;
    let (mut r, mut buf, _, [pill, bar, _]) = morphing(false);
    let orange = buf.px(20, 30);
    let bg = buf.px(100, 5);
    let holder = NodeId::new(300, 0);
    let mut d = SceneDiff::new();
    d.create(holder, NodeKind::Box, Some(bar), 1);
    for (p, v) in at_xy(0.0, 0.0, 200.0, 60.0) {
        set_now(&mut d, holder, p, v);
    }
    set_now(&mut d, holder, Prop::Place, kw("absolute"));
    assert!(r.apply(d).is_empty());
    let t = settle(&mut r, &mut buf, 1000);
    let big = NodeId::new(400, 0);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: pill,
        window: false,
    });
    d.set(holder, Prop::X, num(-40.0));
    morph_box(&mut d, big, holder, (120.0, 5.0, 60.0, 50.0));
    d.push(SceneOp::SetProp {
        id: big,
        prop: Prop::Morph,
        value: text("m"),
        transition: Transition::Duration {
            duration: Duration::from_secs(10),
            easing: Easing::Linear,
        },
    });
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(t + 16));
    // The pill's box was 10..30 × 20..40.
    for (x, y) in [(11, 21), (28, 38), (20, 30)] {
        assert_eq!(buf.px(x, y), orange, "over the pill at ({x}, {y})");
    }
    for (x, y) in [(8, 30), (32, 30), (20, 18), (20, 42)] {
        assert_eq!(buf.px(x, y), bg, "only the pill's box at ({x}, {y})");
    }
}

/// `merge 10` (200 × 60) on a bar holding three 20 px discs: two 6 px
/// apart at x 20 and 46, one far off at x 120 (`#89b4fa`, `#cba6f7`,
/// `#a6e3a1`).
fn gooey() -> (Renderer, Buffer, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(0.0, 0.0, 200.0, 60.0);
    p.extend([(Prop::Place, kw("absolute")), (Prop::Value, num(10.0))]);
    let merge = b.node(NodeKind::Merge, Some(root), p);
    let discs = [(20.0, "#89b4fa"), (46.0, "#cba6f7"), (120.0, "#a6e3a1")]
        .into_iter()
        .map(|(x, c)| {
            let mut p = at_xy(x, 20.0, 20.0, 20.0);
            p.extend([
                (Prop::Place, kw("absolute")),
                (Prop::Bg, color(c)),
                (Prop::Radius, kw("full")),
            ]);
            b.node(NodeKind::Box, Some(merge), p)
        })
        .collect();
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(200, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    (r, buf, discs)
}

/// design.md "Shape and geometry": `merge 10 { … }` melts children
/// together: two discs 6 px apart grow a bridge in their colours, a far
/// one stays alone, and moving it near (by spring) bridges it too (ref
/// `effects_goo.png`).
#[test]
fn goo_merges_near_children() {
    let (mut r, mut buf, discs) = gooey();
    assert_matches_ref("effects_goo", &buf, 2);
    let bg = buf.px(100, 5);
    // The gap between the near pair (x 40..46) is bridged at its middle.
    assert_ne!(buf.px(43, 30), bg, "bridged");
    // Off the bridge, above it, still the bar.
    assert_eq!(buf.px(43, 18), bg);
    // The far disc's gap is not.
    assert_eq!(buf.px(90, 30), bg, "far: no bridge");
    // Moving the far disc next to the pair bridges it.
    let mut d = SceneDiff::new();
    d.set(discs[2], Prop::X, num(72.0));
    assert!(r.apply(d).is_empty());
    settle(&mut r, &mut buf, 1000);
    assert_ne!(buf.px(69, 30), bg, "bridged once near");
}

/// A solid `w × h` PNG of `rgba` in a temporary directory.
fn solid_png(name: &str, rgba: [u8; 4]) -> String {
    let dir = std::env::temp_dir().join(format!("strand-effects-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let px: Vec<u8> = std::iter::repeat_n(rgba, 40 * 40).flatten().collect();
    write_png(&path, 40, 40, &px);
    path.to_string_lossy().into_owned()
}

/// design.md "Motion and time": transition masks on image swaps. An
/// `image` with `transition: wipe(left)` whose source changes shows the
/// new image coming in over the old from the left; with no `transition`
/// it swaps at once (ref `effects_image_swap.png` partway).
#[test]
fn an_image_swap_wipes_the_new_image_in() {
    use std::time::Duration;
    let red = solid_png("red.png", [0xf3, 0x8b, 0xa8, 0xff]);
    let blue = solid_png("blue.png", [0x89, 0xb4, 0xfa, 0xff]);
    for masked in [true, false] {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let mut p = at_xy(10.0, 10.0, 40.0, 40.0);
        p.extend([(Prop::Place, kw("absolute")), (Prop::Source, text(&red))]);
        if masked {
            p.push((Prop::Transition, call("wipe", vec![kw("left")])));
        }
        let img = b.node(NodeKind::Image, Some(root), p);
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(60, 60, Scale::ONE);
        buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
        let redpx = buf.px(30, 30);
        let mut d = SceneDiff::new();
        d.set(img, Prop::Source, text(&blue));
        assert!(r.apply(d).is_empty());
        let mut t = 1000;
        while t < 1064 {
            t += 16;
            buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
        }
        let (left, right) = (buf.px(12, 30), buf.px(47, 30));
        if masked {
            assert_matches_ref("effects_image_swap", &buf, 2);
            assert_ne!(left, redpx, "the new image at the left");
            assert_eq!(right, redpx, "the old one at the right");
            settle(&mut r, &mut buf, t);
            assert_eq!(buf.px(47, 30), left, "all new");
        } else {
            assert!(left != redpx && left == right, "at once");
        }
    }
}

/// An image hidden mid-wipe (its parent's opacity set to 0 at once)
/// ends its swap: it wants no frames while unseen, and shows the new
/// image at rest when it comes back.
#[test]
fn an_image_hidden_mid_swap_wants_no_frames() {
    use std::time::Duration;
    let red = solid_png("red-hidden.png", [0xf3, 0x8b, 0xa8, 0xff]);
    let blue = solid_png("blue-hidden.png", [0x89, 0xb4, 0xfa, 0xff]);
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(10.0, 10.0, 40.0, 40.0);
    p.push((Prop::Place, kw("absolute")));
    let holder = b.node(NodeKind::Box, Some(root), p);
    let img = b.node(
        NodeKind::Image,
        Some(holder),
        vec![
            (Prop::Size, num(40.0)),
            (Prop::Source, text(&red)),
            (Prop::Transition, call("wipe", vec![kw("left")])),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(60, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
    let redpx = buf.px(30, 30);
    let mut d = SceneDiff::new();
    d.set(img, Prop::Source, text(&blue));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1016));
    buf.paint_at(&mut r, S, 1, Duration::from_millis(1032));
    assert!(r.wants_frame(S), "wiping");
    assert_eq!(buf.px(47, 30), redpx, "the old image still at the right");
    let hide = |v: f32| {
        let mut d = SceneDiff::new();
        d.push(SceneOp::SetProp {
            id: holder,
            prop: Prop::Opacity,
            value: num(v),
            transition: Transition::Instant,
        });
        d
    };
    assert!(r.apply(hide(0.0)).is_empty());
    let mut t = 1032;
    while r.wants_frame(S) && t < 1500 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    }
    assert!(t < 1100, "hidden: no frames, not until {t} ms");
    assert!(!r.wants_frame(S) && r.next_wake().is_none());
    assert!(r.apply(hide(1.0)).is_empty());
    t += 16;
    buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    let left = buf.px(12, 30);
    assert_ne!(left, redpx, "the new image");
    assert_eq!(buf.px(47, 30), left, "all new, at rest");
}

/// An image swap under `transition:` to a source that fails to decode (a
/// missing file, an external input) ends at once: the image draws what
/// it would without `transition:` (nothing) and wants no more frames.
#[test]
fn an_image_swap_to_a_broken_source_settles() {
    use std::time::Duration;
    let red = solid_png("red-broken.png", [0xf3, 0x8b, 0xa8, 0xff]);
    let missing = format!("{red}.missing.png");
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(10.0, 10.0, 40.0, 40.0);
    p.extend([
        (Prop::Place, kw("absolute")),
        (Prop::Source, text(&red)),
        (Prop::Transition, call("wipe", vec![kw("left")])),
    ]);
    let img = b.node(NodeKind::Image, Some(root), p);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(60, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_millis(1000));
    let bg = buf.px(5, 5);
    assert_ne!(buf.px(30, 30), bg, "the red image");
    let mut d = SceneDiff::new();
    d.set(img, Prop::Source, text(&missing));
    assert!(r.apply(d).is_empty());
    let mut t = 1000;
    while r.wants_frame(S) && t < 1500 {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
    }
    assert!(t < 1100, "settled at once, not after {t} ms");
    assert!(!r.wants_frame(S) && r.next_wake().is_none());
    assert_eq!(buf.px(30, 30), bg, "nothing drawn, as without transition");
}

/// A 160 × 60 bar with a 20 px `drag:`-style box at (20, 20) with
/// `jelly: amount` (none at 0), painted at 1 s.
fn jellied(amount: f32, reduced: bool) -> (Renderer, Buffer, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut p = at_xy(20.0, 20.0, 20.0, 20.0);
    p.extend([(Prop::Place, kw("absolute")), (Prop::Bg, color("#f9e2af"))]);
    if amount > 0.0 {
        p.push((Prop::Jelly, num(amount)));
    }
    let id = b.node(NodeKind::Box, Some(root), p);
    let mut r = renderer();
    r.set_reduced_motion(reduced);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(160, 60, Scale::ONE);
    buf.paint_at(&mut r, S, 0, std::time::Duration::from_millis(1000));
    (r, buf, id)
}

/// The drawn box of the yellow box: `(left, right, top, bottom)` of the
/// pixels it covers more than half.
fn yellow_extent(buf: &Buffer) -> (u32, u32, u32, u32) {
    let yellow = |x: u32, y: u32| buf.px(x, y)[1] > 140;
    let xs: Vec<u32> = (0..buf.size.w)
        .filter(|x| (0..buf.size.h).any(|y| yellow(*x, y)))
        .collect();
    let ys: Vec<u32> = (0..buf.size.h)
        .filter(|y| (0..buf.size.w).any(|x| yellow(x, *y)))
        .collect();
    (xs[0], xs[xs.len() - 1], ys[0], ys[ys.len() - 1])
}

/// design.md "Motion and time": `jelly: 0.4` squashes and stretches a
/// dragged node. Dragged right fast it is longer along x and shorter
/// along y (area kept), drawn as a transform (ref `effects_jelly.png`
/// mid-drag); stopped, it rings out through a squash and settles back to
/// its 20 px square with no frames wanted. Without `jelly`, or under
/// `reduced_motion`, the dragged box keeps its shape.
#[test]
fn jelly_stretches_a_dragged_node_and_wobbles_out() {
    use std::time::Duration;
    let drag = |r: &mut Renderer, buf: &mut Buffer, id: NodeId| {
        let mut t = 1000;
        for k in 1..=5 {
            t += 16;
            r.lift(id, Some(LogicalPoint::new(20.0 * k as f32, 0.0)));
            buf.paint_at(r, S, 1, Duration::from_millis(t));
        }
        t
    };
    let (mut r, mut buf, id) = jellied(0.4, false);
    let mut t = drag(&mut r, &mut buf, id);
    // At 1,250 px/s (smoothed, and the spring on its way): stretched
    // along x, squashed along y.
    let (l, rt, top, bottom) = yellow_extent(&buf);
    let (w, h) = (rt - l + 1, bottom - top + 1);
    assert!(w >= 22 && h <= 18, "stretched: {w} × {h}");
    let centre = (l + rt) / 2;
    assert!((128..=132).contains(&centre), "about its centre: {centre}");
    assert_matches_ref("effects_jelly", &buf, 2);
    // Held still: the stretch rings out through a squash (taller than
    // wide) and settles to its square.
    let mut squashed = false;
    while r.wants_frame(S) {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
        let (l, rt, top, bottom) = yellow_extent(&buf);
        squashed |= bottom - top > rt - l + 1;
        assert!(t < 6000, "settles");
    }
    assert!(squashed, "overshoots into a squash");
    assert_eq!(yellow_extent(&buf), (120, 139, 20, 39), "square at rest");
    // Let go: it glides home, stretching on the way, and settles.
    r.lift(id, None);
    let mut stretched = false;
    while r.wants_frame(S) {
        t += 16;
        buf.paint_at(&mut r, S, 1, Duration::from_millis(t));
        let (l, rt, top, bottom) = yellow_extent(&buf);
        stretched |= rt - l > bottom - top + 1;
        assert!(t < 9000, "settles");
    }
    assert!(stretched, "stretches as it springs back");
    assert_eq!(yellow_extent(&buf), (20, 39, 20, 39), "home and square");

    for (amount, reduced) in [(0.0, false), (0.4, true)] {
        let (mut r, mut buf, id) = jellied(amount, reduced);
        drag(&mut r, &mut buf, id);
        assert_eq!(
            yellow_extent(&buf),
            (120, 139, 20, 39),
            "no jelly (amount {amount}, reduced {reduced})"
        );
    }
}

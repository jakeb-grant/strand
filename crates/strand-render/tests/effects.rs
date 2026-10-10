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

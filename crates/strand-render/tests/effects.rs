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

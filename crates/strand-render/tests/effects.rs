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

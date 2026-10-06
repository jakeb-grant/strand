//! Offline PNG tests of shapes and paint (design.md, "Shape and
//! geometry", "Paint and light"; features.md M2 "Shapes and paint"):
//! per-corner radii, `radius: full`, `corners: squircle`, borders,
//! `$elevation` shadow lists (cached), linear, radial and conic
//! gradients with dithering, `clip: true`, `opacity`, `scale`, and the
//! `blur` tint fallback.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test paint`.

mod common;

use common::*;
use strand_scene::*;

const TOLERANCE: u8 = 3;

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn stops(cs: &[&str]) -> Vec<GradientStop> {
    let n = (cs.len() - 1).max(1) as f32;
    cs.iter()
        .enumerate()
        .map(|(i, c)| GradientStop {
            offset: i as f32 / n,
            color: hex(c),
        })
        .collect()
}

/// An absolutely placed box under `parent`.
fn at(
    b: &mut Builder,
    parent: NodeId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    mut props: Vec<(Prop, PropValue)>,
) -> NodeId {
    props.extend([
        (Prop::Place, kw("absolute")),
        (Prop::X, num(x)),
        (Prop::Y, num(y)),
        (Prop::Width, num(w)),
        (Prop::Height, num(h)),
    ]);
    b.node(NodeKind::Box, Some(parent), props)
}

fn panel(b: &mut Builder, w: f32, h: f32) -> NodeId {
    b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
        ],
    )
}

fn render_with(r: &mut strand_render::Renderer, diff: SceneDiff, scale: Scale) -> Buffer {
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    let spec = r.surface_spec(root).unwrap().clone();
    let size = scale.physical_size(LogicalSize::new(
        spec.width.unwrap() + spec.overhang.left + spec.overhang.right,
        spec.height.unwrap() + spec.overhang.top + spec.overhang.bottom,
    ));
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), size, scale);
    let mut buf = Buffer::new(size.w, size.h, scale);
    buf.paint(r, SurfaceId(1), 0);
    buf
}

fn render(diff: SceneDiff, scale: Scale) -> Buffer {
    render_with(&mut renderer(), diff, scale)
}

fn corners_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = panel(&mut b, 260.0, 80.0);
    let fill = || (Prop::Bg, color("#89b4fa"));
    // Per-corner radii, clockwise from top-left.
    at(
        &mut b,
        root,
        10.0,
        10.0,
        60.0,
        60.0,
        vec![
            fill(),
            (
                Prop::Radius,
                PropValue::List(vec![num(24.0), num(4.0), num(0.0), num(12.0)]),
            ),
        ],
    );
    // `radius: full`: a pill and a circle.
    at(
        &mut b,
        root,
        80.0,
        25.0,
        60.0,
        30.0,
        vec![fill(), (Prop::Radius, kw("full"))],
    );
    // Round and squircle corners of the same radius, outlined.
    for (i, squircle) in [false, true].into_iter().enumerate() {
        let mut props = vec![
            (Prop::Bg, color("#f5c2e7")),
            (Prop::Radius, num(20.0)),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 2.0,
                    paint: Paint::Solid(hex("#11111b")),
                }),
            ),
        ];
        if squircle {
            props.push((Prop::Corners, kw("squircle")));
        }
        at(
            &mut b,
            root,
            150.0 + 55.0 * i as f32,
            10.0,
            50.0,
            60.0,
            props,
        );
    }
    b.diff
}

#[test]
fn corners_per_corner_full_and_squircle() {
    let buf = render(corners_scene(), Scale::ONE);
    assert_matches_ref("paint_corners", &buf, TOLERANCE);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    // Per-corner: top-left round (24), top-right nearly square,
    // bottom-right square.
    assert_eq!(buf.px(12, 12), bg, "top-left corner is cut");
    assert_ne!(buf.px(68, 11), bg, "top-right radius 4 is nearly square");
    assert_eq!(buf.px(69, 69), [0xfa, 0xb4, 0x89, 0xff], "square corner");
    // The pill's ends are half circles: its corner pixel is background,
    // its end's middle is filled.
    assert_eq!(buf.px(81, 26), bg);
    assert_eq!(buf.px(81, 40), [0xfa, 0xb4, 0x89, 0xff]);
    // A squircle corner starts further along the edge than a circle of
    // the same radius, and bulges out more at 45°.
    // 22 px down the left edge the round corner (r 20) has met the
    // straight edge, the squircle (reaching 25) is still curving in.
    let (round_edge, squircle_edge) = (buf.px(150, 32), buf.px(205, 32));
    assert!(
        squircle_edge[3] == 0xff && squircle_edge != round_edge,
        "the squircle eases in further along: {squircle_edge:?} {round_edge:?}"
    );
    let diag = |x0: u32| buf.px(x0 + 4, 14);
    assert_eq!(diag(150), bg, "45° inside the round corner's cut");
    assert_ne!(diag(205), bg, "the squircle's 45° is fuller");
    let buf = render(corners_scene(), Scale::new(180).unwrap());
    assert_matches_ref("paint_corners_1_5x", &buf, TOLERANCE);
}

fn borders_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = panel(&mut b, 220.0, 70.0);
    at(
        &mut b,
        root,
        10.0,
        10.0,
        60.0,
        50.0,
        vec![
            (Prop::Radius, num(10.0)),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 1.0,
                    paint: Paint::Solid(hex("#a6adc8")),
                }),
            ),
        ],
    );
    at(
        &mut b,
        root,
        80.0,
        10.0,
        60.0,
        50.0,
        vec![
            (Prop::Bg, color("#313244")),
            (Prop::Radius, num(14.0)),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 3.0,
                    paint: Paint::Conic {
                        from: 0.0,
                        stops: stops(&["#89b4fa", "#f5c2e7", "#89b4fa"]),
                    },
                }),
            ),
        ],
    );
    at(
        &mut b,
        root,
        150.0,
        20.0,
        60.0,
        30.0,
        vec![
            (Prop::Bg, color("#45475a")),
            (Prop::Radius, kw("full")),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 1.5,
                    paint: Paint::Solid(hex("#f38ba8")),
                }),
            ),
        ],
    );
    b.diff
}

#[test]
fn borders_solid_gradient_and_on_a_pill() {
    let buf = render(borders_scene(), Scale::ONE);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    assert_eq!(buf.px(40, 35), bg, "a border-only box is hollow");
    assert_eq!(buf.px(40, 10)[1], 0xad, "drawn inside the box's top edge");
    // The gradient ring: blue at the top, pink at the bottom (the conic
    // halfway round).
    let top = buf.px(110, 11);
    let bottom = buf.px(110, 58);
    assert!(top[0] > top[2], "blue on top: {top:?}");
    assert!(bottom[2] > bottom[0], "pink below: {bottom:?}");
    assert_matches_ref("paint_borders", &buf, TOLERANCE);
}

fn elevation_tokens() -> TokenTable {
    let mut t = TokenTable::default();
    let shadow = |x, y, blur, a: f32| Shadow {
        x,
        y,
        blur,
        spread: 0.0,
        color: Color::BLACK.with_alpha(a),
    };
    t.insert(
        "elevation.md",
        PropValue::Shadow(vec![shadow(0.0, 2.0, 8.0, 0.25)]),
    );
    t.insert(
        "elevation.lg",
        PropValue::Shadow(vec![
            shadow(0.0, 8.0, 24.0, 0.3),
            shadow(0.0, 1.0, 2.0, 0.2),
        ]),
    );
    t.insert(
        "elevation.xl",
        PropValue::Shadow(vec![shadow(0.0, 16.0, 48.0, 0.4)]),
    );
    t
}

fn shadows_scene() -> SceneDiff {
    let mut b = Builder::default();
    b.diff.set_tokens(elevation_tokens(), Transition::Instant);
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#eff1f5")),
            (Prop::Width, num(330.0)),
            (Prop::Height, num(130.0)),
        ],
    );
    for (i, level) in ["md", "lg", "xl"].into_iter().enumerate() {
        at(
            &mut b,
            root,
            20.0 + 105.0 * i as f32,
            25.0,
            80.0,
            60.0,
            vec![
                (Prop::Bg, color("#ffffff")),
                (Prop::Radius, num(14.0)),
                (
                    Prop::Shadow,
                    PropValue::Token(TokenExpr::path(format!("elevation.{level}"))),
                ),
            ],
        );
    }
    b.diff
}

#[test]
fn elevation_shadow_lists_are_drawn_and_cached() {
    let mut r = renderer();
    let buf = render_with(&mut r, shadows_scene(), Scale::ONE);
    // Deeper elevations reach further below their box.
    let below = |x: u32| buf.px(x, 85 + 12)[1];
    assert!(below(60) > below(165) && below(165) > below(270));
    // Every shadow of the three lists is a cached pixmap; repainting the
    // same frame builds none, and a repaint of part of it matches.
    let (bytes, builds) = r.paint_cache();
    assert_eq!(builds, 4, "md, lg (two shadows) and xl");
    assert!(bytes > 0 && bytes <= strand_render::PAINT_CACHE_BYTES);
    r.invalidate(SurfaceId(1));
    let mut again = Buffer::new(buf.size.w, buf.size.h, Scale::ONE);
    again.paint(&mut r, SurfaceId(1), 0);
    assert_eq!(r.paint_cache().1, 4);
    assert_eq!(again.pixels, buf.pixels);
    assert_matches_ref("paint_shadows", &buf, TOLERANCE);
}

/// A shadowed node moved by whole pixels (a sliding toast, a FLIP glide)
/// reuses its cached shadow: the key is relative to the pixmap's
/// whole-pixel origin.
#[test]
fn moved_shadows_reuse_their_pixmap() {
    let mut b = Builder::default();
    b.diff.set_tokens(elevation_tokens(), Transition::Instant);
    let root = panel(&mut b, 200.0, 120.0);
    let card = at(
        &mut b,
        root,
        20.0,
        30.0,
        80.0,
        50.0,
        vec![
            (Prop::Bg, color("#ffffff")),
            (Prop::Radius, num(10.0)),
            (
                Prop::Shadow,
                PropValue::Token(TokenExpr::path("elevation.md")),
            ),
        ],
    );
    let mut r = renderer();
    let mut buf = render_with(&mut r, b.diff, Scale::ONE);
    assert_eq!(r.paint_cache().1, 1);
    for i in 1..=10 {
        let mut d = SceneDiff::new();
        d.push(SceneOp::SetProp {
            id: card,
            prop: Prop::X,
            value: num(20.0 + 3.0 * i as f32),
            transition: Transition::Instant,
        });
        assert!(r.apply(d).is_empty());
        buf.paint(&mut r, SurfaceId(1), 1);
    }
    assert_eq!(r.paint_cache().1, 1, "one shadow pixmap for every position");
    // And the moved shadow is the same pixels, 30 px further right.
    let fresh = {
        let mut b = Builder::default();
        b.diff.set_tokens(elevation_tokens(), Transition::Instant);
        let root = panel(&mut b, 200.0, 120.0);
        at(
            &mut b,
            root,
            50.0,
            30.0,
            80.0,
            50.0,
            vec![
                (Prop::Bg, color("#ffffff")),
                (Prop::Radius, num(10.0)),
                (
                    Prop::Shadow,
                    PropValue::Token(TokenExpr::path("elevation.md")),
                ),
            ],
        );
        render(b.diff, Scale::ONE)
    };
    assert_eq!(buf.pixels, fresh.pixels);
}

fn gradients_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = panel(&mut b, 330.0, 100.0);
    let paints = [
        Paint::Linear {
            angle: 45.0,
            stops: stops(&["#89b4fa", "#cba6f7"]),
        },
        Paint::Radial {
            stops: stops(&["#f9e2af", "#fab387", "#1e1e2e"]),
        },
        Paint::Conic {
            from: 90.0,
            stops: stops(&["#a6e3a1", "#89dceb", "#a6e3a1"]),
        },
    ];
    for (i, p) in paints.into_iter().enumerate() {
        at(
            &mut b,
            root,
            10.0 + 105.0 * i as f32,
            10.0,
            100.0,
            80.0,
            vec![(Prop::Bg, PropValue::Paint(p)), (Prop::Radius, num(12.0))],
        );
    }
    b.diff
}

#[test]
fn gradients_linear_radial_conic() {
    let buf = render(gradients_scene(), Scale::ONE);
    // Linear at 45°: bottom-left is the first stop, top-right the last.
    let (bl, tr) = (buf.px(16, 84), buf.px(104, 16));
    assert!(bl[0] > tr[0] - 10 && tr[2] > bl[2], "{bl:?} {tr:?}");
    // Radial: light in the middle, dark in the corners.
    assert!(buf.px(165, 50)[1] > buf.px(118, 86)[1]);
    assert_matches_ref("paint_gradients", &buf, TOLERANCE);
}

/// A slow gradient over a wide box does not band: no run of one value
/// along a row is as long as the band a plain quantised ramp would draw
/// (here 512 px over 8 levels: 64 px).
#[test]
fn wide_slow_gradients_are_dithered() {
    let mut b = Builder::default();
    let root = panel(&mut b, 512.0, 16.0);
    at(
        &mut b,
        root,
        0.0,
        0.0,
        512.0,
        16.0,
        vec![(
            Prop::Bg,
            PropValue::Paint(Paint::Linear {
                angle: 90.0,
                stops: stops(&["#202428", "#282c30"]),
            }),
        )],
    );
    let buf = render(b.diff, Scale::ONE);
    let row: Vec<u8> = (0..512).map(|x| buf.px(x, 7)[1]).collect();
    let longest = row
        .chunk_by(|a, b| a == b)
        .map(|run| run.len())
        .max()
        .unwrap();
    assert!(longest <= 32, "longest band {longest} px");
    assert!(
        row[0] <= 0x25 && row[511] >= 0x2b,
        "{} {}",
        row[0],
        row[511]
    );
}

/// A gradient too large to cache (over `MAX_ENTRY_BYTES`: a launcher at
/// 2×, a full-height panel) is dithered too, cell by cell, and a partial
/// repaint draws the same pixels as a full one.
#[test]
fn large_gradients_are_dithered_too() {
    let (w, h) = (1024u32, 600u32);
    assert!((w * h * 4) as usize > strand_render::MAX_ENTRY_BYTES);
    let scene = |dot: &str| {
        let mut b = Builder::default();
        let root = panel(&mut b, w as f32, h as f32);
        at(
            &mut b,
            root,
            0.0,
            0.0,
            w as f32,
            h as f32,
            vec![(
                Prop::Bg,
                PropValue::Paint(Paint::Linear {
                    angle: 90.0,
                    stops: stops(&["#202428", "#282c30"]),
                }),
            )],
        );
        let dot = at(
            &mut b,
            root,
            500.0,
            300.0,
            20.0,
            20.0,
            vec![(Prop::Bg, color(dot)), (Prop::Radius, num(10.0))],
        );
        (b.diff, dot)
    };
    let (diff, dot) = scene("#ff0000");
    let mut r = renderer();
    let mut buf = render_with(&mut r, diff, Scale::ONE);
    assert_eq!(r.paint_cache().1, 0, "too large to cache");
    for y in [7, 300, 599] {
        let row: Vec<u8> = (0..w).map(|x| buf.px(x, y)[1]).collect();
        let longest = row
            .chunk_by(|a, b| a == b)
            .map(|run| run.len())
            .max()
            .unwrap();
        // 1024 px over 8 levels would band at 128 px.
        assert!(longest <= 64, "row {y}: longest band {longest} px");
        assert!(row[0] <= 0x25 && row[1023] >= 0x2b);
    }
    // A repaint of part of it (the dot changes colour) matches a full
    // paint of the same scene.
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: dot,
        prop: Prop::Bg,
        value: color("#0000ff"),
        transition: Transition::Instant,
    });
    assert!(r.apply(d).is_empty());
    let damage = buf.paint(&mut r, SurfaceId(1), 1);
    assert!(
        damage.rects().iter().all(|d| d.w < 100 && d.h < 100),
        "{damage:?}"
    );
    let fresh = render(scene("#0000ff").0, Scale::ONE);
    assert_eq!(buf.pixels, fresh.pixels);
}

fn effects_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = panel(&mut b, 330.0, 100.0);
    // `clip: true`: an overflowing child cut to the rounded box.
    let clip = at(
        &mut b,
        root,
        10.0,
        10.0,
        80.0,
        80.0,
        vec![
            (Prop::Bg, color("#313244")),
            (Prop::Radius, num(24.0)),
            (Prop::Clip, PropValue::Bool(true)),
        ],
    );
    at(
        &mut b,
        clip,
        40.0,
        -10.0,
        60.0,
        60.0,
        vec![(Prop::Bg, color("#f38ba8"))],
    );
    // `opacity: 0.5` over an overlapping pair: the group fades as one.
    let group = at(
        &mut b,
        root,
        110.0,
        10.0,
        90.0,
        80.0,
        vec![(Prop::Opacity, num(0.5))],
    );
    at(
        &mut b,
        group,
        0.0,
        0.0,
        60.0,
        60.0,
        vec![(Prop::Bg, color("#89b4fa"))],
    );
    at(
        &mut b,
        group,
        30.0,
        20.0,
        60.0,
        60.0,
        vec![(Prop::Bg, color("#a6e3a1"))],
    );
    // `scale: 0.75` about the box's centre, with its child.
    let scaled = at(
        &mut b,
        root,
        220.0,
        10.0,
        100.0,
        80.0,
        vec![
            (Prop::Bg, color("#fab387")),
            (Prop::Radius, num(10.0)),
            (Prop::Scale, num(0.75)),
        ],
    );
    at(
        &mut b,
        scaled,
        10.0,
        10.0,
        30.0,
        30.0,
        vec![(Prop::Bg, color("#11111b"))],
    );
    b.diff
}

#[test]
fn clip_opacity_and_scale() {
    let buf = render(effects_scene(), Scale::ONE);
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    assert_eq!(buf.px(60, 30), [0xa8, 0x8b, 0xf3, 0xff], "child inside");
    assert_eq!(buf.px(88, 12), bg, "child clipped to the rounded corner");
    // Where the two halves of the faded group overlap, the top one shows
    // at half strength over the panel (one layer, not two blended).
    let overlap = buf.px(150, 45);
    let green_alone = buf.px(195, 85);
    assert_eq!(overlap, green_alone);
    // Scaled about its centre: 100 × 80 drawn as 75 × 60.
    assert_eq!(buf.px(225, 15), bg, "outside the scaled box");
    assert_eq!(buf.px(235, 25)[3], 0xff);
    assert_ne!(buf.px(235, 25), bg, "inside the scaled box");
    assert_matches_ref("paint_effects", &buf, TOLERANCE);
}

fn blur_scene(fallback: Option<&str>) -> SceneDiff {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, PropValue::Color(hex("#1e1e2e").with_alpha(0.72))),
            (Prop::Radius, num(14.0)),
            (Prop::Blur, num(24.0)),
        ],
    );
    if let Some(f) = fallback {
        b.diff.set(root, Prop::BlurFallback, kw(f));
    }
    b.diff
}

/// `blur: 24` with no compositor blur draws the tint fallback (alpha
/// up by 0.15), reports its rounded box for the blur ladder, and
/// `blur_fallback: none` or a compositor that blurs leaves the alpha
/// as written.
#[test]
fn blur_tints_until_the_compositor_blurs() {
    use strand_scene::Painter;
    let alpha = |buf: &Buffer| buf.px(50, 20)[3];
    let mut r = renderer();
    let buf = render_with(&mut r, blur_scene(None), Scale::ONE);
    let want = ((0.72 + strand_render::BLUR_TINT) * 255.0_f32).round() as u8;
    assert!(alpha(&buf).abs_diff(want) <= 1, "{}", alpha(&buf));
    let region = r.blur_region(SurfaceId(1));
    assert_eq!(region.len(), 1);
    assert_eq!(region[0].rect, Rect::new(0, 0, 100, 40));
    assert_eq!(region[0].radii, [14.0; 4]);
    assert_eq!(region[0].radius, 24.0);
    let plain = (0.72_f32 * 255.0).round() as u8;
    let buf = render(blur_scene(Some("none")), Scale::ONE);
    assert!(alpha(&buf).abs_diff(plain) <= 1);
    let mut r = renderer();
    r.set_compositor_blur(true);
    let buf = render_with(&mut r, blur_scene(None), Scale::ONE);
    assert!(alpha(&buf).abs_diff(plain) <= 1);
    assert_matches_ref(
        "paint_blur_tint",
        &render(blur_scene(None), Scale::ONE),
        TOLERANCE,
    );
}

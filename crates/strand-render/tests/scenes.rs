//! Offline render tests: scenes painted by vello_cpu, compared with the
//! committed PNGs in `tests/refs` within a per-channel tolerance.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test scenes`.

mod common;

use common::*;
use strand_scene::*;

/// Per-channel tolerance for reference comparisons (SIMD levels differ in
/// rounding on other CPUs).
const TOLERANCE: u8 = 3;

fn stops(a: &str, b: &str) -> Vec<GradientStop> {
    vec![
        GradientStop {
            offset: 0.0,
            color: hex(a),
        },
        GradientStop {
            offset: 1.0,
            color: hex(b),
        },
    ]
}

fn shapes_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    // Rounded card with a soft shadow.
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(16.0)),
            (Prop::Y, num(16.0)),
            (Prop::Width, num(64.0)),
            (Prop::Height, num(40.0)),
            (Prop::Radius, num(10.0)),
            (Prop::Bg, color("#89b4fa")),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 4.0,
                    blur: 10.0,
                    spread: 0.0,
                    color: hex("#000000aa"),
                }]),
            ),
        ],
    );
    // Per-corner radii with a linear gradient.
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(96.0)),
            (Prop::Y, num(16.0)),
            (Prop::Width, num(64.0)),
            (Prop::Height, num(40.0)),
            (
                Prop::Radius,
                PropValue::Corners(Corners {
                    top_left: 14.0,
                    top_right: 14.0,
                    bottom_right: 0.0,
                    bottom_left: 0.0,
                }),
            ),
            (
                Prop::Bg,
                PropValue::Paint(Paint::Linear {
                    angle: 90.0,
                    stops: stops("#f38ba8", "#fab387"),
                }),
            ),
        ],
    );
    // Border only.
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(176.0)),
            (Prop::Y, num(16.0)),
            (Prop::Width, num(48.0)),
            (Prop::Height, num(40.0)),
            (Prop::Radius, num(8.0)),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 2.0,
                    paint: Paint::Solid(hex("#a6e3a1")),
                }),
            ),
        ],
    );
    // Pill (radius: full) at half opacity with a child.
    let pill = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(16.0)),
            (Prop::Y, num(72.0)),
            (Prop::Width, num(80.0)),
            (Prop::Height, num(24.0)),
            (Prop::Radius, num(999.0)),
            (Prop::Bg, color("#cba6f7")),
            (Prop::Opacity, num(0.5)),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(pill),
        vec![
            (Prop::X, num(4.0)),
            (Prop::Y, num(4.0)),
            (Prop::Size, num(16.0)),
            (Prop::Radius, num(999.0)),
            (Prop::Bg, color("#ffffff")),
        ],
    );
    // Clipping container: the child overflows and is cut to the rounded box.
    let clip = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(112.0)),
            (Prop::Y, num(72.0)),
            (Prop::Width, num(48.0)),
            (Prop::Height, num(48.0)),
            (Prop::Radius, num(12.0)),
            (Prop::Clip, PropValue::Bool(true)),
            (Prop::Bg, color("#313244")),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(clip),
        vec![
            (Prop::X, num(24.0)),
            (Prop::Y, num(24.0)),
            (Prop::Size, num(48.0)),
            (Prop::Bg, color("#f9e2af")),
        ],
    );
    // Radial and conic fills.
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(176.0)),
            (Prop::Y, num(72.0)),
            (Prop::Size, num(48.0)),
            (Prop::Radius, num(24.0)),
            (
                Prop::Bg,
                PropValue::Paint(Paint::Conic {
                    from: 0.0,
                    stops: stops("#89dceb", "#1e66f5"),
                }),
            ),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(16.0)),
            (Prop::Y, num(104.0)),
            (Prop::Width, num(80.0)),
            (Prop::Height, num(28.0)),
            (Prop::Radius, num(6.0)),
            (
                Prop::Bg,
                PropValue::Paint(Paint::Radial {
                    stops: stops("#ffffff", "#45475a"),
                }),
            ),
        ],
    );
    // Text with inherited colour and font.
    let label = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(label),
        vec![
            (Prop::X, num(176.0)),
            (Prop::Y, num(122.0)),
            (Prop::Text, text("Strand")),
        ],
    );
    b.diff
}

fn bar_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, PropValue::Color(hex("#1e1e2e").with_alpha(0.9))),
            (Prop::Radius, num(10.0)),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 1.0,
                    paint: Paint::Solid(hex("#45475a")),
                }),
            ),
        ],
    );
    for i in 0..4 {
        let focused = i == 1;
        b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (
                    Prop::X,
                    num(12.0 + 14.0 * i as f32 + if i > 1 { 16.0 } else { 0.0 }),
                ),
                (Prop::Y, num(14.0)),
                (Prop::Width, num(if focused { 24.0 } else { 8.0 })),
                (Prop::Height, num(8.0)),
                (Prop::Radius, num(999.0)),
                (
                    Prop::Bg,
                    if focused {
                        color("#89b4fa")
                    } else {
                        PropValue::Color(hex("#cdd6f4").with_alpha(0.25))
                    },
                ),
            ],
        );
    }
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(150.0)),
            (Prop::Y, num(10.0)),
            (Prop::Width, num(100.0)),
            (Prop::Align, PropValue::Keyword("center".into())),
            (Prop::Text, text("Sat 04  12:59")),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(330.0)),
            (Prop::Y, num(10.0)),
            (Prop::Text, text("87%")),
            (Prop::Color, color("#a6e3a1")),
        ],
    );
    b.diff
}

fn render(diff: SceneDiff, w: u32, h: u32, scale: Scale) -> Buffer {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(SurfaceId(1), root);
    let mut buf = Buffer::new(w, h, scale);
    let d = buf.paint(&mut r, SurfaceId(1), 0);
    assert_eq!(
        d.rects(),
        &[Rect::new(0, 0, w, h)],
        "first frame is full damage"
    );
    buf
}

#[test]
fn shapes_match_reference() {
    let buf = render(shapes_scene(), 240, 140, Scale::ONE);
    // Spot checks independent of the reference image.
    assert_eq!(
        buf.px(2, 2),
        [0x2e, 0x1e, 0x1e, 0xff],
        "panel background in BGRA"
    );
    assert_eq!(buf.px(48, 36), [0xfa, 0xb4, 0x89, 0xff], "card fill");
    assert_eq!(
        buf.px(200, 36),
        [0x2e, 0x1e, 0x1e, 0xff],
        "border box is hollow"
    );
    assert_eq!(buf.px(177, 36)[1], 0xe3, "border is drawn inside the box");
    assert_eq!(
        buf.px(155, 115),
        [0xaf, 0xe2, 0xf9, 0xff],
        "clipped child inside"
    );
    assert_eq!(
        buf.px(159, 119),
        [0x2e, 0x1e, 0x1e, 0xff],
        "child clipped to the rounded corner"
    );
    let below = buf.px(48, 59);
    assert!(
        below[0] < 0x2e && below[3] == 0xff,
        "shadow darkens below the card: {below:?}"
    );
    let (l, r) = (buf.px(98, 36), buf.px(157, 36));
    assert!(
        l[2] < r[2] || l[1] < r[1],
        "gradient runs left to right: {l:?} {r:?}"
    );
    assert_matches_ref("shapes", &buf, TOLERANCE);
}

#[test]
fn bar_matches_reference_at_1x() {
    let buf = render(bar_scene(), 400, 36, Scale::ONE);
    assert_matches_ref("bar_1x", &buf, TOLERANCE);
}

#[test]
fn bar_matches_reference_at_fractional_scales() {
    for (n, name) in [(150, "bar_1_25x"), (180, "bar_1_5x"), (240, "bar_2x")] {
        let s = Scale::new(n).unwrap();
        let size = s.physical_size(LogicalSize::new(400.0, 36.0));
        let buf = render(bar_scene(), size.w, size.h, s);
        assert_matches_ref(name, &buf, TOLERANCE);
    }
}

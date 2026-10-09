//! Offline PNG tests of `attach:` concave fillets (design.md: "Concave
//! fillets, so a panel grows out of the bar or screen edge", `attach:
//! top` on a popup or panel): the target box's corners on the attached
//! side are square, a concave fillet of each corner's radius joins it to
//! the edge outside the box, and the surface's overhang grows by their
//! reach along the edge (none past the edge itself).
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test fillet`.

mod common;

use common::*;
use strand_scene::*;

const TOLERANCE: u8 = 3;

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// Lays out and paints the scene's one surface; returns its spec and
/// pixels.
fn render(diff: SceneDiff) -> (SurfaceSpec, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    let spec = r.surface_spec(root).unwrap().clone();
    let size = Scale::ONE.physical_size(LogicalSize::new(
        spec.width.unwrap() + spec.overhang.left + spec.overhang.right,
        spec.height.unwrap() + spec.overhang.top + spec.overhang.bottom,
    ));
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), size, Scale::ONE);
    let mut buf = Buffer::new(size.w, size.h, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    // The overhang as laid out at the configured size.
    let spec = r.surface_spec(root).unwrap().clone();
    (spec, buf)
}

/// `panel { attach: <edge>; width: 200; height: 100 }` holding one box
/// with a background and `radius`, `inset` from the panel's sides by its
/// padding.
fn attached(edge: Option<&str>, radius: f32, inset: f32) -> SceneDiff {
    let mut b = Builder::default();
    let mut props = vec![
        (Prop::Width, num(200.0)),
        (Prop::Height, num(100.0)),
        (
            Prop::Pad,
            PropValue::Insets(Insets {
                top: 0.0,
                right: inset,
                bottom: 0.0,
                left: inset,
            }),
        ),
    ];
    if let Some(e) = edge {
        props.push((Prop::Attach, kw(e)));
    }
    let root = b.node(NodeKind::Panel, None, props);
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::Width, num(200.0 - 2.0 * inset)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color("#89b4fa")),
            (Prop::Radius, num(radius)),
        ],
    );
    b.diff
}

fn alpha(buf: &Buffer, x: u32, y: u32) -> u8 {
    buf.px(x, y)[3]
}

/// `attach: top` on a panel whose box has radius 16: the overhang grows
/// 16 on the left and right only, the box's top corners are square, its
/// bottom ones round, and beside each top corner a concave fillet meets
/// the edge (filled along the edge, empty by the box's side).
#[test]
fn attach_top_squares_the_corners_and_adds_concave_fillets() {
    let (spec, buf) = render(attached(Some("top"), 16.0, 0.0));
    assert_eq!(spec.attach, Some(Edge::Top));
    assert_eq!(
        spec.overhang,
        Insets {
            top: 0.0,
            right: 16.0,
            bottom: 0.0,
            left: 16.0
        }
    );
    assert_eq!((buf.size.w, buf.size.h), (232, 100));
    // The box spans x 16..216.
    assert_eq!(alpha(&buf, 16, 0), 255, "the top-left corner is square");
    assert_eq!(alpha(&buf, 215, 0), 255, "the top-right corner is square");
    assert_eq!(alpha(&buf, 16, 99), 0, "the bottom-left stays round");
    assert_eq!(alpha(&buf, 14, 0), 255, "the left fillet meets the edge");
    assert_eq!(alpha(&buf, 217, 0), 255, "so does the right one");
    assert_eq!(alpha(&buf, 3, 12), 0, "concave: empty by the box's side");
    assert_eq!(alpha(&buf, 228, 12), 0);
    assert_eq!(alpha(&buf, 3, 50), 0, "nothing below the fillet");
    assert_matches_ref("fillet_top", &buf, TOLERANCE);
}

/// Without `attach` the box is drawn as written: rounded, no overhang.
/// A box inset from the panel's sides needs less room past them.
#[test]
fn the_fillet_follows_the_target_box() {
    let (spec, buf) = render(attached(None, 16.0, 0.0));
    assert_eq!(spec.overhang, Insets::default());
    assert_eq!(alpha(&buf, 0, 0), 0, "rounded");
    let (spec, buf) = render(attached(Some("top"), 16.0, 10.0));
    assert_eq!((spec.overhang.left, spec.overhang.right), (6.0, 6.0));
    // The box spans x 16..216 of the buffer again (6 + 10).
    assert_eq!(alpha(&buf, 16, 0), 255);
    assert_eq!(alpha(&buf, 14, 0), 255);
    // Square corners: no fillet, no overhang.
    let (spec, _) = render(attached(Some("top"), 0.0, 0.0));
    assert_eq!(spec.overhang, Insets::default());
}

/// `attach: left` on a panel that paints its own background: the root is
/// the target, its fillets go above and below its left side, and its
/// shadow's reach past the left edge is dropped (the screen's edge cuts
/// it anyway) while it stays on the other sides.
#[test]
fn attach_left_on_a_painted_root_drops_the_shadow_past_the_edge() {
    let mut b = Builder::default();
    b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(120.0)),
            (Prop::Height, num(80.0)),
            (Prop::Attach, kw("left")),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Radius, num(12.0)),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 0.0,
                    blur: 4.0,
                    spread: 0.0,
                    color: hex("#000000").with_alpha(0.4),
                }]),
            ),
        ],
    );
    let (spec, buf) = render(b.diff);
    let o = spec.overhang;
    assert_eq!(o.left, 0.0, "nothing past the attached edge");
    assert!(o.top >= 12.0 && o.bottom >= 12.0 && o.right > 0.0, "{o:?}");
    let top = o.top as u32;
    assert_eq!(alpha(&buf, 0, top), 255, "square on the edge");
    assert_eq!(alpha(&buf, 0, top - 1), 255, "the upper fillet");
    assert_eq!(alpha(&buf, 0, top + 80), 255, "the lower fillet");
    assert_matches_ref("fillet_left", &buf, TOLERANCE);
}

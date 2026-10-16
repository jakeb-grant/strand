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

/// (m6-owner-fixes, owner-testing.md gap 26) A popup of a 32 px top bar
/// with `attach: top; margin: -32, 0, 0`, the island's covering card:
/// its spec keeps the set negative margin beside `attach` (placement
/// takes it over the bar to the screen's edge, `strand-surface`'s
/// `an_attached_popup_touches_its_anchor_at_its_margin`), and its card's
/// top corners are drawn square with a concave fillet of their radius
/// beside each, while its shadow reaches out on every side but the
/// attached one, which meets the screen's edge.
#[test]
fn a_popup_attached_over_its_bar_draws_its_fillets_at_the_edge() {
    let mut r = renderer();
    let mut b = Builder::default();
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Name, text("Top")),
            (Prop::Height, num(32.0)),
            (Prop::Bg, color("#11111b")),
            (
                Prop::Pad,
                PropValue::Insets(Insets {
                    left: 200.0,
                    ..Insets::default()
                }),
            ),
        ],
    );
    let anchor = b.node(
        NodeKind::Box,
        Some(bar),
        vec![(Prop::Width, num(100.0)), (Prop::Height, num(32.0))],
    );
    let over = PropValue::List(vec![num(-32.0), num(0.0), num(0.0)]);
    let popup = b.node(
        NodeKind::Popup,
        Some(anchor),
        vec![
            (Prop::Open, PropValue::Bool(true)),
            (Prop::Attach, kw("top")),
            (Prop::Margin, over),
            (Prop::Width, num(240.0)),
            (Prop::Height, num(90.0)),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(popup),
        vec![
            (Prop::Width, num(240.0)),
            (Prop::Height, num(90.0)),
            (Prop::Bg, color("#11111b")),
            (
                Prop::Radius,
                PropValue::List(vec![num(14.0), num(14.0), num(24.0), num(24.0)]),
            ),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 6.0,
                    blur: 8.0,
                    spread: 0.0,
                    color: hex("#000000").with_alpha(0.4),
                }]),
            ),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    const BAR: SurfaceId = SurfaceId(1);
    const POP: SurfaceId = SurfaceId(2);
    r.attach_surface(BAR, bar);
    r.configure_surface(BAR, Size::new(800, 32), Scale::ONE);
    let mut bar_buf = Buffer::new(800, 32, Scale::ONE);
    bar_buf.paint(&mut r, BAR, 0);
    r.update();
    let spec = r.surface_spec(popup).unwrap().clone();
    assert_eq!(spec.attach, Some(Edge::Top));
    assert!(spec.margin_set, "a set margin");
    assert_eq!(spec.margin.top, -32.0, "kept beside `attach`");
    assert_eq!(
        spec.anchor_rect,
        Some(LogicalRect::new(200.0, 0.0, 100.0, 32.0))
    );
    let o = spec.overhang;
    assert_eq!(o.top, 0.0, "nothing past the attached edge");
    assert!(o.left >= 14.0 && o.right >= 14.0 && o.bottom > 6.0, "{o:?}");
    let size = Size::new(
        (spec.width.unwrap() + o.left + o.right) as u32,
        (spec.height.unwrap() + o.top + o.bottom) as u32,
    );
    r.attach_surface(POP, popup);
    r.configure_surface(POP, size, Scale::ONE);
    let mut buf = Buffer::new(size.w, size.h, Scale::ONE);
    buf.paint(&mut r, POP, 0);
    let (x0, x1) = (o.left as u32, o.left as u32 + 240);
    assert_eq!(alpha(&buf, x0, 0), 255, "the top-left corner is square");
    assert_eq!(
        alpha(&buf, x1 - 1, 0),
        255,
        "the top-right corner is square"
    );
    assert_eq!(
        alpha(&buf, x0 - 2, 0),
        255,
        "the left fillet meets the edge"
    );
    assert_eq!(alpha(&buf, x1 + 1, 0), 255, "so does the right one");
    assert!(alpha(&buf, x0 - 12, 10) < 255, "concave by the box's side");
    assert!(alpha(&buf, x1 + 11, 10) < 255);
    assert!(alpha(&buf, x0 + 1, 89) < 255, "the far corners stay round");
    assert_matches_ref("fillet_popup_over_bar", &buf, TOLERANCE);
}

/// `panel { attach: top; stack { clip: true; … box { bg; radius: 18 } } }`
/// (300×100 each): the stack's box clips its content, the card. The
/// options add what else the clip holds, each a 60×60 box reaching 30
/// past the stack's left side.
#[derive(Clone, Copy, Default)]
struct Clipped {
    /// The stack's own radius: a rounded clip.
    radius: f32,
    /// A box the stack holds before the card, this far below the edge.
    under: Option<f32>,
    /// A box the stack holds after the card, at the edge.
    over: bool,
    /// A box the card holds, at the edge.
    child: bool,
    /// The card in a second clipping stack inside the first.
    nested: bool,
    /// The card's paint, `#89b4fa` when unset.
    bg: Option<&'static str>,
}

fn overflowing(b: &mut Builder, parent: NodeId, top: f32, bg: &str) {
    b.node(
        NodeKind::Box,
        Some(parent),
        vec![
            (Prop::X, num(-30.0)),
            (
                Prop::Margin,
                PropValue::Insets(Insets {
                    top,
                    ..Insets::default()
                }),
            ),
            (Prop::Width, num(60.0)),
            (Prop::Height, num(60.0)),
            (Prop::Bg, color(bg)),
        ],
    );
}

fn clipped_card(o: Clipped) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Attach, kw("top"))]);
    let mut props = vec![
        (Prop::Clip, PropValue::Bool(true)),
        (Prop::Width, num(300.0)),
        (Prop::Height, num(100.0)),
    ];
    if o.radius > 0.0 {
        props.push((Prop::Radius, num(o.radius)));
    }
    let stack = b.node(NodeKind::Stack, Some(root), props);
    if let Some(top) = o.under {
        overflowing(&mut b, stack, top, "#f38ba8");
    }
    let holder = if o.nested {
        b.node(
            NodeKind::Stack,
            Some(stack),
            vec![(Prop::Clip, PropValue::Bool(true))],
        )
    } else {
        stack
    };
    let card = b.node(
        NodeKind::Box,
        Some(holder),
        vec![
            (Prop::Width, num(300.0)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color(o.bg.unwrap_or("#89b4fa"))),
            (Prop::Radius, num(18.0)),
        ],
    );
    if o.child {
        overflowing(&mut b, card, 0.0, "#a6e3a1");
    }
    if o.over {
        overflowing(&mut b, holder, 0.0, "#f9e2af");
    }
    (b.diff, card)
}

/// (fillet-clip) The card a `clip: true` box holds is the fillet target:
/// its fillets lie outside the clipping box, and they show, since the
/// clips above the target let its fillets through; what else the clip
/// holds stays inside its box.
#[test]
fn a_clip_above_the_card_lets_its_fillets_through() {
    for (under, nested) in [(false, false), (true, false), (true, true)] {
        let (diff, _) = clipped_card(Clipped {
            under: under.then_some(40.0),
            nested,
            ..Clipped::default()
        });
        let (spec, buf) = render(diff);
        assert_eq!((spec.overhang.left, spec.overhang.right), (18.0, 18.0));
        assert_eq!(spec.overhang.top, 0.0);
        assert_eq!((buf.size.w, buf.size.h), (336, 100));
        // The card spans x 18..318.
        assert_eq!(buf.px(18, 0), buf.px(150, 50), "square on the edge");
        assert_eq!(
            buf.px(16, 0),
            buf.px(150, 50),
            "the left fillet meets the edge"
        );
        assert_eq!(buf.px(319, 0), buf.px(150, 50), "so does the right one");
        assert_eq!(alpha(&buf, 3, 12), 0, "concave: empty by the box's side");
        assert_eq!(alpha(&buf, 332, 12), 0);
        assert_eq!(alpha(&buf, 10, 50), 0, "the clip still cuts what overflows");
        assert_eq!(alpha(&buf, 17, 60), 0);
        if under {
            assert!(alpha(&buf, 30, 60) == 255, "the box shows inside the clip");
        }
    }
    let (diff, _) = clipped_card(Clipped {
        under: Some(40.0),
        ..Clipped::default()
    });
    let (_, buf) = render(diff);
    assert_matches_ref("fillet_through_clip", &buf, TOLERANCE);
}

/// (fillet-clip) A change of the card's paint repaints its fillets
/// outside the clip too: its damage takes them in.
#[test]
fn a_clipped_cards_fillets_repaint_with_it() {
    let (diff, card) = clipped_card(Clipped {
        nested: true,
        ..Clipped::default()
    });
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(336, 100), Scale::ONE);
    let mut buf = Buffer::new(336, 100, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    assert_eq!(buf.px(16, 0), buf.px(150, 50));
    let mut d = SceneDiff::new();
    d.set(card, Prop::Bg, color("#a6e3a1"));
    r.apply(d);
    let dmg = buf.paint(&mut r, SurfaceId(1), 1);
    assert!(dmg.covers(Rect::new(0, 0, 18, 18)), "{dmg:?}");
    assert_eq!(
        buf.px(16, 0),
        buf.px(150, 50),
        "the fillet took the new paint"
    );
    assert_eq!(buf.px(319, 0), buf.px(150, 50));
}

/// (fillet-clip) A rounded clip above the card rounds what it holds, but
/// the card's corners on the edge stay square where its fillets meet
/// them: no gap opens between a fillet and the card.
#[test]
fn a_rounded_clip_keeps_the_cards_corners_on_the_edge_square() {
    for nested in [false, true] {
        let (diff, _) = clipped_card(Clipped {
            radius: 18.0,
            nested,
            ..Clipped::default()
        });
        let (_, buf) = render(diff);
        let card = buf.px(150, 50);
        // The card spans x 18..318; each fillet reaches 18 past it.
        assert_eq!(buf.px(18, 0), card, "square on the edge");
        assert_eq!(buf.px(317, 0), card);
        for x in 10..326 {
            assert_eq!(buf.px(x, 1), card, "no gap along the edge at x {x}");
        }
        for y in 0..18 {
            assert_eq!(
                buf.px(18, y),
                card,
                "the left side meets its fillet at y {y}"
            );
            assert_eq!(buf.px(317, y), card, "so does the right one at y {y}");
        }
        assert_eq!(alpha(&buf, 3, 12), 0, "concave: empty by the box's side");
        assert!(alpha(&buf, 19, 98) < 255, "the far corners stay round");
        assert!(alpha(&buf, 316, 98) < 255);
    }
    let (diff, _) = clipped_card(Clipped {
        radius: 18.0,
        ..Clipped::default()
    });
    let (_, buf) = render(diff);
    assert_matches_ref("fillet_through_rounded_clip", &buf, TOLERANCE);
}

/// (fillet-clip) Only the card's fill passes the clip: a box the clip
/// holds after the card, one the card holds, and one under a translucent
/// card, each reaching into a fillet, stay inside the clip's box.
#[test]
fn only_the_cards_fill_passes_the_clip() {
    let cases = [
        (
            "over",
            Clipped {
                over: true,
                ..Clipped::default()
            },
        ),
        (
            "child",
            Clipped {
                child: true,
                ..Clipped::default()
            },
        ),
        (
            "under a translucent card",
            Clipped {
                // A pixel below the edge: one touching it would be the
                // target.
                under: Some(1.0),
                bg: Some("#89b4fa80"),
                ..Clipped::default()
            },
        ),
        (
            "over, nested and rounded",
            Clipped {
                over: true,
                nested: true,
                radius: 18.0,
                ..Clipped::default()
            },
        ),
    ];
    for (what, o) in cases {
        let (diff, _) = clipped_card(o);
        let (_, buf) = render(diff);
        // The card alone, clear of the boxes at the left.
        let card = buf.px(150, 50);
        // Inside the left fillet, outside the stack (x < 18).
        for (x, y) in [(10, 2), (14, 1), (16, 5), (17, 10)] {
            assert_eq!(buf.px(x, y), card, "{what}: the fillet at ({x}, {y})");
        }
        // Outside the stack, past the fillet: nothing.
        assert_eq!(alpha(&buf, 3, 20), 0, "{what}: cut by the clip");
        if o.over || o.child {
            assert_ne!(
                buf.px(30, 30),
                card,
                "{what}: the box shows inside the clip"
            );
        }
    }
}

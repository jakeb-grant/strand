//! (shadow-buffer, decisions.md) A surface's buffer reserves the reach of
//! its shadows as they are drawn (design.md: "Shadows enlarge the
//! buffer"; a `shadow` change springs, like every visual prop). Layout
//! reads a shadow at rest, so a shadow springing back from a large
//! value to a small one (a hover's lift ending) must keep the room it
//! is still drawn in until it settles, and the surface shrinks then,
//! before its next frame is drawn.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const BAR: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

fn shadow(y: f32, blur: f32) -> PropValue {
    PropValue::Shadow(vec![Shadow {
        x: 0.0,
        y,
        blur,
        spread: 0.0,
        color: hex("#00000080"),
    }])
}

fn set_shadow(r: &mut Renderer, id: NodeId, value: PropValue) {
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id,
        prop: Prop::Shadow,
        value,
        transition: Transition::Spring {
            stiffness: 300.0,
            damping: 1.0,
        },
    });
    assert!(r.apply(d).is_empty());
}

/// The bar's buffer for its spec, configured when it changed.
fn configure(r: &mut Renderer, bar: NodeId, buf: &mut Buffer) -> Insets {
    let o = r.surface_spec(bar).unwrap().overhang;
    let size = Size::new(
        (300.0 + o.left + o.right).ceil() as u32,
        (40.0 + o.top + o.bottom).ceil() as u32,
    );
    if size != buf.size {
        r.configure_surface(BAR, size, Scale::ONE);
        *buf = Buffer::new(size.w, size.h, Scale::ONE);
    }
    o
}

#[test]
fn a_shadow_springing_back_keeps_the_room_it_is_drawn_in() {
    let mut b = Builder::default();
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Height, num(40.0)), (Prop::Bg, color("#202020"))],
    );
    let card = b.node(
        NodeKind::Box,
        Some(bar),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(36.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Shadow, shadow(2.0, 4.0)),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.take_surface_changes();
    r.set_surface_bounds(BAR, Some(LogicalSize::new(300.0, 200.0)));
    r.attach_surface(BAR, bar);
    let mut buf = Buffer::new(1, 1, Scale::ONE);
    let small = configure(&mut r, bar, &mut buf);
    let mut k = 0;
    buf.paint_at(&mut r, BAR, 0, frame(k));
    // Lifted: the shadow grows, and the buffer with it at once.
    set_shadow(&mut r, card, shadow(12.0, 24.0));
    while r.wants_frame(BAR) || r.has_surface_changes() {
        r.take_surface_changes();
        configure(&mut r, bar, &mut buf);
        k += 1;
        buf.paint_at(&mut r, BAR, 0, frame(k));
        assert!(k < 200, "the lift never settled");
    }
    let big = configure(&mut r, bar, &mut buf);
    assert!(
        big.bottom > small.bottom + 20.0,
        "the lifted shadow reaches further: {small:?} then {big:?}"
    );
    // Springing back: every frame is drawn in a buffer that holds the
    // shadow as drawn (its last row shows none of it), and the bar's own
    // box stays its 40 px (nothing past it but the shadow).
    set_shadow(&mut r, card, shadow(2.0, 4.0));
    let mut frames = 0;
    let mut held = 0;
    loop {
        r.take_surface_changes();
        let o = configure(&mut r, bar, &mut buf);
        if !r.wants_frame(BAR) {
            break;
        }
        k += 1;
        frames += 1;
        buf.paint_at(&mut r, BAR, 0, frame(k));
        let last = buf.size.h - 1;
        let cut = (0..buf.size.w).map(|x| buf.px(x, last)[3]).max();
        assert!(
            cut <= Some(8),
            "frame {frames} of the spring back: the shadow is cut at the buffer's \
             bottom edge (alpha {cut:?}), overhang {o:?}"
        );
        if o.bottom > small.bottom {
            held += 1;
            let below = (o.top + 41.0) as u32;
            assert_eq!(
                buf.px(250, below)[3],
                0,
                "frame {frames}: the bar is drawn past its 40 px, overhang {o:?}"
            );
        }
        assert!(k < 400, "the spring back never settled");
    }
    assert!(frames > 5, "it sprang back over {frames} frames");
    assert!(
        held > 5,
        "the buffer held the lifted reach for {held} frames"
    );
    // Settled: the buffer is the small shadow's again, asked for before
    // another frame is drawn.
    assert_eq!(configure(&mut r, bar, &mut buf), small);
}

/// (shadow-buffer) A content-sized panel, whose overhang its spec asks
/// for, keeps the room a lifted shadow took: a compositor may animate
/// every resize of a layer surface, so the room is never given back while
/// the config stands, neither as the shadow springs back nor once it has
/// settled, and a second lift asks for nothing.
#[test]
fn a_content_sized_panel_keeps_its_shadow_room() {
    const PANEL: SurfaceId = SurfaceId(2);
    let mut b = Builder::default();
    let panel = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Open, PropValue::Bool(true))],
    );
    let col = b.node(
        NodeKind::Col,
        Some(panel),
        vec![(Prop::Pad, num(8.0)), (Prop::Bg, color("#202020"))],
    );
    let card = b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Shadow, shadow(2.0, 4.0)),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.take_surface_changes();
    let spec = r.surface_spec(panel).cloned().unwrap();
    let small = spec.overhang;
    let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
    let size = Size::new(
        (w + small.left + small.right).ceil() as u32,
        (h + small.top + small.bottom).ceil() as u32,
    );
    r.attach_surface(PANEL, panel);
    r.configure_surface(PANEL, size, Scale::ONE);
    let mut buf = Buffer::new(size.w, size.h, Scale::ONE);
    let mut k = 0;
    buf.paint_at(&mut r, PANEL, 0, frame(k));
    set_shadow(&mut r, card, shadow(12.0, 24.0));
    while r.wants_frame(PANEL) {
        k += 1;
        buf.paint_at(&mut r, PANEL, 0, frame(k));
        assert!(k < 200, "the lift never settled");
    }
    let big = r.surface_spec(panel).unwrap().overhang;
    assert!(
        big.bottom > small.bottom + 20.0,
        "the lifted shadow reaches further: {small:?} then {big:?}"
    );
    r.take_surface_changes();
    for (round, lift) in [(0, shadow(2.0, 4.0)), (1, shadow(12.0, 24.0))] {
        set_shadow(&mut r, card, lift);
        let mut frames = 0;
        while r.wants_frame(PANEL) {
            assert_eq!(
                r.surface_spec(panel).unwrap().overhang,
                big,
                "round {round}, frame {frames}: the room stays"
            );
            assert!(
                r.take_surface_changes().is_empty(),
                "round {round}, frame {frames}: the panel's spec changed"
            );
            k += 1;
            frames += 1;
            buf.paint_at(&mut r, PANEL, 0, frame(k));
            assert!(k < 600, "the spring never settled");
        }
        assert!(frames > 5, "round {round}: it sprang over {frames} frames");
        assert_eq!(r.surface_spec(panel).unwrap().overhang, big, "at rest");
        assert!(
            r.take_surface_changes().is_empty(),
            "round {round}: at rest"
        );
    }
}

/// The bar's overhang with one 60 × 40 box (the bar's height) glowing
/// `glow`, and then, when `later` is given, once logic set the glow to
/// it (`~ instant`) and a frame drew it.
fn glow_overhang(glow: PropValue) -> Insets {
    glow_overhang_then(glow, None)
}

fn glow_overhang_then(glow: PropValue, later: Option<PropValue>) -> Insets {
    let mut b = Builder::default();
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Height, num(40.0)), (Prop::Bg, color("#202020"))],
    );
    let chip = b.node(
        NodeKind::Box,
        Some(bar),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Glow, glow),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.set_surface_bounds(BAR, Some(LogicalSize::new(300.0, 200.0)));
    r.attach_surface(BAR, bar);
    let mut buf = Buffer::new(1, 1, Scale::ONE);
    configure(&mut r, bar, &mut buf);
    buf.paint_at(&mut r, BAR, 0, frame(0));
    r.take_surface_changes();
    if let Some(value) = later {
        let mut d = SceneDiff::new();
        d.push(SceneOp::SetProp {
            id: chip,
            prop: Prop::Glow,
            value,
            transition: Transition::Instant,
        });
        assert!(r.apply(d).is_empty());
        configure(&mut r, bar, &mut buf);
        buf.paint_at(&mut r, BAR, 0, frame(1));
        r.take_surface_changes();
    }
    r.surface_spec(bar).unwrap().overhang
}

/// (integration-r8, decisions.md) A glow is drawn as a shadow (a box's,
/// with no offset), so the buffer keeps room for it as for one; a glow
/// that breathes with time (`glow: 6 + 4 * wave(1.6s), …`, which layout
/// reads at t = 0, its least) keeps room for its peak, as a steady
/// `glow: 10` does, so its peak is not cut at the buffer's edge.
#[test]
fn a_glow_keeps_room_for_its_peak() {
    let n = |v: f32| TokenExpr::value(PropValue::Number(v));
    let bin = |op, lhs, rhs| TokenExpr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    };
    let green = hex("#40a02b73");
    let breathing = PropValue::Token(TokenExpr::Template {
        value: Box::new(PropValue::List(vec![num(0.0), PropValue::Color(green)])),
        colors: vec![None],
        numbers: vec![Some(bin(
            BinOp::Add,
            n(6.0),
            bin(
                BinOp::Mul,
                n(4.0),
                TokenExpr::Wave {
                    period: Duration::from_millis(1600),
                    phase: Box::new(n(0.0)),
                },
            ),
        ))],
    });
    let steady = |r: f32| PropValue::List(vec![num(r), PropValue::Color(green)]);
    let none = glow_overhang(num(0.0));
    let six = glow_overhang(steady(6.0));
    let ten = glow_overhang(steady(10.0));
    let peak = glow_overhang(breathing);
    assert_eq!(none, Insets::default(), "no glow, no overhang");
    assert!(
        six.bottom > 0.0 && ten.bottom > six.bottom,
        "{six:?} {ten:?}"
    );
    assert_eq!(peak, ten, "the breathing glow keeps room for its peak");
    // A clear glow draws nothing and keeps no room.
    let clear = glow_overhang(PropValue::List(vec![
        num(10.0),
        PropValue::Color(Color::TRANSPARENT),
    ]));
    assert_eq!(clear, Insets::default());
    // Logic lighting the glow later lays the surface out again for it.
    assert_eq!(glow_overhang_then(num(0.0), Some(steady(10.0))), ten);
}

/// (shadow-buffer) A panel reserves from its first spec the room of
/// every shadow a node on it may take (`Prop::ShadowRoom`: a hover's, a
/// `when`'s), so the first time one shows asks for nothing; and (on
/// Hyprland, moves painted) the room its own `enter` move takes is
/// added once: diffs elsewhere leave its spec as it was. A `slide`
/// takes its box's size, and an `x` a `when` may set its value.
#[test]
fn a_panel_reserves_the_shadows_it_may_take_once() {
    let mut b = Builder::default();
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Height, num(40.0)), (Prop::Bg, color("#202020"))],
    );
    let panel = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Open, PropValue::Bool(true)),
            (Prop::Width, num(200.0)),
            (Prop::Height, num(100.0)),
            (Prop::Enter, PropValue::Pose(vec![(Prop::Y, num(20.0))])),
        ],
    );
    let card = b.node(
        NodeKind::Box,
        Some(panel),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Shadow, shadow(2.0, 4.0)),
            (
                Prop::ShadowRoom,
                PropValue::List(vec![
                    PropValue::List(vec![shadow(2.0, 4.0), shadow(40.0, 60.0)]),
                    PropValue::List(Vec::new()),
                    PropValue::Bool(false),
                ]),
            ),
        ],
    );
    let mut r = renderer();
    r.set_compositor_pose_offset(false);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(2), panel);
    r.configure_surface(SurfaceId(2), Size::new(400, 400), Scale::ONE);
    let mut buf = Buffer::new(400, 400, Scale::ONE);
    buf.paint_at(&mut r, SurfaceId(2), 0, frame(0));
    r.take_surface_changes();
    let first = r.surface_spec(panel).unwrap().overhang;
    // The lifted shadow (blur 60 at y 40) reaches 131 below the 30 px
    // card, 61 past the 100 px panel, and the enter move 20 more.
    assert_eq!(first.bottom, 81.0, "reserved from the start: {first:?}");
    for (k, bg) in ["#303030", "#404040", "#505050", "#606060"]
        .into_iter()
        .enumerate()
    {
        let mut d = SceneDiff::new();
        d.set(bar, Prop::Bg, color(bg));
        assert!(r.apply(d).is_empty());
        buf.paint_at(&mut r, SurfaceId(2), 0, frame(k as u32 + 1));
        assert_eq!(
            r.surface_spec(panel).unwrap().overhang,
            first,
            "diff {k} elsewhere"
        );
    }
    set_shadow(&mut r, card, shadow(40.0, 60.0));
    let mut k = 10;
    while r.wants_frame(SurfaceId(2)) {
        k += 1;
        buf.paint_at(&mut r, SurfaceId(2), 0, frame(k));
        assert!(k < 400, "the lift never settled");
    }
    assert_eq!(r.surface_spec(panel).unwrap().overhang, first, "lifted");
    assert!(
        r.take_surface_changes().is_empty(),
        "the lift asked for nothing"
    );
    // A top panel sliding in from below its box, and whose `x` a `when`
    // may set to 40 (`shadow_room`'s fifth item), has the room for both
    // from the start: its slide is its own height, 80.
    let mut b = Builder::default();
    let slid = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Open, PropValue::Bool(true)),
            (Prop::Anchor, PropValue::Keyword("top".into())),
            (Prop::Width, num(200.0)),
            (Prop::Height, num(80.0)),
            (
                Prop::Enter,
                PropValue::Call {
                    name: "slide".into(),
                    args: vec![PropValue::Keyword("bottom".into())],
                },
            ),
            (
                Prop::ShadowRoom,
                PropValue::List(vec![
                    PropValue::List(Vec::new()),
                    PropValue::List(Vec::new()),
                    PropValue::Bool(false),
                    PropValue::List(vec![num(0.0), num(40.0)]),
                    PropValue::List(Vec::new()),
                ]),
            ),
        ],
    );
    let mut r = renderer();
    r.set_compositor_pose_offset(false);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(3), slid);
    r.configure_surface(SurfaceId(3), Size::new(400, 400), Scale::ONE);
    buf.paint_at(&mut r, SurfaceId(3), 0, frame(500));
    let o = r.surface_spec(slid).unwrap().overhang;
    assert_eq!((o.bottom, o.right), (80.0, 40.0), "{o:?}");
}

/// (lock-polish) A shadow inside a `lock` is paint, cut at the lock's
/// edge: the compositor sizes a lock surface to its output, so it asks
/// for no overhang and is not sized by its content. Before, a lock was
/// content-sized like a panel: its shadows' room inset the whole lock
/// inside its buffer (black around it), and its first frame waited the
/// first-frame wait for a configure at its content's size plus that room,
/// which never comes (530 ms on the owner's lock).
#[test]
fn a_shadow_inside_a_lock_is_cut_at_its_edge() {
    const LOCK: SurfaceId = SurfaceId(4);
    let mut b = Builder::default();
    let lock = b.node(
        NodeKind::Lock,
        None,
        vec![
            (Prop::Open, PropValue::Bool(true)),
            (Prop::Bg, color("#2050d0")),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(lock),
        vec![
            (Prop::Width, num(640.0)),
            (Prop::Height, num(360.0)),
            (Prop::Align, PropValue::Keyword("center".into())),
            (Prop::Justify, PropValue::Keyword("center".into())),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Width, num(560.0)),
            (Prop::Height, num(200.0)),
            (Prop::Bg, color("#e0e0e0")),
            (Prop::Shadow, shadow(60.0, 48.0)),
        ],
    );
    let mut r = renderer();
    r.set_resize_wait(strand_render::RESIZE_WAIT);
    r.set_first_frame_wait(Duration::from_millis(500));
    assert!(r.apply(b.diff).is_empty());
    r.take_surface_changes();
    r.set_surface_bounds(LOCK, Some(LogicalSize::new(640.0, 360.0)));
    r.attach_surface(LOCK, lock);
    r.configure_surface(LOCK, Size::new(640, 360), Scale::ONE);
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert_eq!(spec.overhang, Insets::default(), "a lock asks for no room");
    assert_eq!((spec.width, spec.height), (None, None), "nor a size");
    assert!(r.wants_frame(LOCK), "its first frame is not held");
    assert_eq!(r.frame_deadline(LOCK), None, "nor waits for a configure");
    let mut buf = Buffer::new(640, 360, Scale::ONE);
    buf.paint_at(&mut r, LOCK, 0, frame(0));
    let bg = [0xd0, 0x50, 0x20, 0xff];
    for (x, y) in [(0, 0), (639, 0), (0, 40), (639, 40), (320, 2)] {
        assert_eq!(buf.px(x, y), bg, "the lock fills its output at {x},{y}");
    }
    // The shadow reaches the bottom edge, and is cut there.
    assert_ne!(buf.px(320, 358), bg, "the shadow is drawn up to the edge");
    assert_eq!(r.surface_spec(lock).unwrap().overhang, Insets::default());
    assert!(
        r.take_surface_changes().is_empty(),
        "the frame asks for nothing"
    );
    assert_matches_ref("lock_shadow_cut", &buf, 2);
}

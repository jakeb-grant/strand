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

/// The same on a content-sized panel, whose overhang its spec asks for:
/// held at the lifted reach while the shadow springs back, and the
/// smaller spec asked for once it settles, before another frame.
#[test]
fn a_content_sized_panel_keeps_its_shadow_room_until_it_settles() {
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
    set_shadow(&mut r, card, shadow(2.0, 4.0));
    // Held at the lifted reach frame by frame, then the resting one's,
    // asked for right after the frame the shadow settled in.
    let mut frames = 0;
    let shrunk = loop {
        let o = r.surface_spec(panel).unwrap().overhang;
        if o != big {
            assert_eq!(
                o, small,
                "frame {frames}: the overhang is the lifted or the rest one"
            );
            assert!(
                r.has_surface_changes(),
                "the smaller spec is asked for at once"
            );
            break frames;
        }
        assert!(
            !r.has_surface_changes(),
            "frame {frames}: the panel's spec changed while the shadow was in flight"
        );
        assert!(
            r.wants_frame(PANEL),
            "frame {frames}: it settled without shrinking"
        );
        k += 1;
        frames += 1;
        buf.paint_at(&mut r, PANEL, 0, frame(k));
        assert!(k < 400, "the spring back never settled");
    };
    assert!(shrunk > 5, "it sprang back over {shrunk} frames");
}

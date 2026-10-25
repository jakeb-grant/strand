//! (window) A `window` surface on the render side (decisions.md
//! `## window`): sized by its content while the compositor leaves the
//! size to it, laid out at whatever size the compositor's configure
//! gives (a tiling layout, a maximize) without asking for another or
//! holding a frame for one, and closed by the compositor (the surface
//! manager's `ClickAway`) but never by Escape or focus loss.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test window`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::{Intent, Renderer, Router};
use strand_scene::*;

const TOLERANCE: u8 = 3;
const WIN: SurfaceId = SurfaceId(1);

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// A settings window: a title bar the config draws over a body.
fn scene(b: &mut Builder) -> NodeId {
    let win = b.node(
        NodeKind::Window,
        None,
        vec![
            (Prop::Name, text("Settings")),
            (Prop::Open, PropValue::Bool(true)),
            (Prop::TwoWay, PropValue::List(vec![kw("open")])),
            (Prop::Decorations, kw("none")),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let col = b.node(NodeKind::Col, Some(win), vec![(Prop::Grow, num(1.0))]);
    let title = b.node(
        NodeKind::Row,
        Some(col),
        vec![
            (Prop::Height, num(32.0)),
            (Prop::Pad, num(8.0)),
            (Prop::Bg, color("#313244")),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(title),
        vec![(Prop::Text, text("Settings"))],
    );
    let body = b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Grow, num(1.0)),
            (Prop::MinWidth, num(240.0)),
            (Prop::MinHeight, num(120.0)),
            (Prop::Pad, num(12.0)),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(body),
        vec![(Prop::Text, text("Theme: dark"))],
    );
    win
}

fn spec_of(changes: &[(NodeId, SurfaceChange)], node: NodeId) -> Option<SurfaceSpec> {
    changes.iter().rev().find_map(|(n, c)| match c {
        SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } if *n == node => {
            Some(s.clone())
        }
        _ => None,
    })
}

fn booted() -> (Renderer, NodeId, SurfaceSpec) {
    let mut r = renderer();
    let mut b = Builder::default();
    let win = scene(&mut b);
    assert!(r.apply(b.diff).is_empty());
    let spec = spec_of(&r.take_surface_changes(), win).expect("the window's spec");
    (r, win, spec)
}

fn paint(r: &mut Renderer, w: u32, h: u32) -> Buffer {
    let mut buf = Buffer::new(w, h, Scale::ONE);
    buf.paint(r, WIN, 0);
    buf
}

/// Left to it, a window asks for its content's size and is drawn at it.
#[test]
fn a_window_asks_for_its_contents_size() {
    let (mut r, win, spec) = booted();
    assert_eq!(spec.kind, NodeKind::Window);
    assert_eq!(spec.decorations, Decorations::None);
    let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
    assert!(w >= 240.0 && h >= 152.0, "{w} × {h}");
    r.attach_surface(WIN, win);
    r.set_window_sized(WIN, (false, false));
    r.configure_surface(WIN, Size::new(w as u32, h as u32), Scale::ONE);
    let buf = paint(&mut r, w as u32, h as u32);
    assert_matches_ref("window_content_size", &buf, TOLERANCE);
}

/// Tiled at another size, it fills it: laid out at the configured size,
/// with no other size asked for and no frame held for one.
#[test]
fn a_tiled_window_fills_the_size_the_compositor_gives() {
    let (mut r, win, spec) = booted();
    r.set_resize_wait(Duration::from_millis(200));
    r.attach_surface(WIN, win);
    r.set_window_sized(WIN, (true, true));
    r.configure_surface(WIN, Size::new(640, 400), Scale::ONE);
    assert!(r.wants_frame(WIN), "drawn at once, no frame held");
    let buf = paint(&mut r, 640, 400);
    let root = r.boxes(WIN).unwrap().rects[&win];
    assert_eq!(root, LogicalRect::new(0.0, 0.0, 640.0, 400.0));
    assert_matches_ref("window_tiled", &buf, TOLERANCE);
    // Its spec still names its content's size, for when it floats.
    let now = r.surface_spec(win).unwrap();
    assert_eq!((now.width, now.height), (spec.width, spec.height));
    // A content change asks for nothing while tiled, and is laid out in
    // the size it has.
    let mut d = SceneDiff::new();
    let more = NodeId::new(900, 0);
    d.create(more, NodeKind::Box, Some(win), u32::MAX);
    d.set(more, Prop::Height, num(300.0));
    r.apply(d);
    r.take_surface_changes();
    paint(&mut r, 640, 400);
    let root = r.boxes(WIN).unwrap().rects[&win];
    assert_eq!(root, LogicalRect::new(0.0, 0.0, 640.0, 400.0));
}

/// The compositor's close writes `open: false`; Escape and focus loss
/// leave a window open (they close a two-way panel or popup).
#[test]
fn only_the_compositor_closes_a_window() {
    let (mut r, win, spec) = booted();
    r.attach_surface(WIN, win);
    let (w, h) = (spec.width.unwrap() as u32, spec.height.unwrap() as u32);
    r.configure_surface(WIN, Size::new(w, h), Scale::ONE);
    paint(&mut r, w, h);
    let mut router = Router::new();
    router.attached(WIN, win);
    let closes = |out: &[Intent]| {
        out.contains(&Intent::Write {
            node: win,
            prop: Prop::Open,
            value: PropValue::Bool(false),
        })
    };
    let mut out = router.handle(&InputEvent::KeyboardEnter { surface: WIN }, &mut r);
    out.extend(router.handle(
        &InputEvent::Key {
            surface: WIN,
            key: KeyInput {
                name: "Escape".into(),
                text: String::new(),
                state: ButtonState::Pressed,
                repeat: false,
                modifiers: Modifiers::default(),
                time: 0,
                latin: None,
            },
        },
        &mut r,
    ));
    out.extend(router.handle(&InputEvent::KeyboardLeave { surface: WIN }, &mut r));
    assert!(!closes(&out), "{out:?}");
    let out = router.handle(&InputEvent::ClickAway { surface: WIN }, &mut r);
    assert!(closes(&out), "{out:?}");
}

/// (window) A window the compositor stops sizing (untiled, restored
/// from a maximize) is configured at its own size at once, so it takes
/// that size at once: no growth spring from the compositor's size, which
/// would lay its content out past the buffer it has. A step of its own
/// content afterwards still springs.
#[test]
fn a_window_the_compositor_lets_go_of_takes_its_own_size_at_once() {
    let (mut r, win, spec) = booted();
    let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
    r.attach_surface(WIN, win);
    r.set_window_sized(WIN, (true, true));
    r.configure_surface(WIN, Size::new(640, 400), Scale::ONE);
    let mut buf = Buffer::new(640, 400, Scale::ONE);
    buf.paint_at(&mut r, WIN, 0, Duration::from_millis(1000));
    assert_eq!(
        r.boxes(WIN).unwrap().rects[&win],
        LogicalRect::new(0.0, 0.0, 640.0, 400.0)
    );
    r.set_window_sized(WIN, (false, false));
    r.configure_surface(WIN, Size::new(w as u32, h as u32), Scale::ONE);
    let mut buf = Buffer::new(w as u32, h as u32, Scale::ONE);
    buf.paint_at(&mut r, WIN, 0, Duration::from_millis(1016));
    assert_eq!(
        r.boxes(WIN).unwrap().rects[&win],
        LogicalRect::new(0.0, 0.0, w, h),
        "its own size at once"
    );
    assert!(
        !r.wants_frame(WIN),
        "nothing springs from the compositor's size"
    );
    // Drawn as a window that was never tiled is.
    assert_matches_ref("window_content_size", &buf, TOLERANCE);
}

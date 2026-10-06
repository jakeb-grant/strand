//! `popup` and `tooltip: expr` on the render side (design.md: popups are
//! anchored xdg_popups that nest; `tooltip: expr` adds a tooltip): the
//! spec a popup reports (its parent surface, its anchor's box, its
//! content size and shadow overhang), the popup painted on its own
//! surface, Escape closing it, and a tooltip shown after the pointer
//! rests on its node and hidden when it leaves.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test popups`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::{Flag, Intent, NodeEvent, Renderer, Router};
use strand_scene::*;

const TOLERANCE: u8 = 3;

const BAR: SurfaceId = SurfaceId(1);
const POP: SurfaceId = SurfaceId(2);

struct Nodes {
    bar: NodeId,
    clock: NodeId,
    popup: NodeId,
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// The design's clock: a bar with a text that holds a popup holding a
/// calendar-like card (bg, radius, shadow).
fn scene(b: &mut Builder) -> Nodes {
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Name, text("Top")),
            (Prop::Height, num(36.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let split = b.node(NodeKind::Split, Some(bar), vec![]);
    let center = b.node(NodeKind::Center, Some(split), vec![]);
    let clock = b.node(
        NodeKind::Text,
        Some(center),
        vec![(Prop::Text, text("Sat 04  12:59"))],
    );
    let popup = b.node(
        NodeKind::Popup,
        Some(clock),
        vec![
            (Prop::Open, PropValue::Bool(false)),
            (Prop::TwoWay, PropValue::List(vec![kw("open")])),
        ],
    );
    let card = b.node(
        NodeKind::Col,
        Some(popup),
        vec![
            (Prop::Pad, num(12.0)),
            (Prop::Gap, num(8.0)),
            (Prop::Bg, color("#313244")),
            (Prop::Radius, num(14.0)),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 8.0,
                    blur: 24.0,
                    spread: 0.0,
                    color: Color::BLACK.with_alpha(0.3),
                }]),
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(card),
        vec![(Prop::Text, text("October 2026"))],
    );
    b.node(
        NodeKind::Button,
        Some(card),
        vec![(Prop::Text, text("Today"))],
    );
    Nodes { bar, clock, popup }
}

fn spec_of(changes: &[(NodeId, SurfaceChange)], node: NodeId) -> Option<SurfaceSpec> {
    changes.iter().rev().find_map(|(n, c)| match c {
        SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } if *n == node => {
            Some(s.clone())
        }
        _ => None,
    })
}

fn open_popup(r: &mut Renderer, n: &Nodes) -> SurfaceSpec {
    let mut d = SceneDiff::new();
    d.set(n.popup, Prop::Open, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    let changes = r.take_surface_changes();
    spec_of(&changes, n.popup).expect("the popup's spec changed")
}

fn bar_shown() -> (Renderer, Nodes) {
    let mut r = renderer();
    let mut b = Builder::default();
    let n = scene(&mut b);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(BAR, n.bar);
    r.configure_surface(BAR, Size::new(800, 36), Scale::ONE);
    let mut buf = Buffer::new(800, 36, Scale::ONE);
    buf.paint(&mut r, BAR, 0);
    r.take_surface_changes();
    (r, n)
}

/// A popup's spec names the surface it nests in and its anchor's box
/// there, and is sized by its content with its shadow's overhang.
#[test]
fn a_popup_reports_its_parent_anchor_and_size() {
    let (mut r, n) = bar_shown();
    let spec = open_popup(&mut r, &n);
    assert_eq!(spec.kind, NodeKind::Popup);
    assert!(spec.open && spec.open_two_way && !spec.tooltip);
    assert_eq!(spec.parent, Some(n.bar));
    let clock = r.boxes(BAR).unwrap().rects[&n.clock];
    assert_eq!(spec.anchor_rect, Some(clock));
    assert!(
        clock.x > 300.0 && clock.x < 400.0,
        "the clock is centred: {clock:?}"
    );
    let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
    assert!(w > 90.0 && h > 50.0, "{w} × {h}");
    assert!(
        spec.overhang.bottom > spec.overhang.top,
        "{:?}",
        spec.overhang
    );
    // Closing its parent closes it.
    let mut d = SceneDiff::new();
    d.set(n.bar, Prop::Open, PropValue::Bool(false));
    r.apply(d);
    let changes = r.take_surface_changes();
    assert!(!spec_of(&changes, n.popup).unwrap().open);
}

/// The popup paints its own subtree on its own surface, inheriting the
/// bar's colour and font, inside its overhang.
#[test]
fn a_popup_paints_on_its_own_surface() {
    let (mut r, n) = bar_shown();
    let spec = open_popup(&mut r, &n);
    let o = spec.overhang;
    let size = Size::new(
        (spec.width.unwrap() + o.left + o.right) as u32,
        (spec.height.unwrap() + o.top + o.bottom) as u32,
    );
    r.attach_surface(POP, n.popup);
    r.configure_surface(POP, size, Scale::ONE);
    let mut buf = Buffer::new(size.w, size.h, Scale::ONE);
    buf.paint(&mut r, POP, 0);
    // The card's background in the middle of the box; the shadow below.
    let mid = buf.px(size.w / 2, (o.top + 4.0) as u32);
    assert_eq!(mid, [0x44, 0x32, 0x31, 0xff]);
    let below = buf.px(size.w / 2, size.h - (o.bottom / 2.0) as u32);
    assert!(below[3] > 0 && below[3] < 0x80, "shadow: {below:?}");
    assert_matches_ref("popup_card", &buf, TOLERANCE);
}

/// Escape on a popup's surface (it has the keyboard through its grab)
/// writes `open: false` and tells it `dismiss`.
#[test]
fn escape_closes_a_popup() {
    let (mut r, n) = bar_shown();
    open_popup(&mut r, &n);
    let mut router = Router::new();
    router.attached(POP, n.popup);
    let mut out = router.handle(&InputEvent::KeyboardEnter { surface: POP }, &mut r);
    out.extend(router.handle(
        &InputEvent::Key {
            surface: POP,
            key: KeyInput {
                name: "Escape".into(),
                text: String::new(),
                state: ButtonState::Pressed,
                repeat: false,
                modifiers: Modifiers::default(),
                time: 0,
            },
        },
        &mut r,
    ));
    assert!(out.contains(&Intent::Write {
        node: n.popup,
        prop: Prop::Open,
        value: PropValue::Bool(false)
    }));
    assert!(out.contains(&Intent::Event {
        node: n.popup,
        event: NodeEvent::Dismiss
    }));
    // A click away the compositor reported (its grab ended) does the same.
    let out = router.handle(&InputEvent::ClickAway { surface: POP }, &mut r);
    assert!(out.contains(&Intent::Event {
        node: n.popup,
        event: NodeEvent::Dismiss
    }));
}

/// `tooltip: "…"`: after the pointer rests on the node for the tooltip
/// delay, a render-owned popup holding the text opens under it (no grab,
/// no input), drawn in the inverse surface colours; leaving the node or
/// pressing hides it at once.
#[test]
fn a_tooltip_shows_after_a_rest_and_hides_on_leave() {
    let (mut r, n) = bar_shown();
    let mut d = SceneDiff::new();
    d.set(n.clock, Prop::Tooltip, text("Calendar"));
    r.apply(d);
    r.take_surface_changes();
    r.set_tooltip_delay(Duration::from_millis(40));
    r.set_widget_flag(n.bar, Flag::Hover, true);
    r.set_widget_flag(n.clock, Flag::Hover, true);
    r.update();
    assert!(r.tooltip_popup().is_none(), "not before the delay");
    std::thread::sleep(Duration::from_millis(60));
    assert!(r.next_wake().is_some());
    r.update();
    let tip = r.tooltip_popup().expect("shown after the delay");
    let changes = r.take_surface_changes();
    let spec = spec_of(&changes, tip).expect("a surface for it");
    assert!(spec.tooltip && spec.open);
    assert_eq!(spec.parent, Some(n.bar));
    assert_eq!(
        spec.anchor_rect,
        Some(r.boxes(BAR).unwrap().rects[&n.clock])
    );
    let size = Size::new(spec.width.unwrap() as u32, spec.height.unwrap() as u32);
    assert!(size.w > 40 && size.h > 14, "{size:?}");
    r.attach_surface(POP, tip);
    r.configure_surface(POP, size, Scale::ONE);
    let mut buf = Buffer::new(size.w, size.h, Scale::ONE);
    buf.paint(&mut r, POP, 0);
    // A dark background and light text.
    assert!(buf.px(2, size.h / 2)[0] < 0x40);
    let lit = (0..size.w).any(|x| buf.px(x, size.h / 2)[1] > 0xc0);
    assert!(lit, "the label is drawn");
    assert_matches_ref("popup_tooltip", &buf, TOLERANCE);
    // Logic changing the text while it shows: the tooltip follows in
    // place (the same popup and surface, resized; no flicker).
    r.take_surface_changes();
    let mut d = SceneDiff::new();
    d.set(n.clock, Prop::Tooltip, text("Calendar and events"));
    r.apply(d);
    r.update();
    let again = r.tooltip_popup().expect("still shown");
    assert_eq!(again, tip, "the same popup");
    let label = r.tree().get(again).unwrap().children[0];
    assert_eq!(
        r.tree().get(label).unwrap().get(Prop::Text),
        Some(&text("Calendar and events"))
    );
    let changes = r.take_surface_changes();
    assert!(
        !changes
            .iter()
            .any(|(_, c)| matches!(c, SurfaceChange::Removed | SurfaceChange::Created(_))),
        "{changes:?}"
    );
    let wider = changes
        .iter()
        .find_map(|(node, c)| match c {
            SurfaceChange::Updated { spec, .. } if *node == tip => spec.width,
            _ => None,
        })
        .expect("its spec follows the text");
    assert!(wider > size.w as f32, "{wider} after {}", size.w);
    // Leaving hides it.
    r.set_widget_flag(n.clock, Flag::Hover, false);
    assert!(r.tooltip_popup().is_none());
    let changes = r.take_surface_changes();
    assert!(
        changes
            .iter()
            .any(|(node, c)| *node == tip && *c == SurfaceChange::Removed)
    );
}

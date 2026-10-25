//! Widgets (design.md, "The proposed API" examples; features.md M2
//! "Widgets"): offline PNGs of `button`, `meter`, `slider`, `segmented`
//! and `input` (caret, selection, placeholder, password), and the input
//! routing that drives them: a slider dragged, a segmented option
//! clicked, text edited at the caret.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test widgets`.

mod common;

use common::*;
use strand_render::widgets::Caret;
use strand_render::{Flag, InputScene, Intent, Renderer, Router};
use strand_scene::input::button;
use strand_scene::*;

const TOLERANCE: u8 = 3;

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn theme() -> TokenTable {
    let mut t = TokenTable::default();
    t.insert("accent", color("#7aa2f7"));
    t.insert("on_accent", color("#11111b"));
    t.insert("surface.hi", color("#313244"));
    t.insert(
        "fg.muted",
        PropValue::Color(hex("#cdd6f4").with_alpha(0.65)),
    );
    t.insert(
        "accent.container",
        PropValue::Color(hex("#7aa2f7").with_alpha(0.3)),
    );
    t.insert("radius.md", num(10.0));
    t
}

/// A panel holding a column of the widgets `add` makes.
fn panel(b: &mut Builder, w: f32, h: f32) -> NodeId {
    b.diff.set_tokens(theme(), Transition::Instant);
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Pad, num(10.0)),
            (Prop::Gap, num(8.0)),
            (Prop::Align, kw("start")),
        ],
    )
}

fn show(r: &mut Renderer, diff: SceneDiff) -> (NodeId, Buffer) {
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    let spec = r.surface_spec(root).unwrap().clone();
    let size = Size::new(spec.width.unwrap() as u32, spec.height.unwrap() as u32);
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), size, Scale::ONE);
    let mut buf = Buffer::new(size.w, size.h, Scale::ONE);
    buf.paint(r, SurfaceId(1), 0);
    (root, buf)
}

fn repaint(r: &mut Renderer, buf: &mut Buffer) {
    buf.paint(r, SurfaceId(1), 1);
}

struct Controls {
    button: NodeId,
    hovered: NodeId,
    meter: NodeId,
    slider: NodeId,
    dragged: NodeId,
    segmented: NodeId,
}

fn controls(b: &mut Builder) -> Controls {
    let col = panel(b, 260.0, 220.0);
    let row = b.node(NodeKind::Row, Some(col), vec![(Prop::Gap, num(8.0))]);
    let button = b.node(
        NodeKind::Button,
        Some(row),
        vec![(Prop::Text, text("Apply"))],
    );
    let hovered = b.node(NodeKind::Button, Some(row), vec![(Prop::Text, text("‹"))]);
    let meter = b.node(
        NodeKind::Meter,
        Some(col),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Height, num(6.0)),
            (Prop::Value, num(0.35)),
            (Prop::Color, PropValue::Token(TokenExpr::path("accent"))),
            (
                Prop::Track,
                PropValue::Color(hex("#cdd6f4").with_alpha(0.15)),
            ),
        ],
    );
    // `value: <-> x`, as the compiler marks it.
    let slider = b.node(
        NodeKind::Slider,
        Some(col),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Value, num(0.6)),
            (Prop::TwoWay, PropValue::List(vec![kw("value")])),
        ],
    );
    let dragged = b.node(
        NodeKind::Slider,
        Some(col),
        vec![(Prop::Width, num(200.0)), (Prop::Value, num(0.6))],
    );
    let segmented = b.node(
        NodeKind::Segmented,
        Some(col),
        vec![
            (
                Prop::Options,
                PropValue::List(vec![kw("auto"), kw("light"), kw("dark")]),
            ),
            (Prop::Value, kw("dark")),
            (Prop::TwoWay, PropValue::List(vec![kw("value")])),
        ],
    );
    Controls {
        button,
        hovered,
        meter,
        slider,
        dragged,
        segmented,
    }
}

#[test]
fn button_meter_slider_and_segmented() {
    let mut r = renderer();
    let mut b = Builder::default();
    let c = controls(&mut b);
    let (_, mut buf) = show(&mut r, b.diff);
    r.set_widget_flag(c.hovered, Flag::Hover, true);
    r.set_drag(c.dragged, Some(0.2));
    repaint(&mut r, &mut buf);
    let boxes = r.boxes(SurfaceId(1)).unwrap().rects.clone();
    let at = |n: NodeId, fx: f32, fy: f32| {
        let b = boxes[&n];
        buf.px((b.x + b.w * fx) as u32, (b.y + b.h * fy) as u32)
    };
    // A button pads its label and has `$surface.hi` behind it.
    let bb = boxes[&c.button];
    assert!(bb.w > 40.0 && bb.h >= 22.0, "{bb:?}");
    assert_eq!(at(c.button, 0.05, 0.5), [0x44, 0x32, 0x31, 0xff]);
    // Hovered: a lighter state layer.
    assert!(at(c.hovered, 0.1, 0.5)[2] > 0x31);
    // The meter: accent up to 35 %, track after.
    assert_eq!(at(c.meter, 0.2, 0.5), [0xf7, 0xa2, 0x7a, 0xff]);
    assert_ne!(at(c.meter, 0.6, 0.5), [0xf7, 0xa2, 0x7a, 0xff]);
    // The slider's fill reaches its value; the dragged one shows the drag.
    assert_eq!(at(c.slider, 0.4, 0.5), [0xf7, 0xa2, 0x7a, 0xff]);
    assert_ne!(at(c.dragged, 0.4, 0.5), [0xf7, 0xa2, 0x7a, 0xff]);
    // The segmented control: the chosen option (the third) on accent.
    let sb = boxes[&c.segmented];
    assert!(sb.w > 90.0, "{sb:?}");
    assert_eq!(at(c.segmented, 0.9, 0.15), [0xf7, 0xa2, 0x7a, 0xff]);
    assert_eq!(at(c.segmented, 0.1, 0.15), [0x44, 0x32, 0x31, 0xff]);
    assert_matches_ref("widgets_controls", &buf, TOLERANCE);
}

/// (graph-follow) `range` on a slider, meters and an arc: a value is
/// drawn as its place between the two ends (`range: 0, 100` at 40 is
/// 40 % along), one past an end at that end, and `range: 0, auto` fits
/// the values shown on the 1, 2, 5 ladder (37 of 50).
struct Ranged {
    slider: NodeId,
    meter: NodeId,
    over: NodeId,
    auto: NodeId,
}

fn ranged(b: &mut Builder) -> Ranged {
    let col = panel(b, 220.0, 170.0);
    let range = |lo: f32, hi: f32| PropValue::List(vec![num(lo), num(hi)]);
    let meter = |b: &mut Builder, value: f32, range: PropValue| {
        b.node(
            NodeKind::Meter,
            Some(col),
            vec![
                (Prop::Width, num(200.0)),
                (Prop::Height, num(6.0)),
                (Prop::Value, num(value)),
                (Prop::Range, range),
            ],
        )
    };
    let slider = b.node(
        NodeKind::Slider,
        Some(col),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Value, num(40.0)),
            (Prop::Range, range(0.0, 100.0)),
            (Prop::TwoWay, PropValue::List(vec![kw("value")])),
        ],
    );
    let ranged = Ranged {
        slider,
        meter: meter(b, 150.0, range(100.0, 200.0)),
        over: meter(b, 300.0, range(0.0, 200.0)),
        auto: meter(b, 37.0, PropValue::List(vec![num(0.0), kw("auto")])),
    };
    b.node(
        NodeKind::Arc,
        Some(col),
        vec![
            (Prop::Size, num(48.0)),
            // (flex) An arc's line is its `stroke:`.
            (
                Prop::Stroke,
                PropValue::Border(Border {
                    width: 5.0,
                    paint: Paint::Solid(hex("#7aa2f7")),
                }),
            ),
            (Prop::Value, num(75.0)),
            (Prop::Range, range(0.0, 100.0)),
        ],
    );
    ranged
}

#[test]
fn a_ranged_slider_meter_and_arc() {
    let mut r = renderer();
    let mut b = Builder::default();
    let c = ranged(&mut b);
    let (_, mut buf) = show(&mut r, b.diff);
    repaint(&mut r, &mut buf);
    let boxes = r.boxes(SurfaceId(1)).unwrap().rects.clone();
    let accent = [0xf7, 0xa2, 0x7a, 0xff];
    let at = |n: NodeId, fx: f32| {
        let b = boxes[&n];
        buf.px((b.x + b.w * fx) as u32, (b.y + b.h / 2.0) as u32)
    };
    // The slider: 40 of 0 to 100.
    assert_eq!(at(c.slider, 0.36), accent);
    assert_ne!(at(c.slider, 0.46), accent);
    // 150 of 100 to 200 is half; 300 of 0 to 200 is full.
    assert_eq!(at(c.meter, 0.45), accent);
    assert_ne!(at(c.meter, 0.55), accent);
    assert_eq!(at(c.over, 0.98), accent);
    // `0, auto`: 37 of 0 to 50.
    assert_eq!(at(c.auto, 0.7), accent);
    assert_ne!(at(c.auto, 0.78), accent);
    assert_matches_ref("widgets_ranged", &buf, TOLERANCE);
}

/// (graph-follow) A slider with `range: 0, 100` dragged writes values in
/// its range: a press a quarter along writes 25, a drag past the end
/// 100.
#[test]
fn a_ranged_slider_writes_in_its_range() {
    let mut r = renderer();
    let mut b = Builder::default();
    let c = ranged(&mut b);
    let (root, _buf) = show(&mut r, b.diff);
    let mut router = Router::new();
    let s = SurfaceId(1);
    router.attached(s, root);
    let sb = r.boxes(s).unwrap().rects[&c.slider];
    let y = sb.y + sb.h / 2.0;
    let (x0, x1) =
        strand_render::widgets::slider_span(sb.x as f64, (sb.x + sb.w) as f64, true, 1.0);
    let x_of = |v: f32| (x0 + v as f64 * (x1 - x0)) as f32;
    let mut out = router.handle(&press(s, x_of(0.25), y, ButtonState::Pressed), &mut r);
    assert_eq!(
        r.widgets().drags.get(&c.slider),
        Some(&0.25),
        "where it is drawn"
    );
    out.extend(router.handle(&motion(s, sb.x + sb.w + 50.0, y), &mut r));
    out.extend(router.handle(&press(s, x_of(0.5), y, ButtonState::Released), &mut r));
    let v: Vec<f32> = writes(&out, c.slider)
        .iter()
        .map(|v| v.as_number().unwrap())
        .collect();
    assert_eq!(v.len(), 3);
    for (got, want) in v.iter().zip([25.0, 100.0, 50.0]) {
        assert!((got - want).abs() < 1e-3, "{v:?}");
    }
}

/// A meter's `value` springs (the OSD's level glides); a frame mid-way
/// shows it between the two values.
#[test]
fn a_meter_value_springs() {
    use std::time::Duration;
    let mut r = renderer();
    let mut b = Builder::default();
    let c = controls(&mut b);
    let (_, mut buf) = show(&mut r, b.diff);
    let ms = |n| Duration::from_millis(n);
    // Shown with a clock (a frame with no change draws nothing).
    r.invalidate(SurfaceId(1));
    buf.paint_at(&mut r, SurfaceId(1), 1, ms(1000));
    let mut d = SceneDiff::new();
    d.set(c.meter, Prop::Value, num(0.85));
    r.apply(d);
    buf.paint_at(&mut r, SurfaceId(1), 1, ms(1017));
    let mb = r.boxes(SurfaceId(1)).unwrap().rects[&c.meter];
    let filled = (0..mb.w as u32)
        .filter(|x| buf.px(mb.x as u32 + x, (mb.y + mb.h / 2.0) as u32)[0] == 0xf7)
        .count() as f32;
    assert!(
        filled > 0.36 * mb.w && filled < 0.84 * mb.w,
        "mid-spring: {filled} of {}",
        mb.w
    );
    assert!(r.animating(SurfaceId(1)));
}

/// (review) A meter with an auto range springing onto a step of the
/// ladder ends full: the range follows the value logic set, not the
/// spring's overshoot past it (which grew `0, auto` to 100 for good, so
/// 50 drew half full).
#[test]
fn a_springing_meter_onto_a_ladder_step_ends_full() {
    use std::time::Duration;
    let mut r = renderer();
    let mut b = Builder::default();
    let c = ranged(&mut b);
    let (_, mut buf) = show(&mut r, b.diff);
    let ms = |n| Duration::from_millis(n);
    r.invalidate(SurfaceId(1));
    buf.paint_at(&mut r, SurfaceId(1), 1, ms(1000));
    let mut d = SceneDiff::new();
    d.set(c.auto, Prop::Value, num(50.0));
    r.apply(d);
    let mut at = 1016;
    buf.paint_at(&mut r, SurfaceId(1), 1, ms(at));
    while r.animating(SurfaceId(1)) {
        at += 16;
        assert!(at < 4000, "settles");
        buf.paint_at(&mut r, SurfaceId(1), 1, ms(at));
    }
    let mb = r.boxes(SurfaceId(1)).unwrap().rects[&c.auto];
    let filled = (0..mb.w as u32)
        .filter(|x| buf.px(mb.x as u32 + x, (mb.y + mb.h / 2.0) as u32)[0] == 0xf7)
        .count() as f32;
    assert!(filled > 0.97 * mb.w, "50 of 50: {filled} of {}", mb.w);
}

/// (review) A ranged meter's value settles within its tolerance of the
/// range's span: a step of 1 % of 0 to 16 GB takes about as many frames
/// as one of 1 % of 0 to 1 (it took 38 frames against 5, springing for
/// a millionth of a byte).
#[test]
fn a_meter_on_a_large_range_settles_as_one_on_0_to_1() {
    use std::time::Duration;
    let frames = |hi: f32| {
        let mut r = renderer();
        let mut b = Builder::default();
        let col = panel(&mut b, 220.0, 40.0);
        let m = b.node(
            NodeKind::Meter,
            Some(col),
            vec![
                (Prop::Width, num(200.0)),
                (Prop::Height, num(6.0)),
                (Prop::Value, num(0.5 * hi)),
                (Prop::Range, PropValue::List(vec![num(0.0), num(hi)])),
            ],
        );
        let (_, mut buf) = show(&mut r, b.diff);
        let ms = |n| Duration::from_millis(n);
        r.invalidate(SurfaceId(1));
        buf.paint_at(&mut r, SurfaceId(1), 1, ms(1000));
        let mut d = SceneDiff::new();
        d.set(m, Prop::Value, num(0.51 * hi));
        r.apply(d);
        let mut n = 0;
        loop {
            n += 1;
            buf.paint_at(&mut r, SurfaceId(1), 1, ms(1000 + 16 * n));
            if !r.animating(SurfaceId(1)) {
                break n;
            }
            assert!(n < 400, "settles");
        }
    };
    let (small, large) = (frames(1.0), frames(1.6e10));
    assert!(large <= small + 2, "{large} frames against {small}");
}

struct Inputs {
    focused: NodeId,
    selected: NodeId,
    empty: NodeId,
    password: NodeId,
}

fn inputs(b: &mut Builder) -> Inputs {
    let col = panel(b, 220.0, 150.0);
    let mk = |b: &mut Builder, t: &str, extra: Vec<(Prop, PropValue)>| {
        let mut props = vec![
            (Prop::Width, num(200.0)),
            (Prop::Text, text(t)),
            (Prop::Placeholder, text("Search apps")),
        ];
        props.extend(extra);
        b.node(NodeKind::Input, Some(col), props)
    };
    Inputs {
        focused: mk(b, "firefox", vec![]),
        selected: mk(b, "firefox", vec![]),
        empty: mk(b, "", vec![]),
        password: mk(b, "hunter2", vec![(Prop::InputType, kw("password"))]),
    }
}

#[test]
fn input_caret_selection_placeholder_and_password() {
    let mut r = renderer();
    let mut b = Builder::default();
    let i = inputs(&mut b);
    let (_, mut buf) = show(&mut r, b.diff);
    for n in [i.focused, i.selected, i.empty, i.password] {
        r.set_widget_flag(n, Flag::Focused, true);
    }
    r.set_caret(i.focused, Some(Caret::at(4)));
    r.set_caret(i.selected, Some(Caret { pos: 4, anchor: 0 }));
    repaint(&mut r, &mut buf);
    let boxes = r.boxes(SurfaceId(1)).unwrap().rects.clone();
    let bf = boxes[&i.focused];
    // The caret: an accent bar inside the text, about four letters in.
    let row = (bf.y + bf.h / 2.0) as u32;
    let caret: Vec<u32> = (0..bf.w as u32)
        .filter(|x| {
            let p = buf.px(bf.x as u32 + x, row);
            p[0] > 0xc0 && p[2] < 0x90
        })
        .collect();
    assert!(
        !caret.is_empty() && caret[0] > 15 && caret[0] < 35,
        "caret columns {caret:?}"
    );
    // The selection: accent.container behind the first four letters.
    let bs = boxes[&i.selected];
    let sel = buf.px(bs.x as u32 + 2, (bs.y + 2.0) as u32);
    assert!(sel[0] > sel[2], "selection is bluish: {sel:?}");
    // An empty focused input shows its placeholder and the caret at 0.
    let be = boxes[&i.empty];
    let p = buf.px(be.x as u32, (be.y + be.h / 2.0) as u32);
    assert!(p[0] > 0xc0, "caret at the start: {p:?}");
    assert_matches_ref("widgets_inputs", &buf, TOLERANCE);
}

fn press(s: SurfaceId, x: f32, y: f32, state: ButtonState) -> InputEvent {
    InputEvent::PointerButton {
        surface: s,
        position: LogicalPoint::new(x, y),
        button: button::LEFT,
        state,
        time: 0,
    }
}

fn motion(s: SurfaceId, x: f32, y: f32) -> InputEvent {
    InputEvent::PointerMotion {
        surface: s,
        position: LogicalPoint::new(x, y),
        time: 0,
    }
}

fn writes(out: &[Intent], node: NodeId) -> Vec<PropValue> {
    out.iter()
        .filter_map(|i| match i {
            Intent::Write { node: n, value, .. } if *n == node => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// `value: <-> x` on a slider: a press moves it under the pointer, a drag
/// follows the pointer (drawn at once, before logic answers) and every
/// step is a two-way write; the release ends the drag.
#[test]
fn a_slider_follows_a_drag_and_writes_its_value() {
    let mut r = renderer();
    let mut b = Builder::default();
    let c = controls(&mut b);
    let (root, _buf) = show(&mut r, b.diff);
    let mut router = Router::new();
    let s = SurfaceId(1);
    router.attached(s, root);
    let sb = r.boxes(s).unwrap().rects[&c.slider];
    let y = sb.y + sb.h / 2.0;
    // Pressed, the knob is drawn larger: its centre runs over the span
    // drawing uses, so it stays under the pointer.
    let (x0, x1) =
        strand_render::widgets::slider_span(sb.x as f64, (sb.x + sb.w) as f64, true, 1.0);
    assert_eq!(
        (x0 - sb.x as f64, sb.x as f64 + sb.w as f64 - x1),
        (8.0, 8.0)
    );
    let x_of = |v: f32| (x0 + v as f64 * (x1 - x0)) as f32;
    let mut out = router.handle(&press(s, x_of(0.25), y, ButtonState::Pressed), &mut r);
    assert_eq!(r.widgets().drags.get(&c.slider), Some(&0.25));
    out.extend(router.handle(&motion(s, x_of(0.5), y), &mut r));
    // Past the end it stops at 1.
    out.extend(router.handle(&motion(s, sb.x + sb.w + 50.0, y), &mut r));
    out.extend(router.handle(&press(s, x_of(0.75), y, ButtonState::Released), &mut r));
    let v: Vec<f32> = writes(&out, c.slider)
        .iter()
        .map(|v| v.as_number().unwrap())
        .collect();
    assert_eq!(v.len(), 4);
    for (got, want) in v.iter().zip([0.25, 0.5, 1.0, 0.75]) {
        assert!((got - want).abs() < 1e-4, "{v:?}");
    }
    assert!(r.widgets().drags.is_empty(), "the release ends the drag");
    assert!(r.widgets().hovered.contains(&c.slider));
    // A display-only slider (`value: level`, one-way) does not move.
    let db = r.boxes(s).unwrap().rects[&c.dragged];
    let (x, y) = (db.x + db.w * 0.2, db.y + db.h / 2.0);
    let mut out = router.handle(&press(s, x, y, ButtonState::Pressed), &mut r);
    assert!(r.widgets().drags.is_empty());
    out.extend(router.handle(&motion(s, x + 40.0, y), &mut r));
    out.extend(router.handle(&press(s, x + 40.0, y, ButtonState::Released), &mut r));
    assert!(writes(&out, c.dragged).is_empty(), "{out:?}");
}

/// (m6-owner-fixes, owner-testing.md bug 16) A slider and a `segmented`
/// inside a card moved by a paint offset (`x: 60; y: 6`, the island
/// card that springs by `x`) and shrunk by `scale: 0.8` (a `when pressed`
/// squeeze) read the pointer where they are drawn, at scales 1 and 1.5:
/// a press on the drawn knob's place for 0.25 writes 0.25, and a click on
/// the drawn middle option writes it. Before, they read it against their
/// laid-out box, so the island's slider jumped 50 px ahead of the
/// pointer.
#[test]
fn widgets_moved_by_paint_offsets_read_the_pointer_where_drawn() {
    for scale in [Scale::ONE, Scale::from_f64(1.5).unwrap()] {
        let mut r = renderer();
        let mut b = Builder::default();
        let col = panel(&mut b, 360.0, 160.0);
        let card = b.node(
            NodeKind::Col,
            Some(col),
            vec![
                (Prop::X, num(60.0)),
                (Prop::Y, num(6.0)),
                (Prop::Scale, num(0.8)),
                (Prop::Gap, num(8.0)),
            ],
        );
        let slider = b.node(
            NodeKind::Slider,
            Some(card),
            vec![
                (Prop::Width, num(200.0)),
                (Prop::Value, num(0.6)),
                (Prop::TwoWay, PropValue::List(vec![kw("value")])),
            ],
        );
        let seg = b.node(
            NodeKind::Segmented,
            Some(card),
            vec![
                (Prop::Width, num(210.0)),
                (
                    Prop::Options,
                    PropValue::List(vec![kw("auto"), kw("light"), kw("dark")]),
                ),
                (Prop::Value, kw("dark")),
                (Prop::TwoWay, PropValue::List(vec![kw("value")])),
            ],
        );
        assert!(r.apply(b.diff).is_empty());
        let root = r.tree().roots()[0];
        let s = SurfaceId(1);
        let k = scale.as_f64();
        let size = Size::new((360.0 * k).round() as u32, (160.0 * k).round() as u32);
        r.attach_surface(s, root);
        r.configure_surface(s, size, scale);
        let mut buf = Buffer::new(size.w, size.h, scale);
        buf.paint(&mut r, s, 0);
        let mut router = Router::new();
        router.attached(s, root);
        // The card's laid-out box, and where it is drawn: moved by its
        // offsets, then scaled 0.8 about its centre.
        let cb = r.boxes(s).unwrap().rects[&card];
        let (cx, cy) = (cb.x + 60.0 + cb.w / 2.0, cb.y + 6.0 + cb.h / 2.0);
        let drawn = |x: f32, y: f32| {
            let (x, y) = (x + 60.0, y + 6.0);
            (cx + (x - cx) * 0.8, cy + (y - cy) * 0.8)
        };
        let sb = r.boxes(s).unwrap().rects[&slider];
        let (x0, x1) =
            strand_render::widgets::slider_span(sb.x as f64, (sb.x + sb.w) as f64, true, 1.0);
        let (px, py) = drawn((x0 + 0.25 * (x1 - x0)) as f32, sb.y + sb.h / 2.0);
        let mut out = router.handle(&press(s, px, py, ButtonState::Pressed), &mut r);
        let (qx, qy) = drawn((x0 + 0.5 * (x1 - x0)) as f32, sb.y + sb.h / 2.0);
        out.extend(router.handle(&motion(s, qx, qy), &mut r));
        out.extend(router.handle(&press(s, qx, qy, ButtonState::Released), &mut r));
        let v: Vec<f32> = writes(&out, slider)
            .iter()
            .map(|v| v.as_number().unwrap())
            .collect();
        assert_eq!(v.len(), 3, "{scale:?}: {v:?}");
        for (got, want) in v.iter().zip([0.25, 0.5, 0.5]) {
            assert!((got - want).abs() < 0.01, "{scale:?}: {v:?}");
        }
        let gb = r.boxes(s).unwrap().rects[&seg];
        // The middle third, drawn: laid out, it is over "auto".
        let (x, y) = drawn(gb.x + gb.w * 0.5, gb.y + gb.h / 2.0);
        let mut out = router.handle(&press(s, x, y, ButtonState::Pressed), &mut r);
        out.extend(router.handle(&press(s, x, y, ButtonState::Released), &mut r));
        assert_eq!(writes(&out, seg), [kw("light")], "{scale:?}");
    }
}

/// `segmented { options: Look; value: <-> theme.look }`: a click writes
/// the option under it.
#[test]
fn a_segmented_click_writes_the_option() {
    let mut r = renderer();
    let mut b = Builder::default();
    let c = controls(&mut b);
    let (root, _buf) = show(&mut r, b.diff);
    let mut router = Router::new();
    let s = SurfaceId(1);
    router.attached(s, root);
    let sb = r.boxes(s).unwrap().rects[&c.segmented];
    let (x, y) = (sb.x + sb.w * 0.5, sb.y + sb.h / 2.0);
    let mut out = router.handle(&press(s, x, y, ButtonState::Pressed), &mut r);
    out.extend(router.handle(&press(s, x, y, ButtonState::Released), &mut r));
    assert_eq!(writes(&out, c.segmented), [kw("light")]);
}

fn key(s: SurfaceId, name: &str, typed: &str, m: Modifiers) -> InputEvent {
    InputEvent::Key {
        surface: s,
        key: KeyInput {
            name: name.into(),
            text: typed.into(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: m,
            time: 0,
            latin: None,
        },
    }
}

/// Typing goes in at the caret, arrows move it, Shift selects and typing
/// replaces the selection; a click places the caret under the pointer.
/// The caret moves on the frame the key arrives.
#[test]
fn text_is_edited_at_the_caret() {
    let mut r = renderer();
    let mut b = Builder::default();
    let i = inputs(&mut b);
    let (root, mut buf) = show(&mut r, b.diff);
    let mut router = Router::new();
    let s = SurfaceId(1);
    router.attached(s, root);
    let bf = r.boxes(s).unwrap().rects[&i.focused];
    let y = bf.y + bf.h / 2.0;
    // A click near the left edge focuses the input and puts the caret at
    // the start.
    let mut out = router.handle(&press(s, bf.x + 1.0, y, ButtonState::Pressed), &mut r);
    out.extend(router.handle(&press(s, bf.x + 1.0, y, ButtonState::Released), &mut r));
    assert!(out.contains(&Intent::Flag {
        node: i.focused,
        flag: Flag::Focused,
        on: true
    }));
    assert_eq!(InputScene::caret(&r, i.focused), Some(Caret::at(0)));
    let none = Modifiers::default();
    let shift = Modifiers {
        shift: true,
        ..none
    };
    let mut text_now = "firefox".to_string();
    let focused = i.focused;
    let mut type_key =
        |router: &mut Router, r: &mut Renderer, name: &str, typed: &str, m: Modifiers| {
            let out = router.handle(&key(s, name, typed, m), r);
            if let Some(PropValue::Text(t)) = writes(&out, focused).last() {
                // Logic answers: the scene shows the new text.
                text_now = t.clone();
                let mut d = SceneDiff::new();
                d.set(focused, Prop::Text, text(t));
                router.observe(&d);
                r.apply(d);
            }
            text_now.clone()
        };
    assert_eq!(type_key(&mut router, &mut r, "Right", "", none), "firefox");
    assert_eq!(type_key(&mut router, &mut r, "x", "x", none), "fxirefox");
    assert_eq!(InputScene::caret(&r, i.focused), Some(Caret::at(2)));
    type_key(&mut router, &mut r, "End", "", shift);
    assert_eq!(
        InputScene::caret(&r, i.focused),
        Some(Caret { pos: 8, anchor: 2 })
    );
    assert_eq!(type_key(&mut router, &mut r, "y", "y", none), "fxy");
    assert_eq!(type_key(&mut router, &mut r, "BackSpace", "", none), "fx");
    // A click past the end puts the caret at the end.
    router.handle(
        &press(s, bf.x + bf.w - 2.0, y, ButtonState::Pressed),
        &mut r,
    );
    router.handle(
        &press(s, bf.x + bf.w - 2.0, y, ButtonState::Released),
        &mut r,
    );
    assert_eq!(InputScene::caret(&r, i.focused), Some(Caret::at(2)));
    // The caret is drawn where it is now, with no logic round trip.
    let before = buf.pixels.clone();
    type_key(&mut router, &mut r, "Home", "", none);
    let d = buf.paint(&mut r, s, 1);
    assert!(!d.is_empty() && buf.pixels != before);
}

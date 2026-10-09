//! Input routing (`Router`): pointer and keyboard input become flags,
//! node events and two-way writes for logic.

use strand_render::{Flag, HitOnly, InputScene, Intent, NodeEvent, Renderer, Router, TextBackend};
use strand_scene::input::button;
use strand_scene::{
    AxisDelta, ButtonState, DropKind, DropPayload, InputEvent, KeyInput, LogicalPoint, NodeId,
    PaintTarget, Painter, Prop, PropValue, Scale, Size, SurfaceId,
};

/// A router and what it asked of logic so far.
#[derive(Default)]
struct R {
    router: Router,
    out: Vec<Intent>,
}

impl R {
    fn attached(&mut self, s: SurfaceId, n: NodeId) {
        self.router.attached(s, n);
    }
    fn input(&mut self, e: &InputEvent, scene: &mut dyn InputScene) {
        self.out.extend(self.router.handle(e, scene));
    }
    fn drain(&mut self) -> Vec<Intent> {
        std::mem::take(&mut self.out)
    }
}

/// Inside a surface the node under the pointer gets the input: its
/// whole chain is hovered (left nodes lose it), the chain under a
/// press is pressed and keeps `hover` until the release, and clicks
/// and scrolls go to the innermost node.
#[test]
fn input_goes_to_the_hit_node() {
    let mut f = R::default();
    let s = SurfaceId(1);
    let (root, row, a, b) = (
        NodeId::new(1, 0),
        NodeId::new(2, 0),
        NodeId::new(3, 0),
        NodeId::new(4, 0),
    );
    f.attached(s, root);
    // x < 10: over `a` in `row`; x < 20: over `b` in `row`; else bare.
    let hit = move |_: SurfaceId, p: LogicalPoint| {
        if p.x < 10.0 {
            vec![a, row, root]
        } else if p.x < 20.0 {
            vec![b, row, root]
        } else {
            vec![root]
        }
    };
    let at = |x| LogicalPoint::new(x, 1.0);
    let motion = |x| InputEvent::PointerMotion {
        surface: s,
        position: at(x),
        time: 0,
    };
    let button = |x, state| InputEvent::PointerButton {
        surface: s,
        position: at(x),
        button: button::LEFT,
        state,
        time: 0,
    };
    f.input(
        &InputEvent::PointerEnter {
            surface: s,
            position: at(5.0),
        },
        &mut HitOnly(hit),
    );
    f.input(&motion(15.0), &mut HitOnly(hit));
    f.input(&button(15.0, ButtonState::Pressed), &mut HitOnly(hit));
    // Dragging out keeps hover latched on the pressed chain.
    f.input(&motion(30.0), &mut HitOnly(hit));
    f.input(&button(30.0, ButtonState::Released), &mut HitOnly(hit));
    let flag = |node, flag, on| Intent::Flag { node, flag, on };
    use Flag::{Hover, Pressed};
    assert_eq!(
        f.drain(),
        vec![
            flag(root, Hover, true),
            flag(row, Hover, true),
            flag(a, Hover, true),
            flag(a, Hover, false),
            flag(b, Hover, true),
            flag(root, Pressed, true),
            flag(row, Pressed, true),
            flag(b, Pressed, true),
            flag(b, Pressed, false),
            flag(row, Pressed, false),
            flag(root, Pressed, false),
            flag(b, Hover, false),
            flag(row, Hover, false),
            Intent::Event {
                node: root,
                event: NodeEvent::Click
            },
        ]
    );
    // Pressed on `a`, released on `b`: their row is clicked, not `b`.
    f.input(&button(5.0, ButtonState::Pressed), &mut HitOnly(hit));
    f.input(&button(15.0, ButtonState::Released), &mut HitOnly(hit));
    let clicks: Vec<Intent> = f
        .drain()
        .into_iter()
        .filter(|m| matches!(m, Intent::Event { .. }))
        .collect();
    assert_eq!(
        clicks,
        [Intent::Event {
            node: row,
            event: NodeEvent::Click
        }]
    );
    // A release with no press (the press was on another surface):
    // no click.
    f.input(&button(5.0, ButtonState::Released), &mut HitOnly(hit));
    assert!(!f.drain().iter().any(|m| matches!(m, Intent::Event { .. })));
    // Scrolls go to the innermost node under the pointer, `dy` from
    // the vertical axis and `dx` from the horizontal one, in notches
    // (smooth scrolling: `WHEEL_STEP` pixels a notch).
    let scroll = |x, dy, dx| InputEvent::PointerAxis {
        surface: s,
        position: at(x),
        horizontal: AxisDelta {
            pixels: dx,
            value120: 0,
            stop: false,
        },
        vertical: AxisDelta {
            pixels: dy,
            value120: 0,
            stop: false,
        },
        source: None,
        time: 0,
    };
    f.input(&scroll(15.0, 45.0, -30.0), &mut HitOnly(hit));
    f.input(&scroll(5.0, -15.0, 0.0), &mut HitOnly(hit));
    f.input(&scroll(30.0, 0.0, 60.0), &mut HitOnly(hit));
    // Right clicks are `secondary` on the innermost node under both
    // the press and the release, like left clicks.
    let right = |x, state| InputEvent::PointerButton {
        surface: s,
        position: at(x),
        button: button::RIGHT,
        state,
        time: 0,
    };
    f.input(&right(15.0, ButtonState::Pressed), &mut HitOnly(hit));
    f.input(&right(15.0, ButtonState::Released), &mut HitOnly(hit));
    f.input(&right(5.0, ButtonState::Pressed), &mut HitOnly(hit));
    f.input(&right(15.0, ButtonState::Released), &mut HitOnly(hit));
    // A right release with no right press: nothing.
    f.input(&right(15.0, ButtonState::Released), &mut HitOnly(hit));
    let events: Vec<Intent> = f
        .drain()
        .into_iter()
        .filter(|m| matches!(m, Intent::Event { .. }))
        .collect();
    let event = |node, event| Intent::Event { node, event };
    assert_eq!(
        events,
        [
            event(b, NodeEvent::Scroll { dy: 3.0, dx: -2.0 }),
            event(a, NodeEvent::Scroll { dy: -1.0, dx: 0.0 }),
            event(root, NodeEvent::Scroll { dy: 0.0, dx: 4.0 }),
            event(b, NodeEvent::Secondary),
            event(row, NodeEvent::Secondary),
        ]
    );
}

/// Keyboard routing on a launcher-like panel: focus goes to the
/// `focus: true` input, typing writes its `text`, arrows move the
/// selection of the list its `nav` names, Return activates the
/// selected row, Escape and focus loss write `open: false`; a click on
/// a list row activates it.
#[test]
fn keys_go_to_the_focused_input_and_its_list() {
    use strand_scene::{Color, Modifiers, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, input, list) = (id(0), id(1), id(2), id(3));
    let rows = [id(4), id(5), id(6)];
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(120.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(
            panel,
            Prop::TwoWay,
            PropValue::List(vec![PropValue::Keyword("open".into())]),
        )
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(col, NodeKind::Col, Some(panel), 0)
        .create(input, NodeKind::Input, Some(col), 0)
        .set(input, Prop::Focus, PropValue::Bool(true))
        .set(input, Prop::Nav, PropValue::Node(list))
        .set(input, Prop::Text, PropValue::Text(String::new()))
        .create(list, NodeKind::List, Some(col), 1);
    for (i, row) in rows.iter().enumerate() {
        d.create(*row, NodeKind::Row, Some(list), i as u32).set(
            *row,
            Prop::Height,
            PropValue::Number(20.0),
        );
    }
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 120 * 4];
    let mut t = PaintTarget::new(&mut px, Size::new(200, 120), 800, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t);

    let mut f = R::default();
    f.attached(s, panel);
    let key = |name: &str, text: &str| InputEvent::Key {
        surface: s,
        key: KeyInput {
            name: name.into(),
            text: text.into(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers::default(),
            time: 0,
        },
    };
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    for k in [
        key("f", "f"),
        key("o", "o"),
        key("BackSpace", ""),
        key("Down", ""),
        key("Down", ""),
        key("Return", ""),
        key("Escape", ""),
    ] {
        f.input(&k, &mut r);
    }
    f.input(&InputEvent::KeyboardLeave { surface: s }, &mut r);
    let msgs: Vec<Intent> = f
        .drain()
        .into_iter()
        .filter(|m| {
            !matches!(
                m,
                Intent::Event {
                    event: NodeEvent::Key { .. },
                    ..
                }
            )
        })
        .collect();
    let flag = |node, flag, on| Intent::Flag { node, flag, on };
    let write = |node, prop, value| Intent::Write { node, prop, value };
    let text = |t: &str| write(input, Prop::Text, PropValue::Text(t.into()));
    let close = write(panel, Prop::Open, PropValue::Bool(false));
    assert_eq!(
        msgs,
        vec![
            flag(input, Flag::Focused, true),
            // The first row is selected at once: Return's row shows.
            flag(rows[0], Flag::Selected, true),
            text("f"),
            // Typed before logic answered: from what was written.
            text("fo"),
            text("f"),
            flag(rows[0], Flag::Selected, false),
            flag(rows[1], Flag::Selected, true),
            flag(rows[1], Flag::Selected, false),
            flag(rows[2], Flag::Selected, true),
            Intent::Event {
                node: rows[2],
                event: NodeEvent::Activate
            },
            close.clone(),
            flag(input, Flag::Focused, false),
            close,
        ]
    );
    // Focus back after a real leave (the launcher opened again): its
    // list starts at the top again, not at the row selected last time.
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    assert_eq!(f.router.selected(list), Some(rows[0]));
    // Keys reach `on key` on the focused node, as `key(k)`.
    f.input(&key("a", "a"), &mut r);
    assert!(f.drain().contains(&Intent::Event {
        node: input,
        event: NodeEvent::Key {
            name: "a".into(),
            text: "a".into(),
            modifiers: Modifiers::default()
        }
    }));
    // A click on a list row: clicked, selected and activated.
    let row = r.boxes(s).unwrap().rects[&rows[2]];
    let at = LogicalPoint::new(row.x + 5.0, row.y + 5.0);
    let button = |state| InputEvent::PointerButton {
        surface: s,
        position: at,
        button: button::LEFT,
        state,
        time: 0,
    };
    f.input(&button(ButtonState::Pressed), &mut r);
    f.input(&button(ButtonState::Released), &mut r);
    let msgs = f.drain();
    assert!(msgs.contains(&Intent::Event {
        node: rows[2],
        event: NodeEvent::Click
    }));
    // (Already selected by the arrows.)
    assert_eq!(f.router.selected(list), Some(rows[2]));
    assert!(msgs.contains(&Intent::Event {
        node: rows[2],
        event: NodeEvent::Activate
    }));
    // The click on the list the input steers left the typing there.
    assert!(!msgs.contains(&flag(list, Flag::Focused, true)));
    f.input(&key("b", "b"), &mut r);
    assert!(
        f.drain()
            .iter()
            .any(|m| matches!(m, Intent::Write { node, prop: Prop::Text, .. } if *node == input))
    );
    // Rows refilled (the selected one gone): once the diff is applied
    // the first row is selected again, and Down moves on from it.
    let mut d = SceneDiff::new();
    d.push(strand_scene::SceneOp::Remove {
        id: rows[2],
        window: false,
    });
    let fresh = id(7);
    d.create(fresh, NodeKind::Row, Some(list), 2)
        .set(fresh, Prop::Height, PropValue::Number(20.0));
    assert!(r.apply(d).is_empty());
    let msgs = f.router.settle(&mut r);
    assert_eq!(msgs, [flag(rows[0], Flag::Selected, true)]);
    assert_eq!(f.router.selected(list), Some(rows[0]));
    assert!(f.router.settle(&mut r).is_empty(), "settled");
    f.input(&key("Down", ""), &mut r);
    let msgs = f.drain();
    assert!(
        msgs.contains(&flag(rows[1], Flag::Selected, true)),
        "{msgs:?}"
    );
    // Results that arrive late for the same query (an `Async` search
    // re-ranking): the row Down moved to stays selected.
    let mut d = SceneDiff::new();
    let late = id(9);
    d.create(late, NodeKind::Row, Some(list), 0)
        .set(late, Prop::Height, PropValue::Number(20.0));
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(rows[1]));
    // New results for a new query (or `on show` clearing it): the first
    // row is selected again.
    let mut d = SceneDiff::new();
    let top = id(8);
    d.push(strand_scene::SceneOp::Remove {
        id: late,
        window: false,
    });
    d.create(top, NodeKind::Row, Some(list), 0)
        .set(top, Prop::Height, PropValue::Number(20.0))
        .set(input, Prop::Text, PropValue::Text("fb".into()));
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(top));
    // Rows changing under the same query with nothing moved by the user
    // follow the top.
    let mut d = SceneDiff::new();
    let first = id(10);
    d.create(first, NodeKind::Row, Some(list), 0)
        .set(first, Prop::Height, PropValue::Number(20.0));
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(first));
    // Arrows then move from it, and keep their row while rows stay.
    f.input(&key("Down", ""), &mut r);
    assert_eq!(f.router.selected(list), Some(top));
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(top));
}

/// The keyboard back on a surface without a leave first (a popup that
/// grabbed it closed) keeps the focus where a click had moved it; after
/// a real leave, focus starts again at the first `focus: true` node.
#[test]
fn focus_survives_a_popup_grab() {
    use strand_scene::{NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, first, second) = (id(0), id(1), id(2), id(3));
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(80.0))
        .create(col, NodeKind::Col, Some(panel), 0);
    for (i, n) in [first, second].into_iter().enumerate() {
        d.create(n, NodeKind::Input, Some(col), i as u32)
            .set(n, Prop::Height, PropValue::Number(30.0))
            .set(n, Prop::Text, PropValue::Text(String::new()));
    }
    d.set(first, Prop::Focus, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 80 * 4];
    let mut t = PaintTarget::new(&mut px, Size::new(200, 80), 800, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    assert_eq!(f.router.focused(s), Some(first));
    let b = r.boxes(s).unwrap().rects[&second];
    let at = LogicalPoint::new(b.x + 5.0, b.y + 5.0);
    for state in [ButtonState::Pressed, ButtonState::Released] {
        let e = InputEvent::PointerButton {
            surface: s,
            position: at,
            button: button::LEFT,
            state,
            time: 0,
        };
        f.input(&e, &mut r);
    }
    assert_eq!(f.router.focused(s), Some(second));
    f.drain();
    // A popup grabbed the keyboard and let it go: enter again, no leave.
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    assert_eq!(f.router.focused(s), Some(second));
    assert!(f.drain().is_empty(), "no focus moved");
    f.input(&InputEvent::KeyboardLeave { surface: s }, &mut r);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    assert_eq!(f.router.focused(s), Some(first));
}

/// A press on a surface's click-away catcher (`ClickAway`), Escape and
/// focus loss write `open: false` on a surface whose `open` is two-way,
/// and on nothing else: a one-way `open` is never written.
#[test]
fn click_away_closes_only_a_two_way_open() {
    use strand_scene::{NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let (two, one) = (NodeId::new(0, 0), NodeId::new(1, 0));
    let mut d = SceneDiff::new();
    for p in [two, one] {
        d.create(p, NodeKind::Panel, None, 0)
            .set(p, Prop::Size, PropValue::Number(50.0))
            .set(p, Prop::Open, PropValue::Bool(true));
    }
    d.set(
        two,
        Prop::TwoWay,
        PropValue::List(vec![PropValue::Keyword("open".into())]),
    );
    assert!(r.apply(d).is_empty());
    let (s2, s1) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    f.attached(s2, two);
    f.attached(s1, one);
    let close = |node| Intent::Write {
        node,
        prop: Prop::Open,
        value: PropValue::Bool(false),
    };
    f.input(&InputEvent::ClickAway { surface: s2 }, &mut r);
    assert_eq!(f.drain(), [close(two)]);
    f.input(&InputEvent::ClickAway { surface: s1 }, &mut r);
    f.input(&InputEvent::KeyboardEnter { surface: s1 }, &mut r);
    f.input(
        &InputEvent::Key {
            surface: s1,
            key: KeyInput {
                name: "Escape".into(),
                text: String::new(),
                state: ButtonState::Pressed,
                repeat: false,
                modifiers: Default::default(),
                time: 0,
            },
        },
        &mut r,
    );
    f.input(&InputEvent::KeyboardLeave { surface: s1 }, &mut r);
    assert!(
        !f.drain().contains(&close(one)),
        "a one-way open is not written"
    );
}

/// A left press on another Strand surface (the bar, which the launcher's
/// click-away catcher does not cover) closes every open `keyboard:
/// exclusive` surface with a two-way `open`, and still reaches the bar;
/// a surface with a one-way `open` or no exclusive keyboard stays open,
/// and a press on the launcher itself closes nothing.
#[test]
fn a_press_on_the_bar_closes_an_exclusive_launcher() {
    use strand_scene::{NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let (bar, launcher, oneway, toasts) = (
        NodeId::new(0, 0),
        NodeId::new(1, 0),
        NodeId::new(2, 0),
        NodeId::new(3, 0),
    );
    let two_way = PropValue::List(vec![PropValue::Keyword("open".into())]);
    let mut d = SceneDiff::new();
    d.create(bar, NodeKind::Bar, None, 0)
        .set(bar, Prop::Height, PropValue::Number(30.0));
    for (p, exclusive, two) in [
        (launcher, true, true),
        (oneway, true, false),
        (toasts, false, true),
    ] {
        d.create(p, NodeKind::Panel, None, 0)
            .set(p, Prop::Size, PropValue::Number(50.0))
            .set(p, Prop::Open, PropValue::Bool(true));
        if exclusive {
            d.set(p, Prop::Keyboard, PropValue::Keyword("exclusive".into()));
        }
        if two {
            d.set(p, Prop::TwoWay, two_way.clone());
        }
    }
    assert!(r.apply(d).is_empty());
    let mut f = R::default();
    for (i, n) in [bar, launcher, oneway, toasts].into_iter().enumerate() {
        f.attached(SurfaceId(i as u32 + 1), n);
    }
    let press = |s, state| InputEvent::PointerButton {
        surface: SurfaceId(s),
        position: LogicalPoint::new(5.0, 5.0),
        button: button::LEFT,
        state,
        time: 0,
    };
    let close = |node| Intent::Write {
        node,
        prop: Prop::Open,
        value: PropValue::Bool(false),
    };
    f.input(&press(2, ButtonState::Pressed), &mut r);
    f.input(&press(2, ButtonState::Released), &mut r);
    let out = f.drain();
    assert!(
        !out.iter().any(|i| matches!(i, Intent::Write { .. })),
        "{out:?}"
    );
    f.input(&press(1, ButtonState::Pressed), &mut r);
    f.input(&press(1, ButtonState::Released), &mut r);
    let out = f.drain();
    let writes: Vec<&Intent> = out
        .iter()
        .filter(|i| matches!(i, Intent::Write { .. }))
        .collect();
    assert_eq!(writes, [&close(launcher)]);
    assert!(
        out.contains(&Intent::Event {
            node: bar,
            event: NodeEvent::Click
        }),
        "the press still reaches the bar: {out:?}"
    );
}

/// Keys typed faster than logic answers build on the last write; logic
/// answering in order does not undo them, and a text of logic's own (a
/// handler clearing the query) wins over the writes in flight.
#[test]
fn logic_clearing_an_input_wins_over_edits_in_flight() {
    use strand_scene::{NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let (p, i) = (NodeId::new(0, 0), NodeId::new(1, 0));
    let mut d = SceneDiff::new();
    d.create(p, NodeKind::Panel, None, 0)
        .set(p, Prop::Size, PropValue::Number(80.0))
        .create(i, NodeKind::Input, Some(p), 0)
        .set(i, Prop::Text, PropValue::Text(String::new()))
        .set(i, Prop::Focus, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, p);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    let key = |f: &mut R, r: &mut Renderer, t: &str| {
        f.input(
            &InputEvent::Key {
                surface: s,
                key: KeyInput {
                    name: t.into(),
                    text: t.into(),
                    state: ButtonState::Pressed,
                    repeat: false,
                    modifiers: Default::default(),
                    time: 0,
                },
            },
            r,
        );
        f.drain()
            .into_iter()
            .find_map(|i| match i {
                Intent::Write {
                    value: PropValue::Text(t),
                    ..
                } => Some(t),
                _ => None,
            })
            .unwrap()
    };
    // Logic sets the text: the router sees the diff first.
    let logic = |f: &mut R, r: &mut Renderer, t: &str| {
        let mut d = SceneDiff::new();
        d.set(i, Prop::Text, PropValue::Text(t.into()));
        f.router.observe(&d);
        r.apply(d);
    };
    assert_eq!(key(&mut f, &mut r, "a"), "a");
    assert_eq!(
        key(&mut f, &mut r, "b"),
        "ab",
        "built on the write in flight"
    );
    logic(&mut f, &mut r, "a");
    assert_eq!(key(&mut f, &mut r, "c"), "abc", "an answer in order");
    logic(&mut f, &mut r, "ab");
    logic(&mut f, &mut r, "abc");
    // A handler clears the query (`query = ""`) within the in-flight
    // window: the next key builds on that.
    logic(&mut f, &mut r, "");
    assert_eq!(key(&mut f, &mut r, "d"), "d", "logic's own text wins");
}

/// An axis frame with no motion (a touchpad finger lifted: `axis_stop`
/// alone) delivers no `scroll`; detents alone scroll by wheel steps.
#[test]
fn an_empty_axis_frame_is_no_scroll() {
    let mut f = R::default();
    let s = SurfaceId(1);
    let root = NodeId::new(1, 0);
    f.attached(s, root);
    let hit = |_: SurfaceId, _: LogicalPoint| vec![NodeId::new(1, 0)];
    let axis = |vertical: AxisDelta| InputEvent::PointerAxis {
        surface: s,
        position: LogicalPoint::new(1.0, 1.0),
        horizontal: AxisDelta::default(),
        vertical,
        source: None,
        time: 0,
    };
    f.input(
        &axis(AxisDelta {
            stop: true,
            ..AxisDelta::default()
        }),
        &mut HitOnly(hit),
    );
    assert!(f.drain().is_empty());
    f.input(
        &axis(AxisDelta {
            value120: 120,
            ..AxisDelta::default()
        }),
        &mut HitOnly(hit),
    );
    assert_eq!(
        f.drain(),
        [Intent::Event {
            node: root,
            // One detent: `on scroll(dy)` counts notches.
            event: NodeEvent::Scroll { dy: 1.0, dx: 0.0 }
        }]
    );
}

/// (M4) `wl_data_device` drags reach the Router, which emits nothing for
/// them until S-lists builds drag and drop.
#[test]
fn drag_events_are_not_routed_yet() {
    let mut f = R::default();
    let s = SurfaceId(1);
    let root = NodeId::new(1, 0);
    f.attached(s, root);
    let hit = |_: SurfaceId, _: LogicalPoint| vec![NodeId::new(1, 0)];
    let at = LogicalPoint::new(4.0, 4.0);
    for e in [
        InputEvent::DragEnter {
            surface: s,
            at,
            kinds: vec![DropKind::Text],
        },
        InputEvent::DragMotion { surface: s, at },
        InputEvent::DragDrop {
            surface: s,
            at,
            payload: DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "hi".into(),
                app_id: None,
            },
        },
        InputEvent::DragLeave { surface: s },
    ] {
        f.input(&e, &mut HitOnly(hit));
    }
    assert!(f.drain().is_empty());
}

/// A focused `input` that logic removes loses focus at once, although
/// it still plays its exit pose (a ghost): keys no longer reach its id.
#[test]
fn a_removed_input_playing_its_exit_loses_focus_at_once() {
    use strand_scene::{Color, Modifiers, NodeKind, SceneDiff, SceneOp};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, input) = (id(0), id(1), id(2));
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(60.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(col, NodeKind::Col, Some(panel), 0)
        .create(input, NodeKind::Input, Some(col), 0)
        .set(input, Prop::Focus, PropValue::Bool(true))
        .set(input, Prop::Text, PropValue::Text(String::new()))
        .set(input, Prop::Exit, PropValue::Keyword("fade".into()));
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 60 * 4];
    // Painted with a clock: removals from now on play their exit.
    let t = PaintTarget::new(&mut px, Size::new(200, 60), 800, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    assert_eq!(f.router.focused(s), Some(input));
    f.drain();
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: input,
        window: false,
    });
    assert!(r.apply(d).is_empty());
    assert!(r.tree().is_ghost(input), "it plays its exit");
    let key = InputEvent::Key {
        surface: s,
        key: KeyInput {
            name: "a".into(),
            text: "a".into(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers::default(),
            time: 0,
        },
    };
    f.input(&key, &mut r);
    assert_eq!(f.router.focused(s), None, "a ghost keeps no focus");
    assert!(
        f.drain().iter().all(|m| match m {
            Intent::Write { node, .. } | Intent::Event { node, .. } => *node != input,
            _ => true,
        }),
        "nothing goes to the removed id"
    );
}

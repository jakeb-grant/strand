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

/// (M4) A scene for drag and drop on surface 1: a 200×240 panel holding
/// a `list` of four 40 px `drag: Pin` rows (logic's window from global
/// row 100) whose `on drop` takes `Pin`, and below it a 40 px box whose
/// `on drop` takes only other programs' `Drop`s. Painted once with a
/// clock, so hits and boxes are known.
fn dnd_scene() -> (Renderer, [NodeId; 7]) {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, list, bin) = (id(0), id(1), id(2), id(3));
    let rows = [id(10), id(11), id(12), id(13)];
    let kw = |k: &str| PropValue::Keyword(k.into());
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(240.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(col, NodeKind::Col, Some(panel), 0)
        .create(list, NodeKind::List, Some(col), 0)
        .set(list, Prop::Height, PropValue::Number(160.0))
        .set(list, Prop::RowFirst, PropValue::Number(100.0))
        .set(list, Prop::RowCount, PropValue::Number(2000.0))
        .set(list, Prop::Accepts, PropValue::List(vec![kw("Pin")]))
        .create(bin, NodeKind::Box, Some(col), 1)
        .set(bin, Prop::Height, PropValue::Number(40.0))
        .set(bin, Prop::Width, PropValue::Number(200.0))
        .set(bin, Prop::Accepts, PropValue::List(vec![kw("Drop")]));
    for (i, row) in rows.iter().enumerate() {
        d.create(*row, NodeKind::Row, Some(list), i as u32)
            .set(*row, Prop::Height, PropValue::Number(40.0))
            .set(*row, Prop::Drag, kw("Pin"));
    }
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 240 * 4];
    let t = PaintTarget::new(&mut px, Size::new(200, 240), 800, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    (r, [panel, col, list, bin, rows[0], rows[1], rows[2]])
}

fn motion(s: SurfaceId, x: f32, y: f32, time: u32) -> InputEvent {
    InputEvent::PointerMotion {
        surface: s,
        position: LogicalPoint::new(x, y),
        time,
    }
}

fn left(s: SurfaceId, x: f32, y: f32, state: ButtonState) -> InputEvent {
    InputEvent::PointerButton {
        surface: s,
        position: LogicalPoint::new(x, y),
        button: button::LEFT,
        state,
        time: 0,
    }
}

fn events(out: Vec<Intent>) -> Vec<(NodeId, NodeEvent)> {
    out.into_iter()
        .filter_map(|i| match i {
            Intent::Event { node, event } => Some((node, event)),
            _ => None,
        })
        .collect()
}

/// (M4) A press on a `drag:` source is a press until the pointer has
/// moved 6 px (a 5 px wobble still clicks); then it is a drag: the
/// source follows the pointer, `Router::drag` says where it would land
/// (the `list` whose `on drop` takes `Pin`, at a global row index: past
/// two of the other rows' middles, so row 100 + 2), and the release
/// drops it there (`on drop(p, at)` on the list) and clicks nothing. A
/// type the target does not take, or no target at all, springs back
/// with no drop.
#[test]
fn a_drag_past_six_pixels_drops_at_a_global_index() {
    let (mut r, [panel, _, list, _, row0, row1, _]) = dnd_scene();
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&motion(s, 50.0, 20.0, 0), &mut r);
    // A 5 px wobble is still a click.
    f.input(&left(s, 50.0, 20.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 53.0, 24.0, 10), &mut r);
    assert_eq!(f.router.drag(), None);
    f.input(&left(s, 53.0, 24.0, ButtonState::Released), &mut r);
    assert!(
        events(f.drain()).contains(&(row0, NodeEvent::Click)),
        "a press that moved under 6 px clicks"
    );
    // Past 6 px: a drag of row 0 (global 100).
    f.input(&left(s, 50.0, 20.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 27.0, 20), &mut r);
    let d = f.router.drag().expect("a drag");
    assert_eq!((d.source, d.surface, d.target), (row0, s, Some(list)));
    // Rows 1..3 have their middles at 60, 100 and 140: at y 105 it
    // lands after two of them.
    f.input(&motion(s, 50.0, 105.0, 40), &mut r);
    let d = f.router.drag().unwrap();
    assert_eq!((d.target, d.index), (Some(list), Some(102)));
    assert!(d.velocity.y > 0.0, "moving down: {:?}", d.velocity);
    assert_eq!(f.router.drop_target(s), Some(list));
    f.input(&left(s, 50.0, 105.0, ButtonState::Released), &mut r);
    assert_eq!(
        events(f.drain()),
        [(
            list,
            NodeEvent::Drop {
                payload: DropPayload::Node(row0),
                at: 102
            }
        )],
        "dropped, and no click"
    );
    assert_eq!(f.router.drag(), None);
    // Over the box that takes only `Drop`s: no target, no drop.
    f.input(&left(s, 50.0, 60.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 200.0, 50), &mut r);
    let d = f.router.drag().unwrap();
    assert_eq!((d.source, d.target, d.index), (row1, None, None));
    assert_eq!(f.router.drop_target(s), None);
    f.input(&left(s, 50.0, 200.0, ButtonState::Released), &mut r);
    assert!(
        events(f.drain()).is_empty(),
        "sprang back, no drop, no click"
    );
    // A row dropped back on its own place lands at its own index.
    f.input(&left(s, 50.0, 60.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 70.0, 60), &mut r);
    f.input(&left(s, 50.0, 70.0, ButtonState::Released), &mut r);
    assert_eq!(
        events(f.drain()),
        [(
            list,
            NodeEvent::Drop {
                payload: DropPayload::Node(row1),
                at: 101
            }
        )]
    );
}

/// (M4) A windowed list mounts rows beyond its view (logic's window
/// overscans), and only the ones in view are laid out: twenty 40 px
/// `drag: Pin` rows from global row 100 in a 160 px list, scrolled so
/// rows 8..11 are shown. Painted again after the scroll.
fn long_dnd_scene() -> (Renderer, NodeId, NodeId, Vec<NodeId>) {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, list) = (id(0), id(1), id(2));
    let rows: Vec<NodeId> = (0..20).map(|i| id(10 + i)).collect();
    let kw = |k: &str| PropValue::Keyword(k.into());
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(240.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(col, NodeKind::Col, Some(panel), 0)
        .create(list, NodeKind::List, Some(col), 0)
        .set(list, Prop::Height, PropValue::Number(160.0))
        .set(list, Prop::RowFirst, PropValue::Number(100.0))
        .set(list, Prop::RowCount, PropValue::Number(2000.0))
        .set(list, Prop::Accepts, PropValue::List(vec![kw("Pin")]));
    for (i, row) in rows.iter().enumerate() {
        d.create(*row, NodeKind::Row, Some(list), i as u32)
            .set(*row, Prop::Height, PropValue::Number(40.0))
            .set(*row, Prop::Drag, kw("Pin"));
    }
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 240 * 4];
    let mut paint = |r: &mut Renderer, secs| {
        let t = PaintTarget::new(&mut px, Size::new(200, 240), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(secs)));
    };
    paint(&mut r, 1);
    // Global row 100 starts at 4,000 px; row 8 of the window at 4,320.
    assert_eq!(
        r.scroll(s, LogicalPoint::new(50.0, 80.0), 4320.0),
        Some(list)
    );
    paint(&mut r, 2);
    (r, panel, list, rows)
}

/// (M4) A drop on a windowed list whose mounted rows run past its view
/// above and below (so most of them have no box) still lands at the
/// global index among all of them: rows 0..6 and 13..19 are mounted but
/// not laid out, and a drop counts them by their place.
#[test]
fn a_drop_counts_mounted_rows_out_of_view() {
    let (mut r, panel, list, rows) = long_dnd_scene();
    let s = SurfaceId(1);
    assert_eq!(r.scroll_offset(list), Some(4320.0));
    let laid: Vec<usize> = (0..rows.len())
        .filter(|i| r.node_rect(s, rows[*i]).is_some())
        .collect();
    assert_eq!(laid, (7..=12).collect::<Vec<_>>(), "rows in view, overscan");
    let mut f = R::default();
    f.attached(s, panel);
    // Row 10 is shown from y 80 to 120; drag it.
    f.input(&motion(s, 50.0, 100.0, 0), &mut r);
    f.input(&left(s, 50.0, 100.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 110.0, 10), &mut r);
    let d = f.router.drag().expect("a drag");
    assert_eq!((d.source, d.target), (rows[10], Some(list)));
    // At y 30: past row 8's middle (y 20), before row 9's (y 60).
    f.input(&motion(s, 50.0, 30.0, 20), &mut r);
    assert_eq!(f.router.drag().unwrap().index, Some(109));
    // At y 150: past row 11's middle (y 140, without row 10 at its old
    // place), before row 12's: lands at 11 without the source.
    f.input(&motion(s, 50.0, 150.0, 30), &mut r);
    assert_eq!(f.router.drag().unwrap().index, Some(111));
    f.input(&left(s, 50.0, 150.0, ButtonState::Released), &mut r);
    assert_eq!(
        events(f.drain()),
        [(
            list,
            NodeEvent::Drop {
                payload: DropPayload::Node(rows[10]),
                at: 111
            }
        )]
    );
    // At the very top, before row 8's middle: lands at 8, above which
    // eight mounted rows are counted though only row 7 has a box.
    f.input(&left(s, 50.0, 60.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 2.0, 40), &mut r);
    let d = f.router.drag().unwrap();
    assert_eq!((d.source, d.index), (rows[9], Some(108)));
    f.input(&left(s, 50.0, 2.0, ButtonState::Released), &mut r);
    assert_eq!(
        events(f.drain()),
        [(
            list,
            NodeEvent::Drop {
                payload: DropPayload::Node(rows[9]),
                at: 108
            }
        )]
    );
}

/// (M4) Escape cancels a drag in flight: the source springs back, no
/// drop, no click on the release, and the key does nothing else (it is
/// not delivered, and an open surface stays open).
#[test]
fn escape_cancels_a_drag() {
    use strand_scene::Modifiers;
    let (mut r, [panel, _, _, _, row0, _, _]) = dnd_scene();
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    f.input(&motion(s, 50.0, 20.0, 0), &mut r);
    f.input(&left(s, 50.0, 20.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 90.0, 10), &mut r);
    assert_eq!(f.router.drag().map(|d| d.source), Some(row0));
    f.drain();
    let escape = InputEvent::Key {
        surface: s,
        key: KeyInput {
            name: "Escape".into(),
            text: String::new(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers::default(),
            time: 0,
        },
    };
    f.input(&escape, &mut r);
    assert_eq!(f.router.drag(), None);
    assert!(f.drain().is_empty(), "Escape only cancelled the drag");
    f.input(&left(s, 50.0, 90.0, ButtonState::Released), &mut r);
    assert!(events(f.drain()).is_empty(), "no drop and no click");
}

/// (M4) Other programs' drags (`wl_data_device`): files over the box
/// whose `on drop` takes `Drop` have a target there (the surface manager
/// accepts the offer then) and none over the `Pin` list; dropped on the
/// box they are its `on drop`, at the box's place among its parent's
/// rows. Leaving forgets the offer. A scene with no tree (nothing takes
/// anything) gets no drop.
#[test]
fn other_programs_drops_reach_the_target_that_takes_drop() {
    let (mut r, [panel, _, _, bin, ..]) = dnd_scene();
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, panel);
    let at = |y| LogicalPoint::new(50.0, y);
    f.input(
        &InputEvent::DragEnter {
            surface: s,
            at: at(60.0),
            kinds: vec![DropKind::Files],
        },
        &mut r,
    );
    assert_eq!(f.router.drop_target(s), None, "the list takes only Pins");
    assert_eq!(f.router.drag(), None, "not a drag of ours");
    f.input(
        &InputEvent::DragMotion {
            surface: s,
            at: at(190.0),
        },
        &mut r,
    );
    assert_eq!(f.router.drop_target(s), Some(bin));
    let payload = DropPayload::External {
        kind: DropKind::Files,
        files: vec!["/tmp/a.png".into()],
        text: String::new(),
        app_id: None,
    };
    f.input(
        &InputEvent::DragDrop {
            surface: s,
            at: at(190.0),
            payload: payload.clone(),
        },
        &mut r,
    );
    // The box is its column's second row; past its middle (180): 2.
    assert_eq!(
        events(f.drain()),
        [(bin, NodeEvent::Drop { payload, at: 2 })]
    );
    assert_eq!(f.router.drop_target(s), None);
    f.input(
        &InputEvent::DragEnter {
            surface: s,
            at: at(190.0),
            kinds: vec![DropKind::Text],
        },
        &mut r,
    );
    assert_eq!(f.router.drop_target(s), Some(bin));
    f.input(&InputEvent::DragLeave { surface: s }, &mut r);
    assert_eq!(f.router.drop_target(s), None);
    // No tree: nothing to take it.
    let mut g = R::default();
    g.attached(s, panel);
    let hit = |_: SurfaceId, _: LogicalPoint| vec![NodeId::new(0, 0)];
    for e in [
        InputEvent::DragEnter {
            surface: s,
            at: at(4.0),
            kinds: vec![DropKind::Text],
        },
        InputEvent::DragDrop {
            surface: s,
            at: at(4.0),
            payload: DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "hi".into(),
                app_id: None,
            },
        },
    ] {
        g.input(&e, &mut HitOnly(hit));
    }
    assert!(g.drain().is_empty());
}

/// (M4) A dock: a 240×60 panel holding a `row` whose `on drop` takes
/// `Pin`, of four 60 px `drag: Pin` items whose own `on drop` takes
/// other programs' `Drop`s, each a `col` holding an icon (20 px) and a
/// label (20 px). Painted once, so hits and boxes are known.
fn dock_scene() -> (Renderer, NodeId, NodeId, Vec<NodeId>) {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, dock) = (id(0), id(1));
    let items: Vec<NodeId> = (0..4).map(|i| id(10 + i)).collect();
    let kw = |k: &str| PropValue::Keyword(k.into());
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(240.0))
        .set(panel, Prop::Height, PropValue::Number(60.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(dock, NodeKind::Row, Some(panel), 0)
        .set(dock, Prop::Accepts, PropValue::List(vec![kw("Pin")]));
    for (i, item) in items.iter().enumerate() {
        d.create(*item, NodeKind::Col, Some(dock), i as u32)
            .set(*item, Prop::Width, PropValue::Number(60.0))
            .set(*item, Prop::Height, PropValue::Number(60.0))
            .set(*item, Prop::Drag, kw("Pin"))
            .set(*item, Prop::Accepts, PropValue::List(vec![kw("Drop")]));
        for (j, h) in [20.0, 20.0].into_iter().enumerate() {
            let c = id(100 + 2 * i as u32 + j as u32);
            d.create(c, NodeKind::Box, Some(*item), j as u32)
                .set(c, Prop::Width, PropValue::Number(40.0))
                .set(c, Prop::Height, PropValue::Number(h));
        }
    }
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 240 * 60 * 4];
    let t = PaintTarget::new(&mut px, Size::new(240, 60), 960, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    (r, panel, dock, items)
}

/// (M4) A per-item `on drop` on a dock item that holds an icon and a
/// label is placed among the dock's items, not among its own content:
/// files dropped on item 1 (x 60..120) past its middle land at 2, before
/// it at 1, though the pointer is over its label (its own second child).
/// The dock itself, whose children are `drag:` items, holds rows: item
/// 0 dragged to x 200 lands before item 3 (index 2 without the source).
#[test]
fn a_per_item_drop_target_with_content_is_placed_among_its_siblings() {
    let (mut r, panel, dock, items) = dock_scene();
    let s = SurfaceId(1);
    assert_eq!(
        r.node_rect(s, items[1]).map(|b| (b.x, b.w)),
        Some((60.0, 60.0))
    );
    let mut f = R::default();
    f.attached(s, panel);
    for (x, at) in [(100.0, 2), (70.0, 1)] {
        let p = LogicalPoint::new(x, 30.0);
        f.input(
            &InputEvent::DragEnter {
                surface: s,
                at: p,
                kinds: vec![DropKind::Files],
            },
            &mut r,
        );
        assert_eq!(f.router.drop_target(s), Some(items[1]));
        let payload = DropPayload::External {
            kind: DropKind::Files,
            files: vec!["/tmp/a.png".into()],
            text: String::new(),
            app_id: None,
        };
        f.input(
            &InputEvent::DragDrop {
                surface: s,
                at: p,
                payload: payload.clone(),
            },
            &mut r,
        );
        assert_eq!(
            events(f.drain()),
            [(items[1], NodeEvent::Drop { payload, at })]
        );
    }
    f.input(&motion(s, 30.0, 30.0, 0), &mut r);
    f.input(&left(s, 30.0, 30.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 200.0, 30.0, 10), &mut r);
    let d = f.router.drag().expect("a drag");
    assert_eq!(
        (d.source, d.target, d.index),
        (items[0], Some(dock), Some(2))
    );
    f.input(&left(s, 200.0, 30.0, ButtonState::Released), &mut r);
    assert_eq!(
        events(f.drain()),
        [(
            dock,
            NodeEvent::Drop {
                payload: DropPayload::Node(items[0]),
                at: 2
            }
        )]
    );
}

/// (M4) A plain container of items that are not `drag:` sources (a
/// `for` of labels under an `on drop`) holds rows once the compiler says
/// its direct child is a `for` (`Prop::DropRows`): text dropped at x 100
/// lands at 2 among its four 60 px items, before item 1 at 1. Without
/// the prop the row is itself a row of its panel (index 0 or 1 there).
#[test]
fn a_container_of_plain_for_items_places_drops_among_them() {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let place = |drop_rows: bool| {
        let data = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(data),
        ]));
        let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
        let id = |i| NodeId::new(i, 0);
        let (panel, row) = (id(0), id(1));
        let mut d = SceneDiff::new();
        d.create(panel, NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, PropValue::Number(240.0))
            .set(panel, Prop::Height, PropValue::Number(60.0))
            .set(panel, Prop::Open, PropValue::Bool(true))
            .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(row, NodeKind::Row, Some(panel), 0)
            .set(
                row,
                Prop::Accepts,
                PropValue::List(vec![PropValue::Keyword("Drop".into())]),
            );
        if drop_rows {
            d.set(row, Prop::DropRows, PropValue::Bool(true));
        }
        for i in 0..4 {
            let c = id(10 + i);
            d.create(c, NodeKind::Box, Some(row), i)
                .set(c, Prop::Width, PropValue::Number(60.0))
                .set(c, Prop::Height, PropValue::Number(60.0));
        }
        assert!(r.apply(d).is_empty());
        let s = SurfaceId(1);
        r.attach_surface(s, panel);
        let mut px = vec![0u8; 240 * 60 * 4];
        let t = PaintTarget::new(&mut px, Size::new(240, 60), 960, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
        let mut f = R::default();
        f.attached(s, panel);
        [100.0, 70.0]
            .into_iter()
            .map(|x| {
                let p = LogicalPoint::new(x, 30.0);
                f.input(
                    &InputEvent::DragEnter {
                        surface: s,
                        at: p,
                        kinds: vec![DropKind::Text],
                    },
                    &mut r,
                );
                assert_eq!(f.router.drop_target(s), Some(row));
                f.input(
                    &InputEvent::DragDrop {
                        surface: s,
                        at: p,
                        payload: DropPayload::External {
                            kind: DropKind::Text,
                            files: vec![],
                            text: "hi".into(),
                            app_id: None,
                        },
                    },
                    &mut r,
                );
                match events(f.drain()).as_slice() {
                    [(n, NodeEvent::Drop { at, .. })] if *n == row => *at,
                    other => panic!("{other:?}"),
                }
            })
            .collect::<Vec<u32>>()
    };
    assert_eq!(place(true), [2, 1]);
    assert!(place(false).iter().all(|at| *at <= 1), "a row of its panel");
}

/// (M4) A drag that leaves its surface with the button held is carried
/// by the compositor (`wl_data_device`): the Router keeps it (its source
/// back in its box meanwhile) and it continues as `Drag*` events with no
/// kinds (our own) on another Strand surface, where it drops as the
/// source's node; a cancelled one (the surface manager's synthetic
/// release on the origin) drops nothing.
#[test]
fn a_drag_carried_to_another_surface_drops_there() {
    let (mut r, [panel, _, list, _, row0, row1, _]) = dnd_scene();
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&motion(s, 50.0, 20.0, 0), &mut r);
    f.input(&left(s, 50.0, 20.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 40.0, 10), &mut r);
    f.input(&InputEvent::PointerLeave { surface: s }, &mut r);
    let d = f.router.drag().expect("still a drag");
    assert_eq!((d.source, d.target), (row0, None));
    f.drain();
    // Back over the panel through the data device (standing in for a
    // second surface: the same tree), dropped after row 1's middle.
    f.input(
        &InputEvent::DragEnter {
            surface: s,
            at: LogicalPoint::new(50.0, 30.0),
            kinds: vec![],
        },
        &mut r,
    );
    f.input(
        &InputEvent::DragMotion {
            surface: s,
            at: LogicalPoint::new(50.0, 75.0),
        },
        &mut r,
    );
    assert_eq!(f.router.drop_target(s), Some(list));
    f.input(
        &InputEvent::DragDrop {
            surface: s,
            at: LogicalPoint::new(50.0, 75.0),
            payload: DropPayload::Node(row0),
        },
        &mut r,
    );
    assert_eq!(
        events(f.drain()),
        [(
            list,
            NodeEvent::Drop {
                payload: DropPayload::Node(row0),
                at: 101
            }
        )]
    );
    assert_eq!(f.router.drag(), None);
    // Carried off and cancelled: the release the surface manager makes
    // up on the origin drops nothing.
    f.input(&motion(s, 50.0, 60.0, 20), &mut r);
    f.input(&left(s, 50.0, 60.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s, 50.0, 80.0, 30), &mut r);
    assert_eq!(f.router.drag().map(|d| d.source), Some(row1));
    f.input(&InputEvent::PointerLeave { surface: s }, &mut r);
    f.input(&left(s, -1e4, -1e4, ButtonState::Released), &mut r);
    assert_eq!(f.router.drag(), None);
    assert!(events(f.drain()).is_empty());
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

/// A virtualised list (logic mounted 32 of its 2,000 rows) steered by
/// an `input`'s `nav`: keys select by global index, past the mounted
/// rows too. A row not mounted is scrolled to and the selection lands
/// when logic mounts it (Return pressed meanwhile activates it then);
/// Home, End, Page_Up and Page_Down move by the list's whole length and
/// by the rows in view. A selected row the window unmounts (the list
/// scrolled away from it) stays selected by its index and is selected
/// again when it comes back, with no first-row reselection in between.
#[test]
fn nav_selects_rows_beyond_the_mounted_window() {
    use strand_render::widgets::Caret;
    use strand_scene::{Color, Modifiers, NodeKind, SceneDiff, SceneOp};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, input, list) = (id(0), id(1), id(2), id(3));
    // Row `i` of the data is node 100 + i.
    let row = |i: u32| id(100 + i);
    let mount = |d: &mut SceneDiff, first: u32, window: bool| {
        for i in first..first + 32 {
            d.push(SceneOp::Create {
                id: row(i),
                kind: NodeKind::Row,
                parent: Some(list),
                index: i - first,
                window,
            });
            d.set(row(i), Prop::Height, PropValue::Number(20.0));
        }
        d.set(list, Prop::RowFirst, PropValue::Number(first as f32));
    };
    let unmount = |d: &mut SceneDiff, first: u32| {
        for i in first..first + 32 {
            d.push(SceneOp::Remove {
                id: row(i),
                window: true,
            });
        }
    };
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(130.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(col, NodeKind::Col, Some(panel), 0)
        .create(input, NodeKind::Input, Some(col), 0)
        .set(input, Prop::Focus, PropValue::Bool(true))
        .set(input, Prop::Nav, PropValue::Node(list))
        .set(input, Prop::Text, PropValue::Text(String::new()))
        .create(list, NodeKind::List, Some(col), 1)
        .set(list, Prop::Height, PropValue::Number(100.0))
        .set(list, Prop::RowCount, PropValue::Number(2000.0));
    mount(&mut d, 0, false);
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 130 * 4];
    let mut paint = |r: &mut Renderer| {
        let mut t = PaintTarget::new(&mut px, Size::new(200, 130), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t);
    };
    paint(&mut r);
    let mut f = R::default();
    f.attached(s, panel);
    let key_with = |name: &str, modifiers: Modifiers| InputEvent::Key {
        surface: s,
        key: KeyInput {
            name: name.into(),
            text: String::new(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers,
            time: 0,
        },
    };
    let key = |name: &str| key_with(name, Modifiers::default());
    // In the `input`, Ctrl+Home and Ctrl+End go to the list's ends.
    let ctrl = |name: &str| {
        key_with(
            name,
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        )
    };
    let shift = |name: &str| {
        key_with(
            name,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        )
    };
    let flag = |node, on| Intent::Flag {
        node,
        flag: Flag::Selected,
        on,
    };
    let activate = |node| Intent::Event {
        node,
        event: NodeEvent::Activate,
    };
    let picked = |msgs: Vec<Intent>| -> Vec<Intent> {
        msgs.into_iter()
            .filter(|m| {
                matches!(
                    m,
                    Intent::Flag {
                        flag: Flag::Selected,
                        ..
                    } | Intent::Event {
                        event: NodeEvent::Activate,
                        ..
                    }
                )
            })
            .collect()
    };
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    assert_eq!(f.router.selected(list), Some(row(0)));
    assert_eq!(r.rows_in_view(list), Some(5));
    f.drain();

    // Page_Down moves by the rows in view, within the mounted rows.
    f.input(&key("Page_Down"), &mut r);
    assert_eq!(picked(f.drain()), [flag(row(0), false), flag(row(5), true)]);
    assert_eq!(f.router.selected_index(list), Some(5));

    // Plain End in the input moves its caret, not the selection.
    f.input(&key("End"), &mut r);
    assert!(picked(f.drain()).is_empty());
    assert_eq!(f.router.selected_index(list), Some(5));

    // Ctrl+End: row 1,999 is not mounted. The selection leaves row 5 and is
    // on its way; the list scrolls there and asks logic for that window.
    f.input(&ctrl("End"), &mut r);
    assert_eq!(picked(f.drain()), [flag(row(5), false)]);
    assert_eq!(f.router.selected(list), None);
    assert_eq!(f.router.selected_index(list), Some(1999));
    paint(&mut r);
    let asked = r.take_list_windows();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].1.contains(&1999), "{asked:?}");
    // Return before it lands: nothing yet, and settling with nothing
    // mounted keeps waiting (no first-row selection).
    f.input(&key("Return"), &mut r);
    assert!(picked(f.drain()).is_empty());
    assert!(picked(f.router.settle(&mut r)).is_empty());
    assert_eq!(f.router.selected(list), None);

    // Logic answers with rows 1,968..2,000: the selection lands on row
    // 1,999 and Return's activate goes to it.
    let mut d = SceneDiff::new();
    unmount(&mut d, 0);
    mount(&mut d, 1968, true);
    f.router.observe(&d);
    assert!(r.apply(d).is_empty());
    assert_eq!(
        picked(f.router.settle(&mut r)),
        [flag(row(1999), true), activate(row(1999))]
    );
    assert_eq!(f.router.selected(list), Some(row(1999)));
    paint(&mut r);
    let shown = r.scroll_offset(list).unwrap();
    assert!(
        (shown - (2000.0 * 20.0 - 100.0)).abs() < 0.6,
        "the view at the end: {shown}"
    );
    // Up: a mounted row, selected at once; Return activates it.
    f.input(&key("Up"), &mut r);
    f.input(&key("Return"), &mut r);
    assert_eq!(
        picked(f.drain()),
        [
            flag(row(1999), false),
            flag(row(1998), true),
            activate(row(1998))
        ]
    );

    // The list scrolls back to the top on its own (a wheel): the window
    // unmounts the selected row. It stays selected by index, and the
    // first row is not selected in its place.
    let mut d = SceneDiff::new();
    unmount(&mut d, 1968);
    mount(&mut d, 0, true);
    f.router.observe(&d);
    assert!(r.apply(d).is_empty());
    assert!(picked(f.router.settle(&mut r)).is_empty());
    assert_eq!(f.router.selected(list), None);
    assert_eq!(f.router.selected_index(list), Some(1998));
    // Back down: selected again where it was, and not activated.
    let mut d = SceneDiff::new();
    unmount(&mut d, 0);
    mount(&mut d, 1968, true);
    f.router.observe(&d);
    assert!(r.apply(d).is_empty());
    assert_eq!(picked(f.router.settle(&mut r)), [flag(row(1998), true)]);
    // Ctrl+Home goes to row 0 (not mounted) and Down moves on from where it
    // is heading: row 1.
    f.input(&ctrl("Home"), &mut r);
    f.input(&key("Down"), &mut r);
    assert_eq!(f.router.selected_index(list), Some(1));
    let mut d = SceneDiff::new();
    unmount(&mut d, 1968);
    mount(&mut d, 0, true);
    f.router.observe(&d);
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(row(1)));
    // A new query starts the results over at the top, even while a
    // selection is on its way.
    f.input(&ctrl("End"), &mut r);
    let mut d = SceneDiff::new();
    d.set(input, Prop::Text, PropValue::Text("q".into()));
    f.router.observe(&d);
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(row(0)));
    assert_eq!(f.router.selected_index(list), Some(0));

    // Home, End and Shift with them edit the query's caret, as in any
    // `input`, and leave the selection where it is.
    let mut d = SceneDiff::new();
    d.set(input, Prop::Text, PropValue::Text("abc".into()));
    f.router.observe(&d);
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    f.drain();
    f.input(&key("Home"), &mut r);
    assert_eq!(InputScene::caret(&r, input), Some(Caret::at(0)));
    f.input(&shift("End"), &mut r);
    assert_eq!(
        InputScene::caret(&r, input),
        Some(Caret { pos: 3, anchor: 0 })
    );
    f.input(&key("KP_End"), &mut r);
    assert_eq!(InputScene::caret(&r, input), Some(Caret::at(3)));
    f.input(&shift("Home"), &mut r);
    assert_eq!(
        InputScene::caret(&r, input),
        Some(Caret { pos: 0, anchor: 3 })
    );
    assert!(picked(f.drain()).is_empty());
    assert_eq!(f.router.selected_index(list), Some(0));
}

/// Router hooks (architecture.md, "Router hooks"): the last pointer
/// position per surface, no drag in flight before drag and drop lands,
/// and submenu keys: Right on a row holding a closed popup with a
/// two-way `open` opens it, Left in a popup opened from another popup
/// closes it (Escape too); Left in a top-level popup does nothing.
#[test]
fn router_hooks_pointer_drag_and_submenu_keys() {
    use strand_scene::{Modifiers, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (menu, list, plain, parent, sub, item) = (id(0), id(1), id(2), id(3), id(4), id(5));
    let two_way = PropValue::List(vec![PropValue::Keyword("open".into())]);
    let mut d = SceneDiff::new();
    d.create(menu, NodeKind::Popup, None, 0)
        .set(menu, Prop::Open, PropValue::Bool(true))
        .set(menu, Prop::TwoWay, two_way.clone())
        .create(list, NodeKind::List, Some(menu), 0)
        .set(list, Prop::Focus, PropValue::Bool(true))
        .create(plain, NodeKind::Row, Some(list), 0)
        .set(plain, Prop::Height, PropValue::Number(20.0))
        .create(parent, NodeKind::Row, Some(list), 1)
        .set(parent, Prop::Height, PropValue::Number(20.0))
        .create(sub, NodeKind::Popup, Some(parent), 0)
        .set(sub, Prop::Open, PropValue::Bool(false))
        .set(sub, Prop::TwoWay, two_way)
        .create(item, NodeKind::Row, Some(sub), 0)
        .set(item, Prop::Focus, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    let (s, s2) = (SurfaceId(1), SurfaceId(2));
    r.attach_surface(s, menu);
    let mut px = vec![0u8; 100 * 60 * 4];
    let mut t = PaintTarget::new(&mut px, Size::new(100, 60), 400, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t);
    let mut f = R::default();
    f.attached(s, menu);
    f.attached(s2, sub);

    // The pointer.
    assert_eq!(f.router.pointer(s), None);
    let at = LogicalPoint::new(12.0, 7.0);
    f.input(
        &InputEvent::PointerMotion {
            surface: s,
            position: at,
            time: 0,
        },
        &mut r,
    );
    assert_eq!(f.router.pointer(s), Some(at));
    assert_eq!(f.router.pointer(s2), None);
    assert_eq!(f.router.drag(), None);
    f.input(&InputEvent::PointerLeave { surface: s }, &mut r);
    assert_eq!(f.router.pointer(s), None);

    let key = |surface, name: &str| InputEvent::Key {
        surface,
        key: KeyInput {
            name: name.into(),
            text: String::new(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers::default(),
            time: 0,
        },
    };
    let open = |node, on| Intent::Write {
        node,
        prop: Prop::Open,
        value: PropValue::Bool(on),
    };
    let writes = |msgs: Vec<Intent>| -> Vec<Intent> {
        msgs.into_iter()
            .filter(|m| {
                matches!(
                    m,
                    Intent::Write { .. }
                        | Intent::Event {
                            event: NodeEvent::Dismiss,
                            ..
                        }
                )
            })
            .collect()
    };
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    // Right on a row with no submenu: nothing opens.
    f.input(&key(s, "Down"), &mut r);
    assert_eq!(f.router.selected(list), Some(plain));
    f.input(&key(s, "Right"), &mut r);
    // Left in the top-level menu: nothing closes.
    f.input(&key(s, "Left"), &mut r);
    assert!(writes(f.drain()).is_empty());
    // Right on the row holding the submenu opens it.
    f.input(&key(s, "Down"), &mut r);
    f.input(&key(s, "Right"), &mut r);
    assert_eq!(writes(f.drain()), [open(sub, true)]);
    // The submenu takes the keyboard: Left closes it (and dismisses it).
    let mut d = SceneDiff::new();
    d.set(sub, Prop::Open, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    f.input(&InputEvent::KeyboardEnter { surface: s2 }, &mut r);
    f.drain();
    f.input(&key(s2, "Left"), &mut r);
    assert_eq!(
        writes(f.drain()),
        [
            open(sub, false),
            Intent::Event {
                node: sub,
                event: NodeEvent::Dismiss
            }
        ]
    );
    f.input(&key(s2, "Escape"), &mut r);
    assert_eq!(writes(f.drain())[0], open(sub, false));
}

/// An `input` with `type: password` is edited by the Router as any
/// `input`: typed text and BackSpace write its `text`, arrows move the
/// caret.
#[test]
fn a_password_input_is_edited_like_any_input() {
    use strand_scene::{Modifiers, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, input) = (id(0), id(1));
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(40.0))
        .create(input, NodeKind::Input, Some(panel), 0)
        .set(input, Prop::Focus, PropValue::Bool(true))
        .set(
            input,
            Prop::InputType,
            PropValue::Keyword("password".into()),
        )
        .set(input, Prop::Text, PropValue::Text(String::new()));
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 40 * 4];
    let mut t = PaintTarget::new(&mut px, Size::new(200, 40), 800, Scale::ONE, 0).unwrap();
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
        key("h", "h"),
        key("u", "u"),
        key("Left", ""),
        key("n", "n"),
        key("End", ""),
        key("BackSpace", ""),
    ] {
        f.input(&k, &mut r);
    }
    let texts: Vec<String> = f
        .drain()
        .into_iter()
        .filter_map(|m| match m {
            Intent::Write {
                node,
                prop: Prop::Text,
                value: PropValue::Text(t),
            } if node == input => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["h", "hu", "hnu", "hn"]);
}

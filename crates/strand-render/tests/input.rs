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
            latin: None,
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

/// (list-scroll-regrow) A launcher's filtered list whose height follows
/// its rows (`height: n > 0 ? 40 * n : 1`): a query matching nothing
/// leaves it 1 px tall, and clearing the query brings the rows back with
/// the first selected. It shows from its first row again: the selection
/// is revealed while the content-sized surface still has its short
/// size, and that reveal is placed again once it grows.
#[test]
fn a_list_grown_back_from_empty_shows_its_first_row() {
    use strand_scene::{Color, NodeKind, SceneDiff, SceneOp};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let (panel, col, input, list) = (id(0), id(1), id(2), id(3));
    let rows = |base: u32, d: &mut SceneDiff| -> Vec<NodeId> {
        (0..7)
            .map(|i| {
                let row = id(base + i);
                d.create(row, NodeKind::Row, Some(list), i).set(
                    row,
                    Prop::Height,
                    PropValue::Number(40.0),
                );
                row
            })
            .collect()
    };
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Open, PropValue::Bool(true))
        .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
        .create(col, NodeKind::Col, Some(panel), 0)
        .create(input, NodeKind::Input, Some(col), 0)
        .set(input, Prop::Focus, PropValue::Bool(true))
        .set(input, Prop::Nav, PropValue::Node(list))
        .set(input, Prop::Text, PropValue::Text(String::new()))
        .create(list, NodeKind::List, Some(col), 1)
        .set(list, Prop::Height, PropValue::Number(200.0));
    let first = rows(10, &mut d);
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    // The surface takes the panel's height (the compositor's configure).
    let paint = |r: &mut Renderer| {
        let h = r.surface_spec(panel).and_then(|s| s.height).unwrap() as u32;
        let mut px = vec![0u8; 200 * h as usize * 4];
        let mut t = PaintTarget::new(&mut px, Size::new(200, h), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t);
    };
    paint(&mut r);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(first[0]));
    assert_eq!(r.scroll_offset(list), Some(0.0));
    // `q = "zzzz"`: no rows, the list 1 px tall, the surface shrunk.
    let mut d = SceneDiff::new();
    for row in &first {
        d.push(SceneOp::Remove {
            id: *row,
            window: false,
        });
    }
    d.set(list, Prop::Height, PropValue::Number(1.0)).set(
        input,
        Prop::Text,
        PropValue::Text("zzzz".into()),
    );
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    paint(&mut r);
    // `q = ""`: the rows are back and the first is selected, while the
    // surface still has its short size.
    let mut d = SceneDiff::new();
    let back = rows(30, &mut d);
    d.set(list, Prop::Height, PropValue::Number(200.0)).set(
        input,
        Prop::Text,
        PropValue::Text(String::new()),
    );
    assert!(r.apply(d).is_empty());
    f.router.settle(&mut r);
    assert_eq!(f.router.selected(list), Some(back[0]));
    {
        let mut px = vec![0u8; 200 * 40 * 4];
        let mut t = PaintTarget::new(&mut px, Size::new(200, 40), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t);
    }
    // Its row is taller than the short view: the view shows its top.
    assert_eq!(
        r.scroll_offset(list),
        Some(0.0),
        "{:?}",
        r.scroll_state(list)
    );
    // Then it grows: the list shows its first row at its top.
    paint(&mut r);
    paint(&mut r);
    assert_eq!(
        r.scroll_offset(list),
        Some(0.0),
        "{:?}",
        r.scroll_state(list)
    );
    let b = r.boxes(s).unwrap();
    assert_eq!(b.rects[&back[0]].y, b.rects[&list].y);
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
                latin: None,
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
                    latin: None,
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
                at: 102,
                hidden: None
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
                at: 101,
                hidden: None
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
                at: 111,
                hidden: None
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
                at: 108,
                hidden: None
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
            latin: None,
        },
    };
    f.input(&escape, &mut r);
    assert_eq!(f.router.drag(), None);
    assert!(f.drain().is_empty(), "Escape only cancelled the drag");
    f.input(&left(s, 50.0, 90.0, ButtonState::Released), &mut r);
    assert!(events(f.drain()).is_empty(), "no drop and no click");
}

/// (M5, S-inspect; inspect-r1-4) While picking, another program's drag
/// finds no drop target (the surface manager declines its offer) and a
/// drop runs no `on drop`; turning pick mode on forgets an offer already
/// over a target.
#[test]
fn pick_mode_holds_drops_from_the_handlers() {
    let (mut r, [panel, _, _, bin, ..]) = dnd_scene();
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, panel);
    let at = LogicalPoint::new(50.0, 190.0);
    let enter = InputEvent::DragEnter {
        surface: s,
        at,
        kinds: vec![DropKind::Files],
    };
    f.input(&enter, &mut r);
    assert_eq!(f.router.drop_target(s), Some(bin));
    assert!(f.router.set_picking(true, &mut r).is_empty());
    assert_eq!(f.router.drop_target(s), None, "pick mode forgets the offer");
    f.input(&enter, &mut r);
    f.input(&InputEvent::DragMotion { surface: s, at }, &mut r);
    assert_eq!(f.router.drop_target(s), None);
    f.input(
        &InputEvent::DragDrop {
            surface: s,
            at,
            payload: DropPayload::External {
                kind: DropKind::Files,
                files: vec!["/tmp/a.png".into()],
                text: String::new(),
                app_id: None,
            },
        },
        &mut r,
    );
    assert!(f.drain().is_empty(), "no handler hears the drop");
}

/// (S-inspect; inspect-r2-4) A drag that entered while picking finds its
/// target again once pick mode ends: its next motion over the box that
/// takes `Files` targets the box, with no leave and enter in between,
/// and the drop reaches its `on drop`. Pick mode turned on mid-drag
/// keeps the surface too.
#[test]
fn a_drag_entered_while_picking_finds_its_target_after() {
    let (mut r, [panel, _, _, bin, ..]) = dnd_scene();
    let s = SurfaceId(1);
    let mut f = R::default();
    f.attached(s, panel);
    let at = LogicalPoint::new(50.0, 190.0);
    let motion = InputEvent::DragMotion { surface: s, at };
    assert!(f.router.set_picking(true, &mut r).is_empty());
    f.input(
        &InputEvent::DragEnter {
            surface: s,
            at,
            kinds: vec![DropKind::Files],
        },
        &mut r,
    );
    f.input(&motion, &mut r);
    assert_eq!(f.router.drop_target(s), None);
    let _ = f.router.set_picking(false, &mut r);
    f.input(&motion, &mut r);
    assert_eq!(
        f.router.drop_target(s),
        Some(bin),
        "the drag's target is back"
    );
    // Pick mode on and off again in the middle of the drag.
    assert!(f.router.set_picking(true, &mut r).is_empty());
    assert_eq!(f.router.drop_target(s), None);
    let _ = f.router.set_picking(false, &mut r);
    f.input(&motion, &mut r);
    assert_eq!(f.router.drop_target(s), Some(bin));
    f.input(
        &InputEvent::DragDrop {
            surface: s,
            at,
            payload: DropPayload::External {
                kind: DropKind::Files,
                files: vec!["/tmp/a.png".into()],
                text: String::new(),
                app_id: None,
            },
        },
        &mut r,
    );
    let got = f.drain();
    assert!(
        got.iter()
            .any(|i| matches!(i, Intent::Event { node, .. } if *node == bin)),
        "the drop reaches the box's `on drop`: {got:?}"
    );
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
        [(
            bin,
            NodeEvent::Drop {
                payload,
                at: 2,
                hidden: None
            }
        )]
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
            [(
                items[1],
                NodeEvent::Drop {
                    payload,
                    at,
                    hidden: None
                }
            )]
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
                at: 2,
                hidden: None
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
                at: 101,
                hidden: None
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

/// (M5) A drag handed to another surface whose list draws the same
/// items (one pin on the dock of each monitor) counts that list's rows
/// without its own copy of the dragged item (`drag_identity`), as
/// `pins.move` counts: [A, B, C] on both, A dragged from the first and
/// let go on the second between B and C lands at 1, not 2; let go
/// before the copy it lands at 0; a row of another value is counted.
#[test]
fn a_drop_on_another_surface_leaves_out_its_copy_of_the_item() {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let kw = |k: &str| PropValue::Keyword(k.into());
    // Rows A, B, C: 40 px tall, each a drag source of identity 1, 2, 3.
    let mut d = SceneDiff::new();
    let mut lists = Vec::new();
    let mut rows = Vec::new();
    for p in 0..2u32 {
        let id = |i: u32| NodeId::new(p * 100 + i, 0);
        let (panel, list) = (id(0), id(1));
        d.create(panel, NodeKind::Panel, None, p)
            .set(panel, Prop::Width, PropValue::Number(200.0))
            .set(panel, Prop::Height, PropValue::Number(120.0))
            .set(panel, Prop::Open, PropValue::Bool(true))
            .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(list, NodeKind::List, Some(panel), 0)
            .set(list, Prop::Height, PropValue::Number(120.0))
            .set(list, Prop::Accepts, PropValue::List(vec![kw("Pin")]));
        let mut mine = Vec::new();
        for k in 0..3u32 {
            let row = id(10 + k);
            d.create(row, NodeKind::Row, Some(list), k)
                .set(row, Prop::Height, PropValue::Number(40.0))
                .set(
                    row,
                    Prop::Drag,
                    PropValue::List(vec![
                        kw("Pin"),
                        PropValue::List(vec![PropValue::Text(format!("pin{k}"))]),
                    ]),
                );
            mine.push(row);
        }
        lists.push(list);
        rows.push(mine);
    }
    assert!(r.apply(d).is_empty());
    let (s1, s2) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    for (s, root) in [(s1, NodeId::new(0, 0)), (s2, NodeId::new(100, 0))] {
        r.attach_surface(s, root);
        f.attached(s, root);
        let mut px = vec![0u8; 200 * 120 * 4];
        let t = PaintTarget::new(&mut px, Size::new(200, 120), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    }
    let across = |f: &mut R, r: &mut Renderer, from: NodeId, y_from: f32, y: f32| {
        f.input(&motion(s1, 50.0, y_from, 0), r);
        f.input(&left(s1, 50.0, y_from, ButtonState::Pressed), r);
        f.input(&motion(s1, 50.0, y_from + 10.0, 10), r);
        f.input(&InputEvent::PointerLeave { surface: s1 }, r);
        f.drain();
        let at = LogicalPoint::new(50.0, y);
        f.input(
            &InputEvent::DragEnter {
                surface: s2,
                at,
                kinds: vec![],
            },
            r,
        );
        f.input(&InputEvent::DragMotion { surface: s2, at }, r);
        f.input(
            &InputEvent::DragDrop {
                surface: s2,
                at,
                payload: DropPayload::Node(from),
            },
            r,
        );
        let got = events(f.drain());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, lists[1]);
        let NodeEvent::Drop { at, .. } = got[0].1 else {
            panic!("{got:?}");
        };
        at
    };
    // A between B and C on the second surface: past its own A, at 1.
    assert_eq!(across(&mut f, &mut r, rows[0][0], 20.0, 80.0), 1);
    // A before the second surface's A: 0.
    assert_eq!(across(&mut f, &mut r, rows[0][0], 20.0, 10.0), 0);
    // C between A and B: the second surface's C is past the pointer, at 1.
    assert_eq!(across(&mut f, &mut r, rows[0][2], 100.0, 40.0), 1);
    // C at the end: its copy left out, at 2 (after A and B).
    assert_eq!(across(&mut f, &mut r, rows[0][2], 100.0, 118.0), 2);
}

/// (M6) A drop handed to another surface's windowed `list` whose rows
/// before the mounted window hold the dragged item's copy: render cannot
/// see it, so the drop names those rows (`HiddenRows`) for logic, which
/// holds every item, to leave the copy out (`Instance::drop_at`). The
/// second surface's list mounts global rows 40..59 (scrolled to row 40);
/// the first surface shows pins 5, 41 and 45. Let go between rows 42 and
/// 43: pin 5 (its copy at row 5, unmounted) is at 43 with rows 0..40
/// hidden, which logic makes 42; pin 41 (its copy mounted before the
/// pointer, left out here) is at 42 with nothing hidden; pin 45 (its
/// copy mounted past the pointer) is at 43 with rows 0..40 hidden, among
/// which logic finds no copy.
#[test]
fn a_drop_on_a_windowed_list_names_the_rows_it_could_not_see() {
    use strand_render::HiddenRows;
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let kw = |k: &str| PropValue::Keyword(k.into());
    let pin = |k: u32| {
        PropValue::List(vec![
            kw("Pin"),
            PropValue::List(vec![PropValue::Text(format!("pin{k}"))]),
        ])
    };
    let id = |i: u32| NodeId::new(i, 0);
    let (panel1, list1, panel2, list2) = (id(0), id(1), id(100), id(101));
    let mut d = SceneDiff::new();
    for (p, l, n) in [(panel1, list1, 0), (panel2, list2, 1)] {
        d.create(p, NodeKind::Panel, None, n)
            .set(p, Prop::Width, PropValue::Number(200.0))
            .set(p, Prop::Height, PropValue::Number(160.0))
            .set(p, Prop::Open, PropValue::Bool(true))
            .set(p, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(l, NodeKind::List, Some(p), 0)
            .set(l, Prop::Height, PropValue::Number(160.0))
            .set(l, Prop::Accepts, PropValue::List(vec![kw("Pin")]));
    }
    let sources: Vec<NodeId> = [5, 41, 45].iter().map(|k| id(10 + k)).collect();
    for (i, (row, k)) in sources.iter().zip([5, 41, 45]).enumerate() {
        d.create(*row, NodeKind::Row, Some(list1), i as u32)
            .set(*row, Prop::Height, PropValue::Number(40.0))
            .set(*row, Prop::Drag, pin(k));
    }
    // The second list: rows 40..59 of 60 mounted.
    d.set(list2, Prop::RowFirst, PropValue::Number(40.0)).set(
        list2,
        Prop::RowCount,
        PropValue::Number(60.0),
    );
    for k in 40..60u32 {
        let row = id(200 + k);
        d.create(row, NodeKind::Row, Some(list2), k - 40)
            .set(row, Prop::Height, PropValue::Number(40.0))
            .set(row, Prop::Drag, pin(k));
    }
    assert!(r.apply(d).is_empty());
    let (s1, s2) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    let mut px = vec![0u8; 200 * 160 * 4];
    let mut paint = |r: &mut Renderer, s, secs| {
        let t = PaintTarget::new(&mut px, Size::new(200, 160), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(secs)));
    };
    for (s, root) in [(s1, panel1), (s2, panel2)] {
        r.attach_surface(s, root);
        f.attached(s, root);
        paint(&mut r, s, 1);
    }
    // Global row 40 starts at 1,600 px: scrolled there, rows 40..43 show.
    assert_eq!(
        r.scroll(s2, LogicalPoint::new(50.0, 80.0), 1600.0),
        Some(list2)
    );
    paint(&mut r, s2, 2);
    assert!(r.node_rect(s2, id(242)).is_some(), "row 42 is laid out");
    let across = |f: &mut R, r: &mut Renderer, from: usize| {
        let y_from = 20.0 + 40.0 * from as f32;
        f.input(&motion(s1, 50.0, y_from, 0), r);
        f.input(&left(s1, 50.0, y_from, ButtonState::Pressed), r);
        f.input(&motion(s1, 50.0, y_from + 10.0, 10), r);
        f.input(&InputEvent::PointerLeave { surface: s1 }, r);
        f.drain();
        // Past row 42's middle (y 100), before row 43's (y 140).
        let at = LogicalPoint::new(50.0, 125.0);
        f.input(
            &InputEvent::DragEnter {
                surface: s2,
                at,
                kinds: vec![],
            },
            r,
        );
        f.input(&InputEvent::DragMotion { surface: s2, at }, r);
        f.input(
            &InputEvent::DragDrop {
                surface: s2,
                at,
                payload: DropPayload::Node(sources[from]),
            },
            r,
        );
        let got = events(f.drain());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, list2);
        let NodeEvent::Drop { at, hidden, .. } = got[0].1 else {
            panic!("{got:?}");
        };
        (at, hidden)
    };
    let hidden = Some(HiddenRows {
        list: list2,
        before: 40,
    });
    assert_eq!(across(&mut f, &mut r, 0), (43, hidden), "pin 5");
    assert_eq!(across(&mut f, &mut r, 1), (42, None), "pin 41");
    assert_eq!(across(&mut f, &mut r, 2), (43, hidden), "pin 45");
    // Within its own windowed list nothing is hidden: the source's row is
    // the one left out. Row 41 let go at the same place: rows 40 and 42
    // before it, at 42.
    f.input(&motion(s2, 50.0, 60.0, 0), &mut r);
    f.input(&left(s2, 50.0, 60.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s2, 50.0, 70.0, 10), &mut r);
    f.input(&motion(s2, 50.0, 125.0, 20), &mut r);
    f.input(&left(s2, 50.0, 125.0, ButtonState::Released), &mut r);
    assert_eq!(
        events(f.drain()),
        [(
            list2,
            NodeEvent::Drop {
                payload: DropPayload::Node(id(241)),
                at: 42,
                hidden: None
            }
        )]
    );
}

/// (M6) The same on a windowed `list` whose rows are per-item drop
/// targets with content of their own (a `col` holding a box), so a drop
/// is placed among the list's rows by the row under the pointer. The
/// second surface's list mounts global rows 40..59 (scrolled to row 40);
/// the first surface shows pins 5, 41, 43 and 42. Each is let go on row
/// 42 (y 80..120): pin 5 (its copy unmounted) past its middle at 43 and
/// before it at 42, both with rows 0..40 hidden; pin 41 (its copy
/// mounted above the pointer, left out) at 42 with nothing hidden; pin
/// 43 (its copy mounted just past the pointer, left out) at 43 with rows
/// 0..40 hidden, as a copy above them would be the first; and pin 42 on
/// its own copy at that copy's place, 42, with rows 0..40 hidden.
#[test]
fn a_per_item_drop_on_a_windowed_list_names_the_rows_it_could_not_see() {
    use strand_render::HiddenRows;
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let kw = |k: &str| PropValue::Keyword(k.into());
    let pin = |k: u32| {
        PropValue::List(vec![
            kw("Pin"),
            PropValue::List(vec![PropValue::Text(format!("pin{k}"))]),
        ])
    };
    let id = |i: u32| NodeId::new(i, 0);
    let (panel1, list1, panel2, list2) = (id(0), id(1), id(100), id(101));
    let mut d = SceneDiff::new();
    for (p, l, n) in [(panel1, list1, 0), (panel2, list2, 1)] {
        d.create(p, NodeKind::Panel, None, n)
            .set(p, Prop::Width, PropValue::Number(200.0))
            .set(p, Prop::Height, PropValue::Number(160.0))
            .set(p, Prop::Open, PropValue::Bool(true))
            .set(p, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(l, NodeKind::List, Some(p), 0)
            .set(l, Prop::Height, PropValue::Number(160.0));
    }
    let pins = [5, 41, 43, 42];
    let sources: Vec<NodeId> = pins.iter().map(|k| id(10 + k)).collect();
    for (i, (row, k)) in sources.iter().zip(pins).enumerate() {
        d.create(*row, NodeKind::Row, Some(list1), i as u32)
            .set(*row, Prop::Height, PropValue::Number(40.0))
            .set(*row, Prop::Drag, pin(k));
    }
    d.set(list2, Prop::RowFirst, PropValue::Number(40.0)).set(
        list2,
        Prop::RowCount,
        PropValue::Number(60.0),
    );
    for k in 40..60u32 {
        let (row, content) = (id(200 + k), id(300 + k));
        d.create(row, NodeKind::Col, Some(list2), k - 40)
            .set(row, Prop::Height, PropValue::Number(40.0))
            .set(row, Prop::Drag, pin(k))
            .set(row, Prop::Accepts, PropValue::List(vec![kw("Pin")]))
            .create(content, NodeKind::Box, Some(row), 0)
            .set(content, Prop::Width, PropValue::Number(40.0))
            .set(content, Prop::Height, PropValue::Number(20.0));
    }
    assert!(r.apply(d).is_empty());
    let (s1, s2) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    let mut px = vec![0u8; 200 * 160 * 4];
    let mut paint = |r: &mut Renderer, s, secs| {
        let t = PaintTarget::new(&mut px, Size::new(200, 160), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(secs)));
    };
    for (s, root) in [(s1, panel1), (s2, panel2)] {
        r.attach_surface(s, root);
        f.attached(s, root);
        paint(&mut r, s, 1);
    }
    assert_eq!(
        r.scroll(s2, LogicalPoint::new(50.0, 80.0), 1600.0),
        Some(list2)
    );
    paint(&mut r, s2, 2);
    assert_eq!(
        r.node_rect(s2, id(242)).map(|b| (b.y, b.h)),
        Some((80.0, 40.0)),
        "row 42 at y 80"
    );
    let across = |f: &mut R, r: &mut Renderer, from: usize, y: f32| {
        let y_from = 20.0 + 40.0 * from as f32;
        f.input(&motion(s1, 50.0, y_from, 0), r);
        f.input(&left(s1, 50.0, y_from, ButtonState::Pressed), r);
        f.input(&motion(s1, 50.0, y_from + 10.0, 10), r);
        f.input(&InputEvent::PointerLeave { surface: s1 }, r);
        f.drain();
        let at = LogicalPoint::new(50.0, y);
        f.input(
            &InputEvent::DragEnter {
                surface: s2,
                at,
                kinds: vec![],
            },
            r,
        );
        f.input(&InputEvent::DragMotion { surface: s2, at }, r);
        f.input(
            &InputEvent::DragDrop {
                surface: s2,
                at,
                payload: DropPayload::Node(sources[from]),
            },
            r,
        );
        let got = events(f.drain());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, id(242), "row 42 takes it");
        let NodeEvent::Drop { at, hidden, .. } = got[0].1 else {
            panic!("{got:?}");
        };
        (at, hidden)
    };
    let hidden = Some(HiddenRows {
        list: list2,
        before: 40,
    });
    assert_eq!(
        across(&mut f, &mut r, 0, 110.0),
        (43, hidden),
        "pin 5, past"
    );
    assert_eq!(
        across(&mut f, &mut r, 0, 90.0),
        (42, hidden),
        "pin 5, before"
    );
    assert_eq!(across(&mut f, &mut r, 1, 110.0), (42, None), "pin 41");
    assert_eq!(across(&mut f, &mut r, 2, 110.0), (43, hidden), "pin 43");
    assert_eq!(across(&mut f, &mut r, 3, 110.0), (42, hidden), "pin 42");
}

/// (M5) Equal items in one list are separate rows: [a, b, a, c] (two
/// rows of one identity) on two surfaces. Within its own list a drag
/// leaves out only its own row, so `pins.move(it, at)` (which takes the
/// first equal item out) lands where it is let go: the first `a` let go
/// at the end is at 3 (giving [b, a, c, a]), the second `a` let go
/// between the first and `b` at 1. Handed to the other surface, only
/// that list's first copy is left out: the first `a` let go at its end
/// is at 3 too.
#[test]
fn equal_items_in_one_list_are_counted_apart() {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let kw = |k: &str| PropValue::Keyword(k.into());
    let mut d = SceneDiff::new();
    let mut lists = Vec::new();
    let mut rows = Vec::new();
    for p in 0..2u32 {
        let id = |i: u32| NodeId::new(p * 100 + i, 0);
        let (panel, list) = (id(0), id(1));
        d.create(panel, NodeKind::Panel, None, p)
            .set(panel, Prop::Width, PropValue::Number(200.0))
            .set(panel, Prop::Height, PropValue::Number(160.0))
            .set(panel, Prop::Open, PropValue::Bool(true))
            .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(list, NodeKind::List, Some(panel), 0)
            .set(list, Prop::Height, PropValue::Number(160.0))
            .set(list, Prop::Accepts, PropValue::List(vec![kw("Pin")]));
        let mut mine = Vec::new();
        for (k, item) in ["a", "b", "a", "c"].into_iter().enumerate() {
            let row = id(10 + k as u32);
            d.create(row, NodeKind::Row, Some(list), k as u32)
                .set(row, Prop::Height, PropValue::Number(40.0))
                .set(
                    row,
                    Prop::Drag,
                    PropValue::List(vec![
                        kw("Pin"),
                        PropValue::List(vec![PropValue::Text(item.into())]),
                    ]),
                );
            mine.push(row);
        }
        lists.push(list);
        rows.push(mine);
    }
    assert!(r.apply(d).is_empty());
    let (s1, s2) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    for (s, root) in [(s1, NodeId::new(0, 0)), (s2, NodeId::new(100, 0))] {
        r.attach_surface(s, root);
        f.attached(s, root);
        let mut px = vec![0u8; 200 * 160 * 4];
        let t = PaintTarget::new(&mut px, Size::new(200, 160), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    }
    let only_drop = |f: &mut R, list: NodeId| {
        let got = events(f.drain());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, list);
        let NodeEvent::Drop { at, .. } = got[0].1 else {
            panic!("{got:?}");
        };
        at
    };
    let within = |f: &mut R, r: &mut Renderer, y_from: f32, y: f32| {
        f.input(&motion(s1, 50.0, y_from, 0), r);
        f.input(&left(s1, 50.0, y_from, ButtonState::Pressed), r);
        f.input(&motion(s1, 50.0, y_from + 10.0, 10), r);
        f.input(&motion(s1, 50.0, y, 20), r);
        f.input(&left(s1, 50.0, y, ButtonState::Released), r);
        only_drop(f, lists[0])
    };
    // The first `a` at the end: past b, the other a and c.
    assert_eq!(within(&mut f, &mut r, 20.0, 158.0), 3);
    // The second `a` between the first and `b`.
    assert_eq!(within(&mut f, &mut r, 100.0, 38.0), 1);
    // The first `a` handed to the second surface, let go at its end.
    let from = rows[0][0];
    f.input(&motion(s1, 50.0, 20.0, 0), &mut r);
    f.input(&left(s1, 50.0, 20.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s1, 50.0, 30.0, 10), &mut r);
    f.input(&InputEvent::PointerLeave { surface: s1 }, &mut r);
    f.drain();
    let at = LogicalPoint::new(50.0, 158.0);
    f.input(
        &InputEvent::DragEnter {
            surface: s2,
            at,
            kinds: vec![],
        },
        &mut r,
    );
    f.input(&InputEvent::DragMotion { surface: s2, at }, &mut r);
    f.input(
        &InputEvent::DragDrop {
            surface: s2,
            at,
            payload: DropPayload::Node(from),
        },
        &mut r,
    );
    assert_eq!(only_drop(&mut f, lists[1]), 3);
}

/// (M5) A drop on an outer list whose rows hold a nested list counts
/// the row holding the dragged item: a kanban board (a `row` taking
/// `Card`s, `Prop::DropRows`) of three 60 px columns [todo, doing,
/// done], each a `for` of two cards (`Prop::DropRows`, as the compiler
/// marks every element whose direct child is a `for`), on two surfaces.
/// A card dragged out of `doing` and let go past the middle of `done`
/// lands at 3, not 2 (the `doing` column is a row of the board, not the
/// card's row); let go between `doing` and `done` at 2. Handed to the
/// other surface's board, whose `doing` holds a copy of the card, it is
/// at 3 there too (the copy is that column's item, not the board's).
#[test]
fn a_drop_on_an_outer_list_counts_the_row_holding_the_item() {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let kw = |k: &str| PropValue::Keyword(k.into());
    let mut d = SceneDiff::new();
    let mut boards = Vec::new();
    let mut cards = Vec::new();
    for p in 0..2u32 {
        let id = |i: u32| NodeId::new(p * 100 + i, 0);
        let (panel, board) = (id(0), id(1));
        d.create(panel, NodeKind::Panel, None, p)
            .set(panel, Prop::Width, PropValue::Number(180.0))
            .set(panel, Prop::Height, PropValue::Number(80.0))
            .set(panel, Prop::Open, PropValue::Bool(true))
            .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(board, NodeKind::Row, Some(panel), 0)
            .set(board, Prop::Height, PropValue::Number(80.0))
            .set(board, Prop::DropRows, PropValue::Bool(true))
            .set(board, Prop::Accepts, PropValue::List(vec![kw("Card")]));
        let mut mine = Vec::new();
        for c in 0..3u32 {
            let col = id(10 + c);
            d.create(col, NodeKind::Col, Some(board), c)
                .set(col, Prop::Width, PropValue::Number(60.0))
                .set(col, Prop::Height, PropValue::Number(80.0))
                .set(col, Prop::DropRows, PropValue::Bool(true));
            for k in 0..2u32 {
                let card = id(20 + c * 2 + k);
                d.create(card, NodeKind::Box, Some(col), k)
                    .set(card, Prop::Width, PropValue::Number(60.0))
                    .set(card, Prop::Height, PropValue::Number(40.0))
                    .set(
                        card,
                        Prop::Drag,
                        PropValue::List(vec![
                            kw("Card"),
                            PropValue::List(vec![PropValue::Text(format!("card{c}{k}"))]),
                        ]),
                    );
                mine.push(card);
            }
        }
        boards.push(board);
        cards.push(mine);
    }
    assert!(r.apply(d).is_empty());
    let (s1, s2) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    for (s, root) in [(s1, NodeId::new(0, 0)), (s2, NodeId::new(100, 0))] {
        r.attach_surface(s, root);
        f.attached(s, root);
        let mut px = vec![0u8; 180 * 80 * 4];
        let t = PaintTarget::new(&mut px, Size::new(180, 80), 720, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    }
    let only_drop = |f: &mut R, board: NodeId| {
        let got = events(f.drain());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, board);
        let NodeEvent::Drop { at, .. } = got[0].1 else {
            panic!("{got:?}");
        };
        at
    };
    // The first card of `doing` (x 60..120, y 0..40).
    let within = |f: &mut R, r: &mut Renderer, x: f32| {
        f.input(&motion(s1, 90.0, 20.0, 0), r);
        f.input(&left(s1, 90.0, 20.0, ButtonState::Pressed), r);
        f.input(&motion(s1, 90.0, 30.0, 10), r);
        f.input(&motion(s1, x, 60.0, 20), r);
        f.input(&left(s1, x, 60.0, ButtonState::Released), r);
        only_drop(f, boards[0])
    };
    // Past the middle of `done`: after all three columns.
    assert_eq!(within(&mut f, &mut r, 170.0), 3);
    // Past the middle of `doing`, before `done`'s: 2.
    assert_eq!(within(&mut f, &mut r, 130.0), 2);
    // Handed to the other surface's board, past the middle of its `done`.
    let from = cards[0][2];
    f.input(&motion(s1, 90.0, 20.0, 0), &mut r);
    f.input(&left(s1, 90.0, 20.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s1, 90.0, 30.0, 10), &mut r);
    f.input(&InputEvent::PointerLeave { surface: s1 }, &mut r);
    f.drain();
    let at = LogicalPoint::new(170.0, 60.0);
    f.input(
        &InputEvent::DragEnter {
            surface: s2,
            at,
            kinds: vec![],
        },
        &mut r,
    );
    f.input(&InputEvent::DragMotion { surface: s2, at }, &mut r);
    f.input(
        &InputEvent::DragDrop {
            surface: s2,
            at,
            payload: DropPayload::Node(from),
        },
        &mut r,
    );
    assert_eq!(only_drop(&mut f, boards[1]), 3);
}

/// (M5) A dock whose items each hold a `for` of window dots beside the
/// pinned app's icon (`row { on drop(p: Pin, at: int) {…} for p in pins
/// { col { for w in p.windows { box {…} } image p.icon { drag: p } } } }`):
/// each item `col` holds a `for` (`Prop::DropRows`), and its icon is the
/// `col`'s own content (`Prop::NotRow`), so the `col` is the icon's row
/// in the dock, not a nested list. Pin 0 dragged past the middle of pin
/// 1 lands at 1 (its own row left out; it was 2), and handed to the
/// other monitor's dock at 1 there too (its copy left out).
#[test]
fn a_dock_item_holding_a_for_beside_its_icon_is_the_icons_row() {
    use strand_scene::{Color, NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let kw = |k: &str| PropValue::Keyword(k.into());
    let mut d = SceneDiff::new();
    let mut docks = Vec::new();
    let mut icons = Vec::new();
    for m in 0..2u32 {
        let id = |i: u32| NodeId::new(m * 100 + i, 0);
        let (panel, dock) = (id(0), id(1));
        d.create(panel, NodeKind::Panel, None, m)
            .set(panel, Prop::Width, PropValue::Number(180.0))
            .set(panel, Prop::Height, PropValue::Number(60.0))
            .set(panel, Prop::Open, PropValue::Bool(true))
            .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(dock, NodeKind::Row, Some(panel), 0)
            .set(dock, Prop::Height, PropValue::Number(60.0))
            .set(dock, Prop::DropRows, PropValue::Bool(true))
            .set(dock, Prop::Accepts, PropValue::List(vec![kw("Pin")]));
        let mut mine = Vec::new();
        for p in 0..3u32 {
            let col = id(10 + p);
            d.create(col, NodeKind::Col, Some(dock), p)
                .set(col, Prop::Width, PropValue::Number(60.0))
                .set(col, Prop::Height, PropValue::Number(60.0))
                .set(col, Prop::DropRows, PropValue::Bool(true));
            for w in 0..2u32 {
                let dot = id(20 + p * 2 + w);
                d.create(dot, NodeKind::Box, Some(col), w)
                    .set(dot, Prop::Width, PropValue::Number(4.0))
                    .set(dot, Prop::Height, PropValue::Number(4.0));
            }
            let icon = id(40 + p);
            d.create(icon, NodeKind::Box, Some(col), 2)
                .set(icon, Prop::Width, PropValue::Number(60.0))
                .set(icon, Prop::Height, PropValue::Number(40.0))
                .set(icon, Prop::NotRow, PropValue::Bool(true))
                .set(
                    icon,
                    Prop::Drag,
                    PropValue::List(vec![
                        kw("Pin"),
                        PropValue::List(vec![PropValue::Text(format!("app{p}"))]),
                    ]),
                );
            mine.push(icon);
        }
        docks.push(dock);
        icons.push(mine);
    }
    assert!(r.apply(d).is_empty());
    let (s1, s2) = (SurfaceId(1), SurfaceId(2));
    let mut f = R::default();
    for (s, root) in [(s1, NodeId::new(0, 0)), (s2, NodeId::new(100, 0))] {
        r.attach_surface(s, root);
        f.attached(s, root);
        let mut px = vec![0u8; 180 * 60 * 4];
        let t = PaintTarget::new(&mut px, Size::new(180, 60), 720, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t.at(std::time::Duration::from_secs(1)));
    }
    let only_drop = |f: &mut R, dock: NodeId| {
        let got = events(f.drain());
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, dock);
        let NodeEvent::Drop { at, .. } = got[0].1 else {
            panic!("{got:?}");
        };
        at
    };
    // Pin 0's icon (x 0..60, y 8..48), let go past the middle of pin 1.
    f.input(&motion(s1, 30.0, 30.0, 0), &mut r);
    f.input(&left(s1, 30.0, 30.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s1, 30.0, 40.0, 10), &mut r);
    f.input(&motion(s1, 100.0, 30.0, 20), &mut r);
    f.input(&left(s1, 100.0, 30.0, ButtonState::Released), &mut r);
    assert_eq!(only_drop(&mut f, docks[0]), 1);
    // Handed to the other monitor's dock, past the middle of its pin 1.
    let from = icons[0][0];
    f.input(&motion(s1, 30.0, 30.0, 0), &mut r);
    f.input(&left(s1, 30.0, 30.0, ButtonState::Pressed), &mut r);
    f.input(&motion(s1, 30.0, 40.0, 10), &mut r);
    f.input(&InputEvent::PointerLeave { surface: s1 }, &mut r);
    f.drain();
    let at = LogicalPoint::new(100.0, 30.0);
    f.input(
        &InputEvent::DragEnter {
            surface: s2,
            at,
            kinds: vec![],
        },
        &mut r,
    );
    f.input(&InputEvent::DragMotion { surface: s2, at }, &mut r);
    f.input(
        &InputEvent::DragDrop {
            surface: s2,
            at,
            payload: DropPayload::Node(from),
        },
        &mut r,
    );
    assert_eq!(only_drop(&mut f, docks[1]), 1);
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
            latin: None,
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
            latin: None,
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
            latin: None,
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
            latin: None,
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

/// Return in an `input` with no `nav:` list activates the input itself
/// (`on activate`, as on a list's selected row), after `on key` hears it,
/// and writes no text.
#[test]
fn return_activates_a_plain_input() {
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
        .set(input, Prop::Text, PropValue::Text("hunter2".into()));
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut px = vec![0u8; 200 * 40 * 4];
    let mut t = PaintTarget::new(&mut px, Size::new(200, 40), 800, Scale::ONE, 0).unwrap();
    r.paint(s, &mut t);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    f.drain();
    for name in ["Return", "KP_Enter"] {
        f.input(
            &InputEvent::Key {
                surface: s,
                key: KeyInput {
                    name: name.into(),
                    text: "\r".into(),
                    state: ButtonState::Pressed,
                    repeat: false,
                    modifiers: Modifiers::default(),
                    time: 0,
                    latin: None,
                },
            },
            &mut r,
        );
        assert_eq!(
            f.drain(),
            vec![
                Intent::Event {
                    node: input,
                    event: NodeEvent::Key {
                        name: name.into(),
                        text: "\r".into(),
                        modifiers: Modifiers::default()
                    }
                },
                Intent::Event {
                    node: input,
                    event: NodeEvent::Activate
                },
            ]
        );
    }
}

/// A bar with two items, `a` (x < 10) and `b` (10 ≤ x < 20), on surface
/// 1; `a`'s island on surface 2 and `b`'s on surface 3, each a popup in
/// its item holding a card (x < 50 on the island; nothing is under
/// x ≥ 100, past the island's edge). `a`'s card holds an item (x < 20)
/// with a popup of its own on surface 4.
struct Islands {
    tree: strand_render::SceneTree,
}

const BAR_N: NodeId = NodeId::new(0, 0);
const ITEM_A: NodeId = NodeId::new(1, 0);
const POP_A: NodeId = NodeId::new(2, 0);
const CARD_A: NodeId = NodeId::new(3, 0);
const ITEM_B: NodeId = NodeId::new(4, 0);
const POP_B: NodeId = NodeId::new(5, 0);
const CARD_B: NodeId = NodeId::new(6, 0);
const SUB_ITEM: NodeId = NodeId::new(7, 0);
const SUB_POP: NodeId = NodeId::new(8, 0);
const SUB_CARD: NodeId = NodeId::new(9, 0);

impl Islands {
    /// The islands' popups with `trigger: hover` when `hover`, waiting
    /// `delay` before they open.
    fn new(hover: bool, delay: Option<std::time::Duration>) -> Islands {
        use strand_scene::{NodeKind, SceneDiff};
        let mut d = SceneDiff::new();
        d.create(BAR_N, NodeKind::Bar, None, 0)
            .create(ITEM_A, NodeKind::Box, Some(BAR_N), 0)
            .create(POP_A, NodeKind::Popup, Some(ITEM_A), 0)
            .create(CARD_A, NodeKind::Box, Some(POP_A), 0)
            .create(SUB_ITEM, NodeKind::Box, Some(CARD_A), 0)
            .create(SUB_POP, NodeKind::Popup, Some(SUB_ITEM), 0)
            .create(SUB_CARD, NodeKind::Box, Some(SUB_POP), 0)
            .create(ITEM_B, NodeKind::Box, Some(BAR_N), 1)
            .create(POP_B, NodeKind::Popup, Some(ITEM_B), 0)
            .create(CARD_B, NodeKind::Box, Some(POP_B), 0);
        if hover {
            for p in [POP_A, POP_B, SUB_POP] {
                d.set(p, Prop::Trigger, PropValue::Keyword("hover".into()));
                if let Some(t) = delay {
                    d.set(p, Prop::Delay, PropValue::Duration(t));
                }
            }
        }
        let mut tree = strand_render::SceneTree::new();
        assert!(tree.apply(d).is_empty());
        Islands { tree }
    }
}

impl InputScene for Islands {
    fn hit(&self, surface: SurfaceId, at: LogicalPoint) -> Vec<NodeId> {
        if at.x >= 100.0 {
            return Vec::new();
        }
        match surface.0 {
            1 if at.x < 10.0 => vec![ITEM_A, BAR_N],
            1 if at.x < 20.0 => vec![ITEM_B, BAR_N],
            1 => vec![BAR_N],
            2 if at.x < 20.0 => vec![SUB_ITEM, CARD_A, POP_A],
            2 if at.x < 50.0 => vec![CARD_A, POP_A],
            2 => vec![POP_A],
            3 if at.x < 50.0 => vec![CARD_B, POP_B],
            3 => vec![POP_B],
            _ => vec![SUB_CARD, SUB_POP],
        }
    }
    fn tree(&self) -> Option<&strand_render::SceneTree> {
        Some(&self.tree)
    }
}

const S_BAR: SurfaceId = SurfaceId(1);
const S_A: SurfaceId = SurfaceId(2);
const S_SUB: SurfaceId = SurfaceId(4);

fn at_x(surface: SurfaceId, x: f32) -> InputEvent {
    InputEvent::PointerMotion {
        surface,
        position: LogicalPoint::new(x, 1.0),
        time: 0,
    }
}

fn enter(surface: SurfaceId, x: f32) -> InputEvent {
    InputEvent::PointerEnter {
        surface,
        position: LogicalPoint::new(x, 1.0),
    }
}

fn leave(surface: SurfaceId) -> InputEvent {
    InputEvent::PointerLeave { surface }
}

fn hover(node: NodeId, on: bool) -> Intent {
    Intent::Flag {
        node,
        flag: Flag::Hover,
        on,
    }
}

/// The popups the router opened (`true`) and closed so far, in order.
fn opens(f: &mut R) -> Vec<(NodeId, bool)> {
    let (opens, rest): (Vec<Intent>, Vec<Intent>) = f.drain().into_iter().partition(|i| {
        matches!(
            i,
            Intent::Write {
                prop: Prop::Open,
                ..
            }
        )
    });
    f.out = rest;
    opens
        .into_iter()
        .filter_map(|i| match i {
            Intent::Write {
                node,
                value: PropValue::Bool(on),
                ..
            } => Some((node, on)),
            _ => None,
        })
        .collect()
}

/// The hover flags the router asked for so far.
fn flags(f: &mut R) -> Vec<Intent> {
    f.drain()
        .into_iter()
        .filter(|i| matches!(i, Intent::Flag { .. }))
        .collect()
}

/// Waits past the hover grace, then ticks the router.
fn after_grace(f: &mut R, scene: &mut dyn InputScene) {
    std::thread::sleep(strand_render::HOVER_GRACE + std::time::Duration::from_millis(50));
    f.out.extend(f.router.tick(scene));
}

/// spec.md 6.3 (decisions.md m6-w2-surface): a `trigger: hover` popup
/// opens as the pointer reaches its anchor, stays open while the
/// pointer crosses the gap to it (within `HOVER_GRACE`) and rests on
/// it, and closes `HOVER_GRACE` after the pointer leaves both. The
/// anchor's own `hover` keeps its meaning: it goes out as the pointer
/// leaves the anchor.
#[test]
fn a_hover_popup_opens_on_its_anchor_and_closes_after_the_grace() {
    let grace = strand_render::HOVER_GRACE;
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)]);
    assert_eq!(flags(&mut f), [hover(BAR_N, true), hover(ITEM_A, true)]);
    assert_eq!(f.router.next_wake(), None, "open, with the pointer on `a`");
    f.attached(S_A, POP_A);

    // Off the bar into the gap: the item's hover goes, its popup stays.
    let left = std::time::Instant::now();
    f.input(&leave(S_BAR), &mut scene);
    assert_eq!(opens(&mut f), []);
    assert_eq!(flags(&mut f), [hover(ITEM_A, false), hover(BAR_N, false)]);
    let due = f.router.next_wake().expect("the grace");
    assert!(due > left && due <= left + grace + std::time::Duration::from_millis(50));
    // On the island in time: it stays, for as long as the pointer does.
    f.input(&enter(S_A, 30.0), &mut scene);
    assert_eq!(opens(&mut f), []);
    assert_eq!(f.router.next_wake(), None, "no grace while on the island");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [], "still open under the pointer");

    // Away from both: it closes once the grace ends, not before.
    f.input(&leave(S_A), &mut scene);
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), [], "graced");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, false)]);
    assert_eq!(f.router.next_wake(), None);

    // Back on the bar off the items: nothing opens.
    f.input(&enter(S_BAR, 50.0), &mut scene);
    assert_eq!(opens(&mut f), []);
}

/// The pointer going straight from item `a` to item `b`: `b`'s popup
/// opens and `a`'s closes in the same step, `a`'s first, with no grace
/// in between (one `trigger: hover` popup open from a surface at a
/// time).
#[test]
fn going_straight_from_a_to_b_swaps_the_popups_with_no_overlap() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)]);
    f.attached(S_A, POP_A);
    f.input(&at_x(S_BAR, 15.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, false), (POP_B, true)]);
    assert_eq!(f.router.next_wake(), None, "no grace left running");
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_B, false), (POP_A, true)]);
}

/// A `trigger: hover` popup opened from inside another keeps it open:
/// the pointer on the inner popup is on the outer one's content. Off
/// both, the inner closes first, then the outer. Inside the outer, off
/// the inner's anchor, only the inner's grace runs.
#[test]
fn a_nested_hover_popup_keeps_the_one_it_opened_from() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.attached(S_A, POP_A);
    f.input(&leave(S_BAR), &mut scene);
    f.input(&enter(S_A, 10.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true), (SUB_POP, true)]);
    f.attached(S_SUB, SUB_POP);
    // Onto the inner popup: both stay.
    f.input(&leave(S_A), &mut scene);
    f.input(&enter(S_SUB, 10.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), []);
    // Back on the outer card, off the inner's anchor: the inner closes
    // after its grace, the outer stays.
    f.input(&leave(S_SUB), &mut scene);
    f.input(&enter(S_A, 30.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(SUB_POP, false)]);
    // Both open again, then the pointer away from all: innermost first.
    f.input(&at_x(S_A, 10.0), &mut scene);
    assert_eq!(opens(&mut f), [(SUB_POP, true)]);
    f.input(&leave(S_A), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(SUB_POP, false), (POP_A, false)]);
}

/// A drag that started on the popup (a slider's) keeps it open while
/// the button is held, even far past the popup's edge; the release
/// starts the grace.
#[test]
fn a_drag_that_leaves_the_popup_keeps_it_open() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.attached(S_A, POP_A);
    f.input(&leave(S_BAR), &mut scene);
    f.input(&enter(S_A, 30.0), &mut scene);
    let button = |x, state| InputEvent::PointerButton {
        surface: S_A,
        position: LogicalPoint::new(x, 1.0),
        button: button::LEFT,
        state,
        time: 0,
    };
    f.input(&button(30.0, ButtonState::Pressed), &mut scene);
    f.input(&at_x(S_A, 400.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)], "held open by the drag");
    f.input(&button(400.0, ButtonState::Released), &mut scene);
    f.input(&leave(S_A), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, false)]);
}

/// (m6-owner-fixes review, bug 15) A popup whose surface goes while a
/// press on it is held (the compositor closed it mid-drag) is pressed no
/// more: its chain hears `pressed` false, so the renderer does not keep
/// its root pressed (and its whole box as its input region) into the
/// next time it opens, and logic's `pressed` goes too.
#[test]
fn a_surface_that_goes_mid_press_is_not_left_pressed() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.attached(S_A, POP_A);
    f.input(&leave(S_BAR), &mut scene);
    f.input(&enter(S_A, 30.0), &mut scene);
    f.input(
        &InputEvent::PointerButton {
            surface: S_A,
            position: LogicalPoint::new(30.0, 1.0),
            button: button::LEFT,
            state: ButtonState::Pressed,
            time: 0,
        },
        &mut scene,
    );
    let pressed = |out: &[Intent], on: bool| -> Vec<NodeId> {
        out.iter()
            .filter_map(|i| match i {
                Intent::Flag {
                    node,
                    flag: Flag::Pressed,
                    on: o,
                } if *o == on => Some(*node),
                _ => None,
            })
            .collect()
    };
    let out = f.drain();
    assert!(pressed(&out, true).contains(&POP_A), "{out:?}");
    f.router.detached(S_A);
    f.out.extend(f.router.tick(&mut scene));
    let out = f.drain();
    let off = pressed(&out, false);
    assert!(
        off.contains(&POP_A) && off.contains(&CARD_A),
        "the chain is let go: {out:?}"
    );
}

/// `delay`: the pointer must rest on the anchor that long before the
/// popup opens; leaving first opens nothing.
#[test]
fn a_hover_popup_waits_for_its_delay() {
    use std::time::Duration;
    let mut scene = Islands::new(true, Some(Duration::from_millis(80)));
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), []);
    assert!(f.router.next_wake().is_some());
    f.input(&at_x(S_BAR, 50.0), &mut scene);
    assert_eq!(f.router.next_wake(), None, "passed over: nothing waits");
    std::thread::sleep(Duration::from_millis(100));
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), []);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    std::thread::sleep(Duration::from_millis(100));
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), [(POP_A, true)]);
}

/// A popup without `trigger: hover` is the program's, as before M6: the
/// router never writes its `open`, and its anchor's `hover` goes as the
/// pointer leaves the anchor.
#[test]
fn a_popup_without_trigger_hover_is_left_to_the_program() {
    let mut scene = Islands::new(false, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.attached(S_A, POP_A);
    f.input(&at_x(S_BAR, 15.0), &mut scene);
    f.input(&leave(S_BAR), &mut scene);
    f.input(&enter(S_A, 10.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), []);
    assert_eq!(
        flags(&mut f),
        [
            hover(BAR_N, true),
            hover(ITEM_A, true),
            hover(ITEM_A, false),
            hover(ITEM_B, true),
            hover(ITEM_B, false),
            hover(BAR_N, false),
            hover(POP_A, true),
            hover(CARD_A, true),
            hover(SUB_ITEM, true),
        ]
    );
    assert_eq!(f.router.next_wake(), None);
}

/// Pick mode ends the graces (the popup closes at once) and opens
/// nothing.
#[test]
fn pick_mode_ends_the_hover_graces() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.attached(S_A, POP_A);
    f.input(&leave(S_BAR), &mut scene);
    assert!(f.router.next_wake().is_some());
    opens(&mut f);
    f.out.extend(f.router.set_picking(true, &mut scene));
    assert_eq!(opens(&mut f), [(POP_A, false)]);
    assert_eq!(f.router.next_wake(), None);
    f.input(&enter(S_BAR, 15.0), &mut scene);
    assert_eq!(opens(&mut f), [], "nothing opens while picking");
}

/// `grace:` sets how long a `trigger: hover` popup outlasts the
/// pointer: 400 ms here, so it is still open after the default 200 ms,
/// and closed after its own.
#[test]
fn a_hover_popup_takes_its_own_grace() {
    use std::time::Duration;
    let mut scene = Islands::new(true, None);
    let mut d = strand_scene::SceneDiff::new();
    d.set(
        POP_A,
        Prop::Grace,
        PropValue::Duration(Duration::from_millis(400)),
    );
    assert!(scene.tree.apply(d).is_empty());
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)]);
    let left = std::time::Instant::now();
    f.input(&leave(S_BAR), &mut scene);
    let due = f.router.next_wake().expect("the grace");
    assert!(due >= left + Duration::from_millis(400));
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [], "open past the default grace");
    std::thread::sleep(Duration::from_millis(200));
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), [(POP_A, false)]);
}

/// A diff setting `open` on popups, as logic sends it.
fn open_diff(popups: &[(NodeId, bool)]) -> strand_scene::SceneDiff {
    let mut d = strand_scene::SceneDiff::new();
    for (p, on) in popups {
        d.set(*p, Prop::Open, PropValue::Bool(*on));
    }
    d
}

/// (Review r2 minor 3) The router's open popups follow logic: its own
/// writes coming back change nothing, even late and in order (closed
/// then opened again); a `trigger: hover` popup logic opened itself (a
/// reload remounting it open) is taken on and closed after the grace,
/// one logic closed itself is not closed again, and one a reload made
/// the program's (`trigger` gone) is never written again.
#[test]
fn the_routers_open_popups_follow_logics_diffs() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    // Its own writes, answered late: a, then b, then a again.
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.input(&at_x(S_BAR, 15.0), &mut scene);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(
        opens(&mut f),
        [
            (POP_A, true),
            (POP_A, false),
            (POP_B, true),
            (POP_B, false),
            (POP_A, true)
        ]
    );
    for d in [
        open_diff(&[(POP_A, true)]),
        open_diff(&[(POP_A, false), (POP_B, true)]),
        open_diff(&[(POP_B, false), (POP_A, true)]),
    ] {
        f.router.observe(&d);
    }
    f.input(&at_x(S_BAR, 50.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, false)], "a, still the router's");
    f.router.observe(&open_diff(&[(POP_A, false)]));

    // Logic opens b itself, the pointer away: closed after the grace.
    f.router.observe(&open_diff(&[(POP_B, true)]));
    f.out.extend(f.router.tick(&mut scene));
    assert!(f.router.next_wake().is_some(), "b's grace runs");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_B, false)]);
    f.router.observe(&open_diff(&[(POP_B, false)]));

    // Logic closes a itself while the router holds it: not again.
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    f.router.observe(&open_diff(&[(POP_A, true)]));
    f.router.observe(&open_diff(&[(POP_A, false)]));
    f.input(&at_x(S_BAR, 50.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)], "no second close");

    // A reload makes b the program's while it is open: never written
    // again.
    f.input(&at_x(S_BAR, 15.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_B, true)]);
    f.router.observe(&open_diff(&[(POP_B, true)]));
    let mut d = strand_scene::SceneDiff::new();
    d.set(POP_B, Prop::Trigger, PropValue::Keyword("open".into()));
    assert!(scene.tree.apply(d).is_empty());
    f.input(&at_x(S_BAR, 50.0), &mut scene);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), []);
    assert_eq!(f.router.next_wake(), None);
}

/// (Review r2 minor 3) The compositor closing a `trigger: hover`
/// popup's surface (`popup_done`, told as a click away on it) closes it
/// in logic too, and it opens again only once the pointer has left its
/// anchor and come back.
#[test]
fn popup_done_closes_a_hover_popup_until_the_pointer_comes_back() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)]);
    f.attached(S_A, POP_A);
    f.input(&InputEvent::ClickAway { surface: S_A }, &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, false)]);
    f.input(&at_x(S_BAR, 6.0), &mut scene);
    assert_eq!(opens(&mut f), [], "not again under the same stay");
    assert_eq!(f.router.next_wake(), None);
    f.input(&at_x(S_BAR, 50.0), &mut scene);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)], "back on its anchor");
}

/// (Review r2 minor 4) A `delay` or `grace` too long for the clock
/// never comes due: no overflow, nothing to wake for, the popup stays
/// as it is.
#[test]
fn a_huge_delay_or_grace_never_comes_due() {
    use std::time::Duration;
    let mut scene = Islands::new(true, None);
    let mut d = strand_scene::SceneDiff::new();
    d.set(POP_A, Prop::Grace, PropValue::Duration(Duration::MAX))
        .set(POP_B, Prop::Delay, PropValue::Duration(Duration::MAX));
    assert!(scene.tree.apply(d).is_empty());
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)]);
    f.input(&leave(S_BAR), &mut scene);
    assert_eq!(f.router.next_wake(), None, "a grace that never ends");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [], "still open");
    // `b` waits for ever: going there closes a, opens nothing.
    f.input(&enter(S_BAR, 15.0), &mut scene);
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), []);
    assert_eq!(f.router.next_wake(), None);
}

/// (Review r2 minor 5) Pick mode ending gives the pointer its hover back
/// at once: the `trigger: hover` popup under it stays open, with no
/// grace started as if the pointer had gone.
#[test]
fn pick_mode_ending_rebuilds_the_hover_under_the_pointer() {
    let mut scene = Islands::new(true, None);
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.input(&at_x(S_BAR, 5.0), &mut scene);
    assert_eq!(opens(&mut f), [(POP_A, true)]);
    f.out.extend(f.router.set_picking(true, &mut scene));
    assert_eq!(opens(&mut f), [], "held: no grace ran");
    flags(&mut f);
    f.out.extend(f.router.set_picking(false, &mut scene));
    assert_eq!(f.router.next_wake(), None, "no grace starts");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), []);
    assert_eq!(flags(&mut f), [hover(BAR_N, true), hover(ITEM_A, true)]);
    assert_eq!(f.router.hovered(S_BAR), [ITEM_A, BAR_N]);
}

/// `SetProp { Hold }` on `popup`, as logic sends `open:` on a `trigger:
/// hover` popup.
fn hold_diff(popup: NodeId, on: bool) -> strand_scene::SceneDiff {
    let mut d = strand_scene::SceneDiff::new();
    d.set(popup, Prop::Hold, PropValue::Bool(on));
    d
}

/// (m6-owner-fixes, G-b) `open:` on a `trigger: hover` popup opens it
/// from code: at once (no `delay`), with the pointer nowhere near, and
/// it stays open past any grace while `open:` holds; once that is false
/// the pointer holds it as usual: it stays while the pointer is on it,
/// and closes the grace after the pointer leaves, or after `open:` lets
/// go with the pointer away.
#[test]
fn open_on_a_hover_popup_opens_and_holds_it() {
    let mut scene = Islands::new(true, Some(std::time::Duration::from_millis(150)));
    let mut f = R::default();
    f.attached(S_BAR, BAR_N);
    f.router.observe(&hold_diff(POP_B, true));
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), [(POP_B, true)], "at once, no delay");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [], "held past the grace");
    assert_eq!(f.router.next_wake(), None);

    // The pointer comes onto it, then the program lets go: it stays
    // while the pointer is there, and goes the grace after it leaves.
    let s_b = SurfaceId(3);
    f.attached(s_b, POP_B);
    f.input(&enter(s_b, 30.0), &mut scene);
    f.router.observe(&hold_diff(POP_B, false));
    f.out.extend(f.router.tick(&mut scene));
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [], "the pointer holds it");
    f.input(&leave(s_b), &mut scene);
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), [], "graced");
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_B, false)]);

    // Let go with the pointer nowhere: it closes after the grace.
    f.router.observe(&hold_diff(POP_B, true));
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), [(POP_B, true)]);
    f.router.observe(&hold_diff(POP_B, false));
    f.out.extend(f.router.tick(&mut scene));
    assert_eq!(opens(&mut f), []);
    after_grace(&mut f, &mut scene);
    assert_eq!(opens(&mut f), [(POP_B, false)]);
}

/// (M6) A panel holding two inputs (the first `focus: true`), the first a
/// password when `password`, on surface 1, the keyboard on it.
fn clipboard_scene(password: bool) -> (Renderer, SurfaceId, NodeId, R) {
    use strand_scene::{NodeKind, SceneDiff};
    let data = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(data),
    ]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let (panel, input, other) = (NodeId::new(0, 0), NodeId::new(1, 0), NodeId::new(2, 0));
    let mut d = SceneDiff::new();
    d.create(panel, NodeKind::Panel, None, 0)
        .set(panel, Prop::Width, PropValue::Number(200.0))
        .set(panel, Prop::Height, PropValue::Number(40.0))
        .create(input, NodeKind::Input, Some(panel), 0)
        .set(input, Prop::Focus, PropValue::Bool(true))
        .set(input, Prop::Text, PropValue::Text(String::new()))
        .create(other, NodeKind::Input, Some(panel), 1)
        .set(other, Prop::Text, PropValue::Text(String::new()));
    if password {
        d.set(
            input,
            Prop::InputType,
            PropValue::Keyword("password".into()),
        );
    }
    assert!(r.apply(d).is_empty());
    let s = SurfaceId(1);
    r.attach_surface(s, panel);
    let mut f = R::default();
    f.attached(s, panel);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    f.drain();
    (r, s, input, f)
}

fn key_with(s: SurfaceId, name: &str, text: &str, m: strand_scene::Modifiers) -> InputEvent {
    InputEvent::Key {
        surface: s,
        key: KeyInput {
            name: name.into(),
            text: text.into(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: m,
            time: 0,
            latin: None,
        },
    }
}

fn ctrl() -> strand_scene::Modifiers {
    strand_scene::Modifiers {
        ctrl: true,
        ..Default::default()
    }
}

fn shift() -> strand_scene::Modifiers {
    strand_scene::Modifiers {
        shift: true,
        ..Default::default()
    }
}

/// The `text` writes and clipboard asks among `out`.
fn text_writes_and_asks(out: Vec<Intent>) -> (Vec<String>, Vec<strand_scene::ClipboardAsk>) {
    let mut writes = Vec::new();
    let mut asks = Vec::new();
    for i in out {
        match i {
            Intent::Write {
                prop: Prop::Text,
                value: PropValue::Text(t),
                ..
            } => writes.push(t),
            Intent::Clipboard(a) => asks.push(a),
            _ => {}
        }
    }
    (writes, asks)
}

/// (M6) Ctrl+C asks the host to copy the focused input's selection (and
/// writes nothing); Ctrl+X asks the same and writes the text without it.
#[test]
fn ctrl_c_and_ctrl_x_ask_to_copy_the_inputs_selection() {
    use strand_scene::ClipboardAsk;
    let (mut r, s, _, mut f) = clipboard_scene(false);
    for k in ["a", "b", "c"] {
        f.input(&key_with(s, k, k, Default::default()), &mut r);
    }
    // Shift+Left selects the "c".
    f.input(&key_with(s, "Left", "", shift()), &mut r);
    f.drain();
    f.input(&key_with(s, "c", "c", ctrl()), &mut r);
    let out = f.drain();
    assert!(
        out.iter().any(|i| matches!(
            i,
            Intent::Event {
                event: NodeEvent::Key { name, .. },
                ..
            } if name == "c"
        )),
        "`on key` still hears Ctrl+C"
    );
    let (writes, asks) = text_writes_and_asks(out);
    assert!(writes.is_empty(), "a copy writes nothing: {writes:?}");
    assert_eq!(
        asks,
        [ClipboardAsk::Copy {
            surface: s,
            text: "c".into()
        }]
    );
    f.input(&key_with(s, "x", "x", ctrl()), &mut r);
    let (writes, asks) = text_writes_and_asks(f.drain());
    assert_eq!(
        asks,
        [ClipboardAsk::Copy {
            surface: s,
            text: "c".into()
        }]
    );
    assert_eq!(writes, ["ab"], "a cut writes the text without it");
    // Nothing selected: Ctrl+C asks nothing.
    f.input(&key_with(s, "c", "c", ctrl()), &mut r);
    assert!(text_writes_and_asks(f.drain()).1.is_empty());
}

/// (M6) Ctrl+V asks the host for the selection; the paste that comes back
/// lands at the caret, replacing the selection, as one line.
#[test]
fn ctrl_v_asks_to_paste_and_the_paste_lands_at_the_caret() {
    use strand_scene::ClipboardAsk;
    let (mut r, s, input, mut f) = clipboard_scene(false);
    for k in ["a", "b"] {
        f.input(&key_with(s, k, k, Default::default()), &mut r);
    }
    f.input(&key_with(s, "Left", "", Default::default()), &mut r);
    f.drain();
    f.input(&key_with(s, "v", "v", ctrl()), &mut r);
    let (writes, asks) = text_writes_and_asks(f.drain());
    assert!(writes.is_empty());
    assert_eq!(asks, [ClipboardAsk::Paste { surface: s }]);
    f.input(
        &InputEvent::Paste {
            surface: s,
            text: "X\nY".into(),
        },
        &mut r,
    );
    let (writes, _) = text_writes_and_asks(f.drain());
    assert_eq!(writes, ["aX Yb"]);
    assert_eq!(
        r.caret(input),
        Some(strand_render::widgets::Caret::at(4)),
        "the caret after the paste"
    );
    // A paste nobody asked for is dropped.
    f.input(
        &InputEvent::Paste {
            surface: s,
            text: "Z".into(),
        },
        &mut r,
    );
    assert!(text_writes_and_asks(f.drain()).0.is_empty());
    // Shift+Insert asks too.
    f.input(&key_with(s, "Insert", "", shift()), &mut r);
    assert_eq!(
        text_writes_and_asks(f.drain()).1,
        [ClipboardAsk::Paste { surface: s }]
    );
}

/// (M6, review r1) With a non-Latin layout active, Ctrl+C, Ctrl+X, Ctrl+V
/// and Ctrl+A match the key's Latin keysym (`KeyInput::latin`, from
/// strand-surface): `ru`'s keys under C, X, V and A copy, cut, paste and
/// select all, and the same keys without Ctrl type their own letters;
/// `on key` still hears the active layout's name.
#[test]
fn a_non_latin_layouts_shortcuts_match_their_latin_keys() {
    use strand_scene::ClipboardAsk;
    let (mut r, s, _, mut f) = clipboard_scene(false);
    let ru = |name: &str, text: &str, latin: &str, m| {
        let mut e = key_with(s, name, text, m);
        if let InputEvent::Key { key, .. } = &mut e {
            key.latin = Some(latin.into());
        }
        e
    };
    // Typed without Ctrl: the active layout's letters.
    f.input(&ru("Cyrillic_es", "с", "c", Default::default()), &mut r);
    f.input(&ru("Cyrillic_em", "м", "v", Default::default()), &mut r);
    let (writes, asks) = text_writes_and_asks(f.drain());
    assert_eq!(writes.last().map(String::as_str), Some("см"));
    assert!(asks.is_empty());
    // Ctrl+A (`Cyrillic_ef`) selects all, Ctrl+C (`Cyrillic_es`) copies it.
    f.input(&ru("Cyrillic_ef", "ф", "a", ctrl()), &mut r);
    f.input(&ru("Cyrillic_es", "с", "c", ctrl()), &mut r);
    let out = f.drain();
    assert!(
        out.iter().any(|i| matches!(
            i,
            Intent::Event {
                event: NodeEvent::Key { name, .. },
                ..
            } if name == "Cyrillic_es"
        )),
        "`on key` hears the active layout's name"
    );
    let (writes, asks) = text_writes_and_asks(out);
    assert!(writes.is_empty(), "{writes:?}");
    assert_eq!(
        asks,
        [ClipboardAsk::Copy {
            surface: s,
            text: "см".into()
        }]
    );
    // Ctrl+X (`Cyrillic_che`) cuts; Ctrl+V (`Cyrillic_em`) pastes.
    f.input(&ru("Cyrillic_che", "ч", "x", ctrl()), &mut r);
    let (writes, asks) = text_writes_and_asks(f.drain());
    assert_eq!(writes, [""]);
    assert_eq!(
        asks,
        [ClipboardAsk::Copy {
            surface: s,
            text: "см".into()
        }]
    );
    f.input(&ru("Cyrillic_em", "м", "v", ctrl()), &mut r);
    let (_, asks) = text_writes_and_asks(f.drain());
    assert_eq!(asks, [ClipboardAsk::Paste { surface: s }]);
}

/// (M6) A paste arriving while keys are still in flight to logic builds on
/// them, as a key does.
#[test]
fn a_paste_builds_on_keys_still_in_flight() {
    let (mut r, s, _, mut f) = clipboard_scene(false);
    // Logic has answered nothing: the scene still shows "".
    for k in ["a", "b"] {
        f.input(&key_with(s, k, k, Default::default()), &mut r);
    }
    f.input(&key_with(s, "v", "v", ctrl()), &mut r);
    f.drain();
    f.input(
        &InputEvent::Paste {
            surface: s,
            text: "cd".into(),
        },
        &mut r,
    );
    f.input(&key_with(s, "e", "e", Default::default()), &mut r);
    let (writes, _) = text_writes_and_asks(f.drain());
    assert_eq!(writes, ["abcd", "abcde"]);
}

/// (M6) A paste that arrives after the focus moved on (the keyboard left
/// and came back) is dropped, so it never lands where nobody asked.
#[test]
fn a_paste_after_focus_moved_is_dropped() {
    let (mut r, s, _, mut f) = clipboard_scene(false);
    f.input(&key_with(s, "v", "v", ctrl()), &mut r);
    f.drain();
    f.input(&InputEvent::KeyboardLeave { surface: s }, &mut r);
    f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
    f.drain();
    f.input(
        &InputEvent::Paste {
            surface: s,
            text: "late".into(),
        },
        &mut r,
    );
    assert!(
        text_writes_and_asks(f.drain()).0.is_empty(),
        "focus left and came back: the paste is dropped"
    );
}

/// (M6) A password input asks for no copy or cut, but takes a paste.
#[test]
fn a_password_input_asks_for_no_copy() {
    use strand_scene::ClipboardAsk;
    let (mut r, s, _, mut f) = clipboard_scene(true);
    for k in ["p", "w"] {
        f.input(&key_with(s, k, k, Default::default()), &mut r);
    }
    f.input(&key_with(s, "a", "a", ctrl()), &mut r);
    f.drain();
    for (name, m) in [
        ("c", ctrl()),
        ("Insert", ctrl()),
        ("x", ctrl()),
        ("Delete", shift()),
    ] {
        f.input(&key_with(s, name, "", m), &mut r);
        let (writes, asks) = text_writes_and_asks(f.drain());
        assert!(asks.is_empty(), "{name}: {asks:?}");
        assert!(writes.is_empty(), "{name}: {writes:?}");
    }
    f.input(&key_with(s, "v", "v", ctrl()), &mut r);
    assert_eq!(
        text_writes_and_asks(f.drain()).1,
        [ClipboardAsk::Paste { surface: s }]
    );
}

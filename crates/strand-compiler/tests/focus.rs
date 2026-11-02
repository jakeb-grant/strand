//! (keyboard, piece 4) Focus on any node, `node.focus()`, the keys of what is
//! focused, arrows by geometry where a container asks, and a `scroll` moved
//! and read from code: whole shells driven through the host loop
//! (`tests/shell`).

mod shell;

use shell::Shell;
use strand_compiler::vm::Value;
use strand_scene::{ButtonState, InputEvent, KeyInput, Modifiers, NodeId, NodeKind, Painter, Prop};

fn entered(src: &str) -> Shell {
    let mut sh = Shell::new(src);
    sh.keyboard_to("P");
    sh.settle();
    sh
}

/// `n` on `self.focused`, for finding what has focus by what it says.
fn tell(n: u32) -> String {
    format!("on change self.focused {{ if self.focused {{ at = {n} }} }}")
}

fn focused(sh: &Shell) -> Option<NodeId> {
    sh.router.focused(sh.keyboard.unwrap())
}

fn tap(sh: &mut Shell, name: &str) {
    sh.key(name, false);
    sh.settle();
}

fn key_with(sh: &Shell, name: &str, text: &str, repeat: bool, ctrl: bool) -> InputEvent {
    InputEvent::Key {
        surface: sh.keyboard.unwrap(),
        key: KeyInput {
            name: name.into(),
            text: text.into(),
            state: ButtonState::Pressed,
            repeat,
            modifiers: Modifiers {
                ctrl,
                ..Modifiers::default()
            },
            time: 0,
            latin: None,
        },
    }
}

fn offset(sh: &Shell, node: NodeId) -> f32 {
    sh.r.scroll_state(node).map_or(0.0, |s| s.offset)
}

fn scroll_node(sh: &Shell) -> NodeId {
    sh.nodes("P", NodeKind::Scroll)[0]
}

// Focus on any node.

/// Tab and Shift+Tab walk everything focusable in tree order, round the ends,
/// past what takes no input or is not shown.
#[test]
fn tab_walks_mixed_elements_in_tree_order() {
    let mut sh = entered(&format!(
        "state at = 0\nstate v = 0.5\nstate pick = \"a\"\n\
         panel P {{ anchor: top_left; width: 300; height: 400; keyboard: exclusive\n\
           col {{\n\
             input {{ {} }}\n\
             text \"not me\"\n\
             button \"b\" {{ {} }}\n\
             box {{ height: 10; focus: false; {} }}\n\
             slider {{ value: <-> v; {} }}\n\
             box {{ height: 10; hit: none; focus: false; {} }}\n\
             button \"gone\" {{ hit: none; {} }}\n\
             box {{ opacity: 0; button \"hidden\" {{ {} }} }}\n\
             segmented {{ options: [\"a\", \"b\"]; value: <-> pick; {} }}\n\
             list {{ height: 20; {} }}\n\
             scroll {{ height: 20; focus: false; {} col {{ box {{ height: 50 }} }} }}\n\
             box {{ height: 10 }}\n\
           }}\n\
         }}\n",
        tell(1),
        tell(2),
        tell(3),
        tell(4),
        tell(90),
        tell(91),
        tell(92),
        tell(5),
        tell(6),
        tell(7),
    ));
    let mut seen = Vec::new();
    for _ in 0..8 {
        tap(&mut sh, "Tab");
        seen.push(sh.num("at") as u32);
    }
    assert_eq!(seen, [1, 2, 3, 4, 5, 6, 7, 1]);
    let mut seen = Vec::new();
    for _ in 0..3 {
        tap(&mut sh, "ISO_Left_Tab");
        seen.push(sh.num("at") as u32);
    }
    assert_eq!(seen, [7, 6, 5]);
}

/// A node on its way out is passed by, as `exit` plays.
#[test]
fn tab_skips_a_node_playing_its_exit() {
    let mut sh = entered(&format!(
        "state at = 0\nstate show = true\n\
         panel P {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           col {{\n\
             button \"a\" {{ {} }}\n\
             if show {{ button \"b\" {{ exit {{ opacity: 0 }}; {} }} }}\n\
             button \"c\" {{ {} }}\n\
           }}\n\
         }}\n",
        tell(1),
        tell(2),
        tell(3),
    ));
    sh.set("show", strand_compiler::vm::Value::Bool(false));
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 1.0);
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 3.0);
}

/// `when focused` follows focus on each kind of node, and a click reaches a
/// node with `focus:`.
#[test]
fn when_focused_holds_on_every_kind() {
    let mut sh = entered(
        "state v = 0.5\n\
         panel P { anchor: top_left; width: 300; height: 200; keyboard: exclusive\n\
           col {\n\
             button \"b\" { height: 20; when focused { opacity: 0.5 } }\n\
             slider { height: 20; value: <-> v; when focused { opacity: 0.5 } }\n\
             box { height: 20; focus: false; when focused { opacity: 0.5 } }\n\
           }\n\
         }\n",
    );
    let col = sh.nodes("P", NodeKind::Col)[0];
    let kids = sh.r.tree().get(col).unwrap().children.clone();
    let dim = |sh: &Shell, n: NodeId| {
        sh.r.tree()
            .get(n)
            .and_then(|n| n.get(Prop::Opacity))
            .is_some_and(|o| *o == strand_scene::PropValue::Number(0.5))
    };
    for (i, n) in kids.iter().enumerate() {
        tap(&mut sh, "Tab");
        for (j, m) in kids.iter().enumerate() {
            assert_eq!(dim(&sh, *m), i == j, "at {i}, {j}");
        }
        assert_eq!(focused(&sh), Some(*n));
    }
    sh.click(5.0, 5.0);
    sh.settle();
    assert_eq!(focused(&sh), Some(kids[0]), "a click on the button");
    sh.click(5.0, 45.0);
    sh.settle();
    assert_eq!(focused(&sh), Some(kids[2]), "a click on the box");
}

/// `focus: cond` moves focus when it turns true on any element.
#[test]
fn a_written_focus_turning_true_moves_focus_on_any_element() {
    let mut sh = entered(
        "state go = false\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           box { height: 20; focus: go }\n\
         }\n",
    );
    let b = sh.nodes("P", NodeKind::Box)[0];
    assert_ne!(focused(&sh), Some(b));
    sh.set("go", strand_compiler::vm::Value::Bool(true));
    assert_eq!(focused(&sh), Some(b));
}

// `node.focus()`.

/// In the order they were asked, with the selection's asks among them.
#[test]
fn focus_asks_apply_in_order_with_the_others() {
    let mut sh = entered(
        "state items: [int] = [0, 1, 2, 3, 4, 5]\n\
         state at = -1\n\
         panel P { anchor: top_left; width: 300; height: 200; keyboard: exclusive\n\
           on key \"f1\" { a.focus(); b.focus() }\n\
           on key \"f2\" { b.focus(); a.focus() }\n\
           on key \"f3\" { field.focus(); results.select(3) }\n\
           on change results.selected_index { at = results.selected_index ?? -1 }\n\
           col {\n\
             box { id: a; height: 20 }\n\
             box { id: b; height: 20 }\n\
             input { id: field; nav: results }\n\
             list { id: results; height: 60\n\
               for i in items key i { box { height: 20 } }\n\
             }\n\
           }\n\
         }\n",
    );
    let boxes = sh.nodes("P", NodeKind::Box);
    tap(&mut sh, "F1");
    assert_eq!(focused(&sh), Some(boxes[1]));
    tap(&mut sh, "F2");
    assert_eq!(focused(&sh), Some(boxes[0]));
    tap(&mut sh, "F3");
    assert_eq!(focused(&sh), Some(sh.nodes("P", NodeKind::Input)[0]));
    assert_eq!(sh.num("at"), 3.0, "the first-row rule did not undo select");
}

/// `s.scroll_to` and `reveal` in either order: the later one stands.
#[test]
fn scroll_asks_apply_in_order() {
    let mut sh = entered(
        "panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on key \"f1\" { s.scroll_to(300); top.reveal() }\n\
           on key \"f2\" { top.reveal(); s.scroll_to(300) }\n\
           scroll { id: s; height: 100\n\
             col { box { id: top; height: 20 } box { height: 1000 } }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    tap(&mut sh, "F2");
    assert_eq!(offset(&sh, s), 300.0);
    tap(&mut sh, "F1");
    assert_eq!(offset(&sh, s), 0.0);
}

/// The focused node going in the same tick as a `focus()` loses nothing: its
/// surface still holds the keyboard.
#[test]
fn focus_asked_as_the_focused_node_goes_lands() {
    let mut sh = entered(
        "state level = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on key \"f1\" { level = 1; other.focus() }\n\
           col {\n\
             if level == 0 { button \"one\" { focus: true } }\n\
             box { id: other; height: 20 }\n\
           }\n\
         }\n",
    );
    assert_eq!(
        sh.r.tree().get(focused(&sh).unwrap()).map(|n| n.kind),
        Some(NodeKind::Button)
    );
    tap(&mut sh, "F1");
    assert_eq!(focused(&sh), Some(sh.nodes("P", NodeKind::Box)[0]));
}

/// Asked before the keyboard comes, it takes it when it does, ahead of `focus:
/// true`.
#[test]
fn focus_asked_without_the_keyboard_waits_for_it() {
    let mut sh = Shell::new(
        "state go = false\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: on_demand\n\
           on change go { b.focus() }\n\
           col {\n\
             input { focus: true }\n\
             box { id: b; height: 20 }\n\
           }\n\
         }\n",
    );
    sh.settle();
    sh.set("go", strand_compiler::vm::Value::Bool(true));
    sh.keyboard_to("P");
    sh.settle();
    assert_eq!(focused(&sh), Some(sh.nodes("P", NodeKind::Box)[0]));
}

// Return and space.

#[test]
fn return_and_space_click_a_focused_button() {
    let mut sh = entered(
        "state clicks = 0\nstate up = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on click { up += 1 }\n\
           col {\n\
             button \"b\" { focus: true; on click { clicks += 1 } }\n\
             button \"bubbles\" { }\n\
           }\n\
         }\n",
    );
    tap(&mut sh, "Return");
    assert_eq!(sh.num("clicks"), 1.0);
    tap(&mut sh, "space");
    assert_eq!(sh.num("clicks"), 2.0);
    tap(&mut sh, "KP_Enter");
    assert_eq!(sh.num("clicks"), 3.0);
    // Held, and with Ctrl: no click.
    let e = key_with(&sh, "Return", "", true, false);
    sh.send(e);
    let e = key_with(&sh, "Return", "", false, true);
    sh.send(e);
    sh.settle();
    assert_eq!(sh.num("clicks"), 3.0);
    assert_eq!(sh.num("up"), 0.0);
    // No handler of its own: out, as a pointer's click goes.
    tap(&mut sh, "Tab");
    tap(&mut sh, "Return");
    assert_eq!(sh.num("up"), 1.0);
}

/// `on activate` first, its `when` counting, and on any node.
#[test]
fn return_prefers_activate_over_click() {
    let mut sh = entered(
        "state clicks = 0\nstate acts = 0\nstate on = true\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           box { height: 20; focus: true\n\
             on click { clicks += 1 }\n\
             when on { on activate { acts += 1 } }\n\
           }\n\
         }\n",
    );
    tap(&mut sh, "Return");
    assert_eq!((sh.num("acts"), sh.num("clicks")), (1.0, 0.0));
    sh.set("on", strand_compiler::vm::Value::Bool(false));
    tap(&mut sh, "space");
    assert_eq!((sh.num("acts"), sh.num("clicks")), (1.0, 1.0));
}

/// The keyboard's `on activate` is not the mouse's: off a list's row a
/// pointer's click runs `on click` and nothing else.
#[test]
fn a_pointers_click_runs_no_activate_off_a_list_row() {
    let mut sh = entered(
        "state clicks = 0\nstate acts = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           button \"b\" { width: 100; height: 40\n\
             on click { clicks += 1 }\n\
             on activate { acts += 1 }\n\
           }\n\
         }\n",
    );
    sh.click(10.0, 10.0);
    sh.settle();
    assert_eq!((sh.num("acts"), sh.num("clicks")), (0.0, 1.0));
}

/// A button in a list's row: its click, and the row chosen, as a pointer's.
#[test]
fn return_on_a_button_in_a_row_activates_the_row() {
    let mut sh = entered(
        "state clicks = 0\nstate acts = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           list { height: 100\n\
             box { height: 20; on activate { acts += 1 }\n\
               button \"b\" { focus: true; on click { clicks += 1 } }\n\
             }\n\
           }\n\
         }\n",
    );
    tap(&mut sh, "Return");
    assert_eq!((sh.num("acts"), sh.num("clicks")), (1.0, 1.0));
}

/// Nothing focused: the surface's own click does not fire.
#[test]
fn return_on_the_surface_itself_clicks_nothing() {
    let mut sh = entered(
        "state clicks = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on click { clicks += 1 }\n\
           text \"x\"\n\
         }\n",
    );
    tap(&mut sh, "Return");
    assert_eq!(sh.num("clicks"), 0.0);
}

/// A key a `slider` or `segmented` doesn't take is a press like any other
/// node's, bound two-way or not.
#[test]
fn return_and_space_press_a_slider_and_a_segmented() {
    for control in [
        "slider { focus: true; value: <-> v; range: 0, 100; on click { clicks += 1 } }",
        "slider { focus: true; value: v; range: 0, 100; on click { clicks += 1 } }",
        "segmented { focus: true; options: [\"a\", \"b\"]; value: <-> pick; on click { clicks += 1 } }",
        "segmented { focus: true; options: [\"a\", \"b\"]; value: pick; on click { clicks += 1 } }",
    ] {
        let mut sh = entered(&format!(
            "state clicks = 0\nstate v = 50.0\nstate pick = \"a\"\n\
             panel P {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
               {control}\n\
             }}\n"
        ));
        tap(&mut sh, "Return");
        assert_eq!(sh.num("clicks"), 1.0, "Return on {control}");
        tap(&mut sh, "space");
        assert_eq!(sh.num("clicks"), 2.0, "space on {control}");
    }
}

// A slider's and a segmented's keys.

fn slider_shell(props: &str) -> Shell {
    entered(&format!(
        "state v = 50.0\n\
         panel P {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           slider {{ focus: true; {props} }}\n\
         }}\n"
    ))
}

#[test]
fn slider_keys_write_as_a_drag_does() {
    let mut sh = slider_shell("value: <-> v; range: 0, 100; step: 5");
    for (key, want) in [
        ("Right", 55.0),
        ("Up", 60.0),
        ("Left", 55.0),
        ("Down", 50.0),
        ("Page_Up", 100.0),
        ("Page_Down", 50.0),
        ("Home", 0.0),
        ("Left", 0.0),
        ("End", 100.0),
        ("Right", 100.0),
    ] {
        tap(&mut sh, key);
        assert_eq!(sh.num("v"), want, "after {key}");
    }
}

/// Without `step:` a hundredth of the range; a decimal step writes what it
/// spells.
#[test]
fn slider_keys_read_step_and_range() {
    let mut sh = slider_shell("value: <-> v; range: 0, 200");
    tap(&mut sh, "Right");
    assert_eq!(sh.num("v"), 52.0);
    tap(&mut sh, "Page_Down");
    assert_eq!(sh.num("v"), 32.0);
    let mut sh = entered(
        "state v = 0.8\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           slider { focus: true; value: <-> v; step: 0.1 }\n\
         }\n",
    );
    tap(&mut sh, "Right");
    assert!((sh.num("v") - 0.9).abs() < 1e-6, "{}", sh.num("v"));
}

/// A slider that shows a value writes nothing.
#[test]
fn a_display_slider_ignores_its_keys() {
    let mut sh = slider_shell("value: v; range: 0, 100");
    tap(&mut sh, "Right");
    assert_eq!(sh.num("v"), 50.0);
}

/// Keys queued while logic is busy all count.
#[test]
fn queued_slider_keys_all_count() {
    let mut sh = slider_shell("value: <-> v; range: 0, 100; step: 5");
    for _ in 0..3 {
        let e = key_with(&sh, "Right", "", true, false);
        sh.queue(e);
    }
    sh.settle();
    assert_eq!(sh.num("v"), 65.0);
    // Answered, and logic's own value next: the keys build on that.
    sh.set("v", strand_compiler::vm::Value::float(10.0));
    tap(&mut sh, "Right");
    assert_eq!(sh.num("v"), 15.0);
}

fn segmented_shell() -> Shell {
    entered(
        "state pick = \"a\"\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           segmented { focus: true; options: [\"a\", \"b\", \"c\"]; value: <-> pick }\n\
         }\n",
    )
}

fn pick(sh: &Shell) -> String {
    format!("{:?}", sh.get("pick"))
}

#[test]
fn segmented_keys_change_the_choice() {
    let mut sh = segmented_shell();
    let at = |c: &str| format!("{:?}", strand_compiler::vm::Value::text(c));
    tap(&mut sh, "Left");
    assert_eq!(pick(&sh), at("a"), "stops at the start");
    tap(&mut sh, "Right");
    assert_eq!(pick(&sh), at("b"));
    tap(&mut sh, "Right");
    tap(&mut sh, "Right");
    assert_eq!(pick(&sh), at("c"), "stops at the end");
    tap(&mut sh, "Left");
    assert_eq!(pick(&sh), at("b"));
    let e = key_with(&sh, "Right", "", true, false);
    sh.queue(e);
    let e = key_with(&sh, "Left", "", true, false);
    sh.queue(e);
    let e = key_with(&sh, "Left", "", true, false);
    sh.queue(e);
    sh.settle();
    assert_eq!(pick(&sh), at("a"), "queued, each from the last");
}

// A claim beats them all.

#[test]
fn a_claim_beats_each_default() {
    let mut sh = entered(
        "state v = 0.5\nstate pick = \"a\"\nstate won = 0\nstate clicks = 0\n\
         panel P { anchor: top_left; width: 300; height: 300; keyboard: exclusive\n\
           on key \"f1\" { sl.focus() }\n\
           on key \"f2\" { seg.focus() }\n\
           on key \"f3\" { s.focus() }\n\
           col { arrows: focus\n\
             button \"b\" { focus: true; on click { clicks += 1 }\n\
               on key \"enter\", \"space\", \"tab\", \"down\" { won += 1 }\n\
             }\n\
             slider { id: sl; value: <-> v; on key \"right\", \"home\" { won += 1 } }\n\
             segmented { id: seg; options: [\"a\", \"b\"]; value: <-> pick\n\
               on key \"right\" { won += 1 }\n\
             }\n\
             scroll { id: s; height: 20; focus: false\n\
               on key \"down\", \"end\" { won += 1 }\n\
               col { box { height: 500 } }\n\
             }\n\
           }\n\
         }\n",
    );
    let first = focused(&sh);
    let mut want = 0.0;
    let mut claimed = |sh: &mut Shell, keys: &[&str]| {
        for k in keys {
            tap(sh, k);
            want += 1.0;
            assert_eq!(sh.num("won"), want, "{k}");
        }
    };
    claimed(&mut sh, &["Return", "space", "Tab", "Down"]);
    assert_eq!(sh.num("clicks"), 0.0);
    assert_eq!(focused(&sh), first, "neither Tab nor Down moved it");
    tap(&mut sh, "F1");
    claimed(&mut sh, &["Right", "Home"]);
    assert_eq!(sh.num("v"), 0.5);
    tap(&mut sh, "F2");
    claimed(&mut sh, &["Right"]);
    assert_eq!(
        pick(&sh),
        format!("{:?}", strand_compiler::vm::Value::text("a"))
    );
    tap(&mut sh, "F3");
    claimed(&mut sh, &["Down", "End"]);
    assert_eq!(offset(&sh, scroll_node(&sh)), 0.0);
}

// Arrows by geometry.

/// `tiles` is the tree under a container with `props`; each `box` with a
/// `tell`. Starts with focus on the box told `1`.
fn tiles(props: &str, tree: &str) -> Shell {
    let mut sh = entered(&format!(
        "state at = 0\n\
         panel P {{ anchor: top_left; width: 300; height: 300; keyboard: exclusive\n\
           col {{ {props}\n\
             {tree}\n\
           }}\n\
         }}\n"
    ));
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 1.0);
    sh
}

fn tile(n: u32, w: u32, h: u32) -> String {
    format!(
        "box {{ width: {w}; height: {h}; focus: false; {} }}",
        tell(n)
    )
}

fn walk(sh: &mut Shell, steps: &[(&str, u32)]) {
    for (k, want) in steps {
        tap(sh, k);
        assert_eq!(sh.num("at"), f64::from(*want), "after {k}");
    }
}

#[test]
fn arrows_walk_a_row_without_wrapping() {
    let row = format!(
        "row {{ {} {} {} }}",
        tile(1, 50, 50),
        tile(2, 50, 50),
        tile(3, 50, 50)
    );
    let mut sh = tiles("arrows: focus", &row);
    walk(
        &mut sh,
        &[
            ("Left", 1),
            ("Right", 2),
            ("Right", 3),
            ("Right", 3),
            ("Left", 2),
            ("Up", 2),
            ("Down", 2),
        ],
    );
}

#[test]
fn arrows_walk_a_column_and_wrap_when_asked() {
    let col = format!(
        "{} {} {}",
        tile(1, 50, 50),
        tile(2, 50, 50),
        tile(3, 50, 50)
    );
    let mut sh = tiles("arrows: focus_wrap", &col);
    walk(
        &mut sh,
        &[
            ("Down", 2),
            ("Down", 3),
            ("Down", 1),
            ("Up", 3),
            ("Right", 3),
        ],
    );
}

/// Tiles of uneven sizes: 1 is tall beside 2 over 3, with 4 wide below.
#[test]
fn arrows_find_the_nearest_among_uneven_tiles() {
    let tree = format!(
        "row {{ {} col {{ {} {} }} }} {}",
        tile(1, 100, 100),
        tile(2, 50, 50),
        tile(3, 50, 50),
        tile(4, 150, 50),
    );
    let mut sh = tiles("arrows: focus", &tree);
    walk(
        &mut sh,
        &[
            ("Right", 2),
            ("Down", 3),
            ("Down", 4),
            // Under both 1 and 3: the one nearer its middle.
            ("Up", 1),
            ("Right", 2),
            ("Left", 1),
            ("Down", 4),
        ],
    );
}

/// A grid laid out with rows of unequal counts: nothing past is nothing.
#[test]
fn arrows_go_to_a_diagonal_only_when_nothing_is_level() {
    let tree = format!(
        "row {{ {} }} row {{ box {{ width: 100; height: 50 }} {} }}",
        tile(1, 50, 50),
        tile(2, 50, 50),
    );
    let mut sh = tiles("arrows: focus", &tree);
    walk(&mut sh, &[("Right", 2), ("Left", 1)]);
}

/// The nearest container with `arrows:` holds them in, and what is in another
/// is out of its reach.
#[test]
fn arrows_stay_inside_the_nearest_container() {
    let tree = format!(
        "{} row {{ arrows: focus; {} {} }} {}",
        tile(1, 50, 50),
        tile(2, 50, 50),
        tile(3, 50, 50),
        tile(4, 50, 50),
    );
    let mut sh = tiles("arrows: focus", &tree);
    walk(&mut sh, &[("Down", 4), ("Up", 1)]);
    tap(&mut sh, "Tab");
    walk(
        &mut sh,
        &[("Right", 3), ("Down", 3), ("Up", 3), ("Left", 2)],
    );
    walk(&mut sh, &[("Left", 2), ("Up", 2)]);
}

/// Without the prop, an arrow on a button moves nothing.
#[test]
fn no_container_no_geometry() {
    let mut sh = tiles("", &format!("{} {}", tile(1, 50, 50), tile(2, 50, 50)));
    walk(&mut sh, &[("Down", 1)]);
}

/// A focused `input`'s caret, a `list`'s rows and a `slider` keep the arrows.
#[test]
fn elements_with_their_own_arrows_keep_them() {
    let mut sh = entered(&format!(
        "state q = \"abc\"\nstate v = 0.5\nstate at = 0\nstate pick = \"a\"\n\
         state items: [int] = [0, 1, 2]\n\
         panel P {{ anchor: top_left; width: 300; height: 300; keyboard: exclusive\n\
           col {{ arrows: focus\n\
             input {{ text: <-> q; {} }}\n\
             list {{ height: 60; {}\n\
               for i in items key i {{ box {{ height: 20 }} }}\n\
             }}\n\
             slider {{ value: <-> v; {} }}\n\
             segmented {{ options: [\"a\", \"b\"]; value: <-> pick; {} }}\n\
             {}\n\
           }}\n\
         }}\n",
        tell(1),
        tell(2),
        tell(3),
        tell(4),
        tile(5, 50, 50),
    ));
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 1.0);
    // The input's caret moves sideways; with no `nav:`, Down is free.
    tap(&mut sh, "Left");
    assert_eq!(sh.num("at"), 1.0);
    tap(&mut sh, "Down");
    assert_eq!(sh.num("at"), 2.0);
    let list = sh.nodes("P", NodeKind::List)[0];
    tap(&mut sh, "Down");
    tap(&mut sh, "Down");
    assert_eq!(sh.num("at"), 2.0);
    assert_eq!(sh.router.selected_index(list), Some(1));
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 3.0);
    tap(&mut sh, "Down");
    assert_eq!(sh.num("at"), 3.0);
    assert!(sh.num("v") < 0.5);
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 4.0);
    // A segmented's Down is free, its Up too.
    tap(&mut sh, "Down");
    assert_eq!(sh.num("at"), 5.0);
}

/// A move by the arrows into what a `scroll` hides shows it.
#[test]
fn arrows_reveal_what_they_reach() {
    let boxes: String = (1..=6)
        .map(|i| tile(i, 50, 50))
        .collect::<Vec<_>>()
        .join(" ");
    let mut sh = entered(&format!(
        "state at = 0\n\
         panel P {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           scroll {{ height: 100; arrows: focus\n\
             col {{ {boxes} }}\n\
           }}\n\
         }}\n"
    ));
    tap(&mut sh, "Tab");
    assert_eq!(sh.num("at"), 1.0);
    let s = scroll_node(&sh);
    tap(&mut sh, "Down");
    assert_eq!(offset(&sh, s), 0.0, "already shown");
    tap(&mut sh, "Down");
    assert_eq!(sh.num("at"), 3.0);
    assert_eq!(offset(&sh, s), 50.0, "the least it takes");
}

// A scroll from code and from keys.

fn scroller(props: &str) -> Shell {
    entered(&format!(
        "state y = -1.0\nstate x = -1.0\n\
         panel P {{ anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" {{ s.scroll_to(300) }}\n\
           on key \"f2\" {{ s.scroll_by(40) }}\n\
           on key \"f3\" {{ s.scroll_by(-5000) }}\n\
           on key \"f4\" {{ s.scroll_to(100000) }}\n\
           on key \"f5\" {{ s.scroll_to(x: 70) }}\n\
           on key \"f6\" {{ s.scroll_to(20, 30) }}\n\
           on key \"f7\" {{ s.scroll_by(dx: 10) }}\n\
           on change s.scroll_y {{ y = s.scroll_y }}\n\
           on change s.scroll_x {{ x = s.scroll_x }}\n\
           scroll {{ id: s; height: 100; width: 200; {props}\n\
             box {{ height: 1000; width: 800 }}\n\
           }}\n\
         }}\n"
    ))
}

#[test]
fn a_focused_scroll_takes_its_keys() {
    let mut sh = scroller("focus: true");
    let s = scroll_node(&sh);
    for (k, want) in [
        ("Down", 40.0),
        ("Down", 80.0),
        ("Up", 40.0),
        ("Page_Down", 140.0),
        ("Page_Up", 40.0),
        ("End", 900.0),
        ("Down", 900.0),
        ("Home", 0.0),
        ("Up", 0.0),
    ] {
        tap(&mut sh, k);
        assert_eq!(offset(&sh, s), want, "after {k}");
    }
    tap(&mut sh, "Right");
    assert_eq!(offset(&sh, s), 0.0, "a vertical one has no x");
}

#[test]
fn a_horizontal_scroll_takes_its_keys_along_x() {
    let mut sh = scroller("focus: true; axis: horizontal");
    let s = scroll_node(&sh);
    let x = |sh: &Shell| {
        sh.r.scroll_state(s)
            .and_then(|st| st.across.as_ref())
            .map_or(0.0, |a| a.offset)
    };
    tap(&mut sh, "Right");
    assert_eq!(x(&sh), 40.0);
    tap(&mut sh, "Left");
    assert_eq!(x(&sh), 0.0);
    tap(&mut sh, "End");
    assert_eq!(x(&sh), 600.0);
    tap(&mut sh, "Page_Up");
    assert_eq!(x(&sh), 400.0);
    assert_eq!(offset(&sh, s), 0.0);
    tap(&mut sh, "Home");
    tap(&mut sh, "F1");
    assert_eq!(x(&sh), 300.0, "y alone is along its one axis");
}

/// One that runs both ways takes both.
#[test]
fn a_scroll_along_both_axes_moves_each() {
    let mut sh = scroller("focus: true; axis: both");
    let s = scroll_node(&sh);
    let x = |sh: &Shell| {
        sh.r.scroll_state(s)
            .and_then(|st| st.across.as_ref())
            .map_or(0.0, |a| a.offset)
    };
    tap(&mut sh, "F5");
    assert_eq!((x(&sh), offset(&sh, s)), (70.0, 0.0));
    tap(&mut sh, "F6");
    assert_eq!((x(&sh), offset(&sh, s)), (30.0, 20.0));
    tap(&mut sh, "F7");
    assert_eq!((x(&sh), offset(&sh, s)), (40.0, 20.0));
    tap(&mut sh, "Down");
    tap(&mut sh, "Right");
    assert_eq!((x(&sh), offset(&sh, s)), (80.0, 60.0));
    assert_eq!((sh.num("x"), sh.num("y")), (80.0, 60.0), "read back");
}

/// `scroll_to` and `scroll_by` spring as a wheel does, kept to the content, and
/// `scroll_y` follows.
#[test]
fn scroll_to_and_scroll_by_spring_and_read_back() {
    let mut sh = scroller("");
    let s = scroll_node(&sh);
    sh.key("F1", false);
    assert!(
        sh.r.scroll_state(s).unwrap().moving(),
        "a spring, not a jump"
    );
    sh.settle();
    assert_eq!(offset(&sh, s), 300.0);
    assert_eq!(sh.num("y"), 300.0);
    tap(&mut sh, "F2");
    assert_eq!(offset(&sh, s), 340.0);
    tap(&mut sh, "F3");
    assert_eq!(offset(&sh, s), 0.0);
    assert_eq!(sh.num("y"), 0.0);
    tap(&mut sh, "F4");
    assert_eq!(offset(&sh, s), 900.0);
}

/// What it moves is what is drawn.
#[test]
fn a_scroll_moved_from_code_draws_there() {
    let mut sh = entered(
        "panel P { anchor: top_left; width: 100; height: 100; keyboard: exclusive; bg: #000000\n\
           on key \"f1\" { s.scroll_to(100) }\n\
           on key \"f2\" { blue.reveal() }\n\
           scroll { id: s; height: 100; width: 100\n\
             col {\n\
               box { height: 100; width: 100; bg: #ff0000 }\n\
               box { height: 100; width: 100; bg: #00ff00 }\n\
               box { id: blue; height: 100; width: 100; bg: #0000ff }\n\
             }\n\
           }\n\
         }\n",
    );
    // Premultiplied BGRA.
    let top = |sh: &mut Shell| shell::px(&sh.shot("P"), 50, 50);
    assert_eq!(top(&mut sh), [0, 0, 255, 255]);
    tap(&mut sh, "F1");
    assert_eq!(top(&mut sh), [0, 255, 0, 255]);
    tap(&mut sh, "F2");
    assert_eq!(top(&mut sh), [255, 0, 0, 255]);
}

/// Several `scroll_by` in a row all count: each from where the last heads.
#[test]
fn scroll_by_builds_on_where_it_is_heading() {
    let mut sh = scroller("");
    let s = scroll_node(&sh);
    let e = key_with(&sh, "F2", "", true, false);
    sh.queue(e);
    let e = key_with(&sh, "F2", "", true, false);
    sh.queue(e);
    sh.settle();
    assert_eq!(offset(&sh, s), 80.0);
}

/// A `list` shares it, along the way it runs.
#[test]
fn a_list_scrolls_from_code_and_reads_back() {
    let items = (0..40)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut sh = entered(&format!(
        "state items: [int] = [{items}]\nstate y = -1.0\nstate x = -1.0\n\
         panel P {{ anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" {{ l.scroll_to(100) }}\n\
           on key \"f2\" {{ h.scroll_by(60) }}\n\
           on change l.scroll_y {{ y = l.scroll_y }}\n\
           on change h.scroll_x {{ x = h.scroll_x }}\n\
           col {{\n\
             list {{ id: l; height: 40\n\
               for i in items key i {{ box {{ height: 20 }} }}\n\
             }}\n\
             list {{ id: h; height: 40; axis: horizontal\n\
               for i in items key i {{ box {{ width: 30 }} }}\n\
             }}\n\
           }}\n\
         }}\n"
    ));
    let lists = sh.nodes("P", NodeKind::List);
    tap(&mut sh, "F1");
    assert_eq!(offset(&sh, lists[0]), 100.0);
    assert_eq!(sh.num("y"), 100.0);
    tap(&mut sh, "F2");
    assert_eq!(
        offset(&sh, lists[1]),
        60.0,
        "a horizontal list's is its one state"
    );
    assert_eq!(sh.num("x"), 60.0);
}

/// `node.reveal()` moves the least it takes, at once, either way, and not if
/// the node shows.
#[test]
fn reveal_moves_the_least_it_takes() {
    let mut sh = entered(
        "panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" { a.reveal() }\n\
           on key \"f2\" { b.reveal() }\n\
           on key \"f3\" { c.reveal() }\n\
           scroll { id: s; height: 100\n\
             col {\n\
               box { id: a; height: 50 }\n\
               box { id: b; height: 50 }\n\
               box { height: 400 }\n\
               box { id: c; height: 50 }\n\
             }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    tap(&mut sh, "F2");
    assert_eq!(offset(&sh, s), 0.0, "it shows");
    sh.key("F3", false);
    assert_eq!(offset(&sh, s), 450.0, "its end at the view's end, at once");
    sh.settle();
    tap(&mut sh, "F2");
    assert_eq!(offset(&sh, s), 50.0, "its start at the view's start");
    tap(&mut sh, "F1");
    assert_eq!(offset(&sh, s), 0.0);
}

/// Through scrolls inside scrolls, and a list's row; along x too.
#[test]
fn reveal_goes_through_every_scroll_around_it() {
    let mut sh = entered(
        "panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" { deep.reveal() }\n\
           on key \"f2\" { side.reveal() }\n\
           scroll { height: 100; width: 200\n\
             col {\n\
               box { height: 300 }\n\
               scroll { height: 80; width: 200\n\
                 col {\n\
                   box { height: 500 }\n\
                   box { id: deep; height: 20 }\n\
                 }\n\
               }\n\
               scroll { height: 50; width: 200; axis: horizontal\n\
                 row {\n\
                   box { width: 900; height: 20 }\n\
                   box { id: side; width: 40; height: 20 }\n\
                 }\n\
               }\n\
             }\n\
           }\n\
         }\n",
    );
    let scrolls = sh.nodes("P", NodeKind::Scroll);
    tap(&mut sh, "F1");
    // The inner brings it to its end, 420 down; it sits 380 down the outer's
    // content, so the outer stops with it at its own end.
    assert_eq!(offset(&sh, scrolls[1]), 440.0);
    assert_eq!(offset(&sh, scrolls[0]), 280.0);
    tap(&mut sh, "F2");
    let x =
        sh.r.scroll_state(scrolls[2])
            .and_then(|st| st.across.as_ref())
            .map_or(0.0, |a| a.offset);
    assert_eq!(x, 740.0);
    assert_eq!(offset(&sh, scrolls[0]), 300.0);
}

/// A node in a `list`'s row: the row comes into view.
#[test]
fn reveal_of_a_node_in_a_row_moves_the_list() {
    let items = (0..40)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut sh = entered(&format!(
        "state items: [int] = [{items}]\n\
         panel P {{ anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" {{ third.reveal() }}\n\
           list {{ height: 40\n\
             box {{ height: 20 }}\n\
             box {{ height: 20 }}\n\
             box {{ height: 20; box {{ id: third; height: 10 }} }}\n\
           }}\n\
         }}\n"
    ));
    let l = sh.nodes("P", NodeKind::List)[0];
    tap(&mut sh, "F1");
    assert_eq!(offset(&sh, l), 20.0);
}

/// A reveal asked with the content that makes it possible waits for its
/// layout.
#[test]
fn reveal_waits_for_the_layout_of_its_diff() {
    let mut sh = entered(
        "state rows: [int] = []\n\
         panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" { rows = [0, 1, 2, 3]; end.reveal() }\n\
           on key \"f2\" { rows = [0, 1, 2, 3, 4, 5]; s.scroll_to(100000) }\n\
           scroll { id: s; height: 100\n\
             col {\n\
               for i in rows key i { box { height: 100 } }\n\
               box { id: end; height: 20 }\n\
             }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    tap(&mut sh, "F1");
    assert_eq!(offset(&sh, s), 320.0);
    assert_eq!(sh.r.scroll_offset(s), Some(320.0), "and drawn there");
    tap(&mut sh, "F2");
    assert_eq!(offset(&sh, s), 520.0, "past what it had before this diff");
    assert_eq!(sh.r.scroll_offset(s), Some(520.0));
}

/// Tab onto a node in a `scroll` shows it, and so does `focus()`, while a click
/// leaves what is under the pointer where it is.
#[test]
fn focus_moving_into_a_scroll_reveals_it() {
    let mut sh = entered(
        "state go = false\n\
         panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" { far.focus() }\n\
           on key \"f2\" { near.focus() }\n\
           scroll { id: s; height: 100\n\
             col {\n\
               button \"near\" { id: near; height: 90 }\n\
               box { height: 20; focus: false }\n\
               box { height: 500 }\n\
               box { id: far; height: 20 }\n\
             }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    tap(&mut sh, "Tab");
    assert_eq!(offset(&sh, s), 0.0);
    tap(&mut sh, "Tab");
    assert_eq!(offset(&sh, s), 10.0, "the second is cut off below");
    tap(&mut sh, "F1");
    assert_eq!(offset(&sh, s), 530.0);
    tap(&mut sh, "F2");
    assert_eq!(offset(&sh, s), 0.0);
    // Pressed where the box is cut off: focused, and nothing moves.
    sh.click(5.0, 95.0);
    sh.settle();
    assert_eq!(offset(&sh, s), 0.0);
    assert_eq!(
        sh.r.tree().get(focused(&sh).unwrap()).map(|n| n.kind),
        Some(NodeKind::Box)
    );
}

/// `focus: cond` turning true reveals too.
#[test]
fn a_turned_focus_reveals() {
    let mut sh = entered(
        "state go = false\n\
         panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           scroll { id: s; height: 100\n\
             col { box { height: 500 } box { height: 20; focus: go } }\n\
           }\n\
         }\n",
    );
    sh.set("go", strand_compiler::vm::Value::Bool(true));
    assert_eq!(offset(&sh, scroll_node(&sh)), 420.0);
}

// A launcher, and clicks.

/// A press on a button beside a focused `input` runs the button and leaves the
/// typing in the field; a click on a `focus:` box still takes it.
#[test]
fn a_click_on_a_button_leaves_a_focused_input_typing() {
    let mut sh = entered(
        "state q = \"\"\nstate clicks = 0\nstate items: [int] = [0, 1, 2]\nstate acts = 0\n\
         panel P { anchor: top_left; width: 200; height: 200; keyboard: exclusive\n\
           col {\n\
             input { height: 20; text: <-> q; focus: true; nav: results }\n\
             button \"Clear\" { height: 20; on click { clicks += 1; q = \"\" } }\n\
             list { id: results; height: 60\n\
               for i in items key i {\n\
                 box { height: 20; on activate { acts += 1 }\n\
                   button \"row\" { height: 20 }\n\
                 }\n\
               }\n\
             }\n\
             box { height: 20; focus: false }\n\
           }\n\
         }\n",
    );
    let input = sh.nodes("P", NodeKind::Input)[0];
    let type_a = |sh: &mut Shell| {
        let e = key_with(sh, "a", "a", false, false);
        sh.send(e);
        sh.settle();
    };
    type_a(&mut sh);
    assert_eq!(
        format!("{:?}", sh.get("q")),
        format!("{:?}", strand_compiler::vm::Value::text("a"))
    );
    // The button: it runs, the field keeps the keys.
    sh.click(10.0, 30.0);
    sh.settle();
    assert_eq!(sh.num("clicks"), 1.0);
    assert_eq!(focused(&sh), Some(input));
    type_a(&mut sh);
    assert_eq!(
        format!("{:?}", sh.get("q")),
        format!("{:?}", strand_compiler::vm::Value::text("a")),
        "typed after the button cleared it"
    );
    // Return is the field's: its list's row, not the button again.
    tap(&mut sh, "Return");
    assert_eq!(sh.num("clicks"), 1.0);
    assert_eq!(sh.num("acts"), 1.0);
    // A button in a row of the list it steers: the row runs, the field keeps.
    sh.click(10.0, 50.0);
    sh.settle();
    assert_eq!(sh.num("acts"), 2.0);
    assert_eq!(focused(&sh), Some(input));
    // A node that says it takes focus does.
    let b = *sh.nodes("P", NodeKind::Box).last().unwrap();
    sh.click(10.0, 110.0);
    sh.settle();
    assert_eq!(focused(&sh), Some(b));
}

/// A press on a button with nothing holding the keys focuses it; a focused list
/// keeps its arrows when a row's button is pressed.
#[test]
fn a_click_focuses_a_button_unless_a_list_holds_the_keys() {
    let mut sh = entered(
        "state items: [int] = [0, 1, 2]\n\
         panel P { anchor: top_left; width: 200; height: 200; keyboard: exclusive\n\
           col {\n\
             button \"b\" { height: 20 }\n\
             list { id: l; height: 60\n\
               for i in items key i { button \"r\" { height: 20 } }\n\
             }\n\
           }\n\
         }\n",
    );
    let list = sh.nodes("P", NodeKind::List)[0];
    let b = sh.nodes("P", NodeKind::Button)[0];
    sh.click(10.0, 10.0);
    sh.settle();
    assert_eq!(focused(&sh), Some(b));
    sh.click(10.0, 30.0);
    sh.settle();
    assert_eq!(
        focused(&sh),
        Some(list),
        "a row's button: the list, as before"
    );
    tap(&mut sh, "Down");
    assert_eq!(sh.router.selected_index(list), Some(1));
    sh.click(10.0, 10.0);
    sh.settle();
    assert_eq!(focused(&sh), Some(list), "the list holds the keys");
}

// Reload.

#[test]
fn a_reload_keeps_focus() {
    let src = |extra: &str| {
        format!(
            "state v = 0.5\n\
             panel P {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
               col {{ {extra}\n\
                 button \"a\" {{ height: 20 }}\n\
                 slider {{ height: 20; value: <-> v; range: 0, 1; step: 0.1 }}\n\
                 box {{ height: 20; focus: false }}\n\
               }}\n\
             }}\n"
        )
    };
    let mut sh = Shell::reloadable(&src(""));
    sh.keyboard_to("P");
    sh.settle();
    tap(&mut sh, "Tab");
    tap(&mut sh, "Tab");
    let slider = sh.nodes("P", NodeKind::Slider)[0];
    assert_eq!(focused(&sh), Some(slider));
    sh.reload(&src("gap: 2"));
    sh.settle();
    assert_eq!(focused(&sh), Some(slider));
    tap(&mut sh, "Right");
    assert!((sh.num("v") - 0.6).abs() < 1e-6, "{}", sh.num("v"));
}

// Cost.

/// A read of `scroll_y` wakes nothing while it stands still, and costs nothing
/// where it is not read.
#[test]
fn reading_scroll_y_wakes_nothing_at_rest() {
    let mut sh = entered(
        "state changes = 0\nstate y = 0.0\n\
         panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" { s.scroll_to(50) }\n\
           on change s.scroll_y { y = s.scroll_y; changes += 1 }\n\
           scroll { id: s; height: 100\n\
             box { height: 1000 }\n\
           }\n\
         }\n",
    );
    let id = sh.surface("P").id;
    let s = scroll_node(&sh);
    assert_eq!(
        sh.r.tree().get(s).and_then(|n| n.get(Prop::Watch)),
        Some(&strand_scene::PropValue::Keyword("scroll".into())),
    );
    let at_rest = sh.num("changes");
    for _ in 0..20 {
        sh.run(1);
        assert!(!sh.r.wants_frame(id));
        assert!(sh.r.take_scroll_facts().is_empty());
        assert!(sh.inst.tick(sh.now).diff.is_empty());
    }
    assert_eq!(sh.num("changes"), at_rest);
    tap(&mut sh, "F1");
    assert!(sh.num("changes") > at_rest, "it follows the spring");
    assert_eq!(sh.num("y"), 50.0);
    let moved = sh.num("changes");
    sh.run(20);
    assert!(!sh.r.wants_frame(id));
    assert_eq!(sh.num("changes"), moved, "and stops with it");
}

#[test]
fn an_unread_position_is_not_reported() {
    let mut sh = entered(
        "panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on key \"f1\" { s.scroll_to(50) }\n\
           scroll { id: s; height: 100\n\
             box { height: 1000 }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    assert_eq!(sh.r.tree().get(s).and_then(|n| n.get(Prop::Watch)), None);
    sh.key("F1", false);
    assert!(sh.r.take_scroll_facts().is_empty());
}

// Checks.

/// A scroll action on what cannot scroll says so; `focus()` and `reveal()` go
/// on anything.
#[test]
fn check_says_a_box_does_not_scroll() {
    let codes = |el: &str, call: &str| -> Vec<String> {
        let mut map = strand_compiler::SourceMap::new();
        map.add(
            "shell.strand",
            format!(
                "panel P {{ anchor: top_left; width: 10; height: 10\n\
                   on click {{ {call} }}\n\
                   {el} {{ id: b }}\n\
                 }}\n"
            ),
        );
        strand_compiler::compile(&map)
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect()
    };
    assert_eq!(codes("box", "b.scroll_to(3)"), ["check::not_a_scroll"]);
    assert_eq!(codes("box", "b.scroll_by(3)"), ["check::not_a_scroll"]);
    assert!(codes("scroll", "b.scroll_to(3)").is_empty());
    assert!(codes("list", "b.scroll_by(dx: 3)").is_empty());
    assert!(codes("box", "b.focus(); b.reveal()").is_empty());
}

// Focus on what stops being shown or taking input.

fn panel_of(sh: &Shell) -> NodeId {
    sh.nodes("P", NodeKind::Panel)[0]
}

fn type_a(sh: &mut Shell) {
    let key = key_with(sh, "a", "a", false, false);
    sh.send(key);
    sh.settle();
}

/// A focused button turning `opacity: 0` is not pressed, `focused` clears, and
/// with nothing else to go to focus is the surface's until it shows again.
#[test]
fn a_button_no_longer_shown_takes_no_press() {
    let mut sh = entered(
        "state clicks = 0\nstate hide = false\nstate lit = true\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on click { clicks += 100 }\n\
           button \"b\" { focus: true; opacity: hide ? 0 : 1\n\
             on click { clicks += 1 }\n\
             on change self.focused { lit = self.focused }\n\
           }\n\
         }\n",
    );
    let (panel, button) = (panel_of(&sh), sh.nodes("P", NodeKind::Button)[0]);
    assert_eq!(focused(&sh), Some(button));
    sh.set("hide", strand_compiler::vm::Value::Bool(true));
    assert_eq!(focused(&sh), Some(panel));
    assert_eq!(sh.get("lit"), strand_compiler::vm::Value::Bool(false));
    tap(&mut sh, "Return");
    tap(&mut sh, "space");
    assert_eq!(sh.num("clicks"), 0.0);
    sh.set("hide", strand_compiler::vm::Value::Bool(false));
    assert_eq!(focused(&sh), Some(button), "back when it shows");
    assert_eq!(sh.get("lit"), strand_compiler::vm::Value::Bool(true));
    tap(&mut sh, "Return");
    assert_eq!(sh.num("clicks"), 1.0);
}

/// `hit: none` hands focus to what Tab would next, and focus moved on
/// meanwhile stays where it went when the node takes input again.
#[test]
fn a_button_turning_hit_none_hands_focus_on_as_tab_would() {
    let mut sh = entered(
        "state hide = false\nstate clicks = 0\n\
         panel P { anchor: top_left; width: 300; height: 200; keyboard: exclusive\n\
           col {\n\
             button \"a\" { }\n\
             button \"b\" { focus: true; when hide { hit: none }; on click { clicks += 1 } }\n\
             button \"c\" { on click { clicks += 10 } }\n\
           }\n\
         }\n",
    );
    let b = sh.nodes("P", NodeKind::Button);
    assert_eq!(focused(&sh), Some(b[1]));
    sh.set("hide", strand_compiler::vm::Value::Bool(true));
    assert_eq!(focused(&sh), Some(b[2]));
    tap(&mut sh, "Return");
    assert_eq!(
        sh.num("clicks"),
        10.0,
        "what focus is on now, not what it left"
    );
    tap(&mut sh, "Tab");
    assert_eq!(focused(&sh), Some(b[0]));
    sh.set("hide", strand_compiler::vm::Value::Bool(false));
    assert_eq!(focused(&sh), Some(b[0]), "it moved on");
}

/// Hidden with a box around it: on to the node after it, not the first.
#[test]
fn focus_hidden_with_its_box_goes_to_the_node_after_it() {
    let mut sh = entered(
        "state hide = false\n\
         panel P { anchor: top_left; width: 300; height: 200; keyboard: exclusive\n\
           col {\n\
             button \"a\" { }\n\
             box { opacity: hide ? 0 : 1; button \"b\" { focus: true } }\n\
             button \"c\" { }\n\
           }\n\
         }\n",
    );
    let b = sh.nodes("P", NodeKind::Button);
    sh.set("hide", strand_compiler::vm::Value::Bool(true));
    assert_eq!(focused(&sh), Some(b[2]));
}

#[test]
fn an_input_no_longer_shown_takes_no_typing() {
    let mut sh = entered(
        "state hide = false\nstate query = \"\"\nstate typed = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on change query { typed += 1 }\n\
           input { focus: true; text: <-> query; when hide { hit: none } }\n\
         }\n",
    );
    type_a(&mut sh);
    assert_eq!(sh.num("typed"), 1.0);
    sh.set("hide", strand_compiler::vm::Value::Bool(true));
    assert_eq!(focused(&sh), Some(panel_of(&sh)));
    type_a(&mut sh);
    assert_eq!(sh.num("typed"), 1.0);
}

#[test]
fn a_list_no_longer_shown_takes_no_keys() {
    let mut sh = entered(
        "state hide = false\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           list { focus: true; height: 80; opacity: hide ? 0 : 1\n\
             box { height: 20 }\n\
             box { height: 20 }\n\
             box { height: 20 }\n\
           }\n\
         }\n",
    );
    let list = sh.nodes("P", NodeKind::List)[0];
    tap(&mut sh, "Down");
    let at = sh.router.selected_index(list);
    assert!(at.is_some());
    sh.set("hide", strand_compiler::vm::Value::Bool(true));
    tap(&mut sh, "Down");
    assert_eq!(sh.router.selected_index(list), at);
    assert_eq!(focused(&sh), Some(panel_of(&sh)));
}

/// A launcher fading out: what was typed stands, Escape still closes it, and
/// opened again before the fade ends it types on.
#[test]
fn a_fading_launcher_still_closes_and_types_on_when_it_comes_back() {
    let mut sh = entered(
        "state fade = false\nstate o = true\nstate query = \"\"\nstate typed = 0\n\
         panel P { open: <-> o; anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on change query { typed += 1 }\n\
           input { focus: true; text: <-> query; opacity: fade ? 0 : 1 }\n\
         }\n",
    );
    let input = sh.nodes("P", NodeKind::Input)[0];
    let key = key_with(&sh, "a", "a", false, false);
    sh.send(key);
    sh.set("fade", strand_compiler::vm::Value::Bool(true));
    assert_eq!(sh.num("typed"), 1.0, "the key before the fade counts");
    type_a(&mut sh);
    assert_eq!(sh.num("typed"), 1.0, "and none during it");
    sh.set("fade", strand_compiler::vm::Value::Bool(false));
    assert_eq!(focused(&sh), Some(input));
    type_a(&mut sh);
    assert_eq!(sh.num("typed"), 2.0);
    sh.set("fade", strand_compiler::vm::Value::Bool(true));
    tap(&mut sh, "Escape");
    assert_eq!(sh.get("o"), strand_compiler::vm::Value::Bool(false));
}

// What existing shells keep.

/// A list is one stop, whatever its rows hold, unless a row's node writes
/// `focus:`.
#[test]
fn a_list_is_one_tab_stop() {
    let mut sh = entered(&format!(
        "state at = 0\n\
         panel P {{ anchor: top_left; width: 300; height: 400; keyboard: exclusive\n\
           col {{\n\
             input {{ {} }}\n\
             list {{ height: 120; {}\n\
               box {{ height: 20; button \"x\" {{ {} }} }}\n\
               box {{ height: 20; slider {{ {} }} }}\n\
               box {{ height: 20; input {{ {} }} }}\n\
               box {{ height: 20; button \"y\" {{ focus: false; {} }} }}\n\
             }}\n\
             input {{ {} }}\n\
           }}\n\
         }}\n",
        tell(1),
        tell(2),
        tell(90),
        tell(91),
        tell(92),
        tell(3),
        tell(4),
    ));
    let mut seen = Vec::new();
    for _ in 0..5 {
        tap(&mut sh, "Tab");
        seen.push(sh.num("at") as u32);
    }
    assert_eq!(seen, [1, 2, 3, 4, 1]);
}

fn on_demand_bar() -> Shell {
    Shell::new(
        "panel P { anchor: top_left; width: 300; height: 100; keyboard: on_demand\n\
           col {\n\
             button \"b\" { height: 30; width: 100 }\n\
             input { focus: true }\n\
           }\n\
         }\n",
    )
}

/// The press that brought the keyboard, on a button, leaves `focus: true` to
/// take it.
#[test]
fn a_press_before_the_keyboard_does_not_beat_focus_true() {
    let mut sh = on_demand_bar();
    sh.settle();
    sh.click(10.0, 10.0);
    sh.settle();
    sh.keyboard_to("P");
    sh.settle();
    assert_eq!(focused(&sh), Some(sh.nodes("P", NodeKind::Input)[0]));
}

/// The keyboard back from a popup that grabbed it, no leave between, finds
/// focus where Tab left it.
#[test]
fn focus_held_with_the_keyboard_stays_when_it_comes_back() {
    let mut sh = on_demand_bar();
    sh.settle();
    sh.keyboard_to("P");
    tap(&mut sh, "ISO_Left_Tab");
    let button = sh.nodes("P", NodeKind::Button)[0];
    assert_eq!(focused(&sh), Some(button));
    sh.keyboard_to("P");
    assert_eq!(focused(&sh), Some(button));
}

/// Return on a focused row: what a pointer's click on it does, once each.
#[test]
fn return_on_a_focused_row_does_what_a_click_on_it_does() {
    let mut sh = entered(
        "state clicks = 0\nstate acts = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           list { height: 100; on activate { acts += 1 }\n\
             box { height: 40; focus: true; on click { clicks += 1 } }\n\
           }\n\
         }\n",
    );
    let list = sh.nodes("P", NodeKind::List)[0];
    tap(&mut sh, "Return");
    assert_eq!((sh.num("acts"), sh.num("clicks")), (1.0, 1.0), "the key");
    assert_eq!(sh.router.selected_index(list), Some(0));
    sh.click(10.0, 10.0);
    sh.settle();
    assert_eq!(
        (sh.num("acts"), sh.num("clicks")),
        (2.0, 2.0),
        "the pointer"
    );
}

/// One press of Return is one `on activate`, on an `input`, a `list` and an
/// `input` with a `nav:` list as on a `button`: a repeat fires nothing, and
/// a new press fires again (spec.md 6.7, site/how-to/keys.md).
#[test]
fn a_held_return_activates_an_input_and_a_list_once() {
    for node in [
        "input { focus: true; on activate { acts += 1 } }",
        "list { focus: true; height: 40; on activate { acts += 1 }\n box { height: 20 }\n}",
        "input { focus: true; nav: rows }\n\
         list { id: rows; height: 40; on activate { acts += 1 }\n box { height: 20 }\n}",
    ] {
        for name in ["Return", "KP_Enter"] {
            let mut sh = entered(&format!(
                "state acts = 0\n\
                 panel P {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
                   {node}\n\
                 }}\n"
            ));
            tap(&mut sh, name);
            assert_eq!(sh.num("acts"), 1.0, "{node}: the press of {name}");
            for _ in 0..3 {
                let e = key_with(&sh, name, "", true, false);
                sh.send(e);
                sh.settle();
            }
            assert_eq!(sh.num("acts"), 1.0, "{node}: {name} held");
            tap(&mut sh, name);
            assert_eq!(sh.num("acts"), 2.0, "{node}: {name} pressed again");
        }
    }
}

/// A repeat of Return is spent, not passed on to the field: its text stays.
#[test]
fn a_held_return_types_nothing_into_an_input() {
    let mut sh = entered(
        "state q = \"ab\"\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           input { focus: true; text: <-> q }\n\
         }\n",
    );
    let e = key_with(&sh, "Return", "\r", true, false);
    sh.send(e);
    sh.settle();
    assert_eq!(sh.get("q"), Value::text("ab"));
}

/// Only Return stops at the press: arrows and Page keys held go on moving a
/// list's selection, inside an `input` with a `nav:` list too, typing held
/// goes on typing, and a claim on Return goes on hearing repeats.
#[test]
fn only_return_stops_at_the_press() {
    let rows = "for n in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] key n { box { height: 20 } }";
    for (field, focus) in [
        ("", "focus: true"),
        ("input { focus: true; nav: rows }\n", ""),
    ] {
        let mut sh = entered(&format!(
            "panel P {{ anchor: top_left; width: 300; height: 300; keyboard: exclusive\n\
               {field}list {{ id: rows; height: 40; {focus}\n {rows}\n }}\n\
             }}\n"
        ));
        let list = sh.nodes("P", NodeKind::List)[0];
        tap(&mut sh, "Down");
        let at = sh.router.selected_index(list).unwrap();
        let e = key_with(&sh, "Down", "", true, false);
        sh.send(e);
        sh.settle();
        assert_eq!(
            sh.router.selected_index(list),
            Some(at + 1),
            "Down held {field}"
        );
        let e = key_with(&sh, "Next", "", true, false);
        sh.send(e);
        sh.settle();
        assert!(
            sh.router.selected_index(list).unwrap() > at + 1,
            "Page_Down held {field}"
        );
    }
    let mut sh = entered(
        "state q = \"\"\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           input { focus: true; text: <-> q }\n\
         }\n",
    );
    for repeat in [false, true] {
        let e = key_with(&sh, "a", "a", repeat, false);
        sh.send(e);
        sh.settle();
    }
    assert_eq!(sh.get("q"), Value::text("aa"), "typing held");
    let mut sh = entered(
        "state acts = 0\nstate claims = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           input { focus: true; on activate { acts += 1 }; on key \"return\" { claims += 1 } }\n\
         }\n",
    );
    tap(&mut sh, "Return");
    let e = key_with(&sh, "Return", "", true, false);
    sh.send(e);
    sh.settle();
    assert_eq!((sh.num("claims"), sh.num("acts")), (2.0, 0.0), "a claim");
}

/// A row with its own `on activate` hears it once, not as a press and again.
#[test]
fn return_on_a_row_with_its_own_activate_runs_it_once() {
    let mut sh = entered(
        "state clicks = 0\nstate acts = 0\n\
         panel P { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           list { height: 100\n\
             box { height: 40; focus: true; on activate { acts += 1 }; on click { clicks += 1 } }\n\
           }\n\
         }\n",
    );
    tap(&mut sh, "Return");
    assert_eq!((sh.num("acts"), sh.num("clicks")), (1.0, 1.0));
}

/// Up and Down are a horizontal scroll's own, inside `arrows:` too.
#[test]
fn up_and_down_stay_a_horizontal_scrolls_inside_arrows() {
    let mut sh = entered(
        "panel P { anchor: top_left; width: 300; height: 300; keyboard: exclusive\n\
           col { arrows: focus\n\
             button \"a\" { }\n\
             scroll { height: 50; width: 200; axis: horizontal; focus: true\n\
               box { width: 800; height: 50 }\n\
             }\n\
             button \"c\" { }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    let x = |sh: &Shell| {
        sh.r.scroll_state(s)
            .and_then(|st| st.across.as_ref())
            .map_or(0.0, |a| a.offset)
    };
    tap(&mut sh, "Down");
    assert_eq!(focused(&sh), Some(s));
    assert_eq!(x(&sh), 40.0);
    tap(&mut sh, "Up");
    assert_eq!(focused(&sh), Some(s));
    assert_eq!(x(&sh), 0.0);
}

/// Space and Shift+space page a focused scroll, and click nothing around it.
#[test]
fn space_pages_a_focused_scroll() {
    let mut sh = entered(
        "state clicks = 0\n\
         panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           box { on click { clicks += 1 }\n\
             scroll { height: 100; width: 200; focus: true\n\
               box { height: 1000 }\n\
             }\n\
           }\n\
         }\n",
    );
    let s = scroll_node(&sh);
    tap(&mut sh, "space");
    assert_eq!(offset(&sh, s), 100.0);
    tap(&mut sh, "space");
    assert_eq!(offset(&sh, s), 200.0);
    sh.key("space", true);
    sh.settle();
    assert_eq!(offset(&sh, s), 100.0);
    // Held, it goes on paging.
    let e = key_with(&sh, "space", " ", true, false);
    sh.send(e);
    sh.settle();
    assert_eq!(offset(&sh, s), 200.0);
    assert_eq!(sh.num("clicks"), 0.0);
}

/// On a list, a view of rows, as the Page keys.
#[test]
fn space_pages_a_focused_list() {
    let mut sh = entered(
        "state clicks = 0\n\
         panel P { anchor: top_left; width: 200; height: 100; keyboard: exclusive\n\
           on click { clicks += 1 }\n\
           list { focus: true; height: 40\n\
             for i in [0, 1, 2, 3, 4, 5, 6, 7] key i { box { height: 20 } }\n\
           }\n\
         }\n",
    );
    let list = sh.nodes("P", NodeKind::List)[0];
    tap(&mut sh, "Down");
    let at = sh.router.selected_index(list).unwrap();
    tap(&mut sh, "space");
    let paged = sh.router.selected_index(list).unwrap();
    assert!(paged > at, "{at} to {paged}");
    sh.key("space", true);
    sh.settle();
    assert_eq!(sh.router.selected_index(list), Some(at));
    // Held, it goes on paging.
    let e = key_with(&sh, "space", " ", true, false);
    sh.send(e);
    sh.settle();
    assert_eq!(sh.router.selected_index(list), Some(paged));
    assert_eq!(sh.num("clicks"), 0.0);
}

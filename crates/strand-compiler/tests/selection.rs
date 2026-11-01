//! (keyboard) A `list`'s selection moved from code (`results.next()`) and
//! read back (`results.selected_index`), `wrap:`, `axis:` and `columns:`, a
//! whole shell driven through the host loop (`tests/shell`).

mod shell;

use shell::Shell;
use strand_compiler::vm::Value;
use strand_scene::{ButtonState, InputEvent, KeyInput, Modifiers, NodeId, NodeKind};

/// A `panel` with a list of `count` rows 20 tall, 100 tall itself (5 rows
/// show), and claims on the letters for each action. `props` go on the list.
fn shell(count: u32, props: &str) -> Shell {
    let items = (0..count)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut sh = Shell::new(&format!(
        "state items: [int] = [{items}]\n\
         state activated = -1\n\
         state at = -9\n\
         panel L {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on key \"n\" {{ results.next() }}\n\
           on key \"p\" {{ results.prev() }}\n\
           on key \"f\" {{ results.first() }}\n\
           on key \"l\" {{ results.last() }}\n\
           on key \"x\" {{ results.select(1) }}\n\
           on key \"y\" {{ results.select(-5) }}\n\
           on key \"z\" {{ results.select(100000) }}\n\
           on key \"a\" {{ results.activate() }}\n\
           on key \"t\" {{ results.next(); results.next(); results.activate() }}\n\
           on change results.selected_index {{ at = results.selected_index ?? -1 }}\n\
           list {{ id: results; height: 100; focus: true; {props}\n\
             for i in items key i {{\n\
               box {{ height: 20; on activate {{ activated = i }} }}\n\
             }}\n\
           }}\n\
         }}\n"
    ));
    sh.keyboard_to("L");
    sh.settle();
    sh
}

fn key_event(sh: &Shell, name: &str) -> InputEvent {
    InputEvent::Key {
        surface: sh.keyboard.unwrap(),
        key: KeyInput {
            name: name.into(),
            text: name.into(),
            state: ButtonState::Pressed,
            repeat: true,
            modifiers: Modifiers::default(),
            time: 0,
            latin: None,
        },
    }
}

fn press(sh: &mut Shell, name: &str) {
    let e = key_event(sh, name);
    sh.send(e);
    sh.run(4);
}

fn list(sh: &Shell) -> NodeId {
    sh.nodes("L", NodeKind::List)[0]
}

fn index(sh: &Shell) -> Option<u32> {
    sh.router.selected_index(list(sh))
}

/// `next`, `prev`, `first`, `last` and `select` from a claim move what the
/// arrow keys move: the `selected` row and what Return activates.
#[test]
fn the_actions_move_the_selection_from_a_claim() {
    let mut sh = shell(20, "");
    assert_eq!(index(&sh), None);
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(0), "from none, as an arrow: the first row");
    press(&mut sh, "n");
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(2));
    let row2 = sh.r.tree().get(list(&sh)).unwrap().children[2];
    assert_eq!(sh.router.selected(list(&sh)), Some(row2));
    press(&mut sh, "p");
    assert_eq!(index(&sh), Some(1));
    press(&mut sh, "l");
    assert_eq!(index(&sh), Some(19));
    press(&mut sh, "f");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "x");
    assert_eq!(index(&sh), Some(1));
    // And an arrow carries on from there, and back.
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(2));
    press(&mut sh, "p");
    assert_eq!(index(&sh), Some(1));
    press(&mut sh, "a");
    assert_eq!(sh.num("activated"), 1.0, "activate runs the selected row's");
}

/// `next()` and `prev()` stop at the ends, as arrows do; `select` keeps
/// within the rows.
#[test]
fn select_clamps_and_moves_stop_at_the_ends() {
    let mut sh = shell(20, "");
    press(&mut sh, "z");
    assert_eq!(index(&sh), Some(19));
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(19));
    press(&mut sh, "y");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "p");
    assert_eq!(index(&sh), Some(0));
}

/// On an empty list every action does nothing, `activate` too.
#[test]
fn on_an_empty_list_every_action_does_nothing() {
    let mut sh = shell(0, "");
    for k in ["n", "p", "f", "l", "x", "y", "z", "a", "t"] {
        press(&mut sh, k);
        assert_eq!(index(&sh), None, "{k}");
    }
    assert_eq!(sh.num("activated"), -1.0);
    assert_eq!(sh.num("at"), -9.0, "no selection was ever told");
}

/// Repeats queued while logic is busy all count, in order, against the
/// router's selection: their bodies run a tick after they were routed, and
/// a move past the mounted rows stands on its way for the next one to build
/// on. None is lost, and the row lands.
#[test]
fn queued_repeats_apply_in_order_with_none_lost() {
    let mut sh = shell(2000, "");
    for _ in 0..7 {
        let e = key_event(&sh, "n");
        sh.queue(e);
    }
    assert_eq!(index(&sh), None, "logic has not run yet");
    sh.run(1);
    assert_eq!(index(&sh), Some(6), "seven, from none");
    press(&mut sh, "l");
    assert_eq!(index(&sh), Some(1999));
    for _ in 0..3 {
        let e = key_event(&sh, "p");
        sh.queue(e);
    }
    // An arrow goes first (the router's own), then what logic asked.
    let e = key_event(&sh, "Up");
    sh.queue(e);
    assert_eq!(index(&sh), Some(1998), "the arrow, at once");
    sh.run(1);
    assert_eq!(index(&sh), Some(1995));
    sh.settle();
    assert_eq!(index(&sh), Some(1995));
    assert!(
        sh.router.selected(list(&sh)).is_some(),
        "it landed on a mounted row"
    );
}

/// Several in one handler build on each other, and `activate` after them
/// goes to where they end.
#[test]
fn several_in_one_handler_build_on_each_other() {
    let mut sh = shell(20, "");
    press(&mut sh, "f");
    press(&mut sh, "t");
    assert_eq!(index(&sh), Some(2));
    assert_eq!(sh.num("activated"), 2.0);
}

/// Past the mounted window: the selection stands on its way, the list
/// scrolls and logic mounts it, and an `activate` meanwhile goes to it when
/// it lands.
#[test]
fn the_actions_reach_rows_beyond_the_window() {
    let mut sh = shell(2000, "");
    let e = key_event(&sh, "l");
    sh.send(e);
    assert_eq!(index(&sh), Some(1999));
    assert_eq!(sh.router.selected(list(&sh)), None, "not mounted yet");
    let e = key_event(&sh, "a");
    sh.send(e);
    sh.settle();
    assert!(sh.router.selected(list(&sh)).is_some(), "it landed");
    assert_eq!(sh.num("activated"), 1999.0);
    let offset = sh.r.scroll_offset(list(&sh)).unwrap();
    assert_eq!(offset, 2000.0 * 20.0 - 100.0, "scrolled into view");
    press(&mut sh, "f");
    sh.settle();
    assert_eq!(sh.r.scroll_offset(list(&sh)), Some(0.0));
}

/// `selected_index` follows the selection, from a key or from code, and is
/// null when the selection leaves with its rows.
#[test]
fn selected_index_is_reactive_and_null_without_a_selection() {
    let mut sh = shell(20, "");
    let read = |sh: &Shell| sh.num("at");
    assert_eq!(read(&sh), -9.0, "nothing has changed yet");
    press(&mut sh, "Down");
    assert_eq!(read(&sh), 0.0);
    press(&mut sh, "n");
    assert_eq!(read(&sh), 1.0);
    press(&mut sh, "z");
    assert_eq!(read(&sh), 19.0);
    sh.set("items", Value::list(Vec::new()));
    assert_eq!(index(&sh), None);
    assert_eq!(read(&sh), -1.0);
}

fn check_codes(src: &str) -> Vec<String> {
    let mut map = strand_compiler::SourceMap::new();
    map.add("shell.strand", src.to_string());
    strand_compiler::compile(&map)
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

/// On an element the checker can see is no list, an error, through a
/// component's root too; a list, by `id:` or a component's root, checks.
#[test]
fn on_another_element_they_are_a_check_error() {
    let with = |el: &str, call: &str| {
        format!(
            "panel L {{ anchor: top_left; width: 300; height: 100\n\
               on key \"n\" {{ {call} }}\n\
               {el}\n\
             }}\n"
        )
    };
    for call in [
        "r.next()",
        "r.prev()",
        "r.first()",
        "r.last()",
        "r.select(1)",
        "r.activate()",
    ] {
        let codes = check_codes(&with("box { id: r }", call));
        assert_eq!(codes, ["check::not_a_list"], "{call}");
    }
    assert!(check_codes(&with("list { id: r }", "r.next()")).is_empty());
    let comp = |root: &str| {
        format!(
            "component C {{ {root} {{ slot }} }}\n{}",
            with("C { id: r }", "r.next()")
        )
    };
    assert_eq!(check_codes(&comp("col")), ["check::not_a_list"]);
    assert!(check_codes(&comp("list")).is_empty());
}

/// Through `self`, which it can't see through, it does nothing at run time,
/// and the shell's log says so once.
#[test]
fn through_self_on_another_element_it_is_a_logged_no_op() {
    let mut sh = Shell::new(
        "panel L { anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
           on key \"n\" { self.next() }\n\
         }\n",
    );
    sh.keyboard_to("L");
    press(&mut sh, "n");
    press(&mut sh, "n");
    assert_eq!(sh.notices.len(), 1, "{:?}", sh.notices);
    assert!(sh.notices[0].contains("not a `list`"), "{:?}", sh.notices);
}

/// `wrap: true`: arrows and `next()`/`prev()` go round; Page, Home and End
/// stop at the ends; by default nothing goes round.
#[test]
fn wrap_goes_round_for_arrows_and_actions_only() {
    let mut sh = shell(20, "wrap: true");
    press(&mut sh, "f");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(19), "Up off the top");
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(0), "Down off the bottom");
    press(&mut sh, "p");
    assert_eq!(index(&sh), Some(19), "prev()");
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(0), "next()");
    press(&mut sh, "Page_Up");
    assert_eq!(index(&sh), Some(0), "Page does not go round");
    press(&mut sh, "End");
    press(&mut sh, "Page_Down");
    assert_eq!(index(&sh), Some(19));
    press(&mut sh, "End");
    assert_eq!(index(&sh), Some(19), "End stays at the end");
    press(&mut sh, "Home");
    assert_eq!(index(&sh), Some(0));

    let mut sh = shell(3, "");
    press(&mut sh, "f");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(0), "off by default");
    press(&mut sh, "l");
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(2));
}

/// Going round to a row past the window lands as any other.
#[test]
fn wrap_reaches_a_row_beyond_the_window() {
    let mut sh = shell(2000, "wrap: true");
    press(&mut sh, "f");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(1999));
    sh.settle();
    assert!(sh.router.selected(list(&sh)).is_some());
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(0));
    sh.settle();
    assert!(sh.router.selected(list(&sh)).is_some());
    assert_eq!(sh.r.scroll_offset(list(&sh)), Some(0.0));
}

/// A `nav:` list remounted (an `if` turning false and true) is a new node:
/// `next()` goes to the list as it is, and its selection starts over.
#[test]
fn a_remounted_list_stays_correct_with_nav() {
    let mut sh = Shell::new(
        "state items: [int] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]\n\
         state shown = true\n\
         state q = \"\"\n\
         state at = -9\n\
         panel L { anchor: top_left; width: 300; height: 130; keyboard: exclusive\n\
           on key \"ctrl+n\" { results.next() }\n\
           col {\n\
             input { height: 30; text: <-> q; focus: true; nav: results }\n\
             if shown {\n\
               list { id: results; height: 100\n\
                 for i in items key i { box { height: 20 } }\n\
               }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    sh.settle();
    let ctrl_n = |sh: &mut Shell| {
        let surface = sh.keyboard.unwrap();
        sh.send(InputEvent::Key {
            surface,
            key: KeyInput {
                name: "n".into(),
                text: String::new(),
                state: ButtonState::Pressed,
                repeat: false,
                modifiers: Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                },
                time: 0,
                latin: None,
            },
        });
        sh.run(4);
    };
    let first = list(&sh);
    assert_eq!(sh.router.selected_index(first), Some(0), "nav's first row");
    ctrl_n(&mut sh);
    ctrl_n(&mut sh);
    assert_eq!(sh.router.selected_index(first), Some(2));
    sh.set("shown", Value::Bool(false));
    // Asked of no list: nothing, and no error.
    ctrl_n(&mut sh);
    sh.set("shown", Value::Bool(true));
    let again = list(&sh);
    assert_ne!(first, again);
    assert_eq!(sh.router.selected_index(again), Some(0), "it starts over");
    ctrl_n(&mut sh);
    assert_eq!(sh.router.selected_index(again), Some(1));
    // A new query starts over too, even after a move from code.
    sh.set("q", Value::text("x"));
    assert_eq!(sh.router.selected_index(again), Some(0));
}

/// A reload keeps the row selected and what `selected_index` says of it.
#[test]
fn a_reload_keeps_the_selection() {
    let src = |extra: &str| {
        format!(
            "state items: [int] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]\n\
             state at = -9\n\
             panel L {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
               on key \"n\" {{ results.next() }}\n\
               on change results.selected_index {{ at = results.selected_index ?? -1 }}\n\
               text join(\"\", results.selected_index ?? -1) {{ {extra} }}\n\
               list {{ id: results; height: 80; focus: true\n\
                 for i in items key i {{ box {{ height: 20 }} }}\n\
               }}\n\
             }}\n"
        )
    };
    let mut sh = Shell::reloadable(&src(""));
    sh.keyboard_to("L");
    sh.settle();
    for _ in 0..4 {
        press(&mut sh, "n");
    }
    let l = list(&sh);
    assert_eq!(sh.router.selected_index(l), Some(3));
    sh.reload(&src("width: 40"));
    sh.settle();
    assert_eq!(list(&sh), l, "the same node");
    assert_eq!(sh.router.selected_index(l), Some(3));
    let label =
        sh.r.tree()
            .get(sh.nodes("L", NodeKind::Text)[0])
            .and_then(|n| n.get(strand_scene::Prop::Text).cloned());
    assert_eq!(label, Some(strand_scene::PropValue::Text("3".into())));
    press(&mut sh, "n");
    assert_eq!(sh.router.selected_index(l), Some(4));
}

/// A panel with a list and claims, for the shapes below: rows as given in
/// `row`, the list's `props`, `w` by `h` itself.
fn shaped(count: u32, w: u32, h: u32, props: &str, row: &str) -> Shell {
    let items = (0..count)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut sh = Shell::new(&format!(
        "state items: [int] = [{items}]\n\
         state activated = -1\n\
         panel L {{ anchor: top_left; width: {w}; height: {h}; keyboard: exclusive; bg: #000000\n\
           on key \"n\" {{ results.next() }}\n\
           on key \"p\" {{ results.prev() }}\n\
           on key \"l\" {{ results.last() }}\n\
           list {{ id: results; focus: true; grow: 1; {props}\n\
             for i in items key i {{\n\
               box {{ {row}; bg: #203040\n\
                 when self.selected {{ bg: #ff0000 }}\n\
                 on activate {{ activated = i }}\n\
               }}\n\
             }}\n\
           }}\n\
         }}\n"
    ));
    sh.keyboard_to("L");
    sh.settle();
    sh
}

fn row_box(sh: &Shell, i: usize) -> strand_scene::LogicalRect {
    let row = sh.r.tree().get(list(sh)).unwrap().children[i];
    let s = sh.surface("L").id;
    *sh.r.boxes(s).unwrap().rects.get(&row).unwrap()
}

/// `axis: horizontal`: rows lie in a line, Left and Right move the selection
/// and scroll it into view along x, and Up and Down do nothing.
#[test]
fn a_horizontal_lists_keys_and_layout_run_along_x() {
    let mut sh = shaped(20, 300, 100, "axis: horizontal; gap: 10", "width: 90");
    let (a, b) = (row_box(&sh, 0), row_box(&sh, 1));
    assert_eq!(
        (a.x, a.y, a.w, a.h),
        (0.0, 0.0, 90.0, 100.0),
        "fills across"
    );
    assert_eq!((b.x, b.y), (100.0, 0.0));
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(0), "from none: the first");
    press(&mut sh, "Right");
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(2));
    press(&mut sh, "Left");
    assert_eq!(index(&sh), Some(1));
    press(&mut sh, "Down");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(1), "not its keys");
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(2), "next() goes along it");
    for _ in 0..3 {
        press(&mut sh, "Right");
    }
    sh.settle();
    assert_eq!(index(&sh), Some(5));
    // The view moved along x, as little as it takes: row 5's right edge at
    // the view's.
    assert_eq!(
        sh.r.scroll_offset(list(&sh)),
        Some(5.0 * 100.0 + 90.0 - 300.0)
    );
    assert_eq!(row_box(&sh, 5).x, 300.0 - 90.0);
    assert_eq!(row_box(&sh, 5).y, 0.0, "nothing moved along y");
    press(&mut sh, "Page_Down");
    assert!(index(&sh) > Some(5), "Page keys move it by a view");
    press(&mut sh, "End");
    sh.settle();
    assert_eq!(index(&sh), Some(19));
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(19), "stops at the end");
}

/// A horizontal list virtualises along x: 2,000 rows mount a window, the
/// selection goes past it and lands, and no frame shows a gap.
#[test]
fn a_horizontal_list_virtualises_along_x() {
    let mut sh = shaped(2000, 300, 60, "axis: horizontal", "width: 50");
    let mounted = sh.r.tree().get(list(&sh)).unwrap().children.len();
    assert!(mounted < 100, "only a window mounts: {mounted}");
    press(&mut sh, "l");
    assert_eq!(index(&sh), Some(1999));
    sh.settle();
    let sel = sh.router.selected(list(&sh)).expect("it landed");
    let s = sh.surface("L").id;
    let r = *sh.r.boxes(s).unwrap().rects.get(&sel).unwrap();
    assert_eq!(r.x + r.w, 300.0, "the last row at the view's end");
    let st = sh.r.scroll_state(list(&sh)).unwrap();
    assert_eq!(st.offset, 2000.0 * 50.0 - 300.0);
    assert!(st.across.is_none(), "a list has one offset");
    press(&mut sh, "Home");
    sh.settle();
    assert_eq!(sh.r.scroll_offset(list(&sh)), Some(0.0));
    assert_eq!(sh.r.list_frames().gaps, 0);
}

/// A plain wheel scrolls a horizontal list sideways.
#[test]
fn a_wheel_scrolls_a_horizontal_list() {
    let mut sh = shaped(50, 300, 60, "axis: horizontal", "width: 50");
    sh.move_to(100.0, 30.0);
    sh.wheel(0.0, 2.0);
    sh.settle();
    let at = sh.r.scroll_offset(list(&sh)).unwrap();
    assert!(at > 0.0, "{at}");
    assert_eq!(row_box(&sh, 0).x, -at);
    assert_eq!(row_box(&sh, 0).y, 0.0);
}

/// With no height it is as tall as its tallest row, and shorter rows fill it.
#[test]
fn a_horizontal_list_is_as_tall_as_its_tallest_row() {
    let mut sh = Shell::new(
        "panel L { anchor: top_left; width: 300; height: 200; keyboard: exclusive\n\
           col {\n\
             list { axis: horizontal; gap: 5\n\
               box { width: 40; height: 30 }\n\
               box { width: 40; height: 50 }\n\
               box { width: 40 }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.settle();
    let l = list(&sh);
    let s = sh.surface("L").id;
    let boxes = sh.r.boxes(s).unwrap();
    assert_eq!(boxes.rects[&l].h, 50.0);
    assert_eq!(boxes.rects[&l].w, 300.0);
    let rows = &sh.r.tree().get(l).unwrap().children;
    assert_eq!(boxes.rects[&rows[0]].h, 30.0, "its own height");
    assert_eq!(boxes.rects[&rows[2]].h, 50.0, "fills");
    assert_eq!(boxes.rects[&rows[2]].x, 90.0);
}

/// In an `input` with `nav:` to a horizontal list, Left and Right stay the
/// caret's; a claim can take them.
#[test]
fn an_inputs_caret_keeps_left_and_right() {
    let src = |claim: &str| {
        format!(
            "state q = \"ab\"\n\
             panel L {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
               {claim}\n\
               col {{\n\
                 input {{ height: 30; text: <-> q; focus: true; nav: results }}\n\
                 list {{ id: results; axis: horizontal; height: 60\n\
                   for i in [\"a\", \"b\", \"c\"] key i {{ box {{ width: 50 }} }}\n\
                 }}\n\
               }}\n\
             }}\n"
        )
    };
    let mut sh = Shell::new(&src(""));
    sh.keyboard_to("L");
    sh.settle();
    assert_eq!(index(&sh), Some(0));
    let typed = |sh: &Shell| match sh.inst.value_of("shell", "q") {
        Ok(Value::Text(t)) => t.to_string(),
        v => panic!("q is {v:?}"),
    };
    press(&mut sh, "Left");
    assert_eq!(index(&sh), Some(0), "the list stays");
    press(&mut sh, "x");
    assert_eq!(typed(&sh), "axb", "the caret moved");
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "y");
    assert_eq!(typed(&sh), "axby");

    let mut sh = Shell::new(&src(
        "on key \"right\" { results.next() }\n on key \"left\" { results.prev() }",
    ));
    sh.keyboard_to("L");
    sh.settle();
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(1), "a claim took it");
    press(&mut sh, "Left");
    assert_eq!(index(&sh), Some(0));
}

/// A horizontal list's row opens its popup with Down, and in a popup opened
/// from another, Up closes it; a vertical list's Right and Left stay as they
/// were.
#[test]
fn a_horizontal_lists_submenu_key_is_down() {
    let src = |axis: &str| {
        format!(
            "state sub = false\n\
             panel L {{ anchor: top_left; width: 300; height: 100; keyboard: exclusive\n\
               list {{ focus: true; height: 60; {axis}\n\
                 box {{ width: 50; height: 20\n\
                   popup {{ open: <-> sub; width: 50; height: 50; text \"x\" }}\n\
                 }}\n\
               }}\n\
             }}\n"
        )
    };
    let mut sh = Shell::new(&src("axis: horizontal"));
    sh.keyboard_to("L");
    sh.settle();
    press(&mut sh, "Right");
    assert!(!sh.inst.value_of("shell", "sub").unwrap().truthy());
    press(&mut sh, "Down");
    assert!(sh.inst.value_of("shell", "sub").unwrap().truthy(), "Down");

    let mut sh = Shell::new(&src(""));
    sh.keyboard_to("L");
    sh.settle();
    press(&mut sh, "Down");
    assert!(!sh.inst.value_of("shell", "sub").unwrap().truthy());
    press(&mut sh, "Right");
    assert!(sh.inst.value_of("shell", "sub").unwrap().truthy(), "Right");
}

/// `axis: both` on a list and `axis` beside `columns` are check errors;
/// `axis: vertical` beside `columns` isn't.
#[test]
fn axis_both_and_a_horizontal_grid_are_check_errors() {
    let list = |p: &str| {
        format!("panel L {{ anchor: top_left; width: 100; height: 100\n  list {{ {p} }}\n}}\n")
    };
    assert_eq!(check_codes(&list("axis: both")), ["check::list_axis"]);
    assert_eq!(
        check_codes(&list("axis: horizontal; columns: 2")),
        ["check::list_axis"]
    );
    assert!(check_codes(&list("axis: vertical; columns: 2")).is_empty());
    assert!(check_codes(&list("axis: horizontal")).is_empty());
    assert!(check_codes(&list("columns: 2; wrap: true")).is_empty());
}

/// `columns: 3`, seven rows: Up and Down move a line, Left and Right one and
/// not past a line's end, `next()` and `prev()` one across lines, and Down
/// from above an empty cell of the short last line goes to the last row.
#[test]
fn a_grids_arrows_move_by_geometry() {
    let mut sh = shaped(7, 300, 200, "columns: 3", "height: 40");
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(0), "from none: the first");
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(3));
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(6));
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(6), "the last line: stays");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(3));
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(4));
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(5));
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(5), "no line wrapping");
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(6), "above an empty cell: the last row");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(3));
    press(&mut sh, "Left");
    assert_eq!(index(&sh), Some(3), "no line wrapping");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "n");
    press(&mut sh, "n");
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(3), "next() goes across lines");
    press(&mut sh, "p");
    assert_eq!(index(&sh), Some(2));
    press(&mut sh, "Return");
    assert_eq!(sh.num("activated"), 2.0);
    press(&mut sh, "Home");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "End");
    assert_eq!(index(&sh), Some(6));
}

/// `wrap: true` on a grid: off an end goes round. Up from the first line to
/// the same column of the last, or its last row if that cell is empty; Down
/// from the last line to the same column of the first; Left and Right on to
/// the next line, and round.
#[test]
fn a_wrapping_grid_goes_round() {
    let mut sh = shaped(7, 300, 200, "columns: 3; wrap: true", "height: 40");
    press(&mut sh, "Home");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(6), "column 0");
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(0), "back to column 0");
    press(&mut sh, "Right");
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(6), "column 1 is empty below: the last row");
    press(&mut sh, "Home");
    press(&mut sh, "Left");
    assert_eq!(index(&sh), Some(6), "Left off the start");
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(0), "Right off the end");
    press(&mut sh, "Right");
    press(&mut sh, "Right");
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(3), "on to the next line");
    press(&mut sh, "Left");
    assert_eq!(index(&sh), Some(2), "and back");
    press(&mut sh, "End");
    press(&mut sh, "n");
    assert_eq!(index(&sh), Some(0), "next() too");
    press(&mut sh, "Page_Up");
    assert_eq!(index(&sh), Some(0), "Page does not");
}

/// Page keys move a view of lines; the selection scrolls by the line; rows
/// past the window land, in the column they were in.
#[test]
fn a_grid_virtualises_and_scrolls_by_the_line() {
    let mut sh = shaped(3000, 300, 100, "columns: 3", "height: 20");
    let mounted = sh.r.tree().get(list(&sh)).unwrap().children.len();
    assert!(mounted < 200, "only a window mounts: {mounted}");
    assert_eq!(mounted % 3, 0, "whole lines: {mounted}");
    press(&mut sh, "Home");
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(1));
    press(&mut sh, "Page_Down");
    assert_eq!(index(&sh), Some(1 + 5 * 3), "five lines show");
    let st = sh.r.scroll_state(list(&sh)).unwrap();
    assert_eq!(st.content, 1000.0 * 20.0, "a thousand lines");
    assert_eq!(sh.r.scroll_offset(list(&sh)), Some(20.0), "by the line");
    press(&mut sh, "l");
    assert_eq!(index(&sh), Some(2999));
    sh.settle();
    assert!(sh.router.selected(list(&sh)).is_some(), "it landed");
    assert_eq!(sh.r.scroll_offset(list(&sh)), Some(1000.0 * 20.0 - 100.0));
    let first = sh.router.selected_index(list(&sh)).unwrap();
    press(&mut sh, "Up");
    assert_eq!(index(&sh), Some(first - 3));
    assert_eq!(sh.r.list_frames().gaps, 0);
    // A short last line counts as a line.
    let sh = shaped(3001, 300, 100, "columns: 3", "height: 20");
    assert_eq!(sh.r.scroll_state(list(&sh)).unwrap().content, 1001.0 * 20.0);
}

/// `columns:` divides the list's width among cells, `gap` between; a line is
/// as tall as its tallest, whose shorter neighbours fill it; drawn so.
#[test]
fn a_grid_lays_out_cells_and_draws_them() {
    let mut sh = Shell::new(
        "panel L { anchor: top_left; width: 300; height: 120; keyboard: exclusive; bg: #000000\n\
           list { id: results; focus: true; grow: 1; columns: 3; gap: 6; pad: 10\n\
             box { height: 30; bg: #ff0000 }\n\
             box { height: 40; bg: #00ff00 }\n\
             box { height: 20; bg: #0000ff }\n\
             box { height: 20; bg: #ffff00 }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    sh.settle();
    let at = |i| {
        let r = row_box(&sh, i);
        (r.x, r.y, r.w, r.h)
    };
    // (280 + 6) / 3 = 95.33 a cell and gap: whole pixels, the last one ending
    // at the content's edge.
    assert_eq!(at(0), (10.0, 10.0, 89.0, 30.0), "its own height");
    assert_eq!(at(1), (105.0, 10.0, 90.0, 40.0));
    assert_eq!(at(2), (201.0, 10.0, 89.0, 20.0));
    assert_eq!(
        at(3),
        (10.0, 56.0, 89.0, 20.0),
        "the next line, at 10 + 40 + 6"
    );
    let shot = sh.shot("L");
    shell::assert_matches_ref("list_columns", shot.0, &shot.1, 2);
    let red = shell::px(&shot, 20, 20);
    let (green, blue) = (shell::px(&shot, 110, 20), shell::px(&shot, 210, 20));
    assert_eq!((red[2], red[1], red[0]), (255, 0, 0));
    assert_eq!((green[2], green[1], green[0]), (0, 255, 0));
    assert_eq!((blue[2], blue[1], blue[0]), (0, 0, 255));
    assert_eq!(shell::px(&shot, 100, 20)[1], 0, "the gap is the panel's");
    let yellow = shell::px(&shot, 20, 60);
    assert_eq!((yellow[2], yellow[1], yellow[0]), (255, 255, 0));
}

/// Cells with no height of their own: the tallest measures the line and the
/// rest fill it.
#[test]
fn a_grids_line_is_filled_by_its_shorter_cells() {
    let mut sh = Shell::new(
        "panel L { anchor: top_left; width: 300; height: 120\n\
           list { grow: 1; columns: 2\n\
             box { box { height: 10 } }\n\
             box { box { height: 44 } }\n\
           }\n\
         }\n",
    );
    sh.settle();
    assert_eq!(row_box(&sh, 0).h, 44.0);
    assert_eq!(row_box(&sh, 1).h, 44.0);
}

/// A horizontal list and a grid drawn with the selection on them.
#[test]
fn a_horizontal_list_draws_along_x() {
    let mut sh = shaped(9, 200, 60, "axis: horizontal; gap: 10", "width: 50");
    press(&mut sh, "Right");
    press(&mut sh, "Right");
    let shot = sh.shot("L");
    shell::assert_matches_ref("list_horizontal", shot.0, &shot.1, 2);
    let (off, on) = (shell::px(&shot, 25, 30), shell::px(&shot, 85, 30));
    assert_eq!((off[2], off[1], off[0]), (0x20, 0x30, 0x40));
    assert_eq!((on[2], on[1], on[0]), (255, 0, 0));
    assert_eq!(shell::px(&shot, 55, 30)[2], 0, "the gap is the panel's");
}

/// A `nav:` grid: Up and Down are the list's, Left and Right the caret's.
#[test]
fn a_nav_grid_leaves_left_and_right_to_the_caret() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         panel L { anchor: top_left; width: 300; height: 130; keyboard: exclusive\n\
           col {\n\
             input { height: 30; text: <-> q; focus: true; nav: results }\n\
             list { id: results; height: 100; columns: 2\n\
               for i in [\"a\", \"b\", \"c\"] key i { box { height: 20 } }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    sh.settle();
    press(&mut sh, "Right");
    assert_eq!(index(&sh), Some(0));
    press(&mut sh, "Down");
    assert_eq!(index(&sh), Some(2));
}

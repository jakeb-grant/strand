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

//! (flex) Interaction additions of the 2026-10-24 gap review
//! (decisions.md `## flex`), a whole shell driven through the host loop
//! (`tests/shell`): the cursor a node asks for.

mod shell;

use shell::Shell;
use strand_scene::CursorShape;

fn cursor(sh: &Shell, surface: &str) -> CursorShape {
    sh.r.cursor(sh.surface(surface).id)
}

/// The innermost node under the pointer with a `cursor:` decides; a
/// held press keeps the node it went down on (`grabbing` while dragged
/// off it); an `input` shows `text` unless it says otherwise; anything
/// else, and nothing, is the arrow.
#[test]
fn the_cursor_is_the_innermost_nodes() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         panel P { anchor: top_left; width: 300; height: 100\n\
           row { width: 300; height: 100\n\
             box { width: 100; height: 100; cursor: pointer\n\
               box { width: 50; height: 50; cursor: grab\n\
                 when pressed { cursor: grabbing }\n\
               }\n\
             }\n\
             input { width: 100; height: 40; text: <-> q }\n\
             box { width: 100; height: 100 }\n\
           }\n\
         }\n",
    );
    assert_eq!(cursor(&sh, "P"), CursorShape::Default);
    sh.move_to(75.0, 75.0);
    assert_eq!(cursor(&sh, "P"), CursorShape::Pointer);
    sh.move_to(10.0, 10.0);
    assert_eq!(cursor(&sh, "P"), CursorShape::Grab);
    sh.button(true);
    assert_eq!(cursor(&sh, "P"), CursorShape::Grabbing);
    // Dragged off it, over a node without one: still the press's.
    sh.move_to(250.0, 50.0);
    assert_eq!(cursor(&sh, "P"), CursorShape::Grabbing);
    sh.button(false);
    assert_eq!(cursor(&sh, "P"), CursorShape::Default);
    sh.move_to(150.0, 50.0);
    assert_eq!(cursor(&sh, "P"), CursorShape::Text);
    sh.move_to(600.0, 600.0);
    assert_eq!(cursor(&sh, "P"), CursorShape::Default);
}

/// `anchor: pointer`: a popup opens where the press that opened it went
/// down, 1 × 1 there, and keeps that place while it stays open; opened
/// again by another press, it opens at that one.
#[test]
fn a_popup_at_the_pointer_opens_at_the_press() {
    let mut sh = Shell::new(
        "state menu = false\n\
         bar Top { height: 30\n\
           box { width: 600; height: 30\n\
             on click { menu = !menu }\n\
             popup { anchor: pointer; open: menu; width: 120; height: 80 }\n\
             popup { open: menu; width: 120; height: 80 }\n\
           }\n\
         }\n",
    );
    let popup = |sh: &Shell, at_pointer: bool| {
        sh.popups()
            .into_iter()
            .find(|s| s.at_pointer == at_pointer)
            .cloned()
            .expect("the popup's spec")
    };
    sh.click(210.0, 12.0);
    let spec = popup(&sh, true);
    assert!(spec.open);
    let at = spec.anchor_rect.expect("anchored");
    assert_eq!((at.x, at.y, at.w, at.h), (210.0, 12.0, 1.0, 1.0));
    // One without `anchor: pointer` keeps its element's box.
    let below = popup(&sh, false).anchor_rect.unwrap();
    assert_eq!((below.x, below.w), (0.0, 600.0));
    // Closed and opened again further along: there.
    sh.click(210.0, 12.0);
    assert!(!popup(&sh, true).open);
    sh.click(450.0, 20.0);
    let at = popup(&sh, true).anchor_rect.unwrap();
    assert_eq!((at.x, at.y), (450.0, 20.0));
}

const FORM: &str = "state shown = true\n\
    state which = 0\n\
    state a = \"\"\n\
    state b = \"\"\n\
    state c = \"\"\n\
    panel Form { anchor: top_left; width: 300; height: 200; open: shown\n\
      col { gap: 10\n\
        input { width: 200; height: 30; text: <-> a; focus: which == 0 }\n\
        box { width: 200; height: 30 }\n\
        input { width: 200; height: 30; text: <-> b; focus: which == 1 }\n\
        input { width: 200; height: 30; text: <-> c }\n\
      }\n\
    }\n";

/// Which of `inputs` render draws focused (with a caret).
fn focused(sh: &Shell, inputs: &[strand_scene::NodeId]) -> Vec<strand_scene::NodeId> {
    let f = &sh.r.widgets().focused;
    inputs.iter().copied().filter(|n| f.contains(n)).collect()
}

/// Tab and Shift+Tab move focus between a surface's inputs in tree
/// order, round from the last to the first.
#[test]
fn tab_moves_focus_between_inputs() {
    let mut sh = Shell::new(FORM);
    let inputs = sh.nodes("Form", strand_scene::NodeKind::Input);
    sh.keyboard_to("Form");
    assert_eq!(focused(&sh, &inputs), [inputs[0]]);
    sh.key("Tab", false);
    assert_eq!(focused(&sh, &inputs), [inputs[1]]);
    sh.key("Tab", false);
    assert_eq!(focused(&sh, &inputs), [inputs[2]]);
    sh.key("Tab", false);
    assert_eq!(focused(&sh, &inputs), [inputs[0]], "round to the first");
    sh.key("Tab", true);
    assert_eq!(focused(&sh, &inputs), [inputs[2]], "Shift+Tab goes back");
    sh.key("ISO_Left_Tab", true);
    assert_eq!(focused(&sh, &inputs), [inputs[1]]);
}

/// `focus:` is reactive: when it turns true on an input, focus moves
/// there on a surface that has the keyboard.
#[test]
fn focus_moves_when_focus_turns_true() {
    let mut sh = Shell::new(FORM);
    let inputs = sh.nodes("Form", strand_scene::NodeKind::Input);
    sh.keyboard_to("Form");
    assert_eq!(focused(&sh, &inputs), [inputs[0]]);
    sh.set("which", strand_compiler::vm::Value::int(1));
    assert_eq!(focused(&sh, &inputs), [inputs[1]]);
    sh.set("which", strand_compiler::vm::Value::int(0));
    assert_eq!(focused(&sh, &inputs), [inputs[0]]);
}

/// A surface closed while an input held focus lets that focus go, so
/// opened again only the input focus gives then has a caret (it kept
/// the old one too: two carets after a reopen).
#[test]
fn a_reopened_surface_has_one_caret() {
    let mut sh = Shell::new(FORM);
    let inputs = sh.nodes("Form", strand_scene::NodeKind::Input);
    sh.keyboard_to("Form");
    sh.click(100.0, 135.0);
    assert_eq!(
        focused(&sh, &inputs),
        [inputs[2]],
        "a click focuses the third"
    );
    sh.set("shown", strand_compiler::vm::Value::Bool(false));
    assert!(
        focused(&sh, &inputs).is_empty(),
        "closed, nothing is focused"
    );
    sh.set("shown", strand_compiler::vm::Value::Bool(true));
    sh.keyboard_to("Form");
    assert_eq!(focused(&sh, &inputs), [inputs[0]], "one caret");
}

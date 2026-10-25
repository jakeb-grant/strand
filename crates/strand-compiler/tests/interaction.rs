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

/// `Node.pointer` follows content that moves under a still pointer: a
/// wheel scroll moves a node 30 px up, so the pointer is 30 px further
/// down in it, without the mouse moving.
#[test]
fn the_pointer_follows_content_moved_under_it() {
    let mut sh = Shell::new(
        "state py = 0.0\n\
         panel P { anchor: top_left; width: 200; height: 200\n\
           scroll { width: 200; height: 200\n\
             box { width: 200; height: 600\n\
               on change self.pointer { py = self.pointer?.y ?? -1.0 }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.move_to(100.0, 51.0);
    sh.settle();
    assert_eq!(sh.num("py"), 51.0);
    sh.wheel(0.0, 2.0);
    sh.settle();
    assert_eq!(sh.num("py"), 81.0, "the content moved 30 px under it");
}

/// `anchor: pointer` takes a press only when it opened the popup: one on
/// another part of the bar, followed by an open from logic (a timer, an
/// IPC call, a key), opens it at its element.
#[test]
fn a_popup_at_the_pointer_ignores_a_press_that_did_not_open_it() {
    let mut sh = Shell::new(
        "state menu = false\n\
         bar Top { height: 30\n\
           row {\n\
             box { width: 300; height: 30; on click { } }\n\
             box { width: 300; height: 30\n\
               popup { anchor: pointer; open: menu; width: 120; height: 80 }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.click(40.0, 12.0);
    sh.set("menu", strand_compiler::vm::Value::Bool(true));
    let spec = sh.popups()[0].clone();
    assert!(spec.open);
    let at = spec.anchor_rect.unwrap();
    assert_eq!((at.x, at.w), (300.0, 300.0), "at its element: {at:?}");
}

/// A press on its element while an `anchor: pointer` popup stays open
/// moves it there once the click is answered (a context menu opened
/// again on another row); a click that closes it does not move it
/// first.
#[test]
fn a_popup_at_the_pointer_moves_to_a_new_press_that_keeps_it_open() {
    let mut sh = Shell::new(
        "state menu = false\n\
         state n = 0\n\
         bar Top { height: 30\n\
           box { width: 600; height: 30\n\
             on secondary { menu = true; n += 1 }\n\
             on click { menu = false }\n\
             bg: n % 2 == 0 ? #101418 : #181c20\n\
             popup { anchor: pointer; open: <-> menu; width: 120; height: 80 }\n\
           }\n\
         }\n",
    );
    let right = |sh: &mut Shell, x: f32| {
        sh.move_to(x, 12.0);
        let s = sh.surface("Top").id;
        for state in [
            strand_scene::ButtonState::Pressed,
            strand_scene::ButtonState::Released,
        ] {
            sh.send(strand_scene::InputEvent::PointerButton {
                surface: s,
                position: strand_scene::LogicalPoint::new(x, 12.0),
                button: strand_scene::input::button::RIGHT,
                state,
                time: sh.now.as_millis() as u32,
            });
            sh.run(2);
        }
    };
    right(&mut sh, 100.0);
    let at = sh.popups()[0].anchor_rect.unwrap();
    assert_eq!(at.x, 100.0);
    right(&mut sh, 450.0);
    let spec = sh.popups()[0].clone();
    assert!(spec.open);
    assert_eq!(spec.anchor_rect.unwrap().x, 450.0, "moved to the new press");
    sh.click(200.0, 12.0);
    let spec = sh.popups()[0].clone();
    assert!(!spec.open, "a click closes it");
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

/// An `on key` that hears Tab takes it: focus stays in the input, so
/// typing still reaches it, and the handler runs (a launcher's Tab that
/// switches its mode, beside a list on the same surface). A surface
/// whose inputs no `on key` hears still moves focus by Tab.
#[test]
fn tab_is_left_to_an_on_key_handler() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         state mode = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             input { width: 200; height: 30; text: <-> q; focus: true\n\
               on key(k) { if k.name == \"Tab\" { mode += 1 } }\n\
             }\n\
             list { width: 200; height: 100\n\
               for x in [\"a\", \"b\"] key x { text x }\n\
             }\n\
           }\n\
         }\n",
    );
    let input = sh.nodes("L", strand_scene::NodeKind::Input)[0];
    sh.keyboard_to("L");
    assert_eq!(focused(&sh, &[input]), [input]);
    sh.key("Tab", false);
    assert_eq!(sh.num("mode"), 1.0, "the handler heard Tab");
    assert_eq!(focused(&sh, &[input]), [input], "focus stays in the input");
    sh.key("Tab", true);
    assert_eq!(focused(&sh, &[input]), [input]);
}

/// Tab passes over what is not shown: an input at `opacity: 0` (or in
/// a faded-out subtree) takes no focus, as it takes no pointer.
#[test]
fn tab_skips_inputs_not_shown() {
    let mut sh = Shell::new(
        "state a = \"\"\n\
         state b = \"\"\n\
         state c = \"\"\n\
         panel F { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             input { width: 200; height: 30; text: <-> a; focus: true }\n\
             input { width: 200; height: 30; text: <-> b; opacity: 0 }\n\
             box { opacity: 0; input { width: 200; height: 30; text: <-> b } }\n\
             input { width: 200; height: 30; text: <-> c }\n\
           }\n\
         }\n",
    );
    let inputs = sh.nodes("F", strand_scene::NodeKind::Input);
    assert_eq!(inputs.len(), 4);
    sh.keyboard_to("F");
    assert_eq!(focused(&sh, &inputs), [inputs[0]]);
    sh.key("Tab", false);
    assert_eq!(focused(&sh, &inputs), [inputs[3]], "past the unseen two");
    sh.key("Tab", false);
    assert_eq!(focused(&sh, &inputs), [inputs[0]]);
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

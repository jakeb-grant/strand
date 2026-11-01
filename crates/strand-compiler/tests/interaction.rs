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

/// (keyboard) A claim on Tab takes it: focus stays in the input, so typing
/// still reaches it, and the handler runs (a launcher's Tab that switches
/// its mode, beside a list on the same surface). An `on key(k)` only
/// observes: it hears Tab, and Tab moves focus.
#[test]
fn tab_is_left_to_a_claim() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         state mode = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             input { width: 200; height: 30; text: <-> q; focus: true\n\
               on key \"tab\", \"shift+tab\" { mode += 1 }\n\
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
    assert_eq!(sh.num("mode"), 1.0, "the claim ran");
    assert_eq!(focused(&sh, &[input]), [input], "focus stays in the input");
    sh.key("ISO_Left_Tab", true);
    assert_eq!(sh.num("mode"), 2.0, "Shift+Tab is its own pattern");
    assert_eq!(focused(&sh, &[input]), [input]);
}

/// (keyboard) An `on key(k)` hears Tab and leaves it to its default.
#[test]
fn an_observer_does_not_stop_tab() {
    let mut sh = Shell::new(
        "state a = \"\"\n\
         state b = \"\"\n\
         state heard = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             on key(k) { if k.name == \"Tab\" { heard += 1 } }\n\
             input { width: 200; height: 30; text: <-> a; focus: true }\n\
             input { width: 200; height: 30; text: <-> b }\n\
           }\n\
         }\n",
    );
    let inputs = sh.nodes("L", strand_scene::NodeKind::Input);
    sh.keyboard_to("L");
    assert_eq!(focused(&sh, &inputs), [inputs[0]]);
    sh.key("Tab", false);
    assert_eq!(sh.num("heard"), 1.0, "it heard");
    assert_eq!(focused(&sh, &inputs), [inputs[1]], "and focus moved");
}

/// A key pressed with its text, and what else it holds.
fn press(sh: &mut Shell, name: &str, text: &str, ctrl: bool) {
    let surface = sh.keyboard.unwrap();
    let key = strand_scene::KeyInput {
        name: name.into(),
        text: text.into(),
        state: strand_scene::ButtonState::Pressed,
        repeat: false,
        modifiers: strand_scene::Modifiers {
            ctrl,
            ..Default::default()
        },
        time: 0,
        latin: None,
    };
    sh.send(strand_scene::InputEvent::Key { surface, key });
    sh.run(2);
}

fn text_of(sh: &Shell, name: &str) -> String {
    match sh.inst.value_of("shell", name) {
        Ok(strand_compiler::vm::Value::Text(t)) => t.to_string(),
        v => panic!("{name} is {v:?}"),
    }
}

fn truth_of(sh: &Shell, name: &str) -> bool {
    sh.inst.value_of("shell", name).unwrap().truthy()
}

/// (keyboard) A claimed key is not typed; the rest are. It claims with
/// modifiers exact, so Ctrl+N is not N.
#[test]
fn a_claim_stops_typing_in_an_input() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         state n = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             on key \"x\", \"ctrl+n\" { n += 1 }\n\
             input { width: 200; height: 30; text: <-> q; focus: true }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    press(&mut sh, "x", "x", false);
    assert_eq!(sh.num("n"), 1.0);
    assert_eq!(text_of(&sh, "q"), "", "not typed");
    press(&mut sh, "y", "y", false);
    assert_eq!(sh.num("n"), 1.0);
    assert_eq!(text_of(&sh, "q"), "y", "the rest are");
    press(&mut sh, "n", "", true);
    assert_eq!(sh.num("n"), 2.0);
    press(&mut sh, "n", "n", false);
    assert_eq!(sh.num("n"), 2.0, "no Ctrl");
    assert_eq!(text_of(&sh, "q"), "yn");
}

/// (keyboard) A claim on Return comes before `on activate`, in an `input`
/// and on a list's row.
#[test]
fn a_claim_on_return_beats_on_activate() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         state claimed = 0\n\
         state activated = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           input { width: 200; height: 30; text: <-> q; focus: true\n\
             on key \"enter\" { claimed += 1 }\n\
             on activate { activated += 1 }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    sh.key("Return", false);
    sh.key("KP_Enter", false);
    assert_eq!(sh.num("claimed"), 2.0);
    assert_eq!(sh.num("activated"), 0.0);
}

/// (keyboard) A claim on Escape stops a two-way `open` closing while its
/// `when` holds, and with it false Escape carries on as if it weren't
/// there: a flip reaches render as any prop does.
#[test]
fn a_claim_in_when_stops_escape_only_while_it_holds() {
    let mut sh = Shell::new(
        "export state shown = true\n\
         state deep = true\n\
         state backs = 0\n\
         panel L { anchor: top_left; width: 300; height: 200; open: <-> shown\n\
           when deep { on key \"esc\" { deep = false; backs += 1 } }\n\
           box { width: 10; height: 10 }\n\
         }\n",
    );
    sh.keyboard_to("L");
    sh.key("Escape", false);
    assert_eq!(sh.num("backs"), 1.0, "the claim ran");
    assert!(truth_of(&sh, "shown"), "Escape did not close it");
    sh.settle();
    sh.key("Escape", false);
    assert_eq!(sh.num("backs"), 1.0, "the condition is false now");
    assert!(!truth_of(&sh, "shown"), "Escape closed it");
}

/// (keyboard) Flipping a `when` changes what is claimed, both ways: the
/// prop render matches, and the key's effect.
#[test]
fn flipping_when_changes_the_claim() {
    let mut sh = Shell::new(
        "state on = false\n\
         state q = \"\"\n\
         state n = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           input { width: 200; height: 30; text: <-> q; focus: true\n\
             when on { on key \"x\" { n += 1 } }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    press(&mut sh, "x", "x", false);
    assert_eq!((sh.num("n"), text_of(&sh, "q").as_str()), (0.0, "x"));
    sh.set("on", strand_compiler::vm::Value::Bool(true));
    press(&mut sh, "x", "x", false);
    assert_eq!((sh.num("n"), text_of(&sh, "q").as_str()), (1.0, "x"));
    sh.set("on", strand_compiler::vm::Value::Bool(false));
    press(&mut sh, "x", "x", false);
    assert_eq!((sh.num("n"), text_of(&sh, "q").as_str()), (1.0, "xx"));
}

/// (keyboard) The innermost active claim wins, wherever others are; with
/// its `when` false the next one out does. On one element, the last written
/// active one wins, and each observer stays out of it.
#[test]
fn the_innermost_claim_wins_and_observers_do_not_hear_it() {
    let mut sh = Shell::new(
        "state inner_on = true\n\
         state q = \"\"\n\
         state inner = 0\n\
         state outer = 0\n\
         state first = 0\n\
         state last = 0\n\
         state heard = 0\n\
         state typed = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             on key \"a\" { outer += 1 }\n\
             on key \"b\" { first += 1 }\n\
             on key \"b\" { last += 1 }\n\
             on key(k) { heard += 1 }\n\
             box { width: 10; height: 10\n\
               input { width: 200; height: 30; text: <-> q; focus: true\n\
                 when inner_on { on key \"a\" { inner += 1 } }\n\
                 on key(k) { typed += 1 }\n\
               }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    press(&mut sh, "a", "a", false);
    assert_eq!((sh.num("inner"), sh.num("outer")), (1.0, 0.0));
    assert_eq!(
        (sh.num("typed"), sh.num("heard")),
        (0.0, 0.0),
        "no observer"
    );
    sh.set("inner_on", strand_compiler::vm::Value::Bool(false));
    press(&mut sh, "a", "a", false);
    assert_eq!((sh.num("inner"), sh.num("outer")), (1.0, 1.0));
    press(&mut sh, "b", "b", false);
    assert_eq!(
        (sh.num("first"), sh.num("last")),
        (0.0, 1.0),
        "the last written"
    );
    assert_eq!((sh.num("typed"), sh.num("heard")), (0.0, 0.0));
    assert_eq!(text_of(&sh, "q"), "", "nothing typed");
    // What nothing claims reaches the nearest observer, and the input.
    press(&mut sh, "c", "c", false);
    assert_eq!((sh.num("typed"), sh.num("heard")), (1.0, 0.0));
    assert_eq!(text_of(&sh, "q"), "c");
}

/// (keyboard) `propagate()` in a claim passes to the next enclosing claim of
/// the key; no observer hears it, and the default stays stopped.
#[test]
fn propagate_passes_a_claim_outward_with_the_default_stopped() {
    let mut sh = Shell::new(
        "state q = \"\"\n\
         state inner = 0\n\
         state outer = 0\n\
         state heard = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           col {\n\
             on key \"x\" { outer += 1 }\n\
             box { width: 10; height: 10\n\
               on key(k) { heard += 1 }\n\
               input { width: 200; height: 30; text: <-> q; focus: true\n\
                 on key \"x\" { inner += 1; propagate() }\n\
               }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.keyboard_to("L");
    press(&mut sh, "x", "x", false);
    assert_eq!((sh.num("inner"), sh.num("outer")), (1.0, 1.0));
    assert_eq!(sh.num("heard"), 0.0);
    assert_eq!(text_of(&sh, "q"), "", "the default stays stopped");
}

/// (keyboard) A claim's `(k)` is the key; `k.repeat` says a held key is
/// repeating, claims and observers fire on repeats, and `on key_release(k)`
/// hears a key let go, a claimed one too, claiming nothing.
#[test]
fn key_release_and_repeat_are_delivered() {
    let mut sh = Shell::new(
        "state down = 0\n\
         state repeats = 0\n\
         state claimed = 0\n\
         state claimed_repeats = 0\n\
         state ups = 0\n\
         state up = \"\"\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           on key(k) { down += 1; if k.repeat { repeats += 1 } }\n\
           on key \"x\" (k) { claimed += 1; if k.repeat { claimed_repeats += 1 } }\n\
           on key_release(k) { ups += 1; up = k.name }\n\
         }\n",
    );
    sh.keyboard_to("L");
    let surface = sh.keyboard.unwrap();
    let send = |sh: &mut Shell, name: &str, state, repeat| {
        let key = strand_scene::KeyInput {
            name: name.into(),
            text: String::new(),
            state,
            repeat,
            modifiers: Default::default(),
            time: 0,
            latin: None,
        };
        sh.send(strand_scene::InputEvent::Key { surface, key });
        sh.run(2);
    };
    use strand_scene::ButtonState::{Pressed, Released};
    send(&mut sh, "a", Pressed, false);
    send(&mut sh, "a", Pressed, true);
    send(&mut sh, "a", Released, false);
    assert_eq!((sh.num("down"), sh.num("repeats")), (2.0, 1.0));
    assert_eq!((sh.num("ups"), text_of(&sh, "up").as_str()), (1.0, "a"));
    send(&mut sh, "x", Pressed, false);
    send(&mut sh, "x", Pressed, true);
    assert_eq!((sh.num("claimed"), sh.num("claimed_repeats")), (2.0, 1.0));
    assert_eq!(sh.num("down"), 2.0, "the observer did not hear them");
    send(&mut sh, "x", Released, false);
    assert_eq!((sh.num("ups"), text_of(&sh, "up").as_str()), (2.0, "x"));
}

/// (keyboard) An element's handler in a `when` runs only while it holds:
/// with it false the event goes on out, as if it weren't there.
#[test]
fn a_handler_in_when_runs_while_it_holds() {
    let mut sh = Shell::new(
        "state armed = false\n\
         state inner = 0\n\
         state outer = 0\n\
         panel L { anchor: top_left; width: 300; height: 200\n\
           box { width: 100; height: 100\n\
             on click { outer += 1 }\n\
             box { width: 50; height: 50\n\
               when armed { on click { inner += 1 } }\n\
             }\n\
           }\n\
         }\n",
    );
    sh.click(10.0, 10.0);
    assert_eq!((sh.num("inner"), sh.num("outer")), (0.0, 1.0));
    sh.set("armed", strand_compiler::vm::Value::Bool(true));
    sh.click(10.0, 10.0);
    assert_eq!((sh.num("inner"), sh.num("outer")), (1.0, 1.0));
    sh.set("armed", strand_compiler::vm::Value::Bool(false));
    sh.click(10.0, 10.0);
    assert_eq!((sh.num("inner"), sh.num("outer")), (1.0, 2.0));
}

/// (keyboard) Keys queued while logic is busy: the router judged the second
/// by the old condition, and logic does not run a handler whose `when` is
/// false by then. The key is spent; the default stayed stopped.
#[test]
fn a_claim_queued_past_its_condition_does_not_run() {
    let mut sh = Shell::new(
        "export state shown = true\n\
         state depth = 1\n\
         panel L { anchor: top_left; width: 300; height: 200; open: <-> shown\n\
           when depth > 0 { on key \"esc\" { depth -= 1 } }\n\
         }\n",
    );
    sh.keyboard_to("L");
    let surface = sh.keyboard.unwrap();
    for repeat in [false, true] {
        let key = strand_scene::KeyInput {
            name: "Escape".into(),
            text: String::new(),
            state: strand_scene::ButtonState::Pressed,
            repeat,
            modifiers: Default::default(),
            time: 0,
            latin: None,
        };
        // Both before logic answers the first.
        let mut idle = Vec::new();
        strand_render::host_loop::input(
            &mut sh,
            &strand_scene::InputEvent::Key { surface, key },
            &mut idle,
        )
        .unwrap();
    }
    sh.settle();
    assert_eq!(sh.num("depth"), 0.0, "once, not below zero");
    assert!(truth_of(&sh, "shown"), "and neither closed it");
    sh.key("Escape", false);
    assert!(!truth_of(&sh, "shown"), "the next one does");
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

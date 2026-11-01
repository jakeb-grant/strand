//! (keyboard) Dead keys and Compose on a headless sway, through a virtual
//! keyboard: the composed text and the real key names reach the host, a
//! held result repeats, and a sequence does not survive the focus going.
//!
//! One test, alone in its binary: it sets `XCOMPOSEFILE` for libxkbcommon
//! before anything else runs, so the table does not depend on the machine.

mod common;

use std::io::Write as _;
use std::os::fd::AsFd;
use std::time::Duration;

use common::{Sway, WAIT, layer_spec};
use strand_scene::{Damage, NodeId, NodeKind, PaintTarget, Painter, SurfaceChange, SurfaceId};
use strand_surface::{ButtonState, Config, InputEvent, SurfaceHost, SurfaceManager};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

#[derive(Default)]
struct Host {
    painted: bool,
    input: Vec<InputEvent>,
}

impl Painter for Host {
    fn paint(&mut self, _: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        target.pixels.fill(0xff);
        self.painted = true;
        Damage::full(target.size)
    }

    fn wants_frame(&self, _: SurfaceId) -> bool {
        !self.painted
    }
}

impl SurfaceHost for Host {
    fn input(&mut self, event: &InputEvent) {
        self.input.push(event.clone());
    }

    fn surface_detached(&mut self, _: SurfaceId) {
        self.painted = false;
    }
}

struct Client;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);
delegate_noop!(Client: ignore wl_seat::WlSeat);

const KEYMAP: &str = "xkb_keymap {\n\
    xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <AD03> = 26; <AD09> = 32;\n\
      <AB03> = 54; <AC11> = 48; <RALT> = 108; };\n\
    xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
    xkb_compatibility \"strand\" { };\n\
    xkb_symbols \"strand\" {\n\
      key <AD03> { [ e ] };\n\
      key <AD09> { [ o ] };\n\
      key <AB03> { [ c ] };\n\
      key <AC11> { [ dead_acute ] };\n\
      key <RALT> { [ Multi_key ] };\n\
    };\n\
    };\n";

const TABLE: &str = "<dead_acute> <e> : \"é\" eacute\n\
    <Multi_key> <o> <c> : \"©\" copyright\n";

// evdev's codes, the keymap's less 8.
const E: u32 = 18;
const O: u32 = 24;
const DEAD: u32 = 40;
const C: u32 = 46;
const MULTI: u32 = 100;

struct Keys {
    _conn: Connection,
    queue: EventQueue<Client>,
    keyboard: ZwpVirtualKeyboardV1,
    time: u32,
}

impl Keys {
    fn new(sway: &Sway) -> Keys {
        let conn = sway.connect();
        let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let path = sway.dir().join("compose.keymap");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(KEYMAP.as_bytes()).unwrap();
        file.write_all(&[0]).unwrap();
        file.flush().unwrap();
        let file = std::fs::File::open(&path).unwrap();
        keyboard.keymap(1, file.as_fd(), KEYMAP.len() as u32 + 1);
        queue.roundtrip(&mut Client).unwrap();
        Keys {
            _conn: conn,
            queue,
            keyboard,
            time: 0,
        }
    }

    fn send(&mut self, code: u32, state: u32) {
        self.time += 10;
        self.keyboard.key(self.time, code, state);
        self.queue.roundtrip(&mut Client).unwrap();
    }

    fn tap(&mut self, code: u32) {
        self.send(code, 1);
        self.send(code, 0);
    }
}

/// The name and text of each press the host has heard, repeats included.
fn presses(host: &Host) -> Vec<(String, String, bool)> {
    host.input
        .iter()
        .filter_map(|e| match e {
            InputEvent::Key { key, .. } if key.state == ButtonState::Pressed => {
                Some((key.name.clone(), key.text.clone(), key.repeat))
            }
            _ => None,
        })
        .collect()
}

/// Waits until the host has heard `n` presses, then takes them.
fn take(mgr: &mut SurfaceManager<Host>, n: usize) -> Vec<(String, String)> {
    let ok = mgr
        .dispatch_until(WAIT, |s| presses(s.host()).len() >= n)
        .unwrap();
    let heard = presses(mgr.state().host());
    assert!(ok, "{n} presses arrived: {heard:?}");
    mgr.state_mut().host_mut().input.clear();
    heard
        .into_iter()
        .map(|(name, text, _)| (name, text))
        .collect()
}

fn pair(name: &str, text: &str) -> (String, String) {
    (name.to_string(), text.to_string())
}

fn panel(mgr: &mut SurfaceManager<Host>, node: NodeId) -> SurfaceId {
    let mut spec = layer_spec(NodeKind::Panel, "Launcher", "center", 200.0, 100.0);
    spec.keyboard = strand_scene::Keyboard::Exclusive;
    mgr.state_mut()
        .apply_surface_change(node, SurfaceChange::Created(spec));
    let ok = mgr
        .dispatch_until(WAIT, |s| !s.surfaces_of(node).is_empty())
        .unwrap();
    assert!(ok, "the panel exists");
    mgr.state().surfaces_of(node)[0]
}

fn entered(mgr: &mut SurfaceManager<Host>, surface: SurfaceId) -> bool {
    mgr.dispatch_until(WAIT, |s| {
        s.host()
            .input
            .contains(&InputEvent::KeyboardEnter { surface })
    })
    .unwrap()
}

#[test]
fn dead_keys_and_compose_type_the_composed_text_under_real_names() {
    let scratch = std::env::temp_dir().join(format!("strand-compose-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let file = scratch.join("Compose");
    std::fs::write(&file, TABLE).unwrap();
    // SAFETY: the only test of this binary, before it starts anything.
    unsafe { std::env::set_var("XCOMPOSEFILE", &file) };
    let Some(sway) = Sway::start("dead_keys_and_compose") else {
        return;
    };
    let mut mgr =
        SurfaceManager::with_connection(sway.connect(), Host::default(), Config::default())
            .expect("surface manager starts");
    const FIRST: NodeId = NodeId::new(7, 0);
    const SECOND: NodeId = NodeId::new(8, 0);
    let first = panel(&mut mgr, FIRST);
    let mut keys = Keys::new(&sway);
    assert!(entered(&mut mgr, first), "the panel has the keyboard");
    let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
    mgr.state_mut().host_mut().input.clear();

    // A dead key: no text until the sequence ends, and the real names.
    keys.tap(DEAD);
    keys.tap(E);
    assert_eq!(
        take(&mut mgr, 2),
        [pair("dead_acute", ""), pair("e", "é")],
        "the dead acute and e"
    );
    keys.tap(E);
    assert_eq!(take(&mut mgr, 1), [pair("e", "e")], "e is itself again");

    // Compose: two keys in, the third types.
    keys.tap(MULTI);
    keys.tap(O);
    keys.tap(C);
    assert_eq!(
        take(&mut mgr, 3),
        [pair("Multi_key", ""), pair("o", ""), pair("c", "©")]
    );

    // A held result repeats what it typed, and a held dead key is one key
    // of a sequence.
    keys.tap(DEAD);
    keys.send(E, 1);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            presses(s.host()).iter().filter(|p| p.2).count() >= 2
        })
        .unwrap();
    assert!(ok, "the key repeats: {:?}", presses(mgr.state().host()));
    keys.send(E, 0);
    let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
    let held = presses(mgr.state().host());
    assert!(
        held.iter().skip(1).all(|(n, t, _)| n == "e" && t == "é"),
        "{held:?}"
    );
    mgr.state_mut().host_mut().input.clear();
    keys.send(DEAD, 1);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            presses(s.host()).iter().filter(|p| p.2).count() >= 2
        })
        .unwrap();
    assert!(
        ok,
        "the dead key repeats: {:?}",
        presses(mgr.state().host())
    );
    keys.send(DEAD, 0);
    let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
    assert!(
        presses(mgr.state().host())
            .iter()
            .all(|(_, t, _)| t.is_empty()),
        "{:?}",
        presses(mgr.state().host())
    );
    mgr.state_mut().host_mut().input.clear();
    keys.tap(E);
    assert_eq!(take(&mut mgr, 1), [pair("e", "é")], "still one sequence");

    // The focus going ends a sequence: `e` in the next panel is `e`.
    keys.tap(DEAD);
    assert_eq!(take(&mut mgr, 1), [pair("dead_acute", "")]);
    mgr.state_mut()
        .apply_surface_change(FIRST, SurfaceChange::Removed);
    let second = panel(&mut mgr, SECOND);
    assert!(entered(&mut mgr, second), "the next panel has the keyboard");
    let _ = mgr.dispatch_until(Duration::from_millis(100), |_| false);
    mgr.state_mut().host_mut().input.clear();
    keys.tap(E);
    assert_eq!(take(&mut mgr, 1), [pair("e", "e")], "the sequence ended");

    let _ = std::fs::remove_dir_all(&scratch);
}

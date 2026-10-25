//! A virtual keyboard (`zwp_virtual_keyboard_v1`) whose self-contained
//! keymap (no xkeyboard-config includes) has two named layouts, "Strand
//! One" and "Strand Two", for the keyboard layout's tests (gap 31).

use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

pub struct Client;
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

/// The keymap: Escape and `a` in two layouts, each named.
pub const KEYMAP: &str = "xkb_keymap {\n\
    xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <ESC> = 9; <AC01> = 38; };\n\
    xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
    xkb_compatibility \"strand\" { };\n\
    xkb_symbols \"strand\" {\n\
      name[Group1] = \"Strand One\";\n\
      name[Group2] = \"Strand Two\";\n\
      key <ESC> { symbols[Group1] = [ Escape ], symbols[Group2] = [ Escape ] };\n\
      key <AC01> { symbols[Group1] = [ a ], symbols[Group2] = [ a ] };\n\
    };\n\
    };\n";

pub struct Keyboard {
    _conn: Connection,
    queue: EventQueue<Client>,
    keyboard: ZwpVirtualKeyboardV1,
}

impl Keyboard {
    /// A keyboard on the compositor at `socket`, its keymap written in
    /// `dir`.
    pub fn new(socket: &Path, dir: &Path) -> Self {
        let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
        let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let path = dir.join("layouts.keymap");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(KEYMAP.as_bytes()).unwrap();
        file.write_all(&[0]).unwrap();
        file.flush().unwrap();
        let file = std::fs::File::open(&path).unwrap();
        keyboard.keymap(1, file.as_fd(), KEYMAP.len() as u32 + 1);
        queue.roundtrip(&mut Client).unwrap();
        Self {
            _conn: conn,
            queue,
            keyboard,
        }
    }

    /// Sends the modifier state: `locked` holds Lock (`1 << 1`, Caps
    /// Lock) and Mod2 (`1 << 4`, Num Lock), `group` the layout.
    pub fn modifiers(&mut self, locked: u32, group: u32) {
        self.keyboard.modifiers(0, 0, locked, group);
        self.queue.roundtrip(&mut Client).unwrap();
    }

    /// (services-round) Presses and releases `a`: input on the seat.
    pub fn tap(&mut self) {
        // evdev's KEY_A (keycode 38 less xkb's offset of 8).
        self.keyboard.key(0, 30, 1);
        self.keyboard.key(1, 30, 0);
        self.queue.roundtrip(&mut Client).unwrap();
    }
}

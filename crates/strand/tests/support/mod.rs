//! Helpers shared by the sway tests (`demo.rs`, `acceptance.rs`): a
//! virtual pointer and a virtual keyboard on the headless seat, and a
//! bounded poll for what arrives off the frame path.

#![allow(dead_code)]

/// A virtual pointer on the sway seat (`zwlr_virtual_pointer_v1`): the
/// headless seat has no pointer of its own.
pub mod pointer {
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
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
    delegate_noop!(Client: ignore ZwlrVirtualPointerManagerV1);
    delegate_noop!(Client: ignore ZwlrVirtualPointerV1);

    pub struct Pointer {
        _conn: Connection,
        queue: EventQueue<Client>,
        pointer: ZwlrVirtualPointerV1,
        time: u32,
    }

    impl Pointer {
        pub fn new(socket: &Path) -> Self {
            let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let qh = queue.handle();
            let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
            let pointer = manager.create_virtual_pointer(None, &qh, ());
            queue.roundtrip(&mut Client).unwrap();
            Self {
                _conn: conn,
                queue,
                pointer,
                time: 0,
            }
        }

        /// The pointer moved to layout position (`x`, `y`) of a layout
        /// `w`×`h` logical pixels (no button).
        pub fn motion(&mut self, x: u32, y: u32, w: u32, h: u32) {
            self.time += 10;
            self.pointer.motion_absolute(self.time, x, y, w, h);
            self.pointer.frame();
            self.queue.roundtrip(&mut Client).unwrap();
        }

        /// A left click at layout position (`x`, `y`) of a layout
        /// `w`×`h` logical pixels.
        pub fn click(&mut self, x: u32, y: u32, w: u32, h: u32) {
            self.click_button(0x110, x, y, w, h);
        }

        /// A right click (`on secondary`) at (`x`, `y`).
        pub fn right_click(&mut self, x: u32, y: u32, w: u32, h: u32) {
            self.click_button(0x111, x, y, w, h);
        }

        fn click_button(&mut self, button: u32, x: u32, y: u32, w: u32, h: u32) {
            self.time += 10;
            self.pointer.motion_absolute(self.time, x, y, w, h);
            self.pointer.frame();
            for state in [
                wl_pointer::ButtonState::Pressed,
                wl_pointer::ButtonState::Released,
            ] {
                self.time += 10;
                self.pointer.button(self.time, button, state);
                self.pointer.frame();
            }
            self.queue.roundtrip(&mut Client).unwrap();
        }

        /// Wheel `notches` detents (positive down) at (`x`, `y`): a
        /// wheel frame as libinput sends it, 15 px and one discrete step
        /// a notch.
        pub fn wheel(&mut self, notches: i32, x: u32, y: u32, w: u32, h: u32) {
            use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
            self.time += 10;
            self.pointer.motion_absolute(self.time, x, y, w, h);
            self.pointer.frame();
            for _ in 0..notches.unsigned_abs() {
                self.time += 10;
                let sign = f64::from(notches.signum());
                self.pointer.axis_source(AxisSource::Wheel);
                self.pointer.axis_discrete(
                    self.time,
                    Axis::VerticalScroll,
                    15.0 * sign,
                    sign as i32,
                );
                self.pointer.frame();
            }
            self.queue.roundtrip(&mut Client).unwrap();
        }
    }
}

/// A virtual keyboard (`zwp_virtual_keyboard_v1`) that types the
/// lowercase letters of a self-contained keymap (no xkeyboard-config
/// includes).
pub mod keyboard {
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

    /// Letter `c`'s evdev code (QWERTY).
    fn code(c: char) -> Option<u32> {
        let row = |s: &str, first: u32| s.find(c).map(|i| first + i as u32);
        row("qwertyuiop", 16)
            .or_else(|| row("asdfghjkl", 30))
            .or_else(|| row("zxcvbnm", 44))
    }

    /// The named keys the keymap has, with their evdev codes.
    const NAMED: [(&str, u32); 9] = [
        ("Escape", 1),
        ("BackSpace", 14),
        ("Return", 28),
        ("Home", 102),
        ("Up", 103),
        ("Left", 105),
        ("Right", 106),
        ("End", 107),
        ("Down", 108),
    ];

    pub struct Keyboard {
        _conn: Connection,
        queue: EventQueue<Client>,
        keyboard: ZwpVirtualKeyboardV1,
        time: u32,
    }

    impl Keyboard {
        pub fn new(socket: &Path, dir: &Path) -> Self {
            let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let qh = queue.handle();
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
            let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
            let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
            let mut codes = String::new();
            let mut symbols = String::new();
            for c in 'a'..='z' {
                let k = code(c).unwrap() + 8;
                codes.push_str(&format!("<K{k}> = {k}; "));
                symbols.push_str(&format!("key <K{k}> {{ [ {c} ] }}; "));
            }
            for (name, k) in NAMED {
                let k = k + 8;
                codes.push_str(&format!("<K{k}> = {k}; "));
                symbols.push_str(&format!("key <K{k}> {{ [ {name} ] }}; "));
            }
            let keymap = format!(
                "xkb_keymap {{\n\
                 xkb_keycodes \"strand\" {{ minimum = 8; maximum = 255; {codes}}};\n\
                 xkb_types \"strand\" {{ type \"ONE_LEVEL\" {{ modifiers = none; level_name[Level1] = \"Any\"; }}; }};\n\
                 xkb_compatibility \"strand\" {{ }};\n\
                 xkb_symbols \"strand\" {{ {symbols}}};\n\
                 }};\n"
            );
            let path = dir.join("keymap");
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(keymap.as_bytes()).unwrap();
            file.write_all(&[0]).unwrap();
            file.flush().unwrap();
            let file = std::fs::File::open(&path).unwrap();
            keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
            queue.roundtrip(&mut Client).unwrap();
            Self {
                _conn: conn,
                queue,
                keyboard,
                time: 0,
            }
        }

        /// Presses and releases a named key (`Return`, `Escape`, `Up`,
        /// `Down`, `Left`, `Right`, `Home`, `End`, `BackSpace`).
        pub fn press(&mut self, name: &str) {
            let k = NAMED
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, k)| *k)
                .expect("a named key of the keymap");
            for state in [1, 0] {
                self.time += 10;
                self.keyboard.key(self.time, k, state);
            }
            self.queue.roundtrip(&mut Client).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }

        /// Presses and releases a named key with Control held (the
        /// keymap's real `Control` modifier, sent as the modifier state).
        pub fn press_ctrl(&mut self, name: &str) {
            const CONTROL: u32 = 1 << 2;
            self.keyboard.modifiers(CONTROL, 0, 0, 0);
            self.queue.roundtrip(&mut Client).unwrap();
            self.press(name);
            self.keyboard.modifiers(0, 0, 0, 0);
            self.queue.roundtrip(&mut Client).unwrap();
        }

        /// Types `text` (lowercase letters), each key pressed and
        /// released.
        pub fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                let k = code(c).expect("a lowercase letter");
                for state in [1, 0] {
                    self.time += 10;
                    self.keyboard.key(self.time, k, state);
                }
                self.queue.roundtrip(&mut Client).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        }
    }
}

/// Content that arrives off the frame path (icons looked up and decoded
/// by a worker, text shaped by the text worker, a service's value) is
/// waited for by what the test can see, bounded, never by a fixed sleep
/// before one screenshot (CI runs 37895260781 and 37897160375 shot the
/// launcher and the bar before their icons drew).
pub mod poll {
    use std::time::{Duration, Instant};

    /// How long content arriving off the frame path is waited for.
    pub const ASYNC: Duration = Duration::from_secs(10);

    /// Calls `look` (a screenshot, or a reading of one) every 100 ms
    /// until `ok` holds for what it returned or [`ASYNC`] has passed, and
    /// returns the last look either way: the caller asserts on it, so a
    /// condition that never holds fails with the caller's own message.
    pub fn until<T>(mut look: impl FnMut() -> T, ok: impl Fn(&T) -> bool) -> T {
        let deadline = Instant::now() + ASYNC;
        loop {
            let seen = look();
            if ok(&seen) || Instant::now() >= deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

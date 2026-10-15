//! (M6) Copy and paste for a focused `input` through `wl_data_device`'s
//! selection on a headless sway (spec.md 6.7; owner-testing gap 30): the
//! host answers Ctrl+C and Ctrl+V with `ClipboardAsk`s (as the Router
//! does for an `input`), and another program (a wlr-data-control client)
//! reads what was copied and sets what is pasted.

mod common;

#[path = "../../strand-services/tests/common/data_control.rs"]
mod data_control;

use std::collections::HashSet;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::time::Duration;

use common::{Sway, WAIT, layer_spec};
use data_control::DataControl;
use strand_scene::{
    ClipboardAsk, Damage, NodeId, NodeKind, PaintTarget, Painter, SurfaceChange, SurfaceId,
};
use strand_surface::{ButtonState, Config, InputEvent, SurfaceHost, SurfaceManager};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

/// Paints white once; answers Ctrl+C with a copy of `copy` and Ctrl+V
/// with a paste, as the Router does for a focused `input`.
#[derive(Default)]
struct ClipHost {
    painted: HashSet<SurfaceId>,
    input: Vec<InputEvent>,
    copy: String,
    asks: Vec<ClipboardAsk>,
}

impl Painter for ClipHost {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        target.pixels.fill(0xff);
        self.painted.insert(surface);
        Damage::full(target.size)
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        !self.painted.contains(&surface)
    }
}

impl SurfaceHost for ClipHost {
    fn input(&mut self, event: &InputEvent) {
        self.input.push(event.clone());
        if let InputEvent::Key { surface, key } = event
            && key.state == ButtonState::Pressed
            && key.modifiers.ctrl
        {
            match key.name.as_str() {
                "c" => self.asks.push(ClipboardAsk::Copy {
                    surface: *surface,
                    text: self.copy.clone(),
                }),
                "v" => self.asks.push(ClipboardAsk::Paste { surface: *surface }),
                _ => {}
            }
        }
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.painted.remove(&surface);
    }

    fn take_clipboard_asks(&mut self) -> Vec<ClipboardAsk> {
        std::mem::take(&mut self.asks)
    }
}

type Mgr = SurfaceManager<ClipHost>;

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

/// A virtual keyboard with `c` and `v` (Control sent as the modifier
/// state).
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
        // KEY_C = 46, KEY_V = 47 (+ 8).
        let keymap = "xkb_keymap {\n\
            xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <AB03> = 54; <AB04> = 55; };\n\
            xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
            xkb_compatibility \"strand\" { };\n\
            xkb_symbols \"strand\" { key <AB03> { [ c ] }; key <AB04> { [ v ] }; };\n\
            };\n";
        let path = sway.dir().join("keymap");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(keymap.as_bytes()).unwrap();
        file.write_all(&[0]).unwrap();
        file.flush().unwrap();
        let file = std::fs::File::open(&path).unwrap();
        keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
        queue.roundtrip(&mut Client).unwrap();
        Keys {
            _conn: conn,
            queue,
            keyboard,
            time: 0,
        }
    }

    /// Ctrl and `c` (`"c"`) or `v` (`"v"`), pressed and released.
    fn ctrl(&mut self, key: &str) {
        let code = if key == "c" { 46 } else { 47 };
        self.keyboard.modifiers(1 << 2, 0, 0, 0);
        for state in [1, 0] {
            self.time += 10;
            self.keyboard.key(self.time, code, state);
        }
        self.keyboard.modifiers(0, 0, 0, 0);
        self.queue.roundtrip(&mut Client).unwrap();
    }
}

/// Sway, a manager showing a `keyboard: exclusive` panel that has the
/// keyboard, a virtual keyboard and the other program.
fn desk(test: &str) -> Option<(Sway, Mgr, SurfaceId, Keys, DataControl)> {
    let sway = Sway::start(test)?;
    let mut mgr =
        SurfaceManager::with_connection(sway.connect(), ClipHost::default(), Config::default())
            .expect("surface manager starts");
    const PANEL: NodeId = NodeId::new(7, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Launcher", "center", 200.0, 100.0);
    spec.keyboard = strand_scene::Keyboard::Exclusive;
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces().iter().any(|i| i.stats.commits > 0))
        .unwrap();
    assert!(ok, "the panel shows");
    let panel = mgr.state().surfaces_of(PANEL)[0];
    let keys = Keys::new(&sway);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .contains(&InputEvent::KeyboardEnter { surface: panel })
        })
        .unwrap();
    assert!(ok, "the panel has the keyboard");
    let other = DataControl::connect(sway.connect()).expect("sway 1.9 offers wlr-data-control");
    Some((sway, mgr, panel, keys, other))
}

fn pump(mgr: &mut Mgr, d: Duration) {
    let _ = mgr.dispatch_until(d, |_| false).unwrap();
}

/// Waits (dispatching) for what another program reads of the selection.
fn read_back(mgr: &mut Mgr, other: &DataControl, mime: &str) -> Option<Vec<u8>> {
    let rx = other.read(mime);
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        if let Ok(r) = rx.try_recv() {
            return r;
        }
        pump(mgr, Duration::from_millis(20));
    }
    None
}

fn pastes(host: &ClipHost) -> Vec<(SurfaceId, String)> {
    host.input
        .iter()
        .filter_map(|e| match e {
            InputEvent::Paste { surface, text } => Some((*surface, text.clone())),
            _ => None,
        })
        .collect()
}

/// Ctrl+C makes the host's text the selection: another program sees the
/// text types offered and reads the text.
#[test]
fn copy_sets_the_selection_other_programs_read() {
    let Some((_sway, mut mgr, _, mut keys, other)) =
        desk("copy_sets_the_selection_other_programs_read")
    else {
        return;
    };
    mgr.state_mut().host_mut().copy = "strand".into();
    keys.ctrl("c");
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            other.seen().mimes.iter().any(|m| m == "UTF8_STRING")
        })
        .unwrap();
    assert!(ok, "the copy became the selection: {:?}", other.seen());
    assert!(mgr.state().holds_selection());
    let mimes = other.seen().mimes;
    for t in strand_surface::SELECTION_TEXT_TYPES {
        assert!(mimes.iter().any(|m| m == t), "{t} offered: {mimes:?}");
    }
    assert_eq!(
        read_back(&mut mgr, &other, "text/plain;charset=utf-8").as_deref(),
        Some(&b"strand"[..])
    );
    assert_eq!(
        read_back(&mut mgr, &other, "STRING").as_deref(),
        Some(&b"strand"[..])
    );
    // The private type is never written.
    let private = mimes
        .iter()
        .find(|m| m.starts_with(strand_surface::STRAND_SELECTION_MIME))
        .cloned()
        .expect("the private type is offered");
    assert_eq!(
        read_back(&mut mgr, &other, &private).as_deref(),
        Some(&b""[..])
    );
    // Another program copying takes the selection from ours.
    other.copy_text("theirs");
    let ok = mgr.dispatch_until(WAIT, |s| !s.holds_selection()).unwrap();
    assert!(ok, "our copy was cancelled");
}

/// Ctrl+V reads another program's selection: it reaches the host as a
/// `Paste` on the focused surface.
#[test]
fn paste_reads_another_programs_selection_as_a_paste_event() {
    let Some((_sway, mut mgr, panel, mut keys, other)) =
        desk("paste_reads_another_programs_selection_as_a_paste_event")
    else {
        return;
    };
    other.copy_text("from elsewhere");
    assert!(other.wait(WAIT, |s| s.selections >= 2 || !s.mimes.is_empty()));
    pump(&mut mgr, Duration::from_millis(200));
    keys.ctrl("v");
    let ok = mgr
        .dispatch_until(WAIT, |s| !pastes(s.host()).is_empty())
        .unwrap();
    assert!(ok, "a paste arrived: {:?}", mgr.state().host().input);
    assert_eq!(
        pastes(mgr.state().host()),
        [(panel, "from elsewhere".to_string())]
    );
    assert!(
        other
            .seen()
            .served
            .iter()
            .any(|m| m == "text/plain;charset=utf-8"),
        "read through a pipe, as UTF-8 text: {:?}",
        other.seen().served
    );
}

/// A paste of our own copy comes from memory: the other program's offer
/// is never read (it never served a read).
#[test]
fn a_paste_of_our_own_copy_needs_no_pipe() {
    let Some((_sway, mut mgr, panel, mut keys, other)) =
        desk("a_paste_of_our_own_copy_needs_no_pipe")
    else {
        return;
    };
    mgr.state_mut().host_mut().copy = "ours".into();
    keys.ctrl("c");
    let ok = mgr
        .dispatch_until(WAIT, |_| !other.seen().mimes.is_empty())
        .unwrap();
    assert!(ok);
    keys.ctrl("v");
    let ok = mgr
        .dispatch_until(WAIT, |s| !pastes(s.host()).is_empty())
        .unwrap();
    assert!(ok, "a paste arrived");
    assert_eq!(pastes(mgr.state().host()), [(panel, "ours".to_string())]);
    // Handed back at once, while the press was handled: right after it.
    let input = &mgr.state().host().input;
    let at = input
        .iter()
        .position(|e| matches!(e, InputEvent::Paste { .. }))
        .unwrap();
    assert!(
        matches!(&input[at - 1], InputEvent::Key { key, .. }
            if key.name == "v" && key.state == ButtonState::Pressed),
        "the paste follows its key press with nothing read between: {input:?}"
    );
}

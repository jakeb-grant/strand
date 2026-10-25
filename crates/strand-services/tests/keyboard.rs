//! The `keyboard` service on the contract (gap 31): the lock keys from
//! the kernel's keyboard lights (a sysfs of the test's own), read only
//! while shown; a focused surface's report winning over them; the layout
//! from the compositor (a fake Hyprland) through the shared hub.

mod common;

use std::time::{Duration, Instant};

use common::hyprland::FakeHyprland;
use strand_core::Runtime;
use strand_services::keyboard::{self, Focused};
use strand_services::wm::{self, WmConfig};
use strand_services::{Builtin, Buses, Services};

/// The tests set process-wide state (the config, the focused report, the
/// read counter): one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    keyboard::focused(None);
    guard
}

fn services(rt: &Runtime) -> (Services, Builtin) {
    let s = Services::new(rt, Buses::none(), || {});
    let b = Builtin::register(&s, rt);
    (s, b)
}

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Pump for `d`.
fn pump_for(rt: &Runtime, s: &Services, d: Duration) {
    let t = Instant::now();
    while t.elapsed() < d {
        s.pump(rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn field(name: &str) -> usize {
    use strand_services::Store as _;
    keyboard::Keyboard::FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap()
}

/// A sysfs whose one keyboard has a Caps Lock and a Num Lock light.
struct Lights(tempfile::TempDir);

impl Lights {
    fn new() -> Lights {
        let dir = tempfile::tempdir().unwrap();
        for light in ["input5::capslock", "input5::numlock"] {
            let d = dir.path().join("class/leds").join(light);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("brightness"), "0\n").unwrap();
        }
        keyboard::configure(Some(keyboard::Config {
            sys: dir.path().to_path_buf(),
            led_period: Duration::from_millis(50),
        }));
        Lights(dir)
    }

    fn set(&self, light: &str, on: bool) {
        let path = self
            .0
            .path()
            .join("class/leds")
            .join(format!("input5::{light}"))
            .join("brightness");
        std::fs::write(path, if on { "1\n" } else { "0\n" }).unwrap();
    }
}

impl Drop for Lights {
    fn drop(&mut self) {
        keyboard::configure(None);
    }
}

#[test]
fn the_lock_keys_follow_the_keyboard_lights_while_read() {
    let _serial = serial();
    let lights = Lights::new();
    let rt = Runtime::new();
    let (s, b) = services(&rt);
    b.keyboard.acquire(&rt);
    b.keyboard.acquire_field(field("caps_lock"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let cells = b.keyboard.cells();
    assert_eq!(cells.caps_lock.get_untracked(&rt), Ok(false));
    lights.set("capslock", true);
    until(&rt, &s, "caps lock on", || {
        cells.caps_lock.get_untracked(&rt) == Ok(true)
    });
    lights.set("capslock", false);
    until(&rt, &s, "caps lock off", || {
        cells.caps_lock.get_untracked(&rt) == Ok(false)
    });
    // A steady light reports nothing to the logic thread.
    let reports = b.keyboard.reports();
    pump_for(&rt, &s, Duration::from_millis(300));
    assert_eq!(b.keyboard.reports(), reports, "nothing changed");

    // Nobody shows a lock key: the lights are not read, though the
    // service runs on.
    b.keyboard.release_field(field("caps_lock"));
    pump_for(&rt, &s, Duration::from_millis(100));
    let reads = keyboard::led_reads();
    lights.set("capslock", true);
    pump_for(&rt, &s, Duration::from_millis(400));
    assert_eq!(keyboard::led_reads(), reads, "no reads while unread");
    assert_eq!(cells.caps_lock.get_untracked(&rt), Ok(false));
    assert!(b.keyboard.running());

    // Shown again (Num Lock this time): read at once.
    b.keyboard.acquire_field(field("num_lock"));
    until(&rt, &s, "read again", || {
        cells.caps_lock.get_untracked(&rt) == Ok(true)
    });
    lights.set("numlock", true);
    until(&rt, &s, "num lock on", || {
        cells.num_lock.get_untracked(&rt) == Ok(true)
    });
    s.shutdown();
}

#[test]
fn a_focused_surfaces_keyboard_state_wins() {
    let _serial = serial();
    let lights = Lights::new();
    let rt = Runtime::new();
    let (s, b) = services(&rt);
    b.keyboard.acquire(&rt);
    b.keyboard.acquire_field(field("caps_lock"));
    b.keyboard.acquire_field(field("layout"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let cells = b.keyboard.cells();
    pump_for(&rt, &s, Duration::from_millis(100));

    // A surface has the keyboard: its report at once, the lights unread
    // (they say off).
    keyboard::focused(Some(Focused {
        caps_lock: true,
        num_lock: true,
        layout: Some("German".into()),
        ..Focused::default()
    }));
    until(&rt, &s, "the focused report", || {
        cells.caps_lock.get_untracked(&rt) == Ok(true)
            && cells.num_lock.get_untracked(&rt) == Ok(true)
            && cells.layout.get_untracked(&rt) == Ok(Some("German".into()))
    });
    pump_for(&rt, &s, Duration::from_millis(100));
    let reads = keyboard::led_reads();
    pump_for(&rt, &s, Duration::from_millis(400));
    assert_eq!(keyboard::led_reads(), reads, "no reads while focused");

    // Caps Lock pressed on it.
    keyboard::focused(Some(Focused {
        caps_lock: false,
        num_lock: true,
        layout: Some("German".into()),
        ..Focused::default()
    }));
    until(&rt, &s, "caps lock off", || {
        cells.caps_lock.get_untracked(&rt) == Ok(false)
    });

    // The focus leaves: the lights again (read at once), and no layout
    // (no compositor here).
    lights.set("capslock", true);
    keyboard::focused(None);
    until(&rt, &s, "the lights", || {
        cells.caps_lock.get_untracked(&rt) == Ok(true)
            && cells.num_lock.get_untracked(&rt) == Ok(false)
            && cells.layout.get_untracked(&rt) == Ok(None)
    });
    assert!(keyboard::led_reads() > reads);
    s.shutdown();
}

#[test]
fn the_layout_comes_from_the_compositor() {
    let _serial = serial();
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let fake = {
        let _enter = tokio.enter();
        FakeHyprland::start()
    };
    wm::configure(Some(WmConfig {
        backend: Some(fake.backend.clone()),
        wayland: None,
        ..Default::default()
    }));
    let rt = Runtime::new();
    let (s, b) = services(&rt);
    b.keyboard.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    // Nobody shows the layout: the compositor is not asked.
    pump_for(&rt, &s, Duration::from_millis(200));
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());

    let cells = b.keyboard.cells();
    b.keyboard.acquire_field(field("layout"));
    // The main keyboard's layout.
    until(&rt, &s, "the layout", || {
        cells.layout.get_untracked(&rt) == Ok(Some("German".into()))
    });
    // A keyboard switched.
    fake.send("activelayout>>at-translated-set-2-keyboard,English (US)\n");
    until(&rt, &s, "the switch", || {
        cells.layout.get_untracked(&rt) == Ok(Some("English (US)".into()))
    });
    // A focused surface whose keymap names its layout wins; one whose
    // keymap names none keeps the compositor's.
    keyboard::focused(Some(Focused {
        layout: Some("Strand One".into()),
        ..Focused::default()
    }));
    until(&rt, &s, "the keymap's name", || {
        cells.layout.get_untracked(&rt) == Ok(Some("Strand One".into()))
    });
    keyboard::focused(Some(Focused::default()));
    until(&rt, &s, "the compositor's again", || {
        cells.layout.get_untracked(&rt) == Ok(Some("English (US)".into()))
    });
    keyboard::focused(None);

    // (services-round) The layouts, named from the main keyboard's rule
    // names; and the switches, Hyprland's `switchxkblayout`.
    b.keyboard.acquire_field(field("layouts"));
    until(&rt, &s, "the layouts", || {
        cells.layouts.get_untracked(&rt)
            == Ok(vec!["English (US)".to_string(), "German".to_string()])
    });
    let act = |name: &str, args: &[strand_services::Data]| {
        b.keyboard
            .dynamic()
            .action(&rt, name, None, args, Default::default())
            .unwrap();
        rt.flush();
    };
    act("next_layout", &[]);
    act("set_layout", &[strand_services::Data::Int(1)]);
    until(&rt, &s, "the switches", || {
        let sent: Vec<String> = fake
            .requests()
            .into_iter()
            .filter(|r| r.starts_with("switchxkblayout"))
            .collect();
        sent == ["switchxkblayout all next", "switchxkblayout all 1"]
    });
    s.shutdown();
    wm::configure(None);
    drop(fake);
    drop(tokio);
}

/// (services-round) The modifiers held are a focused surface's only: no
/// compositor tells another client, so they are false once the focus
/// leaves (an alt-tab switcher commits when `alt` turns false).
#[test]
fn the_modifiers_are_a_focused_surfaces_only() {
    let _serial = serial();
    let _lights = Lights::new();
    let rt = Runtime::new();
    let (s, b) = services(&rt);
    b.keyboard.acquire(&rt);
    for f in ["alt", "ctrl", "shift", "logo"] {
        b.keyboard.acquire_field(field(f));
    }
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let cells = b.keyboard.cells();
    let held = |rt: &Runtime| {
        (
            cells.alt.get_untracked(rt),
            cells.ctrl.get_untracked(rt),
            cells.shift.get_untracked(rt),
            cells.logo.get_untracked(rt),
        )
    };
    assert_eq!(held(&rt), (Ok(false), Ok(false), Ok(false), Ok(false)));
    keyboard::focused(Some(Focused {
        alt: true,
        shift: true,
        ..Focused::default()
    }));
    until(&rt, &s, "alt and shift held", || {
        held(&rt) == (Ok(true), Ok(false), Ok(true), Ok(false))
    });
    keyboard::focused(Some(Focused {
        ctrl: true,
        logo: true,
        ..Focused::default()
    }));
    until(&rt, &s, "ctrl and logo held", || {
        held(&rt) == (Ok(false), Ok(true), Ok(false), Ok(true))
    });
    keyboard::focused(None);
    until(&rt, &s, "nothing held once the focus left", || {
        held(&rt) == (Ok(false), Ok(false), Ok(false), Ok(false))
    });
    s.shutdown();
}

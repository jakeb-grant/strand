//! `brightness` against a fake backlight directory and python-dbusmock's
//! `logind` template on a private bus: the level read and followed
//! (inotify) as the backlight changes, and writes going through logind's
//! `SetBrightness`, whose mock writes the fake backlight as the kernel
//! would.

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::Data;
use strand_services::brightness;
use strand_services::testing::{DbusMock, PrivateBus};
use support::*;

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER: &str = "/org/freedesktop/login1";

/// The roots are the process's: one test at a time sets them.
static SERIAL: Mutex<()> = Mutex::new(());

fn fake(root: &Path, max: u64, now: u64) -> std::path::PathBuf {
    let d = root.join("intel_backlight");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("type"), "raw\n").unwrap();
    std::fs::write(d.join("max_brightness"), format!("{max}\n")).unwrap();
    std::fs::write(d.join("brightness"), format!("{now}\n")).unwrap();
    std::fs::write(d.join("actual_brightness"), format!("{now}\n")).unwrap();
    d
}

#[test]
fn brightness_follows_the_backlight_and_writes_through_logind() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_logind) = DbusMock::start(&bus, "logind", true, None, LOGIND) else {
        return;
    };
    let root = std::env::temp_dir().join(format!("strand-backlight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = fake(&root, 1000, 500);
    // The caller's session, with a SetBrightness that writes the fake
    // backlight as the kernel would.
    let session: String = mock(
        &tokio,
        &conn,
        LOGIND,
        MANAGER,
        "AddSession",
        &("auto", "seat0", 1000u32, "user", true),
    )
    .body()
    .deserialize::<String>()
    .unwrap();
    assert_eq!(session, brightness::SESSION);
    let code = format!(
        "assert args[0] == 'backlight'\n\
         for f in ('brightness', 'actual_brightness'):\n    \
         open('{}/' + args[1] + '/' + f, 'w').write('%d\\n' % args[2])",
        root.display()
    );
    mock(
        &tokio,
        &conn,
        LOGIND,
        &session,
        "AddMethod",
        &(
            "org.freedesktop.login1.Session",
            "SetBrightness",
            "ssu",
            "",
            code.as_str(),
        ),
    );
    brightness::set_backlight_root(Some(root.clone()));
    // No keyboard light (never the machine's own).
    let leds = root.join("no-leds");
    std::fs::create_dir_all(&leds).unwrap();
    brightness::set_leds_root(Some(leds));

    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.brightness.cells();
    let changes = Arc::new(Mutex::new(Vec::new()));
    let c = changes.clone();
    let level = cells.level;
    let _watch = rt.on_change(
        move |rt| level.get(rt),
        move |_, v: &f64| {
            c.lock().unwrap().push(*v);
            Ok(())
        },
    );
    rt.flush();
    b.brightness.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    assert_eq!(cells.available.get_untracked(&rt), Ok(true));
    assert_eq!(cells.level.get_untracked(&rt), Ok(0.5));
    assert_eq!(
        cells.keyboard_present.get_untracked(&rt),
        Ok(false),
        "the two lights are independent"
    );

    // A brightness key: the kernel changes the level.
    std::fs::write(dir.join("actual_brightness"), "250\n").unwrap();
    until(&rt, &s, "the key's level", || {
        cells.level.get_untracked(&rt) == Ok(0.25)
    });
    rt.flush();
    assert_eq!(*changes.lock().unwrap(), [0.25], "an outside change fires");

    // The shell writes: logind sets it (the mock writes the backlight).
    b.brightness
        .dynamic()
        .write(&rt, 0, &[], Data::Float(0.8))
        .unwrap();
    rt.flush();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(&rt);
        rt.flush();
        let raw = std::fs::read_to_string(dir.join("brightness")).unwrap();
        if raw.trim() == "800" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "logind never set 800");
        std::thread::sleep(Duration::from_millis(10));
    }
    until(&rt, &s, "the written level", || {
        cells.level.get_untracked(&rt) == Ok(0.8)
    });
    let calls = mock_calls(&tokio, &conn, LOGIND, &session, "SetBrightness");
    assert_eq!(calls.len(), 1, "{calls:?}");
    // Its echo (the backlight's own notification) is no outside change.
    std::thread::sleep(Duration::from_millis(100));
    s.pump(&rt);
    rt.flush();
    assert_eq!(cells.level.get_untracked(&rt), Ok(0.8));
    s.shutdown();
    brightness::set_backlight_root(None);
    brightness::set_leds_root(None);
    let _ = std::fs::remove_dir_all(&root);
}

/// A light under `root`: `max_brightness`, `brightness` and, when `hw`,
/// a `brightness_hw_changed` (a driver reporting the firmware's changes).
fn led(root: &Path, name: &str, max: u64, now: u64, hw: bool) -> std::path::PathBuf {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("max_brightness"), format!("{max}\n")).unwrap();
    std::fs::write(d.join("brightness"), format!("{now}\n")).unwrap();
    if hw {
        std::fs::write(d.join("brightness_hw_changed"), format!("{now}\n")).unwrap();
    }
    d
}

/// logind's mock on `bus` with the caller's session, whose
/// `SetBrightness("leds", name, raw)` writes `<leds>/<name>/brightness`
/// as the kernel would: the session's path.
fn logind_writing_leds(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    leds: &Path,
) -> String {
    let session: String = mock(
        tokio,
        conn,
        LOGIND,
        MANAGER,
        "AddSession",
        &("auto", "seat0", 1000u32, "user", true),
    )
    .body()
    .deserialize::<String>()
    .unwrap();
    let code = format!(
        "assert args[0] == 'leds'\n\
         open('{}/' + args[1] + '/brightness', 'w').write('%d\\n' % args[2])",
        leds.display()
    );
    mock(
        tokio,
        conn,
        LOGIND,
        &session,
        "AddMethod",
        &(
            "org.freedesktop.login1.Session",
            "SetBrightness",
            "ssu",
            "",
            code.as_str(),
        ),
    );
    session
}

/// (power-media-kbd) The keyboard's backlight: found among the lights
/// (the lock keys' are not it), read in its steps, followed when the
/// firmware changes it, and written through logind's
/// `SetBrightness("leds", …)` to the nearest step. With no screen
/// backlight, the screen's is not `available`.
#[test]
fn the_keyboard_backlight_follows_its_light_and_writes_through_logind() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_logind) = DbusMock::start(&bus, "logind", true, None, LOGIND) else {
        return;
    };
    let root = std::env::temp_dir().join(format!("strand-kbd-light-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (backlights, leds) = (root.join("backlight"), root.join("leds"));
    std::fs::create_dir_all(&backlights).unwrap();
    let kbd = led(&leds, "dell::kbd_backlight", 2, 1, true);
    let caps = led(&leds, "input3::capslock", 1, 0, false);
    let session = logind_writing_leds(&tokio, &conn, &leds);
    brightness::set_backlight_root(Some(backlights));
    brightness::set_leds_root(Some(leds));

    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.brightness.cells();
    let changes = Arc::new(Mutex::new(Vec::new()));
    let c = changes.clone();
    let keyboard = cells.keyboard;
    let _watch = rt.on_change(
        move |rt| keyboard.get(rt),
        move |_, v: &f64| {
            c.lock().unwrap().push(*v);
            Ok(())
        },
    );
    rt.flush();
    b.brightness.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    assert_eq!(cells.available.get_untracked(&rt), Ok(false));
    assert_eq!(cells.keyboard_present.get_untracked(&rt), Ok(true));
    assert_eq!(cells.keyboard.get_untracked(&rt), Ok(0.5));

    // The firmware turns it up (an Fn key): the level changes with no
    // notification of `brightness` (sysfs gives none for it), and the
    // driver notifies `brightness_hw_changed`. The new level is renamed
    // over `brightness`, whose watched inode lives on under a spare name
    // (so no IN_IGNORED) and is never modified: only
    // `brightness_hw_changed` can say it changed.
    let spare = kbd.join("brightness.watched");
    std::fs::hard_link(kbd.join("brightness"), &spare).unwrap();
    let fresh = kbd.join("brightness.new");
    std::fs::write(&fresh, "2\n").unwrap();
    std::fs::rename(&fresh, kbd.join("brightness")).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    rt.flush();
    assert_eq!(
        cells.keyboard.get_untracked(&rt),
        Ok(0.5),
        "the level alone notifies nothing"
    );
    std::fs::write(kbd.join("brightness_hw_changed"), "2\n").unwrap();
    until(&rt, &s, "the firmware's level", || {
        cells.keyboard.get_untracked(&rt) == Ok(1.0)
    });
    assert_eq!(*changes.lock().unwrap(), [1.0], "an outside change fires");
    // The watched inode back as `brightness`, at the same level.
    std::fs::write(&spare, "2\n").unwrap();
    std::fs::rename(&spare, kbd.join("brightness")).unwrap();
    // The lock keys' light is not the keyboard's.
    std::fs::write(caps.join("brightness"), "1\n").unwrap();

    // The shell writes 0.3: the nearest step is 1 of 2.
    let field = b
        .brightness
        .dynamic()
        .fields()
        .iter()
        .position(|f| f.name == "keyboard")
        .unwrap();
    b.brightness
        .dynamic()
        .write(&rt, field, &[], Data::Float(0.3))
        .unwrap();
    rt.flush();
    until(&rt, &s, "the written step", || {
        std::fs::read_to_string(kbd.join("brightness"))
            .unwrap()
            .trim()
            == "1"
            && cells.keyboard.get_untracked(&rt) == Ok(0.5)
    });
    let calls = mock_calls(&tokio, &conn, LOGIND, &session, "SetBrightness");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let leds_arg: String = calls[0][0].try_clone().unwrap().try_into().unwrap();
    let name: String = calls[0][1].try_clone().unwrap().try_into().unwrap();
    let raw: u32 = calls[0][2].try_clone().unwrap().try_into().unwrap();
    assert_eq!(
        (leds_arg.as_str(), name.as_str(), raw),
        ("leds", "dell::kbd_backlight", 1)
    );
    // Full: raw 2.
    b.brightness
        .dynamic()
        .write(&rt, field, &[], Data::Float(1.0))
        .unwrap();
    rt.flush();
    until(&rt, &s, "full", || {
        std::fs::read_to_string(kbd.join("brightness"))
            .unwrap()
            .trim()
            == "2"
            && cells.keyboard.get_untracked(&rt) == Ok(1.0)
    });
    assert_eq!(
        mock_calls(&tokio, &conn, LOGIND, &session, "SetBrightness").len(),
        2
    );
    // The screen has no backlight: a write of it asks nothing.
    b.brightness
        .dynamic()
        .write(&rt, 0, &[], Data::Float(0.7))
        .unwrap();
    rt.flush();
    until(&rt, &s, "the screen's level kept", || {
        cells.level.get_untracked(&rt) == Ok(0.0)
    });
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        mock_calls(&tokio, &conn, LOGIND, &session, "SetBrightness").len(),
        2
    );
    s.shutdown();
    brightness::set_backlight_root(None);
    brightness::set_leds_root(None);
    let _ = std::fs::remove_dir_all(&root);
}

/// (power-media-kbd) Lights, but none a keyboard's: no keyboard light,
/// at 0, and a write of it asks logind nothing.
#[test]
fn without_a_keyboard_backlight_the_keyboard_is_not_present() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_logind) = DbusMock::start(&bus, "logind", true, None, LOGIND) else {
        return;
    };
    let root = std::env::temp_dir().join(format!("strand-no-kbd-light-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let leds = root.join("leds");
    led(&leds, "input3::capslock", 1, 1, false);
    led(&leds, "input3::numlock", 1, 0, false);
    let backlights = root.join("backlight");
    fake(&backlights, 100, 40);
    let session = logind_writing_leds(&tokio, &conn, &leds);
    brightness::set_backlight_root(Some(backlights));
    brightness::set_leds_root(Some(leds));
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.brightness.cells();
    b.brightness.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    assert_eq!(cells.available.get_untracked(&rt), Ok(true));
    assert_eq!(cells.level.get_untracked(&rt), Ok(0.4));
    assert_eq!(cells.keyboard_present.get_untracked(&rt), Ok(false));
    assert_eq!(cells.keyboard.get_untracked(&rt), Ok(0.0));
    let field = b
        .brightness
        .dynamic()
        .fields()
        .iter()
        .position(|f| f.name == "keyboard")
        .unwrap();
    b.brightness
        .dynamic()
        .write(&rt, field, &[], Data::Float(1.0))
        .unwrap();
    rt.flush();
    assert_eq!(cells.keyboard.get_untracked(&rt), Ok(1.0), "held at once");
    until(&rt, &s, "the write answered", || {
        cells.keyboard.get_untracked(&rt) == Ok(0.0)
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(mock_calls(&tokio, &conn, LOGIND, &session, "SetBrightness").is_empty());
    s.shutdown();
    brightness::set_backlight_root(None);
    brightness::set_leds_root(None);
    let _ = std::fs::remove_dir_all(&root);
}

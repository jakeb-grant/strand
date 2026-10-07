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
    let _ = std::fs::remove_dir_all(&root);
}

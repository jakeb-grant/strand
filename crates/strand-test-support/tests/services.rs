//! The real sway and dbus-daemon, started as the other tests start them.
//! Skipped without them unless CI's `STRAND_REQUIRE_*` says they must run.

use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::{Duration, Instant};

use strand_test_support::bus::SessionDaemon;
use strand_test_support::dir::ShortDir;
use strand_test_support::sway;

#[test]
fn a_headless_sway_starts_and_dies_with_its_owner() {
    if Command::new("sway").arg("--version").output().is_err() {
        assert!(
            std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
            "sway is not installed but STRAND_REQUIRE_SWAY is set"
        );
        eprintln!("skipping: sway is not installed");
        return;
    }
    let dir = ShortDir::new("strand-ts-sway");
    let cfg = dir.path().join("sway.cfg");
    std::fs::write(&cfg, "xwayland disable\n").unwrap();
    let log = File::create(dir.path().join("sway.log")).unwrap();
    // The owner is the thread that spawns it, which ends here with it running.
    let (mut child, display, sockets) = std::thread::scope(|s| {
        s.spawn(|| {
            let child = sway::command(&dir, &cfg, log).spawn().unwrap();
            let display = sway::wait_display(dir.path(), Duration::from_secs(10));
            (child, display, sway::sockets_in(dir.path()))
        })
        .join()
        .unwrap()
    });
    // Nothing else stops it: it dies of the death signal, or the wait runs out.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let log = std::fs::read_to_string(dir.path().join("sway.log")).unwrap();
    let display = display.unwrap_or_else(|| panic!("no socket; its log:\n{log}"));
    assert!(display.starts_with("wayland-"), "{display}");
    assert!(sockets.is_some_and(|(d, i)| d == display && i.starts_with("sway-ipc.")));
    let status = status.expect("sway outlived the thread that spawned it");
    assert_eq!(
        status.signal(),
        Some(9),
        "sway ended on its own: {status:?}"
    );
}

#[test]
fn a_session_daemon_prints_the_address_it_listens_on() {
    let dir = ShortDir::new("strand-ts-bus");
    let Some(daemon) = SessionDaemon::start(dir.path()) else {
        return;
    };
    assert!(
        daemon
            .address
            .starts_with(&format!("unix:path={}/bus", dir.path().display())),
        "{}",
        daemon.address
    );
    assert!(dir.path().join("bus").exists());
}

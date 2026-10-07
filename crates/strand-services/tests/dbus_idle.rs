//! The idle budget of the D-Bus services: with every one of them running
//! against its daemon (python-dbusmock's UPower, NetworkManager, BlueZ
//! and logind, a fake backlight, an MPRIS player, the tray's watcher and
//! the notification server) and nothing changing, the services thread
//! does not wake at all: they wait on D-Bus and inotify, nothing polls.
//! A change wakes it, and it sleeps again.
//!
//! A test binary of its own: it counts the context switches of the
//! thread named `strand-services`, which only this test's registry has.

mod support;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::testing::{DbusMock, PrivateBus};
use support::*;
use zbus::zvariant::Value;

/// Context switches of this process's threads named `name`.
fn switches(name: &str) -> u64 {
    let mut total = 0;
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    for task in tasks.flatten() {
        let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if comm.trim() != name {
            continue;
        }
        let status = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
        total += status
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum::<u64>();
    }
    total
}

struct Player;

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        "Paused".into()
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, zbus::zvariant::OwnedValue> {
        HashMap::new()
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        0
    }
}

#[test]
fn the_dbus_services_sleep_when_nothing_changes() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(_upower) = DbusMock::start(&bus, "upower", true, None, "org.freedesktop.UPower")
    else {
        return;
    };
    mock(
        &tokio,
        &conn,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "SetupDisplayDevice",
        &(
            2u32, 2u32, 50.0f64, 50.0f64, 100.0f64, 5.0f64, 3600i64, 0i64, true, "", 1u32,
        ),
    );
    let Some(_nm) = DbusMock::start(
        &bus,
        "networkmanager",
        true,
        None,
        "org.freedesktop.NetworkManager",
    ) else {
        return;
    };
    mock(
        &tokio,
        &conn,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "AddWiFiDevice",
        &("wlan", "wlan0", 100i32),
    );
    let Some(_bluez) = DbusMock::start(&bus, "bluez5", true, None, "org.bluez") else {
        return;
    };
    call(
        &tokio,
        &conn,
        "org.bluez",
        "/",
        "org.bluez.Mock",
        "AddAdapter",
        &("hci0", "laptop"),
    );
    let Some(_logind) = DbusMock::start(&bus, "logind", true, None, "org.freedesktop.login1")
    else {
        return;
    };
    let root = std::env::temp_dir().join(format!("strand-idle-backlight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let light = root.join("acpi_video0");
    std::fs::create_dir_all(&light).unwrap();
    std::fs::write(light.join("max_brightness"), "10\n").unwrap();
    std::fs::write(light.join("actual_brightness"), "5\n").unwrap();
    strand_services::brightness::set_backlight_root(Some(root.clone()));
    let _player = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.idle")
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", Player)
            .unwrap()
            .build()
            .await
            .unwrap()
    });

    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.battery.acquire(&rt);
    b.brightness.acquire(&rt);
    b.network.acquire(&rt);
    b.bluetooth.acquire(&rt);
    b.notifications.acquire(&rt);
    b.media.acquire(&rt);
    b.tray.acquire(&rt);
    assert!(
        s.wait_ready(&rt, Duration::from_secs(10)),
        "every first read"
    );
    assert!(b.battery.cells().present.get_untracked(&rt).unwrap());
    assert!(b.bluetooth.cells().powered.get_untracked(&rt).unwrap());
    assert!(b.brightness.cells().available.get_untracked(&rt).unwrap());
    // Settled: a whole second without a wakeup.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        s.pump(&rt);
        let w = switches("strand-services");
        std::thread::sleep(Duration::from_secs(1));
        if switches("strand-services") == w {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the services thread never settled"
        );
    }
    let before = switches("strand-services");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        switches("strand-services"),
        before,
        "the services thread woke while nothing changed"
    );
    // A change wakes it once, and it sleeps again.
    mock(
        &tokio,
        &conn,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "SetDeviceProperties",
        &(
            zbus::zvariant::ObjectPath::try_from("/org/freedesktop/UPower/devices/DisplayDevice")
                .unwrap(),
            HashMap::from([("Percentage", Value::from(40.0f64))]),
        ),
    );
    until(&rt, &s, "the change", || {
        b.battery.cells().percent.get_untracked(&rt) == Ok(0.4)
    });
    assert!(switches("strand-services") > before);
    std::thread::sleep(Duration::from_millis(300));
    let after = switches("strand-services");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(switches("strand-services"), after, "asleep again");
    s.shutdown();
    strand_services::brightness::set_backlight_root(None);
    let _ = std::fs::remove_dir_all(&root);
}

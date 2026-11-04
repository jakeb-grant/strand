//! `power` against python-dbusmock's power-profiles-daemon templates on a
//! private bus: `upower_power_profiles_daemon` (0.20 and later,
//! `org.freedesktop.UPower.PowerProfiles`) and `power_profiles_daemon`
//! (the older `net.hadess.PowerProfiles`). The profile read and followed,
//! written through `ActiveProfile` (the mock's log shows each `Set`), the
//! daemon restarting and switching names followed without the service
//! restarting, and the daemon never started for the shell.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::power::{LEGACY, LEGACY_PATH, PPD, PPD_PATH, PowerProfile};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Builtin, Services, ToData};
use support::*;
use zbus::zvariant::Value;

const NEW: &str = "upower_power_profiles_daemon";
const OLD: &str = "power_profiles_daemon";

/// The `Set` lines of `mock`'s log (python-dbusmock logs a property
/// written there, not among its method calls).
fn sets(mock: &DbusMock) -> Vec<String> {
    std::fs::read_to_string(mock.log())
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" Set "))
        .map(str::to_string)
        .collect()
}

/// Wait until `mock` logged a `Set` of `ActiveProfile` to `profile` on
/// `path`.
fn until_set(mock: &DbusMock, path: &str, iface: &str, profile: &str) {
    let want = format!("Set {path} {iface}.ActiveProfile \"{profile}\"");
    strand_test_support::wait::until(
        &want,
        Duration::from_secs(10),
        Duration::from_millis(10),
        || sets(mock).iter().any(|l| l.ends_with(&want)),
    );
}

/// `power.profile = p`.
fn write(rt: &Runtime, b: &Builtin, p: PowerProfile) {
    b.power.dynamic().write(rt, 0, &[], p.to_data()).unwrap();
    rt.flush();
}

fn profile(rt: &Runtime, b: &Builtin) -> PowerProfile {
    b.power.cells().profile.get_untracked(rt).unwrap()
}

fn start(rt: &Runtime, bus: &PrivateBus) -> (Services, Builtin) {
    let (s, b) = services(rt, bus.buses());
    b.power.acquire(rt);
    assert!(s.wait_ready(rt, Duration::from_secs(10)), "the boot read");
    rt.flush();
    (s, b)
}

#[test]
fn power_follows_power_profiles_daemon_and_its_restarts() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(ppd) = DbusMock::start(&bus, NEW, true, None, PPD) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.power.cells();
    let changes = Arc::new(Mutex::new(Vec::new()));
    let c = changes.clone();
    let field = cells.profile;
    let _watch = rt.on_change(
        move |rt| field.get(rt),
        move |_, v: &PowerProfile| {
            c.lock().unwrap().push(*v);
            Ok(())
        },
    );
    let degraded = Arc::new(Mutex::new(Vec::new()));
    let d = degraded.clone();
    let field = cells.degraded;
    let _degraded = rt.on_change(
        move |rt| field.get(rt),
        move |_, v: &Option<String>| {
            d.lock().unwrap().push(v.clone());
            Ok(())
        },
    );
    rt.flush();
    b.power.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the boot read");
    rt.flush();
    assert_eq!(cells.available.get_untracked(&rt), Ok(true));
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);
    assert_eq!(
        cells.profiles.get_untracked(&rt).unwrap(),
        [
            PowerProfile::PowerSaver,
            PowerProfile::Balanced,
            PowerProfile::Performance
        ]
    );
    assert_eq!(cells.degraded.get_untracked(&rt), Ok(None));
    assert!(
        changes.lock().unwrap().is_empty() && degraded.lock().unwrap().is_empty(),
        "boot values are not changes"
    );

    // The daemon switches on its own (a key, another tool): followed.
    mock(
        &tokio,
        &conn,
        PPD,
        PPD_PATH,
        "UpdateProperties",
        &(
            "",
            HashMap::from([("ActiveProfile", Value::from("power-saver"))]),
        ),
    );
    until(&rt, &s, "power-saver", || {
        profile(&rt, &b) == PowerProfile::PowerSaver
    });
    assert_eq!(*changes.lock().unwrap(), [PowerProfile::PowerSaver]);
    mock(
        &tokio,
        &conn,
        PPD,
        PPD_PATH,
        "UpdateProperties",
        &(
            "",
            HashMap::from([("PerformanceDegraded", Value::from("lap-detected"))]),
        ),
    );
    until(&rt, &s, "degraded", || {
        cells.degraded.get_untracked(&rt) == Ok(Some("lap-detected".into()))
    });
    assert_eq!(
        *degraded.lock().unwrap(),
        [Some("lap-detected".to_string())]
    );

    // The shell switches: the daemon's `ActiveProfile` is set.
    write(&rt, &b, PowerProfile::Performance);
    until_set(&ppd, PPD_PATH, PPD, "performance");
    until(&rt, &s, "performance", || {
        profile(&rt, &b) == PowerProfile::Performance
    });
    assert_eq!(sets(&ppd).len(), 1, "{:?}", sets(&ppd));
    // Its echo is no change of its own.
    std::thread::sleep(Duration::from_millis(100));
    s.pump(&rt);
    rt.flush();
    assert_eq!(profile(&rt, &b), PowerProfile::Performance);

    // The daemon goes away: unavailable, the service still running.
    drop(ppd);
    until(&rt, &s, "no daemon", || {
        cells.available.get_untracked(&rt) == Ok(false)
    });
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);
    assert!(cells.profiles.get_untracked(&rt).unwrap().is_empty());
    assert_eq!(cells.degraded.get_untracked(&rt), Ok(None));
    assert!(b.power.running());
    // A write while it is gone is not sent and changes nothing.
    write(&rt, &b, PowerProfile::PowerSaver);
    assert_eq!(profile(&rt, &b), PowerProfile::PowerSaver, "held at once");
    until(&rt, &s, "the write answered", || {
        profile(&rt, &b) == PowerProfile::Balanced
    });

    // It comes back (a restart): read afresh, no service restart.
    let Some(_ppd2) = DbusMock::start(
        &bus,
        NEW,
        true,
        Some(r#"{"ActiveProfile":"power-saver"}"#),
        PPD,
    ) else {
        return;
    };
    until(&rt, &s, "the new daemon", || {
        cells.available.get_untracked(&rt) == Ok(true)
            && profile(&rt, &b) == PowerProfile::PowerSaver
    });
    assert_eq!(cells.profiles.get_untracked(&rt).unwrap().len(), 3);
    assert_eq!(b.power.starts(), 1, "followed, not restarted");
    s.shutdown();
}

#[test]
fn power_falls_back_to_the_older_bus_name() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(old) = DbusMock::start(&bus, OLD, true, None, LEGACY) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    assert_eq!(b.power.cells().available.get_untracked(&rt), Ok(true));
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);
    // Only the older daemon runs: it is written.
    write(&rt, &b, PowerProfile::PowerSaver);
    until_set(&old, LEGACY_PATH, LEGACY, "power-saver");
    until(&rt, &s, "power-saver", || {
        profile(&rt, &b) == PowerProfile::PowerSaver
    });

    // The newer name appears: it is the one read and written.
    let Some(new) = DbusMock::start(
        &bus,
        NEW,
        true,
        Some(r#"{"ActiveProfile":"performance"}"#),
        PPD,
    ) else {
        return;
    };
    until(&rt, &s, "the newer daemon's profile", || {
        profile(&rt, &b) == PowerProfile::Performance
    });
    write(&rt, &b, PowerProfile::Balanced);
    until_set(&new, PPD_PATH, PPD, "balanced");
    until(&rt, &s, "balanced", || {
        profile(&rt, &b) == PowerProfile::Balanced
    });
    assert_eq!(
        sets(&old).len(),
        1,
        "the older one no more: {:?}",
        sets(&old)
    );

    // The newer one leaves: the older one serves again.
    drop(new);
    until(&rt, &s, "the older daemon's profile again", || {
        profile(&rt, &b) == PowerProfile::PowerSaver
    });
    assert_eq!(b.power.cells().available.get_untracked(&rt), Ok(true));
    write(&rt, &b, PowerProfile::Performance);
    until_set(&old, LEGACY_PATH, LEGACY, "performance");
    assert_eq!(b.power.starts(), 1, "followed, not restarted");
    s.shutdown();
}

#[test]
fn a_profile_the_daemon_does_not_offer_is_not_asked() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(ppd) = DbusMock::start(&bus, NEW, true, None, PPD) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    // No driver for `performance` here.
    let offered: Vec<HashMap<String, Value>> = ["power-saver", "balanced"]
        .into_iter()
        .map(|p| {
            HashMap::from([
                ("Profile".to_string(), Value::from(p)),
                ("Driver".to_string(), Value::from("dbusmock")),
            ])
        })
        .collect();
    mock(
        &tokio,
        &conn,
        PPD,
        PPD_PATH,
        "UpdateProperties",
        &("", HashMap::from([("Profiles", Value::from(offered))])),
    );
    until(&rt, &s, "two profiles", || {
        b.power.cells().profiles.get_untracked(&rt).unwrap()
            == [PowerProfile::PowerSaver, PowerProfile::Balanced]
    });
    write(&rt, &b, PowerProfile::Performance);
    // A write it offers comes after: once it is in, the first was
    // answered, and never sent.
    write(&rt, &b, PowerProfile::PowerSaver);
    until_set(&ppd, PPD_PATH, PPD, "power-saver");
    until(&rt, &s, "power-saver", || {
        profile(&rt, &b) == PowerProfile::PowerSaver
    });
    let lines = sets(&ppd);
    assert_eq!(lines.len(), 1, "performance was asked: {lines:?}");
    s.shutdown();
}

#[test]
fn power_never_starts_power_profiles_daemon() {
    let Some(bus) = PrivateBus::start_activating(&[(NEW, true, PPD)]) else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    // The bus could start it: a service file names it.
    let dbus = tokio.block_on(zbus::fdo::DBusProxy::new(&conn)).unwrap();
    let activatable = tokio.block_on(dbus.list_activatable_names()).unwrap();
    assert!(
        activatable.iter().any(|n| n.as_str() == PPD),
        "{activatable:?}"
    );
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    assert_eq!(b.power.cells().available.get_untracked(&rt), Ok(false));
    write(&rt, &b, PowerProfile::Performance);
    assert_eq!(profile(&rt, &b), PowerProfile::Performance, "held at once");
    until(&rt, &s, "the write answered", || {
        profile(&rt, &b) == PowerProfile::Balanced
    });
    // Long enough for an activated mock to own its name.
    assert!(
        !bus.wait_for_name(PPD, Duration::from_secs(3)),
        "the power service had power-profiles-daemon started"
    );
    assert_eq!(b.power.cells().available.get_untracked(&rt), Ok(false));
    assert!(b.power.running());
    s.shutdown();
}

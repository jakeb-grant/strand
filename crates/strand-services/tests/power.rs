//! `power` against python-dbusmock's power-profiles-daemon templates on a
//! private bus: `upower_power_profiles_daemon` (0.20 and later,
//! `org.freedesktop.UPower.PowerProfiles`) and `power_profiles_daemon`
//! (the older `net.hadess.PowerProfiles`). The profile read and followed,
//! written through `ActiveProfile` (the mock's log shows each `Set`), the
//! daemon restarting and switching names followed without the service
//! restarting, the daemon never started for the shell, and a daemon that
//! settles on another profile right after a write asked once more
//! ([`settles_elsewhere`] stands in for it).

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::StreamExt;
use strand_core::{Effect, Runtime};
use strand_services::power::{LEGACY, LEGACY_PATH, PPD, PPD_PATH, PowerProfile, SETTLE};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Builtin, Services, ToData};
use support::*;
use zbus::zvariant::{OwnedValue, Value};

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

/// The daemon's own `ActiveProfile`.
fn active(tokio: &tokio::runtime::Runtime, conn: &zbus::Connection) -> String {
    let reply = call(
        tokio,
        conn,
        PPD,
        PPD_PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(PPD, "ActiveProfile"),
    );
    let v: OwnedValue = reply.body().deserialize().unwrap();
    String::try_from(v).unwrap()
}

/// How [`settles_elsewhere`]'s daemon settles on `balanced`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Settle {
    /// Only for `power-saver` set while on `performance`, as on the
    /// owner's laptop.
    FromPerformance,
    /// For every `power-saver`.
    Always,
    /// As `FromPerformance`, and `power-saver` is no longer offered.
    Withdrawn,
    /// As `FromPerformance`, and the daemon refuses the next `Set`.
    Refusing,
}

/// python-dbusmock code for the mock's `Bounce`: `balanced` reported,
/// then `ActiveProfile` dropped, so the mock refuses a `Set` of it
/// ("no such property") as a daemon refuses a switch.
const BOUNCE: &str = "iface = 'org.freedesktop.UPower.PowerProfiles'
self.props[iface]['ActiveProfile'] = 'balanced'
self.EmitSignal('org.freedesktop.DBus.Properties', 'PropertiesChanged', 'sa{sv}as', [iface, {'ActiveProfile': dbus.String('balanced', variant_level=1)}, dbus.Array([], signature='s')])
del self.props[iface]['ActiveProfile']";

/// The `Profiles` property offering `names`.
fn offered(names: &[&str]) -> Value<'static> {
    let list: Vec<HashMap<String, Value<'static>>> = names
        .iter()
        .map(|n| {
            HashMap::from([
                ("Profile".to_string(), Value::from(n.to_string())),
                ("Driver".to_string(), Value::from("placeholder".to_string())),
            ])
        })
        .collect();
    Value::from(list)
}

/// Stands in for power-profiles-daemon on firmware with no low-power
/// profile (the owner's laptop, where `platform_profile_choices` lists
/// only balanced and performance): `power-saver` set while on
/// `performance` settles on `balanced` 80 ms later, the daemon reading its
/// own write to the firmware back as a switch, while from `balanced` it
/// takes (`how` varies it). The switch is the mock's `UpdateProperties`
/// (or `Bounce`), so the mock's log holds only the shell's `Set`s.
/// Watching from `from`, the profile it starts on.
fn settles_elsewhere(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    from: &str,
    how: Settle,
) {
    if how == Settle::Refusing {
        mock(
            tokio,
            conn,
            PPD,
            PPD_PATH,
            "AddMethod",
            &("", "Bounce", "", "", BOUNCE),
        );
    }
    let always = how == Settle::Always;
    let conn = conn.clone();
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .unwrap()
        .member("PropertiesChanged")
        .unwrap()
        .path(PPD_PATH)
        .unwrap()
        .build();
    let mut stream = tokio
        .block_on(zbus::MessageStream::for_match_rule(rule, &conn, None))
        .unwrap();
    let mut prev = from.to_string();
    tokio.spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            let Ok((_, changed, _)) = m
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            else {
                continue;
            };
            let Some(new) = changed
                .get("ActiveProfile")
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            else {
                continue;
            };
            if new == "power-saver" && (always || prev == "performance") {
                prev = "balanced".into();
                let conn = conn.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let mock = Some("org.freedesktop.DBus.Mock");
                    if how == Settle::Refusing {
                        let _ = conn
                            .call_method(Some(PPD), PPD_PATH, Some(PPD), "Bounce", &())
                            .await;
                        return;
                    }
                    let mut changed = HashMap::from([("ActiveProfile", Value::from("balanced"))]);
                    if how == Settle::Withdrawn {
                        changed.insert("Profiles", offered(&["balanced", "performance"]));
                    }
                    let _ = conn
                        .call_method(
                            Some(PPD),
                            PPD_PATH,
                            mock,
                            "UpdateProperties",
                            &("", changed),
                        )
                        .await;
                });
            } else {
                prev = new;
            }
        }
    });
}

/// Every value `power.profile` changes to, from now on.
fn changes(rt: &Runtime, b: &Builtin) -> (Arc<Mutex<Vec<PowerProfile>>>, Effect) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let c = seen.clone();
    let field = b.power.cells().profile;
    let watch = rt.on_change(
        move |rt| field.get(rt),
        move |_, v: &PowerProfile| {
            c.lock().unwrap().push(*v);
            Ok(())
        },
    );
    rt.flush();
    (seen, watch)
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

/// The owner's laptop: power-saver asked while on performance settles on
/// balanced, the daemon reading its own write to the firmware back. The
/// service asks once more, which takes. Without the second ask the
/// profile ends on balanced, one `Set` in the log. `seen` is what the
/// shell showed at its pumps (a few milliseconds apart, each applying
/// only the newest state), so it shows the passing balanced was not left
/// standing; that the service never publishes it at all is its code's
/// order (one update, after the second ask), which no pump can see.
#[test]
fn a_profile_the_daemon_settles_away_from_is_asked_once_more() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(ppd) = DbusMock::start(
        &bus,
        NEW,
        true,
        Some(r#"{"ActiveProfile":"performance"}"#),
        PPD,
    ) else {
        return;
    };
    settles_elsewhere(&tokio, &conn, "performance", Settle::FromPerformance);
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    assert_eq!(profile(&rt, &b), PowerProfile::Performance);
    let (seen, _watch) = changes(&rt, &b);

    write(&rt, &b, PowerProfile::PowerSaver);
    until(&rt, &s, "power-saver asked twice", || sets(&ppd).len() == 2);
    until(&rt, &s, "the daemon on power-saver", || {
        active(&tokio, &conn) == "power-saver" && profile(&rt, &b) == PowerProfile::PowerSaver
    });
    assert!(
        sets(&ppd).iter().all(|l| l.ends_with(&format!(
            "Set {PPD_PATH} {PPD}.ActiveProfile \"power-saver\""
        ))),
        "{:?}",
        sets(&ppd)
    );
    // Nothing more comes: the second ask took.
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert_eq!(profile(&rt, &b), PowerProfile::PowerSaver);
    assert_eq!(active(&tokio, &conn), "power-saver");
    assert_eq!(sets(&ppd).len(), 2, "{:?}", sets(&ppd));
    assert_eq!(*seen.lock().unwrap(), [PowerProfile::PowerSaver]);
    s.shutdown();
}

/// A daemon that settles elsewhere every time is asked once more only,
/// then followed; and a switch it makes on its own once the window is
/// over is followed, never asked away.
#[test]
fn a_daemon_that_settles_elsewhere_again_is_followed() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(ppd) = DbusMock::start(&bus, NEW, true, None, PPD) else {
        return;
    };
    settles_elsewhere(&tokio, &conn, "balanced", Settle::Always);
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);

    write(&rt, &b, PowerProfile::PowerSaver);
    until(&rt, &s, "power-saver asked twice", || sets(&ppd).len() == 2);
    until(&rt, &s, "balanced followed", || {
        active(&tokio, &conn) == "balanced" && profile(&rt, &b) == PowerProfile::Balanced
    });
    // Past the window: no third ask.
    std::thread::sleep(SETTLE + Duration::from_millis(500));
    s.pump(&rt);
    rt.flush();
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);
    assert_eq!(sets(&ppd).len(), 2, "asked a third time: {:?}", sets(&ppd));

    // A write that takes, then the daemon switching on its own (a key)
    // once the window is over: followed.
    write(&rt, &b, PowerProfile::Performance);
    until_set(&ppd, PPD_PATH, PPD, "performance");
    until(&rt, &s, "performance", || {
        profile(&rt, &b) == PowerProfile::Performance
    });
    std::thread::sleep(SETTLE + Duration::from_millis(200));
    mock(
        &tokio,
        &conn,
        PPD,
        PPD_PATH,
        "UpdateProperties",
        &(
            "",
            HashMap::from([("ActiveProfile", Value::from("balanced"))]),
        ),
    );
    until(&rt, &s, "the daemon's own switch", || {
        profile(&rt, &b) == PowerProfile::Balanced
    });
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);
    assert_eq!(active(&tokio, &conn), "balanced");
    assert_eq!(sets(&ppd).len(), 3, "{:?}", sets(&ppd));
    s.shutdown();
}

/// Another program taking a hold (gamemode, `powerprofilesctl launch`)
/// right after a write switches the profile in a change that names the
/// hold. A `Set` would cancel the hold, so it is followed, never asked
/// away; without the hold check a second `Set` of balanced goes out.
#[test]
fn a_hold_taken_right_after_a_write_is_followed() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(ppd) = DbusMock::start(
        &bus,
        NEW,
        true,
        Some(r#"{"ActiveProfile":"power-saver"}"#),
        PPD,
    ) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    assert_eq!(profile(&rt, &b), PowerProfile::PowerSaver);

    write(&rt, &b, PowerProfile::Balanced);
    until_set(&ppd, PPD_PATH, PPD, "balanced");
    // As the daemon's `HoldProfile` reports it: the profile and the holds
    // in one `PropertiesChanged`, well inside the window.
    let holds: Vec<HashMap<String, Value<'static>>> = vec![HashMap::from([
        (
            "Profile".to_string(),
            Value::from("performance".to_string()),
        ),
        ("Reason".to_string(), Value::from("game".to_string())),
        (
            "ApplicationId".to_string(),
            Value::from("gamemode".to_string()),
        ),
    ])];
    mock(
        &tokio,
        &conn,
        PPD,
        PPD_PATH,
        "UpdateProperties",
        &(
            "",
            HashMap::from([
                ("ActiveProfile", Value::from("performance")),
                ("ActiveProfileHolds", Value::from(holds)),
            ]),
        ),
    );
    until(&rt, &s, "the held profile", || {
        profile(&rt, &b) == PowerProfile::Performance
    });
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert_eq!(profile(&rt, &b), PowerProfile::Performance);
    assert_eq!(active(&tokio, &conn), "performance");
    assert_eq!(sets(&ppd).len(), 1, "the hold asked away: {:?}", sets(&ppd));
    s.shutdown();
}

/// The daemon settling away while it stops offering the written profile
/// is followed with no second ask; a second ask the daemon refuses is
/// logged and the daemon's profile shown, never asked a third time.
#[test]
fn a_profile_withdrawn_or_refused_is_not_asked_again() {
    for how in [Settle::Withdrawn, Settle::Refusing] {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let tokio = tokio();
        let conn = connect(&tokio, &bus.address);
        let Some(ppd) = DbusMock::start(
            &bus,
            NEW,
            true,
            Some(r#"{"ActiveProfile":"performance"}"#),
            PPD,
        ) else {
            return;
        };
        settles_elsewhere(&tokio, &conn, "performance", how);
        let rt = Runtime::new();
        let (s, b) = start(&rt, &bus);
        assert_eq!(profile(&rt, &b), PowerProfile::Performance);

        write(&rt, &b, PowerProfile::PowerSaver);
        until_set(&ppd, PPD_PATH, PPD, "power-saver");
        until(&rt, &s, &format!("{how:?}: balanced followed"), || {
            profile(&rt, &b) == PowerProfile::Balanced
        });
        std::thread::sleep(Duration::from_millis(300));
        s.pump(&rt);
        rt.flush();
        assert_eq!(profile(&rt, &b), PowerProfile::Balanced);
        let asked = sets(&ppd);
        match how {
            Settle::Withdrawn => {
                assert_eq!(asked.len(), 1, "asked for a withdrawn profile: {asked:?}");
                assert_eq!(
                    b.power.cells().profiles.get_untracked(&rt).unwrap(),
                    [PowerProfile::Balanced, PowerProfile::Performance]
                );
                assert_eq!(active(&tokio, &conn), "balanced");
            }
            _ => {
                // The mock logs a `Set` before refusing it.
                assert_eq!(asked.len(), 2, "{asked:?}");
                let log = std::fs::read_to_string(ppd.log()).unwrap_or_default();
                assert!(
                    log.lines().any(|l| l.contains("Bounce")),
                    "the daemon never bounced: {log}"
                );
            }
        }
        s.shutdown();
    }
}

/// Two writes in a row (a cycle key pressed twice): the echo of the first
/// is read before the second write, so it is never taken for the daemon
/// settling away from the second, and the second ask is left for the
/// daemon's real bounce. Without that order the echo spends it in about
/// a third of the rounds (the service picks the echo or the write at
/// random) and the profile ends on balanced; eight rounds make that near
/// certain.
#[test]
fn two_writes_in_a_row_still_ask_once_more() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let Some(ppd) = DbusMock::start(&bus, NEW, true, None, PPD) else {
        return;
    };
    settles_elsewhere(&tokio, &conn, "balanced", Settle::FromPerformance);
    let rt = Runtime::new();
    let (s, b) = start(&rt, &bus);
    assert_eq!(profile(&rt, &b), PowerProfile::Balanced);

    for round in 0..8 {
        let before = sets(&ppd).len();
        write(&rt, &b, PowerProfile::Performance);
        write(&rt, &b, PowerProfile::PowerSaver);
        until(
            &rt,
            &s,
            "performance, power-saver and power-saver again",
            || sets(&ppd).len() >= before + 3,
        );
        until(&rt, &s, "the daemon on power-saver", || {
            active(&tokio, &conn) == "power-saver" && profile(&rt, &b) == PowerProfile::PowerSaver
        });
        std::thread::sleep(Duration::from_millis(300));
        s.pump(&rt);
        rt.flush();
        assert_eq!(profile(&rt, &b), PowerProfile::PowerSaver, "round {round}");
        assert_eq!(active(&tokio, &conn), "power-saver", "round {round}");
        assert_eq!(
            sets(&ppd).len(),
            before + 3,
            "round {round}: {:?}",
            sets(&ppd)
        );

        write(&rt, &b, PowerProfile::Balanced);
        until_set(&ppd, PPD_PATH, PPD, "balanced");
        until(&rt, &s, "back on balanced", || {
            sets(&ppd).len() == before + 4 && profile(&rt, &b) == PowerProfile::Balanced
        });
    }
    s.shutdown();
}

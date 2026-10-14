//! `session` against python-dbusmock's `logind` template on a private
//! bus (owner-testing gaps 17): `can_*` from logind's answers, the power
//! actions, lock and log out on this process's own session, logind's
//! `Lock` signal as `lock_requested`, and a refused action as `failed`.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::session::{LOGIND, MANAGER};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Applied, Data, Store, session};
use support::*;

const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";

/// The events seen: name and arguments.
type Events = Vec<(String, Vec<String>)>;

#[test]
fn session_reads_logind_and_acts_through_it() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    // Hibernation is not supported here; suspending asks polkit first.
    let Some(_logind) = DbusMock::start(
        &bus,
        "logind",
        true,
        Some(r#"{"CanHibernate": "na", "CanSuspend": "challenge"}"#),
        LOGIND,
    ) else {
        return;
    };
    // This process's session, `c2`, which `GetSession("auto")` names (the
    // template would answer `…/session/auto`): lock and log out must go
    // by its id, not by "auto" (review minor 8).
    let session: String = mock(
        &tokio,
        &conn,
        LOGIND,
        MANAGER,
        "AddSession",
        &("c2", "seat0", 1000u32, "user", true),
    )
    .body()
    .deserialize()
    .unwrap();

    mock(
        &tokio,
        &conn,
        LOGIND,
        MANAGER,
        "AddMethod",
        &(
            MANAGER_IFACE,
            "GetSession",
            "s",
            "o",
            "ret = '/org/freedesktop/login1/session/c2' if args[0] == 'auto' else \
             '/org/freedesktop/login1/session/' + args[0]",
        ),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.session.cells();
    let seen: Arc<Mutex<Events>> = Arc::default();
    let log = seen.clone();
    b.session.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { event, args } = a {
            let name = <session::Session as Store>::EVENTS[*event].name;
            let args = args
                .iter()
                .map(|d| match d {
                    Data::Text(t) => t.to_string(),
                    d => format!("{d:?}"),
                })
                .collect();
            log.lock().unwrap().push((name.to_string(), args));
        }
    }));
    b.session.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.can_suspend.get_untracked(&rt), Ok(true), "challenge");
    assert_eq!(cells.can_hibernate.get_untracked(&rt), Ok(false), "na");
    assert_eq!(cells.can_reboot.get_untracked(&rt), Ok(true));
    assert_eq!(cells.can_power_off.get_untracked(&rt), Ok(true));
    assert_eq!(
        cells.available.get_untracked(&rt),
        Ok(true),
        "logind answers"
    );

    let act = |name: &str| {
        b.session
            .dynamic()
            .action(&rt, name, None, &[], Default::default())
            .unwrap();
        rt.flush();
    };
    let called = |method: &str| -> Vec<Vec<zbus::zvariant::OwnedValue>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            s.pump(&rt);
            let calls = mock_calls(&tokio, &conn, LOGIND, MANAGER, method);
            if !calls.is_empty() {
                return calls;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never called: {method}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // The power actions, interactive (polkit may ask).
    for (action, method) in [
        ("suspend", "Suspend"),
        ("hibernate", "Hibernate"),
        ("power_off", "PowerOff"),
    ] {
        act(action);
        let calls = called(method);
        assert_eq!(calls.len(), 1, "{method}");
        assert_eq!(
            calls[0][0],
            zbus::zvariant::OwnedValue::from(true),
            "{method}"
        );
    }
    // Lock and log out: on this process's own session, by its id.
    act("lock");
    assert_eq!(
        called("LockSession")[0][0],
        zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from("c2")).unwrap()
    );
    act("log_out");
    assert_eq!(
        called("TerminateSession")[0][0],
        zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from("c2")).unwrap()
    );

    // logind asks the session's locker to lock (`loginctl lock-session`):
    // `lock_requested`.
    call(
        &tokio,
        &conn,
        LOGIND,
        &session,
        "org.freedesktop.login1.Session",
        "Lock",
        &(),
    );
    until(&rt, &s, "lock_requested", || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|(n, _)| n == "lock_requested")
    });

    // polkit refuses a reboot: `failed` says so.
    mock(
        &tokio,
        &conn,
        LOGIND,
        MANAGER,
        "AddMethod",
        &(
            MANAGER_IFACE,
            "Reboot",
            "b",
            "",
            "raise dbus.exceptions.DBusException('Access denied', \
             name='org.freedesktop.DBus.Error.AccessDenied')",
        ),
    );
    act("reboot");
    until(&rt, &s, "the refusal", || {
        seen.lock().unwrap().iter().any(|e| {
            *e == (
                "failed".to_string(),
                vec!["reboot".to_string(), "Access denied".to_string()],
            )
        })
    });
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == "failed")
            .count(),
        1,
        "only the reboot failed: {:?}",
        seen.lock().unwrap()
    );
    s.shutdown();
}

/// Records `session`'s events as (name, texts).
fn record(b: &strand_services::Builtin) -> Arc<Mutex<Events>> {
    let seen: Arc<Mutex<Events>> = Arc::default();
    let log = seen.clone();
    b.session.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { event, args } = a {
            let name = <session::Session as Store>::EVENTS[*event].name;
            let args = args
                .iter()
                .map(|d| match d {
                    Data::Text(t) => t.to_string(),
                    d => format!("{d:?}"),
                })
                .collect();
            log.lock().unwrap().push((name.to_string(), args));
        }
    }));
    seen
}

/// (M6, owner-testing gap 20) Without logind `available` is false and
/// an action is `failed(action, error)`, never the caller's error: with
/// a bus that has no logind (no login session), with no system bus at
/// all (the body idles at its defaults), and with a bus that cannot be
/// reached (the body fails and backs off, so the action never reaches a
/// run: refused on the logic thread). Before, the last was an error the
/// handler faulted on, and the second was dropped without a word.
#[test]
fn without_logind_session_is_unavailable_and_its_actions_fail_as_events() {
    let check = |buses: strand_services::Buses, why: &str, fails: bool| {
        let rt = Runtime::new();
        let (s, b) = services(&rt, buses);
        let seen = record(&b);
        b.session.acquire(&rt);
        assert!(s.wait_ready(&rt, Duration::from_secs(10)), "{why}");
        rt.flush();
        if fails {
            // Its body failed: the action never reaches a run.
            until(&rt, &s, "the failed body", || !b.session.running());
        }
        assert_eq!(
            b.session.cells().available.get_untracked(&rt),
            Ok(false),
            "{why}"
        );
        b.session
            .dynamic()
            .action(&rt, "lock", None, &[], Default::default())
            .unwrap_or_else(|e| panic!("{why}: the action is no error: {e}"));
        until(&rt, &s, why, || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|(n, a)| n == "failed" && a.first().is_some_and(|x| x == "lock"))
        });
        let failed = seen.lock().unwrap().clone();
        s.shutdown();
        failed
    };
    if let Some(bus) = PrivateBus::start() {
        let failed = check(bus.buses(), "a bus without logind", false);
        assert!(
            failed
                .iter()
                .any(|(_, a)| a[1].contains("no login session")),
            "{failed:?}"
        );
    }
    let failed = check(strand_services::Buses::none(), "no system bus", false);
    assert!(
        failed.iter().any(|(_, a)| a[1] == "no system bus"),
        "{failed:?}"
    );
    let gone = strand_services::Bus::Address("unix:path=/nonexistent/strand-test-bus".into());
    let failed = check(
        strand_services::Buses {
            session: gone.clone(),
            system: gone,
        },
        "an unreachable bus",
        true,
    );
    assert!(
        failed.iter().any(|(_, a)| a[1].contains("not running")),
        "{failed:?}"
    );
}

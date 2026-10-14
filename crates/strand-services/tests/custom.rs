//! No-code services run through the registry as `strand run` runs them:
//! a `from listen` command printing faster than a frame wakes the logic
//! thread at most once a frame, its last line always shown; (M6) a `from
//! dbus` declaration's actions call its object's methods.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::custom::{
    self, ActionSpec, Custom, CustomAction, FieldSpec, LISTEN_FLUSH, Source, Spec,
};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Buses, Data, Services};
use support::{connect, mock_calls, tokio, until};

/// (M6) `action hold(profile: text, reason: text, app: text) =
/// HoldProfile` and `action release(cookie: int) = ReleaseProfile` on
/// dbusmock's power-profiles-daemon: each call reaches the method with
/// its arguments as the method's D-Bus types (three strings, one with
/// spaces kept whole; an `int` sent as the `u` it takes), and an action
/// naming no method of the object is refused there without stopping the
/// service.
#[test]
fn a_dbus_declarations_actions_call_its_methods() {
    const PPD: &str = "net.hadess.PowerProfiles";
    const PATH: &str = "/net/hadess/PowerProfiles";
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(_ppd) = DbusMock::start(&bus, "power_profiles_daemon", true, None, PPD) else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let rt = Runtime::new();
    let services = Services::new(&rt, bus.buses(), || {});
    let client = services.register_as::<Custom>(&rt, "ppd");
    let id = custom::register(Spec {
        name: "ppd".into(),
        source: Source::Dbus {
            system: true,
            name: PPD.into(),
            path: None,
        },
        fields: vec![FieldSpec {
            name: "profile".into(),
            key: vec!["ActiveProfile".into()],
            rw: false,
        }],
        actions: vec![
            ActionSpec {
                name: "hold".into(),
                method: "HoldProfile".into(),
            },
            ActionSpec {
                name: "release".into(),
                method: "ReleaseProfile".into(),
            },
            ActionSpec {
                name: "gone".into(),
                method: "NoSuchMethod".into(),
            },
        ],
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let profile = || {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    until(&rt, &services, "the profile is read", || {
        profile() == Some(Data::Text("balanced".into()))
    });
    let act = |index: usize, args: Vec<Data>| {
        client.act(&rt, CustomAction { index, args }).unwrap();
        services.pump(&rt);
    };
    act(2, Vec::new());
    act(
        0,
        vec![
            Data::Text("performance".into()),
            Data::Text("a reason with spaces".into()),
            Data::Text("strand".into()),
        ],
    );
    act(1, vec![Data::Int(7)]);
    let texts = |args: &[zbus::zvariant::OwnedValue]| -> Vec<String> {
        args.iter()
            .map(|v| String::try_from(v.try_clone().unwrap()).unwrap_or_default())
            .collect()
    };
    until(&rt, &services, "both methods were called", || {
        mock_calls(&tokio, &conn, PPD, PATH, "ReleaseProfile").len() == 1
    });
    let held = mock_calls(&tokio, &conn, PPD, PATH, "HoldProfile");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        texts(&held[0]),
        ["performance", "a reason with spaces", "strand"]
    );
    let released = mock_calls(&tokio, &conn, PPD, PATH, "ReleaseProfile");
    assert_eq!(
        u32::try_from(released[0][0].try_clone().unwrap()).unwrap(),
        7
    );
    // The refused action left the service running: it still reads.
    assert!(client.running(), "the service stopped");
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// `while :; do echo "{\"t\": $i}"; done`: 20 000 changing lines as fast
/// as `sh` prints them. Each envelope the service sends wakes the logic
/// thread once; merged, the wakes are bounded by the frames the run
/// took, not by the lines, and the value shown is the last line's.
#[test]
fn a_fast_listen_command_wakes_the_logic_thread_once_a_frame() {
    const LINES: u32 = 20_000;
    let rt = Runtime::new();
    let wakes = Arc::new(AtomicU32::new(0));
    let w = wakes.clone();
    let services = Services::new(&rt, Buses::none(), move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let client = services.register_as::<Custom>(&rt, "l");
    let script = format!(
        "i=0; while [ $i -lt {LINES} ]; do echo \"{{\\\"t\\\": $i}}\"; i=$((i+1)); done; \
         echo done >&2; sleep 30"
    );
    let id = custom::register(Spec {
        name: "l".into(),
        source: Source::Listen {
            command: vec!["sh".into(), "-c".into(), script],
        },
        fields: vec![FieldSpec {
            name: "t".into(),
            key: vec!["t".into()],
            rw: false,
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    let start = Instant::now();
    client.acquire(&rt);
    let last = Data::Int(i64::from(LINES - 1));
    let value = || {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let deadline = start + Duration::from_secs(60);
    loop {
        services.pump(&rt);
        rt.flush();
        if value().as_ref() == Some(&last) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never read the last line: {:?}",
            value()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let took = start.elapsed();
    let woken = wakes.load(Ordering::SeqCst);
    // One wake a frame (plus the ready envelope and slack for timer
    // lateness), never one a line.
    let frames = (took.as_secs_f64() / LISTEN_FLUSH.as_secs_f64()).ceil() as u32;
    assert!(
        woken <= frames + 8 && woken < LINES / 4,
        "{woken} wakes for {LINES} lines in {took:?} ({frames} frames)"
    );
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

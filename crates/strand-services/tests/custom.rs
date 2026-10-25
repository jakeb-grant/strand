//! No-code services run through the registry as `strand run` runs them:
//! a `from listen` command printing faster than a frame wakes the logic
//! thread at most once a frame, its last line always shown; (M6) a `from
//! dbus` declaration's actions call its object's methods; (services-round)
//! once a handler hears it, every line a `from listen` command prints is
//! one `line` event, none lost to the merge.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::custom::{
    self, ActionSpec, Custom, CustomAction, FieldSpec, LISTEN_FLUSH, Source, Spec,
};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Applied, Buses, Data, Services};
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

/// (services-round) `on svc.line(doc)`: a command printing 2 000 lines as
/// fast as `sh` can, with a field the even lines leave out. Once heard,
/// every line arrives as one `line` event, in order, its fields from that
/// line alone (null where it has none), while the fields themselves stay
/// merged and the last line's. A declaration nobody hears sends no event.
#[test]
fn a_heard_listen_command_sends_every_line() {
    const LINES: i64 = 2_000;
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let script = format!(
        "i=0; while [ $i -lt {LINES} ]; do \
         if [ $((i % 2)) -eq 1 ]; then echo \"{{\\\"t\\\": $i, \\\"odd\\\": true}}\"; \
         else echo \"{{\\\"t\\\": $i}}\"; fi; i=$((i+1)); done; sleep 30"
    );
    let spec = |name: &str| Spec {
        name: name.into(),
        source: Source::Listen {
            command: vec!["sh".into(), "-c".into(), script.clone()],
        },
        fields: vec![
            FieldSpec {
                name: "t".into(),
                key: vec!["t".into()],
                rw: false,
            },
            FieldSpec {
                name: "odd".into(),
                key: vec!["odd".into()],
                rw: false,
            },
        ],
        actions: Vec::new(),
    };
    let heard = services.register_as::<Custom>(&rt, "heard");
    let quiet = services.register_as::<Custom>(&rt, "quiet");
    let heard_id = custom::register(spec("heard"));
    let quiet_id = custom::register(spec("quiet"));
    custom::hear_lines(heard_id);
    heard
        .seed(&rt, |s| *s = Custom::seeded(heard_id, 2))
        .unwrap();
    quiet
        .seed(&rt, |s| *s = Custom::seeded(quiet_id, 2))
        .unwrap();
    let lines: Arc<Mutex<Vec<Vec<Data>>>> = Arc::default();
    let log = lines.clone();
    heard.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { args, .. } = a {
            log.lock().unwrap().push(args.clone());
        }
    }));
    let quiet_events = Arc::new(AtomicU32::new(0));
    let q = quiet_events.clone();
    quiet.dynamic().observe(Box::new(move |_, a| {
        if matches!(a, Applied::Event { .. }) {
            q.fetch_add(1, Ordering::SeqCst);
        }
    }));
    heard.acquire(&rt);
    quiet.acquire(&rt);
    let last = Data::Int(LINES - 1);
    let value = |c: &strand_services::Client<Custom>| {
        c.cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        services.pump(&rt);
        rt.flush();
        if lines.lock().unwrap().len() as i64 >= LINES
            && value(&heard).as_ref() == Some(&last)
            && value(&quiet).as_ref() == Some(&last)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {LINES} lines heard",
            lines.lock().unwrap().len()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len() as i64, LINES, "one event a line, none merged");
    for (i, args) in lines.iter().enumerate() {
        let i = i as i64;
        let odd = if i % 2 == 1 {
            Data::Bool(true)
        } else {
            Data::Null
        };
        assert_eq!(args, &vec![Data::List(vec![Data::Int(i), odd])], "line {i}");
    }
    // The merged fields keep the last value each field had.
    let odd = heard
        .cells()
        .values
        .with_untracked(&rt, |v| v.get(&1i64).map(|v| v.value.clone()))
        .ok()
        .flatten();
    assert_eq!(odd, Some(Data::Bool(true)));
    assert_eq!(quiet_events.load(Ordering::SeqCst), 0, "nobody heard it");
    heard.release(&rt);
    quiet.release(&rt);
    services.shutdown();
    custom::forget(heard_id);
    custom::forget(quiet_id);
}

//! No-code services run through the registry as `strand run` runs them:
//! a `from listen` command printing faster than a frame wakes the logic
//! thread at most once a frame, its last line always shown; (M6) a `from
//! dbus` declaration's actions call its object's methods, (dbus-action-run)
//! each with a run that says why its last call failed; (services-round)
//! once a handler hears it, every line a `from listen` command prints is
//! one `line` event, none lost to the merge; (services-round) a `from
//! dir` declaration lists a directory's files and follows them as they
//! come, go, change and move, and the directory itself as it is created
//! and removed; a `from file` rewritten in place (truncated, then
//! written) never shows the empty file in between.

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
use support::{connect, mock, mock_calls, tokio, until};

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
            text: Default::default(),
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

/// (dbus-action-run) A `from dbus` action's run, on dbusmock's
/// power-profiles-daemon with a method added that refuses (`stored`),
/// takes a second (`slow`) or answers at once: a refusal is the run's
/// `error`, the daemon's error name and message; a call in flight is
/// `running` from when it was made (`started`) until its answer; a new
/// call clears the last failure and an answer that is no error leaves it
/// clear; an action naming no method of the object fails at once.
#[test]
fn a_dbus_actions_run_says_why_its_call_failed() {
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
    mock(
        &tokio,
        &conn,
        PPD,
        PATH,
        "AddMethod",
        &(
            PPD,
            "Enroll",
            "s",
            "",
            "import time\n\
             if args[0] == 'stored':\n    \
             raise dbus.exceptions.DBusException('device already stored', \
             name='org.example.Error.Exists')\n\
             if args[0] == 'slow':\n    time.sleep(1)\n",
        ),
    );
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
            text: Default::default(),
        }],
        actions: vec![
            ActionSpec {
                name: "enroll".into(),
                method: "Enroll".into(),
            },
            ActionSpec {
                name: "gone".into(),
                method: "NoSuchMethod".into(),
            },
        ],
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    until(&rt, &services, "the profile is read", || {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
            == Some(Data::Text("balanced".into()))
    });
    let run = |index: i64| -> (i64, Option<i64>, Option<String>) {
        let r = client
            .cells()
            .runs
            .with_untracked(&rt, |v| v.get(&index).cloned())
            .ok()
            .flatten()
            .unwrap_or_default();
        (r.running, r.started, r.error)
    };
    let enroll = |what: &str| {
        client
            .act(
                &rt,
                CustomAction {
                    index: 0,
                    args: vec![Data::Text(what.into())],
                },
            )
            .unwrap();
        services.pump(&rt);
    };
    // Never called: not running, no failure.
    assert_eq!(run(0), (0, None, None));
    const STORED: &str = "org.example.Error.Exists: device already stored";
    enroll("stored");
    until(&rt, &services, "the refusal is the run's error", || {
        run(0) == (0, None, Some(STORED.into()))
    });
    // A call in flight runs from when it was made, and clears the last
    // failure at once, before its answer.
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    enroll("slow");
    until(&rt, &services, "the slow call runs", || {
        let (running, started, error) = run(0);
        running == 1 && started.is_some_and(|t| t >= before) && error.is_none()
    });
    until(&rt, &services, "the slow call is answered", || {
        run(0) == (0, None, None)
    });
    // The failure again, then a call that is answered well.
    enroll("stored");
    until(&rt, &services, "the refusal again", || {
        run(0) == (0, None, Some(STORED.into()))
    });
    enroll("fast");
    until(&rt, &services, "a good answer leaves no failure", || {
        run(0) == (0, None, None)
    });
    assert_eq!(mock_calls(&tokio, &conn, PPD, PATH, "Enroll").len(), 4);
    // A method the object does not have fails at once.
    client
        .act(
            &rt,
            CustomAction {
                index: 1,
                args: Vec::new(),
            },
        )
        .unwrap();
    until(
        &rt,
        &services,
        "a missing method is the run's error",
        || run(1).2.as_deref() == Some("`net.hadess.PowerProfiles` has no method `NoSuchMethod`"),
    );
    assert_eq!(run(1).0, 0);
    assert!(client.running(), "the service stopped");
    // (review of dbus-action-run) A call in flight when the service's
    // run ends (here a restart; the 5 s stop after its last visible
    // reader, or a failed run retried, end it the same way) is lost with
    // the run: the next run says so instead of running for good.
    enroll("slow");
    until(&rt, &services, "the slow call runs again", || run(0).0 == 1);
    client.restart(&rt);
    until(&rt, &services, "the lost call runs no more", || {
        run(0)
            == (
                0,
                None,
                Some(
                    "the service stopped before `net.hadess.PowerProfiles` answered; \
                     whether the call took is unknown"
                        .into(),
                ),
            )
    });
    // Its answer, had it come to the old run, changes nothing.
    std::thread::sleep(Duration::from_millis(1200));
    services.pump(&rt);
    assert_eq!(run(0).0, 0);
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// (review of dbus-action-run) An action a `from dbus` service cannot
/// take at all is its `refused` event, the action's index and why, which
/// the host reads as the action's `.error`: with the bus turned off (the
/// body idles and refuses it), and with a bus that does not answer (the
/// body fails and backs off, so the client refuses it, never the
/// caller's error).
#[test]
fn a_dbus_action_the_service_cannot_take_is_refused() {
    use std::cell::RefCell;
    use std::rc::Rc;
    use strand_core::ActionCtx;
    use strand_services::{Bus, Store};
    let spec = |name: &str| Spec {
        name: name.into(),
        source: Source::Dbus {
            system: true,
            name: "org.example.Gone".into(),
            path: None,
        },
        fields: Vec::new(),
        actions: vec![
            ActionSpec {
                name: "first".into(),
                method: "First".into(),
            },
            ActionSpec {
                name: "enroll".into(),
                method: "Enroll".into(),
            },
        ],
    };
    let gone = Bus::Address("unix:path=/nonexistent/strand-test-bus".into());
    for (buses, why) in [
        (
            Buses {
                session: Bus::Disabled,
                system: Bus::Disabled,
            },
            "no system bus",
        ),
        (
            Buses {
                session: gone.clone(),
                system: gone.clone(),
            },
            "restarts after a backoff",
        ),
    ] {
        let rt = Runtime::new();
        let services = Services::new(&rt, buses, || {});
        let client = services.register_as::<Custom>(&rt, "bolt");
        let id = custom::register(spec("bolt"));
        client.seed(&rt, |s| *s = Custom::seeded(id, 0)).unwrap();
        let heard: Rc<RefCell<Vec<Vec<Data>>>> = Rc::default();
        let h = heard.clone();
        let refused = Custom::EVENTS
            .iter()
            .position(|e| e.name == "refused")
            .unwrap();
        client.dynamic().observe(Box::new(move |_, applied| {
            if let Applied::Event { event, args } = applied
                && *event == refused
            {
                h.borrow_mut().push(args.clone());
            }
        }));
        client.acquire(&rt);
        if why == "restarts after a backoff" {
            until(&rt, &services, "the failed body", || !client.running());
        } else {
            until(&rt, &services, "the idle body", || client.ready(&rt));
        }
        let taken = client.act_or_refuse(
            &rt,
            "enroll",
            CustomAction {
                index: 1,
                args: vec![Data::Text("dev".into())],
            },
            ActionCtx::default(),
            false,
        );
        until(&rt, &services, "the refusal", || !heard.borrow().is_empty());
        let heard = heard.borrow().clone();
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert_eq!(heard[0][0], Data::Int(1), "{heard:?}");
        let Data::Text(text) = &heard[0][1] else {
            panic!("{heard:?}");
        };
        assert!(text.contains(why), "{heard:?}");
        // Only the backing-off service's is refused by the client; the
        // idle one took it, and refused it itself.
        assert_eq!(taken, why == "no system bus", "{heard:?}");
        client.release(&rt);
        services.shutdown();
        custom::forget(id);
    }
}

/// (dbus-unfollowed) The lid recipe of guide level 5 and
/// site/how-to/dbus-and-actions.md, `service upower from dbus system
/// "org.freedesktop.UPower" { lid_closed: bool = LidIsClosed; lid_present:
/// bool = LidIsPresent; on_battery: bool = OnBattery }`, on dbusmock's
/// `upower` template: read at start, then each change UPower signals
/// (`PropertiesChanged`, which the declaration follows; it never polls)
/// is followed: the lid closing, opening, and the charger going.
#[test]
fn the_lid_recipe_follows_upower() {
    const UPOWER: &str = "org.freedesktop.UPower";
    const ROOT: &str = "/org/freedesktop/UPower";
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(_upower) = DbusMock::start(&bus, "upower", true, None, UPOWER) else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let rt = Runtime::new();
    let services = Services::new(&rt, bus.buses(), || {});
    let client = services.register_as::<Custom>(&rt, "upower");
    let field = |name: &str, key: &str| FieldSpec {
        name: name.into(),
        key: vec![key.into()],
        rw: false,
        text: Default::default(),
    };
    let id = custom::register(Spec {
        name: "upower".into(),
        source: Source::Dbus {
            system: true,
            name: UPOWER.into(),
            path: None,
        },
        fields: vec![
            field("lid_closed", "LidIsClosed"),
            field("lid_present", "LidIsPresent"),
            field("on_battery", "OnBattery"),
        ],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 3)).unwrap();
    client.acquire(&rt);
    let read = |i: i64| {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&i).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let lid = || (read(0), read(1), read(2));
    let b = |v: bool| Some(Data::Bool(v));
    until(&rt, &services, "the lid is read", || {
        lid() == (b(false), b(true), b(false))
    });
    let set = |key: &str, v: bool| {
        let mut props = std::collections::HashMap::new();
        props.insert(key, zbus::zvariant::Value::from(v));
        mock(
            &tokio,
            &conn,
            UPOWER,
            ROOT,
            "UpdateProperties",
            &(UPOWER, props),
        );
    };
    set("LidIsClosed", true);
    until(&rt, &services, "the lid closing is followed", || {
        lid() == (b(true), b(true), b(false))
    });
    set("LidIsClosed", false);
    until(&rt, &services, "the lid opening is followed", || {
        lid() == (b(false), b(true), b(false))
    });
    set("OnBattery", true);
    until(&rt, &services, "the charger going is followed", || {
        lid() == (b(false), b(true), b(true))
    });
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
            text: Default::default(),
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
                text: Default::default(),
            },
            FieldSpec {
                name: "odd".into(),
                key: vec!["odd".into()],
                rw: false,
                text: Default::default(),
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

/// (services-round) `service walls from dir "…" { files: [File] }`: the
/// directory's regular files by name (hidden ones and subdirectories
/// left out, a link to a file listed), each with its path, name, size
/// and time; a file added, rewritten, renamed or removed is shown at
/// once; the directory removed lists nothing (null), and made again is
/// followed again. Nothing polls.
#[test]
fn a_dir_is_listed_and_followed() {
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "walls");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("walls");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("b.png"), b"bb").unwrap();
    std::fs::write(dir.join("a.jpg"), b"a").unwrap();
    std::fs::write(dir.join(".hidden"), b"x").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(tmp.path().join("outside.png"), b"ccc").unwrap();
    std::os::unix::fs::symlink(tmp.path().join("outside.png"), dir.join("c.png")).unwrap();
    let id = custom::register(Spec {
        name: "walls".into(),
        source: Source::Dir { path: dir.clone() },
        fields: vec![FieldSpec {
            name: "files".into(),
            key: vec!["files".into()],
            rw: false,
            text: Default::default(),
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let files = || -> Option<Vec<(String, String, i64)>> {
        let v = client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()?;
        let Data::List(items) = v else { return None };
        Some(
            items
                .iter()
                .map(|f| {
                    let text = |k: &str| match f.field(k) {
                        Some(Data::Text(t)) => t.to_string(),
                        other => panic!("{k}: {other:?}"),
                    };
                    let size = match f.field("size") {
                        Some(Data::Int(n)) => *n,
                        other => panic!("size: {other:?}"),
                    };
                    assert!(
                        matches!(f.field("modified"), Some(Data::Record { .. })),
                        "{f:?}"
                    );
                    (text("name"), text("path"), size)
                })
                .collect(),
        )
    };
    let names = || files().map(|f| f.into_iter().map(|(n, ..)| n).collect::<Vec<_>>());
    let wait = |what: &str, want: Option<&[&str]>| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            services.pump(&rt);
            rt.flush();
            let now = names();
            if now
                .as_deref()
                .map(|v| v.iter().map(String::as_str).collect::<Vec<_>>())
                == want.map(<[&str]>::to_vec)
            {
                return;
            }
            assert!(Instant::now() < deadline, "never {what}: {now:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    wait("listed", Some(&["a.jpg", "b.png", "c.png"]));
    let listed = files().unwrap();
    assert_eq!(
        listed[1],
        (
            "b.png".to_string(),
            dir.join("b.png").display().to_string(),
            2
        )
    );
    assert_eq!(listed[2].2, 3, "a link is listed with its file's size");

    std::fs::write(dir.join("d.webp"), b"dddd").unwrap();
    wait("a new file", Some(&["a.jpg", "b.png", "c.png", "d.webp"]));
    std::fs::write(dir.join("a.jpg"), b"aaaaa").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while files().is_none_or(|f| f[0].2 != 5) {
        services.pump(&rt);
        rt.flush();
        assert!(Instant::now() < deadline, "never the rewritten size");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::rename(dir.join("b.png"), dir.join("e.png")).unwrap();
    wait("a rename", Some(&["a.jpg", "c.png", "d.webp", "e.png"]));
    std::fs::rename(dir.join("e.png"), tmp.path().join("e.png")).unwrap();
    std::fs::remove_file(dir.join("d.webp")).unwrap();
    wait("a move out and a removal", Some(&["a.jpg", "c.png"]));

    // The directory goes: nothing to list; made again: followed again.
    std::fs::remove_dir_all(&dir).unwrap();
    wait("the directory gone", None);
    std::fs::create_dir(&dir).unwrap();
    wait("the directory made again", Some(&[]));
    std::fs::write(dir.join("z.png"), b"z").unwrap();
    wait("a file in the new directory", Some(&["z.png"]));

    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// A `from file` document rewritten in place, as `echo 5 > f` and a
/// plain `write` do (truncated, then written), is read once it settles:
/// the field goes from one value to the next, never through the empty
/// file `MODIFY` shows in between; a file left empty is read empty.
#[test]
fn a_file_rewritten_in_place_never_shows_it_empty() {
    use std::io::Write;
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "mood");
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mood.json");
    std::fs::write(&path, r#"{"level": 1}"#).unwrap();
    let id = custom::register(Spec {
        name: "mood".into(),
        source: Source::File { path: path.clone() },
        fields: vec![FieldSpec {
            name: "level".into(),
            key: vec!["level".into()],
            rw: false,
            text: Default::default(),
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let level = || {
        services.pump(&rt);
        rt.flush();
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let wait = |want: Data| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let now = level();
            // (Null is the seed, before the first read.)
            if let Some(v) = now.as_ref().filter(|v| **v != Data::Null) {
                let mut s = seen.lock().unwrap();
                if s.last() != Some(v) {
                    s.push(v.clone());
                }
            }
            if now.as_ref() == Some(&want) {
                return;
            }
            assert!(Instant::now() < deadline, "never {want:?}: {now:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait(Data::Int(1));
    for n in 2..=6 {
        let mut f = std::fs::File::create(&path).unwrap();
        // The truncated file stays empty a while before its bytes come,
        // as a slow writer's would.
        let until = Instant::now() + Duration::from_millis(15);
        while Instant::now() < until {
            let _ = level();
            std::thread::sleep(Duration::from_millis(1));
        }
        write!(f, r#"{{"level": {n}}}"#).unwrap();
        drop(f);
        wait(Data::Int(n));
    }
    assert_eq!(
        *seen.lock().unwrap(),
        (1..=6).map(Data::Int).collect::<Vec<_>>(),
        "one value after the other, never the empty file"
    );
    std::fs::write(&path, "").unwrap();
    wait(Data::text(""));

    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// (services-round) A directory past `MAX_DIR_FILES` lists the first
/// files by name (whatever order the directory keeps them in), a
/// subdirectory among them not counting as one.
#[test]
fn a_huge_dir_lists_its_first_files_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let n = custom::MAX_DIR_FILES;
    std::fs::create_dir(tmp.path().join("0")).unwrap();
    for i in (0..n + 50).rev() {
        std::fs::write(tmp.path().join(format!("{i:06}.png")), b"").unwrap();
    }
    let listed = custom::list_dir(tmp.path()).unwrap().unwrap();
    assert_eq!(listed.len(), n);
    let name = |d: &Data| match d.field("name") {
        Some(Data::Text(t)) => t.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(name(&listed[0]), "000000.png");
    assert_eq!(name(&listed[n - 1]), format!("{:06}.png", n - 1));
}

/// (services-round) A directory that keeps changing (a download, a
/// cache) is listed at most every `DIR_RELIST`, not after each change.
#[test]
fn a_busy_dir_is_listed_at_most_every_relist() {
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "busy");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let id = custom::register(Spec {
        name: "busy".into(),
        source: Source::Dir { path: dir.clone() },
        fields: vec![FieldSpec {
            name: "files".into(),
            key: vec!["files".into()],
            rw: false,
            text: Default::default(),
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    assert!(services.wait_ready(&rt, Duration::from_secs(10)));
    let before = client.reports();
    // A file grows every 20 ms for 2 s: a hundred changes.
    let busy = Duration::from_secs(2);
    let started = Instant::now();
    let mut i = 0u32;
    while started.elapsed() < busy {
        std::fs::write(dir.join("part"), vec![0u8; i as usize]).unwrap();
        i += 1;
        services.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    services.pump(&rt);
    let lists = client.reports() - before;
    let most = (busy.as_millis() / custom::DIR_RELIST.as_millis()) as u64 + 2;
    assert!(
        (1..=most).contains(&lists),
        "{lists} listings for {i} changes in {busy:?}"
    );
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// (service-ready) A `from dbus` declaration is ready once its first
/// `GetAll` is in, not while its daemon is gone (its values null), and
/// ready again once a new daemon has been read.
#[test]
fn a_dbus_declaration_is_ready_while_its_daemon_answers() {
    const PPD: &str = "net.hadess.PowerProfiles";
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(ppd) = DbusMock::start(&bus, "power_profiles_daemon", true, None, PPD) else {
        return;
    };
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
            text: Default::default(),
        }],
        actions: Vec::new(),
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
    assert!(!client.ready(&rt), "not before its first read");
    until(&rt, &services, "ready", || client.ready(&rt));
    assert_eq!(profile(), Some(Data::Text("balanced".into())));
    drop(ppd);
    until(&rt, &services, "not ready without the daemon", || {
        !client.ready(&rt)
    });
    until(&rt, &services, "the values cleared", || {
        profile() == Some(Data::Null)
    });
    assert!(!client.ready(&rt), "still not ready: nobody answers");
    assert!(client.running(), "the run waits for the daemon");
    let Some(_ppd) = DbusMock::start(&bus, "power_profiles_daemon", true, None, PPD) else {
        return;
    };
    until(&rt, &services, "ready again", || client.ready(&rt));
    assert_eq!(profile(), Some(Data::Text("balanced".into())));
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// (service-ready) A `from file` declaration is ready once its first
/// read is in, a `from poll` one once its first poll is; a run that is
/// stopped is not.
#[test]
fn file_and_poll_declarations_are_ready_after_their_first_read() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mood.json");
    std::fs::write(&path, r#"{"level": 1}"#).unwrap();
    for (name, source) in [
        ("mood", Source::File { path: path.clone() }),
        (
            "polled",
            Source::Poll {
                target: custom::PollTarget::Command(vec![
                    "sh".into(),
                    "-c".into(),
                    r#"sleep 0.2; echo '{"level": 1}'"#.into(),
                ]),
                every: Duration::from_secs(60),
            },
        ),
    ] {
        let rt = Runtime::new();
        let services = Services::new(&rt, Buses::none(), || {});
        let client = services.register_as::<Custom>(&rt, name);
        let id = custom::register(Spec {
            name: name.into(),
            source,
            fields: vec![FieldSpec {
                name: "level".into(),
                key: vec!["level".into()],
                rw: false,
                text: Default::default(),
            }],
            actions: Vec::new(),
        });
        client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
        client.acquire(&rt);
        assert!(!client.ready(&rt), "{name}: not before its first read");
        until(&rt, &services, "ready", || client.ready(&rt));
        let level = client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten();
        assert_eq!(level, Some(Data::Int(1)), "{name}: ready with its value");
        client.stop_now(&rt);
        assert!(!client.ready(&rt), "{name}: stopped, not ready");
        services.shutdown();
        custom::forget(id);
    }
}

/// (build-ready) A `from poll` command that fails is a warning naming
/// the service (the log, `strand watch`), once while it fails the same
/// way, and a service whose polls have never answered is not `ready`
/// (though the first frame does not wait for it); the first poll that
/// answers makes it ready with its values.
#[test]
fn a_failing_poll_is_reported_under_its_name_and_is_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let ok = tmp.path().join("ok");
    let runs = tmp.path().join("runs");
    let script = format!(
        "echo x >> '{}'; [ -e '{}' ] && echo '{{\"level\": 2}}' || exit 22",
        runs.display(),
        ok.display()
    );
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "flaky");
    let id = custom::register(Spec {
        name: "flaky".into(),
        source: Source::Poll {
            target: custom::PollTarget::Command(vec!["sh".into(), "-c".into(), script]),
            every: Duration::from_millis(100),
        },
        fields: vec![FieldSpec {
            name: "level".into(),
            key: vec!["level".into()],
            rw: false,
            text: Default::default(),
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let mut seen = Vec::new();
    until(&rt, &services, "the failure reported", || {
        seen.extend(services.take_diagnostics());
        !seen.is_empty()
    });
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].service, "flaky");
    assert!(!seen[0].notice);
    assert_eq!(seen[0].message, "`sh` failed (exit status: 22)");
    assert_eq!(
        seen[0].to_string(),
        "service `flaky`: `sh` failed (exit status: 22)"
    );
    // The first frame's hold is over, and it is not ready.
    assert!(services.wait_ready_of(&rt, Some("flaky"), Duration::from_secs(10)));
    assert!(!client.ready(&rt), "ready with no poll answered");
    // More failing polls of the same kind: no more reports.
    let count = || std::fs::read_to_string(&runs).map_or(0, |r| r.lines().count());
    let after = count();
    until(&rt, &services, "two more polls", || count() >= after + 2);
    services.pump(&rt);
    rt.flush();
    assert!(services.take_diagnostics().is_empty(), "reported again");
    assert!(!client.ready(&rt));
    std::fs::write(&ok, "").unwrap();
    until(&rt, &services, "ready once a poll answers", || {
        client.ready(&rt)
    });
    let level = client
        .cells()
        .values
        .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
        .ok()
        .flatten();
    assert_eq!(level, Some(Data::Int(2)));
    // The failure is resolved, so a late `strand watch` is not told it;
    // failing again is news again.
    let mut seen = Vec::new();
    until(&rt, &services, "the failure resolved", || {
        seen.extend(services.take_diagnostics());
        !seen.is_empty()
    });
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].resolved, "{seen:?}");
    assert_eq!(seen[0].message, "`sh` failed (exit status: 22)");
    std::fs::remove_file(&ok).unwrap();
    let mut seen = Vec::new();
    until(&rt, &services, "the failure reported again", || {
        seen.extend(services.take_diagnostics());
        !seen.is_empty()
    });
    assert!(!seen[0].resolved, "{seen:?}");
    assert!(client.ready(&rt), "it answered once: its values hold");
    services.shutdown();
    custom::forget(id);
}

/// (build-ready review) A poll service whose first poll failed is not
/// made ready by its reader going hidden meanwhile (a popup closed while
/// the poll ran): it is ready only once a poll answers.
#[test]
fn a_failed_first_poll_hidden_meanwhile_is_still_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let go = tmp.path().join("go");
    let ok = tmp.path().join("ok");
    let runs = tmp.path().join("runs");
    // Each poll is counted, waits for `go`, then fails until `ok` exists.
    let script = format!(
        "echo x >> '{runs}'; while [ ! -e '{go}' ]; do sleep 0.02; done; \
         [ -e '{ok}' ] && echo '{{\"level\": 2}}' || exit 22",
        runs = runs.display(),
        go = go.display(),
        ok = ok.display()
    );
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "hidden");
    let id = custom::register(Spec {
        name: "hidden".into(),
        source: Source::Poll {
            target: custom::PollTarget::Command(vec!["sh".into(), "-c".into(), script]),
            every: Duration::from_millis(100),
        },
        fields: vec![FieldSpec {
            name: "level".into(),
            key: vec!["level".into()],
            rw: false,
            text: Default::default(),
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let count = || std::fs::read_to_string(&runs).map_or(0, |r| r.lines().count());
    until(&rt, &services, "the first poll running", || count() >= 1);
    // Its reader goes hidden while the poll runs; then the poll fails.
    client.release(&rt);
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(&go, "").unwrap();
    until(&rt, &services, "the failure reported", || {
        !services.take_diagnostics().is_empty()
    });
    // Shown again: the next poll (also failing) is past the loop's top,
    // where a hidden reader made it ready.
    client.acquire(&rt);
    until(&rt, &services, "a second poll", || count() >= 2);
    let after = count();
    until(&rt, &services, "a third poll", || count() > after);
    services.pump(&rt);
    rt.flush();
    assert!(!client.ready(&rt), "ready with no poll answered");
    std::fs::write(&ok, "").unwrap();
    until(&rt, &services, "ready once a poll answers", || {
        client.ready(&rt)
    });
    services.shutdown();
    custom::forget(id);
}

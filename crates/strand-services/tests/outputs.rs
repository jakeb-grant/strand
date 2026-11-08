//! `outputs` against a real headless sway (sway 1.9 implements
//! `zwlr_output_manager_v1`, version 4; owner-testing gap 18): every
//! head listed, a test-only configuration changing nothing, applied
//! configurations changing mode, place, scale, rotation and turning a
//! head off and on again, and refused configurations arriving as
//! `outputs.failed`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::Sway;
use strand_core::Runtime;
use strand_services::outputs::{self, Output, OutputChange, OutputMode, OutputTransform};
use strand_services::wm::WaylandTarget;
use strand_services::{Applied, Builtin, Buses, Cells, Data, Services, Store, ToData};

/// The tests set the process-wide display `outputs` connects to: one at
/// a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The events seen: name and texts.
type Events = Arc<Mutex<Vec<(String, Vec<String>)>>>;

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, mut cond: impl FnMut() -> bool) {
    strand_test_support::wait::until(
        what,
        Duration::from_secs(10),
        Duration::from_millis(5),
        || {
            s.pump(rt);
            rt.flush();
            cond()
        },
    );
}

struct Harness {
    rt: Runtime,
    s: Services,
    b: Builtin,
    events: Events,
}

impl Harness {
    /// `outputs` on `sway`, read and ready.
    fn start(sway: &Sway) -> Harness {
        Harness::start_on(sway.socket())
    }

    /// `outputs` on the compositor at `socket`, read and ready.
    fn start_on(socket: std::path::PathBuf) -> Harness {
        outputs::configure(Some(WaylandTarget::Socket(socket)));
        let rt = Runtime::new();
        let s = Services::new(&rt, Buses::none(), || {});
        let b = Builtin::register(&s, &rt);
        let events: Events = Arc::default();
        let log = events.clone();
        b.outputs.dynamic().observe(Box::new(move |_, a| {
            if let Applied::Event { event, args } = a {
                let name = <outputs::Outputs as Store>::EVENTS[*event].name;
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
        b.outputs.acquire(&rt);
        assert!(
            s.wait_ready(&rt, Duration::from_secs(10)),
            "the first state"
        );
        rt.flush();
        Harness { rt, s, b, events }
    }

    fn all(&self) -> Vec<Output> {
        self.b
            .outputs
            .cells()
            .snapshot(&self.rt)
            .map(|s| s.all)
            .unwrap_or_default()
    }

    fn output(&self, name: &str) -> Output {
        self.all()
            .into_iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("no {name} in {:#?}", self.all()))
    }

    /// Runs `action` with `changes` as the language does: by name, with
    /// the records as `Data`.
    fn act(&self, action: &str, changes: &[OutputChange]) {
        let list = Data::List(changes.iter().map(ToData::to_data).collect());
        self.b
            .outputs
            .dynamic()
            .action(&self.rt, action, None, &[list], Default::default())
            .unwrap();
        self.rt.flush();
    }

    /// Waits for the next event and returns it (taking it from the log).
    fn next_event(&self, what: &str) -> (String, Vec<String>) {
        until(&self.rt, &self.s, what, || {
            !self.events.lock().unwrap().is_empty()
        });
        self.events.lock().unwrap().remove(0)
    }

    fn until(&self, what: &str, cond: impl Fn(&Harness) -> bool) {
        until(&self.rt, &self.s, what, || cond(self));
    }
}

/// sway's own view of output `name` (`get_outputs`).
fn sway_output(sway: &Sway, name: &str) -> serde_json::Value {
    let all: serde_json::Value = serde_json::from_str(&sway.msg(&["-t", "get_outputs"])).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("sway has no {name}: {all:#}"))
}

/// Waits (5 s at most) until sway's view of `name` satisfies `cond`.
fn sway_until(sway: &Sway, name: &str, what: &str, cond: impl Fn(&serde_json::Value) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let o = sway_output(sway, name);
        if cond(&o) {
            return;
        }
        assert!(Instant::now() < deadline, "sway never: {what}: {o:#}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn mode(width: i64, height: i64, refresh: f64) -> OutputMode {
    OutputMode {
        width,
        height,
        refresh,
    }
}

#[test]
fn outputs_lists_configures_and_tests_sways_heads() {
    let _serial = serial();
    let Some(sway) = Sway::start("outputs_lists_configures_and_tests_sways_heads") else {
        return;
    };
    sway.msg(&["create_output"]);
    sway.msg(&[
        "output",
        "HEADLESS-2",
        "resolution",
        "800x600",
        "position",
        "1280",
        "0",
    ]);
    sway_until(&sway, "HEADLESS-2", "800x600", |o| {
        o["rect"]["width"] == 800
    });
    let h = Harness::start(&sway);
    assert_eq!(
        h.b.outputs.cells().available.get_untracked(&h.rt),
        Ok(true),
        "sway offers zwlr_output_manager_v1"
    );
    h.until("both heads", |h| {
        h.all()
            .iter()
            .any(|o| o.name == "HEADLESS-2" && o.mode.is_some_and(|m| m.width == 800))
    });
    eprintln!("outputs: {:#?}", h.all());

    // Listing.
    let one = h.output("HEADLESS-1");
    assert!(one.enabled);
    assert_eq!(one.mode.map(|m| (m.width, m.height)), Some((1280, 720)));
    assert!(one.modes.contains(&one.mode.unwrap()), "{one:#?}");
    assert_eq!((one.x, one.y, one.scale), (0, 0, 1.0));
    assert_eq!(one.transform, OutputTransform::Normal);
    assert!(!one.description.is_empty());
    let two = h.output("HEADLESS-2");
    assert_eq!((two.x, two.y), (1280, 0));

    // Test only: succeeds and changes nothing.
    h.act(
        "test",
        &[OutputChange {
            scale: Some(2.0),
            x: Some(2000),
            ..OutputChange::of("HEADLESS-2")
        }],
    );
    assert_eq!(
        h.next_event("the test's answer"),
        ("succeeded".into(), vec!["test".into()])
    );
    std::thread::sleep(Duration::from_millis(100));
    h.s.pump(&h.rt);
    assert_eq!(h.output("HEADLESS-2").scale, 1.0);
    assert_eq!(h.output("HEADLESS-2").x, 1280);
    assert_eq!(sway_output(&sway, "HEADLESS-2")["scale"], 1.0);

    // Apply: a mode (a custom one on headless, which lists none of its own).
    h.act(
        "apply",
        &[OutputChange {
            mode: Some(mode(1024, 768, 60.0)),
            ..OutputChange::of("HEADLESS-2")
        }],
    );
    assert_eq!(
        h.next_event("the mode"),
        ("succeeded".into(), vec!["apply".into()])
    );
    h.until("the new mode", |h| {
        h.output("HEADLESS-2")
            .mode
            .is_some_and(|m| (m.width, m.height) == (1024, 768))
    });
    sway_until(&sway, "HEADLESS-2", "1024x768", |o| {
        o["current_mode"]["width"] == 1024 && o["current_mode"]["height"] == 768
    });
    // The other head is left as it was.
    assert_eq!(h.output("HEADLESS-1"), one);

    // Place and scale, on both heads together.
    h.act(
        "apply",
        &[
            OutputChange {
                x: Some(0),
                y: Some(0),
                ..OutputChange::of("HEADLESS-2")
            },
            OutputChange {
                x: Some(512),
                y: Some(0),
                scale: Some(2.0),
                ..OutputChange::of("HEADLESS-1")
            },
        ],
    );
    assert_eq!(
        h.next_event("place and scale"),
        ("succeeded".into(), vec!["apply".into()])
    );
    h.until("placed and scaled", |h| {
        let (one, two) = (h.output("HEADLESS-1"), h.output("HEADLESS-2"));
        (one.x, one.y, one.scale, two.x, two.y) == (512, 0, 2.0, 0, 0)
    });
    sway_until(&sway, "HEADLESS-1", "placed and scaled", |o| {
        o["scale"] == 2.0 && o["rect"]["x"] == 512
    });

    // Rotation.
    h.act(
        "apply",
        &[OutputChange {
            transform: Some(OutputTransform::Rotate90),
            ..OutputChange::of("HEADLESS-2")
        }],
    );
    assert_eq!(
        h.next_event("rotation"),
        ("succeeded".into(), vec!["apply".into()])
    );
    h.until("rotated", |h| {
        h.output("HEADLESS-2").transform == OutputTransform::Rotate90
    });
    // `rotate_90` is `wl_output`'s 90, counter-clockwise; sway's IPC and
    // config count clockwise, so sway calls it 270.
    sway_until(&sway, "HEADLESS-2", "rotated", |o| o["transform"] == "270");

    // Off, still listed; then on again.
    h.act(
        "apply",
        &[OutputChange {
            enabled: Some(false),
            ..OutputChange::of("HEADLESS-2")
        }],
    );
    assert_eq!(
        h.next_event("off"),
        ("succeeded".into(), vec!["apply".into()])
    );
    h.until("off", |h| {
        let two = h.output("HEADLESS-2");
        !two.enabled && two.mode.is_none()
    });
    sway_until(&sway, "HEADLESS-2", "off", |o| o["active"] == false);
    h.act(
        "apply",
        &[OutputChange {
            enabled: Some(true),
            ..OutputChange::of("HEADLESS-2")
        }],
    );
    assert_eq!(
        h.next_event("on"),
        ("succeeded".into(), vec!["apply".into()])
    );
    h.until("on", |h| h.output("HEADLESS-2").enabled);
    sway_until(&sway, "HEADLESS-2", "on", |o| o["active"] == true);
    assert!(h.events.lock().unwrap().is_empty(), "{:?}", h.events);
    h.s.shutdown();
    outputs::configure(None);
}

#[test]
fn refused_configurations_arrive_as_failed() {
    let _serial = serial();
    let Some(sway) = Sway::start("refused_configurations_arrive_as_failed") else {
        return;
    };
    let h = Harness::start(&sway);
    let before = h.all();
    let failed = |h: &Harness, what: &str, action: &str| -> String {
        let (name, args) = h.next_event(what);
        assert_eq!(name, "failed", "{what}: {args:?}");
        assert_eq!(args[0], action, "{what}");
        eprintln!("{what}: {}", args[1]);
        args[1].clone()
    };
    // Refused before anything is sent (the protocol would end the
    // connection over them).
    h.act("apply", &[OutputChange::of("DP-9")]);
    assert!(failed(&h, "an unknown output", "apply").contains("no output named `DP-9`"));
    h.act(
        "test",
        &[OutputChange {
            scale: Some(0.0),
            ..OutputChange::of("HEADLESS-1")
        }],
    );
    assert!(failed(&h, "a zero scale", "test").contains("outside 1/256"));
    h.act(
        "apply",
        &[OutputChange {
            mode: Some(mode(0, 720, 60.0)),
            ..OutputChange::of("HEADLESS-1")
        }],
    );
    assert!(failed(&h, "a zero width", "apply").contains("not a size"));
    h.act(
        "apply",
        &[
            OutputChange::of("HEADLESS-1"),
            OutputChange::of("HEADLESS-1"),
        ],
    );
    assert!(failed(&h, "a name twice", "apply").contains("twice"));

    // Refused by sway: a headless output has no adaptive sync. sway's
    // test only checks the state, so it may pass while the commit fails: a
    // tested configuration may still fail to apply.
    let vrr = [OutputChange {
        adaptive_sync: Some(true),
        ..OutputChange::of("HEADLESS-1")
    }];
    h.act("test", &vrr);
    let (name, args) = h.next_event("adaptive sync, tested");
    eprintln!("adaptive sync, tested: {name} {args:?}");
    assert_eq!(args[0], "test");
    h.act("apply", &vrr);
    assert_eq!(
        failed(&h, "adaptive sync", "apply"),
        "the compositor refused the configuration"
    );
    // Nothing changed, and the connection is still good.
    std::thread::sleep(Duration::from_millis(100));
    h.s.pump(&h.rt);
    assert_eq!(h.all(), before);
    h.act("test", &[OutputChange::of("HEADLESS-1")]);
    assert_eq!(
        h.next_event("a good test"),
        ("succeeded".into(), vec!["test".into()])
    );
    h.s.shutdown();
    outputs::configure(None);
}

/// Without a compositor to reach, the run fails and backs off: `available`
/// is false and an action is `failed`, never the caller's error.
#[test]
fn without_a_compositor_outputs_is_unavailable_and_fails_as_events() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    outputs::configure(Some(WaylandTarget::Socket(dir.path().join("wayland-none"))));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let events: Events = Arc::default();
    let log = events.clone();
    b.outputs.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { args, .. } = a {
            let args = args
                .iter()
                .map(|d| match d {
                    Data::Text(t) => t.to_string(),
                    d => format!("{d:?}"),
                })
                .collect();
            log.lock().unwrap().push(("event".to_string(), args));
        }
    }));
    b.outputs.acquire(&rt);
    until(&rt, &s, "the failed run", || !b.outputs.running());
    assert_eq!(b.outputs.cells().available.get_untracked(&rt), Ok(false));
    let list = Data::List(vec![OutputChange::of("DP-1").to_data()]);
    b.outputs
        .dynamic()
        .action(&rt, "apply", None, &[list], Default::default())
        .expect("a refused action is an event, not an error");
    until(&rt, &s, "the failed event", || {
        !events.lock().unwrap().is_empty()
    });
    assert_eq!(events.lock().unwrap()[0].1[0], "apply");
    s.shutdown();
    outputs::configure(None);
}

/// (service-ready) The compositor going away: `ready` turns false before
/// the list is cleared, so no reader ever sees `all` empty while `ready`
/// is true, and the cleared list is a boot value (an `on change` of the
/// list takes it as its baseline instead of firing).
#[test]
fn a_lost_compositor_is_unready_before_its_heads_clear() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    let _serial = serial();
    let Some(sway) = Sway::start("outputs-unready") else {
        return;
    };
    let h = Harness::start(&sway);
    assert!(!h.b.outputs.ready(&h.rt), "not in the flush of its report");
    h.until("ready", |h| h.b.outputs.ready(&h.rt));
    assert!(!h.all().is_empty());
    // `outputs.all`'s length, read tracked as a binding reads it.
    let all = <outputs::Outputs as Store>::FIELDS
        .iter()
        .position(|f| f.name == "all")
        .unwrap();
    let heads = move |rt: &Runtime, c: &strand_services::Client<outputs::Outputs>| match c
        .cells()
        .read(
        rt, all,
    )? {
        Data::List(l) => Ok(l.len()),
        d => panic!("{d:?}"),
    };
    let client = h.b.outputs.clone();
    let seen: Rc<RefCell<Vec<(bool, usize)>>> = Rc::default();
    let log = seen.clone();
    let _watch = h.rt.effect(move |rt| {
        let ready = client.ready(rt);
        let n = heads(rt, &client)?;
        log.borrow_mut().push((ready, n));
        Ok(())
    });
    let client = h.b.outputs.clone();
    let changes = Rc::new(Cell::new(0));
    let fired = changes.clone();
    let _on_change = h.rt.on_change(
        move |rt| heads(rt, &client),
        move |_, _| {
            fired.set(fired.get() + 1);
            Ok(())
        },
    );
    h.rt.flush();
    drop(sway);
    h.until("the heads cleared", |h| h.all().is_empty());
    for _ in 0..5 {
        h.s.pump(&h.rt);
        h.rt.flush();
    }
    assert!(!h.b.outputs.ready(&h.rt));
    let seen = seen.borrow();
    assert!(
        !seen.iter().any(|&(ready, n)| ready && n == 0),
        "the empty list was seen while ready: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&(false, 0)), "{seen:?}");
    assert_eq!(changes.get(), 0, "the clear is not a change");
    h.s.shutdown();
    outputs::configure(None);
}

/// (outputs-resync) Hyprland 0.57's output management is right only at
/// bind: a change comes as the head's values from before it with no
/// manager `done` (`Lagging`), or as nothing at all (`Silent`: a move
/// made with `hyprctl eval 'hl.monitor(…)'`), and a manager stopped for a
/// resync is sent `done`s while the one bound after it is sent nothing
/// (review round 4, seen on nested Hyprland 0.57). `outputs` checks its
/// heads against each output's xdg-output place once the `wl_output`s
/// settle, binds the manager again when they disagree (or a change has
/// no `done` for 250 ms), and a manager bound after the change gets the
/// outputs as they are: every move is followed, not only the first. The
/// list stays ready and each output keeps its id. The fake's wlroots
/// shape (a change and a `done`) is followed at once.
#[test]
fn a_change_with_no_done_is_read_again() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        assert_eq!((h.output("FAKE-2").x, h.output("FAKE-2").y), (1920, 0));
        let ids: Vec<String> = h.all().into_iter().map(|o| o.id).collect();
        assert_eq!(
            ids,
            ["Fake | FAKE-1 | Fake FAKE-1", "Fake | FAKE-2 | Fake FAKE-2"]
        );
        // Each change is read, the ones after a resync too.
        for (n, x, y) in [(1, 3000, 40), (1, 2000, 0), (0, 0, 500), (1, 4000, 0)] {
            fake.cmd(Cmd::MoveOutput(n, x, y));
            let name = ["FAKE-1", "FAKE-2"][n];
            h.until(&format!("{shape:?}: {name} at {x}, {y}"), |h| {
                let o = h.output(name);
                (o.x, o.y) == (i64::from(x), i64::from(y))
            });
        }
        assert!(h.b.outputs.ready(&h.rt), "{shape:?}: still ready");
        let again: Vec<String> = h.all().into_iter().map(|o| o.id).collect();
        assert_eq!(again, ids, "{shape:?}: the same ids");
        // An apply goes to the manager bound now.
        h.act("apply", &[OutputChange::of("FAKE-1")]);
        assert_eq!(h.next_event("the answer").0, "succeeded", "{shape:?}");
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-mode, review round 4) A monitor plugged in during the session
/// showing a mode not in its list: Hyprland 0.57 sends its head (naming no
/// current mode) and the manager's `done` before the client can bind its
/// `wl_output`, whose mode comes after. The list is published again then,
/// so `Output.mode` is that mode, not null (seen on nested Hyprland:
/// `hyprctl output create headless HOT-1`).
#[test]
fn a_monitor_plugged_in_has_its_wl_outputs_mode() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    for shape in [OutputManagement::Silent, OutputManagement::Lagging] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        fake.cmd(Cmd::AddOutput("HOT-1", 1280, 720));
        h.until(&format!("{shape:?}: HOT-1 with its mode"), |h| {
            h.all()
                .iter()
                .any(|o| o.name == "HOT-1" && o.mode == Some(mode(1280, 720, 60.0)))
        });
        let o = h.output("HOT-1");
        assert!(o.enabled, "{shape:?}");
        assert_eq!((o.x, o.y), (3840, 0), "{shape:?}");
        assert_eq!(o.id, "Fake | HOT-1 | Fake HOT-1", "{shape:?}");
        assert_eq!(h.output("FAKE-1").mode, Some(mode(1920, 1080, 60.0)));
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-mode) An output on that shows a mode not in its list:
/// Hyprland 0.57's head names no current mode for it, and its `wl_output`
/// sends it. `Output.mode` is that mode, not null, which reads as off.
#[test]
fn a_mode_not_in_the_list_is_the_wl_outputs() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    let fake = Fake::builder()
        .toplevel_list(false)
        .outputs(&["FAKE-1", "FAKE-2"])
        .output_management(OutputManagement::Lagging)
        .start();
    fake.cmd(Cmd::CustomMode(0, 1280, 720));
    fake.sync();
    let h = Harness::start_on(fake.socket.clone());
    h.until("ready", |h| h.b.outputs.ready(&h.rt));
    let o = h.output("FAKE-1");
    assert!(o.enabled);
    assert_eq!(o.mode, Some(mode(1280, 720, 60.0)), "{o:#?}");
    assert_eq!(o.modes, [mode(1920, 1080, 60.0)]);
    assert_eq!(h.output("FAKE-2").mode, Some(mode(1920, 1080, 60.0)));
    h.s.shutdown();
    outputs::configure(None);
}

/// (outputs-apply) Hyprland 0.57 saves a configuration in the manager it
/// was made on and follows, for each monitor, the oldest manager still
/// connected that saved one (`COutputManagementProtocol::getOutputStateFor`),
/// answering `succeeded` either way. A resync that bound the manager again
/// on the same connection made every configuration after the first go to
/// a newer manager, which Hyprland did not follow (seen on nested Hyprland
/// 0.57: NESTED-2 stayed at 3000 after a `succeeded` apply to 2560). The
/// service now reads again on a connection of its own and makes every
/// configuration on the one manager it bound: each apply is followed.
#[test]
fn every_apply_is_followed_after_a_resync() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        let place = |n: &str, x: i64, y: i64| OutputChange {
            x: Some(x),
            y: Some(y),
            ..OutputChange::of(n)
        };
        let at = |h: &Harness, n: &str| {
            let o = h.output(n);
            (o.x, o.y)
        };
        // The first apply, a change from outside, then more applies.
        h.act("apply", &[place("FAKE-2", 3000, 0)]);
        assert_eq!(h.next_event("the first").0, "succeeded", "{shape:?}");
        h.until(&format!("{shape:?}: FAKE-2 at 3000"), |h| {
            at(h, "FAKE-2") == (3000, 0)
        });
        fake.cmd(Cmd::MoveOutput(0, 0, 200));
        h.until(&format!("{shape:?}: FAKE-1 moved"), |h| {
            at(h, "FAKE-1") == (0, 200)
        });
        for (one, two) in [((1920, 0), (0, 0)), ((0, 0), (5000, 100))] {
            h.act(
                "apply",
                &[place("FAKE-1", one.0, one.1), place("FAKE-2", two.0, two.1)],
            );
            let (event, args) = h.next_event("the answer");
            assert_eq!(event, "succeeded", "{shape:?}: {args:?}");
            h.until(&format!("{shape:?}: FAKE-1 {one:?}, FAKE-2 {two:?}"), |h| {
                at(h, "FAKE-1") == one && at(h, "FAKE-2") == two
            });
        }
        assert!(h.b.outputs.ready(&h.rt), "{shape:?}: still ready");
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-apply) Hyprland 0.57 never answers `stop` with `finished` and
/// keeps a stopped manager until its client goes. Reading again on a
/// connection of its own leaves none: after many changes the service's
/// connection holds its one manager, none stopped, and each read's
/// manager went with its connection. wlroots' shape, which sends its
/// change and a `done`, is never read again.
#[test]
fn stopped_managers_do_not_pile_up() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        for i in 1..=8 {
            let x = 1920 + 100 * i;
            fake.cmd(Cmd::MoveOutput(1, x, 0));
            h.until(&format!("{shape:?}: FAKE-2 at {x}"), |h| {
                h.output("FAKE-2").x == i64::from(x)
            });
        }
        // Each read's connection is closed before its outputs are
        // published; the fake sees it go soon after.
        h.until(&format!("{shape:?}: one manager, none stopped"), |_| {
            let count = fake.output_managers();
            (count.live, count.stopped) == (1, 0)
        });
        let count = fake.output_managers();
        match shape {
            OutputManagement::Wlroots => assert_eq!(count.bound, 1, "never read again"),
            _ => assert!(
                count.bound >= 9,
                "{shape:?}: read again each time: {count:?}"
            ),
        }
        assert!(h.b.outputs.ready(&h.rt), "{shape:?}: still ready");
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-apply) A configuration includes every head as the service
/// has it. Asked for right after a change Hyprland's manager has not
/// sent (its xdg-output already moved), it is made after reading the
/// outputs again, so it does not move back what the change moved.
#[test]
fn an_apply_right_after_a_change_keeps_it() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    for shape in [OutputManagement::Lagging, OutputManagement::Silent] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        fake.cmd(Cmd::MoveOutput(0, 0, 500));
        fake.sync();
        h.act(
            "apply",
            &[OutputChange {
                x: Some(4000),
                ..OutputChange::of("FAKE-2")
            }],
        );
        let (event, args) = h.next_event("the answer");
        assert_eq!(event, "succeeded", "{shape:?}: {args:?}");
        h.until(&format!("{shape:?}: FAKE-2 at 4000"), |h| {
            h.output("FAKE-2").x == 4000
        });
        let one = h.output("FAKE-1");
        assert_eq!((one.x, one.y), (0, 500), "{shape:?}: FAKE-1 kept its move");
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-apply) `succeeded` is the compositor's word that it applied
/// the configuration, so the service checks it against the outputs: when
/// they do not show what was asked once they are quiet, the apply is
/// `failed`, naming how they differ. Here another client's earlier
/// manager placed FAKE-2, which Hyprland follows before the service's.
#[test]
fn an_apply_the_compositor_did_not_take_fails() {
    use strand_fake_wayland::{Cmd, Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        fake.cmd(Cmd::ShadowOutput(1, 3000, 0));
        h.until(&format!("{shape:?}: FAKE-2 at 3000"), |h| {
            h.output("FAKE-2").x == 3000
        });
        h.act(
            "apply",
            &[OutputChange {
                x: Some(5000),
                ..OutputChange::of("FAKE-2")
            }],
        );
        let (event, args) = h.next_event("the answer");
        match shape {
            // wlroots follows the last configuration.
            OutputManagement::Wlroots => {
                assert_eq!(event, "succeeded", "{args:?}");
                h.until("FAKE-2 at 5000", |h| h.output("FAKE-2").x == 5000);
            }
            _ => {
                assert_eq!(event, "failed", "{shape:?}: {args:?}");
                assert_eq!(args[0], "apply");
                assert!(
                    args[1].contains("said it applied")
                        && args[1].contains("FAKE-2 is at 3000, 0, not 5000, 0"),
                    "{shape:?}: {}",
                    args[1]
                );
                assert_eq!(h.output("FAKE-2").x, 3000, "{shape:?}: as it is");
            }
        }
        // The next apply that matches what Hyprland follows succeeds.
        h.act("apply", &[OutputChange::of("FAKE-1")]);
        assert_eq!(h.next_event("a no-op").0, "succeeded", "{shape:?}");
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-apply, review round 5) Hyprland 0.57 removes a mirrored
/// monitor's `wl_output` and goes on sending its head as on, so a
/// configuration includes it on. Having no `wl_output` then tells
/// nothing of it: the apply is checked on the others, and is
/// `succeeded`, not `failed` with "FAKE-2 is not on".
#[test]
fn an_apply_with_a_mirror_is_checked_on_the_others() {
    use strand_fake_wayland::{Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .mirrors(&["FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        assert!(h.output("FAKE-2").enabled, "{shape:?}: the mirror is on");
        h.act(
            "apply",
            &[OutputChange {
                y: Some(300),
                ..OutputChange::of("FAKE-1")
            }],
        );
        let (event, args) = h.next_event("the answer");
        assert_eq!(event, "succeeded", "{shape:?}: {args:?}");
        h.until(&format!("{shape:?}: FAKE-1 at 0, 300"), |h| {
            h.output("FAKE-1").y == 300
        });
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-apply, review round 6) Hyprland 0.57 answers `succeeded` to
/// a configuration placing a mirror, saves the place and leaves the
/// monitor a mirror where it was (nested Hyprland: HOT-1 asked at 5000,
/// 0 stayed at 0, 200). With no `wl_output` to tell by, the mirror's head
/// is read again and the apply is `failed`; wlroots' shape moves it and
/// it is `succeeded`.
#[test]
fn an_apply_placing_a_mirror_that_stays_fails() {
    use strand_fake_wayland::{Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .mirrors(&["FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        assert_eq!(h.output("FAKE-2").x, 1920, "{shape:?}");
        h.act(
            "apply",
            &[OutputChange {
                x: Some(5000),
                y: Some(0),
                ..OutputChange::of("FAKE-2")
            }],
        );
        let (event, args) = h.next_event("the answer");
        if shape == OutputManagement::Wlroots {
            assert_eq!(event, "succeeded", "{shape:?}: {args:?}");
            assert_eq!(h.output("FAKE-2").x, 5000, "{shape:?}");
        } else {
            assert_eq!(event, "failed", "{shape:?}: {args:?}");
            assert!(
                format!("{args:?}").contains(
                    "the compositor said it applied the configuration, but FAKE-2, \
                     a mirror with no wl_output, is at 1920, 0, not 5000, 0"
                ),
                "{shape:?}: {args:?}"
            );
            assert_eq!(h.output("FAKE-2").x, 1920, "{shape:?}: where it is");
        }
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-apply, review round 5) `succeeded` is emitted once
/// `outputs.all` shows the apply. Hyprland 0.57's reload sends the heads'
/// old values (`Lagging`) or none (`Silent`) while the xdg-outputs move,
/// so the check finds the outputs as asked before the heads are; the
/// heads are read again and published first, and a handler on
/// `succeeded` reading `outputs.all` sees the new places.
#[test]
fn succeeded_comes_after_outputs_all_shows_the_apply() {
    use strand_fake_wayland::{Fake, OutputManagement};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        // `outputs.all` at each event, as a handler on it reads it.
        let seen: Arc<Mutex<Vec<Vec<Output>>>> = Arc::default();
        let log = seen.clone();
        let out = h.b.outputs.clone();
        h.b.outputs.dynamic().observe(Box::new(move |rt, a| {
            if let Applied::Event { .. } = a {
                let all = out.cells().snapshot(rt).map(|s| s.all).unwrap_or_default();
                log.lock().unwrap().push(all);
            }
        }));
        let place = |n: &str, x: i64| OutputChange {
            x: Some(x),
            y: Some(0),
            ..OutputChange::of(n)
        };
        for (one, two) in [(1920, 0), (0, 4000)] {
            h.act("apply", &[place("FAKE-1", one), place("FAKE-2", two)]);
            let (event, args) = h.next_event("the answer");
            assert_eq!(event, "succeeded", "{shape:?}: {args:?}");
            let all = seen.lock().unwrap().remove(0);
            let x = |n: &str| all.iter().find(|o| o.name == n).map(|o| o.x);
            assert_eq!(
                (x("FAKE-1"), x("FAKE-2")),
                (Some(one), Some(two)),
                "{shape:?}: outputs.all at succeeded"
            );
        }
        h.s.shutdown();
    }
    outputs::configure(None);
}

/// (outputs-frame) Hyprland 0.57 applies a configuration, and answers it,
/// at its next frame, and an idle desktop draws none: seen on nested
/// Hyprland, an apply sent while nothing moved waited 13 s for the
/// pointer, and the applies after it ran one change late. An apply left
/// unanswered gets a frame from the service: a 1x1 transparent layer
/// surface, with no input, committed until the answer comes and gone
/// after it. wlroots' shape answers at once and is shown none.
#[test]
fn an_apply_waiting_for_a_frame_is_given_one() {
    use strand_fake_wayland::{Fake, OutputManagement, SurfaceGlobals};
    let _serial = serial();
    for shape in [
        OutputManagement::Lagging,
        OutputManagement::Silent,
        OutputManagement::Wlroots,
    ] {
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&["FAKE-1", "FAKE-2"])
            .output_management(shape)
            .surfaces(SurfaceGlobals::default())
            .applies_at_frame(true)
            .start();
        let h = Harness::start_on(fake.socket.clone());
        h.until("ready", |h| h.b.outputs.ready(&h.rt));
        let frame_surfaces = || {
            fake.surfaces()
                .into_iter()
                .filter(|s| s.namespace.as_deref() == Some("strand-outputs"))
                .collect::<Vec<_>>()
        };
        assert!(
            frame_surfaces().is_empty(),
            "{shape:?}: none before an apply"
        );
        for (n, x) in [(1, 3000), (2, 5000)] {
            let place = OutputChange {
                x: Some(x),
                y: Some(0),
                ..OutputChange::of("FAKE-2")
            };
            let sent = Instant::now();
            h.act("apply", &[place]);
            let (event, args) = h.next_event("the answer");
            assert_eq!(event, "succeeded", "{shape:?}: {args:?}");
            assert!(
                sent.elapsed() < Duration::from_secs(3),
                "{shape:?}: answered in {:?}",
                sent.elapsed()
            );
            assert_eq!(h.output("FAKE-2").x, x, "{shape:?}");
            assert_eq!(fake.held_configurations(), 0, "{shape:?}");
            let made = frame_surfaces();
            if shape == OutputManagement::Wlroots {
                assert!(made.is_empty(), "answered at once: {made:?}");
                continue;
            }
            assert_eq!(made.len(), n, "{shape:?}: one a wait: {made:?}");
            let s = &made[n - 1];
            assert_eq!(s.layer, Some(3), "the overlay layer: {s:?}");
            assert_eq!(s.configured, Some((1, 1)), "{s:?}");
            assert_eq!(
                s.buffer,
                Some(strand_fake_wayland::BufferKind::Shm {
                    width: 1,
                    height: 1
                }),
                "{s:?}"
            );
            assert_eq!(s.input, Some(Default::default()), "no input: {s:?}");
            h.until(&format!("{shape:?}: the frame surface gone"), |_| {
                frame_surfaces().iter().all(|s| s.destroyed)
            });
        }
        h.s.shutdown();
    }
    outputs::configure(None);
}

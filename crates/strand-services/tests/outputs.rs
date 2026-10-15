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
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
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
        outputs::configure(Some(WaylandTarget::Socket(sway.socket())));
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

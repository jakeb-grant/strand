//! (services-round) `idle.after(timeout)` against a real headless sway
//! (sway 1.9 implements `ext_idle_notifier_v1`): a timeout read is
//! followed (one notification object each, made the first time it is
//! read), true once the seat has had no input that long, false again at
//! the next key; and with no compositor to ask, it is false and
//! `available` says why.

mod common;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::Sway;
use common::keyboard::Keyboard;
use strand_core::Runtime;
use strand_services::idle::{self, IdleTimeout};
use strand_services::wm::WaylandTarget;
use strand_services::{Builtin, Buses, Data, DynService, FromData, Services};

/// The tests set the process-wide display `idle` connects to: one at a
/// time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// `idle.after(timeout)` as the language calls it.
fn after(rt: &Runtime, d: &dyn DynService, timeout: Duration) -> bool {
    match d.call(rt, "after", &[Data::Duration(timeout)]) {
        Some(Ok(Data::Bool(b))) => b,
        other => panic!("idle.after: {other:?}"),
    }
}

fn timeouts(rt: &Runtime, b: &Builtin) -> Vec<IdleTimeout> {
    let d = b.idle.dynamic();
    let field = d
        .fields()
        .iter()
        .position(|f| f.name == "timeouts")
        .unwrap();
    d.keyed_items(rt, field)
        .unwrap()
        .iter()
        .map(|t| IdleTimeout::from_data(t).unwrap())
        .collect()
}

#[test]
fn after_follows_the_seats_idle_time() {
    let _one = serial();
    let Some(sway) = Sway::start("after_follows_the_seats_idle_time") else {
        return;
    };
    idle::configure(Some(WaylandTarget::Socket(sway.socket())));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let d = b.idle.dynamic();
    let cells = b.idle.cells();
    // Nothing runs, and nothing is followed, before a reader.
    assert!(!b.idle.running());
    b.idle.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "available", || {
        cells.available.get_untracked(&rt) == Ok(true)
    });
    // A keyboard on the seat, so its keys are input.
    let dir = tempfile::tempdir().unwrap();
    let mut keyboard = Keyboard::new(&sway.socket(), dir.path());
    keyboard.tap();
    let short = Duration::from_millis(300);
    let long = Duration::from_secs(3600);
    // Read: false at first, then followed, and idle after 300 ms.
    let started = Instant::now();
    assert!(!after(&rt, &*d, short));
    assert!(!after(&rt, &*d, long));
    until(&rt, &s, "both timeouts followed", || {
        timeouts(&rt, &b).len() >= 2
    });
    until(&rt, &s, "idle after 300 ms", || after(&rt, &*d, short));
    assert!(started.elapsed() >= short, "{:?}", started.elapsed());
    assert!(!after(&rt, &*d, long), "an hour has not passed");
    assert!(
        timeouts(&rt, &b).contains(&IdleTimeout {
            timeout: short,
            idle: true
        }),
        "{:?}",
        timeouts(&rt, &b)
    );
    // A key: no longer idle.
    keyboard.tap();
    until(&rt, &s, "resumed at the key", || !after(&rt, &*d, short));
    // And idle again once the seat rests.
    until(&rt, &s, "idle again", || after(&rt, &*d, short));
    // Asking again for a timeout already followed makes no second one;
    // sub-millisecond parts are dropped as the protocol does.
    assert!(after(&rt, &*d, Duration::from_micros(300_400)));
    let n = timeouts(&rt, &b).len();
    s.pump(&rt);
    rt.flush();
    assert_eq!(timeouts(&rt, &b).len(), n);
    b.idle.release(&rt);
    s.shutdown();
    // The readers held without a scope go with the runtime.
    rt.shutdown();
    idle::configure(None);
}

#[test]
fn with_no_compositor_after_is_false() {
    let _one = serial();
    let dir = tempfile::tempdir().unwrap();
    idle::configure(Some(WaylandTarget::Socket(dir.path().join("wayland-none"))));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let d = b.idle.dynamic();
    b.idle.acquire(&rt);
    until(&rt, &s, "the run failed", || {
        b.idle.starts() > 0 && !b.idle.running()
    });
    assert_eq!(b.idle.cells().available.get_untracked(&rt), Ok(false));
    assert!(!after(&rt, &*d, Duration::from_millis(1)));
    b.idle.release(&rt);
    s.shutdown();
    // The readers held without a scope go with the runtime.
    rt.shutdown();
    idle::configure(None);
}

/// (services-round) Each evaluation of `after` reads its timeout until
/// it re-runs or its scope goes: a reload that changes the timeout (the
/// old tree's reader gone, the new one's reading another) leaves one
/// notification, the old one destroyed once nobody has read it for
/// `LET_GO`; a reader released and counted again at once (a
/// re-evaluation) keeps its notification.
#[test]
fn a_timeout_nobody_reads_is_let_go() {
    let _one = serial();
    let Some(sway) = Sway::start("a_timeout_nobody_reads_is_let_go") else {
        return;
    };
    idle::configure(Some(WaylandTarget::Socket(sway.socket())));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let d = b.idle.dynamic();
    b.idle.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "available", || {
        b.idle.cells().available.get_untracked(&rt) == Ok(true)
    });
    let followed = || -> Vec<Duration> { timeouts(&rt, &b).iter().map(|t| t.timeout).collect() };
    let (ten, fifteen) = (Duration::from_secs(600), Duration::from_secs(900));
    let (first, _) = rt.scope(|rt| after(rt, &*d, ten));
    until(&rt, &s, "ten minutes followed", || followed() == [ten]);
    // A reload: the old reader goes, the new one reads another timeout.
    first.dispose(&rt);
    let (second, _) = rt.scope(|rt| after(rt, &*d, fifteen));
    until(&rt, &s, "fifteen minutes followed", || {
        followed().contains(&fifteen)
    });
    until(&rt, &s, "ten minutes let go", || followed() == [fifteen]);
    // Released and read again at once: still the one.
    second.dispose(&rt);
    let (third, _) = rt.scope(|rt| after(rt, &*d, fifteen));
    std::thread::sleep(idle::LET_GO + Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert_eq!(followed(), [fifteen]);
    // Nobody reads it: none left.
    third.dispose(&rt);
    until(&rt, &s, "nothing followed", || followed().is_empty());
    b.idle.release(&rt);
    s.shutdown();
    // The readers held without a scope go with the runtime.
    rt.shutdown();
    idle::configure(None);
}

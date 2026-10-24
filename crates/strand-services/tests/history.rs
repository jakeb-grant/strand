//! (history) A service a `graph` keeps history of samples while no reader
//! is visible, at the kept, coarser period, and only then
//! (design.md, "Lifecycle"; decisions.md history): [`Ticker`] follows
//! [`Cx::period`], so a polled service samples every visible period
//! while shown, every kept period while hidden and kept, and never while
//! hidden and not kept (true idle). Its own process: the sample counters
//! are per process.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::{
    Builtin, Buses, Cx, Msg, ServiceError, Services, Store, Ticker, cpu, service,
};

static TICKS: AtomicU64 = AtomicU64::new(0);

/// The tests counting `TICKS` run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The tests counting `cpu::samples` and `cpu::rebases` run one at a
/// time.
static CPU_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How often `sampled` samples while shown.
const SHOWN: Duration = Duration::from_millis(50);

#[service(name = "sampled", schema = "service sampled { n: int }")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Sampled {
    pub n: i64,
}

impl Sampled {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        cx.ready();
        let mut tick = Ticker::new(&cx, SHOWN);
        loop {
            tokio::select! {
                m = cx.recv() => {
                    let Some(m) = m else { return Ok(()) };
                    tick.follow(&cx);
                    if matches!(m, Msg::Visible(true)) {
                        tick.reset();
                    }
                }
                _ = tick.tick() => {
                    TICKS.fetch_add(1, Ordering::Relaxed);
                    if !cx.update(|s| s.n += 1) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Ticks of `sampled` over `span`, pumping the logic side meanwhile.
fn ticks_over(rt: &Runtime, s: &Services, span: Duration) -> u64 {
    let start = TICKS.load(Ordering::Relaxed);
    let end = Instant::now() + span;
    while Instant::now() < end {
        s.pump(rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(5));
    }
    TICKS.load(Ordering::Relaxed) - start
}

#[test]
fn a_kept_service_samples_hidden_at_its_period_and_only_then() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let sampled = s.register::<Sampled>(&rt);
    // Shown: every 50 ms (20 in a second, give or take the scheduler).
    sampled.acquire(&rt);
    let shown = ticks_over(&rt, &s, Duration::from_millis(1000));
    // The lower bound is loose: a loaded runner wakes late, never early.
    assert!((8..=21).contains(&shown), "shown: {shown}");
    // Hidden, nothing keeping it: nothing, through the 5 s grace.
    sampled.release(&rt);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(ticks_over(&rt, &s, Duration::from_millis(600)), 0);
    // Hidden and kept every 250 ms: four a second, not twenty.
    sampled.keep(&rt, Some(Duration::from_millis(250)));
    let kept = ticks_over(&rt, &s, Duration::from_millis(1100));
    assert!((3..=5).contains(&kept), "kept: {kept}");
    // Shown again while kept: the visible period wins.
    sampled.acquire(&rt);
    let both = ticks_over(&rt, &s, Duration::from_millis(1000));
    assert!(both >= 8, "shown and kept: {both}");
    // Hidden again: back to the kept period.
    sampled.release(&rt);
    let kept = ticks_over(&rt, &s, Duration::from_millis(1100));
    assert!((3..=5).contains(&kept), "kept again: {kept}");
    // Let go: nothing more, and it stops after the grace.
    sampled.keep(&rt, None);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(ticks_over(&rt, &s, Duration::from_millis(600)), 0);
    assert!(sampled.running(), "inside the grace");
    s.shutdown();
}

/// A kept period shorter than the visible one never samples faster than
/// shown.
#[test]
fn a_kept_period_is_never_faster_than_the_visible_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let sampled = s.register_as::<Sampled>(&rt, "sampled2");
    sampled.keep(&rt, Some(Duration::from_millis(5)));
    assert!(sampled.running(), "kept starts it");
    assert_eq!(sampled.readers(), 0);
    let n = ticks_over(&rt, &s, Duration::from_millis(1000));
    assert!(n <= 21, "at most every 50 ms: {n}");
    s.shutdown();
}

/// The real `cpu` service: hidden and kept every 2 s it samples at that
/// period (each sample the load since the one before), and stops sampling
/// when let go.
#[test]
fn a_kept_cpu_samples_at_the_kept_period_while_hidden() {
    let _serial = CPU_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.cpu.keep(&rt, Some(Duration::from_secs(2)));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(b.cpu.readers(), 0);
    let start = cpu::samples();
    let end = Instant::now() + Duration::from_millis(4500);
    while Instant::now() < end {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    let n = cpu::samples() - start;
    assert!((2..=3).contains(&n), "every 2 s, not every second: {n}");
    let usage = b.cpu.cells().usage.get_untracked(&rt).unwrap();
    assert!((0.0..=1.0).contains(&usage), "{usage}");
    b.cpu.keep(&rt, None);
    std::thread::sleep(Duration::from_millis(100));
    let after = cpu::samples();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(cpu::samples(), after, "let go: no more samples");
    s.shutdown();
}

/// (history, the review) The real `cpu` takes a fresh baseline whenever
/// sampling resumes after a pause (hidden, then kept) and when a reader
/// is shown, so its next sample is the load since then; while it keeps
/// sampling, nothing is re-based.
#[test]
fn a_cpu_resuming_after_a_pause_takes_a_fresh_baseline() {
    let _serial = CPU_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let pump = |ms: u64| {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            s.pump(&rt);
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    b.cpu.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    pump(200);
    let shown = cpu::rebases();
    // Hidden inside the grace: paused, nothing sampled.
    b.cpu.release(&rt);
    pump(200);
    let paused = cpu::samples();
    pump(1200);
    assert_eq!(cpu::samples(), paused, "paused");
    // Kept: sampling resumes from a fresh baseline.
    b.cpu.keep(&rt, Some(Duration::from_secs(2)));
    pump(200);
    assert_eq!(cpu::rebases(), shown + 1, "resumed: a fresh baseline");
    // Shown again: once more, from now.
    b.cpu.acquire(&rt);
    pump(200);
    assert_eq!(cpu::rebases(), shown + 2, "shown: a fresh baseline");
    pump(1200);
    assert_eq!(cpu::rebases(), shown + 2, "sampling on: none");
    assert!(cpu::samples() > paused, "sampled while shown");
    s.shutdown();
}

#[service(name = "evented", schema = "service evented { n: int }")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Evented {
    pub n: i64,
}

impl Evented {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        cx.ready();
        while cx.recv().await.is_some() {}
        Ok(())
    }
}

/// (history, the review) A body ticking on a clock ([`Ticker`], which
/// asks [`Cx::period`]) says it polls without being told: kept and
/// hidden, its samples are polled ones, at the kept period; shown, or a
/// body reporting its changes as they happen, each value is exact.
#[test]
fn a_ticking_body_says_it_polls() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let sampled = s.register_as::<Sampled>(&rt, "sampled3");
    let evented = s.register::<Evented>(&rt);
    let kept = Duration::from_millis(250);
    sampled.keep(&rt, Some(kept));
    evented.keep(&rt, Some(kept));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    ticks_over(&rt, &s, Duration::from_millis(100));
    assert_eq!(sampled.sampled(), Some(kept), "kept, hidden: polled");
    assert_eq!(evented.sampled(), None, "reports as it changes");
    sampled.acquire(&rt);
    assert_eq!(sampled.sampled(), None, "shown: exact");
    sampled.release(&rt);
    // Told outright, a slower clock than the kept period wins.
    evented.polls(Some(Duration::from_secs(1)));
    assert_eq!(evented.sampled(), Some(Duration::from_secs(1)));
    s.shutdown();
}

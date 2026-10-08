//! The idle hook (`set_idle_hook`) is process-wide, so it is tested in a
//! binary of its own: it runs once each time a worker's queue drains
//! after work, on the worker's thread, and never while the worker idles.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use strand_scene::{Font, Scale};
use strand_text::*;

static RAN: AtomicUsize = AtomicUsize::new(0);
static ON: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

fn hook() {
    if let Ok(mut on) = ON.lock() {
        on.push(std::thread::current().id());
    }
    RAN.fetch_add(1, Ordering::SeqCst);
}

fn request(key: u64, text: &str) -> TextRequest {
    TextRequest {
        key: TextKey(key),
        text: text.into(),
        style: TextStyle {
            font: Font {
                family: TEST_FONT_FAMILY.into(),
                size: 13.0,
                weight: 400,
            },
            ..TextStyle::default()
        },
        max_width: None,
        scale: Scale::ONE,
    }
}

fn wait_for(n: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while RAN.load(Ordering::SeqCst) < n {
        assert!(
            Instant::now() < deadline,
            "the idle hook never ran {n} times"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn the_idle_hook_runs_once_per_drained_burst_on_the_worker() {
    set_idle_hook(hook);
    let data = std::fs::read(test_font_path()).unwrap();
    let worker = TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap();
    // Spawned and idle: nothing done, so no hook.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(RAN.load(Ordering::SeqCst), 0, "ran before any work");
    // One burst of two requests: the hook once, after both.
    worker.request(request(1, "12:59")).unwrap();
    worker.request(request(2, "13:00")).unwrap();
    for _ in 0..2 {
        worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
    }
    wait_for(1);
    // Idle again: it does not run a second time for the same burst.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(RAN.load(Ordering::SeqCst), 1, "ran again while idle");
    // The next burst runs it again.
    worker.request(request(3, "13:01")).unwrap();
    worker
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    wait_for(2);
    let on = ON.lock().unwrap().clone();
    let me = std::thread::current().id();
    assert!(
        on.iter().all(|t| *t != me) && on.windows(2).all(|w| w[0] == w[1]),
        "the hook runs on the worker's own thread: {on:?}"
    );
}

//! The image worker's idle hook (`image::set_idle_hook`) is
//! process-wide, so it is tested in a binary of its own: it runs once
//! each time the worker's queue drains after a decode, on the worker's
//! thread, and never while the worker idles.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use strand_render::image::{
    Fit, IconTheme, ImageBackend, ImageKey, ImageStore, ImageWorker, set_idle_hook,
};
use strand_scene::SurfaceId;

static RAN: AtomicUsize = AtomicUsize::new(0);
static ON: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

fn hook() {
    if let Ok(mut on) = ON.lock() {
        on.push(std::thread::current().id());
    }
    RAN.fetch_add(1, Ordering::SeqCst);
}

fn key(name: &str, w: u32) -> ImageKey {
    ImageKey {
        source: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned(),
        icon: false,
        w,
        h: w,
        fit: Fit::Contain,
        scale: 1,
    }
}

/// Polls `cache` until `n` results arrived, then until the hook ran
/// `ran` times.
fn arrive(cache: &mut ImageStore, n: usize, ran: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = 0;
    while got < n || RAN.load(Ordering::SeqCst) < ran {
        got += cache.poll().len();
        assert!(
            Instant::now() < deadline,
            "{got} of {n} decodes, the hook {} of {ran} times",
            RAN.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn the_idle_hook_runs_once_per_drained_burst_on_the_image_worker() {
    set_idle_hook(hook);
    let worker = ImageWorker::spawn(IconTheme::named("StrandTest"), None).unwrap();
    let mut cache = ImageStore::new(ImageBackend::Worker(worker));
    // Spawned and idle: nothing decoded, so no hook.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(RAN.load(Ordering::SeqCst), 0, "ran before any work");
    // One frame's two images: decoded, then the hook as the queue drains.
    let surface = SurfaceId(1);
    let two = [key("halves.jpg", 32), key("halves-large.jpg", 48)];
    cache.want(surface, &two, false);
    arrive(&mut cache, 2, 1);
    let first = RAN.load(Ordering::SeqCst);
    assert!(first <= 2, "ran {first} times for two decodes");
    // Idle again: it does not run for the same burst.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(RAN.load(Ordering::SeqCst), first, "ran again while idle");
    // The next decode runs it again.
    cache.want(surface, &[key("halves.jpg", 24)], false);
    arrive(&mut cache, 1, first + 1);
    let on = ON.lock().unwrap().clone();
    assert!(
        on.iter().all(|t| *t != std::thread::current().id()),
        "the hook ran on the caller's thread"
    );
    drop(cache);
}

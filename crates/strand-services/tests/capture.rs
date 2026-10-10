//! (M4) Window thumbnails' capture state machine
//! (`strand_services::wm::capture`) against the fake compositor's
//! `ext-image-copy-capture-v1` (sway 1.9 in CI offers none; the live
//! proof is the compositor matrix): a tap captures its window through
//! the protocol thread, frames come only when the window changes and at
//! most `MAX_FPS` a second, new buffer constraints are followed (in any
//! order before their `done`), and the
//! session ends with the last tap or the window; a tap whose window
//! closed is told so (`None`) once.

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Collector;
use strand_fake_wayland::{CAPTURE_SIZE, Cmd, Fake};
use strand_services::wm::capture::{CaptureFrame, MAX_FPS, capture_window};
use strand_services::wm::{self, WaylandTarget, WmConfig};
use tokio::sync::mpsc::unbounded_channel;

fn log(fake: &Fake) -> Vec<String> {
    fake.captures.lock().unwrap().clone()
}

fn count(fake: &Fake, line: &str) -> usize {
    log(fake).iter().filter(|l| *l == line).count()
}

/// Waits (5 s at most) for `f` to hold.
fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "{what}: timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn next(rx: &mpsc::Receiver<CaptureFrame>, what: &str) -> CaptureFrame {
    rx.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("{what}: no frame"))
}

/// The first pixel as straight RGBA.
fn rgba(f: &CaptureFrame) -> [u8; 4] {
    let p = &f.pixels[..4];
    [p[2], p[1], p[0], p[3]]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tap_captures_its_window_as_it_changes() {
    let fake = Fake::builder().capture(true).start();
    fake.cmd(Cmd::AddToplevel("cap-1", "a window", "foot"));
    fake.cmd(Cmd::AddToplevel("cap-2", "another", "foot"));
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: None,
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("the windows", |m| m.windows.len() == 2).await;
    assert_eq!(count(&fake, "session cap-1"), 0, "no tap, no session");

    // A tap: one session, and its first frame at once, scaled down to
    // cover 32 × 24 (the window is 64 × 48).
    let (tx, rx) = mpsc::channel();
    let tap = capture_window("cap-1", (32, 24), move |f| {
        if let Some(f) = f {
            let _ = tx.send(f.clone());
        }
    });
    let first = next(&rx, "the first frame");
    assert_eq!((first.width, first.height), (32, 24));
    assert_eq!(first.pixels.len(), 32 * 24 * 4);
    assert_eq!(rgba(&first), [128, 128, 128, 255]);
    assert_eq!(count(&fake, "session cap-1"), 1);
    assert_eq!(count(&fake, "session cap-2"), 0, "only the tapped window");

    // Still: no frames.
    std::thread::sleep(Duration::from_millis(300));
    assert!(rx.try_recv().is_err(), "a still window sends no frames");
    assert_eq!(count(&fake, "frame cap-1"), 1);

    // A change: a frame in its colour (premultiplied in the buffer).
    fake.cmd(Cmd::Paint("cap-1", [255, 0, 0, 255]));
    let red = next(&rx, "the painted frame");
    assert_eq!(rgba(&red), [255, 0, 0, 255]);

    // Changes faster than MAX_FPS: frames at most that often.
    let start = Instant::now();
    let mut k = 0u8;
    while start.elapsed() < Duration::from_secs(1) {
        k = k.wrapping_add(1);
        fake.cmd(Cmd::Paint("cap-1", [k, 0, 0, 255]));
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(100));
    // The cap is strict (a loaded machine sends fewer frames, never
    // more); how close to it a loaded machine gets is not asserted, only
    // that frames kept coming while the window kept changing.
    let n = rx.try_iter().count() as u32;
    assert!((1..=MAX_FPS + 2).contains(&n), "{n} frames in 1 s");
    if n < MAX_FPS / 2 {
        eprintln!("WARN: {n} frames in 1 s, under half of MAX_FPS ({MAX_FPS})");
    }

    // New constraints: the next frame at the new size.
    fake.cmd(Cmd::ResizeToplevel("cap-1", 128, 32));
    fake.cmd(Cmd::Paint("cap-1", [0, 0, 255, 255]));
    wait("a frame at the new size", || {
        rx.try_iter()
            .last()
            .is_some_and(|f| (f.width, f.height) == (96, 24) && rgba(&f) == [0, 0, 255, 255])
    });

    // The last tap goes: the session ends.
    drop(tap);
    wait("the session ended", || count(&fake, "end cap-1") == 1);

    // A window that closes stops its session; its tap is told the window
    // is gone, once, and gets nothing more.
    let (tx2, rx2) = mpsc::channel::<Option<CaptureFrame>>();
    let _tap2 = capture_window("cap-2", (0, 0), move |f| {
        let _ = tx2.send(f.cloned());
    });
    let full = rx2
        .recv_timeout(Duration::from_secs(5))
        .expect("cap-2's first frame")
        .expect("a frame, not gone");
    assert_eq!(
        (full.width, full.height),
        CAPTURE_SIZE,
        "no limit: full size"
    );
    fake.cmd(Cmd::CloseToplevel("cap-2"));
    wait("stopped", || count(&fake, "stopped cap-2") == 1);
    let gone = rx2
        .recv_timeout(Duration::from_secs(5))
        .expect("told the window is gone");
    assert_eq!(gone, None);
    std::thread::sleep(Duration::from_millis(200));
    assert!(rx2.try_recv().is_err(), "told once, then nothing");
    service.abort();
}

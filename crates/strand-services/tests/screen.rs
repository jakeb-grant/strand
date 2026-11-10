//! (services-round) Screen shots (`strand_services::wm::screen`, the
//! `thumbnail screen` source): one frame of a monitor by its connector
//! name, scaled down to cover the size asked, through
//! `ext-image-copy-capture-v1` with an output source (the fake
//! compositor) and through `wlr-screencopy-unstable-v1` (headless sway
//! 1.9, which offers no ext capture); a monitor that is not there gives
//! `None`.

mod common;

use std::sync::mpsc;
use std::time::Duration;

use common::Sway;
use strand_fake_wayland::{Cmd, Fake};
use strand_services::wm::capture::CaptureFrame;
use strand_services::wm::screen::{SHOT_WAIT, capture_screen};
use strand_services::wm::{self, WaylandTarget, WmConfig};

/// One shot of `connector`, waited for.
fn shoot(connector: &str, max: (u32, u32)) -> Option<CaptureFrame> {
    let (tx, rx) = mpsc::channel();
    let _shot = capture_screen(connector, max, move |f| {
        let _ = tx.send(f);
    });
    rx.recv_timeout(SHOT_WAIT + Duration::from_secs(2))
        .expect("the shot answers")
}

/// The first pixel as straight RGBA.
fn rgba(f: &CaptureFrame) -> [u8; 4] {
    let p = &f.pixels[..4];
    [p[2], p[1], p[0], p[3]]
}

fn aim(socket: std::path::PathBuf) {
    wm::configure(Some(WmConfig {
        backend: None,
        wayland: Some(WaylandTarget::Socket(socket)),
        ..Default::default()
    }));
}

/// The display shots connect to is process-wide: one test at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Both protocols in one test: the display shots connect to is
/// process-wide.
#[test]
fn a_screen_shot_is_one_frame_of_its_monitor() {
    let _serial = serial();
    // ext-image-copy-capture, from the fake's output source.
    let fake = Fake::builder()
        .capture(true)
        .outputs(&["FAKE-1", "FAKE-2"])
        .start();
    aim(fake.socket.clone());
    let f = shoot("FAKE-2", (32, 24)).expect("a frame of FAKE-2");
    assert_eq!((f.width, f.height), (32, 24));
    assert_eq!(f.pixels.len(), 32 * 24 * 4);
    assert_eq!(rgba(&f), [128, 128, 128, 255]);
    fake.cmd(Cmd::Paint("output:FAKE-1", [255, 0, 0, 255]));
    let red = shoot("FAKE-1", (0, 0)).expect("a frame of FAKE-1");
    assert_eq!(
        (red.width, red.height),
        strand_fake_wayland::CAPTURE_SIZE,
        "no limit: the monitor's size"
    );
    assert_eq!(rgba(&red), [255, 0, 0, 255]);
    // A shot returns with its frame and then ends its session; the fake
    // logs the end when it reads the destroy, which may come a moment
    // after (under a loaded CI it did).
    let ended = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let log = loop {
        let log = fake.captures.lock().unwrap().clone();
        if log.contains(&"end output:FAKE-1".to_string()) || std::time::Instant::now() > ended {
            break log;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(
        log.contains(&"session output:FAKE-2".to_string()),
        "{log:?}"
    );
    assert!(
        log.contains(&"end output:FAKE-1".to_string()),
        "a shot ends its session: {log:?}"
    );
    assert!(shoot("HDMI-A-9", (32, 24)).is_none(), "no such monitor");
    drop(fake);

    // wlr-screencopy, from headless sway (1280 × 720).
    let Some(sway) = Sway::start("a_screen_shot_is_one_frame_of_its_monitor") else {
        wm::configure(None);
        return;
    };
    aim(sway.socket());
    let f = shoot("HEADLESS-1", (64, 64)).unwrap_or_else(|| panic!("{}", sway.log()));
    // Covering 64 × 64 keeps the aspect: 114 × 64 (1280 × 720 scaled
    // by 64/720, as large as asked on both sides).
    assert_eq!(f.height, 64, "{}x{}", f.width, f.height);
    assert!(f.width >= 64 && f.width <= 120, "{}x{}", f.width, f.height);
    assert_eq!(f.pixels.len(), (f.width * f.height * 4) as usize);
    assert!(
        f.pixels.chunks(4).all(|p| p[3] == 255),
        "an XRGB frame is opaque"
    );
    assert!(shoot("HEADLESS-9", (64, 64)).is_none());
    wm::configure(None);
}

/// A compositor that takes the connection and never answers (stopped, or
/// stuck in a modeset) gives `None` once the shot has taken
/// [`SHOT_WAIT`], from the connection on: its startup waits under the
/// same deadline as its frame.
#[test]
fn a_stalled_compositor_gives_none_within_shot_wait() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayland-stalled");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (held_tx, held_rx) = mpsc::channel::<()>();
    let _compositor = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // Reads nothing, answers nothing, until the test ends.
        let _ = held_rx.recv();
        drop(stream);
    });
    aim(path);
    let (tx, rx) = mpsc::channel();
    let start = std::time::Instant::now();
    let _shot = capture_screen("FAKE-1", (32, 24), move |f| {
        let _ = tx.send(f.is_none());
    });
    let answer = rx.recv_timeout(SHOT_WAIT + Duration::from_secs(1));
    drop(held_tx);
    wm::configure(None);
    assert_eq!(answer, Ok(true), "no answer within SHOT_WAIT");
    assert!(start.elapsed() >= SHOT_WAIT, "{:?}", start.elapsed());
}

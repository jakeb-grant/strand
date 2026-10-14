//! `media` against small zbus MPRIS players on a private bus: the
//! active player's track, play/pause through the service, the position
//! carried forward (ticking only while watched, never polled), seeking,
//! and the active player's choice as players come and go.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::{Store, media};
use support::*;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

#[derive(Debug)]
struct Track {
    status: String,
    title: String,
    position_us: i64,
    calls: Vec<String>,
    rate: f64,
    /// Its position (and so `GetAll`) does not answer: a player whose
    /// main loop is blocked.
    frozen: bool,
    /// Its `mpris:artUrl`.
    art: String,
    /// Its `mpris:trackid`, when it names one.
    trackid: Option<String>,
    /// Its `CanSeek`.
    can_seek: bool,
}

struct Player(Arc<Mutex<Track>>);

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn play_pause(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        {
            let mut t = self.0.lock().unwrap();
            t.calls.push("PlayPause".into());
            t.status = if t.status == "Playing" {
                "Paused".into()
            } else {
                "Playing".into()
            };
        }
        self.playback_status_changed(&e).await.unwrap();
    }

    async fn next(&self, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        {
            let mut t = self.0.lock().unwrap();
            t.calls.push("Next".into());
            t.title = "Second".into();
            t.position_us = 0;
        }
        self.metadata_changed(&e).await.unwrap();
    }

    async fn previous(&self) {
        self.0.lock().unwrap().calls.push("Previous".into());
    }

    async fn set_position(
        &self,
        track: zbus::zvariant::ObjectPath<'_>,
        position: i64,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) {
        {
            let mut t = self.0.lock().unwrap();
            t.calls.push(format!("SetPosition {track} {position}"));
            // The MPRIS spec: a stale track id is ignored.
            if t.trackid.as_deref() != Some(track.as_str()) {
                return;
            }
            t.position_us = position;
        }
        Self::seeked(&e, position).await.unwrap();
    }

    async fn seek(&self, offset: i64, #[zbus(signal_emitter)] e: SignalEmitter<'_>) {
        let position = {
            let mut t = self.0.lock().unwrap();
            t.calls.push(format!("Seek {offset}"));
            t.position_us = (t.position_us + offset).max(0);
            t.position_us
        };
        Self::seeked(&e, position).await.unwrap();
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.0.lock().unwrap().can_seek
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.0.lock().unwrap().status.clone()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let t = self.0.lock().unwrap();
        let mut m = HashMap::from([
            (
                "xesam:title".to_string(),
                OwnedValue::try_from(Value::from(t.title.as_str())).unwrap(),
            ),
            (
                "xesam:artist".to_string(),
                OwnedValue::try_from(Value::from(vec!["Ann", "Bo"])).unwrap(),
            ),
            ("mpris:length".to_string(), OwnedValue::from(200_000_000i64)),
            (
                "mpris:artUrl".to_string(),
                OwnedValue::try_from(Value::from(t.art.as_str())).unwrap(),
            ),
        ]);
        if let Some(id) = &t.trackid {
            let path = zbus::zvariant::ObjectPath::try_from(id.as_str()).unwrap();
            m.insert(
                "mpris:trackid".to_string(),
                OwnedValue::try_from(Value::from(path)).unwrap(),
            );
        }
        m
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn position(&self) -> i64 {
        let frozen = self.0.lock().unwrap().frozen;
        if frozen {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        self.0.lock().unwrap().position_us
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.0.lock().unwrap().rate
    }

    #[zbus(signal)]
    async fn seeked(e: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

struct Root(String);

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    #[zbus(property)]
    fn identity(&self) -> String {
        self.0.clone()
    }
}

fn player(
    tokio: &tokio::runtime::Runtime,
    address: &str,
    name: &str,
    status: &str,
    title: &str,
    position_us: i64,
) -> (zbus::Connection, Arc<Mutex<Track>>) {
    player_with(tokio, address, name, status, title, position_us, |_| {})
}

fn player_with(
    tokio: &tokio::runtime::Runtime,
    address: &str,
    name: &str,
    status: &str,
    title: &str,
    position_us: i64,
    f: impl FnOnce(&mut Track),
) -> (zbus::Connection, Arc<Mutex<Track>>) {
    let mut t = Track {
        status: status.into(),
        title: title.into(),
        position_us,
        calls: Vec::new(),
        rate: 1.0,
        frozen: false,
        art: "file:///tmp/art.png".into(),
        trackid: None,
        can_seek: true,
    };
    f(&mut t);
    let track = Arc::new(Mutex::new(t));
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(address)
            .unwrap()
            .name(format!("{}{name}", media::PREFIX))
            .unwrap()
            .serve_at(media::PATH, Player(track.clone()))
            .unwrap()
            .serve_at(media::PATH, Root(name.to_string()))
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    (conn, track)
}

#[test]
fn media_follows_the_active_player_without_polling() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (a_conn, a) = player(&tokio, &bus.address, "mpv", "Paused", "First", 30_000_000);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.title.get_untracked(&rt), Ok(Some("First".into())));
    assert_eq!(cells.artist.get_untracked(&rt), Ok(Some("Ann, Bo".into())));
    assert_eq!(cells.player.get_untracked(&rt), Ok(Some("mpv".into())));
    assert_eq!(cells.playing.get_untracked(&rt), Ok(false));
    assert_eq!(
        cells.art.get_untracked(&rt),
        Ok(Some("file:///tmp/art.png".into()))
    );
    assert_eq!(
        cells.length.get_untracked(&rt),
        Ok(Some(Duration::from_secs(200)))
    );
    assert_eq!(
        cells.elapsed.get_untracked(&rt),
        Ok(Duration::from_secs(30))
    );
    assert_eq!(cells.position.get_untracked(&rt), Ok(0.15));

    // Play, through the service: the player is told; it reports.
    b.media
        .dynamic()
        .action(&rt, "play_pause", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "playing", || {
        cells.playing.get_untracked(&rt) == Ok(true)
    });
    assert_eq!(a.lock().unwrap().calls, ["PlayPause"]);

    // Nobody shows the time: no ticks (the player is never polled
    // either: its position is asked for on changes only).
    std::thread::sleep(Duration::from_millis(100));
    s.pump(&rt);
    let quiet = b.media.reports();
    std::thread::sleep(Duration::from_millis(1500));
    s.pump(&rt);
    assert_eq!(
        b.media.reports(),
        quiet,
        "no updates while nobody shows the time"
    );

    // A visible reader shows the elapsed time: it moves on its own.
    let elapsed = media::Media::FIELDS
        .iter()
        .position(|f| f.name == "elapsed")
        .unwrap();
    b.media.acquire_field(elapsed);
    until(&rt, &s, "time to pass", || {
        cells
            .elapsed
            .get_untracked(&rt)
            .is_ok_and(|e| e >= Duration::from_secs(32))
    });
    b.media.release_field(elapsed);

    // It seeks.
    tokio
        .block_on(a_conn.emit_signal(
            None::<&str>,
            media::PATH,
            "org.mpris.MediaPlayer2.Player",
            "Seeked",
            &(120_000_000i64,),
        ))
        .unwrap();
    until(&rt, &s, "the seek", || {
        cells
            .elapsed
            .get_untracked(&rt)
            .is_ok_and(|e| e >= Duration::from_secs(120) && e < Duration::from_secs(125))
    });

    // Another player starts playing: it is the active one now.
    let (b_conn, _b) = player(&tokio, &bus.address, "vlc", "Playing", "Other", 0);
    until(&rt, &s, "vlc active", || {
        cells.player.get_untracked(&rt) == Ok(Some("vlc".into()))
    });
    assert_eq!(cells.title.get_untracked(&rt), Ok(Some("Other".into())));
    // It quits: back to mpv.
    drop(b_conn);
    until(&rt, &s, "mpv again", || {
        cells.player.get_untracked(&rt) == Ok(Some("mpv".into()))
    });
    // Next track.
    b.media
        .dynamic()
        .action(&rt, "next", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "the next track", || {
        cells.title.get_untracked(&rt) == Ok(Some("Second".into()))
    });
    assert!(
        cells
            .elapsed
            .get_untracked(&rt)
            .is_ok_and(|e| e < Duration::from_secs(2)),
        "the new track starts at 0"
    );
    // The last player quits: nothing plays.
    drop(a_conn);
    until(&rt, &s, "no player", || {
        cells.player.get_untracked(&rt) == Ok(None)
    });
    assert_eq!(cells.playing.get_untracked(&rt), Ok(false));
    assert_eq!(b.media.starts(), 1);
    s.shutdown();
}

#[test]
fn an_absurd_rate_is_clamped_not_a_panic() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    // A rate no player plays at, overflowing a duration on the first read.
    let (_fast, _) = player_with(&tokio, &bus.address, "fast", "Playing", "Fast", 0, |t| {
        t.rate = 1e300
    });
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    b.media.acquire(&rt);
    let elapsed = media::Media::FIELDS
        .iter()
        .position(|f| f.name == "elapsed")
        .unwrap();
    b.media.acquire_field(elapsed);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "time to pass, a thousand times as fast", || {
        cells
            .elapsed
            .get_untracked(&rt)
            .is_ok_and(|e| e >= Duration::from_secs(20))
    });
    // At the clamped rate it reaches the track's end, and stays there.
    until(&rt, &s, "the end of the track", || {
        cells.position.get_untracked(&rt) == Ok(1.0)
    });
    // And one far too slow: no tick a long way off overflows either.
    let (_slow, _) = player_with(&tokio, &bus.address, "slow", "Playing", "Slow", 0, |t| {
        t.rate = 1e-300
    });
    until(&rt, &s, "the slow player active", || {
        cells.player.get_untracked(&rt) == Ok(Some("slow".into()))
    });
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert!(b.media.running(), "the media body is still running");
    assert_eq!(b.media.starts(), 1, "it never failed");
    assert!(
        s.take_diagnostics().is_empty(),
        "nothing went wrong to report"
    );
    s.shutdown();
}

#[test]
fn a_frozen_player_does_not_hold_up_the_others() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (_a_conn, a) = player(&tokio, &bus.address, "mpv", "Paused", "First", 0);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.title.get_untracked(&rt), Ok(Some("First".into())));
    // A player whose main loop is blocked appears: it is read in a task
    // of its own.
    let (_frozen, _) = player_with(&tokio, &bus.address, "stuck", "Playing", "Stuck", 0, |t| {
        t.frozen = true
    });
    std::thread::sleep(Duration::from_millis(200));
    // mpv is still answered at once (well inside the frozen player's 2 s).
    let asked = std::time::Instant::now();
    b.media
        .dynamic()
        .action(&rt, "play_pause", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "playing", || {
        cells.playing.get_untracked(&rt) == Ok(true)
    });
    assert!(
        asked.elapsed() < Duration::from_millis(1500),
        "mpv waited for the frozen player: {:?}",
        asked.elapsed()
    );
    assert_eq!(a.lock().unwrap().calls, ["PlayPause"]);
    assert_eq!(cells.player.get_untracked(&rt), Ok(Some("mpv".into())));
    s.shutdown();
}

/// The player on `conn` sends `art` as its art (`PropertiesChanged` with
/// its whole `Metadata`, as players do).
fn set_art(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    track: &Arc<Mutex<Track>>,
    art: &str,
) {
    track.lock().unwrap().art = art.into();
    tokio.block_on(async {
        let iface = conn
            .object_server()
            .interface::<_, Player>(media::PATH)
            .await
            .unwrap();
        iface
            .get()
            .await
            .metadata_changed(iface.signal_emitter())
            .await
            .unwrap();
    });
}

/// A 1x1 PNG of `rgba`.
fn png(rgba: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 1, 1);
        enc.set_color(png::ColorType::Rgba);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&rgba).unwrap();
    }
    out
}

/// An HTTP server on 127.0.0.1 (a thread per connection) answering
/// `GET path` with `pages(path)`: a delay and a response. Its port and
/// the paths asked for, in order.
fn http_server(
    pages: impl Fn(&str) -> (Duration, Vec<u8>) + Send + Sync + 'static,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let a = asked.clone();
    let pages = Arc::new(pages);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let pages = pages.clone();
            let a = a.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                loop {
                    let mut l = String::new();
                    match reader.read_line(&mut l) {
                        Ok(0) | Err(_) => return,
                        Ok(_) if l == "\r\n" => break,
                        Ok(_) => {}
                    }
                }
                a.lock().unwrap().push(path.clone());
                let (delay, reply) = pages(&path);
                std::thread::sleep(delay);
                let _ = stream.write_all(&reply);
            });
        }
    });
    (port, asked)
}

fn ok(body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}

/// (m6-services) Remote art is fetched (here over plain HTTP from a local
/// server; HTTPS is `art`'s own test), shown as a file once it is in,
/// cached by URL and fetched again when the URL changes; a URL that is not
/// a picture leaves the art null and is asked once however often the
/// player repeats it; and a fetch that takes its time holds up nothing.
#[test]
fn remote_art_is_fetched_cached_and_never_stalls_the_player() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let (a_png, b_png) = (png([255, 0, 0, 255]), png([0, 0, 255, 255]));
    let (a, b) = (a_png.clone(), b_png.clone());
    let (port, asked) = http_server(move |path| match path {
        "/a.png" => (Duration::ZERO, ok(&a)),
        "/b.png" => (Duration::ZERO, ok(&b)),
        "/page" => (Duration::ZERO, ok(b"<!doctype html><p>a web page")),
        "/slow.png" => (Duration::from_secs(4), ok(&a)),
        _ => (
            Duration::ZERO,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        ),
    });
    let url = |p: &str| format!("http://127.0.0.1:{port}{p}");
    let count = |p: &str| asked.lock().unwrap().iter().filter(|x| *x == p).count();
    let tokio = tokio();
    let first = url("/a.png");
    let (conn, track) = player_with(&tokio, &bus.address, "spotify", "Playing", "Song", 0, |t| {
        t.art = first.clone()
    });
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let shows = |bytes: &[u8]| match cells.art.get_untracked(&rt) {
        Ok(Some(path)) => std::fs::read(&path).is_ok_and(|b| b == bytes),
        _ => false,
    };
    until(&rt, &s, "the remote art", || shows(&a_png));
    let a_file = cells.art.get_untracked(&rt).unwrap().unwrap();
    assert!(!a_file.starts_with("http"), "a file, not the URL: {a_file}");
    assert_eq!(count("/a.png"), 1);

    // Not a picture: no art, and asked once however often it is sent.
    set_art(&tokio, &conn, &track, &url("/page"));
    until(&rt, &s, "the page asked for", || count("/page") == 1);
    for _ in 0..5 {
        set_art(&tokio, &conn, &track, &url("/page"));
        std::thread::sleep(Duration::from_millis(50));
        s.pump(&rt);
    }
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(cells.art.get_untracked(&rt), Ok(None));
    assert_eq!(count("/page"), 1, "no retry storm");
    // A missing one: no art either.
    set_art(&tokio, &conn, &track, &url("/gone.png"));
    until(&rt, &s, "the missing one asked for", || {
        count("/gone.png") == 1
    });
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(cells.art.get_untracked(&rt), Ok(None));

    // Another URL: fetched; the first again: from the cache, at once.
    set_art(&tokio, &conn, &track, &url("/b.png"));
    until(&rt, &s, "the new art", || shows(&b_png));
    set_art(&tokio, &conn, &track, &url("/a.png"));
    until(&rt, &s, "the first art again", || {
        cells.art.get_untracked(&rt) == Ok(Some(a_file.clone()))
    });
    assert_eq!(count("/a.png"), 1, "cached by URL");

    // A server that takes 4 s: the player is answered meanwhile.
    set_art(&tokio, &conn, &track, &url("/slow.png"));
    until(&rt, &s, "the slow one asked for", || {
        count("/slow.png") == 1
    });
    let pressed = std::time::Instant::now();
    b.media
        .dynamic()
        .action(&rt, "play_pause", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "paused", || {
        cells.playing.get_untracked(&rt) == Ok(false)
    });
    assert!(
        pressed.elapsed() < Duration::from_millis(1500),
        "the player waited for the art: {:?}",
        pressed.elapsed()
    );
    assert_eq!(cells.art.get_untracked(&rt), Ok(None), "not in yet");
    until(&rt, &s, "the slow art", || shows(&a_png));
    assert_eq!(b.media.starts(), 1);

    // (services-r2-4) The body ends (here stopped; a restart after an
    // error ends it the same way) while the store still holds the path:
    // the file is still there for an `image media.art` built meanwhile,
    // and the next body shows it without fetching it again.
    let shown = cells.art.get_untracked(&rt).unwrap().unwrap();
    b.media.stop_now(&rt);
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(1000) {
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(cells.art.get_untracked(&rt), Ok(Some(shown.clone())));
    assert!(
        std::path::Path::new(&shown).exists(),
        "the store shows a file the ended body removed"
    );
    b.media.release(&rt);
    b.media.acquire(&rt);
    until(&rt, &s, "the next body", || b.media.starts() == 2);
    until(&rt, &s, "the art again", || shows(&a_png));
    assert_eq!(count("/slow.png"), 1, "fetched again");
    s.shutdown();
}

/// Gaps 13 (m6-w2-services): writing `media.position` seeks the active
/// player: `SetPosition` with its track's id, `Seek` by the offset when
/// the track has none. The position stands at the target at once and
/// the player's `Seeked` confirms it. A player that cannot seek is not
/// asked and keeps its place.
#[test]
fn writing_the_position_seeks_the_player() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (a_conn, a) = player_with(
        &tokio,
        &bus.address,
        "mpv",
        "Paused",
        "First",
        30_000_000,
        |t| t.trackid = Some("/org/mpv/track/7".into()),
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let position = media::Media::FIELDS
        .iter()
        .position(|f| f.name == "position")
        .unwrap();
    let seek = |to: f64| {
        b.media
            .dynamic()
            .write(&rt, position, &[], strand_services::Data::Float(to))
            .unwrap();
        rt.flush();
    };

    // Halfway: SetPosition on its track, 100 s into 200 s.
    seek(0.5);
    until(&rt, &s, "the SetPosition", || {
        a.lock().unwrap().calls == ["SetPosition /org/mpv/track/7 100000000"]
    });
    until(&rt, &s, "the position", || {
        cells.elapsed.get_untracked(&rt) == Ok(Duration::from_secs(100))
            && cells.position.get_untracked(&rt) == Ok(0.5)
    });
    assert_eq!(a.lock().unwrap().position_us, 100_000_000);

    // A track without an id: Seek by the offset from where it is.
    a.lock().unwrap().trackid = None;
    tokio.block_on(async {
        let i = a_conn
            .object_server()
            .interface::<_, Player>(media::PATH)
            .await
            .unwrap();
        i.get()
            .await
            .metadata_changed(i.signal_emitter())
            .await
            .unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    seek(0.25);
    until(&rt, &s, "the Seek", || {
        a.lock().unwrap().calls.last().map(String::as_str) == Some("Seek -50000000")
    });
    until(&rt, &s, "the new position", || {
        cells.elapsed.get_untracked(&rt) == Ok(Duration::from_secs(50))
    });
    assert_eq!(a.lock().unwrap().position_us, 50_000_000);

    // A player that cannot seek is not asked; the place is kept.
    a.lock().unwrap().can_seek = false;
    tokio.block_on(async {
        let i = a_conn
            .object_server()
            .interface::<_, Player>(media::PATH)
            .await
            .unwrap();
        i.get()
            .await
            .can_seek_changed(i.signal_emitter())
            .await
            .unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    let calls = a.lock().unwrap().calls.len();
    seek(0.9);
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert_eq!(a.lock().unwrap().calls.len(), calls, "not asked");
    assert_eq!(
        cells.elapsed.get_untracked(&rt),
        Ok(Duration::from_secs(50)),
        "the place is kept"
    );
    s.shutdown();
}

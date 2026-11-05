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
    /// It seeks without saying so (no `Seeked`).
    quiet: bool,
    /// Its `CanGoNext`, `CanGoPrevious` and `CanControl`.
    can_go_next: bool,
    can_go_previous: bool,
    can_control: bool,
    /// Its track's `mpris:length` in microseconds, when it names one.
    length_us: Option<i64>,
    /// Its `Shuffle`, `LoopStatus` and `Volume`.
    shuffle: bool,
    loop_status: String,
    volume: f64,
    /// It refuses a property written (its `Set` fails).
    refuse: bool,
    /// Its root's `CanRaise` and `DesktopEntry` ("" for none).
    can_raise: bool,
    desktop_entry: String,
}

impl Track {
    /// A property written: logged, and refused while `refuse` is set.
    fn written(&mut self, what: String) -> zbus::fdo::Result<()> {
        self.calls.push(what);
        if self.refuse {
            return Err(zbus::fdo::Error::Failed("refused".into()));
        }
        Ok(())
    }
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
            if t.quiet {
                return;
            }
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
    fn can_go_next(&self) -> bool {
        self.0.lock().unwrap().can_go_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.0.lock().unwrap().can_go_previous
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        self.0.lock().unwrap().can_control
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
            (
                "mpris:artUrl".to_string(),
                OwnedValue::try_from(Value::from(t.art.as_str())).unwrap(),
            ),
        ]);
        if let Some(us) = t.length_us {
            m.insert("mpris:length".to_string(), OwnedValue::from(us));
        }
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

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.0.lock().unwrap().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&mut self, on: bool) -> zbus::fdo::Result<()> {
        let mut t = self.0.lock().unwrap();
        t.written(format!("Set Shuffle {on}"))?;
        t.shuffle = on;
        Ok(())
    }

    #[zbus(property)]
    fn loop_status(&self) -> String {
        self.0.lock().unwrap().loop_status.clone()
    }

    #[zbus(property)]
    fn set_loop_status(&mut self, status: String) -> zbus::fdo::Result<()> {
        let mut t = self.0.lock().unwrap();
        t.written(format!("Set LoopStatus {status}"))?;
        t.loop_status = status;
        Ok(())
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.0.lock().unwrap().volume
    }

    #[zbus(property)]
    fn set_volume(&mut self, volume: f64) -> zbus::fdo::Result<()> {
        let mut t = self.0.lock().unwrap();
        t.written(format!("Set Volume {volume}"))?;
        t.volume = volume;
        Ok(())
    }

    #[zbus(signal)]
    async fn seeked(e: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

struct Root(String, Arc<Mutex<Track>>);

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    async fn raise(&self) {
        self.1.lock().unwrap().calls.push("Raise".into());
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        self.0.clone()
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        self.1.lock().unwrap().can_raise
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        self.1.lock().unwrap().desktop_entry.clone()
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
        quiet: false,
        can_go_next: true,
        can_go_previous: true,
        can_control: true,
        length_us: Some(200_000_000),
        shuffle: false,
        loop_status: "None".into(),
        volume: 0.8,
        refuse: false,
        can_raise: true,
        desktop_entry: String::new(),
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
            .serve_at(media::PATH, Root(name.to_string(), track.clone()))
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
    let next_at = std::time::Instant::now();
    b.media
        .dynamic()
        .action(&rt, "next", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "the next track", || {
        cells.title.get_untracked(&rt) == Ok(Some("Second".into()))
    });
    // From zero when it came: no more than the time since it was asked for,
    // whatever the load made that.
    assert!(
        cells
            .elapsed
            .get_untracked(&rt)
            .is_ok_and(|e| e <= next_at.elapsed() + Duration::from_secs(1)),
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
    let answered = asked.elapsed();
    assert_eq!(a.lock().unwrap().calls, ["PlayPause"]);
    assert_eq!(cells.player.get_untracked(&rt), Ok(Some("mpv".into())));
    s.shutdown();
    let mut gates = strand_test_support::gate::Gates::default();
    gates.check(answered < Duration::from_millis(1500), || {
        format!("mpv waited for the frozen player: {answered:?}")
    });
    gates.hold();
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
    b.media
        .dynamic()
        .action(&rt, "play_pause", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "paused", || {
        cells.playing.get_untracked(&rt) == Ok(false)
    });
    // Paused with the art still on its way: the player didn't wait for it.
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

    // Halfway: SetPosition on its track, 100 s into 200 s. The player
    // says nothing (no `Seeked`): the position stands at the target from
    // the write itself (review minor 9).
    a.lock().unwrap().quiet = true;
    seek(0.5);
    until(&rt, &s, "the SetPosition", || {
        a.lock().unwrap().calls == ["SetPosition /org/mpv/track/7 100000000"]
    });
    until(&rt, &s, "the position", || {
        cells.elapsed.get_untracked(&rt) == Ok(Duration::from_secs(100))
            && cells.position.get_untracked(&rt) == Ok(0.5)
    });
    assert_eq!(a.lock().unwrap().position_us, 100_000_000);
    a.lock().unwrap().quiet = false;

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

/// The player on `conn` signals its property `$changed` (zbus's
/// generated `…_changed`).
macro_rules! signal {
    ($tokio:expr, $conn:expr, $changed:ident) => {
        $tokio.block_on(async {
            let i = $conn
                .object_server()
                .interface::<_, Player>(media::PATH)
                .await
                .unwrap();
            i.get().await.$changed(i.signal_emitter()).await.unwrap();
        })
    };
}

/// (gap 32, m6-service-gaps) `can_next`, `can_previous` and `can_seek`
/// follow what the active player says it can do (`CanGoNext`,
/// `CanGoPrevious`, `CanSeek`), all false while it takes no control and
/// while no player runs; a track of unknown length cannot be sought; an
/// invalidated property is read again; and another active player brings
/// its own answers.
#[test]
fn media_says_what_the_active_player_can_do() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (a_conn, a) = player_with(&tokio, &bus.address, "mpv", "Paused", "First", 0, |t| {
        t.can_go_previous = false
    });
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    let can = || {
        (
            cells.can_next.get_untracked(&rt) == Ok(true),
            cells.can_previous.get_untracked(&rt) == Ok(true),
            cells.can_seek.get_untracked(&rt) == Ok(true),
        )
    };
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(can(), (true, false, true));

    // It can go back now.
    a.lock().unwrap().can_go_previous = true;
    signal!(tokio, a_conn, can_go_previous_changed);
    until(&rt, &s, "can_previous", || can() == (true, true, true));

    // It takes no control: nothing can be done.
    a.lock().unwrap().can_control = false;
    signal!(tokio, a_conn, can_control_changed);
    until(&rt, &s, "no control", || can() == (false, false, false));
    a.lock().unwrap().can_control = true;
    signal!(tokio, a_conn, can_control_changed);
    until(&rt, &s, "control again", || can() == (true, true, true));

    // It cannot seek; then a track of unknown length cannot be sought.
    a.lock().unwrap().can_seek = false;
    signal!(tokio, a_conn, can_seek_changed);
    until(&rt, &s, "no seeking", || can() == (true, true, false));
    {
        let mut t = a.lock().unwrap();
        t.can_seek = true;
        t.length_us = None;
        t.title = "Live".into();
    }
    signal!(tokio, a_conn, can_seek_changed);
    signal!(tokio, a_conn, metadata_changed);
    until(&rt, &s, "the live track", || {
        cells.title.get_untracked(&rt) == Ok(Some("Live".into()))
    });
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(can(), (true, true, false), "unknown length");
    a.lock().unwrap().length_us = Some(200_000_000);
    signal!(tokio, a_conn, metadata_changed);
    until(&rt, &s, "a known length", || can() == (true, true, true));

    // Invalidated (no value sent): read again.
    a.lock().unwrap().can_go_next = false;
    tokio
        .block_on(a_conn.emit_signal(
            None::<&str>,
            media::PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                "org.mpris.MediaPlayer2.Player",
                HashMap::<String, Value>::new(),
                vec!["CanGoNext"],
            ),
        ))
        .unwrap();
    until(&rt, &s, "the invalidated CanGoNext read", || {
        can() == (false, true, true)
    });

    // Another player plays: its answers.
    let (v_conn, _v) = player_with(&tokio, &bus.address, "vlc", "Playing", "Other", 0, |t| {
        t.can_seek = false
    });
    until(&rt, &s, "vlc active", || {
        cells.player.get_untracked(&rt) == Ok(Some("vlc".into()))
    });
    assert_eq!(can(), (true, true, false));
    drop(v_conn);
    until(&rt, &s, "mpv again", || {
        cells.player.get_untracked(&rt) == Ok(Some("mpv".into()))
    });
    assert_eq!(can(), (false, true, true));

    // No player: nothing can be done.
    drop(a_conn);
    until(&rt, &s, "no player", || {
        cells.player.get_untracked(&rt) == Ok(None)
    });
    assert_eq!(can(), (false, false, false));
    s.shutdown();
}

/// (services-round) `media.players` lists every player keyed by its bus
/// name, only while a visible reader reads it, each with its own track,
/// time and actions; `p.select()` makes `media.*` follow it until
/// another player starts playing; writing an item's `position` seeks
/// that player.
#[test]
fn players_list_every_player_and_select_picks_the_one_media_follows() {
    use strand_services::{Data, FromData, Step, ToData};
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (_a_conn, a) = player_with(
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
    let (_b_conn, v) = player(&tokio, &bus.address, "vlc", "Playing", "Other", 0);
    until(&rt, &s, "vlc active", || {
        cells.player.get_untracked(&rt) == Ok(Some("vlc".into()))
    });
    let list = || -> Vec<media::MediaPlayer> {
        cells
            .players
            .get_untracked(&rt)
            .unwrap()
            .items()
            .iter()
            .map(|(_, p)| p.clone())
            .collect()
    };
    // Nobody reads the list: it is empty, and costs nothing.
    assert!(list().is_empty());
    let field = media::Media::FIELDS
        .iter()
        .position(|f| f.name == "players")
        .unwrap();
    b.media.acquire_field(field);
    until(&rt, &s, "two players listed", || list().len() == 2);
    let ps = list();
    let mpv = ps.iter().find(|p| p.name == "mpv").unwrap().clone();
    let vlc = ps.iter().find(|p| p.name == "vlc").unwrap().clone();
    assert_eq!(mpv.id, "org.mpris.MediaPlayer2.mpv");
    assert_eq!(mpv.title.as_deref(), Some("First"));
    assert_eq!(mpv.elapsed, Duration::from_secs(30));
    assert!(!mpv.active && !mpv.playing);
    assert!(vlc.active && vlc.playing);
    // The playing one's time ticks while the list is read.
    until(&rt, &s, "vlc's time to pass", || {
        list()
            .iter()
            .any(|p| p.name == "vlc" && p.elapsed >= Duration::from_secs(1))
    });

    // Select mpv: `media.*` follows it although vlc plays.
    b.media
        .dynamic()
        .action(&rt, "select", Some(&mpv.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "mpv selected", || {
        cells.player.get_untracked(&rt) == Ok(Some("mpv".into()))
            && list().iter().any(|p| p.name == "mpv" && p.active)
    });
    assert_eq!(cells.title.get_untracked(&rt), Ok(Some("First".into())));
    // An item's action reaches that player only.
    b.media
        .dynamic()
        .action(
            &rt,
            "play_pause",
            Some(&vlc.to_data()),
            &[],
            Default::default(),
        )
        .unwrap();
    until(&rt, &s, "vlc paused", || {
        list().iter().any(|p| p.name == "vlc" && !p.playing)
    });
    assert!(a.lock().unwrap().calls.is_empty(), "mpv was not asked");
    assert_eq!(v.lock().unwrap().calls, ["PlayPause"]);
    assert_eq!(
        cells.player.get_untracked(&rt),
        Ok(Some("mpv".into())),
        "the selection holds"
    );
    // vlc starts playing again, after the selection: it is the active one.
    b.media
        .dynamic()
        .action(
            &rt,
            "play_pause",
            Some(&vlc.to_data()),
            &[],
            Default::default(),
        )
        .unwrap();
    until(&rt, &s, "vlc active again", || {
        cells.player.get_untracked(&rt) == Ok(Some("vlc".into()))
    });

    // Writing an item's position seeks that player, not the active one.
    let held = list().into_iter().find(|p| p.name == "mpv").unwrap();
    b.media
        .dynamic()
        .write_item(
            &rt,
            "Player",
            &held.to_data(),
            &[Step::Field("position".into())],
            Data::Float(0.5),
        )
        .unwrap();
    rt.flush();
    until(&rt, &s, "mpv sought", || {
        a.lock().unwrap().calls == ["SetPosition /org/mpv/track/7 100000000"]
    });
    until(&rt, &s, "mpv's place", || {
        list()
            .iter()
            .any(|p| p.name == "mpv" && p.elapsed == Duration::from_secs(100))
    });
    assert!(v.lock().unwrap().calls.len() == 2, "vlc was not sought");

    // Nobody reads the list any more: it empties.
    b.media.release_field(field);
    until(&rt, &s, "the list dropped", || list().is_empty());
    let _ = media::MediaPlayer::from_data(&held.to_data()).unwrap();
    s.shutdown();
}

/// (power-media-kbd) `media.shuffle`, `media.loop` and `media.volume` are
/// the active player's own, read and followed, each with its `can_`
/// field; writing one sets it on the player (MPRIS `Shuffle`,
/// `LoopStatus`, `Volume`) and holds the value at once; a player taking
/// no control is not asked; a refused write brings the real value back; a
/// volume is held to 1; and an item's write reaches only its player.
#[test]
fn media_sets_shuffle_loop_and_volume_on_the_player() {
    use strand_services::{Data, Step, ToData};
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let (a_conn, a) = player(&tokio, &bus.address, "mpv", "Playing", "First", 0);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    let field = |name: &str| {
        media::Media::FIELDS
            .iter()
            .position(|f| f.name == name)
            .unwrap()
    };
    let write = |name: &str, v: Data| {
        b.media.dynamic().write(&rt, field(name), &[], v).unwrap();
        rt.flush();
    };
    let volume = || cells.volume.get_untracked(&rt).unwrap();
    let can = || {
        (
            cells.can_shuffle.get_untracked(&rt).unwrap(),
            cells.can_loop.get_untracked(&rt).unwrap(),
            cells.can_volume.get_untracked(&rt).unwrap(),
        )
    };
    let calls = |t: &Arc<Mutex<Track>>| -> Vec<String> {
        t.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.starts_with("Set "))
            .cloned()
            .collect()
    };
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    assert_eq!(cells.shuffle.get_untracked(&rt), Ok(false));
    assert_eq!(cells.r#loop.get_untracked(&rt), Ok(media::Loop::None));
    assert_eq!(volume(), 0.8);
    assert_eq!(can(), (true, true, true));

    // Written: the player is asked, and the values hold at once.
    write("shuffle", Data::Bool(true));
    write("loop", media::Loop::Playlist.to_data());
    write("volume", Data::Float(0.3));
    assert_eq!(cells.shuffle.get_untracked(&rt), Ok(true));
    assert_eq!(cells.r#loop.get_untracked(&rt), Ok(media::Loop::Playlist));
    assert_eq!(volume(), 0.3);
    until(&rt, &s, "the three set", || calls(&a).len() == 3);
    // Each property in a task of its own: in any order.
    let mut three = calls(&a);
    three.sort();
    assert_eq!(
        three,
        [
            "Set LoopStatus Playlist",
            "Set Shuffle true",
            "Set Volume 0.3"
        ]
    );
    std::thread::sleep(Duration::from_millis(100));
    s.pump(&rt);
    rt.flush();
    assert_eq!(cells.shuffle.get_untracked(&rt), Ok(true));
    assert_eq!(cells.r#loop.get_untracked(&rt), Ok(media::Loop::Playlist));
    assert_eq!(volume(), 0.3);

    // The player changes on its own: followed.
    a.lock().unwrap().shuffle = false;
    signal!(tokio, a_conn, shuffle_changed);
    until(&rt, &s, "its own shuffle", || {
        cells.shuffle.get_untracked(&rt) == Ok(false)
    });

    // It takes no control: nothing can be set, and nothing is asked.
    a.lock().unwrap().can_control = false;
    signal!(tokio, a_conn, can_control_changed);
    until(&rt, &s, "no control", || can() == (false, false, false));
    write("volume", Data::Float(0.5));
    until(&rt, &s, "the volume kept", || volume() == 0.3);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(calls(&a).len(), 3, "asked without control");
    a.lock().unwrap().can_control = true;
    signal!(tokio, a_conn, can_control_changed);
    until(&rt, &s, "control again", || can() == (true, true, true));

    // It refuses: its real value comes back.
    a.lock().unwrap().refuse = true;
    write("volume", Data::Float(0.6));
    assert_eq!(volume(), 0.6, "held at once");
    until(&rt, &s, "the refused volume back", || {
        calls(&a).len() == 4 && volume() == 0.3
    });
    a.lock().unwrap().refuse = false;

    // Too loud: held to 1.
    write("volume", Data::Float(1.4));
    until(&rt, &s, "held to 1", || {
        calls(&a).last().map(String::as_str) == Some("Set Volume 1") && volume() == 1.0
    });

    // An item's write reaches its player only.
    let (v_conn, v) = player(&tokio, &bus.address, "vlc", "Paused", "Other", 0);
    let players = field("players");
    b.media.acquire_field(players);
    let list = || -> Vec<media::MediaPlayer> {
        cells
            .players
            .get_untracked(&rt)
            .unwrap()
            .items()
            .iter()
            .map(|(_, p)| p.clone())
            .collect()
    };
    until(&rt, &s, "two players listed", || list().len() == 2);
    assert_eq!(
        cells.player.get_untracked(&rt),
        Ok(Some("mpv".into())),
        "mpv plays on"
    );
    let held = list().into_iter().find(|p| p.name == "vlc").unwrap();
    assert!(held.can_volume);
    b.media
        .dynamic()
        .write_item(
            &rt,
            "Player",
            &held.to_data(),
            &[Step::Field("volume".into())],
            Data::Float(0.25),
        )
        .unwrap();
    rt.flush();
    until(&rt, &s, "vlc's volume", || {
        calls(&v) == ["Set Volume 0.25"]
            && list().iter().any(|p| p.name == "vlc" && p.volume == 0.25)
    });
    assert_eq!(calls(&a).len(), 5, "mpv was not asked");
    assert_eq!(volume(), 1.0, "the active player's own");
    // What each can do is its own.
    v.lock().unwrap().can_control = false;
    signal!(tokio, v_conn, can_control_changed);
    until(&rt, &s, "vlc takes no control", || {
        list()
            .iter()
            .any(|p| p.name == "vlc" && !p.can_volume && !p.can_shuffle)
    });
    assert!(list().iter().any(|p| p.name == "mpv" && p.can_volume));
    assert_eq!(can(), (true, true, true));
    b.media.release_field(players);
    s.shutdown();
}

/// (player-raise-seek) `media.raise()` and `p.raise()` call the player's
/// MPRIS `Raise`, an item's raise reaching that player and not the
/// active one, not asked of one whose `CanRaise` is false, which its
/// root's `PropertiesChanged` brings; `media.seek(by: -15s)` and
/// `p.seek(by)` call `Seek` with the offset on a stream of unknown length
/// (which a `position` write cannot seek, `can_seek` false and
/// `can_seek_by` true), the time showing the target
/// at once; and `media.icon`, `media.app` and each item's come from the
/// player's `DesktopEntry` (else its bus name's app part) looked up in
/// the desktop entries, again when they change.
#[test]
fn players_raise_seek_by_an_offset_and_name_their_app() {
    use strand_services::{Data, Step, ToData, apps};
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let root = std::env::temp_dir().join(format!("strand-media-apps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("applications");
    std::fs::create_dir_all(&dir).unwrap();
    let icon = root.join("mpv-icon.png");
    std::fs::write(&icon, "png").unwrap();
    let icon = icon.to_string_lossy().into_owned();
    apps::set_config(Some(apps::Config {
        dirs: vec![dir.clone()],
        ..apps::Config::default()
    }));
    let tokio = tokio();
    let (a_conn, a) = player_with(
        &tokio,
        &bus.address,
        "mpv",
        "Paused",
        "Live",
        60_000_000,
        |t| {
            t.length_us = None;
            t.desktop_entry = "io.mpv.Mpv".into();
        },
    );
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let cells = b.media.cells();
    b.media.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(cells.can_raise.get_untracked(&rt), Ok(true));
    // No entry yet: the name itself, and no app.
    until(&rt, &s, "the unmatched name", || {
        cells.icon.get_untracked(&rt) == Ok(Some("io.mpv.Mpv".into()))
    });
    assert_eq!(cells.app.get_untracked(&rt), Ok(None));
    // Installed: looked up again.
    std::fs::write(
        dir.join("io.mpv.Mpv.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=mpv Media Player\nExec=mpv\nIcon={icon}\n"
        ),
    )
    .unwrap();
    apps::changed();
    until(&rt, &s, "its entry's icon", || {
        cells.icon.get_untracked(&rt) == Ok(Some(icon.clone()))
    });
    let app = cells.app.get_untracked(&rt).unwrap().unwrap();
    assert_eq!(
        (app.id.as_str(), app.name.as_str()),
        ("io.mpv.Mpv", "mpv Media Player")
    );

    // Back 15 s on a stream of unknown length: `Seek`, the time at once.
    assert_eq!(
        cells.can_seek.get_untracked(&rt),
        Ok(false),
        "no position write"
    );
    assert_eq!(cells.can_seek_by.get_untracked(&rt), Ok(true), "a seek");
    b.media
        .dynamic()
        .action(
            &rt,
            "seek",
            None,
            &[Data::NegDuration(Duration::from_secs(15))],
            Default::default(),
        )
        .unwrap();
    rt.flush();
    s.pump(&rt);
    rt.flush();
    until(&rt, &s, "the Seek", || {
        a.lock().unwrap().calls == ["Seek -15000000"]
    });
    until(&rt, &s, "the time back", || {
        cells.elapsed.get_untracked(&rt) == Ok(Duration::from_secs(45))
    });
    assert_eq!(a.lock().unwrap().position_us, 45_000_000);

    // Raised.
    b.media
        .dynamic()
        .action(&rt, "raise", None, &[], Default::default())
        .unwrap();
    until(&rt, &s, "the Raise", || {
        a.lock().unwrap().calls.last().map(String::as_str) == Some("Raise")
    });
    // It says it cannot (its root's signal): not asked.
    a.lock().unwrap().can_raise = false;
    tokio.block_on(async {
        let i = a_conn
            .object_server()
            .interface::<_, Root>(media::PATH)
            .await
            .unwrap();
        i.get()
            .await
            .can_raise_changed(i.signal_emitter())
            .await
            .unwrap();
    });
    until(&rt, &s, "no raising", || {
        cells.can_raise.get_untracked(&rt) == Ok(false)
    });
    b.media
        .dynamic()
        .action(&rt, "raise", None, &[], Default::default())
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(a.lock().unwrap().calls.len(), 2, "not asked");

    // Another player, with no desktop entry and no raising: its item's
    // app name is its bus name's, and its actions reach it alone.
    let (_v_conn, v) = player_with(
        &tokio,
        &bus.address,
        "vlc.instance42",
        "Paused",
        "Other",
        30_000_000,
        |t| t.can_raise = false,
    );
    let players = media::Media::FIELDS
        .iter()
        .position(|f| f.name == "players")
        .unwrap();
    b.media.acquire_field(players);
    let list = || -> Vec<media::MediaPlayer> {
        cells
            .players
            .get_untracked(&rt)
            .unwrap()
            .items()
            .iter()
            .map(|(_, p)| p.clone())
            .collect()
    };
    until(&rt, &s, "two players listed", || list().len() == 2);
    let vlc = list()
        .into_iter()
        .find(|p| p.name == "vlc.instance42")
        .unwrap();
    assert_eq!((vlc.icon.as_str(), vlc.app.as_ref()), ("vlc", None));
    assert!(!vlc.can_raise && vlc.can_seek_by);
    // vlc, the newer of two paused players, is the active one.
    assert!(vlc.active);
    let mpv = list().into_iter().find(|p| p.name == "mpv").unwrap();
    assert_eq!(mpv.icon, icon);
    assert_eq!(mpv.app.as_ref().map(|a| a.id.as_str()), Some("io.mpv.Mpv"));
    assert!(!mpv.active && mpv.can_seek_by);
    // mpv raises again: its item's raise reaches it, not the active vlc.
    a.lock().unwrap().can_raise = true;
    tokio.block_on(async {
        let i = a_conn
            .object_server()
            .interface::<_, Root>(media::PATH)
            .await
            .unwrap();
        i.get()
            .await
            .can_raise_changed(i.signal_emitter())
            .await
            .unwrap();
    });
    until(&rt, &s, "mpv raises again", || {
        list().iter().any(|p| p.name == "mpv" && p.can_raise)
    });
    let mpv = list().into_iter().find(|p| p.name == "mpv").unwrap();
    b.media
        .dynamic()
        .action(&rt, "raise", Some(&mpv.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "mpv raised", || {
        a.lock().unwrap().calls.len() == 3
            && a.lock().unwrap().calls.last().map(String::as_str) == Some("Raise")
    });
    // An item that cannot raise is not asked.
    b.media
        .dynamic()
        .action(&rt, "raise", Some(&vlc.to_data()), &[], Default::default())
        .unwrap();
    b.media
        .dynamic()
        .action(
            &rt,
            "seek",
            Some(&vlc.to_data()),
            &[Data::Duration(Duration::from_secs(10))],
            Default::default(),
        )
        .unwrap();
    until(&rt, &s, "vlc sought", || {
        v.lock().unwrap().calls == ["Seek 10000000"]
    });
    until(&rt, &s, "vlc's time", || {
        list()
            .iter()
            .any(|p| p.name == "vlc.instance42" && p.elapsed == Duration::from_secs(40))
    });
    assert_eq!(a.lock().unwrap().calls.len(), 3, "mpv was not asked");
    // An item's position write still needs a length: mpv's is unknown.
    b.media
        .dynamic()
        .write_item(
            &rt,
            "Player",
            &mpv.to_data(),
            &[Step::Field("position".into())],
            Data::Float(0.5),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    s.pump(&rt);
    assert_eq!(
        a.lock().unwrap().calls.len(),
        3,
        "no length, no position write"
    );
    b.media.release_field(players);
    s.shutdown();
    apps::set_config(None);
    let _ = std::fs::remove_dir_all(&root);
}

//! `media` against small zbus MPRIS players on a private bus: the
//! active player's track, play/pause through the service, the position
//! carried forward (ticking only while watched, never polled), seeking,
//! and the active player's choice as players come and go.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::{DynService as _, Store, media};
use support::*;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

#[derive(Debug)]
struct Track {
    status: String,
    title: String,
    position_us: i64,
    calls: Vec<String>,
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

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.0.lock().unwrap().status.clone()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let t = self.0.lock().unwrap();
        HashMap::from([
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
                OwnedValue::try_from(Value::from("file:///tmp/art.png")).unwrap(),
            ),
        ])
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.0.lock().unwrap().position_us
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
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
    let track = Arc::new(Mutex::new(Track {
        status: status.into(),
        title: title.into(),
        position_us,
        calls: Vec::new(),
    }));
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
        .action(&rt, "play_pause", None, &[])
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
    b.media.dynamic().action(&rt, "next", None, &[]).unwrap();
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

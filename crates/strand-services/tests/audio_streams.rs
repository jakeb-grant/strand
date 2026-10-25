//! (services-round) `audio.streams` against a private PipeWire (null
//! sinks, WirePlumber): a player (`pw-play`) and a recorder
//! (`pw-record`) are listed while a reader reads the field, with their
//! programs' names and directions; Strand's own peak meter is not; a
//! stream's volume and mute writes land where `wpctl` reads them; a
//! stream that ends leaves the list; and nothing is followed (no stream
//! node bound) while nobody reads it.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::time::{Duration, Instant};

use pipewire::PipeWire;
use strand_core::Runtime;
use strand_services::audio::{self, AudioStore, AudioStream, LevelTarget, StreamDirection};
use strand_services::{Builtin, Buses, Cells, Data, Services, Step, Store, ToData};

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

fn state(b: &Builtin, rt: &Runtime) -> AudioStore {
    b.audio.cells().snapshot(rt).unwrap()
}

fn field(name: &str) -> usize {
    AudioStore::FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap()
}

fn stream(b: &Builtin, rt: &Runtime, app: &str) -> Option<AudioStream> {
    state(b, rt).streams.into_iter().find(|s| s.app == app)
}

#[test]
fn streams_are_listed_while_read_and_written() {
    let Some(pw) = PipeWire::start("streams_are_listed_while_read_and_written") else {
        return;
    };
    pw.wait_for_defaults();
    audio::configure(Some(pw.config()));
    let wav = pw.dir.path().join("long.wav");
    pipewire::square_wav(&wav, 120.0, 0.01);
    let mut player = pw.play_with(
        &wav,
        "strand-sink-a",
        &["-P", "{ application.name = \"strand-player\" }"],
    );
    let mut recorder = pw
        .command("pw-record")
        .args(["--target", "strand-source"])
        .args(["-P", "{ application.name = \"strand-recorder\" }"])
        .arg(pw.dir.path().join("rec.wav"))
        .spawn()
        .expect("pw-record starts");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(pw.has_node("pw-play") || pw.run("pw-cli", &["ls", "Node"]).contains("strand-player")) {
        assert!(Instant::now() < deadline, "pw-play never made its stream");
        std::thread::sleep(Duration::from_millis(20));
    }

    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let dynamic = b.audio.dynamic();
    b.audio.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "the devices", || state(&b, &rt).sinks.len() == 2);
    // Read the devices, not the streams: none followed.
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert!(state(&b, &rt).streams.is_empty(), "followed while not read");

    b.audio.acquire_field(field("streams"));
    until(&rt, &s, "the player and the recorder", || {
        stream(&b, &rt, "strand-player").is_some() && stream(&b, &rt, "strand-recorder").is_some()
    });
    let played = stream(&b, &rt, "strand-player").unwrap();
    let recorded = stream(&b, &rt, "strand-recorder").unwrap();
    assert_eq!(played.direction, StreamDirection::Output);
    assert_eq!(recorded.direction, StreamDirection::Input);
    assert!(!played.icon.is_empty());
    assert_eq!(pw.volume(played.id).0, played.volume);

    // Strand's own meter is a capture stream too, and is left out.
    let tap = audio::tap_levels(LevelTarget::DefaultSink, |_| {});
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pw.has_node("strand-levels") {
        assert!(Instant::now() < deadline, "the meter never started");
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    let apps: Vec<String> = state(&b, &rt).streams.into_iter().map(|s| s.app).collect();
    assert_eq!(apps.len(), 2, "{apps:?}");
    drop(tap);

    // `st.volume = 0.5` and `st.muted = true` on an item.
    dynamic
        .write_item(
            &rt,
            "AudioStream",
            &played.to_data(),
            &[Step::Field("volume".into())],
            Data::Float(0.5),
        )
        .unwrap();
    until(&rt, &s, "the stream's volume in PipeWire", || {
        pw.volume(played.id).0 == 0.5
    });
    until(&rt, &s, "the stream's volume shown", || {
        stream(&b, &rt, "strand-player").is_some_and(|s| s.volume == 0.5)
    });
    dynamic
        .write_item(
            &rt,
            "AudioStream",
            &played.to_data(),
            &[Step::Field("muted".into())],
            Data::Bool(true),
        )
        .unwrap();
    until(&rt, &s, "the stream muted in PipeWire", || {
        pw.volume(played.id).1
    });
    until(&rt, &s, "the stream shown muted", || {
        stream(&b, &rt, "strand-player").is_some_and(|s| s.muted)
    });
    // An outside change is shown too.
    pw.wpctl(&["set-volume", &recorded.id.to_string(), "0.3"]);
    until(&rt, &s, "wpctl's stream volume", || {
        stream(&b, &rt, "strand-recorder").is_some_and(|s| s.volume == 0.3)
    });

    // A stream that ends leaves the list.
    let _ = recorder.kill();
    let _ = recorder.wait();
    until(&rt, &s, "the recorder gone", || {
        stream(&b, &rt, "strand-recorder").is_none()
    });

    // Nobody reads it: the stream nodes are let go, and listed as none.
    b.audio.release_field(field("streams"));
    until(&rt, &s, "the streams let go", || {
        state(&b, &rt).streams.is_empty()
    });
    assert!(stream(&b, &rt, "strand-player").is_none());

    let _ = player.kill();
    let _ = player.wait();
    b.audio.release(&rt);
    s.shutdown();
    audio::configure(None);
}

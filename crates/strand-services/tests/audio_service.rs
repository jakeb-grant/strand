//! The `audio` store on the service contract, against a private PipeWire
//! (null sinks, WirePlumber): its loop runs on the service's own thread;
//! outside changes arrive as reports; `audio.sink.volume` and item writes
//! land where `wpctl` reads them, a slider's writes never snap back;
//! `make_default()` moves the default; level meters run only for a tap
//! and only while a reader is visible; the service stops 5 s (logic
//! clock) after its last reader.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::time::{Duration, Instant};

use pipewire::PipeWire;
use strand_core::Runtime;
use strand_services::audio::{self, AudioDeviceAction, AudioStore, LevelTarget};
use strand_services::{Builtin, Buses, Cells, Data, STOP_GRACE, Services, Step, Store, ToData};

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

#[test]
fn the_audio_store_follows_and_writes_pipewire() {
    let Some(pw) = PipeWire::start("the_audio_store_follows_and_writes_pipewire") else {
        return;
    };
    pw.wait_for_defaults();
    audio::configure(Some(pw.config()));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let dynamic = b.audio.dynamic();

    b.audio.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "the devices", || {
        let a = state(&b, &rt);
        a.sink.name == "strand-sink-a"
            && a.source.name == "strand-source"
            && a.sinks.len() == 2
            && a.sinks.iter().any(|d| d.name == "strand-sink-b")
    });
    let a = state(&b, &rt);
    let sink_a = a.sink.id;
    let sink_b = a
        .sinks
        .iter()
        .find(|d| d.name == "strand-sink-b")
        .unwrap()
        .id;
    assert!(a.sink.default);

    // An outside change is a report.
    pw.wpctl(&["set-volume", &sink_a.to_string(), "0.4"]);
    until(&rt, &s, "wpctl's volume", || {
        state(&b, &rt).sink.volume == 0.4
    });

    // `audio.sink.volume = 0.25` lands in PipeWire.
    dynamic
        .write(
            &rt,
            field("sink"),
            &[Step::Field("volume".into())],
            Data::Float(0.25),
        )
        .unwrap();
    assert_eq!(state(&b, &rt).sink.volume, 0.25, "applied at once");
    until(&rt, &s, "the write in PipeWire", || {
        pw.volume(sink_a).0 == 0.25
    });

    // A slider: writes in a row, pumping in between. The store never
    // shows an older value than the last one written (no snap-back), and
    // ends on the last.
    let mut last = 0.25;
    for i in 1..=20 {
        let v = 0.25 + f64::from(i) * 0.02;
        dynamic
            .write(
                &rt,
                field("sink"),
                &[Step::Field("volume".into())],
                Data::Float(v),
            )
            .unwrap();
        last = v;
        for _ in 0..4 {
            s.pump(&rt);
            rt.flush();
            let shown = state(&b, &rt).sink.volume;
            assert!(
                (shown - last).abs() < 1e-9,
                "the slider snapped back to {shown} after writing {last}"
            );
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    until(&rt, &s, "the last slider write in PipeWire", || {
        (pw.volume(sink_a).0 - last).abs() < 0.006
    });
    // Settled: the echoes are spent, the value stays.
    std::thread::sleep(Duration::from_millis(300));
    s.pump(&rt);
    rt.flush();
    assert!((state(&b, &rt).sink.volume - last).abs() < 1e-9);

    // An item write: `s.volume = 0.6` for `s` in `audio.sinks`.
    let item = state(&b, &rt)
        .sinks
        .into_iter()
        .find(|d| d.id == sink_b)
        .unwrap();
    dynamic
        .write_item(
            &rt,
            "AudioDevice",
            &item.to_data(),
            &[Step::Field("volume".into())],
            Data::Float(0.6),
        )
        .unwrap();
    until(&rt, &s, "the item write in PipeWire", || {
        pw.volume(sink_b).0 == 0.6
    });
    until(&rt, &s, "the item's report", || {
        state(&b, &rt)
            .sinks
            .iter()
            .any(|d| d.id == sink_b && d.volume == 0.6)
    });

    // Mute through the default.
    dynamic
        .write(
            &rt,
            field("sink"),
            &[Step::Field("muted".into())],
            Data::Bool(true),
        )
        .unwrap();
    until(&rt, &s, "muted in PipeWire", || pw.volume(sink_a).1);
    until(&rt, &s, "muted icon", || {
        state(&b, &rt).sink.icon == "audio-volume-muted-symbolic"
    });

    // `make_default()` on an item.
    b.audio
        .act(&rt, AudioDeviceAction::MakeDefault { item: item.clone() })
        .unwrap();
    until(&rt, &s, "the new default", || {
        state(&b, &rt).sink.id == sink_b
    });

    // Levels: only for a tap, only while a reader is visible.
    assert!(!pw.has_node("strand-levels"), "no tap, no meter");
    let tap = audio::tap_levels(LevelTarget::DefaultSink, |_| {});
    let wait_node = |present: bool, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pw.has_node("strand-levels") != present {
            assert!(Instant::now() < deadline, "{what}");
            s.pump(&rt);
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait_node(true, "the tapped meter never started");
    // The last reader goes (invisible, inside the 5 s grace): the meter
    // stops while the service still runs.
    b.audio.release(&rt);
    wait_node(false, "a hidden service kept its meter");
    assert!(b.audio.running());
    b.audio.acquire(&rt);
    wait_node(true, "the meter did not come back with a visible reader");
    drop(tap);
    wait_node(false, "the meter outlived its tap");

    // 5 s after the last reader, the service (and its thread) stops.
    b.audio.release(&rt);
    rt.tick(STOP_GRACE + Duration::from_millis(1));
    assert!(!b.audio.running());
    s.shutdown();
    audio::configure(None);
}

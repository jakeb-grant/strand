//! The `audio` store on the service contract, against a private PipeWire
//! (null sinks, WirePlumber): its loop runs on the service's own thread;
//! outside changes arrive as reports; `audio.sink.volume` and item writes
//! land where `wpctl` reads them, a slider's writes never snap back;
//! `make_default()` moves the default; nothing wakes the host while
//! nothing changes; level meters run only for a tap and only while a
//! reader is visible; the service stops 5 s (logic clock) after its last
//! reader.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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
    let wakes = Arc::new(AtomicU32::new(0));
    let w = wakes.clone();
    let s = Services::new(&rt, Buses::none(), move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let b = Builtin::register(&s, &rt);
    let dynamic = b.audio.dynamic();

    let clients_before = clients(&pw);
    assert_eq!(threads_named("strand-audio"), 0);
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

    // Idle: nothing changes, nothing wakes the logic thread (no polling
    // on the service's thread either).
    let before = wakes.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        wakes.load(Ordering::SeqCst),
        before,
        "an idle PipeWire woke the host"
    );

    // A volume write, then at once a write that changes nothing (the
    // "unmute when the slider moves" handler on an unmuted sink): the
    // no-op's answer waits for the volume's, so the cell settles on the
    // volume PipeWire has, not on the state before it.
    assert!(!state(&b, &rt).sink.muted);
    let sink = |v: Data, leaf: &str| {
        dynamic
            .write(&rt, field("sink"), &[Step::Field(leaf.into())], v)
            .unwrap();
    };
    sink(Data::Float(0.7), "volume");
    sink(Data::Bool(false), "muted");
    let deadline = Instant::now() + audio::ANSWER_WAIT * 2;
    while Instant::now() < deadline {
        s.pump(&rt);
        rt.flush();
        let a = state(&b, &rt);
        assert_eq!(
            (a.sink.volume, a.sink.muted),
            (0.7, false),
            "the no-op write's answer moved the cell off the volume written"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(pw.volume(sink_a), (0.7, false));

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

    // 5 s after the last reader, the service stops: its thread ends and
    // its PipeWire client goes.
    b.audio.release(&rt);
    let mut clock = STOP_GRACE + Duration::from_millis(1);
    rt.tick(clock);
    assert!(!b.audio.running());
    gone(&pw, clients_before);

    // A write to the stopped service starts it, lands, and the service
    // stops again 5 s later; then a read starts it anew.
    let starts = b.audio.starts();
    sink(Data::Float(0.33), "volume");
    assert!(b.audio.running(), "the write started the service");
    until(&rt, &s, "the write that started the service", || {
        pw.volume_of("@DEFAULT_AUDIO_SINK@").0 == 0.33
    });
    clock += STOP_GRACE + Duration::from_millis(1);
    rt.tick(clock);
    assert!(!b.audio.running());
    gone(&pw, clients_before);
    b.audio.acquire(&rt);
    until(&rt, &s, "the restarted service's devices", || {
        state(&b, &rt).sink.volume == 0.33
    });
    assert_eq!(b.audio.starts(), starts + 2);
    b.audio.release(&rt);
    clock += STOP_GRACE + Duration::from_millis(1);
    rt.tick(clock);
    assert!(!b.audio.running());
    gone(&pw, clients_before);
    s.shutdown();
    audio::configure(None);
}

/// This process's threads named `name`.
fn threads_named(name: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|comm| comm.trim_end() == name)
        .count()
}

/// The private PipeWire's clients.
fn clients(pw: &PipeWire) -> usize {
    pw.run("pw-cli", &["ls", "Client"])
        .lines()
        .filter(|l| l.contains("PipeWire:Interface:Client"))
        .count()
}

/// Waits until the `strand-audio` thread has ended and PipeWire has as
/// many clients as `before`.
fn gone(pw: &PipeWire, before: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while threads_named("strand-audio") > 0 || clients(pw) != before {
        assert!(
            Instant::now() < deadline,
            "after the stop: {} strand-audio threads, {} PipeWire clients (before: {before})",
            threads_named("strand-audio"),
            clients(pw)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

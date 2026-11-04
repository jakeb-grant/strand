//! (audio-routing) `st.move_to(dev)` on the `audio` store against a
//! private PipeWire (null sinks, WirePlumber): a program's stream that
//! follows the default is moved to another sink through the session
//! manager (the `default` metadata's `target.object`, as `pactl
//! move-sink-input` writes it), stays there when the default changes,
//! and follows the default again once moved onto the default; `st.device`
//! shows where it plays at each step, as `pw-link` does.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::time::{Duration, Instant};

use pipewire::PipeWire;
use strand_core::Runtime;
use strand_services::audio::{self, AudioDevice, AudioDeviceAction, AudioStore, AudioStream};
use strand_services::{Builtin, Buses, Cells, Services, Store};

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, mut cond: impl FnMut() -> bool) {
    strand_test_support::wait::until(
        what,
        Duration::from_secs(10),
        Duration::from_millis(5),
        || {
            s.pump(rt);
            rt.flush();
            cond()
        },
    );
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

fn sink(b: &Builtin, rt: &Runtime, name: &str) -> AudioDevice {
    state(b, rt)
        .sinks
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no sink {name}"))
}

/// Pumps for `ms` without waiting for anything.
fn settle(rt: &Runtime, s: &Services, ms: u64) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(ms) {
        s.pump(rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_stream_moves_and_follows_the_default_again() {
    let Some(pw) = PipeWire::start("a_stream_moves_and_follows_the_default_again") else {
        return;
    };
    pw.wait_for_defaults();
    audio::configure(Some(pw.config()));
    let wav = pw.dir.path().join("long.wav");
    pipewire::square_wav(&wav, 120.0, 0.01);
    let app = "strand-mover";
    let mut player = pw.play_following(&wav, app);

    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.audio.acquire(&rt);
    b.audio.acquire_field(field("streams"));
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "the devices", || {
        let a = state(&b, &rt);
        a.sinks.len() == 2 && a.sink.name == "strand-sink-a"
    });
    let a = sink(&b, &rt, "strand-sink-a");
    let sink_b = sink(&b, &rt, "strand-sink-b");

    // It plays to the default, as pw-link shows it.
    until(&rt, &s, "the player on sink a", || {
        stream(&b, &rt, app).is_some_and(|st| st.device == Some(a.id))
    });
    assert_eq!(pw.linked_to(app), ["strand-sink-a"]);
    let item = stream(&b, &rt, app).unwrap();
    let node = pw.node_of_serial(item.id);
    assert_eq!(pw.metadata_of(node, "target.object"), None);

    // `st.move_to(b)`: what `pactl move-sink-input` writes, then the
    // session manager relinks it.
    b.audio
        .act(
            &rt,
            AudioDeviceAction::MoveTo {
                item: item.clone(),
                device: sink_b.clone(),
            },
        )
        .unwrap();
    let serial_b = pw.serial_of(sink_b.id).to_string();
    until(&rt, &s, "target.object naming sink b", || {
        pw.metadata_of(node, "target.object").as_deref() == Some(serial_b.as_str())
    });
    assert!(
        pw.metadata()
            .lines()
            .any(|l| l.contains(&format!("id:{node} "))
                && l.contains("key:'target.object'")
                && l.contains("type:'Spa:Id'")),
        "{}",
        pw.metadata()
    );
    until(&rt, &s, "the player on sink b", || {
        stream(&b, &rt, app).is_some_and(|st| st.device == Some(sink_b.id))
    });
    assert_eq!(pw.linked_to(app), ["strand-sink-b"]);

    // A new default: the moved stream stays where it was put.
    pw.create_sink("strand-sink-c");
    until(&rt, &s, "sink c", || {
        state(&b, &rt)
            .sinks
            .iter()
            .any(|d| d.name == "strand-sink-c")
    });
    let c = sink(&b, &rt, "strand-sink-c");
    pw.wpctl(&["set-default", &c.id.to_string()]);
    until(&rt, &s, "sink c the default", || {
        state(&b, &rt).sink.name == "strand-sink-c"
    });
    settle(&rt, &s, 600);
    assert_eq!(
        pw.linked_to(app),
        ["strand-sink-b"],
        "a pinned stream moved"
    );
    assert_eq!(stream(&b, &rt, app).unwrap().device, Some(sink_b.id));

    // Moved onto the default: the target is forgotten, and it plays there.
    let item = stream(&b, &rt, app).unwrap();
    let c = sink(&b, &rt, "strand-sink-c");
    b.audio
        .act(
            &rt,
            AudioDeviceAction::MoveTo {
                item,
                device: c.clone(),
            },
        )
        .unwrap();
    until(&rt, &s, "target.object cleared", || {
        pw.metadata_of(node, "target.object").is_none()
    });
    until(&rt, &s, "the player on sink c", || {
        stream(&b, &rt, app).is_some_and(|st| st.device == Some(c.id))
    });
    assert_eq!(pw.linked_to(app), ["strand-sink-c"]);

    // It follows the default again.
    pw.wpctl(&["set-default", &a.id.to_string()]);
    until(&rt, &s, "sink a the default", || {
        state(&b, &rt).sink.name == "strand-sink-a"
    });
    until(
        &rt,
        &s,
        "the player following the default to sink a",
        || stream(&b, &rt, app).is_some_and(|st| st.device == Some(a.id)),
    );
    assert_eq!(pw.linked_to(app), ["strand-sink-a"]);

    // An item of a stream that ended moves nothing: another program's
    // stream holds nothing of it, and nothing is written.
    let item = stream(&b, &rt, app).unwrap();
    let _ = player.kill();
    let _ = player.wait();
    until(&rt, &s, "the player gone", || {
        stream(&b, &rt, app).is_none()
    });
    b.audio
        .act(
            &rt,
            AudioDeviceAction::MoveTo {
                item,
                device: sink_b.clone(),
            },
        )
        .unwrap();
    settle(&rt, &s, 300);
    assert!(
        !pw.metadata().contains("target.object"),
        "{}",
        pw.metadata()
    );

    b.audio.release_field(field("streams"));
    b.audio.release(&rt);
    s.shutdown();
    audio::configure(None);
}

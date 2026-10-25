//! (services-round) `audio.levels(dev, bars)` against a private PipeWire
//! (null sinks, WirePlumber): evaluating it starts the device's meter,
//! a square wave playing on the device shows in its bars and in
//! `audio.meters`, the bars come in the number asked, a device that is
//! not there reads quiet, and once nothing evaluates it for
//! `LEVELS_LEASE` the meter stops and its item leaves `audio.meters`;
//! a quiet device's meter, which sends no reading, stops once the scope
//! that evaluated it goes.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::time::{Duration, Instant};

use pipewire::{PipeWire, square_wav};
use strand_core::Runtime;
use strand_services::audio::{self, AudioStore, LEVELS_LEASE};
use strand_services::{Builtin, Buses, Cells, Data, DynService, Services, ToData};

/// The devices asked about are the process's: one test at a time, each
/// letting its readers go with its runtime.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, mut cond: impl FnMut() -> bool) {
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

/// `audio.levels(dev, bars)` as the language calls it.
fn levels(rt: &Runtime, d: &dyn DynService, dev: Data, bars: i64) -> Vec<f64> {
    match d.call(rt, "levels", &[dev, Data::Int(bars)]) {
        Some(Ok(Data::List(v))) => v
            .into_iter()
            .map(|x| match x {
                Data::Float(f) => f,
                other => panic!("a bar of {other:?}"),
            })
            .collect(),
        other => panic!("audio.levels: {other:?}"),
    }
}

#[test]
fn levels_run_a_meter_while_evaluated() {
    let _one = serial();
    let Some(pw) = PipeWire::start("levels_run_a_meter_while_evaluated") else {
        return;
    };
    pw.wait_for_defaults();
    audio::configure(Some(pw.config()));
    let wav = pw.dir.path().join("square.wav");
    square_wav(&wav, 30.0, 0.5);

    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let dynamic = b.audio.dynamic();
    b.audio.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    until(&rt, &s, "the devices", || state(&b, &rt).sinks.len() == 2);
    let a = state(&b, &rt)
        .sinks
        .into_iter()
        .find(|d| d.name == "strand-sink-a")
        .unwrap();

    // Not evaluated: no meter.
    assert!(!pw.has_node("strand-levels"));
    // A device that is not there (the schema's default record) reads
    // quiet, and starts nothing.
    let none = audio::AudioDevice::default().to_data();
    assert_eq!(levels(&rt, dynamic.as_ref(), none, 4), [0.0; 4]);

    let mut player = pw.play(&wav, "strand-sink-a");
    let mut shown = Vec::new();
    until(&rt, &s, "bars with sound", || {
        shown = levels(&rt, dynamic.as_ref(), a.to_data(), 8);
        shown.iter().any(|&v| v > 0.0)
    });
    assert_eq!(shown.len(), 8);
    assert!(shown.iter().all(|v| (0.0..=1.0).contains(v)), "{shown:?}");
    assert!(pw.has_node("strand-levels"));
    let meters = state(&b, &rt).meters;
    assert_eq!(meters.len(), 1, "{meters:?}");
    assert_eq!(meters[0].device, a.id);
    // The same reading, as more bars than bands, and by id.
    let wide = levels(&rt, dynamic.as_ref(), Data::Int(i64::from(a.id)), 200);
    assert_eq!(wide.len(), 200);
    assert!(wide.iter().any(|&v| v > 0.0));
    assert!(levels(&rt, dynamic.as_ref(), a.to_data(), 0).is_empty());

    // Nobody evaluates it any more: the lease runs out and the meter
    // stops, though the service stays visible.
    let deadline = Instant::now() + LEVELS_LEASE * 5;
    until(&rt, &s, "the meter let go", || {
        assert!(Instant::now() < deadline, "the lease never ran out");
        state(&b, &rt).meters.is_empty() && !pw.has_node("strand-levels")
    });
    let _ = player.kill();
    let _ = player.wait();
    b.audio.release(&rt);
    s.shutdown();
    rt.shutdown();
}

/// A device asked about while quiet sends no reading, so no reading
/// lease runs: its meter stops `LEVELS_LEASE` after the scope whose
/// evaluation asked for it goes, though the service stays visible.
#[test]
fn a_quiet_devices_meter_stops_with_its_reader() {
    let _one = serial();
    let Some(pw) = PipeWire::start("a_quiet_devices_meter_stops_with_its_reader") else {
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
    until(&rt, &s, "the devices", || state(&b, &rt).sinks.len() == 2);
    let quiet = state(&b, &rt)
        .sinks
        .into_iter()
        .find(|d| d.name == "strand-sink-b")
        .unwrap();
    let (reader, bars) = rt.scope(|rt| levels(rt, dynamic.as_ref(), quiet.to_data(), 4));
    assert_eq!(bars, [0.0; 4]);
    until(&rt, &s, "the meter runs", || pw.has_node("strand-levels"));
    // Still read: it keeps running, quiet as it is.
    std::thread::sleep(LEVELS_LEASE * 2);
    s.pump(&rt);
    assert!(pw.has_node("strand-levels"), "read, so metered");
    // Its reader goes: the meter stops within the lease.
    reader.dispose(&rt);
    let deadline = Instant::now() + LEVELS_LEASE * 5;
    until(&rt, &s, "the meter let go", || {
        assert!(Instant::now() < deadline, "the meter never stopped");
        !pw.has_node("strand-levels")
    });
    b.audio.release(&rt);
    s.shutdown();
    rt.shutdown();
}

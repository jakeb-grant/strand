//! The idle budget for audio: with nothing changing in PipeWire, the
//! `strand-pipewire` thread and the threads libpipewire starts in our
//! process are not woken at all, also with a peak meter on a sink that
//! plays nothing. Alone in its binary so no other test's threads share
//! these names.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::collections::HashMap;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use pipewire::PipeWire;
use strand_services::audio::{Audio, AudioAction, AudioChange, DeviceRef, LevelTarget};

/// Context switches (voluntary + involuntary) of this process's threads
/// whose name `wanted` accepts, by thread id.
fn switches(wanted: impl Fn(&str) -> bool) -> HashMap<String, (String, u64)> {
    let mut out = HashMap::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let dir = task.path();
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let comm = comm.trim().to_string();
        if !wanted(&comm) {
            continue;
        }
        let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
        let n: u64 = status
            .lines()
            .filter(|l| l.contains("ctxt_switches"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum();
        out.insert(task.file_name().to_string_lossy().into_owned(), (comm, n));
    }
    out
}

/// The audio thread and every thread libpipewire started (its data loop).
fn audio_threads(name: &str) -> bool {
    name == "strand-pipewire" || name.starts_with("data-loop") || name.starts_with("pw-")
}

fn assert_idle(what: &str, rx: &std::sync::mpsc::Receiver<Vec<AudioChange>>) {
    // Let everything settle, then measure.
    std::thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
    let before = switches(audio_threads);
    assert!(
        before.values().any(|(n, _)| n == "strand-pipewire"),
        "the audio thread is not running: {before:?}"
    );
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    let after = switches(audio_threads);
    for (tid, (name, n)) in &before {
        let m = after.get(tid).map_or(*n, |(_, m)| *m);
        assert_eq!(
            m - n,
            0,
            "{what}: {name} ({tid}) woke {} times in {:?} of idle",
            m - n,
            start.elapsed()
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "{what}: nothing is published while idle"
    );
}

#[test]
fn an_idle_pipewire_wakes_no_audio_thread() {
    let Some(pw) = PipeWire::start("an_idle_pipewire_wakes_no_audio_thread") else {
        return;
    };
    let (tx, rx) = channel::<Vec<AudioChange>>();
    let audio = Audio::spawn(pw.config(), move |b| {
        let _ = tx.send(b);
    })
    .unwrap();
    let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(first.contains(&AudioChange::Connected(true)), "{first:?}");
    eprintln!(
        "threads: {:?}",
        switches(|_| true)
            .values()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
    );

    assert_idle("connected", &rx);

    // A meter on the default sink, which plays nothing: passive, so the
    // sink stays suspended and the meter costs no wakeups.
    audio.set_levels([LevelTarget::DefaultSink]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pw.has_node("strand-levels") {
        assert!(Instant::now() < deadline, "the meter never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_idle("a meter on a silent sink", &rx);

    // A real change still gets through (the thread was asleep, not dead).
    audio
        .request(AudioAction::SetMuted(DeviceRef::DefaultSink, true))
        .wait()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let batch = rx.recv_timeout(left).expect("the mute arrives");
        if batch
            .iter()
            .any(|c| matches!(c, AudioChange::Sink(Some(d)) if d.muted))
        {
            break;
        }
    }
}

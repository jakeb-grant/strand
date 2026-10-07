//! The audio service against a private PipeWire (a null sink tier, as
//! design.md's "Testing" table has it): devices, volume, mute and the
//! default arrive from `wpctl`; the service's writes land where `wpctl`
//! reads them; a daemon restart reconnects; peak meters run only while
//! asked for.

mod pipewire;

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use pipewire::{PipeWire, square_wav};
use strand_services::audio::{
    Audio, AudioAction, AudioChange, AudioConfig, AudioDevice, AudioError, DeviceRef, LevelTarget,
    Mirror,
};

/// The service and a mirror of what it sent.
struct Watch {
    audio: Audio,
    rx: Receiver<Vec<AudioChange>>,
    mirror: Mirror,
    batches: usize,
    levels: Vec<(Instant, f32)>,
}

impl Watch {
    fn start(config: AudioConfig) -> Watch {
        let (tx, rx) = channel();
        let audio = Audio::spawn(config, move |batch| {
            assert!(!batch.is_empty(), "the service sent an empty batch");
            let _ = tx.send(batch);
        })
        .expect("the audio thread starts");
        Watch {
            audio,
            rx,
            mirror: Mirror::default(),
            batches: 0,
            levels: Vec::new(),
        }
    }

    fn take(&mut self, batch: Vec<AudioChange>) {
        self.batches += 1;
        for c in &batch {
            self.mirror
                .apply(c)
                .unwrap_or_else(|e| panic!("inconsistent diff {c:?}: {e}"));
            if let AudioChange::Levels(l) = c {
                self.levels.push((Instant::now(), l.peak()));
            }
        }
    }

    /// Applies what has arrived, without waiting.
    fn poll(&mut self) {
        while let Ok(b) = self.rx.try_recv() {
            self.take(b);
        }
    }

    /// Waits (at most `secs`) until `done` holds for the mirror.
    fn until(&mut self, secs: u64, what: &str, done: impl Fn(&Mirror) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        self.poll();
        while !done(&self.mirror) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(b) => self.take(b),
                Err(_) => panic!("timed out waiting for {what}: {:#?}", self.mirror),
            }
        }
    }

    fn sink(&self, name: &str) -> AudioDevice {
        self.mirror
            .sink_named(name)
            .cloned()
            .unwrap_or_else(|| panic!("no sink {name}: {:#?}", self.mirror))
    }

    fn act(&self, action: AudioAction) -> Result<(), AudioError> {
        self.audio.request(action).wait()
    }
}

/// Connected, with both sinks, the source and both defaults known.
fn ready(m: &Mirror) -> bool {
    m.connected
        && m.sink_named("strand-sink-a").is_some()
        && m.sink_named("strand-sink-b").is_some()
        && m.source_named("strand-source").is_some()
        && m.sink.is_some()
        && m.source.is_some()
}

#[test]
fn devices_volume_mute_and_the_default_arrive() {
    let Some(pw) = PipeWire::start("devices_volume_mute_and_the_default_arrive") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    // The first batch holds every field, `Connected` first.
    assert_eq!(w.batches, 1, "the first state is one batch");
    let a = w.sink("strand-sink-a");
    let b = w.sink("strand-sink-b");
    assert_eq!(a.description, "Strand Sink A");
    assert_eq!(a.channels, 2);
    assert_eq!((a.volume, a.muted), (1.0, false));
    assert_eq!(a.icon(), "audio-volume-high-symbolic");
    let source = w.mirror.source_named("strand-source").cloned().unwrap();
    assert!(source.default);
    // WirePlumber picks the sink with the higher priority.session.
    assert_eq!(w.mirror.sink.as_ref().map(|d| d.id), Some(a.id));
    assert!(a.default && !b.default);
    assert!(w.mirror.sinks.iter().all(|(k, d)| *k == i64::from(d.id)));

    // Volume, on the cubic scale wpctl uses.
    pw.wpctl(&["set-volume", &a.id.to_string(), "0.5"]);
    w.until(5, "sink a at 0.5", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 0.5)
    });
    // The default sink's copy follows.
    assert_eq!(w.mirror.sink.as_ref().map(|d| d.volume), Some(0.5));
    assert_eq!(
        w.sink("strand-sink-a").icon(),
        "audio-volume-medium-symbolic"
    );

    // Unbalanced channels read as the loudest, as wpctl shows them.
    pw.run(
        "pw-cli",
        &[
            "s",
            &a.id.to_string(),
            "Props",
            "{ channelVolumes: [ 0.008, 0.001 ] }",
        ],
    );
    w.until(5, "sink a at 0.2", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| (d.volume - 0.2).abs() < 1e-6)
    });
    assert_eq!(pw.volume(a.id).0, 0.2);

    // Mute, on the other sink.
    pw.wpctl(&["set-mute", &b.id.to_string(), "1"]);
    w.until(5, "sink b muted", |m| {
        m.sink_named("strand-sink-b").is_some_and(|d| d.muted)
    });
    assert_eq!(
        w.sink("strand-sink-b").icon(),
        "audio-volume-muted-symbolic"
    );

    // The default moves to b (wpctl writes default.configured.audio.sink;
    // WirePlumber then sets default.audio.sink).
    pw.wpctl(&["set-default", &b.id.to_string()]);
    w.until(5, "b as the default", |m| {
        m.sink.as_ref().is_some_and(|d| d.name == "strand-sink-b")
    });
    assert!(w.sink("strand-sink-b").default);
    assert!(!w.sink("strand-sink-a").default);
    assert!(w.mirror.sink.as_ref().is_some_and(|d| d.muted));

    // A device that leaves is removed; one that comes is added.
    pw.run(
        "pw-cli",
        &[
            "create-node",
            "adapter",
            "{ factory.name=support.null-audio-sink node.name=strand-sink-c \
             node.description=\"Strand Sink C\" media.class=Audio/Sink \
             audio.position=[FL FR] object.linger=true }",
        ],
    );
    w.until(5, "sink c", |m| m.sink_named("strand-sink-c").is_some());
    let c = w.sink("strand-sink-c");
    assert_eq!(c.description, "Strand Sink C");
    pw.run("pw-cli", &["destroy", &c.id.to_string()]);
    w.until(5, "sink c gone", |m| {
        m.sink_named("strand-sink-c").is_none()
    });
}

#[test]
fn writes_land_where_wpctl_reads_them() {
    let Some(pw) = PipeWire::start("writes_land_where_wpctl_reads_them") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    let b = w.sink("strand-sink-b");
    assert!(a.default);

    // `audio.sink.volume = 0.37`: every channel, on the cubic scale.
    w.act(AudioAction::SetVolume(DeviceRef::DefaultSink, 0.37))
        .unwrap();
    // The echo reads back exactly as written.
    w.until(5, "a at 0.37", |m| {
        m.sink.as_ref().is_some_and(|d| d.volume == 0.37)
    });
    assert_eq!(pw.volume(a.id), (0.37, false));
    let props = pw.run("pw-dump", &[&a.id.to_string()]);
    let lin = 0.37f64.powi(3);
    assert!(
        props.matches(&format!("{:.6}", lin)[..6]).count() >= 2,
        "both channels at {lin}: {props}"
    );

    // Out of range is clamped; not a number is refused.
    w.act(AudioAction::SetVolume(DeviceRef::Id(a.id), 1.7))
        .unwrap();
    w.until(5, "a at 1", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 1.0)
    });
    assert_eq!(pw.volume(a.id).0, 1.0);
    w.act(AudioAction::SetVolume(DeviceRef::Id(a.id), -1.0))
        .unwrap();
    w.until(5, "a at 0", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 0.0)
    });
    assert!(matches!(
        w.act(AudioAction::SetVolume(DeviceRef::Id(a.id), f64::NAN)),
        Err(AudioError::InvalidVolume(v)) if v.is_nan()
    ));

    // A volume another program amplified past 1 is not raised further,
    // but can be lowered.
    pw.run(
        "pw-cli",
        &[
            "s",
            &b.id.to_string(),
            "Props",
            "{ channelVolumes: [ 3.375, 3.375 ] }",
        ],
    );
    w.until(5, "b at 1.5", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| (d.volume - 1.5).abs() < 1e-6)
    });
    assert_eq!(
        w.sink("strand-sink-b").icon(),
        "audio-volume-overamplified-symbolic"
    );
    w.act(AudioAction::SetVolume(DeviceRef::Id(b.id), 2.0))
        .unwrap();
    w.act(AudioAction::SetVolume(DeviceRef::Id(b.id), 1.25))
        .unwrap();
    w.until(5, "b at 1.25", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| d.volume == 1.25)
    });

    // `audio.sink.muted = true`, then on the source.
    w.act(AudioAction::SetMuted(DeviceRef::DefaultSink, true))
        .unwrap();
    w.until(5, "a muted", |m| m.sink.as_ref().is_some_and(|d| d.muted));
    assert!(pw.volume(a.id).1);
    w.act(AudioAction::SetMuted(DeviceRef::DefaultSource, true))
        .unwrap();
    w.until(5, "the source muted", |m| {
        m.source.as_ref().is_some_and(|d| d.muted)
    });
    let source = w.mirror.source.clone().unwrap();
    assert!(pw.volume(source.id).1);
    assert_eq!(source.icon(), "microphone-sensitivity-muted-symbolic");

    // `dev.make_default()`.
    w.act(AudioAction::MakeDefault(DeviceRef::Id(b.id)))
        .unwrap();
    w.until(5, "b as the default", |m| {
        m.sink.as_ref().is_some_and(|d| d.id == b.id)
    });
    let status = pw.wpctl(&["inspect", "@DEFAULT_AUDIO_SINK@"]);
    assert!(status.contains("strand-sink-b"), "{status}");
    assert!(pw.metadata().contains("default.configured.audio.sink"));
    // Writes to the default follow it.
    w.act(AudioAction::SetVolume(DeviceRef::DefaultSink, 0.6))
        .unwrap();
    w.until(5, "b at 0.6", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| d.volume == 0.6)
    });
    assert_eq!(w.sink("strand-sink-a").volume, 0.0);

    // Unknown devices.
    assert_eq!(
        w.act(AudioAction::SetMuted(DeviceRef::Id(99_999), true)),
        Err(AudioError::UnknownDevice(DeviceRef::Id(99_999)))
    );
    assert_eq!(
        w.act(AudioAction::MakeDefault(DeviceRef::Id(99_999))),
        Err(AudioError::UnknownDevice(DeviceRef::Id(99_999)))
    );
}

#[test]
fn a_daemon_restart_reconnects() {
    let Some(mut pw) = PipeWire::start("a_daemon_restart_reconnects") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let before = w.sink("strand-sink-a");

    pw.kill_daemon();
    w.until(5, "the connection lost", |m| !m.connected);
    // The devices stay while it is away.
    assert!(w.mirror.sink_named("strand-sink-a").is_some());
    assert_eq!(
        w.act(AudioAction::SetMuted(DeviceRef::DefaultSink, true)),
        Err(AudioError::NotConnected)
    );

    // Away long enough that the backoff waits seconds: the socket's
    // appearance (inotify) brings it back, not a timer.
    std::thread::sleep(Duration::from_millis(3500));
    pw.start_daemon();
    let back = Instant::now();
    w.until(10, "reconnected", ready);
    assert!(
        back.elapsed() < Duration::from_secs(2),
        "reconnected {:?} after the socket came back",
        back.elapsed()
    );
    let after = w.sink("strand-sink-a");
    assert_eq!(after.description, before.description);
    // And it works.
    w.act(AudioAction::SetVolume(DeviceRef::Id(after.id), 0.5))
        .unwrap();
    w.until(5, "a at 0.5", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 0.5)
    });
    assert_eq!(pw.volume(after.id).0, 0.5);

    // The session manager alone restarting: the defaults come back with
    // its new metadata.
    pw.kill_wireplumber();
    w.until(5, "no default without a session manager", |m| {
        m.sink.is_none()
    });
    pw.start_wireplumber();
    w.until(10, "the default back", |m| m.sink.is_some());
}

#[test]
fn it_starts_without_pipewire_and_connects_when_it_appears() {
    let Some(mut pw) = PipeWire::start("it_starts_without_pipewire_and_connects_when_it_appears")
    else {
        return;
    };
    pw.kill_daemon();
    let mut w = Watch::start(pw.config());
    // The first batch says: nothing, not connected.
    let first = w.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first[0], AudioChange::Connected(false));
    w.take(first);
    assert!(w.mirror.sinks.is_empty() && w.mirror.sink.is_none());
    pw.start_daemon();
    w.until(10, "connected", ready);
}

#[test]
fn peak_meters_run_only_while_asked_for() {
    let Some(pw) = PipeWire::start("peak_meters_run_only_while_asked_for") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    let wav = pw.dir.path().join("square.wav");
    square_wav(&wav, 30.0, 0.5);

    // Not asked for: no stream.
    assert!(!pw.has_node("strand-levels"));

    w.audio.set_levels([LevelTarget::DefaultSink]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pw.has_node("strand-levels") {
        assert!(Instant::now() < deadline, "the meter stream never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Nothing plays: the passive meter keeps the sink idle and sends
    // nothing.
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    assert!(w.levels.is_empty(), "levels while silent: {:?}", w.levels);

    let mut player = pw.play(&wav, "strand-sink-a");
    w.until(10, "a reading of the square wave", |m| {
        m.levels(LevelTarget::DefaultSink)
            .is_some_and(|l| (l.peak() - 0.5).abs() < 0.01)
    });
    let l = w.mirror.levels(LevelTarget::DefaultSink).cloned().unwrap();
    assert_eq!(l.device, a.id);
    assert_eq!(l.peaks.len(), 2);

    // Stopped: the stream goes and the readings stop.
    w.audio.set_levels([]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while pw.has_node("strand-levels") {
        assert!(Instant::now() < deadline, "the meter stream stayed");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    w.poll();
    let stopped = Instant::now();
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    assert!(
        w.levels.iter().all(|(t, _)| *t < stopped),
        "readings after the meter stopped"
    );

    // A meter on the default follows it: b becomes the default while a
    // still plays; b is silent.
    w.audio
        .set_levels([LevelTarget::DefaultSink, LevelTarget::Device(a.id)]);
    w.until(10, "a's own meter reading", |m| {
        m.levels(LevelTarget::Device(a.id))
            .is_some_and(|l| l.peak() > 0.4)
    });
    let b = w.sink("strand-sink-b");
    w.act(AudioAction::MakeDefault(DeviceRef::Id(b.id)))
        .unwrap();
    w.until(10, "the default meter on b, quiet", |m| {
        m.sink.as_ref().is_some_and(|d| d.id == b.id)
            && m.levels(LevelTarget::DefaultSink)
                .is_none_or(|l| l.device == b.id || l.peak() == 0.0)
    });
    let _ = player.kill();
    let _ = player.wait();
    // a falls silent: one quiet reading, then nothing.
    w.until(10, "a quiet", |m| {
        m.levels(LevelTarget::Device(a.id))
            .is_some_and(|l| l.peak() == 0.0)
    });
    std::thread::sleep(Duration::from_millis(300));
    w.poll();
    let quiet = w.levels.len();
    std::thread::sleep(Duration::from_millis(700));
    w.poll();
    assert_eq!(w.levels.len(), quiet, "readings while silent");
}

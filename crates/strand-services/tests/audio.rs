//! The audio service against a private PipeWire (a null sink tier, as
//! design.md's "Testing" table has it): devices, volume, mute and the
//! default arrive from `wpctl`; the service's writes land where `wpctl`
//! reads them; a daemon restart reconnects; peak meters run only while
//! asked for, never leave a stale level and send at most 60 readings a
//! second; a test tone's spectrum peaks in its band, and silence runs no
//! FFT.

#![cfg(feature = "pipewire")]

#[path = "pipewire/meta.rs"]
mod meta;
mod pipewire;

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use meta::MetaClient;
use pipewire::{PipeWire, sine_wav, square_wav};
use strand_services::audio::{
    Audio, AudioAction, AudioChange, AudioConfig, AudioDevice, AudioError, DeviceRef, FRAME,
    LevelTarget, Levels, Mirror, REREAD, SETTLE, UNANSWERED, spectrum,
};

/// The service and a mirror of what it sent.
struct Watch {
    audio: Audio,
    rx: Receiver<Vec<AudioChange>>,
    mirror: Mirror,
    batches: usize,
    levels: Vec<(Instant, Levels)>,
    /// Every sink volume as each batch left it, by name.
    volumes: Vec<(String, f64)>,
    /// While set, every batch that leaves no default or an empty list is
    /// recorded in `gaps`.
    guard: bool,
    gaps: Vec<String>,
    /// The `strand-pipewire` thread's id (`/proc/thread-self`), from its
    /// first batch.
    tid: std::sync::Arc<std::sync::OnceLock<String>>,
}

impl Watch {
    fn start(config: AudioConfig) -> Watch {
        let (tx, rx) = channel();
        let tid = std::sync::Arc::new(std::sync::OnceLock::new());
        let thread_tid = tid.clone();
        let audio = Audio::spawn(config, move |batch| {
            assert!(!batch.is_empty(), "the service sent an empty batch");
            thread_tid.get_or_init(|| {
                std::fs::read_link("/proc/thread-self")
                    .ok()
                    .and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
                    .unwrap_or_default()
            });
            let _ = tx.send(batch);
        })
        .expect("the audio thread starts");
        Watch {
            audio,
            rx,
            mirror: Mirror::default(),
            batches: 0,
            levels: Vec::new(),
            volumes: Vec::new(),
            guard: false,
            gaps: Vec::new(),
            tid,
        }
    }

    fn take(&mut self, batch: Vec<AudioChange>) {
        self.batches += 1;
        for c in &batch {
            self.mirror
                .apply(c)
                .unwrap_or_else(|e| panic!("inconsistent diff {c:?}: {e}"));
            if let AudioChange::Levels(l) = c {
                self.levels.push((Instant::now(), l.clone()));
            }
        }
        for (_, d) in &self.mirror.sinks {
            self.volumes.push((d.name.clone(), d.volume));
        }
        let m = &self.mirror;
        if self.guard
            && (m.sink.is_none()
                || m.source.is_none()
                || m.sinks.is_empty()
                || m.sources.is_empty())
        {
            self.gaps.push(format!("{batch:?}"));
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
        self.until_or(secs, what, done, String::new);
    }

    /// [`Watch::until`], adding `context()` (read at the timeout, e.g. the
    /// session's metadata) to the failure message.
    fn until_or(
        &mut self,
        secs: u64,
        what: &str,
        done: impl Fn(&Mirror) -> bool,
        context: impl FnOnce() -> String,
    ) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        self.poll();
        while !done(&self.mirror) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(b) => self.take(b),
                Err(_) => panic!(
                    "timed out waiting for {what}: {:#?}\n{}",
                    self.mirror,
                    context()
                ),
            }
        }
    }

    fn sink(&self, name: &str) -> AudioDevice {
        self.mirror
            .sink_named(name)
            .cloned()
            .unwrap_or_else(|| panic!("no sink {name}: {:#?}", self.mirror))
    }

    /// Voluntary context switches (wakeups) of the `strand-pipewire`
    /// thread so far.
    fn wakeups(&self) -> u64 {
        let tid = self.tid.get().expect("a batch came");
        let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status"))
            .expect("the audio thread runs");
        status
            .lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("its context switches")
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
    assert_eq!((a.volume, a.muted), (1.0, false));
    assert_eq!(a.icon, "audio-volume-high-symbolic");
    let source = w.mirror.source_named("strand-source").cloned().unwrap();
    assert!(source.default);
    // WirePlumber picks the sink with the higher priority.session.
    assert_eq!(w.mirror.sink.as_ref().map(|d| d.id), Some(a.id));
    assert!(a.default && !b.default);
    assert!(w.mirror.sinks.iter().all(|(k, d)| *k == d.id));

    // Volume, on the cubic scale wpctl uses.
    pw.wpctl(&["set-volume", &a.id.to_string(), "0.5"]);
    w.until(5, "sink a at 0.5", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 0.5)
    });
    // The default sink's copy follows.
    assert_eq!(w.mirror.sink.as_ref().map(|d| d.volume), Some(0.5));
    assert_eq!(w.sink("strand-sink-a").icon, "audio-volume-medium-symbolic");

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
    assert_eq!(w.sink("strand-sink-b").icon, "audio-volume-muted-symbolic");

    // The default moves to b (wpctl writes default.configured.audio.sink;
    // WirePlumber then sets default.audio.sink).
    pw.wpctl(&["set-default", &b.id.to_string()]);
    // On a timeout the `default` metadata shows which side stalled: the
    // session manager (no `default.audio.sink` = b yet) or the service.
    // CI run 37804811859 timed out here once, without that record.
    w.until_or(
        5,
        "b as the default",
        |m| m.sink.as_ref().is_some_and(|d| d.name == "strand-sink-b"),
        || pw.session_state(),
    );
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

/// Clients that bind the `default` metadata and leave (`pw-metadata`,
/// `wpctl`) make the service read it again after `REREAD` (PipeWire
/// drops metadata updates to existing bindings while another client's
/// bind is in its handshake: decisions.md, laptop-flakes). Nothing was
/// lost here, so the read sends nothing; a default changed by such a
/// client still arrives. (That a read happens, and brings a lost update
/// back, is `a_lost_default_update_comes_back_on_a_read_again`.)
#[test]
fn a_metadata_reread_sends_only_what_changed() {
    let Some(pw) = PipeWire::start("a_metadata_reread_sends_only_what_changed") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let b = w.sink("strand-sink-b");
    let batches = w.batches;
    for _ in 0..3 {
        pw.metadata();
        let _ = pw.wpctl(&["status"]);
    }
    std::thread::sleep(REREAD * 2 + Duration::from_millis(300));
    w.poll();
    assert_eq!(
        w.batches, batches,
        "a read again sent a batch: {:#?}",
        w.mirror
    );
    assert!(w.sink("strand-sink-a").default);
    pw.wpctl(&["set-default", &b.id.to_string()]);
    w.until_or(
        5,
        "b as the default",
        |m| m.sink.as_ref().is_some_and(|d| d.id == b.id),
        || pw.session_state(),
    );
}

/// A set and a clear of the effective defaults, lost on purpose in
/// PipeWire's window (`MetaClient::lose`: WirePlumber emits them while a
/// bind waits for its pong), reach the service once a client goes: the
/// read again brings the set back, and the key the clear removed goes
/// too (the replayed keys replace the defaults; they are not laid over
/// them). With no read again the service would show sink a and the source
/// for good.
#[test]
fn a_lost_default_update_comes_back_on_a_read_again() {
    let Some(pw) = PipeWire::start("a_lost_default_update_comes_back_on_a_read_again") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    let b = w.sink("strand-sink-b");
    assert!(a.default);
    let client = MetaClient::connect(&pw);
    // The read again its coming caused is over.
    std::thread::sleep(REREAD * 2 + Duration::from_millis(300));
    w.poll();
    assert_eq!(
        w.mirror.sink.as_ref().map(|d| d.id),
        Some(a.id),
        "{}",
        pw.session_state()
    );

    // WirePlumber does not set the effective keys again on its own
    // (nothing rescans here), so what is lost stays lost.
    client.lose(
        &pw,
        "default.audio.sink",
        Some(r#"{"name":"strand-sink-b"}"#),
    );
    client.lose(&pw, "default.audio.source", None);
    let wanted = |keys: &std::collections::BTreeMap<String, String>| {
        keys.get("default.audio.sink")
            .is_some_and(|v| v.contains("strand-sink-b"))
            && !keys.contains_key("default.audio.source")
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !wanted(&client.keys()) {
        assert!(
            Instant::now() < deadline,
            "WirePlumber did not apply the set and the clear: {:?}",
            client.keys()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The premise: the service missed both (no client came or went, so
    // nothing made it read again).
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    assert_eq!(
        w.mirror.sink.as_ref().map(|d| d.id),
        Some(a.id),
        "the service saw the set: the window did not hold it back"
    );
    assert!(
        w.mirror.source.is_some(),
        "the service saw the clear: the window did not hold it back"
    );

    // A client goes: the service reads the metadata again.
    drop(client);
    w.until_or(
        5,
        "b as the default sink and no default source",
        |m| m.sink.as_ref().is_some_and(|d| d.id == b.id) && m.source.is_none(),
        || pw.session_state(),
    );
    assert!(w.sink("strand-sink-b").default);
    assert!(!w.sink("strand-sink-a").default);
}

/// A read again never overtakes the first read: with WirePlumber slow to
/// answer the first bind's ping (paused here, past [`REREAD`]), the
/// clients that come meanwhile (the service's own, and a `pw-cli`) ask
/// for a read again, and it waits until the first read is back. Were it
/// to replace the first binding before then, that binding's sync would
/// mark the metadata read while its replay was dropped, and the first
/// state would go out with no default sink or source.
#[test]
fn the_first_state_waits_for_a_slow_first_read() {
    let Some(pw) = PipeWire::start("the_first_state_waits_for_a_slow_first_read") else {
        return;
    };
    meta::pause_wireplumber(&pw);
    let mut w = Watch::start(pw.config());
    std::thread::sleep(Duration::from_millis(150));
    // A client that comes and goes after the metadata is bound (`pw-cli`
    // binds every global, the metadata too, so it waits on WirePlumber
    // as well, and is killed).
    let mut cli = pw
        .command("pw-cli")
        .args(["info", "0"])
        .spawn()
        .expect("pw-cli starts");
    std::thread::sleep(REREAD);
    let _ = cli.kill();
    let _ = cli.wait();
    std::thread::sleep(REREAD * 2);
    w.poll();
    let held = !w.mirror.connected;
    meta::resume_wireplumber(&pw);
    // The premise: the first read was held past a read again's time.
    assert!(
        held,
        "the first state went out while WirePlumber was paused: {:#?}",
        w.mirror
    );
    w.until(10, "the first state", |m| m.connected);
    assert_eq!(
        w.mirror.sink.as_ref().map(|d| d.name.as_str()),
        Some("strand-sink-a"),
        "the first state has no default sink: {:#?}",
        w.mirror
    );
    assert_eq!(
        w.mirror.source.as_ref().map(|d| d.name.as_str()),
        Some("strand-source"),
        "the first state has no default source: {:#?}",
        w.mirror
    );
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
    w.act(AudioAction::SetVolume(DeviceRef::id(a.id), 1.7))
        .unwrap();
    w.until(5, "a at 1", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 1.0)
    });
    assert_eq!(pw.volume(a.id).0, 1.0);
    w.act(AudioAction::SetVolume(DeviceRef::id(a.id), -1.0))
        .unwrap();
    w.until(5, "a at 0", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 0.0)
    });
    assert!(matches!(
        w.act(AudioAction::SetVolume(DeviceRef::id(a.id), f64::NAN)),
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
        w.sink("strand-sink-b").icon,
        "audio-volume-overamplified-symbolic"
    );
    w.act(AudioAction::SetVolume(DeviceRef::id(b.id), 2.0))
        .unwrap();
    w.act(AudioAction::SetVolume(DeviceRef::id(b.id), 1.25))
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
    assert_eq!(source.icon, "microphone-sensitivity-muted-symbolic");

    // `dev.make_default()`.
    w.act(AudioAction::MakeDefault(DeviceRef::id(b.id)))
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

    // `strand set audio.sink.volume +5%`, twice before PipeWire echoes the
    // first: the second adds to the first, none is lost.
    let steps = [
        w.audio
            .request(AudioAction::StepVolume(DeviceRef::DefaultSink, 0.05)),
        w.audio
            .request(AudioAction::StepVolume(DeviceRef::DefaultSink, 0.05)),
    ];
    for s in steps {
        s.wait().unwrap();
    }
    w.until(5, "b at 0.7", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| (d.volume - 0.7).abs() < 1e-9)
    });
    assert_eq!(pw.volume(b.id).0, 0.7);
    // Steps clamp as writes do.
    w.act(AudioAction::StepVolume(DeviceRef::id(b.id), 5.0))
        .unwrap();
    w.until(5, "b at 1", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| d.volume == 1.0)
    });
    w.act(AudioAction::StepVolume(DeviceRef::id(b.id), -5.0))
        .unwrap();
    w.until(5, "b at 0", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| d.volume == 0.0)
    });

    // A slider sends 80 writes (more than the thread remembers, `ECHOES`)
    // before the first echo: every echo reads back as one of the values
    // written, never as float noise of one, also when PipeWire echoes an
    // early, forgotten one.
    let from = w.volumes.len();
    let values: Vec<f64> = (0..80).map(|i| f64::from(300 + i) / 1000.0).collect();
    let replies: Vec<_> = values
        .iter()
        .map(|v| {
            w.audio
                .request(AudioAction::SetVolume(DeviceRef::id(b.id), *v))
        })
        .collect();
    for r in replies {
        r.wait().unwrap();
    }
    w.until(5, "b at 0.379", |m| {
        m.sink_named("strand-sink-b")
            .is_some_and(|d| d.volume == 0.379)
    });
    for (name, v) in &w.volumes[from..] {
        if name == "strand-sink-b" {
            assert!(
                *v == 0.0 || values.contains(v),
                "an echo of the slider read as {v}"
            );
        }
    }

    // Unknown devices.
    assert_eq!(
        w.act(AudioAction::SetMuted(DeviceRef::id(99_999), true)),
        Err(AudioError::UnknownDevice(DeviceRef::id(99_999)))
    );
    assert_eq!(
        w.act(AudioAction::MakeDefault(DeviceRef::id(99_999))),
        Err(AudioError::UnknownDevice(DeviceRef::id(99_999)))
    );
}

/// `pw-cli create-node` of a lingering null sink named `name`.
fn create_sink(pw: &PipeWire, name: &str) {
    pw.run(
        "pw-cli",
        &[
            "create-node",
            "adapter",
            &format!(
                "{{ factory.name=support.null-audio-sink node.name={name} \
                 media.class=Audio/Sink audio.position=[FL FR] object.linger=true }}"
            ),
        ],
    );
}

#[test]
fn a_reference_never_reaches_a_device_that_reused_its_id() {
    let Some(pw) = PipeWire::start("a_reference_never_reaches_a_device_that_reused_its_id") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    // The stream carries every device's serial.
    let DeviceRef::Id {
        serial: Some(serial_a),
        ..
    } = w.mirror.device_ref(a.id)
    else {
        panic!("no serial for sink a: {:#?}", w.mirror)
    };
    assert_eq!(
        w.act(AudioAction::SetMuted(
            DeviceRef::device(a.id, serial_a),
            true
        )),
        Ok(())
    );
    w.until(5, "a muted", |m| {
        m.sink_named("strand-sink-a").is_some_and(|d| d.muted)
    });
    // Another serial under a's id is another device: refused.
    let other = DeviceRef::device(a.id, serial_a + 1_000_000);
    assert_eq!(
        w.act(AudioAction::SetMuted(other, false)),
        Err(AudioError::UnknownDevice(other))
    );
    assert!(pw.volume(a.id).1, "a write with another serial landed");

    // A device leaves and PipeWire gives its id to a new one: a reference
    // taken before (an open popup's) is refused, and the new device is
    // untouched. PipeWire hands out freed ids again, so devices are made
    // and destroyed until one lands on the old id.
    create_sink(&pw, "strand-reuse-0");
    w.until(5, "the first device", |m| {
        m.sink_named("strand-reuse-0").is_some()
    });
    let first = w.sink("strand-reuse-0");
    let stale = w.mirror.device_ref(first.id);
    assert!(matches!(
        stale,
        DeviceRef::Id {
            serial: Some(_),
            ..
        }
    ));
    pw.run("pw-cli", &["destroy", &first.id.to_string()]);
    w.until(5, "the first device gone", |m| {
        m.sink_named("strand-reuse-0").is_none()
    });
    let mut tried = Vec::new();
    let reused = (1..=40).find_map(|i| {
        let name = format!("strand-reuse-{i}");
        create_sink(&pw, &name);
        w.until(5, "a new device", |m| m.sink_named(&name).is_some());
        let d = w.sink(&name);
        if d.id == first.id {
            return Some(d);
        }
        tried.push(d.id);
        pw.run("pw-cli", &["destroy", &d.id.to_string()]);
        w.until(5, "the new device gone", |m| m.sink_named(&name).is_none());
        None
    });
    let Some(reused) = reused else {
        panic!(
            "PipeWire never reused id {} (new devices got {tried:?})",
            first.id
        )
    };
    assert_ne!(w.mirror.device_ref(reused.id), stale, "the same serial");
    for action in [
        AudioAction::SetMuted(stale, true),
        AudioAction::SetVolume(stale, 0.2),
        AudioAction::StepVolume(stale, -0.5),
        AudioAction::MakeDefault(stale),
    ] {
        assert_eq!(
            w.act(action.clone()),
            Err(AudioError::UnknownDevice(stale)),
            "{action:?}"
        );
    }
    assert_eq!(pw.volume(reused.id), (1.0, false), "a stale write landed");
    assert_ne!(w.mirror.sink.as_ref().map(|d| d.id), Some(reused.id));
    // A reference taken now reaches it.
    w.act(AudioAction::SetMuted(w.mirror.device_ref(reused.id), true))
        .unwrap();
    w.until(5, "the new device muted", |m| {
        m.sinks.iter().any(|(k, d)| *k == reused.id && d.muted)
    });
}

#[test]
fn a_daemon_restart_reconnects() {
    let Some(mut pw) = PipeWire::start("a_daemon_restart_reconnects") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let before = w.sink("strand-sink-a");

    // A crash leaves the socket behind: attempts are refused until the
    // restarted daemon replaces it.
    pw.crash_daemon();
    let crashed = Instant::now();
    assert!(pw.socket().exists());
    w.until(5, "the connection lost", |m| !m.connected);
    // The devices stay while it is away.
    assert!(w.mirror.sink_named("strand-sink-a").is_some());
    assert_eq!(
        w.act(AudioAction::SetMuted(DeviceRef::DefaultSink, true)),
        Err(AudioError::NotConnected)
    );

    // Away long enough that the backoff waits seconds (attempts at 0.1,
    // 0.3, 0.7, 1.5, 3.1, 6.3, 12.7 s, then every 10 s): back at 13.5 s,
    // the next attempt is 9 s away, so only the socket's replacement
    // (inotify) can bring it back within 6 s (the daemon's and
    // WirePlumber's start on a loaded machine, and at worst `SETTLE`).
    std::thread::sleep(
        (crashed + Duration::from_millis(13_500)).saturating_duration_since(Instant::now()),
    );
    let back = Instant::now();
    pw.start_daemon();
    w.until(10, "reconnected", ready);
    assert!(
        back.elapsed() < Duration::from_secs(6),
        "reconnected {:?} after the daemon restarted",
        back.elapsed()
    );
    let after = w.sink("strand-sink-a");
    assert_eq!(after.description, before.description);
    // And it works.
    w.act(AudioAction::SetVolume(DeviceRef::id(after.id), 0.5))
        .unwrap();
    w.until(5, "a at 0.5", |m| {
        m.sink_named("strand-sink-a")
            .is_some_and(|d| d.volume == 0.5)
    });
    assert_eq!(pw.volume(after.id).0, 0.5);

    // The session manager alone restarting: the defaults shown stay a
    // while (`SETTLE`), and come back with its new metadata.
    pw.kill_wireplumber();
    let gone = Instant::now();
    w.until(10, "no default without a session manager", |m| {
        m.sink.is_none()
    });
    assert!(gone.elapsed() >= SETTLE - Duration::from_millis(100));
    pw.start_wireplumber();
    w.until_or(
        10,
        "the default back",
        |m| m.sink.is_some(),
        || pw.session_state(),
    );
    w.guard = true;
    pw.kill_wireplumber();
    std::thread::sleep(Duration::from_millis(300));
    pw.start_wireplumber();
    w.until_or(
        10,
        "the default",
        |m| m.sink.is_some(),
        || pw.session_state(),
    );
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    assert!(
        w.gaps.is_empty(),
        "a quick session manager restart showed gaps: {:#?}",
        w.gaps
    );
}

#[test]
fn writes_sent_before_the_first_state_land() {
    let Some(pw) = PipeWire::start("writes_sent_before_the_first_state_land") else {
        return;
    };
    // A media key starts the service with a write: it runs once the
    // first state is out, on the default it names.
    let w = Watch::start(pw.config());
    let replies = [
        w.audio
            .request(AudioAction::SetVolume(DeviceRef::DefaultSink, 0.4)),
        w.audio
            .request(AudioAction::SetMuted(DeviceRef::DefaultSink, true)),
    ];
    for r in replies {
        assert_eq!(r.wait(), Ok(()));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while pw.volume_of("@DEFAULT_AUDIO_SINK@") != (0.4, true) {
        assert!(Instant::now() < deadline, "the early writes did not land");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_restart_shows_no_gap_while_the_session_manager_comes_back() {
    let Some(mut pw) =
        PipeWire::start("a_restart_shows_no_gap_while_the_session_manager_comes_back")
    else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    pw.crash_daemon();
    w.until(5, "the connection lost", |m| !m.connected);
    w.guard = true;
    // As on hardware: the daemon comes back bare, the service connects at
    // once, and the session manager then makes the metadata and the
    // devices.
    pw.start_bare_daemon();
    std::thread::sleep(Duration::from_millis(300));
    pw.spawn_wireplumber();
    pw.wait_for_metadata();
    std::thread::sleep(Duration::from_millis(200));
    pw.create_devices();
    let back = Instant::now();
    w.until(10, "reconnected", ready);
    assert!(
        back.elapsed() < Duration::from_secs(3),
        "back {:?} after its devices",
        back.elapsed()
    );
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    assert!(
        w.gaps.is_empty(),
        "an empty default or list between the loss and the recovery: {:#?}",
        w.gaps
    );
    assert_eq!(
        w.mirror.sink.as_ref().map(|d| d.name.as_str()),
        Some("strand-sink-a")
    );
    assert_eq!(
        w.mirror.source.as_ref().map(|d| d.name.as_str()),
        Some("strand-source")
    );
    // A write during the outage waited for it.
    pw.crash_daemon();
    w.until(5, "the connection lost again", |m| !m.connected);
    let mute = w
        .audio
        .request(AudioAction::SetMuted(DeviceRef::DefaultSink, true));
    pw.start_daemon();
    assert_eq!(mute.wait(), Ok(()));
    w.until(10, "the default muted", |m| {
        m.sink.as_ref().is_some_and(|d| d.muted)
    });
    assert!(pw.volume_of("@DEFAULT_AUDIO_SINK@").1);
}

#[test]
fn a_silent_daemon_shows_nothing_and_is_retried() {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let Some(mut pw) = PipeWire::start("a_silent_daemon_shows_nothing_and_is_retried") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    pw.crash_daemon();
    w.until(5, "the connection lost", |m| !m.connected);

    // Socket activation with a failing `pipewire.service`: the socket
    // accepts connections, and nobody ever answers.
    let _ = std::fs::remove_file(pw.socket());
    let listener = UnixListener::bind(pw.socket()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let (accepts, stop) = (accepts.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, _)) => {
                        held.push(s);
                        accepts.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
        })
    };
    let since = Instant::now();
    let mute = w
        .audio
        .request(AudioAction::SetMuted(DeviceRef::DefaultSink, true));
    let mut seen = Vec::new();
    let mut record = |w: &mut Watch, until: Instant| {
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if let Ok(b) = w.rx.recv_timeout(left.min(Duration::from_millis(100))) {
                seen.extend(b.iter().cloned());
                w.take(b);
            }
        }
    };
    // Past SETTLE and UNANSWERED: nothing is published, the devices and
    // defaults of before stay, not connected.
    record(&mut w, since + UNANSWERED + Duration::from_secs(1));
    assert!(accepts.load(Ordering::Relaxed) >= 1, "it never connected");
    assert_eq!(mute.wait(), Err(AudioError::NotConnected));
    // Dropped and retried, as a lost connection.
    let deadline = since + Duration::from_secs(20);
    while accepts.load(Ordering::Relaxed) < 2 {
        assert!(
            Instant::now() < deadline,
            "the silent daemon was not retried"
        );
        record(&mut w, Instant::now() + Duration::from_millis(100));
    }
    assert!(
        seen.is_empty(),
        "a silent daemon changed the state: {seen:#?}"
    );
    assert!(!w.mirror.connected);
    assert!(ready(&Mirror {
        connected: true,
        ..w.mirror.clone()
    }));

    // The real daemon's return reconnects.
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    let _ = std::fs::remove_file(pw.socket());
    pw.start_daemon();
    w.until(15, "reconnected", ready);
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
    w.until_or(10, "connected", ready, || pw.session_state());
}

/// The backoff's attempts while disconnected come at 0.1, 0.3, 0.7, 1.5,
/// 3.1, 6.3 and 12.7 s; after the last one the doubling has passed 10 s.
const BACKOFF_SPENT: Duration = Duration::from_millis(13_500);

/// Starts a thread on `pw` with its daemon down and inotify denied to it;
/// returns it and when it started, after its first (empty) batch.
fn start_without_inotify(pw: &mut PipeWire) -> (Watch, Instant, String) {
    pw.kill_daemon();
    let remote = pw.config().remote.expect("an absolute socket");
    strand_services::audio::deny_inotify(&remote, true);
    let started = Instant::now();
    let mut w = Watch::start(pw.config());
    let first = w.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first[0], AudioChange::Connected(false));
    w.take(first);
    (w, started, remote)
}

#[test]
fn without_inotify_it_reconnects_on_its_timer() {
    let Some(mut pw) = PipeWire::start("without_inotify_it_reconnects_on_its_timer") else {
        return;
    };
    let (mut w, started, remote) = start_without_inotify(&mut pw);
    // No watch: once the doubling passed 10 s, the timer keeps going at
    // 10 s (with a watch it would stop and wait for the socket), so the
    // daemon is reached within 10 s of appearing.
    std::thread::sleep((started + BACKOFF_SPENT).saturating_duration_since(Instant::now()));
    let back = Instant::now();
    pw.start_daemon();
    w.until(20, "reconnected on the timer", ready);
    assert!(
        back.elapsed() < Duration::from_secs(10) + Duration::from_secs(5),
        "reconnected {:?} after the daemon came back",
        back.elapsed()
    );
    strand_services::audio::deny_inotify(&remote, false);
}

#[test]
fn a_watch_that_inotify_refused_comes_back() {
    let Some(mut pw) = PipeWire::start("a_watch_that_inotify_refused_comes_back") else {
        return;
    };
    let (mut w, started, remote) = start_without_inotify(&mut pw);
    // inotify has an instance again (another program freed one): the
    // next attempt (at 1.5 s) makes the watch.
    std::thread::sleep(Duration::from_millis(1_000));
    strand_services::audio::deny_inotify(&remote, false);
    // Past the doubling, only the socket's creation wakes the thread: its
    // next timer attempt would come 9 s after the daemon is back.
    std::thread::sleep((started + BACKOFF_SPENT).saturating_duration_since(Instant::now()));
    let back = Instant::now();
    pw.start_daemon();
    w.until(10, "reconnected", ready);
    assert!(
        back.elapsed() < Duration::from_secs(6),
        "reconnected {:?} after the daemon came back: the watch was not made again",
        back.elapsed()
    );
}

/// The readings of `target` after `from` (an index into `w.levels`).
fn readings(w: &Watch, from: usize, target: LevelTarget) -> Vec<Levels> {
    w.levels[from..]
        .iter()
        .filter(|(_, l)| l.target == target)
        .map(|(_, l)| l.clone())
        .collect()
}

#[test]
fn peak_meters_run_only_while_asked_for() {
    let Some(pw) = PipeWire::start("peak_meters_run_only_while_asked_for") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    let b = w.sink("strand-sink-b");
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

    // Hidden: the stream goes, and its last word is a quiet reading.
    w.audio.set_levels([]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while pw.has_node("strand-levels") {
        assert!(Instant::now() < deadline, "the meter stream stayed");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(200));
    w.poll();
    assert_eq!(
        w.mirror.levels(LevelTarget::DefaultSink).map(Levels::peak),
        Some(0.0),
        "a hidden meter keeps no level"
    );
    let stopped = Instant::now();
    // The sound stops while it is hidden.
    let _ = player.kill();
    let _ = player.wait();
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    assert!(
        w.levels.iter().all(|(t, _)| *t < stopped),
        "readings after the meter stopped"
    );

    // Shown again over silence: still quiet, nothing sent.
    let from = w.levels.len();
    w.audio.set_levels([LevelTarget::DefaultSink]);
    std::thread::sleep(Duration::from_millis(1500));
    w.poll();
    assert_eq!(
        w.mirror.levels(LevelTarget::DefaultSink).map(Levels::peak),
        Some(0.0)
    );
    assert!(readings(&w, from, LevelTarget::DefaultSink).is_empty());

    // A meter on the default follows it. a plays (pinned there), then b,
    // which is silent, becomes the default.
    let mut player = pw.play(&wav, "strand-sink-a");
    w.audio
        .set_levels([LevelTarget::DefaultSink, LevelTarget::Device(a.id)]);
    w.until(10, "both meters reading a", |m| {
        [LevelTarget::DefaultSink, LevelTarget::Device(a.id)]
            .iter()
            .all(|t| {
                m.levels(*t)
                    .is_some_and(|l| l.device == a.id && l.peak() > 0.4)
            })
    });
    w.act(AudioAction::MakeDefault(DeviceRef::id(b.id)))
        .unwrap();
    w.until(10, "b as the default", |m| {
        m.sink.as_ref().is_some_and(|d| d.id == b.id)
    });
    let switched = w.levels.len();
    std::thread::sleep(Duration::from_millis(1000));
    w.poll();
    assert_eq!(pw.linked_to("pw-play"), ["strand-sink-a"], "a still plays");
    // The default's meter said a is no longer its device (a quiet
    // reading), and b, silent, sent nothing after.
    let after = readings(&w, switched, LevelTarget::DefaultSink);
    assert!(
        after.iter().all(|l| l.peak() == 0.0),
        "a sound on the silent default: {after:?}"
    );
    assert_eq!(
        w.mirror.levels(LevelTarget::DefaultSink).map(Levels::peak),
        Some(0.0)
    );
    // a's own meter still reads it.
    assert!(
        readings(&w, switched, LevelTarget::Device(a.id))
            .iter()
            .any(|l| l.peak() > 0.4)
    );

    // b plays: the default's meter reads b.
    let mut player_b = pw.play(&wav, "strand-sink-b");
    w.until(10, "the default meter on b", |m| {
        m.levels(LevelTarget::DefaultSink)
            .is_some_and(|l| l.device == b.id && l.peak() > 0.4)
    });
    let _ = player_b.kill();
    let _ = player_b.wait();
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

#[test]
fn peak_readings_are_capped_at_the_frame_rate() {
    let Some(pw) = PipeWire::start("peak_readings_are_capped_at_the_frame_rate") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    let wav = pw.dir.path().join("square.wav");
    square_wav(&wav, 30.0, 0.5);
    w.audio.set_levels([LevelTarget::Device(a.id)]);
    // A low-latency client shrinks the graph's cycle to 64 samples
    // (1.3 ms, 750 cycles a second).
    let mut player = pw.play_with(&wav, "strand-sink-a", &["--latency", "64"]);
    w.until(10, "a reading", |m| {
        m.levels(LevelTarget::Device(a.id))
            .is_some_and(|l| l.peak() > 0.4)
    });
    std::thread::sleep(Duration::from_millis(300));
    w.poll();
    let (batches, readings) = (w.batches, w.levels.len());
    let woken = w.wakeups();
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    let woken = w.wakeups() - woken;
    w.poll();
    let secs = start.elapsed().as_secs_f64();
    let _ = player.kill();
    let _ = player.wait();
    let per_sec = (w.batches - batches) as f64 / secs;
    let cap = 1.0 / FRAME.as_secs_f64();
    eprintln!(
        "{:.0} batches/s, {:.0} readings/s (cap {cap:.0})",
        per_sec,
        (w.levels.len() - readings) as f64 / secs
    );
    assert!(per_sec <= cap + 2.0, "{per_sec:.0} batches a second");
    // The loop itself wakes about once a frame, not once a cycle: the
    // cycles are read on the data thread.
    let wakes = woken as f64 / secs;
    eprintln!("strand-pipewire: {wakes:.0} wakeups a second");
    assert!(
        wakes <= 70.0,
        "strand-pipewire woke {wakes:.0} times a second"
    );
    // The readings still flow, at about the frame rate, and each is the
    // loudest of the cycles it covers (a client this fast may underrun now
    // and then; a held reading covers a dozen cycles).
    assert!(per_sec >= 20.0, "only {per_sec:.0} batches a second");
    let read = &w.levels[readings..];
    let at_peak = read
        .iter()
        .filter(|(_, l)| (l.peak() - 0.5).abs() < 0.01)
        .count();
    assert!(
        at_peak * 10 >= read.len() * 9,
        "{at_peak} of {} readings at the peak",
        read.len()
    );
}

/// (M4) A 1 kHz test tone played to a sink: its meter's readings carry
/// the spectrum, loudest in the tone's band and quiet an octave and more
/// away; once the tone stops the meter says so once, with no spectrum,
/// and sends nothing more (no FFT runs on silence).
#[test]
fn a_test_tone_lights_its_spectrum_band() {
    let Some(pw) = PipeWire::start("a_test_tone_lights_its_spectrum_band") else {
        return;
    };
    let mut w = Watch::start(pw.config());
    w.until(10, "the devices", ready);
    let a = w.sink("strand-sink-a");
    let wav = pw.dir.path().join("tone.wav");
    sine_wav(&wav, 30.0, 1000.0, 0.5);
    w.audio.set_levels([LevelTarget::Device(a.id)]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pw.has_node("strand-levels") {
        assert!(Instant::now() < deadline, "the meter stream never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut player = pw.play(&wav, "strand-sink-a");
    let band = spectrum::band_of(1000.0);
    w.until(10, "a spectrum peaking at 1 kHz", |m| {
        m.levels(LevelTarget::Device(a.id)).is_some_and(|l| {
            l.bins.len() == spectrum::BANDS
                && l.bins
                    .iter()
                    .enumerate()
                    .max_by(|x, y| x.1.total_cmp(y.1))
                    .is_some_and(|(i, _)| i.abs_diff(band) <= 1)
        })
    });
    let l = w.mirror.levels(LevelTarget::Device(a.id)).cloned().unwrap();
    assert!(l.bins[band].max(l.bins[band + 1]) > 0.8, "{:?}", l.bins);
    for far in [spectrum::band_of(200.0), spectrum::band_of(6000.0)] {
        assert!(
            l.bins[far] < 0.5,
            "band {far} at {}: {:?}",
            l.bins[far],
            l.bins
        );
    }
    // At most one spectrum a frame, like the peaks.
    let from = w.levels.len();
    std::thread::sleep(Duration::from_millis(500));
    w.poll();
    let n = readings(&w, from, LevelTarget::Device(a.id)).len();
    assert!(n <= 34, "{n} spectra in 0.5 s");

    let _ = player.kill();
    let _ = player.wait();
    w.until(10, "the tone stopping", |m| {
        m.levels(LevelTarget::Device(a.id))
            .is_some_and(|l| l.peak() == 0.0)
    });
    let quiet = w.mirror.levels(LevelTarget::Device(a.id)).cloned().unwrap();
    assert!(quiet.bins.is_empty(), "a quiet reading has no spectrum");
    std::thread::sleep(Duration::from_millis(300));
    w.poll();
    let after = w.levels.len();
    std::thread::sleep(Duration::from_millis(700));
    w.poll();
    assert_eq!(w.levels.len(), after, "readings, and FFTs, while silent");
}

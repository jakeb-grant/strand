//! Audio devices over PipeWire (the `audio` service).
//!
//! design.md: "pipewire 0.10 — Audio, levels, default sink. No usable
//! WirePlumber binding; read PipeWire's `default` metadata". pipewire-rs is
//! `!Send`, so [`Audio::spawn`] runs everything on its own
//! `strand-pipewire` thread (design.md, "Threads"): a PipeWire main loop
//! that
//!
//! - follows the registry's audio nodes (`media.class` `Audio/Sink`,
//!   `Audio/Duplex`, `Audio/Source`, `Audio/Source/Virtual`) and each one's
//!   `Props` param: volume (`channelVolumes`, shown on the cubic scale
//!   `pactl` and `wpctl` show) and mute; and the active `Route` params of
//!   the audio devices (cards) those nodes belong to;
//! - reads the default sink and source from the `default` metadata
//!   (`default.audio.sink`, else `default.configured.audio.sink`; the same
//!   for sources), reading it again [`REREAD`] after a client comes or
//!   goes or a default is cleared (PipeWire drops metadata updates to
//!   existing bindings while another client binds it; the replayed keys
//!   replace the ones held, so a lost clear is read too);
//! - writes volume and mute on every channel: through the card's active
//!   `Route` (`save: true`) when the node has one, as `wpctl` and
//!   pipewire-pulse do, else with the node's `set_param(Props)`; and the
//!   default with the metadata (see [`AudioAction`]);
//! - runs a peak meter (a passive capture stream) per [`LevelTarget`] only
//!   while [`Audio::set_levels`] asks for it (the store asks while a
//!   visible reader wants levels), at most one reading per meter per
//!   frame (1/60 s, the loudest of the cycles in between), read on
//!   PipeWire's data thread so the loop wakes at most once a frame;
//! - reconnects when the daemon restarts (100 ms doubling backoff, and an
//!   inotify watch on the socket's directory while disconnected; with no
//!   inotify instance to be had, attempts every 10 s, and the watch is
//!   tried again on each until it can be made), keeping
//!   the last devices meanwhile: a new connection's first state waits for
//!   the session manager (its `default` metadata, and the defaults shown
//!   before) for up to [`SETTLE`], so a restart shows no empty default or
//!   list in between; a daemon that accepts the connection but never
//!   answers its first sync publishes nothing and is dropped after
//!   [`UNANSWERED`] (then retried, as a lost one).
//!
//! The loop runs for one of two hosts. [`AudioStore`] (the `audio`
//! service on the contract, `#[service(thread)]`) runs it on the
//! service's own `strand-audio` thread and serves [`SCHEMA`] (the
//! builtin schema's `audio` declaration); [`Audio::spawn`] (a handle for
//! Rust users and tests) runs it on a `strand-pipewire` thread and gives
//! every change to its sink. Either way changes come as one batch of
//! [`AudioChange`]s per burst of PipeWire events (the [`Publisher`]'s
//! keyed diffs, keyed by the device's `u32` id); [`Mirror`] applies them.
//! Nothing polls: with nothing changing, the thread sleeps in the loop
//! and wakes for nothing.
//!
//! Writes are plain requests to the loop, untagged. The store answers
//! each write tagged ([`crate::Cx::report`]) by the batch that
//! shows its value, in write order per cell (see the store's docs), and
//! does not rely on `strand_core::echo`'s value path. The thread still
//! reports a volume it wrote exactly as written, never as its cube
//! root's float noise (any of its last [`ECHOES`] writes to a device, and
//! any volume on the 1/10 000 grid ([`perceptual`]) even once forgotten;
//! through a card's `Route`, whose mixer steps quantize it, an echo
//! within half a percent reads as the closest write): the store's
//! by-value match needs it, and [`Audio`] handle users whose own echo
//! suppression goes by value rely on it.
//!
//! An action sent before a connection has published its first state (a
//! media key that starts the service with a write, or one sent during a
//! restart) waits for it and runs right after, so `DeviceRef::DefaultSink`
//! resolves against the synced defaults; with no connection at all it
//! waits [`GRACE`], then answers [`AudioError::NotConnected`].

mod meter;
pub mod model;
pub mod pod;
mod schema;
mod service;
mod thread;

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::thread::JoinHandle;

use tokio::sync::oneshot;

pub use model::{
    AudioChange, AudioDevice, AudioState, Direction, LevelTarget, Levels, Mirror, Publisher, icon,
    linear, perceptual,
};
pub use schema::SCHEMA;
pub use service::{
    ANSWER_WAIT, AudioDeviceAction, AudioStore, AudioStoreCells, LevelTap, configure, tap_levels,
};
#[doc(hidden)]
pub use thread::deny_inotify;
pub use thread::{ECHOES, FRAME, GRACE, REREAD, SETTLE, UNANSWERED};

/// Where to connect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioConfig {
    /// The PipeWire socket: a name in the runtime directory
    /// (`$PIPEWIRE_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR`) or an absolute
    /// path. `None` is PipeWire's own default (`$PIPEWIRE_REMOTE`, else
    /// `pipewire-0`).
    pub remote: Option<String>,
}

/// Which device an action is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceRef {
    /// The default output at the moment the action runs (`audio.sink`).
    DefaultSink,
    /// The default input (`audio.source`).
    DefaultSource,
    /// A device by id (an item of `audio.sinks` or `audio.sources`) and,
    /// when known, its `object.serial`. PipeWire reuses a freed id for
    /// an object it creates later; serials are never reused, so a
    /// reference with a serial is refused ([`AudioError::UnknownDevice`])
    /// once its id holds another device, and never reaches that device.
    /// Without a serial it is whatever device holds the id when the
    /// action runs. [`AudioState::device_ref`] and [`Mirror::device_ref`]
    /// build one from the stream's serials ([`AudioChange::Serials`]).
    Id {
        /// The PipeWire global id.
        id: u32,
        /// Its `object.serial`; `None`: any device holding `id`.
        serial: Option<u64>,
    },
}

impl DeviceRef {
    /// Whatever device holds `id` when the action runs.
    pub fn id(id: u32) -> Self {
        Self::Id { id, serial: None }
    }

    /// The device with `id` and `object.serial` `serial`, only while it
    /// holds that id.
    pub fn device(id: u32, serial: u64) -> Self {
        Self::Id {
            id,
            serial: Some(serial),
        }
    }
}

/// A write or action on a device.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioAction {
    /// `audio.sink.volume = v`: every channel to `v` on the perceptual
    /// scale, clamped to 0..1 (or to the current volume, when another
    /// program amplified it past 1: a write never raises it further).
    SetVolume(DeviceRef, f64),
    /// `audio.sink.volume += d` (`strand set audio.sink.volume +5%`): the
    /// step is added to the device's volume on the audio thread when the
    /// action runs (to the last volume written there, if PipeWire has not
    /// echoed it yet), so quick steps never read the same value and lose
    /// one. Clamped as `SetVolume`.
    StepVolume(DeviceRef, f64),
    /// `audio.sink.muted = m`.
    SetMuted(DeviceRef, bool),
    /// `dev.make_default()`: writes the `default` metadata's
    /// `default.configured.audio.{sink,source}` (what `wpctl set-default`
    /// and `pactl set-default-sink` write, which a session manager then
    /// applies) and `default.audio.{sink,source}` (the effective default,
    /// for setups without a session manager).
    MakeDefault(DeviceRef),
}

/// Why an action did not run.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioError {
    /// Not connected to PipeWire (no connection came within [`GRACE`], or
    /// the service stopped before the action ran).
    NotConnected,
    /// No such device (or no default of that direction).
    UnknownDevice(DeviceRef),
    /// The volume is not a number.
    InvalidVolume(f64),
    /// No `default` metadata to write (no session manager has created it).
    NoDefaultMetadata,
    /// PipeWire refused, or the request could not be built.
    Failed(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("PipeWire is not connected"),
            Self::UnknownDevice(d) => write!(f, "no audio device {d:?}"),
            Self::InvalidVolume(v) => write!(f, "invalid volume {v}"),
            Self::NoDefaultMetadata => f.write_str("PipeWire has no `default` metadata"),
            Self::Failed(why) => write!(f, "audio: {why}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// The outcome of an [`AudioAction`]: `Ok` once PipeWire has been asked
/// (the change itself arrives in the stream), or an [`AudioError`]. An
/// action the thread dropped unanswered is [`AudioError::NotConnected`]:
/// a caller never sees a closed channel.
#[derive(Debug)]
pub struct AudioReply(oneshot::Receiver<Result<(), AudioError>>);

impl AudioReply {
    /// Waits for the outcome on a plain thread outside any tokio runtime
    /// (tests, a sink thread of its own); never the logic thread. Inside a
    /// runtime (the services' shared current-thread runtime), `.await`
    /// the reply instead: it is a [`Future`], and blocking there panics.
    pub fn wait(self) -> Result<(), AudioError> {
        debug_assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "AudioReply::wait inside a tokio runtime: await the reply instead"
        );
        self.0
            .blocking_recv()
            .unwrap_or(Err(AudioError::NotConnected))
    }
}

impl Future for AudioReply {
    type Output = Result<(), AudioError>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(AudioError::NotConnected)))
    }
}

/// What the handle (or the [`Host`]) sends the thread.
pub(crate) enum Cmd {
    Action(AudioAction, oneshot::Sender<Result<(), AudioError>>),
    Levels(BTreeSet<LevelTarget>),
    /// The host has work: [`Host::poll`] is called once the queue is
    /// handled (it is after every batch of work anyway).
    Poke,
    Stop,
}

/// Who a PipeWire loop reports to: [`Audio::spawn`]'s sink, or the `audio`
/// store running the loop on its own service thread ([`AudioStore`]).
pub(crate) trait Host {
    /// A batch of changes, on the loop's thread. Must never block.
    fn changes(&mut self, batch: Vec<AudioChange>);
    /// Commands of the host's own (its inbox drained), called after each
    /// burst of work and after each batch; empty when it has none.
    fn poll(&mut self) -> Vec<Cmd> {
        Vec::new()
    }
    /// When the host next needs [`Host::poll`] called with nothing else
    /// happening (a write's answer timing out).
    fn deadline(&self) -> Option<std::time::Instant> {
        None
    }
}

/// [`Audio::spawn`]'s host: the sink alone.
struct SinkHost<F>(F);

impl<F: FnMut(Vec<AudioChange>)> Host for SinkHost<F> {
    fn changes(&mut self, batch: Vec<AudioChange>) {
        (self.0)(batch)
    }
}

/// The running service: its `strand-pipewire` thread. Dropping it asks
/// the thread to stop and returns at once (the thread ends on its own,
/// after the batch it is handling); [`Audio::stop`] also waits for it.
pub struct Audio {
    tx: pipewire::channel::Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audio").finish_non_exhaustive()
    }
}

impl Audio {
    /// Starts the thread. `sink` gets every batch of changes, on that
    /// thread; the first batch (once the first connection has synced, or
    /// the first attempt failed) holds every field, [`AudioChange::Connected`]
    /// first. `sink` must never block (an unbounded channel, or a
    /// `try_send` that drops a full queue): it runs on the PipeWire loop,
    /// and [`Audio::stop`] waits for that loop.
    pub fn spawn(
        config: AudioConfig,
        sink: impl FnMut(Vec<AudioChange>) + Send + 'static,
    ) -> std::io::Result<Audio> {
        let (tx, rx) = pipewire::channel::channel();
        let thread = std::thread::Builder::new()
            .name("strand-pipewire".into())
            .spawn(move || {
                if let Err(e) = thread::run(config, Box::new(SinkHost(sink)), rx) {
                    log::error!("audio: {e}");
                }
            })?;
        Ok(Audio {
            tx,
            thread: Some(thread),
        })
    }

    /// Runs an action: now, or (before a connection's first state) right
    /// after it, in the order sent.
    pub fn request(&self, action: AudioAction) -> AudioReply {
        let (reply, rx) = oneshot::channel();
        // A thread that has ended drops the request, and with it `reply`.
        let _ = self.tx.send(Cmd::Action(action, reply));
        AudioReply(rx)
    }

    /// The peak meters wanted now (replacing the previous set; empty stops
    /// them all). Each sends [`AudioChange::Levels`] while its device
    /// makes sound.
    pub fn set_levels(&self, targets: impl IntoIterator<Item = LevelTarget>) {
        let _ = self.tx.send(Cmd::Levels(targets.into_iter().collect()));
    }

    /// Stops the thread and waits for it (briefly: until the loop has
    /// handled the stop, and the sink has returned). Not on the logic
    /// thread or a shared runtime; dropping the handle does not wait.
    pub fn stop(mut self) {
        let _ = self.tx.send(Cmd::Stop);
        let Some(t) = self.thread.take() else { return };
        // Stopped on its own thread (from the sink): it stops after this
        // batch; joining itself would never return.
        if t.thread().id() == std::thread::current().id() {
            return;
        }
        if t.join().is_err() {
            log::error!("the strand-pipewire thread panicked");
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // Detached: the thread ends on its own once it reads the stop.
        let _ = self.tx.send(Cmd::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handle_and_its_reply_are_send() {
        fn send<T: Send>() {}
        send::<Audio>();
        send::<AudioReply>();
        send::<AudioAction>();
    }

    #[test]
    fn a_handle_without_a_daemon_answers_not_connected() {
        let dir = std::env::temp_dir().join(format!("strand-no-pw-{}", std::process::id()));
        let (tx, rx) = std::sync::mpsc::channel();
        let audio = Audio::spawn(
            AudioConfig {
                remote: Some(dir.join("pipewire-0").display().to_string()),
            },
            move |b| {
                let _ = tx.send(b);
            },
        )
        .unwrap();
        let first = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(first[0], AudioChange::Connected(false));
        assert_eq!(
            audio
                .request(AudioAction::SetMuted(DeviceRef::DefaultSink, true))
                .wait(),
            Err(AudioError::NotConnected)
        );
        audio.stop();
    }
}

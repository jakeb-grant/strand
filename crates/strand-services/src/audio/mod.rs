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
//!   `pactl` and `wpctl` show) and mute;
//! - reads the default sink and source from the `default` metadata
//!   (`default.audio.sink`, else `default.configured.audio.sink`; the same
//!   for sources);
//! - writes volume and mute with `set_param(Props)` on every channel, and
//!   the default with the metadata (see [`AudioAction`]);
//! - runs a peak meter (a passive capture stream) per [`LevelTarget`] only
//!   while [`Audio::set_levels`] asks for it: the store asks while a
//!   visible reader wants levels;
//! - reconnects when the daemon restarts (100 ms doubling backoff, and an
//!   inotify watch on the socket's directory while disconnected), keeping
//!   the last devices meanwhile.
//!
//! Every change goes to the sink given to [`Audio::spawn`] as one batch of
//! [`AudioChange`]s per burst of PipeWire events (the [`Publisher`]'s
//! keyed diffs); [`Mirror`] applies them. Nothing polls: with nothing
//! changing, the thread sleeps in the loop and wakes for nothing.
//!
//! Writes are plain requests; suppressing the echo of a write in the store
//! (the `rw` contract) is the store's job. The thread does report a volume
//! it wrote exactly as written (not its cube root's float noise), so the
//! echo compares equal.

mod meter;
pub mod model;
pub mod pod;
mod thread;

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::thread::JoinHandle;

use tokio::sync::oneshot;

pub use model::{
    AudioChange, AudioDevice, AudioState, Direction, LevelTarget, Levels, Mirror, Publisher,
    linear, perceptual,
};

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
    /// A device by id (an item of `audio.sinks` or `audio.sources`).
    Id(u32),
}

/// A write or action on a device.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioAction {
    /// `audio.sink.volume = v`: every channel to `v` on the perceptual
    /// scale, clamped to 0..1 (or to the current volume, when another
    /// program amplified it past 1: a write never raises it further).
    SetVolume(DeviceRef, f64),
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
    /// Not connected to PipeWire (it is restarting, or the service stopped
    /// before the action ran).
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
    /// Waits for the outcome on a thread that may block (tests, a
    /// service thread; never the logic thread).
    pub fn wait(self) -> Result<(), AudioError> {
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

/// What the handle sends the thread.
pub(crate) enum Cmd {
    Action(AudioAction, oneshot::Sender<Result<(), AudioError>>),
    Levels(BTreeSet<LevelTarget>),
    Stop,
}

/// The running service: its `strand-pipewire` thread. Dropping it stops
/// the thread and waits for it.
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
    /// first.
    pub fn spawn(
        config: AudioConfig,
        sink: impl FnMut(Vec<AudioChange>) + Send + 'static,
    ) -> std::io::Result<Audio> {
        let (tx, rx) = pipewire::channel::channel();
        let thread = std::thread::Builder::new()
            .name("strand-pipewire".into())
            .spawn(move || thread::run(config, Box::new(sink), rx))?;
        Ok(Audio {
            tx,
            thread: Some(thread),
        })
    }

    /// Runs an action.
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

    /// Stops the thread and waits for it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            log::error!("the strand-pipewire thread panicked");
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shutdown();
    }
}

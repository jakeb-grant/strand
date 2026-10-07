//! The `audio` store: the PipeWire loop of [`super::thread`] run on the
//! service's own thread (`Start::Thread`, the contract's helper for
//! `!Send` libraries), its batches turned into the store's patches and
//! its writes and actions turned into [`AudioAction`]s.
//!
//! - **Messages.** The contract's notify hook pokes the loop
//!   ([`Cmd::Poke`]); the loop then drains the service's messages on its
//!   own thread ([`Host::poll`]). A stopped service (its messages closed)
//!   stops the loop.
//! - **Writes.** `audio.sink.volume = v` (and `.muted`) is a write of the
//!   default sink (`DeviceRef::DefaultSink`, resolved when the action
//!   runs, so a write that starts the service lands once PipeWire has
//!   synced); `s.volume = v` for `s` in `audio.sinks` is a write of the
//!   item's device by id. A write is answered ([`Cx::report`], tagged)
//!   by the first batch that shows its value on its device, so the
//!   logic thread ignores the echoes of a slider's earlier writes and
//!   settles on the answer of its last; a write PipeWire refused, or
//!   that shows nowhere within [`ANSWER_WAIT`] (it changed nothing), is
//!   answered with the device as it is.
//! - **Levels.** No schema field carries a level (decisions.md, wave4-wm
//!   (audio)): the peak meters run for [`tap_levels`] taps (the M4
//!   `spectrum` element's hook), and only while a reader of the service
//!   is visible: hidden, every meter stops.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::VecDiff;
use tokio::sync::oneshot;

use super::model::{AudioChange, AudioDevice, LevelTarget, Levels};
use super::{AudioAction, AudioConfig, AudioError, Cmd, DeviceRef, Host, thread};
use crate::{Call, Cx, Data, Msg, ServiceError, Step, Store, Write, service};

/// How long a write waits for PipeWire to show its value before it is
/// answered with the device as it is (it changed nothing, or PipeWire
/// settled elsewhere).
pub const ANSWER_WAIT: Duration = Duration::from_secs(1);

static CONFIG: Mutex<Option<AudioConfig>> = Mutex::new(None);

/// Where `audio` runs from now on connect (tests: a private PipeWire's
/// socket). `None`: PipeWire's own default (`$PIPEWIRE_REMOTE`, else
/// `pipewire-0`).
pub fn configure(config: Option<AudioConfig>) {
    if let Ok(mut c) = CONFIG.lock() {
        *c = config;
    }
}

fn config() -> AudioConfig {
    CONFIG
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .unwrap_or_default()
}

/// `audio`'s actions.
#[derive(Call, Debug)]
pub enum AudioDeviceAction {
    /// `dev.make_default()`.
    MakeDefault { item: AudioDevice },
}

/// Audio devices, from PipeWire.
#[service(name = "audio", schema = super::SCHEMA, action = AudioDeviceAction, thread)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct AudioStore {
    /// The default output: `audio.sink.volume`, `audio.sink.muted`.
    pub sink: AudioDevice,
    /// The default input.
    pub source: AudioDevice,
    /// Every output, keyed by `id`.
    #[store(keyed)]
    pub sinks: Vec<AudioDevice>,
    /// Every input, keyed by `id`.
    #[store(keyed)]
    pub sources: Vec<AudioDevice>,
}

// ---------------------------------------------------------------------------
// Level taps.

type TapFn = Arc<dyn Fn(&Levels) + Send + Sync>;

#[derive(Default)]
struct Taps {
    next: u64,
    taps: Vec<(u64, LevelTarget, TapFn)>,
    /// The running stores' pokes (their loops re-read the taps).
    loops: Vec<(u64, Box<dyn Fn() + Send + Sync>)>,
}

static TAPS: Mutex<Option<Taps>> = Mutex::new(None);

fn with_taps<R>(f: impl FnOnce(&mut Taps) -> R) -> R {
    let mut g = TAPS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Taps::default))
}

fn poke_loops() {
    with_taps(|t| {
        for (_, p) in &t.loops {
            p();
        }
    });
}

/// A subscription to one peak meter's readings: while it lives (and a
/// reader of `audio` is visible), the running `audio` service meters
/// `target` and calls the tap's function with each reading, on the audio
/// thread (it must not block). Dropping it stops the meter when no other
/// tap wants it.
pub struct LevelTap {
    id: u64,
}

impl std::fmt::Debug for LevelTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LevelTap").field("id", &self.id).finish()
    }
}

/// Taps the levels of `target`; see [`LevelTap`].
pub fn tap_levels(target: LevelTarget, f: impl Fn(&Levels) + Send + Sync + 'static) -> LevelTap {
    let id = with_taps(|t| {
        t.next += 1;
        let id = t.next;
        t.taps.push((id, target, Arc::new(f)));
        id
    });
    poke_loops();
    LevelTap { id }
}

impl Drop for LevelTap {
    fn drop(&mut self) {
        with_taps(|t| t.taps.retain(|(id, _, _)| *id != self.id));
        poke_loops();
    }
}

/// The targets tapped now.
fn tapped() -> BTreeSet<LevelTarget> {
    with_taps(|t| t.taps.iter().map(|(_, target, _)| *target).collect())
}

/// Gives `levels` to the taps of its target.
fn deliver(levels: &Levels) {
    let fns: Vec<TapFn> = with_taps(|t| {
        t.taps
            .iter()
            .filter(|(_, target, _)| *target == levels.target)
            .map(|(_, _, f)| f.clone())
            .collect()
    });
    for f in fns {
        f(levels);
    }
}

// ---------------------------------------------------------------------------
// The store's host.

/// Which device field a write changed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Attr {
    Volume(f64),
    Muted(bool),
}

/// Where a written device shows in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// `audio.sink` (`true`) or `audio.source`.
    Default(bool),
    /// An item of `audio.sinks` (`true`) or `audio.sources`.
    Item(bool, u32),
}

/// A write waiting for its answer.
struct Pending {
    write: Write,
    shown: Shown,
    attr: Attr,
    reply: Option<oneshot::Receiver<Result<(), AudioError>>>,
    since: Instant,
}

impl Pending {
    /// A write of the same device's same field.
    fn same(&self, shown: Shown, attr: Attr) -> bool {
        self.shown == shown && std::mem::discriminant(&self.attr) == std::mem::discriminant(&attr)
    }
}

/// The `audio` store's side of the loop.
struct StoreHost {
    cx: Cx<AudioStore>,
    pending: Vec<Pending>,
    /// The meters asked of the loop last.
    levels: BTreeSet<LevelTarget>,
    /// The service was stopped: the loop ends.
    stopped: bool,
    /// Its entry in the taps' poke list.
    loop_id: u64,
}

impl Drop for StoreHost {
    fn drop(&mut self) {
        let id = self.loop_id;
        with_taps(|t| t.loops.retain(|(i, _)| *i != id));
    }
}

/// `d`'s field `attr` as written.
fn holds(d: &AudioDevice, attr: Attr) -> bool {
    match attr {
        Attr::Volume(v) => d.volume == v,
        Attr::Muted(m) => d.muted == m,
    }
}

impl StoreHost {
    fn device(s: &AudioStore, shown: Shown) -> Option<&AudioDevice> {
        match shown {
            Shown::Default(true) => Some(&s.sink),
            Shown::Default(false) => Some(&s.source),
            Shown::Item(true, id) => s.sinks.iter().find(|d| d.id == id),
            Shown::Item(false, id) => s.sources.iter().find(|d| d.id == id),
        }
    }

    /// Answer `write` with the state as it is (plus `f`).
    fn answer(&mut self, write: &Write, f: impl FnOnce(&mut AudioStore)) {
        if !self.cx.report(write, f) {
            self.stopped = true;
        }
    }

    /// A write from the logic thread: its action, or its answer now.
    fn write(&mut self, w: Write) -> Option<Cmd> {
        let shown = match (w.field, &w.key) {
            ("sink", None) => Shown::Default(true),
            ("source", None) => Shown::Default(false),
            ("sinks" | "sources", Some(Data::Int(id))) => match u32::try_from(*id) {
                Ok(id) => Shown::Item(w.field == "sinks", id),
                Err(_) => {
                    self.answer(&w, |_| {});
                    return None;
                }
            },
            _ => {
                self.answer(&w, |_| {});
                return None;
            }
        };
        let device = match shown {
            Shown::Default(true) => DeviceRef::DefaultSink,
            Shown::Default(false) => DeviceRef::DefaultSource,
            Shown::Item(_, id) => DeviceRef::Id(id),
        };
        let leaf = match w.path.as_slice() {
            [Step::Field(f)] => f.as_str(),
            _ => "",
        };
        let current = Self::device(self.cx.state(), shown).cloned();
        let number = match &w.value {
            Data::Float(v) => Some(*v),
            Data::Int(n) => Some(*n as f64),
            _ => None,
        };
        let (action, attr) = match (leaf, number, &w.value) {
            ("volume", Some(v), _) if v.is_finite() => {
                // As the loop clamps: 0 to 1, or to an amplified volume.
                let top = current.as_ref().map_or(1.0, |d| d.volume.max(1.0));
                let v = v.clamp(0.0, top);
                (AudioAction::SetVolume(device, v), Attr::Volume(v))
            }
            ("muted", _, Data::Bool(m)) => (AudioAction::SetMuted(device, *m), Attr::Muted(*m)),
            _ => {
                // Not a writable leaf, or no number: the field as it is.
                self.answer(&w, |_| {});
                return None;
            }
        };
        let others = self.pending.iter().any(|p| p.same(shown, attr));
        let (reply_tx, reply_rx) = oneshot::channel();
        if !others && current.as_ref().is_some_and(|d| holds(d, attr)) {
            // Nothing to wait for: PipeWire may say nothing at all.
            self.answer(&w, |_| {});
            return Some(Cmd::Action(action, reply_tx));
        }
        self.pending.push(Pending {
            write: w,
            shown,
            attr,
            reply: Some(reply_rx),
            since: Instant::now(),
        });
        Some(Cmd::Action(action, reply_tx))
    }

    /// Takes the pending writes `state` answers: per device field, the
    /// last write whose value it shows (the earlier ones of that field
    /// are overtaken, and dropped: answering the last answers them on the
    /// logic thread), and every write PipeWire refused or that waited
    /// [`ANSWER_WAIT`] (answered with the device as it is).
    fn answered(&mut self, state: &AudioStore, by_value: bool) -> Vec<Write> {
        let now = Instant::now();
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.pending[i];
            let refused = match p.reply.as_mut().map(|r| r.try_recv()) {
                Some(Ok(Err(e))) => {
                    log::warn!("audio: {e}");
                    true
                }
                Some(Ok(Ok(()))) => {
                    p.reply = None;
                    false
                }
                Some(Err(oneshot::error::TryRecvError::Closed)) => true,
                Some(Err(oneshot::error::TryRecvError::Empty)) | None => false,
            };
            let late = now.saturating_duration_since(p.since) >= ANSWER_WAIT;
            if refused || late {
                out.push(self.pending.remove(i).write);
                continue;
            }
            i += 1;
        }
        let shows = |p: &Pending| Self::device(state, p.shown).is_some_and(|d| holds(d, p.attr));
        while let Some(last) = self.pending.iter().rposition(shows).filter(|_| by_value) {
            let (shown, attr) = (self.pending[last].shown, self.pending[last].attr);
            let answer = self.pending.remove(last);
            // The writes of that field before it are overtaken.
            let mut j = 0;
            let mut end = last;
            while j < end {
                if self.pending[j].same(shown, attr) {
                    self.pending.remove(j);
                    end -= 1;
                } else {
                    j += 1;
                }
            }
            out.push(answer.write);
        }
        out
    }

    /// Sends `next` (the state after a batch: `by_value`, or the state as
    /// it is, answering only refused and late writes): tagged as the
    /// answer of the writes it settles, else as an outside change.
    fn publish(&mut self, next: AudioStore, by_value: bool) {
        let answered = self.answered(&next, by_value);
        let mut answered = answered.into_iter();
        let ok = match answered.next() {
            None => {
                let mut patches = Vec::new();
                AudioStore::diff(self.cx.state(), &next, &mut patches);
                self.cx.send(patches)
            }
            Some(first) => {
                // The first answer carries the whole change; the others
                // only tag their fields.
                let mut ok = self.cx.report(&first, |s| *s = next);
                for w in answered {
                    ok &= self.cx.report(&w, |_| {});
                }
                ok
            }
        };
        if !ok {
            self.stopped = true;
        }
    }
}

/// Keyed diffs of the loop's lists (keyed by `i64`) as the store's
/// (keyed by the record's `u32` id).
fn store_diffs(diffs: Vec<VecDiff<i64, AudioDevice>>) -> Vec<VecDiff<u32, AudioDevice>> {
    let key = |k: i64| u32::try_from(k).unwrap_or_default();
    diffs
        .into_iter()
        .map(|d| match d {
            VecDiff::Reset { items } => VecDiff::Reset {
                items: items.into_iter().map(|(k, v)| (key(k), v)).collect(),
            },
            VecDiff::Insert {
                index,
                key: k,
                value,
            } => VecDiff::Insert {
                index,
                key: key(k),
                value,
            },
            VecDiff::Update {
                index,
                key: k,
                value,
            } => VecDiff::Update {
                index,
                key: key(k),
                value,
            },
            VecDiff::Remove { index, key: k } => VecDiff::Remove { index, key: key(k) },
            VecDiff::Move { from, to, key: k } => VecDiff::Move {
                from,
                to,
                key: key(k),
            },
        })
        .collect()
}

impl Host for StoreHost {
    fn changes(&mut self, batch: Vec<AudioChange>) {
        let mut patches = Vec::new();
        for change in batch {
            match change {
                AudioChange::Connected(c) => log::info!("audio: PipeWire connected: {c}"),
                AudioChange::Sinks(d) => patches.push(AudioStorePatch::Sinks(store_diffs(d))),
                AudioChange::Sources(d) => patches.push(AudioStorePatch::Sources(store_diffs(d))),
                AudioChange::Sink(d) => patches.push(AudioStorePatch::Sink(d.unwrap_or_default())),
                AudioChange::Source(d) => {
                    patches.push(AudioStorePatch::Source(d.unwrap_or_default()))
                }
                AudioChange::Levels(l) => deliver(&l),
            }
        }
        if patches.is_empty() {
            return;
        }
        let mut next = self.cx.state().clone();
        for p in &patches {
            next.apply(p);
        }
        self.publish(next, true);
        // The first batch holds every field.
        if !self.stopped && !self.cx.ready() {
            self.stopped = true;
        }
    }

    fn poll(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        loop {
            match self.cx.try_recv() {
                Ok(Msg::Write(w)) => cmds.extend(self.write(w)),
                Ok(Msg::Action(AudioDeviceAction::MakeDefault { item })) => {
                    // The outcome arrives in the stream; a refusal is logged.
                    let (tx, rx) = oneshot::channel();
                    cmds.push(Cmd::Action(
                        AudioAction::MakeDefault(DeviceRef::Id(item.id)),
                        tx,
                    ));
                    drop(rx);
                }
                Ok(Msg::Call(c, _)) => match c {},
                Ok(Msg::Visible(_) | Msg::Watch { .. }) => {}
                Err(false) => break,
                Err(true) => {
                    self.stopped = true;
                    break;
                }
            }
        }
        if self.stopped {
            return vec![Cmd::Stop];
        }
        // Writes refused or waiting too long are answered as things are.
        if self
            .pending
            .iter()
            .any(|p| p.reply.is_some() || p.since + ANSWER_WAIT <= Instant::now())
        {
            let now = self.cx.state().clone();
            self.publish(now, false);
        }
        if self.stopped {
            return vec![Cmd::Stop];
        }
        // Meters only for taps, and only while a reader is visible.
        let wanted = if self.cx.visible() {
            tapped()
        } else {
            BTreeSet::new()
        };
        if wanted != self.levels {
            self.levels = wanted.clone();
            cmds.push(Cmd::Levels(wanted));
        }
        cmds
    }

    fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|p| p.since + ANSWER_WAIT).min()
    }
}

impl AudioStore {
    fn run(cx: Cx<Self>) -> Result<(), ServiceError> {
        let (tx, rx) = pipewire::channel::channel::<Cmd>();
        let poke = tx.clone();
        cx.set_notify(move || {
            let _ = poke.send(Cmd::Poke);
        });
        let loop_id = with_taps(|t| {
            t.next += 1;
            let id = t.next;
            let poke = tx.clone();
            t.loops.push((
                id,
                Box::new(move || {
                    let _ = poke.send(Cmd::Poke);
                }),
            ));
            id
        });
        let host = StoreHost {
            cx,
            pending: Vec::new(),
            levels: BTreeSet::new(),
            stopped: false,
            loop_id,
        };
        // Messages sent before the notify hook was set are read now.
        let _ = tx.send(Cmd::Poke);
        thread::run(config(), Box::new(host), rx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Keyed, Service};

    #[test]
    fn the_store_is_the_schema_record() {
        assert_eq!(<AudioStore as Service>::NAME, "audio");
        let names: Vec<&str> = AudioStore::FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(names, ["sink", "source", "sinks", "sources"]);
        assert_eq!((AudioStore::FIELDS[0].ty)(), "AudioDevice");
        assert_eq!((AudioStore::FIELDS[2].ty)(), "[AudioDevice]");
        assert_eq!(AudioDevice::KEY_FIELD, "id");
        let d = store_diffs(vec![VecDiff::Remove { index: 0, key: 7 }]);
        assert!(matches!(d[0], VecDiff::Remove { index: 0, key: 7 }));
    }

    #[test]
    fn taps_come_and_go() {
        let before = tapped();
        let tap = tap_levels(LevelTarget::Device(4242), |_| {});
        assert!(tapped().contains(&LevelTarget::Device(4242)));
        let seen = Arc::new(Mutex::new(0));
        let s = seen.clone();
        let tap2 = tap_levels(LevelTarget::Device(4243), move |_| {
            *s.lock().unwrap() += 1;
        });
        deliver(&Levels {
            target: LevelTarget::Device(4243),
            device: 1,
            peaks: vec![0.5],
        });
        deliver(&Levels {
            target: LevelTarget::Device(4242),
            device: 1,
            peaks: vec![0.5],
        });
        assert_eq!(*seen.lock().unwrap(), 1);
        drop(tap);
        drop(tap2);
        assert_eq!(tapped(), before);
    }
}

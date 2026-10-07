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
//!   that shows nowhere within [`ANSWER_WAIT`] of PipeWire taking it (it
//!   changed nothing), is answered with the device as it is. Answers go
//!   in write order per cell (`audio.sink`, or one item of a list): the
//!   logic thread takes an answer tagged `g` as the answer of every
//!   write of the cell up to `g`, so a write that ended (a no-op
//!   `muted = false` after a `volume = 0.7`) is held until the earlier
//!   writes of its cell ended too, and answered with the state showing
//!   them.
//! - **Levels.** No schema field carries a level (decisions.md, wave4-wm
//!   (audio)): the peak meters run for [`tap_levels`] taps (the M4
//!   `spectrum` element's hook), and only while a reader of the service
//!   is visible: hidden, every meter stops.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

impl Attr {
    /// The same field (whatever the value).
    fn same(self, other: Attr) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

/// Where a written device shows in the store: the write's cell on the
/// logic thread (a field, or a keyed list's item), whose echo state
/// takes answers in generation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// `audio.sink` (`true`) or `audio.source`.
    Default(bool),
    /// An item of `audio.sinks` (`true`) or `audio.sources`.
    Item(bool, u32),
}

/// How a pending write ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Done {
    /// A batch showed its value (on a change of its device).
    Shown,
    /// Answered with the device as it is: it changed nothing, PipeWire
    /// refused it, or nothing showed it in time.
    AsIs,
}

/// A write waiting for its answer. A cell's writes are answered in
/// order: a write that ended is held while an earlier write of its cell
/// still waits, since the logic thread takes an answer tagged `g` as the
/// answer of every write of the cell up to `g`.
struct Pending {
    write: Write,
    shown: Shown,
    /// `None`: nothing to write (not a writable leaf, or no number).
    attr: Option<Attr>,
    /// PipeWire's reply to the action; `None` once it said yes.
    reply: Option<oneshot::Receiver<Result<(), AudioError>>>,
    /// When PipeWire was asked (its reply came back), until then when
    /// the write came.
    since: Instant,
    done: Option<Done>,
}

impl Pending {
    /// When it stops waiting for a batch: [`ANSWER_WAIT`] after PipeWire
    /// was asked, or [`REPLY_WAIT`] after it came with no reply at all.
    fn due(&self) -> Instant {
        self.since
            + if self.reply.is_some() {
                REPLY_WAIT
            } else {
                ANSWER_WAIT
            }
    }

    /// A write of `attr`'s field of cell `shown`.
    fn same(&self, shown: Shown, attr: Attr) -> bool {
        self.shown == shown && self.attr.is_some_and(|a| a.same(attr))
    }
}

/// How long a write waits for PipeWire's reply to its action (an action
/// waits for a connection's first state: up to [`super::GRACE`] with no
/// connection, and [`super::SETTLE`] for the session manager) before it
/// is answered as things are.
const REPLY_WAIT: Duration = Duration::from_secs(6);

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

    /// A write from the logic thread: its action. Its answer waits in
    /// [`Self::pending`], in its cell's order.
    fn write(&mut self, w: Write) -> Option<Cmd> {
        let shown = match (w.field, &w.key) {
            ("sink", None) => Shown::Default(true),
            ("source", None) => Shown::Default(false),
            ("sinks" | "sources", Some(Data::Int(id))) => match u32::try_from(*id) {
                Ok(id) => Shown::Item(w.field == "sinks", id),
                // No device has that id, so no write of it waits.
                Err(_) => {
                    self.answer(&w, |_| {});
                    return None;
                }
            },
            // Not a cell of the store: no write of it waits.
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
        let action = match (leaf, number, &w.value) {
            ("volume", Some(v), _) if v.is_finite() => {
                // As the loop clamps: 0 to 1, or to an amplified volume.
                let top = current.as_ref().map_or(1.0, |d| d.volume.max(1.0));
                let v = v.clamp(0.0, top);
                Some((AudioAction::SetVolume(device, v), Attr::Volume(v)))
            }
            ("muted", _, Data::Bool(m)) => {
                Some((AudioAction::SetMuted(device, *m), Attr::Muted(*m)))
            }
            // Not a writable leaf, or no number: the field as it is.
            _ => None,
        };
        let now = Instant::now();
        let Some((action, attr)) = action else {
            self.pending.push(Pending {
                write: w,
                shown,
                attr: None,
                reply: None,
                since: now,
                done: Some(Done::AsIs),
            });
            self.settle();
            return None;
        };
        // Nothing to wait for (PipeWire may say nothing at all) when the
        // device already holds the value and no earlier write of that
        // field may still change it.
        let earlier = self
            .pending
            .iter()
            .any(|p| p.same(shown, attr) && p.done.is_none());
        let held = !earlier && current.as_ref().is_some_and(|d| holds(d, attr));
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.push(Pending {
            write: w,
            shown,
            attr: Some(attr),
            reply: Some(reply_rx),
            since: now,
            done: held.then_some(Done::AsIs),
        });
        if held {
            self.settle();
        }
        Some(Cmd::Action(action, reply_tx))
    }

    /// Marks the writes that ended: those whose value `next` shows on a
    /// device it changed (from `prev`; only `by_value`), with the earlier
    /// writes of that field of the cell (overtaken); those PipeWire
    /// refused; and those that waited too long.
    fn mark(&mut self, prev: &AudioStore, next: &AudioStore, by_value: bool) {
        let now = Instant::now();
        for p in &mut self.pending {
            match p.reply.as_mut().map(|r| r.try_recv()) {
                Some(Ok(Err(e))) => {
                    log::warn!("audio: {e}");
                    p.reply = None;
                    p.done.get_or_insert(Done::AsIs);
                }
                Some(Ok(Ok(()))) => {
                    // PipeWire was asked just now: the answer clock starts.
                    p.reply = None;
                    p.since = now;
                }
                Some(Err(oneshot::error::TryRecvError::Closed)) => {
                    p.reply = None;
                    p.done.get_or_insert(Done::AsIs);
                }
                Some(Err(oneshot::error::TryRecvError::Empty)) | None => {}
            }
        }
        if by_value {
            for i in 0..self.pending.len() {
                let p = &self.pending[i];
                let (shown, Some(attr), None) = (p.shown, p.attr, p.done) else {
                    continue;
                };
                let after = Self::device(next, shown);
                if Self::device(prev, shown) != after && after.is_some_and(|d| holds(d, attr)) {
                    self.pending[i].done = Some(Done::Shown);
                    for q in &mut self.pending[..i] {
                        if q.same(shown, attr) {
                            q.done.get_or_insert(Done::Shown);
                        }
                    }
                }
            }
        }
        for p in &mut self.pending {
            if p.done.is_none() && now >= p.due() {
                if p.reply.is_some() {
                    log::warn!("audio: PipeWire did not take a write in time");
                }
                p.done = Some(Done::AsIs);
            }
        }
    }

    /// Takes the writes to answer now: per cell, the last of the run of
    /// ended writes at the front of its queue (on the logic thread, its
    /// answer answers the whole run).
    fn take_answers(&mut self) -> Vec<Write> {
        let mut cells: Vec<Shown> = Vec::new();
        for p in &self.pending {
            if !cells.contains(&p.shown) {
                cells.push(p.shown);
            }
        }
        let mut out = Vec::new();
        for cell in cells {
            let mut run = Vec::new();
            for (i, p) in self.pending.iter().enumerate() {
                if p.shown != cell {
                    continue;
                }
                if p.done.is_none() {
                    break;
                }
                run.push(i);
            }
            let mut answer = None;
            for i in run.into_iter().rev() {
                let p = self.pending.remove(i);
                answer.get_or_insert(p.write);
            }
            out.extend(answer);
        }
        out
    }

    /// Answers the writes that ended, with the state as it is.
    fn settle(&mut self) {
        let now = self.cx.state().clone();
        self.publish(now, false);
    }

    /// Sends `next` (the state after a batch, `by_value`; or the state as
    /// it is, answering only the refused, late and held writes): tagged
    /// as the answer of the writes it settles, else as an outside change.
    fn publish(&mut self, next: AudioStore, by_value: bool) {
        let prev = self.cx.state().clone();
        self.mark(&prev, &next, by_value);
        let mut answered = self.take_answers().into_iter();
        let ok = match answered.next() {
            None if prev == next => true,
            None => {
                let mut patches = Vec::new();
                AudioStore::diff(&prev, &next, &mut patches);
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

impl Host for StoreHost {
    fn changes(&mut self, batch: Vec<AudioChange>) {
        let mut patches = Vec::new();
        for change in batch {
            match change {
                AudioChange::Connected(c) => log::info!("audio: PipeWire connected: {c}"),
                AudioChange::Sinks(d) => patches.push(AudioStorePatch::Sinks(d)),
                AudioChange::Sources(d) => patches.push(AudioStorePatch::Sources(d)),
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
                    // The outcome arrives in the stream; the loop logs a
                    // refusal (nobody waits for its reply).
                    let (tx, _) = oneshot::channel();
                    cmds.push(Cmd::Action(
                        AudioAction::MakeDefault(DeviceRef::Id(item.id)),
                        tx,
                    ));
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
        // Replies read; writes refused or waiting too long are answered
        // as things are.
        let now = Instant::now();
        if self
            .pending
            .iter()
            .any(|p| p.reply.is_some() || p.due() <= now)
        {
            self.settle();
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
        self.pending
            .iter()
            .filter(|p| p.done.is_none())
            .map(Pending::due)
            .min()
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
        // No PipeWire library loop: the contract shows the error and
        // starts the service again (with backoff) while it is read.
        thread::run(config(), Box::new(host), rx).map_err(ServiceError)
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

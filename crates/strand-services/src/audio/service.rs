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
//!   item's device by id and `object.serial` (the serial the store shows
//!   under that id; a write made on an item of another device, one that
//!   left while its id was reused, names another `node.name` and changes
//!   nothing). A write is answered ([`Cx::report`], tagged)
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
//! - **Streams.** (services-round) `audio.streams` lists the programs'
//!   streams only while a reader reads it ([`Cx::watched`]): the loop
//!   binds the stream nodes then ([`Cmd::Streams`]), and lets them go
//!   when nobody reads it. A stream's `id` is its node's `object.serial`
//!   (PipeWire reuses node ids, never serials), so `st.volume = v` and
//!   `st.muted = m` from an item held past its stream's end reach no
//!   other stream: they are writes of the node that serial names now, as
//!   for a device's item, answered the same way, and refused when it is
//!   gone. A stream recording a sink's monitor (`stream.capture.sink`: a
//!   visualiser, desktop-audio capture) is `monitor`, not `input`.
//! - **Levels.** The peak meters run for [`tap_levels`] taps (the M4
//!   `spectrum` element's hook) and (services-round) for
//!   `audio.levels(dev, bars)`, and only while a reader of the service
//!   is visible: hidden, every meter stops. `audio.levels` asks for its
//!   device's meter each time it is evaluated ([`ask_levels`]), a reader
//!   until that evaluation re-runs or its scope goes, and reads its
//!   latest spectrum from `audio.meters`. A meter stops, and its item
//!   leaves `audio.meters`, [`LEVELS_LEASE`] after its last reader went
//!   (quiet or not), or once a reading goes that long without an
//!   evaluation asking again (its reader hidden while another keeps the
//!   service visible).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::model::{AudioChange, AudioDevice, AudioStream, LevelTarget, Levels};
use super::{AudioAction, AudioConfig, AudioError, Cmd, DeviceRef, Follow, Host, thread};
use crate::{Call, Cx, Data, Msg, ServiceError, Step, Store, Write, service};

/// How long a write waits for PipeWire to show its value before it is
/// answered with the device as it is (it changed nothing, or PipeWire
/// settled elsewhere).
pub const ANSWER_WAIT: Duration = Duration::from_secs(1);

static CONFIG: Mutex<Option<AudioConfig>> = Mutex::new(None);

/// Where `audio` runs from now on connect (tests: a private PipeWire's
/// socket). `None`: PipeWire's own default (`$PIPEWIRE_REMOTE`, else
/// `pipewire-0`).
/// The same PipeWire serves `video` (services-round).
pub fn configure(config: Option<AudioConfig>) {
    if let Ok(mut c) = CONFIG.lock() {
        *c = config;
    }
}

pub(crate) fn config() -> AudioConfig {
    CONFIG
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .unwrap_or_default()
}

/// (services-round) A peak meter `audio.levels` runs: an item of
/// `audio.meters`.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "AudioMeter", key = device)]
pub struct AudioMeter {
    /// The device it reads (`AudioDevice.id`).
    pub device: u32,
    /// Its latest spectrum: [`super::spectrum::BANDS`] bands from low to
    /// high pitch, each 0 to 1; empty while the device is quiet.
    pub bands: Vec<f64>,
}

/// (services-round) How long a meter `audio.levels` asked for runs after
/// a reading that no evaluation asked for again: its reader is gone.
pub const LEVELS_LEASE: Duration = Duration::from_secs(1);

/// The most bars `audio.levels` gives.
pub const MAX_BARS: usize = 512;

/// `bands` as `n` bars, each the loudest band it covers (a band repeats
/// over the bars it spans when there are more bars than bands); `n`
/// zeros for a quiet reading.
pub fn bars(bands: &[f64], n: usize) -> Vec<f64> {
    let len = bands.len();
    if len == 0 {
        return vec![0.0; n];
    }
    (0..n)
        .map(|i| {
            let lo = (i * len / n).min(len - 1);
            let hi = ((i + 1) * len).div_ceil(n).clamp(lo + 1, len);
            bands[lo..hi].iter().copied().fold(0.0, f64::max)
        })
        .collect()
}

/// One device `audio.levels` asks about.
#[derive(Debug, Default)]
struct Asked {
    /// The evaluations reading it now: each counts from its evaluation
    /// until it re-runs or its scope goes ([`release_levels`]).
    readers: usize,
    /// When a reading was shown that no evaluation asked for again since.
    shown: Option<Instant>,
    /// Such a reading went [`LEVELS_LEASE`] unasked (its reader is hidden
    /// while another keeps the service visible): the meter stops until an
    /// evaluation asks again.
    lapsed: bool,
    /// When the last reader went.
    unread_since: Option<Instant>,
}

/// The devices `audio.levels` asks about.
static ASKED: Mutex<BTreeMap<u32, Asked>> = Mutex::new(BTreeMap::new());

fn with_asked<R>(f: impl FnOnce(&mut BTreeMap<u32, Asked>) -> R) -> R {
    let mut g = ASKED.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

/// `audio.levels(dev, …)` was evaluated: one more reader of device
/// `id`'s meter, which runs (while a reader of `audio` is visible) until
/// that reader goes ([`release_levels`]) or a reading goes
/// [`LEVELS_LEASE`] unasked.
pub fn ask_levels(id: u32) {
    let new = with_asked(|a| {
        let e = a.entry(id).or_default();
        let new = (e.readers == 0 && e.unread_since.is_none()) || e.lapsed;
        e.readers += 1;
        e.shown = None;
        e.lapsed = false;
        e.unread_since = None;
        new
    });
    if new {
        poke_loops();
    }
}

/// A reader [`ask_levels`] counted went (its evaluation re-ran, or its
/// scope was disposed): the last one starts [`LEVELS_LEASE`], after
/// which the meter stops whether or not sound flows.
pub fn release_levels(id: u32) {
    let last = with_asked(|a| match a.get_mut(&id) {
        Some(e) if e.readers > 0 => {
            e.readers -= 1;
            if e.readers == 0 {
                e.unread_since = Some(Instant::now());
            }
            e.readers == 0
        }
        _ => false,
    });
    if last {
        poke_loops();
    }
}

/// The devices whose meter runs now; those nobody reads any more for
/// [`LEVELS_LEASE`] are let go. While no reader is visible no reading
/// lease runs (no meter reads, and a reader shown again may not be
/// evaluated again until its meter's first reading).
fn asked(now: Instant, visible: bool) -> BTreeSet<u32> {
    with_asked(|a| {
        a.retain(|_, e| e.unread_since.is_none_or(|t| now < t + LEVELS_LEASE));
        for e in a.values_mut() {
            if !visible {
                e.shown = None;
                e.lapsed = false;
            } else if e.shown.is_some_and(|t| now >= t + LEVELS_LEASE) {
                e.shown = None;
                e.lapsed = true;
            }
        }
        a.iter()
            .filter(|(_, e)| !e.lapsed)
            .map(|(id, _)| *id)
            .collect()
    })
}

/// When the next lease runs out.
fn lease_due() -> Option<Instant> {
    with_asked(|a| {
        a.values()
            .flat_map(|e| [e.shown, e.unread_since])
            .flatten()
            .map(|t| t + LEVELS_LEASE)
            .min()
    })
}

/// A reading of `id`'s meter was shown: its lease starts unless asked
/// again. `false` when nobody asks about `id`.
fn shown_levels(id: u32, now: Instant) -> bool {
    with_asked(|a| match a.get_mut(&id) {
        Some(e) if !e.lapsed => {
            e.shown.get_or_insert(now);
            true
        }
        _ => false,
    })
}

/// `audio.levels(dev, bars)`: asks for the device's meter and reads its
/// spectrum as `bars` bars.
fn audio_fns(
    cells: &AudioStoreCells,
    rt: &strand_core::Runtime,
    method: &str,
    args: &[Data],
) -> Option<Result<Data, strand_core::Error>> {
    if method != "levels" {
        return None;
    }
    let id = match args.first() {
        Some(Data::Int(id)) => u32::try_from(*id).ok(),
        Some(d @ Data::Record { .. }) => match d.field("id") {
            Some(Data::Int(id)) => u32::try_from(*id).ok(),
            _ => None,
        },
        _ => None,
    };
    let n = match args.get(1) {
        Some(Data::Int(n)) => (*n).clamp(0, MAX_BARS as i64) as usize,
        None | Some(Data::Null) => 16,
        Some(_) => {
            return Some(Err(strand_core::Error::failed(
                "audio.levels takes a number of bars".to_string(),
            )));
        }
    };
    let Some(id) = id else {
        // No device (the schema's default record, id 0, is no device
        // either): quiet.
        return Some(Ok(Data::List(vec![Data::Float(0.0); n])));
    };
    if id != 0 {
        // Read until this evaluation re-runs or its scope goes.
        ask_levels(id);
        rt.on_cleanup(move || release_levels(id));
    }
    Some(cells.meters.with(rt, |m| {
        let bands = m.get(&id).map(|m| m.bands.as_slice()).unwrap_or(&[]);
        Data::List(bars(bands, n).into_iter().map(Data::Float).collect())
    }))
}

/// `audio`'s actions.
#[derive(Call, Debug)]
pub enum AudioDeviceAction {
    /// `dev.make_default()`.
    MakeDefault { item: AudioDevice },
}

/// Audio devices, from PipeWire.
#[service(
    name = "audio",
    schema = super::SCHEMA,
    action = AudioDeviceAction,
    fns = audio_fns,
    thread
)]
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
    /// The programs' streams, keyed by `id`: what plays (a per-app
    /// mixer: `for st in audio.streams.filter(s => s.direction ==
    /// output) key st.id { slider { value: <-> st.volume } }`) and what
    /// records (a microphone indicator: `audio.streams.any(s => s.direction ==
    /// input)`). Followed only while a reader reads it. Peak meters
    /// (pavucontrol's, Strand's own) are left out.
    #[store(keyed, stream)]
    pub streams: Vec<AudioStream>,
    /// The meters `audio.levels` runs, keyed by `device`: each one's
    /// latest spectrum, 64 bands from low to high pitch, each 0 to 1
    /// (none while its device is quiet). Read through `audio.levels`,
    /// which asks for the meter; reading this field runs none.
    #[store(keyed)]
    pub meters: Vec<AudioMeter>,
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
    /// (services-round) An item of `audio.streams`.
    Stream(u32),
}

/// What a written cell shows: a device, or a program's stream.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Entry<'a> {
    Device(&'a AudioDevice),
    Stream(&'a AudioStream),
}

impl Entry<'_> {
    fn volume(self) -> f64 {
        match self {
            Entry::Device(d) => d.volume,
            Entry::Stream(s) => s.volume,
        }
    }

    fn muted(self) -> bool {
        match self {
            Entry::Device(d) => d.muted,
            Entry::Stream(s) => s.muted,
        }
    }
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
    /// The loop follows the programs' streams (`audio.streams` is read).
    streams: bool,
    /// Its entry in the taps' poke list.
    loop_id: u64,
    /// Each listed device's `object.serial`, by id
    /// ([`AudioChange::Serials`]).
    serials: BTreeMap<u32, u64>,
}

impl Drop for StoreHost {
    fn drop(&mut self) {
        let id = self.loop_id;
        with_taps(|t| t.loops.retain(|(i, _)| *i != id));
    }
}

/// `d`'s field `attr` as written.
fn holds(d: Entry<'_>, attr: Attr) -> bool {
    match attr {
        Attr::Volume(v) => d.volume() == v,
        Attr::Muted(m) => d.muted() == m,
    }
}

impl StoreHost {
    /// `item` (the record a write was made on, [`Write::held`]) is device
    /// `d`: it has its `node.name` (or no name to compare). A device's
    /// node name does not change while it lives; one that took a freed
    /// id has its own.
    fn names(item: &Data, d: Entry<'_>) -> bool {
        let (field, now) = match d {
            Entry::Device(d) => ("name", &d.name),
            // A stream has no node name to show: its program's.
            Entry::Stream(s) => ("app", &s.app),
        };
        match item.field(field) {
            Some(Data::Text(name)) => **name == **now,
            _ => true,
        }
    }

    /// (services-round) The node of the stream `audio.streams` shows as
    /// `id` (its serial): the node that has that serial now. One no node
    /// has (a stream that ended) is asked of node `id` with serial `id`,
    /// which is that stream if anything, so the write reaches no other
    /// stream (where PipeWire gives no serials the id is the node's own).
    fn stream_ref(&self, id: u32) -> DeviceRef {
        match self.serials.iter().find(|(_, s)| **s == u64::from(id)) {
            Some((node, serial)) => DeviceRef::Id {
                id: *node,
                serial: Some(*serial),
            },
            None => DeviceRef::Id {
                id,
                serial: Some(u64::from(id)),
            },
        }
    }

    /// The action `make_default()` on `item`: on its device by id and
    /// serial, or `None` when its id holds another device now.
    fn make_default(&self, item: &AudioDevice) -> Option<AudioAction> {
        let s = self.cx.state();
        let now = s.sinks.iter().chain(&s.sources).find(|d| d.id == item.id);
        if now.is_some_and(|d| d.name != item.name) {
            log::debug!(
                "audio: make_default() on {} from a device that left",
                item.id
            );
            return None;
        }
        Some(AudioAction::MakeDefault(DeviceRef::Id {
            id: item.id,
            serial: self.serials.get(&item.id).copied(),
        }))
    }

    fn device(s: &AudioStore, shown: Shown) -> Option<Entry<'_>> {
        match shown {
            Shown::Default(true) => Some(Entry::Device(&s.sink)),
            Shown::Default(false) => Some(Entry::Device(&s.source)),
            Shown::Item(true, id) => s.sinks.iter().find(|d| d.id == id).map(Entry::Device),
            Shown::Item(false, id) => s.sources.iter().find(|d| d.id == id).map(Entry::Device),
            Shown::Stream(id) => s.streams.iter().find(|d| d.id == id).map(Entry::Stream),
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
            ("sinks" | "sources" | "streams", Some(Data::Int(id))) => match u32::try_from(*id) {
                Ok(id) if w.field == "streams" => Shown::Stream(id),
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
            Shown::Item(_, id) => DeviceRef::Id {
                id,
                serial: self.serials.get(&id).copied(),
            },
            Shown::Stream(id) => self.stream_ref(id),
        };
        let leaf = match w.path.as_slice() {
            [Step::Field(f)] => f.as_str(),
            _ => "",
        };
        let state = self.cx.state();
        let current = Self::device(state, shown);
        // An item of the device that held the id before (a popup's item
        // kept past its device leaving, the id reused since): it is not
        // this device's write.
        let stale = matches!(shown, Shown::Item(..) | Shown::Stream(_))
            && w.held
                .as_ref()
                .zip(current)
                .is_some_and(|(held, d)| !Self::names(held, d));
        let top = current.map_or(1.0, |d| d.volume().max(1.0));
        let shows = current.map(|d| (d.volume(), d.muted()));
        let current_holds = move |attr| match (attr, shows) {
            (Attr::Volume(v), Some((volume, _))) => volume == v,
            (Attr::Muted(m), Some((_, muted))) => muted == m,
            (_, None) => false,
        };
        let number = match &w.value {
            Data::Float(v) => Some(*v),
            Data::Int(n) => Some(*n as f64),
            _ => None,
        };
        let action = match (leaf, number, &w.value) {
            _ if stale => {
                log::debug!(
                    "audio: a write of {} from a device that left: {device:?}",
                    w.field
                );
                None
            }
            ("volume", Some(v), _) if v.is_finite() => {
                // As the loop clamps: 0 to 1, or to an amplified volume.
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
        let held = !earlier && current_holds(attr);
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
        let mut readings = Vec::new();
        for change in batch {
            match change {
                AudioChange::Connected(c) => log::info!("audio: PipeWire connected: {c}"),
                AudioChange::Sinks(d) => patches.push(AudioStorePatch::Sinks(d)),
                AudioChange::Sources(d) => patches.push(AudioStorePatch::Sources(d)),
                AudioChange::Streams(d) => patches.push(AudioStorePatch::Streams(d)),
                // The `video` service's loop's; never sent to this one.
                AudioChange::Video(_) => {}
                AudioChange::Sink(d) => patches.push(AudioStorePatch::Sink(d.unwrap_or_default())),
                AudioChange::Source(d) => {
                    patches.push(AudioStorePatch::Source(d.unwrap_or_default()))
                }
                AudioChange::Levels(l) => {
                    deliver(&l);
                    if let LevelTarget::Device(id) = l.target {
                        readings.push((id, l.bins));
                    }
                }
                AudioChange::Serials(s) => self.serials = s,
            }
        }
        let now = Instant::now();
        readings.retain(|(id, _)| shown_levels(*id, now));
        if patches.is_empty() && readings.is_empty() {
            return;
        }
        let mut next = self.cx.state().clone();
        for p in &patches {
            next.apply(p);
        }
        for (device, bins) in readings {
            let bands = bins.into_iter().map(f64::from).collect();
            match next.meters.iter_mut().find(|m| m.device == device) {
                Some(m) => m.bands = bands,
                None => next.meters.push(AudioMeter { device, bands }),
            }
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
                    if let Some(action) = self.make_default(&item) {
                        let (tx, _) = oneshot::channel();
                        cmds.push(Cmd::Action(action, tx));
                    }
                }
                Ok(Msg::Call(c, _)) => match c {},
                Ok(Msg::Visible(_) | Msg::Watch { .. } | Msg::Keep(_)) => {}
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
        // Meters only for taps and `audio.levels`, and only while a
        // reader is visible; a meter no reader asks for any more leaves
        // `audio.meters`.
        let visible = self.cx.visible();
        let asked = asked(now, visible);
        if self
            .cx
            .state()
            .meters
            .iter()
            .any(|m| !asked.contains(&m.device))
        {
            let mut next = self.cx.state().clone();
            next.meters.retain(|m| asked.contains(&m.device));
            self.publish(next, false);
            if self.stopped {
                return vec![Cmd::Stop];
            }
        }
        let wanted = if visible {
            let mut t = tapped();
            t.extend(asked.iter().map(|id| LevelTarget::Device(*id)));
            t
        } else {
            BTreeSet::new()
        };
        if wanted != self.levels {
            self.levels = wanted.clone();
            cmds.push(Cmd::Levels(wanted));
        }
        // Streams only while read.
        let streams = self.cx.watched("streams");
        if streams != self.streams {
            self.streams = streams;
            cmds.push(Cmd::Streams(streams));
        }
        cmds
    }

    fn deadline(&self) -> Option<Instant> {
        self.pending
            .iter()
            .filter(|p| p.done.is_none())
            .map(Pending::due)
            .chain(lease_due())
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
            streams: false,
            loop_id,
            serials: BTreeMap::new(),
        };
        // Messages sent before the notify hook was set are read now.
        let _ = tx.send(Cmd::Poke);
        // No PipeWire library loop: the contract shows the error and
        // starts the service again (with backoff) while it is read.
        thread::run(config(), Follow::Audio, Box::new(host), rx).map_err(ServiceError)
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
        assert_eq!(
            names,
            ["sink", "source", "sinks", "sources", "streams", "meters"]
        );
        assert_eq!((AudioStore::FIELDS[0].ty)(), "AudioDevice");
        assert_eq!((AudioStore::FIELDS[2].ty)(), "[AudioDevice]");
        assert_eq!(AudioDevice::KEY_FIELD, "id");
    }

    #[test]
    fn bands_become_bars() {
        let bands: Vec<f64> = (0..64).map(|i| f64::from(i) / 63.0).collect();
        // Fewer bars: each the loudest band it covers.
        let four = bars(&bands, 4);
        assert_eq!(four.len(), 4);
        assert_eq!(four[3], 1.0);
        assert_eq!(four[0], 15.0 / 63.0);
        // One bar: the loudest band of all (a VU meter).
        assert_eq!(bars(&bands, 1), [1.0]);
        // More bars than bands: each band over the bars it spans.
        let many = bars(&[0.25, 0.75], 4);
        assert_eq!(many, [0.25, 0.25, 0.75, 0.75]);
        // A quiet reading: zeros; none asked: none.
        assert_eq!(bars(&[], 3), [0.0, 0.0, 0.0]);
        assert!(bars(&bands, 0).is_empty());
    }

    #[test]
    fn an_unasked_reading_ends_the_lease() {
        let t0 = Instant::now();
        ask_levels(9_001);
        assert!(asked(t0, true).contains(&9_001));
        assert!(shown_levels(9_001, t0));
        // Asked again after the reading: the lease starts anew.
        ask_levels(9_001);
        assert!(asked(t0 + LEVELS_LEASE * 2, true).contains(&9_001));
        assert!(shown_levels(9_001, t0));
        assert!(asked(t0 + LEVELS_LEASE / 2, true).contains(&9_001));
        // Hidden, no lease runs out; shown again, it starts at the next
        // reading.
        assert!(asked(t0 + LEVELS_LEASE * 5, false).contains(&9_001));
        assert!(asked(t0 + LEVELS_LEASE * 6, true).contains(&9_001));
        assert!(shown_levels(9_001, t0 + LEVELS_LEASE * 6));
        assert!(!asked(t0 + LEVELS_LEASE * 7, true).contains(&9_001));
        // Lapsed, its readings are not shown, until asked again.
        assert!(!shown_levels(9_001, t0 + LEVELS_LEASE * 7));
        ask_levels(9_001);
        assert!(asked(t0 + LEVELS_LEASE * 8, true).contains(&9_001));
        // A reading nobody asked for is not shown.
        assert!(!shown_levels(9_002, t0));
    }

    /// (services-round) A device asked about while quiet sends no
    /// reading, so no reading lease runs: its meter stops once its
    /// readers have gone, [`LEVELS_LEASE`] after the last.
    #[test]
    fn a_quiet_meter_stops_once_its_readers_go() {
        ask_levels(9_101);
        ask_levels(9_101);
        let t0 = Instant::now();
        assert!(asked(t0 + LEVELS_LEASE * 3, true).contains(&9_101));
        release_levels(9_101);
        assert!(asked(Instant::now() + LEVELS_LEASE * 3, true).contains(&9_101));
        release_levels(9_101);
        let gone = Instant::now();
        assert!(asked(gone, true).contains(&9_101), "within the lease");
        assert!(lease_due().is_some());
        assert!(!asked(gone + LEVELS_LEASE, true).contains(&9_101));
        // A release with nobody counted changes nothing.
        release_levels(9_101);
        assert!(!asked(gone + LEVELS_LEASE, true).contains(&9_101));
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
            bins: Vec::new(),
        });
        deliver(&Levels {
            target: LevelTarget::Device(4242),
            device: 1,
            peaks: vec![0.5],
            bins: Vec::new(),
        });
        assert_eq!(*seen.lock().unwrap(), 1);
        drop(tap);
        drop(tap2);
        assert_eq!(tapped(), before);
    }
}

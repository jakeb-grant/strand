//! A service's side of the protocol: [`Cx`].
//!
//! A service body owns a copy of its state ([`Cx::state`]). It changes it
//! with [`Cx::update`] (the changed fields go to the logic thread as one
//! [`Envelope`], applied in one tick), emits events with [`Cx::emit`],
//! answers writes with [`Cx::report`] and says when its first read is
//! complete with [`Cx::ready`]. Messages from the logic thread (writes,
//! actions, async calls, visibility) arrive through [`Cx::recv`] (or
//! [`Cx::blocking_recv`] on a thread of its own); `None` means the
//! service was stopped and the body should return.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use strand_core::Generation;
use tokio::sync::{mpsc, oneshot};

use crate::bus::{self, Buses};
use crate::data::{Data, DataError, FromData, Step, ToData};
use crate::service::Service;
use crate::store::{Patch, Target};

/// What travels from a service thread to the logic thread.
#[derive(Debug)]
pub enum Envelope<P> {
    /// Patches of one update, applied together in one tick.
    Patches {
        patches: Vec<P>,
        /// The local write this answers ([`Cx::report`]): the written
        /// field's index and the write's generation. Only that field's
        /// patch is matched against its pending writes; the update's
        /// other patches are outside changes.
        echo_of: Option<(usize, Generation)>,
        /// Sent before [`Cx::ready`]: boot values (`on change` does not
        /// fire for them).
        initial: bool,
    },
    /// Something the user must act on ([`Cx::notice`]).
    Notice(String),
    /// The run's notice no longer holds ([`Cx::resolve`]).
    Resolved,
    /// A failure the run survives and retries ([`Cx::warn`]).
    Warning(String),
    /// The first read is complete.
    Ready,
    /// The body returned (with its error, if any).
    Ended(Result<(), String>),
}

/// Wakes the logic thread when an envelope arrives: the host's waker (a
/// calloop ping) plus a counter [`crate::Services::wait_ready`] blocks on.
pub(crate) struct Wake {
    host: Box<dyn Fn() + Send + Sync>,
    count: Mutex<u64>,
    cond: Condvar,
}

impl fmt::Debug for Wake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Wake")
    }
}

impl Wake {
    pub(crate) fn new(host: impl Fn() + Send + Sync + 'static) -> Wake {
        Wake {
            host: Box::new(host),
            count: Mutex::new(0),
            cond: Condvar::new(),
        }
    }

    pub(crate) fn wake(&self) {
        // The host first: whoever `wait_past` releases sees it woken.
        (self.host)();
        if let Ok(mut c) = self.count.lock() {
            *c += 1;
        }
        self.cond.notify_all();
    }

    pub(crate) fn count(&self) -> u64 {
        self.count.lock().map_or(0, |c| *c)
    }

    /// Wait until the count moves past `seen`, at most `limit`.
    pub(crate) fn wait_past(&self, seen: u64, limit: std::time::Duration) {
        let Ok(guard) = self.count.lock() else {
            return;
        };
        let _ = self.cond.wait_timeout_while(guard, limit, |c| *c == seen);
    }
}

/// The sending half of a run's envelope channel.
pub(crate) struct Out<P> {
    pub(crate) tx: std::sync::mpsc::Sender<Envelope<P>>,
    pub(crate) wake: Arc<Wake>,
}

impl<P> Out<P> {
    pub(crate) fn send(&self, e: Envelope<P>) -> bool {
        let ok = self.tx.send(e).is_ok();
        if ok {
            self.wake.wake();
        }
        ok
    }
}

/// A callback a service on its own thread sets to be told a message is
/// waiting ([`Cx::set_notify`]).
pub(crate) type Notify = Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>;

/// A local write of an `rw` field (or of a leaf inside one:
/// `audio.sink.volume`), or of an `rw` leaf inside an item of a keyed
/// list (`s.volume` for `s` in `audio.sinks`), passed to the service.
/// Answer it with [`Cx::report`].
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    /// The service field written (`sink`), or the keyed list holding the
    /// written item (`sinks`).
    pub field: &'static str,
    /// An item write: the item's key (`Data::Int(42)` for the sink with
    /// id 42). `None` for a field write.
    pub key: Option<Data>,
    /// The path below the field, or below the item (`[.volume]`; empty
    /// for the field itself).
    pub path: Vec<Step>,
    /// The value written at `path`.
    pub value: Data,
    /// The field's whole new value, or the item's (the list's item under
    /// `key` now, with the write applied).
    pub field_value: Data,
    /// An item write: the item as the writer held it (`s` in `s.volume =
    /// v`), which may be older than the list's item under the same key: a
    /// record kept past its item leaving (a popup's), whose key the
    /// service has since given to another item, as PipeWire reuses ids.
    /// A service whose keys are reused tells them apart with it. `None`
    /// for a field write.
    pub held: Option<Box<Data>>,
    /// The write's tag: [`Cx::report`] it back so the echo is ignored.
    pub generation: Generation,
}

impl Write {
    /// The written value, typed.
    pub fn value<T: FromData>(&self) -> Result<T, DataError> {
        T::from_data(&self.value)
    }

    /// The field's whole new value, typed.
    pub fn field_value<T: FromData>(&self) -> Result<T, DataError> {
        T::from_data(&self.field_value)
    }
}

/// The answer slot of an async method call (`apps.search(q)`): the
/// `Async` the call returned completes with what is sent. Dropped
/// unanswered, the call fails. The caller may give up first (a newer
/// query superseded it): [`Reply::is_closed`].
#[derive(Debug)]
pub struct Reply(pub(crate) oneshot::Sender<Result<Data, String>>);

impl Reply {
    /// Complete the call.
    pub fn send<T: ToData, E: fmt::Display>(self, r: Result<T, E>) {
        let _ = self
            .0
            .send(r.map(|v| v.to_data()).map_err(|e| e.to_string()));
    }

    /// The caller no longer waits (cancelled or superseded).
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Resolves when the caller stops waiting.
    pub async fn closed(&mut self) {
        self.0.closed().await;
    }
}

/// A message from the logic thread.
// A `Write` is moved once per local write, through the run's channel;
// boxing it would cost an allocation per write for no gain.
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum Msg<S: Service> {
    /// A local write ([`Write`]).
    Write(Write),
    /// An action (`notifications.clear()`).
    Action(S::Action),
    /// An async method call, to answer through the [`Reply`].
    Call(S::Call, Reply),
    /// Whether a reader is visible now. [`Cx::visible`] already says so;
    /// a service streaming only while visible (a polled sensor) starts
    /// or stops here.
    Visible(bool),
    /// Whether a visible reader reads `#[store(stream)]` field `field`
    /// now. [`Cx::watched`] already says so; the stream behind it (a
    /// Wi-Fi scan, audio levels) starts or stops here.
    Watch { field: &'static str, on: bool },
    /// (history) How often to sample while no reader is visible, for the
    /// history a `graph` keeps of the service's values (`None`: not at
    /// all, as before). [`Cx::kept`] and [`Cx::period`] already say so; a
    /// sampled service reschedules here ([`Ticker::follow`]).
    Keep(Option<Duration>),
}

impl<S: Service> fmt::Debug for Msg<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Msg::Write(w) => f.debug_tuple("Write").field(w).finish(),
            Msg::Action(_) => f.write_str("Action(..)"),
            Msg::Call(..) => f.write_str("Call(..)"),
            Msg::Visible(v) => f.debug_tuple("Visible").field(v).finish(),
            Msg::Watch { field, on } => f
                .debug_struct("Watch")
                .field("field", field)
                .field("on", on)
                .finish(),
            Msg::Keep(every) => f.debug_tuple("Keep").field(every).finish(),
        }
    }
}

/// A service's context; see the module docs.
pub struct Cx<S: Service> {
    state: S,
    out: Out<S::Patch>,
    msgs: mpsc::UnboundedReceiver<(Msg<S>, Option<String>)>,
    /// (M5) The xdg-activation token the last message came with.
    token: Option<String>,
    visible: bool,
    /// (history) How often to sample while hidden ([`Msg::Keep`]).
    kept: Option<Duration>,
    /// (history) Where [`Cx::period`] tells its client the visible
    /// period it samples at (nanoseconds), so a hidden sample is known
    /// to be polled.
    clocked: Option<Arc<AtomicU64>>,
    /// Per field: a visible reader reads it (meaningful for stream
    /// fields).
    watched: Vec<bool>,
    ready: bool,
    buses: Buses,
    notify: Notify,
    /// Where the actions still queued when the body lets its context go
    /// are left, for the logic thread to refuse as the run ends
    /// ([`Cx`]'s `Drop`).
    unread: Unread<S>,
}

/// The actions a run's body never read (see [`Cx`]'s `Drop`).
pub(crate) type Unread<S> = Arc<Mutex<Vec<<S as Service>::Action>>>;

/// (m6-owner-fixes, review of the integration) A body that ends without
/// reading its queue (it failed connecting before its first `recv`, or
/// returned) leaves its queued actions here, so the client refuses them
/// as the run ends ([`Service::refused`]) instead of dropping them with
/// the channel.
impl<S: Service> Drop for Cx<S> {
    fn drop(&mut self) {
        self.msgs.close();
        let mut left = Vec::new();
        while let Ok((m, _)) = self.msgs.try_recv() {
            if let Msg::Action(a) = m {
                left.push(a);
            }
        }
        if !left.is_empty() {
            self.unread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(left);
        }
    }
}

impl<S: Service> fmt::Debug for Cx<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cx")
            .field("service", &S::NAME)
            .field("visible", &self.visible)
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

impl<S: Service> Cx<S> {
    pub(crate) fn new(
        state: S,
        out: Out<S::Patch>,
        msgs: mpsc::UnboundedReceiver<(Msg<S>, Option<String>)>,
        buses: Buses,
        notify: Notify,
        watched: Vec<bool>,
        unread: Unread<S>,
    ) -> Cx<S> {
        Cx {
            state,
            out,
            msgs,
            token: None,
            visible: true,
            kept: None,
            clocked: None,
            watched,
            ready: false,
            buses,
            notify,
            unread,
        }
    }

    /// The service's name.
    pub fn name(&self) -> &'static str {
        S::NAME
    }

    /// The state as last sent (the logic thread's values when the service
    /// started, then every update).
    pub fn state(&self) -> &S {
        &self.state
    }

    /// Change the state; the fields that changed go to the logic thread
    /// as one envelope (one tick). Returns `false` once the service was
    /// stopped (nothing listens any more: return from the body).
    pub fn update(&mut self, f: impl FnOnce(&mut S)) -> bool {
        self.update_with(f, None)
    }

    fn update_with(
        &mut self,
        f: impl FnOnce(&mut S),
        answer: Option<(&'static str, Generation, Option<&Data>)>,
    ) -> bool {
        let mut new = self.state.clone();
        f(&mut new);
        let mut patches = Vec::new();
        S::diff(&self.state, &new, &mut patches);
        self.state = new;
        // A write's answer always names its field (an item write's, its
        // item), even when the service kept the old value (refused it), so
        // the optimistic local value does not stay. Only that field is
        // tagged with the write.
        let mut echo_of = None;
        if let Some((field, generation, key)) = answer
            && let Some(i) = S::FIELDS.iter().position(|f| f.name == field)
        {
            echo_of = Some((i, generation));
            let extra = match key {
                Some(key) => S::item_patch(&self.state, i, key, &patches),
                None if !patches.iter().any(|p| p.target() == Target::Field(i)) => {
                    S::field_patch(&self.state, i)
                }
                None => None,
            };
            patches.extend(extra);
        }
        if patches.is_empty() {
            return !self.stopped();
        }
        self.out.send(Envelope::Patches {
            patches,
            echo_of,
            initial: !self.ready,
        })
    }

    /// Send patches made by hand (a keyed list's diffs, without diffing
    /// the whole list); they are applied to [`Cx::state`] too.
    pub fn send(&mut self, patches: Vec<S::Patch>) -> bool {
        for p in &patches {
            self.state.apply(p);
        }
        if patches.is_empty() {
            return !self.stopped();
        }
        self.out.send(Envelope::Patches {
            patches,
            echo_of: None,
            initial: !self.ready,
        })
    }

    /// Emit an event (`<Name>Event::Received(n)`): lossless, one
    /// delivery per emit.
    pub fn emit(&mut self, event: S::Event) -> bool {
        self.out.send(Envelope::Patches {
            patches: vec![event.into()],
            echo_of: None,
            initial: false,
        })
    }

    /// Answer `write`: change the state as the service now has it
    /// (the written value, or what it settled on). The written field (an
    /// item write's item) is always reported, tagged with the write's
    /// generation, so the logic thread ignores the echo of a write it
    /// already shows.
    pub fn report(&mut self, write: &Write, f: impl FnOnce(&mut S)) -> bool {
        self.update_with(f, Some((write.field, write.generation, write.key.as_ref())))
    }

    /// The first read is complete: until now updates were boot values
    /// (`on change` takes them as its baseline), from now on they are
    /// changes. Ends the first frame's wait for this service.
    pub fn ready(&mut self) -> bool {
        if self.ready {
            return true;
        }
        self.ready = true;
        self.out.send(Envelope::Ready)
    }

    /// Tell the user something they must act on (another notification
    /// server owns the name): a [`crate::ServiceDiagnostic`] the host
    /// shows (`strand run`: the overlay and `strand watch`), not only a
    /// log line. Repeats of the same notice are dropped. Raise it before
    /// [`Cx::ready`]: a run that becomes ready without one resolves the
    /// last notice (the host takes it away), as a clean stop does.
    /// `false` once stopped.
    pub fn notice(&mut self, message: impl Into<String>) -> bool {
        self.out.send(Envelope::Notice(message.into()))
    }

    /// The notice this run raised no longer holds, while the run goes on
    /// (the name another server held is ours now): the host takes it
    /// away. `false` once stopped.
    pub fn resolve(&mut self) -> bool {
        self.out.send(Envelope::Resolved)
    }

    /// Report a failure the run survives and retries on its own (the
    /// compositor adapter does not understand the compositor, and the
    /// standard protocols serve meanwhile): a [`crate::ServiceDiagnostic`]
    /// with `notice: false`, as a failed run's, which the host logs and
    /// sends to `strand watch` but does not put on the overlay. Repeats of
    /// the last one are dropped as a failed run's are. `false` once
    /// stopped.
    pub fn warn(&mut self, message: impl Into<String>) -> bool {
        self.out.send(Envelope::Warning(message.into()))
    }

    /// Whether [`Cx::ready`] was called.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// A reader of the service is visible (it was acquired and is not
    /// hidden). Streams run only while this holds.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// (history) Start kept every `kept` ([`Client::keep`]); with no
    /// reader (`read` false) a kept service starts hidden. One started
    /// otherwise starts visible, as before.
    ///
    /// [`Client::keep`]: crate::Client::keep
    pub(crate) fn start_kept(
        &mut self,
        read: bool,
        kept: Option<Duration>,
        clocked: Arc<AtomicU64>,
    ) {
        self.kept = kept;
        self.visible = read || kept.is_none();
        self.clocked = Some(clocked);
    }

    /// (history) How often to sample while no reader is visible: a
    /// `graph` in the loaded config keeps history of this service's
    /// values. `None` (no such graph): a hidden service samples nothing.
    pub fn kept(&self) -> Option<Duration> {
        self.kept
    }

    /// (history) How often a service sampling every `visible` while a
    /// reader is visible samples now: `visible` while one is, the kept
    /// hidden period ([`Cx::kept`], never faster than `visible`) while
    /// none is, and `None` (not at all) when nothing keeps it. A polled
    /// service drives its clock with this ([`Ticker`]); asking it tells
    /// the service's client that it samples every `visible`, so its
    /// hidden samples are drawn as ramps over the period (a value it
    /// reports as it changes is drawn as a step at its time).
    pub fn period(&self, visible: Duration) -> Option<Duration> {
        if let Some(c) = &self.clocked {
            c.store(
                u64::try_from(visible.as_nanos()).unwrap_or(u64::MAX).max(1),
                Ordering::Relaxed,
            );
        }
        if self.visible {
            Some(visible)
        } else {
            self.kept.map(|k| k.max(visible))
        }
    }

    /// A visible reader reads `#[store(stream)]` field `field` now: its
    /// stream (a scan, a level meter) runs only while this holds
    /// ([`Msg::Watch`] says when it changes). Unknown names are `false`.
    pub fn watched(&self, field: &str) -> bool {
        S::FIELDS
            .iter()
            .position(|f| f.name == field)
            .and_then(|i| self.watched.get(i).copied())
            .unwrap_or(false)
    }

    /// The next message (shared runtime). `None`: the service was
    /// stopped; return from the body.
    pub async fn recv(&mut self) -> Option<Msg<S>> {
        let m = self.msgs.recv().await;
        self.seen(m)
    }

    /// The next message, blocking (a service on its own thread). `None`:
    /// stopped.
    pub fn blocking_recv(&mut self) -> Option<Msg<S>> {
        let m = self.msgs.blocking_recv();
        self.seen(m)
    }

    /// The next message if one is waiting. `Err(true)`: stopped.
    pub fn try_recv(&mut self) -> Result<Msg<S>, bool> {
        match self.msgs.try_recv() {
            Ok(m) => Ok(self.take(m)),
            Err(mpsc::error::TryRecvError::Empty) => Err(false),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(true),
        }
    }

    /// The service was stopped (its messages end): return from the body.
    pub fn stopped(&self) -> bool {
        self.msgs.is_closed() && self.msgs.is_empty()
    }

    /// (M5) The xdg-activation token the action just received came with
    /// (one of [`Service::ACTIVATION`]'s, from a handler a press started,
    /// answered within `TOKEN_WAIT`), for the app it raises: `None` for
    /// every other message, and when no token came.
    pub fn activation_token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn seen(&mut self, m: Option<(Msg<S>, Option<String>)>) -> Option<Msg<S>> {
        m.map(|m| self.take(m))
    }

    fn take(&mut self, (m, token): (Msg<S>, Option<String>)) -> Msg<S> {
        self.token = token;
        self.note(&m);
        m
    }

    fn note(&mut self, m: &Msg<S>) {
        match m {
            Msg::Visible(v) => self.visible = *v,
            Msg::Keep(k) => self.kept = *k,
            Msg::Watch { field, on } => {
                if let Some(i) = S::FIELDS.iter().position(|f| f.name == *field)
                    && let Some(w) = self.watched.get_mut(i)
                {
                    *w = *on;
                }
            }
            _ => {}
        }
    }

    /// A service on its own thread (with its own event loop: PipeWire's)
    /// is told through `f` whenever a message is waiting and when it is
    /// stopped; it then drains [`Cx::try_recv`].
    pub fn set_notify(&self, f: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut n) = self.notify.lock() {
            *n = Some(Box::new(f));
        }
    }

    /// The buses this runtime uses.
    pub fn buses(&self) -> &Buses {
        &self.buses
    }

    /// The session bus (one connection shared by the services of this
    /// runtime thread). It needs a tokio runtime: a service on a thread
    /// of its own runs its own (or gets an error, never a panic).
    pub async fn session(&self) -> zbus::Result<zbus::Connection> {
        bus::session(&self.buses).await
    }

    /// The system bus (one connection shared by the services of this
    /// runtime thread).
    pub async fn system(&self) -> zbus::Result<zbus::Connection> {
        bus::system(&self.buses).await
    }
}

/// (history) A polled service's clock: it ticks every [`Cx::period`] of
/// the service's visible period, so every `visible` while a reader is
/// visible, at the kept hidden period while none is and a `graph` keeps
/// history of the service, and never otherwise (true idle).
#[derive(Debug)]
pub struct Ticker {
    visible: Duration,
    every: Option<Duration>,
    interval: Option<tokio::time::Interval>,
}

impl Ticker {
    /// A clock for a service sampling every `visible` while shown; its
    /// first tick is one period from now.
    pub fn new<S: Service>(cx: &Cx<S>, visible: Duration) -> Ticker {
        let every = cx.period(visible);
        Ticker {
            visible,
            every,
            interval: every.map(interval),
        }
    }

    /// Follow the period [`Cx::period`] asks for now (call after each
    /// message): a changed period starts again one new period from now.
    /// `true` when it starts ticking after not ticking at all (a sampler
    /// taking differences takes a fresh baseline first).
    pub fn follow<S: Service>(&mut self, cx: &Cx<S>) -> bool {
        let every = cx.period(self.visible);
        if every == self.every {
            return false;
        }
        let resumed = self.every.is_none();
        self.every = every;
        self.interval = every.map(interval);
        resumed && every.is_some()
    }

    /// (services-round) A clock ticking every `every` (`None`: not at
    /// all), for fields that choose their own period rather than the
    /// service's ([`Cx::period`]): `network`'s rates sample once a second
    /// only while read, and at the kept period otherwise, whatever other
    /// field of the service is shown. Its first tick is one period from
    /// now.
    pub fn at(every: Option<Duration>) -> Ticker {
        Ticker {
            visible: every.unwrap_or(Duration::ZERO),
            every,
            interval: every.map(interval),
        }
    }

    /// Follow a period chosen by the caller ([`Ticker::at`]); as
    /// [`Ticker::follow`] otherwise.
    pub fn follow_every(&mut self, every: Option<Duration>) -> bool {
        if every == self.every {
            return false;
        }
        let resumed = self.every.is_none();
        self.every = every;
        self.interval = every.map(interval);
        resumed && every.is_some()
    }

    /// The next tick is one period from now.
    pub fn reset(&mut self) {
        if let Some(i) = &mut self.interval {
            i.reset();
        }
    }

    /// How often it ticks now.
    pub fn every(&self) -> Option<Duration> {
        self.every
    }

    /// The next tick; never, while it does not tick.
    pub async fn tick(&mut self) {
        match &mut self.interval {
            Some(i) => {
                i.tick().await;
            }
            None => std::future::pending().await,
        }
    }
}

fn interval(every: Duration) -> tokio::time::Interval {
    let mut i = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    i
}

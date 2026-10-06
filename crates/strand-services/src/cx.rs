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
use std::sync::{Arc, Condvar, Mutex};

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
        /// The local write this answers ([`Cx::report`]).
        echo_of: Option<Generation>,
        /// Sent before [`Cx::ready`]: boot values (`on change` does not
        /// fire for them).
        initial: bool,
    },
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
        if let Ok(mut c) = self.count.lock() {
            *c += 1;
        }
        self.cond.notify_all();
        (self.host)();
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
/// `audio.sink.volume`), passed to the service. Answer it with
/// [`Cx::report`].
#[derive(Clone, Debug, PartialEq)]
pub struct Write {
    /// The service field written (`sink`).
    pub field: &'static str,
    /// The path below it (`[.volume]`; empty for the field itself).
    pub path: Vec<Step>,
    /// The value written at `path`.
    pub value: Data,
    /// The field's whole new value.
    pub field_value: Data,
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
pub enum Msg<S: Service> {
    /// A local write ([`Write`]).
    Write(Write),
    /// An action (`notifications.clear()`).
    Action(S::Action),
    /// An async method call, to answer through the [`Reply`].
    Call(S::Call, Reply),
    /// Whether a reader is visible now. [`Cx::visible`] already says so;
    /// a service streaming only while visible (a Wi-Fi scan, audio
    /// levels, a polled sensor) starts or stops here.
    Visible(bool),
}

impl<S: Service> fmt::Debug for Msg<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Msg::Write(w) => f.debug_tuple("Write").field(w).finish(),
            Msg::Action(_) => f.write_str("Action(..)"),
            Msg::Call(..) => f.write_str("Call(..)"),
            Msg::Visible(v) => f.debug_tuple("Visible").field(v).finish(),
        }
    }
}

/// A service's context; see the module docs.
pub struct Cx<S: Service> {
    state: S,
    out: Out<S::Patch>,
    msgs: mpsc::UnboundedReceiver<Msg<S>>,
    visible: bool,
    ready: bool,
    buses: Buses,
    notify: Notify,
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
        msgs: mpsc::UnboundedReceiver<Msg<S>>,
        buses: Buses,
        notify: Notify,
    ) -> Cx<S> {
        Cx {
            state,
            out,
            msgs,
            visible: true,
            ready: false,
            buses,
            notify,
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
        self.update_with(f, None, None)
    }

    fn update_with(
        &mut self,
        f: impl FnOnce(&mut S),
        echo_of: Option<Generation>,
        always: Option<&'static str>,
    ) -> bool {
        let mut new = self.state.clone();
        f(&mut new);
        let mut patches = Vec::new();
        S::diff(&self.state, &new, &mut patches);
        self.state = new;
        // A write's answer always names its field, even when the service
        // kept the old value (refused it), so the optimistic local value
        // does not stay.
        if let Some(field) = always
            && let Some(i) = S::FIELDS.iter().position(|f| f.name == field)
            && !patches.iter().any(|p| p.target() == Target::Field(i))
            && let Some(p) = S::field_patch(&self.state, i)
        {
            patches.push(p);
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

    /// Emit an event (its patch variant): lossless, one delivery per
    /// emit.
    pub fn emit(&mut self, event: S::Patch) -> bool {
        self.out.send(Envelope::Patches {
            patches: vec![event],
            echo_of: None,
            initial: false,
        })
    }

    /// Answer `write`: change the state as the service now has it
    /// (the written value, or what it settled on). The written field is
    /// always reported, tagged with the write's generation, so the logic
    /// thread ignores the echo of a write it already shows.
    pub fn report(&mut self, write: &Write, f: impl FnOnce(&mut S)) -> bool {
        self.update_with(f, Some(write.generation), Some(write.field))
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

    /// Whether [`Cx::ready`] was called.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// A reader of the service is visible (it was acquired and is not
    /// hidden). Streams run only while this holds.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The next message (shared runtime). `None`: the service was
    /// stopped; return from the body.
    pub async fn recv(&mut self) -> Option<Msg<S>> {
        let m = self.msgs.recv().await;
        self.seen(&m);
        m
    }

    /// The next message, blocking (a service on its own thread). `None`:
    /// stopped.
    pub fn blocking_recv(&mut self) -> Option<Msg<S>> {
        let m = self.msgs.blocking_recv();
        self.seen(&m);
        m
    }

    /// The next message if one is waiting. `Err(true)`: stopped.
    pub fn try_recv(&mut self) -> Result<Msg<S>, bool> {
        match self.msgs.try_recv() {
            Ok(m) => {
                if let Msg::Visible(v) = m {
                    self.visible = v;
                }
                Ok(m)
            }
            Err(mpsc::error::TryRecvError::Empty) => Err(false),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(true),
        }
    }

    /// The service was stopped (its messages end): return from the body.
    pub fn stopped(&self) -> bool {
        self.msgs.is_closed() && self.msgs.is_empty()
    }

    fn seen(&mut self, m: &Option<Msg<S>>) {
        if let Some(Msg::Visible(v)) = m {
            self.visible = *v;
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
    /// runtime thread).
    pub async fn session(&self) -> zbus::Result<zbus::Connection> {
        bus::session(&self.buses).await
    }

    /// The system bus (one connection shared by the services of this
    /// runtime thread).
    pub async fn system(&self) -> zbus::Result<zbus::Connection> {
        bus::system(&self.buses).await
    }
}

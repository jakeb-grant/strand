//! The logic thread's side: [`Services`] (the registry and its threads)
//! and one [`Client`] per service.
//!
//! **Lifecycle** (design.md, "System services"): a service starts on its
//! first reader ([`Client::acquire`]), is reference-counted, and stops
//! [`STOP_GRACE`] (5 s) after its last reader leaves or goes invisible
//! ([`Client::release`]; a core timer on the logic clock). Acquiring it
//! again inside the grace cancels the stop: the running service carries
//! on, nothing restarts. The service sees [`Cx::visible`] turn false at
//! once on the last release (streams stop then, not 5 s later) and true
//! again on the next acquire. Readers of a `#[store(stream)]` field are
//! counted per field too ([`Client::acquire_field`]): the service sees
//! [`Cx::watched`] for that field, so a Wi-Fi scan runs only while a
//! visible reader reads the access points, not while a bar shows the
//! SSID.
//!
//! A body that ends with an error while readers still hold it is started
//! again on a core timer, backing off from 1 s to [`RETRY_MAX`] (reset
//! only after a run stayed up [`RETRY_MAX`]: a body that fails right
//! after saying it is ready still backs off); [`Client::running`] is
//! false meanwhile. A write, action or async call reaching a stopped
//! service starts it for that one operation (a reader for a moment: it
//! stops 5 s later).
//!
//! **Echoes**: a write the service has not answered when its run ends
//! (the body failed on it, or the 5 s stop came first) never will be: the
//! cells forget their pending writes then ([`Cells::forget_echoes`]), so
//! a later report equal to a lost write is an outside change, not its
//! echo.
//!
//! **Threads**: services marked shared run on one tokio current-thread
//! runtime thread (`strand-services`), started lazily with the first of
//! them; a service that needs a thread of its own gets one per run.
//!
//! **Patches** arrive on a channel per run; [`Services::pump`] applies
//! every waiting envelope (one envelope per update, so one update is one
//! tick) and is called by the host loop before each step. The host's
//! waker (given to [`Services::new`]) is called after every envelope, so
//! an idle shell sleeps until a service has something.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use strand_core::{Error, NodeId, Runtime, Scope, Timer};
use tokio::sync::{mpsc, oneshot};

use crate::bus::Buses;
use crate::cx::{Cx, Envelope, Msg, Notify, Out, Reply, Wake, Write};
use crate::data::{Data, Step, record_field};
use crate::service::{CallSig, FromCall, Service, Start};
use crate::store::{Applied, Cells, EventInfo, FieldInfo, How, Patch, Target};

/// How long a service keeps running after its last reader left.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// The longest wait before a failed body is started again.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// How long [`Services::shutdown`] waits for services on threads of
/// their own to return.
pub const JOIN_LIMIT: Duration = Duration::from_secs(2);

/// How long the shared runtime thread, ending, waits for what dropped
/// bodies left to finish ([`finalize`]: a notification server's closing
/// signals).
pub const FINALIZE_LIMIT: Duration = Duration::from_millis(500);

thread_local! {
    /// What dropped bodies left to finish on this thread's runtime
    /// ([`finalize`]).
    static FINALIZERS: std::cell::RefCell<Vec<tokio::task::JoinHandle<()>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Finish `f` after its body is gone (a drop guard's last words: the
/// notification server's `NotificationClosed` signals). It runs on the
/// current runtime; when that is the shared thread's and the thread ends
/// ([`crate::Services::shutdown`], the registry dropped), it is given up
/// to [`FINALIZE_LIMIT`] before the runtime goes. Without a runtime it is
/// dropped.
pub(crate) fn finalize(f: impl Future<Output = ()> + Send + 'static) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let handle = rt.spawn(f);
    let _ = FINALIZERS.try_with(|v| {
        let mut v = v.borrow_mut();
        v.retain(|h| !h.is_finished());
        v.push(handle);
    });
}

/// Wait (at most [`FINALIZE_LIMIT`]) for this thread's finalizers.
fn run_finalizers(rt: &tokio::runtime::Runtime) {
    let pending = FINALIZERS
        .try_with(|v| std::mem::take(&mut *v.borrow_mut()))
        .unwrap_or_default();
    if pending.iter().all(|h| h.is_finished()) {
        return;
    }
    rt.block_on(async {
        let all = async {
            for h in pending {
                let _ = h.await;
            }
        };
        let _ = tokio::time::timeout(FINALIZE_LIMIT, all).await;
    });
}

thread_local! {
    /// Bodies running on this shared runtime thread: when the last ends,
    /// the bus connections they shared are dropped.
    static SHARED_BODIES: Cell<usize> = const { Cell::new(0) };
}

/// One body counted in [`SHARED_BODIES`] while it lives: dropped when the
/// body returns, is stopped, or panics (its task is dropped then too).
struct SharedBody;

impl SharedBody {
    fn enter() -> SharedBody {
        let _ = SHARED_BODIES.try_with(|n| n.set(n.get() + 1));
        SharedBody
    }
}

impl Drop for SharedBody {
    fn drop(&mut self) {
        // The last body gone: its connections go too (a dead bus is
        // connected afresh next time).
        let last = SHARED_BODIES
            .try_with(|n| {
                n.set(n.get().saturating_sub(1));
                n.get() == 0
            })
            .unwrap_or(false);
        if last {
            crate::bus::forget();
        }
    }
}

/// A job for the shared runtime thread: builds a future there.
type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// The shared tokio current-thread runtime and its thread.
struct Shared {
    jobs: Option<mpsc::UnboundedSender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Shared {
    fn start() -> std::io::Result<Shared> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("strand-services".into())
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    while let Some(job) = rx.recv().await {
                        tokio::task::spawn_local(job());
                    }
                });
                // The services' futures go with the set, then the
                // connections they shared (inside the runtime: dropping a
                // connection spawns its cleanup).
                let enter = rt.enter();
                drop(local);
                drop(enter);
                // What the dropped bodies left to say (a notification
                // server's closing signals) goes out first, bounded.
                run_finalizers(&rt);
                let enter = rt.enter();
                crate::bus::forget();
                drop(enter);
                drop(rt);
            })?;
        Ok(Shared {
            jobs: Some(tx),
            thread: Some(thread),
        })
    }

    fn spawn(&self, job: Job) -> bool {
        self.jobs.as_ref().is_some_and(|j| j.send(job).is_ok())
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the registry asks of every client.
trait Member {
    fn name(&self) -> &'static str;
    fn pump(&self, rt: &Runtime) -> bool;
    /// Running and not ready yet.
    fn waiting(&self) -> bool;
    fn stop_now(&self);
    /// The thread of its last run on a thread of its own, if any.
    fn take_thread(&self) -> Option<JoinHandle<()>>;
}

struct Registry {
    buses: Buses,
    wake: Arc<Wake>,
    shared: RefCell<Option<Shared>>,
    members: RefCell<Vec<Rc<dyn Member>>>,
    anchor: Scope,
    /// Why services failed, not yet taken by the host.
    diagnostics: RefCell<Vec<ServiceDiagnostic>>,
}

/// A service's run failed (its body returned an error, or panicked), or
/// its body raised something the user must act on ([`Cx::notice`]:
/// another notification server owns the name). One per distinct message:
/// a body failing the same way on every retry is reported once, until a
/// run stays up [`RETRY_MAX`] or ends cleanly. `strand run` logs each,
/// sends each to `strand watch`, and shows notices on the overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDiagnostic {
    /// The service (`notifications`).
    pub service: &'static str,
    /// Why (`another notification server, `mako` (pid 4242), owns …`).
    pub message: String,
    /// The user must act on it ([`Cx::notice`]); else a failure the
    /// service retries (a daemon or bus that cannot be reached).
    pub notice: bool,
    /// The notice `message` no longer holds: a later run became ready
    /// without raising it (the other notification server stopped and the
    /// name was taken over), or the service stopped cleanly. The host
    /// takes the notice's rows away.
    pub resolved: bool,
}

impl fmt::Display for ServiceDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.resolved {
            write!(f, "service `{}`: resolved: {}", self.service, self.message)
        } else {
            write!(f, "service `{}`: {}", self.service, self.message)
        }
    }
}

impl Registry {
    fn shared_spawn(&self, job: Job) -> bool {
        let mut shared = self.shared.borrow_mut();
        if shared.is_none() {
            match Shared::start() {
                Ok(s) => *shared = Some(s),
                Err(e) => {
                    log::error!("no services runtime: {e}");
                    return false;
                }
            }
        }
        shared.as_ref().is_some_and(|s| s.spawn(job))
    }
}

/// The services of one logic thread; see the module docs.
#[derive(Clone)]
pub struct Services(Rc<Registry>);

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Services")
            .field("buses", &self.0.buses)
            .field("members", &self.0.members.borrow().len())
            .field("runtime", &self.0.shared.borrow().is_some())
            .finish()
    }
}

impl Services {
    /// A registry on `rt`'s logic thread. `wake` is called (from any
    /// thread) after each envelope a service sends: the host's loop wakes
    /// and calls [`Services::pump`]. Call it where no owner is current
    /// (before mounting): the services' cells and timers are owned by a
    /// scope of their own.
    pub fn new(rt: &Runtime, buses: Buses, wake: impl Fn() + Send + Sync + 'static) -> Services {
        let (anchor, ()) = rt.scope(|_| ());
        rt.set_name(anchor.id(), "services");
        Services(Rc::new(Registry {
            buses,
            wake: Arc::new(Wake::new(wake)),
            shared: RefCell::new(None),
            members: RefCell::new(Vec::new()),
            anchor,
            diagnostics: RefCell::new(Vec::new()),
        }))
    }

    /// The buses services use.
    pub fn buses(&self) -> &Buses {
        &self.0.buses
    }

    /// Register service `S`: its cells are created now (holding the
    /// store's defaults); it starts on its first [`Client::acquire`].
    pub fn register<S: Service>(&self, rt: &Runtime) -> Client<S> {
        let cells = self
            .0
            .anchor
            .run(rt, |rt| {
                rt.untrack(|rt| S::Cells::new(rt, S::NAME, &S::default()))
            })
            .unwrap_or_else(|_| S::Cells::new(rt, S::NAME, &S::default()));
        let inner = Rc::new_cyclic(|me| ClientInner::<S> {
            me: me.clone(),
            reg: Rc::downgrade(&self.0),
            cells,
            refs: Cell::new(0),
            field_refs: S::FIELDS.iter().map(|_| Cell::new(0)).collect(),
            run: RefCell::new(None),
            timer: Cell::new(None),
            retry: Cell::new(None),
            failures: Cell::new(0),
            last_thread: RefCell::new(None),
            observers: RefCell::new(Vec::new()),
            starts: Cell::new(0),
            stops: Cell::new(0),
            reports: Cell::new(0),
            last_error: RefCell::new(None),
            last_notice: RefCell::new(None),
        });
        self.0
            .members
            .borrow_mut()
            .push(inner.clone() as Rc<dyn Member>);
        Client(inner)
    }

    /// Apply every envelope waiting from every service. Returns whether
    /// anything arrived. Call it outside handlers, before a step: service
    /// reports are not handler writes.
    pub fn pump(&self, rt: &Runtime) -> bool {
        let members: Vec<Rc<dyn Member>> = self.0.members.borrow().clone();
        let mut any = false;
        for m in members {
            any |= m.pump(rt);
        }
        any
    }

    /// Pump until every running service said it is ready (its first read
    /// is in), at most `limit`: the first frame waits this long so a
    /// shell does not boot showing defaults. Returns whether all were.
    pub fn wait_ready(&self, rt: &Runtime, limit: Duration) -> bool {
        self.wait_ready_of(rt, None, limit)
    }

    /// [`Services::wait_ready`] for the service named `name` only (every
    /// service when `None`).
    pub fn wait_ready_of(&self, rt: &Runtime, name: Option<&str>, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            let seen = self.0.wake.count();
            self.pump(rt);
            let waiting = self
                .0
                .members
                .borrow()
                .iter()
                .any(|m| name.is_none_or(|n| m.name() == n) && m.waiting());
            if !waiting {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.0.wake.wait_past(seen, left);
        }
    }

    /// The services' failures since the last call, oldest first
    /// ([`ServiceDiagnostic`]); call it after [`Services::pump`].
    pub fn take_diagnostics(&self) -> Vec<ServiceDiagnostic> {
        std::mem::take(&mut *self.0.diagnostics.borrow_mut())
    }

    /// Whether the shared runtime thread was started.
    pub fn runtime_started(&self) -> bool {
        self.0.shared.borrow().is_some()
    }

    /// Stop every service now and end the shared runtime (joining its
    /// thread). Services on threads of their own are told to stop and
    /// joined, [`JOIN_LIMIT`] at most in all (one that overruns is
    /// logged and left).
    pub fn shutdown(&self) {
        let members: Vec<Rc<dyn Member>> = self.0.members.borrow().clone();
        for m in &members {
            m.stop_now();
        }
        self.0.shared.borrow_mut().take();
        join_all(members.iter().filter_map(|m| m.take_thread()).collect());
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let members: Vec<Rc<dyn Member>> = self.members.get_mut().drain(..).collect();
        for m in &members {
            m.stop_now();
        }
        self.shared.get_mut().take();
        join_all(members.iter().filter_map(|m| m.take_thread()).collect());
    }
}

/// Join service threads, [`JOIN_LIMIT`] at most in all.
fn join_all(threads: Vec<JoinHandle<()>>) {
    let deadline = Instant::now() + JOIN_LIMIT;
    for t in threads {
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if t.is_finished() {
            let _ = t.join();
        } else {
            let name = t.thread().name().unwrap_or("a service").to_string();
            log::warn!("{name} did not stop within {JOIN_LIMIT:?}; left running");
        }
    }
}

/// One run of a service: its channels.
struct Run<S: Service> {
    rx: std::sync::mpsc::Receiver<Envelope<S::Patch>>,
    tx: mpsc::UnboundedSender<Msg<S>>,
    /// Ends the body on the shared runtime.
    stop: Option<oneshot::Sender<()>>,
    notify: Notify,
    /// A body on a thread of its own: its thread.
    thread: Option<JoinHandle<()>>,
    ready: bool,
    ended: bool,
    /// It raised a notice ([`Cx::notice`]).
    noticed: bool,
    /// When it started (the logic clock).
    started: Duration,
}

impl<S: Service> Run<S> {
    fn send(&self, m: Msg<S>) -> bool {
        let ok = self.tx.send(m).is_ok();
        self.poke();
        ok
    }

    fn poke(&self) {
        if let Ok(n) = self.notify.lock()
            && let Some(f) = n.as_ref()
        {
            f();
        }
    }
}

/// Told of every keyed change and event applied.
pub type Observer = Box<dyn Fn(&Runtime, &Applied)>;

struct ClientInner<S: Service> {
    me: Weak<ClientInner<S>>,
    reg: Weak<Registry>,
    cells: S::Cells,
    refs: Cell<u32>,
    /// Readers per field (stream fields are told of theirs).
    field_refs: Vec<Cell<u32>>,
    run: RefCell<Option<Run<S>>>,
    timer: Cell<Option<Timer>>,
    retry: Cell<Option<Timer>>,
    /// Failed runs in a row (the retry backoff).
    failures: Cell<u32>,
    /// The last run's thread (a service on a thread of its own).
    last_thread: RefCell<Option<JoinHandle<()>>>,
    observers: RefCell<Vec<Observer>>,
    starts: Cell<u64>,
    stops: Cell<u64>,
    reports: Cell<u64>,
    /// The failure last logged.
    last_error: RefCell<Option<String>>,
    /// The notice last reported ([`ServiceDiagnostic`]).
    last_notice: RefCell<Option<String>>,
}

impl<S: Service> ClientInner<S> {
    /// A run failed with `message`, or its body raised a notice for the
    /// user ([`Cx::notice`]): a [`ServiceDiagnostic`], dropped when it
    /// repeats the last one of its kind (a retry failing the same way),
    /// until a run stays up [`RETRY_MAX`] or ends cleanly.
    fn diagnose(&self, rt: &Runtime, message: &str, notice: bool) {
        let stable = self
            .run
            .borrow()
            .as_ref()
            .is_some_and(|r| rt.now().saturating_sub(r.started) >= RETRY_MAX);
        // A failure the body already raised as a notice is that notice.
        if !notice && self.last_notice.borrow().as_deref() == Some(message) {
            return;
        }
        let slot = if notice {
            &self.last_notice
        } else {
            &self.last_error
        };
        let mut last = slot.borrow_mut();
        if stable {
            *last = None;
        }
        if last.as_deref() == Some(message) {
            return;
        }
        *last = Some(message.to_string());
        if notice {
            log::error!("service `{}`: {message}", S::NAME);
        } else {
            log::warn!("service `{}` failed: {message}", S::NAME);
        }
        if let Some(reg) = self.reg.upgrade() {
            reg.diagnostics.borrow_mut().push(ServiceDiagnostic {
                service: S::NAME,
                message: message.to_string(),
                notice,
                resolved: false,
            });
        }
    }

    /// The notice last reported no longer holds: a resolved
    /// [`ServiceDiagnostic`] for it.
    fn resolve(&self) {
        let Some(message) = self.last_notice.borrow_mut().take() else {
            return;
        };
        log::info!("service `{}`: resolved: {message}", S::NAME);
        if let Some(reg) = self.reg.upgrade() {
            reg.diagnostics.borrow_mut().push(ServiceDiagnostic {
                service: S::NAME,
                message,
                notice: true,
                resolved: true,
            });
        }
    }

    fn start(self: &Rc<Self>, rt: &Runtime) {
        let Some(reg) = self.reg.upgrade() else {
            return;
        };
        let state = match self.cells.snapshot(rt) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("{}: {e}", S::NAME);
                S::default()
            }
        };
        let (out_tx, rx) = std::sync::mpsc::channel();
        let (tx, msgs) = mpsc::unbounded_channel();
        let notify: Notify = Arc::new(Mutex::new(None));
        let out = Out {
            tx: out_tx.clone(),
            wake: reg.wake.clone(),
        };
        let ended = Out {
            tx: out_tx,
            wake: reg.wake.clone(),
        };
        let watched = S::FIELDS
            .iter()
            .zip(&self.field_refs)
            .map(|(f, n)| f.stream && n.get() > 0)
            .collect();
        let cx = Cx::new(state, out, msgs, reg.buses.clone(), notify.clone(), watched);
        let mut stop = None;
        let mut thread = None;
        let started = match S::start(cx) {
            Start::Shared(body) => {
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                stop = Some(stop_tx);
                reg.shared_spawn(Box::new(move || {
                    Box::pin(async move {
                        let _counted = SharedBody::enter();
                        // The shared connections it uses are let go
                        // with it.
                        crate::bus::with_user(async move {
                            tokio::select! {
                                _ = stop_rx => {}
                                r = body() => {
                                    if let Err(e) = &r {
                                        log::debug!("service {} ended: {e}", S::NAME);
                                    }
                                    ended.send(Envelope::Ended(r.map_err(|e| e.0)));
                                }
                            }
                        })
                        .await
                    })
                }))
            }
            Start::Thread(body) => {
                // The previous run's thread (told to stop) is joined by
                // this one before its body starts: two PipeWire or
                // Wayland connections of one service never overlap.
                let prev = self.last_thread.borrow_mut().take();
                let spawned = std::thread::Builder::new()
                    .name(format!("strand-{}", S::NAME))
                    .spawn(move || {
                        if let Some(p) = prev {
                            let _ = p.join();
                        }
                        let r = body();
                        if let Err(e) = &r {
                            log::debug!("service {} ended: {e}", S::NAME);
                        }
                        ended.send(Envelope::Ended(r.map_err(|e| e.0)));
                    });
                match spawned {
                    Ok(h) => {
                        thread = Some(h);
                        true
                    }
                    Err(e) => {
                        log::error!("service {}: no thread: {e}", S::NAME);
                        false
                    }
                }
            }
        };
        if !started {
            return;
        }
        self.starts.set(self.starts.get() + 1);
        *self.run.borrow_mut() = Some(Run {
            rx,
            tx,
            stop,
            notify,
            thread,
            ready: false,
            noticed: false,
            ended: false,
            started: rt.now(),
        });
    }

    fn stop(&self) {
        let run = self.run.borrow_mut().take();
        if let Some(mut run) = run {
            if let Some(s) = run.stop.take() {
                let _ = s.send(());
            }
            if let Some(t) = run.thread.take() {
                *self.last_thread.borrow_mut() = Some(t);
            }
            let notify = run.notify.clone();
            drop(run);
            // A service on its own thread: its messages have ended.
            if let Ok(n) = notify.lock()
                && let Some(f) = n.as_ref()
            {
                f();
            }
            self.stops.set(self.stops.get() + 1);
        }
    }

    fn arm_stop(self: &Rc<Self>, rt: &Runtime) {
        if let Some(t) = self.timer.get()
            && t.restart(rt).is_ok()
        {
            return;
        }
        let Some(reg) = self.reg.upgrade() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let timer = reg.anchor.run(rt, |rt| {
            rt.after(
                STOP_GRACE,
                |_| Ok(true),
                move |rt| {
                    if let Some(c) = weak.upgrade()
                        && c.refs.get() == 0
                    {
                        // A retry pending from a failed run goes with it
                        // (no wakeup later for a service nobody reads).
                        c.disarm_retry(rt);
                        c.failures.set(0);
                        c.stop();
                        // A notice from a failed run no longer holds for
                        // a service nobody uses (its rows leave the
                        // overlay).
                        c.resolve();
                        // Writes it never answered never will be.
                        c.cells.forget_echoes(rt);
                    }
                    Ok(())
                },
            )
        });
        match timer {
            Ok(t) => {
                rt.set_name(t.id(), format!("{} stop", S::NAME));
                self.timer.set(Some(t));
            }
            Err(e) => log::warn!("{}: no stop timer: {e}", S::NAME),
        }
    }

    /// A reader came back inside the grace: the pending stop goes (no
    /// wakeup 5 s later for nothing).
    fn disarm_stop(&self, rt: &Runtime) {
        if let Some(t) = self.timer.take() {
            t.dispose(rt);
        }
    }

    fn disarm_retry(&self, rt: &Runtime) {
        if let Some(t) = self.retry.take() {
            t.dispose(rt);
        }
    }

    /// The body ended with an error while read: start it again after a
    /// backoff (1 s, doubling to [`RETRY_MAX`]).
    fn arm_retry(&self, rt: &Runtime) {
        self.disarm_retry(rt);
        let (Some(me), Some(reg)) = (self.me.upgrade(), self.reg.upgrade()) else {
            return;
        };
        let n = self.failures.get();
        self.failures.set(n.saturating_add(1));
        let wait = Duration::from_secs(1u64 << n.min(5)).min(RETRY_MAX);
        let weak = Rc::downgrade(&me);
        let timer = reg.anchor.run(rt, |rt| {
            rt.after(
                wait,
                |_| Ok(true),
                move |rt| {
                    if let Some(c) = weak.upgrade() {
                        c.retry.set(None);
                        let ended = c.run.borrow().as_ref().is_some_and(|r| r.ended);
                        if c.refs.get() > 0 && ended {
                            c.stop();
                            c.start(rt);
                        }
                    }
                    Ok(())
                },
            )
        });
        match timer {
            Ok(t) => {
                rt.set_name(t.id(), format!("{} retry", S::NAME));
                self.retry.set(Some(t));
            }
            Err(e) => log::warn!("{}: no retry timer: {e}", S::NAME),
        }
    }

    /// The run ended (an `Ended` envelope, or its channel closed).
    fn ended(&self, rt: &Runtime, failed: bool) {
        let (was, started) = match self.run.borrow_mut().as_mut() {
            Some(run) => {
                let was = run.ended;
                run.ended = true;
                run.ready = true;
                (was, run.started)
            }
            None => return,
        };
        if was {
            return;
        }
        // Writes it never answered never will be.
        self.cells.forget_echoes(rt);
        if failed && self.refs.get() > 0 {
            // Only a run that stayed up resets the backoff: one that fails
            // right after saying it is ready (a daemon missing or
            // flapping) keeps backing off.
            if rt.now().saturating_sub(started) >= RETRY_MAX {
                self.failures.set(0);
            }
            self.arm_retry(rt);
        }
    }

    /// Send a committed write to the run current now (a write the rate
    /// guard held may commit after a restart, or after the 5 s stop: a
    /// run is then started for it, as for a write to a stopped service).
    /// When none can start (a failed body backing off), the write is
    /// dropped and the cells forget it: the next run's boot read wins.
    fn deliver(self: &Rc<Self>, rt: &Runtime, write: Write) {
        let live = self.run.borrow().as_ref().is_some_and(|r| !r.ended);
        let sent = if live {
            self.run
                .borrow()
                .as_ref()
                .is_some_and(|run| run.send(Msg::Write(write)))
        } else {
            self.with_run(rt, |run| run.send(Msg::Write(write)))
                .unwrap_or_else(|e| {
                    log::warn!("{}: a write was not sent: {e}", S::NAME);
                    false
                })
        };
        if !sent {
            self.cells.forget_echoes(rt);
        }
    }

    /// Run `f` on the running service; a stopped one, or one whose body
    /// ended, is started for it (acquired and released at once when
    /// nobody reads it: it stops [`STOP_GRACE`] later). A failed body
    /// waiting out its retry backoff is not restarted early.
    fn with_run<R>(
        self: &Rc<Self>,
        rt: &Runtime,
        f: impl FnOnce(&Run<S>) -> R,
    ) -> Result<R, Error> {
        let ended = self.run.borrow().as_ref().is_some_and(|r| r.ended);
        if ended {
            if self.retry.get().is_some() {
                return Err(Error::failed(format!(
                    "`{}` is not running: its body failed and restarts after a backoff",
                    S::NAME
                )));
            }
            self.stop();
        }
        let idle = self.run.borrow().is_none();
        let held = idle && self.refs.get() == 0;
        if held {
            Client(self.clone()).acquire(rt);
        } else if idle {
            self.start(rt);
        }
        let r = match self.run.borrow().as_ref() {
            Some(run) if !run.ended => Ok(f(run)),
            Some(_) => Err(Error::failed(format!("`{}` ended", S::NAME))),
            None => Err(Error::failed(format!(
                "`{}` is not running (it failed to start)",
                S::NAME
            ))),
        };
        if held {
            Client(self.clone()).release(rt);
        }
        r
    }
}

impl<S: Service> Member for ClientInner<S> {
    fn name(&self) -> &'static str {
        S::NAME
    }

    fn pump(&self, rt: &Runtime) -> bool {
        let mut any = false;
        loop {
            let next = match self.run.borrow().as_ref() {
                Some(run) => run.rx.try_recv(),
                None => return any,
            };
            let env = match next {
                Ok(env) => env,
                Err(std::sync::mpsc::TryRecvError::Empty) => return any,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // No `Ended` came first: the body panicked.
                    let live = self.run.borrow().as_ref().is_some_and(|r| !r.ended);
                    if live {
                        self.diagnose(rt, "it stopped unexpectedly (a panic; see the log)", false);
                    }
                    self.ended(rt, true);
                    return any;
                }
            };
            any = true;
            match env {
                Envelope::Patches {
                    patches,
                    echo_of,
                    initial,
                } => {
                    self.reports.set(self.reports.get() + 1);
                    for p in &patches {
                        // Only the written field's patch answers the
                        // write (even before the service is ready: the
                        // answer is newer than its boot read); the rest
                        // are outside changes, or boot values.
                        let how = match echo_of {
                            Some((i, g)) if p.target() == Target::Field(i) => How::Report(Some(g)),
                            _ if initial => How::Initial,
                            _ => How::Report(None),
                        };
                        match self.cells.apply(rt, p, how) {
                            Ok(Some(applied)) => {
                                for o in self.observers.borrow().iter() {
                                    o(rt, &applied);
                                }
                            }
                            Ok(None) => {}
                            Err(e) => log::warn!("{}: {e}", S::NAME),
                        }
                    }
                }
                Envelope::Notice(m) => {
                    if let Some(run) = self.run.borrow_mut().as_mut() {
                        run.noticed = true;
                    }
                    self.diagnose(rt, &m, true)
                }
                Envelope::Resolved => {
                    if let Some(run) = self.run.borrow_mut().as_mut() {
                        run.noticed = false;
                    }
                    self.resolve();
                }
                Envelope::Ready => {
                    // Ready without a notice (raised before readiness):
                    // the last notice no longer holds.
                    let resolves = match self.run.borrow_mut().as_mut() {
                        Some(run) => {
                            run.ready = true;
                            !run.noticed
                        }
                        None => false,
                    };
                    if resolves {
                        self.resolve();
                    }
                }
                Envelope::Ended(r) => {
                    match &r {
                        Err(e) => self.diagnose(rt, e, false),
                        Ok(()) => {
                            *self.last_error.borrow_mut() = None;
                            self.resolve();
                        }
                    }
                    self.ended(rt, r.is_err())
                }
            }
        }
    }

    fn waiting(&self) -> bool {
        self.run.borrow().as_ref().is_some_and(|r| !r.ready)
    }

    fn stop_now(&self) {
        self.stop();
        self.resolve();
    }

    fn take_thread(&self) -> Option<JoinHandle<()>> {
        self.last_thread.borrow_mut().take()
    }
}

/// The logic thread's handle on one service.
pub struct Client<S: Service>(Rc<ClientInner<S>>);

impl<S: Service> Clone for Client<S> {
    fn clone(&self) -> Self {
        Client(self.0.clone())
    }
}

impl<S: Service> fmt::Debug for Client<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("service", &S::NAME)
            .field("readers", &self.0.refs.get())
            .field("running", &self.running())
            .finish()
    }
}

impl<S: Service> Client<S> {
    /// The service's cells.
    pub fn cells(&self) -> &S::Cells {
        &self.0.cells
    }

    /// A reader came: the service starts on its first (or carries on if
    /// its stop was pending), and is visible.
    pub fn acquire(&self, rt: &Runtime) {
        let n = self.0.refs.get() + 1;
        self.0.refs.set(n);
        if n > 1 {
            return;
        }
        self.0.disarm_stop(rt);
        let running = self.0.run.borrow().as_ref().map(|r| r.ended);
        match running {
            Some(false) => {
                if let Some(run) = self.0.run.borrow().as_ref() {
                    run.send(Msg::Visible(true));
                }
            }
            // Ended by itself: a new reader starts it again.
            Some(true) => {
                self.0.disarm_retry(rt);
                self.0.stop();
                self.0.start(rt);
            }
            None => {
                self.0.disarm_retry(rt);
                self.0.start(rt);
            }
        }
    }

    /// A visible reader of field `field` came (on top of
    /// [`Client::acquire`] of the service): a `#[store(stream)]` field's
    /// stream starts with its first ([`Cx::watched`]).
    pub fn acquire_field(&self, field: usize) {
        let Some(n) = self.0.field_refs.get(field) else {
            return;
        };
        n.set(n.get() + 1);
        if n.get() == 1 {
            self.watch(field, true);
        }
    }

    /// The matching release: the stream stops with the field's last
    /// visible reader (at once, not after the grace).
    pub fn release_field(&self, field: usize) {
        let Some(n) = self.0.field_refs.get(field) else {
            return;
        };
        if n.get() == 0 {
            return;
        }
        n.set(n.get() - 1);
        if n.get() == 0 {
            self.watch(field, false);
        }
    }

    /// Visible readers of field `field` now.
    pub fn field_readers(&self, field: usize) -> u32 {
        self.0.field_refs.get(field).map_or(0, Cell::get)
    }

    fn watch(&self, field: usize, on: bool) {
        let Some(info) = S::FIELDS.get(field) else {
            return;
        };
        if !info.stream {
            return;
        }
        if let Some(run) = self.0.run.borrow().as_ref()
            && !run.ended
        {
            run.send(Msg::Watch {
                field: info.name,
                on,
            });
        }
    }

    /// A reader left or was hidden: on the last, the service is not
    /// visible any more and stops [`STOP_GRACE`] later (unless acquired
    /// again first). Safe in scope cleanup (it disposes nothing).
    pub fn release(&self, rt: &Runtime) {
        let n = self.0.refs.get();
        if n == 0 {
            return;
        }
        self.0.refs.set(n - 1);
        if n > 1 {
            return;
        }
        if let Some(run) = self.0.run.borrow().as_ref() {
            run.send(Msg::Visible(false));
        }
        self.0.arm_stop(rt);
    }

    /// Readers holding it now.
    pub fn readers(&self) -> u32 {
        self.0.refs.get()
    }

    /// The service is running: started, not stopped, and its body has
    /// not ended.
    pub fn running(&self) -> bool {
        self.0.run.borrow().as_ref().is_some_and(|r| !r.ended)
    }

    /// How many times it started.
    pub fn starts(&self) -> u64 {
        self.0.starts.get()
    }

    /// How many times it stopped.
    pub fn stops(&self) -> u64 {
        self.0.stops.get()
    }

    /// How many updates (envelopes of patches) it has sent that were
    /// applied: a stream that stopped sends none.
    pub fn reports(&self) -> u64 {
        self.0.reports.get()
    }

    /// Set the cells as boot values (`on change` takes them as its
    /// baseline): a host's remembered values before the service first
    /// reports (the portal's last settings). The service starts from
    /// them.
    pub fn seed(&self, rt: &Runtime, f: impl FnOnce(&mut S)) -> Result<(), Error> {
        let old = self.0.cells.snapshot(rt)?;
        let mut new = old.clone();
        f(&mut new);
        let mut patches = Vec::new();
        S::diff(&old, &new, &mut patches);
        for p in &patches {
            if let Some(applied) = self.0.cells.apply(rt, p, How::Initial)? {
                for o in self.0.observers.borrow().iter() {
                    o(rt, &applied);
                }
            }
        }
        Ok(())
    }

    /// Send an action (a stopped service starts for it).
    pub fn act(&self, rt: &Runtime, action: S::Action) -> Result<(), Error> {
        match self.0.with_run(rt, |run| run.send(Msg::Action(action)))? {
            true => Ok(()),
            false => Err(not_running(S::NAME)),
        }
    }

    /// Call an async method (a stopped service starts for it); the
    /// future completes with its answer.
    pub fn request(
        &self,
        rt: &Runtime,
        call: S::Call,
    ) -> Pin<Box<dyn Future<Output = Result<Data, String>>>> {
        let (tx, rx) = oneshot::channel();
        let sent = self
            .0
            .with_run(rt, |run| run.send(Msg::Call(call, Reply(tx))));
        Box::pin(async move {
            match sent {
                Ok(true) => {}
                Ok(false) => return Err(not_running(S::NAME).to_string()),
                Err(e) => return Err(e.to_string()),
            }
            rx.await
                .unwrap_or_else(|_| Err(format!("`{}` stopped before answering", S::NAME)))
        })
    }

    /// This client as a [`DynService`].
    pub fn dynamic(&self) -> Rc<dyn DynService> {
        Rc::new(self.clone())
    }
}

/// The keyed fields of `S` and the record each hands out (`[Workspace]`
/// is `Workspace`).
fn keyed_records<S: Service>() -> Vec<(usize, String)> {
    S::FIELDS
        .iter()
        .enumerate()
        .filter(|(_, f)| f.keyed)
        .filter_map(|(i, f)| {
            let ty = (f.ty)();
            let rec = ty.strip_prefix('[')?.strip_suffix(']')?.to_string();
            Some((i, rec))
        })
        .collect()
}

/// The run's channel closed under a message: its body just ended.
fn not_running(name: &str) -> Error {
    Error::failed(format!("`{name}` is not running (its body ended)"))
}

/// A service as the language side drives it, by field index and name
/// with [`Data`] values: what the binary's `ServiceHost` adapter wraps.
/// Every [`Client`] is one.
pub trait DynService {
    fn name(&self) -> &'static str;
    /// Its schema text.
    fn schema(&self) -> &'static str;
    fn fields(&self) -> &'static [FieldInfo];
    fn events(&self) -> &'static [EventInfo];
    /// Its action names.
    fn actions(&self) -> &'static [&'static str];
    /// Its async method names.
    fn methods(&self) -> &'static [&'static str];
    /// Its actions, with their arities and item records.
    fn action_sigs(&self) -> Vec<CallSig>;
    /// Its async methods, with their arities and item records.
    fn method_sigs(&self) -> Vec<CallSig>;
    /// The record types of its items: those its keyed lists hand out
    /// (their `rw` fields are written through [`DynService::write_item`])
    /// and those its actions and async methods are called on. The
    /// language side routes item writes and item calls here by them.
    fn item_records(&self) -> Vec<String>;
    /// Field `field`, tracked.
    fn read(&self, rt: &Runtime, field: usize) -> Result<Data, Error>;
    /// The core nodes behind field `field`.
    fn ids(&self, field: usize) -> Vec<NodeId>;
    /// A keyed field's items, untracked.
    fn keyed_items(&self, rt: &Runtime, field: usize) -> Result<Vec<Data>, Error>;
    /// Write `value` at `path` below field `field` (applied at once, sent
    /// to the service tagged; its echo is ignored).
    fn write(&self, rt: &Runtime, field: usize, path: &[Step], value: Data) -> Result<(), Error>;
    /// Write `value` at `path` below `item`, an item of record `record`
    /// one of its keyed lists holds (`s.volume = 0.5` for `s` in
    /// `audio.sinks`): the list holding the item's key is updated at once
    /// and the write sent to the service with the key
    /// ([`Write::key`](crate::Write::key)); the item's echoes are ignored.
    fn write_item(
        &self,
        rt: &Runtime,
        record: &str,
        item: &Data,
        path: &[Step],
        value: Data,
    ) -> Result<(), Error>;
    /// Run action `name` (on `item` for an item's action).
    fn action(
        &self,
        rt: &Runtime,
        name: &str,
        item: Option<&Data>,
        args: &[Data],
    ) -> Result<(), Error>;
    /// A `fn` method, computed on the logic thread; `None`: none such.
    fn call(&self, rt: &Runtime, method: &str, args: &[Data]) -> Option<Result<Data, Error>>;
    /// An async method: the future completes with the service's answer.
    fn fetch(
        &self,
        rt: &Runtime,
        method: &str,
        args: &[Data],
    ) -> Pin<Box<dyn Future<Output = Result<Data, String>>>>;
    fn acquire(&self, rt: &Runtime);
    fn release(&self, rt: &Runtime);
    /// A visible reader of field `field` came ([`Client::acquire_field`]).
    fn acquire_field(&self, field: usize);
    /// It left or was hidden ([`Client::release_field`]).
    fn release_field(&self, field: usize);
    fn readers(&self) -> u32;
    fn running(&self) -> bool;
    fn starts(&self) -> u64;
    /// Called with every keyed change and event applied from now on.
    fn observe(&self, f: Observer);
}

impl<S: Service> DynService for Client<S> {
    fn name(&self) -> &'static str {
        S::NAME
    }

    fn schema(&self) -> &'static str {
        S::schema()
    }

    fn fields(&self) -> &'static [FieldInfo] {
        S::FIELDS
    }

    fn events(&self) -> &'static [EventInfo] {
        S::EVENTS
    }

    fn actions(&self) -> &'static [&'static str] {
        <S::Action as FromCall>::NAMES
    }

    fn methods(&self) -> &'static [&'static str] {
        <S::Call as FromCall>::NAMES
    }

    fn action_sigs(&self) -> Vec<CallSig> {
        <S::Action as FromCall>::signatures()
    }

    fn method_sigs(&self) -> Vec<CallSig> {
        <S::Call as FromCall>::signatures()
    }

    fn item_records(&self) -> Vec<String> {
        let mut v: Vec<String> = keyed_records::<S>().into_iter().map(|(_, r)| r).collect();
        v.extend(<S::Action as FromCall>::item_records());
        v.extend(<S::Call as FromCall>::item_records());
        v.sort();
        v.dedup();
        v
    }

    fn read(&self, rt: &Runtime, field: usize) -> Result<Data, Error> {
        self.0.cells.read(rt, field)
    }

    fn ids(&self, field: usize) -> Vec<NodeId> {
        self.0.cells.ids(field)
    }

    fn keyed_items(&self, rt: &Runtime, field: usize) -> Result<Vec<Data>, Error> {
        self.0.cells.keyed_items(rt, field)
    }

    fn write(&self, rt: &Runtime, field: usize, path: &[Step], value: Data) -> Result<(), Error> {
        let Some(info) = S::FIELDS.get(field) else {
            return Err(Error::failed(format!(
                "`{}` has no field #{field}",
                S::NAME
            )));
        };
        // A stopped service starts for the write.
        self.0.with_run(rt, |_| ())?;
        let whole = if path.is_empty() {
            value.clone()
        } else {
            let cur = rt.untrack(|rt| self.0.cells.read(rt, field))?;
            cur.with_path(path, value.clone())
                .map_err(|e| Error::failed(format!("{}.{}: {e}", S::NAME, info.name)))?
        };
        let (path, name, sent_whole) = (path.to_vec(), info.name, whole.clone());
        let me = Rc::downgrade(&self.0);
        self.0.cells.write(
            rt,
            field,
            &whole,
            Box::new(move |rt, generation| {
                if let Some(c) = me.upgrade() {
                    c.deliver(
                        rt,
                        Write {
                            field: name,
                            key: None,
                            path,
                            value,
                            field_value: sent_whole,
                            generation,
                        },
                    );
                }
            }),
        )
    }

    fn write_item(
        &self,
        rt: &Runtime,
        record: &str,
        item: &Data,
        path: &[Step],
        value: Data,
    ) -> Result<(), Error> {
        let lists: Vec<usize> = keyed_records::<S>()
            .into_iter()
            .filter(|(_, r)| r == record)
            .map(|(i, _)| i)
            .collect();
        let Some(&first) = lists.first() else {
            return Err(Error::failed(format!(
                "`{}` hands out no `{record}` items",
                S::NAME
            )));
        };
        let key_name = S::FIELDS[first].key.unwrap_or("id");
        let key = record_field(item, record, key_name)
            .map_err(|e| Error::failed(format!("{}: {e}", S::NAME)))?
            .clone();
        // A stopped service starts for the write.
        self.0.with_run(rt, |_| ())?;
        let mut last = None;
        for i in lists {
            let (name, me, key2) = (S::FIELDS[i].name, Rc::downgrade(&self.0), key.clone());
            let (path2, value2) = (path.to_vec(), value.clone());
            let send: crate::store::SendItemWrite = Box::new(move |rt, index, item, generation| {
                let Some(c) = me.upgrade() else {
                    return;
                };
                // The language side mirrors the item at once.
                let applied = Applied::Keyed {
                    field: i,
                    diffs: vec![strand_core::VecDiff::Update {
                        index,
                        key: key2.clone(),
                        value: item.clone(),
                    }],
                    initial: false,
                };
                for o in c.observers.borrow().iter() {
                    o(rt, &applied);
                }
                c.deliver(
                    rt,
                    Write {
                        field: name,
                        key: Some(key2),
                        path: path2,
                        value: value2,
                        field_value: item,
                        generation,
                    },
                );
            });
            // The list holding the key takes it (`sinks` or `sources`).
            match self.0.cells.write_item(rt, i, &key, path, &value, send) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::failed(format!("no `{record}` to write"))))
    }

    fn action(
        &self,
        rt: &Runtime,
        name: &str,
        item: Option<&Data>,
        args: &[Data],
    ) -> Result<(), Error> {
        let a = <S::Action as FromCall>::from_call(name, item, args)
            .map_err(|e| Error::failed(format!("{}.{name}: {e}", S::NAME)))?;
        self.act(rt, a)
    }

    fn call(&self, rt: &Runtime, method: &str, args: &[Data]) -> Option<Result<Data, Error>> {
        S::call(&self.0.cells, rt, method, args)
    }

    fn fetch(
        &self,
        rt: &Runtime,
        method: &str,
        args: &[Data],
    ) -> Pin<Box<dyn Future<Output = Result<Data, String>>>> {
        match <S::Call as FromCall>::from_call(method, None, args) {
            Ok(c) => self.request(rt, c),
            Err(e) => {
                let msg = format!("{}.{method}: {e}", S::NAME);
                Box::pin(async move { Err(msg) })
            }
        }
    }

    fn acquire(&self, rt: &Runtime) {
        Client::acquire(self, rt);
    }

    fn release(&self, rt: &Runtime) {
        Client::release(self, rt);
    }

    fn acquire_field(&self, field: usize) {
        Client::acquire_field(self, field);
    }

    fn release_field(&self, field: usize) {
        Client::release_field(self, field);
    }

    fn readers(&self) -> u32 {
        Client::readers(self)
    }

    fn running(&self) -> bool {
        Client::running(self)
    }

    fn starts(&self) -> u64 {
        Client::starts(self)
    }

    fn observe(&self, f: Observer) {
        self.0.observers.borrow_mut().push(f);
    }
}

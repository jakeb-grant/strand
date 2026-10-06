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
//! again on the next acquire.
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
use crate::data::{Data, Step};
use crate::service::{FromCall, Service, Start};
use crate::store::{Applied, Cells, EventInfo, FieldInfo, How};

/// How long a service keeps running after its last reader left.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

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
    fn pump(&self, rt: &Runtime) -> bool;
    /// Running and not ready yet.
    fn waiting(&self) -> bool;
    fn stop_now(&self);
}

struct Registry {
    buses: Buses,
    wake: Arc<Wake>,
    shared: RefCell<Option<Shared>>,
    members: RefCell<Vec<Rc<dyn Member>>>,
    anchor: Scope,
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
        let inner = Rc::new(ClientInner::<S> {
            reg: Rc::downgrade(&self.0),
            cells,
            refs: Cell::new(0),
            run: RefCell::new(None),
            timer: Cell::new(None),
            observers: RefCell::new(Vec::new()),
            starts: Cell::new(0),
            stops: Cell::new(0),
            reports: Cell::new(0),
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
        let deadline = Instant::now() + limit;
        loop {
            let seen = self.0.wake.count();
            self.pump(rt);
            if !self.0.members.borrow().iter().any(|m| m.waiting()) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.0.wake.wait_past(seen, left);
        }
    }

    /// Whether the shared runtime thread was started.
    pub fn runtime_started(&self) -> bool {
        self.0.shared.borrow().is_some()
    }

    /// Stop every service now and end the shared runtime (joining its
    /// thread). Services on threads of their own are told to stop and
    /// not waited for.
    pub fn shutdown(&self) {
        let members: Vec<Rc<dyn Member>> = self.0.members.borrow().clone();
        for m in members {
            m.stop_now();
        }
        self.0.shared.borrow_mut().take();
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        for m in self.members.get_mut().drain(..) {
            m.stop_now();
        }
        self.shared.get_mut().take();
    }
}

/// One run of a service: its channels.
struct Run<S: Service> {
    rx: std::sync::mpsc::Receiver<Envelope<S::Patch>>,
    tx: mpsc::UnboundedSender<Msg<S>>,
    /// Ends the body on the shared runtime.
    stop: Option<oneshot::Sender<()>>,
    notify: Notify,
    ready: bool,
    ended: bool,
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
    reg: Weak<Registry>,
    cells: S::Cells,
    refs: Cell<u32>,
    run: RefCell<Option<Run<S>>>,
    timer: Cell<Option<Timer>>,
    observers: RefCell<Vec<Observer>>,
    starts: Cell<u64>,
    stops: Cell<u64>,
    reports: Cell<u64>,
}

impl<S: Service> ClientInner<S> {
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
        let cx = Cx::new(state, out, msgs, reg.buses.clone(), notify.clone());
        let mut stop = None;
        let started = match S::start(cx) {
            Start::Shared(body) => {
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                stop = Some(stop_tx);
                reg.shared_spawn(Box::new(move || {
                    Box::pin(async move {
                        tokio::select! {
                            _ = stop_rx => {}
                            r = body() => {
                                if let Err(e) = &r {
                                    log::warn!("service {} ended: {e}", S::NAME);
                                }
                                ended.send(Envelope::Ended(r.map_err(|e| e.0)));
                            }
                        }
                    })
                }))
            }
            Start::Thread(body) => std::thread::Builder::new()
                .name(format!("strand-{}", S::NAME))
                .spawn(move || {
                    let r = body();
                    if let Err(e) = &r {
                        log::warn!("service {} ended: {e}", S::NAME);
                    }
                    ended.send(Envelope::Ended(r.map_err(|e| e.0)));
                })
                .map_err(|e| log::error!("service {}: no thread: {e}", S::NAME))
                .is_ok(),
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
            ready: false,
            ended: false,
        });
    }

    fn stop(&self) {
        let run = self.run.borrow_mut().take();
        if let Some(mut run) = run {
            if let Some(s) = run.stop.take() {
                let _ = s.send(());
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
                move |_| {
                    if let Some(c) = weak.upgrade()
                        && c.refs.get() == 0
                    {
                        c.stop();
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
}

impl<S: Service> Member for ClientInner<S> {
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
                    if let Some(run) = self.run.borrow_mut().as_mut() {
                        run.ended = true;
                        run.ready = true;
                    }
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
                    let how = if initial {
                        How::Initial
                    } else {
                        How::Report(echo_of)
                    };
                    for p in &patches {
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
                Envelope::Ready => {
                    if let Some(run) = self.run.borrow_mut().as_mut() {
                        run.ready = true;
                    }
                }
                Envelope::Ended(_) => {
                    if let Some(run) = self.run.borrow_mut().as_mut() {
                        run.ended = true;
                        run.ready = true;
                    }
                }
            }
        }
    }

    fn waiting(&self) -> bool {
        self.run.borrow().as_ref().is_some_and(|r| !r.ready)
    }

    fn stop_now(&self) {
        self.stop();
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
        let running = self.0.run.borrow().as_ref().map(|r| r.ended);
        match running {
            Some(false) => {
                if let Some(run) = self.0.run.borrow().as_ref() {
                    run.send(Msg::Visible(true));
                }
            }
            // Ended by itself (an error): a new reader starts it again.
            Some(true) => {
                self.0.stop();
                self.0.start(rt);
            }
            None => self.0.start(rt),
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

    /// The service is running (started and not stopped).
    pub fn running(&self) -> bool {
        self.0.run.borrow().is_some()
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

    /// Send an action.
    pub fn act(&self, action: S::Action) -> Result<(), Error> {
        match self.0.run.borrow().as_ref() {
            Some(run) if run.send(Msg::Action(action)) => Ok(()),
            _ => Err(not_running(S::NAME)),
        }
    }

    /// Call an async method; the future completes with its answer.
    pub fn request(&self, call: S::Call) -> Pin<Box<dyn Future<Output = Result<Data, String>>>> {
        let (tx, rx) = oneshot::channel();
        let sent = self
            .0
            .run
            .borrow()
            .as_ref()
            .is_some_and(|run| run.send(Msg::Call(call, Reply(tx))));
        Box::pin(async move {
            if !sent {
                return Err(format!("`{}` is not running", S::NAME));
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

fn not_running(name: &str) -> Error {
    Error::failed(format!("`{name}` is not running (nothing reads it)"))
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
    /// The record types whose items its actions are called on.
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
    /// Run action `name` (on `item` for an item's action).
    fn action(&self, name: &str, item: Option<&Data>, args: &[Data]) -> Result<(), Error>;
    /// A `fn` method, computed on the logic thread; `None`: none such.
    fn call(&self, rt: &Runtime, method: &str, args: &[Data]) -> Option<Result<Data, Error>>;
    /// An async method: the future completes with the service's answer.
    fn fetch(
        &self,
        method: &str,
        args: &[Data],
    ) -> Pin<Box<dyn Future<Output = Result<Data, String>>>>;
    fn acquire(&self, rt: &Runtime);
    fn release(&self, rt: &Runtime);
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

    fn item_records(&self) -> Vec<String> {
        let mut v = <S::Action as FromCall>::item_records();
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
        let tx = match self.0.run.borrow().as_ref() {
            Some(run) => (run.tx.clone(), run.notify.clone()),
            None => return Err(not_running(S::NAME)),
        };
        let whole = if path.is_empty() {
            value.clone()
        } else {
            let cur = rt.untrack(|rt| self.0.cells.read(rt, field))?;
            cur.with_path(path, value.clone())
                .map_err(|e| Error::failed(format!("{}.{}: {e}", S::NAME, info.name)))?
        };
        let (path, name, sent_whole) = (path.to_vec(), info.name, whole.clone());
        self.0.cells.write(
            rt,
            field,
            &whole,
            Box::new(move |_, generation| {
                let (tx, notify) = tx;
                let _ = tx.send(Msg::Write(Write {
                    field: name,
                    path,
                    value,
                    field_value: sent_whole,
                    generation,
                }));
                if let Ok(n) = notify.lock()
                    && let Some(f) = n.as_ref()
                {
                    f();
                }
            }),
        )
    }

    fn action(&self, name: &str, item: Option<&Data>, args: &[Data]) -> Result<(), Error> {
        let a = <S::Action as FromCall>::from_call(name, item, args)
            .map_err(|e| Error::failed(format!("{}.{name}: {e}", S::NAME)))?;
        self.act(a)
    }

    fn call(&self, rt: &Runtime, method: &str, args: &[Data]) -> Option<Result<Data, Error>> {
        S::call(&self.0.cells, rt, method, args)
    }

    fn fetch(
        &self,
        method: &str,
        args: &[Data],
    ) -> Pin<Box<dyn Future<Output = Result<Data, String>>>> {
        match <S::Call as FromCall>::from_call(method, None, args) {
            Ok(c) => self.request(c),
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

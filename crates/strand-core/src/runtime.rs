//! The graph: node storage, colouring, pull-based updates, ownership and the
//! per-tick flush.
//!
//! The algorithm is the Reactively/Leptos "push dirty, pull values" scheme:
//!
//! * A write marks the cell's observers `Dirty` and everything downstream of
//!   them `Check`, iteratively. Sinks (effects, watches, timers) that leave
//!   `Clean` are queued for the next flush. Nothing is computed during the
//!   push.
//! * A read of a derived node first brings it up to date: a `Check` node asks
//!   its sources (in the order it last read them) to update; only if one of
//!   them actually changed value does the node become `Dirty` and recompute.
//!   A recompute that produces an equal value does not dirty its observers:
//!   that is the equality cut-off.
//! * Effects run in creation order (owners before the nodes they own), and
//!   only after every write of the tick has been pushed, so no effect ever
//!   sees a half-propagated graph. `on change` handlers run after the
//!   other queued sinks have settled, so they fire once per outside write. An effect re-triggered by a later
//!   effect's write runs again in the same flush; a sink re-triggered past
//!   [`MAX_RUNS_PER_FLUSH`] through a feedback path is a runtime cycle and
//!   is parked.
//! * Edges to sources a computation read for the first time are linked
//!   after it ran; if something was written during the run, the node is
//!   re-checked so a write it caused upstream of a new source can't leave
//!   it clean above a dirty source (it would never run again).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use slotmap::{SecondaryMap, SlotMap, new_key_type};

use crate::error::{CyclePath, Error};

new_key_type! {
    /// A generational node id. Once a node is disposed its id never aliases
    /// a new node; reads through it return [`Error::Disposed`].
    pub struct NodeId;
}

/// What a node is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// Writable state.
    Signal,
    /// Lazy derived value.
    Memo,
    /// Observer at the edge, run at tick end.
    Effect,
    /// A sink without a closure: reports its target in [`Tick::changed`].
    Watch,
    /// An ownership scope.
    Scope,
    /// A handler coroutine.
    Task,
    /// `after T while cond` / `every T while cond` / a debounce.
    Timer,
    /// A lossless event queue.
    Events,
    /// A listener on an event queue.
    Listener,
    /// A keyed collection (source or derived).
    Collection,
}

impl NodeKind {
    /// Sinks are queued for the flush when they leave `Clean`.
    fn is_sink(self) -> bool {
        matches!(self, Self::Effect | Self::Watch | Self::Timer)
    }
    /// Derived nodes are brought up to date before reads.
    fn is_derived(self) -> bool {
        matches!(self, Self::Memo | Self::Collection)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Color {
    Clean,
    Check,
    Dirty,
}

/// What running a node did.
pub(crate) enum RunOutcome {
    /// Value unchanged (or a sink ran).
    Unchanged,
    /// A derived value changed; observers become dirty.
    Changed,
    /// A sink's handler returned an error.
    Failed(Error),
}

/// Type-erased per-kind payload of a node.
pub(crate) trait NodeData: 'static {
    fn as_any(&self) -> &dyn Any;
    /// Recompute a derived value or run a sink. Called with tracking active.
    fn run(&self, _rt: &Runtime, _id: NodeId) -> RunOutcome {
        RunOutcome::Unchanged
    }
    /// Deliver queued events (event queues only).
    fn deliver(&self, _rt: &Runtime, _id: NodeId, _errors: &mut Vec<(NodeId, Error)>) -> bool {
        false
    }
    /// Called once, outside any borrow, when the node is disposed.
    fn on_dispose(&self, _rt: &Runtime, _id: NodeId) {}
}

pub(crate) struct Node {
    pub(crate) kind: NodeKind,
    pub(crate) color: Color,
    /// On the update stack (checking or running): reaching it again is a
    /// cycle.
    pub(crate) computing: bool,
    /// Its closure is executing right now.
    pub(crate) running: bool,
    /// Bumped whenever the node's value changes (a cell write or a derived
    /// recompute that changed), so watches compare a counter, not a copy.
    pub(crate) version: u32,
    pub(crate) seq: u64,
    pub(crate) data: Option<Rc<dyn NodeData>>,
    pub(crate) sources: Vec<NodeId>,
    pub(crate) observers: Vec<NodeId>,
    pub(crate) owner: Option<NodeId>,
}

struct Frame {
    observer: Option<NodeId>,
    sources: Vec<NodeId>,
}

/// How a handler body runs.
#[derive(Copy, Clone, Debug)]
pub(crate) struct HandlerCtx {
    /// Its identity for the write-rate guard and cycle paths.
    pub(crate) writer: NodeId,
    /// Nodes it creates belong here (its component), so they outlive this
    /// one invocation.
    pub(crate) owner: Option<NodeId>,
    /// Tasks it spawns belong here, so disposing the handler cancels them.
    pub(crate) site: Option<NodeId>,
    /// Started by external input: writes are not rate-counted.
    pub(crate) input: bool,
}

/// A non-fatal report the host may show (the overlay, `strand watch`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    /// More than 30 writes per second to one cell from one handler; further
    /// writes in the window are throttled (latest value wins).
    WriteRate {
        /// The cell being written.
        cell: NodeId,
        /// The handler writing it.
        writer: NodeId,
        /// Path `writer -> cell` with debug names.
        names: CyclePath,
    },
    /// A handler coroutine was cancelled while suspended (its owner went
    /// away or it was restarted).
    Cancelled {
        /// The task.
        task: NodeId,
    },
    /// An `every` timer's period evaluated to zero; it is paused until the
    /// period is positive again (a zero period would wake the host forever).
    ZeroPeriod {
        /// The timer.
        timer: NodeId,
    },
    /// A frozen listener missed more than [`MAX_FROZEN_EVENTS`] events of a
    /// lossless queue; the oldest were dropped. Reported when it is
    /// released, before the kept events are delivered.
    EventsDropped {
        /// The event queue.
        queue: NodeId,
        /// The listener that was frozen.
        listener: NodeId,
        /// How many events it lost.
        dropped: usize,
    },
    /// A persisted cell kept its stored value although its declared default
    /// changed (the overlay's `path: kept "…" (default changed) [reset]`).
    /// Reported once: the file is re-stamped with the new default.
    PersistDefaultChanged {
        /// The cell.
        cell: NodeId,
        /// Its persist path.
        path: Arc<str>,
    },
    /// A persisted value could not be read (the cell starts from its
    /// default) or written.
    PersistFailed {
        /// The cell.
        cell: NodeId,
        /// Its persist path.
        path: Arc<str>,
        /// What went wrong.
        error: crate::persist::PersistError,
    },
}

/// Events kept per frozen listener of a lossless queue; past this the
/// oldest are dropped (and counted, see [`Diagnostic::EventsDropped`]), so a
/// component frozen for hours under a chatty service stays bounded.
pub const MAX_FROZEN_EVENTS: usize = 256;

/// What one flush did. The scene emitter builds one diff from this.
#[derive(Debug, Default, Clone)]
pub struct Tick {
    /// Monotonic tick number.
    pub seq: u64,
    /// Watched nodes ([`Runtime::watch`]) whose value changed this tick, in
    /// creation order, each once.
    pub changed: Vec<NodeId>,
    /// Cells (signals, collections) whose value changed since the last
    /// flush, each once.
    pub written: Vec<NodeId>,
    /// Number of sink runs (effects, watches, timers) this flush.
    pub effects_run: usize,
    /// Handler errors and runtime cycles, as values.
    pub errors: Vec<(NodeId, Error)>,
    /// Warnings raised since the previous tick (write-rate throttling,
    /// cancelled handlers, zero periods), so `strand watch --json` and the
    /// overlay can attribute them to this tick.
    pub diagnostics: Vec<Diagnostic>,
}

/// Counters for tests and benchmarks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Derived recomputations since the runtime started.
    pub computations: u64,
    /// Sink runs since the runtime started.
    pub effect_runs: u64,
    /// Derived collections recomputed from scratch instead of from diffs.
    pub rebuilds: u64,
    /// Live nodes.
    pub nodes: usize,
}

/// An effect may be re-triggered within one flush by writes from later
/// effects (and an event queue re-filled by listeners). Past this many runs
/// in one flush the runtime looks for a feedback path through the writes of
/// this flush; if there is one it is a runtime cycle: [`Error::Cycle`] names
/// the path and the node is parked until something outside writes to it.
pub const MAX_RUNS_PER_FLUSH: u32 = 16;

/// A node re-triggered this many times in one flush is parked and reported
/// even when no feedback path was found (a safety net; legitimate fan-in
/// chains stay far below it).
pub const HARD_RUNS_PER_FLUSH: u32 = MAX_RUNS_PER_FLUSH * 16;

type Cleanup = Box<dyn FnOnce()>;

pub(crate) struct Inner {
    pub(crate) nodes: RefCell<SlotMap<NodeId, Node>>,
    pub(crate) owned: RefCell<SecondaryMap<NodeId, Vec<NodeId>>>,
    root_owned: RefCell<Vec<NodeId>>,
    cleanups: RefCell<SecondaryMap<NodeId, Vec<Cleanup>>>,
    root_cleanups: RefCell<Vec<Box<dyn FnOnce()>>>,
    names: RefCell<SecondaryMap<NodeId, Arc<str>>>,
    tracking: RefCell<Vec<Frame>>,
    /// Recycled source lists, so a recompute allocates nothing.
    pool: RefCell<Vec<Vec<NodeId>>>,
    /// Recycled stack for `propagate`.
    scratch: RefCell<Vec<(NodeId, Color)>>,
    computing_stack: RefCell<Vec<NodeId>>,
    pub(crate) owner: Cell<Option<NodeId>>,
    pub(crate) writer: Cell<Option<NodeId>>,
    /// The handler site that owns tasks spawned right now (see
    /// [`Runtime::handler_site`]); `None` means the current owner.
    pub(crate) site: Cell<Option<NodeId>>,
    /// The running handler was started by external input (`on click`,
    /// `on scroll`): its writes are not counted by the write-rate guard.
    pub(crate) input: Cell<bool>,
    /// Handler → its site: disposing the handler disposes the site and so
    /// cancels the handler's in-flight tasks.
    sites: RefCell<SecondaryMap<NodeId, NodeId>>,
    /// Suspended scopes (a faulted component): their sinks, timers, tasks
    /// and listeners do not run until resumed.
    pub(crate) suspended: RefCell<HashSet<NodeId>>,
    /// Sinks and tasks skipped because they are inside a suspended scope
    /// (each once, in the order they were held).
    held: RefCell<Vec<NodeId>>,
    held_set: RefCell<HashSet<NodeId>>,
    /// Sinks that run only after the other queued sinks have settled
    /// (`on change` handlers), so they see one consistent state per tick.
    /// A set, not a node field: few nodes are late and the node stays small.
    late: RefCell<HashSet<NodeId>>,
    /// Event queues holding events for suspended listeners.
    pub(crate) backlogged: RefCell<Vec<NodeId>>,
    /// Bumped at the start of every `advance_to` and `flush`: the write-rate
    /// guard coalesces attempts per logic step, so timer-body writes and the
    /// following flush's writes count separately.
    pub(crate) epoch: Cell<u64>,
    pending: RefCell<Vec<NodeId>>,
    seq: Cell<u64>,
    pub(crate) flushing: Cell<bool>,
    tick: Cell<u64>,
    pub(crate) now: Cell<Duration>,
    pub(crate) written: RefCell<Vec<NodeId>>,
    /// Bumped by every value change and disposal-driven dirtying; lets
    /// `run_node` skip its stale-source check when nothing was written.
    write_epoch: Cell<u64>,
    pub(crate) flush_writes: RefCell<Vec<(NodeId, NodeId)>>,
    pub(crate) diagnostics: RefCell<Vec<Diagnostic>>,
    errors: RefCell<Vec<(NodeId, Error)>>,
    stats: Cell<Stats>,
    pub(crate) echo: RefCell<SecondaryMap<NodeId, Box<dyn Any>>>,
    pub(crate) rate: RefCell<HashMap<(NodeId, NodeId), crate::rate::RateWindow>>,
    pub(crate) throttled: RefCell<Vec<crate::rate::Deferred>>,
    pub(crate) events_pending: RefCell<Vec<NodeId>>,
    pub(crate) timers: RefCell<Vec<NodeId>>,
    /// While timers catch up before a clock advance: the new time, at which
    /// resumed timers start counting.
    pub(crate) resume_at: Cell<Option<Duration>>,
    pub(crate) sleepers: RefCell<crate::task::Sleepers>,
    pub(crate) ready: Arc<crate::task::ReadyQueue>,
}

/// The reactive runtime of one logic thread.
///
/// Cloning is cheap (it is a reference-counted handle). It is not `Send`:
/// the logic thread owns it and other threads talk to it through channels.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Rc<Inner>,
}

/// A weak handle for futures and closures stored inside the runtime, so they
/// don't keep it alive.
#[derive(Clone)]
pub struct WeakRuntime(Weak<Inner>);

impl WeakRuntime {
    /// The runtime, if it is still alive.
    pub fn upgrade(&self) -> Option<Runtime> {
        self.0.upgrade().map(|inner| Runtime { inner })
    }
}

impl fmt::Debug for WeakRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WeakRuntime")
    }
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("nodes", &self.inner.nodes.borrow().len())
            .field("tick", &self.inner.tick.get())
            .field("now", &self.inner.now.get())
            .finish()
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// An ownership scope: disposing it disposes every node created inside it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scope {
    id: NodeId,
}

impl Scope {
    /// The scope's node id.
    pub fn id(self) -> NodeId {
        self.id
    }
    /// Dispose the scope and everything it owns.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }
    /// Run `f` with this scope as the owner of new nodes.
    pub fn run<R>(self, rt: &Runtime, f: impl FnOnce(&Runtime) -> R) -> Result<R, Error> {
        rt.with_owner(self.id, f)
    }
}

impl Runtime {
    /// A fresh runtime at time zero.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner {
                nodes: RefCell::new(SlotMap::with_key()),
                owned: RefCell::new(SecondaryMap::new()),
                root_owned: RefCell::new(Vec::new()),
                cleanups: RefCell::new(SecondaryMap::new()),
                root_cleanups: RefCell::new(Vec::new()),
                names: RefCell::new(SecondaryMap::new()),
                tracking: RefCell::new(Vec::new()),
                pool: RefCell::new(Vec::new()),
                scratch: RefCell::new(Vec::new()),
                computing_stack: RefCell::new(Vec::new()),
                owner: Cell::new(None),
                writer: Cell::new(None),
                site: Cell::new(None),
                input: Cell::new(false),
                sites: RefCell::new(SecondaryMap::new()),
                suspended: RefCell::new(HashSet::new()),
                held: RefCell::new(Vec::new()),
                held_set: RefCell::new(HashSet::new()),
                late: RefCell::new(HashSet::new()),
                backlogged: RefCell::new(Vec::new()),
                epoch: Cell::new(0),
                pending: RefCell::new(Vec::new()),
                seq: Cell::new(0),
                flushing: Cell::new(false),
                tick: Cell::new(0),
                now: Cell::new(Duration::ZERO),
                written: RefCell::new(Vec::new()),
                write_epoch: Cell::new(0),
                flush_writes: RefCell::new(Vec::new()),
                diagnostics: RefCell::new(Vec::new()),
                errors: RefCell::new(Vec::new()),
                stats: Cell::new(Stats::default()),
                echo: RefCell::new(SecondaryMap::new()),
                rate: RefCell::new(HashMap::new()),
                throttled: RefCell::new(Vec::new()),
                events_pending: RefCell::new(Vec::new()),
                timers: RefCell::new(Vec::new()),
                resume_at: Cell::new(None),
                sleepers: RefCell::new(crate::task::Sleepers::default()),
                ready: Arc::new(crate::task::ReadyQueue::default()),
            }),
        }
    }

    /// A weak handle to this runtime.
    pub fn downgrade(&self) -> WeakRuntime {
        WeakRuntime(Rc::downgrade(&self.inner))
    }

    // ----- node storage ---------------------------------------------------

    pub(crate) fn create_node(
        &self,
        kind: NodeKind,
        color: Color,
        data: Option<Rc<dyn NodeData>>,
    ) -> NodeId {
        let seq = self.inner.seq.get();
        self.inner.seq.set(seq + 1);
        let owner = self.inner.owner.get();
        let id = self.inner.nodes.borrow_mut().insert(Node {
            kind,
            color,
            computing: false,
            running: false,
            version: 0,
            seq,
            data,
            sources: Vec::new(),
            observers: Vec::new(),
            owner,
        });
        match owner {
            Some(o) => {
                let mut owned = self.inner.owned.borrow_mut();
                match owned.get_mut(o) {
                    Some(list) => list.push(id),
                    None => {
                        owned.insert(o, vec![id]);
                    }
                }
            }
            None => self.inner.root_owned.borrow_mut().push(id),
        }
        if color != Color::Clean && kind.is_sink() {
            self.inner.pending.borrow_mut().push(id);
        }
        id
    }

    pub(crate) fn data(&self, id: NodeId) -> Result<Rc<dyn NodeData>, Error> {
        self.inner
            .nodes
            .borrow()
            .get(id)
            .and_then(|n| n.data.clone())
            .ok_or(Error::Disposed(id))
    }

    /// Run `f` on the typed payload of `id`.
    pub(crate) fn with_data<D: 'static, R>(
        &self,
        id: NodeId,
        f: impl FnOnce(&D) -> R,
    ) -> Result<R, Error> {
        let data = self.data(id)?;
        let typed = data
            .as_any()
            .downcast_ref::<D>()
            .ok_or(Error::TypeMismatch(id))?;
        Ok(f(typed))
    }

    /// True while `id` is `Check` or `Dirty` (queued to run or recompute).
    pub(crate) fn is_stale(&self, id: NodeId) -> bool {
        self.inner
            .nodes
            .borrow()
            .get(id)
            .is_some_and(|n| n.color != Color::Clean)
    }

    /// Make a sink run after the other queued sinks have settled.
    pub(crate) fn set_late(&self, id: NodeId) {
        if self.exists(id) {
            self.inner.late.borrow_mut().insert(id);
        }
    }

    /// True while `id` names a live node.
    pub fn exists(&self, id: NodeId) -> bool {
        self.inner.nodes.borrow().contains_key(id)
    }

    /// The kind of a live node.
    pub fn kind(&self, id: NodeId) -> Result<NodeKind, Error> {
        self.inner
            .nodes
            .borrow()
            .get(id)
            .map(|n| n.kind)
            .ok_or(Error::Disposed(id))
    }

    /// The owner of a live node (`None` for root-owned nodes).
    pub fn owner_of(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        self.inner
            .nodes
            .borrow()
            .get(id)
            .map(|n| n.owner)
            .ok_or(Error::Disposed(id))
    }

    /// The nodes `id` read on its last run, in read order (graph
    /// introspection for the inspector, `strand watch` and the LSP).
    pub fn sources(&self, id: NodeId) -> Result<Vec<NodeId>, Error> {
        self.inner
            .nodes
            .borrow()
            .get(id)
            .map(|n| n.sources.clone())
            .ok_or(Error::Disposed(id))
    }

    /// The nodes that read `id` on their last run (no particular order).
    pub fn observers(&self, id: NodeId) -> Result<Vec<NodeId>, Error> {
        self.inner
            .nodes
            .borrow()
            .get(id)
            .map(|n| n.observers.clone())
            .ok_or(Error::Disposed(id))
    }

    /// The nodes `id` owns (disposed with it), in creation order.
    pub fn owned(&self, id: NodeId) -> Result<Vec<NodeId>, Error> {
        if !self.exists(id) {
            return Err(Error::Disposed(id));
        }
        Ok(self
            .inner
            .owned
            .borrow()
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    /// Nodes owned by no one (alive until [`Runtime::shutdown`]).
    pub fn root_owned(&self) -> Vec<NodeId> {
        self.inner.root_owned.borrow().clone()
    }

    /// Give a node a debug name used in cycle paths and diagnostics.
    pub fn set_name(&self, id: NodeId, name: impl Into<Arc<str>>) {
        if self.exists(id) {
            self.inner.names.borrow_mut().insert(id, name.into());
        }
    }

    /// The debug name of `id`, or `#<index>`.
    pub fn name(&self, id: NodeId) -> Arc<str> {
        if let Some(n) = self.inner.names.borrow().get(id) {
            return n.clone();
        }
        let raw = slotmap::Key::data(&id).as_ffi();
        Arc::from(format!("#{}", raw & 0xffff_ffff))
    }

    pub(crate) fn path(&self, nodes: Vec<NodeId>) -> CyclePath {
        let names = nodes.iter().map(|&n| self.name(n)).collect();
        CyclePath { nodes, names }
    }

    /// Counters for tests and benchmarks.
    pub fn stats(&self) -> Stats {
        let mut s = self.inner.stats.get();
        s.nodes = self.inner.nodes.borrow().len();
        s
    }

    pub(crate) fn bump(&self, f: impl FnOnce(&mut Stats)) {
        let mut s = self.inner.stats.get();
        f(&mut s);
        self.inner.stats.set(s);
    }

    /// The logic clock: time since the runtime started, as last given to
    /// [`Runtime::advance_to`].
    pub fn now(&self) -> Duration {
        self.inner.now.get()
    }

    /// The current tick number (incremented by every flush).
    pub fn tick_seq(&self) -> u64 {
        self.inner.tick.get()
    }

    /// The handler currently running (effect, listener, timer or task).
    pub fn current_writer(&self) -> Option<NodeId> {
        self.inner.writer.get()
    }

    /// The owner new nodes are attached to.
    pub fn current_owner(&self) -> Option<NodeId> {
        self.inner.owner.get()
    }

    // ----- tracking -------------------------------------------------------

    /// Record that the running computation read `id`.
    pub(crate) fn track(&self, id: NodeId) {
        let mut tracking = self.inner.tracking.borrow_mut();
        if let Some(frame) = tracking.last_mut()
            && frame.observer.is_some()
            && !frame.sources.contains(&id)
        {
            frame.sources.push(id);
        }
    }

    /// Run `f` without recording dependencies.
    pub fn untrack<R>(&self, f: impl FnOnce(&Runtime) -> R) -> R {
        self.inner.tracking.borrow_mut().push(Frame {
            observer: None,
            sources: Vec::new(),
        });
        let r = f(self);
        self.inner.tracking.borrow_mut().pop();
        r
    }

    /// Run `f` with `owner` as the owner of new nodes.
    pub fn with_owner<R>(&self, owner: NodeId, f: impl FnOnce(&Runtime) -> R) -> Result<R, Error> {
        if !self.exists(owner) {
            return Err(Error::Disposed(owner));
        }
        let prev = self.inner.owner.replace(Some(owner));
        let r = f(self);
        self.inner.owner.set(prev);
        Ok(r)
    }

    /// Run a handler body (untracked) as `h` describes.
    pub(crate) fn run_handler<R>(&self, h: HandlerCtx, f: impl FnOnce(&Runtime) -> R) -> R {
        let owner = h.owner.filter(|&o| self.exists(o));
        let prev_writer = self.inner.writer.replace(Some(h.writer));
        let prev_owner = self.inner.owner.replace(owner);
        let prev_site = self.inner.site.replace(h.site);
        let prev_input = self.inner.input.replace(h.input);
        let r = self.untrack(f);
        self.inner.input.set(prev_input);
        self.inner.site.set(prev_site);
        self.inner.owner.set(prev_owner);
        self.inner.writer.set(prev_writer);
        r
    }

    /// A handler site: a node that owns the coroutines a handler starts, so
    /// disposing it (the VM does when handler code changes on reload)
    /// cancels them at their current `await` and reports
    /// [`Diagnostic::Cancelled`]. Pass it to [`Runtime::spawn_for`]. It is
    /// owned by the current owner (the component); listeners, timers and
    /// `on change` handlers get their own site automatically.
    pub fn handler_site(&self) -> NodeId {
        self.create_node(NodeKind::Scope, Color::Clean, None)
    }

    /// Create the site of `handler`: owned by no one, disposed with the
    /// handler (not when the handler re-evaluates its condition).
    pub(crate) fn create_site_for(&self, handler: NodeId) -> NodeId {
        let seq = self.inner.seq.get();
        self.inner.seq.set(seq + 1);
        let site = self.inner.nodes.borrow_mut().insert(Node {
            kind: NodeKind::Scope,
            color: Color::Clean,
            computing: false,
            running: false,
            version: 0,
            seq,
            data: None,
            sources: Vec::new(),
            observers: Vec::new(),
            // Not in the handler's owned list (re-evaluating the handler
            // must not dispose it); the back-pointer is for suspension.
            owner: Some(handler),
        });
        self.inner.sites.borrow_mut().insert(handler, site);
        site
    }

    /// The site owning the tasks a listener, timer or `on change` handler
    /// started (for a listener, the listener itself): `rt.owned(site)` are
    /// its in-flight coroutines (the inspector, reload reporting).
    pub fn site_of(&self, handler: NodeId) -> Option<NodeId> {
        if let Some(&site) = self.inner.sites.borrow().get(handler) {
            return Some(site);
        }
        (self.kind(handler) == Ok(NodeKind::Listener)).then_some(handler)
    }

    /// Create a scope owned by the current owner and run `f` inside it.
    pub fn scope<R>(&self, f: impl FnOnce(&Runtime) -> R) -> (Scope, R) {
        let id = self.create_node(NodeKind::Scope, Color::Clean, None);
        let prev = self.inner.owner.replace(Some(id));
        let r = f(self);
        self.inner.owner.set(prev);
        (Scope { id }, r)
    }

    /// Register `f` to run when the current owner is disposed or re-runs.
    /// Without an owner it runs at [`Runtime::shutdown`].
    pub fn on_cleanup(&self, f: impl FnOnce() + 'static) {
        match self.inner.owner.get() {
            Some(o) if self.exists(o) => {
                let mut c = self.inner.cleanups.borrow_mut();
                match c.get_mut(o) {
                    Some(list) => list.push(Box::new(f)),
                    None => {
                        c.insert(o, vec![Box::new(f)]);
                    }
                }
            }
            _ => self.inner.root_cleanups.borrow_mut().push(Box::new(f)),
        }
    }

    // ----- push -----------------------------------------------------------

    /// Mark `from`'s observers dirty and everything below them `Check`.
    ///
    /// `pulled` means `from` was recomputed lazily by a read: then nodes
    /// whose closure is running right now are skipped, because they are the
    /// reader and get the fresh value. A write (`pulled == false`) dirties a
    /// running effect too, so it runs again.
    pub(crate) fn propagate(&self, from: NodeId, pulled: bool) {
        let mut nodes = self.inner.nodes.borrow_mut();
        let mut pending = self.inner.pending.borrow_mut();
        let mut stack = std::mem::take(&mut *self.inner.scratch.borrow_mut());
        match nodes.get(from) {
            Some(n) => stack.extend(n.observers.iter().map(|&o| (o, Color::Dirty))),
            None => return,
        }
        while let Some((id, color)) = stack.pop() {
            let Some(node) = nodes.get_mut(id) else {
                continue;
            };
            if node.color >= color || pulled && node.running {
                continue;
            }
            let was_clean = node.color == Color::Clean;
            if was_clean && node.kind.is_sink() {
                pending.push(id);
            }
            node.color = color;
            // A node that was already Check has its descendants marked.
            if was_clean {
                stack.extend(node.observers.iter().map(|&o| (o, Color::Check)));
            }
        }
        *self.inner.scratch.borrow_mut() = stack;
    }

    /// Called by every cell write that changed a value.
    pub(crate) fn cell_changed(&self, id: NodeId) {
        self.bump_version(id);
        self.drop_deferred(id);
        self.inner.written.borrow_mut().push(id);
        if self.inner.flushing.get()
            && let Some(w) = self.inner.writer.get()
        {
            self.inner.flush_writes.borrow_mut().push((w, id));
        }
        self.propagate(id, false);
    }

    fn bump_version(&self, id: NodeId) {
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.version = n.version.wrapping_add(1);
        }
        self.inner.write_epoch.set(self.inner.write_epoch.get() + 1);
    }

    /// Error if a derived value is computing (writes there are forbidden).
    pub(crate) fn check_write_allowed(&self, cell: NodeId) -> Result<(), Error> {
        let stack = self.inner.computing_stack.borrow();
        if let Some(&memo) = stack.last() {
            let nodes = self.inner.nodes.borrow();
            if nodes.get(memo).is_some_and(|n| n.kind.is_derived()) {
                return Err(Error::WriteInDerived { cell, memo });
            }
        }
        Ok(())
    }

    // ----- pull -----------------------------------------------------------

    fn cycle_error(&self, id: NodeId) -> Error {
        let stack = self.inner.computing_stack.borrow();
        let start = stack.iter().position(|&n| n == id).unwrap_or(0);
        let mut path: Vec<NodeId> = stack[start..].to_vec();
        drop(stack);
        path.push(id);
        let e = Error::Cycle(Arc::new(self.path(path)));
        // Also report it in the tick: watches only see the memo's value.
        self.record_error(id, e.clone());
        e
    }

    /// Bring `id` up to date if it is `Check` or `Dirty`.
    pub(crate) fn update_if_necessary(&self, id: NodeId) -> Result<(), Error> {
        let (color, computing, kind) = {
            let nodes = self.inner.nodes.borrow();
            let n = nodes.get(id).ok_or(Error::Disposed(id))?;
            (n.color, n.computing, n.kind)
        };
        if computing {
            return Err(self.cycle_error(id));
        }
        if color == Color::Clean {
            return Ok(());
        }
        // Mark as visiting for the check and the run: reaching it again
        // before we are done is a cycle, and the stack names its path.
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.computing = true;
        }
        self.inner.computing_stack.borrow_mut().push(id);
        let r = self.update_visiting(id, color, kind);
        self.inner.computing_stack.borrow_mut().pop();
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.computing = false;
        }
        r
    }

    fn update_visiting(&self, id: NodeId, color: Color, kind: NodeKind) -> Result<(), Error> {
        if color == Color::Check {
            let mut i = 0;
            loop {
                let src = {
                    let nodes = self.inner.nodes.borrow();
                    match nodes.get(id) {
                        Some(n) if n.color == Color::Check => n.sources.get(i).copied(),
                        Some(_) => None,
                        None => return Err(Error::Disposed(id)),
                    }
                };
                let Some(src) = src else { break };
                let derived = self
                    .inner
                    .nodes
                    .borrow()
                    .get(src)
                    .is_some_and(|n| n.kind.is_derived());
                if derived && let Err(e @ Error::Cycle(_)) = self.update_if_necessary(src) {
                    return Err(e);
                }
                i += 1;
            }
        }
        let dirty = {
            let mut nodes = self.inner.nodes.borrow_mut();
            let n = nodes.get_mut(id).ok_or(Error::Disposed(id))?;
            let dirty = n.color == Color::Dirty;
            // Clean before running so a write during the run re-queues it.
            n.color = Color::Clean;
            dirty
        };
        if dirty {
            self.run_node(id, kind);
        }
        Ok(())
    }

    fn run_node(&self, id: NodeId, kind: NodeKind) {
        let Ok(data) = self.data(id) else { return };
        if kind != NodeKind::Scope && kind != NodeKind::Task {
            self.dispose_owned(id);
        }
        let sources = self.inner.pool.borrow_mut().pop().unwrap_or_default();
        let epoch = self.inner.write_epoch.get();
        let written_before = self.inner.written.borrow().len();
        self.inner.tracking.borrow_mut().push(Frame {
            observer: Some(id),
            sources,
        });
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.running = true;
        }
        let prev_owner = self.inner.owner.replace(Some(id));
        // Tasks an effect spawns are its own (cancelled when it re-runs);
        // graph-triggered handlers are always counted by the rate guard.
        let prev_site = self.inner.site.replace(None);
        let prev_input = self.inner.input.replace(false);
        let prev_writer = if kind.is_sink() {
            Some(self.inner.writer.replace(Some(id)))
        } else {
            None
        };

        let outcome = data.run(self, id);

        self.inner.input.set(prev_input);
        self.inner.site.set(prev_site);
        self.inner.owner.set(prev_owner);
        if let Some(w) = prev_writer {
            self.inner.writer.set(w);
        }
        let frame = self.inner.tracking.borrow_mut().pop();
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.running = false;
        }
        if let Some(frame) = frame {
            self.set_sources(id, frame.sources);
        }
        if self.inner.write_epoch.get() != epoch {
            self.recheck_sources(id, written_before);
        }
        if kind.is_derived() {
            self.bump(|s| s.computations += 1);
        } else {
            self.bump(|s| s.effect_runs += 1);
        }
        match outcome {
            RunOutcome::Changed => {
                if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
                    n.version = n.version.wrapping_add(1);
                }
                self.propagate(id, true);
            }
            RunOutcome::Unchanged => {}
            RunOutcome::Failed(e) => self.record_error(id, e),
        }
    }

    /// Something was written while `id` ran. Observer edges to sources it
    /// read for the first time only exist now, so a write made after such a
    /// read (by `id` itself, or a disposal) did not reach it: if any source
    /// is no longer clean, or is a cell written during the run, mark `id`
    /// dirty again. This keeps the invariant that a dirty node's observers
    /// are dirty or queued, so an effect never goes deaf.
    fn recheck_sources(&self, id: NodeId, written_before: usize) {
        let stale = {
            let nodes = self.inner.nodes.borrow();
            let written = self.inner.written.borrow();
            let recent = written.get(written_before..).unwrap_or(&[]);
            nodes.get(id).is_some_and(|n| {
                n.sources.iter().any(|s| {
                    nodes.get(*s).is_some_and(|src| src.color != Color::Clean) || recent.contains(s)
                })
            })
        };
        if stale {
            self.mark_dirty_and_downstream(id);
        }
    }

    /// Install `new` as `id`'s sources and fix observer edges; the list
    /// that is no longer needed goes back to the pool.
    fn set_sources(&self, id: NodeId, new: Vec<NodeId>) {
        let mut nodes = self.inner.nodes.borrow_mut();
        let Some(node) = nodes.get_mut(id) else {
            return;
        };
        let mut spare = if node.sources == new {
            new
        } else {
            let old = std::mem::replace(&mut node.sources, new);
            let large = old.len() > 16 || nodes[id].sources.len() > 16;
            let old_set: std::collections::HashSet<NodeId> = if large {
                old.iter().copied().collect()
            } else {
                Default::default()
            };
            let in_old = |s: &NodeId| {
                if large {
                    old_set.contains(s)
                } else {
                    old.contains(s)
                }
            };
            for i in 0..nodes[id].sources.len() {
                let s = nodes[id].sources[i];
                if !in_old(&s)
                    && let Some(src) = nodes.get_mut(s)
                {
                    src.observers.push(id);
                }
            }
            let new_set: std::collections::HashSet<NodeId> = if large {
                nodes[id].sources.iter().copied().collect()
            } else {
                Default::default()
            };
            for &s in &old {
                let kept = if large {
                    new_set.contains(&s)
                } else {
                    nodes[id].sources.contains(&s)
                };
                if !kept && let Some(src) = nodes.get_mut(s) {
                    unlink(&mut src.observers, id);
                }
            }
            old
        };
        spare.clear();
        drop(nodes);
        let mut pool = self.inner.pool.borrow_mut();
        if pool.len() < 64 {
            pool.push(spare);
        }
    }

    // ----- errors and diagnostics ----------------------------------------

    pub(crate) fn record_error(&self, id: NodeId, e: Error) {
        self.inner.errors.borrow_mut().push((id, e));
    }

    fn take_errors(&self) -> Vec<(NodeId, Error)> {
        std::mem::take(&mut *self.inner.errors.borrow_mut())
    }

    /// Drain warnings raised outside a flush (write-rate throttling,
    /// cancelled handlers). [`Runtime::flush`] drains them into
    /// [`Tick::diagnostics`].
    pub fn take_diagnostics(&self) -> Vec<Diagnostic> {
        std::mem::take(&mut *self.inner.diagnostics.borrow_mut())
    }

    pub(crate) fn diagnose(&self, d: Diagnostic) {
        self.inner.diagnostics.borrow_mut().push(d);
    }

    // ----- disposal -------------------------------------------------------

    /// Dispose `id` and everything it owns: cleanups run (children first),
    /// edges are unlinked, pending handlers are cancelled. Later reads
    /// through stale handles return [`Error::Disposed`].
    pub fn dispose(&self, id: NodeId) {
        if !self.exists(id) {
            return;
        }
        let owner = self.inner.nodes.borrow().get(id).and_then(|n| n.owner);
        // Search from the back: short-lived nodes (finished handlers) are
        // the most recently created.
        let remove = |list: &mut Vec<NodeId>| {
            if let Some(p) = list.iter().rposition(|&c| c == id) {
                list.remove(p);
            }
        };
        match owner {
            Some(o) => {
                if let Some(list) = self.inner.owned.borrow_mut().get_mut(o) {
                    remove(list);
                }
            }
            None => remove(&mut self.inner.root_owned.borrow_mut()),
        }
        self.dispose_tree(vec![id], None);
    }

    /// Dispose the nodes owned by `id` (before it re-runs). `id` itself is
    /// not dirtied by losing children it read: it is about to re-run and
    /// re-reads whatever it needs.
    pub(crate) fn dispose_owned(&self, id: NodeId) {
        let children = self.inner.owned.borrow_mut().remove(id);
        let cleanups = self.inner.cleanups.borrow_mut().remove(id);
        if let Some(cs) = cleanups {
            for c in cs.into_iter().rev() {
                c();
            }
        }
        if let Some(children) = children
            && !children.is_empty()
        {
            self.dispose_tree(children, Some(id));
        }
    }

    /// Dispose `roots` and everything they own. Live observers of disposed
    /// nodes are dirtied (they re-run and see `Disposed`), except `rerun`:
    /// the node whose children these are, which is about to re-run.
    fn dispose_tree(&self, roots: Vec<NodeId>, rerun: Option<NodeId>) {
        // Collect the subtree, parents before children.
        let mut order = Vec::new();
        let mut stack = roots;
        while let Some(n) = stack.pop() {
            order.push(n);
            if let Some(children) = self.inner.owned.borrow_mut().remove(n) {
                stack.extend(children);
            }
            // A handler's site (and so its in-flight tasks) goes with it.
            if let Some(site) = self.inner.sites.borrow_mut().remove(n) {
                stack.push(site);
            }
        }
        // Cleanups and dispose hooks, children first.
        for &n in order.iter().rev() {
            let cleanups = self.inner.cleanups.borrow_mut().remove(n);
            if let Some(cs) = cleanups {
                for c in cs.into_iter().rev() {
                    c();
                }
            }
            if let Ok(data) = self.data(n) {
                data.on_dispose(self, n);
            }
        }
        // Unlink and remove; drop payloads outside the borrow.
        let mut dropped = Vec::with_capacity(order.len());
        {
            let mut nodes = self.inner.nodes.borrow_mut();
            for &n in &order {
                let Some(node) = nodes.remove(n) else {
                    continue;
                };
                for &s in &node.sources {
                    if let Some(src) = nodes.get_mut(s) {
                        unlink(&mut src.observers, n);
                    }
                }
                dropped.push(node);
            }
        }
        let mut names = self.inner.names.borrow_mut();
        let mut echo = self.inner.echo.borrow_mut();
        let mut late = self.inner.late.borrow_mut();
        for &n in &order {
            names.remove(n);
            echo.remove(n);
            if !late.is_empty() {
                late.remove(&n);
            }
        }
        drop(names);
        drop(echo);
        drop(late);
        let mut unfroze = false;
        {
            let mut suspended = self.inner.suspended.borrow_mut();
            if !suspended.is_empty() {
                for &n in &order {
                    unfroze |= suspended.remove(&n);
                }
            }
        }
        {
            let mut held = self.inner.held.borrow_mut();
            if !held.is_empty() {
                let nodes = self.inner.nodes.borrow();
                held.retain(|&n| nodes.contains_key(n));
                self.inner
                    .held_set
                    .borrow_mut()
                    .retain(|&n| nodes.contains_key(n));
            }
        }
        // A suspended scope that went away (its live parts moved out first)
        // no longer freezes what it held.
        self.release_held(unfroze);
        self.forget_rate_state();
        // Live observers of disposed nodes re-run and see the error value.
        for node in &dropped {
            for &o in &node.observers {
                if Some(o) != rerun && self.exists(o) {
                    self.mark_dirty_and_downstream(o);
                }
            }
        }
        drop(dropped);
    }

    // ----- moving and freezing subtrees -----------------------------------

    /// Move a live node (and everything it owns) to `new_owner` (`None`:
    /// owned by no one). Live reload uses it to keep identity: a component
    /// moved from `start` to `end`, or a surface's state kept across a
    /// monitor unplug, keeps its cells (and a keyed cell its diff log, so
    /// items keep their identity) when the old parent is disposed.
    ///
    /// A no-op on a disposed `id`. Making a node its own owner or
    /// ancestor is [`Error::Cycle`] naming the ownership path. The moved
    /// subtree is renumbered after every existing node, keeping its own
    /// order, so effects still run owners before owned and after the new
    /// owner.
    pub fn reparent(&self, id: NodeId, new_owner: Option<NodeId>) -> Result<(), Error> {
        if !self.exists(id) {
            return Ok(());
        }
        if let Some(o) = new_owner {
            if !self.exists(o) {
                return Err(Error::Disposed(o));
            }
            // Walk up from the new owner: reaching `id` is an ownership cycle.
            let mut chain = vec![o];
            let mut cur = Some(o);
            while let Some(c) = cur {
                if c == id {
                    chain.reverse();
                    chain.push(id);
                    return Err(Error::Cycle(Arc::new(self.path(chain))));
                }
                cur = self.inner.nodes.borrow().get(c).and_then(|n| n.owner);
                if let Some(next) = cur {
                    chain.push(next);
                }
            }
        }
        let old = self.inner.nodes.borrow().get(id).and_then(|n| n.owner);
        if old == new_owner {
            return Ok(());
        }
        let remove = |list: &mut Vec<NodeId>| {
            if let Some(p) = list.iter().rposition(|&c| c == id) {
                list.remove(p);
            }
        };
        match old {
            Some(o) => {
                if let Some(list) = self.inner.owned.borrow_mut().get_mut(o) {
                    remove(list);
                }
            }
            None => remove(&mut self.inner.root_owned.borrow_mut()),
        }
        match new_owner {
            Some(o) => {
                let mut owned = self.inner.owned.borrow_mut();
                match owned.get_mut(o) {
                    Some(list) => list.push(id),
                    None => {
                        owned.insert(o, vec![id]);
                    }
                }
            }
            None => self.inner.root_owned.borrow_mut().push(id),
        }
        // Renumber the subtree (and handler sites) in its own order.
        let mut subtree = Vec::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            subtree.push(n);
            if let Some(children) = self.inner.owned.borrow().get(n) {
                stack.extend(children.iter().copied());
            }
            if let Some(&site) = self.inner.sites.borrow().get(n) {
                stack.push(site);
            }
        }
        let mut nodes = self.inner.nodes.borrow_mut();
        subtree.sort_by_key(|&n| nodes.get(n).map_or(0, |n| n.seq));
        for n in subtree {
            if let Some(node) = nodes.get_mut(n) {
                let seq = self.inner.seq.get();
                self.inner.seq.set(seq + 1);
                node.seq = seq;
            }
        }
        if let Some(node) = nodes.get_mut(id) {
            node.owner = new_owner;
        }
        drop(nodes);
        // Moved out of a suspended scope: held work runs again.
        let unfroze = !self.inner.suspended.borrow().is_empty();
        self.release_held(unfroze);
        Ok(())
    }

    /// Freeze a subtree (a component whose handler faulted, outlined in red
    /// until the fixing reload): its effects, watches, timers, listeners
    /// and tasks stop running, but its state is kept (cells keep their
    /// latest value; each held sink runs once on release). Memos stay
    /// readable (they are pure). Work that comes due while frozen is held
    /// and done on [`Runtime::resume`]. Its timers are paused, like a
    /// `while` condition that turned false, and count again from the
    /// release. A frozen subtree schedules nothing, so the runtime can
    /// still be idle.
    ///
    /// Events from input queues ([`Runtime::input_events`]) are dropped for
    /// its listeners (a frozen component ignores input); other events
    /// (service and component events, which are lossless) are kept per
    /// listener and delivered in order after it is released, up to
    /// [`MAX_FROZEN_EVENTS`] per listener: past that the oldest are dropped
    /// and the release reports [`Diagnostic::EventsDropped`] with the
    /// count.
    pub fn suspend(&self, id: NodeId) -> Result<(), Error> {
        if !self.exists(id) {
            return Err(Error::Disposed(id));
        }
        self.inner.suspended.borrow_mut().insert(id);
        // Frozen = paused: its timers stop counting now.
        self.sync_frozen_timers();
        Ok(())
    }

    /// Unfreeze a subtree suspended with [`Runtime::suspend`]: held sinks,
    /// woken tasks and events kept for its listeners run at the next flush;
    /// timers count again from now. Calls the wake hook when it re-queued
    /// work or restarted a timer.
    pub fn resume(&self, id: NodeId) {
        if self.inner.suspended.borrow_mut().remove(&id) {
            self.release_held(true);
        }
    }

    /// Re-queue held work that is no longer suspended (after a resume, a
    /// reparent out of a suspended scope, or the disposal of a suspended
    /// scope), and wake the host if anything is now due. `unfroze`: a
    /// suspension may have ended, so a timer may be overdue.
    fn release_held(&self, unfroze: bool) {
        let backlogged = std::mem::take(&mut *self.inner.backlogged.borrow_mut());
        if !unfroze && self.inner.held.borrow().is_empty() && backlogged.is_empty() {
            return;
        }
        let held = std::mem::take(&mut *self.inner.held.borrow_mut());
        let mut keep = Vec::new();
        let mut requeued = false;
        for n in held {
            if !self.exists(n) {
                continue;
            }
            if self.is_suspended(n) {
                keep.push(n);
            } else if self.kind(n) == Ok(NodeKind::Task) {
                self.inner.ready.push_quiet(n);
                requeued = true;
            } else if self.is_stale(n) {
                self.inner.pending.borrow_mut().push(n);
                requeued = true;
            }
        }
        *self.inner.held_set.borrow_mut() = keep.iter().copied().collect();
        *self.inner.held.borrow_mut() = keep;
        // Queues re-deliver what their released listeners missed; one that
        // still has frozen listeners puts itself back on the list.
        for q in backlogged {
            if self.exists(q) {
                self.inner.events_pending.borrow_mut().push(q);
                requeued = true;
            }
        }
        // Released timers count again from now: their deadlines are back.
        let restarted = unfroze && self.sync_frozen_timers();
        if requeued || restarted {
            self.call_wake_hook();
        }
    }

    /// Sinks and tasks held by suspended scopes, waiting to run when
    /// released (the inspector, reload reporting).
    pub fn held(&self) -> Vec<NodeId> {
        self.inner.held.borrow().clone()
    }

    /// True while `id` or one of its owners (or the handler of its site) is
    /// suspended.
    pub fn is_suspended(&self, id: NodeId) -> bool {
        let suspended = self.inner.suspended.borrow();
        if suspended.is_empty() {
            return false;
        }
        let nodes = self.inner.nodes.borrow();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if suspended.contains(&c) {
                return true;
            }
            cur = nodes.get(c).and_then(|n| n.owner);
        }
        false
    }

    /// Remember a sink or task skipped because it is suspended (once: a
    /// frozen task woken at 60 Hz must not grow the list).
    pub(crate) fn hold(&self, id: NodeId) {
        if self.inner.held_set.borrow_mut().insert(id) {
            self.inner.held.borrow_mut().push(id);
        }
    }

    pub(crate) fn mark_dirty_and_downstream(&self, id: NodeId) {
        self.inner.write_epoch.set(self.inner.write_epoch.get() + 1);
        {
            let mut nodes = self.inner.nodes.borrow_mut();
            if let Some(n) = nodes.get_mut(id) {
                if n.color == Color::Clean && n.kind.is_sink() {
                    self.inner.pending.borrow_mut().push(id);
                }
                n.color = Color::Dirty;
            }
        }
        // Raise downstream to Check.
        let observers: Vec<NodeId> = self
            .inner
            .nodes
            .borrow()
            .get(id)
            .map(|n| n.observers.clone())
            .unwrap_or_default();
        let mut nodes = self.inner.nodes.borrow_mut();
        let mut pending = self.inner.pending.borrow_mut();
        let mut stack = observers;
        while let Some(o) = stack.pop() {
            let Some(node) = nodes.get_mut(o) else {
                continue;
            };
            if node.color >= Color::Check {
                continue;
            }
            if node.kind.is_sink() {
                pending.push(o);
            }
            node.color = Color::Check;
            stack.extend(node.observers.iter().copied());
        }
    }

    /// Dispose every node and run root cleanups. Breaks reference cycles
    /// between the runtime and futures or closures that captured it.
    pub fn shutdown(&self) {
        let roots = std::mem::take(&mut *self.inner.root_owned.borrow_mut());
        self.dispose_tree(roots, None);
        let cleanups = std::mem::take(&mut *self.inner.root_cleanups.borrow_mut());
        for c in cleanups.into_iter().rev() {
            c();
        }
        self.inner.pending.borrow_mut().clear();
        self.inner.throttled.borrow_mut().clear();
        self.inner.sleepers.borrow_mut().clear();
        self.inner.timers.borrow_mut().clear();
    }

    // ----- watch ----------------------------------------------------------

    /// Observe `target` (a memo, signal or collection) at the edge: when its
    /// value changes, the flush lists `target` in [`Tick::changed`]. This is
    /// how the scene emitter learns which props to put in the tick's diff
    /// without a closure per prop. Disposing the returned id stops it; it
    /// also stops by itself once `target` is disposed.
    pub fn watch(&self, target: NodeId) -> Result<NodeId, Error> {
        let kind = self.kind(target)?;
        if kind.is_derived() {
            self.update_if_necessary(target)?;
        }
        let version = self
            .inner
            .nodes
            .borrow()
            .get(target)
            .map(|n| n.version)
            .ok_or(Error::Disposed(target))?;
        let id = self.create_node(
            NodeKind::Watch,
            Color::Clean,
            Some(Rc::new(WatchData {
                target,
                last: Cell::new(version),
            })),
        );
        self.set_sources(id, vec![target]);
        Ok(id)
    }

    // ----- flush ----------------------------------------------------------

    /// True when nothing is queued for the next flush: no dirty sinks, no
    /// events, no woken handlers, no unreported writes. Work scheduled on
    /// the clock (timers, sleeping handlers, throttled writes) is reported
    /// by [`Runtime::next_deadline`] instead; a host is truly idle when
    /// this is true and that is `None`.
    pub fn is_idle(&self) -> bool {
        self.inner.pending.borrow().is_empty()
            && self.inner.events_pending.borrow().is_empty()
            && self.ready_is_empty()
            && self.inner.written.borrow().is_empty()
    }

    /// End the tick: deliver events, poll woken handlers (each at most
    /// once), run dirty sinks (in creation order; `on change` handlers
    /// after the others have settled) until quiescent, and
    /// report what changed. Writes made by sinks during the flush are part
    /// of this tick.
    ///
    /// A task that woke itself during the flush is polled at the next one;
    /// the flush then calls the wake hook ([`Runtime::set_wake_hook`]), so a
    /// host that sleeps until the hook fires or [`Runtime::next_deadline`]
    /// arrives never misses it. (Equivalently: flush again while
    /// [`Runtime::is_idle`] is false.)
    pub fn flush(&self) -> Tick {
        let mut tick = Tick::default();
        if self.inner.flushing.replace(true) {
            let at = self
                .current_writer()
                .or(self.current_owner())
                .unwrap_or_default();
            tick.errors.push((at, Error::Reentrant));
            return tick;
        }
        tick.seq = self.inner.tick.get() + 1;
        self.inner.tick.set(tick.seq);
        self.inner.epoch.set(self.inner.epoch.get() + 1);
        let mut runs: HashMap<NodeId, u32> = HashMap::new();
        let mut polled: HashSet<NodeId> = HashSet::new();
        let mut errors = Vec::new();
        loop {
            let mut progressed = self.poll_ready_tasks(&mut polled);
            progressed |= self.deliver_events(&mut runs, &mut errors);
            let mut batch = std::mem::take(&mut *self.inner.pending.borrow_mut());
            if batch.is_empty() {
                if progressed {
                    continue;
                }
                break;
            }
            {
                let nodes = self.inner.nodes.borrow();
                batch.retain(|&id| nodes.contains_key(id));
                batch.sort_by_key(|&id| nodes[id].seq);
                // `on change` handlers wait until the other sinks (which may
                // write what they track) have settled, so one outside write
                // fires them once, with the final values.
                let late = self.inner.late.borrow();
                if !late.is_empty() && batch.iter().any(|id| !late.contains(id)) {
                    let mut pending = self.inner.pending.borrow_mut();
                    batch.retain(|id| {
                        let is_late = late.contains(id);
                        if is_late {
                            pending.push(*id);
                        }
                        !is_late
                    });
                }
            }
            batch.dedup();
            for &id in &batch {
                let kind = {
                    let nodes = self.inner.nodes.borrow();
                    match nodes.get(id) {
                        Some(n) if n.color != Color::Clean => n.kind,
                        // Disposed, already run or parked.
                        _ => continue,
                    }
                };
                if self.is_suspended(id) {
                    // Frozen with its component until resumed.
                    self.hold(id);
                    continue;
                }
                if self.cycle_cut(id, &mut runs, &mut errors) {
                    self.park(id);
                    continue;
                }
                let before = self.inner.stats.get().effect_runs;
                if let Err(e) = self.update_if_necessary(id) {
                    errors.push((id, e));
                }
                let ran = self.inner.stats.get().effect_runs > before;
                if ran {
                    tick.effects_run += 1;
                    if kind == NodeKind::Watch {
                        match self.watch_changed(id) {
                            WatchOutcome::Changed(t) => tick.changed.push(t),
                            WatchOutcome::Same => {}
                            WatchOutcome::TargetGone => self.dispose(id),
                        }
                    }
                }
            }
        }
        errors.extend(self.take_errors());
        // A cycle seen on several runs is reported once.
        let mut unique: Vec<(NodeId, Error)> = Vec::with_capacity(errors.len());
        for e in errors {
            if !unique.contains(&e) {
                unique.push(e);
            }
        }
        tick.errors = unique;
        let mut written = std::mem::take(&mut *self.inner.written.borrow_mut());
        written.sort();
        written.dedup();
        written.retain(|&w| self.exists(w));
        tick.written = written;
        // `changed` in creation order of the targets, each once.
        {
            let nodes = self.inner.nodes.borrow();
            tick.changed.retain(|&t| nodes.contains_key(t));
            tick.changed.sort_by_key(|&t| nodes[t].seq);
        }
        tick.changed.dedup();
        tick.diagnostics = self.take_diagnostics();
        self.inner.flush_writes.borrow_mut().clear();
        self.inner.flushing.set(false);
        // A task that woke itself during this flush waits for the next one;
        // tell the host so a loop sleeping on the hook polls it.
        if !self.ready_is_empty() {
            self.call_wake_hook();
        }
        tick
    }

    /// Count a run (or delivery) of `id` in this flush. Past
    /// [`MAX_RUNS_PER_FLUSH`] look for a feedback path through this flush's
    /// writes; if there is one (or past [`HARD_RUNS_PER_FLUSH`]) report
    /// [`Error::Cycle`] and return `true`: the caller parks the node.
    pub(crate) fn cycle_cut(
        &self,
        id: NodeId,
        runs: &mut HashMap<NodeId, u32>,
        errors: &mut Vec<(NodeId, Error)>,
    ) -> bool {
        let count = runs.entry(id).or_insert(0);
        *count += 1;
        let count = *count;
        if count <= MAX_RUNS_PER_FLUSH {
            return false;
        }
        let path = self.feedback_path(id);
        if path.is_none() && count <= HARD_RUNS_PER_FLUSH {
            return false;
        }
        let path = path.unwrap_or_else(|| self.path(vec![id, id]));
        errors.push((id, Error::Cycle(Arc::new(path))));
        true
    }

    /// Stop a sink that is part of a runtime cycle until one of its inputs
    /// is written again: bring its derived sources up to date (so a later
    /// write reaches it) and mark it clean without running it.
    fn park(&self, id: NodeId) {
        let sources = self.sources(id).unwrap_or_default();
        for s in sources {
            if self.kind(s).is_ok_and(NodeKind::is_derived) {
                let _ = self.update_if_necessary(s);
            }
        }
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.color = Color::Clean;
        }
    }

    /// Find `start -> cell -> ... -> start` through the writes and event
    /// deliveries made during this flush and the observer edges.
    fn feedback_path(&self, start: NodeId) -> Option<CyclePath> {
        let writes = self.inner.flush_writes.borrow().clone();
        let nodes = self.inner.nodes.borrow();
        let mut prev: HashMap<NodeId, NodeId> = HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(start);
        let mut found = false;
        'bfs: while let Some(n) = queue.pop_front() {
            let mut next: Vec<NodeId> = writes
                .iter()
                .filter(|(w, _)| *w == n)
                .map(|&(_, c)| c)
                .collect();
            if let Some(node) = nodes.get(n) {
                next.extend(node.observers.iter().copied());
            }
            for m in next {
                if m == start {
                    prev.insert(start, n);
                    found = true;
                    break 'bfs;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = prev.entry(m) {
                    e.insert(n);
                    queue.push_back(m);
                }
            }
        }
        drop(nodes);
        if !found {
            return None;
        }
        let mut path = vec![start];
        let mut cur = prev[&start];
        while cur != start {
            path.push(cur);
            cur = prev[&cur];
        }
        path.push(start);
        path.reverse();
        Some(self.path(path))
    }

    /// Advance the logic clock and end the tick: fire due timers, wake due
    /// sleepers, apply due throttled writes, then [`Runtime::flush`].
    pub fn tick(&self, now: Duration) -> Tick {
        let mut errors = self.advance_to(now);
        let mut tick = self.flush();
        errors.append(&mut tick.errors);
        tick.errors = errors;
        tick
    }

    /// Move the logic clock forward (never backward) and run everything that
    /// became due: timer bodies, sleeping handlers and throttled writes.
    /// Returns handler errors from timer bodies.
    ///
    /// Timers whose condition or duration changed since the last flush are
    /// brought up to date first, so `hover = true` followed by
    /// `tick(deadline)` pauses the timer instead of firing it. A pause counts
    /// up to the previous time and a resume from `now`: a timer never counts
    /// time its condition may not have held for.
    pub fn advance_to(&self, now: Duration) -> Vec<(NodeId, Error)> {
        self.inner.epoch.set(self.inner.epoch.get() + 1);
        self.refresh_timers(now);
        if now > self.inner.now.get() {
            self.inner.now.set(now);
        }
        let mut errors = Vec::new();
        self.fire_timers(&mut errors);
        self.wake_sleepers();
        self.apply_throttled();
        errors
    }

    /// The earliest time something is scheduled (timers, sleeping handlers,
    /// throttled writes). `None` means true idle: the host can sleep until a
    /// write or event arrives.
    pub fn next_deadline(&self) -> Option<Duration> {
        let mut best: Option<Duration> = None;
        let mut consider = |d: Duration| {
            best = Some(best.map_or(d, |b| b.min(d)));
        };
        if let Some(d) = self.timer_deadline() {
            consider(d);
        }
        if let Some(d) = self.inner.sleepers.borrow().earliest() {
            consider(d);
        }
        for t in self.inner.throttled.borrow().iter() {
            consider(t.due);
        }
        best
    }
}

struct WatchData {
    target: NodeId,
    /// The target's version last reported. A value written and restored
    /// within one tick may be reported again; that costs one redundant prop
    /// update, where a copy of every watched value would cost memory.
    last: Cell<u32>,
}

enum WatchOutcome {
    Changed(NodeId),
    Same,
    TargetGone,
}

impl Runtime {
    /// After a watch ran: its target if its version moved since the last
    /// report.
    fn watch_changed(&self, watch: NodeId) -> WatchOutcome {
        let Ok(data) = self.data(watch) else {
            return WatchOutcome::Same;
        };
        let Some(w) = data.as_any().downcast_ref::<WatchData>() else {
            return WatchOutcome::Same;
        };
        let version = self.inner.nodes.borrow().get(w.target).map(|n| n.version);
        match version {
            None => WatchOutcome::TargetGone,
            Some(v) if v == w.last.get() => WatchOutcome::Same,
            Some(v) => {
                w.last.set(v);
                WatchOutcome::Changed(w.target)
            }
        }
    }
}

impl NodeData for WatchData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn run(&self, rt: &Runtime, _id: NodeId) -> RunOutcome {
        if rt.kind(self.target).is_ok_and(|k| k.is_derived()) {
            let _ = rt.update_if_necessary(self.target);
        }
        rt.track(self.target);
        RunOutcome::Unchanged
    }
}

/// Remove `id` from an observer list (each observer appears once; order
/// does not matter).
fn unlink(observers: &mut Vec<NodeId>, id: NodeId) {
    if let Some(p) = observers.iter().rposition(|&o| o == id) {
        observers.swap_remove(p);
    }
}

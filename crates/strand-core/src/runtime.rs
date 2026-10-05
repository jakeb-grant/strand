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
//! * Effects run once per flush, in creation order (owners before the nodes
//!   they own), and only after every write of the tick has been pushed, so no
//!   effect ever sees a half-propagated graph.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    /// A copy of the current value, for watches to compare against.
    fn clone_value(&self) -> Option<Box<dyn Any>> {
        None
    }
    /// Whether the current value equals a copy from [`Self::clone_value`].
    fn value_eq(&self, _other: &dyn Any) -> bool {
        false
    }
}

pub(crate) struct Node {
    pub(crate) kind: NodeKind,
    pub(crate) color: Color,
    /// On the update stack (checking or running): reaching it again is a
    /// cycle.
    pub(crate) computing: bool,
    /// Its closure is executing right now.
    pub(crate) running: bool,
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
}

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
}

/// Counters for tests and benchmarks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Derived recomputations since the runtime started.
    pub computations: u64,
    /// Sink runs since the runtime started.
    pub effect_runs: u64,
    /// Live nodes.
    pub nodes: usize,
}

/// An effect may be re-triggered within one flush by writes from later
/// effects; past this many runs in one flush it is a runtime cycle.
pub const MAX_RUNS_PER_FLUSH: u32 = 16;

type Cleanup = Box<dyn FnOnce()>;

pub(crate) struct Inner {
    pub(crate) nodes: RefCell<SlotMap<NodeId, Node>>,
    pub(crate) owned: RefCell<SecondaryMap<NodeId, Vec<NodeId>>>,
    root_owned: RefCell<Vec<NodeId>>,
    cleanups: RefCell<SecondaryMap<NodeId, Vec<Cleanup>>>,
    root_cleanups: RefCell<Vec<Box<dyn FnOnce()>>>,
    names: RefCell<SecondaryMap<NodeId, Arc<str>>>,
    tracking: RefCell<Vec<Frame>>,
    computing_stack: RefCell<Vec<NodeId>>,
    pub(crate) owner: Cell<Option<NodeId>>,
    pub(crate) writer: Cell<Option<NodeId>>,
    pending: RefCell<Vec<NodeId>>,
    seq: Cell<u64>,
    flushing: Cell<bool>,
    tick: Cell<u64>,
    pub(crate) now: Cell<Duration>,
    written: RefCell<Vec<NodeId>>,
    flush_writes: RefCell<Vec<(NodeId, NodeId)>>,
    pub(crate) diagnostics: RefCell<Vec<Diagnostic>>,
    errors: RefCell<Vec<(NodeId, Error)>>,
    stats: Cell<Stats>,
    pub(crate) echo: RefCell<SecondaryMap<NodeId, Box<dyn Any>>>,
    pub(crate) rate: RefCell<HashMap<(NodeId, NodeId), crate::rate::RateWindow>>,
    pub(crate) throttled: RefCell<Vec<crate::rate::Deferred>>,
    pub(crate) events_pending: RefCell<Vec<NodeId>>,
    pub(crate) timers: RefCell<Vec<NodeId>>,
    pub(crate) sleepers: RefCell<Vec<(Duration, std::task::Waker)>>,
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
                computing_stack: RefCell::new(Vec::new()),
                owner: Cell::new(None),
                writer: Cell::new(None),
                pending: RefCell::new(Vec::new()),
                seq: Cell::new(0),
                flushing: Cell::new(false),
                tick: Cell::new(0),
                now: Cell::new(Duration::ZERO),
                written: RefCell::new(Vec::new()),
                flush_writes: RefCell::new(Vec::new()),
                diagnostics: RefCell::new(Vec::new()),
                errors: RefCell::new(Vec::new()),
                stats: Cell::new(Stats::default()),
                echo: RefCell::new(SecondaryMap::new()),
                rate: RefCell::new(HashMap::new()),
                throttled: RefCell::new(Vec::new()),
                events_pending: RefCell::new(Vec::new()),
                timers: RefCell::new(Vec::new()),
                sleepers: RefCell::new(Vec::new()),
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

    fn bump(&self, f: impl FnOnce(&mut Stats)) {
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
        let mut stack: Vec<(NodeId, Color)> = match nodes.get(from) {
            Some(n) => n.observers.iter().map(|&o| (o, Color::Dirty)).collect(),
            None => return,
        };
        while let Some((id, color)) = stack.pop() {
            let Some(node) = nodes.get_mut(id) else {
                continue;
            };
            if node.color >= color || pulled && node.running {
                continue;
            }
            if node.color == Color::Clean && node.kind.is_sink() {
                pending.push(id);
            }
            node.color = color;
            stack.extend(node.observers.iter().map(|&o| (o, Color::Check)));
        }
    }

    /// Called by every cell write that changed a value.
    pub(crate) fn cell_changed(&self, id: NodeId) {
        self.inner.written.borrow_mut().push(id);
        if self.inner.flushing.get()
            && let Some(w) = self.inner.writer.get()
        {
            self.inner.flush_writes.borrow_mut().push((w, id));
        }
        self.propagate(id, false);
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
        Error::Cycle(self.path(path))
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
        self.inner.tracking.borrow_mut().push(Frame {
            observer: Some(id),
            sources: Vec::new(),
        });
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.running = true;
        }
        let prev_owner = self.inner.owner.replace(Some(id));
        let prev_writer = if kind.is_sink() {
            Some(self.inner.writer.replace(Some(id)))
        } else {
            None
        };

        let outcome = data.run(self, id);

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
        if kind.is_derived() {
            self.bump(|s| s.computations += 1);
        } else {
            self.bump(|s| s.effect_runs += 1);
        }
        match outcome {
            RunOutcome::Changed => self.propagate(id, true),
            RunOutcome::Unchanged => {}
            RunOutcome::Failed(e) => self.record_error(id, e),
        }
    }

    fn set_sources(&self, id: NodeId, new: Vec<NodeId>) {
        let mut nodes = self.inner.nodes.borrow_mut();
        let Some(node) = nodes.get_mut(id) else {
            return;
        };
        if node.sources == new {
            return;
        }
        let old = std::mem::replace(&mut node.sources, new.clone());
        let new = &new;
        let small = old.len() <= 16 && new.len() <= 16;
        let (removed, added): (Vec<NodeId>, Vec<NodeId>) = if small {
            (
                old.iter().copied().filter(|s| !new.contains(s)).collect(),
                new.iter().copied().filter(|s| !old.contains(s)).collect(),
            )
        } else {
            let old_set: std::collections::HashSet<NodeId> = old.iter().copied().collect();
            let new_set: std::collections::HashSet<NodeId> = new.iter().copied().collect();
            (
                old.iter()
                    .copied()
                    .filter(|s| !new_set.contains(s))
                    .collect(),
                new.iter()
                    .copied()
                    .filter(|s| !old_set.contains(s))
                    .collect(),
            )
        };
        for s in removed {
            if let Some(src) = nodes.get_mut(s) {
                src.observers.retain(|&o| o != id);
            }
        }
        for s in added {
            if let Some(src) = nodes.get_mut(s) {
                src.observers.push(id);
            }
        }
    }

    // ----- errors and diagnostics ----------------------------------------

    pub(crate) fn record_error(&self, id: NodeId, e: Error) {
        self.inner.errors.borrow_mut().push((id, e));
    }

    fn take_errors(&self) -> Vec<(NodeId, Error)> {
        std::mem::take(&mut *self.inner.errors.borrow_mut())
    }

    /// Drain warnings (write-rate throttling, cancelled handlers).
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
        match owner {
            Some(o) => {
                if let Some(list) = self.inner.owned.borrow_mut().get_mut(o) {
                    list.retain(|&c| c != id);
                }
            }
            None => self.inner.root_owned.borrow_mut().retain(|&c| c != id),
        }
        self.dispose_tree(vec![id]);
    }

    /// Dispose the nodes owned by `id` (before it re-runs).
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
            self.dispose_tree(children);
        }
    }

    fn dispose_tree(&self, roots: Vec<NodeId>) {
        // Collect the subtree, parents before children.
        let mut order = Vec::new();
        let mut stack = roots;
        while let Some(n) = stack.pop() {
            order.push(n);
            if let Some(children) = self.inner.owned.borrow_mut().remove(n) {
                stack.extend(children);
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
                        src.observers.retain(|&o| o != n);
                    }
                }
                dropped.push(node);
            }
        }
        let mut names = self.inner.names.borrow_mut();
        let mut echo = self.inner.echo.borrow_mut();
        for &n in &order {
            names.remove(n);
            echo.remove(n);
        }
        drop(names);
        drop(echo);
        // Live observers of disposed nodes re-run and see the error value.
        for node in &dropped {
            for &o in &node.observers {
                if self.exists(o) {
                    self.mark_dirty_and_downstream(o);
                }
            }
        }
        drop(dropped);
    }

    fn mark_dirty_and_downstream(&self, id: NodeId) {
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
        self.dispose_tree(roots);
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
    /// without a closure per prop. Disposing the returned id stops it.
    pub fn watch(&self, target: NodeId) -> Result<NodeId, Error> {
        let kind = self.kind(target)?;
        if kind.is_derived() {
            self.update_if_necessary(target)?;
        }
        let id = self.create_node(
            NodeKind::Watch,
            Color::Clean,
            Some(Rc::new(WatchData {
                target,
                last: RefCell::new(self.data(target)?.clone_value()),
            })),
        );
        self.set_sources(id, vec![target]);
        Ok(id)
    }

    // ----- flush ----------------------------------------------------------

    /// True when nothing is queued: no dirty sinks, no events, no woken
    /// handlers, no throttled writes. An idle runtime needs no flush.
    pub fn is_idle(&self) -> bool {
        self.inner.pending.borrow().is_empty()
            && self.inner.events_pending.borrow().is_empty()
            && self.ready_is_empty()
            && self.inner.written.borrow().is_empty()
    }

    /// End the tick: deliver events, poll woken handlers, run dirty sinks
    /// (each once, in creation order) until quiescent, and report what
    /// changed. Writes made by sinks during the flush are part of this tick.
    pub fn flush(&self) -> Tick {
        let mut tick = Tick::default();
        if self.inner.flushing.replace(true) {
            tick.errors.push((NodeId::default(), Error::Reentrant));
            return tick;
        }
        tick.seq = self.inner.tick.get() + 1;
        self.inner.tick.set(tick.seq);
        let mut runs: HashMap<NodeId, u32> = HashMap::new();
        let mut errors = Vec::new();
        'outer: loop {
            let mut progressed = self.poll_ready_tasks();
            progressed |= self.deliver_events(&mut errors);
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
            }
            batch.dedup();
            for (i, &id) in batch.iter().enumerate() {
                let count = runs.entry(id).or_insert(0);
                *count += 1;
                if *count > MAX_RUNS_PER_FLUSH {
                    let path = self.feedback_path(id);
                    errors.push((id, Error::Cycle(path)));
                    // Leave the rest queued for the next tick so a loop
                    // can't freeze the logic thread.
                    self.inner
                        .pending
                        .borrow_mut()
                        .extend_from_slice(&batch[i..]);
                    break 'outer;
                }
                let kind = match self.kind(id) {
                    Ok(k) => k,
                    Err(_) => continue,
                };
                let before = self.inner.stats.get().effect_runs;
                if let Err(e) = self.update_if_necessary(id) {
                    errors.push((id, e));
                }
                let ran = self.inner.stats.get().effect_runs > before;
                if ran {
                    tick.effects_run += 1;
                    if kind == NodeKind::Watch {
                        if let Some(t) = self.watch_changed(id) {
                            tick.changed.push(t);
                        }
                    }
                }
            }
        }
        errors.extend(self.take_errors());
        tick.errors = errors;
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
        self.inner.flush_writes.borrow_mut().clear();
        self.inner.flushing.set(false);
        tick
    }

    /// Find `effect -> cell -> ... -> effect` through the writes made during
    /// this flush and the observer edges.
    fn feedback_path(&self, start: NodeId) -> CyclePath {
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
            return self.path(vec![start, start]);
        }
        let mut path = vec![start];
        let mut cur = prev[&start];
        while cur != start {
            path.push(cur);
            cur = prev[&cur];
        }
        path.push(start);
        path.reverse();
        self.path(path)
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
    pub fn advance_to(&self, now: Duration) -> Vec<(NodeId, Error)> {
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
        for (d, _) in self.inner.sleepers.borrow().iter() {
            consider(*d);
        }
        for t in self.inner.throttled.borrow().iter() {
            consider(t.due);
        }
        best
    }
}

struct WatchData {
    target: NodeId,
    /// The value last reported, so a value written and restored within one
    /// tick is not reported.
    last: RefCell<Option<Box<dyn Any>>>,
}

impl Runtime {
    /// After a watch ran: its target if the value differs from the last
    /// reported one.
    fn watch_changed(&self, watch: NodeId) -> Option<NodeId> {
        let data = self.data(watch).ok()?;
        let w = data.as_any().downcast_ref::<WatchData>()?;
        let target = self.data(w.target).ok()?;
        let mut last = w.last.borrow_mut();
        let same = last.as_deref().is_some_and(|l| target.value_eq(l));
        if same {
            return None;
        }
        *last = target.clone_value();
        Some(w.target)
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

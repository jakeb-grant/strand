//! Lossless event queues (`on notifications.received(n)`).
//!
//! State coalesces to its latest value per tick; events never do. Every
//! event emitted is delivered exactly once, in order, to every listener
//! alive when the flush picks it up (the flush that follows the emit; an
//! event emitted by a handler during a flush is picked up right after that
//! handler, so it is delivered in the same flush).
//!
//! Each listener is delivered on its own, at its own rank (see `order`): a
//! listener ranks with its queue and with what it reads, so it runs after
//! the writers of what it reads, and what it writes ranks above it, so
//! the readers of that run after it. Listeners of one queue therefore do
//! not get an event together: one that reads what another writes for the
//! same event runs after it (and after any effect in between), whatever
//! order they were registered in. Each listener gets its events in emit
//! order. A listener's reads are tracked while it runs (without
//! subscribing): one it did not declare ranks it above that source from
//! then on, and is reported in strict mode
//! ([`Runtime::set_strict_edges`]).
//!
//! Listeners that re-emit to each other in a loop are a runtime cycle: past
//! [`crate::MAX_RUNS_PER_FLUSH`] pick-ups of one queue in one flush, if the
//! deliveries and emits of this flush form a path back to the queue, the
//! flush reports [`Error::Cycle`] (`queue -> listener -> queue …`) and parks
//! the queue: its events stay queued (nothing is lost) and are delivered
//! with the next emit to it.
//!
//! A listener inside a suspended (frozen) component does not run. Events of
//! an input queue ([`Runtime::input_events`]) are dropped for it (a frozen
//! component ignores clicks); events of any other queue are kept for it, in
//! order, and delivered once it is released ([`Runtime::resume`], or moved
//! out of the frozen scope), so a service event is not lost. The backlog is
//! bounded: past [`crate::MAX_FROZEN_EVENTS`] per listener the oldest are
//! dropped and the release reports [`Diagnostic::EventsDropped`] with the
//! count (also when the listener is disposed instead of released: the
//! events it still held go with it, the count of those it lost before is
//! reported). (State needs no bound: a cell keeps only its latest value.)

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::error::Error;
use crate::runtime::{
    Color, Diagnostic, HandlerCtx, MAX_FROZEN_EVENTS, NodeData, NodeId, NodeKind, Runtime,
};

/// A lossless queue of `T` events. Copyable handle.
pub struct EventQueue<T> {
    id: NodeId,
    _t: PhantomData<fn() -> T>,
}

impl<T> Clone for EventQueue<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for EventQueue<T> {}
impl<T> fmt::Debug for EventQueue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EventQueue({:?})", self.id)
    }
}

type ListenerFn<T> = Box<dyn FnMut(&Runtime, &T) -> Result<(), Error>>;

struct EventsData<T> {
    /// Emitted, not yet handed to the listeners. Shared (`Rc`) so one
    /// event goes to every listener without `T: Clone`.
    queue: RefCell<VecDeque<Rc<T>>>,
    listeners: RefCell<Vec<NodeId>>,
    /// External input (`on click`, `on scroll`): listeners are input
    /// handlers, not counted by the write-rate guard.
    input: bool,
}

struct ListenerData<T> {
    f: RefCell<ListenerFn<T>>,
    /// Its queue (for diagnostics).
    queue: NodeId,
    /// Its queue is an input queue: events are dropped while it is frozen.
    input: bool,
    /// Events handed to it, not yet delivered: those of this flush, or
    /// those it missed while frozen (at most [`MAX_FROZEN_EVENTS`]).
    inbox: RefCell<VecDeque<Rc<T>>>,
    /// Oldest events dropped past [`MAX_FROZEN_EVENTS`] while frozen, not
    /// yet reported.
    dropped: Cell<usize>,
}

impl<T: 'static> NodeData for EventsData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn downstream(&self) -> Vec<NodeId> {
        self.listeners.borrow().clone()
    }

    /// Hand the queued events to the live listeners; those not frozen are
    /// queued for delivery at their rank.
    fn distribute(&self, rt: &Runtime, id: NodeId) {
        let events: Vec<Rc<T>> = match self.queue.try_borrow_mut() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return,
        };
        if events.is_empty() {
            return;
        }
        let listeners: Vec<NodeId> = {
            let mut list = self.listeners.borrow_mut();
            list.retain(|&l| rt.exists(l));
            list.clone()
        };
        // For cycle paths: this queue reaches its listeners.
        rt.inner
            .flush_writes
            .borrow_mut()
            .extend(listeners.iter().map(|&l| (id, l)));
        for l in listeners {
            let Ok(data) = rt.data(l) else { continue };
            let Some(listener) = data.as_any().downcast_ref::<ListenerData<T>>() else {
                continue;
            };
            if rt.is_suspended(l) {
                // A frozen component ignores input; other events wait.
                if !self.input {
                    for ev in &events {
                        listener.keep(ev.clone());
                    }
                    rt.hold(l);
                }
                continue;
            }
            listener.inbox.borrow_mut().extend(events.iter().cloned());
            rt.inner.listeners_ready.borrow_mut().push(l);
        }
    }
}

impl<T: 'static> NodeData for ListenerData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    /// Deliver the inbox, in order (the flush calls it at the listener's
    /// rank).
    fn deliver(&self, rt: &Runtime, id: NodeId, errors: &mut Vec<(NodeId, Error)>) -> bool {
        let mut any = false;
        loop {
            if !rt.exists(id) {
                // Disposed by its own handler: the rest goes with it.
                self.inbox.borrow_mut().clear();
                break;
            }
            if rt.is_suspended(id) {
                // Frozen (possibly by its own handler): keep the rest.
                self.freeze();
                if !self.inbox.borrow().is_empty() {
                    rt.hold(id);
                }
                break;
            }
            let dropped = self.dropped.replace(0);
            if dropped > 0 {
                // Released: what it lost is reported before what it kept.
                rt.diagnose(Diagnostic::EventsDropped {
                    queue: self.queue,
                    listener: id,
                    dropped,
                });
            }
            let Some(ev) = self.inbox.borrow_mut().pop_front() else {
                break;
            };
            any = true;
            self.run(rt, id, &ev, errors);
        }
        any
    }

    /// A frozen listener disposed instead of released (the reload replaced
    /// its component): what it held is gone; what it had already lost is
    /// still reported.
    fn on_dispose(&self, rt: &Runtime, id: NodeId) {
        let dropped = self.dropped.replace(0);
        if dropped > 0 {
            rt.diagnose(Diagnostic::EventsDropped {
                queue: self.queue,
                listener: id,
                dropped,
            });
        }
        self.inbox.borrow_mut().clear();
    }
}

impl<T: 'static> ListenerData<T> {
    fn run(&self, rt: &Runtime, l: NodeId, ev: &T, errors: &mut Vec<(NodeId, Error)>) {
        let Ok(mut f) = self.f.try_borrow_mut() else {
            return;
        };
        // Nodes the listener creates belong to its component; tasks it
        // starts belong to the listener, so disposing it (unmount or a
        // reload that restarts the handler) cancels them. Its reads are
        // tracked (without subscribing) so an undeclared one ranks it.
        let ctx = HandlerCtx {
            writer: l,
            owner: rt.owner_of(l).ok().flatten(),
            site: Some(l),
            input: self.input,
            rate: None,
        };
        let r = rt.run_handler_tracked(ctx, l, |rt| f(rt, ev));
        if let Err(e) = r {
            errors.push((l, e));
        }
    }

    /// Keep `ev` for this frozen listener, dropping (and counting) the
    /// oldest past [`MAX_FROZEN_EVENTS`].
    fn keep(&self, ev: Rc<T>) {
        let mut inbox = self.inbox.borrow_mut();
        if inbox.len() >= MAX_FROZEN_EVENTS {
            inbox.pop_front();
            self.dropped.set(self.dropped.get() + 1);
        }
        inbox.push_back(ev);
    }

    /// The listener was found frozen with events in its inbox: input is
    /// dropped, the rest bounded.
    fn freeze(&self) {
        let mut inbox = self.inbox.borrow_mut();
        if self.input {
            inbox.clear();
            return;
        }
        while inbox.len() > MAX_FROZEN_EVENTS {
            inbox.pop_front();
            self.dropped.set(self.dropped.get() + 1);
        }
    }
}

impl Runtime {
    /// Create an event queue owned by the current owner (service events,
    /// component events: listeners are counted by the write-rate guard).
    pub fn events<T: 'static>(&self) -> EventQueue<T> {
        self.event_queue(false)
    }

    /// Create a queue of external input events (`on click`, `on scroll(dy)`,
    /// `on activate`). Its listeners, and the tasks they spawn, are input
    /// handlers: like CLI or service writes, their writes are not counted by
    /// the 30 writes/s guard, so 60 Hz smooth scrolling is never throttled.
    pub fn input_events<T: 'static>(&self) -> EventQueue<T> {
        self.event_queue(true)
    }

    fn event_queue<T: 'static>(&self, input: bool) -> EventQueue<T> {
        let id = self.create_node(
            NodeKind::Events,
            Color::Clean,
            Some(Rc::new(EventsData::<T> {
                queue: RefCell::new(VecDeque::new()),
                listeners: RefCell::new(Vec::new()),
                input,
            })),
        );
        EventQueue {
            id,
            _t: PhantomData,
        }
    }

    /// Queues with events to hand out, each once, in the order they were
    /// first emitted to.
    pub(crate) fn take_pending_events(&self) -> Vec<NodeId> {
        let mut queues = std::mem::take(&mut *self.inner.events_pending.borrow_mut());
        if queues.len() > 1 {
            let mut seen = HashSet::with_capacity(queues.len());
            queues.retain(|q| seen.insert(*q));
        }
        queues
    }

    /// Listeners with events to deliver (handed out, or released from a
    /// freeze), in the order they became ready.
    pub(crate) fn take_ready_listeners(&self) -> Vec<NodeId> {
        std::mem::take(&mut *self.inner.listeners_ready.borrow_mut())
    }

    /// Hand what queue `q` holds to its listeners (the flush calls it as
    /// soon as the emit is seen). `runs` counts pick-ups per queue in this
    /// flush (cycle guard).
    pub(crate) fn distribute_queue(
        &self,
        q: NodeId,
        runs: &mut HashMap<NodeId, u32>,
        errors: &mut Vec<(NodeId, Error)>,
    ) {
        let Ok(data) = self.data(q) else { return };
        if self.cycle_cut(q, runs, errors) {
            // Parked: the events stay queued for the next emit.
            return;
        }
        data.distribute(self, q);
    }

    /// Deliver listener `l`'s inbox (the flush calls it at `l`'s rank).
    pub(crate) fn deliver_listener(&self, l: NodeId, errors: &mut Vec<(NodeId, Error)>) {
        let Ok(data) = self.data(l) else { return };
        data.deliver(self, l, errors);
    }
}

impl<T: 'static> EventQueue<T> {
    /// The node id.
    pub fn id(self) -> NodeId {
        self.id
    }

    /// Queue an event for delivery at the next flush.
    pub fn emit(self, rt: &Runtime, event: T) -> Result<(), Error> {
        rt.with_data::<EventsData<T>, _>(self.id, |d| match d.queue.try_borrow_mut() {
            Ok(mut q) => {
                q.push_back(Rc::new(event));
                Ok(())
            }
            Err(_) => Err(Error::Reentrant),
        })??;
        rt.inner.events_pending.borrow_mut().push(self.id);
        rt.note_write(self.id);
        if rt.inner.flushing.get()
            && let Some(w) = rt.current_writer()
        {
            rt.inner.flush_writes.borrow_mut().push((w, self.id));
        }
        Ok(())
    }

    /// Listen: `f` runs once per event, as a handler owned by the current
    /// owner. Dispose the returned id to stop listening.
    pub fn on<F>(self, rt: &Runtime, f: F) -> Result<NodeId, Error>
    where
        F: FnMut(&Runtime, &T) -> Result<(), Error> + 'static,
    {
        if !rt.exists(self.id) {
            return Err(Error::Disposed(self.id));
        }
        let input = rt.with_data::<EventsData<T>, _>(self.id, |d| d.input)?;
        let l = rt.create_node(
            NodeKind::Listener,
            Color::Clean,
            Some(Rc::new(ListenerData::<T> {
                f: RefCell::new(Box::new(f)),
                queue: self.id,
                input,
                inbox: RefCell::new(VecDeque::new()),
                dropped: Cell::new(0),
            })),
        );
        rt.with_data::<EventsData<T>, _>(self.id, |d| d.listeners.borrow_mut().push(l))?;
        rt.inherit_rank(l, self.id);
        Ok(l)
    }

    /// Events emitted but not yet handed to the listeners (those of this
    /// tick, or of a queue parked as a cycle).
    pub fn queued(self, rt: &Runtime) -> Result<usize, Error> {
        rt.with_data::<EventsData<T>, _>(self.id, |d| d.queue.borrow().len())
    }

    /// Dispose the queue; undelivered events are dropped.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }
}

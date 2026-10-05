//! Lossless event queues (`on notifications.received(n)`).
//!
//! State coalesces to its latest value per tick; events never do. Every
//! event emitted is delivered exactly once, in order, to every listener alive
//! at delivery time, during the flush that follows the emit. Events emitted
//! by a listener are delivered in the same flush.
//!
//! Listeners that re-emit to each other in a loop are a runtime cycle: past
//! [`crate::MAX_RUNS_PER_FLUSH`] deliveries of one queue in one flush, if the
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
//! count. (State needs no bound: a cell keeps only its latest value.)

use std::any::Any;
use std::cell::RefCell;
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
    /// Shared so one event can wait for a frozen listener while it is
    /// delivered to the others, without `T: Clone`.
    queue: RefCell<VecDeque<Rc<T>>>,
    listeners: RefCell<Vec<NodeId>>,
    /// Events kept for suspended listeners of a lossless queue, in order,
    /// at most [`MAX_FROZEN_EVENTS`] each, with the count dropped.
    backlog: RefCell<Vec<Backlog<T>>>,
    /// External input (`on click`, `on scroll`): listeners are input
    /// handlers, not counted by the write-rate guard.
    input: bool,
}

/// What a frozen listener missed.
struct Backlog<T> {
    listener: NodeId,
    events: VecDeque<Rc<T>>,
    /// Oldest events dropped past [`MAX_FROZEN_EVENTS`].
    dropped: usize,
}

struct ListenerData<T> {
    f: RefCell<ListenerFn<T>>,
}

impl<T: 'static> NodeData for ListenerData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<T: 'static> NodeData for EventsData<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn deliver(&self, rt: &Runtime, id: NodeId, errors: &mut Vec<(NodeId, Error)>) -> bool {
        let events: Vec<Rc<T>> = match self.queue.try_borrow_mut() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return false,
        };
        // Released listeners first get what they missed while frozen (it is
        // older than anything queued now).
        let released: Vec<Backlog<T>> = {
            let Ok(mut backlog) = self.backlog.try_borrow_mut() else {
                return false;
            };
            backlog.retain(|b| rt.exists(b.listener));
            let (released, frozen) = std::mem::take(&mut *backlog)
                .into_iter()
                .partition(|b| !rt.is_suspended(b.listener));
            *backlog = frozen;
            released
        };
        if events.is_empty() && released.is_empty() {
            self.note_backlog(rt, id);
            return false;
        }
        // For cycle paths: this queue reaches its listeners.
        rt.inner
            .flush_writes
            .borrow_mut()
            .extend(self.listeners.borrow().iter().map(|&l| (id, l)));
        for b in released {
            if b.dropped > 0 {
                rt.diagnose(Diagnostic::EventsDropped {
                    queue: id,
                    listener: b.listener,
                    dropped: b.dropped,
                });
            }
            for ev in b.events {
                self.run_listener(rt, b.listener, &ev, errors);
            }
        }
        for ev in &events {
            let listeners: Vec<NodeId> = self.listeners.borrow().clone();
            for l in listeners {
                if rt.is_suspended(l) {
                    // A frozen component ignores input; other events wait.
                    if !self.input && rt.exists(l) {
                        self.keep_for(l, ev.clone());
                    }
                    continue;
                }
                self.run_listener(rt, l, ev, errors);
            }
        }
        self.listeners.borrow_mut().retain(|&l| rt.exists(l));
        self.note_backlog(rt, id);
        true
    }
}

impl<T: 'static> EventsData<T> {
    fn run_listener(&self, rt: &Runtime, l: NodeId, ev: &T, errors: &mut Vec<(NodeId, Error)>) {
        let Ok(data) = rt.data(l) else { return };
        let Some(listener) = data.as_any().downcast_ref::<ListenerData<T>>() else {
            return;
        };
        let Ok(mut f) = listener.f.try_borrow_mut() else {
            return;
        };
        // Nodes the listener creates belong to its component; tasks it
        // starts belong to the listener, so disposing it (unmount or a
        // reload that restarts the handler) cancels them.
        let ctx = HandlerCtx {
            writer: l,
            owner: rt.owner_of(l).ok().flatten(),
            site: Some(l),
            input: self.input,
        };
        let r = rt.run_handler(ctx, |rt| f(rt, ev));
        if let Err(e) = r {
            errors.push((l, e));
        }
    }

    /// Keep `ev` for the suspended listener `l`, dropping (and counting) the
    /// oldest past [`MAX_FROZEN_EVENTS`].
    fn keep_for(&self, l: NodeId, ev: Rc<T>) {
        let mut backlog = self.backlog.borrow_mut();
        match backlog.iter_mut().find(|b| b.listener == l) {
            Some(b) => {
                if b.events.len() >= MAX_FROZEN_EVENTS {
                    b.events.pop_front();
                    b.dropped += 1;
                }
                b.events.push_back(ev);
            }
            None => backlog.push(Backlog {
                listener: l,
                events: VecDeque::from([ev]),
                dropped: 0,
            }),
        }
    }

    /// Tell the runtime this queue holds events for frozen listeners, so
    /// releasing them re-delivers.
    fn note_backlog(&self, rt: &Runtime, id: NodeId) {
        if !self.backlog.borrow().is_empty() {
            let mut list = rt.inner.backlogged.borrow_mut();
            if !list.contains(&id) {
                list.push(id);
            }
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
                backlog: RefCell::new(Vec::new()),
                input,
            })),
        );
        EventQueue {
            id,
            _t: PhantomData,
        }
    }

    /// Deliver every queued event. Returns whether anything was delivered.
    /// `runs` counts deliveries per queue in this flush (cycle guard).
    pub(crate) fn deliver_events(
        &self,
        runs: &mut HashMap<NodeId, u32>,
        errors: &mut Vec<(NodeId, Error)>,
    ) -> bool {
        let mut any = false;
        loop {
            let mut queues = std::mem::take(&mut *self.inner.events_pending.borrow_mut());
            if queues.is_empty() {
                return any;
            }
            let mut seen = HashSet::new();
            queues.retain(|q| seen.insert(*q));
            for q in queues {
                let Ok(data) = self.data(q) else { continue };
                if self.cycle_cut(q, runs, errors) {
                    // Parked: the events stay queued for the next emit.
                    continue;
                }
                any |= data.deliver(self, q, errors);
            }
        }
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
        let l = rt.create_node(
            NodeKind::Listener,
            Color::Clean,
            Some(Rc::new(ListenerData::<T> {
                f: RefCell::new(Box::new(f)),
            })),
        );
        rt.with_data::<EventsData<T>, _>(self.id, |d| d.listeners.borrow_mut().push(l))?;
        Ok(l)
    }

    /// Events emitted but not yet delivered.
    pub fn queued(self, rt: &Runtime) -> Result<usize, Error> {
        rt.with_data::<EventsData<T>, _>(self.id, |d| d.queue.borrow().len())
    }

    /// Dispose the queue; undelivered events are dropped.
    pub fn dispose(self, rt: &Runtime) {
        rt.dispose(self.id);
    }
}

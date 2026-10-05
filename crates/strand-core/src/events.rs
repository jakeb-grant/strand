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

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::error::Error;
use crate::runtime::{Color, NodeData, NodeId, NodeKind, Runtime};

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
    queue: RefCell<VecDeque<T>>,
    listeners: RefCell<Vec<NodeId>>,
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
        let events: Vec<T> = match self.queue.try_borrow_mut() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return false,
        };
        if events.is_empty() {
            return false;
        }
        // For cycle paths: this queue reaches its listeners.
        rt.inner
            .flush_writes
            .borrow_mut()
            .extend(self.listeners.borrow().iter().map(|&l| (id, l)));
        for ev in &events {
            let listeners: Vec<NodeId> = self.listeners.borrow().clone();
            for l in listeners {
                let Ok(data) = rt.data(l) else { continue };
                let Some(listener) = data.as_any().downcast_ref::<ListenerData<T>>() else {
                    continue;
                };
                let Ok(mut f) = listener.f.try_borrow_mut() else {
                    continue;
                };
                // Nodes the listener creates belong to its component.
                let owner = rt.owner_of(l).ok().flatten();
                let r = rt.run_handler(l, owner, |rt| f(rt, ev));
                if let Err(e) = r {
                    errors.push((l, e));
                }
            }
        }
        self.listeners.borrow_mut().retain(|&l| rt.exists(l));
        true
    }
}

impl Runtime {
    /// Create an event queue owned by the current owner.
    pub fn events<T: 'static>(&self) -> EventQueue<T> {
        let id = self.create_node(
            NodeKind::Events,
            Color::Clean,
            Some(Rc::new(EventsData::<T> {
                queue: RefCell::new(VecDeque::new()),
                listeners: RefCell::new(Vec::new()),
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
                q.push_back(event);
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

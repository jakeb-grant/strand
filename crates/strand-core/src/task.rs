//! Handlers as cancellable coroutines, executor-agnostic.
//!
//! A handler body is a `Future` owned by a node. The runtime polls it during
//! the flush whenever its waker fires; it never blocks and needs no async
//! runtime. Disposing the owner drops the future, so a handler is cancelled
//! at its next `await` and a [`Diagnostic::Cancelled`] is reported. Errors
//! are values: a handler that returns `Err` lands in [`crate::Tick::errors`].
//!
//! Time inside handlers is the logic clock: [`Runtime::sleep`] completes when
//! [`Runtime::advance_to`] passes its deadline. Wakers are thread-safe, so a
//! future woken from another thread (a service reply) works; set a wake hook
//! with [`Runtime::set_wake_hook`] to wake the logic thread's loop.

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use crate::error::Error;
use crate::runtime::{Color, Diagnostic, NodeData, NodeId, NodeKind, Runtime, WeakRuntime};

type BoxFuture = Pin<Box<dyn Future<Output = Result<(), Error>>>>;
type Hook = Arc<dyn Fn() + Send + Sync>;

/// Woken task ids, shared with wakers on any thread.
#[derive(Default)]
pub(crate) struct ReadyQueue {
    ids: Mutex<Vec<NodeId>>,
    hook: Mutex<Option<Hook>>,
}

impl ReadyQueue {
    fn push(&self, id: NodeId) {
        self.ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(id);
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(h) = hook {
            h();
        }
    }
    fn take(&self) -> Vec<NodeId> {
        std::mem::take(&mut *self.ids.lock().unwrap_or_else(PoisonError::into_inner))
    }
    fn is_empty(&self) -> bool {
        self.ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

struct TaskWaker {
    id: NodeId,
    queue: Arc<ReadyQueue>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.queue.push(self.id);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.queue.push(self.id);
    }
}

struct TaskData {
    fut: RefCell<Option<BoxFuture>>,
    waker: RefCell<Option<Waker>>,
}

impl NodeData for TaskData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn on_dispose(&self, rt: &Runtime, id: NodeId) {
        if self.fut.try_borrow().map(|f| f.is_some()).unwrap_or(false) {
            rt.diagnose(Diagnostic::Cancelled { task: id });
        }
    }
}

/// A running handler coroutine.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Task {
    id: NodeId,
}

impl Task {
    /// The node id.
    pub fn id(self) -> NodeId {
        self.id
    }
    /// True once the handler returned or was cancelled.
    pub fn is_finished(self, rt: &Runtime) -> bool {
        !rt.exists(self.id)
    }
    /// Cancel: the future is dropped at its current `await`.
    pub fn cancel(self, rt: &Runtime) {
        rt.dispose(self.id);
    }
}

impl Runtime {
    /// Start a handler coroutine owned by the current owner. It is first
    /// polled at the next flush.
    pub fn spawn<F>(&self, fut: F) -> Task
    where
        F: Future<Output = Result<(), Error>> + 'static,
    {
        let id = self.create_node(
            NodeKind::Task,
            Color::Clean,
            Some(Rc::new(TaskData {
                fut: RefCell::new(Some(Box::pin(fut))),
                waker: RefCell::new(None),
            })),
        );
        let waker = Waker::from(Arc::new(TaskWaker {
            id,
            queue: self.inner.ready.clone(),
        }));
        let _ = self.with_data::<TaskData, _>(id, |d| *d.waker.borrow_mut() = Some(waker));
        self.inner.ready.push(id);
        Task { id }
    }

    /// Called when a handler is woken from outside a flush (possibly from
    /// another thread), so the host can schedule a flush.
    pub fn set_wake_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self
            .inner
            .ready
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    pub(crate) fn ready_is_empty(&self) -> bool {
        self.inner.ready.is_empty()
    }

    /// Poll every woken task once. Returns whether any was polled.
    pub(crate) fn poll_ready_tasks(&self) -> bool {
        let mut ids = self.inner.ready.take();
        if ids.is_empty() {
            return false;
        }
        ids.sort();
        ids.dedup();
        let mut any = false;
        for id in ids {
            let Ok(data) = self.data(id) else { continue };
            let Some(task) = data.as_any().downcast_ref::<TaskData>() else {
                continue;
            };
            let Some(mut fut) = task.fut.borrow_mut().take() else {
                continue;
            };
            let Some(waker) = task.waker.borrow().clone() else {
                continue;
            };
            any = true;
            let prev_writer = self.inner.writer.replace(Some(id));
            let prev_owner = self.inner.owner.replace(Some(id));
            let mut cx = Context::from_waker(&waker);
            let poll = self.untrack(|_| fut.as_mut().poll(&mut cx));
            self.inner.owner.set(prev_owner);
            self.inner.writer.set(prev_writer);
            match poll {
                Poll::Ready(r) => {
                    if let Err(e) = r {
                        self.record_error(id, e);
                    }
                    self.dispose(id);
                }
                Poll::Pending => {
                    if self.exists(id) {
                        *task.fut.borrow_mut() = Some(fut);
                    } else {
                        // Disposed during its own poll: cancelled here.
                        self.diagnose(Diagnostic::Cancelled { task: id });
                    }
                }
            }
        }
        any
    }

    /// A future that completes once the logic clock reaches `now + d`
    /// (measured from its first poll).
    pub fn sleep(&self, d: Duration) -> Sleep {
        Sleep {
            rt: self.downgrade(),
            dur: d,
            deadline: None,
            registration: None,
        }
    }

    pub(crate) fn wake_sleepers(&self) {
        let now = self.now();
        let due: Vec<Waker> = {
            let mut sleepers = self.inner.sleepers.borrow_mut();
            let (due, keep): (Vec<_>, Vec<_>) =
                sleepers.entries.drain(..).partition(|(d, _, _)| *d <= now);
            sleepers.entries = keep;
            due.into_iter().map(|(_, _, w)| w).collect()
        };
        for w in due {
            w.wake();
        }
    }
}

/// Registered [`Sleep`]s: `(deadline, registration, waker)`.
#[derive(Default)]
pub(crate) struct Sleepers {
    next: u64,
    entries: Vec<(Duration, u64, Waker)>,
}

impl Sleepers {
    pub(crate) fn earliest(&self) -> Option<Duration> {
        self.entries.iter().map(|(d, _, _)| *d).min()
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    fn register(&mut self, id: Option<u64>, deadline: Duration, waker: &Waker) -> u64 {
        if let Some(id) = id
            && let Some(e) = self.entries.iter_mut().find(|e| e.1 == id)
        {
            e.2.clone_from(waker);
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.entries.push((deadline, id, waker.clone()));
        id
    }
    fn remove(&mut self, id: u64) {
        self.entries.retain(|e| e.1 != id);
    }
}

/// Future returned by [`Runtime::sleep`].
pub struct Sleep {
    rt: WeakRuntime,
    dur: Duration,
    deadline: Option<Duration>,
    registration: Option<u64>,
}

impl Drop for Sleep {
    /// A cancelled sleep leaves nothing scheduled (true idle).
    fn drop(&mut self) {
        if let (Some(id), Some(rt)) = (self.registration, self.rt.upgrade())
            && let Ok(mut sleepers) = rt.inner.sleepers.try_borrow_mut()
        {
            sleepers.remove(id);
        }
    }
}

impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep")
            .field("dur", &self.dur)
            .field("deadline", &self.deadline)
            .finish()
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let Some(rt) = self.rt.upgrade() else {
            return Poll::Ready(());
        };
        let now = rt.now();
        let dur = self.dur;
        let deadline = *self.deadline.get_or_insert(now + dur);
        if now >= deadline {
            return Poll::Ready(());
        }
        let Ok(mut sleepers) = rt.inner.sleepers.try_borrow_mut() else {
            // Never reached in practice; poll again next flush.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        self.registration = Some(sleepers.register(self.registration, deadline, cx.waker()));
        Poll::Pending
    }
}

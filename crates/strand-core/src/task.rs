//! Handlers as cancellable coroutines, executor-agnostic.
//!
//! A handler body is a `Future` owned by a node. The runtime polls it during
//! the flush whenever its waker fires; it never blocks and needs no async
//! runtime. Disposing the owner drops the future, so a handler is cancelled
//! at its next `await` and a [`Diagnostic::Cancelled`] is reported. Errors
//! are values: a handler that returns `Err` lands in [`crate::Tick::errors`].
//!
//! A task is polled at most once per flush: one that wakes itself (a
//! `yield_now`) runs again at the next flush, so it can't spin the logic
//! thread. Nodes a handler creates belong to the handler's owner (its
//! component), so a load started by a handler outlives the handler.
//!
//! Time inside handlers is the logic clock: [`Runtime::sleep`] completes when
//! [`Runtime::advance_to`] passes its deadline. Wakers are thread-safe, so a
//! future woken from another thread (a service reply) works; set a wake hook
//! with [`Runtime::set_wake_hook`] to wake the logic thread's loop.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use crate::error::Error;
use crate::runtime::{
    Color, Diagnostic, HandlerCtx, NodeData, NodeId, NodeKind, Runtime, WeakRuntime,
};

type BoxFuture = Pin<Box<dyn Future<Output = Result<(), Error>>>>;
type Hook = Arc<dyn Fn() + Send + Sync>;

/// Woken task ids, shared with wakers on any thread.
///
/// Wakes made on the logic thread (by a handler of the flush, a spawn, a
/// sleeper coming due) and wakes from other threads (an IO or D-Bus reply)
/// are kept apart: a flush polls the foreign ones only when it starts, so
/// a reply arriving while sinks run waits for the next flush instead of
/// writing behind readers that already ran (no sink runs twice per tick).
pub(crate) struct ReadyQueue {
    /// `(wake number, task)`: the two lists merge back in wake order.
    ids: Mutex<Vec<(u64, NodeId)>>,
    /// `ids` is not empty (set and cleared under the lock): lets the flush
    /// check for woken tasks between sinks without locking.
    any: AtomicBool,
    /// Woken from other threads.
    foreign: Mutex<Vec<(u64, NodeId)>>,
    any_foreign: AtomicBool,
    wakes: AtomicU64,
    /// The logic thread (the runtime is not `Send`).
    home: std::thread::ThreadId,
    hook: Mutex<Option<Hook>>,
}

impl Default for ReadyQueue {
    fn default() -> Self {
        Self {
            ids: Mutex::new(Vec::new()),
            any: AtomicBool::new(false),
            foreign: Mutex::new(Vec::new()),
            any_foreign: AtomicBool::new(false),
            wakes: AtomicU64::new(0),
            home: std::thread::current().id(),
            hook: Mutex::new(None),
        }
    }
}

impl ReadyQueue {
    /// Queue `id` without calling the wake hook (the runtime already knows
    /// it is not idle).
    pub(crate) fn push_quiet(&self, id: NodeId) {
        let n = self.wakes.fetch_add(1, AtomicOrdering::Relaxed);
        let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
        ids.push((n, id));
        self.any.store(true, AtomicOrdering::Release);
    }
    fn push(&self, id: NodeId) {
        if std::thread::current().id() == self.home {
            self.push_quiet(id);
        } else {
            let n = self.wakes.fetch_add(1, AtomicOrdering::Relaxed);
            let mut ids = self.foreign.lock().unwrap_or_else(PoisonError::into_inner);
            ids.push((n, id));
            self.any_foreign.store(true, AtomicOrdering::Release);
        }
        self.call_hook();
    }
    pub(crate) fn call_hook(&self) {
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(h) = hook {
            h();
        }
    }
    /// Woken ids: the local ones, and (`foreign`) those woken by other
    /// threads first, in wake order.
    fn take(&self, foreign: bool) -> Vec<NodeId> {
        let mut out = Vec::new();
        if foreign && self.any_foreign.load(AtomicOrdering::Acquire) {
            let mut ids = self.foreign.lock().unwrap_or_else(PoisonError::into_inner);
            self.any_foreign.store(false, AtomicOrdering::Release);
            out = std::mem::take(&mut *ids);
        }
        let merge = !out.is_empty();
        if self.any.load(AtomicOrdering::Acquire) {
            let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
            self.any.store(false, AtomicOrdering::Release);
            if out.is_empty() {
                out = std::mem::take(&mut *ids);
            } else {
                out.append(&mut ids);
            }
        }
        if merge {
            out.sort_by_key(|&(n, _)| n);
        }
        out.into_iter().map(|(_, id)| id).collect()
    }
    fn is_empty(&self) -> bool {
        // The flags are set after a push and cleared with the take, both
        // under the list's lock.
        !self.any.load(AtomicOrdering::Acquire) && !self.any_foreign.load(AtomicOrdering::Acquire)
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
    /// The handler identity its writes count against (the listener, effect
    /// or timer that spawned it, or an explicit handler site), so a handler
    /// that spawns one task per event is still one writer.
    writer: NodeId,
    /// Nodes it creates belong here (the component that was the owner when
    /// it was spawned), so they outlive the task. The task node itself is
    /// owned by its handler's site, so disposing the handler cancels it.
    creation_owner: Option<NodeId>,
    /// Started by external input and not yet suspended: its writes are not
    /// rate-counted. Cleared at its first `await` that suspends, so the
    /// synchronous response to the event is exempt but a loop it then runs
    /// (`loop { x += 1; await sleep(10ms) }`) is counted like any handler.
    input: Cell<bool>,
    /// Superseded on purpose (an `async_memo` re-request): no diagnostic.
    quiet: Cell<bool>,
}

impl NodeData for TaskData {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn on_dispose(&self, rt: &Runtime, id: NodeId) {
        if !self.quiet.get() && self.fut.try_borrow().map(|f| f.is_some()).unwrap_or(false) {
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
    /// Start a handler coroutine. It is first polled at the next flush.
    ///
    /// Inside a listener, timer or `on change` handler the task belongs to
    /// that handler's site: disposing the handler (unmount, or a reload that
    /// restarts changed handler code) cancels it at its current `await`.
    /// Inside an effect it belongs to the effect (cancelled when the effect
    /// re-runs); elsewhere to the current owner. Its writes count against
    /// the handler running now (if any) for the write-rate guard, and it
    /// inherits whether that handler is an input handler (exempt up to its
    /// first suspending `await`). Nodes it creates
    /// belong to the component (the owner at spawn time), so a load started
    /// by a handler outlives the handler's invocation.
    pub fn spawn<F>(&self, fut: F) -> Task
    where
        F: Future<Output = Result<(), Error>> + 'static,
    {
        self.spawn_inner(self.current_writer(), None, self.inner.input.get(), fut)
    }

    /// [`Runtime::spawn`] on behalf of a handler site (from
    /// [`Runtime::handler_site`]): the task is owned by `site`, so disposing
    /// the site cancels it, and its writes count against `site` for the
    /// write-rate guard. The VM uses this when it starts a fresh coroutine
    /// per graph-triggered event (`on notifications.received(n)`), so the
    /// guard sees one handler, not one writer per event.
    pub fn spawn_for<F>(&self, site: NodeId, fut: F) -> Task
    where
        F: Future<Output = Result<(), Error>> + 'static,
    {
        self.spawn_inner(Some(site), Some(site), false, fut)
    }

    /// [`Runtime::spawn`] for a handler run by external input (`on click`,
    /// `on scroll(dy)`, a `<->` write from a widget). Its writes are not
    /// counted by the write-rate guard: input at 60 Hz is the user, not a
    /// feedback loop. The exemption covers the body up to its first `await`
    /// that suspends; after that its writes count against its handler
    /// identity (`site`, or the task) like a graph-triggered handler's, so
    /// a runaway loop started by a click is still throttled.
    /// `site` (from [`Runtime::handler_site`]) owns the task
    /// so a reload can cancel it; `None` means the current owner.
    pub fn spawn_input<F>(&self, site: Option<NodeId>, fut: F) -> Task
    where
        F: Future<Output = Result<(), Error>> + 'static,
    {
        self.spawn_inner(site, site, true, fut)
    }

    fn spawn_inner<F>(
        &self,
        writer: Option<NodeId>,
        site: Option<NodeId>,
        input: bool,
        fut: F,
    ) -> Task
    where
        F: Future<Output = Result<(), Error>> + 'static,
    {
        let creation_owner = self.current_owner();
        let home = site
            .or(self.inner.site.get())
            .filter(|&s| self.exists(s))
            .or(creation_owner);
        let prev = self.inner.owner.replace(home);
        let id = self.create_node(NodeKind::Task, Color::Clean, None);
        self.inner.owner.set(prev);
        let data = Rc::new(TaskData {
            fut: RefCell::new(Some(Box::pin(fut))),
            waker: RefCell::new(None),
            writer: writer.unwrap_or(id),
            creation_owner,
            input: Cell::new(input),
            quiet: Cell::new(false),
        });
        if let Some(n) = self.inner.nodes.borrow_mut().get_mut(id) {
            n.data = Some(data);
        }
        let waker = Waker::from(Arc::new(TaskWaker {
            id,
            queue: self.inner.ready.clone(),
        }));
        let _ = self.with_data::<TaskData, _>(id, |d| *d.waker.borrow_mut() = Some(waker));
        self.inner.ready.push(id);
        Task { id }
    }

    /// Called when a handler is woken (possibly from another thread), and at
    /// the end of a flush that left a woken task for the next one, so a host
    /// loop that sleeps until this hook fires or
    /// [`Runtime::next_deadline`] arrives always comes back to flush.
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

    pub(crate) fn call_wake_hook(&self) {
        self.inner.ready.call_hook();
    }

    /// Cancel `task` without a [`Diagnostic::Cancelled`] (it was superseded
    /// on purpose).
    pub(crate) fn cancel_quietly(&self, task: Task) {
        let _ = self.with_data::<TaskData, _>(task.id, |d| d.quiet.set(true));
        self.dispose(task.id);
    }

    /// Mark `task` so that its cancellation is not reported.
    pub(crate) fn set_quiet(&self, task: Task) {
        let _ = self.with_data::<TaskData, _>(task.id, |d| d.quiet.set(true));
    }

    /// Poll every woken task that has not been polled in this flush yet
    /// (`polled`). A task woken again during the flush (a `yield_now`
    /// pattern) waits for the next flush, so a self-waking task can't spin
    /// the flush. Tasks woken by other threads are polled only when
    /// `foreign` is set (at the start of a flush). Returns whether any task
    /// was polled.
    pub(crate) fn poll_ready_tasks(&self, polled: &mut HashSet<NodeId>, foreign: bool) -> bool {
        let mut ids = self.inner.ready.take(foreign);
        if ids.is_empty() {
            return false;
        }
        // Wake order (first wake wins), never slot order: slots are reused,
        // so a later handler must not run before an earlier one and lose
        // "latest value wins" to it.
        let mut seen = HashSet::with_capacity(ids.len());
        ids.retain(|id| seen.insert(*id));
        let mut any = false;
        for id in ids {
            if !polled.insert(id) {
                self.inner.ready.push_quiet(id);
                continue;
            }
            if self.is_suspended(id) {
                // Frozen with its component: polled again on resume.
                self.hold(id);
                continue;
            }
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
            let ctx = HandlerCtx {
                writer: task.writer,
                owner: task.creation_owner,
                // Tasks it spawns share its site.
                site: self.owner_of(id).ok().flatten(),
                input: task.input.get(),
            };
            let mut cx = Context::from_waker(&waker);
            let prev = self.inner.polling.replace(Some(id));
            let poll = self.run_handler(ctx, |_| fut.as_mut().poll(&mut cx));
            self.inner.polling.set(prev);
            match poll {
                Poll::Ready(r) => {
                    if let Err(e) = r {
                        self.record_error(id, e);
                    }
                    // Finished, not cancelled: its last held write lands.
                    self.detach_deferred(id);
                    self.dispose(id);
                }
                Poll::Pending => {
                    // Past its synchronous response to the input event.
                    task.input.set(false);
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
            let (due, keep): (Vec<_>, Vec<_>) = sleepers
                .entries
                .drain(..)
                .partition(|e| e.counting() && e.deadline <= now);
            sleepers.entries = keep;
            due.into_iter().map(|e| e.waker).collect()
        };
        for w in due {
            w.wake();
        }
    }

    /// Pause the sleeps of tasks inside suspended scopes (keeping the time
    /// left) and mark the released ones; see `sync_frozen_timers`.
    pub(crate) fn sync_frozen_sleepers(&self) -> bool {
        let now = self.now();
        let tasks: Vec<(usize, NodeId)> = self
            .inner
            .sleepers
            .borrow()
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((i, e.task?)))
            .collect();
        let frozen: Vec<(usize, bool)> = tasks
            .into_iter()
            .map(|(i, t)| (i, self.is_suspended(t)))
            .collect();
        let mut released = false;
        let mut sleepers = self.inner.sleepers.borrow_mut();
        for (i, frozen) in frozen {
            let e = &mut sleepers.entries[i];
            match (frozen, e.left) {
                (true, None) => e.left = Some(e.deadline.saturating_sub(now)),
                (false, Some(_)) if !e.resume_pending => {
                    e.resume_pending = true;
                    released = true;
                }
                _ => {}
            }
        }
        released
    }

    /// Released sleepers count again from `at`.
    pub(crate) fn start_released_sleepers(&self, at: Duration) {
        let pending: Vec<(usize, NodeId)> = self
            .inner
            .sleepers
            .borrow()
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.resume_pending)
            .filter_map(|(i, e)| Some((i, e.task?)))
            .collect();
        let still: Vec<(usize, bool)> = pending
            .into_iter()
            .map(|(i, t)| (i, self.is_suspended(t)))
            .collect();
        let mut sleepers = self.inner.sleepers.borrow_mut();
        for (i, frozen) in still {
            let e = &mut sleepers.entries[i];
            if frozen {
                // Frozen again before the clock moved: stays paused.
                e.resume_pending = false;
            } else if let Some(left) = e.left.take() {
                e.resume_pending = false;
                e.deadline = at.checked_add(left).unwrap_or(Duration::MAX);
            }
        }
    }
}

/// A registered [`Sleep`].
struct Sleeper {
    deadline: Duration,
    id: u64,
    waker: Waker,
    /// The task polling it, so a frozen task's sleep pauses.
    task: Option<NodeId>,
    /// Paused (its task is frozen): the time it had left.
    left: Option<Duration>,
    /// Released: counts again from the next clock advance.
    resume_pending: bool,
}

impl Sleeper {
    fn counting(&self) -> bool {
        self.left.is_none()
    }
}

/// Registered [`Sleep`]s.
#[derive(Default)]
pub(crate) struct Sleepers {
    next: u64,
    entries: Vec<Sleeper>,
}

impl Sleepers {
    pub(crate) fn earliest(&self) -> Option<Duration> {
        self.entries
            .iter()
            .filter(|e| e.counting())
            .map(|e| e.deadline)
            .min()
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
    fn get(&self, id: u64) -> Option<&Sleeper> {
        self.entries.iter().find(|e| e.id == id)
    }
    fn register(
        &mut self,
        id: Option<u64>,
        deadline: Duration,
        task: Option<NodeId>,
        waker: &Waker,
    ) -> u64 {
        if let Some(id) = id
            && let Some(e) = self.entries.iter_mut().find(|e| e.id == id)
        {
            e.waker.clone_from(waker);
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.entries.push(Sleeper {
            deadline,
            id,
            waker: waker.clone(),
            task,
            left: None,
            resume_pending: false,
        });
        id
    }
    fn remove(&mut self, id: u64) {
        self.entries.retain(|e| e.id != id);
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
        let Ok(mut sleepers) = rt.inner.sleepers.try_borrow_mut() else {
            // Never reached in practice; poll again next flush.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        if let Some(reg) = self.registration {
            match sleepers.get(reg) {
                // Fired (`wake_sleepers` removed it).
                None => {
                    self.registration = None;
                    return Poll::Ready(());
                }
                // The registry holds the deadline: a frozen task's sleep
                // was paused and moved.
                Some(e) if e.counting() => self.deadline = Some(e.deadline),
                Some(_) => {}
            }
        }
        // A deadline past the end of time never comes: stay pending without
        // scheduling anything.
        let Some(deadline) = self.deadline.or_else(|| now.checked_add(dur)) else {
            return Poll::Pending;
        };
        self.deadline = Some(deadline);
        let paused = self
            .registration
            .and_then(|r| sleepers.get(r))
            .is_some_and(|e| !e.counting());
        if now >= deadline && !paused {
            if let Some(reg) = self.registration.take() {
                sleepers.remove(reg);
            }
            return Poll::Ready(());
        }
        let task = rt.inner.polling.get();
        self.registration = Some(sleepers.register(self.registration, deadline, task, cx.waker()));
        Poll::Pending
    }
}

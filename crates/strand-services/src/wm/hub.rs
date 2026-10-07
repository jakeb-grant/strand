//! One compositor service shared by every store that shows it.
//!
//! `workspaces`, `windows` and `wm` are three services to the language
//! (each with its own reader count and 5 s stop), and `screens.focused`
//! reads the focused screen from the same source. They must share one
//! [`run`]: one adapter connection and one protocol thread. A [`WmHub`]
//! runs it while at least one [`WmSubscription`] lives: the first
//! subscriber starts it, the last one to go stops it (at once: the 5 s
//! grace is each store's own, so the hub only sees a body end once that
//! grace has passed). A subscriber that joins a running hub first gets
//! the current state as one batch (`Reset`s and every field, never a
//! past `config_reloaded`), then the live stream.
//!
//! Each start is a numbered run: a batch from a run that was stopped (its
//! task aborted while the runtime thread was already delivering) is
//! dropped, never applied to the next run's state. Dropping the last
//! [`WmHub`] stops the run even while subscriptions live (they then end
//! with `None`).
//!
//! A subscriber's queue is bounded: one that stops draining (a stalled
//! logic thread, a store within its grace) while the compositor is busy
//! holds at most [`MAX_QUEUED`] batches; past that, its queue is replaced
//! by one batch that rebuilds the current state (the late joiner's replay)
//! plus the `config_reloaded` events it had not yet seen.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use strand_core::keyed::VecDiff;

use super::model::Mirror;
use super::{WmAction, WmChange, WmConfig, WmError, WmRequest, run};

/// The most batches a subscriber's queue holds before it is coalesced.
pub const MAX_QUEUED: usize = 64;

/// The shared compositor service. Cloning shares it.
#[derive(Clone)]
pub struct WmHub {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for WmHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("WmHub")
            .field("subscribers", &inner.subs.len())
            .field("running", &inner.running.is_some())
            .finish()
    }
}

struct Running {
    /// Which start this is: batches from any other are stale.
    generation: u64,
    task: JoinHandle<()>,
    requests: UnboundedSender<WmRequest>,
}

/// One subscriber's queue.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    notify: Notify,
}

#[derive(Default)]
struct QueueState {
    batches: VecDeque<Vec<WmChange>>,
    /// The hub is gone: no batch will come.
    closed: bool,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a batch; when the queue is full, `replay` (the state after
    /// it) replaces everything queued, keeping the events.
    fn push(&self, batch: Vec<WmChange>, replay: impl FnOnce() -> Vec<WmChange>) {
        {
            let mut q = self.lock();
            if q.batches.len() < MAX_QUEUED {
                q.batches.push_back(batch);
            } else {
                let mut one = replay();
                let events: Vec<WmChange> = q
                    .batches
                    .drain(..)
                    .flatten()
                    .chain(batch)
                    .filter(|c| matches!(c, WmChange::ConfigReloaded { .. }))
                    .collect();
                one.extend(events);
                if !one.is_empty() {
                    q.batches.push_back(one);
                }
            }
        }
        self.notify.notify_one();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.notify.notify_one();
    }
}

struct Inner {
    config: WmConfig,
    runtime: Handle,
    next_id: u64,
    subs: Vec<(u64, Arc<Queue>)>,
    /// What a subscriber holds after every batch so far.
    mirror: Mirror,
    /// A state was published (not only `Sources`).
    has_state: bool,
    /// `Sources` was published.
    has_sources: bool,
    running: Option<Running>,
    starts: u64,
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    // A panicking subscriber cannot leave the hub half-updated in a way
    // that matters more than losing the service.
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl WmHub {
    /// A hub that runs `config` on `runtime` (the shared current-thread
    /// runtime) while subscribed.
    pub fn new(config: WmConfig, runtime: Handle) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                config,
                runtime,
                next_id: 0,
                subs: Vec::new(),
                mirror: Mirror::default(),
                has_state: false,
                has_sources: false,
                running: None,
                starts: 0,
            })),
        }
    }

    /// Joins: starts the service if this is the first subscriber, and
    /// queues the current state when it already runs.
    pub fn subscribe(&self) -> WmSubscription {
        let mut inner = lock(&self.inner);
        let queue = Arc::new(Queue::default());
        let replay = inner.replay();
        if !replay.is_empty() {
            queue.push(replay, Vec::new);
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.subs.push((id, queue.clone()));
        if inner.running.is_none() {
            inner.starts += 1;
            let generation = inner.starts;
            let (rtx, rrx) = mpsc::unbounded_channel();
            let weak = Arc::downgrade(&self.inner);
            let sink = move |batch: Vec<WmChange>| fan_out(&weak, generation, batch);
            let task = inner.runtime.spawn(run(inner.config.clone(), sink, rrx));
            inner.running = Some(Running {
                generation,
                task,
                requests: rtx,
            });
        }
        let requests = inner
            .running
            .as_ref()
            .map(|r| r.requests.clone())
            .unwrap_or_else(|| mpsc::unbounded_channel().0);
        WmSubscription {
            hub: Arc::downgrade(&self.inner),
            id,
            queue,
            requests,
        }
    }

    /// The service is running.
    pub fn running(&self) -> bool {
        lock(&self.inner).running.is_some()
    }

    /// How many times the service was started.
    pub fn starts(&self) -> u64 {
        lock(&self.inner).starts
    }

    /// The live subscriptions.
    pub fn subscribers(&self) -> usize {
        lock(&self.inner).subs.len()
    }

    #[cfg(test)]
    fn sink_for(&self, generation: u64) -> impl FnMut(Vec<WmChange>) + use<> {
        let weak = Arc::downgrade(&self.inner);
        move |batch| fan_out(&weak, generation, batch)
    }
}

fn fan_out(hub: &Weak<Mutex<Inner>>, generation: u64, batch: Vec<WmChange>) {
    let Some(inner) = hub.upgrade() else {
        return;
    };
    let mut inner = lock(&inner);
    if inner.running.as_ref().map(|r| r.generation) != Some(generation) {
        // A stopped run's last batch: its diffs are against a state the
        // hub (and every current subscriber) no longer holds.
        return;
    }
    for change in &batch {
        if let Err(e) = inner.mirror.apply(change) {
            log::warn!("compositor service: inconsistent change {change:?}: {e}");
        }
        match change {
            WmChange::Workspaces(_) => inner.has_state = true,
            WmChange::Sources(_) => inner.has_sources = true,
            _ => {}
        }
    }
    // Events are not state: a late subscriber never sees a past reload.
    inner.mirror.reloads.clear();
    let inner = &*inner;
    for (_, q) in &inner.subs {
        q.push(batch.clone(), || inner.replay());
    }
}

impl Inner {
    /// The current state as one batch (empty before anything was
    /// published).
    fn replay(&self) -> Vec<WmChange> {
        let mut out = Vec::new();
        if self.has_state {
            let m = &self.mirror;
            out.push(WmChange::Name(m.name.clone()));
            out.push(WmChange::Workspaces(vec![VecDiff::Reset {
                items: m.workspaces.clone(),
            }]));
            out.push(WmChange::Windows(vec![VecDiff::Reset {
                items: m.windows.clone(),
            }]));
            out.push(WmChange::FocusedWorkspace(m.focused_workspace.clone()));
            out.push(WmChange::FocusedWindow(m.focused_window.clone()));
            out.push(WmChange::FocusedScreen(m.focused_screen.clone()));
        }
        if self.has_sources {
            out.push(WmChange::Sources(self.mirror.sources.clone()));
        }
        out
    }

    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.task.abort();
        }
        self.mirror = Mirror::default();
        self.has_state = false;
        self.has_sources = false;
    }
}

impl Drop for Inner {
    /// The last [`WmHub`] is gone: stop the run (a detached task would keep
    /// its sockets and protocol thread until the runtime ends) and end
    /// every subscription's stream.
    fn drop(&mut self) {
        self.stop();
        for (_, q) in &self.subs {
            q.close();
        }
    }
}

/// One reader of the shared service; dropping it leaves (and stops the
/// service when it was the last).
pub struct WmSubscription {
    hub: Weak<Mutex<Inner>>,
    id: u64,
    queue: Arc<Queue>,
    requests: UnboundedSender<WmRequest>,
}

impl std::fmt::Debug for WmSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WmSubscription")
            .field("id", &self.id)
            .finish()
    }
}

impl WmSubscription {
    /// The next batch (`None` only once the hub was dropped and every
    /// queued batch taken). Cancel safe.
    pub async fn recv(&mut self) -> Option<Vec<WmChange>> {
        loop {
            {
                let mut q = self.queue.lock();
                if let Some(b) = q.batches.pop_front() {
                    return Some(b);
                }
                if q.closed {
                    return None;
                }
            }
            // `notify_one` keeps a permit when nobody waits: no lost wakeup
            // between the check above and this wait.
            self.queue.notify.notified().await;
        }
    }

    /// The next batch if one is waiting.
    pub fn try_recv(&mut self) -> Option<Vec<WmChange>> {
        self.queue.lock().batches.pop_front()
    }

    /// Batches waiting.
    pub fn queued(&self) -> usize {
        self.queue.lock().batches.len()
    }

    /// Runs an action; the receiver gets its outcome (`NotConnected` if
    /// the service has stopped).
    pub fn request(&self, action: WmAction) -> oneshot::Receiver<Result<(), WmError>> {
        let (req, rx) = WmRequest::new(action);
        if let Err(mpsc::error::SendError(req)) = self.requests.send(req)
            && let Some(reply) = req.reply
        {
            let _ = reply.send(Err(WmError::NotConnected));
        }
        rx
    }
}

impl Drop for WmSubscription {
    fn drop(&mut self) {
        let Some(inner) = self.hub.upgrade() else {
            return;
        };
        let mut inner = lock(&inner);
        inner.subs.retain(|(id, _)| *id != self.id);
        if inner.subs.is_empty() {
            inner.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::model::{Publisher, WmState, Workspace};

    fn state(focus: i64) -> WmState {
        WmState {
            name: "sway".into(),
            workspaces: (1..=3)
                .map(|id| Workspace {
                    id,
                    name: id.to_string(),
                    focused: id == focus,
                    active: id == focus,
                    screen: "DP-1".into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn drain(sub: &mut WmSubscription, m: &mut Mirror) -> usize {
        let mut n = 0;
        while let Some(b) = sub.try_recv() {
            n += 1;
            for c in &b {
                m.apply(c).unwrap_or_else(|e| panic!("{c:?}: {e}"));
            }
        }
        n
    }

    /// The test runtime is current-thread and these tests never await, so
    /// the spawned run never runs: every batch here comes from the test.
    #[tokio::test]
    async fn a_stopped_runs_batch_never_reaches_the_next_run() {
        let hub = WmHub::new(WmConfig::default(), Handle::current());
        let mut a = hub.subscribe();
        let mut publisher = Publisher::new();
        let mut first = hub.sink_for(1);
        first(publisher.publish(state(1)));
        let mut ma = Mirror::default();
        drain(&mut a, &mut ma);
        assert_eq!(ma.workspaces.len(), 3);

        // Stop and restart; the first run's next batch (a diff against its
        // own state) arrives late.
        drop(a);
        let mut b = hub.subscribe();
        assert_eq!(hub.starts(), 2);
        first(publisher.publish(state(2)));
        assert!(b.try_recv().is_none(), "a stale run's batch was delivered");
        assert_eq!(hub.subscribe().queued(), 0, "nor kept for replay");

        // The current run's batches still go through.
        let mut second = hub.sink_for(2);
        second(Publisher::new().publish(state(3)));
        let mut mb = Mirror::default();
        assert_eq!(drain(&mut b, &mut mb), 1);
        assert_eq!(mb.focused_workspace.unwrap().id, 3);
    }

    /// A subscriber that stops draining holds a bounded queue that still
    /// rebuilds the state, and keeps every reload event.
    #[tokio::test]
    async fn a_lagging_subscribers_queue_is_bounded_and_coalesced() {
        let hub = WmHub::new(WmConfig::default(), Handle::current());
        let mut lagging = hub.subscribe();
        let mut keeping_up = hub.subscribe();
        let mut sink = hub.sink_for(1);
        let mut publisher = Publisher::new();
        let mut reference = Mirror::default();
        let mut send = |batch: Vec<WmChange>, keeping_up: &mut WmSubscription| {
            sink(batch);
            drain(keeping_up, &mut reference);
        };
        send(publisher.publish(state(1)), &mut keeping_up);
        for i in 0..(3 * MAX_QUEUED) {
            send(
                publisher.publish(state(1 + (i as i64 % 3))),
                &mut keeping_up,
            );
            if i % 50 == 0 {
                send(
                    vec![WmChange::ConfigReloaded {
                        failed: Some(i == 100),
                    }],
                    &mut keeping_up,
                );
            }
        }
        assert!(lagging.queued() <= MAX_QUEUED, "{}", lagging.queued());
        let mut m = Mirror::default();
        drain(&mut lagging, &mut m);
        assert_eq!(m, reference, "the coalesced stream rebuilds the state");
        assert_eq!(m.reloads.len(), 4, "no reload is lost");
        assert_eq!(m.reloads[2], Some(true));
    }

    /// Dropping the last hub ends its subscriptions' streams.
    #[tokio::test]
    async fn dropping_the_hub_ends_the_streams() {
        let hub = WmHub::new(WmConfig::default(), Handle::current());
        let mut a = hub.subscribe();
        drop(hub);
        assert_eq!(a.recv().await, None);
    }
}

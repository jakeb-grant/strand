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

use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use strand_core::keyed::VecDiff;

use super::model::Mirror;
use super::{WmAction, WmChange, WmConfig, WmError, WmRequest, run};

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
    task: JoinHandle<()>,
    requests: UnboundedSender<WmRequest>,
}

struct Inner {
    config: WmConfig,
    runtime: Handle,
    next_id: u64,
    subs: Vec<(u64, UnboundedSender<Vec<WmChange>>)>,
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
        let (tx, rx) = mpsc::unbounded_channel();
        let replay = inner.replay();
        if !replay.is_empty() {
            let _ = tx.send(replay);
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.subs.push((id, tx));
        if inner.running.is_none() {
            let (rtx, rrx) = mpsc::unbounded_channel();
            let weak = Arc::downgrade(&self.inner);
            let sink = move |batch: Vec<WmChange>| fan_out(&weak, batch);
            let task = inner.runtime.spawn(run(inner.config.clone(), sink, rrx));
            inner.running = Some(Running {
                task,
                requests: rtx,
            });
            inner.starts += 1;
        }
        let requests = inner
            .running
            .as_ref()
            .map(|r| r.requests.clone())
            .unwrap_or_else(|| mpsc::unbounded_channel().0);
        WmSubscription {
            hub: Arc::downgrade(&self.inner),
            id,
            rx,
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
}

fn fan_out(hub: &Weak<Mutex<Inner>>, batch: Vec<WmChange>) {
    let Some(inner) = hub.upgrade() else {
        return;
    };
    let mut inner = lock(&inner);
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
    inner.subs.retain(|(_, tx)| tx.send(batch.clone()).is_ok());
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

/// One reader of the shared service; dropping it leaves (and stops the
/// service when it was the last).
pub struct WmSubscription {
    hub: Weak<Mutex<Inner>>,
    id: u64,
    rx: UnboundedReceiver<Vec<WmChange>>,
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
    /// The next batch (`None` only if the hub was dropped).
    pub async fn recv(&mut self) -> Option<Vec<WmChange>> {
        self.rx.recv().await
    }

    /// The next batch if one is waiting.
    pub fn try_recv(&mut self) -> Option<Vec<WmChange>> {
        self.rx.try_recv().ok()
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

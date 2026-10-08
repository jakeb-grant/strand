//! The text worker thread: requests in, layouts out, over channels.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use strand_scene::Scale;

use crate::engine::{FontConfig, TextEngine};
use crate::{TextKey, TextLayout, TextRequest};

/// What every text worker runs when its queue drains after work, on
/// its own thread, before it blocks for the next request (see
/// [`set_idle_hook`]).
static IDLE_HOOK: OnceLock<fn()> = OnceLock::new();

/// Installs the hook text workers run when their queue drains after
/// work (the first call wins). `strand run` returns the allocator's
/// freed pages there: a worker that goes quiet after shaping keeps them
/// otherwise, and only its own thread can free them. It runs inside the
/// burst that woke the worker, so it costs no wakeup of its own, once
/// the channel is empty too (requests that arrived while one was being
/// shaped belong to the same burst), and at most once per 5 s per
/// worker plus the drains of that burst's first 250 ms (a stream of
/// keystrokes pays one, not one each), as [`HookGate`] rules. Work
/// whose drain was skipped is answered once the worker has been quiet
/// for 500 ms (one wake, at most one such per 5 s), else at the next
/// drain allowed.
pub fn set_idle_hook(hook: fn()) {
    let _ = IDLE_HOOK.set(hook);
}

/// How often a worker may run the idle hook at a drain (as `strand
/// run`'s inline trim on the main and logic threads), and how often it
/// may wake for an owed one ([`HOOK_QUIET`]).
const HOOK_EVERY: Duration = Duration::from_secs(5);

/// After a drain that ran the idle hook, how long further drains run it
/// too: a minute tick's requests for two outputs can arrive a few
/// milliseconds apart, and what the later one freed stays resident
/// otherwise.
const HOOK_TAIL: Duration = Duration::from_millis(250);

/// How long a worker whose drain skipped the hook stays quiet before it
/// wakes to run it (as `strand run`'s delayed trim, 500 ms after a
/// structural burst): soon enough that the wake belongs to the burst's
/// settling (a shell is idle once a whole second passes without one),
/// late enough that a stream of keystrokes pays it once, at its end.
const HOOK_QUIET: Duration = Duration::from_millis(500);

/// When a worker thread runs its idle hook (the text worker's, and
/// `strand_render`'s image worker's): on a drain after work, at most
/// once per 5 s plus the drains of that burst's first 250 ms. A drain
/// it skips leaves the hook owed: [`HookGate::drained`] then tells the
/// worker to block at most 500 ms for its next request, and if none
/// comes [`HookGate::quiet`] runs the hook. That wake comes only after
/// real work and at most once per 5 s (a 1 s poll's text pays one per
/// five polls, not one each), so an idle worker never wakes; what a
/// drain skipped past it waits for the next drain allowed.
#[derive(Debug, Default)]
pub struct HookGate {
    /// When a drain last began a hook burst.
    last: Option<Instant>,
    /// Until when drains run the hook again.
    tail: Option<Instant>,
    /// When the worker last woke for an owed hook.
    woke: Option<Instant>,
    /// Whether work was done since the hook last ran.
    owed: bool,
}

impl HookGate {
    /// Work was done: its garbage is owed a hook.
    pub fn worked(&mut self) {
        self.owed = true;
    }

    /// The worker's queue drained at `now`: runs `hook` if it is owed
    /// and allowed. Returns how long the worker may block for its next
    /// request before it calls [`HookGate::quiet`], or `None` to block
    /// until one comes.
    pub fn drained(&mut self, now: Instant, hook: impl FnOnce()) -> Option<Duration> {
        if !self.owed {
            return None;
        }
        if self.due(now) {
            self.owed = false;
            hook();
            return None;
        }
        self.woke
            .is_none_or(|w| now.saturating_duration_since(w) >= HOOK_EVERY)
            .then_some(HOOK_QUIET)
    }

    /// The wait [`HookGate::drained`] returned ran out with no request
    /// at `now`: runs the owed hook.
    pub fn quiet(&mut self, now: Instant, hook: impl FnOnce()) {
        if self.owed {
            self.owed = false;
            self.woke = Some(now);
            hook();
        }
    }

    /// A drain after work at `now`: whether it runs the hook.
    fn due(&mut self, now: Instant) -> bool {
        if self.tail.is_some_and(|t| now < t) {
            return true;
        }
        let due = self
            .last
            .is_none_or(|l| now.saturating_duration_since(l) >= HOOK_EVERY);
        if due {
            self.last = Some(now);
            self.tail = Some(now + HOOK_TAIL);
        }
        due
    }
}

/// Errors talking to the text worker.
#[derive(Debug)]
pub enum TextError {
    /// The worker thread could not be started.
    Spawn(std::io::Error),
    /// The worker thread has exited (it panicked or was shut down).
    WorkerGone,
}

impl std::fmt::Display for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "cannot start the text worker: {e}"),
            Self::WorkerGone => f.write_str("the text worker has exited"),
        }
    }
}

impl std::error::Error for TextError {}

enum Msg {
    Layout(Box<TextRequest>),
    /// The requester no longer wants this key: skip it if still queued.
    Cancel(TextKey),
    /// No output uses this scale any more: free its atlas.
    DropScale(Scale),
    /// The installed fonts changed: a fresh engine, and a reset layout
    /// (key 0) telling the receiver every layout and page is stale.
    ReloadFonts,
}

/// Called on the worker thread after each layout is sent, so the render
/// loop can wake (for example a calloop ping).
pub type Waker = Box<dyn Fn() + Send + 'static>;

/// Handle to the text worker. Dropping it stops and joins the thread
/// after at most the request being shaped (requests are capped at
/// [`crate::MAX_TEXT_BYTES`]); queued requests are discarded.
pub struct TextWorker {
    requests: Option<Sender<Msg>>,
    layouts: Receiver<TextLayout>,
    thread: Option<JoinHandle<()>>,
    /// The waker, shared with other workers of the render loop (images).
    waker: Option<Arc<Mutex<Waker>>>,
}

impl std::fmt::Debug for TextWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextWorker").finish_non_exhaustive()
    }
}

fn enqueue(m: Msg, queue: &mut VecDeque<Msg>, cancelled: &mut HashSet<TextKey>) {
    match m {
        Msg::Cancel(k) => {
            cancelled.insert(k);
        }
        m => queue.push_back(m),
    }
}

impl TextWorker {
    /// Starts the worker. Font discovery runs on the worker thread, so this
    /// returns immediately.
    pub fn spawn(config: FontConfig) -> Result<Self, TextError> {
        Self::spawn_with_waker(config, None)
    }

    pub fn spawn_with_waker(config: FontConfig, waker: Option<Waker>) -> Result<Self, TextError> {
        let shared = waker.map(|w| Arc::new(Mutex::new(w)));
        let waker = shared.clone();
        let (req_tx, req_rx) = mpsc::channel::<Msg>();
        let (out_tx, out_rx) = mpsc::channel::<TextLayout>();
        let thread = std::thread::Builder::new()
            .name("strand-text".into())
            .spawn(move || {
                let mut engine = TextEngine::new(config.clone());
                let mut queue: VecDeque<Msg> = VecDeque::new();
                // Keys cancelled while their request is still queued.
                let mut cancelled: HashSet<TextKey> = HashSet::new();
                let mut gate = HookGate::default();
                loop {
                    if queue.is_empty() {
                        // Whatever arrived while the last request was
                        // being shaped is the same burst: drain it before
                        // calling the worker idle.
                        loop {
                            match req_rx.try_recv() {
                                Ok(m) => enqueue(m, &mut queue, &mut cancelled),
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => return,
                            }
                        }
                    }
                    if queue.is_empty() {
                        // A cancel always follows its request on the
                        // channel, so with nothing queued every
                        // remembered cancel is for a finished request.
                        cancelled.clear();
                        let wait = IDLE_HOOK
                            .get()
                            .and_then(|hook| gate.drained(Instant::now(), hook));
                        let next = match wait {
                            None => req_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                            Some(d) => req_rx.recv_timeout(d),
                        };
                        match next {
                            Ok(m) => enqueue(m, &mut queue, &mut cancelled),
                            // Quiet: the owed hook, then block.
                            Err(RecvTimeoutError::Timeout) => {
                                if let Some(hook) = IDLE_HOOK.get() {
                                    gate.quiet(Instant::now(), hook);
                                }
                                continue;
                            }
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    // Fold in whatever arrived meanwhile before each
                    // request, so a cancel sent while a long batch is
                    // being shaped still skips the superseded ones.
                    // A closed channel means the handle was dropped: stop
                    // without shaping the rest of the queue.
                    loop {
                        match req_rx.try_recv() {
                            Ok(m) => enqueue(m, &mut queue, &mut cancelled),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                    let Some(msg) = queue.pop_front() else {
                        continue;
                    };
                    gate.worked();
                    let req = match msg {
                        Msg::Layout(req) if !cancelled.remove(&req.key) => req,
                        Msg::DropScale(s) => {
                            engine.drop_scale(s);
                            continue;
                        }
                        Msg::ReloadFonts => {
                            engine.reload_fonts();
                            if out_tx
                                .send(TextLayout::reset(TextKey(0), Scale::ONE))
                                .is_err()
                            {
                                return;
                            }
                            if let Some(w) = &waker
                                && let Ok(w) = w.lock()
                            {
                                w();
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    let layout = match catch_unwind(AssertUnwindSafe(|| engine.layout(&req))) {
                        Ok(l) => l,
                        Err(_) => {
                            // A bug in shaping must not end text for the
                            // session. The engine's atlas may now be out of
                            // step with what was uploaded, so start a fresh
                            // one and tell the receiver to forget every page
                            // it mirrors (see `TextLayout::is_reset`).
                            engine = TextEngine::new(config.clone());
                            TextLayout::reset(req.key, req.scale)
                        }
                    };
                    if out_tx.send(layout).is_err() {
                        return;
                    }
                    if let Some(w) = &waker
                        && let Ok(w) = w.lock()
                    {
                        w();
                    }
                }
            })
            .map_err(TextError::Spawn)?;
        Ok(Self {
            requests: Some(req_tx),
            layouts: out_rx,
            thread: Some(thread),
            waker: shared,
        })
    }

    /// A waker that runs the one this worker was spawned with (the render
    /// loop's ping), for other workers of the same loop: the image
    /// decoder wakes it the same way.
    pub fn waker(&self) -> Option<Waker> {
        let w = self.waker.clone()?;
        Some(Box::new(move || {
            if let Ok(w) = w.lock() {
                w();
            }
        }))
    }

    /// Queues a request. Never blocks.
    pub fn request(&self, req: TextRequest) -> Result<(), TextError> {
        self.send(Msg::Layout(Box::new(req)))
    }

    /// Withdraws a queued request: if the worker has not started it, it is
    /// skipped and no layout is sent for `key`. Never blocks.
    pub fn cancel(&self, key: TextKey) -> Result<(), TextError> {
        self.send(Msg::Cancel(key))
    }

    /// Frees the atlas for `scale`. Requests sent afterwards rasterise and
    /// upload their glyphs again. Never blocks.
    pub fn drop_scale(&self, scale: Scale) -> Result<(), TextError> {
        self.send(Msg::DropScale(scale))
    }

    /// The installed fonts changed (a fontconfig directory): the worker
    /// looks them up afresh and answers with a reset layout (key 0,
    /// [`TextLayout::is_reset`]), after which every layout must be asked
    /// for again. Never blocks.
    pub fn reload_fonts(&self) -> Result<(), TextError> {
        self.send(Msg::ReloadFonts)
    }

    fn send(&self, msg: Msg) -> Result<(), TextError> {
        self.requests
            .as_ref()
            .ok_or(TextError::WorkerGone)?
            .send(msg)
            .map_err(|_| TextError::WorkerGone)
    }

    /// A finished layout, if one is ready. Never blocks.
    pub fn try_recv(&self) -> Result<Option<TextLayout>, TextError> {
        match self.layouts.try_recv() {
            Ok(l) => Ok(Some(l)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(TextError::WorkerGone),
        }
    }

    /// The worker thread is still running. It runs until the handle is
    /// dropped (a shaping panic is caught and the engine started again),
    /// so `false` with the handle held means the thread itself panicked.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Waits up to `timeout` for a layout (tests, offline rendering).
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<TextLayout>, TextError> {
        match self.layouts.recv_timeout(timeout) {
            Ok(l) => Ok(Some(l)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(TextError::WorkerGone),
        }
    }
}

impl Drop for TextWorker {
    fn drop(&mut self) {
        // Closing the request channel ends the worker loop.
        self.requests.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A stream of drains (keystrokes 100 ms apart for 20 s) runs the
    /// hook at most once per [`HOOK_EVERY`] plus its [`HOOK_TAIL`]: the
    /// drains at 0, 100 and 200 ms (inside the tail), then nothing until
    /// five seconds have passed.
    #[test]
    fn the_hook_runs_at_most_once_per_period_and_its_tail() {
        let t0 = Instant::now();
        let mut g = HookGate::default();
        let ran: Vec<u64> = (0..200)
            .map(|i| i * 100)
            .filter(|&t| g.due(t0 + ms(t)))
            .collect();
        assert_eq!(
            ran,
            [
                0, 100, 200, 5000, 5100, 5200, 10000, 10100, 10200, 15000, 15100, 15200
            ]
        );
    }

    /// A drain past the tail waits out the period; the first drain after
    /// it runs the hook again and starts a new tail.
    #[test]
    fn a_drain_after_the_period_starts_a_new_tail() {
        let t0 = Instant::now();
        let mut g = HookGate::default();
        assert!(g.due(t0), "the first drain");
        assert!(g.due(t0 + ms(249)), "inside the tail");
        assert!(!g.due(t0 + HOOK_TAIL), "past the tail");
        assert!(!g.due(t0 + ms(4999)));
        assert!(g.due(t0 + HOOK_EVERY));
        assert!(g.due(t0 + HOOK_EVERY + ms(10)), "the new tail");
    }

    /// A drain that did no work blocks without a deadline; one whose
    /// hook was skipped blocks [`HOOK_QUIET`] at most, and if no request
    /// comes the hook runs then. Such a wake comes at most once per
    /// [`HOOK_EVERY`]: a skip after it waits for the next drain allowed.
    #[test]
    fn a_skipped_drain_owes_the_hook_until_the_worker_is_quiet() {
        let t0 = Instant::now();
        let mut g = HookGate::default();
        let mut ran = 0;
        assert_eq!(g.drained(t0, || ran += 1), None, "no work, no hook");
        assert_eq!(ran, 0);
        g.worked();
        assert_eq!(g.drained(t0, || ran += 1), None, "the first drain");
        assert_eq!(ran, 1);
        assert_eq!(g.drained(t0 + ms(10), || ran += 1), None, "nothing owed");
        // Keystrokes past the tail: each drain skips, and waits.
        for k in 0..5 {
            g.worked();
            let at = t0 + ms(1000 + 150 * k);
            assert_eq!(g.drained(at, || ran += 1), Some(HOOK_QUIET));
        }
        assert_eq!(ran, 1);
        g.quiet(t0 + ms(2100), || ran += 1);
        assert_eq!(ran, 2, "the owed hook ran once the worker was quiet");
        g.quiet(t0 + ms(2600), || ran += 1);
        assert_eq!(ran, 2, "nothing owed");
        // Within five seconds of that wake: no second one.
        g.worked();
        assert_eq!(g.drained(t0 + ms(3000), || ran += 1), None);
        assert_eq!(ran, 2);
        // The next drain allowed runs it.
        g.worked();
        assert_eq!(g.drained(t0 + HOOK_EVERY, || ran += 1), None);
        assert_eq!(ran, 3);
        // And a skip five seconds after the wake may wake again.
        g.worked();
        assert_eq!(g.drained(t0 + ms(7200), || ran += 1), Some(HOOK_QUIET));
    }
}

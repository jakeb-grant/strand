//! The text worker thread: requests in, layouts out, over channels.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

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
/// burst that woke the worker, so it costs no wakeup of its own.
pub fn set_idle_hook(hook: fn()) {
    let _ = IDLE_HOOK.set(hook);
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
                // Whether work was done since the idle hook last ran.
                let mut worked = false;
                loop {
                    if queue.is_empty() {
                        // A cancel always follows its request on the
                        // channel, so with nothing queued every
                        // remembered cancel is for a finished request.
                        cancelled.clear();
                        if std::mem::take(&mut worked)
                            && let Some(hook) = IDLE_HOOK.get()
                        {
                            hook();
                        }
                        match req_rx.recv() {
                            Ok(m) => enqueue(m, &mut queue, &mut cancelled),
                            Err(_) => return,
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
                    worked = true;
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

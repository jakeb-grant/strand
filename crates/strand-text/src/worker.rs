//! The text worker thread: requests in, layouts out, over channels.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use strand_scene::Scale;

use crate::engine::{FontConfig, TextEngine};
use crate::{TextKey, TextLayout, TextRequest};

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
        let (req_tx, req_rx) = mpsc::channel::<Msg>();
        let (out_tx, out_rx) = mpsc::channel::<TextLayout>();
        let thread = std::thread::Builder::new()
            .name("strand-text".into())
            .spawn(move || {
                let mut engine = TextEngine::new(config.clone());
                let mut queue: VecDeque<Msg> = VecDeque::new();
                // Keys cancelled while their request is still queued.
                let mut cancelled: HashSet<TextKey> = HashSet::new();
                loop {
                    if queue.is_empty() {
                        // A cancel always follows its request on the
                        // channel, so with nothing queued every
                        // remembered cancel is for a finished request.
                        cancelled.clear();
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
                    let req = match msg {
                        Msg::Layout(req) if !cancelled.remove(&req.key) => req,
                        Msg::DropScale(s) => {
                            engine.drop_scale(s);
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
                    if let Some(w) = &waker {
                        w();
                    }
                }
            })
            .map_err(TextError::Spawn)?;
        Ok(Self {
            requests: Some(req_tx),
            layouts: out_rx,
            thread: Some(thread),
        })
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

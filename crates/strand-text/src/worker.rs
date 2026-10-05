//! The text worker thread: requests in, layouts out, over channels.

use std::collections::HashSet;
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

/// Handle to the text worker. Dropping it stops and joins the thread.
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
                while let Ok(first) = req_rx.recv() {
                    // Drain what is queued so cancelled requests are skipped
                    // instead of shaped (text that changes every frame).
                    let mut batch = vec![first];
                    batch.extend(req_rx.try_iter());
                    let cancelled: HashSet<TextKey> = batch
                        .iter()
                        .filter_map(|m| match m {
                            Msg::Cancel(k) => Some(*k),
                            _ => None,
                        })
                        .collect();
                    for msg in batch {
                        let req = match msg {
                            Msg::Layout(req) if !cancelled.contains(&req.key) => req,
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
                                // session. The engine's atlas may now be out
                                // of step with what was uploaded, so start a
                                // fresh one (page generations never repeat).
                                engine = TextEngine::new(config.clone());
                                TextLayout::empty(req.key, req.scale)
                            }
                        };
                        if out_tx.send(layout).is_err() {
                            return;
                        }
                        if let Some(w) = &waker {
                            w();
                        }
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

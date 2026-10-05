//! The text worker thread: requests in, layouts out, over channels.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::engine::{FontConfig, TextEngine};
use crate::{TextLayout, TextRequest};

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
                let mut engine = TextEngine::new(config);
                while let Ok(Msg::Layout(req)) = req_rx.recv() {
                    let layout = engine.layout(&req);
                    if out_tx.send(layout).is_err() {
                        break;
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
        self.requests
            .as_ref()
            .ok_or(TextError::WorkerGone)?
            .send(Msg::Layout(Box::new(req)))
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

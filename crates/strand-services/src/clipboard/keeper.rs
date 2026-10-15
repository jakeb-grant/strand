//! The process-wide selection keeper (decisions.md m6-service-gaps): a
//! thread of its own (`strand-clipboard-keep`) with its own Wayland
//! connection, owning the data-control source of every copy the
//! `clipboard` service makes (`clipboard.copy(value)`, `e.copy()`), so a
//! copied value outlives the service's run, which stops 5 s after its
//! last reader. It writes a copy to each program that pastes it without
//! blocking (5 s without progress ends a write). A newer copy replaces
//! the source; when another program takes the selection (`cancelled`) and
//! no write is left, the keeper closes its connection and its thread
//! ends; the next copy starts another. A copy to another display (tests)
//! replaces the keeper.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFlags};

use super::protocol::{self, Ev, NO_DATA_CONTROL, Source};

/// How long a write may make no progress before it is dropped.
pub const WRITE_STALL: Duration = Duration::from_secs(5);

/// How long [`copy`] waits for the keeper to take a copy.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// One copy: each type offered and the bytes written for it.
pub type Offers = Vec<(String, Arc<[u8]>)>;

/// The copy held, by type.
type Held = HashMap<String, Arc<[u8]>>;

struct Ask {
    offers: Offers,
    answer: mpsc::Sender<Result<(), String>>,
}

struct Handle {
    socket: PathBuf,
    asks: mpsc::Sender<Ask>,
    wake: Arc<OwnedFd>,
    /// Cleared (under [`KEEPER`]'s lock) once the thread takes no more
    /// asks.
    alive: Arc<AtomicBool>,
    /// Set to end a keeper another display's replaces.
    stop: Arc<AtomicBool>,
}

static KEEPER: Mutex<Option<Handle>> = Mutex::new(None);

fn poke(fd: &OwnedFd) {
    let _ = rustix::io::write(fd, &1u64.to_ne_bytes());
}

/// Makes `offers` the selection of the display at `socket`, held by the
/// keeper (started if none runs for that display). `Err`: no data
/// control there, or the display cannot be reached.
pub fn copy(socket: &Path, offers: Offers) -> Result<(), String> {
    let (answer, answered) = mpsc::channel();
    {
        let mut keeper = KEEPER
            .lock()
            .map_err(|_| "the clipboard keeper failed".to_string())?;
        let running = keeper
            .as_ref()
            .filter(|h| h.socket == socket && h.alive.load(Ordering::SeqCst));
        let ask = Ask { offers, answer };
        let ask = match running {
            Some(h) => match h.asks.send(ask) {
                Ok(()) => {
                    poke(&h.wake);
                    None
                }
                Err(mpsc::SendError(ask)) => Some(ask),
            },
            None => Some(ask),
        };
        if let Some(ask) = ask {
            if let Some(old) = keeper.take() {
                old.stop.store(true, Ordering::SeqCst);
                poke(&old.wake);
            }
            *keeper = Some(start(socket, ask)?);
        }
    }
    match answered.recv_timeout(ANSWER_WAIT) {
        Ok(r) => r,
        Err(_) => Err("the clipboard keeper did not answer".into()),
    }
}

/// A keeper runs (tests: it ends once another program takes the
/// selection).
pub fn running() -> bool {
    KEEPER
        .lock()
        .ok()
        .and_then(|k| k.as_ref().map(|h| h.alive.load(Ordering::SeqCst)))
        .unwrap_or(false)
}

fn start(socket: &Path, first: Ask) -> Result<Handle, String> {
    let wake = Arc::new(
        rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
            .map_err(|e| io::Error::from(e).to_string())?,
    );
    let (asks, rx) = mpsc::channel();
    let _ = asks.send(first);
    let alive = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        socket: socket.to_path_buf(),
        asks,
        wake: wake.clone(),
        alive: alive.clone(),
        stop: stop.clone(),
    };
    let path = socket.to_path_buf();
    std::thread::Builder::new()
        .name("strand-clipboard-keep".into())
        .spawn(move || keep(&path, &rx, &wake, &alive, &stop))
        .map_err(|e| e.to_string())?;
    Ok(handle)
}

/// A write in progress to a program pasting.
struct Writing {
    fd: OwnedFd,
    bytes: Arc<[u8]>,
    done: usize,
    progress: Instant,
}

impl Writing {
    /// Writes what the pipe takes now; `true` once it is finished (or
    /// failed: the reader went).
    fn write(&mut self) -> bool {
        loop {
            if self.done >= self.bytes.len() {
                return true;
            }
            match rustix::io::write(&self.fd, &self.bytes[self.done..]) {
                Ok(0) => return true,
                Ok(n) => {
                    self.done += n;
                    self.progress = Instant::now();
                }
                Err(rustix::io::Errno::AGAIN) => return false,
                Err(rustix::io::Errno::INTR) => {}
                Err(_) => return true,
            }
        }
    }
}

fn keep(
    socket: &Path,
    asks: &mpsc::Receiver<Ask>,
    wake: &OwnedFd,
    alive: &AtomicBool,
    stop: &AtomicBool,
) {
    if let Err(e) = serve(socket, asks, wake, alive, stop) {
        log::debug!("clipboard keeper: {e}");
    }
    // Whatever is still asked fails.
    let ended = {
        let _keeper = KEEPER.lock();
        alive.store(false, Ordering::SeqCst);
        asks.try_iter().collect::<Vec<_>>()
    };
    for ask in ended {
        let _ = ask
            .answer
            .send(Err("the clipboard's connection ended".into()));
    }
}

fn serve(
    socket: &Path,
    asks: &mpsc::Receiver<Ask>,
    wake: &OwnedFd,
    alive: &AtomicBool,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (conn, mut queue, mut wl) = match protocol::connect(socket) {
        Ok(c) => c,
        Err(e) => {
            for ask in asks.try_iter() {
                let _ = ask.answer.send(Err(e.clone()));
            }
            return Err(e);
        }
    };
    let qh = queue.handle();
    let mut current: Option<(Source, Held)> = None;
    let mut writes: Vec<Writing> = Vec::new();
    // Asks before the registry is in wait for it.
    let mut waiting: Vec<Ask> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        waiting.extend(asks.try_iter());
        wl.bind(&qh);
        if wl.synced {
            for ask in waiting.drain(..) {
                if !wl.available() {
                    let _ = ask.answer.send(Err(NO_DATA_CONTROL.into()));
                    continue;
                }
                let mimes: Vec<String> = ask.offers.iter().map(|(m, _)| m.clone()).collect();
                match wl.set_selection(&qh, &mimes) {
                    Some(source) => {
                        if let Some((old, _)) = current.take() {
                            old.destroy();
                        }
                        current = Some((source, ask.offers.into_iter().collect()));
                        let _ = ask.answer.send(Ok(()));
                    }
                    None => {
                        let _ = ask.answer.send(Err(NO_DATA_CONTROL.into()));
                    }
                }
            }
        }
        for ev in std::mem::take(&mut wl.events) {
            match ev {
                Ev::Send { source, mime, fd } => {
                    let bytes = current
                        .as_ref()
                        .filter(|(s, _)| s.id() == source)
                        .and_then(|(_, offers)| offers.get(&mime).cloned());
                    if let Some(bytes) = bytes
                        && rustix::io::ioctl_fionbio(&fd, true).is_ok()
                    {
                        let mut w = Writing {
                            fd,
                            bytes,
                            done: 0,
                            progress: Instant::now(),
                        };
                        if !w.write() {
                            writes.push(w);
                        }
                    }
                }
                Ev::Cancelled(id) => {
                    if current.as_ref().is_some_and(|(s, _)| s.id() == id)
                        && let Some((s, _)) = current.take()
                    {
                        s.destroy();
                    }
                }
                Ev::Selection {
                    offer: Some((offer, _)),
                    ..
                } => offer.destroy(),
                Ev::Selection { offer: None, .. } => {}
            }
        }
        if wl.lost {
            return Err(NO_DATA_CONTROL.into());
        }
        // Nothing held, nothing written, nothing asked: the keeper ends
        // (under the lock, so no ask is sent to a keeper that ended).
        if current.is_none() && writes.is_empty() && waiting.is_empty() && wl.synced {
            let _ = conn.flush();
            let Ok(_keeper) = KEEPER.lock() else {
                return Ok(());
            };
            let more: Vec<Ask> = asks.try_iter().collect();
            if more.is_empty() {
                alive.store(false, Ordering::SeqCst);
                return Ok(());
            }
            waiting.extend(more);
            continue;
        }
        let deadline = writes.iter().map(|w| w.progress + WRITE_STALL).min();
        let mut extra = vec![protocol::polled(wake, PollFlags::IN)];
        extra.extend(writes.iter().map(|w| (w.fd.as_fd(), PollFlags::OUT)));
        let revents = protocol::wait(&conn, &mut queue, &mut wl, &extra, deadline)?;
        drop(extra);
        if revents.first().is_some_and(|r| !r.is_empty()) {
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(wake, &mut buf);
        }
        let now = Instant::now();
        let mut i = 0;
        writes.retain_mut(|w| {
            i += 1;
            let ready = revents.get(i).is_some_and(|r| !r.is_empty());
            if ready && w.write() {
                return false;
            }
            now < w.progress + WRITE_STALL
        });
    }
}

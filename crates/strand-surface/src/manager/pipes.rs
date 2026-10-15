//! (M6) Pipes another program reads from or writes to, served on the
//! event loop without blocking it: a drop or a paste read
//! ([`State::read_all`]), a drag's export or our selection written
//! ([`State::write_all`]). Shared by `dnd.rs` and `selection.rs`.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::timer::{TimeoutAction, Timer};
use calloop::{PostAction, RegistrationToken};
use smithay_client_toolkit::data_device_manager::{ReadPipe, WritePipe};

use super::*;

/// What runs once a read ends: the bytes, or `None` when it could not be
/// read whole.
type Done<H> = Box<dyn FnOnce(&mut State<H>, Option<Vec<u8>>)>;

impl<H: SurfaceHost + 'static> State<H> {
    /// Reads `pipe` to its end without blocking the loop, then calls
    /// `done` with what it held. `None` when it cannot be read (a read
    /// error, more than `max` bytes, or no end within `timeout`: a sender
    /// that never closes its pipe costs a reader for that long, not for
    /// the life of the process); `what` names it in the warning
    /// (`"a drop (text/plain)"`).
    pub(super) fn read_all(
        &mut self,
        pipe: ReadPipe,
        max: usize,
        timeout: Duration,
        what: String,
        done: impl FnOnce(&mut Self, Option<Vec<u8>>) + 'static,
    ) {
        // Whichever ends first, the read or the time limit, takes `done`;
        // the other source is then removed.
        let done: Rc<RefCell<Option<Done<H>>>> = Rc::new(RefCell::new(Some(Box::new(done))));
        let limit: Rc<Cell<Option<RegistrationToken>>> = Rc::new(Cell::new(None));
        let mut buf = Vec::new();
        let read = self.handle.insert_source(pipe, {
            let (done, limit, what) = (done.clone(), limit.clone(), what.clone());
            move |_, file, state| {
                let end = |state: &mut Self, bytes: Option<Vec<u8>>| {
                    if let Some(t) = limit.take() {
                        state.handle.remove(t);
                    }
                    let f = done.borrow_mut().take();
                    if let Some(f) = f {
                        f(state, bytes);
                    }
                    PostAction::Remove
                };
                let mut chunk = [0u8; 16 * 1024];
                // Level-triggered: one read per wakeup never blocks.
                let mut f: &File = file;
                match f.read(&mut chunk) {
                    Ok(0) => end(state, Some(std::mem::take(&mut buf))),
                    Ok(n) if buf.len() + n > max => {
                        log::warn!("{what} over {max} bytes is ignored");
                        end(state, None)
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        PostAction::Continue
                    }
                    Err(e)
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                    {
                        PostAction::Continue
                    }
                    Err(e) => {
                        log::warn!("cannot read {what}: {e}");
                        end(state, None)
                    }
                }
            }
        });
        let read = match read {
            Ok(t) => t,
            Err(e) => {
                log::warn!("cannot read {what}: {}", e.error);
                let f = done.borrow_mut().take();
                if let Some(f) = f {
                    f(self, None);
                }
                return;
            }
        };
        let timer = Timer::from_duration(timeout);
        let timed = self.handle.insert_source(timer, {
            let what = what.clone();
            move |_, _, state| {
                let f = done.borrow_mut().take();
                if let Some(f) = f {
                    log::warn!(
                        "{what} not read within {timeout:?} is ignored (its sender never closed \
                         the pipe)"
                    );
                    state.handle.remove(read);
                    f(state, None);
                }
                TimeoutAction::Drop
            }
        });
        match timed {
            Ok(t) => limit.set(Some(t)),
            Err(e) => log::warn!("{what} is read with no time limit: {}", e.error),
        }
    }

    /// Writes `bytes` to `fd` a pipe-buffer's worth per wakeup, so the
    /// loop never blocks on a slow reader; a reader that closes early ends
    /// it, and one that reads nothing for `timeout` is given up on (the
    /// pipe closed). `what` names it in the log (`"a drag's text/plain"`).
    pub(super) fn write_all(
        &mut self,
        fd: WritePipe,
        bytes: impl AsRef<[u8]> + 'static,
        timeout: Duration,
        what: String,
    ) {
        // When the write last made progress, and the time limit's source:
        // whichever ends first, the write or the limit, removes the other.
        let progress = Rc::new(Cell::new(Instant::now()));
        let limit: Rc<Cell<Option<RegistrationToken>>> = Rc::new(Cell::new(None));
        let written = self.handle.insert_source(fd, {
            let (progress, limit, what) = (progress.clone(), limit.clone(), what.clone());
            let mut done = 0;
            move |_, file, state| {
                let end = |state: &mut Self| {
                    if let Some(t) = limit.take() {
                        state.handle.remove(t);
                    }
                    PostAction::Remove
                };
                let bytes = bytes.as_ref();
                // POLLOUT on a pipe: at least PIPE_BUF (4096) bytes fit.
                let stop = (done + 4096).min(bytes.len());
                let mut f: &File = file;
                match f.write(&bytes[done..stop]) {
                    Ok(n) => {
                        done += n;
                        progress.set(Instant::now());
                        if done >= bytes.len() {
                            end(state)
                        } else {
                            PostAction::Continue
                        }
                    }
                    Err(e)
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                    {
                        PostAction::Continue
                    }
                    Err(e) => {
                        log::debug!("{what} was not all read: {e}");
                        end(state)
                    }
                }
            }
        });
        let written = match written {
            Ok(t) => t,
            Err(e) => {
                log::warn!("cannot give {what}: {}", e.error);
                return;
            }
        };
        let named = what.clone();
        let timed =
            self.handle
                .insert_source(Timer::from_duration(timeout), move |due, _, state| {
                    let next = progress.get() + timeout;
                    if next > due {
                        return TimeoutAction::ToInstant(next);
                    }
                    log::warn!("{what} not read for {timeout:?} is given up");
                    state.handle.remove(written);
                    TimeoutAction::Drop
                });
        match timed {
            Ok(t) => limit.set(Some(t)),
            Err(e) => log::warn!("{named} is written with no time limit: {}", e.error),
        }
    }
}

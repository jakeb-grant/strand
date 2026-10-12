//! The inspector's socket client: requests in `strand_core::ipc`'s
//! JSON-line protocol, their answers, and the events pick mode sends on
//! the same connection (`picked`, `pick_ended`), which are kept until
//! asked for.

use std::collections::VecDeque;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value as Json;
use strand_core::ipc::{Request, encode};

/// What the connection's reader hands over.
#[derive(Debug)]
pub enum Line {
    /// An answer or an event.
    Json(Json),
    /// The shell closed the connection (or it broke).
    Closed,
}

/// A connection to a running `strand run`.
#[derive(Debug)]
pub struct Connection {
    stream: UnixStream,
    lines: mpsc::Receiver<Line>,
    events: VecDeque<Json>,
    closed: bool,
}

impl Connection {
    /// Connects to the socket at `path`. Its lines are read on a thread
    /// of their own, and `more` (when given) is called after each, so a
    /// caller can wait on the socket and its terminal together.
    pub fn connect(path: &Path, more: Option<Box<dyn Fn() + Send>>) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let reader = stream.try_clone()?;
        let (tx, lines) = mpsc::channel();
        std::thread::Builder::new()
            .name("inspect-socket".into())
            .spawn(move || {
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    let Ok(v) = serde_json::from_str::<Json>(&line) else {
                        continue;
                    };
                    if tx.send(Line::Json(v)).is_err() {
                        return;
                    }
                    if let Some(m) = &more {
                        m();
                    }
                }
                let _ = tx.send(Line::Closed);
                if let Some(m) = &more {
                    m();
                }
            })?;
        Ok(Self {
            stream,
            lines,
            events: VecDeque::new(),
            closed: false,
        })
    }

    /// Sends `req` and waits (up to `wait`) for its answer; events that
    /// come first are kept for [`Connection::event`].
    pub fn ask(&mut self, req: &Request, wait: Duration) -> io::Result<Json> {
        self.stream.write_all(encode(req).as_bytes())?;
        loop {
            match self.next(wait)? {
                Some(v) if v.get("event").is_some() => self.events.push_back(v),
                Some(v) => return Ok(v),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("no answer to `{}`", req.cmd()),
                    ));
                }
            }
        }
    }

    /// The next event, waiting up to `wait` (zero: only one already
    /// here). `Ok(None)`: none came.
    pub fn event(&mut self, wait: Duration) -> io::Result<Option<Json>> {
        if let Some(e) = self.events.pop_front() {
            return Ok(Some(e));
        }
        loop {
            match self.next(wait)? {
                Some(v) if v.get("event").is_some() => return Ok(Some(v)),
                // A stray answer (nothing is asked meanwhile): skipped.
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }

    /// Whether the shell has closed the connection.
    pub fn closed(&self) -> bool {
        self.closed
    }

    fn next(&mut self, wait: Duration) -> io::Result<Option<Json>> {
        if self.closed {
            return Err(gone());
        }
        let got = match wait.is_zero() {
            true => self.lines.try_recv().ok(),
            false => self.lines.recv_timeout(wait).ok(),
        };
        match got {
            Some(Line::Json(v)) => Ok(Some(v)),
            Some(Line::Closed) => {
                self.closed = true;
                Err(gone())
            }
            None => Ok(None),
        }
    }
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the shell closed the connection")
}

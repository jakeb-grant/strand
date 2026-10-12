//! The inspector's socket client: requests in `strand_core::ipc`'s
//! JSON-line protocol, their answers, and the events pick mode sends on
//! the same connection (`picked`, `pick_ended`), which are kept until
//! asked for.
//!
//! Answers carry no id and do not all come in request order: logic
//! answers most requests as it reads them, but an `inspect.node` pane
//! only once main's layout hop is back, so a pane can come after the
//! answers of requests sent after it. A request that times out
//! therefore leaves no way to tell which later answer is whose, and the
//! connection starts again ([`Connection::ask`]); the shell ends what the
//! old one had on (pick mode, the outline, the flash) when it goes.

use std::collections::VecDeque;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
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

/// Called after each line the connection reads.
pub type More = Arc<dyn Fn() + Send + Sync>;

/// A connection to a running `strand run`.
pub struct Connection {
    path: PathBuf,
    more: Option<More>,
    stream: UnixStream,
    lines: mpsc::Receiver<Line>,
    events: VecDeque<Json>,
    closed: bool,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("path", &self.path)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Connects to the socket at `path`. Its lines are read on a thread
    /// of their own, and `more` (when given) is called after each, so a
    /// caller can wait on the socket and its terminal together.
    pub fn connect(path: &Path, more: Option<More>) -> io::Result<Self> {
        let (stream, lines) = open(path, more.clone())?;
        Ok(Self {
            path: path.to_path_buf(),
            more,
            stream,
            lines,
            events: VecDeque::new(),
            closed: false,
        })
    }

    /// Sends `req` and waits (up to `wait`) for its answer; events that
    /// come first are kept for [`Connection::event`].
    ///
    /// No answer in time (`ErrorKind::TimedOut`): the connection starts
    /// again, since a late answer could not be told from a later
    /// request's (inspect-r3-1); the old one's late answers and events
    /// are dropped with it, and the shell ends its pick mode, outline and
    /// flash. When the socket cannot be reached again the connection is
    /// [`closed`](Connection::closed).
    pub fn ask(&mut self, req: &Request, wait: Duration) -> io::Result<Json> {
        if let Err(e) = self.stream.write_all(encode(req).as_bytes()) {
            // The shell is gone (or the connection broke).
            self.closed = true;
            return Err(e);
        }
        loop {
            match self.next(wait)? {
                Some(v) if v.get("event").is_some() => self.events.push_back(v),
                Some(v) => return Ok(v),
                None => {
                    let why = match self.restart() {
                        Ok(()) => "started a new connection".to_string(),
                        Err(e) => format!("and reconnecting failed: {e}"),
                    };
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("no answer to `{}` in {wait:?}: {why}", req.cmd()),
                    ));
                }
            }
        }
    }

    /// A new connection to the same socket in place of this one.
    fn restart(&mut self) -> io::Result<()> {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        self.events.clear();
        match open(&self.path, self.more.clone()) {
            Ok((stream, lines)) => {
                (self.stream, self.lines) = (stream, lines);
                Ok(())
            }
            Err(e) => {
                self.closed = true;
                Err(e)
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
                // An answer no request waits for (none is sent while one
                // is out): skipped.
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// Forgets the `name` events kept so far. Each came before the
    /// answer just read, so it refers to the state before that request
    /// (a `pick_ended` before `inspect.pick`'s `ok` ends the pick mode
    /// before, never the one just turned on).
    pub fn forget(&mut self, name: &str) {
        self.events.retain(|e| e["event"] != name);
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

/// A connection to `path` and the receiver its reader thread fills.
fn open(path: &Path, more: Option<More>) -> io::Result<(UnixStream, mpsc::Receiver<Line>)> {
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
            // A restarted connection's receiver is gone: its end is
            // nobody's news.
            if tx.send(Line::Closed).is_ok()
                && let Some(m) = &more
            {
                m();
            }
        })?;
    Ok((stream, lines))
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the shell closed the connection")
}

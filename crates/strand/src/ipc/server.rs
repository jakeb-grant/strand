//! The socket's server, on the logic thread's loop: every socket is
//! non-blocking, and a client that does not read its events is dropped
//! once the last flush left more than [`MAX_QUEUED`] unwritten as another
//! line comes, or once one loop turn queued more than [`MAX_TURN_QUEUED`]
//! for it; a line of any size is queued whole.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use serde_json::{Value as Json, json};

use super::{MAX_LINE, Request, parse};

/// Output a client may leave unread before it is dropped, checked as
/// another line is queued against what the last flush left unwritten
/// (never against the line itself, nor lines queued since that flush).
const MAX_QUEUED: usize = 1 << 20;

/// Output a client may have queued at all, checked as another line is
/// queued: lines of one loop turn (thousands of pipelined requests read
/// at once) do not count against [`MAX_QUEUED`], so this bounds them; a
/// client's queue never exceeds it by more than one line.
const MAX_TURN_QUEUED: usize = 16 * MAX_QUEUED;

/// What one [`Server::read`] takes from a client at most: the rest waits
/// for the loop's next turn (its read source is level-triggered), so a
/// client that writes faster than it is read cannot hold the loop.
const READ_BUDGET: usize = 4 * MAX_LINE;

/// A connected client, by number.
pub type ClientId = u64;

#[derive(Debug)]
struct Client {
    stream: UnixStream,
    /// Its read source (`None` once it sent EOF).
    token: Option<RegistrationToken>,
    /// Its write source, registered only while output waits for a
    /// reader that has not taken it (no polling: the loop wakes when
    /// the socket can take more).
    wtoken: Option<RegistrationToken>,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// What of `out` the last [`Server::flush`] could not write: what
    /// the client left unread. Lines queued since (several answers or
    /// events of one loop turn) never count against it.
    backlog: usize,
    watching: bool,
    /// Requests handed to the shell and not answered yet.
    pending: usize,
    /// It shut its writing side (`nc -N`, `socat`): nothing more to
    /// read, but it is kept until its answers and events are written.
    eof: bool,
    closed: bool,
}

/// The listening socket and its clients (the logic thread's). Its
/// sources mark the loop's [`Ready`] so the loop looks again.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    /// The bound socket's `(dev, ino)`: only that file is removed on drop.
    id: Option<(u64, u64)>,
    clients: HashMap<ClientId, Client>,
    next: ClientId,
}

/// What the loop's sources set: something to accept, clients to read.
#[derive(Debug, Default)]
pub struct Ready {
    pub accept: bool,
    pub readable: Vec<ClientId>,
    /// A client's socket can take more of its queued output.
    pub writable: bool,
}

/// The loop data the server's sources write to.
pub trait HasReady {
    fn ready(&mut self) -> &mut Ready;
}

impl Server {
    /// Bind `path` (a stale socket left by a crash is replaced; one a
    /// live shell answers on is not, nor anything that is not a socket:
    /// a `STRAND_SOCKET` naming a regular file is refused, not deleted)
    /// and register it on `handle`.
    pub fn bind<D: HasReady + 'static>(
        path: &Path,
        handle: &LoopHandle<'static, D>,
    ) -> io::Result<Server> {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if !meta.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a socket; not replacing it",
                        path.display()
                    ),
                ));
            }
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another strand answers on {}", path.display()),
                ));
            }
            let _ = std::fs::remove_file(path);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let watch = listener.try_clone()?;
        handle
            .insert_source(
                Generic::new(watch, Interest::READ, Mode::Level),
                |_, _, data: &mut D| {
                    data.ready().accept = true;
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| io::Error::other(e.error))?;
        let id = socket_id(path);
        Ok(Server {
            listener,
            id,
            path: path.to_path_buf(),
            clients: HashMap::new(),
            next: 1,
        })
    }

    /// Accept every waiting connection.
    pub fn accept<D: HasReady + 'static>(&mut self, handle: &LoopHandle<'static, D>) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let id = self.next;
                    self.next += 1;
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let Ok(watch) = stream.try_clone() else {
                        continue;
                    };
                    let token = handle.insert_source(
                        Generic::new(watch, Interest::READ, Mode::Level),
                        move |_, _, data: &mut D| {
                            let r = data.ready();
                            if !r.readable.contains(&id) {
                                r.readable.push(id);
                            }
                            Ok(PostAction::Continue)
                        },
                    );
                    let Ok(token) = token else { continue };
                    self.clients.insert(
                        id,
                        Client {
                            stream,
                            token: Some(token),
                            wtoken: None,
                            inbuf: Vec::new(),
                            out: Vec::new(),
                            backlog: 0,
                            watching: false,
                            pending: 0,
                            eof: false,
                            closed: false,
                        },
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    log::warn!("ipc: accept: {e}");
                    return;
                }
            }
        }
    }

    /// Read what `id` sent: its complete request lines, parsed (a bad
    /// one is answered here).
    pub fn read(&mut self, id: ClientId) -> Vec<Request> {
        let mut out = Vec::new();
        let Some(c) = self.clients.get_mut(&id) else {
            return out;
        };
        let mut buf = [0u8; 4096];
        let mut taken = 0;
        while !c.eof && !c.closed && taken < READ_BUDGET {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.eof = true;
                    break;
                }
                Ok(n) => {
                    taken += n;
                    c.inbuf.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    c.closed = true;
                    break;
                }
            }
        }
        let mut answers = Vec::new();
        while let Some(nl) = c.inbuf.iter().position(|b| *b == b'\n') {
            // A line over the cap closes the connection, whether or not
            // its newline came in the same read (strand-core's MAX_LINE).
            if nl > MAX_LINE {
                c.closed = true;
                break;
            }
            let line: Vec<u8> = c.inbuf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match parse(line) {
                Ok(Request::Watch) => {
                    c.watching = true;
                    answers.push(json!({"ok": true}));
                    // Answered here; handed out too, for what a new
                    // watcher hears at once (after its answer).
                    out.push(Request::Watch);
                }
                Ok(r) => {
                    c.pending += 1;
                    out.push(r);
                }
                Err(e) => answers.push(json!({"ok": false, "error": e})),
            }
        }
        if c.inbuf.len() > MAX_LINE {
            c.closed = true;
        }
        for a in answers {
            self.send(id, &a);
        }
        out
    }

    /// Queue `v` as one line for `id`. A client that left more than
    /// [`MAX_QUEUED`] unread at the last flush is dropped: the cap is for
    /// a client that does not read, so one line of any size (a `get` of
    /// a long list), and any lines after it in the same loop turn, are
    /// queued whole and delivered to a client that reads them
    /// (decisions.md ipc-large-answer). A client with more than
    /// [`MAX_TURN_QUEUED`] queued is dropped too, so a turn's pipelined
    /// requests cannot queue without bound (audit-ipc-turn-backlog).
    pub fn send(&mut self, id: ClientId, v: &Json) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if c.closed {
            return;
        }
        if c.backlog > MAX_QUEUED {
            log::warn!(
                "ipc: client {id} does not read its events: dropped with {} bytes unread",
                c.backlog
            );
            c.closed = true;
            return;
        }
        if c.out.len() > MAX_TURN_QUEUED {
            log::warn!(
                "ipc: client {id} does not read its answers: dropped with {} bytes queued",
                c.out.len()
            );
            c.closed = true;
            return;
        }
        c.out.extend_from_slice(v.to_string().as_bytes());
        c.out.push(b'\n');
    }

    /// Queue the answer to one of `id`'s requests that [`Server::read`]
    /// handed out.
    pub fn answer(&mut self, id: ClientId, v: &Json) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.pending = c.pending.saturating_sub(1);
        }
        self.send(id, v);
    }

    /// (M5) Whether client `id` is still connected (the inspector's
    /// pick mode, outline and flash end with the client that asked).
    pub fn has(&self, id: ClientId) -> bool {
        self.clients.contains_key(&id)
    }

    /// How many clients watch.
    pub fn watchers(&self) -> usize {
        self.clients.values().filter(|c| c.watching).count()
    }

    /// Queue `event` for every watching client.
    pub fn broadcast(&mut self, event: &Json) {
        let ids: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.watching)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.send(id, event);
        }
    }

    /// Write what can be written now; drop closed clients. A client with
    /// output its socket cannot take yet gets a write source that wakes
    /// the loop when it can (removed once the output is written).
    /// Returns true while output is still queued.
    pub fn flush<D: HasReady + 'static>(&mut self, handle: &LoopHandle<'static, D>) -> bool {
        let mut queued = false;
        for c in self.clients.values_mut() {
            while !c.out.is_empty() && !c.closed {
                match c.stream.write(&c.out) {
                    Ok(0) => c.closed = true,
                    Ok(n) => {
                        c.out.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => c.closed = true,
                }
            }
            c.backlog = c.out.len();
            let waiting = !c.out.is_empty() && !c.closed;
            queued |= waiting;
            match (waiting, c.wtoken.is_some()) {
                (true, false) => {
                    let token = c.stream.try_clone().ok().and_then(|w| {
                        handle
                            .insert_source(
                                Generic::new(w, Interest::WRITE, Mode::Level),
                                |_, _, data: &mut D| {
                                    data.ready().writable = true;
                                    Ok(PostAction::Continue)
                                },
                            )
                            .ok()
                    });
                    match token {
                        Some(t) => c.wtoken = Some(t),
                        // Cannot wait for it: it goes.
                        None => c.closed = true,
                    }
                }
                (false, true) => {
                    if let Some(t) = c.wtoken.take() {
                        handle.remove(t);
                    }
                }
                _ => {}
            }
            // Half-closed: its socket stays readable (EOF) for good, so
            // it leaves the loop now; once everything it waits for is
            // written (a watcher: until a write fails), it goes.
            if c.eof {
                if let Some(t) = c.token.take() {
                    handle.remove(t);
                }
                if c.out.is_empty() && c.pending == 0 && !c.watching {
                    c.closed = true;
                }
            }
        }
        let gone: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.closed)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some(c) = self.clients.remove(&id) {
                for t in [c.token, c.wtoken].into_iter().flatten() {
                    handle.remove(t);
                }
            }
        }
        queued
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Only the socket this server bound: a file put in its place since
        // is not ours to delete.
        if self.id.is_some() && socket_id(&self.path) == self.id {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The `(dev, ino)` of the socket at `path`, if a socket is there.
fn socket_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.file_type()
        .is_socket()
        .then(|| (meta.dev(), meta.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::time::Duration;

    use crate::ipc::encode;

    #[derive(Default)]
    struct Data(Ready);
    impl HasReady for Data {
        fn ready(&mut self) -> &mut Ready {
            &mut self.0
        }
    }

    /// A watcher that stops reading (a paused pager) does not make the
    /// loop poll: its output waits on a write source, the loop sleeps
    /// until the watcher reads, and then the rest is written and the
    /// source removed.
    #[test]
    fn a_stalled_watcher_is_waited_for_without_polling() {
        use calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("strand-ipc-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let mut el = EventLoop::<Data>::try_new().unwrap();
        let handle = el.handle();
        let mut server = Server::bind(&path, &handle).unwrap();
        let mut data = Data::default();
        let mut client = std::io::BufReader::new(UnixStream::connect(&path).unwrap());
        client
            .get_mut()
            .write_all(encode(&Request::Watch).as_bytes())
            .unwrap();
        while server.clients.values().all(|c| !c.watching) {
            el.dispatch(Some(Duration::from_secs(5)), &mut data)
                .unwrap();
            let ready = std::mem::take(&mut data.0);
            if ready.accept {
                server.accept(&handle);
            }
            for id in ready.readable {
                server.read(id);
            }
        }
        // More than the socket buffer takes, under the cap.
        let ev = json!({"event": "reload", "pad": "x".repeat(1000)});
        for _ in 0..600 {
            server.broadcast(&ev);
        }
        assert!(server.flush(&handle), "output waits for the reader");
        assert!(server.clients.values().any(|c| c.wtoken.is_some()));
        // Nothing to do while the watcher does not read: the loop sleeps
        // the whole timeout.
        let t = std::time::Instant::now();
        el.dispatch(Some(Duration::from_millis(200)), &mut data)
            .unwrap();
        assert!(!data.0.writable, "woken although the socket is full");
        assert!(t.elapsed() >= Duration::from_millis(150));
        // The watcher reads everything: the loop is woken to write.
        let reader = std::thread::spawn(move || {
            let mut line = String::new();
            let mut n = 0;
            while n < 601 {
                line.clear();
                client.read_line(&mut line).unwrap();
                n += 1;
            }
            n
        });
        while server.flush(&handle) {
            el.dispatch(Some(Duration::from_secs(5)), &mut data)
                .unwrap();
            assert!(std::mem::take(&mut data.0).writable);
        }
        assert_eq!(reader.join().unwrap(), 601);
        assert!(server.clients.values().all(|c| c.wtoken.is_none()));
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// (ipc-large-answer) An answer over the cap (a `get` of a long list
    /// of documents) is delivered whole to a client that reads it, and a
    /// line after it is still queued once it has been read; a watcher
    /// that reads nothing is dropped once more than the cap is unread.
    #[test]
    fn an_answer_over_the_cap_is_delivered_and_a_stalled_watcher_dropped() {
        use calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("strand-ipc-large-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let mut el = EventLoop::<Data>::try_new().unwrap();
        let handle = el.handle();
        let mut server = Server::bind(&path, &handle).unwrap();
        let mut data = Data::default();
        let mut pump = |server: &mut Server, data: &mut Data| {
            el.dispatch(Some(Duration::from_millis(50)), data).unwrap();
            let ready = std::mem::take(&mut data.0);
            if ready.accept {
                server.accept(&handle);
            }
            for id in ready.readable {
                server.read(id);
            }
            server.flush(&handle)
        };
        let mut getter = std::io::BufReader::new(UnixStream::connect(&path).unwrap());
        while server.clients.is_empty() {
            pump(&mut server, &mut data);
        }
        let id = *server.clients.keys().next().unwrap();
        let big = "x".repeat(3 * MAX_QUEUED);
        server.answer(id, &json!({"ok": true, "value": big}));
        let reader = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                getter.read_line(&mut line).unwrap();
                lines.push(line);
            }
            lines
        });
        while pump(&mut server, &mut data) {}
        // Read whole: the next line is queued, not taken for a stall.
        server.send(id, &json!({"ok": true, "value": "after"}));
        while pump(&mut server, &mut data) {}
        let lines = reader.join().unwrap();
        let first: Json = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(first["value"].as_str().map(str::len), Some(3 * MAX_QUEUED));
        assert_eq!(lines[1].trim(), r#"{"ok":true,"value":"after"}"#);

        // A watcher that never reads: kept up to the cap, then dropped.
        let mut watcher = UnixStream::connect(&path).unwrap();
        watcher
            .write_all(encode(&Request::Watch).as_bytes())
            .unwrap();
        while server.clients.values().all(|c| !c.watching) {
            pump(&mut server, &mut data);
        }
        let w = *server
            .clients
            .iter()
            .find(|(_, c)| c.watching)
            .map(|(id, _)| id)
            .unwrap();
        let ev = json!({"event": "reload", "pad": "x".repeat(1000)});
        for _ in 0..(2 * MAX_QUEUED / 1000) {
            server.broadcast(&ev);
            // What its socket takes is written; the rest stays queued.
            server.flush(&handle);
            if !server.has(w) {
                break;
            }
        }
        assert!(!server.has(w), "a watcher that does not read is dropped");
        drop(watcher);
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A request line over MAX_LINE closes the connection even when its
    /// newline comes in the same read, and is never handed out; a client
    /// that streams bytes with no newline is closed after one bounded
    /// read, which returns although the client keeps writing.
    #[test]
    fn an_oversize_line_closes_the_client_and_a_stream_is_read_in_bounds() {
        use calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("strand-ipc-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let mut el = EventLoop::<Data>::try_new().unwrap();
        let handle = el.handle();
        let mut server = Server::bind(&path, &handle).unwrap();
        let mut data = Data::default();
        let mut connect = |server: &mut Server| {
            let stream = UnixStream::connect(&path).unwrap();
            while server.clients.is_empty() {
                el.dispatch(Some(Duration::from_millis(50)), &mut data)
                    .unwrap();
                if std::mem::take(&mut data.0).accept {
                    server.accept(&handle);
                }
            }
            let id = *server.clients.keys().next().unwrap();
            (stream, id)
        };

        // One line over the cap, its newline and EOF after it, all
        // waiting before the server reads (the socket buffer holds it).
        let (mut long, id) = connect(&mut server);
        let mut line = format!("get {}", "x".repeat(MAX_LINE));
        line.push('\n');
        long.write_all(line.as_bytes()).unwrap();
        long.shutdown(std::net::Shutdown::Write).unwrap();
        let handed = server.read(id);
        assert!(handed.is_empty(), "{handed:?}");
        assert!(server.clients[&id].closed, "the over-long line closes it");
        server.flush(&handle);
        assert!(!server.has(id));
        drop(long);

        // A client that never stops writing and never sends a newline.
        let (mut stream, id) = connect(&mut server);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writing = stop.clone();
        let writer = std::thread::spawn(move || {
            let zeros = [0u8; 1 << 16];
            while !writing.load(std::sync::atomic::Ordering::Relaxed) {
                if stream.write_all(&zeros).is_err() {
                    break;
                }
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !server.clients[&id].closed {
            assert!(std::time::Instant::now() < deadline, "never closed");
            server.read(id);
        }
        let c = &server.clients[&id];
        assert!(
            c.inbuf.len() <= MAX_LINE + READ_BUDGET + 4096,
            "read in bounds: {} bytes",
            c.inbuf.len()
        );
        server.flush(&handle);
        assert!(!server.has(id));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// (ipc-large-answer) Two answers queued for one client in one loop
    /// turn, the first over the cap (two pipelined requests, a `get` of
    /// a long list first), are both delivered: the client had no flush
    /// to read from, so nothing is held against it.
    #[test]
    fn answers_queued_in_one_turn_are_delivered_past_the_cap() {
        use calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("strand-ipc-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let mut el = EventLoop::<Data>::try_new().unwrap();
        let handle = el.handle();
        let mut server = Server::bind(&path, &handle).unwrap();
        let mut data = Data::default();
        let mut getter = std::io::BufReader::new(UnixStream::connect(&path).unwrap());
        while server.clients.is_empty() {
            el.dispatch(Some(Duration::from_millis(50)), &mut data)
                .unwrap();
            if std::mem::take(&mut data.0).accept {
                server.accept(&handle);
            }
        }
        let id = *server.clients.keys().next().unwrap();
        let big = "x".repeat(3 * MAX_QUEUED / 2);
        server.answer(id, &json!({"ok": true, "value": big}));
        server.answer(id, &json!({"ok": true, "value": "after"}));
        let reader = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                getter.read_line(&mut line).unwrap();
                lines.push(line);
            }
            lines
        });
        while server.flush(&handle) {
            el.dispatch(Some(Duration::from_secs(5)), &mut data)
                .unwrap();
        }
        assert!(
            server.has(id),
            "a client that had no chance to read is kept"
        );
        let lines = reader.join().unwrap();
        let first: Json = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(
            first["value"].as_str().map(str::len),
            Some(3 * MAX_QUEUED / 2)
        );
        assert_eq!(lines[1].trim(), r#"{"ok":true,"value":"after"}"#);
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// (audit-ipc-turn-backlog) A client that pipelines many requests
    /// with large answers in one write and reads nothing is dropped in
    /// that one loop turn, its queue never more than the turn bound plus
    /// one answer.
    #[test]
    fn pipelined_requests_never_read_close_the_client_in_one_turn() {
        use calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("strand-ipc-pipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let mut el = EventLoop::<Data>::try_new().unwrap();
        let handle = el.handle();
        let mut server = Server::bind(&path, &handle).unwrap();
        let mut data = Data::default();
        let mut client = UnixStream::connect(&path).unwrap();
        while server.clients.is_empty() {
            el.dispatch(Some(Duration::from_millis(50)), &mut data)
                .unwrap();
            if std::mem::take(&mut data.0).accept {
                server.accept(&handle);
            }
        }
        let id = *server.clients.keys().next().unwrap();
        let requests = 200;
        let get = encode(&Request::Get { path: "x".into() });
        client.write_all(get.repeat(requests).as_bytes()).unwrap();
        let handed = server.read(id);
        assert_eq!(handed.len(), requests, "one read takes every request");
        let answer = json!({"ok": true, "value": "x".repeat(200_000)});
        let size = answer.to_string().len() + 1;
        let mut most = 0;
        for _ in &handed {
            server.answer(id, &answer);
            most = most.max(server.clients[&id].out.len());
        }
        let c = &server.clients[&id];
        assert!(c.closed, "a client that reads nothing is dropped");
        assert!(
            most <= MAX_TURN_QUEUED + size,
            "queue bounded: {most} bytes"
        );
        server.flush(&handle);
        assert!(!server.has(id));
        drop(client);
        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// (m4-audit) A path that is not a socket (a mistyped
    /// `STRAND_SOCKET` naming a config file) is refused and left as it
    /// is; a stale socket is replaced; on drop only the bound socket is
    /// removed, not a file put in its place.
    #[test]
    fn bind_replaces_only_a_stale_socket() {
        use calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("strand-ipc-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let el = EventLoop::<Data>::try_new().unwrap();
        let handle = el.handle();

        let notes = dir.join("notes.txt");
        std::fs::write(&notes, "keep me").unwrap();
        let Err(e) = Server::bind(&notes, &handle) else {
            panic!("bound over a regular file");
        };
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "keep me");
        let link = dir.join("link.sock");
        std::os::unix::fs::symlink(&notes, &link).unwrap();
        assert!(Server::bind(&link, &handle).is_err());
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "keep me");

        // A stale socket (its listener gone) is replaced.
        let path = dir.join("s.sock");
        drop(UnixListener::bind(&path).unwrap());
        // A test of this binary that spawns a process at this moment
        // holds a copy of the listener's descriptor until its child
        // execs (close-on-exec closes it only then), so the socket can
        // answer for that instant: wait until it no longer does.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while UnixStream::connect(&path).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the dropped listener still answers"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let server = Server::bind(&path, &handle).expect("a stale socket is replaced");
        assert!(UnixStream::connect(&path).is_ok());
        // A live one is not.
        assert_eq!(
            Server::bind(&path, &handle).err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        // Replaced under it by a file: the drop leaves the file.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "not ours").unwrap();
        drop(server);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not ours");
        // Its own socket is removed on drop.
        std::fs::remove_file(&path).unwrap();
        drop(Server::bind(&path, &handle).unwrap());
        assert!(std::fs::symlink_metadata(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}

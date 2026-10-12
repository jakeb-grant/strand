//! The socket's server, on the logic thread's loop: every socket is
//! non-blocking, and a client that does not read its events is dropped
//! once [`MAX_QUEUED`] is queued for it.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use serde_json::{Value as Json, json};

use super::{MAX_LINE, Request, parse};

/// Output queued for one client before it is dropped.
const MAX_QUEUED: usize = 1 << 20;

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
        while !c.eof {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.eof = true;
                    break;
                }
                Ok(n) => c.inbuf.extend_from_slice(&buf[..n]),
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

    /// Queue `v` as one line for `id`.
    pub fn send(&mut self, id: ClientId, v: &Json) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        c.out.extend_from_slice(v.to_string().as_bytes());
        c.out.push(b'\n');
        if c.out.len() > MAX_QUEUED {
            log::warn!("ipc: client {id} does not read its events: dropped");
            c.closed = true;
        }
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

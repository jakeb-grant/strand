//! The IPC socket: `strand reload [--hard]` and `strand watch [--json]`
//! talk to a running `strand run` over a Unix socket in
//! `$XDG_RUNTIME_DIR` (design.md, "CLI"; the M5 `get | set | toggle |
//! call` commands extend the same protocol).
//!
//! **Protocol (version 1).** Newline-delimited JSON. A client sends one
//! request object per line, `{"v": 1, "cmd": "<name>", …}`; the server
//! answers each with one line, `{"ok": true, …}` or `{"ok": false,
//! "error": "…"}`. An unknown `cmd` (or a newer `v`) is answered `{"ok":
//! false, "error": "unknown command …"}` and the connection stays open,
//! so a newer client can probe an older shell.
//!
//! - `{"cmd": "reload", "hard": false}`: rescan the config now; the
//!   answer comes once that reload is committed (or held back) and
//!   carries its event: `{"ok": true, "event": {…}}`.
//! - `{"cmd": "watch"}`: `{"ok": true}`, then one event object per line
//!   for as long as the connection stays open: `{"event": "reload", …}`
//!   (files, edit classes, kept and reset cells, notices, timing,
//!   diagnostics), `{"event": "fault", …}` (a runtime error that froze a
//!   component).
//!
//! The server lives on the logic thread's loop (`docs/architecture.md`,
//! "Threads"): every socket is non-blocking, a client that does not read
//! its events is dropped once 1 MiB is queued for it.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use serde_json::{Value as Json, json};

/// The protocol version this build speaks.
pub const VERSION: u64 = 1;

/// Output queued for one client before it is dropped.
const MAX_QUEUED: usize = 1 << 20;

/// A request line longer than this closes the connection.
const MAX_LINE: usize = 64 * 1024;

/// The socket of the shell on this Wayland display: `$STRAND_SOCKET`, or
/// `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.sock` (`strand-0.sock`
/// without a display name). `None` without a runtime directory.
pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(
        std::env::var_os("STRAND_SOCKET"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("WAYLAND_DISPLAY"),
    )
}

fn socket_path_from(
    explicit: Option<std::ffi::OsString>,
    runtime: Option<std::ffi::OsString>,
    display: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let dir = PathBuf::from(runtime.filter(|d| !d.is_empty())?);
    if !dir.is_absolute() {
        return None;
    }
    let display = display
        .map(|d| {
            // A display given as a path (`/run/user/1000/wayland-1`) is
            // named by its file name.
            let p = PathBuf::from(d);
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "0".into());
    Some(dir.join(format!("strand-{display}.sock")))
}

/// What a client asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Reload { hard: bool },
    Watch,
}

/// Parse one request line.
pub fn parse(line: &str) -> Result<Request, String> {
    let v: Json = serde_json::from_str(line).map_err(|e| format!("not a JSON request: {e}"))?;
    let version = v.get("v").and_then(Json::as_u64).unwrap_or(VERSION);
    if version > VERSION {
        return Err(format!(
            "protocol version {version} is newer than this shell's ({VERSION})"
        ));
    }
    match v.get("cmd").and_then(Json::as_str) {
        Some("reload") => Ok(Request::Reload {
            hard: v.get("hard").and_then(Json::as_bool).unwrap_or(false),
        }),
        Some("watch") => Ok(Request::Watch),
        Some(other) => Err(format!("unknown command `{other}`")),
        None => Err("a request needs a `cmd`".into()),
    }
}

/// A request as a line.
pub fn encode(req: &Request) -> String {
    let v = match req {
        Request::Reload { hard } => json!({"v": VERSION, "cmd": "reload", "hard": hard}),
        Request::Watch => json!({"v": VERSION, "cmd": "watch"}),
    };
    format!("{v}\n")
}

/// A connected client, by number.
pub type ClientId = u64;

#[derive(Debug)]
struct Client {
    stream: UnixStream,
    token: RegistrationToken,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    watching: bool,
    closed: bool,
}

/// The listening socket and its clients (the logic thread's). Its
/// sources mark the loop's [`Ready`] so the loop looks again.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    clients: HashMap<ClientId, Client>,
    next: ClientId,
}

/// What the loop's sources set: something to accept, clients to read.
#[derive(Debug, Default)]
pub struct Ready {
    pub accept: bool,
    pub readable: Vec<ClientId>,
}

/// The loop data the server's sources write to.
pub trait HasReady {
    fn ready(&mut self) -> &mut Ready;
}

impl Server {
    /// Bind `path` (a stale socket left by a crash is replaced; one a
    /// live shell answers on is not) and register it on `handle`.
    pub fn bind<D: HasReady + 'static>(
        path: &Path,
        handle: &LoopHandle<'static, D>,
    ) -> io::Result<Server> {
        if path.exists() {
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
        Ok(Server {
            listener,
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
                            token,
                            inbuf: Vec::new(),
                            out: Vec::new(),
                            watching: false,
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
        loop {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.closed = true;
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
                }
                Ok(r) => out.push(r),
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

    /// Write what can be written now; drop closed clients. Returns true
    /// while output is still queued (the loop should look again soon).
    pub fn flush<D: 'static>(&mut self, handle: &LoopHandle<'static, D>) -> bool {
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
            queued |= !c.out.is_empty() && !c.closed;
        }
        let gone: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.closed)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some(c) = self.clients.remove(&id) {
                handle.remove(c.token);
            }
        }
        queued
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Connect to the running shell.
pub fn connect() -> Result<UnixStream, String> {
    let path = socket_path().ok_or("no socket: set XDG_RUNTIME_DIR or STRAND_SOCKET")?;
    UnixStream::connect(&path).map_err(|e| {
        format!(
            "no running strand on {} ({e}); start one with `strand run`",
            path.display()
        )
    })
}

/// Send `req` and read the answer line (`timeout` at most).
pub fn request(stream: &UnixStream, req: &Request, timeout: Duration) -> Result<Json, String> {
    let mut w = stream;
    w.write_all(encode(req).as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("no answer: {e}"))?;
    serde_json::from_str(&line).map_err(|e| format!("bad answer {line:?}: {e}"))
}

/// `strand reload [--hard]`.
pub fn reload_cli(args: &[String]) -> Result<String, String> {
    let hard = match args {
        [] => false,
        [a] if a == "--hard" => true,
        _ => return Err("usage: strand reload [--hard]".into()),
    };
    let stream = connect()?;
    let answer = request(&stream, &Request::Reload { hard }, Duration::from_secs(30))?;
    if answer.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(answer
            .get("error")
            .and_then(Json::as_str)
            .unwrap_or("failed")
            .to_string());
    }
    Ok(answer.get("event").map(describe).unwrap_or_default())
}

/// `strand watch [--json]`: print every event until the shell ends.
pub fn watch_cli(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let json = match args {
        [] => false,
        [a] if a == "--json" => true,
        _ => return Err("usage: strand watch [--json]".into()),
    };
    let stream = connect()?;
    let answer = request(&stream, &Request::Watch, Duration::from_secs(10))?;
    if answer.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(format!("refused: {answer}"));
    }
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;
    for line in BufReader::new(&stream).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let text = if json {
            format!("{line}\n")
        } else {
            match serde_json::from_str::<Json>(&line) {
                Ok(v) => describe(&v),
                Err(_) => format!("{line}\n"),
            }
        };
        if out
            .write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .is_err()
        {
            // The reader went away (`strand watch | head`).
            return Ok(());
        }
    }
    Ok(())
}

/// An event as text for people.
pub fn describe(ev: &Json) -> String {
    let s = |v: &Json| v.as_str().unwrap_or("").to_string();
    let list = |key: &str| -> Vec<String> {
        ev.get(key)
            .and_then(Json::as_array)
            .map(|a| a.iter().map(s).collect())
            .unwrap_or_default()
    };
    let mut out = String::new();
    match ev.get("event").and_then(Json::as_str) {
        Some("reload") => {
            let ms = ev
                .pointer("/timing/total_ms")
                .and_then(Json::as_f64)
                .unwrap_or(0.0);
            let classes = list("classes");
            let committed = list("committed");
            let held = list("held");
            out.push_str(&format!(
                "reload ({ms:.1} ms): {}\n",
                if classes.is_empty() {
                    "no change".to_string()
                } else {
                    classes.join(", ")
                }
            ));
            if !committed.is_empty() {
                out.push_str(&format!("  committed: {}\n", committed.join(", ")));
            }
            if !held.is_empty() {
                out.push_str(&format!("  held back: {}\n", held.join(", ")));
            }
            let kept = list("kept");
            if !kept.is_empty() {
                out.push_str(&format!("  kept: {}\n", kept.join(", ")));
            }
            if let Some(reset) = ev.get("reset").and_then(Json::as_array) {
                for r in reset {
                    out.push_str(&format!("  reset: {} ({})\n", s(&r["cell"]), s(&r["why"])));
                }
            }
            for n in list("notices") {
                out.push_str(&format!("  {n}\n"));
            }
            if let Some(ds) = ev.get("diagnostics").and_then(Json::as_array) {
                for d in ds {
                    out.push_str(&format!("  {}\n", s(&d["short"])));
                }
            }
        }
        Some("fault") => {
            out.push_str(&format!("fault: {}\n", s(&ev["message"])));
            if let Some(at) = ev.get("at").and_then(Json::as_str) {
                out.push_str(&format!("  at {at}\n"));
            }
        }
        _ => out.push_str(&format!("{ev}\n")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn the_socket_is_per_display() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            socket_path_from(None, os("/run/user/1"), os("wayland-1")),
            Some(PathBuf::from("/run/user/1/strand-wayland-1.sock"))
        );
        assert_eq!(
            socket_path_from(None, os("/run/user/1"), os("/run/user/1/wayland-2")),
            Some(PathBuf::from("/run/user/1/strand-wayland-2.sock"))
        );
        assert_eq!(
            socket_path_from(None, os("/run/user/1"), None),
            Some(PathBuf::from("/run/user/1/strand-0.sock"))
        );
        assert_eq!(socket_path_from(None, None, os("wayland-1")), None);
        assert_eq!(socket_path_from(None, os("relative"), None), None);
        assert_eq!(
            socket_path_from(os("/tmp/s.sock"), None, None),
            Some(PathBuf::from("/tmp/s.sock"))
        );
    }

    #[test]
    fn requests_round_trip_and_unknown_ones_are_refused() {
        for r in [
            Request::Reload { hard: true },
            Request::Reload { hard: false },
            Request::Watch,
        ] {
            assert_eq!(parse(encode(&r).trim()), Ok(r));
        }
        assert_eq!(
            parse(r#"{"cmd":"reload"}"#),
            Ok(Request::Reload { hard: false })
        );
        assert!(
            parse(r#"{"v":1,"cmd":"toggle","path":"launcher.open"}"#)
                .unwrap_err()
                .contains("unknown command `toggle`")
        );
        assert!(
            parse(r#"{"v":2,"cmd":"watch"}"#)
                .unwrap_err()
                .contains("newer")
        );
        assert!(parse("watch").is_err());
    }

    #[test]
    fn events_read_as_text() {
        let ev = json!({
            "event": "reload",
            "classes": ["prop", "node-added"],
            "committed": ["bar.strand"],
            "held": [],
            "kept": ["Clock.open"],
            "reset": [{"cell": "t.b", "why": "renamed"}],
            "notices": ["launcher.query: kept \"fir\" (default changed) [reset]"],
            "timing": {"total_ms": 12.25},
            "diagnostics": [{"short": "bar.strand:3:1: error[x]: y"}],
        });
        assert_eq!(
            describe(&ev),
            "reload (12.2 ms): prop, node-added\n  committed: bar.strand\n  kept: Clock.open\n  reset: t.b (renamed)\n  launcher.query: kept \"fir\" (default changed) [reset]\n  bar.strand:3:1: error[x]: y\n"
        );
    }
}

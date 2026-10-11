//! The IPC protocol: requests, answers and events, and their JSON-line
//! codec (docs/architecture.md, "M5 interfaces": `strand_core::ipc`).
//! Types and codec only, no I/O and no threads: the binary's socket
//! server and CLI, strand-services' D-Bus front end and `strand-dev
//! inspect` all name these.
//!
//! **Protocol (version 1).** Newline-delimited JSON. A client sends one
//! request object per line, `{"v": 1, "cmd": "<name>", …}`; the server
//! answers each with one line, `{"ok": true, …}` or `{"ok": false,
//! "error": "…"}` ([`ok`], [`refused`]). An unknown `cmd` (or a newer
//! `v`) is answered `{"ok": false, "error": "unknown command …"}` and
//! the connection stays open, so a newer client can probe an older
//! shell. Events (`{"event": "reload", …}`) are lines too ([`line`]).
//!
//! Version 1 gains commands only. M5's (`get`, `toggle`, `call`,
//! `clear`, `report`, the inspector's `inspect.*` and `reset{node}`)
//! have their variants and their encoding here, and their fields are
//! read by [`parse_m5`]; [`parse`] reads one only once the shell serves
//! it, so a shell that does not answers it as an unknown command.
//!
//! [`IpcRequests`] carries requests from another thread (the services
//! thread's D-Bus front end) to the logic loop, which answers each
//! through a [`Reply`] that an async caller awaits without a runtime
//! here; an [`EventSink`] carries the events the socket's watchers get
//! the other way (the D-Bus `Changed` signal). [`socket_path`] is where
//! every client finds the socket.

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use serde_json::{Value as Json, json};

/// The protocol version this build speaks.
pub const VERSION: u64 = 1;

/// A request line longer than this closes the connection. It bounds
/// only incoming requests: an answer is bounded by the connection's
/// output queue (1 MiB).
pub const MAX_LINE: usize = 64 * 1024;

/// The D-Bus bus name and interface name of a running shell, at object
/// path `/` (decisions.md m5-owner Q12). The only spelling of the name:
/// the owner may rename it before 1.0.
pub const DBUS_NAME: &str = "io.github.jakeb_grant.Strand";

/// What a client asked for.
///
/// Every string field but `Set::value` and `Call::args`' items holds a
/// non-empty string: an empty one encodes, but does not parse back (a
/// field a command needs is refused, an optional one reads as absent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `strand reload [--hard]`.
    Reload { hard: bool },
    /// `strand watch`: answered, then every event.
    Watch,
    /// `[reset]` from the command line: a state cell back to its default.
    Reset { path: String },
    /// (M5) `reset{node}`: reset subtree, the state under a node back to
    /// its defaults (`Instance::reset_subtree`). `node` is a node id as
    /// the inspector's answers spell it.
    ResetNode { node: String },
    /// `strand set <path> <value>`: an exported state cell (or a field of
    /// one), the value as text read by the cell's type.
    Set { path: String, value: String },
    /// (M5) `strand get <path>`: what `set` writes.
    Get { path: String },
    /// (M5) `strand toggle <path>`: flip an exported boolean.
    Toggle { path: String },
    /// (M5) `strand call <service.action> [args]`: a service-level
    /// action, its arguments as text read by the parameters' types.
    Call { target: String, args: Vec<String> },
    /// (M5) `strand clear <path>`: drop a runtime overlay (`$accent`).
    Clear { path: String },
    /// (M5) `strand report`.
    Report,
    /// (M5) `inspect.pick{on}`: pick mode on or off.
    InspectPick { on: bool },
    /// (M5) `inspect.node{id}`: one node's pane.
    InspectNode { id: String },
    /// (M5) `inspect.dump{surface?}`: the tree, of one surface or all.
    InspectDump { surface: Option<String> },
    /// (M5) `inspect.highlight{id?}`: outline a node (none: no outline).
    InspectHighlight { id: Option<String> },
    /// (M5) `inspect.flash{on}`: damage flashing on or off.
    InspectFlash { on: bool },
    /// A mock service change (`STRAND_MOCK` only; the acceptance tests'
    /// driver): the request as sent.
    Mock(Json),
}

impl Request {
    /// Its `cmd`.
    #[inline]
    pub fn cmd(&self) -> &'static str {
        match self {
            Request::Reload { .. } => "reload",
            Request::Watch => "watch",
            Request::Reset { .. } | Request::ResetNode { .. } => "reset",
            Request::Set { .. } => "set",
            Request::Get { .. } => "get",
            Request::Toggle { .. } => "toggle",
            Request::Call { .. } => "call",
            Request::Clear { .. } => "clear",
            Request::Report => "report",
            Request::InspectPick { .. } => "inspect.pick",
            Request::InspectNode { .. } => "inspect.node",
            Request::InspectDump { .. } => "inspect.dump",
            Request::InspectHighlight { .. } => "inspect.highlight",
            Request::InspectFlash { .. } => "inspect.flash",
            Request::Mock(_) => "mock",
        }
    }
}

/// A non-empty string field.
#[inline]
fn text(v: &Json, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse one request line: the commands this shell serves.
///
/// M4's commands (`reload`, `watch`, `reset{path}`, `set`, `mock`)
/// parse as they did in M4, field for field (a `reset` naming a `node`
/// beside its `path` still resets the path). M5's commands parse through
/// [`parse_m5`] once their stream serves them, by an arm here
/// (`"get" | "toggle" => return parse_m5(cmd, &v)`); until then a
/// client hears `unknown command`, M4's answer word for word
/// (decisions.md m5-core).
#[inline]
pub fn parse(line: &str) -> Result<Request, String> {
    let v: Json = serde_json::from_str(line).map_err(|e| format!("not a JSON request: {e}"))?;
    let version = v.get("v").and_then(Json::as_u64).unwrap_or(VERSION);
    if version > VERSION {
        return Err(format!(
            "protocol version {version} is newer than this shell's ({VERSION})"
        ));
    }
    let Some(cmd) = v.get("cmd").and_then(Json::as_str) else {
        return Err("a request needs a `cmd`".into());
    };
    Ok(match cmd {
        "reload" => Request::Reload {
            hard: v.get("hard").and_then(Json::as_bool).unwrap_or(false),
        },
        "watch" => Request::Watch,
        "reset" => match text(&v, "path") {
            Some(path) => Request::Reset { path },
            None => return Err("`reset` needs a `path`".into()),
        },
        "set" => match (text(&v, "path"), v.get("value").and_then(Json::as_str)) {
            (Some(path), Some(value)) => Request::Set {
                path,
                value: value.into(),
            },
            _ => return Err("`set` needs a `path` and a `value`".into()),
        },
        "mock" => Request::Mock(v),
        other => return Err(format!("unknown command `{other}`")),
    })
}

/// The fields of M5's command `cmd` (`get`, `toggle`, `call`, `clear`,
/// `report`, `inspect.*`, and `reset` in its `{node}` form), read from
/// the request object `v`; [`parse`] calls it for each command a stream
/// serves. Shared so the streams never write a codec of their own, and
/// unused by [`parse`] until then, so an unserved command costs the
/// binary no code (decisions.md m5-core).
#[inline]
pub fn parse_m5(cmd: &str, v: &Json) -> Result<Request, String> {
    // A field M5's commands need, and what a request without it hears.
    let need = |key: &str| text(v, key).ok_or_else(|| format!("`{cmd}` needs `{key}`"));
    let on = || {
        v.get("on")
            .and_then(Json::as_bool)
            .ok_or_else(|| format!("`{cmd}` needs `on` (true or false)"))
    };
    Ok(match cmd {
        "reset" => Request::ResetNode {
            node: need("node")?,
        },
        "get" => Request::Get {
            path: need("path")?,
        },
        "toggle" => Request::Toggle {
            path: need("path")?,
        },
        "clear" => Request::Clear {
            path: need("path")?,
        },
        "call" => Request::Call {
            target: need("target")?,
            args: match v.get("args") {
                None => Vec::new(),
                Some(a) => a
                    .as_array()
                    .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect())
                    .ok_or("`call`'s `args` are a list of strings")?,
            },
        },
        "report" => Request::Report,
        "inspect.pick" => Request::InspectPick { on: on()? },
        "inspect.node" => Request::InspectNode { id: need("id")? },
        "inspect.dump" => Request::InspectDump {
            surface: text(v, "surface"),
        },
        "inspect.highlight" => Request::InspectHighlight { id: text(v, "id") },
        "inspect.flash" => Request::InspectFlash { on: on()? },
        other => return Err(format!("unknown command `{other}`")),
    })
}

/// A request as a line.
#[inline]
pub fn encode(req: &Request) -> String {
    let mut m = serde_json::Map::new();
    let mut put = |key: &str, v: Json| {
        m.insert(key.into(), v);
    };
    let text = |s: &str| Json::String(s.into());
    put("v", VERSION.into());
    put("cmd", text(req.cmd()));
    match req {
        Request::Mock(v) => return line(v),
        Request::Watch | Request::Report => {}
        Request::Reload { hard } => put("hard", Json::Bool(*hard)),
        Request::Reset { path }
        | Request::Get { path }
        | Request::Toggle { path }
        | Request::Clear { path } => put("path", text(path)),
        Request::ResetNode { node } => put("node", text(node)),
        Request::Set { path, value } => {
            put("path", text(path));
            put("value", text(value));
        }
        Request::Call { target, args } => {
            put("target", text(target));
            put("args", Json::Array(args.iter().map(|a| text(a)).collect()));
        }
        Request::InspectPick { on } | Request::InspectFlash { on } => put("on", Json::Bool(*on)),
        Request::InspectNode { id } | Request::InspectHighlight { id: Some(id) } => {
            put("id", text(id))
        }
        Request::InspectDump { surface: Some(s) } => put("surface", text(s)),
        Request::InspectDump { surface: None } | Request::InspectHighlight { id: None } => {}
    }
    line(&Json::Object(m))
}

/// An answer or an event: a JSON object, one line on the wire.
pub type Answer = Json;

/// `{"ok": true}`.
#[inline]
pub fn ok() -> Answer {
    json!({"ok": true})
}

/// `{"ok": false, "error": why}`.
#[inline]
pub fn refused(why: impl Into<String>) -> Answer {
    json!({"ok": false, "error": why.into()})
}

/// How a shell answers a command it does not serve (yet): as one it
/// does not know, so a client probes it the same way.
#[inline]
pub fn unknown(cmd: &str) -> Answer {
    refused(format!("unknown command `{cmd}`"))
}

/// Why an answer refused, or `None` for `{"ok": true, …}`.
#[inline]
pub fn refusal(answer: &Answer) -> Option<String> {
    (answer.get("ok").and_then(Json::as_bool) != Some(true)).then(|| {
        answer
            .get("error")
            .and_then(Json::as_str)
            .unwrap_or("failed")
            .to_string()
    })
}

/// `v` as one line, newline included.
#[inline]
pub fn line(v: &Json) -> String {
    format!("{v}\n")
}

/// A request from another thread with the one-shot its answer goes to.
#[derive(Debug)]
pub struct IpcCall {
    pub request: Request,
    pub reply: Reply,
}

#[derive(Debug, Default)]
struct Slot {
    answer: Option<Answer>,
    /// Answered (or the reply dropped): the waiter completes.
    done: bool,
    waker: Option<Waker>,
}

/// The answering half of a one-shot: completes once, with
/// [`Reply::send`], or with a refusal when dropped unanswered, so a
/// waiter never hangs on a request the shell let go.
#[derive(Debug)]
pub struct Reply(Arc<Mutex<Slot>>);

/// The waiting half: a future of the [`Answer`], woken through its
/// `Waker` (no runtime needed here).
#[derive(Debug)]
pub struct ReplyFuture(Arc<Mutex<Slot>>);

/// A one-shot for one request's answer.
pub fn reply() -> (Reply, ReplyFuture) {
    let slot = Arc::new(Mutex::new(Slot::default()));
    (Reply(Arc::clone(&slot)), ReplyFuture(slot))
}

impl Reply {
    /// Complete with `answer`.
    pub fn send(self, answer: Answer) {
        self.complete(Some(answer));
    }

    fn complete(&self, answer: Option<Answer>) {
        let waker = match self.0.lock() {
            Ok(mut s) if !s.done => {
                s.done = true;
                s.answer = answer;
                s.waker.take()
            }
            _ => None,
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.complete(None);
    }
}

impl Future for ReplyFuture {
    type Output = Answer;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Answer> {
        let Ok(mut s) = self.0.lock() else {
            return Poll::Ready(refused("the shell's answer was lost"));
        };
        if s.done {
            return Poll::Ready(
                s.answer
                    .take()
                    .unwrap_or_else(|| refused("the shell dropped the request")),
            );
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// The logic loop is gone: nothing answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpcClosed;

impl fmt::Display for IpcClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the shell's logic loop is gone")
    }
}

impl std::error::Error for IpcClosed {}

/// The calls waiting for the logic loop, and whether it still drains
/// them.
#[derive(Debug, Default)]
struct Queue {
    calls: Mutex<Vec<IpcCall>>,
    closed: AtomicBool,
}

/// The sending side of the request channel to the logic loop: a shared
/// queue plus the wake function the binary supplies (the logic loop's
/// sleeper ping). Cheap to clone.
#[derive(Clone)]
pub struct IpcRequests {
    queue: Arc<Queue>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl fmt::Debug for IpcRequests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IpcRequests")
    }
}

/// The logic loop's side: drained, never waited on. Dropping it (the
/// loop ended) refuses what is still queued and every later call.
#[derive(Debug)]
pub struct IpcReceiver(Arc<Queue>);

/// A request channel whose sends call `wake`.
pub fn channel(wake: impl Fn() + Send + Sync + 'static) -> (IpcRequests, IpcReceiver) {
    let queue = Arc::new(Queue::default());
    (
        IpcRequests {
            queue: Arc::clone(&queue),
            wake: Arc::new(wake),
        },
        IpcReceiver(queue),
    )
}

impl IpcRequests {
    /// Hand `request` to the logic loop and wake it; await the result
    /// for its answer.
    pub fn call(&self, request: Request) -> Result<ReplyFuture, IpcClosed> {
        let (reply, answer) = reply();
        {
            let mut calls = self.queue.calls.lock().map_err(|_| IpcClosed)?;
            // Checked under the lock the receiver's drop takes, so no
            // call is queued after its last drain.
            if self.queue.closed.load(Ordering::Acquire) {
                return Err(IpcClosed);
            }
            calls.push(IpcCall { request, reply });
        }
        (self.wake)();
        Ok(answer)
    }
}

impl IpcReceiver {
    /// The calls waiting now, in the order they were sent, without
    /// blocking.
    pub fn drain(&self) -> Vec<IpcCall> {
        self.0
            .calls
            .lock()
            .map(|mut c| std::mem::take(&mut *c))
            .unwrap_or_default()
    }
}

impl Drop for IpcReceiver {
    fn drop(&mut self) {
        let left = match self.0.calls.lock() {
            Ok(mut c) => {
                self.0.closed.store(true, Ordering::Release);
                std::mem::take(&mut *c)
            }
            Err(_) => Vec::new(),
        };
        // Their replies, dropped, refuse.
        drop(left);
    }
}

/// Where the events the socket's watchers get also go: the D-Bus front
/// end's `Changed` signal (S-ipc; docs/architecture.md, "M5
/// interfaces"). The logic loop calls it with every event it streams
/// (`{"event": "reload" | "fault" | "notices", …}`), whether or not a
/// socket is bound or watched; it runs on the logic thread and must not
/// block (hand the event to the services thread and return).
#[derive(Clone)]
pub struct EventSink(Arc<dyn Fn(&Answer) + Send + Sync>);

impl EventSink {
    /// A sink calling `f` with each event.
    pub fn new(f: impl Fn(&Answer) + Send + Sync + 'static) -> Self {
        EventSink(Arc::new(f))
    }

    /// Hand `event` on.
    #[inline]
    pub fn send(&self, event: &Answer) {
        (self.0)(event);
    }
}

impl fmt::Debug for EventSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventSink")
    }
}

/// The socket of the shell on this Wayland display: `$STRAND_SOCKET`, or
/// `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.sock` (`strand-0.sock`
/// without a display name). `None` without a runtime directory. Shared
/// by the shell, its CLI and `strand-dev inspect`, so every client finds
/// the socket by the same rule.
#[inline]
pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(
        std::env::var_os("STRAND_SOCKET"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("WAYLAND_DISPLAY"),
    )
}

/// [`socket_path`] from the three variables' values.
#[inline]
pub fn socket_path_from(
    explicit: Option<OsString>,
    runtime: Option<OsString>,
    display: Option<OsString>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_event_sink_hands_each_event_on() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&seen);
        let sink = EventSink::new(move |ev| into.lock().unwrap().push(ev.clone()));
        let copy = sink.clone();
        sink.send(&json!({"event": "reload"}));
        copy.send(&json!({"event": "notices"}));
        assert_eq!(
            *seen.lock().unwrap(),
            [json!({"event": "reload"}), json!({"event": "notices"})]
        );
        assert_eq!(format!("{sink:?}"), "EventSink");
    }

    fn every_request() -> Vec<Request> {
        vec![
            Request::Reload { hard: true },
            Request::Reload { hard: false },
            Request::Watch,
            Request::Reset {
                path: "launcher.query".into(),
            },
            Request::ResetNode {
                node: "12v3".into(),
            },
            Request::Set {
                path: "theme.look".into(),
                value: "mocha".into(),
            },
            Request::Get {
                path: "theme.look".into(),
            },
            Request::Toggle {
                path: "launcher.open".into(),
            },
            Request::Call {
                target: "notifications.clear".into(),
                args: vec![],
            },
            Request::Call {
                target: "media.seek".into(),
                args: vec!["5".into(), "x y".into()],
            },
            Request::Clear {
                path: "$accent".into(),
            },
            Request::Report,
            Request::InspectPick { on: true },
            Request::InspectPick { on: false },
            Request::InspectNode { id: "4v0".into() },
            Request::InspectDump { surface: None },
            Request::InspectDump {
                surface: Some("bar".into()),
            },
            Request::InspectHighlight { id: None },
            Request::InspectHighlight {
                id: Some("4v0".into()),
            },
            Request::InspectFlash { on: true },
        ]
    }

    /// M4's commands, which [`parse`] reads.
    fn m4(r: &Request) -> bool {
        matches!(
            r,
            Request::Reload { .. } | Request::Watch | Request::Reset { .. } | Request::Set { .. }
        )
    }

    /// A line read as a shell that serves every command reads it.
    fn parse_all(line: &str) -> Result<Request, String> {
        let v: Json = serde_json::from_str(line).unwrap();
        match parse(line) {
            Err(_) => parse_m5(v["cmd"].as_str().unwrap(), &v),
            r => r,
        }
    }

    #[test]
    fn requests_round_trip_and_unknown_ones_are_refused() {
        for r in every_request() {
            let line = encode(&r);
            assert!(line.ends_with('\n') && !line.trim_end().contains('\n'));
            let v: Json = serde_json::from_str(&line).unwrap();
            assert_eq!(v["cmd"], r.cmd(), "{line}");
            assert_eq!(v["v"], VERSION, "{line}");
            assert_eq!(parse_all(line.trim()), Ok(r.clone()));
            if m4(&r) {
                assert_eq!(parse(line.trim()), Ok(r));
            } else {
                // Not served yet: M4's answer, word for word (`reset` is
                // known, and M4 said what it lacks).
                let m4_said = match r {
                    Request::ResetNode { .. } => "`reset` needs a `path`".to_string(),
                    _ => format!("unknown command `{}`", r.cmd()),
                };
                assert_eq!(parse(line.trim()), Err(m4_said), "{line}");
            }
        }
        let mock = parse(r#"{"cmd":"mock","service":"battery"}"#).unwrap();
        assert_eq!(parse(encode(&mock).trim()), Ok(mock));
        assert_eq!(
            parse(r#"{"cmd":"reload"}"#),
            Ok(Request::Reload { hard: false })
        );
        assert!(
            parse(r#"{"v":1,"cmd":"frobnicate"}"#)
                .unwrap_err()
                .contains("unknown command `frobnicate`")
        );
        assert!(
            parse_m5("frobnicate", &json!({}))
                .unwrap_err()
                .contains("unknown command `frobnicate`")
        );
        assert!(
            parse(r#"{"v":2,"cmd":"watch"}"#)
                .unwrap_err()
                .contains("newer")
        );
        assert!(parse("watch").is_err());
        assert!(parse(r#"{"v":1}"#).unwrap_err().contains("needs a `cmd`"));
    }

    /// Empty strings are not values (`Request`'s doc): they encode, and
    /// read back refused or absent; `set`'s value is the exception.
    #[test]
    fn empty_strings_do_not_round_trip_except_set_values() {
        let set = Request::Set {
            path: "a.b".into(),
            value: String::new(),
        };
        assert_eq!(parse(encode(&set).trim()), Ok(set));
        let call = Request::Call {
            target: "a.b".into(),
            args: vec![String::new()],
        };
        assert_eq!(parse_all(encode(&call).trim()), Ok(call));
        for r in [
            Request::Reset {
                path: String::new(),
            },
            Request::Set {
                path: String::new(),
                value: "x".into(),
            },
            Request::ResetNode {
                node: String::new(),
            },
            Request::Get {
                path: String::new(),
            },
            Request::Call {
                target: String::new(),
                args: vec![],
            },
            Request::InspectNode { id: String::new() },
        ] {
            let line = encode(&r);
            assert!(parse_all(line.trim()).is_err(), "{line}");
        }
        assert_eq!(
            parse_all(
                encode(&Request::InspectDump {
                    surface: Some(String::new())
                })
                .trim()
            ),
            Ok(Request::InspectDump { surface: None })
        );
        assert_eq!(
            parse_all(
                encode(&Request::InspectHighlight {
                    id: Some(String::new())
                })
                .trim()
            ),
            Ok(Request::InspectHighlight { id: None })
        );
    }

    #[test]
    fn malformed_requests_say_what_they_lack() {
        // M4's commands, refused as M4 refused them.
        for (line, why) in [
            (r#"{"cmd":"reset"}"#, "`reset` needs a `path`"),
            (r#"{"cmd":"reset","path":""}"#, "`reset` needs a `path`"),
            (r#"{"cmd":"reset","node":"1v0"}"#, "`reset` needs a `path`"),
            (
                r#"{"cmd":"set","path":"a.b"}"#,
                "`set` needs a `path` and a `value`",
            ),
            (r#"{"cmd":"get"}"#, "unknown command `get`"),
            (
                r#"{"cmd":"inspect.pick"}"#,
                "unknown command `inspect.pick`",
            ),
        ] {
            assert_eq!(parse(line), Err(why.to_string()), "{line}");
        }
        // M4 read a `reset` with a `node` beside its `path` as the path.
        assert_eq!(
            parse(r#"{"cmd":"reset","path":"a.b","node":"1v0"}"#),
            Ok(Request::Reset { path: "a.b".into() })
        );
        // M5's, once served.
        for (line, why) in [
            (r#"{"cmd":"reset"}"#, "`reset` needs `node`"),
            (r#"{"cmd":"get"}"#, "`get` needs `path`"),
            (r#"{"cmd":"toggle","path":""}"#, "`toggle` needs `path`"),
            (r#"{"cmd":"clear"}"#, "`clear` needs `path`"),
            (r#"{"cmd":"call"}"#, "`call` needs `target`"),
            (
                r#"{"cmd":"call","target":"a.b","args":[1]}"#,
                "list of strings",
            ),
            (
                r#"{"cmd":"call","target":"a.b","args":"x"}"#,
                "list of strings",
            ),
            (r#"{"cmd":"inspect.pick"}"#, "needs `on`"),
            (r#"{"cmd":"inspect.flash","on":"yes"}"#, "needs `on`"),
            (r#"{"cmd":"inspect.node"}"#, "`inspect.node` needs `id`"),
        ] {
            let v: Json = serde_json::from_str(line).unwrap();
            let e = parse_m5(v["cmd"].as_str().unwrap(), &v).unwrap_err();
            assert!(e.contains(why), "{line}: {e}");
        }
    }

    #[test]
    fn answers_read_back() {
        assert_eq!(line(&ok()), "{\"ok\":true}\n");
        assert_eq!(refusal(&ok()), None);
        assert_eq!(refusal(&refused("no")), Some("no".into()));
        assert_eq!(refusal(&json!({"ok": false})), Some("failed".into()));
        assert_eq!(
            refusal(&unknown("toggle")),
            Some("unknown command `toggle`".into())
        );
    }

    #[test]
    fn the_dbus_name_is_the_owners() {
        assert_eq!(DBUS_NAME, "io.github.jakeb_grant.Strand");
    }

    /// A waker that counts its wakes.
    struct Count(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn poll(f: &mut ReplyFuture, waker: &Waker) -> Poll<Answer> {
        Pin::new(f).poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn a_reply_completes_once_and_wakes_its_waiter() {
        let count = Arc::new(Count(Default::default()));
        let waker = Waker::from(Arc::clone(&count));
        let (reply, mut answer) = super::reply();
        assert!(poll(&mut answer, &waker).is_pending());
        reply.send(ok());
        assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(poll(&mut answer, &waker), Poll::Ready(ok()));

        // Dropped unanswered: the waiter hears a refusal, never hangs.
        let (reply, mut answer) = super::reply();
        assert!(poll(&mut answer, &waker).is_pending());
        drop(reply);
        assert_eq!(count.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        let Poll::Ready(a) = poll(&mut answer, &waker) else {
            panic!("pending after the drop");
        };
        assert!(refusal(&a).unwrap().contains("dropped"));
    }

    #[test]
    fn calls_reach_the_receiver_and_wake_the_loop() {
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = Arc::clone(&woken);
        let (tx, rx) = channel(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(rx.drain().is_empty(), "nothing sent");
        let tx2 = tx.clone();
        let from_thread = std::thread::spawn(move || tx2.call(Request::Report).unwrap())
            .join()
            .unwrap();
        let mut answer = tx.call(Request::Watch).unwrap();
        assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 2);
        let calls = rx.drain();
        assert_eq!(
            calls.iter().map(|c| c.request.clone()).collect::<Vec<_>>(),
            vec![Request::Report, Request::Watch]
        );
        let mut calls = calls.into_iter();
        drop(calls.next());
        calls.next().unwrap().reply.send(refused("no"));
        let waker = Waker::noop();
        assert_eq!(poll(&mut answer, waker), Poll::Ready(refused("no")));
        let mut first = from_thread;
        assert!(matches!(poll(&mut first, waker), Poll::Ready(a) if refusal(&a).is_some()));
        // Queued when the loop ends: refused, as is every later call.
        let mut late = tx.call(Request::Report).unwrap();
        drop(rx);
        assert!(matches!(poll(&mut late, waker), Poll::Ready(a) if refusal(&a).is_some()));
        assert_eq!(tx.call(Request::Report).err(), Some(IpcClosed));
    }
}

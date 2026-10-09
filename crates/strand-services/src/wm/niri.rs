//! niri's IPC, our own implementation (the `niri-ipc` crate is GPL-3.0,
//! and niri adds fields and events in patch versions, so this one parses
//! leniently and sits behind the `niri` feature).
//!
//! JSON lines over `$NIRI_SOCKET`: a request (`"Workspaces"`, `"Windows"`,
//! `"FocusedOutput"`, `{"Action":{…}}`) gets one reply (`{"Ok":…}` or
//! `{"Err":"…"}`); after `"EventStream"` the socket only carries events
//! (`{"WorkspacesChanged":{…}}`, …). Formats are niri 26.04's
//! (`niri-ipc/src/lib.rs`). Unknown events and fields are ignored.
//!
//! Every request gets a connection of its own: niri before 25.05 reads
//! one request per connection and closes it (`src/ipc/server.rs`,
//! `handle_client`); later versions take more, but requests are rare
//! (boot, reconnects, actions), so one per connection works everywhere.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::backoff::Backoff;
use super::lines::next_line;
use super::model::{Window, WmState, Workspace};
use super::{AdapterMsg, Cmd, IpcSnapshot, WmAction, WmError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct NWorkspace {
    pub id: u64,
    pub idx: u64,
    pub name: Option<String>,
    pub output: Option<String>,
    pub is_urgent: bool,
    pub is_active: bool,
    pub is_focused: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct NWindow {
    pub id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub workspace_id: Option<u64>,
    pub is_focused: bool,
    pub is_urgent: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct NOutput {
    name: String,
}

/// What one event did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    None,
    Changed,
    /// `ConfigLoaded { failed }` (not the one every new stream starts with).
    Reloaded(bool),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct State {
    pub workspaces: Vec<NWorkspace>,
    pub windows: Vec<NWindow>,
    pub focused_output: Option<String>,
    /// The stream's first `ConfigLoaded` reports the last load, not a
    /// reload.
    pub seen_config: bool,
}

fn parse<T: for<'de> Deserialize<'de>>(v: &Value) -> Option<T> {
    T::deserialize(v).ok()
}

impl State {
    /// The state the replies to `"Workspaces"`, `"Windows"` and
    /// `"FocusedOutput"` report (each the value under its name in the
    /// reply's `Ok`; `null` when niri has no focused output).
    pub(crate) fn from_replies(
        workspaces: &Value,
        windows: &Value,
        focused_output: &Value,
    ) -> Self {
        Self {
            workspaces: parse(workspaces).unwrap_or_default(),
            windows: parse(windows).unwrap_or_default(),
            focused_output: parse::<Option<NOutput>>(focused_output)
                .flatten()
                .map(|o| o.name),
            seen_config: false,
        }
    }

    /// Applies one event line's JSON (`{"Name": {fields}}`).
    pub(crate) fn apply(&mut self, event: &Value) -> Effect {
        let Some((name, body)) = event.as_object().and_then(|o| o.iter().next()) else {
            return Effect::None;
        };
        match name.as_str() {
            "WorkspacesChanged" => {
                let Some(ws) = body.get("workspaces").and_then(parse::<Vec<NWorkspace>>) else {
                    return Effect::None;
                };
                self.workspaces = ws;
                if let Some(f) = self.workspaces.iter().find(|w| w.is_focused) {
                    self.focused_output = f.output.clone();
                }
                Effect::Changed
            }
            "WorkspaceActivated" => {
                let id = body.get("id").and_then(Value::as_u64);
                let focused = body
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let Some(id) = id else { return Effect::None };
                let Some(output) = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == id)
                    .map(|w| w.output.clone())
                else {
                    return Effect::None;
                };
                for w in &mut self.workspaces {
                    if w.output == output {
                        w.is_active = w.id == id;
                    }
                    if focused {
                        w.is_focused = w.id == id;
                    }
                }
                if focused {
                    self.focused_output = output;
                }
                Effect::Changed
            }
            "WorkspaceUrgencyChanged" => {
                let (Some(id), Some(urgent)) = (
                    body.get("id").and_then(Value::as_u64),
                    body.get("urgent").and_then(Value::as_bool),
                ) else {
                    return Effect::None;
                };
                match self.workspaces.iter_mut().find(|w| w.id == id) {
                    Some(w) if w.is_urgent != urgent => {
                        w.is_urgent = urgent;
                        Effect::Changed
                    }
                    _ => Effect::None,
                }
            }
            "WindowsChanged" => {
                let Some(w) = body.get("windows").and_then(parse::<Vec<NWindow>>) else {
                    return Effect::None;
                };
                self.windows = w;
                Effect::Changed
            }
            "WindowOpenedOrChanged" => {
                let Some(win) = body.get("window").and_then(parse::<NWindow>) else {
                    return Effect::None;
                };
                if win.is_focused {
                    for w in &mut self.windows {
                        w.is_focused = false;
                    }
                }
                match self.windows.iter_mut().find(|w| w.id == win.id) {
                    Some(w) => *w = win,
                    None => self.windows.push(win),
                }
                Effect::Changed
            }
            "WindowClosed" => {
                let Some(id) = body.get("id").and_then(Value::as_u64) else {
                    return Effect::None;
                };
                let before = self.windows.len();
                self.windows.retain(|w| w.id != id);
                if self.windows.len() == before {
                    Effect::None
                } else {
                    Effect::Changed
                }
            }
            "WindowFocusChanged" => {
                let id = body.get("id").and_then(Value::as_u64);
                for w in &mut self.windows {
                    w.is_focused = Some(w.id) == id;
                }
                Effect::Changed
            }
            "WindowUrgencyChanged" => {
                let (Some(id), Some(urgent)) = (
                    body.get("id").and_then(Value::as_u64),
                    body.get("urgent").and_then(Value::as_bool),
                ) else {
                    return Effect::None;
                };
                match self.windows.iter_mut().find(|w| w.id == id) {
                    Some(w) if w.is_urgent != urgent => {
                        w.is_urgent = urgent;
                        Effect::Changed
                    }
                    _ => Effect::None,
                }
            }
            "ConfigLoaded" => {
                let failed = body.get("failed").and_then(Value::as_bool).unwrap_or(false);
                if std::mem::replace(&mut self.seen_config, true) {
                    Effect::Reloaded(failed)
                } else {
                    Effect::None
                }
            }
            // Layouts, keyboard layouts, overview, casts, screenshots and
            // whatever later versions add.
            _ => Effect::None,
        }
    }

    pub(crate) fn snapshot(&self) -> IpcSnapshot {
        let mut workspaces: Vec<&NWorkspace> = self.workspaces.iter().collect();
        // Per output, top to bottom, as niri's own bar order.
        workspaces.sort_by(|a, b| (&a.output, a.idx).cmp(&(&b.output, b.idx)));
        let workspaces = workspaces
            .into_iter()
            .map(|w| Workspace {
                id: w.id as i64,
                name: w.name.clone().unwrap_or_else(|| w.idx.to_string()),
                focused: w.is_focused,
                active: w.is_active,
                urgent: w.is_urgent,
                screen: w.output.clone().unwrap_or_default(),
                ..Default::default()
            })
            .collect();
        // By id, which niri hands out in increasing order: the order the
        // windows opened. The "Windows" reply lists them in niri's own
        // map order (captured: 4, 2, 3), the events in arrival order, so
        // without this a fresh read (boot, reconnect) and a running stream
        // disagree, and a reconnect reorders `windows.all`.
        let mut windows: Vec<&NWindow> = self.windows.iter().collect();
        windows.sort_by_key(|w| w.id);
        let windows = windows
            .into_iter()
            .map(|w| Window {
                id: w.id.to_string(),
                title: w.title.clone().unwrap_or_default(),
                app_id: w.app_id.clone().unwrap_or_default(),
                workspace: w.workspace_id.map(|i| i as i64),
                focused: w.is_focused,
                urgent: w.is_urgent,
                ..Default::default()
            })
            .collect();
        IpcSnapshot {
            state: WmState {
                name: "niri".into(),
                workspaces,
                windows,
                focused_screen: self.focused_output.clone(),
            },
            toplevel_ids: Vec::new(),
        }
    }

    /// The `Action` request for an action.
    pub(crate) fn action_for(&self, action: &WmAction) -> Result<Value, WmError> {
        let window = |id: &str| -> Result<u64, WmError> {
            id.parse::<u64>()
                .ok()
                .filter(|n| self.windows.iter().any(|w| w.id == *n))
                .ok_or_else(|| WmError::UnknownWindow(id.to_string()))
        };
        Ok(match action {
            WmAction::FocusWorkspace(id) => {
                if !self.workspaces.iter().any(|w| w.id as i64 == *id) {
                    return Err(WmError::UnknownWorkspace(*id));
                }
                json!({"Action": {"FocusWorkspace": {"reference": {"Id": id}}}})
            }
            WmAction::FocusWindow(id) => {
                json!({"Action": {"FocusWindow": {"id": window(id)?}}})
            }
            WmAction::CloseWindow(id) => {
                json!({"Action": {"CloseWindow": {"id": window(id)?}}})
            }
            WmAction::MinimizeWindow(_) => {
                return Err(WmError::Unsupported("niri has no minimise"));
            }
        })
    }
}

/// A connection: one request, then its reply (or, after `EventStream`,
/// the events).
struct Conn {
    reader: BufReader<OwnedReadHalf>,
    buf: Vec<u8>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Conn {
    async fn connect(path: &Path) -> io::Result<Self> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            buf: Vec::new(),
            write: w,
        })
    }

    /// The next line (bounded, decoded lossily); cancel safe.
    async fn next_line(&mut self) -> io::Result<Option<String>> {
        Ok(next_line(&mut self.reader, &mut self.buf)
            .await?
            .map(|l| String::from_utf8_lossy(&l).into_owned()))
    }

    /// Sends one request; the reply's `Ok` value, or its `Err` text.
    async fn send(&mut self, request: &Value) -> io::Result<Result<Value, String>> {
        let run = async {
            let mut line = request.to_string();
            line.push('\n');
            self.write.write_all(line.as_bytes()).await?;
            let reply = self
                .next_line()
                .await?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "niri closed"))?;
            decode_reply(&reply)
        };
        tokio::time::timeout(REQUEST_TIMEOUT, run)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "niri did not answer"))?
    }
}

/// One reply line: its `Ok` value, or its `Err` text (niri 26.04 answers
/// a request it cannot parse `{"Err":"error parsing request"}`).
fn decode_reply(line: &str) -> io::Result<Result<Value, String>> {
    let reply: Value =
        serde_json::from_str(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if let Some(ok) = reply.get("Ok") {
        Ok(Ok(ok.clone()))
    } else if let Some(err) = reply.get("Err") {
        Ok(Err(err.as_str().unwrap_or("error").to_string()))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "niri reply is neither Ok nor Err",
        ))
    }
}

/// One request on a connection of its own; the reply's `Ok` value or its
/// `Err` text.
async fn request(socket: &Path, request: &Value) -> io::Result<Result<Value, String>> {
    let connect = tokio::time::timeout(REQUEST_TIMEOUT, Conn::connect(socket));
    let mut conn = connect
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "niri did not accept"))??;
    conn.send(request).await
}

/// A query (`"Workspaces"`, …): the value under its name in the reply.
async fn get(socket: &Path, name: &str) -> io::Result<Value> {
    match request(socket, &json!(name)).await? {
        Ok(v) => Ok(v.get(name).cloned().unwrap_or(Value::Null)),
        Err(e) => Err(io::Error::other(format!("{name}: {e}"))),
    }
}

pub(crate) async fn run(
    socket: PathBuf,
    tx: UnboundedSender<AdapterMsg>,
    mut cmds: UnboundedReceiver<Cmd>,
) {
    let mut backoff = Backoff::new();
    loop {
        match session(&socket, &tx, &mut cmds, &mut backoff).await {
            Ok(()) => return,
            Err(e) => log::warn!("niri IPC: {e}; reconnecting"),
        }
        if tx.send(AdapterMsg::Connected(false)).is_err() {
            return;
        }
        backoff.wait(&mut cmds).await;
    }
}

async fn session(
    socket: &Path,
    tx: &UnboundedSender<AdapterMsg>,
    cmds: &mut UnboundedReceiver<Cmd>,
    backoff: &mut Backoff,
) -> io::Result<()> {
    // The event stream first, so nothing between the reads and it is lost;
    // its first events restate everything anyway.
    let mut events = Conn::connect(socket).await?;
    if let Err(e) = events.send(&json!("EventStream")).await? {
        return Err(io::Error::other(format!("EventStream: {e}")));
    }
    let workspaces = get(socket, "Workspaces").await?;
    let windows = get(socket, "Windows").await?;
    let focused_output = get(socket, "FocusedOutput").await?;
    let mut state = State::from_replies(&workspaces, &windows, &focused_output);
    backoff.connected();
    if tx.send(AdapterMsg::Connected(true)).is_err()
        || tx.send(AdapterMsg::State(state.snapshot())).is_err()
    {
        return Ok(());
    }
    let mut cmds_open = true;
    loop {
        tokio::select! {
            line = events.next_line() => {
                let Some(line) = line? else {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "event stream closed"));
                };
                let mut changed = false;
                let mut reloads = Vec::new();
                let mut handle = |line: &str| match serde_json::from_str::<Value>(line) {
                    Ok(v) => match state.apply(&v) {
                        Effect::None => {}
                        Effect::Changed => changed = true,
                        Effect::Reloaded(f) => reloads.push(f),
                    },
                    Err(e) => log::warn!("niri IPC: bad event: {e}"),
                };
                handle(&line);
                while let Some(next) = futures_lite::future::poll_once(events.next_line()).await {
                    match next? {
                        Some(l) => handle(&l),
                        None => break,
                    }
                }
                if changed && tx.send(AdapterMsg::State(state.snapshot())).is_err() {
                    return Ok(());
                }
                for failed in reloads {
                    if tx.send(AdapterMsg::Reloaded { failed: Some(failed) }).is_err() {
                        return Ok(());
                    }
                }
            }
            cmd = cmds.recv(), if cmds_open => match cmd {
                Some((action, reply)) => {
                    let result = match state.action_for(&action) {
                        Ok(req) => match request(socket, &req).await {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(e)) => Err(WmError::Rejected(e)),
                            Err(e) => Err(WmError::Io(e.to_string())),
                        },
                        Err(e) => Err(e),
                    };
                    if let Some(r) = reply {
                        let _ = r.send(result);
                    }
                }
                None => cmds_open = false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_update_the_state() {
        let mut s = State::default();
        let ev = |s: &mut State, j: &str| s.apply(&serde_json::from_str(j).unwrap());
        assert_eq!(
            ev(
                &mut s,
                r#"{"WorkspacesChanged":{"workspaces":[
                {"id":1,"idx":1,"name":null,"output":"DP-1","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":null},
                {"id":2,"idx":2,"name":"web","output":"DP-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}]}}"#
            ),
            Effect::Changed
        );
        assert_eq!(
            ev(&mut s, r#"{"ConfigLoaded":{"failed":false}}"#),
            Effect::None
        );
        assert_eq!(
            ev(&mut s, r#"{"ConfigLoaded":{"failed":true}}"#),
            Effect::Reloaded(true)
        );
        assert_eq!(
            ev(&mut s, r#"{"WorkspaceActivated":{"id":2,"focused":true}}"#),
            Effect::Changed
        );
        let snap = s.snapshot().state;
        assert_eq!(snap.workspaces[0].name, "1");
        assert_eq!(snap.workspaces[1].name, "web");
        assert!(snap.workspaces[1].focused && snap.workspaces[1].active);
        assert!(!snap.workspaces[0].active);
        assert_eq!(ev(&mut s, r#"{"SomethingNew":{"x":1}}"#), Effect::None);
        assert_eq!(
            ev(
                &mut s,
                r#"{"WindowOpenedOrChanged":{"window":{"id":7,"title":"t","app_id":"foot","pid":1,"workspace_id":2,"is_focused":true,"is_floating":false,"is_urgent":false,"layout":{},"brand_new_field":3}}}"#
            ),
            Effect::Changed
        );
        assert!(s.snapshot().state.windows[0].focused);
        assert_eq!(
            ev(&mut s, r#"{"WindowFocusChanged":{"id":null}}"#),
            Effect::Changed
        );
        assert!(!s.snapshot().state.windows[0].focused);
        assert_eq!(ev(&mut s, r#"{"WindowClosed":{"id":7}}"#), Effect::Changed);
        assert!(s.snapshot().state.windows.is_empty());
    }

    #[test]
    fn actions_are_niri_requests() {
        let mut s = State::default();
        s.workspaces.push(NWorkspace {
            id: 4,
            ..Default::default()
        });
        assert_eq!(
            s.action_for(&WmAction::FocusWorkspace(4))
                .unwrap()
                .to_string(),
            r#"{"Action":{"FocusWorkspace":{"reference":{"Id":4}}}}"#
        );
        assert_eq!(
            s.action_for(&WmAction::FocusWindow("3".into())),
            Err(WmError::UnknownWindow("3".into()))
        );
    }

    // ---- traffic captured from a real niri 26.04 -------------------------

    const CAPTURED: &str = "niri-26.04-captured";

    /// The state a fresh connection reads at checkpoint `dir`, as
    /// `session` builds it from the three replies.
    fn from_captured_replies(dir: &str) -> State {
        let reply = |file: &str, name: &str| -> Value {
            let line = crate::wm::captured::fixture(&format!("{CAPTURED}/{dir}/{file}"));
            let ok = decode_reply(line.trim_end()).unwrap().unwrap();
            ok.get(name).cloned().unwrap_or(Value::Null)
        };
        State::from_replies(
            &reply("reply-workspaces.json", "Workspaces"),
            &reply("reply-windows.json", "Windows"),
            &reply("reply-focused-output.json", "FocusedOutput"),
        )
    }

    /// The captured event stream, replayed burst by burst, keeps the same
    /// state a fresh read of the replies captured at each checkpoint
    /// gives; and the reloads are what niri said they were.
    #[test]
    fn captured_events_agree_with_captured_replies() {
        let text = crate::wm::captured::fixture(&format!("{CAPTURED}/events.txt"));
        let bursts = crate::wm::captured::bursts(&text);
        let names: Vec<&str> = bursts.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names[0], "stream-start");
        let mut s = State::default();
        let mut checked = Vec::new();
        for (name, lines) in &bursts {
            let mut lines = lines.iter().map(String::as_str);
            if name == "stream-start" {
                // The reply to "EventStream" (read by `Conn::send`), then
                // the replicated state.
                assert_eq!(
                    decode_reply(lines.next().unwrap()).unwrap(),
                    Ok(json!("Handled"))
                );
            }
            let mut effects = Vec::new();
            for line in lines {
                let v: Value = serde_json::from_str(line)
                    .unwrap_or_else(|e| panic!("{name}: not JSON ({e}): {line}"));
                effects.push(s.apply(&v));
            }
            let reloads: Vec<&Effect> = effects
                .iter()
                .filter(|e| matches!(e, Effect::Reloaded(_)))
                .collect();
            match name.as_str() {
                "reload" | "reload-fixed" => assert_eq!(reloads, [&Effect::Reloaded(false)]),
                "reload-failed" => assert_eq!(reloads, [&Effect::Reloaded(true)]),
                _ => assert!(reloads.is_empty(), "{name}: {effects:?}"),
            }
            let checkpoint = match name.as_str() {
                "stream-start" => Some("boot"),
                "open-titled" => Some("opened"),
                "move-term-to-2" => Some("moved"),
                "close-focused" => Some("closed"),
                "close-rest" => Some("end"),
                _ => None,
            };
            if let Some(dir) = checkpoint {
                assert_eq!(
                    s.snapshot(),
                    from_captured_replies(dir).snapshot(),
                    "after `{name}`: the events and the replies in {dir}/"
                );
                checked.push(dir);
            }
            // What the bar shows, burst by burst.
            let snap = s.snapshot().state;
            let ws: Vec<&str> = snap.workspaces.iter().map(|w| w.name.as_str()).collect();
            let focused_ws = snap.workspaces.iter().find(|w| w.focused).map(|w| w.id);
            let focused_win = snap
                .windows
                .iter()
                .find(|w| w.focused)
                .map(|w| (w.app_id.as_str(), w.title.as_str()));
            match name.as_str() {
                "stream-start" => {
                    // Unordered in the stream; "chat" is named in the
                    // config, so niri puts it first.
                    assert_eq!(ws, ["chat", "2"]);
                    assert_eq!(focused_ws, Some(1));
                    assert_eq!(snap.focused_screen.as_deref(), Some("winit"));
                }
                "open-titled" => {
                    assert_eq!(focused_win, Some(("titled", "second, with a comma")));
                    assert_eq!(snap.windows.len(), 3);
                }
                "focus-term" => assert_eq!(focused_win, Some(("term", "~"))),
                "move-term-to-2" => {
                    assert_eq!(ws, ["chat", "2", "3"]);
                    assert_eq!(focused_ws, Some(2));
                    assert_eq!(focused_win, Some(("term", "~")));
                    let term = snap.windows.iter().find(|w| w.app_id == "term").unwrap();
                    assert_eq!(term.workspace, Some(2));
                }
                "focus-chat" => {
                    assert_eq!(focused_ws, Some(1));
                    assert_eq!(focused_win, Some(("editor", "notes.txt")));
                }
                "focus-empty" => {
                    assert_eq!(focused_ws, Some(3));
                    assert_eq!(focused_win, None);
                }
                "close-focused" => {
                    assert_eq!(focused_win, Some(("titled", "second, with a comma")));
                    assert!(!snap.windows.iter().any(|w| w.app_id == "editor"));
                }
                "close-rest" => {
                    assert_eq!(ws, ["chat", "2"]);
                    assert!(snap.windows.is_empty());
                }
                _ => {}
            }
        }
        assert_eq!(checked, ["boot", "opened", "moved", "closed", "end"]);
    }

    /// niri's replies to requests that change nothing: an action is
    /// `Handled` even for a workspace that does not exist (which is why
    /// `action_for` checks the id first), and a request niri cannot parse
    /// is an `Err`.
    #[test]
    fn captured_action_replies() {
        let reply = |f: &str| {
            let line = crate::wm::captured::fixture(&format!("{CAPTURED}/{f}"));
            decode_reply(line.trim_end()).unwrap()
        };
        assert_eq!(reply("action-ok.json"), Ok(json!("Handled")));
        assert_eq!(reply("action-err.json"), Ok(json!("Handled")));
        assert_eq!(
            reply("request-unknown.json"),
            Err("error parsing request".to_string())
        );
        let s = from_captured_replies("moved");
        assert_eq!(
            s.action_for(&WmAction::FocusWorkspace(99)),
            Err(WmError::UnknownWorkspace(99))
        );
        assert!(s.action_for(&WmAction::FocusWorkspace(3)).is_ok());
    }
}

//! niri's IPC, our own implementation (the `niri-ipc` crate is GPL-3.0,
//! and niri adds fields and events in patch versions, so this one parses
//! leniently and sits behind the `niri` feature).
//!
//! JSON lines over `$NIRI_SOCKET`: a request (`"Workspaces"`, `"Windows"`,
//! `"FocusedOutput"`, `{"Action":{…}}`) gets one reply (`{"Ok":…}` or
//! `{"Err":"…"}`); after `"EventStream"` the socket only carries events
//! (`{"WorkspacesChanged":{…}}`, …). Formats are niri 26.04's
//! (`niri-ipc/src/lib.rs`). Unknown events and fields are ignored.
//! A known event whose data has another shape is followed by a re-read;
//! a reply the adapter cannot read (or an `Err` to a query), eight lines
//! in a row that are no JSON, or an action niri cannot parse (`error
//! parsing request`) degrade the adapter (`understood`), naming niri's
//! version (`"Version"`).
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
use super::understood::{self, Refused, SessionEnd, Strikes};
use super::{AdapterMsg, Cmd, IpcSnapshot, WmAction, WmError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

// The fields the adapter needs are required (ids, a workspace's index):
// a reply or event without one is not understood, instead of read as
// zero (`understood`). The rest default, and unknown fields are ignored.

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct NWorkspace {
    pub id: u64,
    pub idx: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub is_urgent: bool,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_focused: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct NWindow {
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<u64>,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub is_urgent: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct NOutput {
    name: String,
}

/// What one event did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    None,
    Changed,
    /// An event the adapter knows whose data it cannot read (niri changed
    /// its shape): the state must be read again.
    Requery,
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

/// The value under `name` in a reply's `Ok`, as `T`, or what was not
/// understood.
fn parse_reply<T: for<'de> Deserialize<'de>>(name: &str, v: &Value) -> Result<T, String> {
    T::deserialize(v).map_err(|e| {
        format!(
            "the reply to {name} ({e}: `{}`)",
            understood::excerpt(&v.to_string())
        )
    })
}

impl State {
    /// The state the replies to `"Workspaces"`, `"Windows"` and
    /// `"FocusedOutput"` report (each the value under its name in the
    /// reply's `Ok`; `null` when niri has no focused output), or what in
    /// them was not understood.
    pub(crate) fn from_replies(
        workspaces: &Value,
        windows: &Value,
        focused_output: &Value,
    ) -> Result<Self, String> {
        Ok(Self {
            workspaces: parse_reply("Workspaces", workspaces)?,
            windows: parse_reply("Windows", windows)?,
            focused_output: parse_reply::<Option<NOutput>>("FocusedOutput", focused_output)?
                .map(|o| o.name),
            seen_config: false,
        })
    }

    /// The state read again (`from_replies`), keeping what only the
    /// stream tells.
    fn reread(&mut self, fresh: Self) {
        let seen_config = self.seen_config;
        *self = fresh;
        self.seen_config = seen_config;
    }

    /// Applies one event line's JSON (`{"Name": {fields}}`).
    pub(crate) fn apply(&mut self, event: &Value) -> Effect {
        let Some((name, body)) = event.as_object().and_then(|o| o.iter().next()) else {
            return Effect::None;
        };
        match name.as_str() {
            "WorkspacesChanged" => {
                let Some(ws) = body.get("workspaces").and_then(parse::<Vec<NWorkspace>>) else {
                    return Effect::Requery;
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
                let Some(id) = id else {
                    return Effect::Requery;
                };
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
                    return Effect::Requery;
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
                    return Effect::Requery;
                };
                self.windows = w;
                Effect::Changed
            }
            "WindowOpenedOrChanged" => {
                let Some(win) = body.get("window").and_then(parse::<NWindow>) else {
                    return Effect::Requery;
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
                    return Effect::Requery;
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
                // `null`: no window has the focus.
                let id = match body.get("id") {
                    Some(Value::Null) => None,
                    Some(v) => match v.as_u64() {
                        Some(n) => Some(n),
                        None => return Effect::Requery,
                    },
                    None => return Effect::Requery,
                };
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
                    return Effect::Requery;
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
            // niri's `Window` says neither maximized nor fullscreen: they
            // come from the wlr protocol (`wm::wlr_window_states`).
            window_states: false,
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
            // niri's own toggles, by window id. `MaximizeWindowToEdges` is
            // the window's true maximize (the xdg `maximized` state);
            // `MaximizeColumn` acts on the focused column, not a window.
            WmAction::MaximizeWindow(id) => {
                json!({"Action": {"MaximizeWindowToEdges": {"id": window(id)?}}})
            }
            WmAction::FullscreenWindow(id) => {
                json!({"Action": {"FullscreenWindow": {"id": window(id)?}}})
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

    /// Sends one request; the reply's `Ok` value, or its `Err` text. A
    /// reply that is neither is not understood.
    async fn send(&mut self, request: &Value) -> Result<Result<Value, String>, SessionEnd> {
        let run = async {
            let mut line = request.to_string();
            line.push('\n');
            self.write.write_all(line.as_bytes()).await?;
            let reply = self
                .next_line()
                .await?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "niri closed"))?;
            decode_reply(&reply)
                .map_err(|e| SessionEnd::reply(format!("the reply to {} ({e})", short(request))))
        };
        tokio::time::timeout(REQUEST_TIMEOUT, run)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "niri did not answer"))?
    }
}

/// A request's name for a diagnostic: `"Windows"` is `Windows`, an action
/// `{"Action":{"FocusWindow":…}}` is `Action FocusWindow`.
fn short(request: &Value) -> String {
    match request {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .iter()
            .next()
            .map(|(k, v)| match v.as_object().and_then(|i| i.keys().next()) {
                Some(inner) => format!("{k} {inner}"),
                None => k.clone(),
            })
            .unwrap_or_default(),
        other => other.to_string(),
    }
}

/// One reply line: its `Ok` value, or its `Err` text (niri 26.04 answers
/// a request it cannot parse `{"Err":"error parsing request"}`); `Err`
/// says why a line is neither.
fn decode_reply(line: &str) -> Result<Result<Value, String>, String> {
    let reply: Value =
        serde_json::from_str(line).map_err(|e| format!("{e}: `{}`", understood::excerpt(line)))?;
    if let Some(ok) = reply.get("Ok") {
        Ok(Ok(ok.clone()))
    } else if let Some(err) = reply.get("Err") {
        Ok(Err(err.as_str().unwrap_or("error").to_string()))
    } else {
        Err(format!(
            "neither Ok nor Err: `{}`",
            understood::excerpt(line)
        ))
    }
}

/// Whether niri's `Err` says it could not parse the request: the syntax
/// the adapter writes is not niri's (any more).
fn refuses_syntax(err: &str) -> bool {
    err.starts_with("error parsing request")
}

/// One request on a connection of its own; the reply's `Ok` value or its
/// `Err` text.
async fn request(socket: &Path, request: &Value) -> Result<Result<Value, String>, SessionEnd> {
    let connect = tokio::time::timeout(REQUEST_TIMEOUT, Conn::connect(socket));
    let mut conn = connect
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "niri did not accept"))??;
    conn.send(request).await
}

/// A query (`"Workspaces"`, …): the value under its name in the reply. An
/// `Err` is not understood: niri cannot parse a request it has always
/// answered.
async fn get(socket: &Path, name: &str) -> Result<Value, SessionEnd> {
    match request(socket, &json!(name)).await? {
        Ok(v) => Ok(v.get(name).cloned().unwrap_or(Value::Null)),
        Err(e) => Err(SessionEnd::reply(format!(
            "the request {name} (niri answered `{}`)",
            understood::excerpt(&e)
        ))),
    }
}

/// Reads the whole state.
async fn query(socket: &Path) -> Result<State, SessionEnd> {
    let workspaces = get(socket, "Workspaces").await?;
    let windows = get(socket, "Windows").await?;
    let focused_output = get(socket, "FocusedOutput").await?;
    State::from_replies(&workspaces, &windows, &focused_output).map_err(SessionEnd::reply)
}

/// The version niri reports (`"Version"`), for a diagnostic.
async fn version(socket: &Path) -> Option<String> {
    match request(socket, &json!("Version")).await {
        Ok(Ok(v)) => v.get("Version")?.as_str().map(str::to_string),
        _ => None,
    }
}

/// The adapter: connects, reads, follows events; reconnects with backoff,
/// and degrades (`understood`) while niri is not understood.
pub(crate) async fn run(
    socket: PathBuf,
    tx: UnboundedSender<AdapterMsg>,
    mut cmds: UnboundedReceiver<Cmd>,
) {
    let mut backoff = Backoff::new();
    let mut refused = None;
    loop {
        let msg = match session(&socket, &tx, &mut cmds, &mut backoff, &mut refused).await {
            Ok(()) => return,
            Err(SessionEnd::Io(e)) => {
                log::warn!("niri IPC: {e}; reconnecting");
                AdapterMsg::Connected(false)
            }
            Err(SessionEnd::NotUnderstood(n)) => {
                let v = version(&socket).await;
                understood::degraded("niri", n, v, &mut refused)
            }
        };
        if tx.send(msg).is_err() {
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
    refused: &mut Option<Refused>,
) -> Result<(), SessionEnd> {
    // The event stream first, so nothing between the reads and it is lost;
    // its first events restate everything anyway.
    let mut events = Conn::connect(socket).await?;
    if let Err(e) = events.send(&json!("EventStream")).await? {
        return Err(SessionEnd::reply(format!(
            "the request EventStream (niri answered `{}`)",
            understood::excerpt(&e)
        )));
    }
    let mut state = query(socket).await?;
    // Actions this niri refused stay refused until it reports another
    // version.
    if let Some(r) = refused.as_ref() {
        if r.holds_for(version(socket).await.as_deref()) {
            return Err(SessionEnd::actions(r.what.clone()));
        }
        *refused = None;
    }
    backoff.connected();
    if tx.send(AdapterMsg::Connected(true)).is_err()
        || tx.send(AdapterMsg::State(state.snapshot())).is_err()
    {
        return Ok(());
    }
    let mut cmds_open = true;
    let mut strikes = Strikes::default();
    loop {
        tokio::select! {
            line = events.next_line() => {
                let Some(line) = line? else {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "event stream closed").into());
                };
                let mut changed = false;
                let mut requery = false;
                let mut reloads = Vec::new();
                let mut handle = |line: &str| -> Result<(), SessionEnd> {
                    match serde_json::from_str::<Value>(line) {
                        Ok(v) => {
                            strikes.event();
                            match state.apply(&v) {
                                Effect::None => {}
                                Effect::Changed => changed = true,
                                Effect::Requery => requery = true,
                                Effect::Reloaded(f) => reloads.push(f),
                            }
                        }
                        // Not JSON: no event at all (`understood`). Read
                        // again, and count it.
                        Err(e) => {
                            log::warn!("niri IPC: bad event: {e}");
                            strikes.garbage("niri", line.as_bytes())?;
                            requery = true;
                        }
                    }
                    Ok(())
                };
                handle(&line)?;
                while let Some(next) = futures_lite::future::poll_once(events.next_line()).await {
                    match next? {
                        Some(l) => handle(&l)?,
                        None => break,
                    }
                }
                if requery {
                    state.reread(query(socket).await?);
                }
                if (changed || requery) && tx.send(AdapterMsg::State(state.snapshot())).is_err() {
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
                    let mut refused_syntax = None;
                    let result = match state.action_for(&action) {
                        Ok(req) => match request(socket, &req).await {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(e)) => {
                                if refuses_syntax(&e) {
                                    refused_syntax = Some(format!(
                                        "the action syntax (`{}` answered `{}`)",
                                        understood::excerpt(&req.to_string()),
                                        understood::excerpt(&e)
                                    ));
                                }
                                Err(WmError::Rejected(e))
                            }
                            Err(SessionEnd::Io(e)) => Err(WmError::Io(e.to_string())),
                            Err(end @ SessionEnd::NotUnderstood(_)) => {
                                if let Some(r) = reply {
                                    let _ = r.send(Err(WmError::Io(end.to_string())));
                                }
                                return Err(end);
                            }
                        },
                        Err(e) => Err(e),
                    };
                    if let Some(r) = reply {
                        let _ = r.send(result);
                    }
                    if let Some(what) = refused_syntax {
                        return Err(SessionEnd::actions(what));
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

    /// An event the adapter knows whose data has another shape asks for a
    /// re-read; one it does not know changes nothing; a reply of another
    /// shape is not understood.
    #[test]
    fn shapes_niri_changed_are_reread_or_not_understood() {
        let mut s = State::default();
        let ev = |s: &mut State, j: &str| s.apply(&serde_json::from_str(j).unwrap());
        for changed in [
            r#"{"WorkspacesChanged":{"spaces":[]}}"#,
            r#"{"WorkspacesChanged":{"workspaces":[{"idx":1}]}}"#,
            r#"{"WindowsChanged":{"windows":[{"id":"7"}]}}"#,
            r#"{"WindowOpenedOrChanged":{"win":{}}}"#,
            r#"{"WindowClosed":{"window_id":7}}"#,
            r#"{"WindowFocusChanged":{}}"#,
            r#"{"WindowFocusChanged":{"id":"7"}}"#,
            r#"{"WorkspaceActivated":{"focused":true}}"#,
            r#"{"WindowUrgencyChanged":{"id":7}}"#,
        ] {
            assert_eq!(ev(&mut s, changed), Effect::Requery, "{changed}");
        }
        assert_eq!(
            ev(&mut s, r#"{"WindowFocusChanged":{"id":null}}"#),
            Effect::Changed,
            "null is no focus"
        );
        assert_eq!(ev(&mut s, r#"{"BrandNewEvent":{"id":1}}"#), Effect::None);
        let e = State::from_replies(&json!([{"idx": 1}]), &json!([]), &Value::Null).unwrap_err();
        assert!(
            e.starts_with("the reply to Workspaces (missing field `id`"),
            "{e}"
        );
        assert!(State::from_replies(&json!([]), &json!({"windows": []}), &Value::Null).is_err());
        assert!(State::from_replies(&json!([]), &json!([]), &Value::Null).is_ok());
        assert!(
            decode_reply("not json")
                .unwrap_err()
                .ends_with(": `not json`")
        );
        assert!(decode_reply(r#"{"Okay":1}"#).is_err());
        assert!(refuses_syntax("error parsing request"));
        assert!(!refuses_syntax("no such window"));
        assert_eq!(short(&json!("Windows")), "Windows");
        assert_eq!(
            short(&json!({"Action": {"FocusWindow": {"id": 1}}})),
            "Action FocusWindow"
        );
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
        // `win.maximize()` is the window's own maximize (to the edges),
        // not `MaximizeColumn` (the focused column); both are niri's
        // toggles, by window id.
        s.windows.push(NWindow {
            id: 3,
            ..Default::default()
        });
        assert_eq!(
            s.action_for(&WmAction::MaximizeWindow("3".into()))
                .unwrap()
                .to_string(),
            r#"{"Action":{"MaximizeWindowToEdges":{"id":3}}}"#
        );
        assert_eq!(
            s.action_for(&WmAction::FullscreenWindow("3".into()))
                .unwrap()
                .to_string(),
            r#"{"Action":{"FullscreenWindow":{"id":3}}}"#
        );
        assert_eq!(
            s.action_for(&WmAction::FullscreenWindow("4".into())),
            Err(WmError::UnknownWindow("4".into()))
        );
        assert!(!s.snapshot().window_states, "niri's IPC has neither state");
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
        .unwrap()
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

//! Hyprland's IPC, our own implementation (the `hyprland` crate is
//! GPL-3.0).
//!
//! `.socket.sock` answers one request per connection (`j/workspaces`,
//! `j/monitors`, `j/clients`, `j/activewindow`, `dispatch …`) and closes;
//! `.socket2.sock` streams `EVENT>>DATA` lines. Formats are Hyprland
//! 0.56.2's (`src/debug/HyprCtl.cpp`, the wiki's IPC page).
//!
//! Events that carry what changed (`workspacev2`, `focusedmonv2`,
//! `activewindowv2`, `windowtitlev2`, `urgent`, `minimized`) patch the state
//! in place; the rest (`openwindow`, `closewindow`, `movewindowv2`,
//! workspace and monitor changes, `fullscreen`, `configreloaded`) re-read
//! it once per burst.
//!
//! Hyprland copies window titles as raw bytes (an XWayland `WM_NAME` of
//! type `STRING` is Latin-1), into events and into JSON replies alike, so
//! both are decoded lossily: a title that is not UTF-8 shows U+FFFD where
//! its bad bytes were, instead of costing the connection.
//!
//! Event data never holds a newline: `EventManager::formatEvent` turns
//! each `\n` into a space, while `j/clients` escapes it. Titles from
//! `j/clients` get the same mapping, so a re-read after a
//! `windowtitlev2` does not change a title back (a spurious `Update`).
//!
//! Actions are `dispatch` requests in one of two dialects. Before 0.55
//! (hyprlang configs) a dispatcher and its argument (`dispatch workspace
//! 3`); with a Lua config (0.55 on, the only kind from 0.56) the request's
//! argument is evaluated as `return hl.dispatch(<argument>)`, so it must
//! be a dispatcher object (`dispatch hl.dsp.focus({ workspace = "3" })`)
//! and the classic form is answered `error: [string "return
//! hl.dispatch(workspace 3)"]:1: ')' expected near '3'`. The adapter sends
//! the classic form until Hyprland answers with that Lua error, then the
//! Lua form for the rest of the connection (decisions.md, wave4-exit-ci).
//!
//! `j/clients` reports each window's `stableId`, the same `{:x}` string
//! Hyprland sends as its `ext-foreign-toplevel-list-v1` identifier
//! (`src/protocols/ForeignToplevel.cpp`), so windows join the protocol.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::backoff::Backoff;
use super::lines::{MAX_MESSAGE, next_line, too_long};
use super::model::{Window, WmState, Workspace};
use super::{AdapterMsg, Cmd, IpcSnapshot, WmAction, WmError};

/// How long one request may take; Hyprland answers synchronously.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Hyprland cuts socket2 event data at 1024 bytes
/// (`src/managers/EventManager.cpp`, `data.substr(0, 1024)`).
const EVENT_DATA_CAP: usize = 1024;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct WsRef {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct Monitor {
    pub id: i64,
    pub name: String,
    pub focused: bool,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WsRef,
    pub disabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct HWorkspace {
    pub id: i64,
    pub name: String,
    pub monitor: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct Client {
    pub address: String,
    pub mapped: bool,
    pub workspace: WsRef,
    pub class: String,
    pub title: String,
    /// A bool before Hyprland 0.42, a mode number (0 = none) since.
    pub fullscreen: serde_json::Value,
    /// The `ext-foreign-toplevel-list-v1` identifier (`{:x}` of the
    /// window's stable id); empty before Hyprland reported it.
    #[serde(rename = "stableId")]
    pub stable_id: String,
}

impl Client {
    fn is_fullscreen(&self) -> bool {
        match &self.fullscreen {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
            _ => false,
        }
    }
}

/// What one event did to the state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Nothing the services show.
    None,
    /// The state was patched.
    Changed,
    /// The state must be read again.
    Requery,
    /// `configreloaded`: re-read too (rules may move workspaces).
    Reloaded,
}

/// Hyprland's state as its requests report it, plus what only events
/// tell (urgency, minimised).
#[derive(Clone, Debug, Default)]
pub(crate) struct State {
    pub monitors: Vec<Monitor>,
    pub workspaces: Vec<HWorkspace>,
    pub clients: Vec<Client>,
    /// The focused window's address (`0x…`).
    pub active: Option<String>,
    pub urgent: HashSet<String>,
    pub minimized: HashSet<String>,
}

/// Addresses come as `0x55d…` from requests and as `55d…` in events.
fn address(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("0x") {
        raw.to_string()
    } else {
        format!("0x{raw}")
    }
}

fn is_special(name: &str) -> bool {
    name.starts_with("special:") || name == "special"
}

impl State {
    /// Replaces what requests report, keeping event-only facts for the
    /// windows that still exist.
    pub(crate) fn replace(
        &mut self,
        monitors: Vec<Monitor>,
        workspaces: Vec<HWorkspace>,
        clients: Vec<Client>,
        active: Option<Client>,
    ) {
        self.monitors = monitors;
        self.workspaces = workspaces;
        self.clients = clients;
        for c in &mut self.clients {
            // As `windowtitlev2` sends it (see the module docs).
            if c.title.contains('\n') {
                c.title = c.title.replace('\n', " ");
            }
        }
        self.active = active
            .filter(|c| !c.address.is_empty())
            .map(|c| address(&c.address));
        let live: HashSet<&str> = self.clients.iter().map(|c| c.address.as_str()).collect();
        self.urgent.retain(|a| live.contains(a.as_str()));
        self.minimized.retain(|a| live.contains(a.as_str()));
    }

    /// Applies one raw socket2 line (`EVENT>>DATA`), decoding its data
    /// lossily. A `windowtitlev2` whose data reached Hyprland's cap may be
    /// cut short (even inside a UTF-8 sequence): the title is re-read from
    /// `j/clients`, which is not cut.
    pub(crate) fn apply_line(&mut self, line: &[u8]) -> Effect {
        let (event, data) = match line.windows(2).position(|w| w == b">>") {
            Some(i) => (&line[..i], &line[i + 2..]),
            None => (line, &[][..]),
        };
        let event = String::from_utf8_lossy(event);
        if event == "windowtitlev2" && data.len() >= EVENT_DATA_CAP {
            return Effect::Requery;
        }
        self.apply(&event, &String::from_utf8_lossy(data))
    }

    /// Applies one socket2 event.
    pub(crate) fn apply(&mut self, event: &str, data: &str) -> Effect {
        match event {
            "workspacev2" => {
                let Some((id, _name)) = data.split_once(',') else {
                    return Effect::Requery;
                };
                let Ok(id) = id.parse::<i64>() else {
                    return Effect::Requery;
                };
                let Some(ws) = self.workspaces.iter().find(|w| w.id == id) else {
                    return Effect::Requery;
                };
                let monitor = ws.monitor.clone();
                let mut changed = false;
                for m in &mut self.monitors {
                    let focused = m.name == monitor;
                    if focused && m.active_workspace.id != id {
                        m.active_workspace = WsRef {
                            id,
                            name: ws.name.clone(),
                        };
                        changed = true;
                    }
                    changed |= m.focused != focused;
                    m.focused = focused;
                }
                if changed {
                    Effect::Changed
                } else {
                    Effect::None
                }
            }
            "focusedmonv2" => {
                let Some((mon, id)) = data.split_once(',') else {
                    return Effect::Requery;
                };
                let Ok(id) = id.parse::<i64>() else {
                    return Effect::Requery;
                };
                if !self.monitors.iter().any(|m| m.name == mon)
                    || !self.workspaces.iter().any(|w| w.id == id)
                {
                    return Effect::Requery;
                }
                let name = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == id)
                    .map(|w| w.name.clone())
                    .unwrap_or_default();
                for m in &mut self.monitors {
                    m.focused = m.name == mon;
                    if m.focused {
                        m.active_workspace = WsRef {
                            id,
                            name: name.clone(),
                        };
                    }
                }
                Effect::Changed
            }
            "activewindowv2" => {
                let addr = data.trim();
                let active = (!addr.is_empty() && addr != ",").then(|| address(addr));
                if let Some(a) = &active {
                    self.urgent.remove(a);
                }
                if self.active == active {
                    return Effect::None;
                }
                self.active = active;
                Effect::Changed
            }
            "windowtitlev2" => {
                // The title may hold commas: split at the first only.
                let Some((addr, title)) = data.split_once(',') else {
                    return Effect::None;
                };
                let addr = address(addr);
                match self.clients.iter_mut().find(|c| c.address == addr) {
                    Some(c) if c.title != title => {
                        c.title = title.to_string();
                        Effect::Changed
                    }
                    Some(_) => Effect::None,
                    None => Effect::Requery,
                }
            }
            "urgent" => {
                let addr = address(data);
                if self.active.as_deref() == Some(addr.as_str()) {
                    return Effect::None;
                }
                if self.urgent.insert(addr) {
                    Effect::Changed
                } else {
                    Effect::None
                }
            }
            "minimized" => {
                let Some((addr, state)) = data.split_once(',') else {
                    return Effect::None;
                };
                let addr = address(addr);
                let changed = if state.trim() == "1" {
                    self.minimized.insert(addr)
                } else {
                    self.minimized.remove(&addr)
                };
                if changed {
                    Effect::Changed
                } else {
                    Effect::None
                }
            }
            "configreloaded" => Effect::Reloaded,
            // Structure changed: read it again (once per burst).
            "openwindow" | "closewindow" | "kill" | "movewindow" | "movewindowv2"
            | "createworkspace" | "createworkspacev2" | "destroyworkspace"
            | "destroyworkspacev2" | "moveworkspace" | "moveworkspacev2" | "renameworkspace"
            | "monitoradded" | "monitoraddedv2" | "monitorremoved" | "monitorremovedv2"
            | "fullscreen" => Effect::Requery,
            // `workspace`, `focusedmon`, `activewindow`, `windowtitle` come
            // with their v2 twins; layers, layouts, submaps and the rest
            // show nothing.
            _ => Effect::None,
        }
    }

    /// The typed state. Special workspaces (Hyprland's scratchpads) are
    /// left out; their windows count as minimised.
    pub(crate) fn snapshot(&self) -> IpcSnapshot {
        let focused_mon = self.monitors.iter().find(|m| m.focused);
        let mut workspaces: Vec<Workspace> = self
            .workspaces
            .iter()
            .filter(|w| !is_special(&w.name))
            .map(|w| Workspace {
                id: w.id,
                name: w.name.clone(),
                focused: focused_mon.is_some_and(|m| m.active_workspace.id == w.id),
                active: self
                    .monitors
                    .iter()
                    .any(|m| !m.disabled && m.active_workspace.id == w.id),
                screen: w.monitor.clone(),
                ..Default::default()
            })
            .collect();
        // Numbered workspaces by number, then named ones (negative ids
        // from -1337 down) in creation order.
        workspaces.sort_by_key(|w| (w.id < 0, w.id.unsigned_abs()));
        let mut toplevel_ids = Vec::new();
        let windows = self
            .clients
            .iter()
            .filter(|c| c.mapped)
            .map(|c| {
                if !c.stable_id.is_empty() {
                    toplevel_ids.push((c.address.clone(), c.stable_id.clone()));
                }
                let special = is_special(&c.workspace.name);
                Window {
                    id: c.address.clone(),
                    title: c.title.clone(),
                    app_id: c.class.clone(),
                    icon: String::new(),
                    workspace: (!special && c.workspace.id != -1).then_some(c.workspace.id),
                    focused: self.active.as_deref() == Some(c.address.as_str()),
                    minimized: special || self.minimized.contains(&c.address),
                    fullscreen: c.is_fullscreen(),
                    urgent: self.urgent.contains(&c.address),
                    // Set by `merge` from `toplevel_ids`.
                    toplevel: None,
                }
            })
            .collect();
        IpcSnapshot {
            state: WmState {
                name: "Hyprland".into(),
                workspaces,
                windows,
                focused_screen: focused_mon.map(|m| m.name.clone()),
            },
            toplevel_ids,
        }
    }

    /// The `dispatch` request for an action, in the classic dialect or,
    /// with `lua`, as a Lua dispatcher object (see the module docs).
    pub(crate) fn dispatch_for(&self, action: &WmAction, lua: bool) -> Result<String, WmError> {
        match action {
            WmAction::FocusWorkspace(id) => {
                let ws = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == *id)
                    .ok_or(WmError::UnknownWorkspace(*id))?;
                let target = if *id > 0 {
                    id.to_string()
                } else {
                    format!("name:{}", ws.name)
                };
                Ok(if lua {
                    format!(
                        "dispatch hl.dsp.focus({{ workspace = {} }})",
                        lua_string(&target)
                    )
                } else {
                    format!("dispatch workspace {target}")
                })
            }
            WmAction::FocusWindow(a) => {
                self.client(a)?;
                Ok(if lua {
                    format!(
                        "dispatch hl.dsp.focus({{ window = {} }})",
                        lua_string(&format!("address:{a}"))
                    )
                } else {
                    format!("dispatch focuswindow address:{a}")
                })
            }
            WmAction::CloseWindow(a) => {
                self.client(a)?;
                Ok(if lua {
                    format!(
                        "dispatch hl.dsp.window.close({{ window = {} }})",
                        lua_string(&format!("address:{a}"))
                    )
                } else {
                    format!("dispatch closewindow address:{a}")
                })
            }
            WmAction::MinimizeWindow(_) => Err(WmError::Unsupported(
                "Hyprland has no minimise; move the window to a special workspace",
            )),
        }
    }

    fn client(&self, address: &str) -> Result<&Client, WmError> {
        self.clients
            .iter()
            .find(|c| c.address == address)
            .ok_or_else(|| WmError::UnknownWindow(address.to_string()))
    }
}

/// One request on `.socket.sock`: Hyprland answers and closes.
pub(crate) async fn request(path: &Path, command: &str) -> io::Result<String> {
    let run = async {
        let mut s = UnixStream::connect(path).await?;
        s.write_all(command.as_bytes()).await?;
        let mut out = Vec::new();
        (&mut s)
            .take(MAX_MESSAGE as u64 + 1)
            .read_to_end(&mut out)
            .await?;
        if out.len() > MAX_MESSAGE {
            return Err(too_long());
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    };
    tokio::time::timeout(REQUEST_TIMEOUT, run)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Hyprland did not answer"))?
}

async fn request_json<T: for<'de> Deserialize<'de>>(path: &Path, command: &str) -> io::Result<T> {
    let text = request(path, command).await?;
    parse_reply(command, &text)
}

/// The JSON reply `text` to `command`.
fn parse_reply<T: for<'de> Deserialize<'de>>(command: &str, text: &str) -> io::Result<T> {
    serde_json::from_str(text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{command}: {e}: {}",
                text.chars().take(200).collect::<String>()
            ),
        )
    })
}

/// Reads the whole state.
async fn query(requests: &Path, state: &mut State) -> io::Result<()> {
    let monitors = request_json(requests, "j/monitors").await?;
    let workspaces = request_json(requests, "j/workspaces").await?;
    let clients = request_json(requests, "j/clients").await?;
    // `{}` when no window has focus.
    let active: Client = request_json(requests, "j/activewindow").await?;
    state.replace(monitors, workspaces, clients, Some(active));
    Ok(())
}

/// The adapter: connects, reads, follows events; reconnects with backoff.
pub(crate) async fn run(
    requests: PathBuf,
    events: PathBuf,
    tx: UnboundedSender<AdapterMsg>,
    mut cmds: UnboundedReceiver<Cmd>,
) {
    let mut backoff = Backoff::new();
    let mut state = State::default();
    loop {
        match session(&requests, &events, &tx, &mut cmds, &mut state, &mut backoff).await {
            Ok(()) => return,
            Err(e) => log::warn!("Hyprland IPC: {e}; reconnecting"),
        }
        if tx.send(AdapterMsg::Connected(false)).is_err() {
            return;
        }
        backoff.wait(&mut cmds).await;
    }
}

/// One connection's life. `Ok` only when logic is gone.
async fn session(
    requests: &Path,
    events: &Path,
    tx: &UnboundedSender<AdapterMsg>,
    cmds: &mut UnboundedReceiver<Cmd>,
    state: &mut State,
    backoff: &mut Backoff,
) -> io::Result<()> {
    // Subscribe first, so nothing between the read and the stream is lost.
    let stream = UnixStream::connect(events).await?;
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    query(requests, state).await?;
    backoff.connected();
    if tx.send(AdapterMsg::Connected(true)).is_err()
        || tx.send(AdapterMsg::State(state.snapshot())).is_err()
    {
        return Ok(());
    }
    let mut cmds_open = true;
    // The dispatch dialect: classic until Hyprland answers in Lua.
    let mut lua = false;
    loop {
        tokio::select! {
            line = next_line(&mut reader, &mut buf) => {
                let Some(line) = line? else {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "event socket closed"));
                };
                let mut requery = false;
                let mut changed = false;
                let mut reloads = 0;
                let mut handle = |line: &[u8]| {
                    match state.apply_line(line) {
                        Effect::None => {}
                        Effect::Changed => changed = true,
                        Effect::Requery => requery = true,
                        Effect::Reloaded => {
                            requery = true;
                            reloads += 1;
                        }
                    }
                };
                handle(&line);
                // The rest of a burst that is already here.
                while let Some(next) =
                    futures_lite::future::poll_once(next_line(&mut reader, &mut buf)).await
                {
                    match next? {
                        Some(l) => handle(&l),
                        None => break,
                    }
                }
                if requery {
                    query(requests, state).await?;
                }
                if (changed || requery) && tx.send(AdapterMsg::State(state.snapshot())).is_err() {
                    return Ok(());
                }
                for _ in 0..reloads {
                    if tx.send(AdapterMsg::Reloaded { failed: None }).is_err() {
                        return Ok(());
                    }
                }
            }
            cmd = cmds.recv(), if cmds_open => match cmd {
                Some((action, reply)) => {
                    let mut result = dispatch(requests, state, &action, lua).await;
                    if !lua && matches!(&result, Err(WmError::Rejected(r)) if wants_lua(r)) {
                        lua = true;
                        result = dispatch(requests, state, &action, lua).await;
                    }
                    if let Some(r) = reply {
                        let _ = r.send(result);
                    }
                }
                None => cmds_open = false,
            },
        }
    }
}

/// Sends `action` as a `dispatch` request in the dialect `lua` says.
async fn dispatch(
    requests: &Path,
    state: &State,
    action: &WmAction,
    lua: bool,
) -> Result<(), WmError> {
    let req = state.dispatch_for(action, lua)?;
    match request(requests, &req).await {
        Ok(r) => dispatch_reply(&r),
        Err(e) => Err(WmError::Io(e.to_string())),
    }
}

/// Whether Hyprland's `reply` to a dispatch says it worked: `ok`, in
/// either dialect (a Lua dispatch, `return hl.dispatch(…)`, answers `ok`
/// too: Hyprland 0.56.2 in the CI job `compositors`, run 37653086662).
/// Anything else is the failure and kept: a Lua error (`error: …`), the
/// dispatcher's message, or a value of the chunk (`false`, `nil`, `true`,
/// nothing) no live Hyprland has been seen to answer for a success, so a
/// dispatch that did nothing is not taken for one (decisions.md,
/// wave4-exit-ci). scripts/compositor-matrix.sh prints a live reply.
fn dispatch_reply(reply: &str) -> Result<(), WmError> {
    match reply.trim() {
        "ok" => Ok(()),
        r => Err(WmError::Rejected(r.to_string())),
    }
}

/// Whether a dispatch was refused because Hyprland evaluates dispatches
/// as Lua (a Lua config): `error: [string "return hl.dispatch(…)"]:1: …`.
fn wants_lua(reply: &str) -> bool {
    reply.starts_with("error") && reply.contains("hl.dispatch(")
}

/// `s` as a Lua string literal.
fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut s = State::default();
        s.replace(
            vec![
                Monitor {
                    id: 0,
                    name: "DP-1".into(),
                    focused: true,
                    active_workspace: WsRef {
                        id: 1,
                        name: "1".into(),
                    },
                    disabled: false,
                },
                Monitor {
                    id: 1,
                    name: "HDMI-A-1".into(),
                    focused: false,
                    active_workspace: WsRef {
                        id: 3,
                        name: "3".into(),
                    },
                    disabled: false,
                },
            ],
            vec![
                HWorkspace {
                    id: 3,
                    name: "3".into(),
                    monitor: "HDMI-A-1".into(),
                },
                HWorkspace {
                    id: 1,
                    name: "1".into(),
                    monitor: "DP-1".into(),
                },
                HWorkspace {
                    id: 2,
                    name: "2".into(),
                    monitor: "DP-1".into(),
                },
                HWorkspace {
                    id: -98,
                    name: "special:magic".into(),
                    monitor: "DP-1".into(),
                },
            ],
            vec![Client {
                address: "0xa1".into(),
                mapped: true,
                workspace: WsRef {
                    id: 1,
                    name: "1".into(),
                },
                class: "kitty".into(),
                title: "~".into(),
                fullscreen: serde_json::json!(0),
                stable_id: "1f".into(),
            }],
            None,
        );
        s
    }

    #[test]
    fn events_patch_in_place_or_ask_for_a_requery() {
        let mut s = state();
        let snap = s.snapshot().state;
        assert_eq!(
            snap.workspaces.iter().map(|w| w.id).collect::<Vec<_>>(),
            [1, 2, 3],
            "sorted, special left out"
        );
        assert!(snap.workspaces[0].focused && snap.workspaces[2].active);
        assert!(!snap.workspaces[2].focused);

        assert_eq!(s.apply("workspacev2", "2,2"), Effect::Changed);
        let snap = s.snapshot().state;
        assert!(snap.workspace(2).unwrap().focused);
        assert!(!snap.workspace(1).unwrap().active);

        assert_eq!(s.apply("focusedmonv2", "HDMI-A-1,3"), Effect::Changed);
        assert_eq!(
            s.snapshot().state.focused_screen.as_deref(),
            Some("HDMI-A-1")
        );
        assert!(s.snapshot().state.workspace(3).unwrap().focused);

        assert_eq!(s.apply("activewindowv2", "a1"), Effect::Changed);
        assert!(s.snapshot().state.windows[0].focused);
        assert_eq!(s.apply("activewindowv2", ""), Effect::Changed);
        assert!(!s.snapshot().state.windows[0].focused);

        assert_eq!(s.apply("windowtitlev2", "a1,vim a, b"), Effect::Changed);
        assert_eq!(s.snapshot().state.windows[0].title, "vim a, b");
        assert_eq!(s.apply("windowtitlev2", "ff,x"), Effect::Requery);

        // A newline in a title: `j/clients` escapes it, the event has a
        // space instead. Both read the same, so the event changes nothing.
        let mut nl = state();
        let mut clients = nl.clients.clone();
        clients[0].title = "a\nb".into();
        nl.replace(nl.monitors.clone(), nl.workspaces.clone(), clients, None);
        assert_eq!(nl.snapshot().state.windows[0].title, "a b");
        assert_eq!(nl.apply("windowtitlev2", "a1,a b"), Effect::None);

        assert_eq!(s.apply("urgent", "a1"), Effect::Changed);
        assert!(s.snapshot().state.windows[0].urgent);
        assert_eq!(s.apply("activewindowv2", "a1"), Effect::Changed);
        assert!(
            !s.snapshot().state.windows[0].urgent,
            "focus clears urgency"
        );

        assert_eq!(s.apply("minimized", "a1,1"), Effect::Changed);
        assert!(s.snapshot().state.windows[0].minimized);

        assert_eq!(s.apply("openwindow", "b2,2,foot,foot"), Effect::Requery);
        assert_eq!(s.apply("configreloaded", ""), Effect::Reloaded);
        assert_eq!(s.apply("activelayout", "kb,us"), Effect::None);
        assert_eq!(s.apply("workspacev2", "77,77"), Effect::Requery);
    }

    #[test]
    fn raw_lines_are_decoded_lossily_and_long_titles_reread() {
        let mut s = state();
        assert_eq!(
            s.apply_line(b"windowtitlev2>>a1,caf\xe9, ok"),
            Effect::Changed
        );
        assert_eq!(s.snapshot().state.windows[0].title, "caf\u{fffd}, ok");
        assert_eq!(s.apply_line(b"urgent>>a1"), Effect::Changed);
        assert_eq!(s.apply_line(b"configreloaded>>"), Effect::Reloaded);
        assert_eq!(s.apply_line(b"nonsense"), Effect::None);

        // Data at Hyprland's 1024-byte cap may be cut (here inside `é`):
        // re-read instead of showing the cut title.
        let mut line = b"windowtitlev2>>a1,".to_vec();
        let mut data = b"a1,".to_vec();
        while data.len() < EVENT_DATA_CAP - 1 {
            data.push(b'x');
        }
        data.push(0xc3);
        line.truncate(b"windowtitlev2>>".len());
        line.extend_from_slice(&data);
        assert_eq!(data.len(), EVENT_DATA_CAP);
        assert_eq!(s.apply_line(&line), Effect::Requery);
        assert_eq!(s.snapshot().state.windows[0].title, "caf\u{fffd}, ok");
        // One byte under the cap is whole.
        line.pop();
        assert_eq!(s.apply_line(&line), Effect::Changed);
    }

    #[test]
    fn windows_carry_their_toplevel_identifier() {
        let ids = state().snapshot().toplevel_ids;
        assert_eq!(ids, [("0xa1".to_string(), "1f".to_string())]);
    }

    #[test]
    fn dispatches_name_the_target() {
        let s = state();
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWorkspace(2), false).unwrap(),
            "dispatch workspace 2"
        );
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWindow("0xa1".into()), false)
                .unwrap(),
            "dispatch focuswindow address:0xa1"
        );
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWorkspace(9), false),
            Err(WmError::UnknownWorkspace(9))
        );
        assert!(matches!(
            s.dispatch_for(&WmAction::MinimizeWindow("0xa1".into()), false),
            Err(WmError::Unsupported(_))
        ));
    }

    /// Hyprland with a Lua config (0.55 on) evaluates a dispatch's
    /// argument as `return hl.dispatch(<argument>)`.
    #[test]
    fn lua_dispatches_are_dispatcher_objects() {
        let s = state();
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWorkspace(2), true).unwrap(),
            r#"dispatch hl.dsp.focus({ workspace = "2" })"#
        );
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWindow("0xa1".into()), true)
                .unwrap(),
            r#"dispatch hl.dsp.focus({ window = "address:0xa1" })"#
        );
        assert_eq!(
            s.dispatch_for(&WmAction::CloseWindow("0xa1".into()), true)
                .unwrap(),
            r#"dispatch hl.dsp.window.close({ window = "address:0xa1" })"#
        );
        assert_eq!(lua_string("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        // Hyprland 0.56.2's reply to a classic dispatch with a Lua config.
        assert!(wants_lua(
            r#"error: [string "return hl.dispatch(workspace 2)"]:1: ')' expected near '2'"#
        ));
        assert!(!wants_lua("No such window found"));
        assert!(!wants_lua("ok"));
    }

    /// A dispatch succeeds only on `ok`, the one success reply seen from
    /// a live Hyprland in either dialect; every other reply is the failure.
    #[test]
    fn dispatch_replies() {
        for ok in ["ok", "ok\n"] {
            assert!(dispatch_reply(ok).is_ok(), "{ok:?}");
        }
        for failed in [
            "",
            "true",
            "nil",
            "false",
            "error: [string \"return hl.dispatch(x)\"]:1: nope",
            "No such window found",
        ] {
            assert!(
                matches!(dispatch_reply(failed), Err(WmError::Rejected(r)) if r == failed),
                "{failed:?}"
            );
        }
    }

    // ---- traffic captured from a live Hyprland 0.56.2 ----------------------

    const CAPTURED: &str = "hyprland-0.56.2-captured";

    /// The state `query` reads from the four replies captured in `dir`.
    fn from_captured(dir: &str) -> State {
        let read =
            |name: &str| crate::wm::captured::fixture(&format!("{CAPTURED}/{dir}/{name}.json"));
        let mut s = State::default();
        s.replace(
            parse_reply("j/monitors", &read("monitors")).unwrap(),
            parse_reply("j/workspaces", &read("workspaces")).unwrap(),
            parse_reply("j/clients", &read("clients")).unwrap(),
            Some(parse_reply::<Client>("j/activewindow", &read("activewindow")).unwrap()),
        );
        s
    }

    fn lines(text: &str) -> impl Iterator<Item = &str> {
        text.lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
    }

    /// Bursts from the live session, on the state read before them: what
    /// patches in place does, what needs a re-read asks for one, and a
    /// new window's urgent hint, which Hyprland sends before the window
    /// takes the focus, does not stay.
    #[test]
    fn captured_bursts_patch_or_ask_for_a_read() {
        use Effect::{Changed, None as Nothing, Requery};
        let mut s = from_captured("before");
        let snap = s.snapshot().state;
        assert_eq!(
            snap.workspaces.iter().map(|w| w.id).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(snap.windows.len(), 4);
        // Hyprland's stableIds are 8 hex digits; each window keeps its own
        // as the toplevel identifier it is joined by.
        let ids = s.snapshot().toplevel_ids;
        let pairs: Vec<(&str, &str)> = ids.iter().map(|(a, i)| (a.as_str(), i.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("0x557a6e2e3d90", "18000004"),
                ("0x557a6e37b040", "18000008"),
                ("0x557a6e3f20d0", "1800000a"),
                ("0x557a6df4c490", "18000003"),
            ],
            "every stableId"
        );
        let text = crate::wm::captured::fixture(&format!("{CAPTURED}/bursts.txt"));
        let bursts = crate::wm::captured::bursts(&text);
        let names: Vec<&str> = bursts.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "title",
                "switch",
                "switch-back",
                "new-workspace",
                "open",
                "retitle"
            ]
        );
        for (name, lines) in &bursts {
            let effects: Vec<Effect> = lines.iter().map(|l| s.apply_line(l.as_bytes())).collect();
            let snap = s.snapshot().state;
            let focused = snap.workspaces.iter().find(|w| w.focused).map(|w| w.id);
            let window = |a: &str| snap.windows.iter().find(|w| w.id == a).cloned();
            match name.as_str() {
                "title" => {
                    // The restated active window changes nothing.
                    assert_eq!(effects, [Nothing, Changed, Nothing, Nothing]);
                    assert_eq!(window("0x557a6df4c490").unwrap().title, "◑ term one");
                }
                "switch" => {
                    assert_eq!(effects, [Nothing, Changed, Nothing, Changed]);
                    assert_eq!(focused, Some(3));
                    assert!(window("0x557a6e37b040").unwrap().focused);
                }
                "switch-back" => {
                    assert_eq!(effects, [Nothing, Changed, Nothing, Changed]);
                    assert_eq!(focused, Some(1));
                    assert!(window("0x557a6df4c490").unwrap().focused);
                }
                "new-workspace" => {
                    // A workspace the state does not hold: read again.
                    assert_eq!(
                        effects,
                        [Requery, Requery, Nothing, Changed, Nothing, Requery]
                    );
                    assert!(snap.windows.iter().all(|w| !w.focused));
                }
                "open" => {
                    assert_eq!(
                        effects,
                        [Nothing, Requery, Changed, Requery, Nothing, Changed]
                    );
                    // Focused in the same burst: not left urgent.
                    assert_eq!(s.active.as_deref(), Some("0x557a6e504f50"));
                    assert!(s.urgent.is_empty(), "{:?}", s.urgent);
                    // The adapter reads again at the end of the burst:
                    // the replies read after the session hold the window.
                    let (urgent, minimized) = (s.urgent.clone(), s.minimized.clone());
                    s = from_captured("after");
                    s.urgent = urgent;
                    s.minimized = minimized;
                    assert!(
                        s.snapshot()
                            .state
                            .windows
                            .iter()
                            .any(|w| w.id == "0x557a6e504f50")
                    );
                }
                "retitle" => {
                    assert_eq!(effects, [Nothing, Changed, Nothing, Nothing]);
                    let w = window("0x557a6e504f50").unwrap();
                    assert_eq!(w.title, "user@host:~");
                    assert!(w.focused && !w.urgent);
                    assert_eq!(w.workspace, Some(5));
                }
                other => panic!("unknown burst {other}"),
            }
        }
    }

    /// The whole live stream from the state read before it: about 37
    /// minutes of titles, focus and switches patch in place without one
    /// read (and leave every window that then stays untouched with the
    /// title the later replies give); the new workspace and window ask for
    /// a read; after it, the rest patches to exactly what the replies read
    /// after the session say. The replies and the stream are separate
    /// captures with unrecorded gaps between them (SOURCE.txt): the replay
    /// treats the gaps as empty, and the title and final-state checks are
    /// what would catch one that was not.
    #[test]
    fn captured_stream_ends_where_hyprland_does() {
        let text = crate::wm::captured::fixture(&format!("{CAPTURED}/stream.txt"));
        let all: Vec<&str> = lines(&text).collect();
        assert!(all.len() > 3000, "{}", all.len());
        let structural = all
            .iter()
            .position(|l| l.starts_with("createworkspace"))
            .expect("the new workspace");
        let mut s = from_captured("before");
        for (i, line) in all[..structural].iter().enumerate() {
            let e = s.apply_line(line.as_bytes());
            assert!(
                matches!(e, Effect::None | Effect::Changed),
                "line {i} asked for a read: {line} ({e:?})"
            );
        }
        let after = from_captured("after");
        let titled = |s: &State, a: &str| {
            s.clients
                .iter()
                .find(|c| c.address == a)
                .map(|c| c.title.clone())
        };
        for c in &from_captured("before").clients {
            assert_eq!(
                titled(&s, &c.address),
                titled(&after, &c.address),
                "{}",
                c.address
            );
        }
        // The burst with the new workspace and window, up to the focus of
        // the window: then the adapter reads again.
        let open = all[structural..]
            .iter()
            .position(|l| l.starts_with("activewindowv2>>") && l.len() > "activewindowv2>>".len())
            .map(|i| structural + i + 1)
            .expect("the new window's focus");
        let effects: Vec<Effect> = all[structural..open]
            .iter()
            .map(|l| s.apply_line(l.as_bytes()))
            .collect();
        assert!(effects.contains(&Effect::Requery), "{effects:?}");
        let (urgent, minimized) = (s.urgent.clone(), s.minimized.clone());
        let mut s2 = from_captured("after");
        s2.urgent = urgent;
        s2.minimized = minimized;
        for (i, line) in all[open..].iter().enumerate() {
            let e = s2.apply_line(line.as_bytes());
            assert!(
                matches!(e, Effect::None | Effect::Changed),
                "line {} asked for a read: {line} ({e:?})",
                open + i
            );
        }
        assert_eq!(s2.snapshot(), after.snapshot());
    }
}

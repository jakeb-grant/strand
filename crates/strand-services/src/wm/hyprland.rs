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

    /// The `dispatch` request for an action.
    pub(crate) fn dispatch_for(&self, action: &WmAction) -> Result<String, WmError> {
        match action {
            WmAction::FocusWorkspace(id) => {
                let ws = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == *id)
                    .ok_or(WmError::UnknownWorkspace(*id))?;
                Ok(if *id > 0 {
                    format!("dispatch workspace {id}")
                } else {
                    format!("dispatch workspace name:{}", ws.name)
                })
            }
            WmAction::FocusWindow(a) => {
                self.client(a)?;
                Ok(format!("dispatch focuswindow address:{a}"))
            }
            WmAction::CloseWindow(a) => {
                self.client(a)?;
                Ok(format!("dispatch closewindow address:{a}"))
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
    serde_json::from_str(&text).map_err(|e| {
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
                    let result = match state.dispatch_for(&action) {
                        Ok(req) => match request(requests, &req).await {
                            Ok(r) if r.trim() == "ok" => Ok(()),
                            Ok(r) => Err(WmError::Rejected(r.trim().to_string())),
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
            s.dispatch_for(&WmAction::FocusWorkspace(2)).unwrap(),
            "dispatch workspace 2"
        );
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWindow("0xa1".into()))
                .unwrap(),
            "dispatch focuswindow address:0xa1"
        );
        assert_eq!(
            s.dispatch_for(&WmAction::FocusWorkspace(9)),
            Err(WmError::UnknownWorkspace(9))
        );
        assert!(matches!(
            s.dispatch_for(&WmAction::MinimizeWindow("0xa1".into())),
            Err(WmError::Unsupported(_))
        ));
    }
}

//! sway's IPC through swayipc-async 3.0.
//!
//! One connection subscribes to `workspace`, `window` and `shutdown`
//! events; another reads `get_workspaces` and `get_tree` and runs commands.
//! A window's title change is patched from the event; every other event
//! re-reads the workspaces and the tree, once per burst. sway's `reload`
//! (a `workspace` event with `change: reload`) is `wm.config_reloaded`.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use async_io::Async;
use futures_lite::StreamExt;
use swayipc_async::{Connection, Event, EventType, Node, NodeType, WindowChange, WorkspaceChange};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::backoff::Backoff;
use super::model::{Window, WmState, Workspace};
use super::{AdapterMsg, Cmd, IpcSnapshot, WmAction, WmError};

/// sway's scratchpad workspace.
const SCRATCH: &str = "__i3_scratch";

fn io_err(e: swayipc_async::Error) -> io::Error {
    match e {
        swayipc_async::Error::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

/// Whether an event-stream error means the socket is gone.
fn is_fatal(e: &swayipc_async::Error) -> bool {
    matches!(
        e,
        swayipc_async::Error::Io(_) | swayipc_async::Error::InvalidMagic(_)
    )
}

/// What sway's state turns into.
#[derive(Clone, Debug, Default)]
pub(crate) struct State {
    pub workspaces: Vec<swayipc_async::Workspace>,
    /// Windows with their workspace (`None` in the scratchpad).
    pub windows: Vec<(Option<i64>, Node)>,
}

fn is_window(n: &Node) -> bool {
    matches!(n.node_type, NodeType::Con | NodeType::FloatingCon)
        && (n.pid.is_some() || n.app_id.is_some() || n.window_properties.is_some())
        && n.nodes.is_empty()
}

fn collect_windows(node: &Node, workspace: Option<i64>, out: &mut Vec<(Option<i64>, Node)>) {
    let workspace = if node.node_type == NodeType::Workspace {
        (node.name.as_deref() != Some(SCRATCH)).then_some(node.id)
    } else {
        workspace
    };
    if is_window(node) {
        let mut leaf = node.clone();
        leaf.nodes.clear();
        leaf.floating_nodes.clear();
        out.push((workspace, leaf));
        return;
    }
    for n in node.nodes.iter().chain(&node.floating_nodes) {
        collect_windows(n, workspace, out);
    }
}

impl State {
    pub(crate) fn set_tree(&mut self, tree: &Node) {
        self.windows.clear();
        collect_windows(tree, None, &mut self.windows);
    }

    /// A title change from a `window` event; false when the window is not
    /// known (the caller re-reads).
    pub(crate) fn patch_title(&mut self, container: &Node) -> bool {
        match self.windows.iter_mut().find(|(_, n)| n.id == container.id) {
            Some((_, n)) => {
                n.name = container.name.clone();
                true
            }
            None => false,
        }
    }

    pub(crate) fn snapshot(&self) -> IpcSnapshot {
        let workspaces = self
            .workspaces
            .iter()
            .map(|w| Workspace {
                id: w.id,
                name: w.name.clone(),
                focused: w.focused,
                active: w.visible,
                urgent: w.urgent,
                screen: w.output.clone(),
                ..Default::default()
            })
            .collect();
        let mut toplevel_ids = Vec::new();
        let windows = self
            .windows
            .iter()
            .map(|(ws, n)| {
                let app_id = n
                    .app_id
                    .clone()
                    .or_else(|| n.window_properties.as_ref().and_then(|p| p.class.clone()))
                    .unwrap_or_default();
                if let Some(ident) = &n.foreign_toplevel_identifier {
                    toplevel_ids.push((n.id.to_string(), ident.clone()));
                }
                Window {
                    id: n.id.to_string(),
                    title: n.name.clone().unwrap_or_default(),
                    app_id,
                    workspace: *ws,
                    focused: n.focused,
                    minimized: ws.is_none(),
                    fullscreen: n.fullscreen_mode.is_some_and(|m| m != 0),
                    urgent: n.urgent,
                    ..Default::default()
                }
            })
            .collect();
        IpcSnapshot {
            state: WmState {
                name: "sway".into(),
                workspaces,
                windows,
                focused_screen: None,
            },
            toplevel_ids,
        }
    }

    /// The sway command for an action.
    pub(crate) fn command_for(&self, action: &WmAction) -> Result<String, WmError> {
        let window = |id: &str| -> Result<i64, WmError> {
            id.parse::<i64>()
                .ok()
                .filter(|n| self.windows.iter().any(|(_, w)| w.id == *n))
                .ok_or_else(|| WmError::UnknownWindow(id.to_string()))
        };
        Ok(match action {
            WmAction::FocusWorkspace(id) => {
                let ws = self
                    .workspaces
                    .iter()
                    .find(|w| w.id == *id)
                    .ok_or(WmError::UnknownWorkspace(*id))?;
                let name = ws.name.replace('\\', "\\\\").replace('"', "\\\"");
                format!("workspace --no-auto-back-and-forth \"{name}\"")
            }
            WmAction::FocusWindow(id) => format!("[con_id={}] focus", window(id)?),
            WmAction::CloseWindow(id) => format!("[con_id={}] kill", window(id)?),
            WmAction::MinimizeWindow(id) => format!("[con_id={}] move scratchpad", window(id)?),
        })
    }
}

async fn connect(path: &Path) -> io::Result<Connection> {
    Ok(Connection::from(Async::<UnixStream>::connect(path).await?))
}

async fn query(conn: &mut Connection, state: &mut State) -> io::Result<()> {
    state.workspaces = conn.get_workspaces().await.map_err(io_err)?;
    let tree = conn.get_tree().await.map_err(io_err)?;
    state.set_tree(&tree);
    Ok(())
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
            Err(e) => log::warn!("sway IPC: {e}; reconnecting"),
        }
        if tx.send(AdapterMsg::Connected(false)).is_err() {
            return;
        }
        backoff.wait(&mut cmds).await;
    }
}

/// What one event asks for.
enum Effect {
    None,
    Changed,
    Requery,
    Reloaded,
    Shutdown,
}

fn effect(state: &mut State, event: Event) -> Effect {
    match event {
        Event::Window(w) => match w.change {
            WindowChange::Title => {
                if state.patch_title(&w.container) {
                    Effect::Changed
                } else {
                    Effect::Requery
                }
            }
            WindowChange::Mark => Effect::None,
            _ => Effect::Requery,
        },
        Event::Workspace(w) => match w.change {
            WorkspaceChange::Reload => Effect::Reloaded,
            _ => Effect::Requery,
        },
        Event::Shutdown(_) => Effect::Shutdown,
        _ => Effect::None,
    }
}

async fn session(
    socket: &Path,
    tx: &UnboundedSender<AdapterMsg>,
    cmds: &mut UnboundedReceiver<Cmd>,
    backoff: &mut Backoff,
) -> io::Result<()> {
    let mut events = connect(socket)
        .await?
        .subscribe([EventType::Workspace, EventType::Window, EventType::Shutdown])
        .await
        .map_err(io_err)?;
    let mut conn = connect(socket).await?;
    let mut state = State::default();
    query(&mut conn, &mut state).await?;
    backoff.reset();
    if tx.send(AdapterMsg::Connected(true)).is_err()
        || tx.send(AdapterMsg::State(state.snapshot())).is_err()
    {
        return Ok(());
    }
    let mut cmds_open = true;
    loop {
        tokio::select! {
            event = events.next() => {
                let mut changed = false;
                let mut requery = false;
                let mut reloads = 0;
                let mut shutdown = false;
                let mut pending = Some(event);
                while let Some(event) = pending.take() {
                    match event {
                        None => {
                            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sway closed"));
                        }
                        Some(Err(e)) if is_fatal(&e) => return Err(io_err(e)),
                        Some(Err(e)) => {
                            // An event this version cannot decode: re-read.
                            log::warn!("sway IPC: {e}");
                            requery = true;
                        }
                        Some(Ok(ev)) => match effect(&mut state, ev) {
                            Effect::None => {}
                            Effect::Changed => changed = true,
                            Effect::Requery => requery = true,
                            Effect::Reloaded => {
                                reloads += 1;
                                requery = true;
                            }
                            Effect::Shutdown => shutdown = true,
                        },
                    }
                    // The rest of a burst that is already here.
                    pending = futures_lite::future::poll_once(events.next()).await;
                }
                if shutdown {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "sway is shutting down"));
                }
                if requery {
                    query(&mut conn, &mut state).await?;
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
                    let result = match state.command_for(&action) {
                        Ok(c) => match conn.run_command(&c).await {
                            Ok(outcomes) => outcomes
                                .into_iter()
                                .find_map(Result::err)
                                .map_or(Ok(()), |e| Err(WmError::Rejected(e.to_string()))),
                            Err(e) if is_fatal(&e) => {
                                if let Some(r) = reply {
                                    let _ = r.send(Err(WmError::Io(e.to_string())));
                                }
                                return Err(io_err(e));
                            }
                            Err(e) => Err(WmError::Rejected(e.to_string())),
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

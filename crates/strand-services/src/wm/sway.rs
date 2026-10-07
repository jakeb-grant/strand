//! sway's IPC: swayipc-types (swayipc-async 3.0's types) over our own
//! i3-ipc framing.
//!
//! One connection subscribes to `workspace`, `window` and `shutdown`
//! events; another reads `get_workspaces` and `get_tree` and runs commands.
//! A window's title change is patched from the event; every other event
//! re-reads the workspaces and the tree, once per burst. sway's `reload`
//! (a `workspace` event with `change: reload`) is `wm.config_reloaded`.
//!
//! The framing is ours (swayipc-async keeps its raw API private, and its
//! async-io stack would be a second reactor for nothing) so that
//! payloads are decoded lossily: wlroots copies an XWayland `WM_NAME` of
//! type `STRING` (Latin-1) byte for byte and sway's JSON leaves bytes over
//! 0x7f unescaped, so a title may not be UTF-8. swayipc-async would refuse
//! the whole tree, for as long as that window lives; here the title shows
//! U+FFFD where its bad bytes were. Frames are bounded
//! ([`MAX_MESSAGE`]); the event reader is cancel safe.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use swayipc_types::{
    CommandOutcome, CommandType, Event, Node, NodeType, WindowChange, WorkspaceChange,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::backoff::Backoff;
use super::lines::{MAX_MESSAGE, too_long};
use super::model::{Window, WmState, Workspace};
use super::{AdapterMsg, Cmd, IpcSnapshot, WmAction, WmError};

/// sway's scratchpad workspace.
const SCRATCH: &str = "__i3_scratch";

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER: usize = 14;

/// A payload as valid UTF-8: invalid bytes become U+FFFD.
pub(crate) fn lossy(payload: Vec<u8>) -> Vec<u8> {
    match String::from_utf8(payload) {
        Ok(s) => s.into_bytes(),
        Err(e) => String::from_utf8_lossy(e.as_bytes())
            .into_owned()
            .into_bytes(),
    }
}

/// Decodes a reply (lossily, as sway's tree may nest deeply like
/// swayipc-types does: no recursion limit).
pub(crate) fn decode<T: DeserializeOwned>(payload: Vec<u8>) -> io::Result<T> {
    let payload = lossy(payload);
    let mut de = serde_json::Deserializer::from_slice(&payload);
    de.disable_recursion_limit();
    T::deserialize(&mut de).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// One i3-ipc connection.
struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    write: tokio::net::unix::OwnedWriteHalf,
    buf: Vec<u8>,
}

impl Conn {
    async fn connect(path: &Path) -> io::Result<Self> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            write: w,
            buf: Vec::new(),
        })
    }

    /// The next frame: its type and payload. Cancel safe.
    async fn frame(&mut self) -> io::Result<(u32, Vec<u8>)> {
        loop {
            if self.buf.len() >= HEADER {
                if &self.buf[..6] != MAGIC {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "not an i3-ipc frame",
                    ));
                }
                let word = |at: usize| {
                    let mut b = [0u8; 4];
                    b.copy_from_slice(&self.buf[at..at + 4]);
                    u32::from_ne_bytes(b)
                };
                let len = word(6) as usize;
                let ty = word(10);
                if len > MAX_MESSAGE {
                    return Err(too_long());
                }
                if self.buf.len() >= HEADER + len {
                    let payload = self.buf[HEADER..HEADER + len].to_vec();
                    self.buf.drain(..HEADER + len);
                    return Ok((ty, payload));
                }
            }
            let avail = self.reader.fill_buf().await?;
            if avail.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sway closed"));
            }
            let n = avail.len();
            self.buf.extend_from_slice(avail);
            self.reader.consume(n);
        }
    }

    /// One request and its reply.
    async fn request<T: DeserializeOwned>(
        &mut self,
        ty: CommandType,
        payload: &str,
    ) -> io::Result<T> {
        let run = async {
            self.write.write_all(&ty.encode_with(payload)).await?;
            let (got, reply) = self.frame().await?;
            if got != u32::from(ty) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("sway answered {got} to {ty:?}"),
                ));
            }
            decode(reply)
        };
        tokio::time::timeout(REQUEST_TIMEOUT, run)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sway did not answer"))?
    }
}

#[derive(serde::Deserialize)]
struct Success {
    success: bool,
}

/// What sway's state turns into.
#[derive(Clone, Debug, Default)]
pub(crate) struct State {
    pub workspaces: Vec<swayipc_types::Workspace>,
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

async fn query(conn: &mut Conn, state: &mut State) -> io::Result<()> {
    state.workspaces = conn.request(CommandType::GetWorkspaces, "").await?;
    let tree: Node = conn.request(CommandType::GetTree, "").await?;
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
    let mut events = Conn::connect(socket).await?;
    let subscribed: Success = events
        .request(
            CommandType::Subscribe,
            r#"["workspace","window","shutdown"]"#,
        )
        .await?;
    if !subscribed.success {
        return Err(io::Error::other("sway refused the subscription"));
    }
    let mut conn = Conn::connect(socket).await?;
    let mut state = State::default();
    query(&mut conn, &mut state).await?;
    backoff.connected();
    if tx.send(AdapterMsg::Connected(true)).is_err()
        || tx.send(AdapterMsg::State(state.snapshot())).is_err()
    {
        return Ok(());
    }
    let mut cmds_open = true;
    loop {
        tokio::select! {
            event = events.frame() => {
                let mut changed = false;
                let mut requery = false;
                let mut reloads = 0;
                let mut shutdown = false;
                let mut pending = Some(event);
                while let Some(event) = pending.take() {
                    let (ty, payload) = event?;
                    match Event::decode((ty, lossy(payload))) {
                        Err(e) => {
                            // An event this version cannot decode: re-read.
                            log::warn!("sway IPC: {e}");
                            requery = true;
                        }
                        Ok(ev) => match effect(&mut state, ev) {
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
                    pending = futures_lite::future::poll_once(events.frame()).await;
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
                        Ok(c) => match conn
                            .request::<Vec<CommandOutcome>>(CommandType::RunCommand, &c)
                            .await
                        {
                            Ok(outcomes) => outcomes
                                .into_iter()
                                .map(CommandOutcome::decode)
                                .find_map(Result::err)
                                .map_or(Ok(()), |e| Err(WmError::Rejected(e.to_string()))),
                            Err(e) => {
                                if let Some(r) = reply {
                                    let _ = r.send(Err(WmError::Io(e.to_string())));
                                }
                                return Err(e);
                            }
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

    /// A `get_tree` reply (captured from sway 1.9) whose window title is
    /// Latin-1 decodes, with U+FFFD for the bad byte.
    #[test]
    fn replies_that_are_not_utf8_decode_lossily() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sway-1.9/tree.json"
        );
        let text = std::fs::read_to_string(path).unwrap();
        let (head, tail) = text.split_once("TITLE_HERE").unwrap();
        let mut raw = head.as_bytes().to_vec();
        raw.extend_from_slice(b"caf\xe9");
        raw.extend_from_slice(tail.as_bytes());
        assert!(
            serde_json::from_slice::<Node>(&raw).is_err(),
            "strict fails"
        );
        let tree: Node = decode(raw).unwrap();
        let mut state = State::default();
        state.set_tree(&tree);
        let snap = state.snapshot().state;
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].title, "caf\u{fffd}");
        assert_eq!(snap.windows[0].app_id, "foot");
    }

    #[tokio::test]
    async fn frames_are_bounded_and_cancel_safe() {
        use tokio::io::AsyncWriteExt;
        let (a, b) = UnixStream::pair().unwrap();
        let (r, w) = a.into_split();
        let mut conn = Conn {
            reader: BufReader::new(r),
            write: w,
            buf: Vec::new(),
        };
        let (_br, mut bw) = b.into_split();
        let frame = CommandType::GetTree.encode_with("{}");
        // Half a frame, then a cancelled read, then the rest.
        bw.write_all(&frame[..10]).await.unwrap();
        assert!(
            futures_lite::future::poll_once(conn.frame())
                .await
                .is_none(),
            "not whole yet"
        );
        bw.write_all(&frame[10..]).await.unwrap();
        assert_eq!(conn.frame().await.unwrap(), (4, b"{}".to_vec()));
        // A header that names more than the cap.
        let mut huge = b"i3-ipc".to_vec();
        huge.extend_from_slice(&u32::MAX.to_ne_bytes());
        huge.extend_from_slice(&4u32.to_ne_bytes());
        bw.write_all(&huge).await.unwrap();
        assert_eq!(
            conn.frame().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

//! sway's IPC: swayipc-types (swayipc-async 3.0's types) over our own
//! i3-ipc framing.
//!
//! One connection subscribes to `workspace`, `window`, `output`, `input`,
//! `mode` and `shutdown` events; another reads `get_workspaces` and
//! `get_tree` and runs commands. The keyboard layout (gap 31) and the
//! layouts it switches between are the first keyboard's at connection
//! (`get_inputs`), then follow each keyboard's `xkb_layout` and
//! `xkb_keymap` input events. The binding mode (`wm.mode`) is
//! `get_binding_state`'s at connection, then each `mode` event's; sway's
//! `default` is none. A window's place (`rect`) is its container's, less
//! its output's.
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
//!
//! A reply the adapter cannot decode (or bytes that are no i3-ipc frame),
//! eight event payloads in a row that are no JSON, or a command for an
//! action every sway has (focus, kill, workspace) that sway does not know
//! (`Unknown/invalid command`, [`refuses_syntax`]) degrade the adapter
//! (`understood`), naming sway's version (`get_version`); a later
//! action's unknown command, and any other `parse_error` (sway's
//! `CMD_INVALID`, which states such as no outputs also return), are only
//! rejected (`understood::tells_syntax`). An event of a type it does not know is
//! ignored; one it cannot decode is followed by a re-read, as before.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::DeserializeOwned;
use swayipc_types::{
    CommandOutcome, CommandType, Event, InputChange, Node, NodeType, WindowChange, WorkspaceChange,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::backoff::Backoff;
use super::lines::{MAX_MESSAGE, too_long};
use super::model::{WinRect, Window, WmState, Workspace};
use super::understood::{self, Degradation, SessionEnd, Strikes};
use super::{AdapterMsg, Cmd, IpcSnapshot, LayoutSwitch, WmAction, WmError};

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

    /// The next frame: its type and payload. Cancel safe. Bytes that are
    /// no i3-ipc frame are not understood (nothing after them can be
    /// read); a frame over [`MAX_MESSAGE`] is an I/O error.
    async fn frame(&mut self) -> Result<(u32, Vec<u8>), SessionEnd> {
        loop {
            if self.buf.len() >= HEADER {
                if &self.buf[..6] != MAGIC {
                    return Err(SessionEnd::reply(format!(
                        "its framing (`{}` where an i3-ipc header belongs)",
                        understood::excerpt(&String::from_utf8_lossy(&self.buf[..HEADER]))
                    )));
                }
                let word = |at: usize| {
                    let mut b = [0u8; 4];
                    b.copy_from_slice(&self.buf[at..at + 4]);
                    u32::from_ne_bytes(b)
                };
                let len = word(6) as usize;
                let ty = word(10);
                if len > MAX_MESSAGE {
                    return Err(too_long().into());
                }
                if self.buf.len() >= HEADER + len {
                    let payload = self.buf[HEADER..HEADER + len].to_vec();
                    self.buf.drain(..HEADER + len);
                    return Ok((ty, payload));
                }
            }
            let avail = self.reader.fill_buf().await?;
            if avail.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sway closed").into());
            }
            let n = avail.len();
            self.buf.extend_from_slice(avail);
            self.reader.consume(n);
        }
    }

    /// One request and its reply; a reply of another type or shape is not
    /// understood.
    async fn request<T: DeserializeOwned>(
        &mut self,
        ty: CommandType,
        payload: &str,
    ) -> Result<T, SessionEnd> {
        let run = async {
            self.write.write_all(&ty.encode_with(payload)).await?;
            let (got, reply) = self.frame().await?;
            if got != u32::from(ty) {
                return Err(SessionEnd::reply(format!(
                    "the reply to {ty:?} (a reply of type {got})"
                )));
            }
            let text = String::from_utf8_lossy(&reply).into_owned();
            decode(reply).map_err(|e| {
                SessionEnd::reply(format!(
                    "the reply to {ty:?} ({e}: `{}`)",
                    understood::excerpt(&text)
                ))
            })
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
    /// The config's `workspace <name> output <outputs…>` lines, in
    /// order ([`assignments`]); read when the session starts and on
    /// `reload`.
    pub assignments: Vec<(String, Vec<String>)>,
    /// The outputs (read with the state while there are assignments).
    pub outputs: Vec<SwayOutput>,
    /// The active keyboard layout (gap 31): the first keyboard's at
    /// connection (`get_inputs`), then the last keyboard whose layout or
    /// keymap changed (`input` events).
    pub keyboard_layout: Option<String>,
    /// (services-round) The layouts that keyboard switches between, by
    /// index.
    pub keyboard_layouts: Vec<String>,
    /// (services-round) The binding mode; `None` in sway's `default`.
    pub mode: Option<String>,
}

/// One of `get_inputs`' inputs: only a keyboard's layouts are read.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
pub(crate) struct SwayInput {
    #[serde(default, rename = "type")]
    pub input_type: String,
    #[serde(default)]
    pub xkb_active_layout_name: Option<String>,
    /// (services-round) Its layouts by index (sway may put a `null` in).
    #[serde(default)]
    pub xkb_layout_names: Vec<Option<String>>,
}

/// (services-round) The layouts the first keyboard that names any
/// switches between.
pub(crate) fn first_layouts(inputs: &[SwayInput]) -> Vec<String> {
    inputs
        .iter()
        .filter(|i| i.input_type == "keyboard")
        .map(|i| {
            i.xkb_layout_names
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
        })
        .find(|names| !names.is_empty())
        .unwrap_or_default()
}

/// (services-round) A binding mode's name as `wm.mode` holds it: sway's
/// `default` is none.
fn mode(name: &str) -> Option<String> {
    (!name.is_empty() && name != "default").then(|| name.to_string())
}

/// The layout `keyboard.layout` starts from: the first keyboard's that
/// names one.
pub(crate) fn first_layout(inputs: &[SwayInput]) -> Option<String> {
    inputs
        .iter()
        .filter(|i| i.input_type == "keyboard")
        .find_map(|i| i.xkb_active_layout_name.clone())
        .filter(|l| !l.is_empty())
}

/// One of `get_outputs`' outputs: what an assignment names it by.
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
pub(crate) struct SwayOutput {
    pub name: String,
    #[serde(default)]
    pub make: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub active: bool,
}

impl SwayOutput {
    /// Whether an assignment's `output` argument names it: its connector
    /// or its `make model serial` identifier.
    fn named(&self, arg: &str) -> bool {
        arg == self.name || arg == format!("{} {} {}", self.make, self.model, self.serial)
    }
}

/// The words of a config line as sway splits them: whitespace apart,
/// quotes (`"…"`, `'…'`) keeping words together and dropped.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(n) = chars.next() {
                    word.push(n);
                }
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// The config's top-level `workspace <name> output <outputs…>` lines
/// (Gaps 3, decisions.md m6-w2-services), with `set $var value`
/// variables expanded as sway expands them (longest name first, before
/// the line is split), comments and blocks' contents left out, and
/// continued lines (`\` at the end) joined. Only the text sway's
/// `get_config` returns is read: the main config file, not the files it
/// includes.
pub(crate) fn assignments(config: &str) -> Vec<(String, Vec<String>)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut pending = String::new();
    for raw in config.lines() {
        let line = raw.trim();
        if let Some(head) = line.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        let line = std::mem::take(&mut pending) + line;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let opens = line.ends_with('{');
        if line == "}" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth > 0 {
            depth += usize::from(opens);
            continue;
        }
        if opens {
            depth += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("set ") {
            let rest = rest.trim_start();
            if let Some(name) = rest.strip_prefix('$') {
                let (name, value) = name.split_once(char::is_whitespace).unwrap_or((name, ""));
                let value = expand(value.trim(), &vars);
                vars.retain(|(n, _)| n != name);
                vars.push((name.to_string(), value));
                vars.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
            }
            continue;
        }
        let w = words(&expand(line, &vars));
        if w.first().map(String::as_str) != Some("workspace") {
            continue;
        }
        let Some(k) = w.iter().skip(2).position(|t| t == "output").map(|k| k + 2) else {
            continue;
        };
        let name = w[1..k].join(" ");
        let outputs: Vec<String> = w[k + 1..].to_vec();
        if name.is_empty() || outputs.is_empty() {
            continue;
        }
        out.push((name, outputs));
    }
    out
}

/// `line` with each `$name` of `vars` (longest first) replaced.
fn expand(line: &str, vars: &[(String, String)]) -> String {
    let mut s = line.to_string();
    for (name, value) in vars {
        s = s.replace(&format!("${name}"), value);
    }
    s
}

/// A workspace name's leading number, as sway orders workspaces.
fn leading_number(name: &str) -> Option<i64> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn is_window(n: &Node) -> bool {
    matches!(n.node_type, NodeType::Con | NodeType::FloatingCon)
        && (n.pid.is_some() || n.app_id.is_some() || n.window_properties.is_some())
        && n.nodes.is_empty()
}

/// The windows under `node`, each with its workspace and its `rect`
/// made relative to its output (whose layout position is `origin`).
fn collect_windows(
    node: &Node,
    workspace: Option<i64>,
    origin: (i32, i32),
    out: &mut Vec<(Option<i64>, Node)>,
) {
    let workspace = if node.node_type == NodeType::Workspace {
        (node.name.as_deref() != Some(SCRATCH)).then_some(node.id)
    } else {
        workspace
    };
    let origin = if node.node_type == NodeType::Output {
        (node.rect.x, node.rect.y)
    } else {
        origin
    };
    if is_window(node) {
        let mut leaf = node.clone();
        leaf.nodes.clear();
        leaf.floating_nodes.clear();
        leaf.rect.x -= origin.0;
        leaf.rect.y -= origin.1;
        out.push((workspace, leaf));
        return;
    }
    for n in node.nodes.iter().chain(&node.floating_nodes) {
        collect_windows(n, workspace, origin, out);
    }
}

impl State {
    pub(crate) fn set_tree(&mut self, tree: &Node) {
        self.windows.clear();
        collect_windows(tree, None, (0, 0), &mut self.windows);
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
                    // In the scratchpad, nowhere.
                    rect: ws.map(|_| WinRect {
                        x: n.rect.x.into(),
                        y: n.rect.y.into(),
                        width: n.rect.width.into(),
                        height: n.rect.height.into(),
                    }),
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
                configured: self.configured(),
                keyboard_layout: self.keyboard_layout.clone(),
                keyboard_layouts: self.keyboard_layouts.clone(),
                keyboard_rules: None,
                mode: self.mode.clone(),
            },
            toplevel_ids,
            // `fullscreen_mode`; sway has no maximize, so never maximized.
            window_states: true,
        }
    }

    /// The workspaces the config's assignments place on an output that is
    /// on, and that do not exist yet (Gaps 3): by their name's number,
    /// then in the config's order. sway gives a workspace an id only once
    /// made, so each has a negative id of its own (`-1` for the first
    /// assignment, and on), which `ws.focus()` makes it by; the first
    /// assignment of a name, and the first of its outputs that is on,
    /// place it, as sway does.
    pub(crate) fn configured(&self) -> Vec<Workspace> {
        let mut out: Vec<Workspace> = Vec::new();
        for (i, (name, outputs)) in self.assignments.iter().enumerate() {
            if out.iter().any(|w| w.name == *name)
                || self.workspaces.iter().any(|w| w.name == *name)
            {
                continue;
            }
            let on = outputs.iter().find_map(|arg| {
                self.outputs
                    .iter()
                    .find(|o| o.active && o.named(arg))
                    .map(|o| o.name.clone())
            });
            let Some(screen) = on else {
                continue;
            };
            out.push(Workspace {
                id: -(i as i64) - 1,
                name: name.clone(),
                screen,
                ..Default::default()
            });
        }
        out.sort_by_key(|w| (leading_number(&w.name).is_none(), leading_number(&w.name)));
        out
    }

    /// The sway command switching to the workspace named `name` (made when
    /// it does not exist, on its assigned output if it has one).
    fn switch_to(name: &str) -> Result<String, WmError> {
        Ok(format!(
            "workspace --no-auto-back-and-forth {}",
            quoted(name)?
        ))
    }

    /// The name of workspace `id`: an existing one's, or a configured
    /// one's (made by going to it).
    fn workspace_name(&self, id: i64) -> Result<String, WmError> {
        match self.workspaces.iter().find(|w| w.id == id) {
            Some(ws) => Ok(ws.name.clone()),
            None => self
                .configured()
                .into_iter()
                .find(|w| w.id == id)
                .map(|w| w.name)
                .ok_or(WmError::UnknownWorkspace(id)),
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
            WmAction::FocusWorkspace(id) => Self::switch_to(&self.workspace_name(*id)?)?,
            // `workspace number n`: the workspace whose name starts with
            // n, made when there is none; a negative n is a configured
            // workspace's slot id.
            WmAction::FocusWorkspaceNumber(n) if *n > 0 => {
                format!("workspace --no-auto-back-and-forth number {n}")
            }
            WmAction::FocusWorkspaceNumber(n) if *n < 0 => {
                return self.command_for(&WmAction::FocusWorkspace(*n));
            }
            WmAction::FocusWorkspaceNumber(n) => return Err(WmError::UnknownWorkspace(*n)),
            WmAction::FocusWorkspaceNamed(name) => Self::switch_to(name)?,
            WmAction::FocusWindow(id) => format!("[con_id={}] focus", window(id)?),
            WmAction::CloseWindow(id) => format!("[con_id={}] kill", window(id)?),
            WmAction::MinimizeWindow(id) => format!("[con_id={}] move scratchpad", window(id)?),
            WmAction::MaximizeWindow(_) => {
                return Err(WmError::Unsupported(
                    "sway has no maximize (a tiling window fills its container; \
                     use win.fullscreen())",
                ));
            }
            WmAction::FullscreenWindow(id) => {
                format!("[con_id={}] fullscreen toggle", window(id)?)
            }
            // (services-round) Moved, focus staying where it is; a
            // configured workspace is made by it.
            WmAction::MoveWindow {
                window: id,
                workspace,
            } => format!(
                "[con_id={}] move container to workspace {}",
                window(id)?,
                quoted(&self.workspace_name(*workspace)?)?
            ),
            // Every keyboard, as `keyboard.layout` follows any.
            WmAction::SwitchLayout(LayoutSwitch::Next) => {
                "input type:keyboard xkb_switch_layout next".to_string()
            }
            WmAction::SwitchLayout(LayoutSwitch::Index(i)) => {
                format!("input type:keyboard xkb_switch_layout {i}")
            }
        })
    }
}

/// A workspace name as a command's quoted argument.
fn quoted(name: &str) -> Result<String, WmError> {
    if name.trim().is_empty() {
        return Err(WmError::UnknownWorkspaceName(name.to_string()));
    }
    let name = name.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{name}\""))
}

/// Whether a command's error says sway does not know the command at all.
/// sway sets `parse_error` for every `CMD_INVALID` result, and `focus`,
/// `kill` and `workspace` return one for states too (no outputs
/// connected, `Expected workspace number`, `There is no previous
/// workspace`), so only `execute_command`'s unknown-command reply
/// (`Unknown/invalid command '…'`) tells that the syntax is not sway's
/// (sway 1.10 `sway/commands.c`; decisions.md, laptop-resilience).
fn refuses_syntax(error: &str) -> bool {
    error.trim_start().starts_with("Unknown/invalid command")
}

async fn query(conn: &mut Conn, state: &mut State) -> Result<(), SessionEnd> {
    state.workspaces = conn.request(CommandType::GetWorkspaces, "").await?;
    let tree: Node = conn.request(CommandType::GetTree, "").await?;
    state.set_tree(&tree);
    Ok(())
}

/// The outputs (`get_outputs`), which only the config's assignments
/// need: read when a session starts, on `reload` and on an `output`
/// event (a hotplug, an output switched on or off), not on every re-read.
async fn read_outputs(conn: &mut Conn, state: &mut State) -> Result<(), SessionEnd> {
    state.outputs = if state.assignments.is_empty() {
        Vec::new()
    } else {
        extra(conn.request(CommandType::GetOutputs, "").await)?.unwrap_or_default()
    };
    Ok(())
}

/// The keyboards' layout (`get_inputs`), read when a session starts;
/// `input` events follow each change. Extra: a reply it cannot read
/// leaves the layout unknown.
async fn read_inputs(conn: &mut Conn, state: &mut State) -> Result<(), SessionEnd> {
    let inputs: Option<Vec<SwayInput>> = extra(conn.request(CommandType::GetInputs, "").await)?;
    state.keyboard_layout = inputs.as_deref().and_then(first_layout);
    state.keyboard_layouts = inputs.as_deref().map(first_layouts).unwrap_or_default();
    Ok(())
}

/// (services-round) The binding mode at connection
/// (`get_binding_state`, sway 1.5 on); `mode` events follow it. Extra:
/// a sway without it starts in none.
async fn read_mode(conn: &mut Conn, state: &mut State) -> Result<(), SessionEnd> {
    let binding: Option<swayipc_types::BindingState> =
        extra(conn.request(CommandType::GetBindingState, "").await)?;
    state.mode = binding.and_then(|b| mode(&b.name));
    Ok(())
}

/// The config's workspace assignments (`get_config`), read when a
/// session starts and on `reload`.
async fn read_assignments(conn: &mut Conn, state: &mut State) -> Result<(), SessionEnd> {
    let config: Option<swayipc_types::Config> =
        extra(conn.request(CommandType::GetConfig, "").await)?;
    state.assignments = config.map(|c| assignments(&c.config)).unwrap_or_default();
    Ok(())
}

/// A reply the adapter can do without (the config, the outputs): one it
/// cannot read is none, never a reason to degrade; a lost socket is.
fn extra<T>(reply: Result<T, SessionEnd>) -> Result<Option<T>, SessionEnd> {
    match reply {
        Ok(v) => Ok(Some(v)),
        Err(SessionEnd::Io(e)) => Err(SessionEnd::Io(e)),
        Err(SessionEnd::NotUnderstood(n)) => {
            log::debug!("sway IPC: {n:?}");
            Ok(None)
        }
    }
}

/// The version sway reports (`get_version`'s `human_readable`), for a
/// diagnostic.
async fn version(socket: &Path) -> Option<String> {
    let mut conn = Conn::connect(socket).await.ok()?;
    let v: swayipc_types::Version = conn.request(CommandType::GetVersion, "").await.ok()?;
    (!v.human_readable.is_empty()).then_some(v.human_readable)
}

/// The adapter: connects, reads, follows events; reconnects with backoff,
/// and degrades (`understood`) while sway is not understood.
pub(crate) async fn run(
    socket: PathBuf,
    tx: UnboundedSender<AdapterMsg>,
    mut cmds: UnboundedReceiver<Cmd>,
) {
    let mut backoff = Backoff::new();
    let mut degradation = Degradation::default();
    loop {
        let msg = match session(&socket, &tx, &mut cmds, &mut backoff, &mut degradation).await {
            Ok(()) => return,
            Err(SessionEnd::Io(e)) => {
                log::warn!("sway IPC: {e}; reconnecting");
                Some(AdapterMsg::Connected(false))
            }
            Err(SessionEnd::NotUnderstood(n)) => {
                let v = version(&socket).await;
                degradation.ended("sway", n, v)
            }
        };
        if msg.is_some_and(|m| tx.send(m).is_err()) {
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
    /// The outputs changed: read them again.
    Outputs,
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
        Event::Output(_) => Effect::Outputs,
        // A keyboard's layout switched or its keymap changed (a reload,
        // `input … xkb_layout`); a keyboard plugged in names one only
        // while none is known.
        Event::Input(e) if e.input.input_type == "keyboard" => {
            let layout = e.input.xkb_active_layout_name.filter(|l| !l.is_empty());
            let takes = match e.change {
                InputChange::XkbLayout | InputChange::XkbKeymap => true,
                InputChange::Added => state.keyboard_layout.is_none(),
                _ => false,
            };
            let layouts: Vec<String> = e.input.xkb_layout_names.into_iter().collect();
            let mut changed = false;
            if takes && layout.is_some() && layout != state.keyboard_layout {
                state.keyboard_layout = layout;
                changed = true;
            }
            // A new keymap may switch between other layouts.
            if takes && !layouts.is_empty() && layouts != state.keyboard_layouts {
                state.keyboard_layouts = layouts;
                changed = true;
            }
            if changed {
                Effect::Changed
            } else {
                Effect::None
            }
        }
        Event::Mode(m) => {
            let now = mode(&m.change);
            if now == state.mode {
                Effect::None
            } else {
                state.mode = now;
                Effect::Changed
            }
        }
        _ => Effect::None,
    }
}

/// What an event frame is to the adapter (`understood`).
enum Decoded {
    Event(Event),
    /// An event type this adapter does not know: ignored.
    Unknown,
    /// JSON this version of swayipc-types cannot decode (a new change
    /// kind, a field of another type): read again.
    Reread(String),
    /// No JSON at all: read again, and counted.
    Garbage,
}

fn decode_event(ty: u32, payload: Vec<u8>) -> Decoded {
    let payload = lossy(payload);
    if serde_json::from_slice::<serde::de::IgnoredAny>(&payload).is_err() {
        return Decoded::Garbage;
    }
    match Event::decode((ty, payload)) {
        Ok(ev) => Decoded::Event(ev),
        Err(swayipc_types::Error::UnimplementedEvent(..)) => Decoded::Unknown,
        Err(e) => Decoded::Reread(e.to_string()),
    }
}

async fn session(
    socket: &Path,
    tx: &UnboundedSender<AdapterMsg>,
    cmds: &mut UnboundedReceiver<Cmd>,
    backoff: &mut Backoff,
    degradation: &mut Degradation,
) -> Result<(), SessionEnd> {
    let mut events = Conn::connect(socket).await?;
    let subscribed: Success = events
        .request(
            CommandType::Subscribe,
            r#"["workspace","window","output","input","mode","shutdown"]"#,
        )
        .await?;
    if !subscribed.success {
        return Err(SessionEnd::reply(
            "the subscription to workspace, window, output, input, mode and shutdown events (refused)",
        ));
    }
    let mut conn = Conn::connect(socket).await?;
    let mut state = State::default();
    read_assignments(&mut conn, &mut state).await?;
    read_outputs(&mut conn, &mut state).await?;
    read_inputs(&mut conn, &mut state).await?;
    read_mode(&mut conn, &mut state).await?;
    query(&mut conn, &mut state).await?;
    // Actions this sway refused stay refused until it reports another
    // version.
    degradation.check_refusal(version(socket)).await?;
    // After a stream that was not understood, up only once this one
    // carried an event (`understood::Degradation`).
    let mut up = !degradation.on_probation();
    if up && !degradation.come_up(tx, backoff, state.snapshot()) {
        return Ok(());
    }
    let mut cmds_open = true;
    let mut strikes = Strikes::default();
    loop {
        tokio::select! {
            event = events.frame() => {
                let mut changed = false;
                let mut requery = false;
                let mut reloads = 0;
                let mut outputs = false;
                let mut shutdown = false;
                let mut pending = Some(event);
                while let Some(event) = pending.take() {
                    let (ty, payload) = event?;
                    let sample = payload.clone();
                    match decode_event(ty, payload) {
                        Decoded::Garbage => {
                            strikes.garbage("sway", &sample)?;
                            requery = true;
                        }
                        Decoded::Unknown => strikes.event(),
                        Decoded::Reread(e) => {
                            // An event this version cannot decode: re-read.
                            log::warn!("sway IPC: {e}");
                            strikes.event();
                            requery = true;
                        }
                        Decoded::Event(ev) => {
                            strikes.event();
                            match effect(&mut state, ev) {
                                Effect::None => {}
                                Effect::Changed => changed = true,
                                Effect::Requery => requery = true,
                                Effect::Reloaded => {
                                    reloads += 1;
                                    requery = true;
                                }
                                Effect::Outputs => {
                                    outputs = true;
                                    changed = true;
                                }
                                Effect::Shutdown => shutdown = true,
                            }
                        }
                    }
                    // The rest of a burst that is already here.
                    pending = futures_lite::future::poll_once(events.frame()).await;
                }
                if shutdown {
                    return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "sway is shutting down").into());
                }
                if reloads > 0 {
                    read_assignments(&mut conn, &mut state).await?;
                }
                if reloads > 0 || outputs {
                    read_outputs(&mut conn, &mut state).await?;
                }
                if requery {
                    query(&mut conn, &mut state).await?;
                }
                if !up {
                    if !strikes.seen_event() {
                        continue;
                    }
                    up = true;
                    if !degradation.come_up(tx, backoff, state.snapshot()) {
                        return Ok(());
                    }
                } else if (changed || requery) && tx.send(AdapterMsg::State(Box::new(state.snapshot()))).is_err() {
                    return Ok(());
                }
                for _ in 0..reloads {
                    if tx.send(AdapterMsg::Reloaded { failed: None }).is_err() {
                        return Ok(());
                    }
                }
            }
            cmd = cmds.recv(), if cmds_open => match cmd {
                // Not up: the services act through the protocols.
                Some((_, reply)) if !up => {
                    if let Some(r) = reply {
                        let _ = r.send(Err(WmError::NotConnected));
                    }
                }
                Some((action, reply)) => {
                    let mut refused_syntax = None;
                    let result = match state.command_for(&action) {
                        Ok(c) => match conn
                            .request::<Vec<CommandOutcome>>(CommandType::RunCommand, &c)
                            .await
                        {
                            Ok(outcomes) => outcomes
                                .into_iter()
                                .map(CommandOutcome::decode)
                                .find_map(Result::err)
                                .map_or(Ok(()), |e| {
                                    // sway does not know the command. For
                                    // an action every sway has, its syntax
                                    // is not sway's (any more); a later
                                    // one is only rejected
                                    // (`understood::tells_syntax`). Any
                                    // other `parse_error` is the action's
                                    // own failure ([`refuses_syntax`]).
                                    if let swayipc_types::Error::CommandParse(m) = &e
                                        && refuses_syntax(m)
                                        && understood::tells_syntax(&action)
                                    {
                                        refused_syntax = Some(format!(
                                            "the command syntax (`{}` answered `{}`)",
                                            understood::excerpt(&c),
                                            understood::excerpt(m)
                                        ));
                                    }
                                    Err(WmError::Rejected(e.to_string()))
                                }),
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

    /// Review minor 12: an `output` event (a hotplug, an output switched
    /// on or off) has the outputs read again, so configured slots follow
    /// it without waiting for a workspace event; nothing else re-reads
    /// them.
    #[test]
    fn an_output_event_reads_the_outputs_again() {
        let mut state = State::default();
        let Decoded::Event(ev) = decode_event(0x8000_0001, br#"{"change":"unspecified"}"#.to_vec())
        else {
            panic!("an output event");
        };
        assert!(matches!(effect(&mut state, ev), Effect::Outputs));
        let Decoded::Event(ev) = decode_event(
            0x8000_0000,
            br#"{"change":"focus","current":null,"old":null}"#.to_vec(),
        ) else {
            panic!("a workspace event");
        };
        assert!(matches!(effect(&mut state, ev), Effect::Requery));
    }

    /// Only sway's unknown-command reply refuses the syntax; the
    /// `CMD_INVALID` replies its `focus`, `kill` and `workspace` give for
    /// a state (sway 1.10) do not.
    #[test]
    fn only_an_unknown_command_refuses_the_syntax() {
        assert!(refuses_syntax("Unknown/invalid command 'focus'"));
        assert!(refuses_syntax("Unknown/invalid command 'workspace'"));
        for state in [
            "Can't run this command while there's no outputs connected.",
            "Expected workspace number",
            "There is no previous workspace",
            "No matching node.",
        ] {
            assert!(!refuses_syntax(state), "{state}");
        }
    }

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

    /// `win.fullscreen()` is sway's own toggle on the container; sway has
    /// no maximize, so `win.maximize()` is `Unsupported` and no window is
    /// ever `maximized`.
    #[test]
    fn fullscreen_toggles_and_maximize_is_unsupported() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sway-1.9/tree.json"
        );
        let text = std::fs::read_to_string(path).unwrap();
        let tree: Node = decode(text.into_bytes()).unwrap();
        let mut state = State::default();
        state.set_tree(&tree);
        let snap = state.snapshot();
        assert!(snap.window_states);
        let w = &snap.state.windows[0];
        assert!(!w.maximized);
        assert_eq!(
            state
                .command_for(&WmAction::FullscreenWindow(w.id.clone()))
                .unwrap(),
            format!("[con_id={}] fullscreen toggle", w.id)
        );
        assert!(matches!(
            state.command_for(&WmAction::MaximizeWindow(w.id.clone())),
            Err(WmError::Unsupported(_))
        ));
        assert_eq!(
            state.command_for(&WmAction::FullscreenWindow("0".into())),
            Err(WmError::UnknownWindow("0".into()))
        );
    }

    /// (services-round) A window's place is its container's less its
    /// output's (here a second output at 1280, 0); moves and layout
    /// switches are sway commands; `default` is no mode.
    #[test]
    fn rects_moves_layouts_and_modes() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sway-1.9/tree.json"
        );
        let text = std::fs::read_to_string(path).unwrap();
        let mut tree: serde_json::Value = serde_json::from_str(&text).unwrap();
        // HEADLESS-1 moved right of an output at 0, 0: every rect under
        // it moves with it, as sway's layout coordinates do.
        fn shift(n: &mut serde_json::Value) {
            n["rect"]["x"] = (n["rect"]["x"].as_i64().unwrap() + 1280).into();
            for k in ["nodes", "floating_nodes"] {
                if let Some(a) = n[k].as_array_mut() {
                    a.iter_mut().for_each(shift);
                }
            }
        }
        shift(&mut tree["nodes"][1]);
        let tree: Node = decode(tree.to_string().into_bytes()).unwrap();
        let mut state = State::default();
        state.set_tree(&tree);
        let w = &state.snapshot().state.windows[0];
        assert_eq!(
            w.rect,
            Some(WinRect {
                x: 0,
                y: 25,
                width: 1280,
                height: 695
            })
        );
        state.workspaces = decode(
            br#"[{"id": 4, "num": 1, "name": "1", "layout": "splith", "visible": true,
                  "focused": true, "urgent": false, "representation": null,
                  "orientation": "horizontal", "output": "HEADLESS-1",
                  "rect": {"x": 0, "y": 0, "width": 1280, "height": 720}},
                 {"id": 9, "num": 2, "name": "2: \"web\"", "layout": "splith", "visible": false,
                  "focused": false, "urgent": false, "representation": null,
                  "orientation": "horizontal", "output": "HEADLESS-1",
                  "rect": {"x": 0, "y": 0, "width": 1280, "height": 720}}]"#
                .to_vec(),
        )
        .unwrap();
        assert_eq!(
            state
                .command_for(&WmAction::MoveWindow {
                    window: w.id.clone(),
                    workspace: 9
                })
                .unwrap(),
            format!(
                r#"[con_id={}] move container to workspace "2: \"web\"""#,
                w.id
            )
        );
        assert_eq!(
            state.command_for(&WmAction::MoveWindow {
                window: w.id.clone(),
                workspace: 77
            }),
            Err(WmError::UnknownWorkspace(77))
        );
        assert_eq!(
            state
                .command_for(&WmAction::SwitchLayout(LayoutSwitch::Next))
                .unwrap(),
            "input type:keyboard xkb_switch_layout next"
        );
        assert_eq!(
            state
                .command_for(&WmAction::SwitchLayout(LayoutSwitch::Index(1)))
                .unwrap(),
            "input type:keyboard xkb_switch_layout 1"
        );
        assert_eq!(mode("default"), None);
        assert_eq!(mode(""), None);
        assert_eq!(mode("resize"), Some("resize".into()));
        let inputs: Vec<SwayInput> = decode(
            br#"[{"type": "pointer"},
                 {"type": "keyboard", "xkb_layout_names": []},
                 {"type": "keyboard", "xkb_layout_names": ["English (US)", null, "German"]}]"#
                .to_vec(),
        )
        .unwrap();
        assert_eq!(first_layouts(&inputs), ["English (US)", "German"]);
    }

    /// Gaps 3 (m6-w2-services): the config's top-level `workspace <name>
    /// output <outputs…>` lines, with `set` variables (longest name
    /// first), quotes, continued lines; blocks, comments, bindings and
    /// other `workspace` commands are not assignments.
    #[test]
    fn assignments_are_read_from_the_config() {
        let config = r#"
# workspace 9 output DP-9 (a comment)
set $mon DP-1
set $mon2 "HDMI-A-1"
set $ws3 "3: mail"
workspace 1 output $mon
workspace 2 output $mon2 $mon
workspace $ws3 output \
    $mon2
workspace "4: chat" output 'Dell Inc. DELL U2720Q 7Z9DL13'
workspace 5 gaps inner 4
workspace_layout tabbed
bindsym $mod+1 workspace number 1
mode "resize" {
    workspace 6 output DP-1
}
bar {
    position top
}
workspace 7 output DP-1
"#;
        assert_eq!(
            assignments(config),
            [
                ("1".to_string(), vec!["DP-1".to_string()]),
                ("2".into(), vec!["HDMI-A-1".into(), "DP-1".into()]),
                ("3: mail".into(), vec!["HDMI-A-1".into()]),
                (
                    "4: chat".into(),
                    vec!["Dell Inc. DELL U2720Q 7Z9DL13".into()]
                ),
                ("7".into(), vec!["DP-1".into()]),
            ]
        );
    }

    /// Gaps 2 and 3 (m6-w2-services): an assigned workspace that does
    /// not exist is configured on the first of its outputs that is on (by
    /// connector or by `make model serial`), with a negative id of its
    /// own that `ws.focus()` makes it by; a name is switched to (and
    /// made) as written.
    #[test]
    fn assigned_workspaces_are_configured_and_made_by_switching() {
        let workspaces: Vec<swayipc_types::Workspace> = decode(
            br#"[{"id": 4, "num": 1, "name": "1", "layout": "splith", "visible": true,
                  "focused": true, "urgent": false, "output": "DP-1",
                  "rect": {"x": 0, "y": 0, "width": 1, "height": 1}}]"#
                .to_vec(),
        )
        .unwrap();
        let output = |name: &str, active: bool| SwayOutput {
            name: name.into(),
            make: "Dell Inc.".into(),
            model: "DELL U2720Q".into(),
            serial: format!("S-{name}"),
            active,
        };
        let state = State {
            workspaces,
            assignments: assignments(
                "workspace 1 output DP-1\nworkspace 10 output DP-1\n\
                 workspace 2: web output DP-9 HDMI-A-1\n\
                 workspace chat output 'Dell Inc. DELL U2720Q S-DP-1'\n\
                 workspace 3 output DP-2\nworkspace 2: web output DP-1\n",
            ),
            outputs: vec![
                output("DP-1", true),
                output("HDMI-A-1", true),
                output("DP-2", false),
            ],
            ..Default::default()
        };
        let slots: Vec<(i64, String, String)> = state
            .snapshot()
            .state
            .configured
            .into_iter()
            .map(|w| (w.id, w.name, w.screen))
            .collect();
        assert_eq!(
            slots,
            [
                (-3, "2: web".to_string(), "HDMI-A-1".to_string()),
                (-2, "10".into(), "DP-1".into()),
                (-4, "chat".into(), "DP-1".into()),
            ]
        );
        assert_eq!(
            state.command_for(&WmAction::FocusWorkspace(-3)).unwrap(),
            r#"workspace --no-auto-back-and-forth "2: web""#
        );
        assert_eq!(
            state.command_for(&WmAction::FocusWorkspace(-5)),
            Err(WmError::UnknownWorkspace(-5))
        );
        assert_eq!(
            state
                .command_for(&WmAction::FocusWorkspaceNamed("say \"hi\"".into()))
                .unwrap(),
            r#"workspace --no-auto-back-and-forth "say \"hi\"""#
        );
        assert_eq!(
            state.command_for(&WmAction::FocusWorkspaceNamed(" ".into())),
            Err(WmError::UnknownWorkspaceName(" ".into()))
        );
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
        assert!(matches!(
            conn.frame().await,
            Err(SessionEnd::Io(e)) if e.kind() == io::ErrorKind::InvalidData
        ));
    }
}

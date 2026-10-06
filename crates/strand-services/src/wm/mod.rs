//! Workspaces, windows and the compositor (`workspaces`, `windows`, `wm`).
//!
//! Compositor-agnostic first (design.md, "System services"): the
//! [`ProtocolClient`] thread follows `ext-foreign-toplevel-list-v1` and
//! `ext-workspace-v1` wherever the compositor offers them. IPC adapters
//! fill in what those protocols do not cover (which workspace a window is
//! on, keyboard focus, fullscreen, the focused screen, config reloads) and
//! serve compositors without them: Hyprland and niri (own
//! implementations; niri behind the `niri` feature) and sway (through
//! swayipc-async). [`detect`] picks the adapter from the environment.
//!
//! [`run`] is the service: one future (for the shared tokio current-thread
//! runtime) that starts the protocol thread and the adapter, merges what
//! they report ([`merge`]) and sends each change as a batch of
//! [`WmChange`]s: keyed diffs of `workspaces.all` and `windows.all`, the
//! focused workspace, window and screen, and `wm.config_reloaded` (also
//! sent to `strand-watch`'s [`EventSink`] as
//! [`CompositorEvent::ConfigReloaded`]). Dropping the future stops
//! everything. Actions (`ws.focus()`, `win.close()`, …) go in through
//! [`WmRequest`]s.
//!
//! Lost sockets reconnect with backoff (100 ms doubling to 10 s); while an
//! adapter is away the last state stays. Nothing polls: an idle compositor
//! wakes nothing.

mod backoff;
pub mod detect;
mod hyprland;
pub mod model;
#[cfg(feature = "niri")]
mod niri;
pub mod protocol;
mod sway;

use std::future::Future;
use std::pin::Pin;

use strand_watch::{ChangeEvent, CompositorEvent, EventSink};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

pub use detect::{Backend, detect, detect_with};
pub use model::{CompositorKind, Mirror, Publisher, Sources, Window, WmChange, WmState, Workspace};
pub use protocol::{ProtoWorkspace, ProtocolClient, ProtocolState, Toplevel, WaylandTarget};

/// An action on a workspace or window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WmAction {
    /// `ws.focus()`: switch to the workspace.
    FocusWorkspace(i64),
    /// `win.focus()`: raise and focus the window.
    FocusWindow(String),
    /// `win.close()`: ask the window to close.
    CloseWindow(String),
    /// `win.minimize()`.
    MinimizeWindow(String),
}

/// Why an action did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WmError {
    /// The adapter is reconnecting.
    NotConnected,
    /// This compositor (or protocol) cannot do it.
    Unsupported(&'static str),
    /// No workspace has this id.
    UnknownWorkspace(i64),
    /// No window has this id.
    UnknownWindow(String),
    /// The compositor refused, with its message.
    Rejected(String),
    /// The socket failed.
    Io(String),
}

impl std::fmt::Display for WmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("the compositor is not connected"),
            Self::Unsupported(why) => write!(f, "not supported: {why}"),
            Self::UnknownWorkspace(id) => write!(f, "no workspace {id}"),
            Self::UnknownWindow(id) => write!(f, "no window {id}"),
            Self::Rejected(msg) => write!(f, "the compositor refused: {msg}"),
            Self::Io(msg) => write!(f, "compositor IPC: {msg}"),
        }
    }
}

impl std::error::Error for WmError {}

/// An action, with an optional channel for its outcome.
#[derive(Debug)]
pub struct WmRequest {
    /// What to do.
    pub action: WmAction,
    /// Where the outcome goes.
    pub reply: Option<oneshot::Sender<Result<(), WmError>>>,
}

impl WmRequest {
    /// A request and the receiver of its outcome.
    pub fn new(action: WmAction) -> (Self, oneshot::Receiver<Result<(), WmError>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                action,
                reply: Some(tx),
            },
            rx,
        )
    }
}

/// What [`run`] connects to.
#[derive(Clone, Debug, Default)]
pub struct WmConfig {
    /// The IPC adapter ([`detect`]); `None` for the protocols alone.
    pub backend: Option<Backend>,
    /// The display for the protocol client; `None` to not start it.
    pub wayland: Option<WaylandTarget>,
    /// Where `wm.config_reloaded` also goes as a [`ChangeEvent`].
    pub events: Option<EventSink>,
}

impl WmConfig {
    /// The adapter [`detect`] finds and the protocol client on
    /// `$WAYLAND_DISPLAY`.
    pub fn from_env(events: Option<EventSink>) -> Self {
        Self {
            backend: detect(),
            wayland: Some(WaylandTarget::Env),
            events,
        }
    }
}

/// An adapter's typed state, plus the `ext-foreign-toplevel-list`
/// identifier of the windows whose IPC reports it (sway 1.10 and later),
/// for joining with the protocol.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct IpcSnapshot {
    pub state: WmState,
    /// `(window id, toplevel identifier)`.
    pub toplevel_ids: Vec<(String, String)>,
}

/// What an adapter tells the service.
#[derive(Debug)]
pub(crate) enum AdapterMsg {
    State(IpcSnapshot),
    Reloaded { failed: Option<bool> },
    Connected(bool),
}

pub(crate) type Cmd = (WmAction, Option<oneshot::Sender<Result<(), WmError>>>);

/// Merges an adapter's state with the protocols': the protocol is
/// preferred where it covers a field. With an adapter, its workspace and
/// window sets and ids stand (only they relate windows to workspaces and
/// carry the ids actions need); a workspace joined by name (and screen,
/// when names repeat) takes `active`, `screen` and (or-ed) `urgent` from
/// `ext-workspace-v1`, and a window joined by its toplevel identifier
/// takes `title` and `app_id` from `ext-foreign-toplevel-list-v1`. Without
/// an adapter the protocols are the whole state: workspaces not `hidden`,
/// numbered by [`ProtoWorkspace::key`], `focused` where `active`; windows
/// by identifier, with no workspace or focus.
pub fn merge(ipc: Option<&WmState>, ids: &[(String, String)], proto: &ProtocolState) -> WmState {
    let mut s = match ipc {
        Some(ipc) => {
            let mut s = ipc.clone();
            for ws in &mut s.workspaces {
                let named: Vec<&ProtoWorkspace> = proto
                    .workspaces
                    .iter()
                    .filter(|p| p.name == ws.name)
                    .collect();
                let joined = match named.as_slice() {
                    [one] => Some(*one),
                    many => {
                        let mut on_screen = many.iter().filter(|p| p.screens.contains(&ws.screen));
                        match (on_screen.next(), on_screen.next()) {
                            (Some(p), None) => Some(*p),
                            _ => None,
                        }
                    }
                };
                if let Some(p) = joined {
                    ws.active = p.active;
                    ws.urgent |= p.urgent;
                    if let Some(screen) = p.screens.first() {
                        ws.screen = screen.clone();
                    }
                }
            }
            for w in &mut s.windows {
                let ident = ids.iter().find(|(id, _)| *id == w.id).map(|(_, i)| i);
                if let Some(t) =
                    ident.and_then(|i| proto.toplevels.iter().find(|t| t.identifier == *i))
                {
                    w.title = t.title.clone();
                    w.app_id = t.app_id.clone();
                }
            }
            s
        }
        None => WmState {
            name: String::new(),
            workspaces: proto
                .workspaces
                .iter()
                .filter(|p| !p.hidden)
                .map(|p| Workspace {
                    id: p.key as i64,
                    name: p.name.clone(),
                    focused: p.active,
                    active: p.active,
                    urgent: p.urgent,
                    screen: p.screens.first().cloned().unwrap_or_default(),
                    ..Default::default()
                })
                .collect(),
            windows: proto
                .toplevels
                .iter()
                .map(|t| Window {
                    id: t.identifier.clone(),
                    title: t.title.clone(),
                    app_id: t.app_id.clone(),
                    ..Default::default()
                })
                .collect(),
            focused_screen: None,
        },
    };
    for w in &mut s.windows {
        w.icon.clear();
    }
    s.derive();
    s
}

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

fn adapter(
    backend: Option<Backend>,
    tx: UnboundedSender<AdapterMsg>,
    cmds: UnboundedReceiver<Cmd>,
) -> BoxFuture {
    match backend {
        Some(Backend::Hyprland { requests, events }) => {
            Box::pin(hyprland::run(requests, events, tx, cmds))
        }
        #[cfg(feature = "niri")]
        Some(Backend::Niri { socket }) => Box::pin(niri::run(socket, tx, cmds)),
        #[cfg(not(feature = "niri"))]
        Some(Backend::Niri { .. }) => Box::pin(std::future::pending()),
        Some(Backend::Sway { socket }) => Box::pin(sway::run(socket, tx, cmds)),
        None => Box::pin(std::future::pending()),
    }
}

/// The service: runs until dropped, sending every batch of changes to
/// `sink` (never an empty one) and running `requests`.
pub async fn run<S>(config: WmConfig, mut sink: S, mut requests: UnboundedReceiver<WmRequest>)
where
    S: FnMut(Vec<WmChange>) + Send,
{
    let kind = config.backend.as_ref().map(Backend::kind);
    let (atx, mut arx) = mpsc::unbounded_channel();
    let (ctx, crx) = mpsc::unbounded_channel::<Cmd>();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let protocol = config
        .wayland
        .clone()
        .and_then(|t| match ProtocolClient::spawn(t, ptx) {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("cannot start the Wayland protocol thread: {e}");
                None
            }
        });
    let adapter = adapter(config.backend.clone(), atx, crx);
    let events = config.events;

    let coordinator = async move {
        let mut publisher = Publisher::new();
        let mut ipc: Option<IpcSnapshot> = None;
        let mut connected = false;
        let mut proto = ProtocolState::default();
        // Publish once the sources that will speak have spoken: the
        // adapter's first state, else the protocol's.
        let mut proto_ready = protocol.is_none();
        let mut requests_open = true;
        let mut first = true;
        loop {
            let mut changes = Vec::new();
            let mut reloaded = None;
            if !std::mem::take(&mut first) {
                tokio::select! {
                    Some(msg) = arx.recv() => match msg {
                        AdapterMsg::State(s) => ipc = Some(s),
                        AdapterMsg::Connected(c) => connected = c,
                        AdapterMsg::Reloaded { failed } => reloaded = Some(failed),
                    },
                    Some(p) = prx.recv() => {
                        proto = p;
                        proto_ready = true;
                    }
                    req = requests.recv(), if requests_open => match req {
                        Some(req) => route(req, kind.is_some(), ipc.as_ref(), &proto, &ctx, protocol.as_ref()),
                        None => requests_open = false,
                    },
                    // Every source has ended (no adapter, the display gone,
                    // no more requests): nothing can change any more.
                    else => std::future::pending::<()>().await,
                }
            }
            let ready = if kind.is_some() {
                ipc.is_some()
            } else {
                proto_ready
            };
            if ready {
                let mut state = merge(
                    ipc.as_ref().map(|i| &i.state),
                    ipc.as_ref().map_or(&[][..], |i| &i.toplevel_ids[..]),
                    &proto,
                );
                if let Some(k) = kind {
                    state.name = k.name().to_string();
                }
                changes.extend(publisher.publish(state));
                let sources = Sources {
                    ipc: kind,
                    connected,
                    toplevel_list: proto.toplevel_list,
                    workspace_protocol: proto.workspace_manager,
                };
                changes.extend(publisher.sources(sources));
            }
            if let Some(failed) = reloaded {
                changes.push(WmChange::ConfigReloaded { failed });
                if let Some(ev) = &events {
                    ev.send(ChangeEvent::Compositor(CompositorEvent::ConfigReloaded {
                        failed,
                    }));
                }
            }
            if !changes.is_empty() {
                sink(changes);
            }
        }
    };
    tokio::select! {
        () = adapter => {}
        () = coordinator => {}
    }
}

/// Sends an action where it can run: the adapter when there is one, else
/// `ext-workspace-v1` for workspaces.
fn route(
    req: WmRequest,
    has_adapter: bool,
    ipc: Option<&IpcSnapshot>,
    proto: &ProtocolState,
    adapter: &UnboundedSender<Cmd>,
    protocol: Option<&ProtocolClient>,
) {
    let fail = |r: Option<oneshot::Sender<Result<(), WmError>>>, e: WmError| {
        if let Some(r) = r {
            let _ = r.send(Err(e));
        }
    };
    if has_adapter {
        if ipc.is_none() {
            fail(req.reply, WmError::NotConnected);
        } else if let Err(mpsc::error::SendError((_, r))) = adapter.send((req.action, req.reply)) {
            fail(r, WmError::NotConnected);
        }
        return;
    }
    match req.action {
        WmAction::FocusWorkspace(id) => {
            let Some(client) = protocol.filter(|_| proto.workspace_manager) else {
                return fail(req.reply, WmError::Unsupported("no workspace source"));
            };
            let Some(p) = proto.workspaces.iter().find(|p| p.key as i64 == id) else {
                return fail(req.reply, WmError::UnknownWorkspace(id));
            };
            client.send(protocol::ProtoCmd::Activate(p.key, req.reply));
        }
        _ => fail(
            req.reply,
            WmError::Unsupported("ext-foreign-toplevel-list-v1 has no window actions"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto_ws(key: u64, name: &str, screen: &str, active: bool) -> ProtoWorkspace {
        ProtoWorkspace {
            key,
            name: name.into(),
            active,
            screens: vec![screen.into()],
            can_activate: true,
            ..Default::default()
        }
    }

    #[test]
    fn protocols_alone_make_the_whole_state() {
        let proto = ProtocolState {
            connected: true,
            toplevel_list: true,
            workspace_manager: true,
            toplevels: vec![Toplevel {
                identifier: "abc".into(),
                title: "t".into(),
                app_id: "foot".into(),
            }],
            workspaces: vec![
                proto_ws(1, "1", "DP-1", true),
                ProtoWorkspace {
                    hidden: true,
                    ..proto_ws(2, "scratch", "DP-1", false)
                },
                proto_ws(3, "2", "DP-1", false),
            ],
        };
        let s = merge(None, &[], &proto);
        assert_eq!(s.workspaces.len(), 2, "hidden left out");
        assert_eq!(s.workspaces[0].id, 1);
        assert!(s.workspaces[0].focused);
        assert_eq!(s.windows[0].id, "abc");
        assert_eq!(s.windows[0].icon, "foot");
        assert_eq!(s.focused_screen.as_deref(), Some("DP-1"));
    }

    #[test]
    fn the_protocol_wins_where_it_covers_a_field() {
        let ipc = WmState {
            name: "sway".into(),
            workspaces: vec![
                Workspace {
                    id: 10,
                    name: "1".into(),
                    focused: true,
                    active: true,
                    screen: "DP-1".into(),
                    ..Default::default()
                },
                Workspace {
                    id: 11,
                    name: "1".into(),
                    screen: "DP-2".into(),
                    ..Default::default()
                },
            ],
            windows: vec![Window {
                id: "7".into(),
                title: "old".into(),
                app_id: "foot".into(),
                workspace: Some(10),
                ..Default::default()
            }],
            focused_screen: None,
        };
        let proto = ProtocolState {
            connected: true,
            toplevel_list: true,
            workspace_manager: true,
            toplevels: vec![Toplevel {
                identifier: "x1".into(),
                title: "new".into(),
                app_id: "foot".into(),
            }],
            workspaces: vec![
                ProtoWorkspace {
                    urgent: true,
                    ..proto_ws(1, "1", "DP-1", true)
                },
                proto_ws(2, "1", "DP-2", true),
            ],
        };
        let s = merge(Some(&ipc), &[("7".into(), "x1".into())], &proto);
        assert_eq!(s.workspaces[0].id, 10, "IPC ids stand");
        assert!(s.workspaces[0].urgent);
        assert!(!s.workspaces[1].urgent);
        assert!(s.workspaces[1].active, "joined by screen when names repeat");
        assert_eq!(s.windows[0].title, "new");
        assert!(s.workspaces[0].occupied);
        // No identifier: the IPC title stays.
        let s = merge(Some(&ipc), &[], &proto);
        assert_eq!(s.windows[0].title, "old");
    }

    /// With no adapter, no display and no requester left, the service
    /// publishes its empty state once and then waits quietly (it does not
    /// spin or panic once every source has ended).
    #[tokio::test]
    async fn every_source_gone_leaves_it_waiting() {
        let (tx, rx) = mpsc::unbounded_channel::<WmRequest>();
        drop(tx);
        let batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = batches.clone();
        let config = WmConfig {
            wayland: Some(WaylandTarget::Socket("/nonexistent/wayland-0".into())),
            ..Default::default()
        };
        let fut = run(config, move |b| seen.lock().unwrap().push(b), rx);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), fut)
                .await
                .is_err(),
            "it keeps running until dropped"
        );
        let batches = batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "{batches:?}");
        assert!(
            batches[0]
                .iter()
                .any(|c| matches!(c, WmChange::Sources(s) if !s.toplevel_list))
        );
    }

    /// The service future can go on any runtime (Send).
    #[test]
    fn run_is_send() {
        fn send<T: Send>(_: &T) {}
        let (_tx, rx) = mpsc::unbounded_channel();
        let fut = run(WmConfig::default(), |_| {}, rx);
        send(&fut);
    }
}

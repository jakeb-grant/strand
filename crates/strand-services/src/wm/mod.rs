//! Workspaces, windows and the compositor (`workspaces`, `windows`, `wm`).
//!
//! Compositor-agnostic first (design.md, "System services"): the
//! [`ProtocolClient`] thread follows `ext-foreign-toplevel-list-v1` and
//! `ext-workspace-v1` wherever the compositor offers them. IPC adapters
//! fill in what those protocols do not cover (which workspace a window is
//! on, keyboard focus, fullscreen, the focused screen, config reloads) and
//! serve compositors without them: Hyprland and niri (own
//! implementations; niri behind the `niri` feature) and sway (through
//! swayipc-types, swayipc-async 3.0's types). [`detect`] picks the adapter from the environment.
//!
//! [`run`] is the service: one future (for the shared tokio current-thread
//! runtime) that starts the protocol thread and the adapter, merges what
//! they report ([`merge`]) and sends each change as a batch of
//! [`WmChange`]s: keyed diffs of `workspaces.all` and `windows.all`, the
//! focused workspace, window and screen, and `wm.config_reloaded` (also
//! sent to `strand-watch`'s [`EventSink`] as
//! [`CompositorEvent::ConfigReloaded`] as a live-reload change source;
//! the batch is the one that becomes the language event). Dropping the
//! future stops everything. Actions (`ws.focus()`, `win.close()`, …) go
//! in through [`WmRequest`]s. [`WmHub`] shares one `run` between the
//! `workspaces`, `windows` and `wm` stores (and `screens.focused`).
//!
//! Lost sockets reconnect with backoff (100 ms doubling to 10 s); while an
//! adapter is away the last state stays. Nothing polls: an idle compositor
//! wakes nothing.

mod backoff;
#[cfg(test)]
mod captured;
pub mod detect;
mod hub;
mod hyprland;
mod lines;
pub mod model;
#[cfg(feature = "niri")]
mod niri;
pub mod protocol;
mod schema;
mod service;
mod sway;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use strand_watch::{ChangeEvent, CompositorEvent, EventSink};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

pub use detect::{Backend, detect, detect_with};
pub use hub::{MAX_QUEUED, WmHub, WmSubscription};
pub use model::{CompositorKind, Mirror, Publisher, Sources, Window, WmChange, WmState, Workspace};
pub use protocol::{
    ManagedToplevel, ProtoWorkspace, ProtocolClient, ProtocolState, Toplevel, WaylandTarget,
};
use protocol::{ProtocolSender, WindowOp};
pub use schema::{WINDOWS_SCHEMA, WM_SCHEMA, WORKSPACES_SCHEMA};
pub use service::{
    WindowAction, WindowItem, Windows, WindowsCells, Wm, WmCells, WmEvent, WorkspaceAction,
    WorkspaceItem, Workspaces, WorkspacesCells, configure, hub,
};

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
    /// `win.maximize()`: maximise the window, or restore it when it is
    /// maximised (a toggle).
    MaximizeWindow(String),
    /// `win.fullscreen()`: make the window fullscreen, or restore it when
    /// it is fullscreen (a toggle).
    FullscreenWindow(String),
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

/// The outcome of a [`WmRequest`]: `Ok` or a [`WmError`]. A request the
/// service dropped unanswered (it stopped, or its protocol thread ended,
/// before the request ran) is [`WmError::NotConnected`]: a caller never
/// sees a closed channel.
#[derive(Debug)]
pub struct WmReply(oneshot::Receiver<Result<(), WmError>>);

impl Future for WmReply {
    type Output = Result<(), WmError>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(WmError::NotConnected)))
    }
}

/// An action, with an optional channel for its outcome.
#[derive(Debug)]
pub struct WmRequest {
    /// What to do.
    pub action: WmAction,
    /// Where the outcome goes.
    pub reply: Option<oneshot::Sender<Result<(), WmError>>>,
}

impl WmRequest {
    /// A request and its outcome.
    pub fn new(action: WmAction) -> (Self, WmReply) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                action,
                reply: Some(tx),
            },
            WmReply(rx),
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
    /// Where a compositor reload also goes as a [`ChangeEvent`]: the
    /// live-reload change source. `wm.config_reloaded` itself is the
    /// batch's [`WmChange::ConfigReloaded`]; this copy must not become a
    /// second one (docs/architecture.md, `strand-services`).
    pub events: Option<EventSink>,
    /// `wm.name` when no adapter runs (the protocols alone): the first
    /// entry of `XDG_CURRENT_DESKTOP`, such as `labwc` or `COSMIC`.
    pub desktop: Option<String>,
}

impl WmConfig {
    /// The adapter [`detect`] finds and the protocol client on
    /// `$WAYLAND_DISPLAY`.
    pub fn from_env(events: Option<EventSink>) -> Self {
        Self {
            backend: detect(),
            wayland: Some(WaylandTarget::Env),
            events,
            desktop: std::env::var_os("XDG_CURRENT_DESKTOP")
                .and_then(|d| first_desktop(&d.to_string_lossy())),
        }
    }
}

/// The first entry of an `XDG_CURRENT_DESKTOP` value (`sway:wlroots` is
/// `sway`).
pub fn first_desktop(value: &str) -> Option<String> {
    value
        .split(':')
        .map(str::trim)
        .find(|d| !d.is_empty())
        .map(str::to_string)
}

/// An adapter's typed state, plus the `ext-foreign-toplevel-list`
/// identifier of the windows whose IPC reports it (sway 1.10 and later;
/// Hyprland's `stableId`), for joining with the protocol. niri reports
/// none.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct IpcSnapshot {
    pub state: WmState,
    /// `(window id, toplevel identifier)`.
    pub toplevel_ids: Vec<(String, String)>,
    /// The adapter reports `maximized` and `fullscreen` itself (Hyprland,
    /// sway). niri's IPC does not, so its windows take them from
    /// `zwlr_foreign_toplevel_management_v1` ([`wlr_window_states`]).
    pub window_states: bool,
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
/// when names repeat on either side) takes `active`, `screen` and (or-ed) `urgent` from
/// `ext-workspace-v1`, and a window joined by its toplevel identifier
/// takes `title` and `app_id` from `ext-foreign-toplevel-list-v1` (every
/// window keeps that identifier as [`Window::toplevel`]). Without
/// an adapter the protocols are the whole state: workspaces not `hidden`,
/// numbered by [`ProtoWorkspace::key`]. Windows come from
/// `zwlr_foreign_toplevel_management_v1` when it is bound (ids
/// `wlr-<key>`, [`ManagedToplevel::window_id`]; `focused` is `activated`,
/// with `minimized`, `maximized` and `fullscreen`; the list identifier joined by
/// `join_managed` as [`Window::toplevel`]), else from
/// `ext-foreign-toplevel-list-v1` by identifier, with no state; never with
/// a workspace. `ext-workspace-v1` says which workspace each output shows
/// (`active`), not which output has the keyboard: the keyboard's screen is
/// the one activated wlr window's, when it is on one output, and a
/// workspace is `focused` when it is the one active workspace on that
/// screen, or, with no such screen known, the one active workspace; with
/// several outputs and no activated window none is (and no screen is
/// focused).
///
/// This is the merge for an adapter that reports `maximized` and
/// `fullscreen` itself; [`merge_with`] takes them from the wlr protocol
/// for one that does not (niri).
pub fn merge(ipc: Option<&WmState>, ids: &[(String, String)], proto: &ProtocolState) -> WmState {
    merge_with(ipc, ids, true, proto)
}

/// [`merge`], where `ipc_states` says whether the adapter reports
/// `maximized` and `fullscreen`; when it does not, its windows take them
/// from `zwlr_foreign_toplevel_management_v1` ([`wlr_window_states`]).
pub(crate) fn merge_with(
    ipc: Option<&WmState>,
    ids: &[(String, String)],
    ipc_states: bool,
    proto: &ProtocolState,
) -> WmState {
    let mut s = match ipc {
        Some(ipc) => {
            let mut s = ipc.clone();
            for ws in &mut s.workspaces {
                let named: Vec<&ProtoWorkspace> = proto
                    .workspaces
                    .iter()
                    .filter(|p| p.name == ws.name)
                    .collect();
                // niri names unnamed workspaces by index: `1` on each
                // output. Then even a lone protocol `1` must be on this
                // screen (its other twin may be hidden or not yet sent).
                let ipc_repeats = ipc.workspaces.iter().filter(|w| w.name == ws.name).count() > 1;
                let joined = match named.as_slice() {
                    [one] if !ipc_repeats || one.screens.contains(&ws.screen) => Some(*one),
                    [_] => None,
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
                w.toplevel = ident.cloned();
                if let Some(t) =
                    ident.and_then(|i| proto.toplevels.iter().find(|t| t.identifier == *i))
                {
                    w.title = t.title.clone();
                    w.app_id = t.app_id.clone();
                }
            }
            if !ipc_states {
                wlr_window_states(&mut s.windows, proto);
            }
            s
        }
        None => {
            let shown = || proto.workspaces.iter().filter(|p| !p.hidden);
            // The keyboard's screen: the activated wlr window's, when
            // there is one window activated and it is on one output.
            let keyboard_screen = if proto.toplevel_management {
                let mut activated = proto.managed.iter().filter(|m| m.activated);
                match (activated.next(), activated.next()) {
                    (Some(m), None) if m.screens.len() == 1 => Some(m.screens[0].clone()),
                    _ => None,
                }
            } else {
                None
            };
            let mut active = shown().filter(|p| {
                p.active
                    && keyboard_screen
                        .as_ref()
                        .is_none_or(|s| p.screens.contains(s))
            });
            let sole_active = match (active.next(), active.next()) {
                (Some(p), None) => Some(p.key),
                _ => None,
            };
            let windows = if proto.toplevel_management {
                let joins = join_managed(&proto.managed, &proto.toplevels);
                proto
                    .managed
                    .iter()
                    .zip(joins)
                    .map(|(m, toplevel)| Window {
                        id: m.window_id(),
                        title: m.title.clone(),
                        app_id: m.app_id.clone(),
                        focused: m.activated,
                        minimized: m.minimized,
                        maximized: m.maximized,
                        fullscreen: m.fullscreen,
                        toplevel,
                        ..Default::default()
                    })
                    .collect()
            } else {
                proto
                    .toplevels
                    .iter()
                    .map(|t| Window {
                        id: t.identifier.clone(),
                        title: t.title.clone(),
                        app_id: t.app_id.clone(),
                        toplevel: Some(t.identifier.clone()),
                        ..Default::default()
                    })
                    .collect()
            };
            WmState {
                name: String::new(),
                workspaces: shown()
                    .map(|p| Workspace {
                        id: p.key as i64,
                        name: p.name.clone(),
                        focused: sole_active == Some(p.key),
                        active: p.active,
                        urgent: p.urgent,
                        screen: p.screens.first().cloned().unwrap_or_default(),
                        ..Default::default()
                    })
                    .collect(),
                windows,
                focused_screen: keyboard_screen,
            }
        }
    };
    for w in &mut s.windows {
        w.icon.clear();
    }
    s.derive();
    s
}

/// The `ext-foreign-toplevel-list-v1` identifier of each wlr toplevel, by
/// position: the wlr protocol has no identifier, so a wlr toplevel takes
/// the identifier of the list toplevel with the same app id and title when
/// both protocols show as many toplevels with that app id and title (the
/// n-th oldest of one side pairs with the n-th oldest of the other), and
/// none otherwise (one side has not caught up, or the two disagree).
fn join_managed(managed: &[ManagedToplevel], list: &[Toplevel]) -> Vec<Option<String>> {
    managed
        .iter()
        .map(|m| {
            let twins: Vec<&ManagedToplevel> = managed
                .iter()
                .filter(|o| o.app_id == m.app_id && o.title == m.title)
                .collect();
            let listed: Vec<&Toplevel> = list
                .iter()
                .filter(|t| t.app_id == m.app_id && t.title == m.title)
                .collect();
            if twins.len() != listed.len() {
                return None;
            }
            let nth = twins.iter().position(|o| o.key == m.key)?;
            listed.get(nth).map(|t| t.identifier.clone())
        })
        .collect()
}

/// `maximized` and `fullscreen` for an adapter's windows from
/// `zwlr_foreign_toplevel_management_v1`, for an adapter whose IPC does
/// not report them (niri: its `Window` has neither). Neither side carries
/// an identifier the other knows, so a window pairs with the wlr toplevel
/// of the same app id when each side shows exactly one with that app id;
/// otherwise (twins) the n-th window with that app id and title pairs
/// with the n-th wlr toplevel with them when both sides show as many (the
/// adapter's windows in the order they opened, niri's ids; the wlr
/// toplevels in the order they were announced). An unpaired window has
/// neither state. Pairing a lone app by its app id alone keeps its state
/// through a title change that has reached one protocol before the other.
fn wlr_window_states(windows: &mut [Window], proto: &ProtocolState) {
    if !proto.toplevel_management {
        return;
    }
    let states: Vec<Option<(bool, bool)>> = windows
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let apps: Vec<&ManagedToplevel> = proto
                .managed
                .iter()
                .filter(|m| m.app_id == w.app_id)
                .collect();
            let app_windows = windows.iter().filter(|o| o.app_id == w.app_id).count();
            let m = match (apps.as_slice(), app_windows) {
                ([m], 1) => Some(*m),
                _ => {
                    let twins: Vec<usize> = (0..windows.len())
                        .filter(|j| windows[*j].app_id == w.app_id && windows[*j].title == w.title)
                        .collect();
                    let managed: Vec<&ManagedToplevel> =
                        apps.into_iter().filter(|m| m.title == w.title).collect();
                    if twins.len() == managed.len() {
                        twins
                            .iter()
                            .position(|j| *j == i)
                            .and_then(|n| managed.get(n).copied())
                    } else {
                        None
                    }
                }
            };
            m.map(|m| (m.maximized, m.fullscreen))
        })
        .collect();
    for (w, state) in windows.iter_mut().zip(states) {
        (w.maximized, w.fullscreen) = state.unwrap_or_default();
    }
}

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

static LIVE_RUNS: AtomicUsize = AtomicUsize::new(0);

/// How many [`run`]s of this process are live (started and not yet
/// dropped): tests check that the three stores share one, and that it
/// goes with them.
pub fn live_runs() -> usize {
    LIVE_RUNS.load(Ordering::SeqCst)
}

/// Counts a live [`run`] until dropped.
struct LiveRun;

impl Drop for LiveRun {
    fn drop(&mut self) {
        LIVE_RUNS.fetch_sub(1, Ordering::SeqCst);
    }
}

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
        // `run` turns niri into no adapter without the feature.
        #[cfg(not(feature = "niri"))]
        Some(Backend::Niri { .. }) => Box::pin(std::future::pending()),
        Some(Backend::Sway { socket }) => Box::pin(sway::run(socket, tx, cmds)),
        None => Box::pin(std::future::pending()),
    }
}

/// The service: runs until dropped, sending every batch of changes to
/// `sink` (never an empty one) and running `requests`.
///
/// The first batch is [`WmChange::Sources`] at least, so a consumer can
/// always tell which adapter is meant to run and whether it is connected.
/// The state goes out once a source has spoken: the adapter's first state;
/// or, without an adapter or once the adapter has failed to connect, the
/// protocols' (standard protocols first, IPC as the fallback: a broken
/// adapter does not hide them). When the adapter's state arrives later,
/// its ids replace the protocols' with a `Reset` of both lists (the same
/// id may mean another workspace), not a keyed diff.
///
/// The protocol thread is this run's own: dropping the run stops it
/// without waiting. (A [`WmHub`] starts the thread itself, and joins it
/// when the run stops.)
pub async fn run<S>(config: WmConfig, sink: S, requests: UnboundedReceiver<WmRequest>)
where
    S: FnMut(Vec<WmChange>) + Send,
{
    let (ptx, prx) = mpsc::unbounded_channel();
    let client = spawn_protocol(&config, ptx);
    let sender = client.as_ref().map(ProtocolClient::sender);
    drive(config, sink, requests, sender, prx).await;
    drop(client);
}

/// Starts `config`'s protocol thread, if it names a display.
pub(crate) fn spawn_protocol(
    config: &WmConfig,
    tx: UnboundedSender<ProtocolState>,
) -> Option<ProtocolClient> {
    let target = config.wayland.clone()?;
    ProtocolClient::spawn(target, tx)
        .map_err(|e| log::warn!("cannot start the Wayland protocol thread: {e}"))
        .ok()
}

/// [`run`] with the protocol thread started by the caller (`protocol`
/// sends to it; its states come on `prx`).
pub(crate) async fn drive<S>(
    config: WmConfig,
    mut sink: S,
    mut requests: UnboundedReceiver<WmRequest>,
    protocol: Option<ProtocolSender>,
    mut prx: UnboundedReceiver<ProtocolState>,
) where
    S: FnMut(Vec<WmChange>) + Send,
{
    LIVE_RUNS.fetch_add(1, Ordering::SeqCst);
    let _live = LiveRun;
    #[allow(unused_mut)]
    let mut backend = config.backend.clone();
    #[cfg(not(feature = "niri"))]
    if matches!(backend, Some(Backend::Niri { .. })) {
        log::warn!("built without the `niri` feature: niri is served by the standard protocols");
        backend = None;
    }
    let kind = backend.as_ref().map(Backend::kind);
    let (atx, mut arx) = mpsc::unbounded_channel();
    let (ctx, crx) = mpsc::unbounded_channel::<Cmd>();
    let adapter = adapter(backend, atx, crx);
    let events = config.events;
    let desktop = config.desktop;

    let coordinator = async move {
        let mut publisher = Publisher::new();
        // Whose ids the last published state carried (the adapter's or
        // the protocols').
        let mut published_ipc_ids: Option<bool> = None;
        let mut ipc: Option<IpcSnapshot> = None;
        let mut connected = false;
        // The adapter has reported a failed attempt: the protocols may be
        // published on their own until it comes up.
        let mut adapter_failed = false;
        let mut proto = ProtocolState::default();
        let mut proto_ready = protocol.is_none();
        let mut requests_open = true;
        let mut first = true;
        loop {
            let mut changes = Vec::new();
            let mut reloads = Vec::new();
            if !std::mem::take(&mut first) {
                tokio::select! {
                    Some(msg) = arx.recv() => {
                        // Everything already queued, in order, then one
                        // merge: a busy runtime never diffs stale states.
                        let mut next = Some(msg);
                        while let Some(msg) = next.take() {
                            match msg {
                                AdapterMsg::State(s) => ipc = Some(s),
                                AdapterMsg::Connected(c) => {
                                    connected = c;
                                    adapter_failed |= !c;
                                }
                                AdapterMsg::Reloaded { failed } => reloads.push(failed),
                            }
                            next = arx.try_recv().ok();
                        }
                    }
                    Some(mut p) = prx.recv() => {
                        while let Ok(newer) = prx.try_recv() {
                            p = newer;
                        }
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
            let ready = match kind {
                None => proto_ready,
                Some(_) => ipc.is_some() || (proto_ready && adapter_failed),
            };
            if ready {
                let mut state = merge_with(
                    ipc.as_ref().map(|i| &i.state),
                    ipc.as_ref().map_or(&[][..], |i| &i.toplevel_ids[..]),
                    ipc.as_ref().is_none_or(|i| i.window_states),
                    &proto,
                );
                state.name = match kind {
                    Some(k) => k.name().to_string(),
                    None => desktop.clone().unwrap_or_default(),
                };
                // The adapter came up after the protocols' state went out:
                // equal ids may name different workspaces, so the lists
                // start over (a `Reset`) instead of updating in place.
                let ipc_ids = ipc.is_some();
                if published_ipc_ids.is_some_and(|was| was != ipc_ids) {
                    publisher.forget();
                }
                published_ipc_ids = Some(ipc_ids);
                changes.extend(publisher.publish(state));
            }
            let sources = Sources {
                ipc: kind,
                connected,
                toplevel_list: proto.toplevel_list,
                toplevel_management: proto.toplevel_management,
                workspace_protocol: proto.workspace_manager,
            };
            changes.extend(publisher.sources(sources));
            for failed in reloads {
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

/// Sends an action where it can run: the adapter once its state is out
/// (the ids shown are its own), else `ext-workspace-v1` for workspaces and
/// `zwlr_foreign_toplevel_management_v1` for windows (the ids shown are
/// the protocols'). While an adapter has not come up,
/// what the protocol cannot do answers `NotConnected`.
fn route(
    req: WmRequest,
    has_adapter: bool,
    ipc: Option<&IpcSnapshot>,
    proto: &ProtocolState,
    adapter: &UnboundedSender<Cmd>,
    protocol: Option<&ProtocolSender>,
) {
    let fail = |r: Option<oneshot::Sender<Result<(), WmError>>>, e: WmError| {
        if let Some(r) = r {
            let _ = r.send(Err(e));
        }
    };
    let unsupported = |why: &'static str| {
        if has_adapter {
            WmError::NotConnected
        } else {
            WmError::Unsupported(why)
        }
    };
    if ipc.is_some() {
        if let Err(mpsc::error::SendError((_, r))) = adapter.send((req.action, req.reply)) {
            fail(r, WmError::NotConnected);
        }
        return;
    }
    let (id, op) = match req.action {
        WmAction::FocusWorkspace(id) => {
            let Some(client) = protocol.filter(|_| proto.workspace_manager) else {
                return fail(req.reply, unsupported("no workspace source"));
            };
            let Some(p) = proto.workspaces.iter().find(|p| p.key as i64 == id) else {
                return fail(req.reply, WmError::UnknownWorkspace(id));
            };
            return client.send(protocol::ProtoCmd::Activate(p.key, req.reply));
        }
        WmAction::FocusWindow(id) => (id, WindowOp::Activate),
        WmAction::CloseWindow(id) => (id, WindowOp::Close),
        WmAction::MinimizeWindow(id) => (id, WindowOp::Minimize),
        WmAction::MaximizeWindow(id) => (id, WindowOp::Maximize),
        WmAction::FullscreenWindow(id) => (id, WindowOp::Fullscreen),
    };
    // Window actions: `zwlr_foreign_toplevel_management_v1`, whose ids the
    // windows shown carry whenever it is bound (`merge`).
    let Some(client) = protocol.filter(|_| proto.toplevel_management) else {
        return fail(
            req.reply,
            unsupported(
                "no zwlr_foreign_toplevel_management_v1 \
                 (ext-foreign-toplevel-list-v1 has no window actions)",
            ),
        );
    };
    let Some(m) = proto.managed.iter().find(|m| m.window_id() == id) else {
        return fail(req.reply, WmError::UnknownWindow(id));
    };
    client.send(protocol::ProtoCmd::Window(m.key, op, req.reply));
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
            ..Default::default()
        };
        let s = merge(None, &[], &proto);
        assert_eq!(s.workspaces.len(), 2, "hidden left out");
        assert_eq!(s.workspaces[0].id, 1);
        assert!(s.workspaces[0].focused);
        assert_eq!(s.windows[0].id, "abc");
        assert_eq!(s.windows[0].toplevel.as_deref(), Some("abc"));
        assert_eq!(s.windows[0].icon, "foot");
        assert_eq!(s.focused_screen.as_deref(), Some("DP-1"));

        // Two outputs, each showing one workspace: both are active, but
        // the protocol does not say which output has the keyboard, so
        // neither is focused and no screen is.
        let two = ProtocolState {
            workspaces: vec![
                proto_ws(1, "1", "DP-1", true),
                proto_ws(2, "2", "DP-1", false),
                proto_ws(3, "3", "HDMI-A-1", true),
            ],
            ..proto.clone()
        };
        let s = merge(None, &[], &two);
        assert!(s.workspaces[0].active && s.workspaces[2].active);
        assert!(s.workspaces.iter().all(|w| !w.focused), "{s:?}");
        assert_eq!(s.focused_workspace(), None);
        assert_eq!(s.focused_screen, None);
        // A hidden active workspace does not count against the shown one.
        let hidden = ProtocolState {
            workspaces: vec![
                proto_ws(1, "1", "DP-1", true),
                ProtoWorkspace {
                    hidden: true,
                    ..proto_ws(2, "scratch", "DP-1", true)
                },
            ],
            ..proto
        };
        assert_eq!(
            merge(None, &[], &hidden).focused_workspace().map(|w| w.id),
            Some(1)
        );
    }

    fn managed(key: u64, app: &str, title: &str, screen: &str) -> ManagedToplevel {
        ManagedToplevel {
            key,
            title: title.into(),
            app_id: app.into(),
            screens: vec![screen.into()],
            ..Default::default()
        }
    }

    fn list(ident: &str, app: &str, title: &str) -> Toplevel {
        Toplevel {
            identifier: ident.into(),
            title: title.into(),
            app_id: app.into(),
        }
    }

    /// With no adapter, wlr-foreign-toplevel-management is the window
    /// source when bound: its ids, focus and state; the list identifier
    /// joined by app id and title; the keyboard's screen from the
    /// activated window.
    #[test]
    fn wlr_management_serves_windows_focus_and_state() {
        let mut a = managed(1, "foot", "~", "DP-1");
        a.activated = true;
        let mut b = managed(2, "firefox", "Strand", "HDMI-A-1");
        b.minimized = true;
        b.fullscreen = true;
        let proto = ProtocolState {
            connected: true,
            toplevel_list: true,
            toplevel_management: true,
            workspace_manager: true,
            toplevels: vec![
                list("x-ff", "firefox", "Strand"),
                list("x-foot", "foot", "~"),
            ],
            managed: vec![a, b],
            workspaces: vec![
                proto_ws(1, "1", "DP-1", true),
                proto_ws(2, "2", "DP-1", false),
                proto_ws(3, "3", "HDMI-A-1", true),
            ],
        };
        let s = merge(None, &[], &proto);
        let ids: Vec<&str> = s.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["wlr-1", "wlr-2"]);
        assert_eq!(s.focused_window().map(|w| w.id.as_str()), Some("wlr-1"));
        assert_eq!(s.windows[0].toplevel.as_deref(), Some("x-foot"));
        assert_eq!(s.windows[1].toplevel.as_deref(), Some("x-ff"));
        assert!(s.windows[1].minimized && s.windows[1].fullscreen);
        assert!(!s.windows[1].maximized);
        let mut maxed = proto.clone();
        maxed.managed[0].maximized = true;
        let s2 = merge(None, &[], &maxed);
        assert!(s2.windows[0].maximized && !s2.windows[0].fullscreen);
        assert_eq!(
            s.windows[0].workspace, None,
            "the protocols place no window"
        );
        assert_eq!(s.windows[0].icon, "foot");
        // Two outputs each show a workspace; the activated window says
        // which has the keyboard.
        assert_eq!(s.focused_screen.as_deref(), Some("DP-1"));
        assert_eq!(s.focused_workspace().map(|w| w.id), Some(1));

        // Focus moves to the window on HDMI-A-1.
        let mut moved = proto.clone();
        moved.managed[0].activated = false;
        moved.managed[1].activated = true;
        let s = merge(None, &[], &moved);
        assert_eq!(s.focused_window().map(|w| w.id.as_str()), Some("wlr-2"));
        assert_eq!(s.focused_screen.as_deref(), Some("HDMI-A-1"));
        assert_eq!(s.focused_workspace().map(|w| w.id), Some(3));

        // No window activated: the protocols cannot tell which of two
        // shown workspaces has the keyboard.
        let mut none = proto.clone();
        none.managed[0].activated = false;
        let s = merge(None, &[], &none);
        assert_eq!(s.focused_window(), None);
        assert_eq!(s.focused_workspace(), None);
        assert_eq!(s.focused_screen, None);

        // An activated window on two outputs names no screen either.
        let mut spanning = proto.clone();
        spanning.managed[0].screens.push("HDMI-A-1".into());
        let s = merge(None, &[], &spanning);
        assert_eq!(s.focused_window().map(|w| w.id.as_str()), Some("wlr-1"));
        assert_eq!(s.focused_screen, None);

        // Without wlr management the list serves windows, read-only.
        let read_only = ProtocolState {
            toplevel_management: false,
            managed: vec![],
            ..proto
        };
        let s = merge(None, &[], &read_only);
        assert_eq!(s.windows[0].id, "x-ff");
        assert_eq!(s.focused_window(), None);
    }

    /// The join by app id and title pairs twins in order, and gives up
    /// while the two protocols disagree on how many there are.
    #[test]
    fn wlr_toplevels_join_the_list_by_app_id_and_title_in_order() {
        let m = [
            managed(4, "foot", "~", "DP-1"),
            managed(7, "foot", "~", "DP-1"),
            managed(9, "foot", "vim", "DP-1"),
        ];
        let l = [
            list("a", "foot", "~"),
            list("b", "foot", "vim"),
            list("c", "foot", "~"),
        ];
        assert_eq!(
            join_managed(&m, &l),
            [Some("a".into()), Some("c".into()), Some("b".into())]
        );
        // The list has not seen the second `~` yet: neither twin joins.
        assert_eq!(join_managed(&m, &l[..2]), [None, None, Some("b".into())]);
        assert_eq!(join_managed(&m, &[]), [None, None, None]);
    }

    fn ipc_window(id: &str, app: &str, title: &str) -> Window {
        Window {
            id: id.into(),
            title: title.into(),
            app_id: app.into(),
            workspace: Some(1),
            ..Default::default()
        }
    }

    /// An adapter whose IPC says neither maximized nor fullscreen (niri)
    /// takes them from the wlr protocol: a lone app by its app id (even
    /// while its title differs between the two), twins by app id and
    /// title in order, nothing while twins disagree. An adapter that
    /// reports them (Hyprland, sway) keeps its own.
    #[test]
    fn an_adapter_without_window_states_takes_them_from_wlr() {
        let ipc = WmState {
            name: "niri".into(),
            workspaces: vec![Workspace {
                id: 1,
                name: "1".into(),
                focused: true,
                active: true,
                screen: "DP-1".into(),
                ..Default::default()
            }],
            windows: vec![
                ipc_window("10", "firefox", "Strand"),
                ipc_window("11", "foot", "~"),
                ipc_window("12", "foot", "~"),
            ],
            focused_screen: Some("DP-1".into()),
        };
        let mut ff = managed(1, "firefox", "Strand (old title)", "DP-1");
        ff.maximized = true;
        let foot_a = managed(2, "foot", "~", "DP-1");
        let mut foot_b = managed(3, "foot", "~", "DP-1");
        foot_b.fullscreen = true;
        let proto = ProtocolState {
            connected: true,
            toplevel_management: true,
            managed: vec![ff, foot_a, foot_b],
            ..Default::default()
        };
        let states = |s: &WmState| -> Vec<(bool, bool)> {
            s.windows
                .iter()
                .map(|w| (w.maximized, w.fullscreen))
                .collect()
        };
        let s = merge_with(Some(&ipc), &[], false, &proto);
        assert_eq!(
            states(&s),
            [(true, false), (false, false), (false, true)],
            "{s:?}"
        );
        assert!(
            s.workspaces[0].windows[2].fullscreen,
            "the workspace's copy too"
        );
        // One foot twin renamed on the wlr side only: twins no longer
        // agree, so neither foot has a state; firefox keeps its own.
        let mut renamed = proto.clone();
        renamed.managed[2].title = "vim".into();
        let s = merge_with(Some(&ipc), &[], false, &renamed);
        assert_eq!(states(&s), [(true, false), (false, false), (false, false)]);
        // No wlr protocol: neither state.
        let none = ProtocolState {
            toplevel_management: false,
            ..proto.clone()
        };
        let s = merge_with(Some(&ipc), &[], false, &none);
        assert_eq!(states(&s), [(false, false); 3]);
        // An adapter that reports them keeps its own.
        let mut own = ipc.clone();
        own.windows[1].fullscreen = true;
        assert_eq!(
            states(&merge(Some(&own), &[], &proto)),
            [(false, false), (false, true), (false, false)]
        );
    }

    /// Window actions with no adapter: wlr management takes them on its
    /// ids; without it they are `Unsupported`; an unknown id is
    /// `UnknownWindow`; with an adapter that has not come up they are
    /// `NotConnected`.
    #[tokio::test]
    async fn window_actions_route_to_wlr_management() {
        let (adapter_tx, _adapter_rx) = mpsc::unbounded_channel();
        let proto = ProtocolState {
            connected: true,
            toplevel_management: true,
            managed: vec![managed(3, "foot", "~", "DP-1")],
            ..Default::default()
        };
        // A protocol thread that has already ended answers what reaches
        // it `NotConnected`: that answer proves the request was routed
        // there (not refused as `Unsupported` or `UnknownWindow`).
        let dir = tempfile::tempdir().unwrap();
        let (ptx, mut prx) = mpsc::unbounded_channel();
        let client =
            ProtocolClient::spawn(WaylandTarget::Socket(dir.path().join("gone")), ptx).unwrap();
        let _ = prx.recv().await;
        let sender = client.sender();
        let ask = |action: WmAction, has_adapter: bool, proto: &ProtocolState| {
            let (req, reply) = WmRequest::new(action);
            route(req, has_adapter, None, proto, &adapter_tx, Some(&sender));
            reply
        };
        assert_eq!(
            ask(WmAction::CloseWindow("wlr-3".into()), false, &proto).await,
            Err(WmError::NotConnected),
            "routed to the protocol thread"
        );
        assert_eq!(
            ask(WmAction::FocusWindow("wlr-4".into()), false, &proto).await,
            Err(WmError::UnknownWindow("wlr-4".into()))
        );
        let read_only = ProtocolState {
            toplevel_management: false,
            ..proto.clone()
        };
        assert!(matches!(
            ask(WmAction::MinimizeWindow("wlr-3".into()), false, &read_only).await,
            Err(WmError::Unsupported(_))
        ));
        assert_eq!(
            ask(WmAction::MinimizeWindow("wlr-3".into()), true, &read_only).await,
            Err(WmError::NotConnected)
        );
        // `win.maximize()` and `win.fullscreen()` route the same way:
        // to the wlr protocol, refused on the read-only list.
        for action in [WmAction::MaximizeWindow, WmAction::FullscreenWindow] {
            assert_eq!(
                ask(action("wlr-3".into()), false, &proto).await,
                Err(WmError::NotConnected),
                "routed to the protocol thread"
            );
            assert_eq!(
                ask(action("wlr-9".into()), false, &proto).await,
                Err(WmError::UnknownWindow("wlr-9".into()))
            );
            assert!(matches!(
                ask(action("wlr-3".into()), false, &read_only).await,
                Err(WmError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn desktop_names_take_the_first_entry() {
        assert_eq!(first_desktop("sway:wlroots").as_deref(), Some("sway"));
        assert_eq!(first_desktop("COSMIC").as_deref(), Some("COSMIC"));
        assert_eq!(first_desktop(":labwc").as_deref(), Some("labwc"));
        assert_eq!(first_desktop(""), None);
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
            ..Default::default()
        };
        let s = merge(Some(&ipc), &[("7".into(), "x1".into())], &proto);
        assert_eq!(s.workspaces[0].id, 10, "IPC ids stand");
        assert!(s.workspaces[0].urgent);
        assert!(!s.workspaces[1].urgent);
        assert!(s.workspaces[1].active, "joined by screen when names repeat");
        assert_eq!(s.windows[0].title, "new");
        assert_eq!(s.windows[0].id, "7");
        assert_eq!(
            s.windows[0].toplevel.as_deref(),
            Some("x1"),
            "the join is kept"
        );
        assert!(s.workspaces[0].occupied);
        // No identifier: the IPC title stays.
        let s = merge(Some(&ipc), &[], &proto);
        assert_eq!(s.windows[0].title, "old");
        assert_eq!(s.windows[0].toplevel, None);

        // The IPC has `1` on two screens, the protocol only the one on
        // DP-2: only that one joins; DP-1's keeps its own screen and state.
        let lone = ProtocolState {
            workspaces: vec![ProtoWorkspace {
                urgent: true,
                ..proto_ws(2, "1", "DP-2", true)
            }],
            ..proto.clone()
        };
        let s = merge(Some(&ipc), &[], &lone);
        assert_eq!(s.workspaces[0].screen, "DP-1");
        assert!(!s.workspaces[0].urgent);
        assert_eq!(s.workspaces[1].screen, "DP-2");
        assert!(s.workspaces[1].urgent && s.workspaces[1].active);
        // A name the IPC does not repeat joins its lone match anywhere.
        let mut single = ipc.clone();
        single.workspaces.truncate(1);
        let s = merge(Some(&single), &[], &lone);
        assert_eq!(s.workspaces[0].screen, "DP-2");
    }

    /// With no adapter, no display and no requester left, the service
    /// says where its state comes from, publishes its empty state once the
    /// protocol thread gives up, and then waits quietly (it does not spin
    /// or panic once every source has ended).
    #[tokio::test]
    async fn every_source_gone_leaves_it_waiting() {
        let (tx, rx) = mpsc::unbounded_channel::<WmRequest>();
        drop(tx);
        let batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = batches.clone();
        let config = WmConfig {
            wayland: Some(WaylandTarget::Socket("/nonexistent/wayland-0".into())),
            desktop: Some("labwc".into()),
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
        assert_eq!(batches.len(), 2, "{batches:?}");
        assert!(
            batches[0]
                .iter()
                .any(|c| matches!(c, WmChange::Sources(s) if !s.toplevel_list))
        );
        assert!(
            batches[1].contains(&WmChange::Name("labwc".into())),
            "no adapter: wm.name is the desktop's"
        );
    }

    /// Built without the `niri` feature, a niri backend is no adapter:
    /// the protocols serve it, instead of waiting forever for an adapter
    /// that cannot run.
    #[cfg(not(feature = "niri"))]
    #[tokio::test]
    async fn niri_without_the_feature_is_the_protocols_alone() {
        let (_tx, rx) = mpsc::unbounded_channel::<WmRequest>();
        let batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = batches.clone();
        let config = WmConfig {
            backend: Some(Backend::Niri {
                socket: "/nonexistent/niri.sock".into(),
            }),
            ..Default::default()
        };
        let fut = run(config, move |b| seen.lock().unwrap().push(b), rx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), fut).await;
        let batches = batches.lock().unwrap();
        assert!(
            batches[0]
                .iter()
                .any(|c| matches!(c, WmChange::Sources(s) if s.ipc.is_none())),
            "{batches:?}"
        );
        assert!(
            batches[0]
                .iter()
                .any(|c| matches!(c, WmChange::Workspaces(_)))
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

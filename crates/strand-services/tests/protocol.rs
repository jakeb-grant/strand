//! The protocol client against a fake compositor that implements
//! `ext-foreign-toplevel-list-v1` and `ext-workspace-v1` (wayland-server),
//! since the sway in CI (1.9) offers neither.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use common::Collector;
use rustix::event::{PollFd, PollFlags};
use strand_services::wm::{
    self, ProtocolClient, ProtocolState, WaylandTarget, WmAction, WmConfig, WmError, WmRequest,
};
use tokio::sync::mpsc::unbounded_channel;
use wayland_protocols::ext::foreign_toplevel_list::v1::server::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::wl_output;
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
};

#[derive(Debug)]
enum Cmd {
    AddToplevel(&'static str, &'static str, &'static str),
    SetTitle(&'static str, &'static str),
    CloseToplevel(&'static str),
    AddWorkspace(&'static str, bool),
    /// A workspace in the group of output `n` (by the order of `outputs`).
    AddWorkspaceOn(&'static str, bool, usize),
    SetUrgent(&'static str, bool),
    RemoveWorkspace(&'static str),
    /// A workspace's `active` state, without the manager's `done`.
    SetActive(&'static str, bool),
    /// The manager's `done`.
    Done,
}

struct Toplevel {
    ident: String,
    title: String,
    app_id: String,
    handles: Vec<ExtForeignToplevelHandleV1>,
}

struct Ws {
    name: String,
    group: usize,
    active: bool,
    urgent: bool,
    handles: Vec<ExtWorkspaceHandleV1>,
}

#[derive(Default)]
struct Server {
    lists: Vec<ExtForeignToplevelListV1>,
    toplevels: Vec<Toplevel>,
    managers: Vec<ExtWorkspaceManagerV1>,
    /// Per manager, one group per output name.
    groups: Vec<Vec<ExtWorkspaceGroupHandleV1>>,
    output_names: Vec<String>,
    /// Bound outputs with the index of their name.
    outputs: Vec<(usize, wl_output::WlOutput)>,
    workspaces: Vec<Ws>,
    pending: Vec<String>,
    activated: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn send_toplevel(
        dh: &DisplayHandle,
        list: &ExtForeignToplevelListV1,
        t: &mut Toplevel,
    ) -> Option<()> {
        let client = list.client()?;
        let h = client
            .create_resource::<ExtForeignToplevelHandleV1, (), Server>(dh, list.version(), ())
            .ok()?;
        list.toplevel(&h);
        h.identifier(t.ident.clone());
        h.title(t.title.clone());
        h.app_id(t.app_id.clone());
        h.done();
        t.handles.push(h);
        Some(())
    }

    fn ws_state(ws: &Ws) -> ext_workspace_handle_v1::State {
        let mut s = ext_workspace_handle_v1::State::empty();
        if ws.active {
            s |= ext_workspace_handle_v1::State::Active;
        }
        if ws.urgent {
            s |= ext_workspace_handle_v1::State::Urgent;
        }
        s
    }

    fn send_workspace(
        dh: &DisplayHandle,
        manager: &ExtWorkspaceManagerV1,
        group: &ExtWorkspaceGroupHandleV1,
        ws: &mut Ws,
        index: usize,
    ) -> Option<()> {
        let client = manager.client()?;
        let h = client
            .create_resource::<ExtWorkspaceHandleV1, String, Server>(
                dh,
                manager.version(),
                ws.name.clone(),
            )
            .ok()?;
        manager.workspace(&h);
        h.id(format!("fake-{}", ws.name));
        h.name(ws.name.clone());
        h.coordinates((index as u32).to_ne_bytes().to_vec());
        h.state(Self::ws_state(ws));
        h.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
        group.workspace_enter(&h);
        ws.handles.push(h);
        Some(())
    }

    fn done(&self) {
        for m in &self.managers {
            m.done();
        }
    }

    fn apply(&mut self, dh: &DisplayHandle, cmd: Cmd) {
        match cmd {
            Cmd::AddToplevel(ident, title, app) => {
                let mut t = Toplevel {
                    ident: ident.into(),
                    title: title.into(),
                    app_id: app.into(),
                    handles: Vec::new(),
                };
                for l in &self.lists {
                    Self::send_toplevel(dh, l, &mut t);
                }
                self.toplevels.push(t);
            }
            Cmd::SetTitle(ident, title) => {
                if let Some(t) = self.toplevels.iter_mut().find(|t| t.ident == ident) {
                    t.title = title.into();
                    for h in &t.handles {
                        h.title(title.into());
                        h.done();
                    }
                }
            }
            Cmd::CloseToplevel(ident) => {
                if let Some(i) = self.toplevels.iter().position(|t| t.ident == ident) {
                    for h in &self.toplevels[i].handles {
                        h.closed();
                    }
                    self.toplevels.remove(i);
                }
            }
            Cmd::AddWorkspace(name, active) => self.apply(dh, Cmd::AddWorkspaceOn(name, active, 0)),
            Cmd::AddWorkspaceOn(name, active, group) => {
                let mut ws = Ws {
                    name: name.into(),
                    group,
                    active,
                    urgent: false,
                    handles: Vec::new(),
                };
                let index = self.workspaces.len();
                for (m, gs) in self.managers.iter().zip(&self.groups) {
                    Self::send_workspace(dh, m, &gs[group], &mut ws, index);
                }
                self.workspaces.push(ws);
                self.done();
            }
            Cmd::SetUrgent(name, urgent) => {
                if let Some(ws) = self.workspaces.iter_mut().find(|w| w.name == name) {
                    ws.urgent = urgent;
                    for h in &ws.handles {
                        h.state(Self::ws_state(ws));
                    }
                }
                self.done();
            }
            Cmd::SetActive(name, active) => {
                if let Some(ws) = self.workspaces.iter_mut().find(|w| w.name == name) {
                    ws.active = active;
                    for h in &ws.handles {
                        h.state(Self::ws_state(ws));
                    }
                }
            }
            Cmd::Done => self.done(),
            Cmd::RemoveWorkspace(name) => {
                if let Some(i) = self.workspaces.iter().position(|w| w.name == name) {
                    for h in &self.workspaces[i].handles {
                        for g in self.groups.iter().flatten() {
                            g.workspace_leave(h);
                        }
                        h.removed();
                    }
                    self.workspaces.remove(i);
                }
                self.done();
            }
        }
    }
}

/// Counts the connected clients.
struct ClientState(Arc<AtomicUsize>);
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl GlobalDispatch<ExtForeignToplevelListV1, ()> for Server {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        _: &Client,
        resource: New<ExtForeignToplevelListV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let list = init.init(resource, ());
        for t in &mut state.toplevels {
            Self::send_toplevel(dh, &list, t);
        }
        state.lists.push(list);
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        list: &ExtForeignToplevelListV1,
        request: ext_foreign_toplevel_list_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Request::Stop = request {
            list.finished();
            state.lists.retain(|l| l != list);
        }
    }
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelHandleV1,
        _: ext_foreign_toplevel_handle_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for Server {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let manager = init.init(resource, ());
        let mut groups = Vec::new();
        for n in 0..state.output_names.len() {
            let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, (), Server>(
                dh,
                manager.version(),
                (),
            ) else {
                return;
            };
            manager.workspace_group(&group);
            group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
            for (i, o) in &state.outputs {
                if *i == n && o.client().as_ref() == Some(client) {
                    group.output_enter(o);
                }
            }
            groups.push(group);
        }
        for (i, ws) in state.workspaces.iter_mut().enumerate() {
            Self::send_workspace(dh, &manager, &groups[ws.group], ws, i);
        }
        manager.done();
        state.managers.push(manager);
        state.groups.push(groups);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                for name in std::mem::take(&mut state.pending) {
                    state.activated.lock().unwrap().push(name.clone());
                    let Some(group) = state
                        .workspaces
                        .iter()
                        .find(|w| w.name == name)
                        .map(|w| w.group)
                    else {
                        continue;
                    };
                    // Activation is per group (output).
                    for ws in state.workspaces.iter_mut().filter(|w| w.group == group) {
                        ws.active = ws.name == name;
                        for h in &ws.handles {
                            h.state(Server::ws_state(ws));
                        }
                    }
                }
                state.done();
            }
            ext_workspace_manager_v1::Request::Stop => manager.finished(),
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtWorkspaceGroupHandleV1,
        _: ext_workspace_group_handle_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceHandleV1, String> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        name: &String,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ext_workspace_handle_v1::Request::Activate = request {
            state.pending.push(name.clone());
        }
    }
}

impl GlobalDispatch<wl_output::WlOutput, usize> for Server {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        client: &Client,
        resource: New<wl_output::WlOutput>,
        index: &usize,
        init: &mut DataInit<'_, Self>,
    ) {
        let o = init.init(resource, *index);
        if o.version() >= 4 {
            o.name(state.output_names[*index].clone());
        }
        if o.version() >= 2 {
            o.done();
        }
        for (m, gs) in state.managers.iter().zip(&state.groups) {
            if m.client().as_ref() == Some(client) {
                gs[*index].output_enter(&o);
                m.done();
            }
        }
        state.outputs.push((*index, o));
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        _: wl_output::Request,
        _: &usize,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

/// The fake compositor on its own thread.
struct Fake {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    tx: mpsc::Sender<Cmd>,
    stop: Arc<AtomicBool>,
    activated: Arc<Mutex<Vec<String>>>,
    clients: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    fn start(with_workspaces: bool) -> Fake {
        Self::start_with(with_workspaces, &["FAKE-1"])
    }

    /// A fake with one `wl_output` (and one workspace group) per name.
    fn start_with(with_workspaces: bool, outputs: &'static [&'static str]) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wayland-fake");
        let listener = ListeningSocket::bind_absolute(socket.clone()).unwrap();
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stop = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = stop.clone();
        let thread_activated = activated.clone();
        let clients = Arc::new(AtomicUsize::new(0));
        let thread_clients = clients.clone();
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Server, ExtForeignToplevelListV1, ()>(1, ());
            if with_workspaces {
                dh.create_global::<Server, ExtWorkspaceManagerV1, ()>(1, ());
            }
            for i in 0..outputs.len() {
                dh.create_global::<Server, wl_output::WlOutput, usize>(4, i);
            }
            let mut state = Server {
                activated: thread_activated,
                output_names: outputs.iter().map(|o| o.to_string()).collect(),
                ..Default::default()
            };
            while !thread_stop.load(Ordering::SeqCst) {
                while let Ok(cmd) = rx.try_recv() {
                    state.apply(&dh, cmd);
                }
                if let Ok(Some(stream)) = listener.accept() {
                    thread_clients.fetch_add(1, Ordering::SeqCst);
                    display
                        .handle()
                        .insert_client(stream, Arc::new(ClientState(thread_clients.clone())))
                        .unwrap();
                }
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                let poll_fd = display.backend().poll_fd().try_clone_to_owned().unwrap();
                let mut fds = [
                    PollFd::new(&listener, PollFlags::IN),
                    PollFd::new(&poll_fd, PollFlags::IN),
                ];
                let timeout = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 10_000_000,
                };
                let _ = rustix::event::poll(&mut fds, Some(&timeout));
            }
        });
        Fake {
            _dir: dir,
            socket,
            tx,
            stop,
            activated,
            clients,
            thread: Some(thread),
        }
    }

    /// Clients connected now.
    fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    fn cmd(&self, c: Cmd) {
        self.tx.send(c).unwrap();
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn next_matching(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ProtocolState>,
    what: &str,
    f: impl Fn(&ProtocolState) -> bool,
) -> ProtocolState {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = rx.recv().await.expect("the client stopped");
            if f(&s) {
                return s;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: timed out"))
}

#[tokio::test]
async fn protocol_client_follows_toplevels_and_workspaces() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    // Let the fake apply them before the client connects.
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    let s = next_matching(&mut rx, "first", |s| s.connected).await;
    assert!(s.toplevel_list && s.workspace_manager);
    assert_eq!(s.toplevels.len(), 1);
    assert_eq!(s.toplevels[0].identifier, "tl-1");
    assert_eq!(s.toplevels[0].app_id, "foot");
    assert_eq!(
        s.workspaces
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>(),
        ["1", "2"]
    );
    assert!(s.workspaces[0].active && s.workspaces[0].can_activate);
    assert_eq!(s.workspaces[0].id.as_deref(), Some("fake-1"));
    assert_eq!(s.workspaces[1].coordinates, [1]);
    // The group's output, named by wl_output v4, may come in a later done.
    let named = |s: &ProtocolState| s.workspaces.iter().all(|w| w.screens == ["FAKE-1"]);
    let s = if named(&s) {
        s
    } else {
        next_matching(&mut rx, "screens", named).await
    };
    let key2 = s.workspaces[1].key;

    // A new toplevel, a title change, a close: one state per `done`.
    fake.cmd(Cmd::AddToplevel("tl-2", "Firefox", "firefox"));
    next_matching(&mut rx, "open", |s| s.toplevels.len() == 2).await;
    fake.cmd(Cmd::SetTitle("tl-2", "Strand — Firefox"));
    next_matching(&mut rx, "title", |s| {
        s.toplevels.iter().any(|t| t.title == "Strand — Firefox")
    })
    .await;
    fake.cmd(Cmd::CloseToplevel("tl-1"));
    let s = next_matching(&mut rx, "close", |s| s.toplevels.len() == 1).await;
    assert_eq!(s.toplevels[0].identifier, "tl-2");

    // Workspace state, a new one, a removal.
    fake.cmd(Cmd::SetUrgent("2", true));
    next_matching(&mut rx, "urgent", |s| {
        s.workspaces.iter().any(|w| w.name == "2" && w.urgent)
    })
    .await;
    fake.cmd(Cmd::AddWorkspace("3", false));
    next_matching(&mut rx, "added", |s| s.workspaces.len() == 3).await;
    fake.cmd(Cmd::RemoveWorkspace("3"));
    let s = next_matching(&mut rx, "removed", |s| s.workspaces.len() == 2).await;
    assert_eq!(s.workspaces[1].key, key2, "keys are stable");

    // Idle: nothing is sent.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err()
    );
    drop(client);
    next_matching(&mut rx, "stopped", |s| !s.connected).await;
}

#[tokio::test]
async fn the_protocols_alone_serve_workspaces_and_windows() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: None,
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        events: None,
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| {
        m.sources.workspace_protocol
            && m.workspaces.len() == 2
            && m.workspaces.iter().all(|(_, w)| w.screen == "FAKE-1")
    })
    .await;
    assert!(c.mirror.sources.toplevel_list && c.mirror.sources.ipc.is_none());
    assert_eq!(c.mirror.name, "");
    assert_eq!(c.mirror.windows[0].1.id, "tl-1");
    assert_eq!(c.mirror.windows[0].1.icon, "foot");
    assert_eq!(c.mirror.focused_workspace.as_ref().unwrap().name, "1");
    let ws2 = c.mirror.workspace("2").unwrap().id;

    // ws.focus() activates through ext-workspace-v1.
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("activated", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    assert_eq!(*fake.activated.lock().unwrap(), ["2"]);

    // The list protocol has no window actions.
    let (r, done) = WmRequest::new(WmAction::CloseWindow("tl-1".into()));
    req_tx.send(r).unwrap();
    assert!(matches!(done.await, Err(WmError::Unsupported(_))));
    service.abort();
}

/// Two outputs, each showing a workspace: `ext-workspace-v1` says what
/// each output shows, not which has the keyboard, so with the protocols
/// alone no workspace is `focused` (each shown one is `active`).
#[tokio::test]
async fn two_outputs_alone_mark_active_workspaces_not_focus() {
    let fake = Fake::start_with(true, &["FAKE-1", "FAKE-2"]);
    fake.cmd(Cmd::AddWorkspaceOn("1", true, 0));
    fake.cmd(Cmd::AddWorkspaceOn("2", false, 0));
    fake.cmd(Cmd::AddWorkspaceOn("3", true, 1));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        desktop: Some("labwc".into()),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| {
        m.workspaces.len() == 3 && m.workspaces.iter().all(|(_, w)| !w.screen.is_empty())
    })
    .await;
    let m = &c.mirror;
    assert_eq!(m.name, "labwc", "wm.name falls back to XDG_CURRENT_DESKTOP");
    assert_eq!(m.workspace("1").unwrap().screen, "FAKE-1");
    assert_eq!(m.workspace("3").unwrap().screen, "FAKE-2");
    assert!(m.workspace("1").unwrap().active && m.workspace("3").unwrap().active);
    assert!(m.workspaces.iter().all(|(_, w)| !w.focused), "{m:#?}");
    assert_eq!(m.focused_workspace, None);
    assert_eq!(m.focused_screen, None);

    // Activating 2 changes what FAKE-1 shows, not FAKE-2.
    let ws2 = m.workspace("2").unwrap().id;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("activated", |m| m.workspace("2").is_some_and(|w| w.active))
        .await;
    assert!(!c.mirror.workspace("1").unwrap().active);
    assert!(c.mirror.workspace("3").unwrap().active);
    assert_eq!(c.mirror.focused_workspace, None);
    service.abort();
}

/// An adapter that cannot connect (here: Hyprland's sockets are gone)
/// does not hide the standard protocols: their state is published, the
/// sources say the adapter is down, and what the protocol can do works.
#[tokio::test]
async fn a_broken_adapter_does_not_hide_the_protocols() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    std::thread::sleep(Duration::from_millis(50));
    let gone = tempfile::tempdir().unwrap();
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(wm::Backend::hyprland_in(gone.path(), "stale")),
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("protocol state", |m| m.workspaces.len() == 2).await;
    let m = &c.mirror;
    assert_eq!(m.sources.ipc, Some(wm::CompositorKind::Hyprland));
    assert!(!m.sources.connected && m.sources.workspace_protocol);
    assert_eq!(m.name, "Hyprland");
    assert_eq!(m.windows[0].1.id, "tl-1");
    assert!(
        matches!(
            &c.log[0],
            wm::WmChange::Sources(s) if s.ipc == Some(wm::CompositorKind::Hyprland) && !s.connected
        ),
        "the first batch says which adapter is meant to run: {:?}",
        c.log[0]
    );
    let ws2 = c.mirror.workspace("2").unwrap().id;
    let (r, done) = WmRequest::new(WmAction::FocusWorkspace(ws2));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("activated", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    // Window actions need the adapter.
    let (r, done) = WmRequest::new(WmAction::CloseWindow("tl-1".into()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::NotConnected));
    service.abort();
}

#[tokio::test]
async fn a_compositor_without_ext_workspace_gives_windows_only() {
    let fake = Fake::start(false);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let _client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    let s = next_matching(&mut rx, "first", |s| s.connected).await;
    assert!(s.toplevel_list && !s.workspace_manager);
    assert!(s.workspaces.is_empty());
    assert_eq!(s.toplevels.len(), 1);
}

#[tokio::test]
async fn no_display_reports_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = unbounded_channel();
    let _client =
        ProtocolClient::spawn(WaylandTarget::Socket(dir.path().join("nothing")), tx).unwrap();
    let s = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s, ProtocolState::default());
}

/// A compositor that accepts the connection and never answers: the
/// startup waits in the same poll loop as the rest, so dropping the client
/// still ends its thread and closes the connection (no leaked thread per
/// subscribe while a compositor hangs).
#[tokio::test]
async fn a_hung_compositor_does_not_keep_the_thread() {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wayland-hung");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (tx, mut rx) = unbounded_channel();
    let client = ProtocolClient::spawn(WaylandTarget::Socket(path), tx).unwrap();
    let (mut conn, _) = listener.accept().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "no state from a compositor that never answered"
    );
    drop(client);
    let last = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the thread did not stop");
    assert_eq!(last, Some(ProtocolState::default()));
    let end = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
    assert_eq!(end, Ok(None), "the thread has ended");
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut sent = Vec::new();
    conn.read_to_end(&mut sent)
        .expect("the connection is closed, not left open");
    assert!(!sent.is_empty(), "it had asked for the registry");
}

/// A workspace transaction split across reads (a toplevel's `done` read
/// between its halves) is published only whole, at the manager's `done`:
/// no state ever shows the old workspace inactive and the new one not yet
/// active.
#[tokio::test]
async fn workspace_changes_apply_at_the_managers_done() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("1", true));
    fake.cmd(Cmd::AddWorkspace("2", false));
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let _client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    next_matching(&mut rx, "first", |s| s.connected && s.workspaces.len() == 2).await;
    while rx.try_recv().is_ok() {}

    // The first half, then a toplevel's update, in one flush.
    fake.cmd(Cmd::SetActive("1", false));
    fake.cmd(Cmd::SetTitle("tl-1", "vim"));
    let s = next_matching(&mut rx, "title", |s| s.toplevels[0].title == "vim").await;
    let active = |s: &ProtocolState| -> Vec<String> {
        s.workspaces
            .iter()
            .filter(|w| w.active)
            .map(|w| w.name.clone())
            .collect()
    };
    assert_eq!(active(&s), ["1"], "the half-applied switch is not shown");
    std::thread::sleep(Duration::from_millis(100));
    // The second half and the manager's `done`.
    fake.cmd(Cmd::SetActive("2", true));
    fake.cmd(Cmd::Done);
    let s = next_matching(&mut rx, "switched", |s| active(s) == ["2"]).await;
    assert_eq!(s.toplevels[0].title, "vim");
}

/// Hyprland reports each window's `ext-foreign-toplevel-list-v1`
/// identifier as `stableId` in `j/clients`: the protocol's title wins for
/// the windows it joins; the others keep the IPC's.
#[tokio::test]
async fn hyprland_windows_join_the_toplevel_list_by_stable_id() {
    let hypr = common::hyprland::FakeHyprland::start();
    let fake = Fake::start(true);
    // kitty's stableId in the fixture is "a"; pavucontrol's "b" has no
    // toplevel here.
    fake.cmd(Cmd::AddToplevel("a", "~ (protocol)", "kitty"));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(hypr.backend.clone()),
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("joined", |m| {
        m.sources.connected
            && m.window_by_app("kitty")
                .is_some_and(|w| w.title == "~ (protocol)")
    })
    .await;
    let kitty = c.mirror.window_by_app("kitty").unwrap();
    assert_eq!(kitty.id, "0x55d0c0a1b2c0", "IPC ids stand");
    assert_eq!(
        c.mirror
            .window_by_app("org.pulseaudio.pavucontrol")
            .unwrap()
            .title,
        "Volume Control"
    );
    fake.cmd(Cmd::SetTitle("a", "vim (protocol)"));
    c.until("retitled", |m| {
        m.window_by_app("kitty")
            .is_some_and(|w| w.title == "vim (protocol)")
    })
    .await;
    service.abort();
}

/// Dropping the hub while a store still holds a subscription stops the
/// service (its protocol connection goes) and ends the stream.
#[tokio::test]
async fn dropping_the_hub_stops_the_service_its_subscriptions_held() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddWorkspace("1", true));
    std::thread::sleep(Duration::from_millis(50));
    let hub = wm::WmHub::new(
        WmConfig {
            wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
            ..Default::default()
        },
        tokio::runtime::Handle::current(),
    );
    let mut sub = hub.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.clients() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "never connected");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(hub);
    while fake.clients() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the protocol connection outlived the hub"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while sub.recv().await.is_some() {}
    });
    assert!(end.await.is_ok(), "the stream ends");
}

/// An adapter that comes up after the protocols' state went out replaces
/// it with a `Reset` of both lists, not a keyed diff: the protocol's
/// workspace 1 and Hyprland's workspace 1 are not the same item.
#[tokio::test]
async fn a_late_adapter_resets_the_lists() {
    let fake = Fake::start(true);
    fake.cmd(Cmd::AddToplevel("x", "~", "foot"));
    fake.cmd(Cmd::AddWorkspace("web", true));
    std::thread::sleep(Duration::from_millis(50));
    let late = tempfile::tempdir().unwrap();
    let backend = wm::Backend::hyprland_in(late.path(), "late");
    let (sink, mut c) = Collector::new();
    let (_req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        backend: Some(backend.clone()),
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("protocol state", |m| m.workspace_names() == ["web"])
        .await;
    assert_eq!(c.mirror.workspaces[0].0, 1, "the protocol's key 1");

    let _hypr = common::hyprland::FakeHyprland::start_at(backend);
    c.until_within(10, "adapter up", |m| {
        m.sources.connected && m.workspace_names() == ["1", "2", "3"]
    })
    .await;
    let resets = |which: fn(&wm::WmChange) -> bool| c.log.iter().filter(|ch| which(ch)).count();
    let ws_resets = resets(|ch| {
        matches!(ch, wm::WmChange::Workspaces(d)
            if d.iter().any(|d| matches!(d, strand_core::keyed::VecDiff::Reset { .. })))
    });
    let win_resets = resets(|ch| {
        matches!(ch, wm::WmChange::Windows(d)
            if d.iter().any(|d| matches!(d, strand_core::keyed::VecDiff::Reset { .. })))
    });
    assert_eq!((ws_resets, win_resets), (2, 2), "{:#?}", c.log);
    service.abort();
}

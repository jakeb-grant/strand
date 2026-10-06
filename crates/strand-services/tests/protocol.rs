//! The protocol client against a fake compositor that implements
//! `ext-foreign-toplevel-list-v1` and `ext-workspace-v1` (wayland-server),
//! since the sway in CI (1.9) offers neither.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
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
    SetUrgent(&'static str, bool),
    RemoveWorkspace(&'static str),
}

struct Toplevel {
    ident: String,
    title: String,
    app_id: String,
    handles: Vec<ExtForeignToplevelHandleV1>,
}

struct Ws {
    name: String,
    active: bool,
    urgent: bool,
    handles: Vec<ExtWorkspaceHandleV1>,
}

#[derive(Default)]
struct Server {
    lists: Vec<ExtForeignToplevelListV1>,
    toplevels: Vec<Toplevel>,
    managers: Vec<ExtWorkspaceManagerV1>,
    groups: Vec<ExtWorkspaceGroupHandleV1>,
    outputs: Vec<wl_output::WlOutput>,
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
            Cmd::AddWorkspace(name, active) => {
                let mut ws = Ws {
                    name: name.into(),
                    active,
                    urgent: false,
                    handles: Vec::new(),
                };
                let index = self.workspaces.len();
                for (m, g) in self.managers.iter().zip(&self.groups) {
                    Self::send_workspace(dh, m, g, &mut ws, index);
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
            Cmd::RemoveWorkspace(name) => {
                if let Some(i) = self.workspaces.iter().position(|w| w.name == name) {
                    for h in &self.workspaces[i].handles {
                        for g in &self.groups {
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

struct ClientState;
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
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
        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, (), Server>(
            dh,
            manager.version(),
            (),
        ) else {
            return;
        };
        manager.workspace_group(&group);
        group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
        for o in &state.outputs {
            if o.client().as_ref() == Some(client) {
                group.output_enter(o);
            }
        }
        for (i, ws) in state.workspaces.iter_mut().enumerate() {
            Self::send_workspace(dh, &manager, &group, ws, i);
        }
        manager.done();
        state.managers.push(manager);
        state.groups.push(group);
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
                    for ws in &mut state.workspaces {
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

impl GlobalDispatch<wl_output::WlOutput, ()> for Server {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        client: &Client,
        resource: New<wl_output::WlOutput>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let o = init.init(resource, ());
        if o.version() >= 4 {
            o.name("FAKE-1".into());
        }
        if o.version() >= 2 {
            o.done();
        }
        for (m, g) in state.managers.iter().zip(&state.groups) {
            if g.client().as_ref() == Some(client) {
                g.output_enter(&o);
                m.done();
            }
        }
        state.outputs.push(o);
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        _: wl_output::Request,
        _: &(),
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
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    fn start(with_workspaces: bool) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wayland-fake");
        let listener = ListeningSocket::bind_absolute(socket.clone()).unwrap();
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stop = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = stop.clone();
        let thread_activated = activated.clone();
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Server, ExtForeignToplevelListV1, ()>(1, ());
            if with_workspaces {
                dh.create_global::<Server, ExtWorkspaceManagerV1, ()>(1, ());
            }
            dh.create_global::<Server, wl_output::WlOutput, ()>(4, ());
            let mut state = Server {
                activated: thread_activated,
                ..Default::default()
            };
            while !thread_stop.load(Ordering::SeqCst) {
                while let Ok(cmd) = rx.try_recv() {
                    state.apply(&dh, cmd);
                }
                if let Ok(Some(stream)) = listener.accept() {
                    display
                        .handle()
                        .insert_client(stream, Arc::new(ClientState))
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
            thread: Some(thread),
        }
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
    assert_eq!(done.await.unwrap(), Ok(()));
    c.until("activated", |m| {
        m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    assert_eq!(*fake.activated.lock().unwrap(), ["2"]);

    // The list protocol has no window actions.
    let (r, done) = WmRequest::new(WmAction::CloseWindow("tl-1".into()));
    req_tx.send(r).unwrap();
    assert!(matches!(done.await.unwrap(), Err(WmError::Unsupported(_))));
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

//! The protocol client against a fake compositor that implements
//! `ext-foreign-toplevel-list-v1` and `ext-workspace-v1` (wayland-server),
//! since the sway in CI (1.9) offers neither, and optionally
//! `zwlr_foreign_toplevel_management_v1` with a seat: the shape of labwc,
//! a compositor with no IPC adapter.

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
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, GlobalId};
use wayland_server::protocol::{wl_output, wl_seat};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New,
    Resource,
};

#[derive(Debug)]
enum Cmd {
    AddToplevel(&'static str, &'static str, &'static str),
    /// A toplevel on output `n` (by the order of `outputs`).
    AddToplevelOn(&'static str, &'static str, &'static str, usize),
    /// Activates a toplevel (and deactivates the others), as a click would.
    Activate(&'static str),
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
    /// Removes seat `n`'s global (by the order they were offered), as a
    /// seat going away would.
    RemoveSeat(usize),
}

struct Toplevel {
    ident: String,
    title: String,
    app_id: String,
    /// The output it is on (by the order of `outputs`).
    output: usize,
    activated: bool,
    minimized: bool,
    handles: Vec<ExtForeignToplevelHandleV1>,
    wlr: Vec<ZwlrForeignToplevelHandleV1>,
}

impl Toplevel {
    fn new(ident: &str, title: &str, app_id: &str, output: usize) -> Self {
        Toplevel {
            ident: ident.into(),
            title: title.into(),
            app_id: app_id.into(),
            output,
            activated: false,
            minimized: false,
            handles: Vec::new(),
            wlr: Vec::new(),
        }
    }

    /// Its wlr state array.
    fn wlr_state(&self) -> Vec<u8> {
        let mut values = Vec::new();
        if self.activated {
            values.push(u32::from(zwlr_foreign_toplevel_handle_v1::State::Activated));
        }
        if self.minimized {
            values.push(u32::from(zwlr_foreign_toplevel_handle_v1::State::Minimized));
        }
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    fn send_wlr_state(&self) {
        for h in &self.wlr {
            h.state(self.wlr_state());
            h.done();
        }
    }
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
    wlr_managers: Vec<ZwlrForeignToplevelManagerV1>,
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
    /// The wlr requests received: `activate <ident>`, `close <ident>`,
    /// `minimize <ident>`; an `activate` naming a seat other than the
    /// first offered is logged `activate <ident> on seat <n>`.
    wlr_requests: Arc<Mutex<Vec<String>>>,
    /// The seat globals, by index; `None` once removed.
    seat_globals: Vec<Option<GlobalId>>,
}

impl Server {
    fn send_wlr_toplevel(
        dh: &DisplayHandle,
        manager: &ZwlrForeignToplevelManagerV1,
        outputs: &[(usize, wl_output::WlOutput)],
        t: &mut Toplevel,
    ) -> Option<()> {
        let client = manager.client()?;
        let h = client
            .create_resource::<ZwlrForeignToplevelHandleV1, String, Server>(
                dh,
                manager.version(),
                t.ident.clone(),
            )
            .ok()?;
        manager.toplevel(&h);
        h.title(t.title.clone());
        h.app_id(t.app_id.clone());
        for (i, o) in outputs {
            if *i == t.output && o.client().as_ref() == Some(&client) {
                h.output_enter(o);
            }
        }
        h.state(t.wlr_state());
        h.done();
        t.wlr.push(h);
        Some(())
    }

    fn close(&mut self, ident: &str) {
        if let Some(i) = self.toplevels.iter().position(|t| t.ident == ident) {
            for h in &self.toplevels[i].handles {
                h.closed();
            }
            for h in &self.toplevels[i].wlr {
                h.closed();
            }
            self.toplevels.remove(i);
        }
    }

    fn activate(&mut self, ident: &str) {
        for t in &mut self.toplevels {
            let on = t.ident == ident;
            if t.activated != on || (on && t.minimized) {
                t.activated = on;
                t.minimized &= !on;
                t.send_wlr_state();
            }
        }
    }

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
                self.apply(dh, Cmd::AddToplevelOn(ident, title, app, 0));
            }
            Cmd::AddToplevelOn(ident, title, app, output) => {
                let mut t = Toplevel::new(ident, title, app, output);
                for l in &self.lists {
                    Self::send_toplevel(dh, l, &mut t);
                }
                for m in &self.wlr_managers {
                    Self::send_wlr_toplevel(dh, m, &self.outputs, &mut t);
                }
                self.toplevels.push(t);
            }
            Cmd::Activate(ident) => self.activate(ident),
            Cmd::SetTitle(ident, title) => {
                if let Some(t) = self.toplevels.iter_mut().find(|t| t.ident == ident) {
                    t.title = title.into();
                    for h in &t.handles {
                        h.title(title.into());
                        h.done();
                    }
                    for h in &t.wlr {
                        h.title(title.into());
                        h.done();
                    }
                }
            }
            Cmd::CloseToplevel(ident) => self.close(ident),
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
            Cmd::RemoveSeat(n) => {
                if let Some(id) = self.seat_globals.get_mut(n).and_then(Option::take) {
                    dh.remove_global::<Server>(id);
                }
            }
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
        for t in state.toplevels.iter().filter(|t| t.output == *index) {
            for h in &t.wlr {
                if h.client().as_ref() == Some(client) {
                    h.output_enter(&o);
                    h.done();
                }
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

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for Server {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let manager = init.init(resource, ());
        for t in &mut state.toplevels {
            Self::send_wlr_toplevel(dh, &manager, &state.outputs, t);
        }
        state.wlr_managers.push(manager);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state.wlr_managers.retain(|m| m != manager);
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, String> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        ident: &String,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let log = |what: &str| {
            state
                .wlr_requests
                .lock()
                .unwrap()
                .push(format!("{what} {ident}"));
        };
        match request {
            Request::Activate { seat } => {
                match seat.data::<usize>() {
                    Some(n) if *n > 0 => state
                        .wlr_requests
                        .lock()
                        .unwrap()
                        .push(format!("activate {ident} on seat {n}")),
                    _ => log("activate"),
                }
                state.activate(ident);
            }
            Request::Close => {
                log("close");
                state.close(ident);
            }
            Request::SetMinimized => {
                log("minimize");
                if let Some(t) = state.toplevels.iter_mut().find(|t| t.ident == *ident) {
                    t.minimized = true;
                    t.activated = false;
                    t.send_wlr_state();
                }
            }
            Request::Destroy => {
                for t in &mut state.toplevels {
                    t.wlr.retain(|h| h != handle);
                }
            }
            _ => {}
        }
    }
}

impl GlobalDispatch<wl_seat::WlSeat, usize> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_seat::WlSeat>,
        n: &usize,
        init: &mut DataInit<'_, Self>,
    ) {
        let seat = init.init(resource, *n);
        seat.capabilities(wl_seat::Capability::Keyboard);
    }
}

impl Dispatch<wl_seat::WlSeat, usize> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        _: wl_seat::Request,
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
    wlr_requests: Arc<Mutex<Vec<String>>>,
    clients: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    fn start(with_workspaces: bool) -> Fake {
        Self::start_with(with_workspaces, &["FAKE-1"])
    }

    /// A fake with one `wl_output` (and one workspace group) per name.
    fn start_with(with_workspaces: bool, outputs: &'static [&'static str]) -> Fake {
        Self::start_opts(with_workspaces, false, outputs)
    }

    /// A fake that also offers `zwlr_foreign_toplevel_management_v1` (v3)
    /// and a seat.
    fn start_opts(with_workspaces: bool, wlr: bool, outputs: &'static [&'static str]) -> Fake {
        Self::start_seats(with_workspaces, wlr, usize::from(wlr), outputs)
    }

    /// [`Fake::start_opts`] with `seats` seat globals.
    fn start_seats(
        with_workspaces: bool,
        wlr: bool,
        seats: usize,
        outputs: &'static [&'static str],
    ) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wayland-fake");
        let listener = ListeningSocket::bind_absolute(socket.clone()).unwrap();
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stop = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = stop.clone();
        let thread_activated = activated.clone();
        let wlr_requests = Arc::new(Mutex::new(Vec::new()));
        let thread_wlr_requests = wlr_requests.clone();
        let clients = Arc::new(AtomicUsize::new(0));
        let thread_clients = clients.clone();
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Server, ExtForeignToplevelListV1, ()>(1, ());
            if with_workspaces {
                dh.create_global::<Server, ExtWorkspaceManagerV1, ()>(1, ());
            }
            if wlr {
                dh.create_global::<Server, ZwlrForeignToplevelManagerV1, ()>(3, ());
            }
            let seat_globals = (0..seats)
                .map(|n| Some(dh.create_global::<Server, wl_seat::WlSeat, usize>(1, n)))
                .collect();
            for i in 0..outputs.len() {
                dh.create_global::<Server, wl_output::WlOutput, usize>(4, i);
            }
            let mut state = Server {
                activated: thread_activated,
                wlr_requests: thread_wlr_requests,
                output_names: outputs.iter().map(|o| o.to_string()).collect(),
                seat_globals,
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
            wlr_requests,
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

/// labwc's shape: no IPC adapter; `ext-foreign-toplevel-list-v1`,
/// `zwlr_foreign_toplevel_management_v1` and `ext-workspace-v1` on two
/// outputs. Windows come from the wlr protocol (ids, focus, minimized),
/// each joined to its list identifier; the activated window says which
/// output has the keyboard, so the workspace shown there is focused; the
/// window actions reach the compositor as wlr requests.
#[tokio::test]
async fn wlr_management_serves_focus_state_and_window_actions() {
    let fake = Fake::start_opts(true, true, &["FAKE-1", "FAKE-2"]);
    fake.cmd(Cmd::AddWorkspaceOn("1", true, 0));
    fake.cmd(Cmd::AddWorkspaceOn("2", true, 1));
    fake.cmd(Cmd::AddToplevelOn("tl-1", "~", "foot", 0));
    fake.cmd(Cmd::AddToplevelOn("tl-2", "Firefox", "firefox", 1));
    fake.cmd(Cmd::Activate("tl-1"));
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
        m.windows.len() == 2
            && m.focused_screen.as_deref() == Some("FAKE-1")
            && m.windows.iter().all(|(_, w)| w.toplevel.is_some())
    })
    .await;
    let m = &c.mirror;
    assert!(m.sources.toplevel_management && m.sources.toplevel_list);
    assert_eq!(m.sources.ipc, None);
    assert_eq!(m.name, "labwc");
    let foot = m.window_by_app("foot").unwrap().clone();
    let firefox = m.window_by_app("firefox").unwrap().clone();
    assert!(foot.id.starts_with("wlr-"), "{foot:?}");
    assert_eq!(foot.toplevel.as_deref(), Some("tl-1"));
    assert_eq!(firefox.toplevel.as_deref(), Some("tl-2"));
    assert_eq!(m.focused_window.as_ref().map(|w| &w.id), Some(&foot.id));
    assert_eq!(
        m.focused_workspace.as_ref().map(|w| w.name.as_str()),
        Some("1"),
        "the workspace shown on the keyboard's output"
    );

    // Focus moves from outside: the other output's workspace is focused.
    fake.cmd(Cmd::Activate("tl-2"));
    c.until("firefox focused", |m| {
        m.focused_window
            .as_ref()
            .is_some_and(|w| w.id == firefox.id)
            && m.focused_workspace.as_ref().is_some_and(|w| w.name == "2")
    })
    .await;
    assert_eq!(c.mirror.focused_screen.as_deref(), Some("FAKE-2"));
    assert!(!c.mirror.window_by_app("foot").unwrap().focused);

    // `win.focus()`, `win.minimize()`, `win.close()`.
    let (r, done) = WmRequest::new(WmAction::FocusWindow(foot.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("foot activated", |m| {
        m.focused_window.as_ref().is_some_and(|w| w.id == foot.id)
    })
    .await;
    let (r, done) = WmRequest::new(WmAction::MinimizeWindow(foot.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("foot minimized", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.minimized && !w.focused)
    })
    .await;
    assert_eq!(c.mirror.focused_window, None);
    let (r, done) = WmRequest::new(WmAction::CloseWindow(firefox.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Ok(()));
    c.until("firefox closed", |m| m.window_by_app("firefox").is_none())
        .await;
    assert_eq!(
        *fake.wlr_requests.lock().unwrap(),
        ["activate tl-1", "minimize tl-1", "close tl-2"]
    );

    // A title change is one keyed update; an unknown window is refused.
    fake.cmd(Cmd::SetTitle("tl-1", "vim"));
    c.until("retitled", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "vim" && w.id == foot.id)
    })
    .await;
    let (r, done) = WmRequest::new(WmAction::CloseWindow(firefox.id.clone()));
    req_tx.send(r).unwrap();
    assert_eq!(done.await, Err(WmError::UnknownWindow(firefox.id.clone())));

    // Idle: nothing is sent.
    assert_eq!(c.quiet_for(Duration::from_millis(300)).await, 0);
    service.abort();
}

/// The protocol client reads the wlr state: activated, minimized, the
/// outputs by name, and a closed toplevel leaves.
#[tokio::test]
async fn protocol_client_follows_wlr_toplevels() {
    let fake = Fake::start_opts(false, true, &["FAKE-1"]);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    std::thread::sleep(Duration::from_millis(50));
    let (tx, mut rx) = unbounded_channel();
    let _client = ProtocolClient::spawn(WaylandTarget::Socket(fake.socket.clone()), tx).unwrap();
    let s = next_matching(&mut rx, "first", |s| {
        s.connected && s.managed.first().is_some_and(|m| m.screens == ["FAKE-1"])
    })
    .await;
    assert!(s.toplevel_management && s.toplevel_list && !s.workspace_manager);
    assert_eq!(s.managed.len(), 1);
    assert_eq!(
        (s.managed[0].app_id.as_str(), s.managed[0].title.as_str()),
        ("foot", "~")
    );
    assert!(!s.managed[0].activated);
    let key = s.managed[0].key;
    fake.cmd(Cmd::Activate("tl-1"));
    next_matching(&mut rx, "activated", |s| s.managed[0].activated).await;
    fake.cmd(Cmd::AddToplevel("tl-2", "x", "xterm"));
    let s = next_matching(&mut rx, "second", |s| s.managed.len() == 2).await;
    assert_ne!(s.managed[1].key, key);
    fake.cmd(Cmd::CloseToplevel("tl-1"));
    let s = next_matching(&mut rx, "closed", |s| s.managed.len() == 1).await;
    assert_eq!(s.managed[0].app_id, "xterm");
}

/// Every seat is bound: when the seat `activate` names goes away,
/// `win.focus()` falls back to a seat announced before it, and answers
/// `Unsupported` only once no seat is left.
#[tokio::test]
async fn win_focus_falls_back_to_another_seat_when_one_is_removed() {
    let fake = Fake::start_seats(false, true, 2, &["FAKE-1"]);
    fake.cmd(Cmd::AddToplevel("tl-1", "~", "foot"));
    std::thread::sleep(Duration::from_millis(50));
    let (sink, mut c) = Collector::new();
    let (req_tx, req_rx) = unbounded_channel();
    let config = WmConfig {
        wayland: Some(WaylandTarget::Socket(fake.socket.clone())),
        desktop: Some("labwc".into()),
        ..Default::default()
    };
    let service = tokio::spawn(wm::run(config, sink, req_rx));
    c.until("boot", |m| m.windows.len() == 1).await;
    let foot = c.mirror.window_by_app("foot").unwrap().clone();
    let focus = || {
        let (r, done) = WmRequest::new(WmAction::FocusWindow(foot.id.clone()));
        req_tx.send(r).unwrap();
        done
    };

    // Both seats offered: the first one announced is named.
    assert_eq!(focus().await, Ok(()));
    // The first seat goes. The retitle is sent after the global's removal
    // on the same connection, so once it shows the removal was read.
    fake.cmd(Cmd::RemoveSeat(0));
    fake.cmd(Cmd::SetTitle("tl-1", "one seat"));
    c.until("retitled", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "one seat")
    })
    .await;
    assert_eq!(focus().await, Ok(()));
    // No seat left: refused.
    fake.cmd(Cmd::RemoveSeat(1));
    fake.cmd(Cmd::SetTitle("tl-1", "no seat"));
    c.until("retitled again", |m| {
        m.window_by_app("foot")
            .is_some_and(|w| w.title == "no seat")
    })
    .await;
    assert_eq!(
        focus().await,
        Err(WmError::Unsupported("the compositor offers no seat"))
    );
    assert_eq!(
        *fake.wlr_requests.lock().unwrap(),
        ["activate tl-1", "activate tl-1 on seat 1"]
    );
    service.abort();
}

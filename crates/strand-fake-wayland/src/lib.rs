//! A fake Wayland compositor (wayland-server) for tests, shared by the
//! crates that talk Wayland.
//!
//! - `strand-services`' protocol client: `ext-foreign-toplevel-list-v1`
//!   and `ext-workspace-v1`, and optionally
//!   `zwlr_foreign_toplevel_management_v1` with seats: the shape of labwc,
//!   a compositor with no IPC adapter (`Fake::start*`).
//! - `strand-surface`'s manager: `wl_compositor`, `wl_shm`, layer shell,
//!   viewporter, single-pixel buffers, the alpha modifier and
//!   `ext-background-effect-v1` ([`SurfaceGlobals`], through
//!   [`Fake::builder`]), recording what every surface committed
//!   ([`Fake::surfaces`]). Sway 1.9, the compositor CI tests on, lacks
//!   the alpha modifier and the background effect.
//!
//! - `strand-services`' thumbnails: the image capture source and copy
//!   capture managers ([`FakeBuilder::capture`]; `capture.rs`).
//!
//! It runs on its own thread and is driven by [`Cmd`]s.

mod capture;
mod surfaces;

pub use capture::DEFAULT_SIZE as CAPTURE_SIZE;

pub use surfaces::{BufferKind, Region, RegionOp, SurfaceGlobals, SurfaceRecord};

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rustix::event::{PollFd, PollFlags};
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
pub enum Cmd {
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
    /// Sends `ext_background_effect_manager_v1.capabilities` with these
    /// flags (1: blur) to every bound manager.
    SetEffectCapabilities(u32),
    /// A toplevel draws in this RGBA colour: its capture sessions have a
    /// new frame.
    Paint(&'static str, [u8; 4]),
    /// A toplevel's size changes: its capture sessions get new buffer
    /// constraints.
    ResizeToplevel(&'static str, u32, u32),
    /// A toplevel's capture sessions stop while it stays listed, as a
    /// compositor-side reset would.
    StopCapture(&'static str),
}

struct Toplevel {
    ident: String,
    title: String,
    app_id: String,
    /// The output it is on (by the order of `outputs`).
    output: usize,
    activated: bool,
    minimized: bool,
    maximized: bool,
    fullscreen: bool,
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
            maximized: false,
            fullscreen: false,
            handles: Vec::new(),
            wlr: Vec::new(),
        }
    }

    /// Its wlr state array.
    fn wlr_state(&self) -> Vec<u8> {
        use zwlr_foreign_toplevel_handle_v1::State;
        let mut values = Vec::new();
        for (on, state) in [
            (self.activated, State::Activated),
            (self.minimized, State::Minimized),
            (self.maximized, State::Maximized),
            (self.fullscreen, State::Fullscreen),
        ] {
            if on {
                values.push(u32::from(state));
            }
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
    /// The surface side ([`SurfaceGlobals`]).
    surf: surfaces::Surfaces,
    /// Every client's shm buffer, by object (capture frames write them).
    shm_buffers: std::collections::HashMap<wayland_server::backend::ObjectId, capture::ShmBuf>,
    /// The capture side.
    capture: capture::Capture,
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
            self.capture_closed(ident);
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
            Cmd::SetEffectCapabilities(flags) => self.surf.set_effect_caps(flags),
            Cmd::Paint(ident, colour) => self.capture_paint(ident, colour),
            Cmd::ResizeToplevel(ident, w, h) => self.capture_resize(ident, w, h),
            Cmd::StopCapture(ident) => self.capture_closed(ident),
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
        if let Some(g) = &state.surf.globals {
            let (w, h) = g.output_size;
            o.geometry(
                0,
                0,
                600,
                340,
                wl_output::Subpixel::Unknown,
                "Fake".into(),
                state.output_names[*index].clone(),
                wl_output::Transform::Normal,
            );
            o.mode(wl_output::Mode::Current, w as i32, h as i32, 60_000);
            if o.version() >= 2 {
                o.scale(1);
            }
        }
        if o.version() >= 4 {
            o.name(state.output_names[*index].clone());
            if state.surf.globals.is_some() {
                o.description(format!("Fake {}", state.output_names[*index]));
            }
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
            Request::SetMaximized
            | Request::UnsetMaximized
            | Request::SetFullscreen { .. }
            | Request::UnsetFullscreen => {
                let (what, max, on) = match request {
                    Request::SetMaximized => ("maximize", true, true),
                    Request::UnsetMaximized => ("unmaximize", true, false),
                    Request::SetFullscreen { .. } => ("fullscreen", false, true),
                    _ => ("unfullscreen", false, false),
                };
                log(what);
                if let Some(t) = state.toplevels.iter_mut().find(|t| t.ident == *ident) {
                    if max {
                        t.maximized = on;
                    } else {
                        t.fullscreen = on;
                    }
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
pub struct Fake {
    _dir: tempfile::TempDir,
    pub socket: std::path::PathBuf,
    tx: mpsc::Sender<Cmd>,
    stop: Arc<AtomicBool>,
    /// While set, the fake reads nothing from its clients (a compositor
    /// busy elsewhere); the events it queues still go out.
    pub paused: Arc<AtomicBool>,
    pub activated: Arc<Mutex<Vec<String>>>,
    pub wlr_requests: Arc<Mutex<Vec<String>>>,
    clients: Arc<AtomicUsize>,
    surfaces: Arc<Mutex<Vec<SurfaceRecord>>>,
    /// What the capture side did (`capture.rs`): `session <ident>`,
    /// `frame <ident>`, `failed <ident>`, `stopped <ident>`, `end <ident>`.
    pub captures: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fake")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

/// What a [`Fake`] offers; [`Fake::builder`].
#[derive(Clone, Debug)]
pub struct FakeBuilder {
    workspaces: bool,
    wlr: bool,
    seats: usize,
    outputs: &'static [&'static str],
    surfaces: Option<SurfaceGlobals>,
    toplevel_list: bool,
    capture: bool,
}

impl FakeBuilder {
    /// `ext-workspace-v1`.
    pub fn workspaces(mut self, on: bool) -> Self {
        self.workspaces = on;
        self
    }

    /// `zwlr_foreign_toplevel_management_v1` (v3).
    pub fn wlr(mut self, on: bool) -> Self {
        self.wlr = on;
        self
    }

    /// Seat globals (keyboard only; their requests are not served).
    pub fn seats(mut self, n: usize) -> Self {
        self.seats = n;
        self
    }

    /// One `wl_output` (and one workspace group) per name.
    pub fn outputs(mut self, outputs: &'static [&'static str]) -> Self {
        self.outputs = outputs;
        self
    }

    /// The surface globals ([`SurfaceGlobals`]).
    pub fn surfaces(mut self, globals: SurfaceGlobals) -> Self {
        self.surfaces = Some(globals);
        self
    }

    /// `ext-foreign-toplevel-list-v1` (on by default).
    pub fn toplevel_list(mut self, on: bool) -> Self {
        self.toplevel_list = on;
        self
    }

    /// The image capture source and copy capture managers (and `wl_shm`).
    pub fn capture(mut self, on: bool) -> Self {
        self.capture = on;
        self
    }

    pub fn start(self) -> Fake {
        Fake::start_built(self)
    }
}

impl Fake {
    /// A fake with nothing but `ext-foreign-toplevel-list-v1` and one
    /// output, `FAKE-1`, to add to.
    pub fn builder() -> FakeBuilder {
        FakeBuilder {
            workspaces: false,
            wlr: false,
            seats: 0,
            outputs: &["FAKE-1"],
            surfaces: None,
            toplevel_list: true,
            capture: false,
        }
    }

    /// A compositor for `strand-surface`: every surface global in `g` and
    /// one output, no toplevel or workspace protocols.
    pub fn compositor(g: SurfaceGlobals) -> Fake {
        Self::builder().toplevel_list(false).surfaces(g).start()
    }

    pub fn start(with_workspaces: bool) -> Fake {
        Self::start_with(with_workspaces, &["FAKE-1"])
    }

    /// A fake with one `wl_output` (and one workspace group) per name.
    pub fn start_with(with_workspaces: bool, outputs: &'static [&'static str]) -> Fake {
        Self::start_opts(with_workspaces, false, outputs)
    }

    /// A fake that also offers `zwlr_foreign_toplevel_management_v1` (v3)
    /// and a seat.
    pub fn start_opts(with_workspaces: bool, wlr: bool, outputs: &'static [&'static str]) -> Fake {
        Self::start_seats(with_workspaces, wlr, usize::from(wlr), outputs)
    }

    /// [`Fake::start_opts`] with `seats` seat globals.
    pub fn start_seats(
        with_workspaces: bool,
        wlr: bool,
        seats: usize,
        outputs: &'static [&'static str],
    ) -> Fake {
        Self::builder()
            .workspaces(with_workspaces)
            .wlr(wlr)
            .seats(seats)
            .outputs(outputs)
            .start()
    }

    fn start_built(b: FakeBuilder) -> Fake {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let socket = dir.path().join("wayland-fake");
        let listener = ListeningSocket::bind_absolute(socket.clone()).expect("the fake's socket");
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stop = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = stop.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let thread_paused = paused.clone();
        let thread_activated = activated.clone();
        let wlr_requests = Arc::new(Mutex::new(Vec::new()));
        let thread_wlr_requests = wlr_requests.clone();
        let clients = Arc::new(AtomicUsize::new(0));
        let thread_clients = clients.clone();
        let surfaces = Arc::new(Mutex::new(Vec::new()));
        let thread_surfaces = surfaces.clone();
        let captures = Arc::new(Mutex::new(Vec::new()));
        let thread_captures = captures.clone();
        let FakeBuilder {
            workspaces: with_workspaces,
            wlr,
            seats,
            outputs,
            surfaces: surface_globals,
            toplevel_list,
            capture,
        } = b;
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().expect("a wayland-server display");
            let dh = display.handle();
            if toplevel_list {
                dh.create_global::<Server, ExtForeignToplevelListV1, ()>(1, ());
            }
            if with_workspaces {
                dh.create_global::<Server, ExtWorkspaceManagerV1, ()>(1, ());
            }
            if wlr {
                dh.create_global::<Server, ZwlrForeignToplevelManagerV1, ()>(3, ());
            }
            if let Some(g) = &surface_globals {
                surfaces::create_globals(&dh, g);
            }
            if capture {
                capture::create_globals(&dh, surface_globals.is_none());
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
            state.surf.effect_caps = surface_globals
                .as_ref()
                .and_then(|g| g.background_effect)
                .unwrap_or(0);
            state.surf.globals = surface_globals;
            state.surf.records = thread_surfaces;
            state.capture.log = thread_captures;
            while !thread_stop.load(Ordering::SeqCst) {
                while let Ok(cmd) = rx.try_recv() {
                    state.apply(&dh, cmd);
                }
                if let Ok(Some(stream)) = listener.accept() {
                    thread_clients.fetch_add(1, Ordering::SeqCst);
                    if let Err(e) = display
                        .handle()
                        .insert_client(stream, Arc::new(ClientState(thread_clients.clone())))
                    {
                        eprintln!("fake compositor: a client could not connect: {e}");
                    }
                }
                if thread_paused.load(Ordering::SeqCst) {
                    let _ = display.flush_clients();
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                if let Err(e) = display.dispatch_clients(&mut state) {
                    eprintln!("fake compositor: dispatch failed: {e}");
                }
                let _ = display.flush_clients();
                let Ok(poll_fd) = display.backend().poll_fd().try_clone_to_owned() else {
                    continue;
                };
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
            paused,
            activated,
            wlr_requests,
            clients,
            surfaces,
            captures,
            thread: Some(thread),
        }
    }

    /// Clients connected now.
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub fn cmd(&self, c: Cmd) {
        // The fake's thread outlives every `Fake` handle.
        let _ = self.tx.send(c);
    }

    /// Every surface created so far, in creation order, as last committed.
    pub fn surfaces(&self) -> Vec<SurfaceRecord> {
        self.surfaces.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The live layer surfaces with namespace `ns`.
    pub fn layer(&self, ns: &str) -> Vec<SurfaceRecord> {
        self.surfaces()
            .into_iter()
            .filter(|s| !s.destroyed && s.namespace.as_deref() == Some(ns))
            .collect()
    }

    /// The live subsurfaces of the live layer surface `ns`.
    pub fn subsurfaces_of(&self, ns: &str) -> Vec<SurfaceRecord> {
        let all = self.surfaces();
        all.iter()
            .filter(|s| {
                !s.destroyed
                    && s.subsurface_of
                        .and_then(|p| all.get(p))
                        .is_some_and(|p| !p.destroyed && p.namespace.as_deref() == Some(ns))
            })
            .cloned()
            .collect()
    }

    /// A connection to it.
    pub fn connect(&self) -> std::os::unix::net::UnixStream {
        std::os::unix::net::UnixStream::connect(&self.socket).expect("the fake's socket")
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

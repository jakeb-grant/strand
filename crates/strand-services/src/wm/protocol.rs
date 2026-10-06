//! The standard protocols, compositor-agnostic: `ext-foreign-toplevel-list-v1`
//! (windows: identifier, title, app id) and `ext-workspace-v1`
//! (workspaces, their groups' outputs, active/urgent/hidden, activate).
//!
//! The client runs on its own thread (design.md, "Threads": the Wayland
//! toplevel protocols get their own thread) with its own connection. It
//! sleeps in `poll(2)` on the Wayland socket and an eventfd, so an idle
//! compositor costs no wakeup, and sends a [`ProtocolState`] after every
//! atomic update (a toplevel's `done`, the workspace manager's `done`).

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use rustix::event::{EventfdFlags, PollFd, PollFlags};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

/// Which Wayland display the protocol client connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaylandTarget {
    /// `$WAYLAND_DISPLAY` (or `$WAYLAND_SOCKET`).
    Env,
    /// A socket path.
    Socket(PathBuf),
}

/// A toplevel from `ext-foreign-toplevel-list-v1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Toplevel {
    /// The compositor's identifier, stable while the toplevel lives.
    pub identifier: String,
    /// Its title.
    pub title: String,
    /// Its app id.
    pub app_id: String,
}

/// A workspace from `ext-workspace-v1`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtoWorkspace {
    /// This client's number for it, stable while it lives (the protocol
    /// gives no integer id).
    pub key: u64,
    /// The compositor's optional stable id.
    pub id: Option<String>,
    /// Its name.
    pub name: String,
    /// Its coordinates in its group.
    pub coordinates: Vec<u32>,
    /// Shown on its outputs.
    pub active: bool,
    /// Asks for attention.
    pub urgent: bool,
    /// Not meant for a workspace switcher.
    pub hidden: bool,
    /// The names of its group's outputs.
    pub screens: Vec<String>,
    /// `activate` is available.
    pub can_activate: bool,
}

/// What the standard protocols show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtocolState {
    /// The client is connected (false when it could not connect or the
    /// connection ended).
    pub connected: bool,
    /// `ext_foreign_toplevel_list_v1` is advertised.
    pub toplevel_list: bool,
    /// `ext_workspace_manager_v1` is advertised.
    pub workspace_manager: bool,
    /// Every toplevel past its first `done`, oldest first.
    pub toplevels: Vec<Toplevel>,
    /// Every workspace, oldest first.
    pub workspaces: Vec<ProtoWorkspace>,
}

/// A request to the protocol thread.
#[derive(Debug)]
pub(crate) enum ProtoCmd {
    /// Activate the workspace with this key; the reply says whether it was
    /// sent.
    Activate(
        u64,
        Option<tokio::sync::oneshot::Sender<Result<(), super::WmError>>>,
    ),
    Stop,
}

/// The running client; dropping it stops the thread (without waiting).
#[derive(Debug)]
pub struct ProtocolClient {
    tx: mpsc::Sender<ProtoCmd>,
    wake: Arc<OwnedFd>,
}

impl ProtocolClient {
    /// Starts the client thread. Every state goes to `tx`; the first one
    /// after the initial roundtrips (or one with `connected: false` when
    /// the display cannot be reached).
    pub fn spawn(target: WaylandTarget, tx: UnboundedSender<ProtocolState>) -> io::Result<Self> {
        let wake = Arc::new(rustix::event::eventfd(
            0,
            EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK,
        )?);
        let (ctx, crx) = mpsc::channel();
        let thread_wake = wake.clone();
        std::thread::Builder::new()
            .name("strand-toplevel".into())
            .spawn(move || {
                if let Err(e) = thread_main(target, &tx, &crx, &thread_wake) {
                    log::warn!("Wayland toplevel and workspace protocols: {e}");
                }
                let _ = tx.send(ProtocolState::default());
            })?;
        Ok(Self { tx: ctx, wake })
    }

    pub(crate) fn send(&self, cmd: ProtoCmd) {
        if self.tx.send(cmd).is_ok() {
            let _ = rustix::io::write(&*self.wake, &1u64.to_ne_bytes());
        }
    }
}

impl Drop for ProtocolClient {
    fn drop(&mut self) {
        self.send(ProtoCmd::Stop);
    }
}

#[derive(Debug, Default)]
struct ToplevelEntry {
    pending: Toplevel,
    current: Option<Toplevel>,
}

#[derive(Debug, Default)]
struct GroupEntry {
    outputs: Vec<ObjectId>,
    workspaces: Vec<ObjectId>,
}

#[derive(Debug)]
struct WorkspaceEntry {
    handle: ExtWorkspaceHandleV1,
    data: ProtoWorkspace,
}

#[derive(Debug, Default)]
struct Client {
    toplevel_list: Option<ExtForeignToplevelListV1>,
    workspace_manager: Option<ExtWorkspaceManagerV1>,
    /// wl_output proxies by object, with their global name and `name`.
    outputs: HashMap<ObjectId, (u32, wl_output::WlOutput, String)>,
    toplevels: Vec<(ObjectId, ToplevelEntry)>,
    groups: Vec<(ObjectId, GroupEntry)>,
    workspaces: Vec<(ObjectId, WorkspaceEntry)>,
    next_key: u64,
    dirty: bool,
}

impl Client {
    fn snapshot(&self) -> ProtocolState {
        let screens_of = |ws: &ObjectId| -> Vec<String> {
            self.groups
                .iter()
                .filter(|(_, g)| g.workspaces.contains(ws))
                .flat_map(|(_, g)| g.outputs.iter())
                .filter_map(|o| self.outputs.get(o).map(|(_, _, n)| n.clone()))
                .filter(|n| !n.is_empty())
                .collect()
        };
        ProtocolState {
            connected: true,
            toplevel_list: self.toplevel_list.is_some(),
            workspace_manager: self.workspace_manager.is_some(),
            toplevels: self
                .toplevels
                .iter()
                .filter_map(|(_, t)| t.current.clone())
                .collect(),
            workspaces: self
                .workspaces
                .iter()
                .map(|(id, w)| ProtoWorkspace {
                    screens: screens_of(id),
                    ..w.data.clone()
                })
                .collect(),
        }
    }

    fn bind_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
        self.outputs
            .insert(output.id(), (name, output, String::new()));
    }
}

fn thread_main(
    target: WaylandTarget,
    tx: &UnboundedSender<ProtocolState>,
    cmds: &mpsc::Receiver<ProtoCmd>,
    wake: &OwnedFd,
) -> io::Result<()> {
    let conn = match target {
        WaylandTarget::Env => Connection::connect_to_env().map_err(io::Error::other)?,
        WaylandTarget::Socket(path) => {
            Connection::from_socket(UnixStream::connect(path)?).map_err(io::Error::other)?
        }
    };
    let (globals, mut queue): (_, EventQueue<Client>) =
        registry_queue_init(&conn).map_err(io::Error::other)?;
    let qh = queue.handle();
    let mut client = Client {
        toplevel_list: globals
            .bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ())
            .ok(),
        workspace_manager: globals
            .bind::<ExtWorkspaceManagerV1, _, _>(&qh, 1..=1, ())
            .ok(),
        ..Default::default()
    };
    for g in globals.contents().clone_list() {
        if g.interface == wl_output::WlOutput::interface().name {
            client.bind_output(globals.registry(), g.name, g.version, &qh);
        }
    }
    // Two roundtrips: the binds' first events, then the outputs' names and
    // the handles those events created.
    queue.roundtrip(&mut client).map_err(io::Error::other)?;
    queue.roundtrip(&mut client).map_err(io::Error::other)?;
    if tx.send(client.snapshot()).is_err() {
        return Ok(());
    }
    client.dirty = false;
    loop {
        queue
            .dispatch_pending(&mut client)
            .map_err(io::Error::other)?;
        if std::mem::take(&mut client.dirty) && tx.send(client.snapshot()).is_err() {
            return Ok(());
        }
        queue.flush().map_err(io::Error::other)?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let (wayland_ready, wake_ready) = {
            let conn_fd = guard.connection_fd();
            let mut fds = [
                PollFd::new(&conn_fd, PollFlags::IN),
                PollFd::new(wake, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) => (!fds[0].revents().is_empty(), !fds[1].revents().is_empty()),
                Err(rustix::io::Errno::INTR) => (false, false),
                Err(e) => return Err(e.into()),
            }
        };
        if wayland_ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(io::Error::other(e)),
            }
        } else {
            drop(guard);
        }
        if wake_ready {
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(wake, &mut buf);
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    ProtoCmd::Stop => return Ok(()),
                    ProtoCmd::Activate(key, reply) => {
                        let result = activate(&client, key);
                        if let Some(r) = reply {
                            let _ = r.send(result);
                        }
                    }
                }
            }
        }
    }
}

fn activate(client: &Client, key: u64) -> Result<(), super::WmError> {
    let manager = client
        .workspace_manager
        .as_ref()
        .ok_or(super::WmError::Unsupported("no ext-workspace-v1"))?;
    let (_, ws) = client
        .workspaces
        .iter()
        .find(|(_, w)| w.data.key == key)
        .ok_or(super::WmError::UnknownWorkspace(key as i64))?;
    if !ws.data.can_activate {
        return Err(super::WmError::Unsupported(
            "the compositor does not let this workspace be activated",
        ));
    }
    ws.handle.activate();
    manager.commit();
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name => {
                state.bind_output(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                let gone: Vec<ObjectId> = state
                    .outputs
                    .iter()
                    .filter(|(_, (n, _, _))| *n == name)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in gone {
                    if let Some((_, o, _)) = state.outputs.remove(&id) {
                        if o.version() >= 3 {
                            o.release();
                        }
                        state.dirty = true;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Client {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.outputs.get_mut(&output.id())
        {
            entry.2 = name;
            state.dirty = true;
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                state
                    .toplevels
                    .push((toplevel.id(), ToplevelEntry::default()));
            }
            ext_foreign_toplevel_list_v1::Event::Finished => {
                state.toplevel_list = None;
                state.toplevels.clear();
                state.dirty = true;
            }
            _ => {}
        }
    }

    event_created_child!(Client, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Client {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        let Some(pos) = state.toplevels.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let entry = &mut state.toplevels[pos].1;
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => entry.pending.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                entry.pending.app_id = app_id;
            }
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.pending.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                if entry.current.as_ref() != Some(&entry.pending) {
                    entry.current = Some(entry.pending.clone());
                    state.dirty = true;
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                let (_, e) = state.toplevels.remove(pos);
                state.dirty |= e.current.is_some();
                handle.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state
                    .groups
                    .push((workspace_group.id(), GroupEntry::default()));
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.next_key += 1;
                let key = state.next_key;
                state.workspaces.push((
                    workspace.id(),
                    WorkspaceEntry {
                        handle: workspace,
                        data: ProtoWorkspace {
                            key,
                            ..Default::default()
                        },
                    },
                ));
            }
            ext_workspace_manager_v1::Event::Done => state.dirty = true,
            ext_workspace_manager_v1::Event::Finished => {
                state.workspace_manager = None;
                state.groups.clear();
                state.workspaces.clear();
                state.dirty = true;
            }
            _ => {}
        }
    }

    event_created_child!(Client, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Client {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        let Some(pos) = state.groups.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let group = &mut state.groups[pos].1;
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                if !group.outputs.contains(&output.id()) {
                    group.outputs.push(output.id());
                }
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                group.outputs.retain(|o| *o != output.id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if !group.workspaces.contains(&workspace.id()) {
                    group.workspaces.push(workspace.id());
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                group.workspaces.retain(|w| *w != workspace.id());
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                state.groups.remove(pos);
                handle.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for Client {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        let Some(pos) = state.workspaces.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let data = &mut state.workspaces[pos].1.data;
        match event {
            ext_workspace_handle_v1::Event::Id { id } => data.id = Some(id),
            ext_workspace_handle_v1::Event::Name { name } => data.name = name,
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                data.coordinates = coordinates
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state: s } => {
                let bits = match s {
                    wayland_client::WEnum::Value(v) => v.bits(),
                    wayland_client::WEnum::Unknown(u) => u,
                };
                data.active = bits & ext_workspace_handle_v1::State::Active.bits() != 0;
                data.urgent = bits & ext_workspace_handle_v1::State::Urgent.bits() != 0;
                data.hidden = bits & ext_workspace_handle_v1::State::Hidden.bits() != 0;
            }
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => {
                let bits = match capabilities {
                    wayland_client::WEnum::Value(v) => v.bits(),
                    wayland_client::WEnum::Unknown(u) => u,
                };
                data.can_activate =
                    bits & ext_workspace_handle_v1::WorkspaceCapabilities::Activate.bits() != 0;
            }
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.remove(pos);
                for (_, g) in &mut state.groups {
                    g.workspaces.retain(|w| *w != id);
                }
                handle.destroy();
            }
            _ => {}
        }
    }
}

//! The standard protocols, compositor-agnostic: `ext-foreign-toplevel-list-v1`
//! (windows: identifier, title, app id), `zwlr_foreign_toplevel_management_v1`
//! (windows with their state: activated, minimized, maximized, fullscreen,
//! outputs; activate, close, set_minimized, set/unset_maximized,
//! set/unset_fullscreen) and `ext-workspace-v1`
//! (workspaces, their groups' outputs, active/urgent/hidden, activate).
//! The wlr protocol is what serves `windows.focused` and the window
//! actions on a compositor with no IPC adapter (labwc, wayfire, river; see
//! [`merge`](super::merge)).
//!
//! The client runs on its own thread (design.md, "Threads": the Wayland
//! toplevel protocols get their own thread) with its own connection. It
//! sleeps in `poll(2)` on the Wayland socket and an eventfd, so an idle
//! compositor costs no wakeup, and sends a [`ProtocolState`] after every
//! atomic update (a toplevel's `done`, the workspace manager's `done`).
//! Like a toplevel's events, every workspace and group event (including
//! creations and removals) is held pending until the manager's `done`
//! applies them together: a published state never mixes two workspace
//! transactions, even when a toplevel's `done` is read between them.
//!
//! The thread lives and dies with its connection: when the display goes
//! away it sends a disconnected [`ProtocolState`] and ends (the shell's
//! own Wayland connection is gone then too). Its startup (the registry
//! and the syncs that collect the first state) runs in the same `poll(2)`
//! loop, so dropping the [`ProtocolClient`] stops it even while a hung
//! compositor never answers.
//!
//! A request that reaches the thread after it ended, or that it had not
//! taken when it ended, is answered [`WmError::NotConnected`].
//!
//! M4's window thumbnails (`ext-image-copy-capture-v1`) need the
//! toplevel's handle on this connection: they will add a `ProtoCmd` that
//! captures by [`Toplevel::identifier`] (the model keeps it as
//! [`Window::toplevel`](super::Window::toplevel)).

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
use wayland_client::protocol::{wl_callback, wl_output, wl_registry, wl_seat};
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
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::WmError;

/// The syncs startup waits for, each sent once the one before it is
/// answered: the registry's globals (bound as they arrive), the binds'
/// first events, then the events of the handles those created.
const STARTUP_SYNCS: u32 = 3;

/// Which Wayland display the protocol client connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaylandTarget {
    /// `$WAYLAND_DISPLAY` (under `$XDG_RUNTIME_DIR` unless absolute;
    /// `wayland-0` when unset). Never `$WAYLAND_SOCKET`: that fd belongs to
    /// the shell's main connection.
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

/// A toplevel from `zwlr_foreign_toplevel_management_v1`, with its state.
/// The protocol gives no identifier: [`ManagedToplevel::key`] is this
/// client's own number for the handle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedToplevel {
    /// This client's number for it, stable while it lives.
    pub key: u64,
    /// Its title.
    pub title: String,
    /// Its app id.
    pub app_id: String,
    /// It is active (has the keyboard focus of a seat).
    pub activated: bool,
    /// It is minimized.
    pub minimized: bool,
    /// It is maximized.
    pub maximized: bool,
    /// It is fullscreen (version 2 and later).
    pub fullscreen: bool,
    /// The names of the outputs it is on.
    pub screens: Vec<String>,
}

impl ManagedToplevel {
    /// The window id the services show for it (`windows.all[i].id`).
    pub fn window_id(&self) -> String {
        managed_window_id(self.key)
    }
}

/// The window id of the wlr toplevel with `key`: `wlr-<key>`.
pub(crate) fn managed_window_id(key: u64) -> String {
    format!("wlr-{key}")
}

/// What the standard protocols show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtocolState {
    /// The client is connected (false when it could not connect or the
    /// connection ended).
    pub connected: bool,
    /// `ext_foreign_toplevel_list_v1` is advertised.
    pub toplevel_list: bool,
    /// `zwlr_foreign_toplevel_manager_v1` is advertised (and has not
    /// finished).
    pub toplevel_management: bool,
    /// `ext_workspace_manager_v1` is advertised.
    pub workspace_manager: bool,
    /// Every toplevel past its first `done`, oldest first.
    pub toplevels: Vec<Toplevel>,
    /// Every wlr toplevel past its first `done`, oldest first.
    pub managed: Vec<ManagedToplevel>,
    /// Every workspace, oldest first.
    pub workspaces: Vec<ProtoWorkspace>,
}

/// A window action `zwlr_foreign_toplevel_handle_v1` can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowOp {
    /// `activate` on the first seat still offered.
    Activate,
    /// `close`.
    Close,
    /// `set_minimized`.
    Minimize,
    /// `set_maximized`, or `unset_maximized` when it is maximized.
    Maximize,
    /// `set_fullscreen` (on the output the compositor picks), or
    /// `unset_fullscreen` when it is fullscreen; version 2 and later.
    Fullscreen,
}

type Reply = Option<tokio::sync::oneshot::Sender<Result<(), WmError>>>;

/// A request to the protocol thread.
#[derive(Debug)]
pub(crate) enum ProtoCmd {
    /// Activate the workspace with this key; the reply says whether it was
    /// sent.
    Activate(u64, Reply),
    /// Run a window action on the wlr toplevel with this key; the reply
    /// says whether it was sent.
    Window(u64, WindowOp, Reply),
    Stop,
}

impl ProtoCmd {
    /// The thread is gone: answer `NotConnected`.
    fn refuse(self) {
        if let Self::Activate(_, Some(reply)) | Self::Window(_, _, Some(reply)) = self {
            let _ = reply.send(Err(WmError::NotConnected));
        }
    }
}

/// The running client; dropping it stops the thread (without waiting),
/// during its startup too; [`ProtocolClient::stop`] also waits for it.
#[derive(Debug)]
pub struct ProtocolClient {
    sender: ProtocolSender,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Disconnected once the thread's body has ended (its sender is the
    /// last thing the thread drops), so a stop waits on it instead of
    /// polling `is_finished`.
    done: mpsc::Receiver<()>,
}

/// Sends requests to a [`ProtocolClient`]'s thread (the run that routes
/// actions holds one; the client's owner stops the thread).
#[derive(Clone, Debug)]
pub(crate) struct ProtocolSender {
    tx: mpsc::Sender<ProtoCmd>,
    wake: Arc<OwnedFd>,
}

impl ProtocolSender {
    /// Queues a request and wakes the thread; one the ended thread cannot
    /// take is answered `NotConnected`.
    pub(crate) fn send(&self, cmd: ProtoCmd) {
        match self.tx.send(cmd) {
            Ok(()) => {
                let _ = rustix::io::write(&*self.wake, &1u64.to_ne_bytes());
            }
            Err(mpsc::SendError(cmd)) => cmd.refuse(),
        }
    }
}

/// How long [`ProtocolClient::stop`] waits for the thread (it ends as
/// soon as its poll hears the stop: it never blocks elsewhere).
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

impl ProtocolClient {
    /// Starts the client thread. Every state goes to `tx`; the first one
    /// once the startup syncs are answered (or one with `connected: false` when
    /// the display cannot be reached).
    pub fn spawn(target: WaylandTarget, tx: UnboundedSender<ProtocolState>) -> io::Result<Self> {
        let wake = Arc::new(rustix::event::eventfd(
            0,
            EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK,
        )?);
        let (ctx, crx) = mpsc::channel();
        let thread_wake = wake.clone();
        let (done_tx, done) = mpsc::channel::<()>();
        // Resolved here, on the caller's thread: the client thread never
        // touches the environment.
        let socket = match target {
            WaylandTarget::Env => display_socket(
                std::env::var_os("WAYLAND_DISPLAY"),
                std::env::var_os("XDG_RUNTIME_DIR"),
            ),
            WaylandTarget::Socket(path) => Ok(path),
        };
        let thread = std::thread::Builder::new()
            .name("strand-toplevel".into())
            .spawn(move || {
                let run = socket.and_then(|s| thread_main(s, &tx, &crx, &thread_wake));
                if let Err(e) = run {
                    log::warn!("Wayland toplevel and workspace protocols: {e}");
                }
                // Answer what was still queued, then close the channel: a
                // later `send` fails and answers itself. (One sent between
                // the two is dropped with its reply, which a `WmReply`
                // reads as `NotConnected` too.)
                while let Ok(cmd) = crx.try_recv() {
                    cmd.refuse();
                }
                drop(crx);
                let _ = tx.send(ProtocolState::default());
                drop(done_tx);
            })?;
        Ok(Self {
            sender: ProtocolSender { tx: ctx, wake },
            thread: Some(thread),
            done,
        })
    }

    /// Queues a request and wakes the thread; one the ended thread cannot
    /// take is answered `NotConnected`.
    pub(crate) fn send(&self, cmd: ProtoCmd) {
        self.sender.send(cmd);
    }

    /// A sender of requests to the thread.
    pub(crate) fn sender(&self) -> ProtocolSender {
        self.sender.clone()
    }

    /// Tells the thread to stop without waiting for it: it ends as soon
    /// as its poll hears the stop. [`Self::stop`] (or dropping the client)
    /// later reaps it.
    pub fn request_stop(&self) {
        self.send(ProtoCmd::Stop);
    }

    /// The thread has ended (a stop or join would not wait).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Stops the thread and waits for it to end (2 s at most; it ends at
    /// once unless the system is starved, and is then left to end alone).
    /// Blocks: never call it on the shared services runtime with others
    /// waiting (the hub stops runs with [`Self::request_stop`] and joins
    /// here only when it is dropped).
    pub fn stop(mut self) {
        self.request_stop();
        let Some(t) = self.thread.take() else { return };
        match self.done.recv_timeout(STOP_WAIT) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = t.join();
            }
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {
                log::warn!("strand-toplevel did not stop within {STOP_WAIT:?}; left running");
            }
        }
    }
}

impl Drop for ProtocolClient {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.send(ProtoCmd::Stop);
        }
    }
}

/// The socket `WAYLAND_DISPLAY` names: a path, or a name under the
/// runtime directory.
fn display_socket(
    display: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    let display = PathBuf::from(
        display
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "wayland-0".into()),
    );
    if display.is_absolute() {
        return Ok(display);
    }
    let runtime = runtime_dir
        .filter(|d| !d.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join(display))
}

#[derive(Debug)]
struct ToplevelEntry {
    handle: ExtForeignToplevelHandleV1,
    pending: Toplevel,
    current: Option<Toplevel>,
}

/// A group's membership: `pending` takes the events, `current` (what
/// snapshots read) takes `pending` at the manager's `done`.
#[derive(Clone, Debug, Default, PartialEq)]
struct Members {
    outputs: Vec<ObjectId>,
    workspaces: Vec<ObjectId>,
}

#[derive(Debug, Default)]
struct GroupEntry {
    pending: Members,
    current: Members,
    /// `removed` was received: it goes at the next `done`.
    removed: bool,
}

#[derive(Debug)]
struct WorkspaceEntry {
    handle: ExtWorkspaceHandleV1,
    pending: ProtoWorkspace,
    /// `None` until the first `done` after its creation.
    current: Option<ProtoWorkspace>,
    /// `removed` was received (its handle destroyed): it goes at the next
    /// `done`.
    removed: bool,
}

/// A wlr toplevel's double-buffered state: events go to `pending`, the
/// handle's `done` copies it to `current`.
#[derive(Clone, Debug, Default, PartialEq)]
struct ManagedData {
    title: String,
    app_id: String,
    activated: bool,
    minimized: bool,
    maximized: bool,
    fullscreen: bool,
    outputs: Vec<ObjectId>,
}

#[derive(Debug)]
struct ManagedEntry {
    handle: ZwlrForeignToplevelHandleV1,
    key: u64,
    pending: ManagedData,
    /// `None` until its first `done`.
    current: Option<ManagedData>,
}

#[derive(Debug, Default)]
struct Client {
    toplevel_list: Option<ExtForeignToplevelListV1>,
    toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
    workspace_manager: Option<ExtWorkspaceManagerV1>,
    /// Every seat, with its global name, in the order announced: wlr's
    /// `activate` names the first one still offered, so a removed seat
    /// falls back to the next.
    seats: Vec<(u32, wl_seat::WlSeat)>,
    /// wl_output proxies by object, with their global name and `name`.
    outputs: HashMap<ObjectId, (u32, wl_output::WlOutput, String)>,
    toplevels: Vec<(ObjectId, ToplevelEntry)>,
    managed: Vec<(ObjectId, ManagedEntry)>,
    next_managed_key: u64,
    groups: Vec<(ObjectId, GroupEntry)>,
    workspaces: Vec<(ObjectId, WorkspaceEntry)>,
    next_key: u64,
    dirty: bool,
    /// The highest startup sync answered.
    synced: u32,
}

impl Client {
    fn snapshot(&self) -> ProtocolState {
        let screens_of = |ws: &ObjectId| -> Vec<String> {
            self.groups
                .iter()
                .filter(|(_, g)| g.current.workspaces.contains(ws))
                .flat_map(|(_, g)| g.current.outputs.iter())
                .filter_map(|o| self.outputs.get(o).map(|(_, _, n)| n.clone()))
                .filter(|n| !n.is_empty())
                .collect()
        };
        let output_name = |o: &ObjectId| -> Option<String> {
            self.outputs
                .get(o)
                .map(|(_, _, n)| n.clone())
                .filter(|n| !n.is_empty())
        };
        ProtocolState {
            connected: true,
            toplevel_list: self.toplevel_list.is_some(),
            toplevel_management: self.toplevel_manager.is_some(),
            workspace_manager: self.workspace_manager.is_some(),
            toplevels: self
                .toplevels
                .iter()
                .filter_map(|(_, t)| t.current.clone())
                .collect(),
            managed: self
                .managed
                .iter()
                .filter_map(|(_, m)| {
                    let c = m.current.as_ref()?;
                    Some(ManagedToplevel {
                        key: m.key,
                        title: c.title.clone(),
                        app_id: c.app_id.clone(),
                        activated: c.activated,
                        minimized: c.minimized,
                        maximized: c.maximized,
                        fullscreen: c.fullscreen,
                        screens: c.outputs.iter().filter_map(output_name).collect(),
                    })
                })
                .collect(),
            workspaces: self
                .workspaces
                .iter()
                .filter_map(|(id, w)| {
                    Some(ProtoWorkspace {
                        screens: screens_of(id),
                        ..w.current.clone()?
                    })
                })
                .collect(),
        }
    }

    /// The workspace manager's `done`: every pending workspace and group
    /// change applies at once.
    fn workspaces_done(&mut self) {
        self.workspaces.retain(|(_, w)| !w.removed);
        self.groups.retain(|(_, g)| !g.removed);
        let live: Vec<ObjectId> = self.workspaces.iter().map(|(id, _)| id.clone()).collect();
        for (_, w) in &mut self.workspaces {
            w.current = Some(w.pending.clone());
        }
        for (_, g) in &mut self.groups {
            g.pending.workspaces.retain(|w| live.contains(w));
            g.current = g.pending.clone();
        }
        self.dirty = true;
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
    socket: PathBuf,
    tx: &UnboundedSender<ProtocolState>,
    cmds: &mpsc::Receiver<ProtoCmd>,
    wake: &OwnedFd,
) -> io::Result<()> {
    let conn = Connection::from_socket(UnixStream::connect(socket)?).map_err(io::Error::other)?;
    let mut queue: EventQueue<Client> = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    // The registry binds globals as they arrive (see its `Dispatch`); the
    // syncs mark the end of the startup. No blocking roundtrip: everything
    // goes through the poll loop below, which also hears `Stop`.
    let _registry = display.get_registry(&qh, ());
    display.sync(&qh, 1);
    let mut syncs_sent = 1;
    let mut started = false;
    let mut client = Client::default();
    loop {
        queue
            .dispatch_pending(&mut client)
            .map_err(io::Error::other)?;
        if !started {
            if client.synced >= STARTUP_SYNCS {
                // The first state goes out even when nothing was found.
                started = true;
                client.dirty = true;
            } else if client.synced >= syncs_sent {
                syncs_sent += 1;
                display.sync(&qh, syncs_sent);
            }
        }
        if started && std::mem::take(&mut client.dirty) && tx.send(client.snapshot()).is_err() {
            return Ok(());
        }
        // A full socket buffer is not an error: wait until it drains.
        let unflushed = match queue.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock =>
            {
                true
            }
            Err(e) => return Err(io::Error::other(e)),
        };
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let (wayland_ready, wake_ready) = {
            let conn_fd = guard.connection_fd();
            let wayland_flags = if unflushed {
                PollFlags::IN | PollFlags::OUT
            } else {
                PollFlags::IN
            };
            let mut fds = [
                PollFd::new(&conn_fd, wayland_flags),
                PollFd::new(wake, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) => (
                    fds[0]
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                    !fds[1].revents().is_empty(),
                ),
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
                    ProtoCmd::Window(key, op, reply) => {
                        let result = window_action(&client, key, op);
                        if let Some(r) = reply {
                            let _ = r.send(result);
                        }
                    }
                }
            }
        }
    }
}

fn activate(client: &Client, key: u64) -> Result<(), WmError> {
    let manager = client
        .workspace_manager
        .as_ref()
        .ok_or(WmError::Unsupported("no ext-workspace-v1"))?;
    let (_, ws) = client
        .workspaces
        .iter()
        .find(|(_, w)| !w.removed && w.current.as_ref().is_some_and(|c| c.key == key))
        .ok_or(WmError::UnknownWorkspace(key as i64))?;
    if !ws.current.as_ref().is_some_and(|c| c.can_activate) {
        return Err(WmError::Unsupported(
            "the compositor does not let this workspace be activated",
        ));
    }
    ws.handle.activate();
    manager.commit();
    Ok(())
}

/// Sends a window action on the wlr toplevel with `key`. `Ok` means the
/// request was sent; what the compositor does with it comes back as state
/// (a compositor may ignore it: sway has no minimize).
fn window_action(client: &Client, key: u64, op: WindowOp) -> Result<(), WmError> {
    if client.toplevel_manager.is_none() {
        return Err(WmError::Unsupported(
            "no zwlr_foreign_toplevel_management_v1",
        ));
    }
    let (_, entry) = client
        .managed
        .iter()
        .find(|(_, m)| m.key == key && m.current.is_some())
        .ok_or_else(|| WmError::UnknownWindow(managed_window_id(key)))?;
    match op {
        WindowOp::Activate => {
            let (_, seat) = client
                .seats
                .first()
                .ok_or(WmError::Unsupported("the compositor offers no seat"))?;
            entry.handle.activate(seat);
        }
        WindowOp::Close => entry.handle.close(),
        WindowOp::Minimize => entry.handle.set_minimized(),
        // A toggle from the state the compositor last sent (the one the
        // services show), as `win.maximize()` and `win.fullscreen()` are.
        WindowOp::Maximize => match entry.current.as_ref() {
            Some(c) if c.maximized => entry.handle.unset_maximized(),
            _ => entry.handle.set_maximized(),
        },
        WindowOp::Fullscreen => {
            if entry.handle.version() < 2 {
                return Err(WmError::Unsupported(
                    "zwlr_foreign_toplevel_handle_v1 before version 2 has no fullscreen",
                ));
            }
            match entry.current.as_ref() {
                Some(c) if c.fullscreen => entry.handle.unset_fullscreen(),
                _ => entry.handle.set_fullscreen(None),
            }
        }
    }
    Ok(())
}

impl Dispatch<wl_callback::WlCallback, u32> for Client {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        n: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = state.synced.max(*n);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == wl_output::WlOutput::interface().name {
                    state.bind_output(registry, name, version, qh);
                } else if interface == ExtForeignToplevelListV1::interface().name
                    && state.toplevel_list.is_none()
                {
                    state.toplevel_list = Some(registry.bind(name, version.min(1), qh, ()));
                    state.dirty = true;
                } else if interface == ZwlrForeignToplevelManagerV1::interface().name
                    && state.toplevel_manager.is_none()
                {
                    state.toplevel_manager = Some(registry.bind(name, version.min(3), qh, ()));
                    state.dirty = true;
                } else if interface == wl_seat::WlSeat::interface().name {
                    // Only named by `activate`: version 1 (no events read).
                    // Every seat is bound, so one removed leaves the others.
                    state.seats.push((name, registry.bind(name, 1, qh, ())));
                } else if interface == ExtWorkspaceManagerV1::interface().name
                    && state.workspace_manager.is_none()
                {
                    state.workspace_manager = Some(registry.bind(name, version.min(1), qh, ()));
                    state.dirty = true;
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                // A version 1 seat has no `release`: forget it.
                state.seats.retain(|(n, _)| *n != name);
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
        list: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                state.toplevels.push((
                    toplevel.id(),
                    ToplevelEntry {
                        handle: toplevel,
                        pending: Toplevel::default(),
                        current: None,
                    },
                ));
            }
            ext_foreign_toplevel_list_v1::Event::Finished => {
                // The protocol asks the client to destroy the handles,
                // then the list.
                for (_, t) in state.toplevels.drain(..) {
                    t.handle.destroy();
                }
                list.destroy();
                state.toplevel_list = None;
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
                e.handle.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                state.next_managed_key += 1;
                state.managed.push((
                    toplevel.id(),
                    ManagedEntry {
                        handle: toplevel,
                        key: state.next_managed_key,
                        pending: ManagedData::default(),
                        current: None,
                    },
                ));
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                // A destructor: the manager is gone. Its handles stay
                // valid until destroyed; nothing will update them.
                for (_, m) in state.managed.drain(..) {
                    m.handle.destroy();
                }
                state.toplevel_manager = None;
                state.dirty = true;
            }
            _ => {}
        }
    }

    event_created_child!(Client, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Client {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::{Event, State};
        let id = handle.id();
        let Some(pos) = state.managed.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let entry = &mut state.managed[pos].1;
        let data = &mut entry.pending;
        match event {
            Event::Title { title } => data.title = title,
            Event::AppId { app_id } => data.app_id = app_id,
            Event::OutputEnter { output } => {
                if !data.outputs.contains(&output.id()) {
                    data.outputs.push(output.id());
                }
            }
            Event::OutputLeave { output } => data.outputs.retain(|o| *o != output.id()),
            Event::State { state: bytes } => {
                // An array of `state` enum values (u32, native endian);
                // values this client does not know are ignored.
                let values: Vec<u32> = bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                let has = |s: State| values.contains(&u32::from(s));
                data.activated = has(State::Activated);
                data.minimized = has(State::Minimized);
                data.maximized = has(State::Maximized);
                data.fullscreen = has(State::Fullscreen);
            }
            Event::Done => {
                if entry.current.as_ref() != Some(&entry.pending) {
                    entry.current = Some(entry.pending.clone());
                    state.dirty = true;
                }
            }
            Event::Closed => {
                let (_, e) = state.managed.remove(pos);
                state.dirty |= e.current.is_some();
                e.handle.destroy();
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
                        pending: ProtoWorkspace {
                            key,
                            ..Default::default()
                        },
                        current: None,
                        removed: false,
                    },
                ));
            }
            ext_workspace_manager_v1::Event::Done => state.workspaces_done(),
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
        let members = &mut group.pending;
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                if !members.outputs.contains(&output.id()) {
                    members.outputs.push(output.id());
                }
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                members.outputs.retain(|o| *o != output.id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if !members.workspaces.contains(&workspace.id()) {
                    members.workspaces.push(workspace.id());
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                members.workspaces.retain(|w| *w != workspace.id());
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                // The handle is inert at once; the group leaves the state
                // at the next `done`.
                group.removed = true;
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
        let entry = &mut state.workspaces[pos].1;
        let data = &mut entry.pending;
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
                // Inert at once; out of the state at the next `done`.
                entry.removed = true;
                handle.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_socket_comes_from_wayland_display_only() {
        let p = |d: Option<&str>, r: Option<&str>| {
            display_socket(d.map(Into::into), r.map(Into::into)).ok()
        };
        assert_eq!(
            p(Some("wayland-1"), Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/wayland-1"))
        );
        assert_eq!(
            p(Some("/tmp/w/wayland-9"), None),
            Some(PathBuf::from("/tmp/w/wayland-9"))
        );
        assert_eq!(
            p(None, Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/wayland-0"))
        );
        assert_eq!(p(Some("wayland-1"), None), None);
    }

    /// A request for a thread that has ended (its display gone) is answered
    /// `NotConnected`, not dropped.
    #[tokio::test]
    async fn a_request_after_the_display_went_away_is_not_connected() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let client =
            ProtocolClient::spawn(WaylandTarget::Socket(dir.path().join("gone")), tx).unwrap();
        // The disconnected state is the thread's last act: its command
        // channel is already closed.
        assert_eq!(rx.recv().await, Some(ProtocolState::default()));
        let (reply, outcome) = tokio::sync::oneshot::channel();
        client.send(ProtoCmd::Activate(1, Some(reply)));
        assert_eq!(outcome.await, Ok(Err(WmError::NotConnected)));
    }
}

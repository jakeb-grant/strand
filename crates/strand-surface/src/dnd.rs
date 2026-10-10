//! (M4) Drag and drop over `wl_data_device` (design.md, "Drag and drop";
//! docs/architecture.md, "strand-surface").
//!
//! Inside one surface a drag is the Router's alone (`strand-render`'s
//! `input.rs`): the pointer stays grabbed by the surface it was pressed
//! on, so its motion keeps arriving there. Two things need the
//! compositor:
//!
//! - **Other programs' drops.** An offer entering one of our surfaces
//!   becomes [`InputEvent::DragEnter`] (with what it carries: files, an
//!   app's `.desktop` file or text), `DragMotion`, `DragLeave`, and once
//!   dropped and read, `DragDrop` with a [`DropPayload::External`]. The
//!   offer is accepted (copy only) exactly while the host says something
//!   under the pointer takes it ([`SurfaceHost::drop_accepted`]), so the
//!   other program's cursor shows whether letting go would do anything.
//! - **Drags between our own surfaces.** When the pointer, still held,
//!   leaves the surface a `drag:` source was pressed on
//!   ([`SurfaceHost::drag_source`]), the manager starts a
//!   `wl_data_device` drag with the press serial. Its source offers one
//!   private MIME type naming this process, so when it enters one of our
//!   surfaces it is known for ours: `Drag*` events with no kinds, and a
//!   drop of [`DropPayload::Node`] with the node itself (no data is
//!   read). Dropped elsewhere or cancelled, the origin gets a made-up
//!   left-button release far outside it, which ends the Router's drag
//!   with nothing dropped (the real release went to the compositor).
//! - **Drags out to other programs** (M4 interaction-finish). A source
//!   whose value has a form outside Strand ([`SurfaceHost::drag_data`]:
//!   text, files, an app) offers it too: `text/uri-list` for files (an
//!   app: its `.desktop` file when one is found) and the text types for
//!   text (files: their paths, one a line; an app: its desktop id),
//!   written on request without blocking the loop. Another Strand
//!   process's drag is known by its private type's prefix and read like
//!   any other program's, through those types: across processes there is
//!   no node to deliver, so it arrives as a `Drop`; with nothing else
//!   offered it is refused like any offer of nothing readable.
//!
//! While the compositor carries a drag, its icon follows the pointer
//! (M4 interaction-finish): the source drawn alone at rest
//! ([`SurfaceHost::drag_image`]) in an shm buffer at the surface's
//! scale, held where it was grabbed, while the source itself springs
//! back to its box in its surface.
//!
//! This file is part of the manager (`manager/mod.rs` includes it as
//! `manager::dnd`), so it reaches `State`'s fields like the manager's
//! other parts do.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use calloop::{PostAction, RegistrationToken};
use smithay_client_toolkit::data_device_manager::data_device::{
    DataDevice, DataDeviceData, DataDeviceHandler,
};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{DataSourceHandler, DragSource};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, WritePipe};
use strand_scene::{DropKind, DropPayload};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;

use super::*;

/// The most of another program's drop that is read: more is a drop of
/// nothing (a warning), not unbounded memory.
pub const MAX_DROP_BYTES: usize = 4 << 20;

/// The longest another program's drop is read for: a sender that never
/// closes its pipe is a drop of nothing after this, not a reader (and up
/// to [`MAX_DROP_BYTES`]) kept for the life of the process.
pub const DROP_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// `text/uri-list` (files; an app when every file is a `.desktop` entry).
const URI_LIST: &str = "text/uri-list";
/// Text, in the order taken.
const TEXT: [&str; 4] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
];

/// The private MIME type's prefix every Strand process's drags offer
/// (then `pid=…;manager=…`).
pub const STRAND_MIME: &str = "application/x-strand-node;";

static NEXT_MANAGER: AtomicU32 = AtomicU32::new(1);

/// The manager's drag-and-drop state.
pub(super) struct Dnd {
    /// `wl_data_device_manager` (none: no drag and drop at all).
    manager: Option<DataDeviceManagerState>,
    /// The private MIME type our own drags offer (this process and
    /// manager).
    mime: String,
    devices: Vec<(wl_seat::WlSeat, DataDevice)>,
    /// The offer over one of our surfaces now.
    over: Option<Over>,
    /// Our own drag, while the compositor carries it.
    ours: Option<Ours>,
    /// Where the pointer was last pressed on one of our surfaces: where
    /// a drag icon is held.
    grab: LogicalPoint,
}

struct Over {
    surface: SurfaceId,
    what: Carried,
    /// What the offer was last told (`None`: nothing yet).
    accepted: Option<bool>,
}

enum Carried {
    /// Our own drag's node.
    Ours,
    /// Another program's, read as `mime`.
    Outside { mime: String },
}

struct Ours {
    source: DragSource,
    origin: SurfaceId,
    node: NodeId,
    /// What it gives other programs: MIME type, bytes.
    exports: Vec<(String, Vec<u8>)>,
    /// Its icon under the pointer.
    icon: Option<Icon>,
}

/// A drag icon's surface and what it shows.
struct Icon {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    viewport: Option<WpViewport>,
    _pool: RawPool,
    /// Its corner relative to the pointer.
    hot: (i32, i32),
    /// Buffer pixels.
    size: (i32, i32),
}

impl Drop for Icon {
    fn drop(&mut self) {
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        self.surface.destroy();
        self.buffer.destroy();
    }
}

impl Dnd {
    /// Binds `wl_data_device_manager` when the compositor offers it.
    pub(super) fn bind<H: SurfaceHost + 'static>(
        globals: &GlobalList,
        qh: &QueueHandle<State<H>>,
    ) -> Self {
        let n = NEXT_MANAGER.fetch_add(1, Ordering::Relaxed);
        Self {
            manager: DataDeviceManagerState::bind(globals, qh).ok(),
            mime: format!("{STRAND_MIME}pid={};manager={n}", std::process::id()),
            devices: Vec::new(),
            over: None,
            ours: None,
            grab: LogicalPoint::new(0.0, 0.0),
        }
    }
}

fn drag_offer(device: &WlDataDevice) -> Option<DragOffer> {
    device.data::<DataDeviceData>()?.drag_offer()
}

impl<H: SurfaceHost + 'static> State<H> {
    /// A seat got a pointer: it gets a data device too (once).
    pub(super) fn dnd_seat(&mut self, seat: &wl_seat::WlSeat) {
        let Some(m) = &self.dnd.manager else {
            return;
        };
        if !self.dnd.devices.iter().any(|(s, _)| s == seat) {
            let device = m.get_data_device(&self.qh, seat);
            self.dnd.devices.push((seat.clone(), device));
        }
    }

    /// A seat went: its data device is released.
    pub(super) fn dnd_seat_gone(&mut self, seat: &wl_seat::WlSeat) {
        self.dnd.devices.retain(|(s, _)| s != seat);
    }

    /// True while one of our drags is carried by the compositor.
    pub fn carrying_drag(&self) -> bool {
        self.dnd.ours.is_some()
    }

    /// After the host saw `event`: a held pointer that leaves the surface
    /// its `drag:` source is on hands the drag to the compositor.
    pub(super) fn dnd_input(&mut self, event: &InputEvent) {
        if let InputEvent::PointerButton {
            position,
            state: ButtonState::Pressed,
            ..
        } = event
        {
            self.dnd.grab = *position;
        }
        let InputEvent::PointerMotion {
            surface, position, ..
        } = event
        else {
            return;
        };
        if self.dnd.ours.is_some() || self.dnd.manager.is_none() {
            return;
        }
        let Some(s) = self.surfaces.get(surface) else {
            return;
        };
        let (w, h) = (s.logical.0 as f32, s.logical.1 as f32);
        let inside = position.x >= 0.0 && position.y >= 0.0 && position.x < w && position.y < h;
        if inside {
            return;
        }
        if let Some(node) = self.host.drag_source(*surface) {
            self.start_drag(*surface, node);
        }
    }

    /// The icon of `node` dragged out of `origin`, the source grabbed at
    /// `at` there: committed once, placed so the pointer holds it where
    /// it grabbed the source (as the source followed it in its surface).
    fn drag_icon(&mut self, origin: SurfaceId, node: NodeId, at: LogicalPoint) -> Option<Icon> {
        let img = self.host.drag_image(origin, node)?;
        let (w, h) = (
            i32::try_from(img.size.w).ok()?,
            i32::try_from(img.size.h).ok()?,
        );
        let len = img.pixels.len();
        if w == 0 || h == 0 || len != w as usize * h as usize * 4 {
            return None;
        }
        let mut pool = match RawPool::new(len, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("no drag icon: {e}");
                return None;
            }
        };
        pool.mmap()[..len].copy_from_slice(&img.pixels);
        let buffer = pool.create_buffer(
            0,
            w,
            h,
            w * 4,
            wl_shm::Format::Argb8888,
            scrim::ScrimObject,
            &self.qh,
        );
        let surface = self.compositor.create_surface(&self.qh);
        let scale = img.scale.as_f64();
        let logical = |px: i32| (px as f64 / scale).round() as i32;
        // An integer scale is the buffer's; a fractional one is drawn to
        // its logical size by the viewporter (without one, at 1:1).
        let viewport = if img.scale.is_integer() {
            surface.set_buffer_scale(scale as i32);
            None
        } else {
            self.viewporter.as_ref().map(|vp| {
                let v = vp.get_viewport(&surface, &self.qh, SurfaceTag(origin));
                v.set_destination(logical(w).max(1), logical(h).max(1));
                v
            })
        };
        // The icon's corner relative to the pointer: where it was grabbed.
        let hot = (
            -((at.x - img.origin.x).round() as i32),
            -((at.y - img.origin.y).round() as i32),
        );
        Some(Icon {
            surface,
            buffer,
            viewport,
            _pool: pool,
            hot,
            size: (w, h),
        })
    }

    fn start_drag(&mut self, origin: SurfaceId, node: NodeId) {
        let Some((seat, serial)) = self
            .pointers
            .iter()
            .find_map(|p| p.button_serial.map(|s| (p.seat.clone(), s)))
        else {
            return;
        };
        if self.dnd.manager.is_none() || !self.dnd.devices.iter().any(|(s, _)| *s == seat) {
            return;
        }
        let Some(wl) = self.surfaces.get(&origin).map(|s| s.wl().clone()) else {
            return;
        };
        let icon = self.drag_icon(origin, node, self.dnd.grab);
        let Some(m) = &self.dnd.manager else {
            return;
        };
        let Some((_, device)) = self.dnd.devices.iter().find(|(s, _)| *s == seat) else {
            return;
        };
        let exports = self
            .host
            .drag_data(node)
            .map(|p| exports_of(&p, desktop_file))
            .unwrap_or_default();
        let mimes =
            std::iter::once(self.dnd.mime.as_str()).chain(exports.iter().map(|(m, _)| m.as_str()));
        let source = m.create_drag_and_drop_source(&self.qh, mimes, DndAction::Copy);
        source.start_drag(device, &wl, icon.as_ref().map(|i| &i.surface), serial);
        // Shown once it is the drag's icon: its first commit places its
        // corner at the pointer, moved by the offset (a commit before
        // `start_drag` would leave it there, as sway 1.9 does).
        if let Some(i) = &icon {
            if i.surface.version() >= 5 {
                i.surface.offset(i.hot.0, i.hot.1);
                i.surface.attach(Some(&i.buffer), 0, 0);
            } else {
                i.surface.attach(Some(&i.buffer), i.hot.0, i.hot.1);
            }
            i.surface.damage_buffer(0, 0, i.size.0, i.size.1);
            i.surface.commit();
        }
        log::debug!(
            "drag of {node:?} leaves {origin:?}: the compositor carries it ({} types for other \
             programs)",
            exports.len()
        );
        self.dnd.ours = Some(Ours {
            source,
            origin,
            node,
            exports,
            icon,
        });
    }

    /// Our drag `source` ended (dropped anywhere, or cancelled): the
    /// origin's Router drag ends with a release far outside it.
    fn end_ours(&mut self, source: &WlDataSource) {
        if !self
            .dnd
            .ours
            .as_ref()
            .is_some_and(|o| o.source.inner() == source)
        {
            return;
        }
        let Some(ours) = self.dnd.ours.take() else {
            return;
        };
        if self.surfaces.contains_key(&ours.origin) {
            self.send_input(InputEvent::PointerButton {
                surface: ours.origin,
                position: LogicalPoint::new(-1e6, -1e6),
                button: crate::input::button::LEFT,
                state: ButtonState::Released,
                time: 0,
            });
        }
        // Its icon goes with it.
        drop(ours.icon);
    }

    /// Tells the offer whether a drop now would land: copy, the MIME type
    /// read, when the host has a target under the pointer.
    fn dnd_accept(&mut self, offer: &DragOffer) {
        let Some(over) = &self.dnd.over else {
            return;
        };
        let ok = self.host.drop_accepted(over.surface);
        if over.accepted == Some(ok) {
            return;
        }
        let mime = match &over.what {
            Carried::Ours => self.dnd.mime.clone(),
            Carried::Outside { mime } => mime.clone(),
        };
        offer.accept_mime_type(offer.serial, ok.then_some(mime));
        if let Some(over) = &mut self.dnd.over {
            over.accepted = Some(ok);
        }
    }

    /// Reads another program's drop without blocking the loop, then
    /// hands it to the host and finishes the offer. A drop that cannot be
    /// read (no pipe, a read error, more than [`MAX_DROP_BYTES`], or no
    /// end within [`DROP_READ_TIMEOUT`]) is a drop of nothing: see
    /// [`State::end_drop`].
    fn read_drop(&mut self, surface: SurfaceId, at: LogicalPoint, offer: DragOffer, mime: String) {
        let pipe = match offer.receive(mime.clone()) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("cannot read a drop ({mime}): {e}");
                self.end_drop(surface, at, offer, None);
                return;
            }
        };
        // Whichever ends first, the read or the time limit, takes the
        // offer and ends the drop; the other source is then removed.
        let pending = Rc::new(RefCell::new(Some(offer)));
        let limit: Rc<Cell<Option<RegistrationToken>>> = Rc::new(Cell::new(None));
        let mut buf = Vec::new();
        let read = self.handle.insert_source(pipe, {
            let (pending, limit) = (pending.clone(), limit.clone());
            move |_, file, state| {
                let end = |state: &mut Self, payload: Option<DropPayload>| {
                    if let Some(t) = limit.take() {
                        state.handle.remove(t);
                    }
                    if let Some(offer) = pending.borrow_mut().take() {
                        state.end_drop(surface, at, offer, payload);
                    }
                    PostAction::Remove
                };
                let mut chunk = [0u8; 16 * 1024];
                // Level-triggered: one read per wakeup never blocks.
                let mut f: &File = file;
                match f.read(&mut chunk) {
                    Ok(0) => end(state, Some(payload_of(&mime, &buf))),
                    Ok(n) if buf.len() + n > MAX_DROP_BYTES => {
                        log::warn!("a drop over {MAX_DROP_BYTES} bytes ({mime}) is ignored");
                        end(state, None)
                    }
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        PostAction::Continue
                    }
                    Err(e)
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                    {
                        PostAction::Continue
                    }
                    Err(e) => {
                        log::warn!("cannot read a drop ({mime}): {e}");
                        end(state, None)
                    }
                }
            }
        });
        let read = match read {
            Ok(t) => t,
            Err(e) => {
                log::warn!("cannot read a drop: {}", e.error);
                if let Some(offer) = pending.borrow_mut().take() {
                    self.end_drop(surface, at, offer, None);
                }
                return;
            }
        };
        let timer = Timer::from_duration(DROP_READ_TIMEOUT);
        let timed = self.handle.insert_source(timer, move |_, _, state| {
            if let Some(offer) = pending.borrow_mut().take() {
                log::warn!(
                    "a drop not read within {DROP_READ_TIMEOUT:?} is ignored (its sender never \
                     closed the pipe)"
                );
                state.handle.remove(read);
                state.end_drop(surface, at, offer, None);
            }
            TimeoutAction::Drop
        });
        match timed {
            Ok(t) => limit.set(Some(t)),
            Err(e) => log::warn!("a drop is read with no time limit: {}", e.error),
        }
    }

    /// Ends a dropped offer over `surface`. Read (`payload`): the offer is
    /// finished (its source sees `dnd_finished`) and the host gets
    /// `DragDrop`. Not read (`None`): the offer is destroyed unfinished,
    /// which the compositor tells its source as `cancelled`, and the host
    /// gets `DragLeave`, so nothing keeps the offer. Either way the
    /// dropped offer is destroyed: after a drop it is the client's to
    /// destroy.
    fn end_drop(
        &mut self,
        surface: SurfaceId,
        at: LogicalPoint,
        offer: DragOffer,
        payload: Option<DropPayload>,
    ) {
        if payload.is_some() {
            offer.finish();
        }
        offer.destroy();
        if !self.surfaces.contains_key(&surface) {
            return;
        }
        self.send_input(match payload {
            Some(payload) => InputEvent::DragDrop {
                surface,
                at,
                payload,
            },
            None => InputEvent::DragLeave { surface },
        });
    }
}

impl<H: SurfaceHost + 'static> DataDeviceHandler for State<H> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        wl: &wl_surface::WlSurface,
    ) {
        self.dnd.over = None;
        let Some(surface) = self.surface_for(wl) else {
            return;
        };
        let Some(offer) = drag_offer(device) else {
            return;
        };
        let mimes = offer.with_mime_types(<[String]>::to_vec);
        let (what, kinds) = if self.dnd.ours.is_some() && mimes.contains(&self.dnd.mime) {
            (Carried::Ours, Vec::new())
        } else {
            if mimes.iter().any(|m| m.starts_with(STRAND_MIME)) {
                log::debug!("a drag from another Strand process: read through what it offers");
            }
            match classify(&mimes) {
                Some((mime, kind)) => (Carried::Outside { mime }, vec![kind]),
                None => {
                    // Nothing we read: never accepted.
                    offer.accept_mime_type(offer.serial, None);
                    return;
                }
            }
        };
        offer.set_actions(DndAction::Copy, DndAction::Copy);
        self.dnd.over = Some(Over {
            surface,
            what,
            accepted: None,
        });
        self.send_input(InputEvent::DragEnter {
            surface,
            at: LogicalPoint::new(x as f32, y as f32),
            kinds,
        });
        self.dnd_accept(&offer);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if let Some(over) = self.dnd.over.take() {
            self.send_input(InputEvent::DragLeave {
                surface: over.surface,
            });
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        let Some(surface) = self.dnd.over.as_ref().map(|o| o.surface) else {
            return;
        };
        self.send_input(InputEvent::DragMotion {
            surface,
            at: LogicalPoint::new(x as f32, y as f32),
        });
        if let Some(offer) = drag_offer(device) {
            self.dnd_accept(&offer);
        }
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        let Some(over) = self.dnd.over.take() else {
            return;
        };
        let Some(offer) = drag_offer(device) else {
            // The offer is gone already: nothing to read or finish.
            self.send_input(InputEvent::DragLeave {
                surface: over.surface,
            });
            return;
        };
        let at = LogicalPoint::new(offer.x as f32, offer.y as f32);
        if over.accepted != Some(true) {
            // wlroots never drops an offer nobody accepted; another
            // compositor may. It lands nowhere.
            self.end_drop(over.surface, at, offer, None);
            return;
        }
        match over.what {
            Carried::Ours => {
                let payload = self.dnd.ours.as_ref().map(|o| DropPayload::Node(o.node));
                self.end_drop(over.surface, at, offer, payload);
            }
            Carried::Outside { mime } => self.read_drop(over.surface, at, offer, mime),
        }
    }
}

impl<H: SurfaceHost + 'static> DataOfferHandler for State<H> {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl<H: SurfaceHost + 'static> DataSourceHandler for State<H> {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    /// Our drags carry their node by identity: the private type is
    /// never written (a reader gets nothing). A type the drag exports is
    /// written a pipe-buffer's worth per wakeup, so the loop never blocks
    /// on a slow reader; a reader that closes early ends it.
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        let bytes = self
            .dnd
            .ours
            .as_ref()
            .filter(|o| o.source.inner() == source)
            .and_then(|o| o.exports.iter().find(|(m, _)| *m == mime))
            .map(|(_, b)| b.clone());
        let Some(bytes) = bytes else {
            drop(fd);
            return;
        };
        let mut done = 0;
        let written = self.handle.insert_source(fd, move |_, file, _| {
            // POLLOUT on a pipe: at least PIPE_BUF (4096) bytes fit.
            let end = (done + 4096).min(bytes.len());
            let mut f: &File = file;
            match std::io::Write::write(&mut f, &bytes[done..end]) {
                Ok(n) => {
                    done += n;
                    if done >= bytes.len() {
                        PostAction::Remove
                    } else {
                        PostAction::Continue
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    PostAction::Continue
                }
                Err(e) => {
                    log::debug!("a drag's {mime} was not all read: {e}");
                    PostAction::Remove
                }
            }
        });
        if let Err(e) = written {
            log::warn!("cannot give a drag's data: {}", e.error);
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.end_ours(source);
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.end_ours(source);
    }

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

/// What a drag carrying `payload` offers other programs, MIME type and
/// bytes: files as a `text/uri-list` (percent-encoded `file://` URIs)
/// and as text, their paths one a line; an app as its `.desktop` file's
/// URI when `desktop` finds it (by desktop id) and as text, its desktop
/// id; text as each text type. Nothing for a node of ours.
pub fn exports_of(
    payload: &DropPayload,
    desktop: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<(String, Vec<u8>)> {
    let DropPayload::External {
        kind,
        files,
        text,
        app_id,
    } = payload
    else {
        return Vec::new();
    };
    let (uris, plain): (Vec<PathBuf>, Vec<u8>) = match kind {
        DropKind::Files => {
            let mut plain = Vec::new();
            for (i, f) in files.iter().enumerate() {
                if i > 0 {
                    plain.push(b'\n');
                }
                plain.extend_from_slice(f.as_os_str().as_bytes());
            }
            (files.clone(), plain)
        }
        DropKind::App => {
            let id = app_id.clone().unwrap_or_default();
            (desktop(&id).into_iter().collect(), id.into_bytes())
        }
        DropKind::Text => (Vec::new(), text.clone().into_bytes()),
    };
    let mut out = Vec::new();
    if !uris.is_empty() {
        let mut list = Vec::new();
        for f in &uris {
            list.extend_from_slice(b"file://");
            percent_encode(f.as_os_str().as_bytes(), &mut list);
            list.extend_from_slice(b"\r\n");
        }
        out.push((URI_LIST.to_string(), list));
    }
    if !plain.is_empty() {
        out.extend(TEXT.iter().map(|t| ((*t).to_string(), plain.clone())));
    }
    out
}

/// The `.desktop` file of desktop id `id` in the XDG data directories
/// (`$XDG_DATA_HOME`, then `$XDG_DATA_DIRS`), if there is one.
pub fn desktop_file(id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains('/') {
        return None;
    }
    let home = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from))
        .map(|d| d.join("applications").join(id))
        .find(|p| p.is_file())
}

/// `path` percent-encoded into `out` (RFC 3986: unreserved bytes and `/`
/// kept).
fn percent_encode(path: &[u8], out: &mut Vec<u8>) {
    for &b in path {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b);
        } else {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
}

/// What an offer of `mimes` is read as, and what kind it is: files (or
/// an app, known once read) before text; `None` when it carries nothing
/// a Strand element takes.
pub fn classify(mimes: &[String]) -> Option<(String, DropKind)> {
    if mimes.iter().any(|m| m == URI_LIST) {
        return Some((URI_LIST.into(), DropKind::Files));
    }
    TEXT.iter()
        .find(|t| mimes.iter().any(|m| m == *t))
        .map(|t| ((*t).into(), DropKind::Text))
}

/// The drop `bytes` read as `mime` carry: a `text/uri-list`'s local
/// files, or the app whose `.desktop` entry they all are (by its file
/// name, the desktop entry id), or text.
pub fn payload_of(mime: &str, bytes: &[u8]) -> DropPayload {
    if mime != URI_LIST {
        return DropPayload::External {
            kind: DropKind::Text,
            files: Vec::new(),
            text: String::from_utf8_lossy(bytes).into_owned(),
            app_id: None,
        };
    }
    let files = uri_list(bytes);
    let desktop = |p: &PathBuf| p.extension().is_some_and(|e| e == "desktop");
    match files.first() {
        Some(first) if files.iter().all(desktop) => DropPayload::External {
            kind: DropKind::App,
            files: Vec::new(),
            text: String::new(),
            app_id: first.file_name().map(|n| n.to_string_lossy().into_owned()),
        },
        _ => DropPayload::External {
            kind: DropKind::Files,
            files,
            text: String::new(),
            app_id: None,
        },
    }
}

/// The local paths of a `text/uri-list` (RFC 2483): one URI per line,
/// `#` comments, `file:` URIs with an empty or `localhost` host,
/// percent-decoded. Other URIs are left out.
pub fn uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty() && !l.starts_with(b"#"))
        .filter_map(|l| {
            let rest = l.strip_prefix(b"file://")?;
            let path = match rest.strip_prefix(b"localhost") {
                Some(p) if p.starts_with(b"/") => p,
                _ if rest.starts_with(b"/") => rest,
                _ => return None,
            };
            Some(Path::new(std::ffi::OsStr::from_bytes(&percent_decode(path))).to_path_buf())
        })
        .collect()
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(h), Some(l)) = (
                s.get(i + 1).copied().and_then(hex),
                s.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(m: &[&str]) -> Vec<String> {
        m.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn offers_are_read_as_files_before_text_and_others_are_refused() {
        assert_eq!(
            classify(&mimes(&["text/plain", "text/uri-list"])),
            Some((URI_LIST.into(), DropKind::Files))
        );
        assert_eq!(
            classify(&mimes(&["text/plain", "text/plain;charset=utf-8"])),
            Some(("text/plain;charset=utf-8".into(), DropKind::Text))
        );
        assert_eq!(
            classify(&mimes(&["UTF8_STRING"])),
            Some(("UTF8_STRING".into(), DropKind::Text))
        );
        assert_eq!(classify(&mimes(&["image/png"])), None);
        assert_eq!(classify(&[]), None);
    }

    /// What a drag of ours gives other programs, read back the way a
    /// drop of theirs is: files round-trip through the URI list (spaces
    /// and non-ASCII encoded), an app through its `.desktop` file, text
    /// through each text type; a node gives nothing.
    #[test]
    fn exports_read_back_as_what_was_dragged() {
        let files = DropPayload::External {
            kind: DropKind::Files,
            files: vec!["/tmp/a b.png".into(), "/home/u/n\u{e4}me.txt".into()],
            text: String::new(),
            app_id: None,
        };
        let none = |_: &str| None;
        let out = exports_of(&files, none);
        let types: Vec<&str> = out.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(types, [URI_LIST, TEXT[0], TEXT[1], TEXT[2], TEXT[3]]);
        assert_eq!(
            out[0].1,
            b"file:///tmp/a%20b.png\r\nfile:///home/u/n%C3%A4me.txt\r\n"
        );
        assert_eq!(payload_of(URI_LIST, &out[0].1), files);
        assert_eq!(out[1].1, "/tmp/a b.png\n/home/u/n\u{e4}me.txt".as_bytes());
        let app = DropPayload::External {
            kind: DropKind::App,
            files: vec![],
            text: String::new(),
            app_id: Some("org.x.Y.desktop".into()),
        };
        let found = |id: &str| Some(PathBuf::from(format!("/usr/share/applications/{id}")));
        let out = exports_of(&app, found);
        assert_eq!(payload_of(URI_LIST, &out[0].1), app);
        assert_eq!(out[1].1, b"org.x.Y.desktop");
        let out = exports_of(&app, none);
        assert_eq!(out.len(), TEXT.len(), "no file found: the id as text only");
        let text = DropPayload::External {
            kind: DropKind::Text,
            files: vec![],
            text: "hi \u{2014}".into(),
            app_id: None,
        };
        let out = exports_of(&text, none);
        assert_eq!(out.len(), TEXT.len());
        assert_eq!(payload_of(&out[0].0, &out[0].1), text);
        assert!(exports_of(&DropPayload::Node(NodeId::new(1, 0)), none).is_empty());
        // Another Strand's private type is not read: its other types are.
        assert_eq!(
            classify(&mimes(&[
                "application/x-strand-node;pid=9;manager=1",
                "text/plain"
            ])),
            Some(("text/plain".into(), DropKind::Text))
        );
        assert_eq!(
            classify(&mimes(&["application/x-strand-node;pid=9;manager=1"])),
            None
        );
    }

    #[test]
    fn uri_lists_are_local_paths_and_desktop_entries_are_apps() {
        let list = b"# from a file manager\r\nfile:///tmp/a%20b.png\r\nfile://localhost/home/u/n%C3%A4me.txt\r\nhttps://example.com/x\r\nfile://otherhost/x\r\n";
        assert_eq!(
            uri_list(list),
            [
                PathBuf::from("/tmp/a b.png"),
                PathBuf::from("/home/u/n\u{e4}me.txt")
            ]
        );
        assert_eq!(
            payload_of(URI_LIST, list),
            DropPayload::External {
                kind: DropKind::Files,
                files: vec!["/tmp/a b.png".into(), "/home/u/n\u{e4}me.txt".into()],
                text: String::new(),
                app_id: None,
            }
        );
        assert_eq!(
            payload_of(
                URI_LIST,
                b"file:///usr/share/applications/org.gnome.Nautilus.desktop\n"
            ),
            DropPayload::External {
                kind: DropKind::App,
                files: vec![],
                text: String::new(),
                app_id: Some("org.gnome.Nautilus.desktop".into()),
            }
        );
        assert_eq!(
            payload_of("text/plain", "hi \u{2014} there".as_bytes()),
            DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "hi \u{2014} there".into(),
                app_id: None,
            }
        );
        // A bad escape is kept as it is.
        assert_eq!(uri_list(b"file:///a%zz%4"), [PathBuf::from("/a%zz%4")]);
    }
}

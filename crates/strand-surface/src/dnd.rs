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
//!
//! No drag icon follows the pointer while the compositor carries a drag:
//! the source springs back to its box as the pointer leaves its surface
//! (recorded in docs/decisions.md).
//!
//! This file is part of the manager (`manager/mod.rs` includes it as
//! `manager::dnd`), so it reaches `State`'s fields like the manager's
//! other parts do.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use calloop::PostAction;
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

/// `text/uri-list` (files; an app when every file is a `.desktop` entry).
const URI_LIST: &str = "text/uri-list";
/// Text, in the order taken.
const TEXT: [&str; 4] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
];

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
            mime: format!(
                "application/x-strand-node;pid={};manager={n}",
                std::process::id()
            ),
            devices: Vec::new(),
            over: None,
            ours: None,
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

    fn start_drag(&mut self, origin: SurfaceId, node: NodeId) {
        let Some(m) = &self.dnd.manager else {
            return;
        };
        let Some((seat, serial)) = self
            .pointers
            .iter()
            .find_map(|p| p.button_serial.map(|s| (p.seat.clone(), s)))
        else {
            return;
        };
        let Some((_, device)) = self.dnd.devices.iter().find(|(s, _)| *s == seat) else {
            return;
        };
        let Some(wl) = self.surfaces.get(&origin).map(|s| s.wl().clone()) else {
            return;
        };
        let source =
            m.create_drag_and_drop_source(&self.qh, [self.dnd.mime.as_str()], DndAction::Copy);
        source.start_drag(device, &wl, None, serial);
        log::debug!("drag of {node:?} leaves {origin:?}: the compositor carries it");
        self.dnd.ours = Some(Ours {
            source,
            origin,
            node,
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
    /// hands it to the host and finishes the offer.
    fn read_drop(&mut self, surface: SurfaceId, at: LogicalPoint, offer: DragOffer, mime: String) {
        let pipe = match offer.receive(mime.clone()) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("cannot read a drop ({mime}): {e}");
                return;
            }
        };
        let mut buf = Vec::new();
        let read = self.handle.insert_source(pipe, move |_, file, state| {
            let mut chunk = [0u8; 16 * 1024];
            // Level-triggered: one read per wakeup never blocks.
            let mut f: &File = file;
            match f.read(&mut chunk) {
                Ok(0) => {
                    offer.finish();
                    if state.surfaces.contains_key(&surface) {
                        let payload = payload_of(&mime, &buf);
                        state.send_input(InputEvent::DragDrop {
                            surface,
                            at,
                            payload,
                        });
                    }
                    PostAction::Remove
                }
                Ok(n) if buf.len() + n > MAX_DROP_BYTES => {
                    log::warn!("a drop over {MAX_DROP_BYTES} bytes ({mime}) is ignored");
                    offer.finish();
                    PostAction::Remove
                }
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    PostAction::Continue
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    PostAction::Continue
                }
                Err(e) => {
                    log::warn!("cannot read a drop ({mime}): {e}");
                    PostAction::Remove
                }
            }
        });
        if let Err(e) = read {
            log::warn!("cannot read a drop: {}", e.error);
        }
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
            return;
        };
        if over.accepted != Some(true) {
            return;
        }
        let at = LogicalPoint::new(offer.x as f32, offer.y as f32);
        match over.what {
            Carried::Ours => {
                let Some(node) = self.dnd.ours.as_ref().map(|o| o.node) else {
                    return;
                };
                offer.finish();
                self.send_input(InputEvent::DragDrop {
                    surface: over.surface,
                    at,
                    payload: DropPayload::Node(node),
                });
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

    /// Our drags carry their node by identity, not as data: a reader
    /// (another program) gets nothing.
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: String,
        fd: WritePipe,
    ) {
        drop(fd);
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

//! (M6) Copy and paste for a focused `input` over `wl_data_device`'s
//! selection (spec.md 6.7; decisions.md m6-service-gaps, owner-testing
//! gap 30).
//!
//! Render's Router turns Ctrl+C/X/V in an `input` into
//! [`ClipboardAsk`]s, which the host hands over through
//! [`SurfaceHost::take_clipboard_asks`] right after the key press reached
//! it ([`State::send_input`]), so each is answered with that press's
//! serial:
//!
//! - **Copy**: a `wl_data_source` offering the text types (and a private
//!   type naming this process) becomes the selection of the press's
//!   seat; its bytes are written to whoever asks
//!   ([`State::write_all`], as a drag's export is), until another program
//!   takes the selection (`cancelled`).
//! - **Paste**: the selection's offer is read as text without blocking the
//!   loop (at most [`MAX_PASTE_BYTES`], within [`PASTE_READ_TIMEOUT`]) and
//!   handed back as [`InputEvent::Paste`]. Our own selection is handed
//!   back from memory, no pipe.
//!
//! This works on every compositor with a data device (keyboard focus and
//! a press serial are all it needs), data control or not; the builtin
//! `clipboard` service is a separate client (`strand_services::clipboard`).
//!
//! Part of the manager (`manager/mod.rs` includes it as
//! `manager::selection`), like `dnd.rs`.

use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceData};
use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use strand_scene::ClipboardAsk;
use wayland_client::protocol::wl_data_source::WlDataSource;

use super::*;

/// The most of a paste that is read: more is a paste of nothing (a
/// warning), not unbounded memory.
pub const MAX_PASTE_BYTES: usize = 1 << 20;

/// The longest a paste is read for: a sender that never closes its pipe
/// is a paste of nothing after this.
pub const PASTE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The text types a copy offers and a paste reads, in the order taken.
pub const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// The private type's prefix of every Strand process's copy (then
/// `pid=…;manager=…`): never written, it tells our own selection apart.
pub const STRAND_SELECTION_MIME: &str = "application/x-strand-selection;";

static NEXT_MANAGER: AtomicU32 = AtomicU32::new(1);

/// The manager's selection state.
pub(super) struct Selection {
    /// The private type this manager's copies offer.
    mime: String,
    /// Our copy while it is the selection: its source and its text.
    ours: Option<(CopyPasteSource, Rc<[u8]>)>,
}

impl Selection {
    pub(super) fn new() -> Self {
        let n = NEXT_MANAGER.fetch_add(1, Ordering::Relaxed);
        Self {
            mime: format!(
                "{STRAND_SELECTION_MIME}pid={};manager={n}",
                std::process::id()
            ),
            ours: None,
        }
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// What the host asked of the clipboard while it handled `event`.
    pub(super) fn clipboard_asks(&mut self) {
        for ask in self.host.take_clipboard_asks() {
            match ask {
                ClipboardAsk::Copy { surface, text } => self.copy(surface, text),
                ClipboardAsk::Paste { surface } => self.paste(surface),
            }
        }
    }

    /// The data device of the seat of the last press on our surfaces,
    /// with that press's serial.
    fn press_device(&self) -> Option<(&DataDevice, u32)> {
        let action = self.last_action.as_ref()?;
        let device = self
            .dnd
            .devices
            .iter()
            .find(|(s, _)| *s == action.seat)
            .map(|(_, d)| d)?;
        Some((device, action.serial))
    }

    /// Makes `text` the selection (a copy or a cut in an `input` on
    /// `surface`), with the serial of the key press that asked.
    fn copy(&mut self, surface: SurfaceId, text: String) {
        let Some(manager) = self.dnd.manager.as_ref() else {
            log::debug!("no copy on {surface:?}: the compositor offers no data device");
            return;
        };
        let Some((device, serial)) = self.press_device() else {
            log::debug!("no copy on {surface:?}: no press to copy with");
            return;
        };
        let mimes = std::iter::once(self.selection.mime.as_str()).chain(TEXT_TYPES);
        let source = manager.create_copy_paste_source(&self.qh, mimes);
        source.set_selection(device, serial);
        // The copy before it (if still ours) is destroyed with its source.
        self.selection.ours = Some((source, Rc::from(text.into_bytes())));
    }

    /// Reads the selection as text for `surface` (a paste in an `input`
    /// there): [`InputEvent::Paste`] once read. Nothing when there is no
    /// selection or no text in it.
    fn paste(&mut self, surface: SurfaceId) {
        let offer = self
            .press_device()
            .and_then(|(d, _)| d.inner().data::<DataDeviceData>()?.selection_offer());
        let mimes = offer
            .as_ref()
            .map(|o| o.with_mime_types(<[String]>::to_vec))
            .unwrap_or_default();
        // Our own copy: from memory.
        if let Some((_, bytes)) = &self.selection.ours
            && (offer.is_none() || mimes.contains(&self.selection.mime))
        {
            let text = String::from_utf8_lossy(bytes).into_owned();
            self.send_input(InputEvent::Paste { surface, text });
            return;
        }
        let Some(offer) = offer else {
            log::debug!("nothing to paste on {surface:?}: no selection");
            return;
        };
        let Some(mime) = TEXT_TYPES
            .iter()
            .find(|t| mimes.iter().any(|m| m == *t))
            .map(|t| (*t).to_string())
        else {
            log::debug!("nothing to paste on {surface:?}: the selection holds no text");
            return;
        };
        let pipe = match offer.receive(mime.clone()) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("cannot read a paste ({mime}): {e}");
                return;
            }
        };
        self.read_all(
            pipe,
            MAX_PASTE_BYTES,
            PASTE_READ_TIMEOUT,
            format!("a paste ({mime})"),
            move |state, bytes| {
                if let Some(b) = bytes
                    && state.surfaces.contains_key(&surface)
                {
                    let text = String::from_utf8_lossy(&b).into_owned();
                    state.send_input(InputEvent::Paste { surface, text });
                }
            },
        );
    }

    /// Whether `source` is our copy's.
    pub(super) fn is_our_selection(&self, source: &WlDataSource) -> bool {
        self.selection
            .ours
            .as_ref()
            .is_some_and(|(s, _)| s.inner() == source)
    }

    /// Another program asked for our copy as `mime`: its text, for a text
    /// type (the private type is never written).
    pub(super) fn serve_selection(&mut self, mime: &str, fd: WritePipe) {
        let Some((_, bytes)) = &self.selection.ours else {
            return;
        };
        if !TEXT_TYPES.contains(&mime) {
            drop(fd);
            return;
        }
        let bytes = bytes.clone();
        self.write_all(
            fd,
            bytes,
            super::dnd::DRAG_WRITE_TIMEOUT,
            format!("a copy's {mime}"),
        );
    }

    /// Another program took the selection: our copy goes.
    pub(super) fn selection_taken(&mut self) {
        self.selection.ours = None;
    }

    /// (tests) Whether our copy is the selection, as far as we know.
    pub fn holds_selection(&self) -> bool {
        self.selection.ours.is_some()
    }
}

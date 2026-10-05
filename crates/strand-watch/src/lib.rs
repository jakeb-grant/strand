//! Change sources that become writes into the reactive graph.
//!
//! - [`Watcher`]: directory (never file) inotify watches through `notify`,
//!   acting on `CLOSE_WRITE` and `MOVED_TO` (never `MODIFY`), editor
//!   scratch names filtered, symlink chains followed (the link's directory
//!   and the target's directory are both watched; a link swap is an edit),
//!   network filesystems polled with content comparison, a queue overflow
//!   answered with a full rescan. Saves are coalesced (15 ms after the last
//!   completed write) and deduplicated by BLAKE3 hash, Strand's own writes
//!   included.
//! - [`PortalSettings`] (or [`follow`] on a shared runtime and connection):
//!   `org.freedesktop.portal.Settings` `ReadOne` at boot and again when
//!   the portal (re)starts, and `SettingChanged`, for `system.dark`,
//!   `system.accent`, `system.contrast`.
//! - [`CompositorEvent`]: the slot the M3 compositor adapters send
//!   `wm.config_reloaded` through.
//!
//! Everything arrives as [`ChangeEvent`]s on one channel ([`channel`]). The
//! watcher never parses: it sends paths, kinds and hashes. See
//! `docs/design.md`, "Live reload and real-time config changes", and
//! `docs/architecture.md`, "strand-watch".

mod core;
mod event;
mod paths;
mod portal;
mod watcher;

pub use crate::core::{ConfigWatch, ModuleSet, Options, RescanFn};
pub use event::{
    CacheKind, ChangeEvent, ChangeKind, ColorScheme, CompositorEvent, ContentHash, Contrast,
    EventSink, FileBatch, FileChange, Notice, PollReason, RescanReason, Role, SystemBatch,
    SystemSetting, channel, hash_bytes,
};
pub use paths::is_scratch;
pub use portal::{
    APPEARANCE, BOOT_READ_TIMEOUT, Bus, CONNECT_TIMEOUT, KEYS, PortalSettings, follow,
    parse_setting,
};
pub use watcher::Watcher;

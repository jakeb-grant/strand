//! What the watcher sends: typed change events, never parsed content.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

/// BLAKE3 hash of a file's bytes.
pub type ContentHash = blake3::Hash;

/// Hash bytes the way the watcher does, for [`crate::Watcher::register_own_write`].
pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    blake3::hash(bytes)
}

/// Why a path is watched, echoed on every change to it. A path watched
/// for several reasons (a module also registered as a settings file, a
/// wallpaper also shown by an `image`) gets one [`FileChange`] per role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// A `.strand` file of the config's module set (`source::find_files`).
    Module,
    /// A `.wgsl` file referenced by `shader "…"`.
    Shader,
    /// A settings file (`state x from "prefs.toml"`).
    Settings,
    /// The wallpaper image.
    Wallpaper,
    /// Any other referenced file (`service … from file`, …).
    Other,
    /// A file under an app, icon or font directory: invalidate that cache.
    Cache(CacheKind),
}

/// Cache-invalidation sources (design.md, "Change sources").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CacheKind {
    /// `applications/` directories (desktop entries).
    Apps,
    /// Icon theme directories (`index.theme`).
    Icons,
    /// fontconfig font directories.
    Fonts,
}

/// What happened to a path between two batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// The path did not exist (or was not in the set) and now does.
    Created,
    /// The content changed, or a symlink on the way now resolves to
    /// another file (`canonical` differs; `hash` may be the same, so a
    /// loader skips the recompile but updates the path). Cache-tree
    /// changes are always `Modified` or `Removed`.
    Modified,
    /// The path is gone (or left the module set).
    Removed,
}

/// One changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// The path as registered (module files: as `find_files` returned it;
    /// cache trees: the canonical directory joined with the entry name).
    pub path: PathBuf,
    /// Where `path` resolves after following every symlink; `None` when
    /// removed.
    pub canonical: Option<PathBuf>,
    /// Created, modified or removed.
    pub kind: ChangeKind,
    /// BLAKE3 of the new content; `None` when removed, unreadable (see
    /// `error`) or a cache-tree entry (those are not hashed).
    pub hash: Option<ContentHash>,
    /// Why the path is watched.
    pub role: Role,
    /// Set when the file exists but could not be read.
    pub error: Option<io::ErrorKind>,
}

/// Why a batch rescanned everything instead of following events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    /// The inotify queue overflowed; events were lost.
    Overflow,
    /// `Watcher::rescan` (`strand reload`).
    Requested,
}

/// Something the watcher wants the user to know; not a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// This directory is polled (with content comparison) instead of
    /// watched: a network or FUSE filesystem, a forced poll, or a failed
    /// inotify watch (with the error).
    Polling { dir: PathBuf, reason: PollReason },
    /// The module-set rescan callback failed; the previous set is kept.
    RescanFailed(String),
    /// Reading the inotify queue (or waiting on it) failed.
    Backend(String),
    /// This watched file was being written (`MODIFY` seen) and its writer
    /// neither wrote again nor closed it for `Options::stalled_write`
    /// (5 s), so it was read anyway; the content may be incomplete.
    StalledWrite(PathBuf),
}

/// Why a directory is polled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollReason {
    /// NFS, SMB/CIFS, 9p, Ceph, AFS, Coda or FUSE: no events for remote
    /// writes.
    NoEventsFilesystem,
    /// `Options::force_polling`.
    Forced,
    /// inotify refused the watch (limit reached or another error).
    WatchFailed(String),
}

/// All file changes of one coalesced quiet period ("save all" is one batch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBatch {
    /// Changed paths, sorted by path then role; each (path, role) at most
    /// once.
    pub changes: Vec<FileChange>,
    /// Set when this batch comes from a full rescan.
    pub rescan: Option<RescanReason>,
    /// Polling fallbacks, rescan failures and backend errors.
    pub notices: Vec<Notice>,
    /// When the first event of this quiet period arrived.
    pub first_event: Instant,
    /// When the last event that kept the period open arrived; the batch is
    /// cut a quiet period (15 ms, or the removal grace) after it. Sent
    /// minus `last_event` is the watcher's share of save-to-pixels.
    pub last_event: Instant,
}

/// An `org.freedesktop.appearance` setting from the portal, already typed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SystemSetting {
    /// `system.dark`: `color-scheme` is 1 (prefer dark). `scheme` keeps the
    /// raw preference (no preference and prefer light are both not dark).
    Dark { dark: bool, scheme: ColorScheme },
    /// `system.accent`: `accent-color` as sRGB in 0..=1; `None` when unset
    /// (out-of-range components mean unset per the portal spec).
    Accent(Option<[f64; 3]>),
    /// `system.contrast`: `contrast` (0 normal, 1 high).
    Contrast(Contrast),
}

impl SystemSetting {
    /// The graph path this setting writes.
    pub fn path(&self) -> &'static str {
        match self {
            SystemSetting::Dark { .. } => "system.dark",
            SystemSetting::Accent(_) => "system.accent",
            SystemSetting::Contrast(_) => "system.contrast",
        }
    }
}

/// The portal's `color-scheme` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    NoPreference,
    PreferDark,
    PreferLight,
}

/// The portal's `contrast` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contrast {
    Normal,
    High,
}

/// Portal settings read at boot (`ReadOne`) or changed (`SettingChanged`).
#[derive(Debug, Clone, PartialEq)]
pub struct SystemBatch {
    /// Each setting at most once.
    pub settings: Vec<SystemSetting>,
    /// True for the boot read: these are initial values, not changes, so
    /// `on change` must not fire for them. Values read later (a portal that
    /// starts or restarts after boot, a boot read that timed out) come with
    /// `at_boot: false`; logic writes them like changes, and a value equal
    /// to the current one changes nothing.
    pub at_boot: bool,
    /// When the values were read or the signal arrived.
    pub received: Instant,
}

/// Compositor events (M3 adapters: Hyprland socket2, niri event stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorEvent {
    /// `wm.config_reloaded`: niri `ConfigLoaded { failed }` gives
    /// `Some(failed)`; Hyprland's `configreloaded` carries no result, so
    /// `None`.
    ConfigReloaded { failed: Option<bool> },
}

/// Everything the watcher thread and its sources send to logic.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeEvent {
    /// Files, directories and symlinks.
    Files(FileBatch),
    /// Portal appearance settings.
    System(SystemBatch),
    /// Compositor events, sent by the M3 adapters through an [`EventSink`].
    Compositor(CompositorEvent),
}

/// The sending side shared by every change source. Cloneable; each send
/// calls the optional waker so a calloop or eventfd loop can wake.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<ChangeEvent>,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSink")
            .field("waker", &self.waker.is_some())
            .finish()
    }
}

impl EventSink {
    /// Send one event; `false` when the receiver is gone.
    pub fn send(&self, event: ChangeEvent) -> bool {
        let ok = self.tx.send(event).is_ok();
        if ok && let Some(w) = &self.waker {
            w();
        }
        ok
    }

    /// Call `waker` after every send (e.g. a calloop `Ping`).
    pub fn with_waker(mut self, waker: impl Fn() + Send + Sync + 'static) -> Self {
        self.waker = Some(Arc::new(waker));
        self
    }
}

/// A sink and the receiver logic reads [`ChangeEvent`]s from.
pub fn channel() -> (EventSink, Receiver<ChangeEvent>) {
    let (tx, rx) = mpsc::channel();
    (EventSink { tx, waker: None }, rx)
}

//! The watcher's state machine: raw directory events in, coalesced and
//! hash-checked [`FileBatch`]es out. It reads the filesystem but owns no
//! inotify instance; a [`Backend`] adds and removes directory watches, so
//! tests can drive it with no events at all (the overflow test does).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::event::{
    CacheKind, ChangeKind, ContentHash, FileBatch, FileChange, Notice, PollReason, RescanReason,
    Role,
};
use crate::paths::{self, FsKind, Resolved};

/// Timing knobs. The defaults are design.md's numbers where it gives them.
#[derive(Debug, Clone)]
pub struct Options {
    /// Quiet time after the last completed write before a batch is cut
    /// (design.md: 15 ms, so "save all" is one batch).
    pub coalesce: Duration,
    /// Quiet time when the latest event removed a watched file, so a
    /// delete-and-create save is one `Modified`, not `Removed` + `Created`.
    pub removal_grace: Duration,
    /// A stream of writes that never goes quiet is still cut this long
    /// after its first event. A file still open for writing is not read
    /// at that cut; it waits for its `CLOSE_WRITE` (see `stalled_write`).
    /// A file whose read was put off (it may have been torn) is read
    /// within this long of the event that made it due, however often it
    /// is rewritten.
    pub max_delay: Duration,
    /// A file with a write in progress (`MODIFY` seen, no `CLOSE_WRITE`
    /// yet) is never read. If its writer goes this long without another
    /// write and without closing it, it is read anyway, with
    /// [`Notice::StalledWrite`].
    pub stalled_write: Duration,
    /// How often polled directories (network filesystems) are compared.
    /// A watched file in a polled directory is re-hashed only when its
    /// stat stamp (inode, size, times; refreshed by an `open`) changed.
    pub poll_interval: Duration,
    /// How often polled files are re-hashed even with an unchanged stamp,
    /// in case an attribute cache hid an edit.
    pub content_sweep: Duration,
    /// Files larger than this are never swept, only compared by stamp (a
    /// wallpaper is not re-read over the network every sweep).
    pub sweep_max_bytes: u64,
    /// Poll every directory instead of using inotify (tests, or a user who
    /// knows their filesystem drops events).
    pub force_polling: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            coalesce: Duration::from_millis(15),
            removal_grace: Duration::from_millis(50),
            max_delay: Duration::from_millis(500),
            stalled_write: Duration::from_secs(5),
            poll_interval: Duration::from_secs(1),
            content_sweep: Duration::from_secs(30),
            sweep_max_bytes: 1 << 20,
            force_polling: false,
        }
    }
}

/// The config's `.strand` module set, as `source::find_files` returns it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSet {
    /// Module files (`Discovery::files`).
    pub files: Vec<PathBuf>,
    /// Canonical directories scanned (`Discovery::dirs`).
    pub dirs: Vec<PathBuf>,
    /// Paths that could not be read, with the reason (`Discovery::errors`,
    /// the error as text): a dangling `*.strand` link, an unreadable
    /// directory.
    pub errors: Vec<(PathBuf, String)>,
    /// Directories too deep to load that hold `.strand` files
    /// (`Discovery::too_deep`).
    pub too_deep: Vec<PathBuf>,
}

/// Recomputes the module set (the binary calls `source::find_files`).
pub type RescanFn = Box<dyn FnMut() -> io::Result<ModuleSet> + Send>;

/// The config directory to watch.
pub struct ConfigWatch {
    /// The config directory as the user names it (`~/.config/strand`,
    /// possibly a symlink).
    pub root: PathBuf,
    /// The module set at boot.
    pub modules: ModuleSet,
    /// Called when the set may have changed: a `.strand` file or directory
    /// appeared or vanished, a directory link was swapped, or a rescan.
    pub rescan: RescanFn,
}

impl std::fmt::Debug for ConfigWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigWatch")
            .field("root", &self.root)
            .field("modules", &self.modules)
            .finish_non_exhaustive()
    }
}

/// One directory event, already reduced to what Strand acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Raw {
    /// A completed write: `CLOSE_WRITE` or `MOVED_TO`. Ends any write in
    /// progress on the path.
    Written(PathBuf),
    /// A name created complete, with no `CLOSE_WRITE` to come under it: a
    /// symlink, a hard link, a FIFO, or a file linked in from `O_TMPFILE`.
    /// A plain file created by a writer that already wrote its first
    /// bytes looks the same here, but its `MODIFY` follows at once and
    /// holds it until its `CLOSE_WRITE`.
    Linked(PathBuf),
    /// `DELETE`, `MOVED_FROM`, a directory deleted, or a watched directory
    /// deleted or moved.
    Gone(PathBuf),
    /// A directory created or moved in.
    Dir(PathBuf),
    /// `MODIFY` or an empty file created: a write in progress. The file is
    /// not read until its `CLOSE_WRITE` (or `stalled_write`).
    Busy(PathBuf),
    /// A plain file created with bytes already in it, in a directory
    /// watched without `MODIFY`: linked in complete from `O_TMPFILE`, or a
    /// writer that wrote before the creation was read. No `MODIFY` will
    /// tell them apart, so a watched file is held as if being written
    /// until its `CLOSE_WRITE` or until it stops changing for
    /// `stalled_write`. A cache-tree entry is reported at once.
    Created(PathBuf),
    /// The inotify queue overflowed.
    Overflow,
}

/// What a directory watch is for. Ordered weakest first: a directory
/// wanted for several reasons gets the strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum WatchKind {
    /// An ancestor of a watched directory, watched only so that its
    /// children being moved or deleted is seen (`mv ~/x ~/w` when only
    /// `~/x/y/z` holds a referenced file). Writes in it queue nothing.
    Ancestor,
    /// A directory where only names matter: the config root's parent
    /// (`~/.config`), a directory holding a symlink on the way, or the
    /// nearest existing ancestor of a missing directory. Names appearing
    /// and going are seen; writes to files in it queue nothing.
    Parent,
    /// A directory whose entries' contents matter but which other
    /// programs write to (a wallpaper's or settings file's directory, a
    /// symlink hop such as `~`, a cache tree): completed writes
    /// (`CLOSE_WRITE`, `MOVED_TO`) and names, no `MODIFY`, so a process
    /// writing there costs one wakeup per file closed, not per write.
    Completed,
    /// A config directory: [`WatchKind::Completed`] plus `MODIFY`, which
    /// marks a write in progress so a half-written module is never read.
    Full,
}

/// Adds and removes inotify directory watches. Watching a directory again
/// with another kind replaces its mask.
pub(crate) trait Backend {
    fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String>;
    fn unwatch(&mut self, dir: &Path);
    /// Take every event queued so far without waiting. A flush calls it
    /// after hashing, to see whether a file was written while it was read.
    /// A backend with no queue (polling, most tests) has nothing.
    fn drain(&mut self, _out: &mut Vec<Raw>) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inotify,
    Poll,
    /// An immutable store: nothing changes in place.
    Skip,
}

#[derive(Debug)]
struct Entry {
    /// In the config's module set.
    module: bool,
    /// Joined the module set since the last check while already watched
    /// for another role: `Role::Module` is reported `Created` even though
    /// the bytes are known.
    module_new: bool,
    /// The loader's registrations (`set_referenced`, which replaces
    /// them), counted per role.
    loader: BTreeMap<Role, usize>,
    /// Ad-hoc registrations (`watch_file` / `unwatch_file`), counted per
    /// role; `set_referenced` never touches them.
    explicit: BTreeMap<Role, usize>,
    resolved: Resolved,
    hash: Option<ContentHash>,
    stamp: Option<Stamp>,
    /// Set when the path exists but could not be hashed.
    error: Option<io::ErrorKind>,
    exists: bool,
    /// The canonical path last reported (or seen at registration): a link
    /// swap to identical bytes still reports the new path.
    reported: PathBuf,
}

impl Entry {
    /// A new entry with its links resolved and no baseline yet. Its
    /// directories are watched first and [`Entry::take_baseline`] reads it
    /// after (watch, then read): a save in between is either in the
    /// baseline or makes an event.
    fn unread(path: &Path) -> Entry {
        let resolved = paths::resolve(path);
        Entry {
            module: false,
            module_new: false,
            loader: BTreeMap::new(),
            explicit: BTreeMap::new(),
            reported: resolved.path.clone(),
            resolved,
            hash: None,
            stamp: None,
            error: None,
            exists: false,
        }
    }

    /// Record the file's current state as the baseline, reporting nothing.
    fn take_baseline(&mut self, path: &Path) {
        (self.hash, self.stamp, self.error, self.exists) = match read_hash(path) {
            Ok(r) => (Some(r.hash), Some(r.stamp), None, true),
            Err(e) if is_missing(&e) => (None, None, None, false),
            Err(e) => (None, current_stamp(path), Some(e.kind()), true),
        };
    }

    fn unused(&self) -> bool {
        !self.module && self.loader.is_empty() && self.explicit.is_empty()
    }

    fn roles(&self) -> BTreeSet<Role> {
        let mut roles: BTreeSet<Role> = self
            .loader
            .keys()
            .chain(self.explicit.keys())
            .copied()
            .collect();
        if self.module {
            roles.insert(Role::Module);
        }
        roles
    }
}

#[derive(Debug)]
struct Tree {
    root: PathBuf,
    depth: usize,
    kind: CacheKind,
    resolved: Resolved,
    dirs: Vec<PathBuf>,
}

#[derive(Debug)]
struct OwnWrite {
    seq: u64,
    canonical: PathBuf,
    path: PathBuf,
    hash: ContentHash,
    at: Instant,
}

/// Own-write registrations nobody matched are dropped after this long.
const OWN_WRITE_TTL: Duration = Duration::from_secs(10);

/// Registered own writes. Shared with [`crate::Watcher`], which adds to it
/// on the caller's thread: a registration is in place before the caller
/// writes, whatever the watcher thread is doing (a flush already under
/// way sees it).
#[derive(Debug, Default)]
pub(crate) struct OwnWrites {
    list: Vec<OwnWrite>,
    seq: u64,
}

/// The handle both sides hold.
pub(crate) type SharedOwnWrites = Arc<Mutex<OwnWrites>>;

impl OwnWrites {
    /// Add a registration. `path` is absolute and `canonical` its resolved
    /// form, both computed by the caller before taking the lock (resolving
    /// can be slow on a network filesystem, and the watcher thread takes
    /// this lock for every file it hashes).
    pub(crate) fn register(
        &mut self,
        path: PathBuf,
        canonical: PathBuf,
        hash: ContentHash,
        now: Instant,
    ) {
        self.expire(now);
        self.seq += 1;
        self.list.push(OwnWrite {
            seq: self.seq,
            canonical,
            path,
            hash,
            at: now,
        });
    }

    fn expire(&mut self, now: Instant) {
        self.list
            .retain(|o| now.saturating_duration_since(o.at) < OWN_WRITE_TTL);
    }

    /// Whether `hash` at `path` is a registered own write. Registrations
    /// for the same file made before the matched one were superseded (a
    /// slider written several times in one quiet period) and are dropped
    /// too, so a later user save with those bytes is not swallowed.
    fn take(&mut self, path: &Path, canonical: &Path, hash: ContentHash) -> bool {
        let same_file = |o: &OwnWrite| o.path == path || o.canonical == canonical;
        let Some(seq) = self
            .list
            .iter()
            .filter(|o| o.hash == hash && same_file(o))
            .map(|o| o.seq)
            .max()
        else {
            return false;
        };
        self.list.retain(|o| !(same_file(o) && o.seq <= seq));
        true
    }
}

fn lock(own: &SharedOwnWrites) -> std::sync::MutexGuard<'_, OwnWrites> {
    // A panic while holding it leaves a valid list.
    own.lock().unwrap_or_else(|e| e.into_inner())
}

/// A file the loader read, to register for a role: `loaded` is the hash
/// of the bytes it read, when it knows them. If the file no longer holds
/// those bytes when it is registered (a save landed between the read and
/// the registration), that is reported as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referenced {
    pub path: PathBuf,
    pub role: Role,
    pub loaded: Option<ContentHash>,
}

impl From<(PathBuf, Role)> for Referenced {
    fn from((path, role): (PathBuf, Role)) -> Self {
        Referenced {
            path,
            role,
            loaded: None,
        }
    }
}

impl From<(PathBuf, Role, ContentHash)> for Referenced {
    fn from((path, role, hash): (PathBuf, Role, ContentHash)) -> Self {
        Referenced {
            path,
            role,
            loaded: Some(hash),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dir: bool,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(m: &std::fs::Metadata) -> Stamp {
        Stamp {
            dir: m.is_dir(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }

    fn same(&self, other: &Stamp) -> bool {
        if self.dir && other.dir {
            // A directory's times change whenever an entry does; only its
            // identity matters here.
            self.ino == other.ino
        } else {
            self == other
        }
    }
}

type Snapshot = BTreeMap<OsString, Stamp>;

fn snapshot(dir: &Path) -> Option<Snapshot> {
    let mut snap = Snapshot::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(m) = entry.metadata() else { continue };
        snap.insert(entry.file_name(), Stamp::of(&m));
    }
    Some(snap)
}

#[derive(Debug, Default)]
struct Pending {
    files: BTreeSet<PathBuf>,
    cache: BTreeMap<PathBuf, CacheKind>,
    /// Paths whose latest event was a removal.
    gone: HashSet<PathBuf>,
    config: bool,
    trees: bool,
    /// A watched directory went or was replaced, or a missing one on the
    /// way appeared: the watches must be brought up to date. A batch of
    /// content edits alone leaves them as they are.
    structure: bool,
    full: Option<RescanReason>,
    notices: Vec<Notice>,
    /// When the batch was opened and when its quiet period last started
    /// over: they decide when it is cut.
    first: Option<Instant>,
    last: Option<Instant>,
    /// The earliest and latest event behind the batch's changes, reported
    /// as `first_event` and `last_event`. Unlike `first` and `last`, a
    /// file put back for a later batch (a read that may be torn) brings
    /// the times of the events that made it due, not the flush's.
    events: Option<(Instant, Instant)>,
}

/// A watched file whose read was put off (it may have been torn, or its
/// writer was at it): the open batch is cut for it `max_delay` after the
/// event that first made it due, however often it is written, and from
/// then on a stable read of it is taken even if its modification time is
/// recent.
#[derive(Debug, Clone, Copy)]
struct Deferred {
    /// The first event behind it.
    since: Instant,
    /// Not before this: one quiet period after the flush that put it off,
    /// so a file written without pause is not read in a busy loop.
    retry: Instant,
}

/// A watched file left unread because a write to it is in progress.
#[derive(Debug, Clone, Copy)]
struct Held {
    /// When that was decided.
    at: Instant,
    /// Its modification time then. A writer is stalled only if no event
    /// came for `stalled_write` *and* the file did not change: in a
    /// directory watched without `MODIFY` (a download into `~/Pictures`)
    /// the writes themselves make no event.
    mtime: Option<std::time::SystemTime>,
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Written,
    Gone,
    Dir,
}

pub(crate) struct Core<B> {
    backend: B,
    opts: Options,
    config: Option<ConfigWatch>,
    config_root: Option<Resolved>,
    files: BTreeMap<PathBuf, Entry>,
    trees: Vec<Tree>,
    own: SharedOwnWrites,
    watched: BTreeMap<PathBuf, Mode>,
    /// What each watched directory is watched for.
    kinds: HashMap<PathBuf, WatchKind>,
    /// Inode of each watched directory when its watch was added.
    watched_ino: HashMap<PathBuf, u64>,
    snaps: HashMap<PathBuf, Snapshot>,
    next_sweep: Instant,
    // Index, rebuilt by `sync_watches`.
    by_path: HashMap<PathBuf, Vec<PathBuf>>,
    rescan_paths: HashSet<PathBuf>,
    config_dirs: HashSet<PathBuf>,
    tree_dirs: HashMap<PathBuf, usize>,
    tree_hops: HashMap<PathBuf, usize>,
    /// For each wanted directory that does not exist, the first missing
    /// path below its nearest existing ancestor (which is watched): when it
    /// appears, the watches are brought up to date.
    waiting: HashSet<PathBuf>,
    /// Paths (as events name them) with a write in progress: `MODIFY` or
    /// an empty file created, and no `CLOSE_WRITE`, `MOVED_TO` or removal
    /// since. The time is that of the latest such event.
    writing: HashMap<PathBuf, Instant>,
    /// Watched files left unread at a flush because they were being
    /// written. Re-checked at every flush.
    held: BTreeMap<PathBuf, Held>,
    /// Watched files whose read was put off, until one is taken.
    deferred: HashMap<PathBuf, Deferred>,
    pending: Pending,
    /// How many times the watches were brought up to date.
    #[cfg(test)]
    syncs: usize,
}

impl<B> std::fmt::Debug for Core<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("files", &self.files.len())
            .field("watched", &self.watched.len())
            .finish_non_exhaustive()
    }
}

fn is_missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn open_nonblocking(path: &Path) -> io::Result<std::fs::File> {
    // O_NONBLOCK: a FIFO swapped in after the `stat` still opens at once
    // (and is then refused by `fstat`) instead of blocking this thread.
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
}

/// A regular file's hash and the stamp it had when opened.
#[derive(Debug, Clone, Copy)]
struct Hashed {
    hash: ContentHash,
    stamp: Stamp,
    /// False when the file changed while it was read (its stamp, taken
    /// again from the same descriptor after hashing, differs): the bytes
    /// may be torn.
    stable: bool,
    /// The wall clock right after the read, which the stamp's
    /// modification time is compared with.
    read_at: std::time::SystemTime,
}

/// Hash a regular file, streaming. Anything else (a FIFO, a device, a
/// directory) is `InvalidInput` and never read: a FIFO would block the
/// watcher thread and `/dev/zero` never ends.
fn read_hash(path: &Path) -> io::Result<Hashed> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    if !std::fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    let file = open_nonblocking(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&file)?;
    // A barrier: on ext4, btrfs and tmpfs `SEEK_DATA` takes the inode's
    // lock, which a truncation (`O_TRUNC`) holds until its `MODIFY` is
    // queued. A truncation under way during the read has queued its
    // event once this returns, for the flush's drain to find. Its result
    // (ENXIO for an empty file) does not matter.
    let _ = rustix::fs::seek(&file, rustix::fs::SeekFrom::Data(0));
    let stamp = Stamp::of(&meta);
    let stable = file.metadata().is_ok_and(|m| Stamp::of(&m) == stamp);
    Ok(Hashed {
        hash: hasher.finalize(),
        stamp,
        stable,
        read_at: std::time::SystemTime::now(),
    })
}

/// `now` projected onto the wall clock.
fn wall_clock(now: Instant) -> std::time::SystemTime {
    let (real, wall) = (Instant::now(), std::time::SystemTime::now());
    let projected = if now >= real {
        wall.checked_add(now - real)
    } else {
        wall.checked_sub(real - now)
    };
    projected.unwrap_or(wall)
}

/// How far in the future a local file's modification time may be and
/// still count as recent: the clock was stepped back a little (NTP)
/// between the write and the read. Beyond it (or on a polled network
/// filesystem, whose server clock may run ahead for good) a time in the
/// future is not recent, or the file would never be read.
const MTIME_SKEW: Duration = Duration::from_secs(1);

/// Whether `stamp`'s modification time is less than `window` before
/// `at` (the wall clock right after the read), or at most `skew` after
/// it.
fn written_within(
    stamp: &Stamp,
    at: std::time::SystemTime,
    window: Duration,
    skew: Duration,
) -> bool {
    let (secs, nanos) = stamp.mtime;
    let (Ok(secs), Ok(nanos)) = (u64::try_from(secs), u32::try_from(nanos)) else {
        return false;
    };
    let Some(mtime) =
        std::time::UNIX_EPOCH.checked_add(Duration::new(secs, nanos.min(999_999_999)))
    else {
        return false;
    };
    match at.duration_since(mtime) {
        Ok(age) => age < window,
        Err(ahead) => ahead.duration() <= skew,
    }
}

/// The stamp of what `path` resolves to, without reading it.
fn current_stamp(path: &Path) -> Option<Stamp> {
    std::fs::metadata(path).ok().map(|m| Stamp::of(&m))
}

/// Like [`current_stamp`], but a regular file is opened first: on NFS an
/// `open` revalidates the attribute cache (close-to-open consistency),
/// where a `stat` may serve stale attributes.
fn fresh_stamp(path: &Path) -> Option<Stamp> {
    polled_stamp(path, open_nonblocking)
}

/// [`fresh_stamp`] with the `open` passed in. A file that exists but
/// cannot be opened (EACCES) gets its `stat` stamp, which is what
/// [`Core::check`] stored for it: comparing anything else would re-read
/// it on every poll.
fn polled_stamp(
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<std::fs::File>,
) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return Some(Stamp::of(&m));
    }
    match open(path).and_then(|f| f.metadata()) {
        Ok(m) => Some(Stamp::of(&m)),
        Err(_) => Some(Stamp::of(&m)),
    }
}

fn canonical_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| paths::absolute(d)))
        .collect()
}

/// Breadth-first walk of a cache tree: directories up to `depth` below
/// `root`, symlinks followed, hidden names skipped, canonical dedup.
fn walk(root: &Path, depth: usize) -> Vec<PathBuf> {
    let Ok(root) = std::fs::canonicalize(root) else {
        return Vec::new();
    };
    if !root.is_dir() {
        return Vec::new();
    }
    let mut seen = HashSet::from([root.clone()]);
    let mut out = vec![root.clone()];
    let mut queue = std::collections::VecDeque::from([(root, 0usize)]);
    while let Some((dir, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            if paths::is_hidden(&name) || paths::is_scratch(&name) {
                continue;
            }
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let Ok(c) = std::fs::canonicalize(&p) else {
                continue;
            };
            if seen.insert(c.clone()) {
                out.push(c.clone());
                queue.push_back((c, d + 1));
            }
        }
    }
    out
}

impl<B: Backend> Core<B> {
    /// Set up watches, then record a baseline hash of every module file.
    /// The module set was listed before the watches existed, so it is
    /// listed again at the first quiet period (watch, then list): a module
    /// created in between is reported `Created`; an unchanged set sends
    /// nothing.
    pub(crate) fn new(backend: B, opts: Options, config: Option<ConfigWatch>) -> Self {
        let mut core = Core {
            backend,
            next_sweep: Instant::now() + opts.content_sweep,
            opts,
            config: None,
            config_root: None,
            files: BTreeMap::new(),
            trees: Vec::new(),
            own: SharedOwnWrites::default(),
            watched: BTreeMap::new(),
            kinds: HashMap::new(),
            watched_ino: HashMap::new(),
            snaps: HashMap::new(),
            by_path: HashMap::new(),
            rescan_paths: HashSet::new(),
            config_dirs: HashSet::new(),
            tree_dirs: HashMap::new(),
            tree_hops: HashMap::new(),
            waiting: HashSet::new(),
            writing: HashMap::new(),
            held: BTreeMap::new(),
            deferred: HashMap::new(),
            pending: Pending::default(),
            #[cfg(test)]
            syncs: 0,
        };
        if let Some(mut cfg) = config {
            cfg.root = paths::absolute(&cfg.root);
            cfg.modules.dirs = canonical_dirs(&cfg.modules.dirs);
            core.config_root = Some(paths::resolve(&cfg.root));
            for f in &cfg.modules.files {
                let path = paths::absolute(f);
                core.files
                    .entry(path.clone())
                    .or_insert_with(|| Entry::unread(&path))
                    .module = true;
            }
            core.config = Some(cfg);
        }
        let added = core.sync_watches(true);
        for (path, e) in core.files.iter_mut() {
            e.take_baseline(path);
        }
        core.relist(&added, Instant::now());
        core.mark_if_notices();
        core
    }

    /// The own-write registrations, for [`crate::Watcher`] to add to from
    /// the caller's thread.
    pub(crate) fn own_writes(&self) -> SharedOwnWrites {
        self.own.clone()
    }

    /// Watch, then list: a config or cache-tree directory just watched was
    /// listed before its watch existed, and a file created in between made
    /// no event. List it again at the next quiet period (that listing adds
    /// no new watch, so it ends; an unchanged set is no batch).
    fn relist(&mut self, added: &[PathBuf], now: Instant) {
        if added.iter().any(|d| self.config_dirs.contains(d)) {
            self.pending.config = true;
            self.mark(now);
        }
        if added.iter().any(|d| self.tree_dirs.contains_key(d)) {
            self.pending.trees = true;
            self.mark(now);
        }
    }

    /// Settle entries a registration touched (their directories are
    /// watched by now). A new entry the caller read itself (`loaded`)
    /// takes that hash as its baseline without being read here: it is
    /// compared with the file at the next quiet period, on this thread,
    /// so the caller never waits for a hash. A new entry with no `loaded`
    /// is read now (watch, then read: the caller reads after this). An
    /// entry already watched whose baseline differs from `loaded` (a save
    /// between the caller's read and the registration) is compared again
    /// at the next quiet period.
    fn settle_new(&mut self, fresh: &[PathBuf], loaded: &[(PathBuf, ContentHash)]) {
        let given: HashMap<&PathBuf, ContentHash> = loaded.iter().map(|(p, h)| (p, *h)).collect();
        let mut recheck = false;
        for path in fresh {
            let Some(e) = self.files.get_mut(path) else {
                continue;
            };
            match given.get(path) {
                Some(h) => {
                    e.hash = Some(*h);
                    e.exists = true;
                    e.error = None;
                    self.pending.files.insert(path.clone());
                    recheck = true;
                }
                None => e.take_baseline(path),
            }
        }
        for (path, hash) in loaded {
            if fresh.contains(path) {
                continue;
            }
            let Some(e) = self.files.get_mut(path) else {
                continue;
            };
            if e.hash != Some(*hash) {
                // Compare the file with what the loader holds, not with
                // what the watcher read.
                e.hash = Some(*hash);
                e.exists = true;
                e.error = None;
                self.pending.files.insert(path.clone());
                recheck = true;
            }
        }
        if recheck {
            self.mark(Instant::now());
        }
    }

    /// Watch one more file (settings TOML, wallpaper, shader, …) for
    /// `role`: an ad-hoc registration, counted per (path, role), that
    /// [`Core::set_referenced`] never replaces. The file need not exist
    /// yet, nor its directory: creation is reported. `loaded` is the hash
    /// of the bytes the caller already read, if it did.
    pub(crate) fn add_file(&mut self, path: &Path, role: Role, loaded: Option<ContentHash>) {
        let path = paths::absolute(path);
        let fresh = !self.files.contains_key(&path);
        let e = self
            .files
            .entry(path.clone())
            .or_insert_with(|| Entry::unread(&path));
        *e.explicit.entry(role).or_default() += 1;
        if fresh {
            self.sync_watches(false);
        }
        let fresh: Vec<PathBuf> = fresh.then(|| path.clone()).into_iter().collect();
        let loaded: Vec<_> = loaded.map(|h| (path, h)).into_iter().collect();
        self.settle_new(&fresh, &loaded);
        self.mark_if_notices();
    }

    /// Drop one registration made with [`Core::add_file`]. Module files
    /// and the loader's registrations stay.
    pub(crate) fn remove_file(&mut self, path: &Path, role: Role) {
        let path = paths::absolute(path);
        let Some(e) = self.files.get_mut(&path) else {
            return;
        };
        match e.explicit.get_mut(&role) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                e.explicit.remove(&role);
            }
            None => return,
        }
        if e.unused() {
            self.files.remove(&path);
            self.pending.files.remove(&path);
            self.held.remove(&path);
            self.sync_watches(false);
        }
    }

    /// Replace the loader's registrations by this set (what the compiler
    /// collected from the whole program). A pair listed twice counts
    /// twice. Registrations made with [`Core::add_file`] are kept.
    pub(crate) fn set_referenced(&mut self, refs: Vec<Referenced>) {
        let mut wanted: BTreeMap<PathBuf, BTreeMap<Role, usize>> = BTreeMap::new();
        let mut loaded = Vec::new();
        for r in refs {
            let path = paths::absolute(&r.path);
            if let Some(h) = r.loaded {
                loaded.push((path.clone(), h));
            }
            *wanted.entry(path).or_default().entry(r.role).or_default() += 1;
        }
        for (p, e) in self.files.iter_mut() {
            e.loader = wanted.remove(p).unwrap_or_default();
        }
        self.files.retain(|_, e| !e.unused());
        let mut fresh = Vec::new();
        for (p, refs) in wanted {
            let mut e = Entry::unread(&p);
            e.loader = refs;
            self.files.insert(p.clone(), e);
            fresh.push(p);
        }
        let files = &self.files;
        self.pending.files.retain(|p| files.contains_key(p));
        self.held.retain(|p, _| files.contains_key(p));
        self.sync_watches(false);
        self.settle_new(&fresh, &loaded);
        self.mark_if_notices();
    }

    /// Watch a cache-invalidation tree (apps, icons, fonts).
    pub(crate) fn add_tree(&mut self, root: &Path, depth: usize, kind: CacheKind) {
        let root = paths::absolute(root);
        if self.trees.iter().any(|t| t.root == root) {
            return;
        }
        self.trees.push(Tree {
            dirs: walk(&root, depth),
            resolved: paths::resolve(&root),
            root,
            depth,
            kind,
        });
        let added = self.sync_watches(false);
        self.relist(&added, Instant::now());
        self.mark_if_notices();
    }

    /// The backend was replaced (inotify failed): every watch it held is
    /// gone. Add them all again and rescan everything, since events were
    /// lost.
    pub(crate) fn rewatch_all(&mut self, now: Instant) {
        self.watched.clear();
        self.kinds.clear();
        self.watched_ino.clear();
        self.snaps.clear();
        self.sync_watches(true);
        self.request_rescan(RescanReason::Overflow, now);
    }

    /// Rescan everything at the next flush.
    pub(crate) fn request_rescan(&mut self, reason: RescanReason, now: Instant) {
        if self.pending.full != Some(RescanReason::Overflow) {
            self.pending.full = Some(reason);
        }
        self.mark(now);
    }

    fn mark(&mut self, now: Instant) {
        self.schedule(now);
        self.add_events(now, now);
    }

    /// Open the batch if none is, and start its quiet period over.
    fn schedule(&mut self, now: Instant) {
        self.pending.first.get_or_insert(now);
        self.pending.last = Some(self.pending.last.map_or(now, |l| l.max(now)));
    }

    fn add_events(&mut self, first: Instant, last: Instant) {
        let e = self.pending.events.get_or_insert((first, last));
        *e = (e.0.min(first), e.1.max(last));
    }

    /// Put the read of `path` off to a later batch: it is due again one
    /// quiet period from `now`, and (see [`Deferred`]) at the latest
    /// `max_delay` after `events.0`, the first event behind it. The batch
    /// reports those events' times.
    fn defer(&mut self, path: PathBuf, events: (Instant, Instant), now: Instant) {
        self.note_deferred(&path, events.0, now);
        self.pending.files.insert(path);
        self.schedule(now);
        self.add_events(events.0, events.1);
    }

    fn note_deferred(&mut self, path: &Path, since: Instant, now: Instant) {
        let retry = now + self.opts.coalesce;
        self.deferred
            .entry(path.to_path_buf())
            .and_modify(|d| d.retry = retry)
            .or_insert(Deferred { since, retry });
    }

    /// When the open batch should be cut, if one is open, or when a file
    /// held back because it was being written is due to be read anyway.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let batch = match (self.pending.first, self.pending.last) {
            (Some(first), Some(last)) => {
                let quiet = if self.pending.gone.is_empty() {
                    self.opts.coalesce
                } else {
                    self.opts.removal_grace
                };
                let cut = (last + quiet).min(first + self.opts.max_delay);
                // A file put off at an earlier flush keeps the bound of
                // the batch it was first due in.
                let deferred = self
                    .deferred
                    .iter()
                    .filter(|(f, _)| self.pending.files.contains(*f))
                    .map(|(_, d)| (d.since + self.opts.max_delay).max(d.retry))
                    .min();
                Some(deferred.map_or(cut, |d| cut.min(d)))
            }
            _ => None,
        };
        // A held file still being written is due when its writer counts
        // as stalled. One whose write ended (`CLOSE_WRITE`, `MOVED_TO`,
        // removal) is back in the open batch and waits for its quiet
        // period like any other: the coalesce, or the removal grace when
        // it was deleted. Only one that is neither is due at once.
        let held = self
            .held
            .iter()
            .filter_map(|(f, h)| match self.busy_since(f) {
                Some(t) => Some(t + self.opts.stalled_write),
                None if self.pending.files.contains(f) => None,
                None => Some(h.at),
            })
            .min();
        match (batch, held) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// When the latest write to the file `f` resolves to was seen, if a
    /// write to it is in progress.
    fn busy_since(&self, f: &Path) -> Option<Instant> {
        let e = self.files.get(f)?;
        self.writing.get(&e.resolved.path).copied()
    }

    pub(crate) fn has_polled_dirs(&self) -> bool {
        self.watched.values().any(|m| *m == Mode::Poll)
    }

    pub(crate) fn notice(&mut self, notice: Notice, now: Instant) {
        self.pending.notices.push(notice);
        self.mark(now);
    }

    pub(crate) fn on_raw(&mut self, raw: Raw, now: Instant) {
        match raw {
            Raw::Overflow => self.request_rescan(RescanReason::Overflow, now),
            Raw::Written(p) => {
                if let Some(ino) = tmpfile_ino(&p) {
                    self.closed_tmpfile(&p, ino, now);
                    return;
                }
                self.writing.remove(&p);
                self.on_path(p, What::Written, now);
            }
            // Not the end of a write: a writer that created the file and
            // wrote before the creation was read keeps its write in
            // progress through the `MODIFY` right behind it.
            Raw::Linked(p) => self.on_path(p, What::Written, now),
            Raw::Gone(p) => {
                self.writing.remove(&p);
                self.on_path(p, What::Gone, now);
            }
            Raw::Dir(p) => self.on_path(p, What::Dir, now),
            // Bookkeeping only: the batch's quiet period runs from the last
            // *completed* write, so a writer that keeps its file open (a
            // status file fed by a script, a font being copied) does not
            // stretch every batch to `max_delay`. The file itself is held
            // at the flush while its write is in progress, and held from
            // now on: if its writer stalls (no further write, no close),
            // it is due `stalled_write` after the last one even when no
            // completed write ever opens a batch for it.
            Raw::Busy(p) => {
                if self.hashed(&p) {
                    self.hold_written(&p, now);
                    self.writing.insert(p, now);
                }
            }
            Raw::Created(p) => {
                if self.hashed(&p) {
                    self.writing.insert(p.clone(), now);
                }
                self.on_path(p, What::Written, now);
            }
        }
    }

    /// Hold every watched file that resolves to `p` (being written) and is
    /// not held yet. In a config directory each write makes an event, so
    /// its modification time is not needed (and not read per write).
    fn hold_written(&mut self, p: &Path, now: Instant) {
        let Some(files) = self.by_path.get(p) else {
            return;
        };
        for f in files {
            if self.held.contains_key(f) || !self.files.get(f).is_some_and(|e| e.resolved.path == p)
            {
                continue;
            }
            let mtime = if self.tracked(f) { None } else { mtime(f) };
            self.held.insert(f.clone(), Held { at: now, mtime });
        }
    }

    /// Whether every write to the file `f` resolves to makes an event
    /// (`MODIFY`: its directory is a config directory).
    fn tracked(&self, f: &Path) -> bool {
        self.files
            .get(f)
            .and_then(|e| e.resolved.path.parent())
            .is_some_and(|d| self.kinds.get(d) == Some(&WatchKind::Full))
    }

    /// A `CLOSE_WRITE` under `#<ino>`: a file made with `O_TMPFILE` was
    /// closed, after `linkat` already gave it a name (its creation came
    /// under that name, with bytes, and it is held as being written). End
    /// the write of each name in that directory that is this inode.
    fn closed_tmpfile(&mut self, p: &Path, ino: u64, now: Instant) {
        let dir = p.parent();
        let names: Vec<PathBuf> = self
            .writing
            .keys()
            .filter(|w| w.parent() == dir)
            .filter(|w| std::fs::metadata(w).is_ok_and(|m| m.ino() == ino))
            .cloned()
            .collect();
        for w in names {
            self.writing.remove(&w);
            self.on_path(w, What::Written, now);
        }
    }

    /// Whether `p` (as an event names it) is a file whose content is
    /// hashed: a watched file or a module name in a config directory.
    fn hashed(&self, p: &Path) -> bool {
        self.by_path.contains_key(p)
            || (p.parent().is_some_and(|d| self.config_dirs.contains(d)) && is_module_name(p))
    }

    fn on_path(&mut self, p: PathBuf, what: What, now: Instant) {
        let Some(name) = p.file_name() else {
            return;
        };
        if paths::is_scratch(name) {
            return;
        }
        let mut hit = false;
        if let Some(files) = self.by_path.get(&p) {
            for f in files {
                self.pending.files.insert(f.clone());
                if what != What::Written && self.files.get(f).is_some_and(|e| e.module) {
                    self.pending.config = true;
                }
            }
            hit = true;
        }
        if what == What::Gone {
            self.pending.gone.insert(p.clone());
        } else {
            self.pending.gone.remove(&p);
        }
        if self.rescan_paths.contains(&p) {
            self.pending.config = true;
            hit = true;
        }
        if what != What::Gone && self.waiting.contains(&p) {
            self.wake_under(&p);
            hit = true;
        }
        if let Some(&t) = self.tree_hops.get(&p) {
            self.pending.trees = true;
            let tree = &self.trees[t];
            self.pending.cache.insert(tree.root.clone(), tree.kind);
            hit = true;
        }
        // A watched directory deleted or moved away, or another directory
        // now at its path (a `Dir` for the one already watched, e.g. its
        // own creation event arriving late, changes nothing).
        let replaced = match what {
            What::Gone => true,
            What::Dir => {
                std::fs::metadata(&p).map(|m| m.ino()).ok() != self.watched_ino.get(&p).copied()
            }
            What::Written => false,
        };
        if replaced && self.forget_tree(&p) {
            // Everything below it is re-resolved, re-watched and re-hashed
            // at the flush: a tree moved away and back, or swapped for a
            // copy (`mv cfg cfg.bak; mv cfg.new cfg`), may hold edits made
            // while it was not watched.
            self.wake_under(&p);
            hit = true;
        }
        if let Some(parent) = p.parent()
            && !paths::is_hidden(name)
        {
            // A `.strand` name or a directory (a directory link included:
            // `ln -s` and a renamed-in link arrive as `Written`) can change
            // the module set.
            let set_may_change = || match what {
                What::Dir => true,
                What::Written => {
                    is_module_name(&p) || std::fs::metadata(&p).is_ok_and(|m| m.is_dir())
                }
                What::Gone => false,
            };
            if self.config_dirs.contains(parent)
                && !self.by_path.contains_key(&p)
                && set_may_change()
            {
                self.pending.config = true;
                hit = true;
            }
            if let Some(&t) = self.tree_dirs.get(parent) {
                self.pending.cache.insert(p.clone(), self.trees[t].kind);
                self.pending.trees |= what == What::Dir;
                hit = true;
            }
        }
        if hit {
            self.mark(now);
        } else {
            self.pending.gone.remove(&p);
        }
    }

    /// A missing path on the way to watched things appeared: re-check
    /// everything below it at the next flush (which re-resolves and
    /// re-watches).
    fn wake_under(&mut self, p: &Path) {
        self.pending.structure = true;
        for (f, e) in &self.files {
            if e.resolved.path.starts_with(p) || e.resolved.hops.iter().any(|h| h.starts_with(p)) {
                self.pending.files.insert(f.clone());
            }
        }
        let config_below = self
            .config_root
            .as_ref()
            .is_some_and(|r| r.path.starts_with(p))
            || self.config_dirs.iter().any(|d| d.starts_with(p));
        self.pending.config |= config_below;
        self.pending.trees |= self.trees.iter().any(|t| t.resolved.path.starts_with(p));
    }

    /// Forget the watch on `dir` and on every watched directory below it.
    /// A moved directory's inotify descriptors follow the inodes, so the
    /// paths they were added under are stale for the whole subtree; a
    /// deleted one's are gone. Returns whether anything was watched.
    fn forget_tree(&mut self, dir: &Path) -> bool {
        let below: Vec<PathBuf> = self
            .watched
            .range(dir.to_path_buf()..)
            .map(|(d, _)| d)
            .take_while(|d| d.starts_with(dir))
            .cloned()
            .collect();
        for d in &below {
            self.forget_watch(d);
        }
        !below.is_empty()
    }

    fn forget_watch(&mut self, dir: &Path) {
        if self.watched.remove(dir) == Some(Mode::Inotify) {
            self.backend.unwatch(dir);
        }
        self.kinds.remove(dir);
        self.watched_ino.remove(dir);
        self.snaps.remove(dir);
    }

    /// Compare every polled directory with its last listing, and mark the
    /// watched files in it whose stamp changed (or, every
    /// `content_sweep`, every small one) for a content comparison.
    pub(crate) fn poll(&mut self, now: Instant) {
        let sweep = now >= self.next_sweep;
        if sweep {
            self.next_sweep = now + self.opts.content_sweep;
        }
        let dirs: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(_, m)| **m == Mode::Poll)
            .map(|(d, _)| d.clone())
            .collect();
        for dir in dirs {
            match snapshot(&dir) {
                Some(new) => self.compare_listing(&dir, new, now),
                // Exists but cannot be listed (EACCES): keep the last
                // listing and keep polling, quietly.
                None if dir.is_dir() => {}
                None => {
                    self.on_path(dir, What::Gone, now);
                    continue;
                }
            }
            let mut any = false;
            for (f, e) in &self.files {
                if !e.exists || !e.resolved.watch_dirs().any(|d| d == dir) {
                    continue;
                }
                let stamp = fresh_stamp(f);
                let swept = sweep && stamp.is_some_and(|s| s.len <= self.opts.sweep_max_bytes);
                if stamp != e.stamp || swept {
                    self.pending.files.insert(f.clone());
                    any = true;
                }
            }
            if any {
                self.mark(now);
            }
        }
    }

    fn compare_listing(&mut self, dir: &Path, new: Snapshot, now: Instant) {
        let old = self
            .snaps
            .insert(dir.to_path_buf(), new.clone())
            .unwrap_or_default();
        for (name, st) in &new {
            match old.get(name) {
                None => {
                    let what = if st.dir { What::Dir } else { What::Written };
                    self.on_path(dir.join(name), what, now);
                }
                Some(o) if !o.same(st) => {
                    let what = if st.dir || o.dir {
                        What::Dir
                    } else {
                        What::Written
                    };
                    self.on_path(dir.join(name), what, now);
                }
                Some(_) => {}
            }
        }
        for name in old.keys() {
            if !new.contains_key(name) {
                self.on_path(dir.join(name), What::Gone, now);
            }
        }
    }

    /// Handle every event queued in the backend now. Returns the watched
    /// files a drained event shows a write in progress on, or a removal
    /// of: a `MODIFY`, a creation, a deletion.
    fn drain(&mut self, now: Instant) -> HashSet<PathBuf> {
        let mut raws = Vec::new();
        // A failed read fails again at the watcher thread's next read,
        // which handles it (with what it lost); the events read before
        // the failure are handled here.
        let _ = self.backend.drain(&mut raws);
        let mut unsettled = HashSet::new();
        for raw in raws {
            if let Raw::Busy(p) | Raw::Created(p) | Raw::Gone(p) = &raw
                && let Some(files) = self.by_path.get(p)
            {
                unsettled.extend(files.iter().cloned());
            }
            self.on_raw(raw, now);
        }
        unsettled
    }

    /// Whether the directory the file `f` resolves to is polled (a
    /// network filesystem: its modification times are the server's).
    fn polled(&self, f: &Path) -> bool {
        self.files
            .get(f)
            .and_then(|e| e.resolved.path.parent())
            .is_some_and(|d| self.watched.get(d) == Some(&Mode::Poll))
    }

    /// Cut the open batch: rescan if asked, re-resolve symlinks, re-watch
    /// when the structure may have changed, hash what was touched and is
    /// not being written, and drop what did not change.
    pub(crate) fn flush(&mut self, now: Instant) -> Option<FileBatch> {
        let p = std::mem::take(&mut self.pending);
        let mut changes = Vec::new();
        let mut notices = p.notices;
        let mut touched = p.files;
        // Files held back at an earlier flush are looked at again.
        let was_held = std::mem::take(&mut self.held);
        touched.extend(was_held.keys().cloned());
        if p.full.is_some() || p.config {
            self.rescan_config(&mut touched, &mut changes, &mut notices);
        }
        if p.full.is_some() {
            touched.extend(self.files.keys().cloned());
        }
        let mut cache = p.cache;
        if p.trees || p.full.is_some() {
            for t in &mut self.trees {
                t.resolved = paths::resolve(&t.root);
                t.dirs = walk(&t.root, t.depth);
                if p.full.is_some() {
                    cache.insert(t.root.clone(), t.kind);
                }
            }
        }
        // Resolve first, watch the new link targets, then read: a write
        // that lands after the read is seen by the new watch. A batch of
        // content edits (nothing resolves elsewhere, no directory came or
        // went) leaves the watches alone: its cost does not grow with the
        // number of watched directories.
        let mut resync = p.full.is_some() || p.config || p.trees || p.structure;
        for path in &touched {
            if let Some(e) = self.files.get_mut(path) {
                let resolved = paths::resolve(path);
                if resolved != e.resolved {
                    e.resolved = resolved;
                    resync = true;
                }
            }
        }
        let added = if resync {
            self.sync_watches(p.full.is_some())
        } else {
            Vec::new()
        };
        notices.append(&mut self.pending.notices);
        // An own write matched for one logical path is own for every path
        // that reaches the same file (a symlink alias, a `..` spelling).
        let mut own_seen = HashSet::new();
        let mut reads = Vec::new();
        for path in &touched {
            let mut stalled = false;
            if let Some(since) = self.busy_since(path) {
                let quiet = now.saturating_duration_since(since) >= self.opts.stalled_write;
                // Changed since it was held: its writer is still at it,
                // even though no event said so (no `MODIFY` there; in a
                // config directory every write is an event).
                let tracked = self.tracked(path);
                let m = if tracked { None } else { mtime(path) };
                let changed = !tracked && was_held.get(path).is_some_and(|h| h.mtime != m);
                if !quiet || changed {
                    if quiet && let Some(e) = self.files.get(path) {
                        self.writing.insert(e.resolved.path.clone(), now);
                    }
                    if self.files.contains_key(path) {
                        let since = p.events.map_or(now, |e| e.0);
                        self.note_deferred(path, since, now);
                    }
                    // Never read a file mid-write: wait for its
                    // `CLOSE_WRITE`.
                    self.held.insert(path.clone(), Held { at: now, mtime: m });
                    continue;
                }
                if let Some(e) = self.files.get(path) {
                    self.writing.remove(&e.resolved.path);
                }
                stalled = true;
            }
            if self.files.contains_key(path) {
                reads.push((path.clone(), read_hash(path), stalled));
            }
        }
        // A write that began after the last drain (an in-place save's
        // `O_TRUNC`, its first `write`) may have been read half done. Its
        // `MODIFY` (or `CLOSE_WRITE`, creation, removal) is queued by now:
        // take the queue, and keep a file out of this batch when there is
        // evidence its read may be torn. Its baseline stays as it was; it
        // is held while its write is in progress and read again in a
        // later batch.
        let unsettled = if reads.is_empty() {
            HashSet::new()
        } else {
            self.drain(now)
        };
        let lost = self.pending.full.is_some();
        for (path, read, stalled) in reads {
            let busy = self.busy_since(&path).is_some();
            let torn = match &read {
                Ok(r) => self.maybe_torn(&path, r, &unsettled, p.first, now),
                Err(_) => false,
            };
            if lost || torn || busy {
                let events = p.events.unwrap_or((now, now));
                if busy {
                    let tracked = self.tracked(&path);
                    let m = if tracked { None } else { mtime(&path) };
                    self.note_deferred(&path, events.0, now);
                    self.held.insert(path, Held { at: now, mtime: m });
                } else {
                    self.defer(path, events, now);
                }
                continue;
            }
            // A drained `CLOSE_WRITE` or `MOVED_TO` with no sign of a write
            // under the read: what was read is complete, and the newer
            // version it announces is read in the next batch (the path is
            // pending again).
            self.deferred.remove(&path);
            if stalled {
                notices.push(Notice::StalledWrite(path.clone()));
            }
            self.check(&path, read, &mut changes, &mut own_seen);
        }
        let files = &self.files;
        self.deferred.retain(|f, _| files.contains_key(f));
        // Forget writes nobody closed (a lost `CLOSE_WRITE`) once stale.
        let stalled = self.opts.stalled_write;
        self.writing
            .retain(|_, t| now.saturating_duration_since(*t) < stalled);
        // The rescan above listed directories before their new watches
        // existed.
        self.relist(&added, now);
        for (path, kind) in cache {
            let exists = std::fs::symlink_metadata(&path).is_ok();
            changes.push(FileChange {
                canonical: exists.then(|| paths::resolve(&path).path),
                path,
                kind: if exists {
                    ChangeKind::Modified
                } else {
                    ChangeKind::Removed
                },
                hash: None,
                role: Role::Cache(kind),
                error: None,
            });
        }
        lock(&self.own).expire(now);
        // Stable: of two entries for one (path, role) the first pushed (a
        // module-set change from the rescan) is kept.
        changes.sort_by(|a, b| (&a.path, a.role).cmp(&(&b.path, b.role)));
        changes.dedup_by(|a, b| a.path == b.path && a.role == b.role);
        if changes.is_empty() && notices.is_empty() && p.full.is_none() {
            return None;
        }
        Some(FileBatch {
            changes,
            rescan: p.full,
            notices,
            first_event: p.events.map_or(now, |e| e.0),
            last_event: p.events.map_or(now, |e| e.1),
        })
    }

    /// Whether the stable or unstable read `r` of `path`, taken in a
    /// flush at `now` for a batch opened at `opened`, may hold a write
    /// half done:
    ///
    /// - its stamp moved while it was read;
    /// - a drained event shows a write in progress on it (`MODIFY`, a
    ///   creation) or its removal;
    /// - outside the config directories, where writes make no event, a
    ///   drained `CLOSE_WRITE` names it: it may end a write the read
    ///   overlapped;
    /// - it was modified less than a quiet period before the read: a
    ///   truncation sets the size (and the times) before its `MODIFY` is
    ///   queued, and an in-place writer outside the config directories
    ///   makes no event until it closes. That rule is waived once the
    ///   file has been due for `max_delay` (a file rewritten faster than
    ///   the quiet period would otherwise never be read): the other
    ///   checks still apply.
    fn maybe_torn(
        &self,
        path: &Path,
        r: &Hashed,
        unsettled: &HashSet<PathBuf>,
        opened: Option<Instant>,
        now: Instant,
    ) -> bool {
        if !r.stable || unsettled.contains(path) {
            return true;
        }
        if !self.tracked(path) && self.pending.files.contains(path) {
            return true;
        }
        let since = self.deferred.get(path).map(|d| d.since).or(opened);
        let forced = since.is_some_and(|s| now.saturating_duration_since(s) >= self.opts.max_delay);
        let skew = if self.polled(path) {
            Duration::ZERO
        } else {
            MTIME_SKEW
        };
        // The flush's `now` is taken before anything is read (in a test it
        // may be a deadline ahead of the clock): the age counts from
        // whichever is later, it or the end of the read.
        let at = r.read_at.max(wall_clock(now));
        !forced && written_within(&r.stamp, at, self.opts.coalesce, skew)
    }

    fn rescan_config(
        &mut self,
        touched: &mut BTreeSet<PathBuf>,
        changes: &mut Vec<FileChange>,
        notices: &mut Vec<Notice>,
    ) {
        let Some(cfg) = self.config.as_mut() else {
            return;
        };
        self.config_root = Some(paths::resolve(&cfg.root));
        let set = match (cfg.rescan)() {
            Ok(set) => set,
            Err(e) => {
                notices.push(Notice::RescanFailed(e.to_string()));
                return;
            }
        };
        cfg.modules.dirs = canonical_dirs(&set.dirs);
        if set.errors != cfg.modules.errors || set.too_deep != cfg.modules.too_deep {
            // The loader shows these (a dangling `*.strand` link, an
            // unreadable directory) next to the changes they explain.
            notices.push(Notice::ModuleSet {
                errors: set.errors.clone(),
                too_deep: set.too_deep.clone(),
            });
            cfg.modules.errors = set.errors;
            cfg.modules.too_deep = set.too_deep;
        }
        let new: BTreeSet<PathBuf> = set.files.iter().map(|f| paths::absolute(f)).collect();
        for (path, e) in self.files.iter_mut() {
            if !e.module || new.contains(path) {
                continue;
            }
            e.module = false;
            e.module_new = false;
            if e.exists {
                changes.push(FileChange {
                    path: path.clone(),
                    canonical: None,
                    kind: ChangeKind::Removed,
                    hash: None,
                    role: Role::Module,
                    error: None,
                });
            }
        }
        self.files.retain(|path, e| {
            let keep = !e.unused();
            if !keep {
                touched.remove(path);
            }
            keep
        });
        for path in new {
            match self.files.get_mut(&path) {
                Some(e) if e.module => {}
                Some(e) => {
                    e.module = true;
                    e.module_new = true;
                    touched.insert(path);
                }
                None => {
                    let resolved = paths::resolve(&path);
                    self.files.insert(
                        path.clone(),
                        Entry {
                            module: true,
                            module_new: false,
                            loader: BTreeMap::new(),
                            explicit: BTreeMap::new(),
                            reported: resolved.path.clone(),
                            resolved,
                            hash: None,
                            stamp: None,
                            error: None,
                            exists: false,
                        },
                    );
                    touched.insert(path);
                }
            }
        }
        cfg.modules.files = set.files;
    }

    /// Compare `path` with its last known state and push one change per
    /// role it is watched for.
    fn check(
        &mut self,
        path: &Path,
        read: io::Result<Hashed>,
        out: &mut Vec<FileChange>,
        own_seen: &mut HashSet<(PathBuf, ContentHash)>,
    ) {
        let Some(e) = self.files.get(path) else {
            return;
        };
        let was = e.exists;
        let old = e.hash;
        let old_error = e.error;
        let canonical = e.resolved.path.clone();
        let moved = e.reported != canonical;
        let (base, hash, error) = match read {
            Ok(Hashed { hash: h, stamp, .. }) => {
                let key = (canonical.clone(), h);
                let own = own_seen.contains(&key) || lock(&self.own).take(path, &canonical, h);
                if own {
                    own_seen.insert(key);
                }
                let Some(e) = self.files.get_mut(path) else {
                    return;
                };
                e.exists = true;
                e.hash = Some(h);
                e.stamp = Some(stamp);
                e.error = None;
                let base = if own {
                    None
                } else if !was {
                    Some(ChangeKind::Created)
                } else if old != Some(h) || moved {
                    Some(ChangeKind::Modified)
                } else {
                    None
                };
                (base, Some(h), None)
            }
            Err(err) if is_missing(&err) => {
                let Some(e) = self.files.get_mut(path) else {
                    return;
                };
                e.exists = false;
                e.hash = None;
                e.stamp = None;
                e.error = None;
                (was.then_some(ChangeKind::Removed), None, None)
            }
            Err(err) => {
                let kind = err.kind();
                let Some(e) = self.files.get_mut(path) else {
                    return;
                };
                e.exists = true;
                e.hash = None;
                e.stamp = current_stamp(path);
                e.error = Some(kind);
                let base = if !was {
                    Some(ChangeKind::Created)
                } else if old.is_some() || old_error != Some(kind) || moved {
                    Some(ChangeKind::Modified)
                } else {
                    None
                };
                (base, None, Some(kind))
            }
        };
        let Some(e) = self.files.get_mut(path) else {
            return;
        };
        e.reported = canonical.clone();
        let module_new = std::mem::take(&mut e.module_new) && e.exists;
        for role in e.roles() {
            let kind = if role == Role::Module && module_new {
                Some(ChangeKind::Created)
            } else {
                base
            };
            let Some(kind) = kind else { continue };
            out.push(FileChange {
                path: path.to_path_buf(),
                canonical: (kind != ChangeKind::Removed).then(|| canonical.clone()),
                kind,
                hash,
                role,
                error,
            });
        }
    }

    fn reindex(&mut self) {
        self.by_path.clear();
        self.rescan_paths.clear();
        for (logical, e) in &self.files {
            self.by_path
                .entry(e.resolved.path.clone())
                .or_default()
                .push(logical.clone());
            for hop in &e.resolved.hops {
                if hop != &e.resolved.path {
                    self.by_path
                        .entry(hop.clone())
                        .or_default()
                        .push(logical.clone());
                }
                if e.module && hop.is_dir() {
                    self.rescan_paths.insert(hop.clone());
                }
            }
        }
        if let Some(root) = &self.config_root {
            self.rescan_paths.extend(root.hops.iter().cloned());
            // The root itself, seen from its parent: a config directory
            // that is replaced or deleted and recreated.
            self.rescan_paths.insert(root.path.clone());
        }
        if let Some(cfg) = &self.config {
            self.rescan_paths.insert(cfg.root.clone());
        }
        self.config_dirs = self
            .config
            .as_ref()
            .map(|c| c.modules.dirs.iter().cloned().collect())
            .unwrap_or_default();
        self.tree_dirs.clear();
        self.tree_hops.clear();
        for (i, t) in self.trees.iter().enumerate() {
            for d in &t.dirs {
                self.tree_dirs.entry(d.clone()).or_insert(i);
            }
            for h in &t.resolved.hops {
                self.tree_hops.entry(h.clone()).or_insert(i);
            }
        }
    }

    /// Every directory the state needs, with what it is needed for.
    fn desired_dirs(&self) -> BTreeMap<PathBuf, WatchKind> {
        let mut dirs = BTreeMap::new();
        let mut want = |d: &Path, kind: WatchKind| {
            let k = dirs.entry(d.to_path_buf()).or_insert(kind);
            *k = (*k).max(kind);
        };
        if let Some(cfg) = &self.config {
            for d in &cfg.modules.dirs {
                want(d, WatchKind::Full);
            }
        }
        if let Some(root) = &self.config_root {
            // Only names matter there: the root (or a link on its way)
            // being replaced, moved or deleted.
            for d in root
                .hops
                .iter()
                .chain([&root.path])
                .filter_map(|h| h.parent())
            {
                want(d, WatchKind::Parent);
            }
        }
        for e in self.files.values() {
            // Every link's directory for contents too: a save that
            // replaces the link with a plain file (delete and create) is
            // written there. No `MODIFY` outside the config directories:
            // other programs write in `~`, `~/Pictures` or `~/Downloads`.
            for d in e.resolved.watch_dirs() {
                want(d, WatchKind::Completed);
            }
        }
        for t in &self.trees {
            // Cache entries are never read: a completed write is enough.
            want(&t.resolved.path, WatchKind::Completed);
            for d in &t.dirs {
                want(d, WatchKind::Completed);
            }
            for d in t.resolved.hops.iter().filter_map(|h| h.parent()) {
                want(d, WatchKind::Parent);
            }
        }
        dirs
    }

    /// Bring the backend's watches in line with what the state needs. A
    /// wanted directory that does not exist is replaced by its nearest
    /// existing ancestor, watched for names only ([`WatchKind::Parent`]),
    /// and the first missing path below that ancestor is remembered in
    /// `waiting`. Every ancestor of a wanted directory is watched lightly
    /// ([`WatchKind::Ancestor`]), so that one of them being moved or
    /// deleted is seen even though the descriptors below follow the moved
    /// inodes and report nothing. With `verify`, every watched directory
    /// is stat'ed, and one whose inode changed behind our back is watched
    /// again and what lies below it re-checked at the next flush (a full
    /// rescan does that); without it, only directories not yet watched
    /// are looked at. Returns the directories newly watched or polled
    /// (not ancestors).
    fn sync_watches(&mut self, verify: bool) -> Vec<PathBuf> {
        #[cfg(test)]
        {
            self.syncs += 1;
        }
        self.reindex();
        self.waiting.clear();
        let mut desired: BTreeMap<PathBuf, WatchKind> = BTreeMap::new();
        let mut want = |d: PathBuf, kind: WatchKind| {
            let k = desired.entry(d).or_insert(kind);
            *k = (*k).max(kind);
        };
        for (d, kind) in self.desired_dirs() {
            let known = !verify && self.watched.contains_key(&d);
            if known || d.is_dir() {
                want(d, kind);
                continue;
            }
            let mut child = d.as_path();
            let mut ancestor = d.parent();
            while let Some(a) = ancestor {
                if a.is_dir() {
                    break;
                }
                child = a;
                ancestor = a.parent();
            }
            if let Some(a) = ancestor {
                want(a.to_path_buf(), WatchKind::Parent);
                self.waiting.insert(child.to_path_buf());
            }
        }
        let named: Vec<PathBuf> = desired.keys().cloned().collect();
        for d in named {
            for a in d.ancestors().skip(1) {
                desired
                    .entry(a.to_path_buf())
                    .or_insert(WatchKind::Ancestor);
            }
        }
        let stale: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|d| desired.get(*d) != self.kinds.get(*d))
            .cloned()
            .collect();
        for d in stale {
            // Not `forget_watch`: re-adding an inotify watch replaces its
            // mask, and dropping it first would lose events in between.
            self.watched.remove(&d);
            self.kinds.remove(&d);
            self.snaps.remove(&d);
            if !desired.contains_key(&d) {
                self.backend.unwatch(&d);
                self.watched_ino.remove(&d);
            }
        }
        let mut added = Vec::new();
        for (d, kind) in desired {
            if !verify && self.watched.contains_key(&d) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&d) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let known = self.watched_ino.get(&d).copied();
            if known.is_some_and(|i| i != meta.ino()) {
                self.backend.unwatch(&d);
                self.forget_tree(&d);
                self.watched_ino.remove(&d);
                self.wake_under(&d);
                self.mark(Instant::now());
            } else if self.watched.contains_key(&d) {
                continue;
            }
            // Still known: watched until now for another kind, and its
            // inotify watch (if any) is replaced or dropped here.
            let rewatch = self.watched_ino.contains_key(&d);
            if kind == WatchKind::Ancestor {
                // Best effort and silent: an ancestor that cannot be
                // watched (a network or read-only filesystem, the watch
                // limit) only loses the early notice of a move.
                let mode = match paths::fs_kind(&d) {
                    FsKind::Local if !self.opts.force_polling => {
                        match self.backend.watch(&d, WatchKind::Ancestor) {
                            Ok(()) => Mode::Inotify,
                            Err(_) => Mode::Skip,
                        }
                    }
                    _ => Mode::Skip,
                };
                if rewatch && mode != Mode::Inotify {
                    self.backend.unwatch(&d);
                }
                self.kinds.insert(d.clone(), kind);
                self.watched_ino.insert(d.clone(), meta.ino());
                self.watched.insert(d, mode);
                continue;
            }
            let poll = |reason| (Mode::Poll, Some(reason));
            let (mode, reason) = match paths::fs_kind(&d) {
                FsKind::Immutable => (Mode::Skip, None),
                _ if self.opts.force_polling => poll(PollReason::Forced),
                FsKind::NoEvents => poll(PollReason::NoEventsFilesystem),
                FsKind::Local => match self.backend.watch(&d, kind) {
                    Ok(()) => (Mode::Inotify, None),
                    Err(e) => poll(PollReason::WatchFailed(e)),
                },
            };
            if rewatch && mode != Mode::Inotify {
                self.backend.unwatch(&d);
            }
            if mode == Mode::Poll {
                self.snaps
                    .insert(d.clone(), snapshot(&d).unwrap_or_default());
            }
            if let Some(reason) = reason {
                self.pending.notices.push(Notice::Polling {
                    dir: d.clone(),
                    reason,
                });
            }
            self.kinds.insert(d.clone(), kind);
            self.watched_ino.insert(d.clone(), meta.ino());
            self.watched.insert(d.clone(), mode);
            added.push(d);
        }
        added
    }

    /// Notices raised outside a flush (a new watch fell back to polling)
    /// go out in a batch of their own.
    fn mark_if_notices(&mut self) {
        if !self.pending.notices.is_empty() && self.pending.first.is_none() {
            self.mark(Instant::now());
        }
    }

    /// The backend, for the watcher thread to read its events.
    pub(crate) fn backend(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Directories watched in full (not ancestors).
    #[cfg(test)]
    pub(crate) fn watched_dirs(&self) -> Vec<PathBuf> {
        self.watched
            .keys()
            .filter(|d| self.kinds.get(*d) != Some(&WatchKind::Ancestor))
            .cloned()
            .collect()
    }
}

fn is_module_name(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "strand")
}

/// The inode in an `O_TMPFILE`'s name (`#1925585`), which its events
/// carry until (and after) `linkat` names it.
fn tmpfile_ino(p: &Path) -> Option<u64> {
    let digits = p.file_name()?.to_str()?.strip_prefix('#')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A backend that records watches and delivers no events.
    #[derive(Default, Clone)]
    struct Silent(Arc<Mutex<BTreeSet<PathBuf>>>);

    impl Backend for Silent {
        /// Records full and parent watches (ancestors are bookkeeping).
        fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String> {
            let mut set = self.0.lock().unwrap();
            if kind != WatchKind::Ancestor {
                set.insert(dir.to_path_buf());
            } else {
                set.remove(dir);
            }
            Ok(())
        }
        fn unwatch(&mut self, dir: &Path) {
            self.0.lock().unwrap().remove(dir);
        }
    }

    /// A backend whose every watch fails (inotify limit reached).
    struct Full;

    impl Backend for Full {
        fn watch(&mut self, _: &Path, _: WatchKind) -> Result<(), String> {
            Err("inotify watch limit reached".into())
        }
        fn unwatch(&mut self, _: &Path) {}
    }

    /// A temp dir holding `cfg/`; returns (guard, canonical cfg).
    fn cfg_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = std::fs::canonicalize(tmp.path()).unwrap().join("cfg");
        std::fs::create_dir(&cfg).unwrap();
        (tmp, cfg)
    }

    #[test]
    fn a_failed_watch_falls_back_to_polling() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        let mut core = Core::new(Full, Options::default(), Some(config(&root)));
        assert!(core.has_polled_dirs());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert!(
            b.notices.contains(&Notice::Polling {
                dir: root.clone(),
                reason: PollReason::WatchFailed("inotify watch limit reached".into()),
            }),
            "{b:#?}"
        );
        // Same size, same mtime second: only a content comparison sees it.
        std::fs::write(root.join("a.strand"), "b").unwrap();
        // After the write: the flush is a quiet period later, so the file
        // is not too fresh to read.
        let t0 = Instant::now();
        core.poll(t0);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"b")));
        assert!(b.notices.is_empty(), "each polled dir is reported once");
        // Nothing changed: a poll produces no batch.
        core.poll(t0);
        assert!(core.deadline().is_none());
    }

    /// Polling reads a file only when its stamp changed, except in the
    /// slow content sweep, which skips files over `sweep_max_bytes`.
    #[test]
    fn polling_rehashes_only_on_stamp_change_or_sweep() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        std::fs::write(root.join("big.png"), vec![0u8; 64]).unwrap();
        let opts = Options {
            force_polling: true,
            sweep_max_bytes: 16,
            ..Options::default()
        };
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        core.add_file(&root.join("big.png"), Role::Wallpaper, None);
        core.flush(core.deadline().unwrap());
        let t0 = Instant::now();

        // An edit an attribute cache hides: the bytes change, the stamp the
        // watcher remembers is made to match the new one.
        for (name, body) in [("a.strand", vec![b'b']), ("big.png", vec![1u8; 64])] {
            let p = root.join(name);
            std::fs::write(&p, body).unwrap();
            core.files.get_mut(&p).unwrap().stamp = fresh_stamp(&p);
        }
        core.snaps.insert(root.clone(), snapshot(&root).unwrap());
        core.poll(t0);
        assert!(core.deadline().is_none(), "unchanged stamps are not read");

        // The sweep reads the small file, never the large one.
        core.poll(t0 + opts.content_sweep + Duration::from_secs(1));
        let b = core.flush(core.deadline().unwrap()).unwrap();
        let names: Vec<_> = b.changes.iter().map(|c| c.path.clone()).collect();
        assert_eq!(names, vec![root.join("a.strand")]);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"b")));
    }

    /// Cut the boot listing (watch, then list), which finds nothing new.
    fn settle<B: Backend>(core: &mut Core<B>) {
        let d = core.deadline().expect("the boot listing is due");
        assert!(core.flush(d).is_none(), "nothing changed since the listing");
        assert!(core.deadline().is_none());
    }

    fn strand_files(dir: &Path) -> ModuleSet {
        let mut set = ModuleSet::default();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            set.dirs.push(std::fs::canonicalize(&d).unwrap());
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if is_module_name(&p) {
                    set.files.push(p);
                }
            }
        }
        set.files.sort();
        set
    }

    fn config(dir: &Path) -> ConfigWatch {
        let d = dir.to_path_buf();
        ConfigWatch {
            root: d.clone(),
            modules: strand_files(&d),
            rescan: Box::new(move || Ok(strand_files(&d))),
        }
    }

    /// Overflow: events were lost, so a full rescan must find every change
    /// made behind the watcher's back, including new and removed files.
    #[test]
    fn overflow_rescans_everything() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("bar.strand"), "bar").unwrap();
        std::fs::write(root.join("theme.strand"), "theme").unwrap();
        std::fs::write(root.join("old.strand"), "old").unwrap();
        std::fs::write(root.join("prefs.toml"), "a = 1").unwrap();
        let backend = Silent::default();
        let mut core = Core::new(backend.clone(), Options::default(), Some(config(&root)));
        core.add_file(&root.join("prefs.toml"), Role::Settings, None);
        settle(&mut core);
        assert!(backend.0.lock().unwrap().contains(&root));
        // The root's parent too: a replaced config directory is seen there.
        assert!(backend.0.lock().unwrap().contains(root.parent().unwrap()));

        // Changes nobody hears about.
        std::fs::write(root.join("bar.strand"), "bar 2").unwrap();
        std::fs::write(root.join("new.strand"), "new").unwrap();
        std::fs::remove_file(root.join("old.strand")).unwrap();
        std::fs::write(root.join("prefs.toml"), "a = 2").unwrap();

        let t0 = Instant::now();
        core.on_raw(Raw::Overflow, t0);
        let deadline = core.deadline().unwrap();
        assert_eq!(deadline, t0 + Options::default().coalesce);
        let batch = core.flush(deadline).unwrap();
        assert_eq!(batch.rescan, Some(RescanReason::Overflow));
        assert_eq!((batch.first_event, batch.last_event), (t0, t0));
        let got: Vec<_> = batch
            .changes
            .iter()
            .map(|c| {
                (
                    c.path.file_name().unwrap().to_str().unwrap(),
                    c.kind,
                    c.role,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("bar.strand", ChangeKind::Modified, Role::Module),
                ("new.strand", ChangeKind::Created, Role::Module),
                ("old.strand", ChangeKind::Removed, Role::Module),
                ("prefs.toml", ChangeKind::Modified, Role::Settings),
            ]
        );
        let bar = &batch.changes[0];
        assert_eq!(bar.hash, Some(blake3::hash(b"bar 2")));
        // Nothing left over: the next quiet period is empty.
        assert!(core.deadline().is_none());
    }

    /// A write in progress (`MODIFY`) opens no batch and never moves an
    /// open batch's quiet period: it runs from the last *completed*
    /// write, so a writer that keeps its file open does not stretch every
    /// batch to `max_delay`. Its file is only due if its writer stalls.
    #[test]
    fn busy_never_moves_the_quiet_period() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        std::fs::write(root.join("b.strand"), "b").unwrap();
        let opts = Options::default();
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        settle(&mut core);
        let t0 = Instant::now();
        core.on_raw(Raw::Busy(root.join("a.strand")), t0);
        assert_eq!(core.deadline(), Some(t0 + opts.stalled_write));
        core.on_raw(Raw::Written(root.join("b.strand")), t0);
        for ms in [5, 10, 14] {
            core.on_raw(
                Raw::Busy(root.join("a.strand")),
                t0 + Duration::from_millis(ms),
            );
        }
        assert_eq!(core.deadline(), Some(t0 + Duration::from_millis(15)));
    }

    #[test]
    fn a_linked_file_watches_the_link_and_target_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::write(root.join("store/theme.strand"), "x").unwrap();
        std::os::unix::fs::symlink(
            root.join("store/theme.strand"),
            root.join("cfg/theme.strand"),
        )
        .unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), None);
        core.add_file(&root.join("cfg/theme.strand"), Role::Other, None);
        assert_eq!(
            core.watched_dirs(),
            vec![root.join("cfg"), root.join("store")]
        );
    }

    /// A file whose directory does not exist yet is waited for from its
    /// nearest existing ancestor.
    #[test]
    fn a_missing_directory_is_waited_for_from_its_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), None);
        let prefs = root.join("state/strand/prefs.toml");
        core.add_file(&prefs, Role::Settings, None);
        assert_eq!(core.watched_dirs(), vec![root.clone()]);
        assert!(core.waiting.contains(&root.join("state")));

        std::fs::create_dir_all(root.join("state/strand")).unwrap();
        std::fs::write(&prefs, "a = 1").unwrap();
        let t0 = Instant::now();
        core.on_raw(Raw::Dir(root.join("state")), t0);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert_eq!(core.watched_dirs(), vec![root.join("state/strand")]);
        assert!(core.waiting.is_empty());
    }
    /// Watch, then list: the rescan lists a new directory before its watch
    /// exists, so it is listed once more after the next quiet period. A
    /// file written in between (no event at all) is found then.
    #[test]
    fn a_new_directory_is_listed_again_once_watched() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        let backend = Silent::default();
        let mut core = Core::new(backend.clone(), Options::default(), Some(config(&root)));
        std::fs::create_dir(root.join("w")).unwrap();
        core.on_raw(Raw::Dir(root.join("w")), Instant::now());
        assert!(core.flush(core.deadline().unwrap()).is_none());
        assert!(backend.0.lock().unwrap().contains(&root.join("w")));
        std::fs::write(root.join("w/osd.strand"), "osd").unwrap();
        let b = core
            .flush(core.deadline().expect("a second listing is due"))
            .unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].path, root.join("w/osd.strand"));
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert!(core.deadline().is_none(), "the third listing is not needed");
    }

    /// A directory moved away takes every watch below it along (their
    /// descriptors follow the inodes); a stale sub-directory watch would
    /// otherwise never be added again.
    #[test]
    fn a_moved_directory_forgets_the_watches_below_it() {
        let (_tmp, root) = cfg_dir();
        std::fs::create_dir(root.join("widgets")).unwrap();
        std::fs::write(root.join("widgets/osd.strand"), "osd").unwrap();
        let backend = Silent::default();
        let mut core = Core::new(backend.clone(), Options::default(), Some(config(&root)));
        assert!(core.watched_dirs().contains(&root.join("widgets")));
        let bak = root.with_file_name("cfg.bak");
        std::fs::rename(&root, &bak).unwrap();
        core.on_raw(Raw::Gone(root.clone()), Instant::now());
        let watched = core.watched_dirs();
        assert!(!watched.contains(&root), "{watched:?}");
        assert!(!watched.contains(&root.join("widgets")), "{watched:?}");
        // Back, with an edit made while away: re-watched and re-hashed.
        std::fs::write(bak.join("widgets/osd.strand"), "osd 2").unwrap();
        std::fs::rename(&bak, &root).unwrap();
        core.on_raw(Raw::Dir(root.clone()), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert!(core.watched_dirs().contains(&root.join("widgets")));
        let osd = b
            .changes
            .iter()
            .find(|c| c.path == root.join("widgets/osd.strand"))
            .expect("the nested edit");
        assert_eq!(osd.kind, ChangeKind::Modified);
    }

    /// A backend that saves a file the moment a watch is added: the save
    /// lands between the watch and the baseline read.
    struct SavesDuringWatch {
        file: PathBuf,
        body: &'static str,
        done: bool,
    }

    impl Backend for SavesDuringWatch {
        fn watch(&mut self, _: &Path, _: WatchKind) -> Result<(), String> {
            if !std::mem::replace(&mut self.done, true) {
                std::fs::write(&self.file, self.body).unwrap();
            }
            Ok(())
        }
        fn unwatch(&mut self, _: &Path) {}
    }

    /// Watch, then read: the baseline is taken after the watch exists, so
    /// a save in between is in the baseline (what a loader reading after
    /// `watch_file` holds), and an undo back to the old bytes is reported.
    #[test]
    fn a_save_during_the_watch_is_in_the_baseline() {
        let (_tmp, root) = cfg_dir();
        let prefs = root.join("prefs.toml");
        std::fs::write(&prefs, "a = 1").unwrap();
        let backend = SavesDuringWatch {
            file: prefs.clone(),
            body: "a = 2",
            done: false,
        };
        let mut core = Core::new(backend, Options::default(), None);
        core.add_file(&prefs, Role::Settings, None);
        assert_eq!(core.files[&prefs].hash, Some(blake3::hash(b"a = 2")));
        assert!(core.deadline().is_none());
        std::fs::write(&prefs, "a = 1").unwrap();
        core.on_raw(Raw::Written(prefs.clone()), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Modified);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"a = 1")));

        // The same for module files at boot.
        let module = root.join("bar.strand");
        std::fs::write(&module, "bar 1").unwrap();
        let backend = SavesDuringWatch {
            file: module.clone(),
            body: "bar 2",
            done: false,
        };
        let mut core = Core::new(backend, Options::default(), Some(config(&root)));
        settle(&mut core);
        std::fs::write(&module, "bar 1").unwrap();
        core.on_raw(Raw::Written(module.clone()), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"bar 1")));
    }

    /// The loader read a file, then a save landed before it registered the
    /// path: the hash it passes is compared with the file, and the save is
    /// reported. A matching hash reports nothing.
    #[test]
    fn a_save_between_load_and_registration_is_reported() {
        let (_tmp, root) = cfg_dir();
        let prefs = root.join("prefs.toml");
        let shader = root.join("glow.wgsl");
        std::fs::write(&prefs, "a = 1").unwrap();
        std::fs::write(&shader, "fn main() {}").unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), None);
        let read = blake3::hash(b"a = 1");
        std::fs::write(&prefs, "a = 2").unwrap();
        core.set_referenced(vec![
            (prefs.clone(), Role::Settings, read).into(),
            (shader.clone(), Role::Shader, blake3::hash(b"fn main() {}")).into(),
        ]);
        let b = core
            .flush(core.deadline().expect("the save is due"))
            .unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].path, prefs);
        assert_eq!(b.changes[0].kind, ChangeKind::Modified);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"a = 2")));
        assert!(core.deadline().is_none());

        // An already watched path read stale by the loader is reported
        // too; one it read current is not.
        core.set_referenced(vec![
            (prefs.clone(), Role::Settings, read).into(),
            (shader.clone(), Role::Shader).into(),
        ]);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"a = 2")));
        core.add_file(&prefs, Role::Other, Some(blake3::hash(b"a = 2")));
        assert!(core.deadline().is_none());
    }

    /// A polled file that exists but cannot be opened (EACCES) is compared
    /// by the `stat` stamp `check` stored for it, so it is not re-read on
    /// every poll.
    #[test]
    fn an_unopenable_polled_file_keeps_its_stat_stamp() {
        let (_tmp, root) = cfg_dir();
        let f = root.join("prefs.toml");
        std::fs::write(&f, "a = 1").unwrap();
        let denied = |_: &Path| -> io::Result<std::fs::File> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        };
        let stamp = polled_stamp(&f, denied);
        assert!(stamp.is_some());
        assert_eq!(stamp, current_stamp(&f));
        assert_eq!(polled_stamp(&f, open_nonblocking), current_stamp(&f));
        assert_eq!(polled_stamp(&root.join("missing"), denied), None);
    }

    /// Every ancestor of a watched directory is watched lightly, so moving
    /// one away (the descriptors below follow the inodes and say nothing)
    /// is seen: the file is reported removed, and back again when the
    /// directory returns.
    #[test]
    fn a_moved_ancestor_is_seen() {
        let (_tmp, root) = cfg_dir();
        let z = root.join("x/y/z");
        std::fs::create_dir_all(&z).unwrap();
        let prefs = z.join("prefs.toml");
        std::fs::write(&prefs, "a = 1").unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), None);
        core.add_file(&prefs, Role::Settings, None);
        assert_eq!(core.watched_dirs(), vec![z.clone()]);
        for a in [
            root.join("x/y"),
            root.join("x"),
            root.clone(),
            PathBuf::from("/"),
        ] {
            assert_eq!(core.kinds.get(&a), Some(&WatchKind::Ancestor), "{a:?}");
        }
        std::fs::rename(root.join("x"), root.join("w")).unwrap();
        // What the light watch on `root` reports.
        core.on_raw(Raw::Gone(root.join("x")), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Removed);
        // Waited for from `root`, now watched in full.
        assert_eq!(core.watched_dirs(), vec![root.clone()]);
        assert_eq!(core.kinds.get(&root), Some(&WatchKind::Parent));

        std::fs::rename(root.join("w"), root.join("x")).unwrap();
        core.on_raw(Raw::Dir(root.join("x")), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert_eq!(core.watched_dirs(), vec![z]);
        assert_eq!(core.kinds.get(&root), Some(&WatchKind::Ancestor));
    }

    /// A cache tree is walked before its watches exist: it is walked again
    /// once they do, so a directory made in between is watched.
    #[test]
    fn a_new_tree_is_walked_again_once_watched() {
        let (_tmp, root) = cfg_dir();
        let mut core = Core::new(Silent::default(), Options::default(), None);
        core.add_tree(&root, 2, CacheKind::Icons);
        std::fs::create_dir(root.join("hicolor")).unwrap();
        let d = core.deadline().expect("a second walk is due");
        core.flush(d);
        assert!(core.watched_dirs().contains(&root.join("hicolor")));
    }

    /// A file with a write in progress is never read: it is held at the
    /// flush (no batch, no busy loop: the next deadline is the stall
    /// limit) and read once its `CLOSE_WRITE` arrives.
    #[test]
    fn a_file_being_written_is_held_until_closed() {
        let (_tmp, root) = cfg_dir();
        let theme = root.join("theme.strand");
        std::fs::write(&theme, "a").unwrap();
        let opts = Options::default();
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        settle(&mut core);
        // Delete and create; the new file has its first half.
        std::fs::write(&theme, "ha").unwrap();
        let t0 = Instant::now();
        core.on_raw(Raw::Gone(theme.clone()), t0);
        core.on_raw(Raw::Linked(theme.clone()), t0);
        core.on_raw(Raw::Busy(theme.clone()), t0);
        let d = core.deadline().unwrap();
        assert!(core.flush(d).is_none(), "nothing read mid-write");
        assert_eq!(core.deadline(), Some(t0 + opts.stalled_write));
        std::fs::write(&theme, "half and the rest").unwrap();
        let t1 = t0 + Duration::from_millis(300);
        core.on_raw(Raw::Written(theme.clone()), t1);
        // Back in the open batch: due after the coalesce, like any write,
        // so a "save all" finishing right after is in the same batch.
        assert_eq!(core.deadline(), Some(t1 + opts.coalesce));
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Modified);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"half and the rest")));
        assert!(b.notices.is_empty());
        assert!(core.deadline().is_none());
    }

    /// A writer that neither writes again nor closes for `stalled_write`
    /// has its file read anyway, with a notice.
    #[test]
    fn a_stalled_writer_is_read_with_a_notice() {
        let (_tmp, root) = cfg_dir();
        let osd = root.join("osd.strand");
        let mut core = Core::new(Silent::default(), Options::default(), Some(config(&root)));
        settle(&mut core);
        std::fs::write(&osd, "osd").unwrap();
        let t0 = Instant::now();
        core.on_raw(Raw::Busy(osd.clone()), t0);
        core.on_raw(Raw::Linked(osd.clone()), t0);
        assert!(core.flush(core.deadline().unwrap()).is_none());
        let due = core.deadline().unwrap();
        assert_eq!(due, t0 + Options::default().stalled_write);
        let b = core.flush(due).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert_eq!(b.notices, vec![Notice::StalledWrite(osd)]);
        assert!(core.deadline().is_none());
    }

    /// A file written in place whose writer keeps it open and stops
    /// writing (`MODIFY` only, no completed write ever names it) is due
    /// `stalled_write` after its last write and read with a notice; its
    /// later `CLOSE_WRITE` reports the rest.
    #[test]
    fn a_stalled_in_place_writer_is_read_with_a_notice() {
        let (_tmp, root) = cfg_dir();
        let theme = root.join("theme.strand");
        std::fs::write(&theme, "theme").unwrap();
        let opts = Options::default();
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        settle(&mut core);
        std::fs::write(&theme, "ha").unwrap();
        let t0 = Instant::now();
        core.on_raw(Raw::Busy(theme.clone()), t0);
        let t1 = t0 + Duration::from_millis(100);
        core.on_raw(Raw::Busy(theme.clone()), t1);
        assert_eq!(core.deadline(), Some(t1 + opts.stalled_write));
        let b = core.flush(t1 + opts.stalled_write).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"ha")));
        assert_eq!(b.notices, vec![Notice::StalledWrite(theme.clone())]);
        assert!(core.deadline().is_none());
        std::fs::write(&theme, "half and the rest").unwrap();
        let t2 = t1 + opts.stalled_write + Duration::from_millis(10);
        core.on_raw(Raw::Written(theme.clone()), t2);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"half and the rest")));
        assert!(b.notices.is_empty());
    }

    /// A backend that, the first time it is drained, truncates `file` (as
    /// an in-place save's `O_TRUNC` does, right after the flush's last
    /// drain) and reports the `MODIFY` that makes.
    struct WritesDuringRead {
        file: PathBuf,
        armed: bool,
    }

    impl Backend for WritesDuringRead {
        fn watch(&mut self, _: &Path, _: WatchKind) -> Result<(), String> {
            Ok(())
        }
        fn unwatch(&mut self, _: &Path) {}
        fn drain(&mut self, out: &mut Vec<Raw>) -> io::Result<()> {
            if std::mem::take(&mut self.armed) {
                out.push(Raw::Busy(self.file.clone()));
            }
            Ok(())
        }
    }

    /// A write that starts while a flush reads the file: its `MODIFY` is
    /// in the queue when the flush drains it after hashing, so the bytes
    /// read are not reported (nor kept as the baseline); the file is held
    /// and reported once, whole, after its `CLOSE_WRITE`.
    #[test]
    fn a_write_during_the_read_is_not_reported_until_closed() {
        let (_tmp, root) = cfg_dir();
        let theme = root.join("theme.strand");
        std::fs::write(&theme, "theme").unwrap();
        let opts = Options::default();
        let backend = WritesDuringRead {
            file: theme.clone(),
            armed: false,
        };
        let mut core = Core::new(backend, opts.clone(), Some(config(&root)));
        settle(&mut core);
        // A completed save, then (while it is read) the next one begins.
        std::fs::write(&theme, "").unwrap();
        let t0 = Instant::now();
        core.on_raw(Raw::Written(theme.clone()), t0);
        core.backend().armed = true;
        let cut = core.deadline().unwrap();
        assert!(
            core.flush(cut).is_none(),
            "the truncated file is not reported"
        );
        assert_eq!(core.files[&theme].hash, Some(blake3::hash(b"theme")));
        assert!(core.held.contains_key(&theme));
        assert_eq!(core.deadline(), Some(cut + opts.stalled_write));
        std::fs::write(&theme, "theme 2").unwrap();
        let t1 = t0 + Duration::from_millis(40);
        core.on_raw(Raw::Written(theme.clone()), t1);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"theme 2")));
        assert!(b.notices.is_empty());
        assert!(core.deadline().is_none());
    }

    /// Outside the config directories an in-place write makes no event
    /// until it closes. One that truncates the file after the flush began
    /// (its `now` taken) but before the file is read is caught by the
    /// modification time, measured when the read ends: the torn bytes are
    /// not reported, and the whole file is once it is quiet.
    #[test]
    fn a_write_after_the_flush_began_is_not_read_too_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let prefs = root.join("prefs.toml");
        std::fs::write(&prefs, "v = 1\nw = 1\n").unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), None);
        core.add_file(&prefs, Role::Settings, None);
        assert!(!core.tracked(&prefs));
        std::thread::sleep(Duration::from_millis(30));
        let t0 = Instant::now();
        core.on_raw(Raw::Written(prefs.clone()), t0);
        let now = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(&prefs, "v = 2\n").unwrap();
        assert!(core.flush(now).is_none(), "the torn bytes are not reported");
        assert_eq!(
            core.files[&prefs].hash,
            Some(blake3::hash(b"v = 1\nw = 1\n"))
        );
        std::fs::write(&prefs, "v = 2\nw = 2\n").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"v = 2\nw = 2\n")));
        // The batch reports the event that made the file due, not the
        // flush that put it off.
        assert_eq!((b.first_event, b.last_event), (t0, t0));
    }

    /// A file rewritten faster than the quiet period is always "too
    /// fresh" to read; it is put off, but not past `max_delay` after the
    /// event that first made it due: then a stable read is taken whatever
    /// its modification time, and the batch reports that first event.
    #[test]
    fn a_file_rewritten_without_pause_is_read_within_max_delay() {
        let (_tmp, root) = cfg_dir();
        let theme = root.join("theme.strand");
        std::fs::write(&theme, "theme 0").unwrap();
        let opts = Options::default();
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        settle(&mut core);
        let t0 = Instant::now();
        let mut n = 0;
        let mut first = None;
        let b = loop {
            n += 1;
            std::fs::write(&theme, format!("theme {n}")).unwrap();
            let t = Instant::now();
            first.get_or_insert(t);
            core.on_raw(Raw::Written(theme.clone()), t);
            let due = core.deadline().unwrap();
            assert!(
                due <= (t0 + opts.max_delay).max(t + opts.coalesce),
                "cut at {:?}",
                due - t0
            );
            if let Some(b) = core.flush(t) {
                break b;
            }
            let since = first.unwrap_or(t0);
            assert!(t < since + opts.max_delay, "not read by {:?}", t - since);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(t0.elapsed() >= opts.max_delay);
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(
            b.changes[0].hash,
            Some(blake3::hash(format!("theme {n}").as_bytes()))
        );
        assert_eq!(Some(b.first_event), first);
        assert!(!core.deferred.contains_key(&theme));
    }

    /// A rescan that finds a dangling `*.strand` link reports it as a
    /// notice next to the module's removal, and again (empty) once fixed.
    #[test]
    fn module_set_errors_are_forwarded() {
        let (_tmp, root) = cfg_dir();
        let bar = root.join("bar.strand");
        std::fs::write(&bar, "bar").unwrap();
        let errors = Arc::new(Mutex::new(Vec::new()));
        let d = root.clone();
        let e = errors.clone();
        let cfg = ConfigWatch {
            root: root.clone(),
            modules: strand_files(&root),
            rescan: Box::new(move || {
                let mut set = strand_files(&d);
                set.errors = e.lock().unwrap().clone();
                Ok(set)
            }),
        };
        let mut core = Core::new(Silent::default(), Options::default(), Some(cfg));
        settle(&mut core);
        std::fs::remove_file(&bar).unwrap();
        let why = (bar.clone(), "dangling link".to_string());
        errors.lock().unwrap().push(why.clone());
        core.on_raw(Raw::Gone(bar.clone()), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Removed);
        assert_eq!(
            b.notices,
            vec![Notice::ModuleSet {
                errors: vec![why],
                too_deep: vec![],
            }]
        );
        errors.lock().unwrap().clear();
        std::fs::write(&bar, "bar").unwrap();
        core.on_raw(Raw::Written(bar.clone()), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert_eq!(
            b.notices,
            vec![Notice::ModuleSet {
                errors: vec![],
                too_deep: vec![],
            }]
        );
    }

    /// A held file deleted (its writer still has it open) and created
    /// again: the removal brings it back into the batch, which then waits
    /// for the removal grace, so the re-creation makes one `Modified`.
    #[test]
    fn a_held_file_removed_waits_for_the_removal_grace() {
        let (_tmp, root) = cfg_dir();
        let theme = root.join("theme.strand");
        std::fs::write(&theme, "a").unwrap();
        let opts = Options::default();
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        settle(&mut core);
        let t0 = Instant::now();
        std::fs::write(&theme, "ha").unwrap();
        core.on_raw(Raw::Busy(theme.clone()), t0);
        core.on_raw(Raw::Written(root.join("bar.strand")), t0);
        assert!(core.flush(core.deadline().unwrap()).is_none());
        let t1 = t0 + Duration::from_millis(200);
        core.on_raw(Raw::Gone(theme.clone()), t1);
        assert_eq!(core.deadline(), Some(t1 + opts.removal_grace));
        let t2 = t1 + Duration::from_millis(5);
        std::fs::write(&theme, "whole").unwrap();
        core.on_raw(Raw::Written(theme.clone()), t2);
        assert_eq!(core.deadline(), Some(t2 + opts.coalesce));
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Modified);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"whole")));
    }

    /// In a directory watched without `MODIFY`, a new file that already
    /// has bytes may still be being written: it is held until its
    /// `CLOSE_WRITE`, and a writer counts as stalled only once the file
    /// stopped changing (its writes make no event there). A cache-tree
    /// entry created the same way is reported at once.
    #[test]
    fn a_new_file_without_modify_is_held_while_it_changes() {
        let (_tmp, root) = cfg_dir();
        let pics = root.with_file_name("pics");
        let fonts = root.with_file_name("fonts");
        std::fs::create_dir(&pics).unwrap();
        std::fs::create_dir(&fonts).unwrap();
        let wall = pics.join("wall.png");
        let opts = Options::default();
        let mut core = Core::new(Silent::default(), opts.clone(), Some(config(&root)));
        core.add_file(&wall, Role::Wallpaper, None);
        core.add_tree(&fonts, 1, CacheKind::Fonts);
        settle(&mut core);
        assert_eq!(core.kinds.get(&pics), Some(&WatchKind::Completed));
        assert_eq!(core.kinds.get(&fonts), Some(&WatchKind::Completed));
        assert_eq!(core.kinds.get(&root), Some(&WatchKind::Full));

        std::fs::write(&wall, "png 1").unwrap();
        std::fs::write(fonts.join("a.ttf"), "ttf").unwrap();
        let t0 = Instant::now();
        core.on_raw(Raw::Created(wall.clone()), t0);
        core.on_raw(Raw::Created(fonts.join("a.ttf")), t0);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].role, Role::Cache(CacheKind::Fonts));
        let due = core.deadline().unwrap();
        assert_eq!(due, t0 + opts.stalled_write);
        // Still growing at the stall limit: held again.
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&wall, "png 1 and 2").unwrap();
        assert!(core.flush(due).is_none());
        let due = core.deadline().unwrap();
        assert!(due >= t0 + opts.stalled_write * 2 - Duration::from_millis(50));
        // Unchanged for the stall limit: read, with a notice.
        let b = core.flush(due).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"png 1 and 2")));
        assert_eq!(b.notices, vec![Notice::StalledWrite(wall.clone())]);

        // The usual case: the writer closes it.
        std::fs::remove_file(&wall).unwrap();
        let t3 = Instant::now();
        core.on_raw(Raw::Gone(wall.clone()), t3);
        std::fs::write(&wall, "png 3").unwrap();
        core.on_raw(Raw::Created(wall.clone()), t3);
        assert!(core.flush(core.deadline().unwrap()).is_none());
        let t4 = t3 + Duration::from_millis(100);
        core.on_raw(Raw::Written(wall.clone()), t4);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"png 3")));
        assert!(b.notices.is_empty());
    }

    /// A batch of content edits does not touch the watches, so its cost
    /// does not grow with the number of watched directories (a large icon
    /// tree). A directory going does re-sync.
    #[test]
    fn content_only_flushes_do_not_resync() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("bar.strand"), "bar").unwrap();
        let icons = root.with_file_name("icons");
        for i in 0..200 {
            std::fs::create_dir_all(icons.join(format!("t{i}/apps"))).unwrap();
        }
        let mut core = Core::new(Silent::default(), Options::default(), Some(config(&root)));
        core.add_tree(&icons, 2, CacheKind::Icons);
        while let Some(d) = core.deadline() {
            core.flush(d);
        }
        assert_eq!(
            core.watched_dirs().len(),
            1 + 1 + 1 + 400,
            "cfg, its parent, icons, tree"
        );
        let before = core.syncs;
        for i in 0..3 {
            std::fs::write(root.join("bar.strand"), format!("bar {i}")).unwrap();
            core.on_raw(Raw::Written(root.join("bar.strand")), Instant::now());
            let b = core.flush(core.deadline().unwrap()).unwrap();
            assert_eq!(b.changes.len(), 1);
            std::fs::write(icons.join("t1/apps/x.png"), "png").unwrap();
            core.on_raw(Raw::Written(icons.join("t1/apps/x.png")), Instant::now());
            let b = core.flush(core.deadline().unwrap()).unwrap();
            assert_eq!(b.changes[0].role, Role::Cache(CacheKind::Icons));
        }
        assert_eq!(core.syncs, before, "content edits never re-sync");
        std::fs::remove_dir(icons.join("t2/apps")).unwrap();
        core.on_raw(Raw::Gone(icons.join("t2/apps")), Instant::now());
        core.flush(core.deadline().unwrap());
        assert!(core.syncs > before);
        assert!(!core.watched_dirs().contains(&icons.join("t2/apps")));
    }

    /// The config root's parent (`~/.config`) and a missing directory's
    /// stand-in are watched for names only, never for writes.
    #[test]
    fn the_root_s_parent_is_watched_for_names_only() {
        let (_tmp, root) = cfg_dir();
        let mut core = Core::new(Silent::default(), Options::default(), Some(config(&root)));
        let parent = root.parent().unwrap().to_path_buf();
        assert_eq!(core.kinds.get(&root), Some(&WatchKind::Full));
        assert_eq!(core.kinds.get(&parent), Some(&WatchKind::Parent));
        core.add_file(&parent.join("missing/prefs.toml"), Role::Settings, None);
        assert_eq!(core.kinds.get(&parent), Some(&WatchKind::Parent));
        // A file of its own there makes it a content watch (no `MODIFY`:
        // other apps write in `~/.config`).
        core.add_file(&parent.join("prefs.toml"), Role::Settings, None);
        assert_eq!(core.kinds.get(&parent), Some(&WatchKind::Completed));
    }
}

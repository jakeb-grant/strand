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
    /// after its first event.
    pub max_delay: Duration,
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
    /// A completed write: `CLOSE_WRITE`, `MOVED_TO`, a symlink or hard link
    /// created, or a polled entry that changed.
    Written(PathBuf),
    /// `DELETE`, `MOVED_FROM`, a directory deleted, or a watched directory
    /// deleted or moved.
    Gone(PathBuf),
    /// A directory created or moved in.
    Dir(PathBuf),
    /// `MODIFY` or a plain file created: a write in progress. Never read;
    /// it only keeps an open batch waiting.
    Busy(PathBuf),
    /// The inotify queue overflowed.
    Overflow,
}

/// What a directory watch is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchKind {
    /// A directory whose entries matter: every event Strand acts on.
    Full,
    /// An ancestor of a watched directory, watched only so that its
    /// children being moved or deleted is seen (`mv ~/x ~/w` when only
    /// `~/x/y/z` holds a referenced file). Writes in it queue nothing.
    Ancestor,
}

/// Adds and removes inotify directory watches. Watching a directory again
/// with another kind replaces its mask.
pub(crate) trait Backend {
    fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String>;
    fn unwatch(&mut self, dir: &Path);
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
    /// Explicit registrations (`watch_file`, `set_referenced`), counted
    /// per role.
    refs: BTreeMap<Role, usize>,
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
            refs: BTreeMap::new(),
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
            Ok((h, s)) => (Some(h), Some(s), None, true),
            Err(e) if is_missing(&e) => (None, None, None, false),
            Err(e) => (None, current_stamp(path), Some(e.kind()), true),
        };
    }

    fn unused(&self) -> bool {
        !self.module && self.refs.is_empty()
    }

    fn roles(&self) -> BTreeSet<Role> {
        let mut roles: BTreeSet<Role> = self.refs.keys().copied().collect();
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
    pub(crate) fn register(&mut self, path: &Path, hash: ContentHash, now: Instant) {
        self.expire(now);
        let path = paths::absolute(path);
        self.seq += 1;
        self.list.push(OwnWrite {
            seq: self.seq,
            canonical: paths::resolve(&path).path,
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
    full: Option<RescanReason>,
    notices: Vec<Notice>,
    first: Option<Instant>,
    last: Option<Instant>,
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
    /// Watched directories that are only ancestors ([`WatchKind::Ancestor`]).
    light: HashSet<PathBuf>,
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
    pending: Pending,
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

/// Hash a regular file, streaming. Anything else (a FIFO, a device, a
/// directory) is `InvalidInput` and never read: a FIFO would block the
/// watcher thread and `/dev/zero` never ends.
fn read_hash(path: &Path) -> io::Result<(ContentHash, Stamp)> {
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
    Ok((hasher.finalize(), Stamp::of(&meta)))
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
            light: HashSet::new(),
            watched_ino: HashMap::new(),
            snaps: HashMap::new(),
            by_path: HashMap::new(),
            rescan_paths: HashSet::new(),
            config_dirs: HashSet::new(),
            tree_dirs: HashMap::new(),
            tree_hops: HashMap::new(),
            waiting: HashSet::new(),
            pending: Pending::default(),
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
        let added = core.sync_watches();
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

    /// Take the baseline of entries just created by a registration (their
    /// directories are watched by now), then compare each with the bytes
    /// the loader says it read: a mismatch is a save between the read and
    /// the registration, reported at the next quiet period.
    fn settle_new(&mut self, fresh: &[PathBuf], loaded: &[(PathBuf, ContentHash)]) {
        for path in fresh {
            if let Some(e) = self.files.get_mut(path) {
                e.take_baseline(path);
            }
        }
        let mut stale = false;
        for (path, hash) in loaded {
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
                stale = true;
            }
        }
        if stale {
            self.mark(Instant::now());
        }
    }

    /// Watch one more file (settings TOML, wallpaper, shader, …) for
    /// `role`. Registrations are counted per (path, role). The file need
    /// not exist yet, nor its directory: creation is reported. `loaded` is
    /// the hash of the bytes the caller already read, if it did.
    pub(crate) fn add_file(&mut self, path: &Path, role: Role, loaded: Option<ContentHash>) {
        let path = paths::absolute(path);
        let fresh = !self.files.contains_key(&path);
        let e = self
            .files
            .entry(path.clone())
            .or_insert_with(|| Entry::unread(&path));
        *e.refs.entry(role).or_default() += 1;
        self.sync_watches();
        let fresh: Vec<PathBuf> = fresh.then(|| path.clone()).into_iter().collect();
        let loaded: Vec<_> = loaded.map(|h| (path, h)).into_iter().collect();
        self.settle_new(&fresh, &loaded);
        self.mark_if_notices();
    }

    /// Drop one registration made with [`Core::add_file`]. Module files
    /// belong to the module set and stay.
    pub(crate) fn remove_file(&mut self, path: &Path, role: Role) {
        let path = paths::absolute(path);
        let Some(e) = self.files.get_mut(&path) else {
            return;
        };
        match e.refs.get_mut(&role) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                e.refs.remove(&role);
            }
            None => return,
        }
        if e.unused() {
            self.files.remove(&path);
            self.pending.files.remove(&path);
        }
        self.sync_watches();
    }

    /// Replace every registration made with [`Core::add_file`] by this
    /// set (what the compiler collected from the whole program). A pair
    /// listed twice counts twice.
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
            e.refs = wanted.remove(p).unwrap_or_default();
        }
        self.files.retain(|_, e| !e.unused());
        let mut fresh = Vec::new();
        for (p, refs) in wanted {
            let mut e = Entry::unread(&p);
            e.refs = refs;
            self.files.insert(p.clone(), e);
            fresh.push(p);
        }
        let files = &self.files;
        self.pending.files.retain(|p| files.contains_key(p));
        self.sync_watches();
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
        let added = self.sync_watches();
        self.relist(&added, Instant::now());
        self.mark_if_notices();
    }

    /// The next write of `path` whose content hashes to `hash` is Strand's
    /// own and is not reported.
    /// The backend was replaced (inotify failed): every watch it held is
    /// gone. Add them all again and rescan everything, since events were
    /// lost.
    pub(crate) fn rewatch_all(&mut self, now: Instant) {
        self.watched.clear();
        self.light.clear();
        self.watched_ino.clear();
        self.snaps.clear();
        self.sync_watches();
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
        self.pending.first.get_or_insert(now);
        self.pending.last = Some(now);
    }

    /// When the open batch should be cut, if one is open.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let first = self.pending.first?;
        let last = self.pending.last?;
        let quiet = if self.pending.gone.is_empty() {
            self.opts.coalesce
        } else {
            self.opts.removal_grace
        };
        Some((last + quiet).min(first + self.opts.max_delay))
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
            Raw::Written(p) => self.on_path(p, What::Written, now),
            Raw::Gone(p) => self.on_path(p, What::Gone, now),
            Raw::Dir(p) => self.on_path(p, What::Dir, now),
            Raw::Busy(p) => {
                if self.pending.first.is_some() && self.relevant(&p) {
                    self.pending.last = Some(now);
                }
            }
        }
    }

    fn relevant(&self, p: &Path) -> bool {
        if self.by_path.contains_key(p) {
            return true;
        }
        let Some(parent) = p.parent() else {
            return false;
        };
        (self.config_dirs.contains(parent) && is_module_name(p))
            || self.tree_dirs.contains_key(parent)
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
        self.light.remove(dir);
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

    /// Cut the open batch: rescan if asked, re-resolve symlinks, re-watch,
    /// hash what was touched and drop what did not change.
    pub(crate) fn flush(&mut self, now: Instant) -> Option<FileBatch> {
        let p = std::mem::take(&mut self.pending);
        let mut changes = Vec::new();
        let mut notices = p.notices;
        let mut touched = p.files;
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
        // that lands after the read is seen by the new watch.
        for path in &touched {
            if let Some(e) = self.files.get_mut(path) {
                e.resolved = paths::resolve(path);
            }
        }
        let added = self.sync_watches();
        notices.append(&mut self.pending.notices);
        for path in &touched {
            self.check(path, &mut changes);
        }
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
            first_event: p.first.unwrap_or(now),
            last_event: p.last.unwrap_or(now),
        })
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
                            refs: BTreeMap::new(),
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
    fn check(&mut self, path: &Path, out: &mut Vec<FileChange>) {
        let Some(e) = self.files.get(path) else {
            return;
        };
        let was = e.exists;
        let old = e.hash;
        let old_error = e.error;
        let canonical = e.resolved.path.clone();
        let moved = e.reported != canonical;
        let (base, hash, error) = match read_hash(path) {
            Ok((h, stamp)) => {
                let own = lock(&self.own).take(path, &canonical, h);
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

    fn desired_dirs(&self) -> BTreeSet<PathBuf> {
        let mut dirs = BTreeSet::new();
        if let Some(cfg) = &self.config {
            dirs.extend(cfg.modules.dirs.iter().cloned());
        }
        if let Some(root) = &self.config_root {
            dirs.extend(
                root.hops
                    .iter()
                    .filter_map(|h| h.parent())
                    .map(Path::to_path_buf),
            );
            dirs.extend(root.path.parent().map(Path::to_path_buf));
        }
        for e in self.files.values() {
            dirs.extend(e.resolved.watch_dirs().map(Path::to_path_buf));
        }
        for t in &self.trees {
            dirs.insert(t.resolved.path.clone());
            dirs.extend(t.dirs.iter().cloned());
            dirs.extend(
                t.resolved
                    .hops
                    .iter()
                    .filter_map(|h| h.parent())
                    .map(Path::to_path_buf),
            );
        }
        dirs
    }

    /// Bring the backend's watches in line with what the state needs. A
    /// wanted directory that does not exist is replaced by its nearest
    /// existing ancestor, and the first missing path below that ancestor
    /// is remembered in `waiting`. Every ancestor of a wanted directory is
    /// watched lightly ([`WatchKind::Ancestor`]), so that one of them being
    /// moved or deleted is seen even though the descriptors below follow
    /// the moved inodes and report nothing. A watched directory whose inode
    /// changed behind our back is watched again, and what lies below it
    /// re-checked at the next flush. Returns the directories newly watched
    /// or polled in full.
    fn sync_watches(&mut self) -> Vec<PathBuf> {
        self.reindex();
        self.waiting.clear();
        let mut desired: BTreeMap<PathBuf, WatchKind> = BTreeMap::new();
        for d in self.desired_dirs() {
            if d.is_dir() {
                desired.insert(d, WatchKind::Full);
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
                desired.insert(a.to_path_buf(), WatchKind::Full);
                self.waiting.insert(child.to_path_buf());
            }
        }
        let full: Vec<PathBuf> = desired.keys().cloned().collect();
        for d in full {
            for a in d.ancestors().skip(1) {
                desired
                    .entry(a.to_path_buf())
                    .or_insert(WatchKind::Ancestor);
            }
        }
        let stale: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(d, _)| match desired.get(*d) {
                None => true,
                // Wanted for another reason now: watched again below.
                Some(k) => (*k == WatchKind::Ancestor) != self.light.contains(*d),
            })
            .map(|(d, _)| d.clone())
            .collect();
        for d in stale {
            // Not `forget_watch`: re-adding an inotify watch replaces its
            // mask, and dropping it first would lose events in between.
            self.watched.remove(&d);
            self.light.remove(&d);
            self.snaps.remove(&d);
            if !desired.contains_key(&d) {
                self.backend.unwatch(&d);
                self.watched_ino.remove(&d);
            }
        }
        let mut added = Vec::new();
        for (d, kind) in desired {
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
            // Still known: watched until now for the other kind, and its
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
                self.light.insert(d.clone());
                self.watched_ino.insert(d.clone(), meta.ino());
                self.watched.insert(d, mode);
                continue;
            }
            let poll = |reason| (Mode::Poll, Some(reason));
            let (mode, reason) = match paths::fs_kind(&d) {
                FsKind::Immutable => (Mode::Skip, None),
                _ if self.opts.force_polling => poll(PollReason::Forced),
                FsKind::NoEvents => poll(PollReason::NoEventsFilesystem),
                FsKind::Local => match self.backend.watch(&d, WatchKind::Full) {
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
            .filter(|d| !self.light.contains(*d))
            .cloned()
            .collect()
    }
}

fn is_module_name(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "strand")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A backend that records watches and delivers no events.
    #[derive(Default, Clone)]
    struct Silent(Arc<Mutex<BTreeSet<PathBuf>>>);

    impl Backend for Silent {
        /// Records full watches only (ancestors are bookkeeping).
        fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String> {
            let mut set = self.0.lock().unwrap();
            if kind == WatchKind::Full {
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
        let t0 = Instant::now();
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

    #[test]
    fn busy_extends_only_an_open_batch() {
        let (_tmp, root) = cfg_dir();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), Some(config(&root)));
        settle(&mut core);
        let t0 = Instant::now();
        core.on_raw(Raw::Busy(root.join("a.strand")), t0);
        assert!(core.deadline().is_none());
        core.on_raw(Raw::Written(root.join("a.strand")), t0);
        let later = t0 + Duration::from_millis(10);
        core.on_raw(Raw::Busy(root.join("a.strand")), later);
        assert_eq!(core.deadline(), Some(later + Duration::from_millis(15)));
        // A never-quiet stream is still cut at max_delay.
        let much_later = t0 + Duration::from_secs(2);
        core.on_raw(Raw::Busy(root.join("a.strand")), much_later);
        assert_eq!(core.deadline(), Some(t0 + Duration::from_millis(500)));
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
            assert!(core.light.contains(&a), "{a:?} {:?}", core.light);
        }
        std::fs::rename(root.join("x"), root.join("w")).unwrap();
        // What the light watch on `root` reports.
        core.on_raw(Raw::Gone(root.join("x")), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Removed);
        // Waited for from `root`, now watched in full.
        assert_eq!(core.watched_dirs(), vec![root.clone()]);
        assert!(!core.light.contains(&root));

        std::fs::rename(root.join("w"), root.join("x")).unwrap();
        core.on_raw(Raw::Dir(root.join("x")), Instant::now());
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert_eq!(core.watched_dirs(), vec![z]);
        assert!(core.light.contains(&root));
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
}

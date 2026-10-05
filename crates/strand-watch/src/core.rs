//! The watcher's state machine: raw directory events in, coalesced and
//! hash-checked [`FileBatch`]es out. It reads the filesystem but owns no
//! inotify instance; a [`Backend`] adds and removes directory watches, so
//! tests can drive it with no events at all (the overflow test does).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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
    pub poll_interval: Duration,
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
    /// A completed write: `CLOSE_WRITE`, `MOVED_TO`, a symlink created, or a
    /// polled entry that changed.
    Written(PathBuf),
    /// `DELETE`, `MOVED_FROM`, or a watched directory deleted or moved.
    Gone(PathBuf),
    /// A directory created, removed or moved in.
    Dir(PathBuf),
    /// `MODIFY` or a plain file created: a write in progress. Never read;
    /// it only keeps an open batch waiting.
    Busy(PathBuf),
    /// The inotify queue overflowed.
    Overflow,
}

/// Adds and removes inotify directory watches.
pub(crate) trait Backend {
    fn watch(&mut self, dir: &Path) -> Result<(), String>;
    fn unwatch(&mut self, dir: &Path);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inotify,
    Poll,
    /// Read-only mount: nothing changes in place.
    Skip,
}

#[derive(Debug)]
struct Entry {
    role: Role,
    resolved: Resolved,
    hash: Option<ContentHash>,
    exists: bool,
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
    canonical: PathBuf,
    path: PathBuf,
    hash: ContentHash,
    at: Instant,
}

/// Own-write registrations nobody matched are dropped after this long.
const OWN_WRITE_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dir: bool,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
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
        snap.insert(
            entry.file_name(),
            Stamp {
                dir: m.is_dir(),
                ino: m.ino(),
                len: m.len(),
                mtime: (m.mtime(), m.mtime_nsec()),
                ctime: (m.ctime(), m.ctime_nsec()),
            },
        );
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
    own: Vec<OwnWrite>,
    watched: BTreeMap<PathBuf, Mode>,
    snaps: HashMap<PathBuf, Snapshot>,
    // Index, rebuilt by `reindex`.
    by_path: HashMap<PathBuf, Vec<PathBuf>>,
    rescan_paths: HashSet<PathBuf>,
    config_dirs: HashSet<PathBuf>,
    tree_dirs: HashMap<PathBuf, usize>,
    tree_hops: HashMap<PathBuf, usize>,
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

fn read_hash(path: &Path) -> io::Result<ContentHash> {
    std::fs::read(path).map(|b| blake3::hash(&b))
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
    /// Set up watches and record a baseline hash of every module file.
    pub(crate) fn new(backend: B, opts: Options, config: Option<ConfigWatch>) -> Self {
        let mut core = Core {
            backend,
            opts,
            config: None,
            config_root: None,
            files: BTreeMap::new(),
            trees: Vec::new(),
            own: Vec::new(),
            watched: BTreeMap::new(),
            snaps: HashMap::new(),
            by_path: HashMap::new(),
            rescan_paths: HashSet::new(),
            config_dirs: HashSet::new(),
            tree_dirs: HashMap::new(),
            tree_hops: HashMap::new(),
            pending: Pending::default(),
        };
        if let Some(mut cfg) = config {
            cfg.root = paths::absolute(&cfg.root);
            cfg.modules.dirs = canonical_dirs(&cfg.modules.dirs);
            core.config_root = Some(paths::resolve(&cfg.root));
            for f in &cfg.modules.files {
                core.insert_baseline(paths::absolute(f), Role::Module);
            }
            core.config = Some(cfg);
        }
        core.sync_watches();
        core.mark_if_notices();
        core
    }

    fn insert_baseline(&mut self, path: PathBuf, role: Role) {
        let resolved = paths::resolve(&path);
        let hash = read_hash(&path);
        let exists = !matches!(&hash, Err(e) if e.kind() == io::ErrorKind::NotFound
            || e.kind() == io::ErrorKind::NotADirectory);
        self.files.insert(
            path,
            Entry {
                role,
                resolved,
                hash: hash.ok(),
                exists,
            },
        );
    }

    /// Watch one more file (settings TOML, wallpaper, shader, …). It need
    /// not exist yet; its directory is watched and creation is reported.
    pub(crate) fn add_file(&mut self, path: &Path, role: Role) {
        let path = paths::absolute(path);
        match self.files.get_mut(&path) {
            Some(e) => e.role = role,
            None => self.insert_baseline(path, role),
        }
        self.sync_watches();
        self.mark_if_notices();
    }

    /// Stop watching a file added with [`Core::add_file`]. Module files
    /// belong to the module set and stay.
    pub(crate) fn remove_file(&mut self, path: &Path) {
        let path = paths::absolute(path);
        if self
            .files
            .get(&path)
            .is_some_and(|e| e.role != Role::Module)
        {
            self.files.remove(&path);
            self.pending.files.remove(&path);
            self.sync_watches();
        }
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
        self.sync_watches();
        self.mark_if_notices();
    }

    /// The next write of `path` whose content hashes to `hash` is Strand's
    /// own and is not reported.
    pub(crate) fn register_own_write(&mut self, path: &Path, hash: ContentHash, now: Instant) {
        let path = paths::absolute(path);
        self.own.push(OwnWrite {
            canonical: paths::resolve(&path).path,
            path,
            hash,
            at: now,
        });
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
                if what != What::Written
                    && self.files.get(f).is_some_and(|e| e.role == Role::Module)
                {
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
        if let Some(&t) = self.tree_hops.get(&p) {
            self.pending.trees = true;
            let tree = &self.trees[t];
            self.pending.cache.insert(tree.root.clone(), tree.kind);
            hit = true;
        }
        if what != What::Written && self.watched.contains_key(&p) {
            self.forget_watch(&p);
            self.pending.config |= self.config_dirs.contains(&p);
            self.pending.trees |= self.tree_dirs.contains_key(&p);
            for (f, e) in &self.files {
                if e.resolved.watch_dirs().any(|d| d == p) {
                    self.pending.files.insert(f.clone());
                }
            }
            hit = true;
        }
        if let Some(parent) = p.parent()
            && !paths::is_hidden(name)
        {
            if self.config_dirs.contains(parent)
                && !self.by_path.contains_key(&p)
                && (what == What::Dir || (what == What::Written && is_module_name(&p)))
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

    fn forget_watch(&mut self, dir: &Path) {
        if self.watched.remove(dir) == Some(Mode::Inotify) {
            self.backend.unwatch(dir);
        }
        self.snaps.remove(dir);
    }

    /// Compare every polled directory with its last listing, and mark the
    /// files watched through it for a content comparison.
    pub(crate) fn poll(&mut self, now: Instant) {
        let dirs: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|(_, m)| **m == Mode::Poll)
            .map(|(d, _)| d.clone())
            .collect();
        for dir in dirs {
            let Some(new) = snapshot(&dir) else {
                self.on_path(dir, What::Gone, now);
                continue;
            };
            let old = self
                .snaps
                .insert(dir.clone(), new.clone())
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
            for (name, o) in &old {
                if !new.contains_key(name) {
                    let what = if o.dir { What::Dir } else { What::Gone };
                    self.on_path(dir.join(name), what, now);
                }
            }
            // Content comparison: attribute caches on network filesystems
            // can hide a change from the listing, never from the bytes.
            let mut any = false;
            for (f, e) in &self.files {
                if e.exists && e.resolved.watch_dirs().any(|d| d == dir) {
                    self.pending.files.insert(f.clone());
                    any = true;
                }
            }
            if any {
                self.mark(now);
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
        self.sync_watches();
        notices.append(&mut self.pending.notices);
        for path in &touched {
            if let Some(c) = self.check(path) {
                changes.push(c);
            }
        }
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
        self.own
            .retain(|o| now.duration_since(o.at) < OWN_WRITE_TTL);
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes.dedup_by(|a, b| a.path == b.path && a.role == b.role);
        if changes.is_empty() && notices.is_empty() && p.full.is_none() {
            return None;
        }
        Some(FileBatch {
            changes,
            rescan: p.full,
            notices,
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
        let gone: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|(p, e)| e.role == Role::Module && !new.contains(*p))
            .map(|(p, _)| p.clone())
            .collect();
        for path in gone {
            touched.remove(&path);
            if let Some(e) = self.files.remove(&path)
                && e.exists
            {
                changes.push(FileChange {
                    path,
                    canonical: None,
                    kind: ChangeKind::Removed,
                    hash: None,
                    role: Role::Module,
                    error: None,
                });
            }
        }
        for path in new {
            if !self.files.contains_key(&path) {
                self.files.insert(
                    path.clone(),
                    Entry {
                        role: Role::Module,
                        resolved: paths::resolve(&path),
                        hash: None,
                        exists: false,
                    },
                );
                touched.insert(path);
            }
        }
        cfg.modules.files = set.files;
    }

    fn take_own_write(&mut self, path: &Path, canonical: &Path, hash: ContentHash) -> bool {
        match self
            .own
            .iter()
            .position(|o| o.hash == hash && (o.path == path || o.canonical == canonical))
        {
            Some(i) => {
                self.own.swap_remove(i);
                true
            }
            None => false,
        }
    }

    fn check(&mut self, path: &Path) -> Option<FileChange> {
        let e = self.files.get(path)?;
        let was = e.exists;
        let old = e.hash;
        let role = e.role;
        let canonical = e.resolved.path.clone();
        let (kind, hash, error) = match read_hash(path) {
            Ok(h) => {
                let own = self.take_own_write(path, &canonical, h);
                let e = self.files.get_mut(path)?;
                e.exists = true;
                e.hash = Some(h);
                if own || (was && old == Some(h)) {
                    return None;
                }
                let kind = if was {
                    ChangeKind::Modified
                } else {
                    ChangeKind::Created
                };
                (kind, Some(h), None)
            }
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    || err.kind() == io::ErrorKind::NotADirectory =>
            {
                let e = self.files.get_mut(path)?;
                e.exists = false;
                e.hash = None;
                if !was {
                    return None;
                }
                (ChangeKind::Removed, None, None)
            }
            Err(err) => {
                let e = self.files.get_mut(path)?;
                e.exists = true;
                e.hash = None;
                let kind = if was {
                    ChangeKind::Modified
                } else {
                    ChangeKind::Created
                };
                (kind, None, Some(err.kind()))
            }
        };
        Some(FileChange {
            path: path.to_path_buf(),
            canonical: (kind != ChangeKind::Removed).then_some(canonical),
            kind,
            hash,
            role,
            error,
        })
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
                if e.role == Role::Module && hop.is_dir() {
                    self.rescan_paths.insert(hop.clone());
                }
            }
        }
        if let Some(root) = &self.config_root {
            self.rescan_paths.extend(root.hops.iter().cloned());
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
        }
        for e in self.files.values() {
            dirs.extend(e.resolved.watch_dirs().map(Path::to_path_buf));
        }
        for t in &self.trees {
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

    /// Bring the backend's watches in line with what the state needs.
    fn sync_watches(&mut self) {
        self.reindex();
        let desired = self.desired_dirs();
        let stale: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|d| !desired.contains(*d))
            .cloned()
            .collect();
        for d in stale {
            self.forget_watch(&d);
        }
        for d in desired {
            if self.watched.contains_key(&d) || !d.is_dir() {
                continue;
            }
            let poll = |reason| (Mode::Poll, Some(reason));
            let (mode, reason) = match paths::fs_kind(&d) {
                FsKind::ReadOnly => (Mode::Skip, None),
                _ if self.opts.force_polling => poll(PollReason::Forced),
                FsKind::NoEvents => poll(PollReason::NoEventsFilesystem),
                FsKind::Local => match self.backend.watch(&d) {
                    Ok(()) => (Mode::Inotify, None),
                    Err(e) => poll(PollReason::WatchFailed(e)),
                },
            };
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
            self.watched.insert(d, mode);
        }
    }

    /// Notices raised outside a flush (a new watch fell back to polling)
    /// go out in a batch of their own.
    fn mark_if_notices(&mut self) {
        if !self.pending.notices.is_empty() && self.pending.first.is_none() {
            self.mark(Instant::now());
        }
    }

    #[cfg(test)]
    pub(crate) fn watched_dirs(&self) -> Vec<PathBuf> {
        self.watched.keys().cloned().collect()
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
        fn watch(&mut self, dir: &Path) -> Result<(), String> {
            self.0.lock().unwrap().insert(dir.to_path_buf());
            Ok(())
        }
        fn unwatch(&mut self, dir: &Path) {
            self.0.lock().unwrap().remove(dir);
        }
    }

    /// A backend whose every watch fails (inotify limit reached).
    struct Full;

    impl Backend for Full {
        fn watch(&mut self, _: &Path) -> Result<(), String> {
            Err("inotify watch limit reached".into())
        }
        fn unwatch(&mut self, _: &Path) {}
    }

    #[test]
    fn a_failed_watch_falls_back_to_polling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        let mut core = Core::new(Full, Options::default(), Some(config(&root)));
        assert!(core.has_polled_dirs());
        let t0 = Instant::now();
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(
            b.notices,
            vec![Notice::Polling {
                dir: root.clone(),
                reason: PollReason::WatchFailed("inotify watch limit reached".into()),
            }]
        );
        // Same size, same mtime second: only a content comparison sees it.
        std::fs::write(root.join("a.strand"), "b").unwrap();
        core.poll(t0);
        let b = core.flush(core.deadline().unwrap()).unwrap();
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"b")));
        // Nothing changed: a poll produces no batch.
        core.poll(t0);
        assert!(core.flush(core.deadline().unwrap()).is_none());
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
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("bar.strand"), "bar").unwrap();
        std::fs::write(root.join("theme.strand"), "theme").unwrap();
        std::fs::write(root.join("old.strand"), "old").unwrap();
        std::fs::write(root.join("prefs.toml"), "a = 1").unwrap();
        let backend = Silent::default();
        let mut core = Core::new(backend.clone(), Options::default(), Some(config(&root)));
        core.add_file(&root.join("prefs.toml"), Role::Settings);
        assert!(backend.0.lock().unwrap().contains(&root));

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
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("a.strand"), "a").unwrap();
        let mut core = Core::new(Silent::default(), Options::default(), Some(config(&root)));
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
        core.add_file(&root.join("cfg/theme.strand"), Role::Other);
        assert_eq!(
            core.watched_dirs(),
            vec![root.join("cfg"), root.join("store")]
        );
    }
}

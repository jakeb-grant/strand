//! `state x = … persist`: values kept across restarts.
//!
//! Each persisted cell is one small file under `$XDG_STATE_HOME/strand/
//! persist/` (or `~/.local/state/strand/persist/`), named by the cell's path
//! (`launcher.query`, `bar.dnd`: the `file.name` path `export` uses, plus
//! the instance identity when a component has several instances,
//! `bar[<monitor>].expanded`).
//! The file stores the value's bytes together with a hash of the default
//! it was declared with, so a changed default can be noticed ("Persistence
//! and settings" in the design):
//!
//! * nothing stored: the default;
//! * stored under the same default: the stored value;
//! * the default changed and the stored value *was* the old default (never
//!   changed by the user): the new default is adopted;
//! * the default changed and the user had changed the value: it is kept and
//!   reported once ([`Diagnostic::PersistDefaultChanged`], the overlay's
//!   `launcher.query: kept "fir" (default changed) [reset]`).
//!
//! Values are opaque bytes: the VM encodes and decodes its own values
//! ([`Runtime::persisted`] takes the codec). A file that cannot be read back
//! (bad header, checksum mismatch, a value that no longer decodes because
//! the type changed) is moved aside to `.<name>.corrupt` and the default is
//! used, with [`Diagnostic::PersistFailed`]. Writes are atomic (temp file,
//! `fsync`, rename, directory `fsync`), so a crash never leaves a torn
//! file, done on the store's IO thread (never on the logic tick), and
//! debounced by [`PERSIST_DEBOUNCE`] of logic time; the cell's live value
//! is queued when its owner is disposed (unmount), at shutdown and when the
//! last runtime handle is dropped, so a write made in the same tick as the
//! unmount is not lost. [`Persisted::redeclare`] follows a default changed
//! by a live reload and [`Persisted::reset`] is `@reset`.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use crate::error::Error;
use crate::runtime::{Diagnostic, NodeId, Runtime};
use crate::signal::Signal;

/// Quiet time after the last change before a persisted value is written
/// (a slider drag writes once, when it stops).
pub const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);

/// First line of every persisted file.
const MAGIC: &str = "strand-persist 1";

/// Longest file name used as is; longer paths are shortened with a hash.
const MAX_NAME: usize = 200;

/// Why persisted storage failed. Comparable and cheap to clone, so it can
/// sit in a [`Diagnostic`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistError {
    /// Neither `$XDG_STATE_HOME` nor `$HOME` is set.
    NoStateDir,
    /// The cell path is empty.
    EmptyPath,
    /// Reading, writing or renaming failed.
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The OS error.
        message: Arc<str>,
    },
    /// The file exists but is not a valid persisted value (moved aside to
    /// `.<name>.corrupt`).
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        reason: Arc<str>,
    },
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStateDir => f.write_str("neither XDG_STATE_HOME nor HOME is set"),
            Self::EmptyPath => f.write_str("empty persist path"),
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Corrupt { path, reason } => {
                write!(f, "{}: corrupt persisted value ({reason})", path.display())
            }
        }
    }
}

impl std::error::Error for PersistError {}

pub(crate) fn io_error(path: &Path, e: &std::io::Error) -> PersistError {
    PersistError::Io {
        path: path.to_path_buf(),
        message: Arc::from(e.to_string()),
    }
}

/// A stable 64-bit hash (FNV-1a) of a value's bytes: the same in every
/// build, so a hash written by one version is understood by the next.
pub fn value_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A value read back from the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// The value's bytes.
    pub value: Vec<u8>,
    /// [`value_hash`] of the default it was saved under.
    pub default_hash: u64,
}

/// What [`PersistStore::restore`] decided for a cell at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restore {
    /// Nothing stored: the default.
    Default,
    /// The stored value, saved under the same default.
    Stored(Vec<u8>),
    /// The default changed and the stored value was the old default: the
    /// new default is adopted (the stale file is removed).
    Adopted,
    /// The default changed but the stored value had been changed: it is
    /// kept, and the file is re-stamped with the new default so this is
    /// reported once.
    KeptOverNewDefault(Vec<u8>),
    /// The file could not be read back: the default (a corrupt file was
    /// moved aside).
    Failed(PersistError),
}

/// Where persisted values live. Cheap to clone: clones share one IO
/// thread.
///
/// Writes, removals and quarantines are queued to a persist IO thread
/// (started on first use), coalesced to the latest operation per file, so
/// the logic thread never waits for `fsync` on a slow or networked disk.
/// [`PersistStore::load`] sees queued operations before the disk, so a
/// value written by an unmounting component is read back by its
/// replacement even before it reaches the disk. [`PersistStore::sync`]
/// waits for the queue to drain ([`Runtime::shutdown`] does, bounded);
/// dropping the last clone drains the queue (bounded) and joins the
/// thread.
#[derive(Clone)]
pub struct PersistStore {
    inner: Arc<StoreInner>,
}

impl fmt::Debug for PersistStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistStore")
            .field("dir", &self.inner.shared.dir)
            .finish()
    }
}

impl PartialEq for PersistStore {
    fn eq(&self, other: &Self) -> bool {
        self.inner.shared.dir == other.inner.shared.dir
    }
}

impl Eq for PersistStore {}

/// How long [`Runtime::shutdown`] (and dropping a store) waits for queued
/// writes before giving up on them.
pub const PERSIST_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// Temp files of other processes older than this are removed.
const STALE_TEMP: Duration = Duration::from_secs(60);

type IoHook = Arc<dyn Fn(&Path) + Send + Sync>;
type WriteObserver = Arc<dyn Fn(&OwnWrite<'_>) + Send + Sync>;

/// A file the store's IO thread is about to change: what
/// [`PersistStore::on_written`] observers receive, so the watcher can
/// pre-register the hash of Strand's own writes ("Live reload", step 2 in
/// the design) and stop at its no-op check instead of reloading them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnWrite<'a> {
    /// The file as queued: a persisted cell's file, a settings file's
    /// declared path, a settings overlay or a last-good snapshot.
    pub path: &'a Path,
    /// The file actually replaced: `path` with symlinks followed, as an
    /// absolute path with a canonical directory (the same file as `path`
    /// when it is not a link).
    pub target: &'a Path,
    /// The complete new content of `target`, byte for byte; `None` when
    /// the file is removed (or moved aside as corrupt).
    pub content: Option<&'a [u8]>,
}

/// Calls the store's observers (none set: nothing).
pub(crate) struct Observers<'a>(Option<&'a WriteObserver>);

impl Observers<'_> {
    pub(crate) fn report(&self, path: &Path, target: &Path, content: Option<&[u8]>) {
        let Some(f) = self.0 else {
            return;
        };
        // The target's directory made canonical (the file itself may not
        // exist yet).
        let canonical = match (target.parent(), target.file_name()) {
            (Some(dir), Some(name)) => fs::canonicalize(dir).ok().map(|d| d.join(name)),
            _ => None,
        };
        f(&OwnWrite {
            path,
            target: canonical.as_deref().unwrap_or(target),
            content,
        });
    }
}

struct StoreInner {
    shared: Arc<Shared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for StoreInner {
    /// Drain queued writes (bounded) and join the IO thread.
    fn drop(&mut self) {
        let drained = self.shared.wait_idle(PERSIST_SHUTDOWN_WAIT);
        self.shared.lock().stop = true;
        self.shared.changed.notify_all();
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let (true, Some(h)) = (drained, handle) {
            let _ = h.join();
        }
    }
}

/// State shared with the IO thread.
struct Shared {
    dir: PathBuf,
    queue: Mutex<Queue>,
    changed: Condvar,
    /// Called before every queued operation (tests: a slow disk).
    io_hook: Option<IoHook>,
    /// [`PersistStore::on_written`].
    observer: Mutex<Option<WriteObserver>>,
}

#[derive(Default)]
struct Queue {
    /// Pending operations, at most one per file, oldest first.
    ops: Vec<(PathBuf, Job)>,
    /// The operation the IO thread is performing.
    in_flight: Option<(PathBuf, Op)>,
    stop: bool,
    /// Old temp files were swept (once per store).
    swept: bool,
    /// Per settings key, the highest job sequence number done (written or
    /// failed): a reload trusts what it read only for fields whose last
    /// edit is at most this ([`crate::settings`]).
    settings_landed: std::collections::HashMap<PathBuf, u64>,
}

struct Job {
    op: Op,
    report: Option<Reporter>,
}

#[derive(Clone)]
enum Op {
    Write {
        default_hash: u64,
        value: Arc<[u8]>,
    },
    Remove,
    /// Move the file aside to `.<name>.corrupt`.
    Quarantine,
    /// Per-field edits of a settings file (or its overlay), applied with
    /// `toml_edit` to what the file holds when the IO thread gets to it.
    Settings(crate::settings::SettingsJob),
}

/// Where a queued operation reports its failure: the runtime drains it in
/// its next flush as [`Diagnostic::PersistFailed`].
#[derive(Clone)]
pub(crate) struct Reporter {
    cell: NodeId,
    path: Arc<str>,
    sink: Arc<FailSink>,
}

/// Failures from the IO thread, for one runtime.
pub(crate) struct FailSink {
    list: Mutex<Vec<Diagnostic>>,
    /// `list` is not empty: every flush checks without locking.
    any: std::sync::atomic::AtomicBool,
    ready: Arc<crate::task::ReadyQueue>,
}

impl FailSink {
    pub(crate) fn new(ready: Arc<crate::task::ReadyQueue>) -> Self {
        Self {
            list: Mutex::new(Vec::new()),
            any: std::sync::atomic::AtomicBool::new(false),
            ready,
        }
    }
    /// Failures are waiting to be reported (lock-free).
    pub(crate) fn is_pending(&self) -> bool {
        self.any.load(Ordering::Acquire)
    }
    pub(crate) fn take(&self) -> Vec<Diagnostic> {
        if !self.any.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut list = self.list.lock().unwrap_or_else(PoisonError::into_inner);
        self.any.store(false, Ordering::Release);
        std::mem::take(&mut *list)
    }
    pub(crate) fn push(&self, d: Diagnostic) {
        {
            let mut list = self.list.lock().unwrap_or_else(PoisonError::into_inner);
            list.push(d);
            self.any.store(true, Ordering::Release);
        }
        // Wake the logic thread so the next flush reports it.
        self.ready.call_hook();
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until nothing is queued or in flight; false on timeout.
    fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut q = self.lock();
        while !q.ops.is_empty() || q.in_flight.is_some() {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            q = self
                .changed
                .wait_timeout(q, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// The IO thread.
    fn run(&self) {
        loop {
            let (file, job, sweep) = {
                let mut q = self.lock();
                loop {
                    if !q.ops.is_empty() {
                        let (file, job) = q.ops.remove(0);
                        q.in_flight = Some((file.clone(), job.op.clone()));
                        let sweep = !std::mem::replace(&mut q.swept, true);
                        break (file, job, sweep);
                    }
                    if q.stop {
                        return;
                    }
                    q = self.changed.wait(q).unwrap_or_else(PoisonError::into_inner);
                }
            };
            if sweep {
                sweep_temps(&self.dir, None);
            }
            if let Some(hook) = &self.io_hook {
                hook(&file);
            }
            let r = self.perform(&file, &job.op);
            let settings_seq = match &job.op {
                Op::Settings(j) => Some(j.seq),
                _ => None,
            };
            // Reported before the operation counts as done, so a `sync`
            // that returns has its failures in the runtime.
            if let (Err(error), Some(rep)) = (r, job.report) {
                rep.sink.push(Diagnostic::PersistFailed {
                    cell: rep.cell,
                    path: rep.path,
                    error,
                });
            }
            {
                let mut q = self.lock();
                if let Some(seq) = settings_seq {
                    let landed = q.settings_landed.entry(file).or_insert(0);
                    *landed = (*landed).max(seq);
                }
                q.in_flight = None;
            }
            self.changed.notify_all();
        }
    }

    fn perform(&self, file: &Path, op: &Op) -> Result<(), PersistError> {
        let observer = self
            .observer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let observe = &Observers(observer.as_ref());
        match op {
            Op::Write {
                default_hash,
                value,
            } => write_file(&self.dir, file, *default_hash, value, observe),
            Op::Remove => {
                if !file.exists() {
                    return Ok(());
                }
                observe.report(file, file, None);
                match fs::remove_file(file) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(io_error(file, &e)),
                }
            }
            Op::Quarantine => {
                observe.report(file, file, None);
                quarantine(file);
                Ok(())
            }
            // Reports its own outcome (notices and failures).
            Op::Settings(job) => {
                crate::settings::perform(file, job, observe);
                Ok(())
            }
        }
    }
}

/// Distinct temp names within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl PersistStore {
    /// A store keeping its files in `dir` (created on the first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::build(dir.into(), None)
    }

    /// A store whose IO thread calls `hook` with the file before every
    /// queued operation: tests simulate a slow disk with it.
    #[doc(hidden)]
    pub fn with_io_hook(
        dir: impl Into<PathBuf>,
        hook: impl Fn(&Path) + Send + Sync + 'static,
    ) -> Self {
        Self::build(dir.into(), Some(Arc::new(hook)))
    }

    fn build(dir: PathBuf, io_hook: Option<IoHook>) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                shared: Arc::new(Shared {
                    dir,
                    queue: Mutex::new(Queue::default()),
                    changed: Condvar::new(),
                    io_hook,
                    observer: Mutex::new(None),
                }),
                worker: Mutex::new(None),
            }),
        }
    }

    /// Observe every file this store's IO thread writes or removes
    /// (persisted cells, settings files, overlays, last-good snapshots):
    /// `f` runs on the IO thread with the complete new content once it is
    /// on disk under a temp name and *before* the rename that makes it
    /// visible, so a watcher that registers the content's hash here sees
    /// the hash before the change event (design, "Live reload" step 2:
    /// Strand's own writes are pre-registered). If the rename then fails,
    /// the bytes never appear and the failure is reported as usual. One
    /// observer per store; a later call replaces it. Keep `f` short: the
    /// IO thread waits for it.
    pub fn on_written(&self, f: impl Fn(&OwnWrite<'_>) + Send + Sync + 'static) {
        *self
            .inner
            .shared
            .observer
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(f));
    }

    /// `$XDG_STATE_HOME/strand/persist`, or `$HOME/.local/state/strand/
    /// persist` when `XDG_STATE_HOME` is unset or not absolute (as the XDG
    /// spec says to ignore relative paths).
    pub fn from_env() -> Result<Self, PersistError> {
        Self::from_vars(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
    }

    /// [`PersistStore::from_env`] with the variables given.
    pub fn from_vars(
        xdg_state_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    ) -> Result<Self, PersistError> {
        let state = xdg_state_home
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                home.map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|h| h.join(".local/state"))
            })
            .ok_or(PersistError::NoStateDir)?;
        Ok(Self::new(state.join("strand").join("persist")))
    }

    /// The directory holding the files.
    pub fn dir(&self) -> &Path {
        &self.inner.shared.dir
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// The file for a cell path. Bytes outside `[A-Za-z0-9_.-]` (and a
    /// leading `.`) are percent-escaped, so a path can never name a file
    /// outside the directory, a hidden file or a temp file; very long
    /// paths are shortened with their hash.
    pub fn file_of(&self, path: &str) -> Result<PathBuf, PersistError> {
        if path.is_empty() {
            return Err(PersistError::EmptyPath);
        }
        Ok(self.dir().join(escape_name(path)))
    }

    /// A settings store sharing this store's IO thread, keeping overlays
    /// for read-only settings files in `settings/` next to this store's
    /// directory (`$XDG_STATE_HOME/strand/settings`).
    pub fn settings(&self) -> crate::settings::SettingsStore {
        let dir = self
            .dir()
            .parent()
            .map_or_else(|| self.dir().join("settings"), |p| p.join("settings"));
        crate::settings::SettingsStore::sharing(self.clone(), dir)
    }

    /// Queue settings edits for `key` (a settings file, or its overlay),
    /// merged into edits already queued for it (a later edit of a field
    /// replaces an earlier one).
    pub(crate) fn enqueue_settings(&self, key: PathBuf, job: crate::settings::SettingsJob) {
        let shared = &self.inner.shared;
        {
            let mut q = shared.lock();
            let queued = q.ops.iter_mut().find_map(|(f, j)| match &mut j.op {
                Op::Settings(old) if *f == key => Some(old),
                _ => None,
            });
            match queued {
                Some(old) => old.merge(job),
                None => q.ops.push((
                    key,
                    Job {
                        op: Op::Settings(job),
                        report: None,
                    },
                )),
            }
        }
        self.ensure_worker();
        shared.changed.notify_all();
    }

    /// For a settings reload, under one lock: the highest job sequence
    /// number done for `key`, and the edits not yet on disk (in flight
    /// first, then queued), so the reload sees what the file is about to
    /// hold. Taken *before* the file is read: an edit that lands in
    /// between is then both in the file and in the list, and applying it
    /// twice changes nothing.
    pub(crate) fn settings_mark(&self, key: &Path) -> (u64, Vec<crate::settings::Edit>) {
        let q = self.inner.shared.lock();
        let landed = q.settings_landed.get(key).copied().unwrap_or(0);
        let in_flight = q
            .in_flight
            .iter()
            .filter(|(f, _)| f == key)
            .map(|(_, op)| op);
        let queued = q.ops.iter().filter(|(f, _)| f == key).map(|(_, j)| &j.op);
        let edits = in_flight
            .chain(queued)
            .filter_map(|op| match op {
                Op::Settings(job) => Some(job.edits.iter().cloned()),
                _ => None,
            })
            .flatten()
            .collect();
        (landed, edits)
    }

    /// Read a stored value. `Ok(None)` when nothing is stored; a file that
    /// is not a valid persisted value is moved aside to `.<name>.corrupt`
    /// and reported as [`PersistError::Corrupt`]. Operations still queued
    /// for the file count: the latest one is what the file will hold.
    pub fn load(&self, path: &str) -> Result<Option<Stored>, PersistError> {
        let file = self.file_of(path)?;
        {
            let q = self.inner.shared.lock();
            let queued = q
                .ops
                .iter()
                .rev()
                .find(|(f, _)| *f == file)
                .map(|(_, j)| &j.op)
                .or(q
                    .in_flight
                    .as_ref()
                    .filter(|(f, _)| *f == file)
                    .map(|(_, op)| op));
            match queued {
                Some(Op::Write {
                    default_hash,
                    value,
                }) => {
                    return Ok(Some(Stored {
                        value: value.to_vec(),
                        default_hash: *default_hash,
                    }));
                }
                Some(Op::Remove | Op::Quarantine) => return Ok(None),
                Some(Op::Settings(_)) | None => {}
            }
        }
        let bytes = match fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(&file, &e)),
        };
        match parse(&bytes) {
            Ok(stored) => Ok(Some(stored)),
            Err(reason) => {
                // Best effort: the default is used either way, and the next
                // write replaces the file.
                quarantine(&file);
                Err(PersistError::Corrupt {
                    path: file,
                    reason: Arc::from(reason),
                })
            }
        }
    }

    /// Store `value` for `path`, stamped with the hash of `default`, now
    /// (on the calling thread, after anything queued for the file).
    /// Atomic: a reader sees the old file or the new one, never a mix.
    /// Persisted cells write through the IO thread instead.
    pub fn save(&self, path: &str, default: &[u8], value: &[u8]) -> Result<(), PersistError> {
        let file = self.file_of(path)?;
        self.now(
            &file,
            &Op::Write {
                default_hash: value_hash(default),
                value: Arc::from(value),
            },
        )
    }

    /// Forget the stored value now (tools; a persisted cell's `@reset` is
    /// [`Persisted::reset`]).
    pub fn remove(&self, path: &str) -> Result<(), PersistError> {
        let file = self.file_of(path)?;
        self.now(&file, &Op::Remove)
    }

    /// Wait until every queued write has reached the disk, at most
    /// `timeout`. Returns false if the queue did not drain in time.
    pub fn sync(&self, timeout: Duration) -> bool {
        self.inner.shared.wait_idle(timeout)
    }

    /// Perform `op` on the calling thread, replacing what is queued for
    /// `file` and after an in-flight operation on it.
    fn now(&self, file: &Path, op: &Op) -> Result<(), PersistError> {
        let shared = &self.inner.shared;
        let mut q = shared.lock();
        q.ops.retain(|(f, _)| f != file);
        while q.in_flight.as_ref().is_some_and(|(f, _)| f == file) {
            q = shared
                .changed
                .wait(q)
                .unwrap_or_else(PoisonError::into_inner);
        }
        // Holding the lock keeps the IO thread off this file meanwhile.
        let r = shared.perform(file, op);
        drop(q);
        shared.changed.notify_all();
        r
    }

    /// Queue `op` for `file`, replacing what is queued for it.
    fn enqueue(&self, file: PathBuf, op: Op, report: Option<Reporter>) {
        let shared = &self.inner.shared;
        {
            let mut q = shared.lock();
            q.ops.retain(|(f, _)| *f != file);
            q.ops.push((file, Job { op, report }));
        }
        self.ensure_worker();
        shared.changed.notify_all();
    }

    fn ensure_worker(&self) {
        let mut worker = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if worker.is_some() {
            return;
        }
        let shared = self.inner.shared.clone();
        match std::thread::Builder::new()
            .name("strand-persist".into())
            .spawn(move || shared.run())
        {
            Ok(h) => *worker = Some(h),
            // No thread: do the work here rather than lose it.
            Err(_) => {
                let shared = &self.inner.shared;
                let jobs = std::mem::take(&mut shared.lock().ops);
                for (file, job) in jobs {
                    if let (Err(error), Some(rep)) = (shared.perform(&file, &job.op), job.report) {
                        rep.sink.push(Diagnostic::PersistFailed {
                            cell: rep.cell,
                            path: rep.path,
                            error,
                        });
                    }
                }
            }
        }
    }

    /// Decide a cell's starting value from what is stored and its declared
    /// `default` (encoded). See [`Restore`]. Removing an adopted file and
    /// re-stamping a kept one are queued.
    pub fn restore(&self, path: &str, default: &[u8]) -> Restore {
        self.restore_reporting(path, default, None)
    }

    fn restore_reporting(&self, path: &str, default: &[u8], report: Option<Reporter>) -> Restore {
        let stored = match self.load(path) {
            Ok(None) => return Restore::Default,
            Ok(Some(s)) => s,
            Err(e) => return Restore::Failed(e),
        };
        let new_default = value_hash(default);
        if stored.default_hash == new_default {
            return Restore::Stored(stored.value);
        }
        let Ok(file) = self.file_of(path) else {
            return Restore::Failed(PersistError::EmptyPath);
        };
        if value_hash(&stored.value) == stored.default_hash || stored.value == default {
            // Never changed from the old default, or already the new one:
            // take the new default (as `Persisted::redeclare` does). The
            // stale file would only say the same again next time.
            self.enqueue(file, Op::Remove, report);
            return Restore::Adopted;
        }
        // Kept; re-stamp so the change of default is reported once.
        self.enqueue(
            file,
            Op::Write {
                default_hash: new_default,
                value: Arc::from(&stored.value[..]),
            },
            report,
        );
        Restore::KeptOverNewDefault(stored.value)
    }
}

/// A path as one file name: bytes outside `[A-Za-z0-9_.-]` (and a leading
/// `.`) percent-escaped, very long names shortened with their hash.
pub(crate) fn escape_name(path: &str) -> String {
    let mut name = String::with_capacity(path.len());
    for (i, b) in path.bytes().enumerate() {
        let plain = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' && i > 0;
        if plain {
            name.push(char::from(b));
        } else {
            name.push_str(&format!("%{b:02X}"));
        }
    }
    if name.len() > MAX_NAME {
        let cut = (0..=MAX_NAME - 20)
            .rev()
            .find(|&i| name.is_char_boundary(i))
            .unwrap_or(0);
        name = format!("{}~{:016x}", &name[..cut], value_hash(path.as_bytes()));
    }
    name
}

/// A fresh temp name next to `file`: `.<name>.tmp.<pid>.<n>`.
pub(crate) fn temp_next_to(dir: &Path, file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `value` to `file` atomically: temp file, `fsync`, rename,
/// directory `fsync`.
fn write_file(
    dir: &Path,
    file: &Path,
    default_hash: u64,
    value: &[u8],
    observe: &Observers<'_>,
) -> Result<(), PersistError> {
    create_private_dir(dir)?;
    let mut body = format!(
        "{MAGIC}\ndefault {default_hash:016x}\ncheck {:016x}\n\n",
        value_hash(value)
    )
    .into_bytes();
    body.extend_from_slice(value);
    let temp = temp_next_to(dir, file);
    let written = (|| {
        let mut f = fs::File::create(&temp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        observe.report(file, file, Some(&body));
        fs::rename(&temp, file)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(io_error(file, &e));
    }
    // Make the rename itself durable; failing here loses nothing that is
    // not already on its way to disk.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Where [`quarantine`] moves `file`: `.<name>.corrupt`, a name
/// [`PersistStore::file_of`] never produces (it escapes a leading `.`), so
/// no cell path reads another cell's quarantined bytes.
pub(crate) fn quarantine_path(file: &Path) -> PathBuf {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    file.with_file_name(format!(".{name}.corrupt"))
}

/// Move `file` aside to `.<name>.corrupt` (best effort).
pub(crate) fn quarantine(file: &Path) {
    let _ = fs::rename(file, quarantine_path(file));
}

/// Remove temp files (`.<name>.tmp.<pid>.<n>`) a crash left between create
/// and rename: those of processes that are gone, or older than a minute
/// and not ours. `only`: just the temp files of that file name (a settings
/// file's directory is the user's).
pub(crate) fn sweep_temps(dir: &Path, only: Option<&str>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix('.') else {
            continue;
        };
        let Some((of, tail)) = rest.rsplit_once(".tmp.") else {
            continue;
        };
        if only.is_some_and(|o| o != of) {
            continue;
        }
        // Exactly `<pid>.<n>`: a quarantined `.<name>.corrupt` is kept.
        let Some((pid, n)) = tail.split_once('.') else {
            continue;
        };
        let (Ok(pid), true) = (pid.parse::<u32>(), n.parse::<u64>().is_ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let gone = !Path::new(&format!("/proc/{pid}")).exists();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_TEMP);
        if gone || old {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Create `dir` (and parents) readable only by the user.
pub(crate) fn create_private_dir(dir: &Path) -> Result<(), PersistError> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io_error(dir, &e))
}

/// Parse a persisted file: magic, `default <hex>`, `check <hex>`, a blank
/// line, then the value bytes.
fn parse(bytes: &[u8]) -> Result<Stored, String> {
    let mut rest = bytes;
    let mut line = || -> Result<&str, String> {
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("truncated header")?;
        let l = std::str::from_utf8(&rest[..end]).map_err(|_| "header is not text")?;
        rest = &rest[end + 1..];
        Ok(l)
    };
    if line()? != MAGIC {
        return Err("not a strand persist file".into());
    }
    let hex = |l: &str, key: &str| -> Result<u64, String> {
        l.strip_prefix(key)
            .and_then(|h| h.strip_prefix(' '))
            .filter(|h| h.len() == 16)
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .ok_or_else(|| format!("bad `{key}` line"))
    };
    let default_hash = hex(line()?, "default")?;
    let check = hex(line()?, "check")?;
    if !line()?.is_empty() {
        return Err("missing blank line after the header".into());
    }
    if value_hash(rest) != check {
        return Err("checksum mismatch".into());
    }
    Ok(Stored {
        value: rest.to_vec(),
        default_hash,
    })
}

/// A persisted cell: [`Runtime::persisted`]. Keep it to follow live
/// reloads ([`Persisted::redeclare`]) and `@reset` ([`Persisted::reset`]).
pub struct Persisted<T> {
    /// The state cell (an ordinary [`Signal`]).
    pub signal: Signal<T>,
    /// How its starting value was chosen.
    pub restored: Restore,
    writer: Rc<Writer<T>>,
}

impl<T> fmt::Debug for Persisted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Persisted")
            .field("signal", &self.signal)
            .field("path", &self.writer.path)
            .field("restored", &self.restored)
            .finish()
    }
}

/// What [`Persisted::redeclare`] did with a changed default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Redeclared {
    /// The default did not change.
    Unchanged,
    /// The value still held the old default: it took the new one.
    Adopted,
    /// The value had been changed: it is kept (and reported once).
    Kept,
}

type Encode<T> = Box<dyn Fn(&T) -> Vec<u8>>;
type Decode<T> = Box<dyn Fn(&[u8]) -> Option<T>>;

/// Who holds a persist file: the live cell that writes it and, oldest
/// first, the cells created on the same path while it was held (a
/// replacement mounted before the old instance went away). When the owner
/// is disposed, the oldest live waiter takes over.
pub(crate) struct PathSlot {
    owner: NodeId,
    waiting: Vec<Weak<dyn Waiter>>,
}

/// A persisted cell waiting for its path.
pub(crate) trait Waiter {
    fn cell(&self) -> NodeId;
    /// The path is now this cell's: take what the file holds (after the old
    /// owner's last write) or write its own value.
    fn promote(&self, rt: &Runtime);
    /// Read the cell's live value and queue it if the file does not hold
    /// it yet: at unmount and shutdown, when the tracking effect may not
    /// have seen the last write (the owner went in the same tick, or the
    /// effect is held by a frozen component).
    fn capture(&self, rt: &Runtime);
}

/// Write-behind for one persisted cell.
struct Writer<T> {
    store: PersistStore,
    path: Arc<str>,
    file: PathBuf,
    cell: NodeId,
    signal: Signal<T>,
    /// The declared default, its encoding and hash (changed by a reload).
    default: RefCell<(T, Vec<u8>, u64)>,
    encode: Encode<T>,
    decode: Decode<T>,
    /// The encoding of the value the cell started from: a waiting cell
    /// still holding it when promoted takes what the old owner left.
    start: Vec<u8>,
    /// What the file holds (or will, once the queue drains); the default
    /// when nothing is stored.
    baseline: RefCell<Vec<u8>>,
    /// Encoded value not yet queued.
    pending: RefCell<Option<Vec<u8>>>,
    /// False while another live cell owns the path: it does not write
    /// until it is promoted ([`PathSlot`]).
    active: Cell<bool>,
    report: Reporter,
}

impl<T> Writer<T> {
    /// The value changed: remember it unless the file already says it.
    fn note(&self, value: &T) {
        if !self.active.get() {
            return;
        }
        let bytes = (self.encode)(value);
        let fresh = *self.baseline.borrow() != bytes;
        *self.pending.borrow_mut() = fresh.then_some(bytes);
    }

    /// Queue the pending value, if any, for the IO thread.
    fn flush(&self) {
        let Some(bytes) = self.pending.borrow_mut().take() else {
            return;
        };
        let default_hash = self.default.borrow().2;
        self.store.enqueue(
            self.file.clone(),
            Op::Write {
                default_hash,
                value: Arc::from(&bytes[..]),
            },
            Some(self.report.clone()),
        );
        *self.baseline.borrow_mut() = bytes;
    }
}

impl<T: Clone + PartialEq + 'static> Waiter for Writer<T> {
    fn cell(&self) -> NodeId {
        self.cell
    }

    fn capture(&self, rt: &Runtime) {
        if let Ok(v) = self.signal.get_untracked(rt) {
            self.note(&v);
        }
        self.flush();
    }

    fn promote(&self, rt: &Runtime) {
        self.active.set(true);
        let default_bytes = self.default.borrow().1.clone();
        // The old owner's last value is queued by now; `restore` sees it
        // and applies the state-default rule to it.
        let restored =
            self.store
                .restore_reporting(&self.path, &default_bytes, Some(self.report.clone()));
        let file_bytes = match &restored {
            Restore::Stored(b) | Restore::KeptOverNewDefault(b) => b.clone(),
            Restore::Default | Restore::Adopted | Restore::Failed(_) => default_bytes.clone(),
        };
        match restored {
            Restore::KeptOverNewDefault(_) => rt.diagnose(Diagnostic::PersistDefaultChanged {
                cell: self.cell,
                path: self.path.clone(),
            }),
            Restore::Failed(error) => rt.diagnose(Diagnostic::PersistFailed {
                cell: self.cell,
                path: self.path.clone(),
                error,
            }),
            _ => {}
        }
        *self.baseline.borrow_mut() = file_bytes.clone();
        let Ok(live) = self.signal.get_untracked(rt) else {
            return;
        };
        let live_bytes = (self.encode)(&live);
        if live_bytes == self.start {
            // Untouched while it waited: it continues from the old owner.
            if live_bytes == file_bytes {
                return;
            }
            match (self.decode)(&file_bytes) {
                Some(v) if rt.check_write_allowed(self.cell).is_ok() => {
                    let _ = self.signal.set_raw(rt, v);
                    return;
                }
                // Inside a derived value's computation: keep its own value.
                Some(_) => {}
                None => {
                    self.store.enqueue(
                        self.file.clone(),
                        Op::Quarantine,
                        Some(self.report.clone()),
                    );
                    rt.diagnose(Diagnostic::PersistFailed {
                        cell: self.cell,
                        path: self.path.clone(),
                        error: PersistError::Corrupt {
                            path: self.file.clone(),
                            reason: Arc::from(
                                "the stored value does not decode as this cell's type",
                            ),
                        },
                    });
                    *self.baseline.borrow_mut() = default_bytes;
                }
            }
        }
        // Changed while it waited: its value wins and is written now.
        let fresh = *self.baseline.borrow() != live_bytes;
        *self.pending.borrow_mut() = fresh.then_some(live_bytes);
        self.flush();
    }
}

impl<T> Drop for Writer<T> {
    /// A runtime dropped without [`Runtime::shutdown`] still writes what
    /// was pending (the store drains its queue when it goes).
    fn drop(&mut self) {
        self.flush();
    }
}

impl<T: Clone + PartialEq + 'static> Persisted<T> {
    /// The persist path.
    pub fn path(&self) -> &str {
        &self.writer.path
    }

    /// Live reload changed the declared default (`state x = 40 persist`
    /// became `= 60`): the reconciler calls this instead of creating a new
    /// cell. The "state default" rule: a value that still holds the old
    /// default takes the new one (the stored file is removed: nothing
    /// stored means the default); a changed value is kept, reported once
    /// as [`Diagnostic::PersistDefaultChanged`] and re-stamped with the new
    /// default's hash, so the next start neither adopts it nor reports it
    /// again.
    pub fn redeclare(&self, rt: &Runtime, new_default: T) -> Result<Redeclared, Error> {
        let w = &self.writer;
        let new_bytes = (w.encode)(&new_default);
        let new_hash = value_hash(&new_bytes);
        let old_bytes = {
            let d = w.default.borrow();
            if d.2 == new_hash {
                return Ok(Redeclared::Unchanged);
            }
            d.1.clone()
        };
        let live = self.signal.get_untracked(rt)?;
        let live_bytes = (w.encode)(&live);
        *w.default.borrow_mut() = (new_default.clone(), new_bytes.clone(), new_hash);
        *w.pending.borrow_mut() = None;
        if live_bytes == old_bytes || live_bytes == new_bytes {
            *w.baseline.borrow_mut() = new_bytes;
            if w.active.get() {
                w.store
                    .enqueue(w.file.clone(), Op::Remove, Some(w.report.clone()));
            }
            self.signal.set(rt, new_default)?;
            return Ok(Redeclared::Adopted);
        }
        if w.active.get() {
            w.store.enqueue(
                w.file.clone(),
                Op::Write {
                    default_hash: new_hash,
                    value: Arc::from(&live_bytes[..]),
                },
                Some(w.report.clone()),
            );
        }
        *w.baseline.borrow_mut() = live_bytes;
        rt.diagnose(Diagnostic::PersistDefaultChanged {
            cell: w.cell,
            path: w.path.clone(),
        });
        Ok(Redeclared::Kept)
    }

    /// `@reset` (and the overlay's `[reset]`): forget the stored value and
    /// go back to the default. A write still pending or queued for the
    /// cell is cancelled, so it cannot bring the old value back.
    pub fn reset(&self, rt: &Runtime) -> Result<(), Error> {
        let w = &self.writer;
        let (default, bytes) = {
            let d = w.default.borrow();
            (d.0.clone(), d.1.clone())
        };
        *w.pending.borrow_mut() = None;
        *w.baseline.borrow_mut() = bytes;
        if w.active.get() {
            w.store
                .enqueue(w.file.clone(), Op::Remove, Some(w.report.clone()));
        }
        self.signal.set(rt, default)
    }
}

impl Runtime {
    /// `state x = default persist`: a [`Signal`] whose value survives
    /// restarts, stored under `path` in `store`. `encode`/`decode` are the
    /// VM's codec for the value (any stable byte encoding).
    ///
    /// `path` names one live cell: the cell's `file.name` path plus the
    /// identity of its instance when the component has several (`bar[<make
    /// model description>].expanded` for a `bar` on every monitor, the item
    /// key for state on list items). A second live cell on a path already
    /// in use is reported as [`Diagnostic::PersistPathInUse`] and does not
    /// write while the first cell owns the file; it starts from the stored
    /// value. When the owner is disposed the oldest waiting cell takes the
    /// path over (a replacement mounted before the old instance went
    /// away): if it still holds the value it started from it continues from
    /// what the old owner left (its last value is flushed first), otherwise
    /// its own value is written.
    ///
    /// The starting value follows [`PersistStore::restore`]; a kept value
    /// over a changed default reports [`Diagnostic::PersistDefaultChanged`],
    /// and a file that cannot be read (or a value that no longer decodes,
    /// such as after a type change, which is then moved aside to
    /// `.<name>.corrupt`) starts from the default and reports
    /// [`Diagnostic::PersistFailed`]. Changes are queued for the store's IO
    /// thread [`PERSIST_DEBOUNCE`] after the last one (logic time, so the
    /// host's `tick` drives it); the live value (also one written in the
    /// same tick) is queued when the current owner is disposed, at
    /// [`Runtime::shutdown`] (which waits for the queue, bounded by
    /// [`PERSIST_SHUTDOWN_WAIT`]) and when the last runtime handle is
    /// dropped. Write failures arrive as
    /// [`Diagnostic::PersistFailed`] in a later tick.
    pub fn persisted<T, E, D>(
        &self,
        store: &PersistStore,
        path: &str,
        default: T,
        encode: E,
        decode: D,
    ) -> Persisted<T>
    where
        T: Clone + PartialEq + 'static,
        E: Fn(&T) -> Vec<u8> + 'static,
        D: Fn(&[u8]) -> Option<T> + 'static,
    {
        let path: Arc<str> = Arc::from(path);
        let default_bytes = encode(&default);
        let signal = self.signal(default.clone());
        self.set_name(signal.id(), path.clone());
        let report = Reporter {
            cell: signal.id(),
            path: path.clone(),
            sink: self.inner.persist_failures.clone(),
        };
        let file = store.file_of(&path);
        // One live cell per file.
        let mut active = file.is_ok();
        if let Ok(file) = &file {
            let mut bound = self.inner.persist_paths.borrow_mut();
            match bound.get_mut(file) {
                Some(slot) if self.exists(slot.owner) => {
                    active = false;
                    self.diagnose(Diagnostic::PersistPathInUse {
                        cell: signal.id(),
                        other: slot.owner,
                        path: path.clone(),
                    });
                }
                Some(slot) => slot.owner = signal.id(),
                None => {
                    bound.insert(
                        file.clone(),
                        PathSlot {
                            owner: signal.id(),
                            waiting: Vec::new(),
                        },
                    );
                }
            }
            drop(bound);
            let mut stores = self.inner.persist_stores.borrow_mut();
            if !stores.iter().any(|s| s.same(store)) {
                stores.push(store.clone());
            }
        }
        let mut restored = if active {
            store.restore_reporting(&path, &default_bytes, Some(report.clone()))
        } else {
            match store.load(&path) {
                Ok(Some(s)) => Restore::Stored(s.value),
                Ok(None) => Restore::Default,
                Err(e) => Restore::Failed(e),
            }
        };
        let mut baseline = default_bytes.clone();
        let initial = match &restored {
            Restore::Stored(bytes) | Restore::KeptOverNewDefault(bytes) => match decode(bytes) {
                Some(v) => {
                    baseline.clone_from(bytes);
                    Some(v)
                }
                None => {
                    let file = file.clone().unwrap_or_default();
                    if active {
                        // Moved aside, like a corrupt file: the warning is
                        // not repeated on every start.
                        store.enqueue(file.clone(), Op::Quarantine, Some(report.clone()));
                    }
                    restored = Restore::Failed(PersistError::Corrupt {
                        path: file,
                        reason: Arc::from("the stored value does not decode as this cell's type"),
                    });
                    None
                }
            },
            Restore::Default | Restore::Adopted | Restore::Failed(_) => None,
        };
        if let Some(v) = initial {
            // Its starting value, not a write: nothing observes it yet.
            signal.init_value(self, v);
        }
        match &restored {
            Restore::KeptOverNewDefault(_) => self.diagnose(Diagnostic::PersistDefaultChanged {
                cell: signal.id(),
                path: path.clone(),
            }),
            Restore::Failed(error) => self.diagnose(Diagnostic::PersistFailed {
                cell: signal.id(),
                path: path.clone(),
                error: error.clone(),
            }),
            _ => {}
        }
        let default_hash = value_hash(&default_bytes);
        let writer = Rc::new(Writer {
            store: store.clone(),
            path,
            file: file.unwrap_or_default(),
            cell: signal.id(),
            signal,
            default: RefCell::new((default, default_bytes, default_hash)),
            encode: Box::new(encode),
            decode: Box::new(decode),
            start: baseline.clone(),
            baseline: RefCell::new(baseline),
            pending: RefCell::new(None),
            active: Cell::new(active),
            report,
        });
        let waiter: Weak<dyn Waiter> = Rc::downgrade(&writer) as Weak<Writer<T>>;
        {
            let mut live = self.inner.persist_writers.borrow_mut();
            if live.len().is_power_of_two() {
                live.retain(|w| w.strong_count() > 0);
            }
            live.push(waiter.clone());
        }
        if !active && let Some(slot) = self.inner.persist_paths.borrow_mut().get_mut(&writer.file) {
            slot.waiting.push(waiter);
        }
        let w = writer.clone();
        let saver = Rc::downgrade(&writer);
        self.on_change_after(
            move |rt| {
                let v = signal.get(rt)?;
                w.note(&v);
                Ok(v)
            },
            PERSIST_DEBOUNCE,
            move |_| {
                if let Some(w) = saver.upgrade() {
                    w.flush();
                }
                Ok(())
            },
        );
        let flusher = Rc::downgrade(&writer);
        let rt = self.downgrade();
        self.on_cleanup(move || {
            let Some(w) = flusher.upgrade() else {
                return;
            };
            let Some(rt) = rt.upgrade() else {
                w.flush();
                return;
            };
            // Cleanups run before the nodes go: the cell is still readable,
            // so a write its tracking effect never saw is not lost.
            w.capture(&rt);
            let next = {
                let mut bound = rt.inner.persist_paths.borrow_mut();
                let Some(slot) = bound.get_mut(&w.file) else {
                    return;
                };
                if slot.owner != w.cell {
                    // A waiter going away.
                    slot.waiting
                        .retain(|o| o.upgrade().is_some_and(|o| o.cell() != w.cell));
                    return;
                }
                // The owner going away: the oldest live waiter takes over,
                // else the path is free for the next cell (a remount).
                let mut next = None;
                while next.is_none() && !slot.waiting.is_empty() {
                    next = slot
                        .waiting
                        .remove(0)
                        .upgrade()
                        .filter(|o| rt.exists(o.cell()));
                }
                match &next {
                    Some(o) => slot.owner = o.cell(),
                    None => {
                        bound.remove(&w.file);
                    }
                }
                next
            };
            if let Some(o) = next {
                o.promote(&rt);
            }
        });
        Persisted {
            signal,
            restored,
            writer,
        }
    }

    /// [`Runtime::persisted`] for values with a built-in text encoding
    /// ([`PersistValue`]).
    pub fn persisted_value<T>(&self, store: &PersistStore, path: &str, default: T) -> Persisted<T>
    where
        T: PersistValue + Clone + PartialEq + 'static,
    {
        self.persisted(store, path, default, T::encode, T::decode)
    }

    /// Queue every live persisted cell's current value that the file does
    /// not hold yet (shutdown and dropping the last handle: root-level
    /// cells are disposed before root cleanups run).
    pub(crate) fn capture_persist(&self) {
        let live: Vec<_> = self
            .inner
            .persist_writers
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for w in live {
            w.capture(self);
        }
    }

    /// Wait (bounded) for the persist stores this runtime used to write
    /// what is queued; at shutdown.
    pub(crate) fn sync_persist(&self) {
        let stores = std::mem::take(&mut *self.inner.persist_stores.borrow_mut());
        for s in stores {
            s.sync(PERSIST_SHUTDOWN_WAIT);
        }
    }
}

/// A plain text encoding for common value types, for Rust-side state and
/// tests; the VM brings its own codec.
pub trait PersistValue: Sized {
    /// The value's bytes.
    fn encode(&self) -> Vec<u8>;
    /// The value back, or `None` if the bytes are not one.
    fn decode(bytes: &[u8]) -> Option<Self>;
}

macro_rules! persist_by_display {
    ($($t:ty),*) => {$(
        impl PersistValue for $t {
            fn encode(&self) -> Vec<u8> {
                self.to_string().into_bytes()
            }
            fn decode(bytes: &[u8]) -> Option<Self> {
                std::str::from_utf8(bytes).ok()?.parse().ok()
            }
        }
    )*};
}

persist_by_display!(bool, i32, i64, u32, u64, f64);

impl PersistValue for String {
    fn encode(&self) -> Vec<u8> {
        self.clone().into_bytes()
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        String::from_utf8(bytes.to_vec()).ok()
    }
}

/// Errors from the persist layer as graph errors (for handlers that call
/// the store directly).
impl From<PersistError> for Error {
    fn from(e: PersistError) -> Self {
        Error::failed(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable_fnv1a() {
        // Reference values of FNV-1a 64.
        assert_eq!(value_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(value_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(value_hash(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn paths_cannot_escape_the_directory() {
        let store = PersistStore::new("/state");
        let name = |p: &str| {
            store
                .file_of(p)
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name("launcher.query"), "launcher.query");
        assert_eq!(name("../etc/passwd"), "%2E.%2Fetc%2Fpasswd");
        assert_eq!(name(".hidden"), "%2Ehidden");
        assert_eq!(name("bar[DP-1].pins"), "bar%5BDP-1%5D.pins");
        assert_eq!(name("ü"), "%C3%BC");
        let long = "x".repeat(500);
        let n = name(&long);
        assert!(n.len() <= MAX_NAME, "{}", n.len());
        assert_ne!(
            n,
            name(&"x".repeat(501)),
            "distinct long paths stay distinct"
        );
        assert_eq!(store.file_of(""), Err(PersistError::EmptyPath));
        for p in ["../etc/passwd", ".hidden", &long] {
            assert_eq!(
                store.file_of(p).unwrap().parent(),
                Some(Path::new("/state"))
            );
        }
    }

    #[test]
    fn the_header_is_checked() {
        let good = b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c\n\na";
        assert_eq!(
            parse(good),
            Ok(Stored {
                value: b"a".to_vec(),
                default_hash: 1
            })
        );
        for bad in [
            &b""[..],
            b"strand-persist 2\n",
            b"strand-persist 1\ndefault 1\ncheck af63dc4c8601ec8c\n\na",
            b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c\n\nb",
            b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c\nx\na",
            b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }
}

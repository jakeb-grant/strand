//! The watcher thread: raw inotify events and polling in, coalesced
//! [`FileBatch`](crate::FileBatch)es out through an [`EventSink`].
//!
//! inotify is used directly (through `rustix`), not through `notify`:
//! `notify` adds `IN_OPEN` and `IN_ATTRIB` to every watch, so any process
//! opening any file in a watched directory (every font an app loads, every
//! read of `~/.config`) would wake this thread. The mask here holds only
//! the events Strand acts on, and the kernel never queues the rest.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

use crate::core::{
    Backend, ConfigWatch, Core, Options, Raw, Referenced, SharedOwnWrites, WatchKind,
};
use crate::event::{CacheKind, ChangeEvent, ContentHash, EventSink, Notice, RescanReason, Role};

enum Ctl {
    /// A registration; with a hash, the caller does not wait (no ack).
    AddFile(PathBuf, Role, Option<ContentHash>, Option<Sender<()>>),
    RemoveFile(PathBuf, Role),
    SetReferenced(Vec<Referenced>, Sender<()>),
    AddTree(PathBuf, usize, CacheKind, Sender<()>),
    Rescan,
    Stop,
}

/// What a directory watch listens for. Never `IN_OPEN`, `IN_ACCESS`,
/// `IN_CLOSE_NOWRITE` or `IN_ATTRIB`: reads cost nothing. Only the config
/// directories get `IN_MODIFY` (one event per `write(2)`), which marks a
/// write in progress so the file is not read before its `IN_CLOSE_WRITE`.
/// Every other content directory (a wallpaper's or settings file's
/// directory, a symlink hop such as `~`, `~/Downloads`, a font, icon or
/// `applications` tree) hears completed writes and names only, so a
/// download, a log or a package upgrade there costs one wakeup per file
/// closed, not one per write. A parent watch (`~/.config` above the
/// config root, a missing directory's nearest ancestor) hears names come
/// and go, never writes. An ancestor watch hears only its children being
/// moved away or deleted (and itself going): writes, new files and edits
/// in `~` or `/` queue nothing.
///
/// `IN_EXCL_UNLINK` (no events for a file once it has no name here) is
/// set everywhere except on completed-write watches. A config
/// directory needs it: a `MODIFY` from a writer still holding a deleted
/// module would otherwise come under the name a new file now has, and
/// hold that file. A completed-write watch must not have it: a file made
/// with `O_TMPFILE` and linked in (`linkat`) is created with its bytes,
/// held as being written, and its `CLOSE_WRITE` comes only under its
/// unnamed `#<ino>`, which `IN_EXCL_UNLINK` drops (the file would wait
/// for `stalled_write`). The cost there: a deleted file's writer closing
/// it reports a `CLOSE_WRITE` under its old name, which reads whatever
/// file has that name now.
pub(crate) fn watch_mask(kind: WatchKind) -> WatchFlags {
    let gone = WatchFlags::MOVED_FROM
        | WatchFlags::DELETE
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
        | WatchFlags::ONLYDIR;
    let completed = gone | WatchFlags::CLOSE_WRITE | WatchFlags::MOVED_TO | WatchFlags::CREATE;
    match kind {
        WatchKind::Ancestor => gone | WatchFlags::EXCL_UNLINK,
        WatchKind::Parent => {
            gone | WatchFlags::CREATE | WatchFlags::MOVED_TO | WatchFlags::EXCL_UNLINK
        }
        WatchKind::Completed => completed,
        WatchKind::Full => completed | WatchFlags::MODIFY | WatchFlags::EXCL_UNLINK,
    }
}

/// One inotify instance and its watch-descriptor map. The map is the only
/// record of which directory a descriptor names; [`Core`] decides when a
/// watch goes (a moved directory's descriptor follows the inode, so its
/// path is stale and core drops it, and everything below it).
pub(crate) struct Inotify {
    fd: OwnedFd,
    /// Each descriptor's directory, and what it is watched for.
    dirs: HashMap<i32, (PathBuf, WatchKind)>,
    wds: HashMap<PathBuf, i32>,
    buf: Vec<MaybeUninit<u8>>,
    /// Events read early, before a directory's mask changed: each is
    /// classified under the mask it was queued with (a creation queued
    /// under a names-only watch has no `CLOSE_WRITE` coming, even if the
    /// directory is a content watch by the time it is handled).
    early: Vec<Raw>,
}

impl Inotify {
    pub(crate) fn new() -> io::Result<Inotify> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        Ok(Inotify {
            fd,
            dirs: HashMap::new(),
            wds: HashMap::new(),
            buf: vec![MaybeUninit::uninit(); 16 * 1024],
            early: Vec::new(),
        })
    }

    fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String> {
        if self
            .wds
            .get(dir)
            .and_then(|wd| self.dirs.get(wd))
            .is_some_and(|(_, k)| *k != kind)
        {
            let mut early = std::mem::take(&mut self.early);
            // A failing read fails again at the watcher's next read, which
            // handles it.
            let _ = self.read_queued(&mut early);
            self.early = early;
        }
        let wd = inotify::add_watch(&self.fd, dir, watch_mask(kind))
            .map_err(|e| io::Error::from(e).to_string())?;
        if let Some(old) = self.wds.insert(dir.to_path_buf(), wd)
            && old != wd
        {
            // This path held another inode's watch: drop it.
            self.dirs.remove(&old);
            let _ = inotify::remove_watch(&self.fd, old);
        }
        // The same inode under another path (a stale name) now has this
        // one: inotify gives one descriptor per inode.
        if let Some((prev, _)) = self.dirs.insert(wd, (dir.to_path_buf(), kind))
            && prev != dir
        {
            self.wds.remove(&prev);
        }
        Ok(())
    }

    fn unwatch(&mut self, dir: &Path) {
        if let Some(wd) = self.wds.remove(dir) {
            self.dirs.remove(&wd);
            // Fails harmlessly when the kernel already dropped it.
            let _ = inotify::remove_watch(&self.fd, wd);
        }
    }

    /// Whether events read early wait to be handled.
    fn has_early(&self) -> bool {
        !self.early.is_empty()
    }

    /// Every event read early, then every queued event (the fd is
    /// non-blocking).
    fn read(&mut self, out: &mut Vec<Raw>) -> io::Result<()> {
        out.append(&mut self.early);
        self.read_queued(out)
    }

    fn read_queued(&mut self, out: &mut Vec<Raw>) -> io::Result<()> {
        let Inotify {
            fd, dirs, wds, buf, ..
        } = self;
        let mut reader = inotify::Reader::new(&*fd, buf);
        loop {
            let ev = match reader.next() {
                Ok(ev) => ev,
                Err(Errno::AGAIN) => return Ok(()),
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            };
            let flags = ev.events();
            if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                out.push(Raw::Overflow);
                continue;
            }
            if flags.contains(ReadFlags::IGNORED) {
                // The kernel dropped the watch. After our own
                // `remove_watch` it is no longer mapped; otherwise its
                // directory is gone (DELETE_SELF or UNMOUNT came first).
                if let Some((dir, _)) = dirs.remove(&ev.wd()) {
                    if wds.get(&dir) == Some(&ev.wd()) {
                        wds.remove(&dir);
                    }
                    out.push(Raw::Gone(dir));
                }
                continue;
            }
            // Events still queued for a descriptor already removed.
            let Some((dir, kind)) = dirs.get(&ev.wd()) else {
                continue;
            };
            let path = match ev.file_name() {
                Some(name) => dir.join(OsStr::from_bytes(name.to_bytes())),
                None => dir.clone(),
            };
            if let Some(raw) = classify(flags, ev.file_name().is_some(), *kind, path) {
                out.push(raw);
            }
        }
    }
}

/// Reduce one inotify event to what Strand acts on: completed writes
/// (`CLOSE_WRITE`, `MOVED_TO`), names created complete (a symlink, a hard
/// link), removals, directory changes. `MODIFY` and an empty new file are
/// a write in progress: the file is not read until it is closed.
/// `named` is false for events about the watched directory itself;
/// `kind` is what the directory is watched for (its mask).
pub(crate) fn classify(
    flags: ReadFlags,
    named: bool,
    kind: WatchKind,
    path: PathBuf,
) -> Option<Raw> {
    if !named {
        // The watched directory was deleted, moved (its descriptor now
        // follows the inode elsewhere) or unmounted.
        let gone = ReadFlags::DELETE_SELF | ReadFlags::MOVE_SELF | ReadFlags::UNMOUNT;
        return flags.intersects(gone).then_some(Raw::Gone(path));
    }
    let is_dir = flags.contains(ReadFlags::ISDIR);
    Some(if flags.contains(ReadFlags::CLOSE_WRITE) {
        Raw::Written(path)
    } else if flags.contains(ReadFlags::MOVED_TO) {
        if is_dir {
            Raw::Dir(path)
        } else {
            Raw::Written(path)
        }
    } else if flags.intersects(ReadFlags::MOVED_FROM | ReadFlags::DELETE) {
        Raw::Gone(path)
    } else if flags.contains(ReadFlags::CREATE) {
        if is_dir {
            Raw::Dir(path)
        } else {
            // A names-only watch queues no CLOSE_WRITE: what is there now
            // is all it will say (and a creation queued before the watch
            // became a content watch must not wait for one).
            let closes = kind >= WatchKind::Completed;
            match std::fs::symlink_metadata(&path) {
                // `ln -s` makes no CLOSE_WRITE: the link is complete now.
                Ok(m) if m.file_type().is_symlink() => Raw::Linked(path),
                Ok(m) if m.is_dir() => Raw::Dir(path),
                // An empty new file is being written: wait for CLOSE_WRITE.
                Ok(m) if !closes && !m.is_dir() => Raw::Linked(path),
                Ok(m) if m.is_file() && m.nlink() == 1 && m.len() == 0 => Raw::Busy(path),
                // A file with bytes already: linked in complete from
                // `O_TMPFILE` (its writes and CLOSE_WRITE, if any, came
                // under its `#<ino>` name), or a writer that wrote its
                // first bytes before this creation was read. With
                // `MODIFY` in the mask, that writer's MODIFY is queued
                // right behind and holds the file until its CLOSE_WRITE;
                // without it, core holds the file itself.
                Ok(m) if m.is_file() && m.nlink() == 1 && kind == WatchKind::Completed => {
                    Raw::Created(path)
                }
                // `ln` (a new name for complete content), `mkfifo` and the
                // like make no CLOSE_WRITE.
                Ok(_) => Raw::Linked(path),
                Err(_) if closes => Raw::Busy(path),
                Err(_) => Raw::Linked(path),
            }
        }
    } else if flags.contains(ReadFlags::MODIFY) {
        Raw::Busy(path)
    } else {
        return None;
    })
}

/// The kernel side: an inotify instance, or none when `inotify_init`
/// failed (`fs.inotify.max_user_instances` reached), in which case every
/// directory is polled.
pub(crate) enum Kernel {
    Inotify(Inotify),
    Unavailable(String),
}

impl Kernel {
    fn new() -> Kernel {
        match Inotify::new() {
            Ok(i) => Kernel::Inotify(i),
            Err(e) => Kernel::Unavailable(format!("inotify unavailable: {e}")),
        }
    }
}

impl Backend for Kernel {
    fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String> {
        match self {
            Kernel::Inotify(i) => i.watch(dir, kind),
            Kernel::Unavailable(reason) => Err(reason.clone()),
        }
    }

    fn unwatch(&mut self, dir: &Path) {
        if let Kernel::Inotify(i) = self {
            i.unwatch(dir);
        }
    }

    fn drain(&mut self, out: &mut Vec<Raw>) -> io::Result<()> {
        match self {
            Kernel::Inotify(i) => i.read(out),
            Kernel::Unavailable(_) => Ok(()),
        }
    }
}

/// Watches the config directory, referenced files and cache trees on its
/// own thread. Dropping it stops the thread.
#[derive(Debug)]
pub struct Watcher {
    tx: Sender<Ctl>,
    own: SharedOwnWrites,
    /// Wakes the thread's `poll` after a control message.
    wake: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Ctl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ctl")
    }
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "strand-watch thread stopped")
}

impl Watcher {
    /// Start watching. When this returns, every directory is watched and
    /// every module file's baseline hash is recorded, so a save made right
    /// after is reported. Start the watcher before loading files.
    ///
    /// If no inotify instance can be created (the per-user instance limit
    /// is reached), every directory is polled instead and reported once as
    /// [`Notice::Polling`] with [`PollReason::WatchFailed`](crate::PollReason).
    pub fn spawn(
        config: Option<ConfigWatch>,
        options: Options,
        sink: EventSink,
    ) -> io::Result<Watcher> {
        Self::spawn_with(Kernel::new(), config, options, sink)
    }

    pub(crate) fn spawn_with(
        kernel: Kernel,
        config: Option<ConfigWatch>,
        options: Options,
        sink: EventSink,
    ) -> io::Result<Watcher> {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(rustix::event::eventfd(
            0,
            EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK,
        )?);
        let core = Core::new(kernel, options.clone(), config);
        let own = core.own_writes();
        let thread_wake = wake.clone();
        let thread = std::thread::Builder::new()
            .name("strand-watch".into())
            .spawn(move || run(core, rx, &thread_wake, sink, options))?;
        Ok(Watcher {
            tx,
            own,
            wake,
            thread: Some(thread),
        })
    }

    fn send(&self, ctl: Ctl) -> io::Result<()> {
        self.tx.send(ctl).map_err(|_| stopped())?;
        // A full counter (never, in practice) still leaves it readable.
        let _ = rustix::io::write(&*self.wake, &1u64.to_ne_bytes());
        Ok(())
    }

    fn call(&self, make: impl FnOnce(Sender<()>) -> Ctl) -> io::Result<()> {
        let (ack, done) = mpsc::channel();
        self.send(make(ack))?;
        done.recv().map_err(|_| stopped())
    }

    /// Watch a referenced file (settings TOML, wallpaper, shader) for
    /// `role`: its directory and every symlink's directory. Neither the
    /// file nor its directory need exist yet. This is an ad-hoc
    /// registration, counted per (path, role), that
    /// [`Watcher::set_referenced`] never replaces; a path watched for
    /// several roles gets one change per role.
    ///
    /// Register, then read: a save made after this returns is reported.
    /// It blocks until the watch exists and a new file's baseline is read
    /// (on the watcher thread), so call it from the loader or a worker,
    /// never from the logic thread: there, read first and use
    /// [`Watcher::watch_file_loaded`].
    pub fn watch_file(&self, path: impl Into<PathBuf>, role: Role) -> io::Result<()> {
        let path = path.into();
        self.call(|ack| Ctl::AddFile(path, role, None, Some(ack)))
    }

    /// [`Watcher::watch_file`] for a file the caller already read, with
    /// `hash` ([`hash_bytes`](crate::hash_bytes)) of the bytes it holds.
    /// Returns at once (safe on the logic thread): the watcher compares
    /// the file with `hash` once the watch is in place, so a save made at
    /// any time after the read is reported.
    pub fn watch_file_loaded(
        &self,
        path: impl Into<PathBuf>,
        role: Role,
        hash: ContentHash,
    ) -> io::Result<()> {
        self.send(Ctl::AddFile(path.into(), role, Some(hash), None))
    }

    /// Drop one [`Watcher::watch_file`] registration of `path` for `role`.
    /// The path stays watched while other registrations (the loader's
    /// set, the module set) hold it. Returns at once.
    pub fn unwatch_file(&self, path: impl Into<PathBuf>, role: Role) -> io::Result<()> {
        self.send(Ctl::RemoveFile(path.into(), role))
    }

    /// Replace the loader's registrations with this set: the referenced
    /// paths the compiler collected from the whole program after a reload,
    /// as `(path, role)` or `(path, role, hash)` where `hash` is
    /// [`hash_bytes`](crate::hash_bytes) of what the loader read.
    /// [`Watcher::watch_file`] registrations are separate and kept. Paths
    /// that stay keep their baseline; dropped ones are no longer watched
    /// (unless something else holds them). A file that no longer holds
    /// the bytes its `hash` names (saved between the read and this call)
    /// is reported at the next quiet period. It blocks until the watches
    /// exist; a new path given without a hash is also read before it
    /// returns, one given with a hash is not (the watcher compares it
    /// later). Call it from the loader, not the logic thread.
    pub fn set_referenced<R: Into<Referenced>>(
        &self,
        refs: impl IntoIterator<Item = R>,
    ) -> io::Result<()> {
        let refs: Vec<Referenced> = refs.into_iter().map(Into::into).collect();
        self.call(|ack| Ctl::SetReferenced(refs, ack))
    }

    /// Watch a cache-invalidation tree (`applications/`, an icon theme
    /// directory, a font directory) to `depth` directories below `root`.
    /// Changes come as [`Role::Cache`] entries, unhashed. It blocks while
    /// the tree is walked and watched; call it at boot or from the
    /// service that owns the cache, not from the logic thread.
    pub fn watch_tree(
        &self,
        root: impl Into<PathBuf>,
        depth: usize,
        kind: CacheKind,
    ) -> io::Result<()> {
        let root = root.into();
        self.call(|ack| Ctl::AddTree(root, depth, kind, ack))
    }

    /// Strand is about to write `path` with content hashing to `hash`
    /// (`hash_bytes`); that write is not reported. Call it before writing;
    /// the registration is in place when this returns. Write atomically
    /// (a temporary file renamed over `path`): an in-place write can be
    /// read half done, and that content is not the registered one.
    pub fn register_own_write(&self, path: impl Into<PathBuf>, hash: ContentHash) {
        // Resolved before taking the lock the watcher thread takes for
        // every file it hashes: resolving can be slow on NFS.
        let path = crate::paths::absolute(&path.into());
        let canonical = crate::paths::resolve(&path).path;
        let mut own = self.own.lock().unwrap_or_else(|e| e.into_inner());
        own.register(path, canonical, hash, Instant::now());
    }

    /// Rescan everything now (`strand reload`); the batch is marked
    /// [`RescanReason::Requested`].
    pub fn rescan(&self) {
        let _ = self.send(Ctl::Rescan);
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.send(Ctl::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Handle one control message; `false` to stop.
fn control(core: &mut Core<Kernel>, ctl: Ctl) -> bool {
    match ctl {
        Ctl::AddFile(p, role, loaded, ack) => {
            core.add_file(&p, role, loaded);
            if let Some(ack) = ack {
                let _ = ack.send(());
            }
        }
        Ctl::RemoveFile(p, role) => core.remove_file(&p, role),
        Ctl::SetReferenced(refs, ack) => {
            core.set_referenced(refs);
            let _ = ack.send(());
        }
        Ctl::AddTree(p, depth, kind, ack) => {
            core.add_tree(&p, depth, kind);
            let _ = ack.send(());
        }
        Ctl::Rescan => core.request_rescan(RescanReason::Requested, Instant::now()),
        Ctl::Stop => return false,
    }
    true
}

fn run(mut core: Core<Kernel>, rx: Receiver<Ctl>, wake: &OwnedFd, sink: EventSink, opts: Options) {
    let mut next_poll = Instant::now() + opts.poll_interval;
    let mut raws = Vec::new();
    // A failing `poll(2)` (ENOMEM) is retried after a growing pause, and
    // each distinct error is reported once.
    let mut backoff = Duration::ZERO;
    let mut reported = HashSet::new();
    loop {
        let now = Instant::now();
        let mut deadline = core.deadline();
        if core.has_polled_dirs() {
            deadline = Some(deadline.map_or(next_poll, |w| w.min(next_poll)));
        }
        // Events read early (a mask change during the last flush) are
        // handled at once.
        let early = matches!(core.backend(), Kernel::Inotify(i) if i.has_early());
        if early {
            deadline = Some(now);
        }
        let timeout =
            deadline.and_then(|d| Timespec::try_from(d.saturating_duration_since(now)).ok());
        let (ctl_ready, fs_ready) = {
            let mut fds = vec![PollFd::new(wake, PollFlags::IN)];
            if let Kernel::Inotify(i) = core.backend() {
                fds.push(PollFd::new(&i.fd, PollFlags::IN));
            }
            match rustix::event::poll(&mut fds, timeout.as_ref()) {
                Ok(_) => {
                    backoff = Duration::ZERO;
                    (
                        !fds[0].revents().is_empty(),
                        fds.get(1).is_some_and(|f| !f.revents().is_empty()),
                    )
                }
                Err(Errno::INTR) => (false, false),
                Err(e) => {
                    let msg = format!("poll: {e}");
                    if reported.insert(msg.clone()) {
                        core.notice(Notice::Backend(msg), Instant::now());
                    }
                    backoff = (backoff * 2)
                        .max(Duration::from_millis(10))
                        .min(opts.poll_interval);
                    std::thread::sleep(backoff);
                    (true, true)
                }
            }
        };
        if ctl_ready {
            // Reset the counter, then take every queued message: a message
            // sent after the drain writes the counter again.
            let mut word = [0u8; 8];
            let _ = rustix::io::read(wake, &mut word);
            loop {
                match rx.try_recv() {
                    Ok(ctl) => {
                        if !control(&mut core, ctl) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        }
        let early = matches!(core.backend(), Kernel::Inotify(i) if i.has_early());
        if (fs_ready || early)
            && let Kernel::Inotify(i) = core.backend()
        {
            let read = i.read(&mut raws);
            let now = Instant::now();
            for raw in raws.drain(..) {
                core.on_raw(raw, now);
            }
            if let Err(e) = read {
                // The fd would stay readable and fail again on every turn:
                // drop inotify, poll everything, and rescan for what the
                // failed read lost.
                let reason = format!("inotify read failed: {e}");
                core.notice(Notice::Backend(reason.clone()), now);
                *core.backend() = Kernel::Unavailable(reason);
                core.rewatch_all(now);
            }
        }
        let now = Instant::now();
        if core.has_polled_dirs() && now >= next_poll {
            core.poll(now);
            next_poll = now + opts.poll_interval;
        }
        if core.deadline().is_some_and(|d| now >= d)
            && let Some(batch) = core.flush(now)
            && !sink.send(ChangeEvent::Files(batch))
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn translation_keeps_completed_writes_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let file = dir.join("bar.strand");
        std::fs::write(&file, "x").unwrap();
        let link = dir.join("link.strand");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let alone = dir.join("alone.strand");
        std::fs::write(&alone, "").unwrap();
        // Complete before its creation is read (`O_TMPFILE` + `linkat`).
        let whole = dir.join("whole.strand");
        std::fs::write(&whole, "y").unwrap();
        let hard = dir.join("hard.strand");
        std::fs::hard_link(&file, &hard).unwrap();
        let sub = dir.join("sub");
        let f = |flags, p: &Path| classify(flags, true, WatchKind::Full, p.to_path_buf());
        let got: Vec<Option<Raw>> = vec![
            f(ReadFlags::MODIFY, &file),
            f(ReadFlags::CREATE, &alone),
            f(ReadFlags::CREATE, &whole),
            f(ReadFlags::CREATE, &link),
            f(ReadFlags::CREATE, &hard),
            f(ReadFlags::CLOSE_WRITE, &file),
            f(ReadFlags::CLOSE_NOWRITE, &file),
            f(ReadFlags::OPEN, &file),
            f(ReadFlags::ATTRIB, &file),
            f(ReadFlags::MOVED_TO, &file),
            f(ReadFlags::MOVED_TO | ReadFlags::ISDIR, &sub),
            f(ReadFlags::CREATE | ReadFlags::ISDIR, &sub),
            f(ReadFlags::MOVED_FROM, &file),
            f(ReadFlags::DELETE | ReadFlags::ISDIR, &sub),
            classify(ReadFlags::MOVE_SELF, false, WatchKind::Full, dir.clone()),
            classify(ReadFlags::DELETE_SELF, false, WatchKind::Full, dir.clone()),
            classify(ReadFlags::MODIFY, false, WatchKind::Full, dir.clone()),
            // Without `MODIFY` in the mask, a new file with bytes is held
            // by core; a hard link and a symlink as above.
            classify(ReadFlags::CREATE, true, WatchKind::Completed, whole.clone()),
            classify(ReadFlags::CREATE, true, WatchKind::Completed, hard.clone()),
            classify(ReadFlags::CREATE, true, WatchKind::Completed, link.clone()),
            // A names-only watch has no CLOSE_WRITE to wait for.
            classify(ReadFlags::CREATE, true, WatchKind::Parent, alone.clone()),
            classify(ReadFlags::CREATE, true, WatchKind::Parent, whole.clone()),
        ];
        assert_eq!(
            got,
            vec![
                Some(Raw::Busy(file.clone())),
                Some(Raw::Busy(alone.clone())),
                Some(Raw::Linked(whole.clone())),
                Some(Raw::Linked(link.clone())),
                Some(Raw::Linked(hard.clone())),
                Some(Raw::Written(file.clone())),
                None,
                None,
                None,
                Some(Raw::Written(file.clone())),
                Some(Raw::Dir(sub.clone())),
                Some(Raw::Dir(sub.clone())),
                Some(Raw::Gone(file)),
                Some(Raw::Gone(sub)),
                Some(Raw::Gone(dir.clone())),
                Some(Raw::Gone(dir)),
                None,
                Some(Raw::Created(whole.clone())),
                Some(Raw::Linked(hard)),
                Some(Raw::Linked(link)),
                Some(Raw::Linked(alone.clone())),
                Some(Raw::Linked(whole)),
            ]
        );
    }

    /// Reads cost nothing: the watch mask has no `IN_OPEN`, `IN_ACCESS`,
    /// `IN_CLOSE_NOWRITE` or `IN_ATTRIB`, so the kernel queues no event at
    /// all for them (an idle shell does zero work while fonts, icons and
    /// `~/.config` files are opened around it).
    #[test]
    fn reading_a_watched_file_queues_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let file = dir.join("font.ttf");
        std::fs::write(&file, "glyphs").unwrap();
        let mut ino = Inotify::new().unwrap();
        ino.watch(&dir, WatchKind::Full).unwrap();
        let mut out = Vec::new();
        for _ in 0..100 {
            let mut s = String::new();
            std::fs::File::open(&file)
                .unwrap()
                .read_to_string(&mut s)
                .unwrap();
            std::fs::metadata(&file).unwrap();
        }
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();
        ino.read(&mut out).unwrap();
        assert_eq!(out, vec![], "reads and chmod queue nothing");
        // A write does.
        std::fs::write(dir.join("other"), "x").unwrap();
        ino.read(&mut out).unwrap();
        assert!(out.contains(&Raw::Written(dir.join("other"))), "{out:?}");
    }

    /// A content watch outside the config directories (a wallpaper's
    /// directory, `~`, a font tree) has no `MODIFY`: a process writing
    /// there (a download, a log, shell history) queues one event when it
    /// creates the file and one when it closes it, never one per write.
    /// A config directory's watch does hear each write.
    #[test]
    fn writes_queue_events_only_in_config_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let (pics, cfg) = (base.join("pics"), base.join("cfg"));
        std::fs::create_dir(&pics).unwrap();
        std::fs::create_dir(&cfg).unwrap();
        let mut ino = Inotify::new().unwrap();
        ino.watch(&pics, WatchKind::Completed).unwrap();
        ino.watch(&cfg, WatchKind::Full).unwrap();
        let mut out = Vec::new();
        let download = pics.join("download.part");
        let mut f = std::fs::File::create(&download).unwrap();
        for _ in 0..1000 {
            std::io::Write::write_all(&mut f, b"0123456789abcdef").unwrap();
            // Drained after every write, as the watcher thread would.
            ino.read(&mut out).unwrap();
        }
        drop(f);
        ino.read(&mut out).unwrap();
        // The creation is read after the first write here, so it comes
        // as `Created` (a file with bytes, held until closed).
        assert_eq!(
            out,
            vec![Raw::Created(download.clone()), Raw::Written(download)],
            "creation and close only"
        );
        out.clear();
        let log = cfg.join("status.txt");
        let mut f = std::fs::File::create(&log).unwrap();
        for _ in 0..10 {
            std::io::Write::write_all(&mut f, b"x").unwrap();
            ino.read(&mut out).unwrap();
        }
        assert_eq!(
            out.iter().filter(|r| **r == Raw::Busy(log.clone())).count(),
            10,
            "one per write"
        );
    }

    /// A creation queued under a names-only watch is classified under that
    /// mask even when the directory became a content watch before it was
    /// read (a file registered right after it was written): it has no
    /// `CLOSE_WRITE` coming, so it must not be held waiting for one.
    #[test]
    fn events_keep_the_mask_they_were_queued_under() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let mut ino = Inotify::new().unwrap();
        ino.watch(&dir, WatchKind::Parent).unwrap();
        std::fs::write(dir.join("wall.png"), "png").unwrap();
        std::fs::write(dir.join("empty"), "").unwrap();
        ino.watch(&dir, WatchKind::Completed).unwrap();
        std::fs::write(dir.join("next.png"), "png").unwrap();
        let mut out = Vec::new();
        ino.read(&mut out).unwrap();
        assert_eq!(
            out,
            vec![
                Raw::Linked(dir.join("wall.png")),
                Raw::Linked(dir.join("empty")),
                Raw::Created(dir.join("next.png")),
                Raw::Written(dir.join("next.png")),
            ]
        );
    }

    /// A parent watch (`~/.config` above the config root) hears names
    /// come and go, never writes: an app writing its own file there wakes
    /// nothing.
    #[test]
    fn a_parent_watch_hears_names_not_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let sibling = dir.join("other-app.conf");
        std::fs::write(&sibling, "a").unwrap();
        let mut ino = Inotify::new().unwrap();
        ino.watch(&dir, WatchKind::Parent).unwrap();
        let mut out = Vec::new();
        for i in 0..50 {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&sibling)
                .unwrap();
            std::io::Write::write_all(&mut f, format!("{i}").as_bytes()).unwrap();
        }
        std::fs::read(&sibling).unwrap();
        ino.read(&mut out).unwrap();
        assert_eq!(out, vec![], "writes to a sibling queue nothing");
        // The config directory appearing, going, and a link to it do.
        let cfg = dir.join("strand");
        std::fs::create_dir(&cfg).unwrap();
        std::fs::rename(&cfg, dir.join("strand.bak")).unwrap();
        std::os::unix::fs::symlink(dir.join("strand.bak"), &cfg).unwrap();
        ino.read(&mut out).unwrap();
        assert_eq!(
            out,
            vec![
                Raw::Dir(cfg.clone()),
                Raw::Gone(cfg.clone()),
                Raw::Dir(dir.join("strand.bak")),
                Raw::Linked(cfg),
            ]
        );
    }

    /// A moved directory's descriptor follows the inode; once core drops
    /// it, nothing in the moved tree is reported under the old name.
    #[test]
    fn a_moved_directory_reports_its_own_move() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let cfg = base.join("cfg");
        std::fs::create_dir(&cfg).unwrap();
        let mut ino = Inotify::new().unwrap();
        ino.watch(&cfg, WatchKind::Full).unwrap();
        std::fs::rename(&cfg, base.join("cfg.bak")).unwrap();
        let mut out = Vec::new();
        ino.read(&mut out).unwrap();
        assert_eq!(out, vec![Raw::Gone(cfg.clone())]);
        ino.unwatch(&cfg);
        std::fs::write(base.join("cfg.bak/bar.strand"), "x").unwrap();
        out.clear();
        ino.read(&mut out).unwrap();
        assert_eq!(out, vec![]);
    }

    /// With no inotify instance (the per-user limit reached), every
    /// directory is polled, reported once, and edits are still seen.
    #[test]
    fn no_inotify_instance_falls_back_to_polling() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = std::fs::canonicalize(tmp.path()).unwrap().join("cfg");
        std::fs::create_dir(&cfg).unwrap();
        std::fs::write(cfg.join("bar.strand"), "bar 1").unwrap();
        let set = crate::ModuleSet {
            files: vec![cfg.join("bar.strand")],
            dirs: vec![cfg.clone()],
            ..Default::default()
        };
        let rescan_set = set.clone();
        let config = ConfigWatch {
            root: cfg.clone(),
            modules: set,
            rescan: Box::new(move || Ok(rescan_set.clone())),
        };
        let opts = Options {
            poll_interval: std::time::Duration::from_millis(20),
            ..Options::default()
        };
        let (sink, rx) = crate::channel();
        let reason = "inotify unavailable: Too many open files".to_string();
        let w = Watcher::spawn_with(
            Kernel::Unavailable(reason.clone()),
            Some(config),
            opts,
            sink,
        )
        .unwrap();
        let next = || match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(ChangeEvent::Files(b)) => b,
            other => panic!("{other:?}"),
        };
        let b = next();
        assert!(
            b.notices.contains(&Notice::Polling {
                dir: cfg.clone(),
                reason: crate::PollReason::WatchFailed(reason),
            }),
            "{b:#?}"
        );
        std::fs::write(cfg.join("bar.strand"), "bar 22").unwrap();
        let b = next();
        assert_eq!(b.changes.len(), 1, "{b:#?}");
        assert_eq!(b.changes[0].hash, Some(blake3::hash(b"bar 22")));
        drop(w);
    }
}

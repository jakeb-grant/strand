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
    AddFile(PathBuf, Role, Sender<()>),
    RemoveFile(PathBuf, Role, Sender<()>),
    SetReferenced(Vec<Referenced>, Sender<()>),
    AddTree(PathBuf, usize, CacheKind, Sender<()>),
    Rescan,
    Stop,
}

/// What a directory watch listens for. Never `IN_OPEN`, `IN_ACCESS`,
/// `IN_CLOSE_NOWRITE` or `IN_ATTRIB`: reads cost nothing. `IN_MODIFY` only
/// keeps an already-open batch waiting. An ancestor watch hears only its
/// children being moved away or deleted (and itself going): writes, new
/// files and edits in `~` or `/` queue nothing.
fn watch_mask(kind: WatchKind) -> WatchFlags {
    let gone = WatchFlags::MOVED_FROM
        | WatchFlags::DELETE
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF
        | WatchFlags::ONLYDIR
        | WatchFlags::EXCL_UNLINK;
    match kind {
        WatchKind::Ancestor => gone,
        WatchKind::Full => {
            gone | WatchFlags::CLOSE_WRITE
                | WatchFlags::MOVED_TO
                | WatchFlags::CREATE
                | WatchFlags::MODIFY
        }
    }
}

/// One inotify instance and its watch-descriptor map. The map is the only
/// record of which directory a descriptor names; [`Core`] decides when a
/// watch goes (a moved directory's descriptor follows the inode, so its
/// path is stale and core drops it, and everything below it).
pub(crate) struct Inotify {
    fd: OwnedFd,
    dirs: HashMap<i32, PathBuf>,
    wds: HashMap<PathBuf, i32>,
    buf: Vec<MaybeUninit<u8>>,
}

impl Inotify {
    pub(crate) fn new() -> io::Result<Inotify> {
        let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
        Ok(Inotify {
            fd,
            dirs: HashMap::new(),
            wds: HashMap::new(),
            buf: vec![MaybeUninit::uninit(); 16 * 1024],
        })
    }

    fn watch(&mut self, dir: &Path, kind: WatchKind) -> Result<(), String> {
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
        if let Some(prev) = self.dirs.insert(wd, dir.to_path_buf())
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

    /// Read every queued event (the fd is non-blocking).
    fn read(&mut self, out: &mut Vec<Raw>) -> io::Result<()> {
        let Inotify { fd, dirs, wds, buf } = self;
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
                if let Some(dir) = dirs.remove(&ev.wd()) {
                    if wds.get(&dir) == Some(&ev.wd()) {
                        wds.remove(&dir);
                    }
                    out.push(Raw::Gone(dir));
                }
                continue;
            }
            // Events still queued for a descriptor already removed.
            let Some(dir) = dirs.get(&ev.wd()) else {
                continue;
            };
            let path = match ev.file_name() {
                Some(name) => dir.join(OsStr::from_bytes(name.to_bytes())),
                None => dir.clone(),
            };
            if let Some(raw) = classify(flags, ev.file_name().is_some(), path) {
                out.push(raw);
            }
        }
    }
}

/// Reduce one inotify event to what Strand acts on: completed writes
/// (`CLOSE_WRITE`, `MOVED_TO`, a new symlink or hard link), removals,
/// directory changes. `MODIFY` is only ever a "write in progress" hint.
/// `named` is false for events about the watched directory itself.
pub(crate) fn classify(flags: ReadFlags, named: bool, path: PathBuf) -> Option<Raw> {
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
            match std::fs::symlink_metadata(&path) {
                // `ln -s` makes no CLOSE_WRITE: the link is complete now.
                Ok(m) if m.file_type().is_symlink() => Raw::Written(path),
                Ok(m) if m.is_dir() => Raw::Dir(path),
                // An empty new file is being written: wait for CLOSE_WRITE.
                // But `ln` (a new name for complete content), a file
                // linked in complete from `O_TMPFILE` (its CLOSE_WRITE, if
                // any, came under its `#<ino>` name), `mkfifo` and the
                // like make no CLOSE_WRITE either. A file that already has
                // bytes when its creation is read is reported; a write
                // still in progress extends the batch with MODIFY.
                Ok(m) if m.is_file() && m.nlink() == 1 && m.len() == 0 => Raw::Busy(path),
                Ok(_) => Raw::Written(path),
                Err(_) => Raw::Busy(path),
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
    /// file nor its directory need exist yet. Registrations are counted
    /// per (path, role); a path watched for several roles gets one change
    /// per role.
    ///
    /// Register, then read: a save made after this returns is reported.
    /// A file read before registering is passed with the hash of what was
    /// read through [`Watcher::set_referenced`] instead.
    pub fn watch_file(&self, path: impl Into<PathBuf>, role: Role) -> io::Result<()> {
        let path = path.into();
        self.call(|ack| Ctl::AddFile(path, role, ack))
    }

    /// Drop one [`Watcher::watch_file`] registration of `path` for `role`.
    /// The path stays watched while other registrations (or the module
    /// set) hold it.
    pub fn unwatch_file(&self, path: impl Into<PathBuf>, role: Role) -> io::Result<()> {
        let path = path.into();
        self.call(|ack| Ctl::RemoveFile(path, role, ack))
    }

    /// Replace every [`Watcher::watch_file`] registration with this set:
    /// the referenced paths the compiler collected from the whole program
    /// after a reload, as `(path, role)` or `(path, role, hash)` where
    /// `hash` is [`hash_bytes`](crate::hash_bytes) of what the loader
    /// read. Paths that stay keep their baseline; new ones get one;
    /// dropped ones are no longer watched (unless they are modules). A
    /// file that no longer holds the bytes its `hash` names (saved between
    /// the read and this call) is reported at the next quiet period.
    pub fn set_referenced<R: Into<Referenced>>(
        &self,
        refs: impl IntoIterator<Item = R>,
    ) -> io::Result<()> {
        let refs: Vec<Referenced> = refs.into_iter().map(Into::into).collect();
        self.call(|ack| Ctl::SetReferenced(refs, ack))
    }

    /// Watch a cache-invalidation tree (`applications/`, an icon theme
    /// directory, a font directory) to `depth` directories below `root`.
    /// Changes come as [`Role::Cache`] entries, unhashed.
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
        let path = path.into();
        let mut own = self.own.lock().unwrap_or_else(|e| e.into_inner());
        own.register(&path, hash, Instant::now());
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
        Ctl::AddFile(p, role, ack) => {
            core.add_file(&p, role, None);
            let _ = ack.send(());
        }
        Ctl::RemoveFile(p, role, ack) => {
            core.remove_file(&p, role);
            let _ = ack.send(());
        }
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
        if fs_ready && let Kernel::Inotify(i) = core.backend() {
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
        let f = |flags, p: &Path| classify(flags, true, p.to_path_buf());
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
            classify(ReadFlags::MOVE_SELF, false, dir.clone()),
            classify(ReadFlags::DELETE_SELF, false, dir.clone()),
            classify(ReadFlags::MODIFY, false, dir.clone()),
        ];
        assert_eq!(
            got,
            vec![
                Some(Raw::Busy(file.clone())),
                Some(Raw::Busy(alone)),
                Some(Raw::Written(whole)),
                Some(Raw::Written(link)),
                Some(Raw::Written(hard)),
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

//! The watcher thread: notify (inotify) events and polling in, coalesced
//! [`FileBatch`](crate::FileBatch)es out through an [`EventSink`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher as _};

use crate::core::{Backend, ConfigWatch, Core, Options, Raw};
use crate::event::{CacheKind, ChangeEvent, ContentHash, EventSink, Notice, RescanReason, Role};

enum Msg {
    Fs(notify::Result<notify::Event>),
    Ctl(Ctl),
}

enum Ctl {
    AddFile(PathBuf, Role, Sender<()>),
    RemoveFile(PathBuf, Sender<()>),
    AddTree(PathBuf, usize, CacheKind, Sender<()>),
    OwnWrite(PathBuf, ContentHash),
    Rescan,
    Stop,
}

struct NotifyBackend(notify::RecommendedWatcher);

impl Backend for NotifyBackend {
    fn watch(&mut self, dir: &Path) -> Result<(), String> {
        self.0
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| e.to_string())
    }

    fn unwatch(&mut self, dir: &Path) {
        // notify drops a watch itself when its directory is deleted.
        let _ = self.0.unwatch(dir);
    }
}

/// Reduce one notify event to what Strand acts on: completed writes
/// (`CLOSE_WRITE`, `MOVED_TO`, a new symlink), removals, directory changes
/// and overflow. `MODIFY` is only ever a "write in progress" hint.
pub(crate) fn translate(event: notify::Event, out: &mut Vec<Raw>) {
    if event.need_rescan() {
        out.push(Raw::Overflow);
        return;
    }
    let lstat_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    for p in event.paths {
        let raw = match event.kind {
            EventKind::Access(AccessKind::Close(AccessMode::Write)) => Raw::Written(p),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                if lstat_dir(&p) {
                    Raw::Dir(p)
                } else {
                    Raw::Written(p)
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => Raw::Gone(p),
            EventKind::Modify(ModifyKind::Data(_)) => Raw::Busy(p),
            EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
                Raw::Dir(p)
            }
            EventKind::Create(_) => match std::fs::symlink_metadata(&p) {
                // `ln -s` makes no CLOSE_WRITE: the link is complete now.
                Ok(m) if m.file_type().is_symlink() => Raw::Written(p),
                Ok(m) if m.is_dir() => Raw::Dir(p),
                _ => Raw::Busy(p),
            },
            EventKind::Remove(_) => Raw::Gone(p),
            _ => continue,
        };
        out.push(raw);
    }
}

/// Watches the config directory, referenced files and cache trees on its
/// own thread. Dropping it stops the thread.
#[derive(Debug)]
pub struct Watcher {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Msg::Fs(_) => "Fs",
            Msg::Ctl(_) => "Ctl",
        })
    }
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "strand-watch thread stopped")
}

impl Watcher {
    /// Start watching. When this returns, every directory is watched and
    /// every module file's baseline hash is recorded, so a save made right
    /// after is reported. Start the watcher before loading files.
    pub fn spawn(
        config: Option<ConfigWatch>,
        options: Options,
        sink: EventSink,
    ) -> io::Result<Watcher> {
        let (tx, rx) = mpsc::channel();
        let fs_tx = tx.clone();
        let notify = notify::recommended_watcher(move |ev| {
            let _ = fs_tx.send(Msg::Fs(ev));
        })
        .map_err(io::Error::other)?;
        let core = Core::new(NotifyBackend(notify), options.clone(), config);
        let thread = std::thread::Builder::new()
            .name("strand-watch".into())
            .spawn(move || run(core, rx, sink, options))?;
        Ok(Watcher {
            tx,
            thread: Some(thread),
        })
    }

    fn call(&self, make: impl FnOnce(Sender<()>) -> Ctl) -> io::Result<()> {
        let (ack, done) = mpsc::channel();
        self.tx.send(Msg::Ctl(make(ack))).map_err(|_| stopped())?;
        done.recv().map_err(|_| stopped())
    }

    /// Watch a referenced file (settings TOML, wallpaper, shader): its
    /// directory and every symlink's directory. It need not exist yet.
    pub fn watch_file(&self, path: impl Into<PathBuf>, role: Role) -> io::Result<()> {
        let path = path.into();
        self.call(|ack| Ctl::AddFile(path, role, ack))
    }

    /// Stop watching a file added with [`Watcher::watch_file`].
    pub fn unwatch_file(&self, path: impl Into<PathBuf>) -> io::Result<()> {
        let path = path.into();
        self.call(|ack| Ctl::RemoveFile(path, ack))
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
    /// (`hash_bytes`); that write is not reported. Call it before writing.
    pub fn register_own_write(&self, path: impl Into<PathBuf>, hash: ContentHash) {
        let _ = self.tx.send(Msg::Ctl(Ctl::OwnWrite(path.into(), hash)));
    }

    /// Rescan everything now (`strand reload`); the batch is marked
    /// [`RescanReason::Requested`].
    pub fn rescan(&self) {
        let _ = self.tx.send(Msg::Ctl(Ctl::Rescan));
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Ctl(Ctl::Stop));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(mut core: Core<NotifyBackend>, rx: Receiver<Msg>, sink: EventSink, opts: Options) {
    let mut next_poll = Instant::now() + opts.poll_interval;
    let mut raws = Vec::new();
    loop {
        let now = Instant::now();
        let mut wake = core.deadline();
        if core.has_polled_dirs() {
            wake = Some(wake.map_or(next_poll, |w| w.min(next_poll)));
        }
        let msg = match wake {
            Some(w) => rx.recv_timeout(w.saturating_duration_since(now)),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Msg::Fs(Ok(ev))) => {
                translate(ev, &mut raws);
                let now = Instant::now();
                for raw in raws.drain(..) {
                    core.on_raw(raw, now);
                }
            }
            Ok(Msg::Fs(Err(e))) => core.notice(Notice::Backend(e.to_string()), Instant::now()),
            Ok(Msg::Ctl(ctl)) => match ctl {
                Ctl::AddFile(p, role, ack) => {
                    core.add_file(&p, role);
                    let _ = ack.send(());
                }
                Ctl::RemoveFile(p, ack) => {
                    core.remove_file(&p);
                    let _ = ack.send(());
                }
                Ctl::AddTree(p, depth, kind, ack) => {
                    core.add_tree(&p, depth, kind);
                    let _ = ack.send(());
                }
                Ctl::OwnWrite(p, h) => core.register_own_write(&p, h, Instant::now()),
                Ctl::Rescan => core.request_rescan(RescanReason::Requested, Instant::now()),
                Ctl::Stop => return,
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
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
    use notify::event::Flag;

    #[test]
    fn translation_keeps_completed_writes_only() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bar.strand");
        std::fs::write(&file, "x").unwrap();
        let link = tmp.path().join("link.strand");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let ev = |kind: EventKind, p: &Path| notify::Event::new(kind).add_path(p.to_path_buf());
        let mut out = Vec::new();
        translate(
            ev(
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &file,
            ),
            &mut out,
        );
        translate(ev(EventKind::Create(CreateKind::File), &file), &mut out);
        translate(ev(EventKind::Create(CreateKind::File), &link), &mut out);
        translate(
            ev(
                EventKind::Access(AccessKind::Close(AccessMode::Write)),
                &file,
            ),
            &mut out,
        );
        translate(
            ev(
                EventKind::Access(AccessKind::Close(AccessMode::Read)),
                &file,
            ),
            &mut out,
        );
        translate(
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &file),
            &mut out,
        );
        translate(
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                tmp.path(),
            ),
            &mut out,
        );
        translate(
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), &file),
            &mut out,
        );
        translate(ev(EventKind::Remove(RemoveKind::File), &file), &mut out);
        translate(
            notify::Event::new(EventKind::Other).set_flag(Flag::Rescan),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                Raw::Busy(file.clone()),
                Raw::Busy(file.clone()),
                Raw::Written(link),
                Raw::Written(file.clone()),
                Raw::Written(file.clone()),
                Raw::Dir(tmp.path().to_path_buf()),
                Raw::Gone(file),
                Raw::Overflow,
            ]
        );
    }
}

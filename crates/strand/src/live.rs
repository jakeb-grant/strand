//! The live-reload pipeline's threads (design.md, "The pipeline";
//! `docs/architecture.md`, "Threads"): the `strand-watch` watcher and the
//! compiler worker that owns the [`Loader`].
//!
//! - The watcher starts first, with the module set from
//!   [`find_files`] and a rescan callback that calls it again, so a save
//!   made while the config loads is not lost.
//! - The worker waits on one channel: a poke from the watcher's sink
//!   (its batches are drained together, so a burst is one compile),
//!   `strand reload` from logic, and logic's list of settings files to
//!   watch. Each module batch is compiled off the logic thread: the
//!   largest consistent set is committed, the rest held back with its
//!   diagnostics, and the last good sources are cached (keyed by their
//!   hashes, the compiler version and the schema hash) so a config
//!   broken at boot still runs its last good version.
//! - What it made goes to logic as [`FromWorker`]; logic commits it
//!   ([`Instance::reload`](strand_compiler::instantiate::Instance::reload)).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

use strand_compiler::reconcile::loader::{Cache, Loader, Outcome};
use strand_compiler::schema::Schema;
use strand_compiler::source::find_files;
use strand_watch::{
    ChangeEvent, ChangeKind, ConfigWatch, FileBatch, ModuleSet, Notice, Options, Role, Watcher,
};

/// What the worker hands the logic thread.
#[derive(Debug)]
pub enum FromWorker {
    /// A load attempt: a build to commit (or none), what was held back,
    /// the diagnostics of the attempt.
    Loaded(Box<Loaded>),
    /// Settings files that changed (`Instance::reload_settings`).
    Settings(Vec<PathBuf>),
}

/// One load attempt and how it came about.
#[derive(Debug)]
pub struct Loaded {
    pub outcome: Outcome,
    /// `strand reload` asked for it.
    pub requested: bool,
    /// The IPC clients whose `strand reload` this load answers.
    pub clients: Vec<u64>,
    /// `strand reload --hard`.
    pub hard: bool,
    /// The module files the batch named.
    pub files: Vec<PathBuf>,
    /// The latest file event behind it (save time, as the watcher saw
    /// it); `None` for a requested reload.
    pub saved: Option<Instant>,
    /// When the worker started compiling.
    pub started: Instant,
    /// Watcher notices (polled directories, stalled writes, module-set
    /// problems), as text.
    pub notices: Vec<String>,
}

/// What the worker waits on.
#[derive(Debug)]
pub enum Job {
    /// The watcher sent something: drain its channel.
    Poll,
    /// `strand reload [--hard]`, from the IPC client `client` (answered
    /// with the event of the load it causes).
    Reload {
        hard: bool,
        client: Option<u64>,
    },
    /// Watch these referenced files (settings files the program mounts).
    Referenced(Vec<(PathBuf, Role)>),
    Stop,
}

/// The worker thread and its watcher.
#[derive(Debug)]
pub struct Worker {
    jobs: mpsc::Sender<Job>,
    watcher: Option<Arc<Watcher>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Start watching `dir`, load the config (the boot outcome is
    /// returned, so the shell starts with it), then compile every later
    /// change on the `strand-compile` thread and send the results to
    /// `out`. `cache`: the last-good cache directory.
    pub fn spawn(
        dir: &Path,
        cache: Option<PathBuf>,
        out: calloop::channel::Sender<FromWorker>,
    ) -> io::Result<(Worker, Outcome)> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (sink, events) = strand_watch::channel();
        let poke = jobs.clone();
        let sink = sink.with_waker(move || {
            let _ = poke.send(Job::Poll);
        });
        let modules = module_set(dir).unwrap_or_default();
        let root = dir.to_path_buf();
        let config = ConfigWatch {
            root: root.clone(),
            modules,
            rescan: Box::new(move || module_set(&root)),
        };
        // The watcher first, then the load: a save made in between is
        // reported.
        let watcher = match Watcher::spawn(Some(config), Options::default(), sink) {
            Ok(w) => Some(Arc::new(w)),
            Err(e) => {
                log::warn!("live reload is off: cannot watch {}: {e}", dir.display());
                None
            }
        };
        let mut loader = Loader::new(dir, Schema::builtin().clone(), cache);
        let boot = loader.boot();
        if let Some(e) = loader.cache_error() {
            log::warn!("last-good cache: {e}");
        }
        let shared = watcher.clone();
        let thread = std::thread::Builder::new()
            .name("strand-compile".into())
            .spawn(move || run(loader, watcher, rx, events, out))?;
        Ok((
            Worker {
                jobs,
                watcher: shared,
                thread: Some(thread),
            },
            boot,
        ))
    }

    /// Strand's own writes (persisted cells, settings write-back) are
    /// registered with the watcher before they land, so they never come
    /// back as changes (design.md, "The pipeline" step 2).
    pub fn register_own_writes(&self, storage: &strand_compiler::instantiate::Storage) {
        let Some(w) = self.watcher.clone() else {
            return;
        };
        let observe = move |ow: &strand_core::OwnWrite<'_>| {
            if let Some(bytes) = ow.content {
                let hash = strand_watch::hash_bytes(bytes);
                w.register_own_write(ow.target, hash);
                if ow.path != ow.target {
                    w.register_own_write(ow.path, hash);
                }
            }
        };
        // One observer per IO thread: the persist store's covers the
        // settings store that shares its thread.
        match (&storage.persist, &storage.settings) {
            (Some(p), _) => p.on_written(observe),
            (None, Some(s)) => s.on_written(observe),
            (None, None) => {}
        }
    }

    /// A handle that sends jobs (logic's `strand reload`, settings files).
    pub fn jobs(&self) -> mpsc::Sender<Job> {
        self.jobs.clone()
    }
}

impl Worker {
    /// Stop the compiler worker and its watcher and wait for both: `Err`
    /// with the panic payload if either thread panicked (dropping a
    /// `Worker` stops them too, and ignores how they ended).
    pub fn join(mut self) -> std::thread::Result<()> {
        let _ = self.jobs.send(Job::Stop);
        let worker = match self.thread.take() {
            Some(t) => t.join(),
            None => Ok(()),
        };
        // The worker thread held the other handle: it is the last one.
        let watcher = match self.watcher.take().map(Arc::try_unwrap) {
            Some(Ok(w)) => w.join(),
            Some(Err(_)) | None => Ok(()),
        };
        worker.and(watcher)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The last-good cache directory (`$XDG_CACHE_HOME/strand/last-good`),
/// unless `STRAND_NO_CACHE` is set.
pub fn cache_dir() -> Option<PathBuf> {
    if std::env::var_os("STRAND_NO_CACHE").is_some() {
        return None;
    }
    Cache::default_dir()
}

/// [`find_files`] as the watcher's module set.
fn module_set(dir: &Path) -> io::Result<ModuleSet> {
    let d = find_files(dir)?;
    Ok(ModuleSet {
        files: d.files,
        dirs: d.dirs,
        errors: d
            .errors
            .into_iter()
            .map(|(p, e)| (p, e.to_string()))
            .collect(),
        too_deep: d.too_deep,
    })
}

fn notice_text(n: &Notice) -> String {
    match n {
        Notice::Polling { dir, reason } => format!("polling {} ({reason:?})", dir.display()),
        Notice::RescanFailed(e) => format!("module rescan failed: {e}"),
        Notice::Backend(e) => format!("watcher: {e}"),
        Notice::StalledWrite(p) => format!("{}: read while still being written", p.display()),
        Notice::ModuleSet { errors, too_deep } => {
            let mut parts: Vec<String> = errors
                .iter()
                .map(|(p, e)| format!("{}: {e}", p.display()))
                .collect();
            parts.extend(
                too_deep
                    .iter()
                    .map(|p| format!("{}: too deep to load", p.display())),
            );
            parts.join("; ")
        }
    }
}

fn run(
    mut loader: Loader,
    watcher: Option<Arc<Watcher>>,
    jobs: mpsc::Receiver<Job>,
    events: mpsc::Receiver<ChangeEvent>,
    out: calloop::channel::Sender<FromWorker>,
) {
    while let Ok(job) = jobs.recv() {
        // Everything queued now is one batch of work.
        let mut queue = vec![job];
        while let Ok(j) = jobs.try_recv() {
            queue.push(j);
        }
        let mut batches: Vec<FileBatch> = Vec::new();
        let mut reload: Option<bool> = None;
        let mut clients: Vec<u64> = Vec::new();
        let mut referenced: Option<Vec<(PathBuf, Role)>> = None;
        for j in queue {
            match j {
                Job::Poll => {}
                Job::Reload { hard, client } => {
                    reload = Some(reload.unwrap_or(false) || hard);
                    clients.extend(client);
                }
                Job::Referenced(r) => referenced = Some(r),
                Job::Stop => return,
            }
        }
        while let Ok(ev) = events.try_recv() {
            match ev {
                ChangeEvent::Files(b) => batches.push(b),
                // Portal and compositor events belong to the services
                // (M3); nothing sends them here yet.
                ChangeEvent::System(_) | ChangeEvent::Compositor(_) => {}
            }
        }
        if let (Some(r), Some(w)) = (referenced, &watcher)
            && let Err(e) = w.set_referenced(r)
        {
            log::warn!("watching settings files: {e}");
        }
        let started = Instant::now();
        let mut modules: Vec<(PathBuf, bool)> = Vec::new();
        let mut settings: Vec<PathBuf> = Vec::new();
        let mut rescan = false;
        let mut saved: Option<Instant> = None;
        let mut notices = Vec::new();
        for b in &batches {
            rescan |= b.rescan.is_some();
            saved = saved.max(Some(b.last_event));
            notices.extend(b.notices.iter().map(notice_text));
            for c in &b.changes {
                match c.role {
                    Role::Module => {
                        modules.retain(|(p, _)| *p != c.path);
                        modules.push((c.path.clone(), c.kind != ChangeKind::Removed));
                    }
                    Role::Settings if !settings.contains(&c.path) => {
                        settings.push(c.path.clone());
                    }
                    _ => {}
                }
            }
        }
        for n in &notices {
            log::warn!("{n}");
        }
        if !settings.is_empty() && out.send(FromWorker::Settings(settings)).is_err() {
            return;
        }
        if let (Some(_), Some(w)) = (reload, &watcher) {
            // The watcher lists the module set again too; what it finds
            // the loader has already read (no second compile).
            w.rescan();
        }
        let files: Vec<PathBuf> = modules.iter().map(|(p, _)| p.clone()).collect();
        let outcome = if reload.is_some() || rescan {
            loader.rescan()
        } else if !modules.is_empty() {
            loader.changed(modules)
        } else {
            continue;
        };
        if let Some(e) = loader.cache_error() {
            log::warn!("last-good cache: {e}");
        }
        // The watcher's own re-listing after a reload, or a save that
        // changed nothing the loader keeps: nothing to report. A save
        // that reverts a broken one to the last good text changes
        // nothing either, but the problems it clears are reported gone
        // (`cleared`: the overlay closes, `strand watch` hears it).
        //
        // The watcher's re-listing after a `strand reload` on a broken
        // config finds the files as the reload did: the loader repeats
        // that attempt's problems without compiling, and they are not
        // reported twice.
        let quiet = outcome.repeated
            || (outcome.build.is_none()
                && outcome.diagnostics.is_empty()
                && outcome.held.is_empty()
                && outcome.unreadable.is_empty()
                && !outcome.cleared);
        if quiet && reload.is_none() {
            continue;
        }
        let loaded = Loaded {
            outcome,
            requested: reload.is_some(),
            clients,
            hard: reload == Some(true),
            files,
            saved,
            started,
            notices,
        };
        if out.send(FromWorker::Loaded(Box::new(loaded))).is_err() {
            return;
        }
    }
}

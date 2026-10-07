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
use strand_compiler::source::find_files;
use strand_watch::{
    CacheKind, ChangeEvent, ChangeKind, ConfigWatch, FileBatch, ModuleSet, Notice, Options, Role,
    Watcher,
};

/// What the worker hands the logic thread.
#[derive(Debug)]
pub enum FromWorker {
    /// A load attempt: a build to commit (or none), what was held back,
    /// the diagnostics of the attempt.
    Loaded(Box<Loaded>),
    /// Settings files that changed, read on the worker
    /// (`Instance::reload_settings_with`).
    Settings(Vec<SettingsChange>),
    /// Wallpapers and imported palette files that changed
    /// (`Instance::theme_files_changed`).
    Theme(Vec<PathBuf>),
}

/// A settings file that changed, as the worker read it (with its
/// overlay): `None` when the worker had no sources for it (the logic
/// thread reads it then).
#[derive(Debug)]
pub struct SettingsChange {
    pub path: PathBuf,
    pub read: Option<strand_core::SettingsRead>,
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
    /// Watch these referenced files (settings files the program mounts,
    /// wallpapers and imported palette files the theme reads); `settings`
    /// reads the settings files when they change.
    Referenced {
        files: Vec<(PathBuf, Role)>,
        settings: Vec<strand_core::SettingsSources>,
    },
    /// Watch these cache sources ([`cache_sources`]) and tell `changed`
    /// which cache a change under them invalidates.
    Caches {
        sources: Vec<CacheSource>,
        changed: CacheSink,
    },
    Stop,
}

/// Called (on the worker thread) with each cache a batch invalidated.
pub struct CacheSink(pub Box<dyn Fn(CacheKind) + Send>);

impl std::fmt::Debug for CacheSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheSink")
    }
}

/// A cache source the watcher follows (design.md, "Change sources": apps,
/// icons, fonts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheSource {
    /// A directory tree, to `depth` directories below it.
    Tree(PathBuf, usize, CacheKind),
    /// One file (GTK's settings, which name the icon theme).
    File(PathBuf, CacheKind),
}

/// What strand run watches for its caches: every `applications/`
/// directory the `apps` service reads (and their subdirectories), every
/// icon base directory to one level (a theme's `index.theme` and
/// `icon-theme.cache`, a theme installed or removed) and GTK's settings
/// (the icon theme's name), and the font directories fontconfig reads by
/// default (`$XDG_DATA_HOME/fonts`, `~/.fonts`, each
/// `$XDG_DATA_DIRS/fonts`, three levels down) with fontconfig's user
/// configuration (`$XDG_CONFIG_HOME/fontconfig`).
pub fn cache_sources() -> Vec<CacheSource> {
    let mut out = Vec::new();
    for d in strand_services::apps::Config::current().dirs {
        out.push(CacheSource::Tree(d, 3, CacheKind::Apps));
    }
    for d in strand_icons::base_dirs() {
        out.push(CacheSource::Tree(d, 1, CacheKind::Icons));
    }
    for f in strand_icons::theme_setting_files() {
        out.push(CacheSource::File(f, CacheKind::Icons));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut fonts = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    fonts.extend(data_home.map(|d| d.join("fonts")));
    fonts.extend(home.as_ref().map(|h| h.join(".fonts")));
    let data = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    fonts.extend(
        data.split(':')
            .filter(|d| d.starts_with('/'))
            .map(|d| Path::new(d).join("fonts")),
    );
    let mut seen = std::collections::HashSet::new();
    for d in fonts.into_iter().filter(|d| seen.insert(d.clone())) {
        out.push(CacheSource::Tree(d, 3, CacheKind::Fonts));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    if let Some(c) = config {
        out.push(CacheSource::Tree(c.join("fontconfig"), 1, CacheKind::Fonts));
    }
    out
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
        let mut loader = Loader::new(dir, crate::services::schema().clone(), cache);
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
        // A weak handle: the IO thread can outlive the worker (a drain that
        // times out leaves it running), and the worker's handle must stay
        // the last strong one so `join` can stop the watcher and see how
        // it ended.
        let Some(w) = self.watcher.as_ref().map(Arc::downgrade) else {
            return;
        };
        let observe = move |ow: &strand_core::OwnWrite<'_>| {
            let Some(w) = w.upgrade() else {
                return;
            };
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
        // The worker thread held the other strong handle, and the own-write
        // observer holds a weak one: this is the last, unless an IO write
        // is registering right now.
        let watcher = match self.watcher.take().map(Arc::try_unwrap) {
            Some(Ok(w)) => w.join(),
            Some(Err(w)) => {
                log::warn!(
                    "the watcher is still shared ({} handles): stopped when the last goes, not joined",
                    Arc::strong_count(&w)
                );
                Ok(())
            }
            None => Ok(()),
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
    // The referenced files the watcher has, so a newly registered one is
    // read once more after its registration (below).
    let mut registered: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // How to read each settings file, off the logic thread.
    let mut sources: std::collections::HashMap<PathBuf, strand_core::SettingsSources> =
        std::collections::HashMap::new();
    // Who is told when a cache source changes.
    let mut caches: Option<CacheSink> = None;
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
                Job::Caches { sources, changed } => {
                    if let Some(w) = &watcher {
                        for src in &sources {
                            let r = match src {
                                CacheSource::Tree(d, depth, kind) => w.watch_tree(d, *depth, *kind),
                                CacheSource::File(f, kind) => w.watch_file(f, Role::Cache(*kind)),
                            };
                            if let Err(e) = r {
                                log::warn!("watching {src:?}: {e}");
                            }
                        }
                    }
                    caches = Some(changed);
                }
                Job::Poll => {}
                Job::Reload { hard, client } => {
                    reload = Some(reload.unwrap_or(false) || hard);
                    clients.extend(client);
                }
                Job::Referenced { files, settings } => {
                    sources = settings
                        .into_iter()
                        .map(|s| (s.path().to_path_buf(), s))
                        .collect();
                    referenced = Some(files);
                }
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
        let mut settings: Vec<PathBuf> = Vec::new();
        let mut theme: Vec<PathBuf> = Vec::new();
        if let (Some(r), Some(w)) = (referenced, &watcher) {
            match w.set_referenced(r.clone()) {
                // A file edited after the program read it and before the
                // watcher had it (a save right after boot or a reload)
                // was not seen: each newly registered file is read again
                // now that later edits are (unchanged content changes
                // nothing; a wallpaper's stamp tells it).
                Ok(()) => {
                    for (path, role) in &r {
                        if registered.contains(path) {
                            continue;
                        }
                        match role {
                            Role::Settings => settings.push(path.clone()),
                            Role::Wallpaper | Role::Other => theme.push(path.clone()),
                            _ => {}
                        }
                    }
                    registered = r.into_iter().map(|(p, _)| p).collect();
                }
                Err(e) => log::warn!("watching settings files: {e}"),
            }
        }
        let started = Instant::now();
        let mut modules: Vec<(PathBuf, bool)> = Vec::new();
        let mut rescan = false;
        let mut saved: Option<Instant> = None;
        let mut notices = Vec::new();
        let mut invalid: std::collections::BTreeSet<CacheKind> = std::collections::BTreeSet::new();
        for b in &batches {
            rescan |= b.rescan.is_some();
            if b.rescan == Some(strand_watch::RescanReason::Overflow) {
                // Events were lost: every cache may be stale.
                invalid.extend([CacheKind::Apps, CacheKind::Icons, CacheKind::Fonts]);
            }
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
                    Role::Wallpaper | Role::Other if !theme.contains(&c.path) => {
                        theme.push(c.path.clone());
                    }
                    Role::Cache(kind) => {
                        invalid.insert(kind);
                    }
                    _ => {}
                }
            }
        }
        if let Some(sink) = &caches {
            for kind in &invalid {
                (sink.0)(*kind);
            }
        }
        for n in &notices {
            log::warn!("{n}");
        }
        if !settings.is_empty() {
            // Read here: a slow or hung home directory stalls this
            // thread, never a frame.
            let changes = settings
                .into_iter()
                .map(|path| SettingsChange {
                    read: sources.get(&path).map(|s| s.read()),
                    path,
                })
                .collect();
            if out.send(FromWorker::Settings(changes)).is_err() {
                return;
            }
        }
        if !theme.is_empty() && out.send(FromWorker::Theme(theme)).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A referenced file is read once more as soon as the watcher has
    /// it: an edit made between the program's read and the registration
    /// (a save right after boot) is not lost. Registering the same list
    /// again sends nothing.
    #[test]
    fn newly_referenced_files_are_read_again_once_registered() {
        let dir = std::env::temp_dir().join(format!("strand-live-ref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/shell.strand"), "").unwrap();
        let prefs = dir.join("config/prefs.toml");
        let wall = dir.join("wall.png");
        std::fs::write(&prefs, "gap = 6\n").unwrap();
        std::fs::write(&wall, b"png").unwrap();
        let (out, rx) = calloop::channel::channel::<FromWorker>();
        let (worker, _boot) = Worker::spawn(&dir.join("config"), None, out).unwrap();
        let refs = vec![
            (prefs.clone(), Role::Settings),
            (wall.clone(), Role::Wallpaper),
        ];
        let job = |files: &Vec<(PathBuf, Role)>| Job::Referenced {
            files: files.clone(),
            settings: Vec::new(),
        };
        worker.jobs().send(job(&refs)).unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while got.len() < 2 && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(m) => got.push(m),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert!(
            got.iter().any(
                |m| matches!(m, FromWorker::Settings(c) if c.len() == 1 && c[0].path == prefs)
            ),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|m| matches!(m, FromWorker::Theme(p) if p == std::slice::from_ref(&wall))),
            "{got:?}"
        );
        worker.jobs().send(job(&refs)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            rx.try_recv().is_err(),
            "nothing new registered, nothing re-read"
        );
        assert!(worker.join().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// design.md, "Change sources": `applications/`, an icon base
    /// directory (`index.theme`), GTK's settings and a font directory are
    /// watched on the worker, and a change under each tells the sink which
    /// cache it invalidates (once per batch), and nothing else.
    #[test]
    fn cache_sources_report_the_cache_they_invalidate() {
        let dir = std::env::temp_dir().join(format!("strand-live-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in [
            "config",
            "applications",
            "icons/Theme",
            "fonts/truetype",
            "gtk-3.0",
        ] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("config/shell.strand"), "").unwrap();
        let gtk = dir.join("gtk-3.0/settings.ini");
        std::fs::write(&gtk, "[Settings]\n").unwrap();
        let (out, rx) = calloop::channel::channel::<FromWorker>();
        let (worker, _boot) = Worker::spawn(&dir.join("config"), None, out).unwrap();
        let (tx, kinds) = std::sync::mpsc::channel::<CacheKind>();
        let tx = std::sync::Mutex::new(tx);
        worker
            .jobs()
            .send(Job::Caches {
                sources: vec![
                    CacheSource::Tree(dir.join("applications"), 3, CacheKind::Apps),
                    CacheSource::Tree(dir.join("icons"), 1, CacheKind::Icons),
                    CacheSource::File(gtk.clone(), CacheKind::Icons),
                    CacheSource::Tree(dir.join("fonts"), 3, CacheKind::Fonts),
                ],
                changed: CacheSink(Box::new(move |k| {
                    let _ = tx.lock().map(|t| t.send(k));
                })),
            })
            .unwrap();
        // The job is taken before the next poke: give the watches a moment.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let next = || {
            kinds
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("no cache change reported")
        };
        std::fs::write(dir.join("applications/foot.desktop"), "[Desktop Entry]\n").unwrap();
        assert_eq!(next(), CacheKind::Apps);
        std::fs::write(dir.join("icons/Theme/index.theme"), "[Icon Theme]\n").unwrap();
        assert_eq!(next(), CacheKind::Icons);
        std::fs::write(&gtk, "[Settings]\ngtk-icon-theme-name=Theme\n").unwrap();
        assert_eq!(next(), CacheKind::Icons);
        std::fs::write(dir.join("fonts/truetype/new.ttf"), "ttf").unwrap();
        assert_eq!(next(), CacheKind::Fonts);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(kinds.try_recv().is_err(), "once per change");
        assert!(rx.try_recv().is_err(), "no reload for a cache change");
        assert!(worker.join().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The own-write observer must not keep the watcher alive: `join`
    /// stops and joins it even with storage configured, so a watcher
    /// panic is reported.
    #[test]
    fn the_own_write_observer_leaves_the_watcher_joinable() {
        let dir = std::env::temp_dir().join(format!("strand-live-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(dir.join("config/shell.strand"), "").unwrap();
        let (out, _rx) = calloop::channel::channel::<FromWorker>();
        let (worker, _boot) = Worker::spawn(&dir.join("config"), None, out).unwrap();
        let storage =
            strand_compiler::instantiate::Storage::in_dirs(dir.join("state"), dir.join("config"));
        worker.register_own_writes(&storage);
        // The worker's handle and the compiler thread's: nothing else.
        let w = worker.watcher.as_ref().unwrap();
        assert_eq!(Arc::strong_count(w), 2);
        assert!(worker.join().is_ok());
        drop(storage);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

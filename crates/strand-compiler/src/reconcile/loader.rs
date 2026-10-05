//! The loader: which files a config is, which of their saved texts run,
//! and the last good version that survives a broken config across
//! restarts (design.md, "The pipeline").
//!
//! It is plain logic, no threads: the binary's compiler worker owns one
//! and feeds it the watcher's batches. The module set is
//! [`crate::source::find_files`]'s, never a second scan.
//!
//! - **Compile.** Each batch re-reads the changed files and compiles the
//!   config as one program (the checker is whole-program, so "changed
//!   modules and their dependents" is every module; parsing is cheap and
//!   checking a config takes milliseconds).
//! - **Commit the largest consistent set.** If the program with every
//!   changed file has errors, files with errors of their own are held
//!   back first; if what is left still fails (an error in an unchanged
//!   file that a changed one caused, `bar.strand:12: unknown prop
//!   "expanded"`), the changed file whose hold-back clears the most errors
//!   goes next, until the rest compiles. What compiles is committed; the
//!   rest is held back with the diagnostics of the whole attempt and
//!   retried with the next save.
//! - **Keep the last good tree.** Nothing with errors is ever committed.
//!   Each commit writes its sources to a cache keyed by their hash, the
//!   compiler version and the schema hash, so a config that is broken at
//!   boot runs its last good version (with the broken files held back
//!   on top of it).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Build, COMPILER_VERSION};
use crate::diagnostic::Diagnostic;
use crate::schema::Schema;
use crate::source::{SourceMap, find_files};

/// What one load or reload attempt did.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// A new build to commit (nothing changed, or nothing consistent:
    /// `None`).
    pub build: Option<Build>,
    /// Files whose new text the build commits.
    pub committed: Vec<PathBuf>,
    /// Changed files held back (they, or what they break, have errors),
    /// and files that cannot be read: those keep running their last good
    /// text (an unreadable file is never taken for a deleted one).
    pub held: Vec<PathBuf>,
    /// Errors and warnings of the whole attempt (every saved file), with
    /// the sources they point into. Empty when everything committed
    /// cleanly.
    pub diagnostics: Vec<Diagnostic>,
    pub sources: Arc<SourceMap>,
    /// Files that could not be read, and why.
    pub unreadable: Vec<(PathBuf, String)>,
    /// The build came from the last-good cache (a config broken at boot).
    pub from_cache: bool,
    /// Time spent compiling.
    pub compile_time: Duration,
}

impl Outcome {
    /// The attempt's errors (not warnings).
    pub fn errors(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_error()).count()
    }
}

/// See the module docs.
#[derive(Debug)]
pub struct Loader {
    root: PathBuf,
    schema: Schema,
    /// The saved text of every module file (or why it cannot be read).
    disk: BTreeMap<PathBuf, Result<Arc<str>, String>>,
    /// The texts of the running build.
    live: BTreeMap<PathBuf, Arc<str>>,
    last: Option<Build>,
    cache: Option<Cache>,
    /// Directories the module set was found in (for the watcher).
    dirs: Vec<PathBuf>,
    /// Directories and entries the last listing could not read: running
    /// files under them are kept, not dropped.
    unlisted: Vec<(PathBuf, String)>,
    cache_error: Option<String>,
}

/// Compile attempts per batch at most (each held-back file costs one per
/// file still in the set).
const MAX_ATTEMPTS: usize = 64;

impl Loader {
    /// A loader for the config in `root` against `schema`, with a cache
    /// directory for last good sources (`None`: no cache).
    pub fn new(root: impl Into<PathBuf>, schema: Schema, cache: Option<PathBuf>) -> Self {
        let root = root.into();
        let cache = cache.map(|dir| Cache::new(dir, &root));
        Loader {
            root,
            schema,
            disk: BTreeMap::new(),
            live: BTreeMap::new(),
            last: None,
            cache,
            dirs: Vec::new(),
            unlisted: Vec::new(),
            cache_error: None,
        }
    }

    /// The config directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The module files, as last listed.
    pub fn files(&self) -> Vec<PathBuf> {
        self.disk.keys().cloned().collect()
    }

    /// Directories the module set lives in (link targets included).
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The running build.
    pub fn last(&self) -> Option<&Build> {
        self.last.as_ref()
    }

    /// The module set as `find_files` lists it now (the watcher's rescan
    /// callback calls the same).
    pub fn list(&mut self) -> io::Result<Vec<PathBuf>> {
        let found = find_files(&self.root)?;
        self.dirs = found.dirs;
        self.unlisted = found
            .errors
            .into_iter()
            .map(|(p, e)| (p, e.to_string()))
            .collect();
        Ok(found.files)
    }

    /// Boot: list and read every module, commit the largest consistent
    /// set. With errors and no last good build yet, the cache's last good
    /// sources (if they still compile with this compiler and schema) are
    /// the base the consistent set is taken on.
    pub fn boot(&mut self) -> Outcome {
        let files = match self.list() {
            Ok(f) => f,
            Err(e) => {
                return Outcome {
                    unreadable: vec![(self.root.clone(), e.to_string())],
                    ..Outcome::default()
                };
            }
        };
        for f in &files {
            self.disk.insert(f.clone(), read(f));
        }
        // Broken at boot: start from the cached last good sources, if
        // any, and take the saved files on top as far as they are
        // consistent.
        let all: BTreeSet<PathBuf> = self.disk.keys().cloned().collect();
        let broken = crate::compile_with(&self.assemble(&all), &self.schema).errors() > 0;
        let cached = match broken {
            true => self.cache.as_ref().and_then(|c| c.load(&self.schema)),
            false => None,
        };
        let Some(cached) = cached else {
            return self.reconcile(false);
        };
        let mut map = SourceMap::new();
        for (p, t) in &cached {
            map.add(p.display().to_string(), t.clone());
        }
        let started = Instant::now();
        let Ok(build) = Build::compile_with(None, map, &self.schema) else {
            return self.reconcile(false);
        };
        self.live = cached;
        self.last = Some(build);
        // Then the saved files on top of it, as far as they are
        // consistent.
        let mut out = self.reconcile(true);
        if out.build.is_none() {
            out.build = self.last.clone();
        }
        out.from_cache = true;
        out.compile_time += started.elapsed();
        out
    }

    /// Files changed on disk (`None`: removed): re-read them and commit
    /// what is consistent.
    pub fn changed(&mut self, paths: impl IntoIterator<Item = (PathBuf, bool)>) -> Outcome {
        for (p, exists) in paths {
            if exists {
                self.disk.insert(p.clone(), read(&p));
            } else {
                self.disk.remove(&p);
            }
        }
        self.reconcile(false)
    }

    /// `strand reload`: list the module set again, re-read every file and
    /// commit what is consistent.
    pub fn rescan(&mut self) -> Outcome {
        match self.list() {
            Ok(files) => {
                self.disk.clear();
                for f in &files {
                    self.disk.insert(f.clone(), read(f));
                }
                // A running file under a directory (or link) the listing
                // could not read is unreadable, not deleted: it keeps its
                // last good text.
                for p in self.live.keys() {
                    if self.disk.contains_key(p) {
                        continue;
                    }
                    if let Some((d, e)) = self.unlisted.iter().find(|(d, _)| p.starts_with(d)) {
                        self.disk
                            .insert(p.clone(), Err(format!("{}: {e}", d.display())));
                    }
                }
                self.reconcile(false)
            }
            Err(e) => Outcome {
                unreadable: vec![(self.root.clone(), e.to_string())],
                ..Outcome::default()
            },
        }
    }

    /// The source map of `live` with `take` taken from disk.
    fn assemble(&self, take: &BTreeSet<PathBuf>) -> SourceMap {
        let mut texts: BTreeMap<&PathBuf, Arc<str>> = BTreeMap::new();
        for (p, t) in &self.live {
            if !take.contains(p) {
                texts.insert(p, t.clone());
            }
        }
        for p in take {
            if let Some(Ok(t)) = self.disk.get(p) {
                texts.insert(p, t.clone());
            }
        }
        let mut map = SourceMap::new();
        for (p, t) in texts {
            map.add(p.display().to_string(), t);
        }
        map
    }

    fn reconcile(&mut self, force: bool) -> Outcome {
        let mut unreadable = Vec::new();
        let mut changed: BTreeSet<PathBuf> = BTreeSet::new();
        for (p, t) in &self.disk {
            match t {
                Ok(t) => {
                    if self.live.get(p) != Some(t) {
                        changed.insert(p.clone());
                    }
                }
                Err(e) => unreadable.push((p.clone(), e.clone())),
            }
        }
        // Only a file gone from the listing (or reported removed) is
        // removed; one that cannot be read keeps its live text (it is not
        // in `changed`, so `assemble` keeps it) and is held back.
        for p in self.live.keys() {
            if !self.disk.contains_key(p) {
                changed.insert(p.clone());
            }
        }
        let unread: Vec<PathBuf> = unreadable.iter().map(|(p, _)| p.clone()).collect();
        if changed.is_empty() && !(force || self.last.is_none()) {
            return Outcome {
                held: unread,
                unreadable,
                ..Outcome::default()
            };
        }
        let started = Instant::now();
        let full = self.assemble(&changed);
        let compiled = crate::compile_with(&full, &self.schema);
        let mut out = Outcome {
            unreadable,
            ..Outcome::default()
        };
        if compiled.errors() == 0 {
            out.build = Some(self.commit(&changed, full, &compiled));
            out.committed = changed.into_iter().collect();
            out.held = unread;
            out.compile_time = started.elapsed();
            return out;
        }
        // Hold back what breaks, keep what does not.
        let names: BTreeMap<String, PathBuf> = changed
            .iter()
            .map(|p| (p.display().to_string(), p.clone()))
            .collect();
        let file_of = |map: &SourceMap, d: &Diagnostic| -> Option<PathBuf> {
            map.get(d.file()).and_then(|f| names.get(&f.name)).cloned()
        };
        let mut set = changed.clone();
        // Changed files with errors of their own go first.
        for d in compiled.diagnostics.iter().filter(|d| d.is_error()) {
            if let Some(p) = file_of(&full, d) {
                set.remove(&p);
            }
        }
        let mut attempts = 1;
        let mut done: Option<(SourceMap, crate::Compiled)> = None;
        while !set.is_empty() && attempts < MAX_ATTEMPTS {
            let map = self.assemble(&set);
            let c = crate::compile_with(&map, &self.schema);
            attempts += 1;
            if c.errors() == 0 {
                done = Some((map, c));
                break;
            }
            // Errors in a changed file still in the set: hold it back.
            let own: Vec<PathBuf> = c
                .diagnostics
                .iter()
                .filter(|d| d.is_error())
                .filter_map(|d| file_of(&map, d))
                .filter(|p| set.contains(p))
                .collect();
            if !own.is_empty() {
                for p in own {
                    set.remove(&p);
                }
                continue;
            }
            // Errors only in files that did not change: hold back the
            // changed file whose absence clears the most.
            let mut best: Option<(usize, PathBuf)> = None;
            for p in set.iter() {
                if attempts >= MAX_ATTEMPTS {
                    break;
                }
                let mut s = set.clone();
                s.remove(p);
                let errors = crate::compile_with(&self.assemble(&s), &self.schema).errors();
                attempts += 1;
                if best.as_ref().is_none_or(|(e, _)| errors < *e) {
                    best = Some((errors, p.clone()));
                }
                if errors == 0 {
                    break;
                }
            }
            match best {
                Some((_, p)) => {
                    set.remove(&p);
                }
                None => break,
            }
        }
        if let Some((map, c)) = done {
            out.build = Some(self.commit(&set, map, &c));
            out.committed = set.iter().cloned().collect();
        }
        out.held = changed.difference(&set).cloned().collect();
        if out.build.is_none() {
            out.held = changed.into_iter().collect();
            out.committed.clear();
        }
        out.held.extend(unread);
        out.diagnostics = compiled.diagnostics;
        out.sources = Arc::new(full);
        out.compile_time = started.elapsed();
        out
    }

    fn commit(&mut self, set: &BTreeSet<PathBuf>, map: SourceMap, c: &crate::Compiled) -> Build {
        for p in set {
            match self.disk.get(p) {
                Some(Ok(t)) => {
                    self.live.insert(p.clone(), t.clone());
                }
                // Unreadable: never in a committed set, but if it were,
                // its last good text stays.
                Some(Err(_)) => {}
                None => {
                    self.live.remove(p);
                }
            }
        }
        let build = Build::lowered(self.last.as_ref(), map, c, &self.schema);
        // Best effort: a cache that cannot be written only loses the
        // fallback for the next broken boot.
        if let Some(cache) = &self.cache {
            self.cache_error = cache
                .store(&self.live, &self.schema)
                .err()
                .map(|e| e.to_string());
        }
        self.last = Some(build.clone());
        build
    }

    /// Why the last good sources could not be cached, if they could not.
    pub fn cache_error(&self) -> Option<&str> {
        self.cache_error.as_deref()
    }
}

fn read(p: &Path) -> Result<Arc<str>, String> {
    match std::fs::read(p) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Ok(s.into()),
            Err(_) => Err("not UTF-8".to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// The last good sources of one config, under `dir/<hash of the config
/// path>`: a manifest (`key`, compiler version, schema hash, then one
/// relative path per line) and the files' texts.
#[derive(Debug)]
pub struct Cache {
    dir: PathBuf,
    root: PathBuf,
}

impl Cache {
    pub fn new(base: impl AsRef<Path>, root: &Path) -> Self {
        let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let tag = blake3::hash(canon.as_os_str().as_encoded_bytes()).to_hex();
        Cache {
            dir: base.as_ref().join(&tag.as_str()[..16]),
            root: root.to_path_buf(),
        }
    }

    /// `$XDG_CACHE_HOME/strand/last-good` (or `~/.cache/…`).
    pub fn default_dir() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CACHE_HOME") {
            Some(d) if Path::new(&d).is_absolute() => PathBuf::from(d),
            _ => {
                let home = std::env::var_os("HOME")?;
                let home = PathBuf::from(home);
                if !home.is_absolute() {
                    return None;
                }
                home.join(".cache")
            }
        };
        Some(base.join("strand").join("last-good"))
    }

    /// The cache key of `files` (relative paths and texts) for this
    /// compiler and `schema`.
    pub fn key(files: &BTreeMap<PathBuf, Arc<str>>, root: &Path, schema: &Schema) -> String {
        let mut h = blake3::Hasher::new();
        h.update(COMPILER_VERSION.as_bytes());
        h.update(b"\0");
        h.update(&schema.fingerprint());
        for (p, t) in files {
            let rel = p.strip_prefix(root).unwrap_or(p);
            h.update(rel.as_os_str().as_encoded_bytes());
            h.update(b"\0");
            h.update(blake3::hash(t.as_bytes()).as_bytes());
        }
        h.finalize().to_hex().to_string()
    }

    /// Write `files` as the last good sources (atomically: a new
    /// directory renamed over the old one). A no-op when the stored key
    /// is the same.
    pub fn store(&self, files: &BTreeMap<PathBuf, Arc<str>>, schema: &Schema) -> io::Result<()> {
        let key = Self::key(files, &self.root, schema);
        if let Ok(m) = std::fs::read_to_string(self.dir.join("manifest"))
            && m.lines().next() == Some(key.as_str())
        {
            return Ok(());
        }
        let parent = self
            .dir
            .parent()
            .ok_or_else(|| io::Error::other("no cache parent"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".{}.tmp.{}",
            self.dir
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let mut manifest = format!(
            "{key}\n{COMPILER_VERSION}\n{}\n",
            blake3::Hash::from(schema.fingerprint()).to_hex()
        );
        for (i, (p, t)) in files.iter().enumerate() {
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            std::fs::write(tmp.join(format!("{i}.strand")), t.as_bytes())?;
            manifest.push_str(&rel.display().to_string());
            manifest.push('\n');
        }
        std::fs::write(tmp.join("manifest"), manifest)?;
        let old = parent.join(format!(
            ".{}.old.{}",
            self.dir
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            std::process::id()
        ));
        let had = std::fs::rename(&self.dir, &old).is_ok();
        std::fs::rename(&tmp, &self.dir)?;
        if had {
            let _ = std::fs::remove_dir_all(&old);
        }
        Ok(())
    }

    /// The last good sources, if they were stored by this compiler
    /// version against this schema (absolute paths under the config).
    pub fn load(&self, schema: &Schema) -> Option<BTreeMap<PathBuf, Arc<str>>> {
        let m = std::fs::read_to_string(self.dir.join("manifest")).ok()?;
        let mut lines = m.lines();
        let key = lines.next()?;
        if lines.next()? != COMPILER_VERSION {
            return None;
        }
        let fp = blake3::Hash::from(schema.fingerprint()).to_hex();
        if lines.next()? != fp.as_str() {
            return None;
        }
        let mut out = BTreeMap::new();
        for (i, rel) in lines.enumerate() {
            let text = std::fs::read_to_string(self.dir.join(format!("{i}.strand"))).ok()?;
            out.insert(self.root.join(rel), Arc::from(text));
        }
        // A cache that does not hash to its own key is not trusted.
        (Self::key(&out, &self.root, schema) == key).then_some(out)
    }
}

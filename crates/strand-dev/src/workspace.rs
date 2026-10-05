//! Open documents, the configs they belong to, and their analyses.
//!
//! A document is checked with the rest of its config, by the rule `strand
//! check <file>` uses: the default config directory if the file is in its
//! module set ([`find_files`]); else a workspace folder that is itself a
//! config (it holds `.strand` files directly) and has the file in its
//! module set; else the file's own directory's module set; else the file
//! alone. Open documents replace their files' text on disk.
//!
//! An analysis is reused until a document opens, changes, saves or closes,
//! the client reports a watched file changed, or a file or directory it
//! read changed on disk (size or modification time), so edits made outside
//! the editor are seen even by clients that do not watch files.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use lsp_types::{Position, Range};
use strand_compiler::schema::Schema;
use strand_compiler::source::find_files;
use strand_compiler::syntax::Span;
use strand_compiler::{Compiled, FileId, SourceMap};

use crate::text::{Lines, path_to_uri, uri_to_path};

/// An open document.
#[derive(Clone, Debug)]
pub struct Doc {
    pub text: Arc<str>,
    pub version: i32,
}

/// Which files are checked together.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConfigKey {
    /// The module set of a directory (canonical).
    Dir(PathBuf),
    /// One document on its own (not on disk, or outside every module set).
    Single(String),
}

/// What a file or directory on disk looked like when it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(path: &Path) -> Option<Stamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(Stamp {
            path: path.to_path_buf(),
            len: m.len(),
            modified: m.modified().ok(),
        })
    }

    fn fresh(&self) -> bool {
        Stamp::of(&self.path).as_ref() == Some(self)
    }
}

/// One file of a config, as it is compiled.
#[derive(Debug)]
pub struct Input {
    pub uri: String,
    pub name: String,
    pub text: Arc<str>,
    /// Read from disk (rather than an open document): checked for changes.
    disk: Option<Stamp>,
}

impl Input {
    pub fn new(uri: String, name: String, text: Arc<str>) -> Self {
        Self {
            uri,
            name,
            text,
            disk: None,
        }
    }
}

/// One config, compiled.
#[derive(Debug)]
pub struct Analysis {
    pub key: ConfigKey,
    pub map: SourceMap,
    /// The URI of each file, by `FileId` index.
    pub uris: Vec<String>,
    lines: Vec<Lines>,
    pub compiled: Compiled,
    /// The schema it was checked against.
    pub schema: Arc<Schema>,
    /// Files and directories read from disk, to notice changes.
    stamps: Vec<Stamp>,
}

impl Analysis {
    /// Compiles `files` as one config.
    pub fn new(key: ConfigKey, files: Vec<Input>, schema: Arc<Schema>) -> Self {
        let mut map = SourceMap::new();
        let mut uris = Vec::new();
        let mut lines = Vec::new();
        let mut stamps = Vec::new();
        for f in files {
            lines.push(Lines::new(&f.text));
            map.add(f.name, f.text);
            uris.push(f.uri);
            stamps.extend(f.disk);
        }
        let compiled = strand_compiler::compile_with(&map, &schema);
        Self {
            key,
            map,
            uris,
            lines,
            compiled,
            schema,
            stamps,
        }
    }

    /// The same config with one file's text replaced.
    pub fn with_text(&self, file: FileId, text: &str) -> Analysis {
        self.with_texts(|id, old| if id == file { text.into() } else { old })
    }

    /// The same config with each file's text mapped by `f`.
    pub fn with_texts(&self, mut f: impl FnMut(FileId, Arc<str>) -> Arc<str>) -> Analysis {
        let files = self
            .map
            .iter()
            .map(|(id, s)| {
                Input::new(
                    self.uri(id).to_string(),
                    s.name.clone(),
                    f(id, s.text.clone()),
                )
            })
            .collect();
        Analysis::new(self.key.clone(), files, self.schema.clone())
    }

    /// Nothing it read from disk has changed since.
    fn fresh(&self) -> bool {
        self.stamps.iter().all(Stamp::fresh)
    }

    pub fn file(&self, uri: &str) -> Option<FileId> {
        let i = self.uris.iter().position(|u| u == uri)?;
        self.map.iter().nth(i).map(|(id, _)| id)
    }

    pub fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.map.iter().map(|(id, _)| id)
    }

    fn index(&self, file: FileId) -> usize {
        self.map.iter().position(|(id, _)| id == file).unwrap_or(0)
    }

    pub fn uri(&self, file: FileId) -> &str {
        self.uris.get(self.index(file)).map_or("", String::as_str)
    }

    pub fn text(&self, file: FileId) -> &str {
        self.map.get(file).map_or("", |f| &f.text)
    }

    pub fn range(&self, file: FileId, span: Span) -> Range {
        self.lines[self.index(file)].range(self.text(file), span)
    }

    pub fn position(&self, file: FileId, offset: u32) -> Position {
        self.lines[self.index(file)].position(self.text(file), offset)
    }

    pub fn offset(&self, file: FileId, pos: Position) -> u32 {
        self.lines[self.index(file)].offset(self.text(file), pos)
    }

    /// The parse of `file`.
    pub fn parse(&self, file: FileId) -> Option<&strand_compiler::syntax::Parse> {
        self.compiled.parses.iter().find(|p| p.file_id == file)
    }
}

/// `$XDG_CONFIG_HOME/strand`, else `$HOME/.config/strand` (the rule of
/// `strand check`'s `default_dir`).
pub fn default_dir(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let xdg = xdg_config_home
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    xdg.or_else(|| {
        home.filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".config"))
    })
    .map(|base| base.join("strand"))
}

/// Everything the server knows about files.
#[derive(Debug)]
pub struct Workspace {
    /// Workspace folders, canonical.
    pub roots: Vec<PathBuf>,
    /// The default config directory, canonical (when it exists).
    pub default_dir: Option<PathBuf>,
    pub docs: HashMap<String, Doc>,
    schema: Arc<Schema>,
    /// Bumped whenever a document opens, changes, saves or closes, or a
    /// watched file changes.
    generation: u64,
    cache: HashMap<ConfigKey, (u64, Arc<Analysis>)>,
    /// The config of each URI asked about, until files are added, removed
    /// or saved.
    configs: HashMap<String, ConfigKey>,
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `dir` is a config: it holds `.strand` files itself.
fn is_config_root(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let p = e.path();
            p.extension().is_some_and(|x| x == "strand")
                && !e.file_name().to_string_lossy().starts_with('.')
                && p.is_file()
        })
    })
}

impl Workspace {
    pub fn new(roots: Vec<PathBuf>, default_dir: Option<PathBuf>, schema: Arc<Schema>) -> Self {
        Self {
            roots: roots.iter().map(|r| canonical(r)).collect(),
            default_dir: default_dir.and_then(|d| std::fs::canonicalize(d).ok()),
            docs: HashMap::new(),
            schema,
            generation: 0,
            cache: HashMap::new(),
            configs: HashMap::new(),
        }
    }

    pub fn open(&mut self, uri: String, text: String, version: i32) {
        self.docs.insert(
            uri,
            Doc {
                text: text.into(),
                version,
            },
        );
        self.disk_changed();
    }

    pub fn change(&mut self, uri: &str, text: String, version: i32) {
        if let Some(d) = self.docs.get_mut(uri) {
            d.text = text.into();
            d.version = version;
        }
        self.generation += 1;
    }

    pub fn close(&mut self, uri: &str) {
        self.docs.remove(uri);
        self.disk_changed();
    }

    /// Files on disk may have been written, added or removed.
    pub fn disk_changed(&mut self) {
        self.generation += 1;
        self.configs.clear();
    }

    /// The config `uri` belongs to.
    pub fn config_of(&mut self, uri: &str) -> ConfigKey {
        if let Some(k) = self.configs.get(uri) {
            return k.clone();
        }
        let k = self.find_config(uri);
        self.configs.insert(uri.to_string(), k.clone());
        k
    }

    fn find_config(&self, uri: &str) -> ConfigKey {
        let Some(path) = uri_to_path(uri) else {
            return ConfigKey::Single(uri.to_string());
        };
        let path = canonical(&path);
        let in_set = |dir: &Path| {
            find_files(dir).is_ok_and(|found| found.files.iter().any(|f| canonical(f) == path))
        };
        // Inside the default config directory, that is the config (or
        // the file is alone, if it is hidden or too deep), as for the
        // runtime and `strand check`.
        if let Some(d) = self.default_dir.as_ref().filter(|d| path.starts_with(d)) {
            return if in_set(d) {
                ConfigKey::Dir(d.clone())
            } else {
                ConfigKey::Single(uri.to_string())
            };
        }
        // A workspace folder that is a config itself; the deepest first.
        let mut roots: Vec<&PathBuf> = self
            .roots
            .iter()
            .filter(|r| path.starts_with(r) && is_config_root(r))
            .collect();
        roots.sort_by_key(|r| std::cmp::Reverse(r.components().count()));
        for r in roots {
            if in_set(r) {
                return ConfigKey::Dir(r.clone());
            }
        }
        if let Some(parent) = path.parent()
            && in_set(parent)
        {
            return ConfigKey::Dir(parent.to_path_buf());
        }
        ConfigKey::Single(uri.to_string())
    }

    /// The analysis of a config, compiled now if anything changed since.
    pub fn analysis(&mut self, key: &ConfigKey) -> Arc<Analysis> {
        if let Some((generation, a)) = self.cache.get(key)
            && *generation == self.generation
        {
            if a.fresh() {
                return a.clone();
            }
            // Changed on disk behind the editor's back: which files
            // belong where may have changed too.
            self.configs.clear();
        }
        let (files, dirs) = self.files(key);
        let mut a = Analysis::new(key.clone(), files, self.schema.clone());
        a.stamps.extend(dirs);
        let a = Arc::new(a);
        self.cache.insert(key.clone(), (self.generation, a.clone()));
        a
    }

    /// The analysis holding `uri`.
    pub fn analysis_of(&mut self, uri: &str) -> Arc<Analysis> {
        let key = self.config_of(uri);
        self.analysis(&key)
    }

    /// Forgets the analysis of a config nothing has open.
    pub fn forget(&mut self, key: &ConfigKey) {
        self.cache.remove(key);
        self.configs.retain(|_, k| k != key);
    }

    /// Some open document belongs to `key`.
    pub fn has_open(&mut self, key: &ConfigKey) -> bool {
        let uris: Vec<String> = self.docs.keys().cloned().collect();
        uris.iter().any(|u| self.config_of(u) == *key)
    }

    /// `key` is a directory under a workspace folder or the default
    /// config directory: its diagnostics stay shown with nothing open.
    pub fn is_workspace(&self, key: &ConfigKey) -> bool {
        match key {
            ConfigKey::Dir(d) => self
                .roots
                .iter()
                .chain(&self.default_dir)
                .any(|r| d.starts_with(r)),
            ConfigKey::Single(_) => false,
        }
    }

    /// The files of a config, and the directories they were found in.
    fn files(&self, key: &ConfigKey) -> (Vec<Input>, Vec<Stamp>) {
        match key {
            ConfigKey::Single(uri) => {
                let name =
                    uri_to_path(uri).map_or_else(|| uri.clone(), |p| p.display().to_string());
                let mut input = Input::new(uri.clone(), name, "".into());
                if let Some(d) = self.docs.get(uri) {
                    input.text = d.text.clone();
                } else if let Some(p) = uri_to_path(uri) {
                    input.disk = Stamp::of(&p);
                    input.text = std::fs::read_to_string(p).unwrap_or_default().into();
                }
                (vec![input], Vec::new())
            }
            ConfigKey::Dir(dir) => {
                let found = find_files(dir).unwrap_or_default();
                // Open documents by canonical path.
                let open: HashMap<PathBuf, (&String, &Doc)> = self
                    .docs
                    .iter()
                    .filter_map(|(u, d)| Some((canonical(&uri_to_path(u)?), (u, d))))
                    .collect();
                let inputs: Vec<Input> = found
                    .files
                    .into_iter()
                    .filter_map(|path| {
                        let name = path.display().to_string();
                        match open.get(&canonical(&path)) {
                            Some((uri, doc)) => {
                                Some(Input::new((*uri).clone(), name, doc.text.clone()))
                            }
                            None => {
                                let disk = Stamp::of(&path);
                                let text = std::fs::read_to_string(&path).ok()?;
                                let mut i = Input::new(path_to_uri(&path), name, text.into());
                                i.disk = disk;
                                Some(i)
                            }
                        }
                    })
                    .collect();
                // A file added to or removed from a directory changes it.
                let dirs = found.dirs.iter().filter_map(|d| Stamp::of(d)).collect();
                (inputs, dirs)
            }
        }
    }
}

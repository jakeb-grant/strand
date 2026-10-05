//! Open documents, the configs they belong to, and their analyses.
//!
//! A document is checked with the rest of its config, as `strand check
//! <file>` does: the workspace folder holding it if the file is in that
//! folder's `.strand` module set ([`find_files`]), else its own directory's
//! module set, else alone. Open documents replace their files' text on
//! disk. Analyses are cached until a document changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::{Position, Range};
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
    /// The module set of a directory.
    Dir(PathBuf),
    /// One document on its own (not on disk, or outside every module set).
    Single(String),
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
}

impl Analysis {
    /// Compiles `files` (URI, name, text) as one config.
    pub fn new(key: ConfigKey, files: Vec<(String, String, Arc<str>)>) -> Self {
        let mut map = SourceMap::new();
        let mut uris = Vec::new();
        let mut lines = Vec::new();
        for (uri, name, text) in files {
            lines.push(Lines::new(&text));
            map.add(name, text);
            uris.push(uri);
        }
        let compiled = strand_compiler::compile(&map);
        Self {
            key,
            map,
            uris,
            lines,
            compiled,
        }
    }

    /// The same config with one file's text replaced.
    pub fn with_text(&self, file: FileId, text: &str) -> Analysis {
        let files = self
            .map
            .iter()
            .map(|(id, f)| {
                let t: Arc<str> = if id == file {
                    text.into()
                } else {
                    f.text.clone()
                };
                (self.uri(id).to_string(), f.name.clone(), t)
            })
            .collect();
        Analysis::new(self.key.clone(), files)
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

/// Everything the server knows about files.
#[derive(Debug, Default)]
pub struct Workspace {
    /// Workspace folders, canonical.
    pub roots: Vec<PathBuf>,
    pub docs: HashMap<String, Doc>,
    /// Bumped whenever a document opens, changes, saves or closes.
    generation: u64,
    cache: HashMap<ConfigKey, (u64, Arc<Analysis>)>,
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl Workspace {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots: roots.iter().map(|r| canonical(r)).collect(),
            ..Self::default()
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
        self.generation += 1;
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
        self.generation += 1;
    }

    /// Something on disk may have changed.
    pub fn touch(&mut self) {
        self.generation += 1;
    }

    /// The config `uri` belongs to.
    pub fn config_of(&self, uri: &str) -> ConfigKey {
        let Some(path) = uri_to_path(uri) else {
            return ConfigKey::Single(uri.to_string());
        };
        let path = canonical(&path);
        let parent = path.parent().map(Path::to_path_buf);
        let candidates = self
            .roots
            .iter()
            .filter(|r| path.starts_with(r))
            .cloned()
            .chain(parent);
        for dir in candidates {
            let Ok(found) = find_files(&dir) else {
                continue;
            };
            if found.files.iter().any(|f| canonical(f) == path) {
                return ConfigKey::Dir(dir);
            }
        }
        ConfigKey::Single(uri.to_string())
    }

    /// The analysis of a config, compiled now if anything changed since.
    pub fn analysis(&mut self, key: &ConfigKey) -> Arc<Analysis> {
        if let Some((generation, a)) = self.cache.get(key)
            && *generation == self.generation
        {
            return a.clone();
        }
        let a = Arc::new(Analysis::new(key.clone(), self.files(key)));
        self.cache.insert(key.clone(), (self.generation, a.clone()));
        a
    }

    /// The analysis holding `uri`.
    pub fn analysis_of(&mut self, uri: &str) -> Arc<Analysis> {
        let key = self.config_of(uri);
        self.analysis(&key)
    }

    /// (URI, name, text) of each file of a config.
    fn files(&self, key: &ConfigKey) -> Vec<(String, String, Arc<str>)> {
        match key {
            ConfigKey::Single(uri) => {
                let text = self.docs.get(uri).map(|d| d.text.clone()).or_else(|| {
                    let p = uri_to_path(uri)?;
                    std::fs::read_to_string(p).ok().map(Into::into)
                });
                let name =
                    uri_to_path(uri).map_or_else(|| uri.clone(), |p| p.display().to_string());
                vec![(uri.clone(), name, text.unwrap_or_else(|| "".into()))]
            }
            ConfigKey::Dir(dir) => {
                let found = find_files(dir).map(|f| f.files).unwrap_or_default();
                // Open documents by canonical path.
                let open: HashMap<PathBuf, (&String, &Doc)> = self
                    .docs
                    .iter()
                    .filter_map(|(u, d)| Some((canonical(&uri_to_path(u)?), (u, d))))
                    .collect();
                found
                    .into_iter()
                    .filter_map(|path| {
                        let name = path.display().to_string();
                        match open.get(&canonical(&path)) {
                            Some((uri, doc)) => Some(((*uri).clone(), name, doc.text.clone())),
                            None => {
                                let text = std::fs::read_to_string(&path).ok()?;
                                Some((path_to_uri(&path), name, text.into()))
                            }
                        }
                    })
                    .collect()
            }
        }
    }
}

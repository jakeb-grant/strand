//! Source files: identity, the files table, and which files a config loads.
//!
//! A [`Span`](crate::syntax::Span) is a byte range inside one file; a
//! [`FileId`] says which file. Diagnostics label `(FileId, Span)` pairs so a
//! single error can point into several files (a name declared twice in two
//! files, a rename across the config).
//!
//! [`find_files`] is the one definition of "the `.strand` files of a config
//! directory", shared by `strand check`, the loader and the watcher so they
//! can never load different sets (`docs/architecture.md`, "Config files").

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Identifies one file in a [`SourceMap`]. Ids are dense indices in the
/// order files were added; `FileId::default()` is the first file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// One loaded file: the name shown in diagnostics and its text.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Usually the path as the user wrote or the loader found it.
    pub name: String,
    pub text: Arc<str>,
}

/// The table of loaded files that [`FileId`]s index.
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a file and returns its id.
    pub fn add(&mut self, name: impl Into<String>, text: impl Into<Arc<str>>) -> FileId {
        let id = FileId(u32::try_from(self.files.len()).unwrap_or(u32::MAX));
        self.files.push(SourceFile {
            name: name.into(),
            text: text.into(),
        });
        id
    }

    /// A map holding just one file, for single-file tools and tests.
    pub fn single(name: impl Into<String>, text: impl Into<Arc<str>>) -> (Self, FileId) {
        let mut map = Self::new();
        let id = map.add(name, text);
        (map, id)
    }

    pub fn get(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(id.0 as usize)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (FileId, &SourceFile)> {
        self.files
            .iter()
            .enumerate()
            .map(|(i, f)| (FileId(u32::try_from(i).unwrap_or(u32::MAX)), f))
    }
}

/// How many directories below the config directory files are loaded
/// (`design.md`, "Change sources": the watcher watches to depth 3).
pub const MAX_DEPTH: usize = 3;

/// The result of scanning a config directory.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Every `.strand` file, sorted, as found under the directory (so
    /// diagnostics show the path the user knows). A file reachable by
    /// several paths appears once, under its shallowest path (the one that
    /// sorts first among equally deep ones).
    pub files: Vec<PathBuf>,
    /// The canonical path of every directory scanned, `dir` included: where
    /// the `.strand` set can change. Link targets outside `dir` appear here
    /// under their real location, so a watcher can watch them.
    pub dirs: Vec<PathBuf>,
    /// Directories or entries that could not be read, with the reason.
    /// They are reported, and the scan carries on past them. A dangling
    /// link named `*.strand` is one of these: it was meant to be loaded.
    pub errors: Vec<(PathBuf, std::io::Error)>,
    /// Directories past [`MAX_DEPTH`] that hold `.strand` files, which are
    /// therefore not loaded: worth a warning, since nothing else says why.
    pub too_deep: Vec<PathBuf>,
}

/// Finds the `.strand` files of a config directory.
///
/// The rules, which the loader and the watcher must share (they call this):
/// - files ending in `.strand`, at most [`MAX_DEPTH`] directories below
///   `dir` (`dir/a/b/c/x.strand` is loaded; one level deeper is not);
/// - names starting with `.` are skipped, files and directories alike;
/// - symlinks are followed (stowed dotfiles). The walk is breadth-first and
///   directories and files are deduplicated by canonical path, so each is
///   first reached at its shallowest depth and loads once, however many
///   links point at it;
/// - an unreadable sub-directory, or a dangling `*.strand` link, is
///   recorded in [`Discovery::errors`] and skipped; the rest of the tree is
///   still scanned.
///
/// If `dir` is itself a file, it is the only file. Reading `dir` itself
/// failing is the returned error.
pub fn find_files(dir: &Path) -> std::io::Result<Discovery> {
    let mut found = Discovery::default();
    let meta = std::fs::metadata(dir)?;
    if meta.is_file() {
        found.files.push(dir.to_path_buf());
        if let Some(parent) = std::fs::canonicalize(dir)
            .ok()
            .and_then(|c| c.parent().map(Path::to_path_buf))
        {
            found.dirs.push(parent);
        }
        return Ok(found);
    }
    let root = std::fs::read_dir(dir)?;
    let mut seen_files = HashSet::new();
    let mut seen_dirs = HashSet::new();
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    seen_dirs.insert(canon.clone());
    found.dirs.push(canon);
    // Breadth-first: every directory at depth d is queued before any at
    // depth d + 1, so the shallowest path to a directory claims it.
    let mut queue = VecDeque::from([(dir.to_path_buf(), 0usize, Some(root))]);
    while let Some((path, depth, entries)) = queue.pop_front() {
        let entries = match entries {
            Some(e) => e,
            None => match std::fs::read_dir(&path) {
                Ok(e) => e,
                Err(e) => {
                    found.errors.push((path, e));
                    continue;
                }
            },
        };
        scan(
            entries,
            &path,
            depth,
            &mut found,
            &mut seen_files,
            &mut seen_dirs,
            &mut queue,
        );
    }
    found.files.sort();
    Ok(found)
}

type Queue = VecDeque<(PathBuf, usize, Option<std::fs::ReadDir>)>;

fn scan(
    entries: std::fs::ReadDir,
    dir: &Path,
    depth: usize,
    found: &mut Discovery,
    seen_files: &mut HashSet<PathBuf>,
    seen_dirs: &mut HashSet<PathBuf>,
    queue: &mut Queue,
) {
    // Sorted, so which alias of a linked file wins is deterministic.
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => paths.push(e.path()),
            Err(e) => found.errors.push((dir.to_path_buf(), e)),
        }
    }
    paths.sort();
    for path in paths {
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let is_strand = path.extension().is_some_and(|e| e == "strand");
        // `metadata` follows symlinks.
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                // A dangling `bar.strand` link was meant to be loaded.
                if is_strand {
                    found.errors.push((path, e));
                }
                continue;
            }
        };
        if meta.is_dir() {
            if depth >= MAX_DEPTH {
                if holds_strand_files(&path) {
                    found.too_deep.push(path);
                }
                continue;
            }
            // A link back up the tree, or a second link to a directory,
            // would otherwise be walked again.
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen_dirs.insert(canon.clone()) {
                found.dirs.push(canon);
                queue.push_back((path, depth + 1, None));
            }
        } else if meta.is_file() && is_strand {
            match std::fs::canonicalize(&path) {
                Ok(canon) => {
                    if seen_files.insert(canon) {
                        found.files.push(path);
                    }
                }
                Err(e) => found.errors.push((path, e)),
            }
        }
    }
}

/// Whether `.strand` files lie anywhere under `dir` (not following
/// directory links, skipping hidden names, looking at no more than a few
/// thousand entries).
fn holds_strand_files(dir: &Path) -> bool {
    let mut budget = 4096usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            budget = match budget.checked_sub(1) {
                Some(b) => b,
                None => return false,
            };
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.extension().is_some_and(|e| e == "strand") {
                return true;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_map_hands_out_dense_ids() {
        let mut map = SourceMap::new();
        let a = map.add("a.strand", "x");
        let b = map.add("b.strand", String::from("y"));
        assert_eq!((a, b), (FileId(0), FileId(1)));
        assert_eq!(map.get(b).map(|f| &*f.text), Some("y"));
        assert_eq!(map.len(), 2);
        assert!(map.get(FileId(9)).is_none());
    }
}

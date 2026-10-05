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

use std::collections::HashSet;
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
    /// several paths appears once, under the path that sorts first.
    pub files: Vec<PathBuf>,
    /// Directories or entries that could not be read, with the reason.
    /// They are reported, and the scan carries on past them.
    pub errors: Vec<(PathBuf, std::io::Error)>,
}

/// Finds the `.strand` files of a config directory.
///
/// The rules, which the watcher must match exactly:
/// - files ending in `.strand`, at most [`MAX_DEPTH`] directories below
///   `dir` (`dir/a/b/c/x.strand` is loaded; one level deeper is not);
/// - names starting with `.` are skipped, files and directories alike;
/// - symlinks are followed (stowed dotfiles), and files are deduplicated
///   by canonical path so a file reached through two links loads once;
/// - an unreadable sub-directory is recorded in [`Discovery::errors`] and
///   skipped; the rest of the tree is still scanned.
///
/// If `dir` is itself a file, it is the only file. Reading `dir` itself
/// failing is the returned error.
pub fn find_files(dir: &Path) -> std::io::Result<Discovery> {
    let mut found = Discovery::default();
    let meta = std::fs::metadata(dir)?;
    if meta.is_file() {
        found.files.push(dir.to_path_buf());
        return Ok(found);
    }
    let entries = std::fs::read_dir(dir)?;
    let mut seen = HashSet::new();
    let mut seen_dirs = HashSet::new();
    if let Ok(canon) = std::fs::canonicalize(dir) {
        seen_dirs.insert(canon);
    }
    walk(entries, dir, 0, &mut found, &mut seen, &mut seen_dirs);
    found.files.sort();
    Ok(found)
}

fn walk(
    entries: std::fs::ReadDir,
    dir: &Path,
    depth: usize,
    found: &mut Discovery,
    seen: &mut HashSet<PathBuf>,
    seen_dirs: &mut HashSet<PathBuf>,
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
        // `metadata` follows symlinks.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue; // dangling link
        };
        if meta.is_dir() {
            if depth >= MAX_DEPTH {
                continue;
            }
            // A link back up the tree would otherwise be walked again.
            if let Ok(canon) = std::fs::canonicalize(&path) {
                if !seen_dirs.insert(canon) {
                    continue;
                }
            }
            match std::fs::read_dir(&path) {
                Ok(sub) => walk(sub, &path, depth + 1, found, seen, seen_dirs),
                Err(e) => found.errors.push((path, e)),
            }
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "strand") {
            match std::fs::canonicalize(&path) {
                Ok(canon) => {
                    if seen.insert(canon) {
                        found.files.push(path);
                    }
                }
                Err(e) => found.errors.push((path, e)),
            }
        }
    }
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

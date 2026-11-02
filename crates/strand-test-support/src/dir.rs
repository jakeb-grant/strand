//! Directories for tests.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Lines of a log [`ShortDir`] prints when its test failed.
const LOG_TAIL: usize = 40;

/// A private (0700) directory under the system's temp dir, named
/// `<tag>-<pid>-<n>`, removed on drop. Short on purpose: a Wayland, IPC or
/// bus socket goes in it, and `sun_path` holds 108 bytes, which a
/// worktree's `target/` can spend on its own. When its test panics, each
/// `*.log` in it says its last lines on stderr before it goes, which is
/// where a test runner's report of the failure looks.
#[derive(Debug)]
pub struct ShortDir(PathBuf);

impl ShortDir {
    /// Makes the directory; a test can't go on without it, so it panics
    /// when it can't.
    pub fn new(tag: &str) -> ShortDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        make_private(&path);
        ShortDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ShortDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ShortDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            print_logs(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Says the last lines of each `*.log` directly in `dir` on stderr.
pub fn print_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "log"))
        .collect();
    logs.sort();
    for log in logs {
        let text = String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let tail = &lines[lines.len().saturating_sub(LOG_TAIL)..];
        eprintln!("--- the end of {}:\n{}", log.display(), tail.join("\n"));
    }
}

/// Makes `path` and its parents, with `path` itself private (0700).
pub fn make_private(path: &Path) {
    if let Err(e) = std::fs::create_dir_all(path) {
        panic!("{}: {e}", path.display());
    }
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
        panic!("{}: {e}", path.display());
    }
}

/// A fresh directory that no other test makes or removes anything beside,
/// nor in any ancestor: `<target>/strand-own-<name>/<pid>`, given the
/// target's temp dir (`env!("CARGO_TARGET_TMPDIR")`), whose parent is
/// `<target>`. For a shell's config or home in a test that asserts a quiet
/// window: the watcher watches each ancestor of a config for children
/// moved or deleted, and tests of every binary make and remove their own
/// directories in the system's temp dir and in `CARGO_TARGET_TMPDIR`, side
/// by side under nextest. `name` is the test's own, so that its parent
/// never loses another child either. Too long for a socket: see
/// [`ShortDir`]. The caller removes it.
pub fn own_dir(target_tmp: &str, name: &str) -> PathBuf {
    let target_tmp = Path::new(target_tmp);
    let path = target_tmp
        .parent()
        .unwrap_or(target_tmp)
        .join(format!("strand-own-{name}"))
        .join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&path);
    if let Err(e) = std::fs::create_dir_all(&path) {
        panic!("{}: {e}", path.display());
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_dir_is_private_short_and_removed() {
        let dir = ShortDir::new("strand-ts");
        let path = dir.path().to_path_buf();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // A socket's name still fits in `sun_path` on the usual temp dir.
        if std::env::temp_dir() == Path::new("/tmp") {
            assert!(path.as_os_str().len() < 64, "{path:?}");
        }
        assert!(path.starts_with(std::env::temp_dir()));
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn two_short_dirs_of_one_tag_differ() {
        let (a, b) = (ShortDir::new("strand-ts"), ShortDir::new("strand-ts"));
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn a_failing_test_says_its_logs_and_the_directory_still_goes() {
        let dir = ShortDir::new("strand-ts-log");
        let path = dir.path().to_path_buf();
        std::fs::write(path.join("a.log"), "the line\n").unwrap();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _dir = dir;
            panic!("on purpose");
        }));
        assert!(panicked.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn an_own_dir_sits_beside_the_targets_tmp_and_starts_empty() {
        let base = ShortDir::new("strand-ts-own");
        let tmp = base.path().join("tmp");
        let tmp = tmp.to_str().unwrap();
        let first = own_dir(tmp, "a");
        std::fs::write(first.join("left"), "").unwrap();
        let again = own_dir(tmp, "a");
        assert_eq!(first, again);
        assert!(!again.join("left").exists());
        assert!(again.starts_with(base.path().join("strand-own-a")));
        assert_ne!(again, own_dir(tmp, "b"));
    }
}

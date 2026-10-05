//! `strand fmt [--check] [paths]`: format `.strand` files in place with
//! [`strand_compiler::fmt`], or with `--check` only report the files that
//! are not formatted.
//!
//! A directory stands for its `.strand` module set
//! ([`strand_compiler::source::find_files`]); no path means the config
//! directory. A file with syntax errors is reported and left as written.
//! Files are rewritten through a temporary file renamed over the canonical
//! path, so a symlinked dotfile keeps its link and the watcher sees one
//! `MOVED_TO` (design.md, "Watch directories, not files").

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::fmt::{FormatError, format};
use strand_compiler::source::find_files;
use strand_compiler::{FileId, SourceMap};

use crate::check::default_dir;

const USAGE: &str = "usage: strand fmt [--check] [dir | file]...\n\n\
    Formats .strand files in place (a directory means its .strand files, \
    as `strand check` loads them; default $XDG_CONFIG_HOME/strand). With \
    --check, writes nothing, lists the files that are not formatted and \
    exits non-zero if there are any. Files with syntax errors are reported \
    and left as written.\n";

/// What a run did.
#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    /// Files rewritten (or, with `--check`, that would be).
    pub changed: Vec<PathBuf>,
    /// Files that could not be formatted: syntax errors, I/O.
    pub failed: usize,
    /// Messages for stderr.
    pub text: String,
}

/// Runs `strand fmt` with its arguments (after `fmt`). Returns the text for
/// stderr and whether the run succeeded.
pub fn run(args: &[String], style: Style) -> (String, bool) {
    let mut check = false;
    let mut paths = Vec::new();
    for a in args {
        match a.as_str() {
            "-h" | "--help" => return (USAGE.into(), true),
            "--check" => check = true,
            s if s.starts_with('-') => {
                return (
                    format!("strand fmt: unknown option `{s}`\n\n{USAGE}"),
                    false,
                );
            }
            s => paths.push(PathBuf::from(s)),
        }
    }
    if paths.is_empty() {
        match default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        ) {
            Some(d) => paths.push(d),
            None => {
                return (
                    "strand fmt: set XDG_CONFIG_HOME or HOME, or pass a path\n".into(),
                    false,
                );
            }
        }
    }
    let report = fmt_paths(&paths, check, style);
    let ok = report.failed == 0 && (!check || report.changed.is_empty());
    (report.text, ok)
}

/// Formats (or, with `check`, inspects) every file the paths stand for.
pub fn fmt_paths(paths: &[PathBuf], check: bool, style: Style) -> Report {
    let mut report = Report::default();
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            match find_files(p) {
                Ok(found) => {
                    for (path, e) in &found.errors {
                        report.failed += 1;
                        let _ = writeln!(report.text, "error: cannot read {}: {e}", path.display());
                    }
                    files.extend(found.files);
                }
                Err(e) => {
                    report.failed += 1;
                    let _ = writeln!(report.text, "error: cannot read {}: {e}", p.display());
                }
            }
        } else {
            files.push(p.clone());
        }
    }
    for file in &files {
        report.files += 1;
        match fmt_file(file, check, style) {
            Ok(false) => {}
            Ok(true) => {
                if check {
                    let _ = writeln!(report.text, "not formatted: {}", file.display());
                }
                report.changed.push(file.clone());
            }
            Err(msg) => {
                report.failed += 1;
                report.text.push_str(&msg);
            }
        }
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let n = report.changed.len();
    let _ = writeln!(
        report.text,
        "strand fmt: {} file{}, {n} {}{}",
        report.files,
        plural(report.files),
        if check {
            "not formatted"
        } else {
            "reformatted"
        },
        if report.failed > 0 {
            format!(", {} failed", report.failed)
        } else {
            String::new()
        },
    );
    report
}

/// Formats one file; true if its text changed (or would, with `check`).
fn fmt_file(path: &Path, check: bool, style: Style) -> Result<bool, String> {
    let name = path.display().to_string();
    let src = match std::fs::read(path).map(String::from_utf8) {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => return Err(format!("error: {name}: not valid UTF-8\n")),
        Err(e) => return Err(format!("error: {name}: {e}\n")),
    };
    let out = match format(&src) {
        Ok(out) => out,
        Err(FormatError::Syntax(diags)) => {
            let (map, _) = SourceMap::single(name.clone(), src);
            let diags: Vec<_> = diags
                .into_iter()
                .map(|d| d.in_file(FileId::default()))
                .collect();
            return Err(format!(
                "{}error: {name}: not formatted: it has syntax errors\n",
                render(&diags, &map, style)
            ));
        }
        Err(e) => return Err(format!("error: {name}: {e}\n")),
    };
    if out == src {
        return Ok(false);
    }
    if !check {
        write_replacing(path, &out).map_err(|e| format!("error: {name}: cannot write: {e}\n"))?;
    }
    Ok(true)
}

/// Writes `text` to the file `path` resolves to, through a temporary file
/// renamed over it: a symlink stays a link, permissions are kept, and a
/// reader never sees half a file.
fn write_replacing(path: &Path, text: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path)?;
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("the file has no directory"))?;
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{file_name}.strand-fmt-{}", std::process::id()));
    let perms = std::fs::metadata(&target)?.permissions();
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, perms)?;
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("strand-fmt-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }

        fn read(&self, rel: &str) -> String {
            std::fs::read_to_string(self.0.join(rel)).unwrap()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MESSY: &str = "state  open=false\nbar Top{edge:top}\n";
    const TIDY: &str = "state open = false\nbar Top { edge: top }\n";

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn check_lists_unformatted_files_and_writes_nothing() {
        let t = TempDir::new();
        t.write("bar.strand", MESSY);
        t.write("theme.strand", TIDY);
        let dir = t.0.display().to_string();
        let (text, ok) = run(&args(&["--check", &dir]), Style::Plain);
        assert!(!ok, "{text}");
        assert!(
            text.contains("not formatted: ") && text.contains("bar.strand"),
            "{text}"
        );
        assert!(!text.contains("theme.strand"), "{text}");
        assert!(text.contains("2 files, 1 not formatted"), "{text}");
        assert_eq!(t.read("bar.strand"), MESSY);
    }

    #[test]
    fn formats_in_place() {
        let t = TempDir::new();
        t.write("bar.strand", MESSY);
        t.write("sub/osd.strand", TIDY);
        let dir = t.0.display().to_string();
        let (text, ok) = run(&args(&[&dir]), Style::Plain);
        assert!(ok, "{text}");
        assert!(text.contains("2 files, 1 reformatted"), "{text}");
        assert_eq!(t.read("bar.strand"), TIDY);
        // Formatted now, so a check passes.
        let (text, ok) = run(&args(&["--check", &dir]), Style::Plain);
        assert!(ok, "{text}");
        // No temporary file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("strand-fmt"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_file_with_syntax_errors_is_reported_and_kept() {
        let t = TempDir::new();
        let broken = "bar Top {\n  text \"x\"\n";
        let path = t.write("bar.strand", broken);
        let (text, ok) = run(&args(&[&path.display().to_string()]), Style::Plain);
        assert!(!ok);
        assert!(text.contains("unclosed"), "{text}");
        assert!(
            text.contains("not formatted: it has syntax errors"),
            "{text}"
        );
        assert_eq!(t.read("bar.strand"), broken);
    }

    #[test]
    fn a_symlinked_file_keeps_its_link() {
        let t = TempDir::new();
        let real = t.write("dotfiles/bar.strand", MESSY);
        let link = t.0.join("config/bar.strand");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (text, ok) = run(&args(&[&link.display().to_string()]), Style::Plain);
        assert!(ok, "{text}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(t.read("dotfiles/bar.strand"), TIDY);
    }

    #[test]
    fn options_are_checked() {
        let (text, ok) = run(&args(&["--frob"]), Style::Plain);
        assert!(!ok && text.contains("unknown option `--frob`"), "{text}");
        let (text, ok) = run(&args(&["--help"]), Style::Plain);
        assert!(ok && text.starts_with("usage: strand fmt"));
    }
}

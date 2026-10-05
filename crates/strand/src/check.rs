//! `strand check [dir]`: parse every `.strand` file in the config directory
//! and report diagnostics without running anything.
//!
//! Syntax only for now; name resolution and type checking join when the
//! checker lands (M1, wave 2).

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::syntax::parse;

/// How deep below the config directory files are loaded, matching the
/// watcher's depth (`design.md`, "Change sources").
pub const MAX_DEPTH: usize = 3;

/// What a check found.
#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    pub errors: usize,
    pub warnings: usize,
    /// Rendered diagnostics and the summary line.
    pub text: String,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.errors == 0
    }
}

/// `$XDG_CONFIG_HOME/strand`, else `$HOME/.config/strand`.
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

/// Every `.strand` file under `dir`, at most [`MAX_DEPTH`] directories
/// down, sorted. Hidden directories are skipped; symlinks are followed.
pub fn find_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(dir, 0, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let hidden = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        // `metadata` follows symlinks, so stowed dotfiles are found.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue; // dangling link
        };
        if meta.is_dir() {
            if depth < MAX_DEPTH && !hidden {
                walk(&path, depth + 1, out)?;
            }
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "strand") {
            out.push(path);
        }
    }
    Ok(())
}

/// Parses every file under `dir` and renders what it found.
pub fn check_dir(dir: &Path, style: Style) -> Result<Report, String> {
    let files =
        find_files(dir).map_err(|e| format!("strand check: cannot read {}: {e}", dir.display()))?;
    if files.is_empty() {
        return Err(format!(
            "strand check: no .strand files in {}",
            dir.display()
        ));
    }
    let mut report = Report {
        files: files.len(),
        ..Report::default()
    };
    for path in &files {
        let name = path.display().to_string();
        let src = match std::fs::read(path).map(String::from_utf8) {
            Ok(Ok(src)) => src,
            Ok(Err(_)) => {
                report.errors += 1;
                let _ = writeln!(report.text, "error: {name}: not valid UTF-8\n");
                continue;
            }
            Err(e) => {
                report.errors += 1;
                let _ = writeln!(report.text, "error: {name}: {e}\n");
                continue;
            }
        };
        let parsed = parse(&src);
        for d in &parsed.diagnostics {
            if d.is_error() {
                report.errors += 1;
            } else {
                report.warnings += 1;
            }
        }
        report
            .text
            .push_str(&render(&parsed.diagnostics, &name, &src, style));
    }
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let _ = writeln!(
        report.text,
        "strand check: {} in {}: {}, {}",
        plural(report.files, "file"),
        dir.display(),
        plural(report.errors, "error"),
        plural(report.warnings, "warning"),
    );
    Ok(report)
}

/// Runs `strand check` with its arguments (after `check`). Returns the text
/// for stderr and whether the check passed.
pub fn run(args: &[String], style: Style) -> (String, bool) {
    let dir = match args {
        [] => match default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        ) {
            Some(d) => d,
            None => {
                return (
                    "strand check: set XDG_CONFIG_HOME or HOME, or pass a directory\n".into(),
                    false,
                );
            }
        },
        [dir] if !dir.starts_with('-') => PathBuf::from(dir),
        _ => return ("usage: strand check [dir]\n".into(), false),
    };
    match check_dir(&dir, style) {
        Ok(report) => {
            let ok = report.ok();
            (report.text, ok)
        }
        Err(e) => (format!("{e}\n"), false),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("strand-check-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_dir_prefers_xdg() {
        assert_eq!(
            default_dir(Some("/x".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x/strand"))
        );
        assert_eq!(
            default_dir(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/strand"))
        );
        // The XDG spec ignores relative values.
        assert_eq!(
            default_dir(Some("rel".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/strand"))
        );
        assert_eq!(default_dir(None, None), None);
    }

    #[test]
    fn finds_strand_files_to_depth_three() {
        let t = TempDir::new();
        t.write("bar.strand", "");
        t.write("a/b/c/deep.strand", "");
        t.write("a/b/c/d/too_deep.strand", "");
        t.write(".git/hidden.strand", "");
        t.write("notes.txt", "");
        let files = find_files(&t.0).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&t.0).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a/b/c/deep.strand", "bar.strand"]);
    }

    #[test]
    fn clean_config_passes() {
        let t = TempDir::new();
        t.write("bar.strand", "bar Top {\n  edge: top; height: 32\n}\n");
        t.write(
            "widgets/clock.strand",
            "component Clock { text clock.format(\"%H:%M\") }\n",
        );
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert!(report.ok());
        assert_eq!(report.files, 2);
        assert!(report.text.contains("2 files"), "{}", report.text);
        assert!(report.text.contains("0 errors"), "{}", report.text);
    }

    #[test]
    fn errors_are_rendered_with_file_line_and_caret() {
        let t = TempDir::new();
        t.write("good.strand", "state x = 0\n");
        t.write("bad.strand", "component A {\n  box { width: 12pz }\n}\n");
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert!(!report.ok());
        assert_eq!(report.errors, 1);
        assert!(report.text.contains("bad.strand:2:"), "{}", report.text);
        assert!(
            report.text.contains("did you mean `px`?"),
            "{}",
            report.text
        );
        assert!(report.text.contains("1 error,"), "{}", report.text);
    }

    #[test]
    fn invalid_utf8_is_an_error() {
        let t = TempDir::new();
        std::fs::write(t.0.join("bin.strand"), [0xff, 0xfe]).unwrap();
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert_eq!(report.errors, 1);
        assert!(report.text.contains("not valid UTF-8"));
    }

    #[test]
    fn missing_or_empty_dirs_fail() {
        let t = TempDir::new();
        assert!(
            check_dir(&t.0, Style::Plain)
                .unwrap_err()
                .contains("no .strand files")
        );
        assert!(
            check_dir(&t.0.join("nope"), Style::Plain)
                .unwrap_err()
                .contains("cannot read")
        );
        let (text, ok) = run(&["--frob".into()], Style::Plain);
        assert!(!ok && text.contains("usage"));
    }

    #[test]
    fn design_examples_check_clean() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../strand-compiler/tests/fixtures");
        let (text, ok) = run(&[fixtures.display().to_string()], Style::Plain);
        assert!(ok, "{text}");
    }
}

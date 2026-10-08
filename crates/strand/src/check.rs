//! `strand check [dir]`: parse, resolve and type-check every `.strand` file
//! in the config directory as one program, and report diagnostics (with
//! did-you-mean fixes) without running anything.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use strand_compiler::SourceMap;
use strand_compiler::diagnostic::{Style, render};
use strand_compiler::source::find_files;

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

/// Checks every `.strand` file under `dir` as one config and renders what
/// it found. Which files count is [`strand_compiler::source::find_files`],
/// the same rule the loader and watcher use. If `dir` is a file, see
/// [`check_file`] (with the default config directory from the
/// environment).
pub fn check_dir(dir: &Path, style: Style) -> Result<Report, String> {
    if dir.is_file() {
        let root = default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        );
        return check_file(dir, root.as_deref(), style);
    }
    check_config(dir, None, style)
}

/// Checks one file as part of its config, so its references to other
/// files resolve: the config is `default_root` if the file is inside it,
/// else the file's directory. Every file of that config is compiled, and
/// only the diagnostics that point into `file` are reported. A file the config's
/// module set does not include (hidden, or too deep) is checked alone.
pub fn check_file(
    file: &Path,
    default_root: Option<&Path>,
    style: Style,
) -> Result<Report, String> {
    let canonical = std::fs::canonicalize(file)
        .map_err(|e| format!("strand check: cannot read {}: {e}", file.display()))?;
    let inside_default = default_root
        .and_then(|r| std::fs::canonicalize(r).ok())
        .filter(|r| canonical.starts_with(r));
    let root = match inside_default {
        Some(r) => r,
        None => canonical
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
    };
    let in_set = find_files(&root).is_ok_and(|found| {
        found
            .files
            .iter()
            .any(|f| std::fs::canonicalize(f).is_ok_and(|f| f == canonical))
    });
    if in_set {
        check_config(&root, Some(&canonical), style)
    } else {
        check_config(file, None, style)
    }
}

/// Checks the config at `dir`; with `focus`, reports only the diagnostics
/// with a label (primary or secondary) in that file (a canonical path).
fn check_config(dir: &Path, focus: Option<&Path>, style: Style) -> Result<Report, String> {
    let found =
        find_files(dir).map_err(|e| format!("strand check: cannot read {}: {e}", dir.display()))?;
    let mut report = Report {
        files: found.files.len(),
        ..Report::default()
    };
    // Problems elsewhere in the config are not this file's.
    let whole = focus.is_none();
    for (path, e) in found.errors.iter().filter(|_| whole) {
        report.errors += 1;
        let _ = writeln!(report.text, "error: cannot read {}: {e}\n", path.display());
    }
    for path in found.too_deep.iter().filter(|_| whole) {
        report.warnings += 1;
        let _ = writeln!(
            report.text,
            "warning: {} holds .strand files but is more than {} directories deep, \
             so they are not loaded\n",
            path.display(),
            strand_compiler::source::MAX_DEPTH,
        );
    }
    if found.files.is_empty() && found.errors.is_empty() {
        return Err(format!(
            "strand check: no .strand files in {}",
            dir.display()
        ));
    }
    let mut map = SourceMap::new();
    let mut focus_id = None;
    for path in &found.files {
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
        let id = map.add(name, src);
        if focus.is_some_and(|f| std::fs::canonicalize(path).is_ok_and(|p| p == f)) {
            focus_id = Some(id);
        }
    }
    let compiled = strand_compiler::compile_with(&map, crate::services::schema());
    let mut diags = compiled.diagnostics;
    // `from dbus` services against the bus's introspection (a warning
    // when the bus cannot be reached).
    diags.extend(strand_compiler::check::dbus::check(
        &compiled.program,
        &crate::services::custom::BusIntrospector::default(),
    ));
    // `from file`/`from poll` paths that name a program (read, not run).
    diags.extend(strand_compiler::check::paths::check(
        &compiled.program,
        Some(dir),
    ));
    if focus.is_some() {
        // A diagnostic is the file's if any of its labels is there: the
        // first declaration of a name redeclared in another file, a call
        // site of a parameter whose callers disagree.
        diags.retain(|d| d.labels.iter().any(|l| Some(l.file) == focus_id));
    }
    for d in &diags {
        if d.is_error() {
            report.errors += 1;
        } else {
            report.warnings += 1;
        }
    }
    report.text.push_str(&render(&diags, &map, style));
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let _ = match focus {
        Some(f) => writeln!(
            report.text,
            "strand check: {} (checked with the {} in {}): {}, {}",
            f.display(),
            plural(report.files, "file"),
            dir.display(),
            plural(report.errors, "error"),
            plural(report.warnings, "warning"),
        ),
        None => writeln!(
            report.text,
            "strand check: {} in {}: {}, {}",
            plural(report.files, "file"),
            dir.display(),
            plural(report.errors, "error"),
            plural(report.warnings, "warning"),
        ),
    };
    Ok(report)
}

const USAGE: &str = "usage: strand check [dir | file]\n\n\
    Parses and type-checks every .strand file under the directory (default \
    $XDG_CONFIG_HOME/strand) as one config and prints diagnostics; exits \
    non-zero on errors. Given a file, checks it with the rest of its config \
    (the default directory if the file is in it, else the file's directory) \
    and prints the diagnostics in that file.\n";

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
        [flag] if flag == "-h" || flag == "--help" => return (USAGE.into(), true),
        [dir] if !dir.starts_with('-') => PathBuf::from(dir),
        _ => return (USAGE.into(), false),
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
        t.write(".hidden.strand", "");
        t.write("notes.txt", "");
        std::os::unix::fs::symlink(t.0.join("bar.strand"), t.0.join("z_alias.strand")).unwrap();
        let found = find_files(&t.0).unwrap();
        assert!(found.errors.is_empty());
        let names: Vec<_> = found
            .files
            .iter()
            .map(|p| p.strip_prefix(&t.0).unwrap().to_string_lossy().into_owned())
            .collect();
        // The link to bar.strand is the same file, so it loads once.
        assert_eq!(names, ["a/b/c/deep.strand", "bar.strand"]);
        // The directory skipped for depth is named, so a warning can say
        // why its file is not loaded; `.git` is hidden, not too deep.
        assert_eq!(found.too_deep, [t.0.join("a/b/c/d")]);
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert!(report.ok());
        assert_eq!(report.warnings, 1);
        assert!(
            report.text.contains("a/b/c/d holds .strand files"),
            "{}",
            report.text
        );
    }

    #[test]
    fn a_deeper_link_does_not_hide_the_real_directory() {
        // With a sorted depth-first walk, `a/b/link` reached `z` first at
        // depth 3, and `z/sub` (depth 4 through the link) was then never
        // scanned: stowing a link silently dropped a module.
        let t = TempDir::new();
        t.write("z/sub/deep.strand", "");
        t.write("top.strand", "");
        std::fs::create_dir_all(t.0.join("a/b")).unwrap();
        std::os::unix::fs::symlink("../../z", t.0.join("a/b/link")).unwrap();
        let found = find_files(&t.0).unwrap();
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        let names: Vec<_> = found
            .files
            .iter()
            .map(|p| p.strip_prefix(&t.0).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["top.strand", "z/sub/deep.strand"]);
        // The canonical directories scanned include the link target once.
        let z = std::fs::canonicalize(t.0.join("z")).unwrap();
        assert_eq!(found.dirs.iter().filter(|d| **d == z).count(), 1);
    }

    #[test]
    fn a_dangling_strand_link_is_reported() {
        let t = TempDir::new();
        t.write("ok.strand", "state x = 1\n");
        std::os::unix::fs::symlink(t.0.join("gone.strand"), t.0.join("bar.strand")).unwrap();
        std::os::unix::fs::symlink(t.0.join("gone.txt"), t.0.join("notes.txt")).unwrap();
        let found = find_files(&t.0).unwrap();
        assert_eq!(found.errors.len(), 1, "{:?}", found.errors);
        assert!(found.errors[0].0.ends_with("bar.strand"));
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert!(!report.ok());
        assert!(report.text.contains("bar.strand"), "{}", report.text);
    }

    #[test]
    fn unreadable_subdirectories_do_not_stop_the_check() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new();
        t.write("ok.strand", "state x = 1\n");
        t.write("locked/inner.strand", "state y = 1\n");
        let locked = t.0.join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything; the rule is only observable as a user.
        let readable = std::fs::read_dir(&locked).is_ok();
        let report = check_dir(&t.0, Style::Plain);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let report = report.unwrap();
        if !readable {
            assert_eq!(report.files, 1);
            assert_eq!(report.errors, 1);
            assert!(report.text.contains("cannot read"), "{}", report.text);
            assert!(report.text.contains("locked"), "{}", report.text);
        }
    }

    #[test]
    fn a_single_file_can_be_checked() {
        let t = TempDir::new();
        t.write("one.strand", "state x = \n");
        let report = check_dir(&t.0.join("one.strand"), Style::Plain).unwrap();
        assert_eq!(report.files, 1);
        assert!(!report.ok());
        assert!(report.text.contains("one.strand:1:"), "{}", report.text);
    }

    /// A file is checked with the rest of its config: names other files
    /// declare resolve, and only its own diagnostics are reported.
    #[test]
    fn a_file_is_checked_with_its_config() {
        let t = TempDir::new();
        t.write(
            "theme.strand",
            "enum Look { light, dark }\nexport state look = light\ncomponent Dot { box {} }\nlet unused = nope\n",
        );
        t.write(
            "bar.strand",
            "bar Top {\n  text theme.look == dark ? \"d\" : \"l\"\n  Dot\n}\n",
        );
        // No default root: the file's directory is its config.
        let report = check_file(&t.0.join("bar.strand"), None, Style::Plain).unwrap();
        assert_eq!(report.errors, 0, "{}", report.text);
        assert_eq!(report.files, 2, "{}", report.text);
        assert!(
            report.text.contains("bar.strand (checked with the 2 files"),
            "{}",
            report.text
        );
        // theme.strand's own error is reported when it is the file asked for.
        let report = check_file(&t.0.join("theme.strand"), None, Style::Plain).unwrap();
        assert_eq!(report.errors, 1, "{}", report.text);
        assert!(
            report.text.contains("unknown name `nope`"),
            "{}",
            report.text
        );
        // Inside the default config directory, the whole of it counts.
        t.write("widgets/use.strand", "bar Side { Dot }\n");
        let report = check_file(&t.0.join("widgets/use.strand"), Some(&t.0), Style::Plain).unwrap();
        assert_eq!(report.errors, 0, "{}", report.text);
        assert_eq!(report.files, 3, "{}", report.text);
        // Outside it, only its own directory.
        let report = check_file(&t.0.join("widgets/use.strand"), None, Style::Plain).unwrap();
        assert_eq!(report.errors, 1, "{}", report.text);
        assert!(
            report.text.contains("unknown element `Dot`"),
            "{}",
            report.text
        );
        // A name declared in two files is both files' error, though the
        // primary label is on the second declaration.
        let t = TempDir::new();
        t.write("a.strand", "component Card { box {} }\n");
        t.write("b.strand", "component Card { box {} }\n");
        for f in ["a.strand", "b.strand"] {
            let report = check_file(&t.0.join(f), None, Style::Plain).unwrap();
            assert_eq!(report.errors, 1, "{f}: {}", report.text);
            assert!(!report.ok(), "{f}");
        }
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
        // Asking for help is not a failure.
        for flag in ["-h", "--help"] {
            let (text, ok) = run(&[flag.into()], Style::Plain);
            assert!(ok && text.contains("usage"), "{flag}");
        }
    }

    /// design.md's four shells, theme and rice as one config, and its
    /// hello bar as another, type-check with no diagnostics.
    #[test]
    fn design_examples_check_clean() {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../strand-compiler/tests/fixtures");
        let configs: [&[&str]; 2] = [
            &["bar", "launcher", "toasts", "osd", "theme", "rice_now"],
            &["hello_bar"],
        ];
        for files in configs {
            let t = TempDir::new();
            for f in files {
                let text = std::fs::read_to_string(fixtures.join(format!("{f}.strand"))).unwrap();
                t.write(&format!("{f}.strand"), &text);
            }
            let report = check_dir(&t.0, Style::Plain).unwrap();
            assert!(report.ok(), "{}", report.text);
            assert_eq!(report.warnings, 0, "{}", report.text);
        }
    }

    #[test]
    fn type_errors_have_did_you_mean_across_files() {
        let t = TempDir::new();
        t.write(
            "theme.strand",
            "enum Look { light, dark }\nexport state look = light\n",
        );
        t.write(
            "bar.strand",
            "bar Top {\n  edge: top\n  text theme.look == drak ? \"d\" : \"l\"\n  whn hover { bg: $accent }\n}\n",
        );
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert_eq!(report.errors, 2, "{}", report.text);
        assert!(report.text.contains("bar.strand:3:"), "{}", report.text);
        assert!(
            report.text.contains("did you mean `dark`?"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("did you mean `when`?"),
            "{}",
            report.text
        );
    }

    #[test]
    fn redeclaring_across_files_names_both() {
        let t = TempDir::new();
        t.write("a.strand", "component Dot { box {} }\n");
        t.write("b.strand", "component Dot { box {} }\n");
        let report = check_dir(&t.0, Style::Plain).unwrap();
        assert_eq!(report.errors, 1, "{}", report.text);
        assert!(report.text.contains("declared twice"), "{}", report.text);
        assert!(report.text.contains("a.strand"), "{}", report.text);
        assert!(report.text.contains("b.strand"), "{}", report.text);
    }
}

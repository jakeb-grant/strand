//! Plugin schema files: the `.schema` texts plugin binaries write with
//! `--write-schemas` into `$XDG_DATA_HOME/strand/schemas/`, read by
//! `strand check`, `strand run` and the LSP alike (docs/architecture.md,
//! "Plugin builder"; decisions.md m5-plugins).
//!
//! [`dir`] finds the directory, [`load`] reads it (a file it cannot use
//! is skipped with a [`Warning`]) and [`apply`] extends a schema with
//! each file in name order, skipping, with a warning, a file that does
//! not apply. Nothing here fails: a bad file never stops a shell.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::Schema;

/// The largest schema file read.
pub const MAX_BYTES: u64 = 1 << 20;

/// What a file's first line says before the writer's path.
const HEADER_START: &str = "// strand plugin schema, written by ";
/// And after it.
const HEADER_END: &str = " --write-schemas; do not edit";

/// A schema file that was skipped, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// `$XDG_DATA_HOME/strand/schemas/` when that is absolute, else
/// `$HOME/.local/share/strand/schemas/`; `None` with neither.
pub fn dir() -> Option<PathBuf> {
    dir_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

/// [`dir`] from the two variables' values.
pub fn dir_from(
    xdg_data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let base = match xdg_data_home.map(PathBuf::from) {
        Some(x) if x.is_absolute() => x,
        _ => PathBuf::from(home.filter(|h| !h.is_empty())?).join(".local/share"),
    };
    Some(base.join("strand/schemas"))
}

/// The first line of a file `exe` writes.
pub fn header(exe: &str) -> String {
    format!("{HEADER_START}{exe}{HEADER_END}\n")
}

/// The writer a file's header names, if it has one.
pub fn writer(text: &str) -> Option<&str> {
    let line = text.lines().next()?;
    line.strip_prefix(HEADER_START)?.strip_suffix(HEADER_END)
}

/// A file's schema text: all of it but a writer header.
pub fn body(text: &str) -> &str {
    match writer(text) {
        Some(_) => text.split_once('\n').map_or("", |(_, rest)| rest),
        None => text,
    }
}

/// The `*.schema` files of `dir` but dotfiles, in file-name order, with
/// their text: regular files (symlinks followed) of at most
/// [`MAX_BYTES`], UTF-8. Anything else is skipped with a warning. A
/// missing directory has none.
#[cold]
#[inline(never)]
pub fn load(dir: &Path) -> (Vec<(PathBuf, String)>, Vec<Warning>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let warn = |path: &Path, message: String| Warning {
        path: path.to_path_buf(),
        message,
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (files, warnings),
        Err(e) => {
            warnings.push(warn(dir, format!("cannot read the directory: {e}")));
            return (files, warnings);
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && name.ends_with(".schema")
        })
        .map(|e| e.path())
        .collect();
    paths.sort();
    for path in paths {
        match read(&path) {
            Ok(text) => files.push((path, text)),
            Err(why) => warnings.push(warn(&path, why)),
        }
    }
    (files, warnings)
}

/// One file's text, or why it is skipped.
#[cold]
#[inline(never)]
fn read(path: &Path) -> Result<String, String> {
    // Checked before opening: opening a FIFO would wait for a writer.
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file; skipped".into());
    }
    if meta.len() > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes; skipped"));
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes; skipped"));
    }
    String::from_utf8(bytes).map_err(|_| "not valid UTF-8; skipped".into())
}

/// Extend `schema` with each of `files` in order, skipping a file whose
/// stem names one of `linked` (a service this binary serves: its own
/// text was applied) and, with a warning, one that does not apply or
/// that redeclares a builtin service's stub (a skipped file adds
/// nothing).
#[cold]
#[inline(never)]
pub fn apply(schema: &mut Schema, files: &[(PathBuf, String)], linked: &[&str]) -> Vec<Warning> {
    let mut warnings = Vec::new();
    for (path, text) in files {
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| linked.contains(&s))
        {
            continue;
        }
        let mut next = schema.clone();
        let why = match next.extend(body(text)) {
            Err(errors) => errors.iter().map(ToString::to_string).collect(),
            // A builtin's stub is the builtin services' to replace.
            Ok(()) if next.provisional.len() < schema.provisional.len() => {
                vec!["it redeclares a builtin service".to_string()]
            }
            Ok(()) => {
                *schema = next;
                continue;
            }
        };
        warnings.push(Warning {
            path: path.clone(),
            message: format!("does not apply, skipped: {}", why.join("; ")),
        });
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = "/// Hello.\nservice hello {\n  /// A greeting.\n  greeting: text\n}\n";

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let p =
                std::env::temp_dir().join(format!("strand-plugins-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Temp(p)
        }

        fn put(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn base() -> Schema {
        Schema::builtin().clone()
    }

    #[test]
    fn the_directory_follows_xdg_with_a_home_fallback() {
        let d = |x: Option<&str>, h: Option<&str>| dir_from(x.map(Into::into), h.map(Into::into));
        assert_eq!(
            d(Some("/data"), Some("/home/u")),
            Some(PathBuf::from("/data/strand/schemas"))
        );
        // A relative XDG_DATA_HOME is ignored (the XDG rule).
        assert_eq!(
            d(Some("rel"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/strand/schemas"))
        );
        assert_eq!(
            d(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/share/strand/schemas"))
        );
        assert_eq!(d(Some("rel"), None), None);
        assert_eq!(d(None, Some("")), None);
    }

    #[test]
    fn the_header_names_its_writer() {
        let text = format!("{}{HELLO}", header("/opt/my-shell"));
        assert_eq!(writer(&text), Some("/opt/my-shell"));
        assert_eq!(body(&text), HELLO);
        assert_eq!(writer(HELLO), None);
        assert_eq!(body(HELLO), HELLO);
    }

    #[test]
    fn a_missing_directory_is_no_plugins() {
        let (files, warnings) = load(Path::new("/nonexistent/strand/schemas"));
        assert!(files.is_empty() && warnings.is_empty());
    }

    /// Files apply in name order (`b` names a record `a` declares), and
    /// the fingerprint changes with each.
    #[test]
    fn files_apply_in_name_order() {
        let t = Temp::new("order");
        t.put(
            "a.schema",
            "/// A thing.\nrecord Thing {\n  /// Its name.\n  name: text\n}\n",
        );
        t.put(
            "b.schema",
            "/// B.\nservice bee {\n  /// The thing.\n  thing: Thing\n}\n",
        );
        t.put(".hidden.schema", "nonsense");
        t.put("notes.txt", "nonsense");
        let (files, warnings) = load(&t.0);
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<_> = files.iter().map(|(p, _)| p.file_name().unwrap()).collect();
        assert_eq!(names, ["a.schema", "b.schema"]);
        let mut s = base();
        let before = s.fingerprint();
        assert!(apply(&mut s, &files, &[]).is_empty());
        assert!(s.services.contains_key("bee"));
        assert_ne!(s.fingerprint(), before);
        // A different file, a different fingerprint.
        t.put(
            "b.schema",
            "/// B.\nservice bee {\n  /// The thing.\n  thing: Thing?\n}\n",
        );
        let mut again = base();
        apply(&mut again, &load(&t.0).0, &[]);
        assert_ne!(again.fingerprint(), s.fingerprint());
    }

    /// A bad, colliding, oversized, non-UTF-8 file and a FIFO are each
    /// skipped with a warning naming them, and change nothing.
    #[test]
    fn unusable_files_are_skipped_with_a_warning() {
        let t = Temp::new("bad");
        let bad = t.put("bad.schema", "service {");
        let clash = t.put(
            "clash.schema",
            "/// Again.\nservice battery {\n  /// x.\n  x: int\n}\n",
        );
        let big = t.put("big.schema", vec![b' '; MAX_BYTES as usize + 1]);
        let latin = t.put("latin.schema", [0xff, 0xfe, b'x']);
        let fifo = t.0.join("pipe.schema");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        let hello = t.put("hello.schema", HELLO);
        let (files, warnings) = load(&t.0);
        let warned: Vec<&Path> = warnings.iter().map(|w| w.path.as_path()).collect();
        assert!(warned.contains(&big.as_path()), "{warnings:?}");
        assert!(warned.contains(&latin.as_path()), "{warnings:?}");
        if made {
            assert!(warned.contains(&fifo.as_path()), "{warnings:?}");
        }
        let mut s = base();
        let before = s.fingerprint();
        let applied = apply(&mut s, &files, &[]);
        let warned: Vec<&Path> = applied.iter().map(|w| w.path.as_path()).collect();
        assert_eq!(warned, [bad.as_path(), clash.as_path()], "{applied:?}");
        assert!(applied[1].message.contains("builtin"), "{applied:?}");
        // Only hello applied.
        let mut only = base();
        only.extend(HELLO).unwrap();
        assert_eq!(s.fingerprint(), only.fingerprint());
        assert_ne!(s.fingerprint(), before);
        assert!(hello.exists());
    }

    /// A file whose stem names a linked service is skipped silently: its
    /// code's own text was applied.
    #[test]
    fn a_linked_service_skips_its_file() {
        let t = Temp::new("linked");
        t.put("hello.schema", HELLO);
        let (files, _) = load(&t.0);
        let mut s = base();
        let before = s.fingerprint();
        assert!(apply(&mut s, &files, &["hello"]).is_empty());
        assert_eq!(s.fingerprint(), before);
        assert!(!s.services.contains_key("hello"));
    }
}

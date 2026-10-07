//! No-code services that read a path (`from file`, and `from poll` of a
//! whitespace-free path, grammar.md) checked against the file there: a
//! program at the path was almost certainly meant to run, and reading it
//! fails at runtime ("a path that names a program"), so the check says
//! so when the config is checked (`strand check`, `strand run`'s loader,
//! the LSP).
//!
//! This touches the disk (one `stat` and a four-byte read per such
//! service), so it is not part of [`crate::compile`]: the callers that
//! check against the outside world run it, like [`super::dbus::check`].
//! A path that is missing or unreadable is not reported: the service
//! waits for its file at runtime.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::diagnostic::Diagnostic;
use crate::hir::{self, PollTarget, SourceSpec};

/// Warn on every `from file`/`from poll` path of `program` that names a
/// program. `~/` is the home directory, a relative path is under
/// `config_dir` (not checked without one), as at runtime.
pub fn check(program: &hir::Program, config_dir: Option<&Path>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for file in &program.files {
        for item in &file.items {
            let hir::Item::Service(s) = item else {
                continue;
            };
            let (path, poll) = match &s.spec {
                Some(SourceSpec::File { path }) => (path, false),
                Some(SourceSpec::Poll {
                    target: PollTarget::File(path),
                    ..
                }) => (path, true),
                _ => continue,
            };
            let Some(resolved) = resolve(path, config_dir) else {
                continue;
            };
            if !is_program(&resolved) {
                continue;
            }
            let mut d = Diagnostic::warning(
                "check::poll_program",
                format!("`{path}` is a program: this reads the file, it does not run it"),
            )
            .with_label_in(s.file, s.source_span, "reads the program's bytes");
            d.help = Some(if poll {
                format!("to run it write `[\"{path}\"]` and `permit exec \"{path}\"`")
            } else {
                format!(
                    "to read what it prints, `from listen [\"{path}\"]` or \
                     `from poll [\"{path}\"] every 5s`, with `permit exec \"{path}\"`"
                )
            });
            out.push(d);
        }
    }
    out
}

/// The path a service reads, as the runtime resolves it.
fn resolve(path: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    let p = PathBuf::from(path);
    if p.is_relative() {
        return config_dir.map(|d| d.join(p));
    }
    Some(p)
}

/// An executable regular file that starts like a program (a script's
/// `#!` or an ELF header): the runtime's rule for refusing to read it.
pub fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // A stat first: only a regular file is opened (a FIFO's open would
    // block).
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {}
        _ => return false,
    }
    let mut head = [0u8; 4];
    let n = std::fs::File::open(path)
        .and_then(|f| {
            let mut buf = Vec::with_capacity(4);
            f.take(4).read_to_end(&mut buf)?;
            Ok(buf)
        })
        .map(|b| {
            head[..b.len()].copy_from_slice(&b);
            b.len()
        })
        .unwrap_or(0);
    head[..n].starts_with(b"\x7fELF") || head[..n].starts_with(b"#!")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceMap;

    fn warnings(src: &str, dir: &Path) -> Vec<Diagnostic> {
        let mut map = SourceMap::new();
        map.add("a.strand", src.to_string());
        let c = crate::compile(&map);
        assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
        check(&c.program, Some(dir))
    }

    /// `from poll "/usr/bin/uptime"` reads the program's bytes: the check
    /// says so (and how to run it); a document, a missing file, a
    /// non-executable script and a command are not reported.
    #[test]
    fn a_path_that_names_a_program_is_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("strand-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("gpu-temp");
        std::fs::write(&script, "#!/bin/sh\necho 40\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plain = dir.join("plain.sh");
        std::fs::write(&plain, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(dir.join("doc.json"), "{\"a\": 1}").unwrap();
        let s = script.display();

        let w = warnings(
            &format!("service t from poll \"{s}\" every 5s {{ up: text = a }}"),
            &dir,
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].code, "check::poll_program");
        assert!(!w[0].is_error());
        assert!(
            w[0].help
                .as_deref()
                .unwrap()
                .contains(&format!("[\"{s}\"]"))
        );

        // Relative to the config directory, as at runtime; `from file`
        // too.
        let w = warnings("service t from file \"./gpu-temp\" { up: text = a }", &dir);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].help.as_deref().unwrap().contains("from listen"));

        for quiet in [
            "service t from poll \"./doc.json\" every 5s { a: int = a }".to_string(),
            "service t from poll \"./missing\" every 5s { a: int = a }".to_string(),
            "service t from file \"./plain.sh\" { a: int = a }".to_string(),
            format!("permit exec\nservice t from poll [\"{s}\"] every 5s {{ a: text = a }}"),
        ] {
            assert!(warnings(&quiet, &dir).is_empty(), "{quiet}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_elf_binary_is_a_program() {
        // Any ELF the test can find: its own executable.
        let me = std::env::current_exe().unwrap();
        assert!(is_program(&me));
        assert!(!is_program(Path::new("/nonexistent/strand")));
        assert!(!is_program(Path::new("/")));
    }
}

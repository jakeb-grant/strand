//! The local files an `image`, `svg` or `lottie` draws (spec.md 6.1): a
//! relative path is under the config directory, as a settings file's, a
//! service file's, a shader's and a template's are.
//!
//! Render reads a file by the path it is handed, and the process's
//! directory is not the config's (bug 8, owner testing 2026-10-14), so
//! instantiate makes a relative source absolute with [`local_source`]
//! before it reaches render, whether written as a literal or computed.
//! [`check`] warns when a literal source names no file. Which `image`
//! sources are files and which are icon names is
//! [`strand_scene::source_is_file`], the rule render draws by, so an icon
//! name is never looked for on disk. [`check`] touches the disk, so it
//! is not part of [`crate::compile`]: `strand check` and the LSP run it,
//! like [`super::files::templates`].

use std::path::{Path, PathBuf};

use crate::diagnostic::Diagnostic;
use crate::hir::{self, ExprKind, Node};
use crate::source::FileId;
use crate::syntax::Span;

/// The elements whose source is a local file (`image`'s only when
/// [`strand_scene::source_is_file`] says so).
pub const KINDS: [&str; 3] = ["image", "svg", "lottie"];

/// Whether `source`, the source of an element of kind `kind`, is a local
/// file: an `svg`'s or `lottie`'s always, an `image`'s when it has a
/// path's shape, nothing else's.
pub fn is_local(kind: &str, source: &str) -> bool {
    match kind {
        "svg" | "lottie" => true,
        "image" => strand_scene::source_is_file(source),
        _ => false,
    }
}

/// A relative local source made absolute under `config_dir`
/// (`dot.svg`, `icons/wifi.svg`, `./a.png`), against the process's
/// directory too when `config_dir` is relative (`strand run
/// target/try/island`), with its `.` components dropped, as the watcher
/// reports a file ([`absolute`]); `None` for anything that stays as
/// written: an absolute path, `~/…`, `file://…`, another URL, an icon
/// name, an empty source or a non-UTF-8 config directory.
pub fn local_source(kind: &str, source: &str, config_dir: &Path) -> Option<String> {
    let src = source.trim();
    if src.is_empty() || !is_local(kind, src) || src.starts_with("~/") || src.contains("://") {
        return None;
    }
    let p = Path::new(src);
    if !p.is_relative() {
        return None;
    }
    let rest = src.strip_prefix("./").unwrap_or(src);
    absolute(&config_dir.join(rest))
        .to_str()
        .map(str::to_string)
}

/// `path` made absolute against the process's directory, `.` components
/// dropped and nothing else resolved (`..` and symlinks stay): the path
/// the file watcher reports a file by (`strand_watch::paths::absolute`),
/// so a recorded asset and a reported change are the same path.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The file a local source names: `~/` under the home directory,
/// `file://` decoded, a relative path under `config_dir` (`None` without
/// one), as at run time; [`absolute`], as the watcher reports it.
pub fn file_of(source: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    let src = source.trim();
    if let Some(rest) = src.strip_prefix("file://") {
        return Some(absolute(Path::new(&percent_decode(rest))));
    }
    if src.contains("://") {
        return None;
    }
    if let Some(rest) = src.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| absolute(&PathBuf::from(h).join(rest)));
    }
    let p = Path::new(src);
    if p.is_relative() {
        let rest = src.strip_prefix("./").unwrap_or(src);
        return config_dir.map(|d| absolute(&d.join(rest)));
    }
    Some(absolute(p))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Warn (`check::missing_file`) on every literal local source of an
/// `image`, `svg` or `lottie` in `program` that names no file. A computed
/// source and an icon name are never checked; a relative path is not
/// checked without a `config_dir`.
pub fn check(program: &hir::Program, config_dir: Option<&Path>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    each_source(program, &mut |file, kind, span, path| {
        if !is_local(kind, path) {
            return;
        }
        let Some(full) = file_of(path, config_dir) else {
            return;
        };
        // A stat: a FIFO is a file that exists, and is never opened here.
        if std::fs::metadata(&full).is_ok_and(|m| !m.is_dir()) {
            return;
        }
        let relative = Path::new(path.trim()).is_relative() && !path.trim().starts_with("~/");
        let help = if relative {
            format!(
                "a relative path is under the config directory: {}",
                full.display()
            )
        } else {
            format!("nothing at {}", full.display())
        };
        let mut d = Diagnostic::warning(
            "check::missing_file",
            format!("`{path}` names no file: this `{kind}` draws nothing"),
        )
        .with_label_in(file, span, "no such file")
        .with_help(help);
        if kind == "image" {
            // The way out for a name meant as an icon.
            if let Some(h) = &mut d.help {
                h.push_str("; an icon name is written without an extension");
            }
        }
        out.push(d);
    });
    out
}

/// Every literal source (the positional, or a `source:` prop) of an
/// `image`, `svg` or `lottie` in `program`.
fn each_source(program: &hir::Program, f: &mut dyn FnMut(FileId, &str, Span, &str)) {
    let mut visit = |file: FileId, e: &hir::Element| {
        let hir::ElementKind::Builtin(kind) = &e.kind else {
            return;
        };
        if !KINDS.contains(&kind.as_str()) {
            return;
        }
        let exprs = e.arg.iter().chain(
            e.props
                .iter()
                .filter(|p| p.name == "source")
                .map(|p| &p.value),
        );
        for x in exprs {
            if let ExprKind::Text(path) = &x.kind {
                f(file, kind, x.span, path);
            }
        }
    };
    for file in &program.files {
        for item in &file.items {
            match item {
                hir::Item::Component(c) => nodes(file.file, &c.body, &mut visit),
                hir::Item::Surface(s) => element(file.file, &s.element, &mut visit),
                _ => {}
            }
        }
    }
}

fn nodes(file: FileId, ns: &[Node], f: &mut dyn FnMut(FileId, &hir::Element)) {
    for n in ns {
        match n {
            Node::Element(e) => element(file, e, f),
            Node::If(i) => {
                nodes(file, &i.then, f);
                nodes(file, &i.else_, f);
            }
            Node::For(l) => nodes(file, &l.body, f),
            Node::Match(m) => {
                for (_, arm) in &m.arms {
                    nodes(file, arm, f);
                }
            }
            _ => {}
        }
    }
}

fn element(file: FileId, e: &hir::Element, f: &mut dyn FnMut(FileId, &hir::Element)) {
    f(file, e);
    nodes(file, &e.children, f);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_local_sources_go_under_the_config_dir() {
        let d = Path::new("/cfg");
        let abs = |k: &str, s: &str| local_source(k, s, d);
        assert_eq!(abs("svg", "dot.svg").as_deref(), Some("/cfg/dot.svg"));
        assert_eq!(abs("svg", "gauge").as_deref(), Some("/cfg/gauge"));
        assert_eq!(
            abs("image", "icons/wifi.svg").as_deref(),
            Some("/cfg/icons/wifi.svg")
        );
        assert_eq!(abs("image", "./a.png").as_deref(), Some("/cfg/a.png"));
        assert_eq!(
            abs("lottie", "loader.json").as_deref(),
            Some("/cfg/loader.json")
        );
        for (k, s) in [
            ("image", "firefox"),
            ("image", "network-wireless-symbolic"),
            ("image", "/abs/a.png"),
            ("image", "~/a.png"),
            ("image", "file:///a.png"),
            ("svg", "https://example.com/a.svg"),
            ("icon", "dot.svg"),
            ("svg", ""),
        ] {
            assert_eq!(abs(k, s), None, "{k} {s}");
        }
    }

    /// (m6-owner-fixes review) A relative config directory (`strand run
    /// target/try/island`) still gives an absolute source, under the
    /// process's directory, and `.` components go: the path the watcher
    /// reports the file by. `file_of` gives the same path, for any
    /// source shape.
    #[test]
    fn a_relative_config_dir_gives_the_path_the_watcher_reports() {
        let cwd = std::env::current_dir().unwrap();
        let d = Path::new("target/try/island");
        let want = cwd.join("target/try/island/icons/a.svg");
        assert_eq!(
            local_source("svg", "icons/./a.svg", d).as_deref(),
            want.to_str()
        );
        assert_eq!(file_of("icons/./a.svg", Some(d)), Some(want.clone()));
        assert_eq!(
            file_of(want.to_str().unwrap(), None),
            Some(want.clone()),
            "as recorded from the source drawn"
        );
        assert_eq!(
            file_of("/abs/./x.png", None),
            Some(PathBuf::from("/abs/x.png"))
        );
    }
}

//! Strand configs on the site checked as `strand check` checks them,
//! offline: the language server's analysis (`Analysis::offline`, as
//! strand-dev's guide test uses it), so the site promises what the guide
//! does. The two checks that look outside a config (`from dbus`
//! introspection and the installed lock helper) are not run.

use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_dev::workspace::{Analysis, ConfigKey, Input};

/// What a config's check found.
#[derive(Debug)]
pub struct Checked {
    pub errors: usize,
    pub warnings: usize,
    /// The diagnostics as `strand check` prints them, without colour.
    pub text: String,
    /// Files `strand fmt` would change.
    pub unformatted: Vec<String>,
}

fn scratch(tag: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "strand-site-{}-{tag}-{:x}",
        std::process::id(),
        blake3::hash(tag.as_bytes()).as_bytes()[0]
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    dir.canonicalize().map_err(|e| e.to_string())
}

/// Checks the config made of `files` (name, text), written into a
/// scratch directory of its own, and whether each is formatted.
pub fn config(tag: &str, files: &[(String, String)]) -> Result<Checked, String> {
    let dir = scratch(tag)?;
    let result = check_in(&dir, files);
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn check_in(dir: &Path, files: &[(String, String)]) -> Result<Checked, String> {
    let mut inputs = Vec::new();
    for (name, text) in files {
        if name.contains('/') || !name.ends_with(".strand") {
            return Err(format!("`{name}`: a config file is a bare `<name>.strand`"));
        }
        let p = dir.join(name);
        std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
        inputs.push(Input::new(
            format!("file://{}", p.display()),
            p.display().to_string(),
            text.as_str().into(),
        ));
    }
    let an = Analysis::offline(
        ConfigKey::Dir(dir.to_path_buf()),
        inputs,
        strand_dev::server::schema(),
    );
    let diags = &an.compiled.diagnostics;
    let text = render(diags, &an.map, Style::Plain);
    // The scratch directory's path is not the reader's: show the names.
    let text = text.replace(&format!("{}/", dir.display()), "");
    let unformatted = files
        .iter()
        .filter(|(_, t)| strand_compiler::fmt::format(t).map_or(true, |f| &f != t))
        .map(|(n, _)| n.clone())
        .collect();
    Ok(Checked {
        errors: diags.iter().filter(|d| d.is_error()).count(),
        warnings: diags.iter().filter(|d| !d.is_error()).count(),
        text,
        unformatted,
    })
}

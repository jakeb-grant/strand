//! Test support: the IPC traffic captured from real compositors
//! (`tests/fixtures/*-captured`), read for the adapters' unit tests.

use std::path::PathBuf;

/// The text of `tests/fixtures/<path>`.
pub(crate) fn fixture(path: &str) -> String {
    let full = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// The `## name` sections of an events file, in order, each with its
/// traffic lines (lines starting with `#` and blank lines are markers).
pub(crate) fn bursts(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            out.push((name.trim().to_string(), Vec::new()));
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else {
            let Some((_, lines)) = out.last_mut() else {
                panic!("traffic before the first `## name`: {line}");
            };
            lines.push(line.to_string());
        }
    }
    out
}

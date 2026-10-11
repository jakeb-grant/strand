//! The guide's code blocks check (m5-plan, S-docs' tests; written by S-lsp,
//! decisions.md m5-lsp): applied in page order, each block replacing the
//! file it names, the config after every step has no error and no
//! warning, and every file in it is already formatted. The `strand new`
//! template (`docs/guide/template/bar.strand`) passes the same.
//!
//! The checks are the language server's (`Analysis`: the compiler with the
//! builtin and service schemas, the `from file`/`poll` path and shader
//! checks), which are `strand check`'s less the two that look outside the
//! config: `from dbus` introspection (the LSP asks the bus in the
//! background and never waits; a check here must not depend on a bus) and
//! the installed lock helper.

mod support;

use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_dev::workspace::{Analysis, ConfigKey, Input};
use support::guide;

/// A fresh directory for one config.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-guide-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// Checks the config in `dir` as `strand check` would (see the header);
/// on any error or warning, panics naming the step and showing them.
fn check(dir: &Path, step: &str) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    files.sort();
    let inputs = files
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p).unwrap();
            Input::new(
                format!("file://{}", p.display()),
                p.display().to_string(),
                text.into(),
            )
        })
        .collect();
    let an = Analysis::new(
        ConfigKey::Dir(dir.to_path_buf()),
        inputs,
        strand_dev::server::schema(),
    );
    let diags = &an.compiled.diagnostics;
    assert!(
        diags.is_empty(),
        "{step}: {} error(s), {} warning(s) in the config after this step:\n{}",
        diags.iter().filter(|d| d.is_error()).count(),
        diags.iter().filter(|d| !d.is_error()).count(),
        render(diags, &an.map, Style::Plain),
    );
    for p in &files {
        let text = std::fs::read_to_string(p).unwrap();
        let formatted = strand_compiler::fmt::format(&text)
            .unwrap_or_else(|e| panic!("{step}: {} does not format: {e:?}", p.display()));
        assert!(
            formatted == text,
            "{step}: `strand fmt` would change {}:\n--- as written\n{text}--- formatted\n{formatted}",
            p.display(),
        );
    }
}

#[test]
fn every_guide_block_checks() {
    let blocks = guide::blocks();
    assert!(
        !blocks.is_empty(),
        "no ```strand level=N file=NAME blocks found"
    );
    let dir = scratch("steps");
    for b in &blocks {
        std::fs::write(dir.join(&b.file), &b.text).unwrap();
        check(&dir, &format!("{} (level {}, {})", b.at, b.level, b.file));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_new_template_checks() {
    let template = guide::root().join("docs/guide/template/bar.strand");
    let dir = scratch("template");
    std::fs::copy(&template, dir.join("bar.strand")).unwrap();
    check(&dir, "docs/guide/template/bar.strand");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_block_nested_in_a_longer_fence_is_not_a_step() {
    let page = "\
````
```strand level=2 file=dot.strand
component Dot(ws: Workspace) { … }
```
````

```strand level=1 file=bar.strand
bar Top { edge: top }
```
";
    let fences = guide::fences(page);
    assert_eq!(fences.len(), 2);
    assert_eq!(fences[0].1, "");
    assert!(fences[0].2.contains("```strand level=2"));
    assert_eq!(fences[1].1, "strand level=1 file=bar.strand");
    assert_eq!(fences[1].2, "bar Top { edge: top }\n");
}

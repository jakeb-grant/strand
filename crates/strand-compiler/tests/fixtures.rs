//! Every `.strand` code block in `docs/design.md` (plus the table snippets
//! and grammar examples) parses with zero diagnostics, losslessly, with
//! nested spans, and with the AST shape pinned by a snapshot.

use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::syntax::{dump, lexer, parse};
use strand_compiler::{FileId, SourceMap};

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    files.sort();
    files
}

fn name(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}

/// The design doc's code blocks, in order, must match the fixture files
/// byte for byte, so the fixtures cannot drift from the design.
#[test]
fn fixtures_are_verbatim_design_blocks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let design = std::fs::read_to_string(root.join("docs/design.md")).unwrap();
    let mut blocks = Vec::new();
    let mut lines = design.lines();
    while let Some(line) = lines.next() {
        if line.starts_with("```") {
            let mut block = String::new();
            for l in lines.by_ref() {
                if l.starts_with("```") {
                    break;
                }
                block.push_str(l);
                block.push('\n');
            }
            blocks.push(block);
        }
    }
    let names = [
        "hello_bar",
        "bar",
        "launcher",
        "toasts",
        "osd",
        "theme",
        "rice_now",
    ];
    assert_eq!(
        blocks.len(),
        names.len(),
        "design.md gained or lost a code block"
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (block, name) in blocks.iter().zip(names) {
        let fixture = std::fs::read_to_string(dir.join(format!("{name}.strand"))).unwrap();
        assert_eq!(&fixture, block, "{name}.strand differs from design.md");
    }
}

#[test]
fn fixtures_parse_without_diagnostics() {
    let files = fixtures();
    assert!(files.len() >= 9, "fixtures missing: {files:?}");
    for path in files {
        let src = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(FileId::default(), &src);
        assert!(
            parsed.diagnostics.is_empty(),
            "{}:\n{}",
            path.display(),
            render(
                &parsed.diagnostics,
                &SourceMap::single(path.display().to_string(), String::from(&src)).0,
                Style::Plain
            )
        );
    }
}

#[test]
fn fixtures_lex_losslessly() {
    for path in fixtures() {
        let src = std::fs::read_to_string(&path).unwrap();
        let (tokens, diags) = lexer::lex(&src);
        assert!(diags.is_empty(), "{}", path.display());
        let joined: String = tokens.iter().map(|t| t.span.text(&src)).collect();
        assert_eq!(joined, src, "{}", path.display());
    }
}

#[test]
fn fixture_spans_nest() {
    for path in fixtures() {
        let src = std::fs::read_to_string(&path).unwrap();
        let tree = dump::tree(&parse(FileId::default(), &src).file);
        let len = src.len() as u32;
        let mut count = 0;
        tree.walk(&mut |node, parent| {
            count += 1;
            assert!(
                node.span.end <= len,
                "{}: {} out of bounds",
                path.display(),
                node.label
            );
            assert!(
                node.span.start <= node.span.end,
                "{}: {} inverted",
                path.display(),
                node.label
            );
            if let Some(parent) = parent {
                assert!(
                    parent.span.contains(node.span),
                    "{}: `{}` {:?} escapes parent `{}` {:?}",
                    path.display(),
                    node.label,
                    node.span,
                    parent.label,
                    parent.span
                );
            }
            if !node.inline && node.label.starts_with("element ") {
                let kind = node.label.trim_start_matches("element ");
                assert!(
                    node.span.text(&src).starts_with(kind),
                    "{}: element span does not start at its kind",
                    path.display()
                );
            }
        });
        assert!(count > 10);
    }
}

#[test]
fn fixture_ast_snapshots() {
    for path in fixtures() {
        let src = std::fs::read_to_string(&path).unwrap();
        let tree = dump::tree(&parse(FileId::default(), &src).file).render();
        insta::with_settings!({ snapshot_suffix => name(&path), prepend_module_to_snapshot => false }, {
            insta::assert_snapshot!("ast", tree);
        });
    }
}

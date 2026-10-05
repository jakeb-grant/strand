//! Diagnostics: file, line, column, caret, labels and did-you-mean, plus
//! recovery that keeps parsing after an error.

use strand_compiler::diagnostic::{Style, render, render_short};
use strand_compiler::syntax::ast::ItemKind;
use strand_compiler::syntax::{dump, parse};

/// Renders every diagnostic for `src` as the overlay and `strand check`
/// would (without colour).
fn report(src: &str) -> String {
    let parsed = parse(src);
    render(&parsed.diagnostics, "e.strand", src, Style::Plain)
}

fn helps(src: &str) -> Vec<String> {
    parse(src)
        .diagnostics
        .into_iter()
        .filter_map(|d| d.help)
        .collect()
}

#[test]
fn did_you_mean_keywords() {
    let cases = [
        ("componnet Foo { }", "did you mean `component`?"),
        (
            "component A { on chnage a, b { x = 1 } }",
            "did you mean `change`?",
        ),
        (
            "component A { for x im xs { text x } }",
            "did you mean `in`?",
        ),
        ("component A { box { width: 12pz } }", "did you mean `px`?"),
        (
            "service x from dbsu system \"a\" { f: int }",
            "did you mean `dbus`?",
        ),
        (
            "service x from dbus sytem \"a\" { f: int }",
            "did you mean `system`?",
        ),
        ("component A { stat x = 0 }", "did you mean `state`?"),
        (
            "component A { after 1s whlie x { y = 1 } }",
            "did you mean `while`?",
        ),
        ("use tokns base", "did you mean `tokens`?"),
        (
            "component A { on click { if x { a = 1 } esle { a = 2 } } }",
            "did you mean `else`?",
        ),
        (
            "component A { text pct (x) }",
            "to call or index, remove the space before `(`",
        ),
    ];
    for (src, help) in cases {
        let got = helps(src);
        assert!(
            got.iter().any(|h| h == help),
            "{src}: wanted {help:?}, got {got:?}"
        );
    }
}

#[test]
fn every_error_has_a_located_label() {
    for src in [
        "bar { }",
        "component A { box { bg: } }",
        "let a = (1 + ",
        "state x",
        "enum E { a b }",
        "tokens t { a }",
        "component A { box { x: 1 } : }",
        "\"open",
        "let x = 1 &",
        "@reset",
    ] {
        let parsed = parse(src);
        assert!(!parsed.diagnostics.is_empty(), "{src}: expected an error");
        for d in &parsed.diagnostics {
            let span = d.primary_span().expect("label");
            assert!(span.end as usize <= src.len());
        }
        let short = render_short(&parsed.diagnostics, "e.strand", src);
        assert!(short.starts_with("e.strand:1:"), "{short}");
    }
}

#[test]
fn rendered_reports() {
    let cases = [
        ("unknown_declaration", "componnet Foo {\n  text \"hi\"\n}\n"),
        (
            "misspelt_change",
            "component A {\n  on chnage a, b { x = 1 }\n}\n",
        ),
        ("unknown_unit", "component A {\n  box { width: 12pz }\n}\n"),
        ("missing_value", "component A {\n  box { bg: }\n}\n"),
        ("spaced_call", "component A {\n  text pct (x)\n}\n"),
        ("chained_comparison", "let a = x < y < z\n"),
        ("selector_as_colour", "let c = #needle\n"),
        (
            "unclosed_at_eof",
            "component A {\n  box {\n    text \"x\"\n}\n",
        ),
    ];
    for (name, src) in cases {
        insta::assert_snapshot!(name, report(src));
    }
}

/// design.md, "What you see" #3: a missing `}` fails, the error points at
/// the line, and the rest of the file still parses.
#[test]
fn missing_brace_in_bar_points_at_the_block() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bar.strand");
    let src = std::fs::read_to_string(path).unwrap();
    let broken = src.replacen(
        "    on click { ws.focus() }\n  }\n",
        "    on click { ws.focus() }\n",
        1,
    );
    assert_ne!(broken, src);
    let parsed = parse(&broken);
    assert_eq!(parsed.diagnostics.len(), 1, "{:?}", parsed.diagnostics);
    let d = &parsed.diagnostics[0];
    assert_eq!(d.code, "syntax::unclosed");
    // The secondary label names the `box {` whose `}` went missing.
    let box_open = broken.find("box { size: 8").unwrap() + 4;
    assert!(
        d.labels.iter().any(|l| l.span.start as usize == box_open),
        "{d:?}"
    );
    // Every component after the break is still a top-level item.
    let names: Vec<_> = parsed
        .file
        .items
        .iter()
        .filter_map(|i| match &i.kind {
            ItemKind::Component(c) => Some(c.name.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["Dot", "Volume", "Battery", "Clock", "Calendar"]);
    insta::assert_snapshot!("missing_brace_in_bar", report(&broken));
}

#[test]
fn recovery_continues_after_bad_items() {
    let src = "\
component A {
  box { width: 12 13 ; ) height: 4 }
  text \"ok\" { color: $fg }
  state = 3
  row { gap: 1 }
}
component B { text \"b\" }
";
    let parsed = parse(src);
    assert!(parsed.has_errors());
    let tree = dump::tree(&parsed.file).render();
    assert!(tree.contains("element text \"ok\""), "{tree}");
    assert!(tree.contains("prop color: $fg"), "{tree}");
    assert!(tree.contains("element row"), "{tree}");
    assert!(tree.contains("prop height: 4"), "{tree}");
    assert!(tree.contains("component B"), "{tree}");
}

#[test]
fn errors_do_not_cascade() {
    // One mistake, one diagnostic.
    for src in [
        "component A {\n  box { bg: }\n  text \"x\"\n}\n",
        "component A {\n  text a b c d\n}\n",
        "let x = f(1, 2\nlet y = 3\n",
        "component A {\n  for x im xs { text x }\n}\n",
    ] {
        let n = parse(src).diagnostics.len();
        assert_eq!(n, 1, "{src}\n{}", report(src));
    }
}

#[test]
fn misplaced_items_are_reported_but_parsed() {
    let parsed = parse("text \"top level\"\ncomponent A {\n  component B { }\n  enum E { a }\n}\n");
    let codes: Vec<_> = parsed.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(
        codes,
        [
            "syntax::misplaced",
            "syntax::misplaced",
            "syntax::misplaced"
        ]
    );
    assert_eq!(parsed.file.items.len(), 2);
}

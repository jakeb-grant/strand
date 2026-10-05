//! Diagnostics: file, line, column, caret, labels and did-you-mean, plus
//! recovery that keeps parsing after an error.

use strand_compiler::diagnostic::{Style, render, render_short};
use strand_compiler::syntax::ast::ItemKind;
use strand_compiler::syntax::{dump, parse};
use strand_compiler::{FileId, SourceMap};

/// Renders every diagnostic for `src` as the overlay and `strand check`
/// would (without colour).
fn report(src: &str) -> String {
    let parsed = parse(FileId::default(), src);
    render(
        &parsed.diagnostics,
        &SourceMap::single("e.strand", String::from(src)).0,
        Style::Plain,
    )
}

fn helps(src: &str) -> Vec<String> {
    parse(FileId::default(), src)
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
        // Clause keywords.
        (
            "component A { for x in xs kye x.id { text x } }",
            "did you mean `key`?",
        ),
        ("state p: [P] kye app = []", "did you mean `key`?"),
        ("state q = 1 persits", "did you mean `persist`?"),
        ("tokens c extnd base { a: 1 }", "did you mean `extends`?"),
        (
            "component A { on change a, b aftr 1s { x = 1 } }",
            "did you mean `after`?",
        ),
        ("state s frm \"a.toml\" { a: bool }", "did you mean `from`?"),
        ("type T { a: bool rx = true }", "did you mean `rw`?"),
        (
            "component A { if a { b } els { c } }",
            "did you mean `else`?",
        ),
        // A misspelt `override` is an error, not a new token (design.md,
        // "Loud overrides").
        (
            "tokens c extends base { overide space { 1: 2px } }",
            "did you mean `override`?",
        ),
        (
            "tokens c extends base {\n  overrde accent: #fff\n}\n",
            "did you mean `override`?",
        ),
        // Every keyword position, not just item starts.
        ("export stat x = 0", "did you mean `state`?"),
        (
            "component X(n: int) tokns { a: 1 } { text \"x\" }",
            "did you mean `tokens`?",
        ),
        ("permit exc \"a\"", "did you mean `exec`?"),
        (
            "service s from poll \"x\" evry 2s { a: int }",
            "did you mean `every`?",
        ),
        // A handler keyword is known by its block of statements.
        (
            "component A {\n  evry 1s { t += 1 }\n}\n",
            "did you mean `every`?",
        ),
        (
            "component A {\n  aftr 6s { n.expire() }\n}\n",
            "did you mean `after`?",
        ),
        (
            "component A {\n  onn click { open = !open }\n}\n",
            "did you mean `on`?",
        ),
        ("compnent Foo(n: int = 1) { }", "did you mean `component`?"),
        // Names are snake_case.
        (
            "component A { text \"x\" { max-width: 40% } }",
            "names are snake_case: `max_width`",
        ),
        (
            "component A { text \"x\" { color: $fg-muted } }",
            "names are snake_case: `$fg_muted`; or put spaces around `-` to subtract",
        ),
        // There is no unary `+`, so the separate-term fix drops the sign.
        (
            "component A { box { margin: 0 +2px } }",
            "write `2px` (no sign) for a separate term, \
             or put a space after the sign (`+ 2px`) to add",
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
        "component A { if a { b } els { c } }",
    ] {
        let parsed = parse(FileId::default(), src);
        assert!(!parsed.diagnostics.is_empty(), "{src}: expected an error");
        for d in &parsed.diagnostics {
            let span = d.primary_span().expect("label");
            assert!(span.end as usize <= src.len());
        }
        let short = render_short(
            &parsed.diagnostics,
            &SourceMap::single("e.strand", String::from(src)).0,
        );
        assert!(short.starts_with("e.strand:1:"), "{short}");
    }
    // An error at the start of an item points at the bad token on its own
    // line, not at the end of the line before.
    for (src, at) in [
        ("component A {\n  123\n  width: 1\n}\n", "e.strand:2:3:"),
        ("component A {\n  width: 1\n  ]\n}\n", "e.strand:3:3:"),
        ("component A {\n  box\n  => x\n}\n", "e.strand:3:3:"),
        (
            "component A {\n  on click {\n    ]\n  }\n}\n",
            "e.strand:3:5:",
        ),
        (
            "component A {\n  match m {\n    a => b\n    ]\n  }\n}\n",
            "e.strand:4:5:",
        ),
        ("enum E {\n  a\n  ]\n}\n", "e.strand:3:3:"),
    ] {
        let parsed = parse(FileId::default(), src);
        let short = render_short(
            &parsed.diagnostics,
            &SourceMap::single("e.strand", String::from(src)).0,
        );
        assert_eq!(parsed.diagnostics.len(), 1, "{src}\n{short}");
        assert!(short.starts_with(at), "{src}\nwanted {at}, got {short}");
        assert!(!short.contains("line break"), "{short}");
    }
}

#[test]
fn a_misspelt_clause_keyword_does_not_ask_for_a_value() {
    for src in [
        "state p: [P] kye app = []",
        "state s frm \"a.toml\" { a: bool }",
        "state a: Int kye = 3",
        "component A { for x in xs kye { text x } }",
        "tokens c extends base { overide space { 1: 2px } }",
        "tokens c extends base {\n  overrde accent: #fff\n}\n",
        "export stat x = 0",
        "component X(n: int) tokns { a: 1 } { text \"x\" }",
        "permit exc \"a\"",
        "service s from poll \"x\" evry 2s { a: int }",
        "component A {\n  evry 1s { t += 1 }\n}\n",
        "component A {\n  aftr 6s { n.expire() }\n}\n",
        "component A {\n  onn change a, b after 1s { x = 1 }\n}\n",
    ] {
        let parsed = parse(FileId::default(), src);
        assert_eq!(parsed.diagnostics.len(), 1, "{}", report(src));
        assert!(
            !report(src).contains("state needs a value"),
            "{}",
            report(src)
        );
    }
}

/// A misspelt keyword is read as the keyword: the tree has what was meant.
#[test]
fn a_misspelt_keyword_parses_as_the_keyword() {
    for (src, want) in [
        (
            "tokens c extends base { overide space { 1: 2px } }",
            "override token space",
        ),
        ("component A { aftr 6s { n.expire() } }", "after 6s"),
        ("component A { evry 1s { t += 1 } }", "every 1s"),
        ("component A { onn click { open = !open } }", "on click"),
        ("tokns base { space { 1: 4px } }", "tokens base"),
        ("barr Top { }", "bar Top"),
        ("export stat x = 0", "export state x"),
    ] {
        let t = dump::tree(&parse(FileId::default(), src).file).render();
        assert!(t.contains(want), "{src}: wanted {want:?} in\n{t}");
    }
}

/// Short names are not keywords (`n` is not `on`, `i` is not `if`), and a
/// statement in a tree says where statements go.
#[test]
fn statements_in_a_tree_are_not_misspelt_keywords() {
    for src in [
        "component A {\n  n.expire()\n}\n",
        "component A {\n  i.f()\n}\n",
        "component A {\n  x = 1\n}\n",
        "component A {\n  t += 1\n}\n",
        "component A {\n  n?.expire()\n}\n",
        "x = 1\n",
    ] {
        let parsed = parse(FileId::default(), src);
        assert_eq!(parsed.diagnostics.len(), 1, "{}", report(src));
        assert_eq!(
            parsed.diagnostics[0].help.as_deref(),
            Some("statements go inside a handler: `on click { … }`"),
            "{}",
            report(src)
        );
    }
}

#[test]
fn misplaced_elements_are_not_misspelt_keywords() {
    // `text` is two edits from `let`, `icon` two from `on`: these are
    // elements in the wrong place, not typos.
    for src in ["text \"x\"\n", "icon \"y\"\n"] {
        let got = helps(src);
        assert!(got.is_empty(), "{src}: {got:?}");
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
    let parsed = parse(FileId::default(), &broken);
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
    let parsed = parse(FileId::default(), src);
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
        "component A {\n  box\n  {\n    width: 1\n  }\n  text \"x\"\n}\nstate y = 1\n",
        "component A {\n  box { width: 1 }\n  {\n    width: 2\n  }\n  text \"x\"\n}\nstate y = 1\n",
        "component A {\n  box { width: 1 } : 2 ]\n  text \"x\"\n}\n",
        // A misspelt declaration keyword parses as the keyword.
        "compnent Foo(n: int = 1) {\n  text \"x\"\n}\n",
        "compnent Foo(n: int, d: bool = false) {\n  text \"x\"\n}\n",
        "tokns base { space { 1: 4px } }\n",
        "component A {\n  box {\n    height: 36 ~\n  }\n}\n",
    ] {
        let n = parse(FileId::default(), src).diagnostics.len();
        assert_eq!(n, 1, "{src}\n{}", report(src));
    }
    // An Allman `{` stays with its element, and nothing leaves `A`.
    let src = "component A {\n  box\n  {\n    width: 1\n  }\n  text \"x\"\n}\nstate y = 1\n";
    let t = dump::tree(&parse(FileId::default(), src).file).render();
    assert!(
        t.contains("component A\n    element box\n      prop width: 1\n    element text \"x\""),
        "{t}"
    );
}

#[test]
fn misplaced_items_are_reported_but_parsed() {
    let parsed = parse(
        FileId::default(),
        "text \"top level\"\ncomponent A {\n  component B { }\n  enum E { a }\n}\n",
    );
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

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn component_names(file: &strand_compiler::syntax::ast::File) -> Vec<String> {
    file.items
        .iter()
        .filter_map(|i| match &i.kind {
            ItemKind::Component(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect()
}

/// A missing `}` in an `enum` or a `match` stops at the next declaration
/// at column 0, like any other block.
#[test]
fn unclosed_enum_and_match_stop_at_the_next_declaration() {
    let src = fixture("theme.strand") + &fixture("bar.strand");
    let clean = parse(FileId::default(), &src);
    assert!(clean.diagnostics.is_empty());
    let want = component_names(&clean.file);
    assert!(want.len() >= 5, "{want:?}");
    let breaks = [
        // The enum's `}`.
        ("mocha }\n", "mocha\n"),
        // A one-line `match`'s `}`.
        ("_ => true }\n", "_ => true\n"),
        // A multi-line `match`'s `}`, alone at column 0.
        (
            "contrast: system.contrast),\n}\n",
            "contrast: system.contrast),\n",
        ),
    ];
    for (from, to) in breaks {
        let broken = src.replacen(from, to, 1);
        assert_ne!(broken, src, "{from:?}");
        let parsed = parse(FileId::default(), &broken);
        assert_eq!(
            parsed.diagnostics.len(),
            1,
            "{from:?}:\n{}",
            report(&broken)
        );
        assert_eq!(parsed.diagnostics[0].code, "syntax::unclosed");
        assert_eq!(component_names(&parsed.file), want, "{from:?}");
        // Every top-level item after the break survives.
        assert_eq!(parsed.file.items.len(), clean.file.items.len(), "{from:?}");
    }
}

/// The "probably missing its `}`" hint only names a sloppy `}` inside the
/// unclosed block, never one in an earlier, balanced declaration.
#[test]
fn missing_brace_hint_stays_inside_the_unclosed_block() {
    let src = fixture("bar.strand");
    // Mis-indent (but keep) the `}` of `image item.icon {` in `bar Top`.
    let sloppy = src.replacen(
        "          on secondary { item.menu.open() }\n        }\n",
        "          on secondary { item.menu.open() }\n      }\n",
        1,
    );
    assert_ne!(sloppy, src);
    assert!(parse(FileId::default(), &sloppy).diagnostics.is_empty());
    // Then lose a `}` inside `component Calendar`.
    let broken = sloppy.replacen(
        "      button \"›\" { on click { month = month.add(months: 1) } }\n    }\n",
        "      button \"›\" { on click { month = month.add(months: 1) } }\n",
        1,
    );
    assert_ne!(broken, sloppy);
    let parsed = parse(FileId::default(), &broken);
    assert_eq!(parsed.diagnostics.len(), 1, "{}", report(&broken));
    let image = broken.find("image item.icon {").unwrap() as u32;
    let calendar = broken.find("component Calendar").unwrap() as u32;
    for l in &parsed.diagnostics[0].labels {
        assert!(
            l.span.start > calendar,
            "label {:?} points before Calendar (image at {image}):\n{}",
            l,
            report(&broken)
        );
    }
    // Losing the `}` of Clock's `on click` points at that line (61), not
    // at the `text … {` that encloses it (60): the innermost suspect wins.
    let broken = src.replacen(
        "    on click { open = !open }\n",
        "    on click { open = !open\n",
        1,
    );
    assert_ne!(broken, src);
    let parsed = parse(FileId::default(), &broken);
    let short = render_short(
        &parsed.diagnostics,
        &SourceMap::single("e.strand", broken.clone()).0,
    );
    // The handler swallows the `popup` line, which reads as a statement
    // followed by `{`; that second error is a true consequence.
    let unclosed = parsed
        .diagnostics
        .iter()
        .find(|d| d.code == "syntax::unclosed")
        .unwrap_or_else(|| panic!("{short}"));
    let line_of = |off: u32| broken[..off as usize].matches('\n').count() + 1;
    let hint = unclosed
        .labels
        .iter()
        .find(|l| l.message.contains("probably missing"))
        .unwrap_or_else(|| panic!("no hint:\n{}", report(&broken)));
    assert_eq!(line_of(hint.span.start), 61, "{}", report(&broken));
    // The same with two tiny components.
    let src = "component A {\n  box {\n    text \"a\"\n      }\n}\ncomponent B {\n  box {\n    text \"b\"\n}\ncomponent C { }\n";
    let parsed = parse(FileId::default(), src);
    assert_eq!(parsed.diagnostics.len(), 1, "{}", report(src));
    let b = src.find("component B").unwrap() as u32;
    assert!(
        parsed.diagnostics[0]
            .labels
            .iter()
            .all(|l| l.span.start > b),
        "{}",
        report(src)
    );
}

#[test]
fn a_misspelt_top_level_keyword_is_one_error() {
    let src = "stat y = 1\ncomponent A { }\n";
    let parsed = parse(FileId::default(), src);
    assert_eq!(parsed.diagnostics.len(), 1, "{}", report(src));
    assert_eq!(
        parsed.diagnostics[0].help.as_deref(),
        Some("did you mean `state`?")
    );
    assert_eq!(component_names(&parsed.file), ["A"]);
}

#[test]
fn a_leading_byte_order_mark_is_fine() {
    let src = "\u{feff}component A { text \"x\" }\n";
    assert!(parse(FileId::default(), src).diagnostics.is_empty());
}

#[test]
fn diagnostics_carry_their_file() {
    let mut map = SourceMap::new();
    let _ = map.add("a.strand", "state x = 1\n");
    let b_src = "component B { box { width: 12pz } }\n";
    let b = map.add("b.strand", b_src);
    let parsed = parse(b, b_src);
    assert_eq!(parsed.file_id, b);
    assert!(!parsed.diagnostics.is_empty());
    assert!(parsed.diagnostics.iter().all(|d| d.file() == b));
    let out = render(&parsed.diagnostics, &map, Style::Plain);
    assert!(out.contains("[b.strand:1:30]"), "{out}");
    // The token stream is part of the result and is lossless.
    let joined: String = parsed.tokens.iter().map(|t| t.span.text(b_src)).collect();
    assert_eq!(joined, b_src);
}

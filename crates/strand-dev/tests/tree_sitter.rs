//! The tree-sitter grammar (`tree-sitter-strand`) against the compiler's
//! parser, which is the language (m5-plan, S-lsp; Risk 10, "tree-sitter
//! drifts from the parser"):
//! - every `.strand` file in the repository the parser accepts (the design
//!   fixtures, the test configs, the template, the negative fixtures that
//!   are only checker errors) and every guide block parses without an
//!   error node;
//! - the negative syntax fixtures, and a set of syntax errors grammar.md
//!   names, are errors to both;
//! - the queries compile, and the highlights capture every keyword in
//!   grammar.md's table as a keyword where the grammar takes it as one.
//!
//! The grammar accepts some lines the parser rejects with a better message
//! (a misspelt `els`, a `.` that ends a line); editors only highlight
//! them, and `strand check` and the language server still report them.

mod support;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use strand_compiler::FileId;
use strand_compiler::syntax::parse;
use support::guide;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

fn language() -> Language {
    tree_sitter_strand::LANGUAGE.into()
}

fn ts_parse(src: &str) -> Tree {
    let mut parser = Parser::new();
    parser.set_language(&language()).unwrap();
    parser.parse(src, None).unwrap()
}

/// The first error or missing node, as `line:column kind`, if any.
fn first_error(tree: &Tree) -> Option<String> {
    fn walk(n: Node<'_>) -> Option<Node<'_>> {
        if n.is_error() || n.is_missing() {
            return Some(n);
        }
        if !n.has_error() {
            return None;
        }
        (0..n.child_count()).find_map(|i| n.child(i).and_then(walk))
    }
    walk(tree.root_node()).map(|n| {
        let p = n.start_position();
        format!(
            "{}:{} {}{}",
            p.row + 1,
            p.column + 1,
            if n.is_missing() { "missing " } else { "" },
            n.kind()
        )
    })
}

/// The compiler parser's syntax errors (codes), warnings left out.
fn syntax_errors(src: &str) -> Vec<&'static str> {
    parse(FileId::default(), src)
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code)
        .collect()
}

/// Every `.strand` file under `crates/` and `docs/`, build output left out.
fn repo_strand_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "target" && !name.starts_with('.') {
                    walk(&path, out);
                }
            } else if name.ends_with(".strand") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for top in ["crates", "docs"] {
        walk(&guide::root().join(top), &mut out);
    }
    out.sort();
    out
}

/// A negative fixture's `// expect:` codes.
fn expected_codes(src: &str) -> Vec<String> {
    src.lines()
        .find_map(|l| l.strip_prefix("// expect:"))
        .map(|codes| codes.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default()
}

#[test]
fn every_accepted_file_and_guide_block_parses_without_errors() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for path in repo_strand_files() {
        let src = std::fs::read_to_string(&path).unwrap();
        if !syntax_errors(&src).is_empty() {
            continue;
        }
        checked += 1;
        if let Some(e) = first_error(&ts_parse(&src)) {
            failures.push(format!("{}:{e}", path.display()));
        }
    }
    let fixtures = guide::root().join("crates/strand-compiler/tests/fixtures");
    assert!(
        checked >= 9 + 60,
        "only {checked} files checked: the fixtures in {} and the negative fixtures should all be found",
        fixtures.display()
    );
    let blocks = guide::blocks();
    assert!(!blocks.is_empty(), "no guide blocks found");
    for b in blocks.iter().filter(|b| b.is_strand()) {
        assert_eq!(
            syntax_errors(&b.text),
            Vec::<&str>::new(),
            "{}: the compiler's parser rejects the block",
            b.at
        );
        if let Some(e) = first_error(&ts_parse(&b.text)) {
            failures.push(format!("{} (block {}): {e}", b.at, b.file));
        }
    }
    assert!(
        failures.is_empty(),
        "tree-sitter errors:\n{}",
        failures.join("\n")
    );
}

/// Syntax errors grammar.md names, each one the compiler's parser rejects.
const SYNTAX_ERRORS: &[&str] = &[
    // A prop's `:` ending a line has no value (rule 5).
    "bar Top {\n  bg:\n  edge: top\n}\n",
    // Two props on one line need a `;` (rule 2).
    "bar Top { edge: top height: 36 }\n",
    // Only a `}` ends an item before the next on its line: not a `)`, not
    // a `}` before tokens the line continued past.
    "bar Top { on click { x = 1 y = 2 } }\n",
    "let m = (1) let k = 2\n",
    "let m = match a { _ => 1 } + 1 let k = 2\n",
    "let g = f(x => { x }) let k = 2\n",
    // A lambda takes no member access (the parser's `postfix` wraps a
    // primary only).
    "bar Top { on click { f = x => { x }.y } }\n",
    // An optional block's `{` must be on the head's line (rule 6).
    "bar Top {\n  box\n  {\n  }\n}\n",
    // A `fn` body is a block.
    "fn get() = 1\n",
    // Strings do not span lines.
    "bar Top { text \"abc }\n",
    // Outside a prop value two terms in a row are an error.
    "state x = 1 2\n",
    "bar Top { text pct (x) }\n",
    // `persist` follows the value.
    "state early persist = 2\n",
    // A transition needs its value.
    "bar Top { width: 24 ~ }\n",
    // Elements and statements cannot stand at the top level.
    "text \"hi\"\n",
    "x = 1\n",
    "componnet Foo { }\n",
    // An item cannot start with a number.
    "bar Top { 1 + 2 }\n",
    // There are no tuples.
    "let x = (1, 2)\n",
    // An assignment needs its right side.
    "bar Top { on click { x = } }\n",
    // Braces must balance.
    "bar Top { edge: top\n",
    "bar Top { edge: top } }\n",
    // A source kind and a bus are one of the listed words; `poll` needs
    // `every`.
    "service t from pol \"x\" every 1s { a: text }\n",
    "service t from dbus systm \"a.b\" { a: text = A }\n",
    "service t from poll \"/tmp/x\" { a: text }\n",
];

#[test]
fn negative_syntax_fixtures_and_named_errors_are_errors() {
    let mut failures = Vec::new();
    let mut fixtures = 0;
    let dir = guide::root().join("crates/strand-compiler/tests/negative");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    paths.sort();
    for path in paths {
        let src = std::fs::read_to_string(&path).unwrap();
        if !expected_codes(&src)
            .iter()
            .any(|c| c.starts_with("syntax::"))
        {
            continue;
        }
        fixtures += 1;
        assert!(
            !syntax_errors(&src).is_empty(),
            "{}: expects a syntax error the parser does not report",
            path.display()
        );
        if first_error(&ts_parse(&src)).is_none() {
            failures.push(path.display().to_string());
        }
    }
    assert!(
        fixtures >= 5,
        "only {fixtures} negative syntax fixtures found"
    );
    for src in SYNTAX_ERRORS {
        assert!(
            !syntax_errors(src).is_empty(),
            "the compiler's parser accepts {src:?}"
        );
        if first_error(&ts_parse(src)).is_none() {
            failures.push(format!("{src:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "tree-sitter accepts what the parser rejects:\n{}",
        failures.join("\n")
    );
}

/// The words in grammar.md's "Keywords are contextual" table.
fn grammar_md_keywords() -> BTreeSet<String> {
    let text = std::fs::read_to_string(guide::root().join("docs/grammar.md")).unwrap();
    let section = text
        .split("#### Keywords are contextual")
        .nth(1)
        .expect("grammar.md has its keyword table");
    let mut words = BTreeSet::new();
    for line in section.lines().skip_while(|l| !l.starts_with('|')) {
        if !line.starts_with('|') {
            break;
        }
        let Some(cell) = line.split('|').nth(2) else {
            continue;
        };
        // The words in backticks (not the `{` of "`set` (only before `{`)").
        for (i, part) in cell.split('`').enumerate() {
            if i % 2 == 1 && part.chars().all(|c| c.is_ascii_alphabetic()) {
                words.insert(part.to_string());
            }
        }
    }
    assert!(
        words.len() > 40,
        "grammar.md's keyword table read as {words:?}"
    );
    words
}

/// Every keyword in grammar.md's table, each where the grammar takes it.
const KEYWORDS: &str = "\
enum Kind { a, b }
type Pin { app: text rw = \"\"; label: text = \"\" }
export state open = false persist
state pins: [Pin] key app = []
state prefs from \"p.toml\" { n: int = 1 }
let dark = match open { true => false, _ => null }
fn half(x: float) -> float { x / 2 }
tokens base { space { 1: 4px } }
tokens compact extends base { override space { 1: 2px } }
use tokens base, palette material(seed: #7aa2f7)
service ppd from dbus system \"net.hadess.PowerProfiles\" { profile: text rw = ActiveProfile }
service user from dbus session \"org.example.X\" { n: int = N }
permit exec \"sensors\"
action temps(chip: text) [\"sensors\", chip]
service ups from dbus system \"org.example.Ups\" { action: text = Action; action hold(r: text) = Hold }
keyframes shake { 0%, 100% { x: 0 }; duration: 300ms }
on notifications.received(n) { open = true }
after 1s while open { open = false }
every 1s { open = !open }
component Card { col { slot } }
bar Top {
  when hover { opacity: 1 }
  if open { text \"a\" } else { text \"b\" }
  match open { true => text \"y\", _ => text \"n\" }
  for p in pins key p.app { text p.label }
  on change open after 1s { await sleep(1s); play shake; let k = 1; if k > 0 { open = false } }
  enter { opacity: 0 }
  exit { opacity: 0 }
  set { $surface: $surface.alpha(0.5) }
  play shake
  state n = 0
  let m = n
  Card
}
panel P { text \"p\" }
osd O { text \"o\" }
lock { text \"l\" }
";

#[test]
fn highlights_capture_every_keyword_in_grammar_md() {
    let lang = language();
    for (name, q) in [
        ("highlights", tree_sitter_strand::HIGHLIGHTS_QUERY),
        ("locals", tree_sitter_strand::LOCALS_QUERY),
        ("indents", tree_sitter_strand::INDENTS_QUERY),
        ("injections", tree_sitter_strand::INJECTIONS_QUERY),
    ] {
        if let Err(e) = Query::new(&lang, q) {
            panic!("queries/{name}.scm does not compile: {e}");
        }
    }

    assert_eq!(syntax_errors(KEYWORDS), Vec::<&str>::new());
    let tree = ts_parse(KEYWORDS);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());

    // Capture names by node, for every capture the highlights make.
    let query = Query::new(&lang, tree_sitter_strand::HIGHLIGHTS_QUERY).unwrap();
    let names = query.capture_names();
    let mut captured: HashMap<usize, Vec<&str>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(&query, tree.root_node(), KEYWORDS.as_bytes());
    while let Some((m, i)) = captures.next() {
        let c = m.captures()[*i];
        captured
            .entry(c.node.id())
            .or_default()
            .push(names[c.index as usize]);
    }

    // Each keyword's tokens: the leaves whose text is the word and which
    // the grammar took as that word rather than as a name.
    let mut tokens: HashMap<String, Vec<Node<'_>>> = HashMap::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
        if n.child_count() == 0 && n.kind() != "identifier" {
            let text = &KEYWORDS[n.byte_range()];
            tokens.entry(text.to_string()).or_default().push(n);
        }
    }
    // `true`, `false` and `null` are values, highlighted as such.
    let keyword_like =
        |name: &str| name.starts_with("keyword") || name == "boolean" || name == "constant.builtin";
    let mut failures = Vec::new();
    for kw in grammar_md_keywords() {
        let Some(nodes) = tokens.get(&kw) else {
            failures.push(format!("`{kw}` is not a keyword anywhere in the sample"));
            continue;
        };
        for n in nodes {
            // The token, or a node that is only the token (`(boolean)`,
            // `(slot)`).
            let mut caps = Vec::new();
            let mut at = Some(*n);
            while let Some(node) = at.filter(|a| a.byte_range() == n.byte_range()) {
                caps.extend(captured.get(&node.id()).cloned().unwrap_or_default());
                at = node.parent();
            }
            if !caps.iter().any(|c| keyword_like(c)) {
                let p = n.start_position();
                failures.push(format!(
                    "`{kw}` at {}:{} ({}) is captured as {caps:?}",
                    p.row + 1,
                    p.column + 1,
                    n.parent().map_or("", |p| p.kind()),
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The capture tree-sitter-highlight gives each node that the highlights
/// capture: its first pattern's (the CLI and helix take the first match).
fn first_captures<'t>(tree: &'t Tree, src: &str) -> HashMap<usize, (Node<'t>, String)> {
    let query = Query::new(&language(), tree_sitter_strand::HIGHLIGHTS_QUERY).unwrap();
    let names = query.capture_names();
    let mut first: HashMap<usize, (usize, Node<'t>, String)> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(&query, tree.root_node(), src.as_bytes());
    while let Some((m, i)) = captures.next() {
        let c = m.captures()[*i];
        let name = names[c.index as usize].to_string();
        first
            .entry(c.node.id())
            .and_modify(|e| {
                if m.pattern_index < e.0 {
                    *e = (m.pattern_index, c.node, name.clone());
                }
            })
            .or_insert((m.pattern_index, c.node, name));
    }
    first
        .into_iter()
        .map(|(id, (_, n, name))| (id, (n, name)))
        .collect()
}

#[test]
fn a_ternarys_two_halves_highlight_alike() {
    let src = "let t = a ? b : c\n";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let mut by_text: Vec<(&str, String)> = first_captures(&tree, src)
        .into_values()
        .filter(|(n, _)| n.parent().is_some_and(|p| p.kind() == "ternary_expression"))
        .filter(|(n, _)| !n.is_named())
        .map(|(n, name)| (&src[n.byte_range()], name))
        .collect();
    by_text.sort();
    assert_eq!(
        by_text,
        [
            (":", "keyword.conditional.ternary".to_string()),
            ("?", "keyword.conditional.ternary".to_string()),
        ]
    );
}

#[test]
fn a_contextual_keyword_is_a_name_elsewhere() {
    // grammar.md: no reserved words; `IDENT ':'` at an item's start is a
    // prop, and a keyword outside its position is an identifier.
    let src = "\
let dark = match look { auto => system.dark, _ => false }
component C(key: text) {
  input { type: password; enter: popin(0.8) }
  box { on key(k) { propagate() } }
  box { on change(x) { log(x) } }
  box { bg: conic(from: 90deg, $accent, $secondary) }
  text key.in
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let sexp = tree.root_node().to_sexp();
    for (node, want) in [
        ("system", "identifier"),
        ("type", "identifier"),
        ("enter", "identifier"),
        ("key", "identifier"),
        ("change", "identifier"),
        ("from", "identifier"),
    ] {
        let mut stack = vec![tree.root_node()];
        let mut kinds = BTreeSet::new();
        while let Some(n) = stack.pop() {
            let mut c = n.walk();
            stack.extend(n.children(&mut c));
            if n.child_count() == 0 && &src[n.byte_range()] == node {
                kinds.insert(n.kind().to_string());
            }
        }
        assert_eq!(
            kinds.iter().map(String::as_str).collect::<Vec<_>>(),
            [want],
            "`{node}` in {sexp}"
        );
    }
}

#[test]
fn line_breaks_end_items_unless_the_next_line_continues() {
    // grammar.md, "Lines and termination", rules 2, 3 and 4.
    let src = "\
state a = 1
state b = a
  ?? 2
state c = a +
  2
let d = [
  1,
  2,
]
fn f(x: int) -> int {
  x
  - 1
}
fn g(x: int) -> int {
  x
  -1
}
bar Top {
  edge: top; height: 36
  box { } text \"x\"
  text b
    .format()
  if a > 0 { text \"a\" }
  else { text \"b\" }
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let sexp = tree.root_node().to_sexp();
    // `?? 2`, `+ 2` and `- 1` continue their lines; `.format()` too.
    assert_eq!(sexp.matches("(state ").count(), 3, "{sexp}");
    assert!(
        sexp.contains("(binary_expression left: (identifier) right: (number))"),
        "{sexp}"
    );
    assert!(
        sexp.contains("(call_expression function: (member_expression"),
        "{sexp}"
    );
    // `else` on its own line belongs to the `if`.
    assert!(sexp.contains("alternative: (tree_block"), "{sexp}");
    // A line starting with a spaced `- 1` continues `f`'s body.
    let f = tree.root_node().named_child(4).unwrap();
    let f_body = f.child_by_field_name("body").unwrap();
    assert_eq!(f_body.named_child_count(), 1, "{}", f_body.to_sexp());
    // A line starting with a touching `-` starts a new statement.
    let g = tree.root_node().named_child(5).unwrap();
    let g_body = g.child_by_field_name("body").unwrap();
    assert_eq!(g_body.named_child_count(), 2, "{}", g_body.to_sexp());
    // `box { } text "x"`: two elements on one line.
    let bar = tree.root_node().named_child(6).unwrap();
    assert_eq!(bar.kind(), "surface");
    let body = bar.child_by_field_name("body").unwrap();
    let mut c = body.walk();
    let kinds: Vec<_> = body.named_children(&mut c).map(|n| n.kind()).collect();
    assert_eq!(
        kinds,
        ["prop", "prop", "element", "element", "element", "if_tree"]
    );
}

/// The named children's kinds of `node`.
fn kinds<'t>(node: Node<'t>) -> Vec<&'t str> {
    let mut c = node.walk();
    node.named_children(&mut c).map(|n| n.kind()).collect()
}

#[test]
fn an_item_ending_in_a_brace_may_be_followed_on_its_line() {
    // grammar.md rule 2: "An item that ends with a closing `}`" may be
    // followed by another on its line, whatever kind of item it is: the
    // parser's test is the previous token (`at_item_end`).
    let src = "\
let m = match a { _ => 1 } let k = 2
let g = x => { x } let h = 3
state s = match a { _ => 1 } persist
bar Top {
  text match open { true => \"a\", _ => \"b\" } Dot
  padding: match a { _ => 1 } 4
  on click { x = match a { _ => 1 } y = 2 }
  if a { text \"a\" } else { text \"b\" }
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let root = tree.root_node();
    assert_eq!(
        kinds(root),
        ["let", "let", "let", "let", "state", "surface"],
        "{}",
        root.to_sexp()
    );
    let state = root.named_child(4).unwrap();
    assert!(state.to_sexp().ends_with("))"), "{}", state.to_sexp());
    assert_eq!(
        state.child(state.child_count() - 1).unwrap().kind(),
        "persist"
    );
    let body = root
        .named_child(5)
        .unwrap()
        .child_by_field_name("body")
        .unwrap();
    assert_eq!(
        kinds(body),
        ["element", "element", "prop", "on_handler", "if_tree"],
        "{}",
        body.to_sexp()
    );
    // `padding: match … 4` keeps both terms, as the parser's group does.
    let padding = body.named_child(2).unwrap();
    assert!(
        padding.to_sexp().contains("(group "),
        "{}",
        padding.to_sexp()
    );
    let handler = body.named_child(3).unwrap();
    let stmts = handler.child_by_field_name("body").unwrap();
    assert_eq!(
        kinds(stmts),
        ["assignment", "assignment"],
        "{}",
        stmts.to_sexp()
    );
}

#[test]
fn a_spaced_value_after_a_brace_stops_where_the_parsers_does() {
    // The parser's `starts_term`: a prop name (a word before `:`) and a
    // tree keyword end a prop value's space-separated terms, so after the
    // `}` the next item starts on the same line (rule 2).
    let src = "\
bar Top {
  padding: match a { _ => 1 } edge: top
  padding: match a { _ => 1 } if x { text \"a\" }
  margin: 1, match a { _ => 1 } when open { text \"b\" }
  padding: match a { _ => 1 } (2)
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let body = tree
        .root_node()
        .named_child(0)
        .unwrap()
        .child_by_field_name("body")
        .unwrap();
    assert_eq!(
        kinds(body),
        ["prop", "prop", "prop", "if_tree", "prop", "when", "prop"],
        "{}",
        body.to_sexp()
    );
    // A spaced `(` is still a further term of a value, as in the parser.
    let last = body.named_child(6).unwrap();
    assert!(last.to_sexp().contains("(group "), "{}", last.to_sexp());
}

#[test]
fn a_brace_is_not_continued_by_what_cannot_continue_its_item() {
    // A call or an index touches (`touching()`), and a statement block
    // takes no binary `-`: after such a `}` the parser's `at_item_end`
    // starts the next item. Where a binary `-` may follow, it continues.
    let src = "\
bar Top {
  on click { match d { 1 => { x = 1 } -1 => { x = 2 } } }
  on click { x = match a { _ => 1 } (y) }
  on click { x = match a { _ => 1 } -1 }
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let body = tree
        .root_node()
        .named_child(0)
        .unwrap()
        .child_by_field_name("body")
        .unwrap();
    let stmts = |i| {
        let h = body.named_child(i).unwrap();
        h.child_by_field_name("body").unwrap()
    };
    let arms = stmts(0)
        .named_child(0)
        .unwrap()
        .named_child(0)
        .unwrap()
        .child_by_field_name("arms")
        .unwrap();
    assert_eq!(
        kinds(arms),
        ["match_arm", "match_arm"],
        "{}",
        arms.to_sexp()
    );
    assert_eq!(
        kinds(stmts(1)),
        ["assignment", "expression_statement"],
        "{}",
        stmts(1).to_sexp()
    );
    let minus = stmts(2);
    assert_eq!(kinds(minus), ["assignment"], "{}", minus.to_sexp());
    let right = minus
        .named_child(0)
        .unwrap()
        .child_by_field_name("right")
        .unwrap();
    assert_eq!(right.kind(), "binary_expression", "{}", minus.to_sexp());
}

#[test]
fn an_attribute_or_a_clause_word_prop_after_a_value_brace_starts_an_item() {
    // The parser's `starts_term` takes no `@` and no word followed by `:`,
    // `key:`, `persist:` and `while:` included, so a value ends there and
    // `at_item_end` starts the next item after the `}` (rule 2).
    let src = "\
bar Top {
  padding: match a { _ => 1 } @reset state q = \"\"
  box { padding: match a { _ => 1 } key: 2 }
  padding: match a { _ => 1 } persist: 1
  padding: match a { _ => 1 } while: 2
  padding: match a { _ => 1 } key 2
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let body = tree
        .root_node()
        .named_child(0)
        .unwrap()
        .child_by_field_name("body")
        .unwrap();
    let sexp = body.to_sexp();
    let ks = kinds(body);
    assert_eq!(ks.iter().filter(|k| **k == "prop").count(), 6, "{sexp}");
    assert_eq!(ks.iter().filter(|k| **k == "state").count(), 1, "{sexp}");
    let boxed = body
        .named_children(&mut body.walk())
        .find(|n| n.kind() == "element")
        .unwrap();
    let inner = boxed.child_by_field_name("body").unwrap();
    assert_eq!(kinds(inner), ["prop", "prop"], "{}", inner.to_sexp());
    // Without a `:`, `key` is the value's next term, as in the parser.
    let last = body
        .named_child(body.named_child_count() as u32 - 1)
        .unwrap();
    assert!(last.to_sexp().contains("(group "), "{}", last.to_sexp());
}

#[test]
fn a_touching_bracket_after_a_block_that_takes_no_postfix_starts_an_item() {
    // The parser applies a call or an index only to a primary (`postfix`),
    // and `match` is the only primary that ends in `}`: a lambda's block
    // body takes none, so after it `at_item_end` starts the next item.
    let src = "\
bar Top {
  on click { f = x => { x }(1) }
  on click { f = x => { x }[0] }
  on click { g = match a { _ => [h] }[0](1) }
}
";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let body = tree
        .root_node()
        .named_child(0)
        .unwrap()
        .child_by_field_name("body")
        .unwrap();
    let stmts = |i| {
        let h = body.named_child(i).unwrap();
        h.child_by_field_name("body").unwrap()
    };
    for i in 0..2 {
        assert_eq!(
            kinds(stmts(i)),
            ["assignment", "expression_statement"],
            "{}",
            stmts(i).to_sexp()
        );
        let right = stmts(i)
            .named_child(0)
            .unwrap()
            .child_by_field_name("right")
            .unwrap();
        assert_eq!(right.kind(), "lambda", "{}", stmts(i).to_sexp());
    }
    // A `match` is a primary: a touching index and call continue it.
    let m = stmts(2);
    assert_eq!(kinds(m), ["assignment"], "{}", m.to_sexp());
    let right = m
        .named_child(0)
        .unwrap()
        .child_by_field_name("right")
        .unwrap();
    assert_eq!(right.kind(), "call_expression", "{}", m.to_sexp());
}

#[test]
fn strings_hold_slashes_and_escapes() {
    let src = "bar Top { text \"https://example.org \\\"q\\\" \\u{2022}\" // a comment\n}\n";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let sexp = tree.root_node().to_sexp();
    assert!(
        sexp.contains("(string (escape_sequence) (escape_sequence) (escape_sequence))"),
        "{sexp}"
    );
    assert!(sexp.contains("(comment)"), "{sexp}");
}

/// (M6, grammar.md "Qualified names") `tray.Row e { … }` is one element
/// in both parsers, its file captured as a module and its name as a tag;
/// `n.expire()` in a tree stays an error in both.
#[test]
fn a_qualified_component_call_is_one_element() {
    let src = "bar Top {\n  tray.Row 3\n  tray.Row 4 { on click { n += 1 } }\n}\n";
    assert_eq!(syntax_errors(src), Vec::<&str>::new());
    let tree = ts_parse(src);
    assert_eq!(first_error(&tree), None, "{}", tree.root_node().to_sexp());
    let mut caps: Vec<(&str, String)> = first_captures(&tree, src)
        .into_values()
        .filter(|(n, _)| n.parent().is_some_and(|p| p.kind() == "element"))
        .filter(|(n, _)| matches!(&src[n.byte_range()], "tray" | "Row"))
        .map(|(n, name)| (&src[n.byte_range()], name))
        .collect();
    caps.sort();
    assert_eq!(
        caps,
        [
            ("Row", "tag".to_string()),
            ("Row", "tag".to_string()),
            ("tray", "module".to_string()),
            ("tray", "module".to_string()),
        ]
    );
    let bad = "component A {\n  n.expire()\n}\n";
    assert!(!syntax_errors(bad).is_empty());
    assert!(first_error(&ts_parse(bad)).is_some());
    // (Review minor 12) The touch rules agree: what follows the name
    // decides, in both parsers alike.
    for (line, ok) in [
        ("n.expire(1)", false),
        ("n.items[0]", false),
        ("a.b.c 1", false),
        ("a.b?.c", false),
        ("a.b = 1", false),
        ("a.b += 1", false),
        ("a.b=1", false),
        ("a. Row", false),
        ("a .Row", false),
        ("tray.Row (1)", true),
        ("tray.Row [1]", true),
        ("tray.Row \"x\" == \"y\"", true),
        ("tray.Row -1", true),
    ] {
        let src = format!("bar Top {{\n  {line}\n}}\n");
        let compiler = syntax_errors(&src).is_empty();
        let ts = ts_parse(&src);
        let ts_ok = first_error(&ts).is_none();
        assert_eq!(
            (compiler, ts_ok),
            (ok, ok),
            "{line}: {}",
            ts.root_node().to_sexp()
        );
    }
}

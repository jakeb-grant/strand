//! The disambiguation rules of `docs/grammar.md`, one assertion each.

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::syntax::{dump, parse};

/// The rendered tree of the items inside `component R { … }`.
fn body(src: &str) -> String {
    let full = format!("component R {{\n{src}\n}}\n");
    let parsed = parse(&full);
    assert!(
        parsed.diagnostics.is_empty(),
        "{}",
        render(&parsed.diagnostics, "t.strand", &full, Style::Plain)
    );
    let tree = dump::tree(&parsed.file).render();
    tree.lines()
        .skip(2)
        .map(|l| l.strip_prefix("    ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn top(src: &str) -> String {
    let parsed = parse(src);
    assert!(
        parsed.diagnostics.is_empty(),
        "{}",
        render(&parsed.diagnostics, "t.strand", src, Style::Plain)
    );
    let tree = dump::tree(&parsed.file).render();
    tree.lines()
        .skip(1)
        .map(|l| l.strip_prefix("  ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn errors(src: &str) -> usize {
    parse(src).diagnostics.len()
}

#[test]
fn items_end_at_semicolons_and_line_breaks() {
    assert_eq!(
        body("box { a: 1; b: 2\n c: 3 }"),
        "element box\n  prop a: 1\n  prop b: 2\n  prop c: 3"
    );
    assert_eq!(
        body("box { bg: a\n b: c }"),
        "element box\n  prop bg: a\n  prop b: c"
    );
}

#[test]
fn an_item_ending_in_a_block_needs_no_separator() {
    assert_eq!(body("box {} text \"x\""), "element box\nelement text \"x\"");
}

#[test]
fn leading_operators_continue_the_line() {
    assert_eq!(body("text a\n  ?? b"), "element text (?? a b)");
    assert_eq!(
        body("text a\n  .b()\n  ?.c"),
        "element text (?. (call (. a b)) c)"
    );
    assert_eq!(body("text c\n  ? x\n  : y"), "element text (?: c x y)");
    assert_eq!(
        body("text a\n  && b\n  || c"),
        "element text (|| (&& a b) c)"
    );
    assert_eq!(
        body("box { w: 1\n  ~ instant }"),
        "element box\n  prop w: 1 (~ instant)"
    );
}

#[test]
fn trailing_operators_and_commas_continue_the_line() {
    assert_eq!(body("text a ??\n  b"), "element text (?? a b)");
    assert_eq!(
        body("box { margin: 8,\n  8, 0 }"),
        "element box\n  prop margin: (commas 8 8 0)"
    );
}

#[test]
fn brackets_ignore_line_breaks_but_braces_restore_them() {
    assert_eq!(body("text f(a,\n  b)"), "element text (call f a b)");
    assert_eq!(body("text [\n 1,\n 2,\n]"), "element text (array 1 2)");
    assert_eq!(
        body("box { draw: (c) => {\n  c.a()\n  c.b()\n} }"),
        "element box\n  prop draw: (=> (params c) (block (expr (call (. c a))) (expr (call (. c b)))))"
    );
}

#[test]
fn else_may_start_a_line() {
    assert_eq!(
        body("if a { x }\nelse { y }"),
        "if a\n  then\n    element x\n  else\n    element y"
    );
}

#[test]
fn optional_blocks_must_start_on_the_head_line() {
    // `row` then a stray block on the next line: the line break ended `row`.
    assert!(errors("component R {\n  row\n  { gap: 1 }\n}\n") > 0);
    // Mandatory blocks may follow a line break.
    assert_eq!(body("when hover\n{ bg: x }"), "when hover\n  prop bg: x");
}

#[test]
fn clause_keywords_stay_on_their_line() {
    assert_eq!(body("state y = 1 persist"), "state y (= 1) persist");
    // On the next line `persist` is a separate (unknown) element, not a clause.
    assert_eq!(
        body("state y = 1\npersist"),
        "state y (= 1)\nelement persist"
    );
}

#[test]
fn positional_then_block() {
    assert_eq!(
        body("text windows.focused?.title ?? \"\" { color: $fg }"),
        "element text (?? (?. (. windows focused) title) \"\")\n  prop color: $fg"
    );
    assert_eq!(
        body("text match k { a => 1, _ => 2 } { x: 1 }"),
        "element text (match k (arm a 1) (arm _ 2))\n  prop x: 1"
    );
    assert_eq!(
        body("Toast n { dense: true }"),
        "element Toast n\n  prop dense: true"
    );
    assert_eq!(body("Volume"), "element Volume");
}

#[test]
fn named_head_argument() {
    assert_eq!(
        body("pages current: page { page wifi {} }"),
        "element pages (current: page)\n  element page wifi"
    );
}

#[test]
fn commas_spaces_and_transitions_in_values() {
    assert_eq!(
        body("box { margin: 8, 8, 0 }"),
        "element box\n  prop margin: (commas 8 8 0)"
    );
    assert_eq!(
        body("box { s: 0 8px 24px $a, 0 1px 2px $b }"),
        "element box\n  prop s: (commas (spaced 0 8px 24px $a) (spaced 0 1px 2px $b))"
    );
    assert_eq!(
        body("box { glow: 10 * wave(2s), $c }"),
        "element box\n  prop glow: (commas (* 10 (call wave 2s)) $c)"
    );
    assert_eq!(
        body("box { margin: 8, 8, 0 ~ instant }"),
        "element box\n  prop margin: (commas 8 8 0) (~ instant)"
    );
    assert_eq!(
        body("box { v: <-> audio.sink.volume ~ 200ms }"),
        "element box\n  prop v: <-> (. (. audio sink) volume) (~ 200ms)"
    );
}

#[test]
fn operators_always_bind_so_whitespace_never_changes_meaning() {
    assert_eq!(
        body("box { s: 0 -2px 8px }"),
        "element box\n  prop s: (spaced (- 0 2px) 8px)"
    );
    assert_eq!(
        body("box { s: 0 - 2px 8px }"),
        "element box\n  prop s: (spaced (- 0 2px) 8px)"
    );
    assert_eq!(
        body("box { s: 0 (-2px) 8px }"),
        "element box\n  prop s: (spaced 0 (paren (neg 2px)) 8px)"
    );
}

#[test]
fn calls_touch() {
    assert_eq!(
        body("box { v: pct(x) }"),
        "element box\n  prop v: (call pct x)"
    );
    assert_eq!(
        body("box { v: pct (x) }"),
        "element box\n  prop v: (spaced pct (paren x))"
    );
    assert_eq!(body("text xs[0]"), "element text (index xs 0)");
}

#[test]
fn a_missing_semicolon_is_not_swallowed_as_a_term() {
    assert!(errors("component R { box { a: 1 b: 2 } }") > 0);
    assert!(errors("component R { box { size: 16 on click { x() } } }") > 0);
}

#[test]
fn token_paths_and_methods() {
    assert_eq!(
        body("box { a: $space.2 }"),
        "element box\n  prop a: $space.2"
    );
    assert_eq!(
        body("box { a: $fg.muted }"),
        "element box\n  prop a: $fg.muted"
    );
    assert_eq!(
        body("box { a: $surface.alpha(0.5).mix($bg, 8%) }"),
        "element box\n  prop a: (call (. (call (. $surface alpha) 0.5) mix) $bg 8%)"
    );
    assert_eq!(
        body("box { a: $Toast.radius }"),
        "element box\n  prop a: $Toast.radius"
    );
}

#[test]
fn named_and_from_arguments() {
    assert_eq!(
        body("box { a: oklch(from $c, l: l + 0.1) }"),
        "element box\n  prop a: (call oklch (from $c) (l: (+ l 0.1)))"
    );
    assert_eq!(
        body("box { a: conic(from: 90deg) }"),
        "element box\n  prop a: (call conic (from: 90deg))"
    );
    assert_eq!(
        body("box { a: f(from) }"),
        "element box\n  prop a: (call f from)"
    );
}

#[test]
fn lambdas() {
    assert_eq!(
        top("let a = xs.filter(n => !n.x)"),
        "let a (= (call (. xs filter) (=> (params n) (! (. n x)))))"
    );
    assert_eq!(
        top("let a = (a: int, b) => a + b"),
        "let a (= (=> (params (a :int) b) (+ a b)))"
    );
    assert_eq!(top("let a = () => 1"), "let a (= (=> params 1))");
    assert_eq!(top("let a = (b)"), "let a (= (paren b))");
}

#[test]
fn precedence() {
    assert_eq!(
        top("let a = a ?? b || c && d == e < f + g * h"),
        "let a (= (?? a (|| b (&& c (== d (< e (+ f (* g h))))))))"
    );
    assert_eq!(
        top("let a = !a.b?.c(1)[2]"),
        "let a (= (! (index (call (?. (. a b) c) 1) 2)))"
    );
    assert_eq!(
        top("let a = c ? x : d ? y : z"),
        "let a (= (?: c x (?: d y z)))"
    );
    assert_eq!(top("let a = a - b - c"), "let a (= (- (- a b) c))");
    assert_eq!(top("let a = a ?? b ?? c"), "let a (= (?? a (?? b c)))");
}

#[test]
fn literals_and_units() {
    assert_eq!(
        top(
            "let a = [36, 0.72, 4px, 40%, 4ch, 270deg, 6s, 1.2s, 200ms, #7aa2f7, #fff, \"‹\", true, null]"
        ),
        "let a (= (array 36 0.72 4px 40% 4ch 270deg 6s 1.2s 200ms #7aa2f7ff #ffffffff \"‹\" true null))"
    );
    assert_eq!(top("let a = 8 % 3"), "let a (= (% 8 3))");
}

#[test]
fn handler_statements() {
    assert_eq!(
        body("on click { kind = volume; shown = true\n audio.sink.volume -= dy * 0.05 }"),
        "on click\n  assign = kind volume\n  assign = shown true\n  assign -= (. (. audio sink) volume) (* dy 0.05)"
    );
}

#[test]
fn types() {
    assert_eq!(
        top("state a: Async<[Hit]>? = null"),
        "state a :Async<[Hit]>? (= null)"
    );
    assert_eq!(
        top("state pins: [Pin] key app = []"),
        "state pins :[Pin] (key app) (= array)"
    );
}

#[test]
fn tokens_keys() {
    assert_eq!(
        top("tokens t { 1: 4px; $a.b: 1; c.d: 2; override e { f: 3 } }"),
        "tokens t\n  token 1 4px\n  token $a.b 1\n  token c.d 2\n  override token e\n    token f 3"
    );
}

#[test]
fn match_arms_split_on_commas_or_lines() {
    assert_eq!(
        top("let a = match x {\n  a => 1\n  b => 2,\n  _ => 3,\n}"),
        "let a (= (match x (arm a 1) (arm b 2) (arm _ 3)))"
    );
    assert_eq!(
        top("let a = match x { -1 => \"neg\", \"s\" => 2 }"),
        "let a (= (match x (arm (neg 1) \"neg\") (arm \"s\" 2)))"
    );
}

#[test]
fn keywords_are_contextual() {
    assert_eq!(
        body("input { type: password; text: <-> q; enter: fade; key: 1 }"),
        "element input\n  prop type: password\n  prop text: <-> q\n  prop enter: fade\n  prop key: 1"
    );
    assert_eq!(top("let state_of = in_month"), "let state_of (= in_month)");
}

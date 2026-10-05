//! The formatter: design.md's examples are already formatted, the result
//! is idempotent, means what the input meant (the syntax tree is equal up
//! to spans), keeps every comment, and never refuses a valid file. The
//! properties run on every fixture and on random valid edits of them.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use strand_compiler::FileId;
use strand_compiler::fmt::{FormatError, format, shape};
use strand_compiler::syntax::lexer::{TokenKind, lex};
use strand_compiler::syntax::parse;

fn fixture_paths() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    files.sort();
    files
}

fn fixtures() -> Vec<(String, String)> {
    fixture_paths()
        .into_iter()
        .map(|p| {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read_to_string(&p).unwrap())
        })
        .collect()
}

fn comments(src: &str) -> Vec<String> {
    let (toks, _) = lex(src);
    toks.iter()
        .filter(|t| t.kind == TokenKind::Comment)
        .map(|t| t.span.text(src).trim_end().to_string())
        .collect()
}

fn valid(src: &str) -> bool {
    !parse(FileId::default(), src).has_errors()
}

/// Everything the formatter promises, for one valid input.
fn assert_formats(name: &str, src: &str) -> String {
    let once = format(src).unwrap_or_else(|e| panic!("{name}: {e}\n--- input\n{src}"));
    let twice = format(&once).unwrap_or_else(|e| panic!("{name} (second pass): {e}"));
    assert_eq!(
        once, twice,
        "{name}: not idempotent\n--- input\n{src}\n--- once\n{once}"
    );
    let a = parse(FileId::default(), src);
    let b = parse(FileId::default(), &once);
    assert!(!b.has_errors(), "{name}: formatted text has errors");
    assert_eq!(
        shape(&a.file),
        shape(&b.file),
        "{name}: the syntax tree changed\n--- input\n{src}\n--- output\n{once}"
    );
    assert_eq!(comments(src), comments(&once), "{name}: comments changed");
    assert!(once.is_empty() || once.ends_with('\n') && !once.ends_with("\n\n"));
    for line in once.lines() {
        assert_eq!(line, line.trim_end(), "{name}: trailing whitespace");
        assert!(!line.contains('\t') || line.contains("//") || line.contains('"'));
    }
    once
}

/// The code blocks of design.md are written in the style the formatter
/// produces, so they are fixed points; so are the other fixtures.
#[test]
fn fixtures_are_formatted() {
    for (name, src) in fixtures() {
        let out = assert_formats(&name, &src);
        assert_eq!(out, src, "{name}.strand is not formatted");
    }
}

#[test]
fn messy_input_is_normalised() {
    let src = r#"

// header
state  open=false;
state pins : [Pin] key app=[]
let x=a+b*-c ;
let y = f( 1 ,2 ) ?? g(3)
type Pin { app : AppId ; label:text }
enum Kind{volume,brightness}
component  Dot( ws:Workspace , n:int=3 ){
        box{size:8;radius:full;bg:$fg.alpha( 0.25 ) ; hit:grow(6)
          when ws.occupied{bg:$fg.muted}


          when hover   { bg: $accent.hover }   // trailing
    on click{ ws.focus() ; open=!open; }
  }
}
fn clamp01(x:float)->float{x<0?0:x>1?1:x}
bar Top {
edge:top;;height:36
   if open {
     text "a"
   } else { text "b" }
}


"#;
    let want = r#"// header
state open = false
state pins: [Pin] key app = []
let x = a + b * -c
let y = f(1, 2) ?? g(3)
type Pin { app: AppId; label: text }
enum Kind { volume, brightness }
component Dot(ws: Workspace, n: int = 3) {
  box { size: 8; radius: full; bg: $fg.alpha(0.25); hit: grow(6)
    when ws.occupied { bg: $fg.muted }

    when hover   { bg: $accent.hover }         // trailing
    on click { ws.focus(); open = !open }
  }
}
fn clamp01(x: float) -> float { x < 0 ? 0 : x > 1 ? 1 : x }
bar Top {
  edge: top; height: 36
  if open {
    text "a"
  } else { text "b" }
}
"#;
    assert_eq!(assert_formats("messy", src), want);
}

#[test]
fn layout_rules() {
    let cases = [
        // A hanging block keeps lines aligned under its first item.
        (
            "bar B {\n    col { width: 600\n          bg: $surface\n      text \"x\"\n    }\n}\n",
            "bar B {\n  col { width: 600\n        bg: $surface\n    text \"x\"\n  }\n}\n",
        ),
        // Continuation lines keep their offset, at least two spaces.
        (
            "let a = b\n?? c\nlet d = e +\n      f\n",
            "let a = b\n  ?? c\nlet d = e +\n      f\n",
        ),
        // Calls touch; a spaced `(` is a new term and stays one.
        (
            "bar B { shadow: 0 (-2px) 8px $shadow }\n",
            "bar B { shadow: 0 (-2px) 8px $shadow }\n",
        ),
        // Two prefix minuses stay apart: `--b` reads like a decrement.
        (
            "let a = - -b\nlet c = -  -1\nlet d = !-e\nlet f = -g\n",
            "let a = - -b\nlet c = - -1\nlet d = !-e\nlet f = -g\n",
        ),
        // Types are written tight, ternaries spaced.
        (
            "let h : Async< [ Hit ] >? = x?y:z\n",
            "let h: Async<[Hit]>? = x ? y : z\n",
        ),
        // Brackets: contents relative to the opening line.
        (
            "let xs = [\n1,\n    2,\n        ]\n",
            "let xs = [\n  1,\n    2,\n]\n",
        ),
        // A separator a line break or brace already gives is dropped, but
        // not one that keeps the next line from continuing this one.
        (
            "component C { box { a: 1; }; }\n",
            "component C { box { a: 1 } }\n",
        ),
        // Blank lines: at most one, none after `{` or before `}`.
        (
            "component C {\n\n\n  text \"a\"\n\n\n\n  text \"b\"\n\n}\n",
            "component C {\n  text \"a\"\n\n  text \"b\"\n}\n",
        ),
        // Line endings become `\n`; a byte-order mark goes.
        (
            "\u{feff}state a = 1\r\nstate b = 2\rstate c = 3",
            "state a = 1\nstate b = 2\nstate c = 3\n",
        ),
        // A declaration under an attribute on its own line is an item,
        // at the top level and in a tree, not a continuation.
        (
            "@reset\n  state x = 1\n@a(1)\n@b\nlet y = 2\n",
            "@reset\nstate x = 1\n@a(1)\n@b\nlet y = 2\n",
        ),
        (
            "component C {\n  @reset\n    row { text \"a\" }\n  @reset\n  state s = 0\n}\n",
            "component C {\n  @reset\n  row { text \"a\" }\n  @reset\n  state s = 0\n}\n",
        ),
        // Empty blocks close up.
        ("component C { }\n", "component C {}\n"),
        // A mandatory block's `{` on its own line sits at its head.
        (
            "component C {\n  box {\n    every 1s\n        {\n      x = 1\n    }\n  }\n}\n",
            "component C {\n  box {\n    every 1s\n    {\n      x = 1\n    }\n  }\n}\n",
        ),
    ];
    for (src, want) in cases {
        assert!(valid(src), "not valid: {src:?}");
        assert_eq!(assert_formats("case", src), want, "input: {src:?}");
    }
}

#[test]
fn a_file_with_syntax_errors_is_left_alone() {
    let err = format("bar Top {\n  text \"x\"\n").unwrap_err();
    let FormatError::Syntax(diags) = &err else {
        panic!("{err}")
    };
    assert_eq!(diags.len(), 1);
    assert!(err.to_string().contains("1 syntax error"));
    assert_eq!(format("").unwrap(), "");
    assert_eq!(format("\n\n  \n").unwrap(), "");
    assert_eq!(format("// only\n").unwrap(), "// only\n");
}

// ---------------------------------------------------------------------------
// Random valid edits

/// Edits that keep most files valid: layout, separators, comments and
/// whole lines.
fn edit(src: &str, op: u8, seed: u64) -> String {
    let (toks, _) = lex(src);
    if toks.is_empty() {
        return src.to_string();
    }
    let pick = |n: usize| (seed % n.max(1) as u64) as usize;
    let t = toks[pick(toks.len())];
    let (s, e) = (t.span.start as usize, t.span.end as usize);
    let spaces = " ".repeat(1 + (seed >> 8) as usize % 6);
    let lines: Vec<&str> = src.split_inclusive('\n').collect();
    let line = pick(lines.len());
    match op % 10 {
        // Whitespace becomes other whitespace.
        0 if t.kind == TokenKind::Whitespace => format!("{}{spaces}{}", &src[..s], &src[e..]),
        // Space between two tokens.
        1 => format!("{} {}", &src[..s], &src[s..]),
        // A whitespace token goes.
        2 if t.kind == TokenKind::Whitespace => format!("{}{}", &src[..s], &src[e..]),
        // Re-indent a line.
        3 => {
            let mut out: String = lines[..line].concat();
            out.push_str(&spaces);
            out.push_str(lines[line].trim_start_matches([' ', '\t']));
            out.extend(lines[line + 1..].iter().copied());
            out
        }
        // Blank lines.
        4 => format!("{}\n\n{}", &src[..s], &src[s..]),
        // A trailing comment, or a comment line.
        5 => {
            let mut out: String = lines[..line].concat();
            let l = lines[line].trim_end_matches('\n');
            if seed & 1 == 0 {
                out.push_str(&format!("{l}{spaces}// note {seed}\n"));
            } else {
                out.push_str(&format!("{spaces}// note {seed}\n{}", lines[line]));
            }
            out.extend(lines[line + 1..].iter().copied());
            out
        }
        // A separator before a line break or `}`.
        6 if matches!(t.kind, TokenKind::Newline | TokenKind::RBrace) => {
            format!("{};{}", &src[..s], &src[s..])
        }
        // Join two lines with `;`.
        7 if t.kind == TokenKind::Newline => format!("{}; {}", &src[..s], &src[e..]),
        // Duplicate or drop a line.
        8 => {
            let mut out: String = lines[..line].concat();
            if seed & 1 == 0 {
                out.push_str(lines[line]);
            }
            out.extend(lines[line..].iter().skip((seed & 1) as usize).copied());
            out
        }
        // A tab, or a line break between two tokens.
        _ => format!(
            "{}{}{}",
            &src[..s],
            if seed & 1 == 0 { "\t" } else { "\n" },
            &src[s..]
        ),
    }
}

/// Applies each edit that keeps the file valid.
fn edited(src: &str, ops: &[(u8, u64)]) -> String {
    let mut cur = src.to_string();
    for &(op, seed) in ops {
        let next = edit(&cur, op, seed);
        if valid(&next) {
            cur = next;
        }
    }
    cur
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn random_valid_edits_format_stably(
        which in 0usize..64,
        ops in prop::collection::vec((any::<u8>(), any::<u64>()), 1..12),
    ) {
        let all = fixtures();
        let (name, src) = &all[which % all.len()];
        let input = edited(src, &ops);
        prop_assume!(valid(&input));
        assert_formats(name, &input);
    }
}

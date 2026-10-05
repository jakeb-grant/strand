//! The parser and the checker never panic, always terminate quickly, and
//! lexing stays lossless on arbitrary damaged input: 10,000 random edits of
//! the design fixtures, plus pathological nesting.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::{Duration, Instant};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::syntax::{dump, lexer, parse};
use strand_compiler::{FileId, SourceMap};

/// splitmix64: small, deterministic, good enough to drive edits.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Fragments worth inserting: every token class, keywords in odd places,
/// unbalanced brackets and quotes, multi-byte characters.
const FRAGMENTS: &[&str] = &[
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    ",",
    ";",
    ":",
    ".",
    "\n",
    "\r\n",
    "\r",
    " ",
    "\t",
    "=>",
    "->",
    "<->",
    "?.",
    "??",
    "?",
    "~",
    "=",
    "==",
    "!=",
    "<",
    ">=",
    "&&",
    "||",
    "!",
    "-",
    "+=",
    "*",
    "/",
    "%",
    "&",
    "|",
    "'",
    "$",
    "$x",
    "$a.2",
    "#",
    "#fff",
    "#needle",
    "@",
    "@reset",
    "\"",
    "\"str\"",
    "\"\\u{",
    "//",
    "when ",
    "if ",
    "else ",
    "match ",
    "for ",
    " in ",
    " key ",
    "on ",
    "change ",
    "after ",
    "every ",
    "while ",
    "state ",
    "let ",
    "export ",
    "persist",
    "component ",
    "bar ",
    "tokens ",
    "extends ",
    "override ",
    "use ",
    "palette ",
    "service ",
    "from ",
    "dbus ",
    "permit ",
    "fn ",
    "keyframes ",
    "enter ",
    "exit ",
    "slot",
    "set ",
    "play ",
    "await ",
    "enum ",
    "type ",
    "true",
    "null",
    "_",
    "x",
    "Foo",
    "text ",
    "1",
    "0.5",
    "1.5s",
    "12px",
    "40%",
    "270deg",
    "4ch",
    "12pz",
    "1.",
    "…",
    "‹",
    "¤",
    "é",
    "\u{0}",
];

fn fixtures() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect()
}

fn boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn mutate(rng: &mut Rng, src: &mut String) {
    let edits = 1 + rng.below(4);
    for _ in 0..edits {
        let at = boundary(src, rng.below(src.len() + 1));
        match rng.below(10) {
            // Delete a short range.
            0..=2 => {
                let end = boundary(src, at + 1 + rng.below(24));
                src.replace_range(at..end, "");
            }
            // Insert a fragment.
            3..=6 => src.insert_str(at, FRAGMENTS[rng.below(FRAGMENTS.len())]),
            // Truncate.
            7 => src.truncate(at),
            // Duplicate a range somewhere else.
            8 => {
                let end = boundary(src, at + rng.below(80));
                let chunk = src[at..end].to_string();
                let to = boundary(src, rng.below(src.len() + 1));
                src.insert_str(to, &chunk);
            }
            // Delete a whole line.
            _ => {
                let start = src[..at].rfind('\n').map_or(0, |i| i + 1);
                let end = src[at..].find('\n').map_or(src.len(), |i| at + i + 1);
                src.replace_range(start..end, "");
            }
        }
    }
}

/// Parses `src` with every invariant checked; returns how long parsing
/// took and the diagnostic codes.
fn check(src: &str, render_too: bool) -> (Duration, Vec<&'static str>) {
    let started = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| parse(FileId::default(), src)));
    let elapsed = started.elapsed();
    let parsed = match result {
        Ok(p) => p,
        Err(_) => panic!("parser panicked on:\n----\n{src}\n----"),
    };
    let (tokens, _) = lexer::lex(src);
    let joined: String = tokens.iter().map(|t| t.span.text(src)).collect();
    assert_eq!(joined, src, "lexer lost bytes");
    for d in &parsed.diagnostics {
        assert!(
            !d.labels.is_empty(),
            "diagnostic without a label: {d:?}\n----\n{src}"
        );
        for l in &d.labels {
            let r = l.span.range();
            assert!(
                r.end <= src.len() && src.is_char_boundary(r.start) && src.is_char_boundary(r.end),
                "bad label span {r:?} in {d:?}"
            );
        }
    }
    // Every node lies inside its parent, even in a damaged tree: the LSP's
    // node-at-offset lookup and reload identity rely on it.
    let len = src.len() as u32;
    dump::tree(&parsed.file).walk(&mut |node, parent| {
        assert!(
            node.span.start <= node.span.end && node.span.end <= len,
            "bad span {:?} on `{}`\n----\n{src}\n----",
            node.span,
            node.label
        );
        if let Some(parent) = parent {
            assert!(
                parent.span.contains(node.span),
                "`{}` {:?} escapes `{}` {:?}\n----\n{src}\n----",
                node.label,
                node.span,
                parent.label,
                parent.span
            );
        }
    });
    // The checker runs on every damaged tree too (the reload pipeline and
    // the LSP check what does not parse cleanly).
    let map = SourceMap::single("fuzz.strand", String::from(src)).0;
    let compiled = match catch_unwind(AssertUnwindSafe(|| strand_compiler::compile(&map))) {
        Ok(c) => c,
        Err(_) => panic!("checker panicked on:\n----\n{src}\n----"),
    };
    for d in &compiled.diagnostics {
        assert!(!d.labels.is_empty(), "diagnostic without a label: {d:?}");
        for l in &d.labels {
            let r = l.span.range();
            assert!(
                r.end <= src.len() && src.is_char_boundary(r.start) && src.is_char_boundary(r.end),
                "bad label span {r:?} in {d:?}\n----\n{src}\n----"
            );
        }
    }
    if render_too {
        let out = catch_unwind(AssertUnwindSafe(|| {
            render(
                &parsed.diagnostics,
                &SourceMap::single("fuzz.strand", String::from(src)).0,
                Style::Plain,
            )
        }));
        assert!(out.is_ok(), "rendering panicked on:\n----\n{src}\n----");
    }
    (elapsed, parsed.diagnostics.iter().map(|d| d.code).collect())
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

/// 10,000 edits by default; `STRAND_FUZZ_ITERS` and `STRAND_FUZZ_SEED`
/// allow longer nightly runs.
#[test]
fn ten_thousand_random_edits() {
    let bases = fixtures();
    let iters = env_u64("STRAND_FUZZ_ITERS").unwrap_or(10_000);
    let mut rng = Rng(env_u64("STRAND_FUZZ_SEED").unwrap_or(0x5eed_5742_a9d0));
    let mut worst = Duration::ZERO;
    let started = Instant::now();
    for i in 0..iters {
        let mut src = bases[rng.below(bases.len())].clone();
        mutate(&mut rng, &mut src);
        // Sometimes keep damaging the same text.
        if rng.below(4) == 0 {
            mutate(&mut rng, &mut src);
        }
        worst = worst.max(check(&src, i % 50 == 0).0);
    }
    let total = started.elapsed();
    eprintln!("{iters} edits: total {total:?}, worst parse {worst:?}");
    // Generous bounds for unoptimised builds on a shared machine; a stuck
    // or quadratic parser blows through them by orders of magnitude.
    assert!(
        worst < Duration::from_secs(1),
        "slowest parse took {worst:?}"
    );
    assert!(
        total < Duration::from_secs(120),
        "10k parses took {total:?}"
    );
}

#[test]
fn random_bytes() {
    let mut rng = Rng(42);
    for _ in 0..2_000 {
        let len = rng.below(200);
        let src: String = (0..len)
            .map(|_| FRAGMENTS[rng.below(FRAGMENTS.len())])
            .collect();
        check(&src, false);
    }
}

#[test]
fn pathological_nesting_reports_instead_of_overflowing() {
    let n = 100_000;
    // Unclosed brackets that cannot start an item are skipped as junk,
    // not nested into, so they report something other than `too_deep`.
    for src in [
        "(".repeat(n),
        "[".repeat(n),
        format!("component X {}", "{".repeat(n)),
    ] {
        let (elapsed, codes) = check(&src, true);
        assert!(!codes.is_empty(), "nothing reported for {}", &src[..20]);
        assert!(elapsed < Duration::from_secs(5));
    }
    let cases = [
        format!("component X {{ {} }}", "box {".repeat(n)),
        format!("let x = {}1", "!".repeat(n)),
        format!("let x = {}1", "-".repeat(n)),
        format!("let x = {}1", "a ?? ".repeat(n)),
        format!("let x = {}1", "a ? b : ".repeat(n)),
        format!("let x = {}1", "x => ".repeat(n)),
        format!("let x = {}1", "match a { _ => ".repeat(n)),
        format!("let x = a{}", ".b".repeat(n)),
        format!("let x = f{}", "(x)".repeat(n)),
        format!(
            "component X {{ on click {{ {} }} }}",
            "if a {} else ".repeat(n)
        ),
        format!("state x: {}int = 1", "[".repeat(n)),
        format!("let x = {}", "(".repeat(n) + &")".repeat(n)),
        format!(
            "component X {{ on click {{ {} }} }}",
            "for a in b { ".repeat(n)
        ),
        format!("component X {{ {} }}", "if a { ".repeat(n)),
        format!("let x = {}", "[".repeat(n)),
        format!("let x = {}1", "a + ".repeat(n)),
    ];
    for src in &cases {
        // Run on a thread with the default 2 MiB stack used by test threads.
        let src2 = src.clone();
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || check(&src2, true))
            .unwrap();
        let (elapsed, codes) = handle
            .join()
            .unwrap_or_else(|_| panic!("panicked on {}", &src[..60]));
        // Deep input is reported, never silently cut off, and once.
        assert!(
            codes.contains(&"syntax::too_deep"),
            "{codes:?} on {}",
            &src[..60]
        );
        assert!(codes.len() <= 2, "{codes:?} on {}", &src[..60]);
        assert!(
            elapsed < Duration::from_secs(5),
            "{elapsed:?} on {}",
            &src[..60]
        );
    }
}

/// miette panics drawing a column past 65,535; such lines (minified or
/// generated configs) are reported one line each instead.
#[test]
fn errors_far_along_a_long_line_render() {
    for src in [
        format!("bar T {{ x: {}}} }}", " ".repeat(70_000)),
        format!("let x = {}1 +", "1 + ".repeat(20_000)),
        format!(
            "component A {{ text \"{}\" {{ width: 12pz }} }}",
            "é".repeat(70_000)
        ),
    ] {
        let (_, codes) = check(&src, true);
        assert!(!codes.is_empty());
        let parsed = parse(FileId::default(), &src);
        let out = render(
            &parsed.diagnostics,
            &SourceMap::single("long.strand", src.clone()).0,
            Style::Plain,
        );
        assert!(
            out.contains("long.strand:1:"),
            "{}",
            &out[..out.len().min(300)]
        );
        assert!(out.len() < 10_000, "the long line was drawn");
    }
}

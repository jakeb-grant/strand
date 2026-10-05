//! Long chains of declarations check on a small stack.
//!
//! `let`s, `state`s, `fn`s and token entries are checked on first use, so
//! a chain of them nests one checking frame per link. The LSP and the
//! reload compile run on worker threads, whose stacks are small; a chain
//! of thousands of links must give its diagnostics, not abort the process.

use strand_compiler::{Compiled, SourceMap, compile};

const LINKS: usize = 5_000;
const STACK: usize = 2 * 1024 * 1024;

fn compile_on_small_stack(src: String) -> Compiled {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            let (map, _) = SourceMap::single("deep.strand", src);
            compile(&map)
        })
        .unwrap()
        .join()
        .unwrap()
}

fn codes(out: &Compiled) -> Vec<&'static str> {
    out.diagnostics.iter().map(|d| d.code).collect()
}

#[test]
fn let_chain() {
    let mut src = String::new();
    for i in 0..LINKS {
        src.push_str(&format!("let a{i} = a{} + 1\n", i + 1));
    }
    src.push_str(&format!("let a{LINKS} = 1\n"));
    let out = compile_on_small_stack(src);
    assert_eq!(codes(&out), Vec::<&str>::new());
}

#[test]
fn let_cycle() {
    let mut src = String::new();
    for i in 0..LINKS {
        src.push_str(&format!("let a{i} = a{} + 1\n", (i + 1) % LINKS));
    }
    let out = compile_on_small_stack(src);
    assert_eq!(codes(&out), ["check::cycle"]);
}

#[test]
fn state_chain() {
    let mut src = String::new();
    for i in 0..LINKS {
        src.push_str(&format!("state s{i} = s{} + 1\n", i + 1));
    }
    src.push_str(&format!("state s{LINKS} = 1\n"));
    let out = compile_on_small_stack(src);
    assert_eq!(codes(&out), Vec::<&str>::new());
}

#[test]
fn fn_chain() {
    let mut src = String::new();
    for i in 0..LINKS {
        src.push_str(&format!("fn f{i}() {{ f{}() }}\n", i + 1));
    }
    src.push_str(&format!("fn f{LINKS}() {{ 1 }}\n"));
    let out = compile_on_small_stack(src);
    assert_eq!(codes(&out), Vec::<&str>::new());
}

#[test]
fn token_chain() {
    let mut src = String::from("tokens deep {\n");
    for i in 0..LINKS {
        src.push_str(&format!("  t{i}: $t{}\n", i + 1));
    }
    src.push_str(&format!("  t{LINKS}: $accent\n}}\n"));
    let out = compile_on_small_stack(src);
    assert_eq!(codes(&out), Vec::<&str>::new());
}

#[test]
fn token_cycle() {
    let mut src = String::from("tokens deep {\n");
    for i in 0..LINKS {
        src.push_str(&format!("  t{i}: $t{}\n", (i + 1) % LINKS));
    }
    src.push_str("}\n");
    let out = compile_on_small_stack(src);
    assert_eq!(codes(&out), ["check::cycle"]);
}

//! Prints the syntax tree and diagnostics of `.strand` files.
//!
//! `cargo run -p strand-compiler --example parse -- file.strand…`

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::syntax::{dump, parse};

fn main() {
    for path in std::env::args().skip(1) {
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{path}: {e}");
                continue;
            }
        };
        let parsed = parse(&src);
        print!("{}", dump::tree(&parsed.file).render());
        eprint!("{}", render(&parsed.diagnostics, &path, &src, Style::Plain));
    }
}

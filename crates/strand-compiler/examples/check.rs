//! Checks `.strand` files as one config and prints the diagnostics.
//!
//! `cargo run -p strand-compiler --example check -- a.strand b.strand…`

use strand_compiler::SourceMap;
use strand_compiler::diagnostic::{Style, render};

fn main() {
    let mut map = SourceMap::new();
    for path in std::env::args().skip(1) {
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                map.add(path, s);
            }
            Err(e) => eprintln!("{path}: {e}"),
        }
    }
    let out = strand_compiler::compile(&map);
    eprint!("{}", render(&out.diagnostics, &map, Style::Plain));
    eprintln!("{} errors, {} warnings", out.errors(), out.warnings());
}

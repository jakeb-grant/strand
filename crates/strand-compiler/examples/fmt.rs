//! Formats a `.strand` file to stdout: `cargo run --example fmt -- file`.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: fmt <file.strand>");
        std::process::exit(2);
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    match strand_compiler::fmt::format(&src) {
        Ok(out) => print!("{out}"),
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    }
}

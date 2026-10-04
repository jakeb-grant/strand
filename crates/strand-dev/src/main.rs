//! Developer tooling kept out of the runtime binary: the language server and
//! the inspector. Shares the parser and type checker with the runtime through
//! `strand-compiler`.
//!
//! See `docs/design.md`, "Developer experience". The basic LSP lands in M1,
//! the inspector in M5.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "-h" | "--help") => {
            println!("usage: strand-dev <lsp | inspect>");
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("strand-dev {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(cmd @ ("lsp" | "inspect")) => {
            eprintln!("strand-dev {cmd}: not implemented yet");
            ExitCode::FAILURE
        }
        Some(cmd) => {
            eprintln!("strand-dev: unknown command `{cmd}`");
            ExitCode::FAILURE
        }
    }
}

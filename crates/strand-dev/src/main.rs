//! `strand-dev`: the language server (`strand-dev lsp`, over stdio) and,
//! later, the inspector.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "-h" | "--help") => {
            println!(
                "usage: strand-dev <lsp | inspect>\n\n  lsp      the language server, over stdio\n  inspect  the inspector (M5)"
            );
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("strand-dev {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("lsp") => match strand_dev::run_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("strand-dev lsp: {e}");
                ExitCode::FAILURE
            }
        },
        Some("inspect") => {
            eprintln!("strand-dev inspect: not implemented yet (M5)");
            ExitCode::FAILURE
        }
        Some(cmd) => {
            eprintln!("strand-dev: unknown command `{cmd}`");
            ExitCode::FAILURE
        }
    }
}

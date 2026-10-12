//! `strand-dev`: the language server (`strand-dev lsp`, over stdio), the
//! reference generator (`strand-dev reference [dir]`) and, later, the
//! inspector.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "-h" | "--help") => {
            println!(
                "usage: strand-dev <lsp | reference [dir] | inspect>\n\n  lsp        the language server, over stdio\n  reference  writes the schema reference into dir (default docs/reference)\n  inspect    the inspector (M5)"
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
        Some("reference") => reference(std::env::args().nth(2)),
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

/// `strand-dev reference [dir]`: the pages of `docs/reference/` written
/// into `dir` (default `docs/reference`, from the repository's root).
fn reference(dir: Option<String>) -> ExitCode {
    use strand_dev::reference;
    let dir = std::path::PathBuf::from(dir.unwrap_or_else(|| "docs/reference".into()));
    let schema = match reference::schema() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("strand-dev reference: {e}");
            return ExitCode::FAILURE;
        }
    };
    match reference::write(&dir, &reference::pages(&schema)) {
        Ok(changed) if changed.is_empty() => {
            println!("{}: up to date", dir.display());
            ExitCode::SUCCESS
        }
        Ok(changed) => {
            println!("{}: wrote {}", dir.display(), changed.join(", "));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("strand-dev reference: {}: {e}", dir.display());
            ExitCode::FAILURE
        }
    }
}

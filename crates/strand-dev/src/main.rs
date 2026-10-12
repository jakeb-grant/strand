//! `strand-dev`: the language server (`strand-dev lsp`, over stdio), the
//! reference generator (`strand-dev reference [dir]`) and the inspector's
//! terminal client (`strand-dev inspect [socket]`).

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "-h" | "--help") => {
            println!(
                "usage: strand-dev <lsp | reference [dir] | inspect>\n\n  lsp        the language server, over stdio\n  reference  writes the schema reference into dir (default docs/reference)\n  inspect    the inspector, over the shell's socket ($STRAND_SOCKET or the default)"
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
            match strand_dev::inspect::run(std::env::args_os().nth(2).map(Into::into)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("strand-dev inspect: {e}");
                    ExitCode::FAILURE
                }
            }
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
    let dir = match dir {
        Some(d) => std::path::PathBuf::from(d),
        // The default is relative, so it means the committed pages only
        // from the repository's root; anywhere else it would make a stray
        // `docs/reference`.
        None if std::path::Path::new("docs/reference").is_dir() => "docs/reference".into(),
        None => {
            eprintln!(
                "strand-dev reference: no docs/reference here; run it from the repository's \
                 root or name the directory"
            );
            return ExitCode::FAILURE;
        }
    };
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

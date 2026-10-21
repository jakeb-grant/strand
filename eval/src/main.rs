//! `strand-eval`: see the library's docs and docs/eval.md.

use std::process::ExitCode;

fn main() -> ExitCode {
    // (replay-owner) An argument that is not UTF-8 (a path `strand
    // replay` passed on as it is) is refused plainly, not a panic.
    let mut args: Vec<String> = Vec::new();
    for a in std::env::args_os().skip(1) {
        match a.into_string() {
            Ok(a) => args.push(a),
            Err(a) => {
                eprintln!(
                    "strand-eval: the argument {} is not UTF-8; rename it or pass another path",
                    a.to_string_lossy()
                );
                return ExitCode::from(2);
            }
        }
    }
    match strand_eval::main_with(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(strand_eval::Error::Failed(msg)) => {
            eprintln!("strand-eval: {msg}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("strand-eval: {e}");
            ExitCode::from(2)
        }
    }
}

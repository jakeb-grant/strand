//! `strand-eval`: see the library's docs and docs/eval.md.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match strand_eval::main_with(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("strand-eval: {e}");
            ExitCode::from(2)
        }
    }
}

//! The animation evaluation sandbox (docs/eval.md; docs/decisions.md
//! eval-sandbox): scenario scripts run on a virtual clock against
//! strand-render (with its `eval-hooks` feature) and the logic VM in
//! one process, every painted frame traced node by node and checked.
//!
//! - [`scenario`], [`steps`], [`select`]: the `.scn` format, parsed with
//!   typed errors at `file:line`.

use std::fmt;

pub mod scenario;
pub mod select;
pub mod steps;

/// What the sandbox can fail with; every command returns one of these
/// rather than panicking.
#[derive(Debug)]
pub enum Error {
    /// The command line asked for something this build cannot do yet.
    Unsupported(String),
    /// The command line itself is wrong.
    Usage(String),
    /// A scenario is wrong: its file, the line and why.
    Scenario {
        file: String,
        line: u32,
        msg: String,
    },
    /// The config did not load or compile.
    Config(String),
    /// The run went wrong (a scene error, a step the shell refused).
    Run(String),
    /// A file could not be read or written.
    Io(String, std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "{what}: not implemented in this build"),
            Error::Usage(msg) => write!(f, "{msg}"),
            Error::Scenario { file, line, msg } => write!(f, "{file}:{line}: {msg}"),
            Error::Config(msg) => write!(f, "config: {msg}"),
            Error::Run(msg) => write!(f, "run: {msg}"),
            Error::Io(what, e) => write!(f, "{what}: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The subcommands `strand-eval` knows, in the order docs/eval.md
/// describes them.
pub const COMMANDS: [&str; 10] = [
    "run", "check", "matrix", "promote", "verdict", "explain", "ab", "bench", "new", "selftest",
];

/// Runs `strand-eval` with `args` (without the program name).
pub fn main_with(args: &[String]) -> Result<(), Error> {
    let Some(cmd) = args.first() else {
        return Err(Error::Usage(usage()));
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        // `selftest` is scripts/eval/ci.sh, run by scripts/eval/eval.sh.
        c if COMMANDS.contains(&c) => Err(Error::Unsupported(format!("strand-eval {c}"))),
        other => Err(Error::Usage(format!(
            "unknown command `{other}`\n{}",
            usage()
        ))),
    }
}

/// The usage text.
pub fn usage() -> String {
    format!(
        "usage: strand-eval <command> [args...]\ncommands: {}",
        COMMANDS.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_command_is_a_usage_error() {
        let e = main_with(&["frobnicate".into()]).unwrap_err();
        assert!(matches!(e, Error::Usage(m) if m.contains("frobnicate")));
    }

    #[test]
    fn no_command_is_a_usage_error() {
        assert!(matches!(main_with(&[]), Err(Error::Usage(_))));
    }

    #[test]
    fn a_known_command_not_built_yet_is_a_typed_error() {
        let e = main_with(&["matrix".into()]).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e}");
    }
}

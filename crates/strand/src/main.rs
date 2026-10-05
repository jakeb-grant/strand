//! The Strand shell runtime and CLI.
//!
//! See `docs/design.md` for the design and roadmap.

mod check;

use std::io::IsTerminal;
use std::process::ExitCode;

use strand_compiler::diagnostic::Style;

/// Subcommands the design commits to, with the milestone that delivers each.
const COMMANDS: &[(&str, &str, &str)] = &[
    ("run", "start the shell from the config directory", "M0"),
    ("check", "check the config without running it [dir]", "M1"),
    ("watch", "stream reload events (--json)", "M1"),
    (
        "reload",
        "rescan now (--hard drops non-persisted state)",
        "M1",
    ),
    ("get", "read an exported value", "M5"),
    ("set", "write an exported value, token or setting", "M5"),
    ("toggle", "flip an exported boolean", "M5"),
    ("call", "invoke a service action", "M5"),
    ("new", "scaffold a working shell", "M5"),
    (
        "export",
        "write the palette for gtk, kitty or hyprland",
        "M5",
    ),
    (
        "compositor-rules",
        "print blur rules for the compositor",
        "M4",
    ),
    (
        "report",
        "dump compositor, protocols, scales and frame stats",
        "M5",
    ),
];

fn usage() -> String {
    let mut s = String::from("usage: strand <command> [args]\n\ncommands:\n");
    for (name, about, _) in COMMANDS {
        s.push_str(&format!("  {name:<18}{about}\n"));
    }
    s
}

fn dispatch(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => Ok(usage()),
        Some("-V" | "--version") => Ok(format!("strand {}\n", env!("CARGO_PKG_VERSION"))),
        Some(cmd) => match COMMANDS.iter().find(|(name, ..)| *name == cmd) {
            Some((name, _, milestone)) => {
                Err(format!("strand {name}: not implemented yet ({milestone})"))
            }
            None => Err(format!("strand: unknown command `{cmd}`\n\n{}", usage())),
        },
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("check") {
        let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        let style = if color { Style::Color } else { Style::Plain };
        let (text, ok) = check::run(&args[1..], style);
        eprint!("{text}");
        return if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    match dispatch(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<String, String> {
        dispatch(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn help_lists_every_command() {
        let out = run(&[]).unwrap();
        for (name, ..) in COMMANDS {
            assert!(out.contains(name), "help is missing `{name}`");
        }
    }

    #[test]
    fn known_commands_name_their_milestone() {
        assert_eq!(
            run(&["watch"]).unwrap_err(),
            "strand watch: not implemented yet (M1)"
        );
    }

    #[test]
    fn unknown_commands_fail() {
        assert!(
            run(&["frobnicate"])
                .unwrap_err()
                .contains("unknown command `frobnicate`")
        );
    }
}

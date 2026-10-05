//! The Strand shell runtime and CLI.
//!
//! See `docs/design.md` for the design and roadmap.

mod check;
mod demo;
mod logging;
mod run;

use std::io::IsTerminal;
use std::process::ExitCode;

use strand_compiler::diagnostic::Style;

/// mimalloc for the whole runtime (`docs/design.md`, "Stack").
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Subcommands the design commits to, with the milestone that delivers each.
const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "run",
        "run the config [dir] (default $XDG_CONFIG_HOME/strand; --demo: the M0 hello bar)",
        "M1",
    ),
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

/// What a command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Print(String),
    /// `strand run --demo`.
    Demo,
    /// `strand run [dir]`.
    Run(Option<std::path::PathBuf>),
}

fn dispatch(args: &[String]) -> Result<Action, String> {
    match args.first().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => Ok(Action::Print(usage())),
        Some("-V" | "--version") => Ok(Action::Print(format!(
            "strand {}\n",
            env!("CARGO_PKG_VERSION")
        ))),
        Some("run") if args[1..] == ["--demo"] => Ok(Action::Demo),
        Some("run") if args[1..].iter().any(|a| a == "--demo") => {
            Err("strand run --demo: takes no other arguments".into())
        }
        Some("run") => match &args[1..] {
            [] => Ok(Action::Run(None)),
            [dir] if !dir.starts_with('-') => Ok(Action::Run(Some(dir.into()))),
            _ => Err("usage: strand run [dir | --demo]".into()),
        },
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
        Ok(Action::Print(out)) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Ok(Action::Demo) => {
            let log = logging::LogConfig::from_env();
            log.install();
            match demo::run(&log) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("strand run: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Action::Run(dir)) => {
            let log = logging::LogConfig::from_env();
            log.install();
            let dir = dir.or_else(|| {
                check::default_dir(
                    std::env::var_os("XDG_CONFIG_HOME"),
                    std::env::var_os("HOME"),
                )
            });
            let Some(dir) = dir else {
                eprintln!("strand run: no config directory (set XDG_CONFIG_HOME or HOME)");
                return ExitCode::FAILURE;
            };
            match run::run(&dir, &log) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("strand run: {err}");
                    ExitCode::FAILURE
                }
            }
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

    fn run(args: &[&str]) -> Result<Action, String> {
        dispatch(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn help_lists_every_command() {
        let Action::Print(out) = run(&[]).unwrap() else {
            panic!("help prints");
        };
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
    fn run_demo_is_the_m0_bar() {
        assert_eq!(run(&["run", "--demo"]).unwrap(), Action::Demo);
        assert_eq!(run(&["run"]).unwrap(), Action::Run(None));
        assert_eq!(
            run(&["run", "conf"]).unwrap(),
            Action::Run(Some("conf".into()))
        );
        assert!(run(&["run", "--demo", "x"]).is_err());
        assert!(run(&["run", "a", "b"]).is_err());
    }

    #[test]
    fn mimalloc_is_the_global_allocator() {
        let b = Box::new([0u8; 64]);
        let p = Box::into_raw(b);
        // SAFETY: `p` is a live allocation; the query only reads metadata.
        let ours = unsafe { libmimalloc_sys::mi_is_in_heap_region(p.cast()) };
        // SAFETY: `p` came from `Box::into_raw` above.
        drop(unsafe { Box::from_raw(p) });
        assert!(ours, "the Box was not allocated by mimalloc");
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

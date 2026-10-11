//! The `strand` command line: the COMMANDS table, `dispatch` and `main`'s
//! body, in the library so a plugin binary answers every command as
//! `strand` does (docs/architecture.md, "Plugin builder"; decisions.md
//! m5-plugins). `main.rs` keeps `process_setup!` and `main`, which calls
//! [`main`] through `strand::cli`.

use std::io::IsTerminal;
use std::process::ExitCode;

use crate::cli;
use strand_compiler::diagnostic::Style;

/// Subcommands the design commits to, with the milestone that delivers each.
const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "run",
        "run the config [dir] (default $XDG_CONFIG_HOME/strand; --demo: the M0 hello bar)",
        "M1",
    ),
    ("check", "check the config without running it [dir]", "M1"),
    (
        "fmt",
        "format .strand files in place [--check] [paths]",
        "M1",
    ),
    ("watch", "stream reload events (--json)", "M1"),
    (
        "reload",
        "rescan now (--hard drops non-persisted state)",
        "M1",
    ),
    (
        "get",
        "read an exported value, a settings field or a token ('$accent') (--json)",
        "M5",
    ),
    (
        "set",
        "write an exported value (strand set theme.look mocha; '$accent' overlays a token)",
        "M2",
    ),
    ("toggle", "flip an exported boolean", "M5"),
    (
        "call",
        "invoke a service action (strand call notifications.clear) (--json)",
        "M5",
    ),
    (
        "clear",
        "drop a runtime overlay (strand clear '$accent')",
        "M5",
    ),
    ("new", "scaffold a working shell", "M5"),
    (
        "export",
        "write the palette: json | base24 | --template <file> (M5 wave 2)",
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

/// A subcommand that reports through stderr text and a pass/fail flag.
type Tool = fn(&[String], Style) -> (String, bool);

/// `main`'s body: the idle hooks, then the command line.
pub fn main() -> ExitCode {
    // A text worker that goes quiet after shaping keeps the allocator's
    // freed pages, which only its own thread can return: it trims as its
    // queue drains (decisions.md, wave4-exitMemory); so does the image
    // decoder.
    strand_text::set_idle_hook(cli::trim);
    strand_render::image::set_idle_hook(cli::trim);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tool: Option<Tool> = match args.first().map(String::as_str) {
        Some("check") => Some(cli::check),
        Some("fmt") => Some(cli::fmt),
        _ => None,
    };
    if let Some(tool) = tool {
        let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        let style = if color { Style::Color } else { Style::Plain };
        let (text, ok) = tool(&args[1..], style);
        eprint!("{text}");
        return if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    // The socket's one-shot commands: text on stdout, why on stderr.
    type OneShot = fn(&[String]) -> Result<String, String>;
    let ipc: Option<OneShot> = match args.first().map(String::as_str) {
        Some("get") => Some(cli::get_cli),
        Some("toggle") => Some(cli::toggle_cli),
        Some("call") => Some(cli::call_cli),
        Some("clear") => Some(cli::clear_cli),
        _ => None,
    };
    if let Some(command) = ipc {
        return match command(&args[1..]) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("strand {}: {e}", args[0]);
                ExitCode::FAILURE
            }
        };
    }
    match args.first().map(String::as_str) {
        Some("reload") => {
            return match cli::reload_cli(&args[1..]) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("strand reload: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("set") => {
            return match cli::set_cli(&args[1..]) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("strand set: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("compositor-rules") => {
            // Printed for the user to paste; never applied to the session.
            return match cli::compositor_rules(&args[1..]) {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("strand compositor-rules: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("watch") => {
            return match cli::watch_cli(&args[1..], &mut std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("strand watch: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {}
    }
    match dispatch(&args) {
        Ok(Action::Print(out)) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Ok(Action::Demo) => {
            let log = cli::LogConfig::from_env();
            log.install();
            match cli::demo(&log) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("strand run: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Action::Run(dir)) => {
            let log = cli::LogConfig::from_env();
            log.install();
            let dir = dir.or_else(|| {
                cli::default_dir(
                    std::env::var_os("XDG_CONFIG_HOME"),
                    std::env::var_os("HOME"),
                )
            });
            let Some(dir) = dir else {
                eprintln!("strand run: no config directory (set XDG_CONFIG_HOME or HOME)");
                return ExitCode::FAILURE;
            };
            match cli::run(&dir, &log) {
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
            run(&["new"]).unwrap_err(),
            "strand new: not implemented yet (M5)"
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
    fn unknown_commands_fail() {
        assert!(
            run(&["frobnicate"])
                .unwrap_err()
                .contains("unknown command `frobnicate`")
        );
    }
}

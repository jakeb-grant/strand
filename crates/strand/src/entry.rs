//! The `strand` command line: the COMMANDS table, `dispatch` and `main`'s
//! body, in the library so a plugin binary answers every command as
//! `strand` does (docs/architecture.md, "Plugin builder"; decisions.md
//! m5-plugins). `main.rs` keeps `process_setup!` and `main`, which calls
//! [`main`] through `strand::Shell::builder().run()` (`plugin.rs`).

use std::io::IsTerminal;
use std::process::ExitCode;

use crate::cli;
use strand_compiler::diagnostic::Style;

/// Subcommands the design commits to; each is served (`tool`, `one_shot`
/// or `run`).
const COMMANDS: &[(&str, &str)] = &[
    (
        "run",
        "run the config [dir] (default $XDG_CONFIG_HOME/strand; --demo: the M0 hello bar)",
    ),
    ("check", "check the config without running it [dir]"),
    ("fmt", "format .strand files in place [--check] [paths]"),
    ("watch", "stream reload events (--json)"),
    ("reload", "rescan now (--hard drops non-persisted state)"),
    (
        "get",
        "read an exported value, a settings field or a token ('$accent') (--json)",
    ),
    (
        "set",
        "write an exported value (strand set theme.look mocha; '$accent' overlays a token)",
    ),
    ("toggle", "flip an exported boolean"),
    (
        "call",
        "invoke a service action (strand call notifications.clear) (--json)",
    ),
    ("clear", "drop a runtime overlay (strand clear '$accent')"),
    (
        "new",
        "write a starter shell into [dir] (default $XDG_CONFIG_HOME/strand; refuses a non-empty dir)",
    ),
    (
        "export",
        "write the palette: json | base24 | --template <file> [--palette <expr>] [-o <file>]",
    ),
    ("compositor-rules", "print blur rules for the compositor"),
    (
        "report",
        "dump compositor, protocols, scales, memory, GPU and frame stats (--json)",
    ),
];

fn usage() -> String {
    let mut s = String::from("usage: strand <command> [args]\n\ncommands:\n");
    for (name, about) in COMMANDS {
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
        Some(cmd) => Err(format!("strand: unknown command `{cmd}`\n\n{}", usage())),
    }
}

/// A subcommand that reports through stderr text and a pass/fail flag.
type Tool = fn(&[String], Style) -> (String, bool);

fn tool(cmd: &str) -> Option<Tool> {
    match cmd {
        "check" => Some(cli::check),
        "fmt" => Some(cli::fmt),
        _ => None,
    }
}

/// A subcommand that prints its text on stdout, or why it failed on
/// stderr: the socket's commands, `new` and `compositor-rules` (printed
/// for the user to paste; never applied to the session).
type OneShot = fn(&[String]) -> Result<String, String>;

fn one_shot(cmd: &str) -> Option<OneShot> {
    Some(match cmd {
        "get" => cli::get_cli,
        "toggle" => cli::toggle_cli,
        "call" => cli::call_cli,
        "clear" => cli::clear_cli,
        "export" => cli::export_cli,
        "reload" => cli::reload_cli,
        "set" => cli::set_cli,
        "new" => cli::new_cli,
        "report" => cli::report_cli,
        "compositor-rules" => cli::compositor_rules,
        "watch" => |a| cli::watch_cli(a, &mut std::io::stdout()).map(|()| String::new()),
        _ => return None,
    })
}

/// `main`'s body: the idle hooks, then the command line.
pub fn main() -> ExitCode {
    // A text worker that goes quiet after shaping keeps the allocator's
    // freed pages, which only its own thread can return: it trims as its
    // queue drains (decisions.md, wave4-exitMemory); so does the image
    // decoder.
    strand_text::set_idle_hook(cli::trim);
    strand_render::image::set_idle_hook(cli::trim);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--write-schemas") {
        return crate::plugin::write_schemas_main(&args[1..]);
    }
    if let Some(tool) = args.first().and_then(|a| tool(a)) {
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
    if let Some(command) = args.first().and_then(|a| one_shot(a)) {
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

    /// Every command the help lists is served: `run`, a tool or a
    /// one-shot (none answers "not implemented yet" any more).
    #[test]
    fn known_commands_are_all_served() {
        for (name, ..) in COMMANDS {
            assert!(
                *name == "run" || tool(name).is_some() || one_shot(name).is_some(),
                "`strand {name}` is listed but not served"
            );
        }
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

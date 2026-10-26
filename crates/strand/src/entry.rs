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
    ("check", "check the config without running it [dir | file]"),
    ("fmt", "format .strand files in place [--check] [paths]"),
    ("watch", "stream reload events, faults and notices (--json)"),
    ("reload", "rescan now (--hard drops non-persisted state)"),
    (
        "get",
        "read an exported value, a settings field or a token ('$accent') (--json)",
    ),
    (
        "set",
        "write an exported value, a settings field or an rw service field (strand set theme.look mocha; audio.sink.volume +5%; '$accent' overlays a token)",
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
        "dump compositor, protocols, scales, memory, GPU and frame stats (--json); with words (strand report the clock jumped), save a glitch report",
    ),
    (
        "replay",
        "replay a glitch report [report-dir] (default: the newest) in the evaluation sandbox and open its viewer (--no-open); needs strand-eval next to strand",
    ),
];

/// Flags that stand in for a command (not COMMANDS rows: `dispatch` and
/// `main` take them before any command).
const OPTIONS: &[(&str, &str)] = &[
    (
        "--write-schemas",
        "write this plugin binary's service schemas for strand check and the LSP",
    ),
    ("-V, --version", "print the version"),
];

/// The variables that address one shell (spec section 13): never passed
/// to a program the shell starts.
const ENVIRONMENT: &[(&str, &str)] = &[
    (
        "STRAND_SOCKET",
        "the shell's socket (default $XDG_RUNTIME_DIR/strand-<display>.sock)",
    ),
    (
        "STRAND_STATE_DIR",
        "the shell's state directory (default $XDG_STATE_HOME/strand)",
    ),
];

fn usage() -> String {
    let mut s = String::from("usage: strand <command> [args]\n\ncommands:\n");
    for (name, about) in COMMANDS {
        s.push_str(&format!("  {name:<18}{about}\n"));
    }
    s.push_str("\noptions:\n");
    for (name, about) in OPTIONS {
        s.push_str(&format!("  {name:<18}{about}\n"));
    }
    s.push_str("\nenvironment (not passed to programs the shell starts):\n");
    for (name, about) in ENVIRONMENT {
        s.push_str(&format!("  {name:<18}{about}\n"));
    }
    s
}

/// `strand run`'s usage.
const RUN_USAGE: &str = "usage: strand run [dir | --demo]";

/// The usage of each command in COMMANDS, which `strand <command> --help`
/// (or `-h`) prints on stdout.
fn command_usage(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "run" => RUN_USAGE,
        "check" => crate::check::USAGE,
        "fmt" => crate::fmt::USAGE,
        "watch" => crate::ipc::WATCH_USAGE,
        "reload" => crate::ipc::RELOAD_USAGE,
        "get" => crate::ipc::GET_USAGE,
        "set" => crate::ipc::SET_USAGE,
        "toggle" => crate::ipc::TOGGLE_USAGE,
        "call" => crate::ipc::CALL_USAGE,
        "clear" => crate::ipc::CLEAR_USAGE,
        "new" => crate::new::USAGE,
        "export" => crate::export::USAGE,
        "compositor-rules" => crate::rules::USAGE,
        "report" => crate::report::USAGE,
        "replay" => crate::report::replay::USAGE,
        _ => return None,
    })
}

/// `strand <command> --help`: the command's usage, when the first
/// argument after a command asks for it (`-h` or `--help`). Later in the
/// line it is the command's own to read (a value, a report's words).
fn help_asked(args: &[std::ffi::OsString]) -> Option<String> {
    let [cmd, flag, ..] = args else {
        return None;
    };
    if flag != "-h" && flag != "--help" {
        return None;
    }
    let usage = command_usage(cmd.to_str()?)?;
    let mut out = usage.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
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
            _ => Err(RUN_USAGE.into()),
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
    // `strand <command> --help`: its usage on stdout, as asked for.
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(usage) = help_asked(&raw) {
        print!("{usage}");
        return ExitCode::SUCCESS;
    }
    // (replay-owner) Handed off to `strand-eval`, which is not part of
    // the shell (`report::replay`), its arguments as they are: a path
    // need not be UTF-8.
    if std::env::args_os().nth(1).is_some_and(|a| a == "replay") {
        let rest: Vec<std::ffi::OsString> = std::env::args_os().skip(2).collect();
        return crate::report::replay::main(&rest);
    }
    // Every other command reads words: an argument that is not UTF-8 is
    // read with its bad bytes replaced, never a panic.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
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
        // The flags a plugin author and a script need, though no row.
        assert!(
            out.contains(
                "\noptions:\n  --write-schemas   write this plugin binary's service schemas"
            ),
            "{out}"
        );
        assert!(out.contains("-V, --version"), "{out}");
        // The variables that pick a second shell's socket and state.
        assert!(out.contains("\n  STRAND_SOCKET "), "{out}");
        assert!(out.contains("\n  STRAND_STATE_DIR "), "{out}");
    }

    /// Every command the help lists is served: `run`, a tool or a
    /// one-shot (none answers "not implemented yet" any more).
    #[test]
    fn known_commands_are_all_served() {
        for (name, ..) in COMMANDS {
            assert!(
                *name == "run"
                    || *name == "replay"
                    || tool(name).is_some()
                    || one_shot(name).is_some(),
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

    /// (strand-fixes) Every command answers `--help` and `-h` as its
    /// first argument with its own usage; later in the line, or for no
    /// command, it is not help.
    #[test]
    fn every_command_has_its_help() {
        let os = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        for (name, ..) in COMMANDS {
            for flag in ["--help", "-h"] {
                let usage = help_asked(&os(&[name, flag]))
                    .unwrap_or_else(|| panic!("`strand {name} {flag}` is no help"));
                assert!(
                    usage.starts_with(&format!("usage: strand {name}")),
                    "{name}: {usage}"
                );
                assert!(usage.ends_with('\n'), "{name}: {usage:?}");
            }
        }
        assert_eq!(help_asked(&os(&["set", "a.b", "--help"])), None);
        assert_eq!(help_asked(&os(&["frobnicate", "--help"])), None);
        assert_eq!(help_asked(&os(&["--help"])), None);
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

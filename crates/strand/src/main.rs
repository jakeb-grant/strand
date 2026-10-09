//! The Strand shell runtime and CLI.
//!
//! See `docs/design.md` for the design and roadmap.

mod check;
mod demo;
mod fmt;
mod ipc;
mod live;
mod logging;
mod mock;
mod overlay;
mod rules;
mod run;
mod services;
mod system;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod fuzz;

use std::io::IsTerminal;
use std::process::ExitCode;

use strand_compiler::diagnostic::Style;

/// mimalloc for the whole runtime (`docs/design.md`, "Stack").
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// No transparent huge pages, from before the first allocation. On a
/// system with THP `always` (GitHub's runners) the kernel backs a
/// 2 MiB-aligned anonymous range with a huge page on its first touch:
/// mimalloc's arenas filled 2 MiB pages for a few KiB of heap (55 MB PSS
/// for design.md's bar instead of about 25). mimalloc's `no_thp` only
/// stops it asking for them, and turning THP off in `main` came after
/// the runtime's first allocations (the thread handle, the arguments)
/// had already faulted two huge pages in (4 MB of `AnonHugePages`, the
/// M0 gate missed by 265 kB in CI run 37644817292). An ELF constructor
/// at priority 100 runs before `main` and every other constructor in the
/// executable but std's argv capture (priority 99, which does not
/// allocate): mimalloc's own included (checked in the release binary's
/// `.init_array`: argv, this, then the unprioritised rest). The call
/// makes two `prctl`s and does not allocate. A failure (an old kernel)
/// leaves the default. The kernel keeps the flag across `fork` and
/// `exec`: every program strand starts gets back what strand inherited
/// (`strand_services::child::restore_in_child`, in each `pre_exec`).
#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array.00100")]
static NO_THP: extern "C" fn() = {
    extern "C" fn no_thp() {
        strand_services::child::thp_off();
    }
    no_thp
};

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
    ("get", "read an exported value", "M5"),
    (
        "set",
        "write an exported value (strand set theme.look mocha)",
        "M2",
    ),
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

/// A subcommand that reports through stderr text and a pass/fail flag.
type Tool = fn(&[String], Style) -> (String, bool);

fn main() -> ExitCode {
    // A text worker that goes quiet after shaping keeps the allocator's
    // freed pages, which only its own thread can return: it trims as its
    // queue drains (decisions.md, wave4-exitMemory); so does the image
    // decoder.
    strand_text::set_idle_hook(run::trim);
    strand_render::image::set_idle_hook(run::trim);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tool: Option<Tool> = match args.first().map(String::as_str) {
        Some("check") => Some(check::run),
        Some("fmt") => Some(fmt::run),
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
    match args.first().map(String::as_str) {
        Some("reload") => {
            return match ipc::reload_cli(&args[1..]) {
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
            return match ipc::set_cli(&args[1..]) {
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
            return match rules::run(&args[1..]) {
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
            return match ipc::watch_cli(&args[1..], &mut std::io::stdout()) {
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
            run(&["get"]).unwrap_err(),
            "strand get: not implemented yet (M5)"
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

    /// `NO_THP` ran before the test harness's `main`.
    #[test]
    #[cfg(target_os = "linux")]
    fn transparent_huge_pages_are_off_before_main() {
        if let Ok(off) = rustix::thread::transparent_huge_pages_are_disabled() {
            assert!(off, "THP is on for the process");
        }
    }

    /// A program strand starts gets back the THP setting strand inherited
    /// (its parent's): `NO_THP` turned it off for this process only.
    #[test]
    #[cfg(target_os = "linux")]
    fn programs_strand_starts_get_back_the_inherited_thp_setting() {
        let thp = |p: &str| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| strand_services::child::thp_enabled(&s))
        };
        let parent = format!("/proc/{}/status", std::os::unix::process::parent_id());
        let (Some(inherited), Some(own)) = (thp(&parent), thp("/proc/self/status")) else {
            eprintln!("skipped: no THP_enabled in /proc/<pid>/status");
            return;
        };
        assert!(!own, "THP is on for the process");
        let out = std::env::temp_dir().join(format!("strand-main-thp-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let part = out.with_extension("part");
        let script = format!(
            "grep THP_enabled /proc/self/status > '{}' && mv '{}' '{}'",
            part.display(),
            part.display(),
            out.display()
        );
        strand_services::apps::spawn_detached(&["sh".into(), "-c".into(), script], None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let got = thp(&out.display().to_string());
        let _ = std::fs::remove_file(&out);
        assert_eq!(got, Some(inherited), "the child kept strand's THP off");
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

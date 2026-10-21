//! (replay-owner) `strand replay [report-dir] [--no-open] …`: a glitch
//! report replayed in the evaluation sandbox and its viewer opened
//! (design.md, "Reporting a glitch"; docs/architecture.md, "Glitch
//! reports"). The sandbox is not part of the shipped shell: `strand`
//! hands off git-style to a `strand-eval` executable found next to the
//! running `strand` binary, else on `PATH`, as `strand-eval replay
//! <report-dir> …`, and says in plain words when there is none.
//!
//! With no directory named, the newest report under the state
//! directory's `reports/` is replayed (bundle names start with their
//! local time, so the last by name is the newest).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The sandbox's executable name.
pub(crate) const EVAL: &str = "strand-eval";

/// `strand-eval` beside `exe` (the running `strand`), else the first on
/// `path` (a `PATH` value). Only a regular file with an execute bit
/// counts.
pub(crate) fn find_eval(exe: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let runnable = |p: &Path| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if let Some(dir) = exe.and_then(Path::parent) {
        let p = dir.join(EVAL);
        if runnable(&p) {
            return Some(p);
        }
    }
    std::env::split_paths(path?)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(EVAL))
        .find(|p| runnable(p))
}

/// The newest report under `reports`: the last directory by name that
/// holds a `bundle.json`.
pub(crate) fn newest(reports: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(reports)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                && p.join("bundle.json").is_file()
        })
        .collect();
    dirs.sort();
    dirs.pop()
}

/// The options `strand-eval replay` takes a value with (eval/src/cli.rs
/// `replay`, `-o` the short `--out`): their value is never the report
/// directory.
const VALUED: [&str; 6] = ["--out", "-o", "--to", "--output", "--traced", "--frames"];

/// Whether `args` name a report directory (a word that is no option and
/// no option's value).
fn names_dir(args: &[OsString]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let s = a.to_string_lossy();
        if VALUED.contains(&s.as_ref()) {
            it.next();
        } else if !s.starts_with('-') {
            return true;
        }
    }
    false
}

/// The command `strand replay args` runs: `strand-eval replay` with the
/// arguments, the newest report first when none is named. `Err`: what
/// to say instead.
pub(crate) fn command(
    args: &[OsString],
    exe: Option<&Path>,
    path: Option<&OsStr>,
    reports: Option<&Path>,
) -> Result<std::process::Command, String> {
    let Some(eval) = find_eval(exe, path) else {
        let beside = exe.and_then(Path::parent).map_or_else(
            || "next to strand".to_string(),
            |d| format!("in {}", d.display()),
        );
        return Err(format!(
            "replay needs strand-eval, the evaluation sandbox, installed next to strand \
             ({beside}) or on PATH; it is not part of the shell. Build it from the strand \
             checkout (cargo build --release --manifest-path eval/Cargo.toml) and put the \
             strand-eval binary beside strand."
        ));
    };
    let mut cmd = std::process::Command::new(eval);
    cmd.arg("replay");
    if !names_dir(args) {
        let Some(reports) = reports else {
            return Err("no state directory to find reports in (set HOME)".into());
        };
        let Some(dir) = newest(reports) else {
            return Err(format!(
                "no glitch report under {} (save one with `strand report <what you saw>`)",
                reports.display()
            ));
        };
        cmd.arg(dir);
    }
    cmd.args(args);
    Ok(cmd)
}

/// `strand replay …`: replaces this process with `strand-eval replay …`
/// (its output and exit status are the command's), or says why not.
pub(crate) fn main(args: &[OsString]) -> ExitCode {
    let exe = std::env::current_exe().ok();
    let path = std::env::var_os("PATH");
    let reports = strand_core::persist::state_dir().map(|d| d.join("reports"));
    match command(args, exe.as_deref(), path.as_deref(), reports.as_deref()) {
        Ok(mut cmd) => {
            use std::os::unix::process::CommandExt as _;
            // The sandbox gets back the huge page setting strand
            // inherited (`process_setup!` turned it off for strand only).
            strand_services::child::restore_in_child();
            let err = cmd.exec();
            eprintln!("strand replay: could not run {}: {err}", EVAL);
            ExitCode::FAILURE
        }
        Err(why) => {
            eprintln!("strand replay: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strand-replay-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn stub(at: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_sibling_comes_before_path_and_a_plain_file_is_not_one() {
        let d = tmp("find");
        let exe = d.join("bin/strand");
        stub(&d.join("on-path").join(EVAL));
        let path = std::env::join_paths([d.join("on-path")]).unwrap();
        assert_eq!(
            find_eval(Some(&exe), Some(&path)),
            Some(d.join("on-path").join(EVAL))
        );
        // Not executable: not one.
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin").join(EVAL), "data").unwrap();
        assert_eq!(
            find_eval(Some(&exe), Some(&path)),
            Some(d.join("on-path").join(EVAL))
        );
        stub(&d.join("bin").join(EVAL));
        assert_eq!(
            find_eval(Some(&exe), Some(&path)),
            Some(d.join("bin").join(EVAL))
        );
        assert_eq!(find_eval(None, None), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_newest_report_is_the_last_by_name() {
        let d = tmp("newest");
        for n in [
            "2026-10-20_10-00-00",
            "2026-10-21_09-00-00",
            "2026-10-21_09-00-00-2",
        ] {
            std::fs::create_dir_all(d.join(n)).unwrap();
            std::fs::write(d.join(n).join("bundle.json"), "{}").unwrap();
        }
        // No bundle.json, or being written: not reports.
        std::fs::create_dir_all(d.join("2027-01-01_00-00-00")).unwrap();
        std::fs::create_dir_all(d.join(".2027-01-01_00-00-01-1.partial")).unwrap();
        std::fs::write(d.join(".2027-01-01_00-00-01-1.partial/bundle.json"), "{}").unwrap();
        assert_eq!(newest(&d), Some(d.join("2026-10-21_09-00-00-2")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_named_directory_is_passed_on_and_an_options_value_is_not_one() {
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(names_dir(&os(&["/r/x"])));
        assert!(names_dir(&os(&["--no-open", "/r/x"])));
        assert!(!names_dir(&os(&["--no-open", "--traced", "5"])));
        assert!(!names_dir(&os(&["--output", "DP-1"])));
        // `-o` is `--out`: its value is no report.
        assert!(!names_dir(&os(&["-o", "/tmp/r"])));
        assert!(!names_dir(&os(&[
            "--no-open",
            "-o",
            "/tmp/r",
            "--traced",
            "all"
        ])));
        assert!(names_dir(&os(&["-o", "/tmp/r", "/r/x"])));
    }

    #[test]
    fn without_strand_eval_it_says_what_to_install() {
        let d = tmp("none");
        let err =
            command(&[], Some(&d.join("strand")), Some(OsStr::new("")), Some(&d)).unwrap_err();
        assert!(err.contains("replay needs strand-eval"), "{err}");
        assert!(err.contains(&d.display().to_string()), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }
}

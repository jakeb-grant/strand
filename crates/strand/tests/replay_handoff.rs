//! (replay-owner) `strand replay` hands off git-style to `strand-eval`
//! (design.md, "Reporting a glitch"; docs/architecture.md, "Glitch
//! reports"): the one next to the running `strand` first, else the first
//! on `PATH`, given `replay` and the arguments, the newest report when
//! none is named; and a plain error when there is none. The `strand`
//! binary is linked into scratch directories so "next to it" is a place
//! the test chooses; `strand-eval` is a stub that writes down how it was
//! run.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch(tag: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("replay-handoff-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `strand` in `dir`: a hard link to the built binary (a copy where a
/// link cannot be made), so its own path is `dir/strand`.
fn strand_in(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let to = dir.join("strand");
    if std::fs::hard_link(env!("CARGO_BIN_EXE_strand"), &to).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_strand"), &to).unwrap();
    }
    to
}

/// A `strand-eval` stub in `dir` that writes its name and arguments,
/// one per line, to `log`.
fn stub_in(dir: &Path, name: &str, log: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("strand-eval");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\n{{ echo {name}; for a in \"$@\"; do echo \"$a\"; done; }} > '{}'\nexit 3\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(strand: &Path, args: &[&str], path: &Path, state: &Path) -> Output {
    Command::new(strand)
        .arg("replay")
        .args(args)
        .env("PATH", path)
        .env("XDG_STATE_HOME", state)
        .env_remove("STRAND_STATE_DIR")
        .output()
        .unwrap()
}

fn report(state: &Path, name: &str) -> PathBuf {
    let d = state.join("strand/reports").join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("bundle.json"), "{}").unwrap();
    d
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_sibling_strand_eval_is_run_with_the_arguments() {
    let d = scratch("sibling");
    let log = d.join("log");
    let strand = strand_in(&d.join("bin"));
    stub_in(&d.join("bin"), "sibling", &log);
    stub_in(&d.join("path"), "path", &log);
    let out = run(
        &strand,
        &["/r/x", "--no-open"],
        &d.join("path"),
        &d.join("state"),
    );
    // Its exit status is the sandbox's.
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(lines(&log), ["sibling", "replay", "/r/x", "--no-open"]);
}

#[test]
fn path_is_the_fallback_and_the_newest_report_the_default() {
    let d = scratch("path");
    let log = d.join("log");
    let strand = strand_in(&d.join("bin"));
    stub_in(&d.join("path"), "path", &log);
    let state = d.join("state");
    report(&state, "2026-10-20_10-00-00");
    let newest = report(&state, "2026-10-21_08-30-00");
    let out = run(&strand, &["--no-open"], &d.join("path"), &state);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(
        lines(&log),
        [
            "path".to_string(),
            "replay".into(),
            newest.display().to_string(),
            "--no-open".into()
        ]
    );
}

#[test]
fn without_strand_eval_it_says_so_plainly() {
    let d = scratch("none");
    let strand = strand_in(&d.join("bin"));
    std::fs::create_dir_all(d.join("empty")).unwrap();
    let out = run(&strand, &[], &d.join("empty"), &d.join("state"));
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("strand replay: replay needs strand-eval")
            && err.contains("installed next to strand")
            && err.contains(&d.join("bin").display().to_string()),
        "{err}"
    );
}

#[test]
fn with_no_report_it_says_where_it_looked() {
    let d = scratch("noreport");
    let log = d.join("log");
    let strand = strand_in(&d.join("bin"));
    stub_in(&d.join("bin"), "sibling", &log);
    let out = run(&strand, &[], &d.join("empty"), &d.join("state"));
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no glitch report under"), "{err}");
    assert!(!log.exists(), "strand-eval ran with no report");
}

/// (replay-owner review) A report directory whose name is not UTF-8 is
/// passed on byte for byte, never a panic.
#[test]
fn a_directory_that_is_not_utf8_is_passed_on_as_it_is() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    let d = scratch("bytes");
    let log = d.join("log");
    let strand = strand_in(&d.join("bin"));
    stub_in(&d.join("bin"), "sibling", &log);
    let name = OsStr::from_bytes(b"/r/caf\xe9");
    let out = Command::new(&strand)
        .arg("replay")
        .arg(name)
        .arg("-o")
        .arg("/tmp/r")
        .env("PATH", d.join("path"))
        .env("XDG_STATE_HOME", d.join("state"))
        .env_remove("STRAND_STATE_DIR")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let got = std::fs::read(&log).unwrap();
    let want = [
        b"sibling\nreplay\n".as_slice(),
        b"/r/caf\xe9\n",
        b"-o\n/tmp/r\n",
    ]
    .concat();
    assert_eq!(got, want);
}

/// (replay-owner review) `-o` is `--out`: its value is no report, so the
/// newest report is still put first.
#[test]
fn the_short_out_option_keeps_the_newest_report_the_default() {
    let d = scratch("short-out");
    let log = d.join("log");
    let strand = strand_in(&d.join("bin"));
    stub_in(&d.join("bin"), "sibling", &log);
    let state = d.join("state");
    let newest = report(&state, "2026-10-21_08-30-00");
    let out = run(&strand, &["-o", "/tmp/r"], &d.join("path"), &state);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert_eq!(
        lines(&log),
        [
            "sibling".to_string(),
            "replay".into(),
            newest.display().to_string(),
            "-o".into(),
            "/tmp/r".into()
        ]
    );
}

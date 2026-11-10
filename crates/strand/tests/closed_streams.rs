//! The `strand` binary with its stderr or stdout gone (a closed pipe, a
//! terminal closed under `strand run &`, `strand ... | head`): a log line,
//! a last error or a command's output is dropped, never a panic (exit 101).

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

/// A pipe whose read end is already closed: every write to it is EPIPE.
fn dead_pipe() -> Stdio {
    let (r, w) = std::io::pipe().unwrap();
    drop(r);
    Stdio::from(w)
}

fn strand_run(dir: &Path, tmp: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
    cmd.arg("run")
        .arg(dir)
        .env("STRAND_LOG", "warn")
        .env("XDG_CACHE_HOME", tmp.join("cache"))
        .env("XDG_RUNTIME_DIR", tmp)
        .env("WAYLAND_DISPLAY", "strand-test-no-such-display")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    cmd
}

#[test]
fn a_warning_to_a_closed_stderr_does_not_panic() {
    let tmp = std::env::temp_dir().join(format!("strand-closed-stderr-{}", std::process::id()));
    let dir = tmp.join("cfg");
    std::fs::create_dir_all(&dir).unwrap();

    // Readable stderr: the warning and the final error both print.
    let out = strand_run(&dir, &tmp)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nothing to run yet"), "{err}");
    let readable = out.status.code();

    // Dead stderr: the same run fails the same way, without a panic.
    let status: ExitStatus = strand_run(&dir, &tmp).stderr(dead_pipe()).status().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert_ne!(
        status.code(),
        Some(101),
        "strand panicked writing to a closed stderr"
    );
    assert_eq!(status.code(), readable);
}

#[test]
fn the_last_error_to_a_closed_stderr_does_not_panic() {
    let status = Command::new(env!("CARGO_BIN_EXE_strand"))
        .args(["run", "/strand-test-nonexistent-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(dead_pipe())
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(1),
        "strand run on a missing dir fails, quietly"
    );
}

#[test]
fn a_one_shot_command_whose_reader_closed_ends_quietly() {
    // `strand help | head -0`: the reader is gone before the output.
    for args in [&["help"][..], &["--version"], &["check", "--help"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(dead_pipe())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {err}");
    }
}

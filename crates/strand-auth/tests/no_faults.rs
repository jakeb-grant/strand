//! The `faults` feature's code never ships: the helper built with the
//! default features, in the dev and the release profile, holds none of
//! the test hooks' strings (`strings` on the binary, as bytes), while the
//! `faults` build the other tests use does. The builds go to a target
//! directory of their own, so they never disturb the test build's.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Strings only the `faults` code holds.
const FAULT_STRINGS: [&[u8]; 3] = [
    b"STRAND_FAULT",
    b"STRAND_AUTH_PAM_CONFDIR",
    b"pam_start_confdir",
];

fn holds(binary: &Path, needle: &[u8]) -> bool {
    let bytes = std::fs::read(binary).unwrap();
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// `<target root>/strand-auth-no-faults` (the test runs from
/// `<target root>/<profile>/deps`).
fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let root = exe.ancestors().nth(3).unwrap().to_path_buf();
    root.join("strand-auth-no-faults")
}

fn build(release: bool) -> PathBuf {
    let target = target_dir();
    let mut cmd = Command::new(env!("CARGO"));
    cmd.args([
        "build",
        "--offline",
        "-p",
        "strand-auth",
        "--bin",
        "strand-auth",
    ])
    .arg("--manifest-path")
    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
    .arg("--target-dir")
    .arg(&target);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().unwrap();
    assert!(status.success(), "building the default helper failed");
    target
        .join(if release { "release" } else { "debug" })
        .join("strand-auth")
}

#[test]
fn default_and_release_helpers_carry_no_fault_code() {
    let faulty = Path::new(env!("CARGO_BIN_EXE_strand-auth"));
    for s in FAULT_STRINGS {
        assert!(
            holds(faulty, s),
            "the test build lacks {:?}: the check would prove nothing",
            String::from_utf8_lossy(s)
        );
    }
    for release in [false, true] {
        let bin = build(release);
        for s in FAULT_STRINGS {
            assert!(
                !holds(&bin, s),
                "{} holds {:?}",
                bin.display(),
                String::from_utf8_lossy(s)
            );
        }
    }
}

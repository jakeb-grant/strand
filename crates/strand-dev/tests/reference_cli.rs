//! `strand-dev reference [dir]` from the command line: it writes only into
//! a reference directory and never makes a stray `docs/reference` (m5-docs3).

use std::path::PathBuf;
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-ref-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// With no directory named, away from the repository's root, it fails and
/// makes nothing.
#[test]
fn the_default_directory_is_only_the_repositorys() {
    let d = scratch("default");
    let out = Command::new(env!("CARGO_BIN_EXE_strand-dev"))
        .arg("reference")
        .current_dir(&d)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no docs/reference here"));
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&d);
}

/// `strand-dev reference docs` (a directory of hand-written pages) fails
/// and leaves every page as it was.
#[test]
fn a_directory_of_hand_written_pages_is_refused() {
    let d = scratch("docs");
    std::fs::write(d.join("design.md"), "# Design\n").unwrap();
    std::fs::write(d.join("README.md"), "# Readme\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_strand-dev"))
        .arg("reference")
        .arg(&d)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a reference directory"));
    assert_eq!(
        std::fs::read_to_string(d.join("design.md")).unwrap(),
        "# Design\n"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("README.md")).unwrap(),
        "# Readme\n"
    );
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

/// A new directory is filled, and a second run finds it up to date.
#[test]
fn a_new_directory_is_written_then_up_to_date() {
    let d = scratch("new").join("reference");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_strand-dev"))
            .arg("reference")
            .arg(&d)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(d.join("elements.md").exists());
    let second = run();
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("up to date"));
    let _ = std::fs::remove_dir_all(d.parent().unwrap());
}

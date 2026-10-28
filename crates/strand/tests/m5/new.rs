//! S-tools: `strand new [dir]`: the guide's level-1
//! hello bar written into a new or empty directory (the default
//! `$XDG_CONFIG_HOME/strand`), which `strand check` passes; a directory
//! with anything in it is refused and left as it was, and the template
//! is the guide's first level-1 block. That the written shell runs on
//! two outputs, against grim references, is `acceptance.rs`'s
//! `the_new_shell_checks_and_runs_on_two_outputs` (that binary holds the
//! reference machinery and the frozen-clock mock); `report.rs` runs it
//! too, under `strand report`.

use std::path::Path;
use std::process::{Command, Output};

/// The template `strand new` embeds.
pub(super) fn template() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/guide/template/bar.strand"
    ))
    .unwrap()
}

fn strand(args: &[&str], xdg: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_strand"))
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .env("HOME", xdg.join("home"))
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}

fn temp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("strand-new-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_new_shell_is_the_template_and_checks() {
    let xdg = temp("default");
    // No argument: `$XDG_CONFIG_HOME/strand`, made.
    let out = strand(&["new"], &xdg);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = xdg.join("strand");
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(
        said.contains(&dir.join("bar.strand").display().to_string()),
        "{said}"
    );
    assert!(said.contains("strand run"), "{said}");
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["bar.strand"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("bar.strand")).unwrap(),
        template()
    );
    let check = strand(&["check", dir.to_str().unwrap()], &xdg);
    assert!(
        check.status.success(),
        "strand check on the new shell: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    // A named empty directory works too.
    let other = xdg.join("empty");
    std::fs::create_dir_all(&other).unwrap();
    let out = strand(&["new", other.to_str().unwrap()], &xdg);
    assert!(out.status.success());
    assert!(other.join("bar.strand").is_file());
    let _ = std::fs::remove_dir_all(xdg);
}

#[test]
fn a_non_empty_dir_is_refused() {
    let xdg = temp("full");
    let dir = xdg.join("strand");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bar.strand"), "// mine\n").unwrap();
    let out = strand(&["new"], &xdg);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.starts_with("strand new: "), "{err}");
    assert!(err.contains("is not empty"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join("bar.strand")).unwrap(),
        "// mine\n"
    );
    // A usage error says how it is used.
    let out = strand(&["new", "a", "b"], &xdg);
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: strand new [dir]"));
    let _ = std::fs::remove_dir_all(xdg);
}

/// The template is byte for byte the guide's first level-1 block, the
/// one level 1 shows as "what it wrote" (new.rs's TEMPLATE says so):
/// an edit to either alone fails here.
#[test]
fn the_template_is_the_guides_first_block() {
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/guide/level-1.md"
    ))
    .unwrap();
    let fence = "```strand level=1 file=bar.strand\n";
    let start = page.find(fence).expect("level 1 has a bar.strand block") + fence.len();
    let len = page[start..].find("```\n").expect("the block ends");
    assert_eq!(&page[start..start + len], template());
}

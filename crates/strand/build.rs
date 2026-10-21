//! (report) `STRAND_COMMIT`: the commit a glitch report names
//! (docs/architecture.md, "Glitch reports"): `git rev-parse HEAD`,
//! `-dirty` appended when the work tree had changes as the binary was
//! built, `unknown` outside a checkout or without git.

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"]).filter(|c| !c.is_empty());
    let value = match &commit {
        Some(c) => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            if dirty {
                format!("{c}-dirty")
            } else {
                c.clone()
            }
        }
        None => "unknown".to_string(),
    };
    println!("cargo:rustc-env=STRAND_COMMIT={value}");
    // Built again when HEAD moves or the index changes (a commit, a
    // checkout, a staged edit); an unstaged edit alone keeps the last
    // value until then.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        let dir = PathBuf::from(dir);
        println!("cargo:rerun-if-changed={}", dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", dir.join("index").display());
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            let common = git(&["rev-parse", "--git-common-dir"]).map(PathBuf::from);
            let base = common.unwrap_or(dir);
            println!("cargo:rerun-if-changed={}", base.join(r).display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

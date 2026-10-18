//! eval/ is its own workspace, so it cannot inherit the root's profiles
//! and lints: eval/Cargo.toml copies them, and this test keeps the copy
//! equal (docs/decisions.md eval-sandbox). The precedent is
//! crates/strand/tests/m5/plugin.rs's guide profile check.
//!
//! (eval-host-loop) The driver runs the shell's frame sequence through
//! the host loop's reactions (`strand_render::host_loop`), and this test
//! keeps it from taking any of their steps by hand, where it could drift
//! from the shell unseen (crates/strand/src/demo/mod.rs holds the
//! shell's side).

use std::path::Path;

/// The marker both manifests share: everything from it to the end is
/// the root's lints and profiles.
const FROM: &str = "[workspace.lints.rust]";

/// The sections of `toml` from [`FROM`] on, each its header and lines,
/// without comments or blank lines.
fn sections(toml: &str) -> Vec<Vec<String>> {
    let at = toml
        .find(FROM)
        .expect("the manifest has [workspace.lints.rust]");
    let mut out: Vec<Vec<String>> = Vec::new();
    for line in toml[at..].lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') || out.is_empty() {
            out.push(Vec::new());
        }
        if let Some(last) = out.last_mut() {
            last.push(line.to_owned());
        }
    }
    out
}

/// The package a `[profile.<name>.package.<pkg>]` header names (`None`
/// for any other header, and for `"*"`).
fn profile_package(header: &str) -> Option<&str> {
    let inner = header.strip_prefix("[profile.")?.strip_suffix(']')?;
    let (_, pkg) = inner.split_once(".package.")?;
    let pkg = pkg.trim_matches('"');
    (pkg != "*").then_some(pkg)
}

/// The package names in a Cargo.lock.
fn locked(lock: &str) -> Vec<&str> {
    lock.lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect()
}

#[test]
fn the_profiles_and_lints_match_the_root_workspace() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = std::fs::read_to_string(here.join("../Cargo.toml")).unwrap();
    let eval = std::fs::read_to_string(here.join("Cargo.toml")).unwrap();
    let lock = std::fs::read_to_string(here.join("Cargo.lock")).unwrap();
    let names = locked(&lock);
    assert!(
        names.contains(&"strand-render"),
        "eval/Cargo.lock lists its packages"
    );
    // The root's, but for the per-package levels of packages eval's
    // graph does not have (cargo warns about each).
    let want: Vec<Vec<String>> = sections(&root)
        .into_iter()
        .filter(|s| profile_package(&s[0]).is_none_or(|p| names.contains(&p)))
        .collect();
    let headers: Vec<&str> = want.iter().map(|s| s[0].as_str()).collect();
    // The lints, dev, test, release (and its per-package levels) and
    // timing profiles are all there.
    for section in [
        "[workspace.lints.clippy]",
        "[profile.dev]",
        "[profile.dev.package.\"*\"]",
        "[profile.test]",
        "[profile.release]",
        "[profile.timing]",
    ] {
        assert!(headers.contains(&section), "root lacks {section}");
    }
    assert!(
        headers
            .iter()
            .any(|h| profile_package(h) == Some("strand-icons")),
        "a per-package level eval's graph has is kept: {headers:?}"
    );
    assert_eq!(
        sections(&eval),
        want,
        "eval/Cargo.toml's lints and profiles differ from the root Cargo.toml's: copy the root's \
         from {FROM} to the end, leaving out `[profile.*.package.X]` for packages not in \
         eval/Cargo.lock"
    );
}

#[test]
fn the_package_is_its_own_workspace_and_turns_the_hooks_on() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let eval = std::fs::read_to_string(here.join("Cargo.toml")).unwrap();
    let lines: Vec<&str> = eval.lines().map(str::trim).collect();
    assert!(
        lines.contains(&"[workspace]"),
        "eval/Cargo.toml is a workspace root"
    );
    assert!(
        !lines.iter().any(|l| l.starts_with("members")),
        "eval's workspace has no other members"
    );
    let render = lines
        .iter()
        .find(|l| l.starts_with("strand-render ="))
        .expect("eval depends on strand-render");
    assert!(render.contains("\"eval-hooks\""), "{render}");
}

/// The host loop's steps a host must not take by hand: routing's, the
/// input wake's, the surfaces' and the renderer's own.
const STEPS: [&str; 14] = [
    "router.observe(",
    "router.settle(",
    "router.tick(",
    "router.handle(",
    "router.attached(",
    "router.detached(",
    "set_input_wake(",
    "attach_surface(",
    "detach_surface(",
    "configure_surface(",
    ".r.apply(",
    ".r.update()",
    "renderer.apply(",
    "renderer.update()",
];

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// (eval-host-loop) The driver takes logic's diffs, its wakes, input and
/// its surfaces' attach, configure, detach and paints through the host
/// loop's reactions, as the shell's main thread does, and takes none of
/// their steps by hand outside tests.
///
/// (eval-r3) A tripwire, not a proof: it matches the call spellings in
/// `STEPS`, so a step reached through an alias (`let r = &mut self.r;
/// r.update()`) or a helper that takes the router by another name gets
/// past it. The steps stay public because the crates' own tests and the
/// shell's other hosts call them; what holds the order is review, with
/// this test catching the plain case (docs/decisions.md eval-r3).
#[test]
fn the_driver_takes_the_host_loops_steps_only_through_its_reactions() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    files.sort();
    let mut taken = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&src).unwrap().display().to_string();
        let text = std::fs::read_to_string(f).unwrap();
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        // A file's code ends where its test module starts.
        let end = lines
            .windows(2)
            .position(|w| w[0] == "#[cfg(test)]" && w[1].starts_with("mod tests"))
            .unwrap_or(lines.len());
        for (i, line) in lines[..end].iter().enumerate() {
            if line.starts_with("//") {
                continue;
            }
            for step in STEPS.iter().filter(|s| line.contains(*s)) {
                taken.push(format!("{rel}:{}: {step}", i + 1));
            }
        }
    }
    assert!(
        taken.is_empty(),
        "host loop steps taken by hand (run them through strand_render::host_loop):\n{}",
        taken.join("\n")
    );
    let harness = std::fs::read_to_string(src.join("harness.rs")).unwrap();
    for reaction in [
        "apply",
        "woke",
        "input",
        "attached",
        "configured",
        "detached",
        "painted",
    ] {
        assert!(
            harness.contains(&format!("host_loop::{reaction}(")),
            "the driver runs no `host_loop::{reaction}`"
        );
    }
}

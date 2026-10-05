//! The loader: the largest consistent set, the last good tree and its
//! cache (`strand_compiler::reconcile::loader`).

use std::path::{Path, PathBuf};

use strand_compiler::reconcile::loader::Loader;
use strand_compiler::schema::Schema;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-loader-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("conf")).unwrap();
    d
}

fn write(d: &Path, name: &str, text: &str) -> PathBuf {
    let p = d.join("conf").join(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn loader(d: &Path) -> Loader {
    Loader::new(
        d.join("conf"),
        Schema::builtin().clone(),
        Some(d.join("cache")),
    )
}

const BAR: &str = "bar Top {\n  Clock { open: true }\n}\n";
const CLOCK: &str = "component Clock(open: bool = false) {\n  text open ? \"open\" : \"shut\"\n}\n";

#[test]
fn a_clean_config_commits_and_a_change_recommits() {
    let d = dir("clean");
    write(&d, "bar.strand", BAR);
    let clock = write(&d, "clock.strand", CLOCK);
    let mut l = loader(&d);
    let out = l.boot();
    assert!(out.build.is_some(), "{:?}", out.diagnostics);
    assert_eq!(out.committed.len(), 2);
    // Nothing changed: nothing to commit.
    assert!(l.changed([(clock.clone(), true)]).build.is_none());
    std::fs::write(&clock, CLOCK.replace("shut", "closed")).unwrap();
    let out = l.changed([(clock.clone(), true)]);
    assert_eq!(out.committed, [clock]);
    assert!(out.diagnostics.is_empty());
    let _ = std::fs::remove_dir_all(d);
}

#[test]
fn a_broken_file_is_held_back_and_the_last_good_tree_stays() {
    let d = dir("broken");
    let bar = write(&d, "bar.strand", BAR);
    write(&d, "clock.strand", CLOCK);
    let mut l = loader(&d);
    let first = l.boot().build.unwrap();
    std::fs::write(&bar, "bar Top {\n  Clock { open: true }\n").unwrap();
    let out = l.changed([(bar.clone(), true)]);
    assert!(out.build.is_none());
    assert_eq!(out.held, std::slice::from_ref(&bar));
    assert!(out.errors() > 0);
    assert!(std::sync::Arc::ptr_eq(
        &l.last().unwrap().program,
        &first.program
    ));
    // The fix commits.
    std::fs::write(&bar, BAR.replace("true", "false")).unwrap();
    let out = l.changed([(bar.clone(), true)]);
    assert!(out.build.is_some());
    assert!(out.held.is_empty());
    let _ = std::fs::remove_dir_all(d);
}

/// design.md, "What you see" 2: renaming a component's parameter in one
/// file breaks its caller in another. Saved alone, the rename is held
/// back with the caller's error; saved with the caller, both commit
/// together. A third file saved meanwhile commits on its own.
#[test]
fn the_largest_consistent_set_commits_and_the_rest_is_held() {
    let d = dir("set");
    let bar = write(&d, "bar.strand", BAR);
    let clock = write(&d, "clock.strand", CLOCK);
    let extra = write(&d, "extra.strand", "state n = 1\n");
    let mut l = loader(&d);
    assert!(l.boot().build.is_some());
    std::fs::write(&clock, CLOCK.replace("open", "expanded")).unwrap();
    std::fs::write(&extra, "state n = 2\n").unwrap();
    let out = l.changed([(clock.clone(), true), (extra.clone(), true)]);
    assert_eq!(out.committed, std::slice::from_ref(&extra));
    assert_eq!(out.held, std::slice::from_ref(&clock));
    let shown = strand_compiler::diagnostic::render_short(&out.diagnostics, &out.sources);
    assert!(shown.contains("bar.strand"), "{shown}");
    // The caller follows: both commit.
    std::fs::write(&bar, BAR.replace("open", "expanded")).unwrap();
    let out = l.changed([(bar.clone(), true)]);
    assert!(out.build.is_some(), "{:?}", out.diagnostics);
    let mut both = out.committed.clone();
    both.sort();
    assert_eq!(both, [bar, clock]);
    let _ = std::fs::remove_dir_all(d);
}

/// A config broken at boot runs its last good version from the cache,
/// with the broken file held back on top of it; a file removed and
/// re-added follows.
#[test]
fn a_config_broken_at_boot_runs_its_last_good_version() {
    let d = dir("cache");
    let bar = write(&d, "bar.strand", BAR);
    let clock = write(&d, "clock.strand", CLOCK);
    {
        let mut l = loader(&d);
        assert!(l.boot().build.is_some());
        assert!(l.cache_error().is_none(), "{:?}", l.cache_error());
    }
    std::fs::write(&bar, "bar Top {\n  Clock { open: tru }\n}\n").unwrap();
    let mut l = loader(&d);
    let out = l.boot();
    assert!(out.from_cache);
    let build = out.build.clone().expect("the cached build");
    assert!(build.program.files.iter().any(|f| f.module == "bar"));
    assert_eq!(out.held, std::slice::from_ref(&bar));
    assert!(out.errors() > 0);
    // Without a cache only what is consistent on its own runs: the
    // component file, not the broken bar.
    let mut bare = Loader::new(d.join("conf"), Schema::builtin().clone(), None);
    let out = bare.boot();
    assert!(!out.from_cache);
    let b = out.build.unwrap();
    assert!(!b.program.files.iter().any(|f| f.module == "bar"));
    // A removed file is a change too.
    std::fs::write(&bar, BAR.replace("true", "false")).unwrap();
    assert!(l.changed([(bar.clone(), true)]).build.is_some());
    std::fs::remove_file(&clock).unwrap();
    let out = l.changed([(clock.clone(), false)]);
    assert!(out.build.is_none(), "bar.strand needs Clock");
    assert_eq!(out.held, [clock]);
    let _ = std::fs::remove_dir_all(d);
}

//! Real-filesystem tests: the five editor save styles (design.md, "How
//! reload is tested"), scratch names, no-op and own writes, GNU stow
//! directory links, "save all" coalescing, settings, wallpapers, cache
//! trees and the polling fallback.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use strand_watch::{
    CacheKind, ChangeEvent, ChangeKind, ConfigWatch, FileBatch, FileChange, ModuleSet, Notice,
    Options, RescanReason, Role, Watcher, channel, hash_bytes,
};

/// Long enough that a second batch from the same save would have arrived
/// (coalesce 15 ms, removal grace 50 ms), with slack for a loaded machine.
const SETTLE: Duration = Duration::from_millis(300);
const FIRST: Duration = Duration::from_secs(5);

/// The module set from the real `source::find_files`, as the binary
/// builds it.
fn modules(root: &Path) -> std::io::Result<ModuleSet> {
    let found = strand_compiler::source::find_files(root)?;
    Ok(ModuleSet {
        files: found.files,
        dirs: found.dirs,
    })
}

fn config(root: &Path) -> ConfigWatch {
    let r = root.to_path_buf();
    ConfigWatch {
        root: root.to_path_buf(),
        modules: modules(root).unwrap(),
        rescan: Box::new(move || modules(&r)),
    }
}

struct Fx {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    cfg: PathBuf,
    watcher: Watcher,
    rx: Receiver<ChangeEvent>,
}

/// A config dir `cfg/` holding `bar.strand` and `theme.strand`.
fn fixture() -> Fx {
    fixture_with(Options::default(), |_| {})
}

fn fixture_with(opts: Options, setup: impl FnOnce(&Path)) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let cfg = base.join("cfg");
    fs::create_dir(&cfg).unwrap();
    fs::write(cfg.join("bar.strand"), "bar {}\n").unwrap();
    fs::write(
        cfg.join("theme.strand"),
        "tokens base { accent: #7aa2f7 }\n",
    )
    .unwrap();
    setup(&base);
    let (sink, rx) = channel();
    let watcher = Watcher::spawn(Some(config(&cfg)), opts, sink).unwrap();
    Fx {
        _tmp: tmp,
        base,
        cfg,
        watcher,
        rx,
    }
}

fn next_files(rx: &Receiver<ChangeEvent>, within: Duration) -> Option<FileBatch> {
    let end = Instant::now() + within;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ChangeEvent::Files(b)) => {
                // Polling notices come alone; they are not changes.
                if b.changes.is_empty() && b.rescan.is_none() {
                    continue;
                }
                return Some(b);
            }
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Exactly one batch arrives, and nothing after it.
fn one_batch(fx: &Fx) -> FileBatch {
    let b = next_files(&fx.rx, FIRST).expect("no batch");
    if let Some(extra) = next_files(&fx.rx, SETTLE) {
        panic!("a second batch: {extra:#?}\nafter: {b:#?}");
    }
    b
}

fn no_batch(fx: &Fx) {
    if let Some(b) = next_files(&fx.rx, SETTLE) {
        panic!("unexpected batch: {b:#?}");
    }
}

fn assert_modified(c: &FileChange, path: &Path, content: &str) {
    assert_eq!(c.path, path);
    assert_eq!(c.kind, ChangeKind::Modified);
    assert_eq!(c.hash, Some(hash_bytes(content.as_bytes())));
    assert_eq!(c.error, None);
}

const NEW: &str = "tokens base { accent: #ff9e64 }\n";

fn only_theme_changed(fx: &Fx, content: &str) {
    let b = one_batch(fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &fx.cfg.join("theme.strand"), content);
    assert_eq!(b.changes[0].role, Role::Module);
    assert_eq!(b.rescan, None);
}

// --- The five save styles -------------------------------------------------

/// VS Code: truncate and rewrite in place. The half-written file (MODIFY)
/// is never read; the batch comes after CLOSE_WRITE.
#[test]
fn save_in_place() {
    let fx = fixture();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(fx.cfg.join("theme.strand"))
        .unwrap();
    f.write_all(&NEW.as_bytes()[..10]).unwrap();
    f.flush().unwrap();
    // A slow writer: MODIFY seen, no close yet. Nothing may be reported.
    no_batch(&fx);
    f.write_all(&NEW.as_bytes()[10..]).unwrap();
    drop(f);
    only_theme_changed(&fx, NEW);
}

/// Helix `atomic-save`: write a temp file, rename it over the original.
#[test]
fn save_rename_over() {
    let fx = fixture();
    let tmp = fx.cfg.join("theme.strand.helix-1a2b");
    fs::write(&tmp, NEW).unwrap();
    fs::rename(&tmp, fx.cfg.join("theme.strand")).unwrap();
    only_theme_changed(&fx, NEW);
}

/// Vim `backupcopy=auto`: probe with `4913`, rename the original to a `~`
/// backup, write a new file, delete the backup; a swap file churns too.
#[test]
fn save_backup_then_rename() {
    let fx = fixture();
    let theme = fx.cfg.join("theme.strand");
    fs::write(fx.cfg.join(".theme.strand.swp"), "swap").unwrap();
    fs::write(fx.cfg.join("4913"), "").unwrap();
    fs::remove_file(fx.cfg.join("4913")).unwrap();
    fs::rename(&theme, fx.cfg.join("theme.strand~")).unwrap();
    fs::write(&theme, NEW).unwrap();
    fs::remove_file(fx.cfg.join("theme.strand~")).unwrap();
    fs::write(fx.cfg.join(".theme.strand.swp"), "swap 2").unwrap();
    only_theme_changed(&fx, NEW);
}

/// Delete, then create: one `Modified`, never `Removed` then `Created`.
#[test]
fn save_delete_and_create() {
    let fx = fixture();
    let theme = fx.cfg.join("theme.strand");
    fs::remove_file(&theme).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    fs::write(&theme, NEW).unwrap();
    only_theme_changed(&fx, NEW);
}

/// home-manager: the file is a link into a read-only store; a switch swaps
/// the link (new link, renamed over). The link's directory sees it.
#[test]
fn save_symlink_swap() {
    let fx = fixture_with(Options::default(), |base| {
        let v1 = base.join("store/v1");
        fs::create_dir_all(&v1).unwrap();
        fs::write(v1.join("theme.strand"), "tokens base { accent: #7aa2f7 }\n").unwrap();
        fs::remove_file(base.join("cfg/theme.strand")).unwrap();
        symlink(v1.join("theme.strand"), base.join("cfg/theme.strand")).unwrap();
    });
    let v2 = fx.base.join("store/v2");
    fs::create_dir_all(&v2).unwrap();
    fs::write(v2.join("theme.strand"), NEW).unwrap();
    // Unrelated store churn is not a change.
    no_batch(&fx);
    let tmp = fx.cfg.join(".theme.strand.hm-tmp");
    symlink(v2.join("theme.strand"), &tmp).unwrap();
    fs::rename(&tmp, fx.cfg.join("theme.strand")).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &fx.cfg.join("theme.strand"), NEW);
    assert_eq!(b.changes[0].canonical, Some(v2.join("theme.strand")));

    // The new target's directory is watched now; the old one's is not
    // needed. Editing the target through its own path is an edit.
    fs::write(v2.join("theme.strand"), "x").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &fx.cfg.join("theme.strand"), "x");
    fs::write(fx.base.join("store/v1/theme.strand"), "old target").unwrap();
    no_batch(&fx);

    // A swap to a target with the same bytes is still an edit: the hash
    // is unchanged (no recompile) but the canonical path is new (the old
    // store path may be garbage-collected).
    let v3 = fx.base.join("store/v3");
    fs::create_dir_all(&v3).unwrap();
    fs::write(v3.join("theme.strand"), "x").unwrap();
    symlink(v3.join("theme.strand"), &tmp).unwrap();
    fs::rename(&tmp, fx.cfg.join("theme.strand")).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &fx.cfg.join("theme.strand"), "x");
    assert_eq!(b.changes[0].canonical, Some(v3.join("theme.strand")));
}

// --- Filtering and hashing -------------------------------------------------

#[test]
fn scratch_files_are_ignored() {
    let fx = fixture();
    for name in [
        "4913",
        "theme.strand.swp",
        "bar.strand~",
        "bar.strand___jb_tmp___",
        "bar.strand___jb_old___",
        ".hidden.strand",
    ] {
        fs::write(fx.cfg.join(name), "scratch").unwrap();
    }
    for name in ["4913", "bar.strand___jb_tmp___"] {
        fs::remove_file(fx.cfg.join(name)).unwrap();
    }
    // Not a module: other extensions in the config dir are not watched
    // unless referenced.
    fs::write(fx.cfg.join("notes.txt"), "hi").unwrap();
    no_batch(&fx);
}

#[test]
fn no_op_saves_are_skipped() {
    let fx = fixture();
    let bar = fx.cfg.join("bar.strand");
    fs::write(&bar, "bar {}\n").unwrap();
    let tmp = fx.cfg.join("bar.strand.tmp");
    fs::write(&tmp, "bar {}\n").unwrap();
    fs::rename(&tmp, &bar).unwrap();
    no_batch(&fx);
}

#[test]
fn own_writes_are_skipped() {
    let fx = fixture();
    let bar = fx.cfg.join("bar.strand");
    let ours = "bar { height: 32 }\n";
    fx.watcher
        .register_own_write(&bar, hash_bytes(ours.as_bytes()));
    let tmp = fx.cfg.join(".bar.strand.strand-tmp");
    fs::write(&tmp, ours).unwrap();
    fs::rename(&tmp, &bar).unwrap();
    no_batch(&fx);
    // The registration is spent: the user's next save is reported, even
    // with the same bytes Strand wrote earlier… once they differ from now.
    fs::write(&bar, "bar { height: 40 }\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &bar, "bar { height: 40 }\n");
}

/// "Save all" across files is one batch.
#[test]
fn save_all_is_one_batch() {
    let fx = fixture();
    let mut expect = Vec::new();
    for (i, name) in ["bar.strand", "theme.strand", "sub/osd.strand"]
        .iter()
        .enumerate()
    {
        let p = fx.cfg.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let body = format!("edit {i}\n");
        fs::write(&p, &body).unwrap();
        expect.push((p, body));
        std::thread::sleep(Duration::from_millis(1));
    }
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 3, "{b:#?}");
    assert!(b.first_event <= b.last_event && b.last_event <= Instant::now());
    expect.sort();
    for (c, (p, body)) in b.changes.iter().zip(&expect) {
        assert_eq!(&c.path, p);
        assert_eq!(c.hash, Some(hash_bytes(body.as_bytes())));
        let created = p.ends_with("sub/osd.strand");
        let kind = if created {
            ChangeKind::Created
        } else {
            ChangeKind::Modified
        };
        assert_eq!(c.kind, kind);
    }
}

// --- Module set ------------------------------------------------------------

#[test]
fn modules_appear_and_disappear() {
    let fx = fixture();
    let osd = fx.cfg.join("widgets/osd.strand");
    fs::create_dir(fx.cfg.join("widgets")).unwrap();
    fs::write(&osd, "osd {}\n").unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, osd);
    assert_eq!(b.changes[0].kind, ChangeKind::Created);

    // The new directory is watched: an edit inside it is seen.
    fs::write(&osd, "osd { x: 1 }\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &osd, "osd { x: 1 }\n");

    fs::remove_file(fx.cfg.join("bar.strand")).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, fx.cfg.join("bar.strand"));
    assert_eq!(b.changes[0].kind, ChangeKind::Removed);
    assert_eq!(b.changes[0].hash, None);
}

/// GNU stow: `~/.config/strand` itself links into `~/dotfiles`. Edits in
/// the target are reported under the config path; restowing (re-pointing
/// the directory link) is seen in the link's directory and rescans.
#[test]
fn stow_directory_link() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let dots = base.join("dotfiles/strand");
    fs::create_dir_all(&dots).unwrap();
    fs::write(dots.join("bar.strand"), "bar {}\n").unwrap();
    fs::create_dir_all(base.join("home/.config")).unwrap();
    let cfg = base.join("home/.config/strand");
    symlink(&dots, &cfg).unwrap();
    let (sink, rx) = channel();
    let _w = Watcher::spawn(Some(config(&cfg)), Options::default(), sink).unwrap();
    let fx_rx = |within| next_files(&rx, within);

    // Edit through the dotfiles path.
    fs::write(dots.join("bar.strand"), "bar { a: 1 }\n").unwrap();
    let b = fx_rx(FIRST).expect("edit in the link target");
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, cfg.join("bar.strand"));
    assert_eq!(b.changes[0].canonical, Some(dots.join("bar.strand")));
    assert!(fx_rx(SETTLE).is_none());

    // Restow onto another tree.
    let dots2 = base.join("dotfiles2/strand");
    fs::create_dir_all(&dots2).unwrap();
    fs::write(dots2.join("bar.strand"), "bar { a: 2 }\n").unwrap();
    fs::write(dots2.join("osd.strand"), "osd {}\n").unwrap();
    let tmp_link = base.join("home/.config/.strand-restow");
    symlink(&dots2, &tmp_link).unwrap();
    fs::rename(&tmp_link, &cfg).unwrap();
    let b = fx_rx(FIRST).expect("restow");
    assert!(fx_rx(SETTLE).is_none());
    let got: Vec<_> = b
        .changes
        .iter()
        .map(|c| (c.path.clone(), c.kind, c.canonical.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                cfg.join("bar.strand"),
                ChangeKind::Modified,
                Some(dots2.join("bar.strand"))
            ),
            (
                cfg.join("osd.strand"),
                ChangeKind::Created,
                Some(dots2.join("osd.strand"))
            ),
        ]
    );

    // The old tree is no longer watched; the new one is.
    fs::write(dots.join("bar.strand"), "stale").unwrap();
    assert!(fx_rx(SETTLE).is_none());
    fs::write(dots2.join("osd.strand"), "osd { b: 1 }\n").unwrap();
    let b = fx_rx(FIRST).expect("edit in the new tree");
    assert_eq!(b.changes[0].path, cfg.join("osd.strand"));
}

// --- Referenced files --------------------------------------------------------

#[test]
fn settings_file_created_later_and_edited() {
    let fx = fixture();
    let prefs = fx.cfg.join("prefs.toml");
    fx.watcher.watch_file(&prefs, Role::Settings).unwrap();
    fs::write(&prefs, "compact = true\n").unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, prefs);
    assert_eq!(b.changes[0].kind, ChangeKind::Created);
    assert_eq!(b.changes[0].role, Role::Settings);

    // A toml_edit write-back by Strand (temp + rename) is ours.
    let ours = "compact = false\n";
    fx.watcher
        .register_own_write(&prefs, hash_bytes(ours.as_bytes()));
    let tmp = fx.cfg.join(".prefs.toml.tmp");
    fs::write(&tmp, ours).unwrap();
    fs::rename(&tmp, &prefs).unwrap();
    no_batch(&fx);

    fx.watcher.unwatch_file(&prefs, Role::Settings).unwrap();
    fs::write(&prefs, "compact = true\n").unwrap();
    no_batch(&fx);
}

/// The wallpaper path and its symlink target are both watched (`swww` or a
/// script replacing either re-themes).
#[test]
fn wallpaper_link_and_target() {
    let fx = fixture();
    let pics = fx.base.join("pics");
    fs::create_dir(&pics).unwrap();
    fs::write(pics.join("a.png"), "png a").unwrap();
    fs::write(pics.join("b.png"), "png b").unwrap();
    let current = fx.base.join("wallpaper");
    symlink(pics.join("a.png"), &current).unwrap();
    fx.watcher.watch_file(&current, Role::Wallpaper).unwrap();

    fs::write(pics.join("a.png"), "png a2").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &current, "png a2");
    assert_eq!(b.changes[0].role, Role::Wallpaper);

    // `ln -sf` unlinks and re-creates the link: no CLOSE_WRITE at all.
    fs::remove_file(&current).unwrap();
    symlink(pics.join("b.png"), &current).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &current, "png b");
    assert_eq!(b.changes[0].canonical, Some(pics.join("b.png")));
}

#[test]
fn cache_trees_report_unhashed_paths() {
    let fx = fixture();
    let apps = fx.base.join("applications");
    fs::create_dir(&apps).unwrap();
    fx.watcher.watch_tree(&apps, 2, CacheKind::Apps).unwrap();
    fs::write(apps.join("foot.desktop"), "[Desktop Entry]").unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, apps.join("foot.desktop"));
    assert_eq!(b.changes[0].role, Role::Cache(CacheKind::Apps));
    assert_eq!(b.changes[0].hash, None);

    fs::create_dir(apps.join("kde")).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes[0].path, apps.join("kde"));
    fs::write(apps.join("kde/k.desktop"), "[Desktop Entry]").unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes[0].path, apps.join("kde/k.desktop"));

    fs::remove_file(apps.join("foot.desktop")).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes[0].kind, ChangeKind::Removed);
}

// --- Rescans and polling -------------------------------------------------------

#[test]
fn requested_rescan_is_marked() {
    let fx = fixture();
    fx.watcher.rescan();
    let b = one_batch(&fx);
    assert_eq!(b.rescan, Some(RescanReason::Requested));
    assert!(b.changes.is_empty(), "{b:#?}");
}

/// Network filesystems emit no events; polling compares content. Forced
/// here, since the test runs on a local filesystem.
#[test]
fn polling_compares_content() {
    let opts = Options {
        force_polling: true,
        poll_interval: Duration::from_millis(40),
        ..Options::default()
    };
    let tmp = tempfile::tempdir().unwrap();
    let cfg = fs::canonicalize(tmp.path()).unwrap().join("cfg");
    fs::create_dir(&cfg).unwrap();
    fs::write(cfg.join("bar.strand"), "bar {}\n").unwrap();
    let (sink, rx) = channel();
    let _w = Watcher::spawn(Some(config(&cfg)), opts, sink).unwrap();
    match rx.recv_timeout(FIRST) {
        Ok(ChangeEvent::Files(b)) => assert!(
            b.notices
                .iter()
                .any(|n| matches!(n, Notice::Polling { dir, .. } if *dir == cfg)),
            "{b:#?}"
        ),
        other => panic!("expected the polling notice, got {other:?}"),
    }
    // Same length, same second: only the bytes differ.
    fs::write(cfg.join("bar.strand"), "baz {}\n").unwrap();
    let b = next_files(&rx, FIRST).expect("polled change");
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].hash, Some(hash_bytes(b"baz {}\n")));
    assert!(next_files(&rx, SETTLE).is_none());
    fs::write(cfg.join("new.strand"), "new").unwrap();
    let b = next_files(&rx, FIRST).expect("polled new file");
    assert_eq!(b.changes[0].kind, ChangeKind::Created);
}

// --- Directories that come and go ---------------------------------------------

/// Wait for a batch that satisfies `pred`, skipping others (a removal
/// batch, notices).
fn until(rx: &Receiver<ChangeEvent>, what: &str, pred: impl Fn(&FileBatch) -> bool) -> FileBatch {
    let end = Instant::now() + FIRST;
    while let Some(b) = next_files(rx, end.saturating_duration_since(Instant::now())) {
        if pred(&b) {
            return b;
        }
    }
    panic!("no batch with {what}");
}

fn has(b: &FileBatch, path: &Path, kind: ChangeKind) -> bool {
    b.changes.iter().any(|c| c.path == path && c.kind == kind)
}

/// A dotfile script replaces the whole config directory: the root's parent
/// is watched, so the recreated directory is watched again.
#[test]
fn a_recreated_config_directory_is_watched_again() {
    let fx = fixture();
    fs::remove_dir_all(&fx.cfg).unwrap();
    let bar = fx.cfg.join("bar.strand");
    until(&fx.rx, "the removal", |b| has(b, &bar, ChangeKind::Removed));
    fs::create_dir(&fx.cfg).unwrap();
    fs::write(&bar, "bar { new: 1 }\n").unwrap();
    until(&fx.rx, "the recreated module", |b| {
        has(b, &bar, ChangeKind::Created)
    });
    fs::write(&bar, "bar { new: 2 }\n").unwrap();
    let b = until(&fx.rx, "an edit", |b| has(b, &bar, ChangeKind::Modified));
    assert_modified(&b.changes[0], &bar, "bar { new: 2 }\n");
}

/// A settings file in a directory created later (`~/.local/state/strand`).
#[test]
fn a_file_whose_directory_is_created_later() {
    let fx = fixture();
    let prefs = fx.base.join("state/strand/prefs.toml");
    fx.watcher.watch_file(&prefs, Role::Settings).unwrap();
    fs::create_dir_all(prefs.parent().unwrap()).unwrap();
    fs::write(&prefs, "a = 1\n").unwrap();
    let b = until(&fx.rx, "the new file", |b| {
        has(b, &prefs, ChangeKind::Created)
    });
    assert_eq!(b.changes[0].hash, Some(hash_bytes(b"a = 1\n")));
    fs::write(&prefs, "a = 2\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &prefs, "a = 2\n");
}

/// GNU stow folding a new sub-directory in as a directory link, by `ln -s`
/// and by renaming a link in.
#[test]
fn a_new_directory_link_is_scanned() {
    let fx = fixture();
    for (i, how) in ["ln", "rename"].iter().enumerate() {
        let src = fx.base.join(format!("dotfiles/widgets{i}"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("osd.strand"), "osd {}\n").unwrap();
        no_batch(&fx);
        let link = fx.cfg.join(format!("widgets{i}"));
        if *how == "ln" {
            symlink(&src, &link).unwrap();
        } else {
            let tmp = fx.cfg.join(format!(".widgets{i}.tmp"));
            symlink(&src, &tmp).unwrap();
            fs::rename(&tmp, &link).unwrap();
        }
        let b = one_batch(&fx);
        assert_eq!(b.changes.len(), 1, "{how}: {b:#?}");
        assert_eq!(b.changes[0].path, link.join("osd.strand"));
        assert_eq!(b.changes[0].kind, ChangeKind::Created);
        assert_eq!(b.changes[0].canonical, Some(src.join("osd.strand")));
    }
}

/// `ln a.strand b.strand` makes only IN_CREATE: still a new module.
#[test]
fn a_hard_linked_module_is_seen() {
    let fx = fixture();
    let copy = fx.cfg.join("bar2.strand");
    fs::hard_link(fx.cfg.join("bar.strand"), &copy).unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, copy);
    assert_eq!(b.changes[0].kind, ChangeKind::Created);
}

/// `find_files` loads `.strand` files at most three directories down; the
/// watcher follows the set it returns.
#[test]
fn depth_three_is_watched_and_depth_four_is_not() {
    let fx = fixture();
    let c = fx.cfg.join("a/b/c");
    fs::create_dir_all(c.join("d")).unwrap();
    fs::write(c.join("x.strand"), "x {}\n").unwrap();
    fs::write(c.join("d/y.strand"), "y {}\n").unwrap();
    let b = one_batch(&fx);
    let paths: Vec<_> = b.changes.iter().map(|c| c.path.clone()).collect();
    assert_eq!(paths, vec![c.join("x.strand")]);
    assert_eq!(b.changes[0].kind, ChangeKind::Created);
    // The depth-3 directory is watched; the depth-4 one is not.
    fs::write(c.join("d/y.strand"), "y { b: 1 }\n").unwrap();
    no_batch(&fx);
    fs::write(c.join("x.strand"), "x { b: 1 }\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &c.join("x.strand"), "x { b: 1 }\n");
}

// --- Registrations -----------------------------------------------------------

/// Registrations are counted per (path, role); a path held for two roles
/// is reported once per role; a module registered as a file stays a
/// module.
#[test]
fn registrations_are_counted_per_role() {
    let fx = fixture();
    let wall = fx.base.join("wall.png");
    fs::write(&wall, "png 1").unwrap();
    fx.watcher.watch_file(&wall, Role::Wallpaper).unwrap();
    fx.watcher.watch_file(&wall, Role::Other).unwrap();
    fx.watcher.watch_file(&wall, Role::Other).unwrap();
    fx.watcher.unwatch_file(&wall, Role::Other).unwrap();
    fs::write(&wall, "png 2").unwrap();
    let b = one_batch(&fx);
    let roles: Vec<_> = b.changes.iter().map(|c| (c.path.clone(), c.role)).collect();
    assert_eq!(
        roles,
        vec![(wall.clone(), Role::Wallpaper), (wall.clone(), Role::Other)]
    );

    let theme = fx.cfg.join("theme.strand");
    fx.watcher.watch_file(&theme, Role::Settings).unwrap();
    fx.watcher.unwatch_file(&theme, Role::Settings).unwrap();
    fs::write(&theme, NEW).unwrap();
    only_theme_changed(&fx, NEW);

    // The whole set at once: only `wall.png` for the wallpaper remains.
    fx.watcher
        .set_referenced([(wall.clone(), Role::Wallpaper)])
        .unwrap();
    fs::write(&wall, "png 3").unwrap();
    let b = one_batch(&fx);
    let roles: Vec<_> = b.changes.iter().map(|c| c.role).collect();
    assert_eq!(roles, vec![Role::Wallpaper]);
    fx.watcher
        .set_referenced(Vec::<(PathBuf, Role)>::new())
        .unwrap();
    fs::write(&wall, "png 4").unwrap();
    no_batch(&fx);
}

/// A slider writes prefs.toml twice in one quiet period: the superseded
/// registration must not swallow a later user save with those bytes.
#[test]
fn superseded_own_writes_are_dropped() {
    let fx = fixture();
    let prefs = fx.cfg.join("prefs.toml");
    fs::write(&prefs, "v = 0\n").unwrap();
    fx.watcher.watch_file(&prefs, Role::Settings).unwrap();
    for v in ["v = 1\n", "v = 2\n"] {
        fx.watcher
            .register_own_write(&prefs, hash_bytes(v.as_bytes()));
        fs::write(&prefs, v).unwrap();
    }
    no_batch(&fx);
    // The user (an undo in an editor) writes the first value again.
    fs::write(&prefs, "v = 1\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &prefs, "v = 1\n");
}

/// A FIFO or device at a watched path is never read: the watcher (and
/// `watch_file`) must not block, and later saves still arrive.
#[test]
fn fifos_are_not_read() {
    let fx = fixture();
    let fifo = fx.base.join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("skipping: mkfifo unavailable");
        return;
    }
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            fx.watcher.watch_file(&fifo, Role::Settings).unwrap();
            let _ = done_tx.send(());
        });
        done.recv_timeout(FIRST)
            .expect("watch_file blocked on a FIFO");
    });

    // A module renamed over by a FIFO is reported as unreadable. (Once the
    // boot listing is done: a module-set listing that runs after the
    // rename drops it from the set instead, as `find_files` lists regular
    // files only.)
    std::thread::sleep(SETTLE);
    let theme = fx.cfg.join("theme.strand");
    let fifo2 = fx.base.join("fifo2");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo2)
        .status()
        .unwrap();
    assert!(status.success());
    fs::rename(&fifo2, &theme).unwrap();
    let b = until(&fx.rx, "the unreadable module", |b| {
        b.changes
            .iter()
            .any(|c| c.path == theme && c.error == Some(std::io::ErrorKind::InvalidInput))
    });
    assert!(b.changes.iter().all(|c| c.hash.is_none()), "{b:#?}");

    // The watcher is not wedged.
    fs::write(fx.cfg.join("bar.strand"), "bar { ok: 1 }\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &fx.cfg.join("bar.strand"), "bar { ok: 1 }\n");
}

// --- Directory trees moved and swapped ---------------------------------------

/// Take every batch until the watcher has been quiet for `SETTLE`.
fn drain(fx: &Fx) {
    while next_files(&fx.rx, SETTLE).is_some() {}
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_tree(&p, &to.join(e.file_name()));
        } else {
            fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

fn with_widgets(base: &Path) {
    fs::create_dir(base.join("cfg/widgets")).unwrap();
    fs::write(base.join("cfg/widgets/osd.strand"), "osd {}\n").unwrap();
}

/// A dotfile manager builds the new config beside the old one and swaps
/// them (`mv cfg cfg.bak; mv cfg.new cfg`): the swap reports the changed
/// file in the sub-directory, the new tree stays watched, and the old one
/// (now `cfg.bak`) is no longer reported under the old names.
#[test]
fn a_swapped_config_tree_keeps_nested_watches() {
    let fx = fixture_with(Options::default(), with_widgets);
    let osd = fx.cfg.join("widgets/osd.strand");
    let new = fx.base.join("cfg.new");
    copy_tree(&fx.cfg, &new);
    fs::write(new.join("widgets/osd.strand"), "osd { v: 2 }\n").unwrap();
    drain(&fx);
    fs::rename(&fx.cfg, fx.base.join("cfg.bak")).unwrap();
    fs::rename(&new, &fx.cfg).unwrap();
    let b = until(&fx.rx, "the nested change", |b| {
        has(b, &osd, ChangeKind::Modified)
    });
    let c = b.changes.iter().find(|c| c.path == osd).unwrap();
    assert_eq!(c.hash, Some(hash_bytes(b"osd { v: 2 }\n")));
    drain(&fx);

    // The old tree is not watched any more.
    fs::write(fx.base.join("cfg.bak/widgets/osd.strand"), "osd { old }\n").unwrap();
    fs::write(fx.base.join("cfg.bak/bar.strand"), "bar { old }\n").unwrap();
    no_batch(&fx);
    // The new one is, sub-directory included.
    fs::write(&osd, "osd { v: 3 }\n").unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &osd, "osd { v: 3 }\n");
}

/// `mv cfg cfg.bak; cp -r cfg.bak cfg`: the copy's sub-directory is
/// watched again.
#[test]
fn a_moved_and_copied_back_config_keeps_nested_watches() {
    let fx = fixture_with(Options::default(), with_widgets);
    let osd = fx.cfg.join("widgets/osd.strand");
    let bak = fx.base.join("cfg.bak");
    fs::rename(&fx.cfg, &bak).unwrap();
    copy_tree(&bak, &fx.cfg);
    drain(&fx);
    fs::write(&osd, "osd { v: 2 }\n").unwrap();
    let b = until(&fx.rx, "the nested edit", |b| {
        has(b, &osd, ChangeKind::Modified)
    });
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &osd, "osd { v: 2 }\n");
}

/// A sub-directory moved away and back (`mv cfg/a X; mv X cfg/a`): the
/// directories below it are watched again.
#[test]
fn a_subdirectory_moved_away_and_back() {
    let fx = fixture_with(Options::default(), |base| {
        fs::create_dir_all(base.join("cfg/a/b")).unwrap();
        fs::write(base.join("cfg/a/b/osd.strand"), "osd {}\n").unwrap();
    });
    let osd = fx.cfg.join("a/b/osd.strand");
    let away = fx.base.join("X");
    fs::rename(fx.cfg.join("a"), &away).unwrap();
    fs::rename(&away, fx.cfg.join("a")).unwrap();
    drain(&fx);
    fs::write(&osd, "osd { v: 2 }\n").unwrap();
    let b = until(&fx.rx, "the nested edit", |b| {
        has(b, &osd, ChangeKind::Modified)
    });
    assert_modified(&b.changes[0], &osd, "osd { v: 2 }\n");
    // And nothing is reported for the path it was moved through.
    fs::write(fx.cfg.join("a/b/osd.strand"), "osd { v: 3 }\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &osd, "osd { v: 3 }\n");
}

/// Watch, then list: a file written into a new sub-directory after the
/// module-set rescan listed it, but before its watch was added, made no
/// event; the second listing (after the watch exists) finds it.
#[test]
fn a_file_written_while_a_new_directory_is_being_watched() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let cfg = fs::canonicalize(tmp.path()).unwrap().join("cfg");
    fs::create_dir(&cfg).unwrap();
    fs::write(cfg.join("bar.strand"), "bar {}\n").unwrap();
    let osd = cfg.join("w/osd.strand");
    let wrote = Arc::new(AtomicBool::new(false));
    let (r, o, w) = (cfg.clone(), osd.clone(), wrote.clone());
    let config = ConfigWatch {
        root: cfg.clone(),
        modules: modules(&cfg).unwrap(),
        rescan: Box::new(move || {
            let set = modules(&r)?;
            // The slow `cp -r`: the file lands right after the listing.
            if r.join("w").is_dir() && !w.swap(true, Ordering::SeqCst) {
                fs::write(&o, "osd {}\n")?;
            }
            Ok(set)
        }),
    };
    let (sink, rx) = channel();
    let _watcher = Watcher::spawn(Some(config), Options::default(), sink).unwrap();
    fs::create_dir(cfg.join("w")).unwrap();
    let b = until(&rx, "the file written in the window", |b| {
        has(b, &osd, ChangeKind::Created)
    });
    assert!(wrote.load(Ordering::SeqCst));
    let c = b.changes.iter().find(|c| c.path == osd).unwrap();
    assert_eq!(c.hash, Some(hash_bytes(b"osd {}\n")));
}

// --- Registration races -------------------------------------------------------

/// Watch, then list, at boot too: a module (and a directory of modules)
/// created after the caller listed the config but before the watches
/// existed is reported once, as `Created`.
#[test]
fn a_module_created_before_the_watch_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = fs::canonicalize(tmp.path()).unwrap().join("cfg");
    fs::create_dir(&cfg).unwrap();
    fs::write(cfg.join("bar.strand"), "bar {}\n").unwrap();
    let listed = config(&cfg);
    fs::write(cfg.join("late.strand"), "late {}\n").unwrap();
    fs::create_dir(cfg.join("widgets")).unwrap();
    fs::write(cfg.join("widgets/osd.strand"), "osd {}\n").unwrap();
    let (sink, rx) = channel();
    let _w = Watcher::spawn(Some(listed), Options::default(), sink).unwrap();
    let b = next_files(&rx, FIRST).expect("no batch");
    let got: Vec<_> = b
        .changes
        .iter()
        .map(|c| (c.path.clone(), c.kind, c.role))
        .collect();
    assert_eq!(
        got,
        vec![
            (cfg.join("late.strand"), ChangeKind::Created, Role::Module),
            (
                cfg.join("widgets/osd.strand"),
                ChangeKind::Created,
                Role::Module
            ),
        ]
    );
    assert_eq!(b.changes[0].hash, Some(hash_bytes(b"late {}\n")));
    if let Some(extra) = next_files(&rx, SETTLE) {
        panic!("a second batch: {extra:#?}");
    }
    // The new directory is watched.
    fs::write(cfg.join("widgets/osd.strand"), "osd { a: 1 }\n").unwrap();
    let b = next_files(&rx, FIRST).expect("the nested edit");
    assert_eq!(b.changes.len(), 1, "{b:#?}");
}

/// The loader read a referenced file, a save landed, then it registered
/// the path with the hash of what it read: the save is reported.
#[test]
fn a_save_between_read_and_registration_is_reported() {
    let fx = fixture();
    let prefs = fx.cfg.join("prefs.toml");
    fs::write(&prefs, "a = 1\n").unwrap();
    let read = hash_bytes(&fs::read(&prefs).unwrap());
    fs::write(&prefs, "a = 2\n").unwrap();
    fx.watcher
        .set_referenced([(prefs.clone(), Role::Settings, read)])
        .unwrap();
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_modified(&b.changes[0], &prefs, "a = 2\n");

    // Register, then read: nothing to report, and the next save is seen.
    let wall = fx.cfg.join("wall.png");
    fs::write(&wall, "png 1").unwrap();
    fx.watcher.watch_file(&wall, Role::Wallpaper).unwrap();
    let _loaded = fs::read(&wall).unwrap();
    no_batch(&fx);
    fs::write(&wall, "png 2").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &wall, "png 2");
}

/// `register_own_write` is in place when it returns, whatever the watcher
/// thread is doing: a tight loop of registered atomic writes reports
/// nothing, even with flushes running concurrently.
#[test]
fn own_writes_in_a_tight_loop_are_never_reported() {
    let fx = fixture();
    let prefs = fx.cfg.join("prefs.toml");
    fs::write(&prefs, "v = 0\n").unwrap();
    fx.watcher.watch_file(&prefs, Role::Settings).unwrap();
    let tmp = fx.cfg.join(".prefs.toml.tmp");
    let end = Instant::now() + Duration::from_millis(1500);
    let mut writes = 0u32;
    while Instant::now() < end {
        writes += 1;
        let body = format!("v = {writes}\n");
        fx.watcher
            .register_own_write(&prefs, hash_bytes(body.as_bytes()));
        fs::write(&tmp, &body).unwrap();
        fs::rename(&tmp, &prefs).unwrap();
    }
    assert!(writes > 100, "{writes}");
    no_batch(&fx);
}

/// A file made complete through `O_TMPFILE` and linked in (`linkat`, as
/// systemd's `link_tmpfile` does) is reported: its creation already has
/// bytes, and its `CLOSE_WRITE` comes under the unnamed `#<ino>`.
#[test]
fn a_file_linked_in_from_o_tmpfile_is_reported() {
    use rustix::fs::{AtFlags, CWD, Mode, OFlags};
    let fx = fixture();
    let prefs = fx.cfg.join("prefs.toml");
    fx.watcher.watch_file(&prefs, Role::Settings).unwrap();
    let fd = match rustix::fs::openat(
        CWD,
        &fx.cfg,
        OFlags::TMPFILE | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("skipping: O_TMPFILE unsupported here ({e})");
            return;
        }
    };
    rustix::io::write(&fd, b"a = 1\n").unwrap();
    let proc_path = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd));
    rustix::fs::linkat(
        CWD,
        proc_path.as_str(),
        CWD,
        &prefs,
        AtFlags::SYMLINK_FOLLOW,
    )
    .unwrap();
    drop(fd);
    let b = one_batch(&fx);
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    assert_eq!(b.changes[0].path, prefs);
    assert_eq!(b.changes[0].kind, ChangeKind::Created);
    assert_eq!(b.changes[0].hash, Some(hash_bytes(b"a = 1\n")));
}

/// An ancestor of a watched directory moved while its own parent holds no
/// full watch: the light ancestor watch sees it, the file is reported
/// removed, and created again when the directory comes back.
#[test]
fn a_moved_ancestor_directory_is_seen() {
    let fx = fixture();
    let z = fx.base.join("x/y/z");
    fs::create_dir_all(&z).unwrap();
    let prefs = z.join("prefs.toml");
    fs::write(&prefs, "a = 1\n").unwrap();
    fx.watcher.watch_file(&prefs, Role::Settings).unwrap();
    // `x` is watched only as an ancestor of `x/y/z`.
    fs::rename(fx.base.join("x/y"), fx.base.join("x/q")).unwrap();
    let b = until(&fx.rx, "the removal", |b| {
        has(b, &prefs, ChangeKind::Removed)
    });
    assert_eq!(b.changes.len(), 1, "{b:#?}");
    // A write in the moved tree is not reported under the old name.
    fs::write(fx.base.join("x/q/z/prefs.toml"), "a = 2\n").unwrap();
    no_batch(&fx);
    fs::rename(fx.base.join("x/q"), fx.base.join("x/y")).unwrap();
    let b = until(&fx.rx, "the return", |b| {
        has(b, &prefs, ChangeKind::Created)
    });
    assert_eq!(b.changes[0].hash, Some(hash_bytes(b"a = 2\n")));
    fs::write(&prefs, "a = 3\n").unwrap();
    let b = one_batch(&fx);
    assert_modified(&b.changes[0], &prefs, "a = 3\n");
}

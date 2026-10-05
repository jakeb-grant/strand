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

/// The module set as `source::find_files` defines it: `.strand` files up to
/// three directories down, hidden names skipped, links followed.
fn modules(root: &Path) -> std::io::Result<ModuleSet> {
    let mut set = ModuleSet::default();
    let mut queue = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        set.dirs.push(fs::canonicalize(&dir)?);
        for e in fs::read_dir(&dir)?.flatten() {
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                if depth < 3 {
                    queue.push((p, depth + 1));
                }
            } else if p.extension().is_some_and(|x| x == "strand") && p.exists() {
                set.files.push(p);
            }
        }
    }
    set.files.sort();
    Ok(set)
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

    // A swap to a target with the same bytes is a no-op.
    let v3 = fx.base.join("store/v3");
    fs::create_dir_all(&v3).unwrap();
    fs::write(v3.join("theme.strand"), "x").unwrap();
    symlink(v3.join("theme.strand"), &tmp).unwrap();
    fs::rename(&tmp, fx.cfg.join("theme.strand")).unwrap();
    no_batch(&fx);
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

    fx.watcher.unwatch_file(&prefs).unwrap();
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
    let cfg = fs::canonicalize(tmp.path()).unwrap();
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

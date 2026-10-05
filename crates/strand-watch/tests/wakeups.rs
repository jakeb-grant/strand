//! The idle-wakeup budget (design.md: "an idle shell does zero work"):
//! programs writing next to watched files (a download in the wallpaper's
//! directory, `.xsession-errors` in `~` where a home-manager link lives, a
//! font being copied into a font tree) wake the watcher thread once per
//! file created or closed, never once per `write(2)`, and never hold back
//! a config batch.
//!
//! Alone in its test binary: it counts the context switches of the one
//! `strand-watch` thread in the process.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use strand_watch::{
    CacheKind, ChangeEvent, ConfigWatch, FileBatch, ModuleSet, Options, Role, Watcher, channel,
    hash_bytes,
};

fn modules(root: &Path) -> std::io::Result<ModuleSet> {
    let found = strand_compiler::source::find_files(root)?;
    Ok(ModuleSet {
        files: found.files,
        dirs: found.dirs,
    })
}

/// The watcher thread's id, found by its name.
fn watcher_tid() -> PathBuf {
    let mut found = Vec::new();
    for task in fs::read_dir("/proc/self/task").unwrap() {
        let task = task.unwrap().path();
        if fs::read_to_string(task.join("comm")).is_ok_and(|c| c.trim() == "strand-watch") {
            found.push(task);
        }
    }
    assert_eq!(found.len(), 1, "one watcher thread: {found:?}");
    found.remove(0)
}

/// Times the thread blocked and was woken again.
fn wakeups(task: &Path) -> u64 {
    let status = fs::read_to_string(task.join("status")).unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn drain(rx: &Receiver<ChangeEvent>) {
    while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
}

/// Many small writes, each its own `write(2)`, spread over time so that
/// the kernel could not merge them into one event.
fn trickle(path: &Path, writes: usize) {
    let mut f = fs::File::create(path).unwrap();
    for _ in 0..writes {
        f.write_all(&[b'x'; 512]).unwrap();
        std::thread::sleep(Duration::from_micros(100));
    }
}

#[test]
fn writers_next_to_watched_files_do_not_wake_the_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let cfg = home.join("cfg");
    let pics = home.join("pics");
    let fonts = home.join("fonts");
    for d in [&cfg, &pics, &fonts] {
        fs::create_dir(d).unwrap();
    }
    fs::write(
        cfg.join("theme.strand"),
        "tokens base { accent: #7aa2f7 }\n",
    )
    .unwrap();
    fs::write(pics.join("wall.png"), "png").unwrap();
    // home-manager's `~/.background-image`: a link in `~`.
    let link = home.join(".background-image");
    std::os::unix::fs::symlink(pics.join("wall.png"), &link).unwrap();

    let (sink, rx) = channel();
    let r = cfg.clone();
    let config = ConfigWatch {
        root: cfg.clone(),
        modules: modules(&cfg).unwrap(),
        rescan: Box::new(move || modules(&r)),
    };
    let watcher = Watcher::spawn(Some(config), Options::default(), sink).unwrap();
    watcher.watch_file(&link, Role::Wallpaper).unwrap();
    watcher.watch_tree(&fonts, 1, CacheKind::Fonts).unwrap();
    drain(&rx);

    let task = watcher_tid();
    let before = wakeups(&task);
    let writes = 1500;
    let writers = [
        home.join(".xsession-errors"),
        pics.join("download.part"),
        fonts.join("new.ttf"),
    ]
    .map(|p| std::thread::spawn(move || trickle(&p, writes)));

    // A config save while they write: cut 15 ms after it, as ever.
    std::thread::sleep(Duration::from_millis(50));
    let theme = cfg.join("theme.strand");
    let new = "tokens base { accent: #ff9e64 }\n";
    fs::write(&theme, new).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let batch: FileBatch = loop {
        match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(ChangeEvent::Files(b)) if b.changes.iter().any(|c| c.path == theme) => break b,
            Ok(_) => continue,
            Err(e) => panic!("no config batch: {e}"),
        }
    };
    let theme_change = batch.changes.iter().find(|c| c.path == theme).unwrap();
    assert_eq!(theme_change.hash, Some(hash_bytes(new.as_bytes())));
    assert!(
        batch.last_event - batch.first_event < Duration::from_millis(50),
        "{batch:#?}"
    );

    for w in writers {
        w.join().unwrap();
    }
    drain(&rx);
    let woken = wakeups(&task) - before;
    // Per file: its creation and its close, the config save, the font
    // tree's batches and their quiet periods: a handful. One per write
    // would be thousands.
    assert!(woken < 60, "{woken} wakeups for {} writes", 3 * writes);
    drop(watcher);
}

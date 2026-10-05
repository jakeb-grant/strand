//! `state x = … persist`: values stored with a hash of their default under
//! `$XDG_STATE_HOME/strand`, atomic writes, corrupt files fall back to the
//! default with a warning.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_core::persist::{PERSIST_DEBOUNCE, value_hash};
use strand_core::{
    Diagnostic, PersistError, PersistStore, PersistValue, Redeclared, Restore, Runtime,
};

/// A fresh directory, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "strand-persist-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn store(&self) -> PersistStore {
        PersistStore::new(self.0.join("persist"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Boot a runtime with one persisted `i64` (`default`), apply `writes`,
/// let the debounce pass and shut down. Returns the restore decision, the
/// final value and the diagnostics.
fn session(store: &PersistStore, default: i64, writes: &[i64]) -> (Restore, i64, Vec<Diagnostic>) {
    let rt = Runtime::new();
    let p = rt.persisted_value(store, "osd.level", default);
    let mut diags = rt.take_diagnostics();
    let mut t = Duration::ZERO;
    rt.flush();
    for &w in writes {
        p.signal.set(&rt, w).unwrap();
        t += Duration::from_millis(10);
        diags.extend(rt.tick(t).diagnostics);
    }
    diags.extend(rt.tick(t + PERSIST_DEBOUNCE).diagnostics);
    let value = p.signal.get(&rt).unwrap();
    rt.shutdown();
    diags.extend(rt.take_diagnostics());
    (p.restored, value, diags)
}

#[test]
fn a_value_survives_a_restart() {
    let tmp = TempDir::new("restart");
    let store = tmp.store();
    let (restored, value, diags) = session(&store, 40, &[41, 55]);
    assert_eq!(restored, Restore::Default);
    assert_eq!(value, 55);
    assert!(diags.is_empty(), "{diags:?}");
    let (restored, value, diags) = session(&store, 40, &[]);
    assert_eq!(restored, Restore::Stored(b"55".to_vec()));
    assert_eq!(value, 55);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(files(store.dir()), vec!["osd.level"], "no temp files left");
}

#[test]
fn the_file_records_the_hash_of_the_default() {
    let tmp = TempDir::new("hash");
    let store = tmp.store();
    session(&store, 40, &[7]);
    let stored = store.load("osd.level").unwrap().unwrap();
    assert_eq!(stored.value, b"7");
    assert_eq!(stored.default_hash, value_hash(&40i64.encode()));
    let text = fs::read_to_string(store.dir().join("osd.level")).unwrap();
    assert!(text.starts_with("strand-persist 1\ndefault "), "{text}");
}

#[test]
fn an_unchanged_value_adopts_a_new_default() {
    let tmp = TempDir::new("adopt");
    let store = tmp.store();
    // Changed and changed back: the file holds the old default.
    session(&store, 40, &[41]);
    session(&store, 40, &[40]);
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"40");
    let (restored, value, diags) = session(&store, 60, &[]);
    assert_eq!(restored, Restore::Adopted);
    assert_eq!(value, 60);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(store.load("osd.level"), Ok(None), "the stale file is gone");
}

#[test]
fn a_changed_value_is_kept_over_a_new_default_and_reported_once() {
    let tmp = TempDir::new("kept");
    let store = tmp.store();
    session(&store, 40, &[7]);
    let (restored, value, diags) = session(&store, 60, &[]);
    assert_eq!(restored, Restore::KeptOverNewDefault(b"7".to_vec()));
    assert_eq!(value, 7);
    assert!(
        matches!(&diags[..], [Diagnostic::PersistDefaultChanged { path, .. }] if &**path == "osd.level"),
        "{diags:?}"
    );
    let (restored, value, diags) = session(&store, 60, &[]);
    assert_eq!(restored, Restore::Stored(b"7".to_vec()));
    assert_eq!(value, 7);
    assert!(diags.is_empty(), "noticed once: {diags:?}");
}

#[test]
fn a_corrupt_file_gives_the_default_and_a_warning() {
    let tmp = TempDir::new("corrupt");
    let store = tmp.store();
    session(&store, 40, &[7]);
    let file = store.dir().join("osd.level");
    // A flipped byte in the value.
    let mut bytes = fs::read(&file).unwrap();
    let last = bytes.len() - 1;
    bytes[last] = b'8';
    fs::write(&file, &bytes).unwrap();
    let (restored, value, diags) = session(&store, 40, &[]);
    assert!(
        matches!(&restored, Restore::Failed(PersistError::Corrupt { reason, .. }) if &**reason == "checksum mismatch"),
        "{restored:?}"
    );
    assert_eq!(value, 40);
    assert!(
        matches!(
            &diags[..],
            [Diagnostic::PersistFailed {
                error: PersistError::Corrupt { .. },
                ..
            }]
        ),
        "{diags:?}"
    );
    assert_eq!(
        files(store.dir()),
        vec!["osd.level.corrupt"],
        "moved aside for inspection"
    );
    // Garbage, truncation and a foreign file are corrupt too.
    for junk in [
        &b"\x00\x01\x02"[..],
        b"strand-persist 1\ndefault",
        b"hello\n",
    ] {
        fs::write(&file, junk).unwrap();
        let (restored, value, _) = session(&store, 40, &[]);
        assert!(matches!(restored, Restore::Failed(_)), "{restored:?}");
        assert_eq!(value, 40);
    }
    // The next write repairs it.
    session(&store, 40, &[9]);
    assert_eq!(session(&store, 40, &[]).1, 9);
}

#[test]
fn a_value_that_no_longer_decodes_starts_from_the_default() {
    let tmp = TempDir::new("retyped");
    let store = tmp.store();
    // `state level = "loud" persist` became `state level = 40 persist`.
    store.save("osd.level", b"40", b"loud").unwrap();
    let (restored, value, diags) = session(&store, 40, &[]);
    assert!(matches!(restored, Restore::Failed(_)), "{restored:?}");
    assert_eq!(value, 40);
    assert!(
        matches!(&diags[..], [Diagnostic::PersistFailed { .. }]),
        "{diags:?}"
    );
    // Moved aside like a corrupt file, so the warning is not repeated on
    // every start (even while the value stays at its default).
    assert_eq!(files(store.dir()), vec!["osd.level.corrupt"]);
    let (restored, value, diags) = session(&store, 40, &[]);
    assert_eq!(restored, Restore::Default);
    assert_eq!(value, 40);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn writes_are_debounced_and_flushed_on_unmount() {
    let tmp = TempDir::new("debounce");
    let store = tmp.store();
    let rt = Runtime::new();
    let (scope, p) = rt.scope(|rt| rt.persisted_value(&store, "launcher.query", String::new()));
    rt.flush();
    let mut t = Duration::ZERO;
    // Typing: a change every 50 ms never leaves 250 ms of quiet.
    for q in ["f", "fi", "fir", "fire"] {
        p.signal.set(&rt, q.to_string()).unwrap();
        t += Duration::from_millis(50);
        rt.tick(t);
        assert_eq!(store.load("launcher.query"), Ok(None), "not yet");
    }
    rt.tick(t + PERSIST_DEBOUNCE);
    assert_eq!(
        store.load("launcher.query").unwrap().unwrap().value,
        b"fire"
    );
    // A change still in its quiet period is written when the component
    // unmounts.
    p.signal.set(&rt, "firef".to_string()).unwrap();
    rt.tick(t + PERSIST_DEBOUNCE + Duration::from_millis(10));
    scope.dispose(&rt);
    assert_eq!(
        store.load("launcher.query").unwrap().unwrap().value,
        b"firef"
    );
    assert_eq!(rt.next_deadline(), None, "nothing left scheduled");
}

#[test]
fn a_failed_write_is_a_warning_and_the_value_stays() {
    let tmp = TempDir::new("unwritable");
    // The store directory is a file: every write fails.
    let blocked = tmp.0.join("blocked");
    fs::write(&blocked, b"").unwrap();
    let store = PersistStore::new(&blocked);
    let (restored, value, diags) = session(&store, 1, &[2]);
    assert!(
        matches!(restored, Restore::Failed(PersistError::Io { .. })),
        "{restored:?}"
    );
    assert_eq!(value, 2);
    assert!(
        diags.iter().any(|d| matches!(
            d,
            Diagnostic::PersistFailed {
                error: PersistError::Io { .. },
                ..
            }
        )),
        "{diags:?}"
    );
}

#[test]
fn the_store_lives_under_xdg_state_home() {
    let s = PersistStore::from_vars(
        Some(OsString::from("/x/state")),
        Some(OsString::from("/home/u")),
    )
    .unwrap();
    assert_eq!(s.dir(), Path::new("/x/state/strand/persist"));
    // A relative XDG_STATE_HOME is ignored (XDG base directory spec).
    let s = PersistStore::from_vars(
        Some(OsString::from("state")),
        Some(OsString::from("/home/u")),
    )
    .unwrap();
    assert_eq!(s.dir(), Path::new("/home/u/.local/state/strand/persist"));
    assert_eq!(
        PersistStore::from_vars(None, None),
        Err(PersistError::NoStateDir)
    );
}

#[test]
fn reset_forgets_the_stored_value() {
    let tmp = TempDir::new("reset");
    let store = tmp.store();
    session(&store, 40, &[7]);
    store.remove("osd.level").unwrap();
    store.remove("osd.level").unwrap();
    assert_eq!(session(&store, 40, &[]).0, Restore::Default);
}

#[test]
fn a_default_changed_by_a_live_reload_follows_the_state_default_rule() {
    let tmp = TempDir::new("redeclare");
    let store = tmp.store();
    // Unchanged value: takes the new default, and a restart agrees.
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    assert_eq!(p.redeclare(&rt, 40), Ok(Redeclared::Unchanged));
    assert_eq!(p.redeclare(&rt, 60), Ok(Redeclared::Adopted));
    assert_eq!(p.signal.get(&rt), Ok(60));
    assert!(rt.flush().diagnostics.is_empty());
    rt.shutdown();
    let (restored, value, diags) = session(&store, 60, &[]);
    assert_eq!(restored, Restore::Default);
    assert_eq!(value, 60);
    assert!(diags.is_empty(), "{diags:?}");
    // Changed value: kept, reported once, and re-stamped so a restart
    // with the new default neither adopts it nor reports it again.
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 60i64);
    rt.flush();
    p.signal.set(&rt, 7).unwrap();
    rt.tick(PERSIST_DEBOUNCE);
    assert_eq!(p.redeclare(&rt, 80), Ok(Redeclared::Kept));
    assert_eq!(p.signal.get(&rt), Ok(7));
    let diags = rt.flush().diagnostics;
    assert!(
        matches!(&diags[..], [Diagnostic::PersistDefaultChanged { path, .. }] if &**path == "osd.level"),
        "{diags:?}"
    );
    // Later writes carry the new default's hash.
    p.signal.set(&rt, 8).unwrap();
    rt.tick(2 * PERSIST_DEBOUNCE);
    rt.shutdown();
    let stored = store.load("osd.level").unwrap().unwrap();
    assert_eq!(stored.default_hash, value_hash(&80i64.encode()));
    let (restored, value, diags) = session(&store, 80, &[]);
    assert_eq!(restored, Restore::Stored(b"8".to_vec()));
    assert_eq!(value, 8);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn reset_cancels_a_pending_write() {
    let tmp = TempDir::new("reset-pending");
    let store = tmp.store();
    session(&store, 40, &[7]);
    // Changed again, still in its quiet period: `@reset` must not be
    // undone by the debounced write.
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    p.signal.set(&rt, 9).unwrap();
    rt.tick(Duration::from_millis(10));
    p.reset(&rt).unwrap();
    assert_eq!(p.signal.get(&rt), Ok(40));
    rt.tick(Duration::from_millis(10) + 2 * PERSIST_DEBOUNCE);
    rt.shutdown();
    assert!(files(store.dir()).is_empty(), "{:?}", files(store.dir()));
    assert_eq!(session(&store, 40, &[]).0, Restore::Default);
}

#[test]
fn reset_cancels_a_write_queued_behind_a_slow_disk() {
    let tmp = TempDir::new("reset-queued");
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let store = PersistStore::with_io_hook(tmp.0.join("persist"), move |_| {
        let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
    });
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    p.signal.set(&rt, 9).unwrap();
    rt.tick(PERSIST_DEBOUNCE);
    // The write of 9 is with the IO thread now, which is stuck.
    p.signal.set(&rt, 11).unwrap();
    rt.tick(2 * PERSIST_DEBOUNCE);
    p.reset(&rt).unwrap();
    assert_eq!(store.load("osd.level"), Ok(None), "the queued removal wins");
    for _ in 0..3 {
        let _ = release.send(());
    }
    rt.shutdown();
    assert!(files(store.dir()).is_empty(), "{:?}", files(store.dir()));
}

#[test]
fn a_slow_disk_does_not_stall_the_logic_tick() {
    let tmp = TempDir::new("slow");
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let store = PersistStore::with_io_hook(tmp.0.join("persist"), move |_| {
        // A disk that takes until the test lets it go (at most 10 s).
        let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
    });
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "launcher.query", String::new());
    rt.flush();
    p.signal.set(&rt, "fire".to_string()).unwrap();
    let start = std::time::Instant::now();
    rt.tick(Duration::from_millis(16));
    rt.tick(PERSIST_DEBOUNCE + Duration::from_millis(16));
    rt.tick(PERSIST_DEBOUNCE + Duration::from_millis(32));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the tick waited for the disk: {:?}",
        start.elapsed()
    );
    // Not on disk yet, but readable: the queue counts.
    assert_eq!(
        store.load("launcher.query").unwrap().unwrap().value,
        b"fire"
    );
    let _ = release.send(());
    // Shutdown waits for the queue, so the write is not lost at exit.
    rt.shutdown();
    let text = fs::read(store.dir().join("launcher.query")).unwrap();
    assert!(
        text.ends_with(b"\n\nfire"),
        "{:?}",
        String::from_utf8_lossy(&text)
    );
}

#[test]
fn a_failed_write_is_reported_in_a_later_tick() {
    let tmp = TempDir::new("late-failure");
    let blocked = tmp.0.join("blocked");
    fs::write(&blocked, b"").unwrap();
    let store = PersistStore::new(&blocked);
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 1i64);
    rt.take_diagnostics();
    let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let w = woke.clone();
    rt.set_wake_hook(move || w.store(true, std::sync::atomic::Ordering::SeqCst));
    rt.flush();
    p.signal.set(&rt, 2).unwrap();
    rt.tick(Duration::from_millis(10));
    rt.tick(Duration::from_millis(10) + PERSIST_DEBOUNCE);
    assert!(store.sync(Duration::from_secs(5)));
    assert!(
        woke.load(std::sync::atomic::Ordering::SeqCst),
        "the host is woken"
    );
    let diags = rt.flush().diagnostics;
    assert!(
        matches!(
            &diags[..],
            [Diagnostic::PersistFailed { cell, error: PersistError::Io { .. }, .. }] if *cell == p.signal.id()
        ),
        "{diags:?}"
    );
}

#[test]
fn two_live_cells_on_one_path_are_reported() {
    // A `bar` on every monitor with the same path: the second instance is
    // reported and never overwrites the first one's file.
    let tmp = TempDir::new("shared-path");
    let store = tmp.store();
    let rt = Runtime::new();
    let (left, a) = rt.scope(|rt| rt.persisted_value(&store, "bar.expanded", false));
    let b = rt.persisted_value(&store, "bar.expanded", false);
    let diags = rt.take_diagnostics();
    assert!(
        matches!(
            &diags[..],
            [Diagnostic::PersistPathInUse { cell, other, .. }]
                if *cell == b.signal.id() && *other == a.signal.id()
        ),
        "{diags:?}"
    );
    rt.flush();
    a.signal.set(&rt, true).unwrap();
    rt.tick(PERSIST_DEBOUNCE);
    b.signal.set(&rt, false).unwrap();
    rt.tick(3 * PERSIST_DEBOUNCE);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("bar.expanded").unwrap().unwrap().value, b"true");
    // With the instance identity in the path they are separate.
    let c = rt.persisted_value(&store, "bar[Dell U2720Q].expanded", false);
    assert!(rt.take_diagnostics().is_empty());
    // Once the owner unmounts, the path is free again.
    left.dispose(&rt);
    rt.persisted_value(&store, "bar.expanded", false);
    assert!(rt.take_diagnostics().is_empty());
    drop(c);
    rt.shutdown();
}

#[test]
fn a_pending_write_survives_dropping_the_runtime() {
    let tmp = TempDir::new("dropped");
    let store = tmp.store();
    {
        let rt = Runtime::new();
        let p = rt.persisted_value(&store, "osd.level", 40i64);
        rt.flush();
        p.signal.set(&rt, 3).unwrap();
        rt.tick(Duration::from_millis(10));
        drop(p);
        // No shutdown: the runtime just goes away.
    }
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"3");
}

#[test]
fn temp_files_left_by_a_crash_are_swept() {
    let tmp = TempDir::new("sweep");
    let store = tmp.store();
    fs::create_dir_all(store.dir()).unwrap();
    // A process that no longer exists (above any pid_max) and our own
    // (possibly mid-write).
    let dead = store.dir().join(".osd.level.tmp.999999999.0");
    let ours = store
        .dir()
        .join(format!(".osd.level.tmp.{}.12345", std::process::id()));
    fs::write(&dead, b"x").unwrap();
    fs::write(&ours, b"x").unwrap();
    session(&store, 40, &[5]);
    assert!(!dead.exists(), "a dead process's temp file is removed");
    assert!(ours.exists(), "ours is left alone");
}

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
    /// Declare it first in a test, so the runtimes and stores using it
    /// (and their IO threads) are gone before the directory is removed.
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
fn a_stored_value_that_equals_the_new_default_is_adopted_silently() {
    // Saved 60 under default 40; the default is now 60: the value *is* the
    // default, so nothing is "kept over" it (the same as a live reload).
    let tmp = TempDir::new("equals-new-default");
    let store = tmp.store();
    store.save("osd.level", b"40", b"60").unwrap();
    let (restored, value, diags) = session(&store, 60, &[]);
    assert_eq!(restored, Restore::Adopted);
    assert_eq!(value, 60);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(store.load("osd.level"), Ok(None), "the stale file is gone");
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
        vec![".osd.level.corrupt"],
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
    assert_eq!(files(store.dir()), vec![".osd.level.corrupt"]);
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
    // The disk is fast again: every later operation goes through at once,
    // so nothing is still queued when the directory is removed.
    drop(release);
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
    // The disk is fast again (every later operation too).
    drop(release);
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
    // The IO thread waits at a gate until the second tick has returned, so
    // the failure can only arrive after it (deterministic, whatever the
    // scheduler does).
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let g = gate.clone();
    let store = PersistStore::with_io_hook(&blocked, move |_| {
        let (open, cv) = &*g;
        let mut open = open.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    });
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 1i64);
    rt.take_diagnostics();
    let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let w = woke.clone();
    rt.set_wake_hook(move || w.store(true, std::sync::atomic::Ordering::SeqCst));
    rt.flush();
    p.signal.set(&rt, 2).unwrap();
    rt.tick(Duration::from_millis(10));
    let t2 = rt.tick(Duration::from_millis(10) + PERSIST_DEBOUNCE);
    assert!(
        !t2.diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::PersistFailed { .. })),
        "the write has not run yet: {:?}",
        t2.diagnostics
    );
    {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }
    assert!(store.sync(Duration::from_secs(5)));
    assert!(
        woke.load(std::sync::atomic::Ordering::SeqCst),
        "the host is woken"
    );
    assert!(!rt.is_idle(), "a host checking is_idle flushes for it");
    let diags = rt.flush().diagnostics;
    assert!(
        matches!(
            &diags[..],
            [Diagnostic::PersistFailed { cell, error: PersistError::Io { .. }, .. }] if *cell == p.signal.id()
        ),
        "{diags:?}"
    );
    assert!(rt.is_idle());
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
    // Once the owner unmounts, the waiting cell takes the path over; it was
    // never changed, so it continues from the owner's value.
    left.dispose(&rt);
    rt.flush();
    assert!(b.signal.get(&rt).unwrap());
    let (gone, d) = rt.scope(|rt| rt.persisted_value(&store, "bar.expanded", false));
    assert!(
        matches!(
            &rt.take_diagnostics()[..],
            [Diagnostic::PersistPathInUse { other, .. }] if *other == b.signal.id()
        ),
        "the promoted cell owns the path"
    );
    gone.dispose(&rt);
    drop((c, d));
    rt.shutdown();
}

#[test]
fn a_cell_waiting_for_its_path_takes_over_when_the_owner_goes() {
    // A reconcile mounts the replacement before disposing the old instance
    // (surface recreate, monitor replug, a list item re-created under its
    // key): the replacement's later changes must still be saved.
    let tmp = TempDir::new("promote");
    let store = tmp.store();
    let rt = Runtime::new();
    let (old, a) = rt.scope(|rt| rt.persisted_value(&store, "bar.level", 0i64));
    let b = rt.persisted_value(&store, "bar.level", 0i64);
    rt.take_diagnostics();
    rt.flush();
    // The old instance changes, and goes before its debounce ran out: its
    // last value is flushed, then b takes over.
    a.signal.set(&rt, 5).unwrap();
    rt.tick(Duration::from_millis(10));
    b.signal.set(&rt, 7).unwrap();
    rt.tick(Duration::from_millis(20));
    old.dispose(&rt);
    let t = rt.tick(Duration::from_millis(30));
    assert!(t.diagnostics.is_empty(), "{:?}", t.diagnostics);
    // b was changed while it waited: its value wins.
    assert_eq!(b.signal.get(&rt).unwrap(), 7);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("bar.level").unwrap().unwrap().value, b"7");
    // And it keeps saving.
    b.signal.set(&rt, 42).unwrap();
    rt.tick(Duration::from_millis(40));
    rt.tick(Duration::from_millis(40) + PERSIST_DEBOUNCE);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("bar.level").unwrap().unwrap().value, b"42");
    rt.shutdown();
}

#[test]
fn an_untouched_replacement_continues_from_the_old_instance() {
    let tmp = TempDir::new("promote-adopt");
    let store = tmp.store();
    let rt = Runtime::new();
    let (old, a) = rt.scope(|rt| rt.persisted_value(&store, "bar.level", 0i64));
    rt.flush();
    a.signal.set(&rt, 3).unwrap();
    rt.tick(Duration::from_millis(10));
    // Mounted while a's write is still pending: it starts from the disk.
    let (_new, b) = rt.scope(|rt| rt.persisted_value(&store, "bar.level", 0i64));
    assert_eq!(b.signal.get(&rt).unwrap(), 0);
    rt.take_diagnostics();
    old.dispose(&rt);
    rt.flush();
    assert_eq!(
        b.signal.get(&rt).unwrap(),
        3,
        "the old instance's last value"
    );
    b.signal.set(&rt, 9).unwrap();
    rt.tick(Duration::from_millis(20) + PERSIST_DEBOUNCE);
    rt.shutdown();
    assert_eq!(store.load("bar.level").unwrap().unwrap().value, b"9");
}

#[test]
fn a_path_never_reads_another_paths_quarantined_file() {
    let tmp = TempDir::new("quarantine-name");
    let store = tmp.store();
    store.save("x", b"0", b"hello").unwrap();
    let rt = Runtime::new();
    // "hello" is not an i64: moved aside.
    let x = rt.persisted_value(&store, "x", 0i64);
    assert!(matches!(x.restored, Restore::Failed(_)));
    assert!(store.sync(Duration::from_secs(5)));
    let other = rt.persisted_value(&store, "x.corrupt", String::from("default"));
    assert_eq!(other.restored, Restore::Default);
    assert_eq!(other.signal.get(&rt).unwrap(), "default");
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

#[test]
fn a_write_then_unmount_in_the_same_tick_is_saved() {
    let tmp = TempDir::new("unmount-same-tick");
    let store = tmp.store();
    let rt = Runtime::new();
    let (scope, p) = rt.scope(|rt| rt.persisted_value(&store, "osd.level", 40i64));
    rt.flush();
    // No flush between the write and the unmount: the tracking effect
    // never saw 5.
    p.signal.set(&rt, 5).unwrap();
    scope.dispose(&rt);
    rt.flush();
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"5");
    rt.shutdown();
}

#[test]
fn a_write_then_shutdown_without_a_flush_is_saved() {
    let tmp = TempDir::new("shutdown-no-flush");
    let store = tmp.store();
    // A root-level cell: it is disposed before root cleanups run.
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    p.signal.set(&rt, 6).unwrap();
    rt.shutdown();
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"6");
    // And one inside a component scope.
    let rt = Runtime::new();
    let (_scope, p) = rt.scope(|rt| rt.persisted_value(&store, "osd.level", 40i64));
    rt.flush();
    p.signal.set(&rt, 8).unwrap();
    rt.shutdown();
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"8");
}

#[test]
fn a_write_then_drop_without_a_flush_is_saved() {
    let tmp = TempDir::new("drop-no-flush");
    let store = tmp.store();
    {
        let rt = Runtime::new();
        let p = rt.persisted_value(&store, "osd.level", 40i64);
        rt.flush();
        p.signal.set(&rt, 9).unwrap();
    }
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"9");
}

#[test]
fn a_write_in_the_tick_that_closes_its_owner_is_saved() {
    // `if open { state level = 40 persist }` and a handler doing
    // `level = 5; open = false`: the `if` re-runs first and disposes the
    // branch before the persist effect inside sees 5.
    use std::cell::RefCell;
    use std::rc::Rc;
    let tmp = TempDir::new("owner-closes");
    let store = tmp.store();
    let rt = Runtime::new();
    let open = rt.signal(true);
    let cell = Rc::new(RefCell::new(None));
    let slot = cell.clone();
    let st = store.clone();
    rt.effect(move |rt| {
        if open.get(rt)? {
            *slot.borrow_mut() = Some(rt.persisted_value(&st, "osd.level", 40i64).signal);
        } else {
            *slot.borrow_mut() = None;
        }
        Ok(())
    });
    rt.flush();
    let level = cell.borrow().unwrap();
    level.set(&rt, 5).unwrap();
    open.set(&rt, false).unwrap();
    let mut t = Duration::from_millis(10);
    rt.tick(t);
    t += PERSIST_DEBOUNCE * 2;
    rt.tick(t);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"5");
    // Reopened: the branch starts from the saved value.
    open.set(&rt, true).unwrap();
    rt.tick(t + Duration::from_millis(10));
    let level = cell.borrow().unwrap();
    assert_eq!(level.get(&rt).unwrap(), 5);
    rt.shutdown();
}

#[test]
fn a_write_to_a_frozen_component_is_saved_when_it_is_replaced() {
    let tmp = TempDir::new("frozen-replaced");
    let store = tmp.store();
    let rt = Runtime::new();
    let (scope, p) = rt.scope(|rt| rt.persisted_value(&store, "osd.level", 40i64));
    rt.flush();
    rt.suspend(scope.id()).unwrap();
    // Its tracking effect is held while frozen.
    p.signal.set(&rt, 12).unwrap();
    let mut t = Duration::from_millis(10);
    rt.tick(t);
    t += PERSIST_DEBOUNCE * 2;
    rt.tick(t);
    scope.dispose(&rt);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"12");
    rt.shutdown();
}

#[test]
fn own_writes_and_removals_are_reported_to_the_observer() {
    use std::sync::{Arc, Mutex};
    let tmp = TempDir::new("observer");
    let store = tmp.store();
    type Seen = Vec<(PathBuf, Option<Vec<u8>>)>;
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let log = seen.clone();
    store.on_written(move |w| {
        assert_eq!(
            fs::canonicalize(w.path.parent().unwrap()).unwrap(),
            w.target.parent().unwrap()
        );
        log.lock()
            .unwrap()
            .push((w.path.to_path_buf(), w.content.map(<[u8]>::to_vec)));
    });
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    p.signal.set(&rt, 3).unwrap();
    rt.tick(Duration::from_millis(10));
    rt.tick(Duration::from_millis(10) + PERSIST_DEBOUNCE);
    assert!(store.sync(Duration::from_secs(5)));
    let file = store.file_of("osd.level").unwrap();
    let on_disk = fs::read(&file).unwrap();
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![(file.clone(), Some(on_disk))]
    );
    p.reset(&rt).unwrap();
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(seen.lock().unwrap().last().cloned(), Some((file, None)));
    rt.shutdown();
}

#[test]
fn a_failed_write_is_retried_and_restored_on_the_next_start() {
    let tmp = TempDir::new("retry");
    // The store's parent is a file: the first write fails (the store
    // directory cannot be created) until the file goes.
    let blocker = tmp.0.join("blocker");
    fs::write(&blocker, b"").unwrap();
    let store = PersistStore::new(blocker.join("persist"));
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    p.signal.set(&rt, 7).unwrap();
    rt.tick(Duration::from_millis(10));
    // The IO thread may fail the write before the tick that queued it
    // ends (then that tick reports it) or after.
    let mut diags = rt
        .tick(Duration::from_millis(10) + PERSIST_DEBOUNCE)
        .diagnostics;
    assert!(store.sync(Duration::from_secs(5)));
    diags.extend(
        rt.tick(Duration::from_millis(20) + PERSIST_DEBOUNCE)
            .diagnostics,
    );
    assert!(
        diags
            .iter()
            .any(|d| matches!(d, Diagnostic::PersistFailed { .. })),
        "{diags:?}"
    );
    // The disk recovers; the value is not changed again.
    fs::remove_file(&blocker).unwrap();
    rt.shutdown();
    drop(p);
    let (restored, value, diags) = session(&store, 40, &[]);
    assert_eq!(restored, Restore::Stored(b"7".to_vec()));
    assert_eq!(value, 7);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn save_runs_its_io_outside_the_store_lock() {
    use std::sync::{Arc, Mutex};
    let tmp = TempDir::new("save-lock");
    let store = tmp.store();
    let seen: Arc<Mutex<Vec<Option<i64>>>> = Arc::default();
    let (log, reader) = (seen.clone(), store.clone());
    // The observer runs on this thread, during `save`'s write: reading the
    // store from it used to deadlock on the queue lock.
    store.on_written(move |_| {
        let v = reader
            .load("other")
            .unwrap()
            .map(|s| i64::decode(&s.value).unwrap());
        log.lock().unwrap().push(v);
    });
    store.save("other", b"0", b"4").unwrap();
    store.save("x", b"0", b"1").unwrap();
    // The in-flight write counts for `load`, as a queued one does.
    assert_eq!(*seen.lock().unwrap(), vec![Some(4), Some(4)]);
    // `save` replaces what is queued for the file.
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "x", 0i64);
    p.signal.set(&rt, 2).unwrap();
    rt.shutdown();
    store.save("x", b"0", b"3").unwrap();
    assert_eq!(store.load("x").unwrap().unwrap().value, b"3");
}

#[test]
fn a_panicking_observer_is_removed_and_writes_go_on() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tmp = TempDir::new("observer-panic");
    let store = tmp.store();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    store.on_written(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        panic!("observer bug");
    });
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "osd.level", 40i64);
    rt.flush();
    let mut t = Duration::ZERO;
    let mut write = |v: i64| {
        p.signal.set(&rt, v).unwrap();
        t += Duration::from_millis(10);
        rt.tick(t);
        t += PERSIST_DEBOUNCE;
        rt.tick(t);
        assert!(store.sync(Duration::from_secs(5)), "the IO thread is alive");
        t += Duration::from_millis(10);
        rt.tick(t).diagnostics
    };
    let diags = write(5);
    assert!(
        diags
            .iter()
            .any(|d| matches!(d, Diagnostic::PersistFailed { .. })),
        "{diags:?}"
    );
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"5");
    write(6);
    assert_eq!(store.load("osd.level").unwrap().unwrap().value, b"6");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "not called again");
    rt.shutdown();
}

#[test]
fn removing_a_dangling_link_removes_the_link() {
    let tmp = TempDir::new("dangling");
    let store = tmp.store();
    fs::create_dir_all(store.dir()).unwrap();
    let file = store.file_of("x").unwrap();
    std::os::unix::fs::symlink(tmp.0.join("nowhere"), &file).unwrap();
    store.remove("x").unwrap();
    assert!(fs::symlink_metadata(&file).is_err(), "the link is gone");
}

#[test]
fn instance_files_no_cell_claimed_for_the_retention_period_are_swept_at_exit() {
    use std::time::SystemTime;
    use strand_core::persist::PERSIST_RETENTION;
    let tmp = TempDir::new("retention");
    let dir = tmp.0.join("persist");
    let age = |p: &Path, by: Duration| {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    };
    let mtime_age = |p: &Path| {
        fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
    };
    let store = PersistStore::new(&dir);
    let file = |path: &str| store.file_of(path).unwrap();
    let name = |path: &str| {
        file(path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    {
        let tool = PersistStore::new(&dir);
        for path in [
            "list[claimed].x",
            "list[released].x",
            "list[unclaimed].x",
            "list[recent].x",
        ] {
            tool.save(path, b"0", b"1").unwrap();
        }
    }
    let old = PERSIST_RETENTION + Duration::from_secs(3600);
    for path in ["list[claimed].x", "list[released].x", "list[unclaimed].x"] {
        age(&file(path), old);
    }
    let corrupt = dir.join(".gone.corrupt");
    fs::write(&corrupt, b"x").unwrap();
    age(&corrupt, old);
    // A corrupt file moved aside in this session is kept for inspection,
    // even if it was last written long ago.
    fs::write(file("list[bad].x"), b"not a strand file").unwrap();
    age(&file("list[bad].x"), old);
    {
        let store = PersistStore::new(&dir);
        let rt = Runtime::new();
        let _kept = rt.persisted_value(&store, "list[claimed].x", 0i64);
        let _bad = rt.persisted_value(&store, "list[bad].x", 0i64);
        let (scope, _released) =
            rt.scope(|rt| rt.persisted_value(&store, "list[released].x", 0i64));
        rt.flush();
        scope.dispose(&rt);
        rt.shutdown();
    }
    let mut left = files(&dir);
    left.sort();
    let mut want = vec![
        name("list[claimed].x"),
        name("list[recent].x"),
        name("list[released].x"),
        format!(".{}.corrupt", name("list[bad].x")),
    ];
    want.sort();
    assert_eq!(left, want);
    for path in ["list[claimed].x", "list[released].x"] {
        assert!(
            mtime_age(&file(path)) < Duration::from_secs(3600),
            "{path}: a claim refreshes the time"
        );
    }
    // A store no cell used (a tool) sweeps nothing.
    age(&file("list[recent].x"), old);
    drop(PersistStore::new(&dir));
    assert!(file("list[recent].x").exists());
}

#[test]
fn a_declared_cell_unmounted_for_the_retention_period_is_kept() {
    use std::time::SystemTime;
    use strand_core::persist::PERSIST_RETENTION;
    let tmp = TempDir::new("retention-plain");
    let store = tmp.store();
    // `if open { state level persist }`: set once, then not mounted for a
    // long time.
    store.save("popup.level", b"0", b"7").unwrap();
    let f = store.file_of("popup.level").unwrap();
    fs::File::options()
        .write(true)
        .open(&f)
        .unwrap()
        .set_modified(SystemTime::now() - PERSIST_RETENTION - Duration::from_secs(86_400))
        .unwrap();
    {
        // A session whose cells are all elsewhere sweeps at exit.
        let store = tmp.store();
        let rt = Runtime::new();
        let _other = rt.persisted_value(&store, "bar.dnd", false);
        rt.flush();
        rt.shutdown();
    }
    drop(store);
    let rt = Runtime::new();
    let p = rt.persisted_value(&tmp.store(), "popup.level", 0i64);
    assert_eq!(p.restored, Restore::Stored(b"7".to_vec()));
    assert_eq!(p.signal.get(&rt).unwrap(), 7);
}

#[test]
fn a_remount_behind_a_slow_disk_sees_the_write_in_flight() {
    let tmp = TempDir::new("remount-slow");
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = std::sync::Mutex::new(gate);
    let store = PersistStore::with_io_hook(tmp.0.join("persist"), move |_| {
        let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
    });
    let rt = Runtime::new();
    let mount = |rt: &Runtime| rt.scope(|rt| rt.persisted_value(&store, "bar.level", 0i64));
    let (a, pa) = mount(&rt);
    rt.flush();
    pa.signal.set(&rt, 5).unwrap();
    rt.flush();
    // A live reload: dispose and remount, twice, while the write of 5 is
    // stuck with the IO thread.
    a.dispose(&rt);
    let (b, pb) = mount(&rt);
    assert_eq!(pb.signal.get(&rt).unwrap(), 5);
    rt.flush();
    b.dispose(&rt);
    let (_c, pc) = mount(&rt);
    assert_eq!(pc.restored, Restore::Stored(b"5".to_vec()));
    assert_eq!(pc.signal.get(&rt).unwrap(), 5);
    // The disk is fast again (every later operation too).
    drop(release);
    rt.shutdown();
    let rt = Runtime::new();
    let p = rt.persisted_value(&tmp.store(), "bar.level", 0i64);
    assert_eq!(p.signal.get(&rt).unwrap(), 5);
}

#[test]
fn a_write_before_the_first_tracking_run_is_written_after_the_debounce() {
    let tmp = TempDir::new("mount-write");
    let store = tmp.store();
    let rt = Runtime::new();
    // `on mount { boots += 1 }`: written in the tick that creates it.
    let p = rt.persisted_value(&store, "shell.boots", 0i64);
    p.signal.set(&rt, 7).unwrap();
    rt.flush();
    rt.tick(PERSIST_DEBOUNCE + Duration::from_millis(16));
    assert!(store.sync(Duration::from_secs(5)));
    let text = fs::read(store.file_of("shell.boots").unwrap()).unwrap();
    assert!(
        text.ends_with(b"\n\n7"),
        "on disk without a shutdown: {:?}",
        String::from_utf8_lossy(&text)
    );
    rt.shutdown();
}

#[test]
fn redeclare_does_not_fire_on_change() {
    let tmp = TempDir::new("redeclare-on-change");
    let store = tmp.store();
    let rt = Runtime::new();
    let p = rt.persisted_value(&store, "bar.level", 5i64);
    let fired = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = fired.clone();
    let level = p.signal;
    rt.on_change(
        move |rt| level.get(rt),
        move |_, v| {
            seen.borrow_mut().push(*v);
            Ok(())
        },
    );
    let debounced = std::rc::Rc::new(std::cell::Cell::new(0));
    let d = debounced.clone();
    rt.on_change_after(
        move |rt| level.get(rt),
        Duration::from_millis(100),
        move |_| {
            d.set(d.get() + 1);
            Ok(())
        },
    );
    rt.flush();
    // A live reload: `= 5` became `= 7`; the value held the old default.
    assert_eq!(p.redeclare(&rt, 7), Ok(Redeclared::Adopted));
    rt.flush();
    assert_eq!(p.signal.get(&rt).unwrap(), 7);
    rt.tick(Duration::from_secs(1));
    assert!(fired.borrow().is_empty(), "{:?}", fired.borrow());
    assert_eq!(debounced.get(), 0, "no debounce restarted");
    // `@reset` at reload is a reload write too.
    p.signal.set(&rt, 9).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), vec![9], "a real change still fires");
    p.reset_reloaded(&rt).unwrap();
    rt.flush();
    assert_eq!(p.signal.get(&rt).unwrap(), 7);
    assert_eq!(*fired.borrow(), vec![9]);
    // The overlay's `[reset]` is the user's: an ordinary write.
    p.signal.set(&rt, 3).unwrap();
    rt.flush();
    p.reset(&rt).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), vec![9, 3, 7]);
    rt.shutdown();
}

#[test]
fn a_promoted_waiter_does_not_fire_on_change() {
    let tmp = TempDir::new("promote-on-change");
    let store = tmp.store();
    let rt = Runtime::new();
    let (old, p_old) = rt.scope(|rt| rt.persisted_value(&store, "bar.level", 0i64));
    rt.flush();
    p_old.signal.set(&rt, 5).unwrap();
    rt.flush();
    // The replacement is mounted before the old instance goes: it waits.
    let (_new, p_new) = rt.scope(|rt| rt.persisted_value(&store, "bar.level", 0i64));
    let fired = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = fired.clone();
    let level = p_new.signal;
    rt.on_change(
        move |rt| level.get(rt),
        move |_, v| {
            seen.borrow_mut().push(*v);
            Ok(())
        },
    );
    rt.flush();
    p_old.signal.set(&rt, 6).unwrap();
    rt.flush();
    old.dispose(&rt);
    rt.flush();
    assert_eq!(p_new.signal.get(&rt).unwrap(), 6, "continues from the old");
    assert!(fired.borrow().is_empty(), "{:?}", fired.borrow());
    p_new.signal.set(&rt, 8).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), vec![8]);
    rt.shutdown();
}

/// `state history: [Note] key id = [] persist`, the shape notification
/// history and launcher recents have.
type Note = (u32, String);

fn notes(items: &[(u32, &str)]) -> strand_core::KeyedVec<u32, Note> {
    strand_core::KeyedVec::from_values(
        |n: &Note| n.0,
        items.iter().map(|&(k, t)| (k, t.to_string())),
    )
    .unwrap()
}

fn encode_notes(v: &strand_core::KeyedVec<u32, Note>) -> Vec<u8> {
    v.items()
        .iter()
        .map(|(_, (k, t))| format!("{k}\t{t}\n"))
        .collect::<String>()
        .into_bytes()
}

fn decode_notes(b: &[u8]) -> Option<Vec<Note>> {
    std::str::from_utf8(b)
        .ok()?
        .lines()
        .map(|l| {
            let (k, t) = l.split_once('\t')?;
            Some((k.parse().ok()?, t.to_string()))
        })
        .collect()
}

fn history(
    rt: &Runtime,
    store: &PersistStore,
    default: &[(u32, &str)],
) -> strand_core::PersistedKeyed<u32, Note> {
    rt.persisted_keyed(
        store,
        "notifications.history",
        notes(default),
        encode_notes,
        decode_notes,
    )
}

#[test]
fn a_keyed_collection_persists_and_keeps_its_keys() {
    use strand_core::KeyedSource;
    let tmp = TempDir::new("keyed");
    let store = tmp.store();
    let rt = Runtime::new();
    let h = history(&rt, &store, &[]);
    assert_eq!(h.restored, Restore::Default);
    rt.flush();
    h.cell.push(&rt, (7, "low battery".into())).unwrap();
    h.cell.push(&rt, (9, "update ready".into())).unwrap();
    rt.tick(PERSIST_DEBOUNCE + Duration::from_millis(16));
    assert!(store.sync(Duration::from_secs(5)));
    rt.shutdown();

    let rt = Runtime::new();
    let h = history(&rt, &store, &[]);
    assert_eq!(
        h.restored,
        Restore::Stored(b"7\tlow battery\n9\tupdate ready\n".to_vec())
    );
    let snap = h.cell.snapshot(&rt).unwrap();
    assert_eq!(snap.keys().copied().collect::<Vec<_>>(), vec![7, 9]);
    // Removing one item is one keyed diff, written after the debounce.
    let v = snap.version();
    h.cell.remove_key(&rt, &7).unwrap();
    rt.tick(PERSIST_DEBOUNCE + Duration::from_millis(16));
    let diffs = h.cell.snapshot(&rt).unwrap().diffs_since(v).unwrap();
    assert!(
        matches!(diffs[..], [strand_core::VecDiff::Remove { key: 7, .. }]),
        "{diffs:?}"
    );
    rt.shutdown();
    let rt = Runtime::new();
    let h = history(&rt, &store, &[]);
    assert_eq!(
        h.cell.get_untracked(&rt).unwrap().items(),
        &[(9, (9, "update ready".to_string()))]
    );
    rt.shutdown();
    // Stored values with a repeated key do not decode: the default, with
    // a warning, the file moved aside.
    store
        .save(
            "notifications.history",
            &encode_notes(&notes(&[])),
            b"1\ta\n1\tb\n",
        )
        .unwrap();
    let rt = Runtime::new();
    let h = history(&rt, &store, &[]);
    assert!(matches!(
        h.restored,
        Restore::Failed(PersistError::Corrupt { .. })
    ));
    assert!(h.cell.get_untracked(&rt).unwrap().is_empty());
    rt.shutdown();
}

#[test]
fn keyed_redeclare_and_reload_reset_do_not_fire_on_change() {
    use strand_core::KeyedSource;
    let tmp = TempDir::new("keyed-redeclare");
    let store = tmp.store();
    let rt = Runtime::new();
    let h = history(&rt, &store, &[(1, "welcome")]);
    let cell = h.cell;
    let fired = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = fired.clone();
    rt.on_change(
        move |rt| cell.with(rt, |v| v.len()),
        move |_, n| {
            seen.borrow_mut().push(*n);
            Ok(())
        },
    );
    rt.flush();
    let v = cell.snapshot(&rt).unwrap().version();
    // A live reload: the default gained an item; the list held the old
    // default, so it takes the new one, by key.
    assert_eq!(
        h.redeclare(&rt, notes(&[(1, "welcome"), (2, "tips")])),
        Ok(Redeclared::Adopted)
    );
    rt.flush();
    rt.tick(Duration::from_secs(1));
    assert!(fired.borrow().is_empty(), "{:?}", fired.borrow());
    let diffs = cell.snapshot(&rt).unwrap().diffs_since(v).unwrap();
    assert!(
        matches!(diffs[..], [strand_core::VecDiff::Insert { key: 2, .. }]),
        "keyed, not a reset: {diffs:?}"
    );
    // A real change fires; `@reset` at reload does not; the overlay's
    // `[reset]` does.
    cell.push(&rt, (3, "x".into())).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), vec![3]);
    h.reset_reloaded(&rt).unwrap();
    rt.flush();
    assert_eq!(cell.with_untracked(&rt, |v| v.len()), Ok(2));
    assert_eq!(*fired.borrow(), vec![3]);
    cell.push(&rt, (4, "y".into())).unwrap();
    rt.flush();
    h.reset(&rt).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), vec![3, 3, 2]);
    // A changed list is kept over a new default, reported once.
    cell.push(&rt, (5, "z".into())).unwrap();
    rt.flush();
    rt.take_diagnostics();
    assert_eq!(h.redeclare(&rt, notes(&[])), Ok(Redeclared::Kept));
    assert_eq!(cell.with_untracked(&rt, |v| v.len()), Ok(3));
    assert!(
        rt.take_diagnostics()
            .iter()
            .any(|d| matches!(d, Diagnostic::PersistDefaultChanged { .. }))
    );
    rt.shutdown();
}

#[test]
fn a_reload_write_to_a_keyed_cell_is_keyed_and_does_not_fire_on_change() {
    use strand_core::KeyedSource;
    // Plain keyed state (no persist): the reconciler adopts a changed
    // default through `replace_all_reloaded`.
    let rt = Runtime::new();
    let cell = rt.keyed(notes(&[(1, "a"), (2, "b")]));
    let fired = std::rc::Rc::new(std::cell::RefCell::new(0));
    let f = fired.clone();
    rt.on_change(
        move |rt| cell.with(rt, |v| v.items().to_vec()),
        move |_, _| {
            *f.borrow_mut() += 1;
            Ok(())
        },
    );
    rt.flush();
    let v = cell.snapshot(&rt).unwrap().version();
    let values = [(2, "b".to_string()), (3, "c".to_string())];
    assert_eq!(cell.replace_all_reloaded(&rt, values.clone()), Ok(true));
    rt.flush();
    assert_eq!(*fired.borrow(), 0);
    let diffs = cell.snapshot(&rt).unwrap().diffs_since(v).unwrap();
    assert_eq!(diffs.len(), 2, "remove 1, insert 3: {diffs:?}");
    assert_eq!(
        cell.replace_all_reloaded(&rt, values),
        Ok(false),
        "unchanged"
    );
    cell.push(&rt, (4, "d".into())).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), 1, "a real change still fires");
}

//! `state x = … persist`: values stored with a hash of their default under
//! `$XDG_STATE_HOME/strand`, atomic writes, corrupt files fall back to the
//! default with a warning.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_core::persist::{PERSIST_DEBOUNCE, value_hash};
use strand_core::{Diagnostic, PersistError, PersistStore, PersistValue, Restore, Runtime};

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

//! Settings files (`state prefs from "prefs.toml" { … }`), one test per
//! rule of "Settings files" in the design: per-field checking, last good
//! values on a syntax error, deleted keys back to their default,
//! `toml_edit` write-back, symlink-following writes, the read-only overlay
//! and runtime overlay > file > default.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_core::persist::PERSIST_DEBOUNCE;
use strand_core::settings::toml_edit::{self, Item};
use strand_core::{
    Diagnostic, FieldSpec, PersistStore, Runtime, Settings, SettingsIssue, SettingsNotice,
    SettingsStore,
};

/// The VM's value type, as far as these fields need it.
#[derive(Clone, Debug, PartialEq)]
enum V {
    Color(String),
    Bool(bool),
}

fn color(name: &str, default: &str) -> FieldSpec<V> {
    FieldSpec::new(
        name,
        V::Color(default.into()),
        |item: &Item| {
            item.as_str()
                .filter(|s| s.len() == 7 && s.starts_with('#'))
                .map(|s| V::Color(s.into()))
                .ok_or_else(|| "expected a colour like \"#7aa2f7\"".to_string())
        },
        |v: &V| match v {
            V::Color(c) => toml_edit::value(c.as_str()),
            V::Bool(b) => toml_edit::value(*b),
        },
    )
}

fn flag(name: &str, default: bool) -> FieldSpec<V> {
    FieldSpec::new(
        name,
        V::Bool(default),
        |item: &Item| {
            item.as_bool()
                .map(V::Bool)
                .ok_or_else(|| "expected true or false".to_string())
        },
        |v: &V| match v {
            V::Bool(b) => toml_edit::value(*b),
            V::Color(c) => toml_edit::value(c.as_str()),
        },
    )
}

/// `state prefs from "prefs.toml" { accent: color = #7aa2f7; compact:
/// bool = false }`
fn prefs(rt: &Runtime, store: &SettingsStore, path: &Path) -> Settings<V> {
    rt.settings_file(
        store,
        path,
        vec![color("accent", "#7aa2f7"), flag("compact", false)],
    )
}

/// A fresh directory, removed when dropped (permissions restored first).
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "strand-settings-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("config")).unwrap();
        Self(dir)
    }
    fn config(&self, name: &str) -> PathBuf {
        self.0.join("config").join(name)
    }
    fn store(&self) -> SettingsStore {
        PersistStore::new(self.0.join("state/strand/persist")).settings()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn get(rt: &Runtime, s: &Settings<V>, field: &str) -> V {
    s.signal(field).unwrap().get(rt).unwrap()
}

fn settings_notices(diags: &[Diagnostic]) -> Vec<SettingsNotice> {
    diags
        .iter()
        .filter_map(|d| match d {
            Diagnostic::Settings(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

fn issues(diags: &[Diagnostic]) -> Vec<(Option<String>, SettingsIssue)> {
    settings_notices(diags)
        .into_iter()
        .map(|n| (n.field.map(|f| f.to_string()), n.issue))
        .collect()
}

/// Write through the field and let the debounce pass; wait for the disk.
/// Returns the diagnostics of those ticks and of the IO thread.
fn write(
    rt: &Runtime,
    s: &Settings<V>,
    store: &SettingsStore,
    field: &str,
    v: V,
    at: Duration,
) -> Vec<Diagnostic> {
    s.set(rt, field, v).unwrap();
    let mut d = rt.tick(at).diagnostics;
    d.extend(rt.tick(at + PERSIST_DEBOUNCE).diagnostics);
    assert!(store.sync(Duration::from_secs(5)));
    d.extend(rt.flush().diagnostics);
    d
}

#[test]
fn each_field_is_checked_on_its_own() {
    let tmp = TempDir::new("per-field");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#ff0000\"\ncompact = \"yes\"\n").unwrap();
    let rt = Runtime::new();
    let s = prefs(&rt, &tmp.store(), &file);
    // The good field applies, the bad one keeps its last good value (the
    // default at boot), with a diagnostic naming it.
    assert_eq!(get(&rt, &s, "accent"), V::Color("#ff0000".into()));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
    let d = rt.take_diagnostics();
    assert!(
        matches!(&issues(&d)[..], [(Some(f), SettingsIssue::BadValue(_))] if f == "compact"),
        "{d:?}"
    );
    // Now compact is good and accent is bad: accent keeps "#ff0000".
    fs::write(&file, "accent = 12\ncompact = true\n").unwrap();
    s.reload(&rt);
    let d = rt.flush().diagnostics;
    assert_eq!(get(&rt, &s, "accent"), V::Color("#ff0000".into()));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert!(
        matches!(&issues(&d)[..], [(Some(f), SettingsIssue::BadValue(_))] if f == "accent"),
        "{d:?}"
    );
}

#[test]
fn a_syntax_error_keeps_every_last_good_value() {
    let tmp = TempDir::new("syntax");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\ncompact = true\n").unwrap();
    let rt = Runtime::new();
    let s = prefs(&rt, &tmp.store(), &file);
    rt.flush();
    fs::write(&file, "accent = \"#0000ff\"\ncompact = tru\n[oops\n").unwrap();
    s.reload(&rt);
    let tick = rt.flush();
    // Not even the accent line that parses on its own applies.
    assert_eq!(get(&rt, &s, "accent"), V::Color("#00ff00".into()));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert!(tick.changed.is_empty());
    let d = tick.diagnostics;
    assert!(
        matches!(&issues(&d)[..], [(None, SettingsIssue::Syntax(_))]),
        "{d:?}"
    );
    // The fix applies.
    fs::write(&file, "accent = \"#0000ff\"\ncompact = true\n").unwrap();
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "accent"), V::Color("#0000ff".into()));
    assert!(rt.take_diagnostics().is_empty());
}

#[test]
fn a_deleted_key_springs_back_to_its_default() {
    let tmp = TempDir::new("deleted");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\ncompact = true\n").unwrap();
    let rt = Runtime::new();
    let s = prefs(&rt, &tmp.store(), &file);
    let accent = s.signal("accent").unwrap();
    rt.watch(accent.id()).unwrap();
    rt.flush();
    fs::write(&file, "compact = true\n").unwrap();
    s.reload(&rt);
    let tick = rt.flush();
    assert_eq!(tick.changed, vec![accent.id()]);
    assert_eq!(get(&rt, &s, "accent"), V::Color("#7aa2f7".into()));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    // A deleted file is every key deleted.
    fs::remove_file(&file).unwrap();
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
    assert!(rt.take_diagnostics().is_empty());
}

#[test]
fn ui_writes_go_through_toml_edit_keeping_comments_spacing_and_order() {
    let tmp = TempDir::new("toml-edit");
    let file = tmp.config("prefs.toml");
    let original = "\
# Colours
accent   =  \"#7aa2f7\"   # blue, like the logo

[bar]   # a section the schema does not know about
height = 32
";
    fs::write(&file, original).unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    // A UI write (or `strand set prefs.compact true`), then another field.
    let mut d = write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#ff9e64".into()),
        Duration::from_millis(10),
    );
    d.extend(write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_secs(1),
    ));
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "\
# Colours
accent   =  \"#ff9e64\"   # blue, like the logo
compact = true

[bar]   # a section the schema does not know about
height = 32
"
    );
    assert!(d.is_empty(), "{d:?}");
    // Several writes within the debounce are one write of the last value.
    s.set(&rt, "accent", V::Color("#000001".into())).unwrap();
    rt.tick(Duration::from_secs(2));
    s.set(&rt, "accent", V::Color("#000002".into())).unwrap();
    rt.tick(Duration::from_secs(2) + Duration::from_millis(100));
    assert!(fs::read_to_string(&file).unwrap().contains("#ff9e64"));
    rt.tick(Duration::from_secs(3));
    assert!(store.sync(Duration::from_secs(5)));
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("accent   =  \"#000002\"   # blue")
    );
}

#[test]
fn writes_follow_symlinks_and_replace_the_target() {
    // GNU stow: the config file is a link into a dotfiles repository.
    let tmp = TempDir::new("symlink");
    let dotfiles = tmp.0.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("prefs.toml");
    fs::write(&real, "compact = false # keep\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
    let link = tmp.config("prefs.toml");
    symlink("../dotfiles/prefs.toml", &link).unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &link);
    rt.flush();
    let d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(10),
    );
    // The link is still a link; its target was replaced with the new text
    // (keeping its permissions), and no temp file is left behind.
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&real).unwrap(),
        "compact = true # keep\n"
    );
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let mut names: Vec<_> = fs::read_dir(&dotfiles)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, vec!["prefs.toml"]);
    assert!(d.is_empty(), "{d:?}");
    assert!(!s.is_read_only());
}

#[test]
fn a_read_only_target_goes_to_an_overlay_with_a_notice() {
    // home-manager: the config file links into the read-only /nix/store.
    let tmp = TempDir::new("read-only");
    let nix = tmp.0.join("nix/store/abc-prefs");
    fs::create_dir_all(&nix).unwrap();
    let real = nix.join("prefs.toml");
    fs::write(&real, "accent = \"#00ff00\"\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&nix, fs::Permissions::from_mode(0o555)).unwrap();
    let link = tmp.config("prefs.toml");
    symlink(&real, &link).unwrap();
    let store = tmp.store();
    {
        let rt = Runtime::new();
        let s = prefs(&rt, &store, &link);
        assert!(s.is_read_only());
        rt.flush();
        let mut d = write(
            &rt,
            &s,
            &store,
            "compact",
            V::Bool(true),
            Duration::from_millis(10),
        );
        d.extend(write(
            &rt,
            &s,
            &store,
            "accent",
            V::Color("#123456".into()),
            Duration::from_secs(1),
        ));
        // The store file is untouched; the overlay holds the writes.
        assert_eq!(fs::read_to_string(&real).unwrap(), "accent = \"#00ff00\"\n");
        let overlay = s.overlay_path().to_path_buf();
        assert!(overlay.starts_with(store.dir()));
        assert!(store.dir().ends_with("state/strand/settings"));
        let text = fs::read_to_string(&overlay).unwrap();
        assert!(text.contains("compact = true"), "{text}");
        assert!(text.contains("accent = \"#123456\""), "{text}");
        // One notice, naming the overlay.
        assert!(
            matches!(&issues(&d)[..], [(None, SettingsIssue::ReadOnly { overlay: o })] if *o == overlay),
            "{d:?}"
        );
        assert_eq!(get(&rt, &s, "accent"), V::Color("#123456".into()));
        rt.shutdown();
    }
    // The next start: overlay > file.
    let rt = Runtime::new();
    let s = prefs(&rt, &store, &link);
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert_eq!(get(&rt, &s, "accent"), V::Color("#123456".into()));
    assert_eq!(s.overlay("accent"), Some(V::Color("#123456".into())));
}

#[test]
fn a_write_that_fails_is_reported_and_the_value_stays() {
    // A write that fails for another reason than permissions (here the
    // parent is a file: ENOTDIR) is reported; the value stays live.
    let tmp = TempDir::new("refused");
    let blocker = tmp.config("not-a-dir");
    fs::write(&blocker, "").unwrap();
    let file = blocker.join("prefs.toml");
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.take_diagnostics();
    rt.flush();
    let d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(10),
    );
    assert!(
        matches!(&issues(&d)[..], [(None, SettingsIssue::WriteFailed(_))]),
        "{d:?}"
    );
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
}

#[test]
fn a_file_with_a_syntax_error_is_not_overwritten() {
    let tmp = TempDir::new("no-clobber");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    // The user is half-way through an edit when the UI writes.
    fs::write(&file, "compact = false\naccent = \n").unwrap();
    let d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(10),
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "compact = false\naccent = \n"
    );
    assert!(
        matches!(&issues(&d)[..], [(None, SettingsIssue::WriteFailed(m))] if m.contains("syntax")),
        "{d:?}"
    );
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
}

#[test]
fn the_runtime_overlay_wins_over_the_file_and_says_so() {
    let tmp = TempDir::new("overlay");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    s.set_overlay(&rt, "accent", V::Color("#ff00ff".into()))
        .unwrap();
    rt.flush();
    assert_eq!(get(&rt, &s, "accent"), V::Color("#ff00ff".into()));
    // The file changes the shadowed field: the overlay still wins, and the
    // reload says so.
    fs::write(&file, "accent = \"#0000ff\"\ncompact = true\n").unwrap();
    s.reload(&rt);
    let d = rt.flush().diagnostics;
    assert_eq!(get(&rt, &s, "accent"), V::Color("#ff00ff".into()));
    assert_eq!(
        get(&rt, &s, "compact"),
        V::Bool(true),
        "unshadowed fields apply"
    );
    let notices = settings_notices(&d);
    assert!(
        matches!(&notices[..], [n] if n.issue == SettingsIssue::Shadowed),
        "{d:?}"
    );
    assert_eq!(
        notices[0].to_string(),
        "accent: file changed but runtime overlay wins [clear]"
    );
    // A reload that leaves the field alone says nothing.
    s.reload(&rt);
    assert!(rt.flush().diagnostics.is_empty());
    // A UI write to the shadowed field updates the overlay, not the file.
    let d = write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#111111".into()),
        Duration::from_millis(10),
    );
    assert!(d.is_empty(), "{d:?}");
    assert!(fs::read_to_string(&file).unwrap().contains("#0000ff"));
    assert_eq!(s.overlay("accent"), Some(V::Color("#111111".into())));
    // [clear]: the file applies again, and the overlay file goes away.
    s.clear_overlay(&rt, "accent").unwrap();
    rt.flush();
    assert_eq!(get(&rt, &s, "accent"), V::Color("#0000ff".into()));
    assert!(store.sync(Duration::from_secs(5)));
    assert!(!s.overlay_path().exists());
    // And the default is the last resort.
    fs::write(&file, "").unwrap();
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "accent"), V::Color("#7aa2f7".into()));
}

#[test]
fn a_reload_never_undoes_a_write_that_has_not_reached_the_disk() {
    // The watcher fires (another field edited in the editor) while the UI
    // write is still debouncing, and again while it waits for a slow disk.
    let tmp = TempDir::new("reload-race");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let g = gate.clone();
    let io = PersistStore::with_io_hook(tmp.0.join("state/strand/persist"), move |_| {
        let (open, cv) = &*g;
        let mut open = open.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    });
    let store = io.settings();
    let rt = Runtime::new();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    s.set(&rt, "compact", V::Bool(true)).unwrap();
    rt.tick(Duration::from_millis(10));
    // Debouncing: the user's value is kept, the file's other field applies.
    fs::write(&file, "compact = false\naccent = \"#00ff00\"\n").unwrap();
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert_eq!(get(&rt, &s, "accent"), V::Color("#00ff00".into()));
    // Queued behind a slow disk: still kept.
    rt.tick(Duration::from_millis(10) + PERSIST_DEBOUNCE);
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    {
        let (open, cv) = &*gate;
        *open.lock().unwrap() = true;
        cv.notify_all();
    }
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "compact = true\naccent = \"#00ff00\"\n"
    );
    assert!(rt.take_diagnostics().is_empty());
}

#[test]
fn a_pending_write_is_saved_on_unmount() {
    let tmp = TempDir::new("unmount");
    let file = tmp.config("prefs.toml");
    let rt = Runtime::new();
    let store = tmp.store();
    let (scope, s) = rt.scope(|rt| prefs(rt, &store, &file));
    rt.flush();
    s.set(&rt, "compact", V::Bool(true)).unwrap();
    rt.tick(Duration::from_millis(10));
    scope.dispose(&rt);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "compact = true\n");
}

#[test]
fn a_target_made_read_only_later_falls_back_on_the_io_thread() {
    // Writable at boot; read-only by the time the write reaches the disk
    // (a `home-manager switch` in between). The IO thread redirects the
    // write to the overlay, reports it once, and later writes go straight
    // there.
    let tmp = TempDir::new("late-read-only");
    let dir = tmp.0.join("config/sub");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    assert!(!s.is_read_only());
    rt.flush();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let mut d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(10),
    );
    assert!(s.is_read_only());
    assert_eq!(fs::read_to_string(&file).unwrap(), "compact = false\n");
    assert_eq!(
        fs::read_to_string(s.overlay_path()).unwrap(),
        "compact = true\n"
    );
    d.extend(write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#abcdef".into()),
        Duration::from_secs(1),
    ));
    assert!(
        matches!(&issues(&d)[..], [(None, SettingsIssue::ReadOnly { .. })]),
        "once: {d:?}"
    );
    assert!(
        fs::read_to_string(s.overlay_path())
            .unwrap()
            .contains("#abcdef")
    );
    // The overlay is what a reload (and the next start) sees on top.
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert_eq!(get(&rt, &s, "accent"), V::Color("#abcdef".into()));
    assert!(rt.take_diagnostics().is_empty());
}

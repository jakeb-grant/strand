//! Settings files (`state prefs from "prefs.toml" { … }`), one test per
//! rule of "Settings files" in the design: per-field checking, last good
//! values on a syntax error, deleted keys back to their default,
//! `toml_edit` write-back, symlink-following writes, the read-only overlay
//! and runtime overlay > file > default; plus last good values across a
//! restart, reads made on the watcher's thread, several handles on one
//! file and live redeclaration.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_core::persist::PERSIST_DEBOUNCE;
use strand_core::settings::toml_edit::{self, Item};
use strand_core::{
    Diagnostic, FieldSpec, PersistStore, Runtime, Settings, SettingsIssue, SettingsLayer,
    SettingsNotice, SettingsStore,
};

/// The VM's value type, as far as these fields need it.
#[derive(Clone, Debug, PartialEq)]
enum V {
    Color(String),
    Bool(bool),
    /// (settings-tables) A value in the form the compiler encodes it, as
    /// the TOML of a key `x`: a dictionary a `[x]` section of inline
    /// values, a list of records an array of inline tables.
    Toml(String),
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
            V::Toml(_) => Item::None,
        },
    )
    .with_type("color")
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
            V::Toml(_) => Item::None,
        },
    )
    .with_type("bool")
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

/// (settings-tables) A field whose value in the file is bad is not
/// written over: a dictionary of records held at its default by one
/// stale key would otherwise lose every other entry the user wrote. The
/// value stays live with a notice naming the field; the other fields
/// are saved; once the file reads again, it wins.
#[test]
fn a_field_whose_file_value_is_bad_is_not_overwritten() {
    let tmp = TempDir::new("bad-held");
    let file = tmp.config("prefs.toml");
    let text = "# mine\naccent = \"#ff0000\"\ncompact = \"yes\"\n";
    fs::write(&file, text).unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    rt.take_diagnostics();
    let d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(10),
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), text, "the file is kept");
    let notices = settings_notices(&d);
    assert!(
        matches!(&notices[..], [n] if n.field.as_deref() == Some("compact")
            && matches!(n.issue, SettingsIssue::WriteFailed(_))
            && n.to_string().contains("compact: not saved while its value in the file is bad")),
        "{d:?}"
    );
    assert_eq!(
        get(&rt, &s, "compact"),
        V::Bool(true),
        "the value stays live"
    );
    // Another field still saves, and leaves the bad one as written.
    write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#00ff00".into()),
        Duration::from_millis(400),
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "# mine\naccent = \"#00ff00\"\ncompact = \"yes\"\n"
    );
    // The user's fix wins over the unsaved value, and writes save again.
    fs::write(&file, "# mine\naccent = \"#00ff00\"\ncompact = false\n").unwrap();
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
    write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(800),
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "# mine\naccent = \"#00ff00\"\ncompact = true\n"
    );
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

#[test]
fn last_good_values_survive_a_restart_with_a_broken_file() {
    let tmp = TempDir::new("last-good");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\ncompact = true\n").unwrap();
    let store = tmp.store();
    {
        let rt = Runtime::new();
        let s = prefs(&rt, &store, &file);
        rt.flush();
        // A UI write is a last good value too.
        write(
            &rt,
            &s,
            &store,
            "accent",
            V::Color("#123456".into()),
            Duration::from_millis(10),
        );
        assert!(s.last_good_path().starts_with(store.dir()));
        rt.shutdown();
    }
    assert!(store.sync(Duration::from_secs(5)));
    // Broken while Strand was not running: the next boot keeps every last
    // good value instead of flashing the defaults.
    fs::write(&file, "accent = \"#123456\"\ncompact = tru\n[oops\n").unwrap();
    {
        let rt = Runtime::new();
        let s = prefs(&rt, &store, &file);
        assert_eq!(get(&rt, &s, "accent"), V::Color("#123456".into()));
        assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
        assert_eq!(s.layer("accent"), Some(SettingsLayer::File));
        let d = rt.take_diagnostics();
        assert!(
            matches!(&issues(&d)[..], [(None, SettingsIssue::Syntax(_))]),
            "{d:?}"
        );
        rt.shutdown();
    }
    // A bad value at boot keeps that field's last good value; the others
    // apply (and become the new last good values).
    fs::write(&file, "accent = 12\ncompact = false\n").unwrap();
    {
        let rt = Runtime::new();
        let s = prefs(&rt, &store, &file);
        assert_eq!(get(&rt, &s, "accent"), V::Color("#123456".into()));
        assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
        let d = rt.take_diagnostics();
        assert!(
            matches!(&issues(&d)[..], [(Some(f), SettingsIssue::BadValue(_))] if f == "accent"),
            "{d:?}"
        );
        rt.shutdown();
    }
    fs::write(&file, "[oops\n").unwrap();
    let rt = Runtime::new();
    let s = prefs(&rt, &store, &file);
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
    // A key deleted from a good file is the default again, there too.
    fs::write(&file, "accent = \"#123456\"\n").unwrap();
    s.reload(&rt);
    rt.shutdown();
    assert!(store.sync(Duration::from_secs(5)));
    fs::write(&file, "[oops\n").unwrap();
    let rt = Runtime::new();
    let s = prefs(&rt, &store, &file);
    assert_eq!(get(&rt, &s, "accent"), V::Color("#123456".into()));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
    assert_eq!(s.layer("compact"), Some(SettingsLayer::Default));
}

#[test]
fn a_link_swapped_to_a_writable_file_takes_writes_again() {
    // `home-manager` at first, then the user moves to stow: the link now
    // points at a writable file, and the next write reaches it.
    let tmp = TempDir::new("swap-writable");
    let nix = tmp.0.join("nix/store/abc-prefs");
    fs::create_dir_all(&nix).unwrap();
    let frozen = nix.join("prefs.toml");
    fs::write(&frozen, "accent = \"#00ff00\"\n").unwrap();
    fs::set_permissions(&frozen, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&nix, fs::Permissions::from_mode(0o555)).unwrap();
    let dotfiles = tmp.0.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let writable = dotfiles.join("prefs.toml");
    fs::write(&writable, "accent = \"#00ff00\"\n").unwrap();
    let link = tmp.config("prefs.toml");
    symlink(&frozen, &link).unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
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
    // The swap.
    fs::remove_file(&link).unwrap();
    symlink(&writable, &link).unwrap();
    s.reload(&rt);
    rt.flush();
    assert!(!s.is_read_only());
    d.extend(write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#abcdef".into()),
        Duration::from_secs(1),
    ));
    assert_eq!(
        fs::read_to_string(&writable).unwrap(),
        "accent = \"#abcdef\"\n"
    );
    // The field written while read-only stays in the overlay, which wins.
    assert_eq!(s.layer("compact"), Some(SettingsLayer::Overlay));
    assert_eq!(s.layer("accent"), Some(SettingsLayer::File));
    // And back to read-only: the notice comes again.
    fs::remove_file(&link).unwrap();
    symlink(&frozen, &link).unwrap();
    s.reload(&rt);
    assert!(s.is_read_only());
    d.extend(write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#fedcba".into()),
        Duration::from_secs(2),
    ));
    let notices: Vec<_> = issues(&d)
        .into_iter()
        .filter(|(_, i)| matches!(i, SettingsIssue::ReadOnly { .. }))
        .collect();
    assert_eq!(notices.len(), 2, "{d:?}");
    assert!(
        fs::read_to_string(s.overlay_path())
            .unwrap()
            .contains("#fedcba")
    );
}

#[test]
fn the_watcher_can_read_on_its_own_thread() {
    let tmp = TempDir::new("off-thread");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    fs::write(&file, "accent = \"#0000ff\"\ncompact = true\n").unwrap();
    let sources = s.sources();
    // The watcher hashes the bytes it read: it marks first, then reads,
    // then hands both over, so the file is read once.
    let read = std::thread::spawn(move || {
        let mark = sources.mark();
        let text = fs::read_to_string(sources.path());
        sources.read_from(mark, text)
    })
    .join()
    .unwrap();
    s.reload_with(&rt, read);
    rt.flush();
    assert_eq!(get(&rt, &s, "accent"), V::Color("#0000ff".into()));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert!(rt.take_diagnostics().is_empty());
}

#[test]
fn a_read_from_before_a_write_landed_does_not_undo_it() {
    // The watcher read the file, then Strand's own write was queued and
    // reached the disk, then the stale read arrives.
    let tmp = TempDir::new("stale-read");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    s.set(&rt, "compact", V::Bool(true)).unwrap();
    rt.tick(Duration::from_millis(10));
    let stale = s.sources().read();
    rt.tick(Duration::from_millis(10) + PERSIST_DEBOUNCE);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "compact = true\n");
    s.reload_with(&rt, stale);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    // A later write still goes out (the shown value was not reset).
    let d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(false),
        Duration::from_secs(1),
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "compact = false\n");
    // A fresh read applies again.
    fs::write(&file, "compact = true\n").unwrap();
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
}

#[test]
fn two_handles_on_one_file_see_each_others_writes() {
    // `state prefs from "prefs.toml"` in a component mounted per monitor.
    let tmp = TempDir::new("siblings");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let (left, a) = rt.scope(|rt| prefs(rt, &store, &file));
    let b = prefs(&rt, &store, &file);
    rt.flush();
    let d = write(
        &rt,
        &a,
        &store,
        "accent",
        V::Color("#123456".into()),
        Duration::from_millis(10),
    );
    assert!(d.is_empty(), "{d:?}");
    // No reload: b took a's write when it was queued.
    assert_eq!(get(&rt, &b, "accent"), V::Color("#123456".into()));
    assert_eq!(fs::read_to_string(&file).unwrap(), "accent = \"#123456\"\n");
    b.set_overlay(&rt, "compact", V::Bool(true)).unwrap();
    rt.flush();
    assert_eq!(get(&rt, &a, "compact"), V::Bool(true));
    assert_eq!(a.layer("compact"), Some(SettingsLayer::Overlay));
    b.clear_overlay(&rt, "compact").unwrap();
    rt.flush();
    assert_eq!(get(&rt, &a, "compact"), V::Bool(false));
    // A handle that went away is not told anything.
    left.dispose(&rt);
    let d = write(
        &rt,
        &b,
        &store,
        "accent",
        V::Color("#654321".into()),
        Duration::from_secs(1),
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(get(&rt, &b, "accent"), V::Color("#654321".into()));
}

#[test]
fn a_corrupt_overlay_is_moved_aside_and_writes_go_on() {
    let tmp = TempDir::new("corrupt-overlay");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    s.set_overlay(&rt, "accent", V::Color("#ff00ff".into()))
        .unwrap();
    rt.flush();
    assert!(store.sync(Duration::from_secs(5)));
    let overlay = s.overlay_path().to_path_buf();
    let corrupt = overlay.with_file_name(format!(
        ".{}.corrupt",
        overlay.file_name().unwrap().to_string_lossy()
    ));
    // Broken by a crash or a hand edit; the reload reports it once and
    // goes on from an empty overlay.
    fs::write(&overlay, "accent = \"#ff00ff\n").unwrap();
    s.reload(&rt);
    let d = rt.flush().diagnostics;
    assert!(
        matches!(&issues(&d)[..], [(None, SettingsIssue::CorruptOverlay { moved_to, .. })] if *moved_to == corrupt),
        "{d:?}"
    );
    assert_eq!(get(&rt, &s, "accent"), V::Color("#00ff00".into()));
    s.clear_overlay(&rt, "accent").unwrap();
    s.set_overlay(&rt, "compact", V::Bool(true)).unwrap();
    let mut d = write(
        &rt,
        &s,
        &store,
        "accent",
        V::Color("#111111".into()),
        Duration::from_millis(10),
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(fs::read_to_string(&overlay).unwrap(), "compact = true\n");
    assert!(fs::read_to_string(&file).unwrap().contains("#111111"));
    assert!(corrupt.exists());
    // Broken again with no read in between: the write moves it aside and
    // says so.
    fs::write(&overlay, "[oops\n").unwrap();
    s.set_overlay(&rt, "compact", V::Bool(false)).unwrap();
    assert!(store.sync(Duration::from_secs(5)));
    d = rt.flush().diagnostics;
    assert!(
        matches!(
            &issues(&d)[..],
            [(None, SettingsIssue::CorruptOverlay { .. })]
        ),
        "{d:?}"
    );
    assert_eq!(fs::read_to_string(&overlay).unwrap(), "compact = false\n");
}

#[test]
fn a_reload_that_cannot_write_leaves_the_field_to_the_next_one() {
    // A reload from inside a derived value cannot set the signal; the
    // stale signal must not be taken for a user write and saved over the
    // external edit.
    let tmp = TempDir::new("stale-show");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    fs::write(&file, "compact = true\n").unwrap();
    let inside = s.clone();
    let m = rt.memo(move |rt| {
        inside.reload(rt);
        Ok(0)
    });
    m.get(&rt).unwrap();
    s.write_out(&rt);
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(fs::read_to_string(&file).unwrap(), "compact = true\n");
    s.reload(&rt);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
}

#[test]
fn strands_own_file_write_is_not_reported_as_shadowed() {
    let tmp = TempDir::new("own-write");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    s.set(&rt, "compact", V::Bool(true)).unwrap();
    s.write_out(&rt);
    s.set_overlay(&rt, "compact", V::Bool(false)).unwrap();
    assert!(store.sync(Duration::from_secs(5)));
    s.reload(&rt);
    let d = rt.flush().diagnostics;
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
}

#[test]
fn a_missing_directory_is_created_for_the_first_write() {
    let tmp = TempDir::new("missing-dir");
    let file = tmp.config("strand/settings/prefs.toml");
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    let d = write(
        &rt,
        &s,
        &store,
        "compact",
        V::Bool(true),
        Duration::from_millis(10),
    );
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "compact = true\n");
}

#[test]
fn temp_files_a_crash_left_next_to_the_file_are_swept() {
    let tmp = TempDir::new("sweep");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "").unwrap();
    // A process that is gone left one of ours, and one of another file.
    let dead = 999_999_999u32;
    let ours = tmp.config(&format!(".prefs.toml.tmp.{dead}.0"));
    let other = tmp.config(&format!(".other.toml.tmp.{dead}.0"));
    let live = tmp.config(&format!(".prefs.toml.tmp.{}.7", std::process::id()));
    for f in [&ours, &other, &live] {
        fs::write(f, "x").unwrap();
    }
    let rt = Runtime::new();
    let _s = prefs(&rt, &tmp.store(), &file);
    assert!(!ours.exists());
    assert!(other.exists(), "only this file's temp files");
    assert!(live.exists(), "never this process's own");
}

#[test]
fn redeclare_keeps_cells_by_name_and_resets_a_changed_type() {
    let tmp = TempDir::new("redeclare");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\"\ndense = true\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    let accent = s.signal("accent").unwrap();
    let compact = s.signal("compact").unwrap();
    assert_eq!(s.layer("accent"), Some(SettingsLayer::File));
    assert_eq!(s.layer("compact"), Some(SettingsLayer::Default));
    // A new default for a field nothing set is adopted.
    s.redeclare(&rt, vec![color("accent", "#7aa2f7"), flag("compact", true)]);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert_eq!(s.signal("compact"), Some(compact), "same cell");
    // A field the user wrote keeps its value over a new default.
    s.set(&rt, "compact", V::Bool(false)).unwrap();
    s.redeclare(&rt, vec![color("accent", "#000000"), flag("compact", true)]);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false));
    assert_eq!(
        get(&rt, &s, "accent"),
        V::Color("#00ff00".into()),
        "the file still wins"
    );
    assert!(rt.take_diagnostics().is_empty());
    // A type change resets that field only; a new field is read from the
    // file; a removed one is gone.
    s.redeclare(
        &rt,
        vec![
            flag("accent", false),
            flag("compact", true),
            flag("dense", false),
        ],
    );
    let d = rt.flush().diagnostics;
    assert_eq!(s.signal("accent"), Some(accent), "same cell");
    assert_eq!(get(&rt, &s, "accent"), V::Bool(false));
    assert_eq!(get(&rt, &s, "dense"), V::Bool(true));
    assert_eq!(get(&rt, &s, "compact"), V::Bool(false), "kept");
    let found = issues(&d);
    assert!(
        found.iter().any(|(f, i)| f.as_deref() == Some("accent")
            && matches!(i, SettingsIssue::TypeChanged { from, to } if &**from == "color" && &**to == "bool")),
        "{d:?}"
    );
    s.redeclare(&rt, vec![flag("compact", true), flag("dense", false)]);
    assert!(s.signal("accent").is_none());
    assert!(
        accent.get(&rt).is_err(),
        "the removed field's cell is disposed"
    );
    // The saver tracks the field list as it is now.
    let d = write(
        &rt,
        &s,
        &store,
        "dense",
        V::Bool(false),
        Duration::from_secs(1),
    );
    assert!(d.is_empty(), "{d:?}");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("dense = false"), "{text}");
    assert!(text.contains("compact = false"), "{text}");
    assert!(text.contains("accent = \"#00ff00\""), "{text}");
}

/// (dictionary) A redeclare that keeps a field's type carries what the
/// handle holds into the new program through the field's carry
/// (`FieldSpec::with_carry`, the compiler's `translate`): the file's
/// layer, the cell and a write not saved yet, which is then saved as
/// carried; the cell keeps its identity and no notice is given. A carry
/// that refuses a value reads the field again from its file.
#[test]
fn redeclare_carries_each_layer_through_its_carry() {
    let tmp = TempDir::new("redeclare-carry");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "accent = \"#00ff00\" # mine\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    let accent = s.signal("accent").unwrap();
    let upper = |v: &V| match v {
        V::Color(c) => Some(V::Color(c.to_uppercase())),
        V::Bool(b) => Some(V::Bool(*b)),
        V::Toml(t) => Some(V::Toml(t.clone())),
    };
    s.redeclare(
        &rt,
        vec![
            color("accent", "#7aa2f7").with_carry(upper),
            flag("compact", false).with_carry(upper),
        ],
    );
    let d = rt.flush().diagnostics;
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(s.signal("accent"), Some(accent), "same cell");
    assert_eq!(get(&rt, &s, "accent"), V::Color("#00FF00".into()));
    assert_eq!(s.layer("accent"), Some(SettingsLayer::File));
    // A write not saved yet (its tick ended, its debounce running) is
    // carried, then saved as carried.
    s.set(&rt, "accent", V::Color("#abcdef".into())).unwrap();
    rt.tick(Duration::from_millis(500));
    s.redeclare(
        &rt,
        vec![
            color("accent", "#7aa2f7").with_carry(upper),
            flag("compact", false),
        ],
    );
    assert_eq!(get(&rt, &s, "accent"), V::Color("#ABCDEF".into()));
    let mut d = rt.tick(Duration::from_secs(1)).diagnostics;
    d.extend(
        rt.tick(Duration::from_secs(1) + PERSIST_DEBOUNCE)
            .diagnostics,
    );
    assert!(store.sync(Duration::from_secs(5)));
    d.extend(rt.flush().diagnostics);
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "accent = \"#ABCDEF\" # mine\n"
    );
    // A carry that refuses: the field is read again from its file.
    fs::write(&file, "accent = \"#123abc\"\n").unwrap();
    s.redeclare(
        &rt,
        vec![
            color("accent", "#7aa2f7").with_carry(|_| None),
            flag("compact", false),
        ],
    );
    let d = rt.flush().diagnostics;
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(get(&rt, &s, "accent"), V::Color("#123abc".into()));
}

#[test]
fn redeclare_keeps_a_live_value_the_broken_file_could_not_take() {
    let tmp = TempDir::new("redeclare-broken");
    let file = tmp.config("prefs.toml");
    fs::write(&file, "compact = false\n").unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    rt.flush();
    fs::write(&file, "compact = false\n[oops\n").unwrap();
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
    s.redeclare(
        &rt,
        vec![
            color("accent", "#7aa2f7"),
            flag("compact", false),
            flag("dense", true),
        ],
    );
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    assert_eq!(get(&rt, &s, "dense"), V::Bool(true));
}

#[test]
fn own_writes_are_reported_with_the_bytes_that_land() {
    // The watcher pre-registers the hash of Strand's own writes ("Live
    // reload" step 2): the observer gets each file's exact new content
    // before it becomes visible.
    use std::sync::{Arc, Mutex};
    type Seen = Vec<(PathBuf, PathBuf, Option<Vec<u8>>)>;
    let tmp = TempDir::new("own-writes");
    let dotfiles = tmp.0.join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("prefs.toml");
    fs::write(&real, "# mine\ncompact = false\n").unwrap();
    let link = tmp.config("prefs.toml");
    symlink("../dotfiles/prefs.toml", &link).unwrap();
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let rt = Runtime::new();
    let store = tmp.store();
    let log = seen.clone();
    store.on_written(move |w| {
        // Not visible yet: the rename comes after the observer.
        if let Some(c) = w.content {
            assert_ne!(fs::read(w.target).ok().as_deref(), Some(c));
        }
        log.lock().unwrap().push((
            w.path.to_path_buf(),
            w.target.to_path_buf(),
            w.content.map(<[u8]>::to_vec),
        ));
    });
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
    assert!(d.is_empty(), "{d:?}");
    let on_disk = fs::read(&real).unwrap();
    assert_eq!(on_disk, b"# mine\ncompact = true\n");
    let seen = seen.lock().unwrap();
    let own: Vec<_> = seen.iter().filter(|(p, _, _)| *p == link).collect();
    assert_eq!(own.len(), 1, "{seen:?}");
    assert_eq!(
        own[0].1,
        fs::canonicalize(&real).unwrap(),
        "the target, symlinks followed"
    );
    assert_eq!(own[0].2.as_deref(), Some(&on_disk[..]));
    // Every other file it wrote (the last-good snapshot) is reported with
    // what is on disk too: the last report per file is its content now.
    let mut last = std::collections::HashMap::new();
    for (_, target, content) in seen.iter() {
        last.insert(target.clone(), content.clone());
    }
    assert!(last.len() >= 2, "{seen:?}");
    for (target, content) in last {
        assert_eq!(fs::read(&target).ok(), content, "{target:?}");
    }
}

#[test]
fn a_field_written_in_the_mount_tick_is_saved_after_the_debounce() {
    let tmp = TempDir::new("mount-write");
    let file = tmp.config("prefs.toml");
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    // Written before the saver's first tracking run.
    s.set(&rt, "compact", V::Bool(true)).unwrap();
    rt.flush();
    rt.tick(PERSIST_DEBOUNCE + Duration::from_millis(16));
    assert!(store.sync(Duration::from_secs(5)));
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "compact = true\n",
        "saved without an unmount"
    );
    rt.shutdown();
}

#[test]
fn redeclare_does_not_fire_on_change() {
    let tmp = TempDir::new("redeclare-on-change");
    let file = tmp.config("prefs.toml");
    let rt = Runtime::new();
    let store = tmp.store();
    let s = prefs(&rt, &store, &file);
    let compact = s.signal("compact").unwrap();
    let fired = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = fired.clone();
    rt.on_change(
        move |rt| compact.get(rt),
        move |_, v| {
            seen.borrow_mut().push(v.clone());
            Ok(())
        },
    );
    rt.flush();
    // A live reload adopts a new default: not a change.
    s.redeclare(&rt, vec![color("accent", "#7aa2f7"), flag("compact", true)]);
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Bool(true));
    // A type change resets the field: not a change either.
    s.redeclare(
        &rt,
        vec![color("accent", "#7aa2f7"), color("compact", "#000000")],
    );
    rt.flush();
    assert_eq!(get(&rt, &s, "compact"), V::Color("#000000".into()));
    assert!(fired.borrow().is_empty(), "{:?}", fired.borrow());
    // A write is.
    s.set(&rt, "compact", V::Color("#111111".into())).unwrap();
    rt.flush();
    assert_eq!(*fired.borrow(), vec![V::Color("#111111".into())]);
    rt.shutdown();
}

/// (settings-tables) A field whose values are TOML as the compiler
/// encodes them ([`V::Toml`]); it reads anything.
fn toml_field(name: &str) -> FieldSpec<V> {
    FieldSpec::new(
        name,
        V::Toml(String::new()),
        |item: &Item| {
            let mut doc = toml_edit::DocumentMut::new();
            doc.insert("x", item.clone());
            Ok(V::Toml(doc.to_string()))
        },
        |v: &V| match v {
            V::Toml(t) => {
                let doc: toml_edit::DocumentMut = t.parse().unwrap();
                doc["x"].clone()
            }
            _ => Item::None,
        },
    )
}

/// (settings-tables) Write `value` (the TOML of a key `x`) to `field` of
/// `file` as it reads `before`, and return the file as written.
fn write_toml(name: &str, before: &str, field: &str, value: &str) -> String {
    write_toml_among(name, before, &[field], field, value)
}

/// [`write_toml`] in a file that declares a TOML field for each of
/// `fields` (and `compact`).
fn write_toml_among(name: &str, before: &str, fields: &[&str], field: &str, value: &str) -> String {
    let tmp = TempDir::new(name);
    let file = tmp.config("prefs.toml");
    fs::write(&file, before).unwrap();
    let rt = Runtime::new();
    let store = tmp.store();
    let mut specs: Vec<FieldSpec<V>> = fields.iter().map(|f| toml_field(f)).collect();
    specs.push(flag("compact", false));
    let s = rt.settings_file(&store, &file, specs);
    rt.flush();
    let d = write(
        &rt,
        &s,
        &store,
        field,
        V::Toml(value.into()),
        Duration::from_millis(10),
    );
    assert!(settings_notices(&d).is_empty(), "{d:?}");
    let out = fs::read_to_string(&file).unwrap();
    rt.shutdown();
    out
}

const PROFILES: &str = "\
# Display profiles, kept in chezmoi
compact = true

# At the desk: the Dell on the left.
[profiles.desk]
lid = \"BOE 0x0BCA\"   # the laptop's own panel

# the big one
[[profiles.desk.mons]]
id = \"Dell U2720Q\"   # left
scale = 1.5

[[profiles.desk.mons]]
id = \"BOE 0x0BCA\"
scale = 1.25

# On the couch: just the laptop.
[profiles.couch]
[[profiles.couch.mons]]
id = \"BOE 0x0BCA\"   # alone
scale = 1
";

/// (settings-tables) A write to one record of a dictionary written as
/// sections leaves every other record's text and comments as they were,
/// byte for byte (`scale = 1` is the `1.0` written); the changed one
/// stays a `[section]` with its `[[…]]` tables, each table that reads the
/// same keeping its own text and comments.
#[test]
fn a_write_to_one_record_leaves_the_others_as_written() {
    let out = write_toml(
        "one-record",
        PROFILES,
        "profiles",
        "[x]\n\
         desk = { lid = \"BOE 0x0BCA\", mons = [{ id = \"Dell U2720Q\", scale = 1.5 }, { id = \"BOE 0x0BCA\", scale = 1.0 }] }\n\
         couch = { mons = [{ id = \"BOE 0x0BCA\", scale = 1.0 }] }\n",
    );
    assert_eq!(
        out,
        PROFILES.replace("scale = 1.25", "scale = 1.0"),
        "{out}"
    );
    // A new record among records written as sections is a section too,
    // shaped like them, after them.
    let out = write_toml(
        "new-record",
        PROFILES,
        "profiles",
        "[x]\n\
         desk = { lid = \"BOE 0x0BCA\", mons = [{ id = \"Dell U2720Q\", scale = 1.5 }, { id = \"BOE 0x0BCA\", scale = 1.25 }] }\n\
         couch = { mons = [{ id = \"BOE 0x0BCA\", scale = 1.0 }] }\n\
         tv = { lid = \"BOE 0x0BCA\", mons = [{ id = \"LG TV\", scale = 2.0 }] }\n",
    );
    assert_eq!(
        out,
        format!(
            "{PROFILES}\n[profiles.tv]\nlid = \"BOE 0x0BCA\"\n\n[[profiles.tv.mons]]\nid = \"LG TV\"\nscale = 2.0\n"
        ),
        "{out}"
    );
}

/// (settings-tables) A list of records written over `[[name]]` is merged
/// by value: an element inserted in the middle is written fresh where it
/// goes, and every other keeps the comments written on it, also when the
/// list is reordered; an element no longer there goes with its comments.
#[test]
fn an_array_of_tables_is_merged_by_value() {
    let before = "\
compact = true

# Pinned apps, in the launcher's order.
[[pins]]
id = \"firefox\"   # the browser

# the terminal
[[pins]]
id = \"kitty\"
";
    let out = write_toml(
        "aot-insert",
        before,
        "pins",
        "x = [{ id = \"firefox\" }, { id = \"files\" }, { id = \"kitty\" }]",
    );
    assert_eq!(
        out,
        "\
compact = true

# Pinned apps, in the launcher's order.
[[pins]]
id = \"firefox\"   # the browser

[[pins]]
id = \"files\"

# the terminal
[[pins]]
id = \"kitty\"
",
        "{out}"
    );
    // Reordered, one gone: each keeps its comments, in the new order.
    let out = write_toml(
        "aot-reorder",
        &out,
        "pins",
        "x = [{ id = \"kitty\" }, { id = \"firefox\" }]",
    );
    assert_eq!(
        out,
        "\
compact = true

# the terminal
[[pins]]
id = \"kitty\"

# Pinned apps, in the launcher's order.
[[pins]]
id = \"firefox\"   # the browser
",
        "{out}"
    );
    // An element that left a field to its default takes the record that
    // adds it, keeping its comment.
    let out = write_toml(
        "aot-default",
        before,
        "pins",
        "x = [{ id = \"firefox\", hidden = false }, { id = \"kitty\", hidden = false }]",
    );
    assert_eq!(
        out,
        "\
compact = true

# Pinned apps, in the launcher's order.
[[pins]]
id = \"firefox\"   # the browser
hidden = false

# the terminal
[[pins]]
id = \"kitty\"
hidden = false
",
        "{out}"
    );
    // An empty list is `pins = []`: no `[[pins]]` would be the default.
    let out = write_toml("aot-empty", before, "pins", "x = []");
    assert_eq!(
        out,
        "compact = true
pins = []
",
        "{out}"
    );
}

/// (settings-tables) An array the user spread over lines keeps its layout
/// when what it reads does not change (`1` for `1.0` included), and when
/// it does, each element that stays keeps the comment on its line.
#[test]
fn a_multi_line_array_keeps_its_layout() {
    let before = "\
compact = true
order = [
  \"DP-1\",    # left
  \"eDP-1\",   # laptop
]
";
    let out = write_toml("array-same", before, "order", "x = [\"DP-1\", \"eDP-1\"]");
    assert_eq!(out, before);
    let out = write_toml(
        "array-insert",
        before,
        "order",
        "x = [\"eDP-1\", \"HDMI-A-1\", \"DP-1\"]",
    );
    assert_eq!(
        out,
        "\
compact = true
order = [
  \"eDP-1\",   # laptop
  \"HDMI-A-1\",
  \"DP-1\",    # left
]
",
        "{out}"
    );
    // (settings-tables follow-up) One changed in place, where no other
    // was, keeps the comment on its line.
    let out = write_toml(
        "array-replace",
        before,
        "order",
        "x = [\"HDMI-A-1\", \"eDP-1\"]",
    );
    assert_eq!(out, before.replace("\"DP-1\"", "\"HDMI-A-1\""), "{out}");
    // A record nested in a section keeps its inline array as it was.
    let before = "\
[profiles.desk]
lid = \"a\"
order = [
  \"DP-1\",  # left
  \"eDP-1\",
]
";
    let out = write_toml(
        "array-nested",
        before,
        "profiles",
        "[x]\ndesk = { lid = \"b\", order = [\"DP-1\", \"eDP-1\"] }\n",
    );
    assert_eq!(out, before.replace("\"a\"", "\"b\""), "{out}");
}

/// (settings-tables) A record the user wrote as a `[section]` stays one
/// after a write, merged key by key; so does a dictionary of records,
/// each of its records a section.
#[test]
fn a_record_section_stays_a_section() {
    let before = "\
compact = true

[size]  # the panel
w = 3   # wide
h = 4
";
    let out = write_toml("record-section", before, "size", "x = { w = 5, h = 4 }");
    assert_eq!(out, before.replace("w = 3", "w = 5"), "{out}");
    let out = write_toml(
        "dict-section",
        PROFILES,
        "profiles",
        "[x]\n\
         desk = { mons = [{ id = \"Dell U2720Q\", scale = 1.5 }, { id = \"BOE 0x0BCA\", scale = 1.25 }] }\n\
         couch = { mons = [{ id = \"BOE 0x0BCA\", scale = 1.0 }] }\n",
    );
    assert!(
        out.contains("\n[profiles.desk]\n\n# the big one\n[[profiles.desk.mons]]"),
        "{out}"
    );
    assert!(!out.contains("lid"), "{out}");
}

/// (settings-tables follow-up) A hand-kept file with a header, a section
/// and `[[profiles]]` elements each with comments (above the table,
/// after a key, between keys).
const KEPT: &str = "\
# Monitor profiles, kept in chezmoi.
# The shell writes saved layouts back here.

compact = true   # a small bar

# The panel.
[size]   # in cells
w = 3    # wide

# ---- notes on the panel

# tall
h = 4

# Saved layouts, one per place.

# At the desk.
[[profiles]]
name = \"desk\"   # the docked set
# the Dell, turned
x = 0
scale = 1.5     # 4K at 150%

# On the couch.
[[profiles]]
name = \"couch\"
x = 1440        # right of the Dell

scale = 1.25

# The TV.
[[profiles]]
name = \"tv\"
x = 0
scale = 2.0
";

const DESK: &str = "{ name = \"desk\", x = 0, scale = 1.5 }";
const COUCH: &str = "{ name = \"couch\", x = 1440, scale = 1.25 }";
const TV: &str = "{ name = \"tv\", x = 0, scale = 2.0 }";

fn write_kept(name: &str, field: &str, value: &str) -> String {
    write_toml_among(name, KEPT, &["profiles", "size"], field, value)
}

/// (settings-tables follow-up) A write keeps every comment and blank line
/// outside what it changed: an element changed in a field keeps its
/// comments, keys and their order, only the value replaced; a new element
/// is written in the shape of its siblings; a removed element's own
/// comments go with it, and the file's comments above it (a blank line
/// apart) stay; a reordered list keeps the file's comments where they
/// are and each element's own with it.
#[test]
fn a_write_keeps_every_comment_it_did_not_change() {
    // One field of one element.
    let out = write_kept(
        "kept-change",
        "profiles",
        &format!("x = [{{ name = \"desk\", x = 0, scale = 1.75 }}, {COUCH}, {TV}]"),
    );
    assert_eq!(out, KEPT.replace("scale = 1.5 ", "scale = 1.75 "), "{out}");
    // One more, written like its siblings, keys in their order.
    let out = write_kept(
        "kept-add",
        "profiles",
        &format!("x = [{DESK}, {COUCH}, {TV}, {{ scale = 1.0, name = \"lab\", x = 0 }}]"),
    );
    assert_eq!(
        out,
        format!("{KEPT}\n[[profiles]]\nname = \"lab\"\nx = 0\nscale = 1.0\n"),
        "{out}"
    );
    // The first one gone: its comment goes, the file's above it stays.
    let out = write_kept("kept-remove", "profiles", &format!("x = [{COUCH}, {TV}]"));
    let removed = "\
# At the desk.
[[profiles]]
name = \"desk\"   # the docked set
# the Dell, turned
x = 0
scale = 1.5     # 4K at 150%

";
    assert_eq!(out, KEPT.replace(removed, ""), "{out}");
    // Reordered: the file's comments stay in place, each element's own
    // go with it.
    let out = write_kept(
        "kept-reorder",
        "profiles",
        &format!("x = [{TV}, {DESK}, {COUCH}]"),
    );
    let (head, _) = KEPT.split_once("# At the desk.").unwrap();
    assert_eq!(
        out,
        format!(
            "{head}\
# The TV.
[[profiles]]
name = \"tv\"
x = 0
scale = 2.0

# At the desk.
[[profiles]]
name = \"desk\"   # the docked set
# the Dell, turned
x = 0
scale = 1.5     # 4K at 150%

# On the couch.
[[profiles]]
name = \"couch\"
x = 1440        # right of the Dell

scale = 1.25
"
        ),
        "{out}"
    );
    // A key gone from a section: its own comment goes, the file's note
    // above it stays.
    let out = write_kept("kept-key", "size", "x = { w = 3 }");
    assert_eq!(out, KEPT.replace("# tall\nh = 4\n\n", ""), "{out}");
}

/// (settings-tables follow-up) The file's header above the first table
/// stays when that table changes or goes, also when the element is a
/// record of two fields that changes in one (it changed in place: no
/// other element was there, or a new one shares less with it), and when
/// a dictionary entry goes.
#[test]
fn a_header_above_the_first_table_stays() {
    let before = "\
# Profiles, kept in chezmoi.

# At the desk.
[[profiles]]
name = \"desk\"   # docked
scale = 1.5

[[profiles]]
name = \"couch\"
scale = 1.25
";
    let out = write_toml(
        "header-change",
        before,
        "profiles",
        "x = [{ name = \"desk\", scale = 1.75 }, { name = \"couch\", scale = 1.25 }]",
    );
    assert_eq!(out, before.replace("1.5", "1.75"), "{out}");
    let out = write_toml(
        "header-replace",
        before,
        "profiles",
        "x = [{ name = \"office\", scale = 1.0 }, { name = \"couch\", scale = 1.25 }]",
    );
    assert_eq!(
        out,
        before
            .replace("\"desk\"", "\"office\"")
            .replace("1.5", "1.0"),
        "{out}"
    );
    // One added before it in the same gap takes none of its comments:
    // the changed one shares more with what it was.
    let out = write_toml(
        "header-add-change",
        before,
        "profiles",
        "x = [{ name = \"lab\", scale = 2.0 }, { name = \"desk\", scale = 1.75 }, { name = \"couch\", scale = 1.25 }]",
    );
    assert_eq!(
        out,
        "\
# Profiles, kept in chezmoi.

[[profiles]]
name = \"lab\"
scale = 2.0

# At the desk.
[[profiles]]
name = \"desk\"   # docked
scale = 1.75

[[profiles]]
name = \"couch\"
scale = 1.25
",
        "{out}"
    );
    let out = write_toml(
        "header-remove",
        before,
        "profiles",
        "x = [{ name = \"couch\", scale = 1.25 }]",
    );
    assert_eq!(
        out, "# Profiles, kept in chezmoi.\n\n[[profiles]]\nname = \"couch\"\nscale = 1.25\n",
        "{out}"
    );
    let before = "\
# Display profiles, kept in chezmoi.

# At the desk.
[profiles.desk]
lid = \"a\"

# On the couch.
[profiles.couch]
lid = \"b\"
";
    let out = write_toml(
        "header-entry",
        before,
        "profiles",
        "[x]\ncouch = { lid = \"b\" }\n",
    );
    assert_eq!(
        out,
        "# Display profiles, kept in chezmoi.\n\n# On the couch.\n[profiles.couch]\nlid = \"b\"\n",
        "{out}"
    );
}

/// (settings-shape) A record that holds records, written where the file
/// has nothing to follow (no file yet, or the dictionary's first entry),
/// is written as the settings-file and monitor-profiles pages show it: a
/// `[section]` with a `[[…]]` table for each record of its list, not one
/// long inline line. The next entry takes the same shape, a record of
/// plain values stays inline, and an entry next to ones written inline
/// is inline too.
#[test]
fn a_new_record_of_records_is_written_as_sections() {
    let save = "[x]\n\"A + B\" = { mons = [{ id = \"A\", x = 0 }, { id = \"B\", x = 1920 }] }\n";
    let out = write_toml("first-save", "compact = true\n", "profiles", save);
    assert_eq!(
        out,
        "compact = true\n\
         \n\
         [profiles.\"A + B\"]\n\
         [[profiles.\"A + B\".mons]]\n\
         id = \"A\"\n\
         x = 0\n\
         \n\
         [[profiles.\"A + B\".mons]]\n\
         id = \"B\"\n\
         x = 1920\n",
        "{out}"
    );
    // The next set saved: a section like the first, which stays as it was.
    let out = write_toml(
        "second-save",
        &out,
        "profiles",
        "[x]\n\"A + B\" = { mons = [{ id = \"A\", x = 0 }, { id = \"B\", x = 1920 }] }\n\
         \"A\" = { mons = [{ id = \"A\", x = 0 }] }\n",
    );
    assert!(
        out.contains("[profiles.A]\n[[profiles.A.mons]]\nid = \"A\"\nx = 0\n"),
        "{out}"
    );
    assert!(
        out.contains("[[profiles.\"A + B\".mons]]\nid = \"B\""),
        "{out}"
    );
    assert!(!out.contains("= {"), "no inline line: {out}");
    // A record of plain values, and a record written into a file whose
    // entries are inline, stay inline.
    let out = write_toml("plain", "compact = true\n", "size", "x = { w = 5, h = 4 }");
    assert_eq!(out, "compact = true\nsize = { w = 5, h = 4 }\n", "{out}");
    let inline = "[profiles]\n\"A\" = { mons = [{ id = \"A\", x = 0 }] }\n";
    let out = write_toml(
        "beside-inline",
        inline,
        "profiles",
        "[x]\n\"A\" = { mons = [{ id = \"A\", x = 0 }] }\n\"B\" = { mons = [{ id = \"B\", x = 0 }] }\n",
    );
    assert!(out.contains("\nB = { mons = [{ id = \"B\""), "{out}");
}

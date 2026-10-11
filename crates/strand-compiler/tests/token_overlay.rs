//! The token overlay layer (S-ipc; docs/architecture.md, "The token
//! overlay layer"; decisions.md m5-owner Q10) and the instance's other
//! IPC entry points: `toggle`, a settings field's layer and `clear`.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, SceneMirror, Storage, TOKENS_FILE, Update};
use strand_compiler::reconcile::{Build, EditClass};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::{Diagnostic, Runtime, SettingsIssue, SettingsLayer};
use strand_scene::tokens::OVERLAY_ORIGIN;
use strand_scene::{Color, PropValue, SceneOp, Transition};

struct Shell {
    _rt: Runtime,
    inst: Instance,
    scene: SceneMirror,
    build: Build,
}

fn compile(prev: Option<&Build>, files: &[(&str, &str)]) -> Build {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.to_string());
    }
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

fn boot(files: &[(&str, &str)], storage: Storage) -> (Shell, Update) {
    let build = compile(None, files);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::from_build(&rt, &build, host, storage);
    let mut shell = Shell {
        _rt: rt,
        inst,
        scene: SceneMirror::new(),
        build,
    };
    let u = shell.flush();
    (shell, u)
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
        u
    }

    fn reload(&mut self, files: &[(&str, &str)]) -> (Vec<EditClass>, Update) {
        let build = compile(Some(&self.build), files);
        let report = self.inst.reload(&build);
        self.build = build;
        (report.classes, self.flush())
    }

    /// The scene's token, as render evaluates it.
    fn token(&self, path: &str) -> Option<PropValue> {
        self.scene.tokens.lookup(path)
    }

    fn origin(&self, path: &str) -> Option<&str> {
        self.scene.tokens.origin(path)
    }
}

fn hex(s: &str) -> PropValue {
    PropValue::Color(Color::from_hex(s).unwrap())
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "strand-token-overlay-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A bar painted with `$probe.c`, defined as `color` in `tokens base`,
/// and a gap token read as a length.
fn shell(color: &str) -> String {
    format!(
        "tokens base {{ probe.c: {color}; probe.gap: 4px; probe.speed: 150ms }}\n\
         export state open = false\n\
         export state label = \"hi\"\n\
         bar Top {{\n  edge: top; height: 40\n  bg: $probe.c\n  text label\n}}\n"
    )
}

/// The shadowed-save notices of an update, as the overlay shows them.
fn shadowed(u: &Update) -> Vec<String> {
    u.diagnostics
        .iter()
        .filter_map(|d| match d {
            Diagnostic::Settings(n) if n.issue == SettingsIssue::Shadowed => Some(n.to_string()),
            _ => None,
        })
        .collect()
}

fn tokens_toml(state: &Path) -> String {
    std::fs::read_to_string(state.join(TOKENS_FILE)).unwrap_or_default()
}

/// `strand set '$probe.c' '#ff8800'`: the overlay wins over the file's
/// tier, takes the swap path (a `SetTokens` that springs), records
/// `OVERLAY_ORIGIN`, is kept in `tokens.toml` and comes back at the next
/// boot; `strand clear` puts the file's value and origin back.
#[test]
fn a_token_overlay_wins_springs_and_persists() {
    let dir = temp_dir("wins");
    let state = dir.join("state");
    let storage = Storage::in_dirs(&state, &dir);
    let src = shell("#112233");
    let (mut sh, _) = boot(&[("bar.strand", &src)], storage.clone());
    assert_eq!(sh.token("probe.c"), Some(hex("#112233")));
    assert_eq!(sh.origin("probe.c"), Some("tokens base"));

    sh.inst.set_token_overlay("probe.c", " #ff8800 ").unwrap();
    let u = sh.flush();
    assert!(
        u.diff.ops.iter().any(|op| matches!(
            op,
            SceneOp::SetTokens {
                transition: Transition::Default,
                ..
            }
        )),
        "the swap path: {:?}",
        u.diff.ops
    );
    assert_eq!(sh.token("probe.c"), Some(hex("#ff8800")));
    assert_eq!(sh.origin("probe.c"), Some(OVERLAY_ORIGIN));
    assert_eq!(
        sh.inst.token("probe.c"),
        Some((hex("#ff8800"), Some(OVERLAY_ORIGIN.to_string())))
    );
    // A length and a duration, read as `strand set` reads them.
    sh.inst.set_token_overlay("probe.gap", "8px").unwrap();
    sh.inst.set_token_overlay("probe.speed", "0.2s").unwrap();
    sh.flush();
    assert_eq!(sh.token("probe.gap"), Some(PropValue::Number(8.0)));
    assert_eq!(
        sh.token("probe.speed"),
        Some(PropValue::Duration(Duration::from_millis(200)))
    );
    assert!(storage.tokens.sync(Duration::from_secs(10)));
    let file = tokens_toml(&state);
    assert!(file.contains("\"probe.c\" = \"#ff8800\""), "{file}");
    assert!(file.contains("\"probe.gap\" = \"8px\""), "{file}");

    // A fresh boot on the same state reads it back.
    drop(sh);
    let again = Storage::in_dirs(&state, &dir);
    let (mut sh, _) = boot(&[("bar.strand", &src)], again.clone());
    assert_eq!(sh.token("probe.c"), Some(hex("#ff8800")));
    assert_eq!(sh.origin("probe.c"), Some(OVERLAY_ORIGIN));

    // `strand clear '$probe.c'`: the file's value and origin again.
    assert!(sh.inst.clear_token_overlay("probe.c").unwrap());
    assert!(
        !sh.inst.clear_token_overlay("probe.c").unwrap(),
        "nothing left"
    );
    let u = sh.flush();
    assert!(
        u.diff
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::SetTokens { .. }))
    );
    assert_eq!(sh.token("probe.c"), Some(hex("#112233")));
    assert_eq!(sh.origin("probe.c"), Some("tokens base"));
    assert!(again.tokens.sync(Duration::from_secs(10)));
    let file = tokens_toml(&state);
    assert!(!file.contains("probe.c"), "{file}");
    assert!(
        file.contains("probe.gap"),
        "the other overlay stays: {file}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// What the overlay refuses: an unknown token (with the closest name)
/// and a value its type does not read. Nothing changes.
#[test]
fn a_bad_token_overlay_is_refused() {
    let src = shell("#112233");
    let (mut sh, _) = boot(&[("bar.strand", &src)], Storage::none());
    let err = sh
        .inst
        .set_token_overlay("probe.cc", "#ff8800")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no token `$probe.cc`; did you mean `$probe.c`?"),
        "{err}"
    );
    let err = sh
        .inst
        .set_token_overlay("probe.c", "orange")
        .unwrap_err()
        .to_string();
    assert!(err.contains("`$probe.c`: `orange` is not a color"), "{err}");
    let err = sh
        .inst
        .set_token_overlay("probe.gap", "wide")
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not a length"), "{err}");
    let u = sh.flush();
    assert!(
        !u.diff
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::SetTokens { .. }))
    );
    assert_eq!(sh.token("probe.c"), Some(hex("#112233")));
}

/// A save that changes the file's value of an overlaid token keeps the
/// overlay and raises settings' notice, `probe.c: file changed but
/// runtime overlay wins [clear]`; a save that leaves it alone does not.
/// The notice's `[clear]` (`clear_settings_overlay` on the overlay file)
/// drops the overlay, and the file's new value applies.
#[test]
fn a_shadowed_save_says_the_overlay_wins_and_clear_drops_it() {
    let dir = temp_dir("shadowed");
    let state = dir.join("state");
    let storage = Storage::in_dirs(&state, &dir);
    let (mut sh, _) = boot(&[("bar.strand", &shell("#112233"))], storage.clone());
    sh.inst.set_token_overlay("probe.c", "#ff8800").unwrap();
    sh.flush();
    // Another edit: no notice.
    let other = shell("#112233").replace("\"hi\"", "\"ho\"");
    let (_, u) = sh.reload(&[("bar.strand", &other)]);
    assert_eq!(shadowed(&u), Vec::<String>::new());
    // The overlaid token's own value changes: the overlay wins, said once.
    let (classes, u) = sh.reload(&[("bar.strand", &shell("#445566"))]);
    assert_eq!(
        shadowed(&u),
        ["probe.c: file changed but runtime overlay wins [clear]"]
    );
    assert!(
        !classes.contains(&EditClass::Token),
        "the scene's table did not change: {classes:?}"
    );
    assert_eq!(sh.token("probe.c"), Some(hex("#ff8800")));
    assert_eq!(shadowed(&sh.flush()), Vec::<String>::new(), "once");
    // The notice names the overlay file: its `[clear]` drops the overlay.
    let file = state.join(TOKENS_FILE).to_string_lossy().into_owned();
    let Some(Diagnostic::Settings(n)) = u.diagnostics.first() else {
        panic!("{:?}", u.diagnostics);
    };
    assert_eq!(&*n.file, file);
    assert!(sh.inst.clear_settings_overlay(&file, "probe.c"));
    sh.flush();
    assert_eq!(sh.token("probe.c"), Some(hex("#445566")));
    assert_eq!(sh.origin("probe.c"), Some("tokens base"));
    let _ = std::fs::remove_dir_all(dir);
}

/// `tokens.toml` is Strand's own file: one that is not valid TOML is
/// moved aside with a notice and the overlay starts empty; an entry the
/// token's type does not read (a hand edit) leaves the theme's value,
/// said once; a comment the user added survives a write.
#[test]
fn a_broken_overlay_file_is_moved_aside_and_bad_entries_are_said() {
    let dir = temp_dir("broken");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join(TOKENS_FILE), "probe.c = \n").unwrap();
    let src = shell("#112233");
    let (sh, u) = boot(&[("bar.strand", &src)], Storage::in_dirs(&state, &dir));
    let said: Vec<String> = u.diagnostics.iter().map(|d| format!("{d:?}")).collect();
    assert!(
        u.diagnostics.iter().any(|d| matches!(
            d,
            Diagnostic::Settings(n) if matches!(n.issue, SettingsIssue::CorruptOverlay { .. })
        )),
        "{said:?}"
    );
    assert!(state.join(format!(".{TOKENS_FILE}.corrupt")).exists());
    assert_eq!(sh.token("probe.c"), Some(hex("#112233")));
    drop(sh);

    std::fs::write(
        state.join(TOKENS_FILE),
        "# mine\n\"probe.c\" = \"orange\"\n\"probe.gap\" = 6\n",
    )
    .unwrap();
    let storage = Storage::in_dirs(&state, &dir);
    let (mut sh, u) = boot(&[("bar.strand", &src)], storage.clone());
    assert_eq!(sh.token("probe.c"), Some(hex("#112233")));
    assert_eq!(
        sh.token("probe.gap"),
        Some(PropValue::Number(6.0)),
        "a number"
    );
    assert_eq!(
        u.notices,
        [
            "$probe.c: runtime overlay `orange` is not a color; the theme's value applies \
          (strand clear '$probe.c')"
        ]
    );
    sh.inst.set_token_overlay("probe.c", "#00ff00").unwrap();
    let u = sh.flush();
    assert!(u.notices.is_empty(), "{:?}", u.notices);
    assert!(storage.tokens.sync(Duration::from_secs(10)));
    let file = tokens_toml(&state);
    assert!(file.starts_with("# mine\n"), "{file}");
    assert!(file.contains("\"probe.c\" = \"#00ff00\""), "{file}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `strand toggle`: an exported bool flips; anything else is refused,
/// naming its type; a settings field's layer and `strand clear` on one.
#[test]
fn toggle_flips_booleans_only_and_settings_fields_have_layers() {
    let dir = temp_dir("toggle");
    std::fs::write(dir.join("prefs.toml"), "compact = true\n").unwrap();
    let src = format!(
        "{}state prefs from \"prefs.toml\" {{ compact: bool = false; gap: int = 4 }}\n",
        shell("#112233")
    );
    let (mut sh, _) = boot(
        &[("bar.strand", &src)],
        Storage::in_dirs(dir.join("state"), &dir),
    );
    assert!(sh.inst.toggle("bar.open").unwrap());
    assert_eq!(sh.inst.get("bar.open").unwrap(), Value::Bool(true));
    assert!(!sh.inst.toggle("bar.open").unwrap());
    let err = sh.inst.toggle("bar.label").unwrap_err().to_string();
    assert!(
        err.contains("`bar.label` is a text, not a bool: `toggle` flips booleans only"),
        "{err}"
    );
    assert!(sh.inst.toggle("bar.nothing").is_err());
    // A settings field toggles too, and says where its value comes from.
    assert_eq!(sh.inst.layer("prefs.compact"), Some(SettingsLayer::File));
    assert_eq!(sh.inst.layer("prefs.gap"), Some(SettingsLayer::Default));
    assert_eq!(sh.inst.layer("bar.open"), None, "not a settings field");
    assert!(!sh.inst.toggle("prefs.compact").unwrap());
    sh.flush();
    assert_eq!(sh.inst.get("prefs.compact").unwrap(), Value::Bool(false));
    // No runtime overlay to clear: not an error.
    assert!(!sh.inst.clear_overlay("prefs.gap").unwrap());
    let err = sh.inst.clear_overlay("bar.open").unwrap_err().to_string();
    assert!(err.contains("not a settings field or a token"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `strand clear '$probe.cc'`: an unknown token is refused with the
/// closest name, as `set` refuses it; a known token without an overlay
/// clears to `false`; an overlay of a token the build no longer defines
/// is still cleared.
#[test]
fn clearing_an_unknown_token_is_refused_with_the_closest_name() {
    let dir = temp_dir("clear-unknown");
    let state = dir.join("state");
    let storage = Storage::in_dirs(&state, &dir);
    let (mut sh, _) = boot(&[("bar.strand", &shell("#112233"))], storage.clone());
    sh.inst.set_token_overlay("probe.c", "#ff8800").unwrap();
    sh.flush();
    let err = sh
        .inst
        .clear_token_overlay("probe.cc")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("no token `$probe.cc`; did you mean `$probe.c`?"),
        "{err}"
    );
    assert_eq!(sh.token("probe.c"), Some(hex("#ff8800")), "still overlaid");
    assert!(!sh.inst.clear_token_overlay("probe.gap").unwrap());

    // A build without `probe.c`: its overlay is kept, and clears.
    let gone = shell("#112233").replace("probe.c: #112233; ", "");
    let gone = gone.replace("bg: $probe.c", "bg: #000000");
    sh.reload(&[("bar.strand", &gone)]);
    assert_eq!(sh.token("probe.c"), None);
    assert!(sh.inst.clear_token_overlay("probe.c").unwrap());
    assert!(storage.tokens.sync(Duration::from_secs(10)));
    assert!(!tokens_toml(&state).contains("probe.c"));
    let _ = std::fs::remove_dir_all(dir);
}

/// A `tokens.toml` that is there but cannot be read (here: not UTF-8)
/// is said once and never written over: `set` and `clear` are refused,
/// the file keeps every overlay it held, and once it reads again they
/// apply and the next `set` keeps them.
#[test]
fn an_unreadable_overlay_file_is_not_written_over() {
    let dir = temp_dir("unreadable");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let file = state.join(TOKENS_FILE);
    let mut held = b"\"probe.gap\" = \"6px\"\n# \xff\n".to_vec();
    std::fs::write(&file, &held).unwrap();
    let storage = Storage::in_dirs(&state, &dir);
    let (mut sh, u) = boot(&[("bar.strand", &shell("#112233"))], storage.clone());
    assert!(
        u.diagnostics.iter().any(|d| matches!(
            d,
            Diagnostic::Settings(n) if matches!(n.issue, SettingsIssue::Unreadable(_))
        )),
        "{:?}",
        u.diagnostics
    );
    let err = sh
        .inst
        .set_token_overlay("probe.c", "#ffffff")
        .unwrap_err()
        .to_string();
    assert!(err.contains("could not be read"), "{err}");
    let err = sh
        .inst
        .clear_token_overlay("probe.gap")
        .unwrap_err()
        .to_string();
    assert!(err.contains("could not be read"), "{err}");
    let u = sh.flush();
    assert!(
        !u.diagnostics.iter().any(|d| matches!(
            d,
            Diagnostic::Settings(n) if matches!(n.issue, SettingsIssue::Unreadable(_))
        )),
        "said once: {:?}",
        u.diagnostics
    );
    assert!(storage.tokens.sync(Duration::from_secs(10)));
    assert_eq!(std::fs::read(&file).unwrap(), held, "not written over");

    // Fixed by hand: the next `set` reads it, and keeps what it held.
    held.truncate(held.len() - 4);
    std::fs::write(&file, &held).unwrap();
    sh.inst.set_token_overlay("probe.c", "#ffffff").unwrap();
    sh.flush();
    assert_eq!(sh.token("probe.gap"), Some(PropValue::Number(6.0)));
    assert_eq!(sh.token("probe.c"), Some(hex("#ffffff")));
    assert!(storage.tokens.sync(Duration::from_secs(10)));
    let text = tokens_toml(&state);
    assert!(text.contains("\"probe.gap\" = \"6px\""), "{text}");
    assert!(text.contains("\"probe.c\" = \"#ffffff\""), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

/// An entry that is not a token's value is said, not dropped silently:
/// an unquoted dotted key (`probe.gap = "8px"` makes a table) is told to
/// quote the path, and an array is named.
#[test]
fn an_entry_that_is_not_a_value_is_said() {
    let dir = temp_dir("table");
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join(TOKENS_FILE),
        "probe.gap = \"8px\"\nlist = [1, 2]\n",
    )
    .unwrap();
    let (mut sh, u) = boot(
        &[("bar.strand", &shell("#112233"))],
        Storage::in_dirs(&state, &dir),
    );
    assert_eq!(sh.token("probe.gap"), Some(PropValue::Number(4.0)));
    assert_eq!(u.notices.len(), 2, "{:?}", u.notices);
    assert!(
        u.notices[0].contains(
            "`probe` is a table, not a token's value, and is ignored; quote a token's \
             path as its key (\"probe.gap\" = ...)"
        ),
        "{:?}",
        u.notices
    );
    assert!(
        u.notices[1].contains("`list` is not text, a number or a bool"),
        "{:?}",
        u.notices
    );
    assert!(sh.flush().notices.is_empty(), "said once");
    let _ = std::fs::remove_dir_all(dir);
}

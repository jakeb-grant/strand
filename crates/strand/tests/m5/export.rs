//! S-tools: `strand export json | base24 | --template <file>` (decisions.md
//! m5-tools, "the CLI").
//!
//! Offline (no shell on the socket): the config mounted on read-only
//! storage, `system` seeded from `palettes/system`, the persisted palette
//! pinned, the overlays applied, the state directory left byte-identical;
//! `--palette` without any config; `-o` atomic; a broken theme says "no
//! palette" with its cause and never falls back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A config, state and runtime directory for one test; no shell runs.
pub(super) struct Home {
    pub dir: PathBuf,
}

impl Home {
    pub fn new(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("strand-export-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["config/strand", "state/strand/palettes", "run"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        Home { dir }
    }

    pub fn state(&self) -> PathBuf {
        self.dir.join("state/strand")
    }

    pub fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    /// `strand export <args>` with this home's directories and a socket
    /// nobody listens on (or `socket`).
    pub fn export(&self, args: &[&str], socket: Option<&Path>) -> Output {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("export")
            .args(args)
            .current_dir(&self.dir)
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env(
                "STRAND_SOCKET",
                socket.map_or_else(|| self.dir.join("run/none.sock"), Path::to_path_buf),
            )
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap()
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.export(args, None);
        assert!(
            out.status.success(),
            "strand export {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn err(&self, args: &[&str]) -> String {
        let out = self.export(args, None);
        assert!(!out.status.success(), "strand export {args:?} succeeded");
        assert!(out.stdout.is_empty());
        String::from_utf8(out.stderr).unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// design.md's theme (its settings file in the config directory).
pub(super) fn theme() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../strand-compiler/tests/fixtures/theme.strand"
    ))
    .unwrap()
    .replace("~/.config/strand/prefs.toml", "prefs.toml")
}

/// A template over both modes, the extras and the format functions.
pub(super) const TEMPLATE: &str = "/* {{ $palette.name }} ({{ $palette.source }}) */\n\
     :root { --accent: {{ $accent }}; --fg: {{ $fg }}; \
     --glow: {{ hex($accent.alpha(0.4), alpha: true) }}; --red: {{ $red }} }\n\
     @media (prefers-color-scheme: light) { :root { --surface: {{ $surface.light() ?? $surface }} } }\n\
     @media (prefers-color-scheme: dark) { :root { --surface: {{ $surface.dark() ?? $surface }} } }\n";

/// Every file under `dir`, with its bytes.
pub(super) fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

pub(super) fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The config mounted offline: `system.dark` from `palettes/system`, the
/// overlay file, both modes in a template; the state directory
/// byte-identical after every export.
#[test]
fn offline_exports_read_the_state_and_write_nothing() {
    let home = Home::new("offline");
    home.write("config/strand/theme.strand", &theme());
    home.write("config/strand/prefs.toml", "accent = \"#7aa2f7\"\n");
    home.write("state/strand/palettes/system", "dark=true,prefer-dark\n");
    home.write("t.css.tmpl", TEMPLATE);
    let before = snapshot(&home.state());
    // `look = auto` follows the kept `system.dark`.
    let j = json(&home.ok(&["json"]));
    assert_eq!(j["dark"], true, "{j}");
    assert!(j["roles"]["accent"].as_str().unwrap().starts_with('#'));
    // `$fg.muted` is translucent: `#rrggbbaa`.
    assert_eq!(j["tokens"]["fg.muted"].as_str().unwrap().len(), 9, "{j}");
    let css = home.ok(&["--template", "t.css.tmpl"]);
    // Both modes, so the two blocks differ.
    let surfaces: Vec<&str> = css
        .lines()
        .filter_map(|l| l.split("--surface: ").nth(1))
        .collect();
    assert_eq!(surfaces.len(), 2, "{css}");
    assert_ne!(surfaces[0], surfaces[1], "{css}");
    let b24 = home.ok(&["base24"]);
    assert!(
        b24.contains("system: \"base24\"") && b24.contains("variant: \"dark\""),
        "{b24}"
    );
    assert_eq!(
        snapshot(&home.state()),
        before,
        "an offline export wrote state"
    );
    // The token overlay file is applied, to the roles too.
    home.write("state/strand/tokens.toml", "fg = \"#ffffff\"\n");
    let state = snapshot(&home.state());
    let j = json(&home.ok(&["json"]));
    assert_eq!(j["tokens"]["fg"], "#ffffff", "{j}");
    assert_eq!(j["roles"]["fg"], "#ffffff", "{j}");
    assert_eq!(snapshot(&home.state()), state);
}

/// The persisted palette is pinned: the export writes the palette on
/// screen, not a fresh solve, and still reads the config's tokens.
#[test]
fn the_persisted_palette_is_pinned() {
    let home = Home::new("pinned");
    home.write("config/strand/theme.strand", &theme());
    home.write("config/strand/prefs.toml", "accent = \"#7aa2f7\"\n");
    let latte = strand_theme::import("catppuccin:latte", None).unwrap();
    home.write("state/strand/palettes/palette", &latte.to_text());
    let before = snapshot(&home.state());
    let j = json(&home.ok(&["json"]));
    assert_eq!(j["source"], "catppuccin:latte", "{j}");
    assert_eq!(j["dark"], false);
    assert_eq!(
        j["roles"]["accent"],
        strand_theme::export::hex(latte.get(strand_theme::Role::Accent))
    );
    // `surface.hi` is the theme's derived token over the pinned palette.
    assert!(j["tokens"]["surface.hi"].is_string(), "{j}");
    assert_eq!(snapshot(&home.state()), before);
}

/// `--palette` mounts no config; `-o` writes the file and nothing to
/// stdout; a template's error carries its file, line and caret and
/// nothing is written.
#[test]
fn palette_expressions_and_output_files() {
    let home = Home::new("palette");
    // A broken config is never read with `--palette`.
    home.write("config/strand/theme.strand", "let = \n");
    home.write(
        "t.tmpl",
        "{{ $palette.name }} {{ $surface.light() ?? \"one mode\" }}\n",
    );
    let mocha = "import(\"catppuccin:mocha\")";
    let out = home.ok(&["--template", "t.tmpl", "--palette", mocha]);
    assert_eq!(out, "Catppuccin Mocha one mode\n");
    // Both modes from a palette that reads `system.dark`.
    let out = home.ok(&[
        "--template",
        "t.tmpl",
        "--palette",
        "material(seed: #ff8800, dark: system.dark)",
    ]);
    assert!(!out.contains("one mode"), "{out}");
    // `-o`: atomic, stdout empty.
    let target = home.dir.join("out/mocha.json");
    let to = target.to_str().unwrap();
    assert_eq!(home.ok(&["json", "--palette", mocha, "-o", to]), "");
    let j = json(&std::fs::read_to_string(&target).unwrap());
    assert_eq!(j["source"], "catppuccin:mocha");
    // A broken template: located, and the target is left as it was.
    home.write("bad.tmpl", "ok\n{{ $acent }}\n");
    let err = home.err(&["--template", "bad.tmpl", "--palette", mocha, "-o", to]);
    assert!(
        err.contains("bad.tmpl:2:4") && err.contains("$accent"),
        "{err}"
    );
    let j = json(&std::fs::read_to_string(&target).unwrap());
    assert_eq!(j["source"], "catppuccin:mocha");
    // A bad expression is reported against `--palette`.
    let err = home.err(&["json", "--palette", "import("]);
    assert!(err.contains("--palette"), "{err}");
    // As a `file` element writes (spec 9.7): a link is followed and
    // kept, and the target keeps its mode; a dangling link is refused.
    use std::os::unix::fs::PermissionsExt as _;
    let real = home.dir.join("dots/base24.yaml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, "old\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    let link = home.dir.join("out/base24.yaml");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let to = link.to_str().unwrap();
    assert_eq!(home.ok(&["base24", "--palette", mocha, "-o", to]), "");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is kept"
    );
    assert!(std::fs::read_to_string(&real).unwrap().contains("base00"));
    let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    let dangling = home.dir.join("out/gone.yaml");
    std::os::unix::fs::symlink(home.dir.join("nowhere/x.yaml"), &dangling).unwrap();
    let err = home.err(&["json", "--palette", mocha, "-o", dangling.to_str().unwrap()]);
    assert!(err.contains("link to a missing file"), "{err}");
    assert!(!home.dir.join("nowhere").exists());
}

/// A broken theme is "no palette" with its cause, never the built-in
/// palette; so is a config that does not compile.
#[test]
fn a_broken_theme_has_no_palette() {
    let home = Home::new("broken");
    home.write(
        "config/strand/theme.strand",
        "use palette import(\"catppuccin:nope\")\n",
    );
    let err = home.err(&["json"]);
    assert!(
        err.contains("strand export: no palette: run strand once, or fix the theme"),
        "{err}"
    );
    assert!(err.contains("nope"), "{err}");
    // A template too: never the built-in palette the token table falls
    // back to, from the config or from `--palette`.
    let tmpl = home.write("t.tmpl", "{{ $accent }}\n");
    let tmpl = tmpl.to_str().unwrap();
    let err = home.err(&["--template", tmpl]);
    assert!(err.contains("no palette") && err.contains("nope"), "{err}");
    let err = home.err(&[
        "--template",
        tmpl,
        "--palette",
        "import(\"catppuccin:nope\")",
    ]);
    assert!(err.contains("no palette") && err.contains("nope"), "{err}");
    home.write(
        "config/strand/theme.strand",
        "use palette import(\"base16:missing.yaml\")\n",
    );
    let err = home.err(&["--template", tmpl]);
    assert!(
        err.contains("no palette") && err.contains("missing"),
        "{err}"
    );
    home.write("config/strand/theme.strand", "let x: Int = \"a\"\n");
    let err = home.err(&["base24"]);
    assert!(
        err.contains("no palette") && err.contains("strand check"),
        "{err}"
    );
    let err = home.err(&["yaml"]);
    assert!(err.contains("usage: strand export"), "{err}");
}

/// A bar that reads the theme, with the `bar.label` the desk waits for.
const BAR: &str = "export state label = \"hi\"\n\
                   bar Top {\n  edge: top; height: 40\n  bg: $surface\n  text label\n}\n";

/// The running shell answers over the socket, overlays included, and
/// agrees with the offline mount of the same config and state for every
/// format, both modes included; a template error is located the same
/// way; with the shell gone the CLI mounts offline.
#[test]
fn the_running_shell_answers_and_agrees_with_offline() {
    if !super::cli::tools() {
        return;
    }
    let prefs = "accent = \"#7aa2f7\"\n";
    let mut desk = super::cli::Desk::start(
        "export",
        &[
            ("bar.strand", BAR),
            ("theme.strand", &theme()),
            ("prefs.toml", prefs),
        ],
        &[],
    );
    let root = desk.root().to_path_buf();
    // `$XDG_CONFIG_HOME/strand` is the desk's config, for offline.
    std::fs::create_dir_all(root.join("xdg")).unwrap();
    std::os::unix::fs::symlink(root.join("config"), root.join("xdg/strand")).unwrap();
    std::fs::write(root.join("t.css.tmpl"), TEMPLATE).unwrap();
    std::fs::write(root.join("bad.tmpl"), "ok\n{{ $acent }}\n").unwrap();
    desk.ok(&["set", "$accent", "#ff0000"]);
    let offline = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("export")
            .args(args)
            .current_dir(&root)
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("STRAND_SOCKET", root.join("nobody.sock"))
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap()
    };
    let live = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("export")
            .args(args)
            .current_dir(&root)
            .env("XDG_CONFIG_HOME", root.join("nowhere"))
            .env("XDG_STATE_HOME", root.join("nowhere"))
            .env("STRAND_SOCKET", root.join("s.sock"))
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap()
    };
    let text = |o: Output| {
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    };
    // The overlay is in the live table: the shell has it at once.
    let j = json(&text(live(&["json"])));
    assert_eq!(j["roles"]["accent"], "#ff0000", "{j}");
    // Offline reads the overlay file once its writer has written it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_to_string(root.join("state/strand/tokens.toml"))
        .is_ok_and(|t| t.contains("ff0000"))
    {
        assert!(std::time::Instant::now() < deadline, "tokens.toml");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for args in [&["json"][..], &["base24"], &["--template", "t.css.tmpl"]] {
        let (a, b) = (text(live(args)), text(offline(args)));
        assert_eq!(a, b, "strand export {args:?}: IPC and offline disagree");
    }
    let css = text(live(&["--template", "t.css.tmpl"]));
    assert!(css.contains("--accent: #ff0000"), "{css}");
    // Errors are the template's, located, from the shell too.
    let err = live(&["--template", "bad.tmpl"]);
    assert!(!err.status.success());
    let err = String::from_utf8(err.stderr).unwrap();
    assert!(err.contains("bad.tmpl:2:4"), "{err}");
    // With the shell gone, the CLI mounts offline.
    let before = text(live(&["json"]));
    desk.stop();
    let after = live(&["json"]);
    // `nowhere` has no config: offline says why.
    assert!(!after.status.success());
    assert!(String::from_utf8_lossy(&after.stderr).contains("no palette"));
    assert_eq!(text(offline(&["json"])), before);
}

/// Waits (bounded) for `f` to hold.
fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn lines(p: &Path) -> usize {
    std::fs::read_to_string(p).map_or(0, |t| t.lines().count())
}

/// `file` outputs on a running shell: written at boot, once per change
/// (and `run:` once per write, 200 ms after it), nothing for a change
/// that leaves the text as it was; a template edit re-renders; an
/// unowned target and a failing `run:` are notices, the file kept.
#[test]
fn file_outputs_follow_the_theme() {
    if !super::cli::tools() {
        return;
    }
    let base = std::env::temp_dir().join(format!("strand-fo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let runs = base.join("runs.log");
    let theme = format!(
        "permit exec\n\
         export state flip = false\n\
         export state other = 1\n\
         use palette material(seed: #7aa2f7, dark: flip ? !system.dark : system.dark)\n\
         file \"out/accent.txt\" {{ text: $accent; run: [\"sh\", \"-c\", \"echo x >> {runs}\"] }}\n\
         file \"out/both.css\" {{ template: \"both.tmpl\" }}\n\
         file \"mine.txt\" {{ text: $fg }}\n\
         file \"out/fails.txt\" {{ text: $fg; run: [\"sh\", \"-c\", \"echo boom >&2; exit 3\"] }}\n",
        runs = runs.display()
    );
    let mut desk = super::cli::Desk::start(
        "files",
        &[
            ("bar.strand", BAR),
            ("theme.strand", &theme),
            (
                "both.tmpl",
                "{{ $surface }} {{ $surface.dark() ?? $surface }}\n",
            ),
            ("mine.txt", "the user's\n"),
        ],
        &[],
    );
    let (_watch, events) = desk.watch();
    let config = desk.root().join("config");
    let accent = config.join("out/accent.txt");
    until("the boot write", || accent.exists());
    until("the boot run", || lines(&runs) == 1);
    let first = std::fs::read_to_string(&accent).unwrap();
    assert!(first.starts_with('#'), "{first}");
    let both = std::fs::read_to_string(config.join("out/both.css")).unwrap();
    let parts: Vec<&str> = both.split_whitespace().collect();
    assert_eq!(parts.len(), 2, "{both}");
    assert_ne!(
        parts[0], parts[1],
        "the dark mode is the other mode: {both}"
    );
    // The flip: written once, run once.
    desk.ok(&["set", "theme.flip", "true"]);
    until("the flip's write", || {
        std::fs::read_to_string(&accent).is_ok_and(|t| t != first)
    });
    until("the flip's run", || lines(&runs) == 2);
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert_eq!(lines(&runs), 2, "one run per write");
    // A change the outputs do not read: nothing written, nothing run.
    let stamp = std::fs::metadata(&accent).unwrap().modified().unwrap();
    desk.ok(&["set", "theme.other", "2"]);
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert_eq!(
        std::fs::metadata(&accent).unwrap().modified().unwrap(),
        stamp
    );
    assert_eq!(lines(&runs), 2);
    // A template edit re-renders.
    desk.save("both.tmpl", "edited {{ $accent }}\n");
    until("the template edit", || {
        std::fs::read_to_string(config.join("out/both.css"))
            .is_ok_and(|t| t.starts_with("edited #"))
    });
    // The unowned target is refused and kept; the failing run is a
    // notice with its stderr; its file is written.
    assert_eq!(
        std::fs::read_to_string(config.join("mine.txt")).unwrap(),
        "the user's\n"
    );
    assert!(config.join("out/fails.txt").exists());
    let mut heard_refused = false;
    let mut heard_boom = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !(heard_refused && heard_boom) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let Ok(ev) = events.recv_timeout(left) else {
            break;
        };
        for n in ev["notices"].as_array().into_iter().flatten() {
            let n = n.as_str().unwrap_or_default();
            heard_refused |= n.contains("mine.txt") && n.contains("did not write");
            heard_boom |= n.contains("fails.txt") && n.contains("boom");
        }
    }
    assert!(heard_refused && heard_boom, "{}", desk.log_text());
    desk.stop();
    let _ = std::fs::remove_dir_all(&base);
}

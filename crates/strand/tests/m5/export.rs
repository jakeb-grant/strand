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
    home.write("config/strand/theme.strand", "let x: Int = \"a\"\n");
    let err = home.err(&["base24"]);
    assert!(
        err.contains("no palette") && err.contains("strand check"),
        "{err}"
    );
    let err = home.err(&["yaml"]);
    assert!(err.contains("usage: strand export"), "{err}");
}

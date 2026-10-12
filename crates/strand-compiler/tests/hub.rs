//! (M5) The theming hub's compiler side (decisions.md m5-tools, "the
//! owner-approved theming design"): the palette with no fallback, the
//! other mode (its `let`s evaluated again, overlays in both modes, what
//! only it reads tracked, nothing written), and the offline mount's
//! storage, which leaves the state directory byte-identical.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use strand_compiler::instantiate::theme::other_differs;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::template::Template;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{FileId, SourceMap};
use strand_core::Runtime;
use strand_scene::{Color, PropValue, TokenTable};
use strand_theme::{Options, Role, from_seed};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// design.md's theme with its settings file in the config directory.
fn theme() -> String {
    fixture("theme.strand").replace("~/.config/strand/prefs.toml", "prefs.toml")
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-hub-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn boot(files: &[(&str, String)], storage: Storage, dark: bool) -> Shell {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.clone());
    }
    let build = Build::compile(None, map).unwrap_or_else(|d| panic!("{d:#?}"));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    host.set(&rt, "system.dark", Value::Bool(dark)).unwrap();
    let inst = Instance::from_build(&rt, &build, host.clone(), storage);
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    Shell { rt, host, inst }
}

impl Shell {
    fn flush(&self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    }

    fn portal_dark(&self, dark: bool) {
        self.host
            .set(&self.rt, "system.dark", Value::Bool(dark))
            .unwrap();
        self.flush();
    }

    fn look(&self, v: &str) {
        let look = self.host.variant("Look", v);
        self.inst.set("theme.look", look).unwrap();
        self.flush();
    }

    fn table(&self) -> TokenTable {
        self.inst.tokens_now().unwrap()
    }
}

fn color(t: &TokenTable, path: &str) -> Color {
    match t.lookup(path) {
        Some(PropValue::Color(c)) => c,
        other => panic!("{path}: {other:?}"),
    }
}

fn seed(dark: bool) -> strand_theme::Palette {
    from_seed(
        Color::from_hex("#7aa2f7").unwrap(),
        Options {
            dark,
            ..Options::default()
        },
    )
}

/// design.md's theme with `look = auto` and the portal dark: the other
/// mode evaluates the `dark` and `scheme` lets again, so `.light()` is
/// the light palette's; with `look = mocha` the mode is absent.
#[test]
fn the_other_mode_evaluates_the_lets_again() {
    let shell = boot(&[("theme.strand", theme())], Storage::none(), true);
    let current = shell.table();
    assert_eq!(color(&current, "accent"), seed(true).get(Role::Accent));
    let other = shell.inst.other_table().unwrap().expect("a light mode");
    assert_eq!(color(&other, "surface"), seed(false).get(Role::Surface));
    assert_eq!(color(&other, "accent"), seed(false).get(Role::Accent));
    assert_ne!(color(&current, "surface"), color(&other, "surface"));
    // The template reads it.
    let t = Template::compile(FileId::default(), "{{ $surface.light() }}|{{ $surface }}").unwrap();
    let out = String::from_utf8(t.render(&current, Some(&other)).unwrap()).unwrap();
    let hex = strand_theme::export::hex;
    assert_eq!(
        out,
        format!(
            "{}|{}",
            hex(seed(false).get(Role::Surface)),
            hex(seed(true).get(Role::Surface))
        )
    );
    // The hold was released after the pull.
    assert_eq!(shell.inst.other_mode_holders(), 0);
    // `look = mocha` ignores `system.dark`: the mode is absent, and the
    // template's `.light()` is an error unless `??` replaces it.
    shell.look("mocha");
    assert!(shell.inst.other_table().unwrap().is_none());
    let current = shell.table();
    assert!(t.render(&current, None).is_err());
    let t = Template::compile(FileId::default(), "{{ $surface.light() ?? $surface }}").unwrap();
    assert_eq!(
        String::from_utf8(t.render(&current, None).unwrap()).unwrap(),
        hex(color(&current, "surface"))
    );
}

/// `Instance::palette` never falls back: a broken palette is its error
/// while the token table keeps the built-in palette for the screen.
#[test]
fn the_palette_has_no_fallback() {
    let src = "use palette import(\"catppuccin:nope\")\n".to_string();
    let shell = {
        let mut map = SourceMap::new();
        map.add("theme.strand", src);
        let build = Build::compile(None, map).unwrap();
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
        let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
        let _ = inst.flush();
        Shell { rt, host, inst }
    };
    let err = shell.inst.palette().unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
    // The screen still has colours (the built-in palette).
    assert!(shell.table().get("accent").is_some());
    // The other mode fails too, rather than falling back.
    assert!(shell.inst.other_table().is_err());
    // With no `use palette`, the built-in palette is the theme's own.
    let shell = boot(
        &[("a.strand", "let x = 1\n".into())],
        Storage::none(),
        false,
    );
    let p = shell.inst.palette().unwrap();
    assert_eq!(p.source(), Some("built-in"));
    assert!(!p.is_dark());
    let other = shell
        .inst
        .other_table()
        .unwrap()
        .expect("the built-in palette has both");
    assert_eq!(other.palette_value("dark"), Some(&PropValue::Bool(true)));
}

/// What only the other mode reads (a file imported in its branch) is
/// tracked: an edit to it changes the other table, not the current one.
#[test]
fn an_edit_only_the_other_mode_reads_changes_it() {
    let dir = temp_dir("other-reads");
    let scheme = |base00: &str| {
        let mut s =
            String::from("system: \"base16\"\nname: \"Day\"\nvariant: \"light\"\npalette:\n");
        for i in 0..16 {
            let v = if i == 0 {
                base00.to_string()
            } else {
                format!("#{:02x}{:02x}{:02x}", 200 - i * 9, 180, 120 + i * 7)
            };
            s.push_str(&format!("  base{i:02X}: \"{v}\"\n"));
        }
        s
    };
    let file = dir.join("day.yaml");
    std::fs::write(&file, scheme("#fafafa")).unwrap();
    let src = format!(
        "let scheme: Palette = system.dark ? import(\"catppuccin:mocha\") : import(\"base16:{}\")\n\
         use palette scheme\n",
        file.display()
    );
    let shell = boot(
        &[("theme.strand", src)],
        Storage::in_dirs(dir.join("state"), &dir),
        true,
    );
    let other = shell.inst.other_table().unwrap().unwrap();
    assert_eq!(color(&other, "bg"), Color::from_hex("#fafafa").unwrap());
    // Held, as an output that reads `.light()` holds it.
    let hold = shell.inst.hold_other_mode();
    let memo = hold.memo();
    let before = memo.get_untracked(&shell.rt).unwrap();
    let current = shell.table();
    std::fs::write(&file, scheme("#f0e0d0")).unwrap();
    assert!(shell.inst.theme_files_changed(std::slice::from_ref(&file)));
    shell.flush();
    let after = memo.get_untracked(&shell.rt).unwrap();
    assert_ne!(before, after);
    assert_eq!(color(&after, "bg"), Color::from_hex("#f0e0d0").unwrap());
    assert_eq!(
        shell.table(),
        current,
        "the current mode never read the file"
    );
    shell.inst.release_other_mode(hold);
    assert_eq!(shell.inst.other_mode_holders(), 0);
}

/// The other mode writes nothing: after any number of flips with an
/// output holding it, `palettes/palette` holds the current mode, and a
/// failing other-mode palette leaves the current one alone.
#[test]
fn the_other_mode_writes_nothing() {
    let dir = temp_dir("writes-nothing");
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let shell = boot(&[("theme.strand", theme())], storage, true);
    let hold = shell.inst.hold_other_mode();
    let memo = hold.memo();
    for dark in [false, true, false, true, true, false] {
        shell.portal_dark(dark);
        let other = memo.get_untracked(&shell.rt).unwrap();
        assert_eq!(other.palette_value("dark"), Some(&PropValue::Bool(!dark)));
        let theme = shell.inst.theme().unwrap();
        assert!(theme.sync(Duration::from_secs(5)));
        let text = std::fs::read_to_string(dir.join("state/palettes/palette")).unwrap();
        let persisted = strand_theme::Palette::from_text(&text).unwrap();
        assert_eq!(
            persisted.is_dark(),
            dark,
            "the persisted palette is the current mode"
        );
    }
    shell.inst.release_other_mode(hold);
}

/// A failing other-mode palette fails that table only.
#[test]
fn a_failing_other_mode_leaves_the_current_one() {
    let src = "let scheme: Palette = system.dark ? import(\"catppuccin:mocha\") : import(\"catppuccin:nope\")\n\
               use palette scheme\n";
    let shell = boot(&[("theme.strand", src.into())], Storage::none(), true);
    let current = shell.table();
    assert!(shell.inst.other_table().is_err());
    assert_eq!(shell.table(), current);
    assert_eq!(
        shell.inst.palette().unwrap().source(),
        Some("catppuccin:mocha")
    );
}

/// `strand set $accent` reaches both modes, and both still after the
/// portal flips; `strand clear` takes it from both.
#[test]
fn overlays_reach_both_modes() {
    let dir = temp_dir("overlays");
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let shell = boot(&[("theme.strand", theme())], storage, true);
    shell.inst.set_token_overlay("accent", "#ff0000").unwrap();
    shell.flush();
    let red = Color::from_hex("#ff0000").unwrap();
    let hold = shell.inst.hold_other_mode();
    let memo = hold.memo();
    for dark in [true, false, true] {
        shell.portal_dark(dark);
        assert_eq!(color(&shell.table(), "accent"), red);
        let other = memo.get_untracked(&shell.rt).unwrap();
        assert!(other_differs(&shell.table(), &other));
        assert_eq!(color(&other, "accent"), red);
    }
    assert!(shell.inst.clear_token_overlay("accent").unwrap());
    shell.flush();
    let other = memo.get_untracked(&shell.rt).unwrap();
    assert_ne!(color(&other, "accent"), red);
    shell.inst.release_other_mode(hold);
}

/// Every file under `dir`, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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

/// The offline mount reads persisted cells, settings, the overlay file
/// and the persisted palette (pinned), and leaves the state directory
/// byte-identical, with and without a persisted palette.
#[test]
fn the_offline_storage_writes_nothing() {
    let dir = temp_dir("offline");
    std::fs::write(dir.join("prefs.toml"), "accent = \"#ff8800\"\n").unwrap();
    let state = dir.join("state");
    // A real run first: it persists `look`, the palette and an overlay.
    {
        let shell = boot(
            &[("theme.strand", theme())],
            Storage::in_dirs(&state, &dir),
            false,
        );
        shell.look("mocha");
        shell.inst.set_token_overlay("fg", "#ffffff").unwrap();
        shell.flush();
        assert!(shell.inst.theme().unwrap().sync(Duration::from_secs(5)));
        drop(shell);
    }
    let before = snapshot(&state);
    assert!(before.keys().any(|p| p.ends_with("palettes/palette")));
    for with_palette in [true, false] {
        if !with_palette {
            std::fs::remove_file(state.join("palettes/palette")).unwrap();
        }
        let before = snapshot(&state);
        let shell = boot(
            &[("theme.strand", theme())],
            Storage::read_only(&state, &dir),
            false,
        );
        let t = shell.table();
        // The persisted `look` (mocha) and the overlay are read.
        assert_eq!(
            shell.inst.palette().unwrap().source(),
            Some("catppuccin:mocha")
        );
        assert_eq!(color(&t, "fg"), Color::from_hex("#ffffff").unwrap());
        // Writing state in the mount changes nothing on disk.
        shell.look("light");
        shell.inst.set_token_overlay("accent", "#00ff00").ok();
        shell.flush();
        drop(shell);
        assert_eq!(snapshot(&state), before, "with_palette: {with_palette}");
    }
}

/// A hold taken before a reload lets go of nothing the reloaded program
/// holds: an `export` request waiting across a config save, released
/// after it, leaves a mounted output's hold (and its memo) in place.
#[test]
fn a_hold_from_before_a_reload_releases_nothing_after_it() {
    let dir = temp_dir("stale-hold");
    let src = |label: &str| {
        format!(
            "{}\nexport state label = \"{label}\"\n\
             file \"out/both.txt\" {{ text: join(\" \", $surface, $surface.light() ?? $surface) }}\n",
            theme()
        )
    };
    let mut shell = boot(
        &[("theme.strand", src("a"))],
        Storage::in_dirs(dir.join("state"), &dir),
        true,
    );
    let first = shell.inst.take_outputs();
    assert!(first.iter().any(|c| both(c).is_some()), "{first:?}");
    assert_eq!(shell.inst.other_mode_holders(), 1, "the output holds it");
    // The request's hold, then a save.
    let request = shell.inst.hold_other_mode();
    assert_eq!(shell.inst.other_mode_holders(), 2);
    let mut map = SourceMap::new();
    map.add("theme.strand", src("b"));
    let build = Build::compile(None, map).unwrap_or_else(|d| panic!("{d:#?}"));
    shell.inst.reload(&build);
    shell.flush();
    let _ = shell.inst.take_outputs();
    assert_eq!(shell.inst.other_mode_holders(), 1, "the remounted output");
    shell.inst.release_other_mode(request);
    assert_eq!(
        shell.inst.other_mode_holders(),
        1,
        "the stale hold let go of the output's"
    );
    // The output still renders both modes after the next flip.
    shell.portal_dark(false);
    let out = shell.inst.take_outputs();
    let text = out.iter().find_map(both).unwrap();
    let parts: Vec<&str> = text.split(' ').collect();
    assert_eq!(parts.len(), 2, "{text}");
    assert_eq!(parts[0], parts[1], "light is the current mode now: {text}");
    shell.portal_dark(true);
    let out = shell.inst.take_outputs();
    let text = out.iter().find_map(both).unwrap();
    let parts: Vec<&str> = text.split(' ').collect();
    assert_ne!(parts[0], parts[1], "the other mode is still there: {text}");
}

/// `out/both.txt`'s text, if `c` writes it.
fn both(c: &strand_compiler::instantiate::OutputChange) -> Option<String> {
    use strand_compiler::instantiate::OutputContent;
    if !c.path.as_ref()?.ends_with("out/both.txt") {
        return None;
    }
    match &c.content {
        OutputContent::Write { bytes, .. } => Some(String::from_utf8(bytes.clone()).unwrap()),
        OutputContent::Failed { diags, .. } => panic!("{diags:?}"),
    }
}

/// The offline mount reads the running shell's seed index: with no
/// persisted palette, a wallpaper the shell quantised is answered at
/// once, not quantised again, and the state directory is left as it was.
#[test]
fn the_offline_mount_reads_the_seed_index() {
    let dir = temp_dir("seed-index");
    let img = image::RgbImage::from_fn(64, 64, |x, _| {
        image::Rgb(if x < 48 {
            [30, 90, 200]
        } else {
            [240, 200, 40]
        })
    });
    img.save(dir.join("wall.png")).unwrap();
    let src = "use palette material(image: \"wall.png\", dark: true) ?? material(seed: #ff0000)\n"
        .to_string();
    let state = dir.join("state");
    {
        let shell = boot(
            &[("theme.strand", src.clone())],
            Storage::in_dirs(&state, &dir),
            true,
        );
        let theme = shell.inst.theme().unwrap();
        assert!(theme.wait_images(&shell.rt, Duration::from_secs(30)));
        shell.flush();
        assert!(theme.sync(Duration::from_secs(5)));
    }
    std::fs::remove_file(state.join("palettes/palette")).unwrap();
    let before = snapshot(&state);
    let shell = boot(
        &[("theme.strand", src)],
        Storage::read_only(&state, &dir),
        true,
    );
    let p = shell.inst.palette().expect("answered from the index");
    assert_eq!(p.source(), Some("wallpaper"));
    drop(shell);
    assert_eq!(snapshot(&state), before);
}

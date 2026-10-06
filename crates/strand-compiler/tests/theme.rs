//! Theming end to end on the mock host: design.md's `theme.strand` with
//! every `look`, the built-in theme, `material(image:)` holding the old
//! palette, file imports re-read on change and the contrast pairs.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_scene::{Color, MIN_CONTRAST, PropValue, SceneOp, TokenTable, Transition};
use strand_theme::{Options, Role, from_seed};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
}

impl Shell {
    fn flush(&mut self) -> Vec<SceneOp> {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
        u.diff.ops
    }

    fn tokens(&self) -> &TokenTable {
        &self.scene.tokens
    }

    fn color(&self, path: &str) -> Color {
        match self.tokens().lookup(path) {
            Some(PropValue::Color(c)) => c,
            other => panic!("{path}: {other:?}"),
        }
    }

    fn look(&mut self, v: &str) {
        let look = self.host.variant("Look", v);
        self.inst.set("theme.look", look).unwrap();
        self.flush();
    }

    /// Let wallpaper jobs finish and their results reach the scene.
    fn settle(&mut self) {
        let theme = self.inst.theme().unwrap();
        assert!(theme.wait_images(&self.rt, Duration::from_secs(30)));
        self.flush();
    }
}

fn boot(files: &[(&str, String)], storage: Storage) -> Shell {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.clone());
    }
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::new(&rt, program, host.clone(), storage);
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
    };
    shell.flush();
    shell
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("strand-theme-inst-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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

fn hello() -> String {
    fixture("hello_bar.strand")
}

fn png(major: [u8; 3], minor: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_fn(320, 180, |x, _| {
        image::Rgb(if x < 240 { major } else { minor })
    });
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn hex(s: &str) -> Color {
    Color::from_hex(s).unwrap()
}

/// Every declared text/background pair of the table reads at 3:1.
fn readable(t: &TokenTable) {
    assert!(!t.contrast.is_empty());
    for (text, bgs) in &t.contrast {
        let Some(PropValue::Color(fg)) = t.lookup(text) else {
            panic!("{text}")
        };
        for b in bgs {
            let Some(PropValue::Color(bg)) = t.lookup(b) else {
                panic!("{b}")
            };
            assert!(
                fg.contrast(bg) >= MIN_CONTRAST,
                "{text} on {b}: {}",
                fg.contrast(bg)
            );
        }
    }
}

/// A config without a theme file is themed: the built-in base tokens and
/// the built-in palette, following the portal's dark, accent and
/// contrast settings.
#[test]
fn the_hello_bar_alone_is_themed_by_the_built_in_theme() {
    let mut shell = boot(&[("hello_bar.strand", hello())], Storage::none());
    let t = shell.tokens();
    for path in [
        "space.2",
        "radius.lg",
        "font.ui",
        "motion.spatial",
        "fg.muted",
        "border",
        "elevation.md",
    ] {
        assert!(t.lookup(path).is_some(), "{path}");
    }
    let seed = hex(strand_theme::defaults::DEFAULT_SEED);
    assert_eq!(
        shell.color("accent"),
        from_seed(seed, Options::default()).get(Role::Accent)
    );
    readable(shell.tokens());
    // The portal says dark: one new table, sprung.
    shell
        .host
        .set(&shell.rt, "system.dark", Value::Bool(true))
        .unwrap();
    let ops = shell.flush();
    assert!(matches!(
        ops.as_slice(),
        [SceneOp::SetTokens {
            transition: Transition::Default,
            ..
        }]
    ));
    assert!(shell.color("surface").to_oklch().l < 0.3);
    // The desktop's accent becomes the seed; high contrast follows.
    shell
        .host
        .set(&shell.rt, "system.accent", Value::Color(hex("#e01b24")))
        .unwrap();
    shell
        .host
        .set(&shell.rt, "system.contrast", Value::float(1.0))
        .unwrap();
    shell.flush();
    let want = from_seed(
        hex("#e01b24"),
        Options {
            dark: true,
            contrast: 1.0,
            ..Options::default()
        },
    );
    assert_eq!(shell.color("accent"), want.get(Role::Accent));
    readable(shell.tokens());
}

/// design.md's theme.strand: every `look`.
#[test]
fn every_look_of_the_theme_file() {
    let dir = temp_dir("looks");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let wall = config.join("wall.png");
    std::fs::write(&wall, png([30, 90, 200], [240, 200, 40])).unwrap();
    std::fs::write(
        config.join("prefs.toml"),
        format!("wallpaper = \"{}\"\n", wall.display()),
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot(
        &[("theme.strand", theme()), ("hello_bar.strand", hello())],
        storage.clone(),
    );
    let seed = |c: &str, dark| {
        from_seed(
            hex(c),
            Options {
                dark,
                ..Options::default()
            },
        )
    };
    // auto: the portal's preference (light here), the prefs accent.
    assert_eq!(
        shell.color("accent"),
        seed("#7aa2f7", false).get(Role::Accent)
    );
    assert!(shell.color("surface").to_oklch().l > 0.9);
    readable(shell.tokens());
    shell.look("dark");
    assert_eq!(
        shell.color("accent"),
        seed("#7aa2f7", true).get(Role::Accent)
    );
    shell.look("light");
    assert_eq!(
        shell.color("surface"),
        seed("#7aa2f7", false).get(Role::Surface)
    );
    // mocha: Catppuccin, whole.
    shell.look("mocha");
    assert_eq!(shell.color("accent"), hex("#cba6f7"));
    assert_eq!(shell.color("surface"), hex("#1e1e2e"));
    readable(shell.tokens());
    // The derived base tokens follow the palette.
    assert_eq!(shell.color("fg.muted"), hex("#cdd6f4").with_alpha(0.65));
    // wallpaper: quantising, so `?? material(seed: …)` holds first...
    shell.look("wallpaper");
    assert_eq!(
        shell.color("accent"),
        seed("#7aa2f7", true).get(Role::Accent)
    );
    // ...then the image's palette, with no other write.
    shell.settle();
    let from_image = shell.color("accent");
    assert_ne!(from_image, seed("#7aa2f7", true).get(Role::Accent));
    assert!(
        (from_image.to_oklch().h - 260.0).abs() < 30.0,
        "blue wallpaper: {:?}",
        from_image.to_oklch()
    );
    readable(shell.tokens());
    // The wallpaper is a file to watch.
    assert_eq!(shell.inst.theme_files().0, std::slice::from_ref(&wall));
    assert!(shell.inst.take_theme_files_changed());

    // A new wallpaper (swww writing a new file over it): the old palette
    // holds while it is quantised, then the new one springs in.
    std::fs::write(config.join("next.png"), png([200, 40, 40], [20, 20, 20])).unwrap();
    std::fs::rename(config.join("next.png"), &wall).unwrap();
    assert!(shell.inst.theme_files_changed(std::slice::from_ref(&wall)));
    shell.flush();
    assert_eq!(shell.color("accent"), from_image, "the old palette holds");
    shell.settle();
    let red = shell.color("accent");
    assert_ne!(red, from_image);
    let h = red.to_oklch().h;
    assert!(!(200.0..=300.0).contains(&h), "no longer blue: {h}");
    drop(shell);

    // A restart: the persisted look, and the unchanged wallpaper's palette
    // in the boot table itself, without a frame of default colours.
    let shell = boot(
        &[("theme.strand", theme()), ("hello_bar.strand", hello())],
        storage.clone(),
    );
    assert_eq!(
        shell.inst.get("theme.look").unwrap(),
        shell.host.variant("Look", "wallpaper")
    );
    assert_eq!(
        shell.color("accent"),
        red,
        "the boot table is the wallpaper's"
    );
    assert_eq!(shell.inst.theme().unwrap().quantised(), 0, "from the cache");
    drop(shell);
    if let Some(p) = &storage.persist {
        assert!(p.sync(Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A missing wallpaper falls back to the seed palette ("or if missing").
#[test]
fn a_missing_wallpaper_falls_back_to_the_seed() {
    let dir = temp_dir("missing");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("prefs.toml"), "wallpaper = \"nope.png\"\n").unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot(
        &[("theme.strand", theme()), ("hello_bar.strand", hello())],
        storage,
    );
    shell.look("wallpaper");
    shell.settle();
    let seed = from_seed(
        hex("#7aa2f7"),
        Options {
            dark: true,
            ..Options::default()
        },
    );
    assert_eq!(shell.color("accent"), seed.get(Role::Accent));
    let _ = std::fs::remove_dir_all(dir);
}

/// `import("base16:…")` reads a file next to the config, and re-reads it
/// when the watcher says it changed.
#[test]
fn imported_files_are_read_again_when_they_change() {
    let dir = temp_dir("import");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let scheme = |bg: &str, blue: &str| {
        let mut s =
            format!("variant: \"dark\"\npalette:\n  base00: \"{bg}\"\n  base05: \"#c0caf5\"\n");
        s.push_str(&format!("  base0D: \"{blue}\"\n"));
        s
    };
    std::fs::write(config.join("scheme.yaml"), scheme("#1a1b26", "#7aa2f7")).unwrap();
    let src =
        "let p = import(\"base16:scheme.yaml\")\nuse palette p\nbar B { text \"x\" }\n".to_string();
    let mut shell = boot(
        &[("t.strand", src)],
        Storage::in_dirs(dir.join("state"), &config),
    );
    assert_eq!(shell.color("accent"), hex("#7aa2f7"));
    assert_eq!(shell.color("surface"), hex("#1a1b26"));
    assert_eq!(shell.inst.theme_files().1, [config.join("scheme.yaml")]);
    std::fs::write(config.join("scheme.yaml"), scheme("#24283b", "#2ac3de")).unwrap();
    assert!(
        shell
            .inst
            .theme_files_changed(&[config.join("scheme.yaml")])
    );
    shell.flush();
    assert_eq!(shell.color("accent"), hex("#2ac3de"));
    assert_eq!(shell.color("surface"), hex("#24283b"));
    let _ = std::fs::remove_dir_all(dir);
}

/// `set { $x: … }` sees the inherited `$x` on its right, and the contrast
/// guard holds inside the subtree too.
#[test]
fn subtree_overrides_inherit_and_stay_readable() {
    let src = "bar B {\n  box {\n    set { $surface: $surface.mix(#000000, 90%) }\n    text \"x\" { color: $fg }\n  }\n}\n".to_string();
    let shell = boot(&[("t.strand", src)], Storage::none());
    let boxes = shell.scene.of_kind(strand_scene::NodeKind::Box);
    let Some(PropValue::Tokens(set)) = shell.scene.prop(boxes[0], strand_scene::Prop::Tokens)
    else {
        panic!("no set")
    };
    let levels = [shell.tokens(), set.as_ref()];
    let scope = strand_scene::TokenScope::new(&levels);
    let Some(PropValue::Color(surface)) = scope.lookup("surface") else {
        panic!()
    };
    let outer = shell.color("surface");
    assert!(
        surface.to_oklch().l < outer.to_oklch().l * 0.5,
        "darker than the inherited surface"
    );
    let Some(PropValue::Color(fg)) = scope.lookup("fg") else {
        panic!()
    };
    assert!(
        fg.contrast(surface) >= MIN_CONTRAST,
        "{}",
        fg.contrast(surface)
    );
    // The built-in light theme's fg is dark; over the darkened surface it
    // was solved lighter.
    assert!(fg.to_oklch().l > shell.color("fg").to_oklch().l);
}

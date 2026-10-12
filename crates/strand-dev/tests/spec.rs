//! The freeze review of `docs/spec.md` (m5-plan, "Language spec frozen";
//! decisions.md m5-docs, "spec.md marks what it cannot pin down"): each
//! point the draft marked **(unverified)** is settled here by what the
//! checker and the runtime do, and spec.md states it.
//!
//! - 7.1: a bare number where a length is expected is logical pixels;
//! - 7.2: which comparisons of numbers with units are accepted, for
//!   order and for equality alike;
//! - 6.5: a number key keeps its unit;
//! - 10.4: `@reset` resets its cell on every reload that commits the
//!   declaration with the attribute, not only the first;
//! - 11: a settings file that does not exist gives every field its
//!   default, and reading it creates nothing.

use std::rc::Rc;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_dev::workspace::{Analysis, ConfigKey, Input};

fn build(prev: Option<&Build>, files: &[(&str, &str)]) -> Build {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, (*text).to_string());
    }
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

/// A running instance on the schema host's mock services, one monitor.
struct Shell {
    rt: Runtime,
    inst: Instance,
    scene: SceneMirror,
    last: Build,
}

impl Shell {
    fn boot(files: &[(&str, &str)], storage: Storage) -> Shell {
        let rt = Runtime::new();
        let first = build(None, files);
        let host = Rc::new(SchemaHost::mock(&rt, &first.program.types));
        let screen = host.record(
            "Screen",
            &[
                ("id", Value::text("Mock | DP-1 | Display")),
                ("name", Value::text("DP-1")),
                ("make", Value::text("Mock")),
                ("model", Value::text("DP-1")),
                ("description", Value::text("Display")),
            ],
        );
        host.set(&rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
        let inst = Instance::from_build(&rt, &first, host, storage);
        let mut shell = Shell {
            rt,
            inst,
            scene: SceneMirror::new(),
            last: first,
        };
        shell.flush();
        shell
    }

    fn flush(&mut self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
    }

    fn reload(&mut self, files: &[(&str, &str)]) -> strand_compiler::reconcile::Report {
        let next = build(Some(&self.last), files);
        let report = self.inst.reload(&next);
        self.last = next;
        self.flush();
        report
    }
}

/// The diagnostics' codes for one file, checked as `strand check` does.
fn codes(text: &str) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("strand-spec-{}", std::process::id()));
    let uri = format!("file://{}/bar.strand", dir.display());
    let an = Analysis::offline(
        ConfigKey::Single(uri.clone()),
        vec![Input::new(uri, "bar.strand".into(), text.into())],
        strand_dev::server::schema(),
    );
    an.compiled
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

/// spec.md 7.1: `width: 90` is 90 logical pixels, the same value as
/// `width: 90px`; a percentage is not.
#[test]
fn a_bare_number_for_a_length_is_logical_pixels() {
    let shell = Shell::boot(
        &[(
            "bar.strand",
            "bar Top {\n  box { width: 90 }\n  box { width: 90px }\n  box { width: 90% }\n}\n",
        )],
        Storage::none(),
    );
    let scene = shell.scene.render();
    let widths: Vec<&str> = scene
        .lines()
        .filter_map(|l| l.split_whitespace().find(|w| w.starts_with("width=")))
        .collect();
    assert_eq!(widths.len(), 3, "{scene}");
    assert_eq!(widths[0], widths[1], "{scene}");
    assert_ne!(widths[0], widths[2], "{scene}");
    drop(shell.rt);
}

/// spec.md 7.2: a comparison takes two plain numbers, two values of the
/// same kind, or a length, angle or percentage beside a plain number
/// (which takes its unit), and a length beside a percentage; a duration
/// compares only with a duration, and different units never mix.
#[test]
fn mixed_unit_comparisons() {
    let accepted = [
        "1 < 2.5",
        "10px < 20px",
        "10px < 20",
        "90deg > 45",
        "40% < 50",
        "40% < 50%",
        "10px < 50%",
        "1s < 500ms",
    ];
    let rejected = [
        "1s < 2",
        "2 < 1s",
        "10px < 1s",
        "90deg < 10px",
        "40% < 90deg",
    ];
    let file = |cmp: &str| format!("bar Top {{\n  text {cmp} ? \"a\" : \"b\"\n}}\n");
    for cmp in accepted {
        assert_eq!(codes(&file(cmp)), Vec::<String>::new(), "{cmp}");
    }
    for cmp in rejected {
        assert_eq!(codes(&file(cmp)), ["check::type_mismatch"], "{cmp}");
    }
    // A bare number is never a duration: the one diagnostic is the type
    // mismatch, where `2ms` checks clean.
    let prop = |p: &str| format!("bar Top {{\n  box {{ {p} }}\n}}\n");
    assert_eq!(codes(&prop("stagger: 2")), [STAGGER_BARE]);
    assert_eq!(codes(&prop("stagger: 2ms")), Vec::<String>::new());
}

/// The code `stagger: 2` is refused with.
const STAGGER_BARE: &str = "check::type_mismatch";

/// One bar of `src` on the mock services, laid out 120x64 and painted
/// with the renderer `strand run` uses: its pixels.
fn paint(src: &str) -> Vec<u8> {
    use strand_scene::{PaintTarget, Painter as _, Scale, Size, SurfaceId};
    let rt = Runtime::new();
    let first = build(None, &[("bar.strand", src)]);
    let host = Rc::new(SchemaHost::mock(&rt, &first.program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
            ("make", Value::text("Mock")),
            ("model", Value::text("DP-1")),
            ("description", Value::text("Display")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::from_build(&rt, &first, host, Storage::none());
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    let mut scene = SceneMirror::new();
    scene.apply(&u.diff).unwrap();
    let font = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
        std::sync::Arc::new(font),
    ]));
    let mut render =
        strand_render::Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
    assert!(render.apply(u.diff).is_empty());
    let bar = scene.roots()[0];
    let (w, h) = (120, 64);
    render.attach_surface(SurfaceId(1), bar);
    render.configure_surface(SurfaceId(1), Size::new(w, h), Scale::ONE);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let target = PaintTarget::new(&mut px, Size::new(w, h), w * 4, Scale::ONE, 0).unwrap();
    render.paint(
        SurfaceId(1),
        &mut target.at(std::time::Duration::from_secs(1)),
    );
    drop(inst);
    px
}

/// spec.md 7.1: where an angle is expected a bare number is degrees:
/// `rotate: 45` paints what `rotate: 45deg` paints (radians or turns
/// would not), and not what `rotate: 0` or `rotate: 90deg` paints.
#[test]
fn a_bare_number_for_an_angle_is_degrees() {
    let bar = |r: &str| {
        paint(&format!(
            "bar Top {{\n  bg: #ffffff\n  box {{ x: 30; y: 12; width: 60; height: 12; \
             bg: #ff0000; rotate: {r} }}\n}}\n"
        ))
    };
    let bare = bar("45");
    assert!(bare == bar("45deg"), "rotate: 45 is not rotate: 45deg");
    assert!(bare != bar("0"), "rotate: 45 did not turn the box");
    assert!(bare != bar("90deg"));
}

/// spec.md 7.2: lengths in different units (`px`, `ch`, `%`) compare by
/// their numbers as written; a comparison resolves neither a `ch` against
/// a font nor a percentage against a size, so `2ch < 10px` is true
/// although 2ch is wider than 10px in any text font.
#[test]
fn lengths_in_different_units_compare_as_written() {
    for cmp in ["2ch < 10px", "10px > 2ch", "2ch < 10%", "4ch < 5ch"] {
        assert_eq!(
            codes(&format!("bar Top {{\n  text {cmp} ? \"a\" : \"b\"\n}}\n")),
            Vec::<String>::new(),
            "{cmp}"
        );
    }
    let shell = Shell::boot(
        &[(
            "bar.strand",
            "bar Top {\n  text 2ch < 10px ? \"ch-as-written\" : \"ch-resolved\"\n  \
             text 30px < 2ch ? \"px-resolved\" : \"px-as-written\"\n}\n",
        )],
        Storage::none(),
    );
    assert!(
        shell.scene.find_text("ch-as-written").is_some()
            && shell.scene.find_text("px-as-written").is_some(),
        "{}",
        shell.scene.render()
    );
    drop(shell.rt);
}

/// spec.md 7.2: `==` and `!=` take numbers by the ordering rule, so units
/// that never mix are a load error for equality too (the checker once
/// accepted any two numbers and the VM compared them unit-blind, so
/// `90deg == 90px` and `1s == 1000px` showed true), and lengths in
/// different units are equal by their numbers as written.
#[test]
fn equality_takes_numbers_by_the_ordering_rule() {
    let file = |cmp: &str| format!("bar Top {{\n  text {cmp} ? \"a\" : \"b\"\n}}\n");
    for cmp in [
        "1 == 1.0",
        "10px == 10",
        "10px != 20px",
        "2ch == 2px",
        "50% == 50px",
        "50% == 50",
        "90deg == 90",
        "1s == 1000ms",
        "1s != 2s",
    ] {
        assert_eq!(codes(&file(cmp)), Vec::<String>::new(), "{cmp}");
    }
    for cmp in [
        "90deg == 90px",
        "90px != 90deg",
        "1s == 1000",
        "1000 == 1s",
        "1s == 1000px",
        "1s != 1000%",
        "40% == 90deg",
    ] {
        assert_eq!(codes(&file(cmp)), ["check::type_mismatch"], "{cmp}");
    }
    let shell = Shell::boot(
        &[(
            "bar.strand",
            "bar Top {\n  text 2ch == 2px ? \"ch-eq\" : \"ch-ne\"\n  \
             text 50% != 50px ? \"pct-ne\" : \"pct-eq\"\n  \
             text 1s == 1000ms ? \"s-eq\" : \"s-ne\"\n  \
             text 10px == 10 ? \"bare-eq\" : \"bare-ne\"\n}\n",
        )],
        Storage::none(),
    );
    for want in ["ch-eq", "pct-eq", "s-eq", "bare-eq"] {
        assert!(
            shell.scene.find_text(want).is_some(),
            "{want}: {}",
            shell.scene.render()
        );
    }
    drop(shell.rt);
}

/// spec.md 6.5: a key is an identity, not an equality: a number key keeps
/// its unit, so `2px` and `2ch` mount two copies although `2ch == 2px`.
#[test]
fn a_number_key_keeps_its_unit() {
    let shell = Shell::boot(
        &[(
            "bar.strand",
            "bar Top {\n  for w in [2px, 2ch] key w {\n    text \"copy\"\n  }\n}\n",
        )],
        Storage::none(),
    );
    let copies = shell.scene.texts().iter().filter(|t| *t == "copy").count();
    assert_eq!(copies, 2, "{}", shell.scene.render());
    drop(shell.rt);
}

/// spec.md 10.4: `@reset` resets its cell on the reload that adds it, and
/// on every later reload that commits the declaration with it still
/// there, so it is meant to be removed after one save.
#[test]
fn at_reset_resets_on_every_reload_that_keeps_it() {
    let src = |reset: bool, other: &str| {
        format!(
            "{}state a = 1\nstate b = 1\nbar Top {{ text join(\" \", a, b, \"{other}\") }}\n",
            if reset { "@reset " } else { "" }
        )
    };
    let mut shell = Shell::boot(&[("t.strand", &src(false, "x"))], Storage::none());
    let set = |shell: &mut Shell| {
        shell.inst.set_value("t", "a", Value::int(7)).unwrap();
        shell.inst.set_value("t", "b", Value::int(7)).unwrap();
        shell.flush();
    };
    set(&mut shell);
    let report = shell.reload(&[("t.strand", &src(true, "x"))]);
    assert_eq!(report.reset, [("t.a".to_string(), "@reset".to_string())]);
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(1));
    assert_eq!(shell.inst.value_of("t", "b").unwrap(), Value::int(7));
    // A later edit of the same file that keeps the attribute resets it
    // again.
    set(&mut shell);
    let report = shell.reload(&[("t.strand", &src(true, "y"))]);
    assert_eq!(report.reset, [("t.a".to_string(), "@reset".to_string())]);
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(1));
    // So does a reload for another file's edit.
    set(&mut shell);
    let report = shell.reload(&[("t.strand", &src(true, "y")), ("u.strand", "state c = 1\n")]);
    assert_eq!(report.reset, [("t.a".to_string(), "@reset".to_string())]);
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(1));
    // Removed, the cell is kept again.
    set(&mut shell);
    let report = shell.reload(&[
        ("t.strand", &src(false, "y")),
        ("u.strand", "state c = 1\n"),
    ]);
    assert!(report.reset.is_empty(), "{report:?}");
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(7));
}

/// spec.md 11: a settings file that does not exist yet gives every field
/// its default, and reading it writes nothing.
#[test]
fn a_missing_settings_file_gives_every_default() {
    let dir = std::env::temp_dir().join(format!("strand-spec-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let shell = Shell::boot(
        &[(
            "bar.strand",
            "state prefs from \"prefs.toml\" { compact: bool = true; size: int = 3; name: text = \"n\" }\n\
             bar Top { text join(\" \", prefs.compact, prefs.size, prefs.name) }\n",
        )],
        Storage::in_dirs(dir.join("state"), dir.join("config")),
    );
    assert!(
        shell.scene.find_text("true 3 n").is_some(),
        "{}",
        shell.scene.render()
    );
    assert!(!dir.join("config/prefs.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

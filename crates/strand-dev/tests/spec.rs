//! The freeze review of `docs/spec.md` (m5-plan, "Language spec frozen";
//! decisions.md m5-docs, "spec.md marks what it cannot pin down"): each
//! point the draft marked **(unverified)** is settled here by what the
//! checker and the runtime do, and spec.md states it.
//!
//! - 7.1: a bare number where a length is expected is logical pixels;
//! - 7.2: which comparisons of numbers with units are accepted;
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
    // A bare number is degrees where an angle is expected, and never a
    // duration.
    let prop = |p: &str| format!("bar Top {{\n  box {{ {p} }}\n}}\n");
    assert_eq!(codes(&prop("rotate: 45")), Vec::<String>::new());
    assert_ne!(codes(&prop("stagger: 2")), Vec::<String>::new());
    assert_eq!(codes(&prop("stagger: 2ms")), Vec::<String>::new());
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

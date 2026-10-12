//! The guide's code blocks check (m5-plan, S-docs' tests; written by S-lsp,
//! decisions.md m5-lsp): applied in page order, each block replacing the
//! file it names, the config after every step has no error and no
//! warning, and every file in it is already formatted. The `strand new`
//! template (`docs/guide/template/bar.strand`) passes the same.
//!
//! Beside it (S-docs, decisions.md m5-docs review round 1): the guide's
//! copies of design.md's blocks are the accepted fixtures, and level 5's
//! dock and control centre run on the schema host's mock services.
//!
//! The checks are the language server's, offline (`Analysis::offline`: the
//! compiler with the builtin and service schemas, the `from file`/`poll`
//! path and shader checks), which are `strand check`'s less the two that
//! look outside the config: `from dbus` introspection (the language server
//! asks the bus in the background and never waits, so its result depends
//! on when the answer came in; a check here must not depend on a bus) and
//! the installed lock helper.

mod support;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::SourceMap;
use strand_compiler::diagnostic::{Style, render};
use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::lower;
use strand_compiler::schema::Schema;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_dev::workspace::{Analysis, ConfigKey, Input};
use support::guide;

/// A fresh directory for one config.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-guide-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// The config in `dir`, compiled by `analyse` (`Analysis::offline` or
/// `Analysis::new`).
fn analysis(
    dir: &Path,
    analyse: fn(ConfigKey, Vec<Input>, std::sync::Arc<strand_compiler::schema::Schema>) -> Analysis,
) -> (Vec<PathBuf>, Analysis) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    files.sort();
    let inputs = files
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p).unwrap();
            Input::new(
                format!("file://{}", p.display()),
                p.display().to_string(),
                text.into(),
            )
        })
        .collect();
    let an = analyse(
        ConfigKey::Dir(dir.to_path_buf()),
        inputs,
        strand_dev::server::schema(),
    );
    (files, an)
}

/// Checks the config in `dir` as `strand check` would (see the header);
/// on any error or warning, panics naming the step and showing them.
fn check(dir: &Path, step: &str) {
    let (files, an) = analysis(dir, Analysis::offline);
    let diags = &an.compiled.diagnostics;
    assert!(
        diags.is_empty(),
        "{step}: {} error(s), {} warning(s) in the config after this step:\n{}",
        diags.iter().filter(|d| d.is_error()).count(),
        diags.iter().filter(|d| !d.is_error()).count(),
        render(diags, &an.map, Style::Plain),
    );
    for p in &files {
        let text = std::fs::read_to_string(p).unwrap();
        let formatted = strand_compiler::fmt::format(&text)
            .unwrap_or_else(|e| panic!("{step}: {} does not format: {e:?}", p.display()));
        assert!(
            formatted == text,
            "{step}: `strand fmt` would change {}:\n--- as written\n{text}--- formatted\n{formatted}",
            p.display(),
        );
    }
}

#[test]
fn every_guide_block_checks() {
    let blocks = guide::blocks();
    assert!(
        !blocks.is_empty(),
        "no ```strand level=N file=NAME blocks found"
    );
    let dir = scratch("steps");
    for b in &blocks {
        std::fs::write(dir.join(&b.file), &b.text).unwrap();
        check(&dir, &format!("{} (level {}, {})", b.at, b.level, b.file));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_new_template_checks() {
    let template = guide::root().join("docs/guide/template/bar.strand");
    let dir = scratch("template");
    std::fs::copy(&template, dir.join("bar.strand")).unwrap();
    check(&dir, "docs/guide/template/bar.strand");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_block_nested_in_a_longer_fence_is_not_a_step() {
    let page = "\
````
```strand level=2 file=dot.strand
component Dot(ws: Workspace) { … }
```
````

```strand level=1 file=bar.strand
bar Top { edge: top }
```
";
    let fences = guide::fences(page);
    assert_eq!(fences.len(), 2);
    assert_eq!(fences[0].1, "");
    assert!(fences[0].2.contains("```strand level=2"));
    assert_eq!(fences[1].1, "strand level=1 file=bar.strand");
    assert_eq!(fences[1].2, "bar Top { edge: top }\n");
}

/// Level 5 declares a `from dbus` service (level-5.md item 6); a step
/// after it must check the same whether or not the background
/// introspection has answered by then. The bus here has no
/// `org.strand.Absent` (or there is no system bus), so once the answer is
/// in, the language server's own analysis warns `check::dbus_unchecked`;
/// the guide's check does not.
#[test]
fn a_from_dbus_service_checks_the_same_once_the_bus_has_answered() {
    let dir = scratch("dbus");
    std::fs::write(
        dir.join("bar.strand"),
        "service absent from dbus system \"org.strand.Absent\" { profile: text = ActiveProfile }\n\n\
         bar Top {\n  edge: top\n  text absent.profile ?? \"none\"\n}\n",
    )
    .unwrap();
    // The first analysis asks the bus in the background; wait for its
    // answer, then make sure the server's analysis now sees it.
    let (_, first) = analysis(&dir, Analysis::new);
    assert!(
        !first
            .compiled
            .diagnostics
            .iter()
            .any(|d| d.code == "check::dbus_unchecked"),
        "the first analysis has no answer yet",
    );
    strand_dev::workspace::introspected()
        .recv_timeout(Duration::from_secs(30))
        .expect("the introspection answer comes in");
    let (_, answered) = analysis(&dir, Analysis::new);
    assert!(
        answered
            .compiled
            .diagnostics
            .iter()
            .any(|d| d.code == "check::dbus_unchecked"),
        "the answered analysis warns: {:?}",
        answered.compiled.diagnostics,
    );
    check(&dir, "a `from dbus` service after the bus answered");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The schema `strand check` uses, without the host's plugin files.
fn builtin_schema() -> Schema {
    Schema::builtin_with(&strand_services_schema::schemas()).unwrap()
}

/// The guide's copies of design.md's blocks are the accepted fixtures,
/// byte for byte, as the pages say: the OSD, toasts and launcher at their
/// first step, and the theme file.
#[test]
fn guide_copies_of_design_blocks_are_verbatim() {
    let steps = guide::blocks();
    let fixtures = guide::root().join("crates/strand-compiler/tests/fixtures");
    for (file, fixture) in [
        ("osd.strand", "osd.strand"),
        ("toasts.strand", "toasts.strand"),
        ("launcher.strand", "launcher.strand"),
        ("theme.strand", "theme.strand"),
    ] {
        let first = steps
            .iter()
            .find(|s| s.file == file)
            .unwrap_or_else(|| panic!("no step writes {file}"));
        let want = std::fs::read_to_string(fixtures.join(fixture)).unwrap();
        assert_eq!(
            first.text, want,
            "{}: {file} is not design.md's block",
            first.at
        );
    }
}

/// The last block the guide shows for `file`.
fn last_block(file: &str) -> String {
    guide::blocks()
        .into_iter()
        .rev()
        .find(|s| s.file == file)
        .unwrap_or_else(|| panic!("no step writes {file}"))
        .text
}

/// The guide's dock on the mock services, with `storage`.
struct Dock {
    // Dropped before `rt`.
    inst: Instance,
    scene: SceneMirror,
    _rt: Runtime,
}

impl Dock {
    /// `installed`: the app ids the mock `apps.all` holds.
    fn boot(storage: Storage, installed: &[&str]) -> Dock {
        let schema = builtin_schema();
        let mut map = SourceMap::new();
        map.add("dock.strand", last_block("dock.strand"));
        let compiled = strand_compiler::compile_with(&map, &schema);
        assert_eq!(
            compiled.errors(),
            0,
            "{}",
            render(&compiled.diagnostics, &map, Style::Plain)
        );
        let program = Arc::new(lower::lower(&compiled.program, &schema));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
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
        let apps = installed
            .iter()
            .map(|id| {
                host.record(
                    "App",
                    &[
                        ("id", Value::text(*id)),
                        ("name", Value::text(*id)),
                        ("icon", Value::text(format!("{id}-icon"))),
                    ],
                )
            })
            .collect();
        host.set(&rt, "apps.all", Value::list(apps)).unwrap();
        let inst = Instance::new(&rt, program, host, storage);
        let mut dock = Dock {
            inst,
            scene: SceneMirror::new(),
            _rt: rt,
        };
        dock.flush();
        dock
    }

    fn flush(&mut self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
    }

    /// How many icons the dock draws.
    fn icons(&self) -> usize {
        self.scene
            .walk()
            .into_iter()
            .filter(|n| self.scene.kind(*n).is_some_and(|k| k.name() == "image"))
            .count()
    }

    fn pin_list(&self) -> Vec<Value> {
        let v = self.inst.value_of("dock", "pins").unwrap();
        v.as_list().unwrap().to_vec()
    }

    /// The pins' app ids, in order.
    fn pins(&self) -> Vec<String> {
        self.pin_list()
            .iter()
            .map(|p| {
                p.as_record().unwrap().fields[0]
                    .as_text()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// Pins `app` at the end, as a handler's `pins.push` would.
    fn push(&mut self, app: &str, label: &str) {
        let mut list = self.pin_list();
        let ty = list[0].as_record().unwrap().ty;
        list.push(Value::record(
            ty,
            vec![Value::text(app), Value::text(label)],
        ));
        self.inst
            .set_value("dock", "pins", Value::list(list))
            .unwrap();
        self.flush();
    }

    /// Drags the pin `app` and drops it where the renderer would report
    /// `at`: its index among the row's other drawn icons.
    fn drop_at(&mut self, app: &str, at: i64) {
        let icon = self
            .scene
            .walk()
            .into_iter()
            .find(|n| self.scene.kind(*n).is_some_and(|k| k.name() == "image"))
            .unwrap();
        let row = self.scene.parent(icon).unwrap();
        let pin = self
            .pin_list()
            .into_iter()
            .find(|p| p.as_record().unwrap().fields[0].as_text() == Some(app))
            .unwrap();
        assert!(
            self.inst.event(row, "drop", vec![pin, Value::int(at)]),
            "the row takes the drop:\n{}",
            self.scene.render()
        );
        self.flush();
    }
}

/// The dock draws one icon per pin, an uninstalled app's included, so the
/// index a drop reports among the drawn icons is the index in `pins`
/// (strand-render's `drop_index` counts the row's drawn children, the
/// dragged one left out); and its keyed, persisted list of records keeps
/// its order across a restart (decisions.md m5-docs, unverified item 3).
#[test]
fn the_guide_dock_reorders_every_pin_and_keeps_the_order() {
    let dir = scratch("dock");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    // Nautilus is not installed: its pin still draws an icon.
    let mut dock = Dock::boot(storage.clone(), &["firefox", "foot"]);
    assert_eq!(dock.pins(), ["firefox", "org.gnome.Nautilus"]);
    assert_eq!(dock.icons(), 2, "{}", dock.scene.render());
    dock.push("foot", "Foot");
    assert_eq!(dock.pins(), ["firefox", "org.gnome.Nautilus", "foot"]);
    assert_eq!(dock.icons(), 3, "{}", dock.scene.render());
    // Firefox dragged past foot: the other drawn icons are
    // [nautilus, foot], and past foot is index 2.
    dock.drop_at("firefox", 2);
    assert_eq!(dock.pins(), ["org.gnome.Nautilus", "foot", "firefox"]);
    drop(dock);
    // After a restart: the same order.
    let dock = Dock::boot(storage.clone(), &["firefox", "foot"]);
    assert_eq!(dock.pins(), ["org.gnome.Nautilus", "foot", "firefox"]);
    drop(dock);
    if let Some(p) = &storage.persist {
        assert!(p.sync(std::time::Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The control centre's `set { $Tile.pad: $space.2 }` reaches the tiles
/// in its row and no other (decisions.md m5-docs, unverified item 4):
/// the row carries the override for its subtree, and the tiles read
/// `$Tile.pad` under it.
#[test]
fn the_guide_control_centre_overrides_a_component_token() {
    let schema = builtin_schema();
    let mut map = SourceMap::new();
    for file in ["theme.strand", "services.strand", "control.strand"] {
        map.add(file, last_block(file));
    }
    let compiled = strand_compiler::compile_with(&map, &schema);
    assert_eq!(
        compiled.errors(),
        0,
        "{}",
        render(&compiled.diagnostics, &map, Style::Plain)
    );
    let program = Arc::new(lower::lower(&compiled.program, &schema));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    let inst = Instance::new(&rt, program, host, Storage::none());
    let mut scene = SceneMirror::new();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    scene.apply(&u.diff).unwrap();
    inst.set("control.open", Value::Bool(true)).unwrap();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    scene.apply(&u.diff).unwrap();
    let shown = scene.render();
    // The row of sliders carries the override, and its tile reads the
    // token under it.
    let row = "row gap=$space.2 tokens={$Tile.pad: $space.2}\n";
    let at = shown.find(row).unwrap_or_else(|| panic!("{shown}"));
    let inside = &shown[at + row.len()..];
    assert!(
        inside.trim_start().starts_with("col pad=$Tile.pad "),
        "{shown}"
    );
    assert_eq!(shown.matches("tokens={$Tile.pad").count(), 1, "{shown}");
    // Elsewhere `$Tile.pad` is the component's default, `$space.3`.
    let tokens = scene.render_tokens();
    assert!(
        tokens.lines().any(|l| l == "Tile.pad := $space.3"),
        "{tokens}"
    );
}

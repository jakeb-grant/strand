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
use strand_compiler::instantiate::{Instance, SceneMirror, Storage, show_expr};
use strand_compiler::lower;
use strand_compiler::schema::Schema;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_dev::workspace::{Analysis, ConfigKey, Input};
use strand_scene::{
    ButtonState, DropPayload, InputEvent, LogicalPoint, LogicalRect, NodeId, PaintTarget, Painter,
    Prop, PropValue, Scale, Size, SurfaceId,
};
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
    /// The scene as `strand run`'s main thread holds it: laid out and
    /// hit-tested, so drops are placed by `strand_render::Router`.
    render: strand_render::Renderer,
    /// The bars attached to the renderer, and the paint clock.
    surfaces: Vec<(SurfaceId, NodeId)>,
    clock: u64,
    _rt: Runtime,
}

impl Dock {
    /// `installed`: the app ids the mock `apps.all` holds; `screens`: the
    /// monitors' names (a dock on each).
    fn boot(storage: Storage, installed: &[&str], screens: &[&str]) -> Dock {
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
        let screens = screens
            .iter()
            .map(|name| {
                host.record(
                    "Screen",
                    &[
                        ("id", Value::text(format!("Mock | {name} | Display"))),
                        ("name", Value::text(*name)),
                        ("make", Value::text("Mock")),
                        ("model", Value::text(*name)),
                        ("description", Value::text("Display")),
                    ],
                )
            })
            .collect();
        host.set(&rt, "screens.all", Value::list(screens)).unwrap();
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
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine =
            strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![Arc::new(font)]));
        let mut dock = Dock {
            inst,
            scene: SceneMirror::new(),
            render: strand_render::Renderer::new(strand_render::TextBackend::Inline(Box::new(
                engine,
            ))),
            surfaces: Vec::new(),
            clock: 0,
            _rt: rt,
        };
        dock.flush();
        dock
    }

    fn flush(&mut self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
        assert!(self.render.apply(u.diff).is_empty());
    }

    /// The docks' bars, one surface each (`SurfaceId(1)`, `SurfaceId(2)`,
    /// …), laid out 800 px wide and painted once the icons have sprung in
    /// (or glided to their new places).
    fn paint(&mut self) -> Vec<(SurfaceId, NodeId)> {
        if self.surfaces.is_empty() {
            let bars: Vec<NodeId> = self
                .scene
                .walk()
                .into_iter()
                .filter(|n| {
                    self.scene.parent(*n).is_none()
                        && self.scene.kind(*n).is_some_and(|k| k.name() == "bar")
                })
                .collect();
            for (i, bar) in bars.into_iter().enumerate() {
                let s = SurfaceId(i as u32 + 1);
                self.render.attach_surface(s, bar);
                self.render
                    .configure_surface(s, Size::new(800, 64), Scale::ONE);
                self.surfaces.push((s, bar));
            }
        }
        let mut px = vec![0u8; 800 * 64 * 4];
        for step in [1, 5] {
            let at = Duration::from_secs(self.clock + step);
            for (s, _) in &self.surfaces {
                let target =
                    PaintTarget::new(&mut px, Size::new(800, 64), 3200, Scale::ONE, 0).unwrap();
                self.render.paint(*s, &mut target.at(at));
            }
        }
        self.clock += 5;
        self.surfaces.clone()
    }

    /// The icon of pin `label` on the dock under `bar`, and its box.
    fn icon(&self, s: SurfaceId, bar: NodeId, label: &str) -> (NodeId, LogicalRect) {
        let under = |mut n: NodeId| {
            while let Some(p) = self.scene.parent(n) {
                n = p;
            }
            n == bar
        };
        let icon = self
            .scene
            .walk()
            .into_iter()
            .find(|n| {
                under(*n)
                    && self.scene.kind(*n).is_some_and(|k| k.name() == "image")
                    && matches!(self.scene.prop(*n, Prop::Tooltip),
                        Some(PropValue::Text(t)) if t.as_str() == label)
            })
            .unwrap_or_else(|| panic!("no {label} icon:\n{}", self.scene.render()));
        let rect = self
            .render
            .node_rect(s, icon)
            .unwrap_or_else(|| panic!("{label}'s icon is not laid out"));
        (icon, rect)
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

    /// Drops the pin `app` on the first dock's row at `at`, as
    /// [`Dock::drop_on`] would.
    fn drop_at(&mut self, app: &str, at: i64) {
        let icon = self
            .scene
            .walk()
            .into_iter()
            .find(|n| self.scene.kind(*n).is_some_and(|k| k.name() == "image"))
            .unwrap();
        let row = self.scene.parent(icon).unwrap();
        self.drop_on(row, app, at);
    }

    /// Runs `row`'s `on drop(p, at)` with the pin `app`: what `strand
    /// run` does with the router's `Drop { payload, at }` on `row`.
    fn drop_on(&mut self, row: NodeId, app: &str, at: i64) {
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
    let mut dock = Dock::boot(storage.clone(), &["firefox", "foot"], &["DP-1"]);
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
    let dock = Dock::boot(storage.clone(), &["firefox", "foot"], &["DP-1"]);
    assert_eq!(dock.pins(), ["org.gnome.Nautilus", "foot", "firefox"]);
    drop(dock);
    if let Some(p) = &storage.persist {
        assert!(p.sync(std::time::Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Where the router places a drop on the guide's dock, two monitors, a
/// dock on each (decisions.md m5-docs, review round 3 item 1). Dragged
/// along its own dock, an icon lands where it is let go: the `at`
/// strand-render's `drop_index` reports leaves the dragged icon out, as
/// `pins.move` takes it out before it inserts. Dropped on the other
/// monitor's dock, the `at` leaves out that dock's own icon for the same
/// pin too (its `drag_identity` is the dragged one's; decisions.md
/// m5-fixups), so the drop is exact before and past that icon, as level
/// 5 says.
#[test]
fn the_guide_dock_drops_where_the_pointer_is() {
    let dir = scratch("dock-drops");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let mut dock = Dock::boot(storage.clone(), &["firefox", "foot"], &["DP-1", "DP-2"]);
    dock.push("foot", "Foot");
    let bars = dock.paint();
    assert_eq!(bars.len(), 2, "{}", dock.scene.render());
    let ((a, bar_a), (b, bar_b)) = (bars[0], bars[1]);
    let mut router = strand_render::Router::default();
    router.attached(a, bar_a);
    router.attached(b, bar_b);
    let centre = |r: LogicalRect| LogicalPoint::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
    let between = |l: LogicalRect, r: LogicalRect| {
        let (l, r) = (centre(l), centre(r));
        LogicalPoint::new((l.x + r.x) / 2.0, l.y)
    };
    let press = |s, at, state| InputEvent::PointerButton {
        surface: s,
        position: at,
        button: strand_scene::input::button::LEFT,
        state,
        time: 0,
    };
    let motion = |s, at, time| InputEvent::PointerMotion {
        surface: s,
        position: at,
        time,
    };
    // The drop the router reports for `events`: its row and its `at`.
    let dropped = |dock: &mut Dock, router: &mut strand_render::Router, events: Vec<InputEvent>| {
        let mut out = Vec::new();
        for e in &events {
            out.extend(router.handle(e, &mut dock.render));
        }
        let drops: Vec<(NodeId, u32)> = out
            .into_iter()
            .filter_map(|i| match i {
                strand_render::Intent::Event {
                    node,
                    event: strand_render::NodeEvent::Drop { at, .. },
                } => Some((node, at)),
                _ => None,
            })
            .collect();
        assert_eq!(drops.len(), 1, "one drop: {drops:?}");
        drops[0]
    };

    // Along DP-1's dock: Firefox let go between Files and Foot.
    let (firefox, ff) = dock.icon(a, bar_a, "Firefox");
    let (_, files) = dock.icon(a, bar_a, "Files");
    let (_, foot) = dock.icon(a, bar_a, "Foot");
    let to = between(files, foot);
    let (row, at) = dropped(
        &mut dock,
        &mut router,
        vec![
            motion(a, centre(ff), 0),
            press(a, centre(ff), ButtonState::Pressed),
            motion(a, LogicalPoint::new(centre(ff).x + 20.0, centre(ff).y), 10),
            motion(a, to, 20),
            press(a, to, ButtonState::Released),
        ],
    );
    assert_eq!(Some(row), dock.scene.parent(firefox));
    assert_eq!(at, 1);
    dock.drop_on(row, "firefox", i64::from(at));
    assert_eq!(dock.pins(), ["org.gnome.Nautilus", "firefox", "foot"]);
    dock.paint();

    // From DP-1's dock to DP-2's, through the compositor: `at` counts
    // DP-2's own icon for the dragged pin.
    let mut across = |dock: &mut Dock, label: &str, app: &str, left: &str, right: &str| {
        let (source, from) = dock.icon(a, bar_a, label);
        let (_, l) = dock.icon(b, bar_b, left);
        let (_, r) = dock.icon(b, bar_b, right);
        let to = between(l, r);
        let (row, at) = dropped(
            dock,
            &mut router,
            vec![
                motion(a, centre(from), 0),
                press(a, centre(from), ButtonState::Pressed),
                motion(
                    a,
                    LogicalPoint::new(centre(from).x + 20.0, centre(from).y),
                    10,
                ),
                InputEvent::PointerLeave { surface: a },
                InputEvent::DragEnter {
                    surface: b,
                    at: to,
                    kinds: vec![],
                },
                InputEvent::DragMotion { surface: b, at: to },
                InputEvent::DragDrop {
                    surface: b,
                    at: to,
                    payload: DropPayload::Node(source),
                },
            ],
        );
        assert_ne!(Some(row), dock.scene.parent(source), "DP-2's row");
        dock.drop_on(row, app, i64::from(at));
        dock.paint();
        at
    };
    // Foot between Files and Firefox, before DP-2's Foot: right.
    assert_eq!(across(&mut dock, "Foot", "foot", "Files", "Firefox"), 1);
    assert_eq!(dock.pins(), ["org.gnome.Nautilus", "foot", "firefox"]);
    // Files between Foot and Firefox, past DP-2's Files: right too.
    assert_eq!(
        across(&mut dock, "Files", "org.gnome.Nautilus", "Foot", "Firefox"),
        1
    );
    assert_eq!(dock.pins(), ["foot", "org.gnome.Nautilus", "firefox"]);

    // The page says so.
    let page = std::fs::read_to_string(guide::root().join("docs/guide/level-5.md")).unwrap();
    let page = page.split_whitespace().collect::<Vec<_>>().join(" ");
    let said = "so the pin lands where you let it go on either dock";
    assert!(page.contains(said), "level-5.md no longer says: {said}");
    drop(dock);
    let _ = std::fs::remove_dir_all(dir);
}

/// The control centre's `set { $Tile.pad: $space.2 }` reaches the tiles
/// in its row and no other (decisions.md m5-docs, unverified item 4):
/// the row carries the override for its subtree, the tiles read
/// `$Tile.pad` under it, and resolved as render resolves it (the global
/// table, then each ancestor's `tokens=` level, nearest last) a tile in
/// the row is `$space.2` and one outside it `$space.3`.
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
    // Resolved: every tile's `pad`, in the scope render gives its node.
    let top = [&scene.tokens];
    let global = strand_scene::TokenScope::new(&top);
    let tight = global.lookup("space.2").unwrap();
    let loose = global.lookup("space.3").unwrap();
    assert_ne!(tight, loose);
    let (mut inside, mut outside) = (0, 0);
    for id in scene.walk() {
        let Some(pad @ PropValue::Token(_)) = scene.prop(id, Prop::Pad) else {
            continue;
        };
        if !matches!(pad, PropValue::Token(e) if show_expr(e) == "$Tile.pad") {
            continue;
        }
        // The levels from the root down to the node itself.
        let mut chain = vec![id];
        while let Some(p) = scene.parent(*chain.last().unwrap()) {
            chain.push(p);
        }
        let mut levels: Vec<&strand_scene::TokenTable> = vec![&scene.tokens];
        let mut overridden = false;
        for n in chain.iter().rev() {
            if let Some(PropValue::Tokens(t)) = scene.prop(*n, Prop::Tokens) {
                overridden |=
                    t.derived.contains_key("Tile.pad") || t.tokens.contains_key("Tile.pad");
                levels.push(t);
            }
        }
        let got = strand_scene::TokenScope::new(&levels)
            .resolve(pad)
            .map(|v| v.into_owned());
        if overridden {
            inside += 1;
            assert_eq!(got.as_ref(), Some(&tight), "a tile in the row: {shown}");
        } else {
            outside += 1;
            assert_eq!(got.as_ref(), Some(&loose), "a tile outside it: {shown}");
        }
    }
    // The Volume tile (and Brightness, with a backlight) in the row; the
    // rest outside it.
    assert!(inside >= 1, "{shown}");
    assert!(outside >= 1, "{shown}");
}

/// What each file level 5's `from file` services read looks like on a
/// real system, the awkward cases included (decisions.md m5-docs, review
/// round 2 item 1).
const FILE_SAMPLES: &[(&str, &str)] = &[
    ("/proc/sys/kernel/hostname", "mylaptop\n"),
    // openSUSE Tumbleweed's, which carries a comment line.
    (
        "/etc/os-release",
        "NAME=\"openSUSE Tumbleweed\"\n# VERSION=\"20240101\"\nID=\"opensuse-tumbleweed\"\nPRETTY_NAME=\"openSUSE Tumbleweed\"\n",
    ),
];

/// Every field of a level-5 `from file` service reads one value from its
/// file, on the samples above: one line, no quotes (a field that read a
/// whole multi-line file, or a value still in quotes, would show as such
/// in the control centre).
#[test]
fn the_guide_file_services_read_one_value() {
    let text = last_block("services.strand");
    let mut seen = 0;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("service ") else {
            continue;
        };
        let Some((_, rest)) = rest.split_once(" from file \"") else {
            continue;
        };
        seen += 1;
        let (path, rest) = rest.split_once('"').unwrap();
        let sample = FILE_SAMPLES
            .iter()
            .find(|(p, _)| *p == path)
            .unwrap_or_else(|| panic!("no sample of {path}: add one to FILE_SAMPLES"))
            .1;
        let doc = strand_services::custom::Document::parse(sample);
        let body = rest.split_once('{').unwrap().1.rsplit_once('}').unwrap().0;
        for field in body.split(',') {
            let (name, ty) = field.split_once(':').unwrap();
            let key = match ty.split_once('=') {
                Some((_, k)) => k.trim(),
                None => name.trim(),
            };
            let key: Vec<String> = key.split('.').map(str::to_string).collect();
            let got = doc
                .get(&key)
                .unwrap_or_else(|| panic!("{line}: no {key:?} in {path}"));
            let strand_services::Data::Text(got) = got else {
                panic!("{line}: reads {got:?} from {path}");
            };
            assert!(
                !got.is_empty() && !got.contains(['\n', '"', '\'']),
                "{line}: reads {got:?} from {path}"
            );
        }
    }
    assert!(seen >= 1, "level 5 has no `from file` service:\n{text}");
}

/// Level 5's `host` service, run as `strand run` runs it, reads the
/// name the kernel runs under: one line, present on every system, which
/// `/etc/hostname` is not (decisions.md m5-docs, review round 3 item 2:
/// hostname(5) allows `#` comment lines there, which the parser keeps,
/// and some systems leave the file out, which reads as nothing).
#[test]
fn the_guide_host_service_reads_the_kernels_name() {
    use strand_services::custom::{self, Custom, FieldSpec, Source, Spec};
    use strand_services::{Buses, Data, Services};
    let text = last_block("services.strand");
    let line = text
        .lines()
        .find(|l| l.starts_with("service host from file "))
        .unwrap_or_else(|| panic!("no `host` file service:\n{text}"));
    let path = line.split('"').nth(1).unwrap();
    assert_eq!(path, "/proc/sys/kernel/hostname", "{line}");
    let kernel = std::fs::read_to_string(path).unwrap();
    let kernel = kernel.trim();
    assert!(!kernel.is_empty() && !kernel.contains('\n'));
    // The file the page no longer reads, as hostname(5) allows it: the
    // comment is in what every field reads.
    let commented = strand_services::custom::Document::parse("# set by the installer\nmylaptop\n");
    assert_eq!(
        commented.get(&["name".to_string()]),
        Some(Data::Text("# set by the installer\nmylaptop".into()))
    );

    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "host");
    let id = custom::register(Spec {
        name: "host".into(),
        source: Source::File { path: path.into() },
        fields: vec![FieldSpec {
            name: "name".into(),
            key: vec!["name".into()],
            rw: false,
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let value = || {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let want = Some(Data::Text(kernel.into()));
    loop {
        services.pump(&rt);
        rt.flush();
        if value() == want {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "host.name is {:?}, the kernel's name {kernel:?}",
            value()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// What level 5 states for `key=value` files: blank and `#` comment
/// lines are skipped, a value's double or single quotes removed, and one
/// line that is neither a pair nor a comment makes the whole file text,
/// which every field reads.
#[test]
fn the_guide_states_the_pairs_rule() {
    use strand_services::custom::Document;
    let commented = FILE_SAMPLES
        .iter()
        .find(|(p, _)| *p == "/etc/os-release")
        .unwrap()
        .1;
    let Document::Pairs(p) = Document::parse(commented) else {
        panic!("os-release with a comment is not pairs");
    };
    assert!(p.contains(&("PRETTY_NAME".into(), "openSUSE Tumbleweed".into())));
    let Document::Pairs(p) = Document::parse("A=\"x\"\nB='y'\n") else {
        panic!("not pairs");
    };
    assert_eq!(p, [("A".into(), "x".into()), ("B".into(), "y".into())]);
    assert!(matches!(
        Document::parse("A=x\nnot a pair\n"),
        Document::Text(_)
    ));
    let page = std::fs::read_to_string(guide::root().join("docs/guide/level-5.md")).unwrap();
    let page = page.split_whitespace().collect::<Vec<_>>().join(" ");
    for said in [
        "with blank lines and `#` comment lines skipped and the double or single quotes around a value removed, but only when every other line is a pair",
        "one line that is neither makes the whole file text again",
    ] {
        assert!(page.contains(said), "level-5.md no longer says: {said}");
    }
}

/// (Review minor 13) Level 2's "same name in two files" paragraph quotes
/// the compiler's error and help as they are, and its token path reads
/// as it says.
#[test]
fn the_guide_quotes_the_same_name_error() {
    let item = |s: &str| {
        format!("component Item tokens {{ gap: 4 }} {{ text \"{s}\" {{ margin: $Item.gap }} }}\n")
    };
    let mut map = SourceMap::new();
    map.add("dot.strand", item("d"));
    map.add("tray.strand", item("t"));
    map.add(
        "bar.strand",
        "bar B { Item; text \"x\" { margin: $dot.Item.gap } }\n",
    );
    let out = strand_compiler::compile(&map);
    let d: Vec<_> = out.diagnostics.iter().collect();
    assert_eq!(
        d.len(),
        1,
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    let page = std::fs::read_to_string(guide::root().join("docs/guide/level-2.md")).unwrap();
    let error = format!("× {}", d[0].message);
    let help = format!("help: {}", d[0].help.as_deref().unwrap());
    assert!(page.contains(&error), "level-2.md does not quote: {error}");
    assert!(page.contains(&help), "level-2.md does not quote: {help}");
}

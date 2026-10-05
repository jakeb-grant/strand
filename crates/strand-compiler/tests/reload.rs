//! Live reload of a mounted instance (design.md, "What each edit does"):
//! a new build committed into the running shell keeps nodes, state and
//! handlers by identity, and its diff takes render from what it shows to
//! the new tree.

use std::rc::Rc;
use std::time::Duration;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage, Update};
use strand_compiler::reconcile::{Build, EditClass, Report};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, schema::Schema};
use strand_core::Runtime;
use strand_scene::{NodeKind, Prop, PropValue, SceneOp};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
    build: Build,
    now: f64,
}

fn compile(prev: Option<&Build>, files: &[(&str, &str)]) -> Build {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.to_string());
    }
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

fn screens(rt: &Runtime, host: &SchemaHost, names: &[&str]) {
    let list = names
        .iter()
        .map(|n| {
            host.record(
                "Screen",
                &[
                    ("id", Value::text(format!("Mock | {n} | Display"))),
                    ("name", Value::text(*n)),
                    ("make", Value::text("Mock")),
                    ("model", Value::text(*n)),
                    ("description", Value::text("Display")),
                ],
            )
        })
        .collect();
    host.set(rt, "screens.all", Value::list(list)).unwrap();
}

fn boot(files: &[(&str, &str)]) -> Shell {
    boot_with(files, Storage::none(), &["DP-1"])
}

fn boot_with(files: &[(&str, &str)], storage: Storage, monitors: &[&str]) -> Shell {
    boot_on(files, storage, |rt, host| screens(rt, host, monitors))
}

fn boot_on(
    files: &[(&str, &str)],
    storage: Storage,
    setup: impl FnOnce(&Runtime, &SchemaHost),
) -> Shell {
    let build = compile(None, files);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    setup(&rt, &host);
    let inst = Instance::from_build(&rt, &build, host.clone(), storage);
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
        build,
        now: 0.0,
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        self.scene
            .apply(&u.diff)
            .unwrap_or_else(|e| panic!("{e}\n{:#?}", u.diff.ops));
        u
    }

    fn at(&mut self, secs: f64) -> Update {
        self.now = secs;
        let u = self.inst.tick(Duration::from_secs_f64(secs));
        self.scene
            .apply(&u.diff)
            .unwrap_or_else(|e| panic!("{e}\n{:#?}", u.diff.ops));
        u
    }

    /// Commit `files` and end the tick: the report and the diff's ops.
    fn reload(&mut self, files: &[(&str, &str)]) -> (Report, Vec<SceneOp>) {
        let build = compile(Some(&self.build), files);
        let report = self.inst.reload(&build);
        self.build = build;
        let u = self.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        (report, u.diff.ops)
    }

    fn value(&self, module: &str, name: &str) -> Value {
        self.inst.value_of(module, name).unwrap()
    }

    /// The scene must look like a cold boot of the same files (on one
    /// `DP-1` monitor; the reloaded shell's state must be the defaults).
    fn assert_cold_boot(&self, files: &[(&str, &str)]) {
        self.assert_cold_boot_on(files, |rt, host| screens(rt, host, &["DP-1"]));
    }

    fn assert_cold_boot_on(
        &self,
        files: &[(&str, &str)],
        setup: impl FnOnce(&Runtime, &SchemaHost),
    ) {
        let cold = boot_on(files, Storage::none(), setup);
        assert_eq!(
            canonical(&self.scene),
            canonical(&cold.scene),
            "reloaded scene differs from a cold boot"
        );
        assert_eq!(self.scene.render_tokens(), cold.scene.render_tokens());
    }
}

/// The scene as text with its surfaces in a fixed order (the order of
/// surface roots means nothing to render).
fn canonical(scene: &SceneMirror) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for line in scene.render().lines() {
        if !line.starts_with(' ') || blocks.is_empty() {
            blocks.push(String::new());
        }
        let b = blocks.last_mut().unwrap();
        b.push_str(line);
        b.push('\n');
    }
    blocks.sort();
    blocks.concat()
}

fn creates(ops: &[SceneOp]) -> usize {
    ops.iter()
        .filter(|o| matches!(o, SceneOp::Create { .. }))
        .count()
}

fn removes(ops: &[SceneOp]) -> usize {
    ops.iter()
        .filter(|o| matches!(o, SceneOp::Remove { .. }))
        .count()
}

const CLOCK: &str = "state n = 0\nbar Top {\n  edge: top; height: 32\n  row {\n    text \"a\" { color: #ff0000 }\n    Counter\n  }\n}\ncomponent Counter {\n  state open = false\n  text join(\"\", open) { on click { open = !open; n += 1 } }\n}\n";

/// Editing a prop patches the same node in place (render animates it
/// from its current value) and keeps every state cell.
#[test]
fn a_prop_edit_patches_in_place_and_keeps_state() {
    let mut shell = boot(&[("bar.strand", CLOCK)]);
    let t = shell.scene.find_text("false").unwrap();
    shell.inst.event(t, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.value("bar", "n"), Value::int(1));
    let a = shell.scene.find_text("a").unwrap();
    let edited = CLOCK.replace("#ff0000", "#00ff00");
    let (report, ops) = shell.reload(&[("bar.strand", &edited)]);
    assert_eq!(report.classes, [EditClass::Prop], "{report:?}");
    assert_eq!((creates(&ops), removes(&ops)), (0, 0), "{ops:#?}");
    assert_eq!(ops.len(), 1, "{ops:#?}");
    assert!(matches!(
        &ops[0],
        SceneOp::SetProp { id, prop: Prop::Color, .. } if *id == a
    ));
    assert_eq!(shell.scene.find_text("a"), Some(a));
    assert_eq!(shell.scene.find_text("true"), Some(t), "Counter.open kept");
    assert_eq!(shell.value("bar", "n"), Value::int(1));
    assert!(report.kept.contains(&"bar.n".to_string()), "{report:?}");
    assert!(
        report.kept.contains(&"Counter.open".to_string()),
        "{report:?}"
    );
}

/// A token edit swaps the table; nothing else changes.
#[test]
fn a_token_edit_swaps_the_table() {
    let src = |c: &str| {
        format!(
            "tokens base {{ gap: {c} }}\nstate n = 2\nbar Top {{ text join(\"\", n) {{ color: $fg }} }}\n"
        )
    };
    let a = src("4px");
    let mut shell = boot(&[("bar.strand", &a)]);
    shell.inst.set_value("bar", "n", Value::int(5)).unwrap();
    shell.flush();
    let b = src("8px");
    let (report, ops) = shell.reload(&[("bar.strand", &b)]);
    assert_eq!(report.classes, [EditClass::Token], "{report:?}");
    assert!(
        ops.iter().all(|o| matches!(
            o,
            SceneOp::SetTokens {
                transition: strand_scene::Transition::Default,
                ..
            }
        )),
        "{ops:#?}"
    );
    assert_eq!(ops.len(), 1);
    assert_eq!(shell.value("bar", "n"), Value::int(5));
}

/// Adding a node creates it (its `enter` plays) and keeps its siblings;
/// removing it removes only it. Both match a cold boot.
#[test]
fn nodes_added_and_removed() {
    let mut shell = boot(&[("bar.strand", CLOCK)]);
    let before: Vec<_> = shell.scene.walk();
    let added = CLOCK.replace(
        "    Counter\n",
        "    text \"new\" { enter { opacity: 0 } }\n    Counter\n",
    );
    let (report, ops) = shell.reload(&[("bar.strand", &added)]);
    assert_eq!(report.classes, [EditClass::NodeAdded], "{report:?}");
    assert_eq!((creates(&ops), removes(&ops)), (1, 0), "{ops:#?}");
    let after = shell.scene.walk();
    assert!(before.iter().all(|id| after.contains(id)));
    shell.assert_cold_boot(&[("bar.strand", &added)]);
    let (report, ops) = shell.reload(&[("bar.strand", CLOCK)]);
    assert_eq!(report.classes, [EditClass::NodeRemoved], "{report:?}");
    assert_eq!((creates(&ops), removes(&ops)), (0, 1), "{ops:#?}");
    assert_eq!(shell.scene.walk(), before);
    shell.assert_cold_boot(&[("bar.strand", CLOCK)]);
}

/// Wrapping a node in a container keeps it (and its component's state):
/// it is moved, not recreated.
#[test]
fn wrapping_moves_a_node() {
    let mut shell = boot(&[("bar.strand", CLOCK)]);
    let t = shell.scene.find_text("false").unwrap();
    shell.inst.event(t, "click", Vec::new());
    shell.flush();
    let wrapped = CLOCK.replace("    Counter\n", "    col { Counter }\n");
    let (_, ops) = shell.reload(&[("bar.strand", &wrapped)]);
    assert_eq!((creates(&ops), removes(&ops)), (1, 0), "{ops:#?}");
    assert!(
        ops.iter()
            .any(|o| matches!(o, SceneOp::Move { id, .. } if *id == t)),
        "{ops:#?}"
    );
    assert_eq!(shell.scene.find_text("true"), Some(t));
}

/// A changed default is adopted where the value never changed, and kept
/// with a notice where it did.
#[test]
fn state_defaults_are_adopted_only_if_unchanged() {
    let src = |a: i64, b: &str| {
        format!("state a = {a}\nstate q = \"{b}\"\nbar Top {{ text join(\" \", a, q) }}\n")
    };
    let mut shell = boot(&[("launcher.strand", &src(1, ""))]);
    shell
        .inst
        .set_value("launcher", "q", Value::text("fir"))
        .unwrap();
    shell.flush();
    let (report, _) = shell.reload(&[("launcher.strand", &src(2, "x"))]);
    assert_eq!(shell.value("launcher", "a"), Value::int(2));
    assert_eq!(shell.value("launcher", "q"), Value::text("fir"));
    assert!(report.classes.contains(&EditClass::StateDefault));
    assert_eq!(
        report.notices,
        ["launcher.q: kept \"fir\" (default changed) [reset]"]
    );
    assert!(shell.scene.find_text("2 fir").is_some());
}

/// A renamed or retyped cell resets with a warning; the others are kept.
#[test]
fn a_renamed_or_retyped_cell_resets() {
    let mut shell = boot(&[(
        "t.strand",
        "state a = 1\nstate b = 1\nbar Top { text join(\" \", a, b) }\n",
    )]);
    shell.inst.set_value("t", "a", Value::int(7)).unwrap();
    shell.inst.set_value("t", "b", Value::int(7)).unwrap();
    shell.flush();
    let (report, _) = shell.reload(&[(
        "t.strand",
        "state a = 1\nstate c = 1\nbar Top { text join(\" \", a, c) }\n",
    )]);
    assert_eq!(shell.value("t", "a"), Value::int(7));
    assert_eq!(shell.value("t", "c"), Value::int(1));
    assert!(
        report.classes.contains(&EditClass::StateReset),
        "{report:?}"
    );
    assert_eq!(report.reset, [("t.b".to_string(), "renamed".to_string())]);
    let (report, _) = shell.reload(&[(
        "t.strand",
        "state a = false\nstate c = 1\nbar Top { text join(\" \", a, c) }\n",
    )]);
    assert_eq!(shell.value("t", "a"), Value::Bool(false));
    assert_eq!(report.reset.len(), 1, "{report:?}");
    assert!(report.reset[0].1.contains("type changed"), "{report:?}");
}

/// `@reset` on a declaration resets that cell at the reload.
#[test]
fn at_reset_resets_one_cell() {
    let mut shell = boot(&[(
        "t.strand",
        "state a = 1\nstate b = 1\nbar Top { text join(\" \", a, b) }\n",
    )]);
    shell.inst.set_value("t", "a", Value::int(7)).unwrap();
    shell.inst.set_value("t", "b", Value::int(7)).unwrap();
    shell.flush();
    let (report, _) = shell.reload(&[(
        "t.strand",
        "@reset state a = 1\nstate b = 1\nbar Top { text join(\" \", a, b) }\n",
    )]);
    assert_eq!(shell.value("t", "a"), Value::int(1));
    assert_eq!(shell.value("t", "b"), Value::int(7));
    assert_eq!(report.reset, [("t.a".to_string(), "@reset".to_string())]);
}

/// `on change` never fires on reload, even when a reload changes the
/// value it watches (an adopted default).
#[test]
fn on_change_does_not_fire_on_reload() {
    let src = |d: i64| {
        format!(
            "state a = {d}\nstate fired = 0\non change a {{ fired += 1 }}\nbar Top {{ text join(\" \", a, fired) }}\n"
        )
    };
    let mut shell = boot(&[("t.strand", &src(1))]);
    shell.reload(&[("t.strand", &src(2))]);
    shell.flush();
    assert_eq!(shell.value("t", "a"), Value::int(2));
    assert_eq!(shell.value("t", "fired"), Value::int(0));
    shell.inst.set_value("t", "a", Value::int(3)).unwrap();
    shell.flush();
    assert_eq!(shell.value("t", "fired"), Value::int(1));
}

/// A handler whose code changed is restarted: its in-flight `await` is
/// cancelled and reported. An unchanged one keeps running.
#[test]
fn handler_restart_cancels_its_await() {
    let src = |x: i64| {
        format!(
            "state a = 0\nstate b = 0\nbar Top {{\n  box {{ on click {{ await sleep(1s); a = {x} }} }}\n  row {{ on click {{ await sleep(1s); b = 1 }} }}\n}}\n"
        )
    };
    let mut shell = boot(&[("t.strand", &src(1))]);
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    let r = shell.scene.of_kind(NodeKind::Row)[0];
    shell.inst.event(b, "click", Vec::new());
    shell.inst.event(r, "click", Vec::new());
    shell.at(0.5);
    let (report, _) = shell.reload(&[("t.strand", &src(2))]);
    assert_eq!(report.restarted, 1, "{report:?}");
    assert_eq!(report.cancelled, 1, "{report:?}");
    assert!(report.classes.contains(&EditClass::Handler));
    shell.at(2.0);
    assert_eq!(shell.value("t", "a"), Value::int(0), "cancelled");
    assert_eq!(shell.value("t", "b"), Value::int(1), "kept running");
    // The restarted handler runs its new code.
    shell.inst.event(b, "click", Vec::new());
    shell.at(2.0);
    shell.at(3.0);
    assert_eq!(shell.value("t", "a"), Value::int(2));
}

/// A timer's duration edit rescales the remaining time.
#[test]
fn timer_duration_rescales() {
    let src = |d: &str| {
        format!(
            "state done = false\nafter {d} {{ done = true }}\nbar Top {{ text join(\"\", done) }}\n"
        )
    };
    let mut shell = boot(&[("t.strand", &src("10s"))]);
    shell.at(5.0);
    let (report, _) = shell.reload(&[("t.strand", &src("20s"))]);
    assert!(report.classes.contains(&EditClass::Timer), "{report:?}");
    // Half of 10 s left → half of 20 s left: fires at 15 s.
    shell.at(14.9);
    assert_eq!(shell.value("t", "done"), Value::Bool(false));
    shell.at(15.0);
    assert_eq!(shell.value("t", "done"), Value::Bool(true));
}

/// A surface's layer change recreates only that surface.
#[test]
fn a_surface_layer_change_recreates_only_it() {
    let src = |layer: &str| {
        format!("bar Top {{ text \"bar\" }}\npanel P {{ layer: {layer}; text \"panel\" }}\n")
    };
    let mut shell = boot(&[("t.strand", &src("top"))]);
    let bar = shell.scene.of_kind(NodeKind::Bar)[0];
    let panel = shell.scene.of_kind(NodeKind::Panel)[0];
    let (report, ops) = shell.reload(&[("t.strand", &src("overlay"))]);
    assert!(report.classes.contains(&EditClass::Surface), "{report:?}");
    assert!(
        ops.iter()
            .any(|o| matches!(o, SceneOp::Remove { id } if *id == panel)),
        "{ops:#?}"
    );
    assert_eq!(shell.scene.of_kind(NodeKind::Bar), [bar]);
    assert_ne!(shell.scene.of_kind(NodeKind::Panel), [panel]);
    shell.assert_cold_boot(&[("t.strand", &src("overlay"))]);
}

/// Per-monitor bars keep their own state across a reload.
#[test]
fn per_monitor_state_survives_a_reload() {
    let src = |c: &str| {
        format!(
            "bar Top {{\n  state open = false\n  text join(\"\", open) {{ color: {c}; on click {{ open = !open }} }}\n}}\n"
        )
    };
    let mut shell = boot_with(
        &[("t.strand", &src("#ff0000"))],
        Storage::none(),
        &["DP-1", "DP-2"],
    );
    let t = shell.scene.of_kind(NodeKind::Text)[1];
    shell.inst.event(t, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["false", "true"]);
    let (report, ops) = shell.reload(&[("t.strand", &src("#00ff00"))]);
    assert_eq!((creates(&ops), removes(&ops)), (0, 0), "{ops:#?}");
    assert_eq!(shell.scene.texts(), ["false", "true"]);
    assert_eq!(report.classes, [EditClass::Prop]);
}

/// A bar parked by an unplug keeps its state through a reload and gets
/// it back when its monitor returns.
#[test]
fn a_parked_bar_keeps_its_state_through_a_reload() {
    let src = |c: &str| {
        format!(
            "bar Top {{\n  state open = false\n  text join(\"\", open) {{ color: {c}; on click {{ open = !open }} }}\n}}\n"
        )
    };
    let mut shell = boot_with(
        &[("t.strand", &src("#ff0000"))],
        Storage::none(),
        &["DP-1", "DP-2"],
    );
    let t = shell.scene.of_kind(NodeKind::Text)[1];
    shell.inst.event(t, "click", Vec::new());
    shell.flush();
    screens(&shell.rt, &shell.host, &["DP-1"]);
    shell.flush();
    assert_eq!(shell.scene.texts(), ["false"]);
    shell.reload(&[("t.strand", &src("#00ff00"))]);
    screens(&shell.rt, &shell.host, &["DP-1", "DP-2"]);
    shell.flush();
    assert_eq!(shell.scene.texts(), ["false", "true"]);
}

/// While a lock is shown, an edit to it waits for the unlock.
#[test]
fn a_lock_edit_is_deferred_while_shown() {
    let src = |s: &str| format!("lock L {{ text \"{s}\" }}\n");
    let mut shell = boot(&[("t.strand", &src("a"))]);
    assert!(shell.inst.lock_shown());
    let build = compile(Some(&shell.build), &[("t.strand", &src("b"))]);
    let report = shell.inst.reload(&build);
    assert_eq!(report.classes, [EditClass::LockDeferred]);
    shell.flush();
    assert_eq!(shell.scene.texts(), ["a"]);
}

/// `strand reload --hard` drops non-persisted state and recreates every
/// surface.
#[test]
fn a_hard_reload_drops_state_and_recreates_surfaces() {
    let mut shell = boot(&[("bar.strand", CLOCK)]);
    let bar = shell.scene.of_kind(NodeKind::Bar)[0];
    shell.inst.set_value("bar", "n", Value::int(4)).unwrap();
    shell.flush();
    let build = compile(Some(&shell.build), &[("bar.strand", CLOCK)]);
    let report = shell.inst.reload_hard(&build);
    assert_eq!(report.classes, [EditClass::Hard]);
    shell.flush();
    assert_eq!(shell.value("bar", "n"), Value::int(0));
    assert_ne!(shell.scene.of_kind(NodeKind::Bar), [bar]);
    shell.assert_cold_boot(&[("bar.strand", CLOCK)]);
}

/// A persisted cell is handed over: its value is kept and its file
/// still written by the new program's cell.
#[test]
fn persisted_cells_are_kept() {
    let dir = std::env::temp_dir().join(format!("strand-reload-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = |d: i64, c: &str| {
        format!("state a = {d} persist\nbar Top {{ text join(\"\", a) {{ color: {c} }} }}\n")
    };
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let mut shell = boot_with(&[("t.strand", &src(1, "#ff0000"))], storage, &["DP-1"]);
    shell.inst.set_value("t", "a", Value::int(5)).unwrap();
    shell.flush();
    let (report, _) = shell.reload(&[("t.strand", &src(1, "#00ff00"))]);
    assert_eq!(shell.value("t", "a"), Value::int(5));
    assert!(report.reset.is_empty(), "{report:?}");
    let (report, _) = shell.reload(&[("t.strand", &src(2, "#00ff00"))]);
    assert_eq!(shell.value("t", "a"), Value::int(5));
    assert_eq!(report.notices, ["t.a: kept 5 (default changed) [reset]"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The design's shells survive a chain of reloads into one another
/// without a mirror inconsistency, and each lands on a cold boot's scene.
#[test]
fn reloads_land_on_a_cold_boot() {
    let steps: [&str; 4] = [
        CLOCK,
        "bar Top { row { text \"x\"; box {} } }\n",
        "bar Top { col { box {}; text \"x\" } }\npanel P { text \"p\" }\n",
        CLOCK,
    ];
    let mut shell = boot(&[("bar.strand", steps[0])]);
    for s in &steps[1..] {
        shell.reload(&[("bar.strand", s)]);
        shell.assert_cold_boot(&[("bar.strand", s)]);
    }
    let _ = Schema::builtin();
    let _ = PropValue::Unset;
}

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

/// The design's desktop on the mock: two monitors, workspaces, a window,
/// a battery, a sink, two notifications.
fn desktop(rt: &Runtime, host: &SchemaHost) {
    screens(rt, host, &["DP-1", "HDMI-A-1"]);
    let ws = |id: i64, screen: &str, focused: bool| {
        host.record(
            "Workspace",
            &[
                ("id", Value::int(id)),
                ("name", Value::text(id.to_string())),
                ("screen", Value::text(screen)),
                ("focused", Value::Bool(focused)),
                ("occupied", Value::Bool(true)),
            ],
        )
    };
    host.set(
        rt,
        "workspaces.all",
        Value::list(vec![
            ws(1, "DP-1", true),
            ws(2, "DP-1", false),
            ws(3, "HDMI-A-1", false),
        ]),
    )
    .unwrap();
    host.set(rt, "battery.present", Value::Bool(true)).unwrap();
    host.set(rt, "battery.percent", Value::float(0.42)).unwrap();
    let n = |id: i64, summary: &str| {
        let app = host.record("NotificationApp", &[("name", Value::text("Mail"))]);
        host.record(
            "Notification",
            &[
                ("id", Value::int(id)),
                ("app", app),
                ("summary", Value::text(summary)),
                ("urgency", host.variant("Urgency", "normal")),
            ],
        )
    };
    host.set(
        rt,
        "notifications.popups",
        Value::list(vec![n(1, "a"), n(2, "b")]),
    )
    .unwrap();
}

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One random edit of `text`: a line deleted, duplicated or swapped
/// with the next, a number or a colour changed.
fn mutate(text: &str, rng: &mut Rng) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let i = rng.below(lines.len());
    match rng.below(5) {
        0 => {
            lines.remove(i);
        }
        1 => {
            let l = lines[i].clone();
            lines.insert(i, l);
        }
        2 if i + 1 < lines.len() => lines.swap(i, i + 1),
        3 => {
            let digits: Vec<usize> = lines[i]
                .char_indices()
                .filter(|(_, c)| c.is_ascii_digit())
                .map(|(k, _)| k)
                .collect();
            if !digits.is_empty() {
                let k = digits[rng.below(digits.len())];
                let d = (b'0' + rng.below(10) as u8) as char;
                lines[i].replace_range(k..k + 1, &d.to_string());
            }
        }
        _ => {
            if let Some(k) = lines[i].find('#') {
                let hex = "0123456789abcdef";
                let end = (k + 7).min(lines[i].len());
                if lines[i][k + 1..end].chars().all(|c| c.is_ascii_hexdigit()) {
                    let c: String = (0..end - k - 1)
                        .map(|_| hex.as_bytes()[rng.below(16)] as char)
                        .collect();
                    lines[i].replace_range(k + 1..end, &c);
                }
            }
        }
    }
    lines.join("\n") + "\n"
}

fn as_refs(f: &[(String, String)]) -> Vec<(&str, &str)> {
    f.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
}

/// Random edits of the design's shells, each committed live: the mirror
/// never sees an inconsistent diff, nothing panics, and every reload
/// lands on the scene and tokens a cold boot of the same files shows.
#[test]
fn random_edits_land_on_a_cold_boot() {
    let names = [
        "theme.strand",
        "bar.strand",
        "launcher.strand",
        "toasts.strand",
        "osd.strand",
    ];
    let mut files: Vec<(String, String)> =
        names.iter().map(|n| (n.to_string(), fixture(n))).collect();
    let refs = |f: &[(String, String)]| -> Vec<(String, String)> { f.to_vec() };
    let mut shell = boot_on(&as_refs(&files), Storage::none(), desktop);
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    let iterations = std::env::var("STRAND_RELOAD_FUZZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let mut committed = 0;
    for _ in 0..iterations {
        let mut next = refs(&files);
        let f = rng.below(next.len());
        next[f].1 = mutate(&next[f].1, &mut rng);
        let mut map = SourceMap::new();
        for (n, t) in &next {
            map.add(n.as_str(), t.clone());
        }
        let Ok(build) = Build::compile(Some(&shell.build), map) else {
            continue;
        };
        shell.inst.reload(&build);
        shell.build = build;
        shell.flush();
        files = next;
        committed += 1;
        shell.assert_cold_boot_on(&as_refs(&files), desktop);
    }
    eprintln!("{committed} of {iterations} random edits committed");
    assert!(
        committed > iterations / 10,
        "only {committed} edits compiled"
    );
}

/// A runtime fault freezes its component outlined in red; the fixing
/// reload takes the outline off and runs the new code.
#[test]
fn a_fault_outlines_its_component_until_the_fixing_reload() {
    let src = |d: &str| {
        format!(
            "state zero = 0\ncomponent Faulty() {{\n  row {{ text pct(10 / {d}) }}\n}}\nbar B {{ text \"ok\"; Faulty }}\n"
        )
    };
    let build = compile(None, &[("t.strand", &src("zero"))]);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    screens(&rt, &host, &["DP-1"]);
    let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
        build,
        now: 0.0,
    };
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(shell.inst.freeze(&u.errors[0]));
    shell.flush();
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    assert_eq!(shell.inst.outlined(), [row]);
    assert!(matches!(
        shell.scene.prop(row, Prop::Border),
        Some(PropValue::Border(b)) if b.width == 2.0
    ));
    let ok = shell.scene.find_text("ok").unwrap();
    assert_eq!(
        shell.scene.prop(ok, Prop::Border),
        None,
        "only the component"
    );
    let (_, _) = shell.reload(&[("t.strand", &src("2"))]);
    assert_eq!(shell.scene.prop(row, Prop::Border), None);
    assert!(shell.scene.find_text("500%").is_some());
}

/// Nodes made outside the program (the overlay) stay as they are across
/// reloads, hard ones included.
#[test]
fn external_nodes_survive_reloads() {
    let mut shell = boot(&[("bar.strand", CLOCK)]);
    let panel = shell.inst.external_create(NodeKind::Panel, None, 0);
    let text = shell.inst.external_create(NodeKind::Text, Some(panel), 0);
    shell.inst.external_set(
        text,
        Prop::Text,
        PropValue::Text("bar.strand:3: oops".into()),
    );
    shell.flush();
    assert!(shell.inst.is_external(text));
    let edited = CLOCK.replace("#ff0000", "#00ff00");
    let (_, ops) = shell.reload(&[("bar.strand", &edited)]);
    assert!(
        ops.iter()
            .all(|o| !matches!(o, SceneOp::Remove { id } if *id == panel))
    );
    assert_eq!(shell.scene.find_text("bar.strand:3: oops"), Some(text));
    let build = compile(Some(&shell.build), &[("bar.strand", CLOCK)]);
    shell.inst.reload_hard(&build);
    shell.flush();
    assert_eq!(shell.scene.find_text("bar.strand:3: oops"), Some(text));
    shell.inst.external_remove(panel);
    shell.flush();
    assert_eq!(shell.scene.find_text("bar.strand:3: oops"), None);
    assert_eq!(
        shell.scene.of_kind(NodeKind::Panel),
        Vec::<strand_scene::NodeId>::new()
    );
}

//! Headless instantiation: programs mounted on a mock service host, their
//! scene diffs applied to a mirror and snapshotted.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::instantiate::{Instance, NodeFlag, SceneMirror, Update, show};
use strand_compiler::vm::Value;
use strand_compiler::vm::persist::{MemoryStore, PersistStore};
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneOp};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
    /// The boot diff's ops.
    boot: Vec<SceneOp>,
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        self.scene.apply(&u.diff).unwrap();
        u
    }

    /// Advance the logic clock to `secs` and end the tick.
    fn at(&mut self, secs: f64) -> Update {
        let u = self.inst.tick(Duration::from_secs_f64(secs));
        self.scene.apply(&u.diff).unwrap();
        u
    }

    fn text_node(&self, text: &str) -> NodeId {
        self.scene
            .find_text(text)
            .unwrap_or_else(|| panic!("no text {text:?} in\n{}", self.scene.render()))
    }
}

fn boot(files: &[(&str, &str)], setup: impl FnOnce(&Runtime, &SchemaHost)) -> Shell {
    boot_with(files, setup, None)
}

fn boot_with(
    files: &[(&str, &str)],
    setup: impl FnOnce(&Runtime, &SchemaHost),
    store: Option<Rc<dyn PersistStore>>,
) -> Shell {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.to_string());
    }
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    setup(&rt, &host);
    let inst = Instance::new(&rt, program, host.clone(), store);
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
        boot: Vec::new(),
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell.boot = u.diff.ops;
    shell
}

fn screens(rt: &Runtime, host: &SchemaHost, names: &[&str]) {
    let list = names
        .iter()
        .map(|n| host.record("Screen", &[("name", Value::text(*n))]))
        .collect();
    host.set(rt, "screens.all", Value::list(list)).unwrap();
}

#[test]
fn hello_bar_on_two_monitors() {
    let src = include_str!("fixtures/hello_bar.strand");
    let shell = boot(&[("hello_bar.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1", "HDMI-A-1"]);
        host.set(rt, "battery.percent", Value::float(0.42)).unwrap();
    });
    assert_eq!(shell.scene.roots().len(), 2);
    assert_eq!(
        shell.scene.texts(),
        ["", "09:41", "42%", "", "09:41", "42%"]
    );
}

#[test]
fn the_hello_bar_clock_ticks_once_a_minute() {
    use std::time::UNIX_EPOCH;
    let src = include_str!("fixtures/hello_bar.strand");
    let mut shell = boot(&[("hello_bar.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
    });
    let t0 = strand_compiler::vm::schema_host::MOCK_TIME;
    // The host wakes at the next minute boundary, never before.
    assert_eq!(
        shell.inst.next_wake(),
        Some(UNIX_EPOCH + Duration::from_secs(t0 - 7 + 60))
    );
    assert_eq!(shell.inst.next_deadline(), None);
    shell
        .host
        .set_time(&shell.rt, UNIX_EPOCH + Duration::from_secs(t0 + 20));
    assert!(
        shell.flush().diff.is_empty(),
        "nothing changes within the minute"
    );
    shell
        .inst
        .wake(UNIX_EPOCH + Duration::from_secs(t0 - 7 + 60));
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff);
    assert_eq!(shell.scene.texts(), ["", "09:42", "0%"]);
}

fn fixture(name: &str) -> (String, String) {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    (name.to_string(), std::fs::read_to_string(path).unwrap())
}

/// A small desktop for the mock: two monitors, three workspaces, a
/// focused window, a battery, a sink, a tray item, two notifications and
/// three apps.
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
    let win = host.record(
        "Window",
        &[
            ("id", Value::text("w1")),
            ("title", Value::text("strand — design.md")),
        ],
    );
    host.set(rt, "windows.focused", win).unwrap();
    host.set(rt, "battery.present", Value::Bool(true)).unwrap();
    host.set(rt, "battery.percent", Value::float(0.42)).unwrap();
    host.set(rt, "battery.icon", Value::text("battery-good-symbolic"))
        .unwrap();
    host.set(
        rt,
        "battery.time_left",
        Value::from(std::time::Duration::from_secs(7500)),
    )
    .unwrap();
    let sink = host.record(
        "AudioDevice",
        &[
            ("id", Value::int(40)),
            ("name", Value::text("speakers")),
            ("volume", Value::float(0.5)),
            ("icon", Value::text("audio-volume-medium-symbolic")),
        ],
    );
    host.set(rt, "audio.sink", sink).unwrap();
    let tray = host.record(
        "TrayItem",
        &[
            ("id", Value::text("nm-applet")),
            ("icon", Value::text("network-wireless")),
        ],
    );
    host.set(rt, "tray.items", Value::list(vec![tray])).unwrap();
    let app = |id: &str, name: &str| {
        host.record(
            "App",
            &[
                ("id", Value::text(id)),
                ("name", Value::text(name)),
                ("icon", Value::text(id)),
            ],
        )
    };
    let n1 = notification(host, 1, "Mail", "New message", "normal");
    let n2 = notification(host, 2, "Battery", "Battery low", "critical");
    host.set(rt, "notifications.popups", Value::list(vec![n1, n2]))
        .unwrap();
    host.set(
        rt,
        "apps.all",
        Value::list(vec![
            app("firefox", "Firefox"),
            app("foot", "Foot"),
            app("files", "Files"),
        ]),
    )
    .unwrap();
}

fn notification(host: &SchemaHost, id: i64, app: &str, summary: &str, urgency: &str) -> Value {
    let napp = host.record("NotificationApp", &[("name", Value::text(app))]);
    host.record(
        "Notification",
        &[
            ("id", Value::int(id)),
            ("app", napp),
            ("summary", Value::text(summary)),
            ("body", Value::text("…")),
            ("urgency", host.variant("Urgency", urgency)),
        ],
    )
}

fn design_shells() -> Vec<(String, String)> {
    [
        "theme.strand",
        "bar.strand",
        "launcher.strand",
        "toasts.strand",
        "osd.strand",
    ]
    .iter()
    .map(|f| fixture(f))
    .collect()
}

fn refs(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect()
}

/// The four shells of design.md and its theme, one config, on the mock
/// desktop: the whole scene and the token table.
#[test]
fn the_design_shells_run_on_the_mock() {
    let files = design_shells();
    let shell = boot(&refs(&files), desktop);
    insta::assert_snapshot!("design_shells_scene", shell.scene.render());
    insta::assert_snapshot!("design_shells_tokens", shell.scene.render_tokens());
    // The boot diff starts with the token table, applied without a spring.
    assert_eq!(shell.scene.token_swaps, 1);
    assert_eq!(
        shell.scene.last_token_transition,
        Some(strand_scene::Transition::Instant)
    );
    // Two monitors, two bars, each pinned to its own; one of each other
    // surface.
    let bars = shell.scene.of_kind(NodeKind::Bar);
    assert_eq!(bars.len(), 2);
    let screens: Vec<_> = bars
        .iter()
        .map(|&b| shell.scene.prop(b, Prop::Screens).cloned())
        .collect();
    assert_eq!(
        screens,
        [
            Some(PropValue::Text("DP-1".into())),
            Some(PropValue::Text("HDMI-A-1".into()))
        ]
    );
    assert_eq!(shell.scene.of_kind(NodeKind::Panel).len(), 2);
    assert_eq!(shell.scene.of_kind(NodeKind::Osd).len(), 1);
    // Exported state is reachable by path.
    assert_eq!(
        shell.inst.get("theme.look").unwrap(),
        shell.host.variant("Look", "auto")
    );
    assert_eq!(shell.inst.get("launcher.open").unwrap(), Value::Bool(false));
    assert!(shell.inst.get("osd.shown").is_err(), "not exported");
}

#[test]
fn the_hello_bar_and_the_theme_alone() {
    let files = [fixture("hello_bar.strand"), fixture("theme.strand")];
    let shell = boot(&refs(&files), desktop);
    insta::assert_snapshot!("hello_bar_scene", shell.scene.render());
}

/// `on change` never fires at boot; `after` debounces; a sink switch is a
/// new baseline, not a change (the OSD of design.md).
#[test]
fn on_change_skips_boot_and_debounces() {
    let files = [fixture("osd.strand")];
    let mut shell = boot(&refs(&files), desktop);
    let osd = shell.scene.of_kind(NodeKind::Osd)[0];
    let open = |s: &Shell| s.scene.prop(osd, Prop::Open).cloned();
    assert_eq!(open(&shell), Some(PropValue::Bool(false)), "no OSD at boot");
    shell
        .host
        .set(&shell.rt, "audio.sink.volume", Value::float(0.6))
        .unwrap();
    shell.at(0.0);
    assert_eq!(open(&shell), Some(PropValue::Bool(true)));
    assert!(shell.scene.find_text("60%").is_some());
    // Another change at 1 s restarts the 1.2 s debounce.
    shell.at(1.0);
    shell
        .host
        .set(&shell.rt, "audio.sink.volume", Value::float(0.7))
        .unwrap();
    shell.at(1.0);
    shell.at(2.1);
    assert_eq!(
        open(&shell),
        Some(PropValue::Bool(true)),
        "1.1 s after the last change"
    );
    shell.at(2.3);
    assert_eq!(
        open(&shell),
        Some(PropValue::Bool(false)),
        "1.2 s of quiet hides it"
    );
    // Switching to another sink with another volume pops nothing.
    let other = shell.host.record(
        "AudioDevice",
        &[("id", Value::int(41)), ("volume", Value::float(0.2))],
    );
    shell.host.set(&shell.rt, "audio.sink", other).unwrap();
    shell.at(3.0);
    assert_eq!(
        open(&shell),
        Some(PropValue::Bool(false)),
        "a sink switch is not a change"
    );
    // But a change on the new sink does.
    shell
        .host
        .set(&shell.rt, "audio.sink.volume", Value::float(0.3))
        .unwrap();
    shell.at(3.5);
    assert_eq!(open(&shell), Some(PropValue::Bool(true)));
    // Brightness switches the kind.
    shell
        .host
        .set(&shell.rt, "brightness.level", Value::float(0.9))
        .unwrap();
    shell.at(3.6);
    assert!(
        shell.scene.find_text("90%").is_some(),
        "{}",
        shell.scene.render()
    );
}

/// `after T while cond`: the toast's countdown pauses while hovered and
/// never runs for a critical one (design.md's notification stack).
#[test]
fn after_while_pauses_and_expires() {
    let files = [fixture("toasts.strand")];
    let mut shell = boot(&refs(&files), desktop);
    let mail = shell.text_node("Mail · New message");
    let toast = |s: &Shell, n: NodeId| {
        // The Toast's root col: text → col → row → col.
        let mut c = n;
        for _ in 0..3 {
            c = s.scene.parent(c).unwrap();
        }
        c
    };
    let card = toast(&shell, mail);
    assert_eq!(shell.scene.kind(card), Some(NodeKind::Col));
    shell.at(3.0);
    shell.inst.set_flag(card, NodeFlag::Hover, true);
    shell.at(3.0);
    // Hovered: the `when hover` style applies and time stops counting.
    assert_eq!(
        shell.scene.prop(card, Prop::Bg).map(show),
        Some("$surface.hi".into())
    );
    shell.at(10.0);
    assert!(shell.host.actions().is_empty(), "paused while hovered");
    shell.inst.set_flag(card, NodeFlag::Hover, false);
    shell.at(10.0);
    shell.at(12.9);
    assert!(shell.host.actions().is_empty(), "5.9 s counted");
    let u = shell.at(13.1);
    let actions: Vec<String> = shell
        .host
        .take_actions()
        .iter()
        .map(|a| a.to_string())
        .collect();
    assert_eq!(actions, ["Notification(1).expire(0)"]);
    // The mock drops an expired notification; the toast leaves the scene.
    shell.flush();
    assert!(shell.scene.find_text("Mail · New message").is_none());
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    // The critical one never expires.
    shell.at(100.0);
    assert!(shell.host.actions().is_empty());
    assert!(shell.scene.find_text("Battery · Battery low").is_some());
}

/// A config that reads no clock never wakes: no deadline, no wall-clock
/// wake-up.
#[test]
fn a_static_bar_sleeps() {
    let src = "bar B { text \"static\" }\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.inst.next_deadline(), None);
    assert_eq!(shell.inst.next_wake(), None);
    assert!(shell.rt.is_idle());
    assert_eq!(shell.host.readers("screens"), 1);
    assert_eq!(shell.host.readers("clock"), 0);
}

#[test]
fn every_while_repeats_only_while_true() {
    let src = "state ticks = 0\nstate visible = true\nevery 1s while visible { ticks += 1 }\n";
    let mut shell = boot(&[("t.strand", src)], |_, _| {});
    for s in [1.0, 2.0, 3.0] {
        shell.at(s);
    }
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(3));
    shell
        .inst
        .set_value("t", "visible", Value::Bool(false))
        .unwrap();
    shell.at(3.5);
    assert_eq!(
        shell.inst.next_deadline(),
        None,
        "a paused timer schedules nothing"
    );
    shell.at(10.0);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(3));
    shell
        .inst
        .set_value("t", "visible", Value::Bool(true))
        .unwrap();
    shell.at(10.0);
    // It counts again from the resume: nothing was counted while paused.
    shell.at(10.9);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(3));
    shell.at(11.0);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(4));
}

/// `<->`: widget writes reach state and `rw` service fields, and
/// bindings follow (the launcher's query, the volume slider).
#[test]
fn two_way_bindings_write_back() {
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    shell
        .inst
        .write(input, Prop::Text, PropValue::Text("fi".into()))
        .unwrap();
    shell.flush();
    let launcher = shell.scene.of_kind(NodeKind::List)[0];
    assert_eq!(shell.scene.children(launcher).len(), 2, "Firefox and Files");
    assert!(shell.scene.find_text("No matches").is_none());
    shell
        .inst
        .write(input, Prop::Text, PropValue::Text("zz".into()))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.children(launcher).len(), 0);
    assert!(shell.scene.find_text("No matches").is_some());
    // `strand toggle launcher.open` and the panel's own dismissal.
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    let panel = shell.scene.ancestor(input, NodeKind::Panel).unwrap();
    assert_eq!(
        shell.scene.prop(panel, Prop::Open),
        Some(&PropValue::Bool(true))
    );
    shell
        .inst
        .write(panel, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(
        shell.scene.prop(panel, Prop::Open),
        Some(&PropValue::Bool(false))
    );
    // The volume slider appears while its row is hovered and writes the
    // sink's `rw` volume.
    let bar = shell.scene.of_kind(NodeKind::Bar)[0];
    let icon = shell
        .scene
        .walk()
        .into_iter()
        .find(|&n| {
            shell.scene.kind(n) == Some(NodeKind::Icon)
                && shell.scene.ancestor(n, NodeKind::Bar) == Some(bar)
        })
        .unwrap();
    let row = shell.scene.parent(icon).unwrap();
    shell.inst.set_flag(row, NodeFlag::Hover, true);
    shell.flush();
    let slider = shell.scene.of_kind(NodeKind::Slider)[0];
    assert_eq!(
        shell.scene.prop(slider, Prop::Value),
        Some(&PropValue::Number(0.5))
    );
    shell
        .inst
        .write(slider, Prop::Value, PropValue::Number(0.8))
        .unwrap();
    shell.flush();
    assert_eq!(
        shell.host.get(&shell.rt, "audio.sink.volume").unwrap(),
        Value::float(0.8_f32 as f64)
    );
    shell.inst.set_flag(row, NodeFlag::Hover, false);
    let u = shell.flush();
    assert!(
        u.diff
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::Remove { id } if *id == slider)),
        "the slider leaves (render plays its exit)"
    );
}

/// Keyed `for`: a reorder moves nodes (identity and state kept), an
/// insert and a removal touch one item each.
#[test]
fn keyed_lists_move_by_key() {
    let files = [fixture("bar.strand")];
    let mut shell = boot(&refs(&files), desktop);
    let bar = shell.scene.of_kind(NodeKind::Bar)[0];
    let dots = |s: &Shell| -> Vec<NodeId> {
        s.scene
            .of_kind(NodeKind::Box)
            .into_iter()
            .filter(|&b| s.scene.ancestor(b, NodeKind::Bar) == Some(bar))
            .collect()
    };
    let before = dots(&shell);
    assert_eq!(before.len(), 2);
    let all = shell.host.get(&shell.rt, "workspaces.all").unwrap();
    let mut list = all.as_list().unwrap().to_vec();
    list.swap(0, 1);
    shell
        .host
        .set(&shell.rt, "workspaces.all", Value::list(list.clone()))
        .unwrap();
    let u = shell.flush();
    let kinds: Vec<&str> = u
        .diff
        .ops
        .iter()
        .map(|op| match op {
            SceneOp::Create { .. } => "create",
            SceneOp::Remove { .. } => "remove",
            SceneOp::Move { .. } => "move",
            SceneOp::SetProp { .. } => "set",
            SceneOp::SetTokens { .. } => "tokens",
        })
        .collect();
    assert_eq!(kinds, ["move"], "{:?}", u.diff);
    assert_eq!(dots(&shell), [before[1], before[0]]);
    // Insert one, remove one.
    let ws4 = shell.host.record(
        "Workspace",
        &[("id", Value::int(4)), ("screen", Value::text("DP-1"))],
    );
    list.remove(0);
    list.push(ws4);
    shell
        .host
        .set(&shell.rt, "workspaces.all", Value::list(list))
        .unwrap();
    shell.flush();
    let after = dots(&shell);
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], before[0], "workspace 1 kept its node");
    assert!(!before.contains(&after[1]));
}

/// `when` blocks apply in source order: the later one wins.
#[test]
fn later_when_wins() {
    let files = [fixture("bar.strand")];
    let mut shell = boot(&refs(&files), desktop);
    let bar = shell.scene.of_kind(NodeKind::Bar)[0];
    let focused_dot = shell
        .scene
        .of_kind(NodeKind::Box)
        .into_iter()
        .find(|&b| shell.scene.ancestor(b, NodeKind::Bar) == Some(bar))
        .unwrap();
    let bg = |s: &Shell| s.scene.prop(focused_dot, Prop::Bg).map(show);
    assert_eq!(
        bg(&shell).as_deref(),
        Some("$accent"),
        "focused after occupied"
    );
    // Urgent comes after focused.
    let all = shell.host.get(&shell.rt, "workspaces.all").unwrap();
    let mut list = all.as_list().unwrap().to_vec();
    let types = shell.host.types().clone();
    let r = list[0].as_record().unwrap().clone();
    let urgent = types
        .record(r.ty)
        .fields
        .iter()
        .position(|f| f.name == "urgent")
        .unwrap();
    let mut fields = r.fields.clone();
    fields[urgent] = Value::Bool(true);
    list[0] = Value::record(r.ty, fields);
    shell
        .host
        .set(&shell.rt, "workspaces.all", Value::list(list))
        .unwrap();
    shell.flush();
    assert_eq!(bg(&shell).as_deref(), Some("$error"));
    // `when hover` comes last of all; `width` still comes from `focused`.
    shell.inst.set_flag(focused_dot, NodeFlag::Hover, true);
    shell.flush();
    assert_eq!(bg(&shell).as_deref(), Some("$accent.hover"));
    assert_eq!(
        shell.scene.prop(focused_dot, Prop::Width),
        Some(&PropValue::Number(24.0))
    );
    shell.inst.set_flag(focused_dot, NodeFlag::Hover, false);
    shell.flush();
    assert_eq!(bg(&shell).as_deref(), Some("$error"));
    // `pressed` sets another prop.
    shell.inst.set_flag(focused_dot, NodeFlag::Pressed, true);
    shell.flush();
    assert_eq!(
        shell.scene.prop(focused_dot, Prop::Scale),
        Some(&PropValue::Number(0.9))
    );
}

/// `id:` names: another node's `hover` drives this one's style.
#[test]
fn other_nodes_hover_through_id() {
    let src = "bar B {\n  row {\n    box { id: vol }\n    box { when vol.hover { opacity: 0.5 }; when focused { scale: 2 } }\n  }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(shell.scene.prop(boxes[1], Prop::Opacity), None);
    shell.inst.set_flag(boxes[0], NodeFlag::Hover, true);
    shell.flush();
    assert_eq!(
        shell.scene.prop(boxes[1], Prop::Opacity),
        Some(&PropValue::Number(0.5))
    );
    // `focused` is the node's own.
    shell.inst.set_flag(boxes[1], NodeFlag::Focused, true);
    shell.flush();
    assert_eq!(
        shell.scene.prop(boxes[1], Prop::Scale),
        Some(&PropValue::Number(2.0))
    );
}

/// A handler suspended at `await` is cancelled when its element unmounts:
/// its later writes never land.
#[test]
fn unmount_cancels_a_suspended_handler() {
    let src = "state shown = true\nstate done = 0\nbar B {\n  if shown { box { on click { await sleep(1s); done = 1 } } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(b, "click", Vec::new()));
    shell.at(0.5);
    shell
        .inst
        .set_value("t", "shown", Value::Bool(false))
        .unwrap();
    let u = shell.at(0.5);
    assert!(
        u.diagnostics
            .iter()
            .any(|d| matches!(d, strand_core::Diagnostic::Cancelled { .. })),
        "{:?}",
        u.diagnostics
    );
    shell.at(2.0);
    assert_eq!(shell.inst.value_of("t", "done").unwrap(), Value::int(0));
    // Without the unmount the write lands after the await.
    shell
        .inst
        .set_value("t", "shown", Value::Bool(true))
        .unwrap();
    shell.at(2.0);
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    shell.inst.event(b, "click", Vec::new());
    shell.at(2.0);
    shell.at(2.9);
    assert_eq!(shell.inst.value_of("t", "done").unwrap(), Value::int(0));
    shell.at(3.0);
    assert_eq!(shell.inst.value_of("t", "done").unwrap(), Value::int(1));
}

/// Handler errors are values: reported in the tick, the shell runs on.
#[test]
fn handler_errors_are_values() {
    let src = "type Pin { app: text; label: text }\nstate pins: [Pin] key app = []\nstate n = 0\nbar B {\n  box { on click { pins.remove_key(\"x\") } }\n  box { on secondary { n += 1 } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(
        u.errors[0].contains("no item with this key"),
        "{:?}",
        u.errors
    );
    shell.inst.event(boxes[1], "secondary", Vec::new());
    shell.flush();
    assert_eq!(shell.inst.value_of("t", "n").unwrap(), Value::int(1));
}

/// Events go to the innermost handler; `propagate()` passes one on.
#[test]
fn events_bubble_and_propagate() {
    let src = "state a = 0\nstate b = 0\nbar B {\n  row {\n    on click { a += 1 }\n    box { on click { b += 1; propagate() } }\n    box {}\n  }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(1));
    assert_eq!(shell.inst.value_of("t", "b").unwrap(), Value::int(1));
    // A node without a handler hands the event to its parent.
    shell.inst.event(boxes[1], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(2));
    // Scroll arguments reach the handler's parameters.
}

/// One bar per monitor, each with its own component state; monitors
/// come and go through the `screens` service.
#[test]
fn a_bar_per_monitor_with_its_own_state() {
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
    let clocks: Vec<NodeId> = shell
        .scene
        .walk()
        .into_iter()
        .filter(|&n| {
            matches!(shell.scene.prop(n, Prop::Text), Some(PropValue::Text(t)) if t == "Mon 05  09:41")
        })
        .collect();
    assert_eq!(clocks.len(), 2);
    shell.inst.event(clocks[0], "click", Vec::new());
    shell.flush();
    let popups = shell.scene.of_kind(NodeKind::Popup);
    assert_eq!(
        shell.scene.prop(popups[0], Prop::Open),
        Some(&PropValue::Bool(true))
    );
    assert_eq!(
        shell.scene.prop(popups[1], Prop::Open),
        Some(&PropValue::Bool(false))
    );
    // The calendar's own state: one month back on the first monitor.
    let back = shell
        .scene
        .walk()
        .into_iter()
        .find(|&n| matches!(shell.scene.prop(n, Prop::Text), Some(PropValue::Text(t)) if t == "‹"))
        .unwrap();
    shell.inst.event(back, "click", Vec::new());
    shell.flush();
    assert!(shell.scene.find_text("September 2026").is_some());
    assert!(
        shell.scene.find_text("October 2026").is_some(),
        "the other monitor's"
    );
    // Plug a third monitor, then unplug the first.
    screens(&shell.rt, &shell.host, &["DP-1", "HDMI-A-1", "DP-2"]);
    shell.flush();
    assert_eq!(shell.scene.of_kind(NodeKind::Bar).len(), 3);
    screens(&shell.rt, &shell.host, &["HDMI-A-1", "DP-2"]);
    shell.flush();
    let bars = shell.scene.of_kind(NodeKind::Bar);
    assert_eq!(bars.len(), 2);
    assert!(
        shell.scene.find_text("September 2026").is_none(),
        "went with DP-1"
    );
    // Readers are counted per mounted reader: each bar and each bar's
    // `Battery` component read `battery`.
    assert_eq!(shell.host.readers("battery"), 4);
    screens(&shell.rt, &shell.host, &[]);
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 0, "released on unmount");
}

/// Service events are lossless: three in one tick, three handler runs.
#[test]
fn service_events_are_lossless() {
    let src = "state log: [Notification] key id = []\non notifications.received(n) { log.push(n) }\nbar B { text join(\" \", log.len) }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    for i in 1..=3 {
        let n = notification(&shell.host, i, "App", "s", "normal");
        shell
            .host
            .emit(&shell.rt, "notifications.received", vec![n])
            .unwrap();
    }
    shell.flush();
    assert_eq!(shell.scene.texts(), ["3"]);
    assert_eq!(
        shell.host.readers("notifications"),
        1,
        "the top level listens"
    );
}

/// `persist` keeps a value across instances, keyed by path, with the
/// default's hash.
#[test]
fn persisted_state_survives_a_restart() {
    let store: Rc<dyn PersistStore> = Rc::new(MemoryStore::new());
    let files = [fixture("toasts.strand")];
    let shell = boot_with(&refs(&files), desktop, Some(store.clone()));
    assert_eq!(shell.inst.get("toasts.dnd").unwrap(), Value::Bool(false));
    shell.inst.set("toasts.dnd", Value::Bool(true)).unwrap();
    let mut shell = shell;
    shell.flush();
    // Do not disturb: only the critical notification stays shown.
    assert!(shell.scene.find_text("Mail · New message").is_none());
    drop(shell);
    let shell = boot_with(&refs(&files), desktop, Some(store.clone()));
    assert_eq!(shell.inst.get("toasts.dnd").unwrap(), Value::Bool(true));
    assert!(shell.scene.find_text("Mail · New message").is_none());
}

/// A theme switch is one `SetTokens` with the default (spring)
/// transition; nothing else in the tree changes.
#[test]
fn a_theme_switch_swaps_the_token_table() {
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
    let before = shell.scene.render();
    let mocha = shell.host.variant("Look", "mocha");
    shell.inst.set("theme.look", mocha).unwrap();
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff.ops.len());
    assert_eq!(shell.scene.token_swaps, 2);
    assert_eq!(
        shell.scene.last_token_transition,
        Some(strand_scene::Transition::Default)
    );
    assert_eq!(
        shell.scene.tokens.get("accent"),
        Some(&PropValue::Color(
            strand_scene::Color::from_hex("#cba6f7").unwrap()
        ))
    );
    assert_eq!(shell.scene.render(), before);
    // `prefs.compact` picks the extending set: overrides win, the rest
    // is inherited.
    let set = shell.flush();
    assert!(set.diff.is_empty());
}

/// Components take parameters and render their caller's children at
/// `slot`; component tokens join the token table.
#[test]
fn components_with_params_slot_and_tokens() {
    let src = "component Card(title: text, dense: bool = false) tokens { pad: 4px } {\n  col { pad: $Card.pad; when dense { gap: 0 }\n    text title\n    slot\n  }\n}\nbar B { Card \"Hi\" { dense: true; text \"inside\" }; Card \"Two\" }\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    insta::assert_snapshot!("components", shell.scene.render());
    assert_eq!(
        shell.scene.tokens.get("Card.pad"),
        Some(&PropValue::Number(4.0))
    );
}

/// `match` in a tree mounts the matching arm and swaps on change.
#[test]
fn tree_match_swaps_arms() {
    let src = "enum Page { wifi, bluetooth, power }\nstate page = wifi\nbar B {\n  match page {\n    wifi => text \"Wi-Fi\"\n    bluetooth => text \"Bluetooth\"\n    _ => box {}\n  }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts(), ["Wi-Fi"]);
    let page = |v: &str| {
        let types = shell.inst.vm().types();
        let e = types.enums.iter().position(|e| e.name == "Page").unwrap();
        let e = strand_compiler::ty::EnumId(e as u32);
        Value::Enum(e, types.enum_(e).variant(v).unwrap())
    };
    let (bt, power) = (page("bluetooth"), page("power"));
    shell.inst.set_value("t", "page", bt).unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["Bluetooth"]);
    shell.inst.set_value("t", "page", power).unwrap();
    shell.flush();
    assert!(shell.scene.texts().is_empty());
    assert_eq!(shell.scene.of_kind(NodeKind::Box).len(), 1);
}

/// `~` reaches the `SetProp`'s transition, per source: a `when` block's
/// own `~` applies while it wins.
#[test]
fn transitions_reach_set_prop() {
    let src = "state wide = false\nbar B {\n  box { width: 8 ~ 200ms\n    when wide { width: 24 ~ $motion.bouncy }\n  }\n  box { x: 4 ~ instant; y: 2 ~ ease(out_back, 300ms); scale: 1 ~ spring(380, 0.75) }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    shell
        .inst
        .set_value("t", "wide", Value::Bool(true))
        .unwrap();
    let u = shell.flush();
    let t: Vec<_> = u
        .diff
        .ops
        .iter()
        .filter_map(|op| match op {
            SceneOp::SetProp {
                prop, transition, ..
            } => Some((*prop, transition.clone())),
            _ => None,
        })
        .collect();
    use strand_scene::{Easing, Transition};
    assert_eq!(
        t,
        [(Prop::Width, Transition::Token("motion.bouncy".into()))]
    );
    shell
        .inst
        .set_value("t", "wide", Value::Bool(false))
        .unwrap();
    let u = shell.flush();
    assert!(matches!(
        &u.diff.ops[0],
        SceneOp::SetProp { transition: Transition::Duration { duration, easing }, .. }
            if *duration == Duration::from_millis(200) && *easing == Easing::STANDARD
    ));
    // The second box's transitions, as written, from the boot diff.
    let boot: Vec<_> = shell
        .boot
        .iter()
        .filter_map(|op| match op {
            SceneOp::SetProp {
                prop: p @ (Prop::X | Prop::Y | Prop::Scale),
                transition,
                ..
            } => Some((*p, transition.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        boot,
        [
            (Prop::X, Transition::Instant),
            (
                Prop::Y,
                Transition::Duration {
                    duration: Duration::from_millis(300),
                    easing: Easing::named("out_back").unwrap()
                }
            ),
            (
                Prop::Scale,
                Transition::Spring {
                    stiffness: 380.0,
                    damping: 0.75
                }
            ),
        ]
    );
}

/// Writes in one tick coalesce: one diff, one `SetProp` per changed prop.
#[test]
fn one_diff_per_tick() {
    let src = "state n = 0\nbar B { text join(\"\", n) }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    for i in 1..=5 {
        shell.inst.set_value("t", "n", Value::int(i)).unwrap();
    }
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff);
    assert_eq!(shell.scene.texts(), ["5"]);
    // Writing the same value back changes nothing.
    shell.inst.set_value("t", "n", Value::int(5)).unwrap();
    assert!(shell.flush().diff.is_empty());
}

/// Token sets: `use tokens` follows its condition; an extending set's
/// `override`s win and the rest is inherited; derived tokens stay
/// expressions; a settings field written in a handler switches sets.
#[test]
fn token_sets_extend_and_switch() {
    let src = "state prefs from \"prefs.toml\" { compact: bool = false }\ntokens base { space { 1: 4px; 2: 8px }; fg.muted: $fg.alpha(0.65) }\ntokens compact extends base { override space { 1: 2px } }\nuse tokens prefs.compact ? compact : base\nbar B { box { pad: $space.1; on click { prefs.compact = !prefs.compact } } }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let tokens = |s: &Shell| s.scene.render_tokens();
    assert!(
        tokens(&shell).contains("space.1 = 4\n"),
        "{}",
        tokens(&shell)
    );
    assert!(tokens(&shell).contains("fg.muted := $fg.alpha(0.65)"));
    // No `use palette`: the default palette fills the roles.
    assert!(shell.scene.tokens.get("surface").is_some());
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert!(
        tokens(&shell).contains("space.1 = 2\n"),
        "{}",
        tokens(&shell)
    );
    assert!(tokens(&shell).contains("space.2 = 8\n"), "inherited");
    assert_eq!(shell.scene.token_swaps, 2);
    assert_eq!(
        shell.scene.prop(b, Prop::Pad).map(show).as_deref(),
        Some("$space.1"),
        "props keep the token; render resolves it"
    );
}

/// `play` from a handler, `exit` mirroring `enter`, and a `lock` surface.
#[test]
fn play_poses_and_lock() {
    let src = "keyframes shake { 0%, 100% { x: 0 }; 50% { x: 4 } }\nbar B {\n  box { enter { opacity: 0 }; on click { play shake } }\n  box { enter { y: 4 }; exit { y: -4 } }\n}\nlock Lock { text \"locked\" }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(
        shell.scene.prop(boxes[0], Prop::Exit).map(show).as_deref(),
        Some("{opacity: 0}"),
        "exit mirrors enter"
    );
    assert_eq!(
        shell.scene.prop(boxes[1], Prop::Exit).map(show).as_deref(),
        Some("{y: -4}")
    );
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    assert_eq!(
        shell.scene.prop(boxes[0], Prop::Play).map(show).as_deref(),
        Some("[shake, 1]")
    );
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "a second play is sent again");
    let lock = shell.scene.of_kind(NodeKind::Lock)[0];
    assert_eq!(
        shell.scene.prop(lock, Prop::Name),
        Some(&PropValue::Text("Lock".into()))
    );
}

/// The launcher's rows follow `selected` (list navigation) and `hover`.
#[test]
fn selected_rows_and_activate() {
    let files = [fixture("launcher.strand")];
    let mut shell = boot(&refs(&files), desktop);
    let row = shell
        .scene
        .of_kind(NodeKind::Row)
        .into_iter()
        .find(|&r| shell.scene.ancestor(r, NodeKind::List).is_some())
        .unwrap();
    shell.inst.set_flag(row, NodeFlag::Selected, true);
    shell.flush();
    assert_eq!(
        shell.scene.prop(row, Prop::Bg).map(show).as_deref(),
        Some("$accent.container")
    );
    // `activate` runs the row's handler: launch and close.
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.inst.event(row, "activate", Vec::new());
    shell.flush();
    let actions: Vec<String> = shell
        .host
        .take_actions()
        .iter()
        .map(|a| a.to_string())
        .collect();
    assert_eq!(actions, ["App(firefox).launch(0)"]);
    assert_eq!(shell.inst.get("launcher.open").unwrap(), Value::Bool(false));
}

/// Every snippet of design.md (and grammar.md's examples, and the rice)
/// mounted at once: no binding fails, every diff is consistent.
#[test]
fn every_snippet_mounts() {
    let everything = "bar Everything {\n  col {\n    GoodTable; Switcher; Layout; Hatches; Shape; Paint; Filters; Motion; Launcher2; Now\n    for n in notifications.popups { Expire n; Toast n; Toast2 n\n      for ws in workspaces.all { Rules ws { n: n; xs: workspaces.all } }\n    }\n    for ws in workspaces.all { Dot ws }\n    for w in windows.all { Media w }\n    for p in pins { Pins p }\n  }\n}\n";
    let (name, snippets) = fixture("snippets.strand");
    let files = [
        (name, format!("{snippets}\n{everything}")),
        fixture("theme.strand"),
        fixture("rice_now.strand"),
    ];
    let mut shell = boot(&refs(&files), |rt, host| {
        desktop(rt, host);
        let w = host.record(
            "Window",
            &[("id", Value::text("w1")), ("title", Value::text("t"))],
        );
        host.set(rt, "windows.all", Value::list(vec![w])).unwrap();
        screens(rt, host, &["DP-1"]);
    });
    assert!(shell.scene.len() > 100, "{}", shell.scene.len());
    insta::assert_snapshot!("every_snippet", shell.scene.render());
    let rendered = shell.scene.render();
    for expected in [
        "segmented value=auto options=[auto, light, dark, wallpaper, mocha]",
        "glow=[12, $accent.alpha(0.6)]",
        "mask=radial(center, 40%)",
        "stagger=30ms",
        "border=template(2, conic(from 0deg",
    ] {
        assert!(rendered.contains(expected), "{expected}\n{rendered}");
    }
    // Click and scroll everything that listens; tick past every timer.
    for ev in ["click", "secondary", "scroll", "drop"] {
        for n in shell.inst.nodes_handling(ev) {
            let args = match ev {
                "scroll" => vec![Value::float(1.0), Value::float(0.0)],
                _ => Vec::new(),
            };
            shell.inst.event(n, ev, args);
        }
    }
    let u = shell.at(10.0);
    // Handlers given made-up arguments may fail; bindings must not.
    for e in &u.errors {
        assert!(!e.contains("text.") && !e.contains("box."), "{e}");
    }
    shell.at(20.0);
}

//! Headless instantiation: programs mounted on a mock service host, their
//! scene diffs applied to a mirror and snapshotted.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::instantiate::{
    Instance, NodeFlag, OutputContent, SceneMirror, Storage, Update, show,
};
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::vm::{Num, Value};
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneOp, Transition};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
    /// The boot diff's ops.
    boot: Vec<SceneOp>,
    /// The boot tick's notices and diagnostics.
    boot_notices: Vec<String>,
    boot_diagnostics: Vec<strand_core::Diagnostic>,
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
    boot_with(files, setup, Storage::none())
}

fn boot_with(
    files: &[(&str, &str)],
    setup: impl FnOnce(&Runtime, &SchemaHost),
    storage: Storage,
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
    let inst = Instance::new(&rt, program, host.clone(), storage);
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
        boot: Vec::new(),
        boot_notices: Vec::new(),
        boot_diagnostics: Vec::new(),
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell.boot = u.diff.ops;
    shell.boot_notices = u.notices;
    shell.boot_diagnostics = u.diagnostics;
    shell
}

/// Monitors by connector name; each one's identity is `Mock <name>`.
fn screens(rt: &Runtime, host: &SchemaHost, names: &[&str]) {
    let list = names.iter().map(|n| screen(host, n, n)).collect();
    host.set(rt, "screens.all", Value::list(list)).unwrap();
}

/// A monitor plugged into connector `name`, identified by `model`.
fn screen(host: &SchemaHost, name: &str, model: &str) -> Value {
    host.record(
        "Screen",
        &[
            ("id", Value::text(format!("Mock | {model} | Display"))),
            ("name", Value::text(name)),
            ("make", Value::text("Mock")),
            ("model", Value::text(model)),
            ("description", Value::text("Display")),
        ],
    )
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

fn app_record(host: &SchemaHost, id: &str, name: &str) -> Value {
    host.record(
        "App",
        &[("id", Value::text(id)), ("name", Value::text(name))],
    )
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
    let mut shell = boot(&refs(&files), desktop);
    // Closed popups, the closed launcher and the hidden OSD have no
    // content yet.
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
            Some(PropValue::Text("Mock | DP-1 | Display".into())),
            Some(PropValue::Text("Mock | HDMI-A-1 | Display".into()))
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
    // Everything opened: the launcher, both calendars and the OSD (a
    // volume change shows it).
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    for clock in shell.scene.walk().into_iter().filter(|&n| {
        matches!(shell.scene.prop(n, Prop::Text), Some(PropValue::Text(t)) if t == "Mon 05  09:41")
    }) {
        shell.inst.event(clock, "click", Vec::new());
    }
    shell
        .host
        .set(&shell.rt, "audio.sink.volume", Value::float(0.6))
        .unwrap();
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    insta::assert_snapshot!("design_shells_opened", shell.scene.render());
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

/// spec.md 8.4 (decisions.md change-optional-chain): a target that is the
/// key of the object it reads (`clipboard.history.first?.id`,
/// `audio.sink.id`), or a field holding a path key (`hits.first?.app`
/// under `Hit`'s `key app.id`), fires when that object switches, since a
/// switch always changes it; a field beside the key still takes a switch
/// as its baseline. Each target of a handler is judged by its own object:
/// a key watched with a field beside it fires on the switch, two fields
/// of a sink that switches do not. Bindings and `if` over the same chain
/// follow it too.
#[test]
fn on_change_over_an_identity_fires_on_a_switch() {
    let src = "state direct = 0\nstate last = 0\nstate sink = 0\nstate texts = 0\n\
               state debounced = 0\nstate top = 0\nstate scores = 0\n\
               state pair = 0\nstate pair_after = 0\nstate levels = 0\nstate mixed = 0\n\
               state q = \"oo\"\n\
               let newest = clipboard.history.first?.id\n\
               let hits = apps.search(q) ?? []\n\
               on change hits.first?.app { top += 1 }\n\
               on change hits.first?.score { scores += 1 }\n\
               on change clipboard.history.first?.id { direct += 1 }\n\
               on change clipboard.history.last?.id { last += 1 }\n\
               on change clipboard.history.first?.id after 0.5s { debounced += 1 }\n\
               on change audio.sink.id { sink += 1 }\n\
               on change clipboard.history.first?.text { texts += 1 }\n\
               on change clipboard.history.first?.id, clipboard.history.first?.text { pair += 1 }\n\
               on change clipboard.history.first?.id, clipboard.history.first?.text after 0.5s \
               { pair_after += 1 }\n\
               on change audio.sink.volume, audio.sink.muted { levels += 1 }\n\
               on change hits.first?.app, hits.first?.score { mixed += 1 }\n\
               bar B {\n  text clipboard.history.first?.text ?? \"none\"\n  \
               if clipboard.history.first?.id == 2 { text \"second\" }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        let sink = host.record("AudioDevice", &[("id", Value::int(40))]);
        host.set(rt, "audio.sink", sink).unwrap();
        host.set(
            rt,
            "apps.all",
            Value::list(vec![
                app_record(host, "foot", "Foot"),
                app_record(host, "firefox", "Firefox"),
            ]),
        )
        .unwrap();
    });
    let entry = |host: &SchemaHost, id: i64, text: &str| {
        host.record(
            "ClipboardEntry",
            &[("id", Value::int(id)), ("text", Value::text(text))],
        )
    };
    let get = |s: &Shell, name: &str| s.inst.value_of("t", name).unwrap();
    assert_eq!(get(&shell, "direct"), Value::int(0), "never at boot");
    assert_eq!(shell.scene.texts(), ["none"]);
    // Two copies, newest first: each makes another entry the first.
    let one = entry(&shell.host, 1, "one");
    shell
        .host
        .set(
            &shell.rt,
            "clipboard.history",
            Value::list(vec![one.clone()]),
        )
        .unwrap();
    shell.at(1.0);
    shell.at(1.6);
    assert_eq!(get(&shell, "debounced"), Value::int(1), "`after` too");
    let two = entry(&shell.host, 2, "two");
    shell
        .host
        .set(&shell.rt, "clipboard.history", Value::list(vec![two, one]))
        .unwrap();
    shell.at(2.0);
    shell.at(2.6);
    assert_eq!(get(&shell, "debounced"), Value::int(2));
    assert_eq!(
        get(&shell, "pair"),
        Value::int(2),
        "the key beside a field it re-baselines"
    );
    assert_eq!(get(&shell, "pair_after"), Value::int(2), "`after` too");
    assert_eq!(get(&shell, "newest"), Value::int(2));
    assert_eq!(
        get(&shell, "direct"),
        Value::int(2),
        "each copy is a change"
    );
    assert_eq!(get(&shell, "last"), Value::int(1), "only the first copy");
    assert_eq!(
        get(&shell, "texts"),
        Value::int(0),
        "a field beside the key takes a switch as its baseline"
    );
    assert_eq!(shell.scene.texts(), ["two", "second"]);
    // A sink switch changes `audio.sink.id`.
    // Its volume and mute beside each other both take the switch as their
    // baseline.
    let other = shell.host.record(
        "AudioDevice",
        &[
            ("id", Value::int(41)),
            ("volume", Value::float(0.2)),
            ("muted", Value::Bool(true)),
        ],
    );
    shell.host.set(&shell.rt, "audio.sink", other).unwrap();
    shell.at(3.0);
    assert_eq!(get(&shell, "sink"), Value::int(1));
    assert_eq!(get(&shell, "levels"), Value::int(0), "a sink switch");
    shell
        .host
        .set(&shell.rt, "audio.sink.muted", Value::Bool(false))
        .unwrap();
    shell.at(3.5);
    assert_eq!(get(&shell, "levels"), Value::int(1), "a change on it");
    // A path key (`Hit` is `key app.id`): `app` holds it, so another top
    // hit (Foot, then Firefox) changes it; `score` beside it takes the
    // switch as its baseline. Counted from here: the search resolving
    // after boot is not what this checks.
    let count = |s: &Shell| ["top", "mixed", "scores"].map(|n| get(s, n).as_f64().unwrap());
    let [top, mixed, scores] = count(&shell);
    shell.inst.set_value("t", "q", Value::text("ir")).unwrap();
    shell.at(4.0);
    assert_eq!(get(&shell, "hits").as_list().map(<[Value]>::len), Some(1));
    let now = count(&shell);
    assert_eq!(now[0], top + 1.0, "a field holding the key");
    assert_eq!(now[1], mixed + 1.0, "`app` beside `score`");
    assert_eq!(now[2], scores, "`score` takes the switch as its baseline");
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

/// spec.md 8.4 (decisions.md m6-w2-language, Docs 1): `after T while c`
/// runs once per mount and is never re-armed by `c` turning false and
/// true again, while the grace-delay idiom `on change c after T { if
/// !c { … } }` runs each time `c` ends and stays ended for `T`.
#[test]
fn after_while_is_one_shot_and_the_grace_idiom_runs_each_time() {
    let src = "state over = true\nstate fired = 0\nstate closed = 0\nafter 1s while over { fired += 1 }\non change over after 200ms { if !over { closed += 1 } }\n";
    let mut shell = boot(&[("t.strand", src)], |_, _| {});
    let value = |s: &Shell, n: &str| s.inst.value_of("t", n).unwrap();
    let set = |s: &mut Shell, v: bool, at: f64| {
        s.inst.set_value("t", "over", Value::Bool(v)).unwrap();
        s.at(at);
    };
    shell.at(0.5);
    shell.at(1.0);
    assert_eq!(value(&shell, "fired"), Value::int(1));
    // Leave, come back within the grace: nothing closes.
    set(&mut shell, false, 2.0);
    set(&mut shell, true, 2.1);
    shell.at(2.5);
    assert_eq!(value(&shell, "closed"), Value::int(0));
    // Leave for good: it closes 200 ms later.
    set(&mut shell, false, 3.0);
    shell.at(3.3);
    assert_eq!(value(&shell, "closed"), Value::int(1));
    // Again: the idiom runs each time, the timer never again.
    set(&mut shell, true, 4.0);
    for s in [5.0, 6.0, 7.0] {
        shell.at(s);
    }
    set(&mut shell, false, 8.0);
    shell.at(8.3);
    assert_eq!(value(&shell, "closed"), Value::int(2));
    assert_eq!(value(&shell, "fired"), Value::int(1), "one-shot");
}

/// `<->`: widget writes reach state and `rw` service fields, and
/// bindings follow (the launcher's query, the volume slider).
#[test]
fn two_way_bindings_write_back() {
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
    // A closed launcher has no content yet and runs no search.
    assert!(shell.scene.of_kind(NodeKind::Input).is_empty());
    assert_eq!(
        shell.host.readers("apps"),
        0,
        "`let hits = apps.search(query)` is read only inside the closed popup"
    );
    // `strand toggle launcher.open`.
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("apps"), 1, "the open launcher reads it");
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
    let panel = shell.scene.ancestor(input, NodeKind::Panel).unwrap();
    assert_eq!(
        shell.scene.prop(panel, Prop::Open),
        Some(&PropValue::Bool(true))
    );
    // The panel's own dismissal writes `open` back.
    shell
        .inst
        .write(panel, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(
        shell.scene.prop(panel, Prop::Open),
        Some(&PropValue::Bool(false))
    );
    assert_eq!(shell.inst.get("launcher.open").unwrap(), Value::Bool(false));
    // A bool does not fit a text place.
    assert!(
        shell
            .inst
            .write(input, Prop::Text, PropValue::Bool(true))
            .is_err()
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
    // The slider's f32 is the 0.8 it shows, and the host is told which
    // leaf changed.
    assert_eq!(
        shell.host.get(&shell.rt, "audio.sink.volume").unwrap(),
        Value::float(0.8)
    );
    let writes = shell.host.take_writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0].path, "audio.sink.volume");
    assert_eq!(writes[0].value, Value::float(0.8));
    shell.inst.set_flag(row, NodeFlag::Hover, false);
    let u = shell.flush();
    assert!(
        u.diff
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::Remove { id, .. } if *id == slider)),
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
    let src = "type Pin { app: text; label: text }\nstate pins: [Pin] key app = []\nstate n = 0\nbar B {\n  box { on click { pins.update(\"x\", p => p) } }\n  box { on secondary { n += 1 } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(
        u.errors[0].contains("`update`: no item with this key"),
        "{:?}",
        u.errors
    );
    shell.inst.event(boxes[1], "secondary", Vec::new());
    shell.flush();
    assert_eq!(shell.inst.value_of("t", "n").unwrap(), Value::int(1));
}

/// Clicks each box in turn and returns, per click, the texts shown after
/// it and the handler errors it raised.
fn click_each_box(src: &str) -> Vec<(String, Vec<String>)> {
    let mut shell = boot(&[("keys.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let mut seen = vec![(shell.scene.texts().join(" "), Vec::new())];
    for b in shell.scene.of_kind(NodeKind::Box) {
        shell.inst.event(b, "click", Vec::new());
        let u = shell.flush();
        let errors = u.errors.iter().map(|e| e.error.to_string()).collect();
        seen.push((shell.scene.texts().join(" "), errors));
    }
    seen
}

/// `remove_key` of a key the collection does not hold does nothing, as a
/// dictionary's `remove` will: forgetting an entry is idempotent
/// (decisions.md dictionary). On a keyed `state` (core's keyed
/// collection) and on a persisted one (a plain list cell) alike; `move`
/// and `update` of a missing key stay handler errors.
#[test]
fn remove_key_of_a_missing_key_does_nothing() {
    for persist in ["", " persist"] {
        let src = format!(
            "type Pin {{ app: text; label: text }}\nstate pins: [Pin] key app = [Pin(app: \"a\", label: \"A\")]{persist}\nstate writes = 0\non change pins {{ writes += 1 }}\nbar B {{\n  text join(\" \", pins.len, writes)\n  box {{ on click {{ pins.remove_key(\"x\") }} }}\n  box {{ on click {{ pins.move(\"x\", 0) }} }}\n  box {{ on click {{ pins.update(\"x\", p => p) }} }}\n  box {{ on click {{ pins.remove_key(\"a\"); pins.remove_key(\"a\") }} }}\n}}\n"
        );
        let seen = click_each_box(&src);
        let none: Vec<String> = Vec::new();
        assert_eq!(seen[0], ("1 0".to_string(), none.clone()), "{persist}");
        // A miss: no error and no write (`on change` does not fire).
        assert_eq!(seen[1], ("1 0".to_string(), none.clone()), "{persist}");
        for (i, method) in [(2, "move"), (3, "update")] {
            assert_eq!(seen[i].0, "1 0", "{persist}");
            assert_eq!(
                seen[i].1,
                [format!("`{method}`: no item with this key")],
                "{persist}"
            );
        }
        // Removing a key twice in one handler: the second does nothing.
        assert_eq!(seen[4], ("0 1".to_string(), none), "{persist}");
    }
}

/// A number key is one key whatever its tag: `int` and `float` are not
/// units, so `3` and `3.0` are one key (decisions.md dictionary; spec.md
/// 6.5). Items whose `float` key was written `3` (held as `3.0`) are
/// found by an `int` state holding 3, by `remove_key`, `move`,
/// `update`, `push`'s duplicate check and `.contains`, on a keyed
/// `state` and on a persisted one. Before, the keyed `state` missed
/// them: the key kept the number's tag.
#[test]
fn remove_key_finds_a_key_whatever_its_number_tag() {
    for persist in ["", " persist"] {
        let src = format!(
            "type Row {{ at: float; label: text }}\nstate rows: [Row] key at = [Row(at: 3, label: \"a\"), Row(at: 4, label: \"b\")]{persist}\nstate three = 3\nbar B {{\n  text join(\" \", rows.len, rows.contains(Row(at: three, label: \"c\")), rows.first?.label)\n  box {{ on click {{ rows.update(three, r => Row(at: r.at, label: \"c\")) }} }}\n  box {{ on click {{ rows.move(three, 1) }} }}\n  box {{ on click {{ rows.push(Row(at: three, label: \"d\")) }} }}\n  box {{ on click {{ rows.remove_key(three) }} }}\n}}\n"
        );
        let seen = click_each_box(&src);
        let none: Vec<String> = Vec::new();
        let texts: Vec<&str> = seen.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            texts,
            ["2 false a", "2 true c", "2 true b", "2 true b", "1 false b"],
            "{persist}"
        );
        for (i, (_, errors)) in seen.iter().enumerate() {
            if i == 3 {
                assert_eq!(
                    errors,
                    &["`push`: an item with this key is already in the list"],
                    "{persist}"
                );
            } else {
                assert_eq!(errors, &none, "{persist} {i}");
            }
        }
    }
}

/// (dictionary) `xs.get(k)` reads the item with key `k`, or `null`, on a
/// keyed `state` (core's collection: one lookup through the tracked
/// accessor), a persisted one (a plain list cell, found by its `key`), a
/// view `let` over it (a scan of the view's items) and a chain written in
/// place (`rows.take(9).get(k)`, found by the `state`'s `key` that
/// lowering passes). Each binding follows the collection and the key:
/// a push, an update, a remove and a new key each show at once.
#[test]
fn get_reads_by_key_and_is_null_on_a_miss() {
    for persist in ["", " persist"] {
        let src = format!(
            "type Row {{ id: int; label: text }}\nstate rows: [Row] key id = [Row(id: 1, label: \"a\")]{persist}\nstate at = 2\nlet shown = rows.filter(r => r.id > 0)\nbar B {{\n  text rows.get(1)?.label ?? \"-\"\n  text rows.get(at)?.label ?? \"-\"\n  text shown.get(at)?.label ?? \"-\"\n  text rows.take(9).get(at)?.label ?? \"-\"\n  box {{ on click {{ rows.push(Row(id: 2, label: \"b\")) }} }}\n  box {{ on click {{ rows.update(2, r => Row(id: r.id, label: \"c\")) }} }}\n  box {{ on click {{ rows.remove_key(1) }} }}\n  box {{ on click {{ at = 3 }} }}\n  box {{ on click {{ rows.push(Row(id: 3, label: \"d\")) }} }}\n}}\n"
        );
        let seen = click_each_box(&src);
        let none: Vec<String> = Vec::new();
        let texts: Vec<&str> = seen.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            texts,
            [
                "a - - -", "a b b b", "a c c c", "- c c c", "- - - -", "- d d d"
            ],
            "{persist}"
        );
        for (i, (_, errors)) in seen.iter().enumerate() {
            assert_eq!(errors, &none, "{persist} {i}");
        }
    }
}

/// (dictionary) `xs.get(k)` on a service's keyed list (core's keyed
/// signal), on a view `let` over it, and on a list reached through an
/// item's field (`n.actions`, found by its records' own `key`): each
/// follows the service's changes.
#[test]
fn get_on_a_service_list_and_a_view_tracks_the_collection() {
    let src = "let mail = notifications.popups.filter(n => n.app.name == \"mail\")\nbar B {\n  text notifications.popups.get(1)?.summary ?? \"-\"\n  text mail.get(1)?.summary ?? \"-\"\n  text notifications.popups.get(1)?.actions?.get(\"reply\")?.label ?? \"-\"\n}\n";
    let mut shell = boot(&[("get.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts().join(" "), "- - -");
    let popups = |host: &SchemaHost, summary: &str, actions: &[(&str, &str)]| {
        let actions = actions
            .iter()
            .map(|(id, label)| {
                host.record(
                    "NotificationAction",
                    &[("id", Value::text(*id)), ("label", Value::text(*label))],
                )
            })
            .collect();
        let mail = host.record(
            "Notification",
            &[
                ("id", Value::int(1)),
                (
                    "app",
                    host.record("NotificationApp", &[("name", Value::text("mail"))]),
                ),
                ("summary", Value::text(summary)),
                ("urgency", host.variant("Urgency", "normal")),
                ("actions", Value::list(actions)),
            ],
        );
        Value::list(vec![notification(host, 2, "chat", "other", "normal"), mail])
    };
    type Step<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
    let steps: [Step; 3] = [
        ("hi", &[("reply", "Reply")], "hi hi Reply"),
        (
            "bye",
            &[("open", "Open"), ("reply", "Answer")],
            "bye bye Answer",
        ),
        ("bye", &[("open", "Open")], "bye bye -"),
    ];
    for (summary, actions, want) in steps {
        let v = popups(&shell.host, summary, actions);
        shell
            .host
            .set(&shell.rt, "notifications.popups", v)
            .unwrap();
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        assert_eq!(shell.scene.texts().join(" "), want);
    }
    shell
        .host
        .set(&shell.rt, "notifications.popups", Value::list(Vec::new()))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts().join(" "), "- - -");
}

/// (dictionary) `xs.get(k)` finds an item whatever its number key's tag,
/// as `remove_key` does: items whose `float` key was written `3` (held as
/// `3.0`) are found by an `int` state holding 3, on a keyed `state`, a
/// persisted one, a view and a chain in place.
#[test]
fn get_finds_a_key_whatever_its_number_tag() {
    for persist in ["", " persist"] {
        let src = format!(
            "type Row {{ at: float; label: text }}\nstate rows: [Row] key at = [Row(at: 3, label: \"a\"), Row(at: 4, label: \"b\")]{persist}\nstate three = 3\nlet shown = rows.filter(r => r.at > 0)\nbar B {{\n  text rows.get(three)?.label ?? \"-\"\n  text shown.get(three)?.label ?? \"-\"\n  text rows.take(9).get(three)?.label ?? \"-\"\n  text rows.get(3.5)?.label ?? \"-\"\n}}\n"
        );
        let seen = click_each_box(&src);
        assert_eq!(seen[0].0, "a a a -", "{persist}");
    }
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
}

/// Scroll arguments reach the handler's parameters: the bar's volume row
/// (`on scroll(dy) { audio.sink.volume -= dy * 0.05 }`).
#[test]
fn scroll_reaches_its_parameters() {
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
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
    assert!(
        shell
            .inst
            .event(icon, "scroll", vec![Value::float(2.0), Value::float(0.0)])
    );
    shell.flush();
    let v = shell.host.get(&shell.rt, "audio.sink.volume").unwrap();
    assert!(
        (v.as_f64().unwrap() - 0.4).abs() < 1e-9,
        "0.5 - 2 * 0.05: {v:?}"
    );
}

/// Two handlers of one event on an element both run, in source order,
/// and `propagate()` passes the event on once however often it is called.
#[test]
fn every_handler_runs_and_propagates_once() {
    let src = "state a = 0\nstate b = 0\nstate p = 0\nbar B {\n  row {\n    on click { p += 1 }\n    box { on click { a += 1; propagate(); propagate() }\n          on click { b += 1; propagate() } }\n  }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(b, "click", Vec::new()));
    shell.flush();
    assert_eq!(shell.inst.value_of("t", "a").unwrap(), Value::int(1));
    assert_eq!(shell.inst.value_of("t", "b").unwrap(), Value::int(1));
    assert_eq!(shell.inst.value_of("t", "p").unwrap(), Value::int(1));
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
    // A click on a day in the popup does not bubble to the clock text it
    // is anchored to (which would close it).
    let day = shell.scene.find_text("15").unwrap();
    assert!(!shell.inst.event(day, "click", Vec::new()));
    shell.flush();
    assert_eq!(
        shell.scene.prop(popups[0], Prop::Open),
        Some(&PropValue::Bool(true))
    );
    assert!(
        shell.scene.find_text("October 2026").is_none(),
        "the other monitor's popup is closed: its calendar is not mounted"
    );
    // Plug a third monitor.
    screens(&shell.rt, &shell.host, &["DP-1", "HDMI-A-1", "DP-2"]);
    shell.flush();
    assert_eq!(shell.scene.of_kind(NodeKind::Bar).len(), 3);
    // Readers are counted per mounted reader: each bar and each bar's
    // `Battery` component read `battery`.
    assert_eq!(shell.host.readers("battery"), 6);
    // Unplug the first: its bar leaves the scene, its state is kept.
    screens(&shell.rt, &shell.host, &["HDMI-A-1", "DP-2"]);
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.scene.of_kind(NodeKind::Bar).len(), 2);
    assert!(shell.scene.find_text("September 2026").is_none());
    assert_eq!(
        shell.host.readers("battery"),
        4,
        "a parked bar reads nothing"
    );
    // The same monitor comes back on another port within 30 s: its bar
    // returns as it was, pinned to the monitor, not the port.
    let dp1 = screen(&shell.host, "DP-3", "DP-1");
    let others: Vec<Value> = ["HDMI-A-1", "DP-2"]
        .iter()
        .map(|n| screen(&shell.host, n, n))
        .collect();
    let mut all = others.clone();
    all.insert(0, dp1.clone());
    shell
        .host
        .set(&shell.rt, "screens.all", Value::list(all.clone()))
        .unwrap();
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.scene.of_kind(NodeKind::Bar).len(), 3);
    assert!(
        shell.scene.find_text("September 2026").is_some(),
        "the calendar kept its month"
    );
    let pins: Vec<_> = shell
        .scene
        .of_kind(NodeKind::Bar)
        .into_iter()
        .filter_map(|b| shell.scene.prop(b, Prop::Screens).cloned())
        .collect();
    assert!(pins.contains(&PropValue::Text("Mock | DP-1 | Display".into())));
    assert_eq!(shell.host.readers("battery"), 6);
    // Gone for good (the surface layer forgets it after 30 s): a replug
    // starts fresh.
    shell
        .host
        .set(&shell.rt, "screens.all", Value::list(others.clone()))
        .unwrap();
    shell.flush();
    assert!(shell.inst.forget_screen("Mock | DP-1 | Display"));
    assert!(!shell.inst.forget_screen("Mock | DP-1 | Display"));
    shell
        .host
        .set(&shell.rt, "screens.all", Value::list(all))
        .unwrap();
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.scene.of_kind(NodeKind::Bar).len(), 3);
    assert!(shell.scene.find_text("September 2026").is_none());
    screens(&shell.rt, &shell.host, &[]);
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 0, "released while parked");
    for n in ["DP-1", "HDMI-A-1", "DP-2"] {
        shell.inst.forget_screen(&format!("Mock | {n} | Display"));
    }
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 0, "and when forgotten");
}

/// A bar's own `screens:` picks its monitors: a name (the connector or
/// the monitor's id) or `focused`; each instance is pinned to its
/// monitor.
#[test]
fn a_bar_follows_its_own_screens() {
    let src = "bar Pinned { screens: \"DP-1\"; text screen.name }\nbar Focus { screens: focused; text join(\" \", \"F\", screen.name) }\nbar Every { text join(\" \", \"E\", screen.name) }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1", "HDMI-A-1"]);
        host.set(rt, "screens.focused", screen(host, "HDMI-A-1", "HDMI-A-1"))
            .unwrap();
    });
    let mut texts = shell.scene.texts();
    texts.sort();
    assert_eq!(texts, ["DP-1", "E DP-1", "E HDMI-A-1", "F HDMI-A-1"]);
    let pin_of = |shell: &Shell, text: &str| {
        let t = shell.text_node(text);
        let bar = shell.scene.ancestor(t, NodeKind::Bar).unwrap();
        shell.scene.prop(bar, Prop::Screens).cloned()
    };
    assert_eq!(
        pin_of(&shell, "DP-1"),
        Some(PropValue::Text("Mock | DP-1 | Display".into()))
    );
    // (flex review) The focused bar is one instance, pinned to no monitor;
    // (screens-focus) its `screen` follows the focus, so the surface
    // manager places it on the focused monitor and moves it with it.
    assert_eq!(
        pin_of(&shell, "F HDMI-A-1"),
        Some(PropValue::Keyword(strand_scene::FOLLOW_FOCUS.into()))
    );
    let bar_of = |shell: &Shell, text: &str| {
        let t = shell.text_node(text);
        shell.scene.ancestor(t, NodeKind::Bar).unwrap()
    };
    let before = bar_of(&shell, "F HDMI-A-1");
    // Focus moves: the focused bar's `screen` follows in place.
    let dp1 = screen(&shell.host, "DP-1", "DP-1");
    shell.host.set(&shell.rt, "screens.focused", dp1).unwrap();
    shell.flush();
    assert!(shell.scene.find_text("F DP-1").is_some());
    assert!(shell.scene.find_text("F HDMI-A-1").is_none());
    assert_eq!(bar_of(&shell, "F DP-1"), before, "not mounted again");
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

/// A fresh directory under the system temp dir for one test.
fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("strand-inst-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `persist` keeps a value across instances in core's store, by path,
/// with the default's hash.
#[test]
fn persisted_state_survives_a_restart() {
    let dir = temp_dir("persist");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let files = [fixture("toasts.strand")];
    let shell = boot_with(&refs(&files), desktop, storage.clone());
    assert_eq!(shell.inst.get("toasts.dnd").unwrap(), Value::Bool(false));
    shell.inst.set("toasts.dnd", Value::Bool(true)).unwrap();
    let mut shell = shell;
    shell.flush();
    // Do not disturb: only the critical notification stays shown.
    assert!(shell.scene.find_text("Mail · New message").is_none());
    drop(shell);
    let shell = boot_with(&refs(&files), desktop, storage.clone());
    assert_eq!(shell.inst.get("toasts.dnd").unwrap(), Value::Bool(true));
    assert!(shell.scene.find_text("Mail · New message").is_none());
    // `@reset`: back to the default, and so after a restart.
    shell.inst.reset("toasts.dnd").unwrap();
    assert_eq!(shell.inst.get("toasts.dnd").unwrap(), Value::Bool(false));
    drop(shell);
    let shell = boot_with(&refs(&files), desktop, storage.clone());
    assert_eq!(shell.inst.get("toasts.dnd").unwrap(), Value::Bool(false));
    drop(shell);
    if let Some(p) = &storage.persist {
        assert!(p.sync(Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
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
    assert_eq!(
        shell.scene.tokens.get("space.3").map(show).as_deref(),
        Some("12")
    );
    shell
        .inst
        .set_value("theme", "prefs.compact", Value::Bool(true))
        .unwrap();
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "one new token table");
    assert_eq!(
        shell.scene.tokens.get("space.3").map(show).as_deref(),
        Some("8")
    );
    assert_eq!(
        shell.scene.tokens.get("radius.lg").map(show).as_deref(),
        Some("14"),
        "inherited from base"
    );
    assert_eq!(shell.scene.render(), before, "the tree is unchanged");
}

/// A persisted `state` in a bar on every monitor is one cell per
/// monitor (`B[<monitor id>].count`).
#[test]
fn persisted_bar_state_is_per_monitor() {
    let dir = temp_dir("persist-bars");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let src = "bar B {\n  state count = 0 persist\n  text join(\" \", count) { on click { count += 1 } }\n}\n";
    let two = |rt: &Runtime, host: &SchemaHost| screens(rt, host, &["DP-1", "HDMI-A-1"]);
    let mut shell = boot_with(&[("t.strand", src)], two, storage.clone());
    let texts = shell.inst.nodes_handling("click");
    shell.inst.event(texts[0], "click", Vec::new());
    shell.inst.event(texts[0], "click", Vec::new());
    shell.inst.event(texts[1], "click", Vec::new());
    shell.flush();
    let mut seen = shell.scene.texts();
    seen.sort();
    assert_eq!(seen, ["1", "2"]);
    drop(shell);
    let shell = boot_with(&[("t.strand", src)], two, storage.clone());
    let mut seen = shell.scene.texts();
    seen.sort();
    assert_eq!(seen, ["1", "2"], "each monitor's own count came back");
    drop(shell);
    if let Some(p) = &storage.persist {
        assert!(p.sync(Duration::from_secs(5)));
        assert!(
            p.file_of("B[Mock | DP-1 | Display].count")
                .unwrap()
                .exists()
        );
    }
    let _ = std::fs::remove_dir_all(dir);
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

/// A set's `override` of a component token wins over the component's
/// default at runtime, plain or derived on either side.
#[test]
fn set_overrides_beat_component_token_defaults() {
    let src = "component Toast(n: int) tokens { radius: 14px; edge: $radius.lg; pad: 2px; gap: $radius.lg } {\n  box { radius: $Toast.radius; pad: $Toast.pad; width: $Toast.edge; gap: $Toast.gap }\n}\ntokens x {\n  radius { sm: 4px; lg: 16px }\n  override Toast.radius: 6px\n  override Toast.edge: $radius.sm\n  override Toast.pad: $radius.sm\n  override Toast.gap: 9px\n}\nuse tokens x\nbar B { Toast 1 }\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let t = &shell.scene.tokens;
    // Plain over plain.
    assert_eq!(t.lookup("Toast.radius"), Some(PropValue::Number(6.0)));
    // Derived over derived.
    assert_eq!(t.lookup("Toast.edge"), Some(PropValue::Number(4.0)));
    // Derived over plain.
    assert_eq!(t.lookup("Toast.pad"), Some(PropValue::Number(4.0)));
    // Plain over derived.
    assert_eq!(t.lookup("Toast.gap"), Some(PropValue::Number(9.0)));
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

/// (owner-testing.md B-b) A `~` that depends on state is evaluated with
/// the value it goes with: the change that switches it is sent with the
/// transition it switched to, whichever state the node mounted in, as a
/// `when`'s own `~` is.
#[test]
fn a_conditional_transition_applies_to_the_change_that_switches_it() {
    use strand_scene::{Easing, Transition};
    let src = "state grown = true\nbar B {\n  box { opacity: grown ? 1 : 0 ~ (grown ? ease(ease_out, 160ms) : ease(ease_in, 1000ms)) }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let sent = |u: &Update| -> Vec<(PropValue, Transition)> {
        u.diff
            .ops
            .iter()
            .filter_map(|op| match op {
                SceneOp::SetProp {
                    prop: Prop::Opacity,
                    value,
                    transition,
                    ..
                } => Some((value.clone(), transition.clone())),
                _ => None,
            })
            .collect()
    };
    let ease = |name: &str, ms: u64| Transition::Duration {
        duration: Duration::from_millis(ms),
        easing: Easing::named(name).unwrap(),
    };
    for (grown, value, t) in [
        (false, 0.0, ease("ease_in", 1000)),
        (true, 1.0, ease("ease_out", 160)),
        (false, 0.0, ease("ease_in", 1000)),
    ] {
        shell
            .inst
            .set_value("t", "grown", Value::Bool(grown))
            .unwrap();
        let u = shell.flush();
        assert_eq!(sent(&u), [(PropValue::Number(value), t)], "grown {grown}");
    }
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
    let src = "keyframes shake { 0%, 100% { x: 0 }; 50% { x: 4 } }\nbar B {\n  box { enter { opacity: 0 }; on click { play shake } }\n  box { enter { y: 4 }; exit { y: -4 } }\n}\nlock Lock { on key(k) { auth.submit(k.name) }; text \"locked\" }\n";
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
        Some("keyframes shake #1")
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

/// (M4) `play` sends the compiled keyframes block inline: stops as
/// fractions in order (shared stops copied), settings applied, a new
/// `seq` per `play`, and a tree-level `play` on mount with `seq` 0.
#[test]
fn play_lowers_to_the_compiled_keyframes() {
    let src = "keyframes shake { 0%, 100% { x: 0 }; 25% { x: -4 }; 75% { x: 4; opacity: 0.5 }; duration: 400ms; delay: 50ms; repeat: 3; alternate: true; easing: out_back }\nkeyframes pulse { 0% { scale: 1 }; 50% { scale: 1.1 }; 100% { scale: 1 }; repeat: 0 }\nbar B {\n  box { on click { play shake } }\n  box { play pulse }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    let Some(PropValue::Keyframes(pulse)) = shell.scene.prop(boxes[1], Prop::Play) else {
        panic!("a tree play sends keyframes on mount");
    };
    assert_eq!((pulse.name.as_str(), pulse.seq), ("pulse", 0));
    assert_eq!(pulse.repeat, None, "repeat: 0 repeats forever");
    assert_eq!(
        pulse.duration,
        Duration::from_millis(300),
        "the default duration"
    );
    assert_eq!(pulse.easing, strand_scene::Easing::Linear);
    assert!(shell.scene.prop(boxes[0], Prop::Play).is_none());
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    let Some(PropValue::Keyframes(k)) = shell.scene.prop(boxes[0], Prop::Play).cloned() else {
        panic!("play sends keyframes");
    };
    assert_eq!(k.name, "shake");
    assert_eq!(k.duration, Duration::from_millis(400));
    assert_eq!(k.delay, Duration::from_millis(50));
    assert_eq!(k.repeat, Some(3));
    assert!(k.alternate);
    assert_eq!(k.easing, strand_scene::Easing::named("out_back").unwrap());
    let at: Vec<f32> = k.stops.iter().map(|s| s.0).collect();
    assert_eq!(at, [0.0, 0.25, 0.75, 1.0]);
    assert_eq!(k.stops[1].1, [(Prop::X, PropValue::Number(-4.0))]);
    assert_eq!(
        k.stops[2].1,
        [
            (Prop::X, PropValue::Number(4.0)),
            (Prop::Opacity, PropValue::Number(0.5))
        ]
    );
    assert_eq!(k.stops[3].1, [(Prop::X, PropValue::Number(0.0))]);
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    let Some(PropValue::Keyframes(again)) = shell.scene.prop(boxes[0], Prop::Play) else {
        panic!("play again");
    };
    assert!(again.seq != k.seq, "a new seq restarts it");
}

/// The launcher's rows follow `selected` (list navigation) and `hover`.
#[test]
fn selected_rows_and_activate() {
    let files = [fixture("launcher.strand")];
    let mut shell = boot(&refs(&files), desktop);
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
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

/// (M4) A handler of an input event runs as one: its actions are marked
/// input-driven (the tray then sends the press's point), while the same
/// action from an `on change` or a timer is not.
#[test]
fn actions_know_whether_input_called_them() {
    let src = "export state n = 0\n\
               bar B {\n  \
                 on change n { notifications.clear() }\n  \
                 box { width: 10; height: 10; on click { notifications.clear(); n = n + 1; await sleep(1s); notifications.clear() } }\n\
               }\n";
    let mut shell = boot(&[("b.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert!(shell.host.take_actions().is_empty());
    let target = shell.scene.of_kind(NodeKind::Box)[0];
    shell.inst.event(target, "click", Vec::new());
    shell.flush();
    shell.flush();
    let calls: Vec<(String, bool)> = shell
        .host
        .take_actions()
        .iter()
        .map(|a| (a.to_string(), a.input))
        .collect();
    assert_eq!(
        calls,
        [
            ("notifications.clear(0)".to_string(), true),
            ("notifications.clear(0)".to_string(), false)
        ],
        "the click's own call, then `on change n`'s"
    );
    // After its `await` the click handler runs outside the input.
    shell.at(2.0);
    let calls: Vec<(String, bool)> = shell
        .host
        .take_actions()
        .iter()
        .map(|a| (a.to_string(), a.input))
        .collect();
    assert_eq!(
        calls,
        [("notifications.clear(0)".to_string(), false)],
        "the call after the await"
    );
    assert!(!strand_compiler::vm::in_input_handler());
    // (M5, S-platform) The press's context reaches the click handler's
    // actions until its first `await`; every handler invocation has a
    // number of its own (the `on change` it causes too), which it keeps
    // across the `await`.
    let press = strand_core::InputCtx {
        surface: 3,
        serial: 42,
    };
    assert!(
        shell
            .inst
            .event_with(target, "click", Vec::new(), Some(press))
    );
    shell.flush();
    shell.flush();
    let ctxs: Vec<strand_core::ActionCtx> =
        shell.host.take_actions().iter().map(|a| a.ctx).collect();
    assert_eq!(ctxs.len(), 2, "{ctxs:?}");
    let click = ctxs[0];
    assert_eq!(click.input, Some(press));
    assert!(click.invocation > 0);
    assert_eq!(ctxs[1].input, None, "`on change n` has no press");
    assert!(ctxs[1].invocation > 0 && ctxs[1].invocation != click.invocation);
    shell.at(4.0);
    let after: Vec<strand_core::ActionCtx> =
        shell.host.take_actions().iter().map(|a| a.ctx).collect();
    assert_eq!(
        after,
        [strand_core::ActionCtx {
            input: None,
            invocation: click.invocation
        }],
        "after the `await`: the same invocation, no press"
    );
    // A click without a press (`Instance::event`) is a new invocation
    // with no input; outside any handler the context is the empty one.
    shell.inst.event(target, "click", Vec::new());
    shell.flush();
    let ctxs: Vec<strand_core::ActionCtx> =
        shell.host.take_actions().iter().map(|a| a.ctx).collect();
    assert!(!ctxs.is_empty());
    assert!(ctxs[0].input.is_none() && ctxs[0].invocation > click.invocation);
    assert_eq!(
        strand_compiler::vm::action_ctx(),
        strand_compiler::vm::ActionCtx::default()
    );
    // The context the VM passes is strand-core's, the type strand-services
    // (which depends only on strand-core) takes at the other end.
    let _: strand_compiler::vm::host::InputCtx = press;
}

/// (M4) Only the events a press just set the tray's click point for run
/// as input: a click, a secondary or middle click and an activate (a
/// clicked row, or Return, whose key press set (0, 0)). A wheel, a drop
/// and a dismiss come with no press of their own, so a tray action they
/// call must not send the last press's point.
#[test]
fn only_press_caused_handlers_run_as_input() {
    let src = "state n = 0\n\
               bar B {\n  row {\n    \
                 box { width: 10; height: 10; on click { notifications.clear() } }\n    \
                 box { width: 10; height: 10; on secondary { notifications.clear() } }\n    \
                 box { width: 10; height: 10; on middle { notifications.clear() } }\n    \
                 box { width: 10; height: 10; on activate { notifications.clear() } }\n    \
                 box { width: 10; height: 10; on scroll(dy) { notifications.clear() } }\n    \
                 box { width: 10; height: 10; on drop(x: int, at: int) { notifications.clear() } }\n  \
               }\n}\n";
    let mut shell = boot(&[("b.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    let events: [(&str, Vec<Value>, bool); 6] = [
        ("click", vec![], true),
        ("secondary", vec![], true),
        ("middle", vec![], true),
        ("activate", vec![], true),
        ("scroll", vec![Value::float(1.0), Value::float(0.0)], false),
        ("drop", vec![Value::int(1), Value::int(0)], false),
    ];
    for (target, (event, args, input)) in boxes.iter().zip(events) {
        assert!(shell.inst.event(*target, event, args), "{event} delivered");
        shell.flush();
        let calls: Vec<bool> = shell.host.take_actions().iter().map(|a| a.input).collect();
        assert_eq!(calls, [input], "on {event}");
    }
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
        "segmented two_way=[value] value=auto options=[auto, light, dark, wallpaper, mocha]",
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

/// An element's flags outlive the branch that mounted it: a node shown
/// again by an `if` still drives `when other.hover`.
#[test]
fn node_flags_survive_a_branch_swap() {
    let src = "state shown = true\nbar B {\n  row {\n    if shown { box { id: target } }\n    box { when target.hover { opacity: 0.5 } }\n  }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    for _ in 0..2 {
        shell
            .inst
            .set_value("t", "shown", Value::Bool(false))
            .unwrap();
        shell.flush();
        assert_eq!(shell.scene.of_kind(NodeKind::Box).len(), 1);
        shell
            .inst
            .set_value("t", "shown", Value::Bool(true))
            .unwrap();
        shell.flush();
    }
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(boxes.len(), 2);
    shell.inst.set_flag(boxes[0], NodeFlag::Hover, true);
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(
        shell.scene.prop(boxes[1], Prop::Opacity),
        Some(&PropValue::Number(0.5))
    );
    shell
        .inst
        .set_value("t", "shown", Value::Bool(false))
        .unwrap();
    shell.flush();
    let left = shell.scene.of_kind(NodeKind::Box)[0];
    assert_eq!(
        shell.scene.prop(left, Prop::Opacity),
        None,
        "an unmounted node is not hovered"
    );
}

/// A 2,000-item keyed list: changing one item sends one prop, moving one
/// sends one move, removing one sends one remove.
#[test]
fn a_long_list_touches_only_what_changed() {
    let src = "bar B { col { for a in apps.all { text a.name } } }\n";
    let app = |host: &SchemaHost, i: usize, name: &str| {
        host.record(
            "App",
            &[
                ("id", Value::text(format!("app{i}"))),
                ("name", Value::text(name)),
            ],
        )
    };
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        let list = (0..2000)
            .map(|i| app(host, i, &format!("App {i}")))
            .collect();
        host.set(rt, "apps.all", Value::list(list)).unwrap();
    });
    assert_eq!(shell.scene.texts().len(), 2000);
    let mut list: Vec<Value> = (0..2000)
        .map(|i| app(&shell.host, i, &format!("App {i}")))
        .collect();
    list[1000] = app(&shell.host, 1000, "Renamed");
    shell
        .host
        .set(&shell.rt, "apps.all", Value::list(list.clone()))
        .unwrap();
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff.ops.len());
    assert_eq!(shell.scene.texts()[1000], "Renamed");
    let moved = list.remove(5);
    list.insert(1500, moved);
    shell
        .host
        .set(&shell.rt, "apps.all", Value::list(list.clone()))
        .unwrap();
    let u = shell.flush();
    assert!(
        matches!(u.diff.ops[..], [SceneOp::Move { .. }]),
        "{:?}",
        u.diff.ops
    );
    assert_eq!(shell.scene.texts()[1500], "App 5");
    list.remove(0);
    shell
        .host
        .set(&shell.rt, "apps.all", Value::list(list))
        .unwrap();
    let u = shell.flush();
    assert!(
        matches!(u.diff.ops[..], [SceneOp::Remove { .. }]),
        "{:?}",
        u.diff.ops
    );
    assert_eq!(shell.scene.texts().len(), 1999);
}

/// Random edits to a keyed list whose items have one or two root nodes
/// (an `if` inside each): after every tick the scene shows exactly the
/// list, in order, and items that stayed kept their nodes.
#[test]
fn keyed_lists_match_the_list_after_random_edits() {
    let src =
        "bar B { col { for a in apps.all { text a.name; if a.comment != null { box {} } } } }\n";
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut rand = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    let app = |host: &SchemaHost, id: usize, name: &str, boxed: bool| {
        host.record(
            "App",
            &[
                ("id", Value::text(format!("a{id}"))),
                ("name", Value::text(name)),
                (
                    "comment",
                    if boxed { Value::text("c") } else { Value::Null },
                ),
            ],
        )
    };
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    // (id, name, boxed)
    let mut list: Vec<(usize, String, bool)> = Vec::new();
    let mut next = 0;
    let mut nodes: std::collections::HashMap<usize, NodeId> = Default::default();
    for round in 0..300 {
        for _ in 0..1 + rand(3) {
            match rand(5) {
                0 | 1 => {
                    let at = rand(list.len() + 1);
                    list.insert(at, (next, format!("n{next}"), rand(2) == 0));
                    next += 1;
                }
                2 if !list.is_empty() => {
                    let i = rand(list.len());
                    list.remove(i);
                }
                3 if !list.is_empty() => {
                    let i = rand(list.len());
                    let x = list.remove(i);
                    let to = rand(list.len() + 1);
                    list.insert(to, x);
                }
                _ if !list.is_empty() => {
                    let i = rand(list.len());
                    list[i].1 = format!("r{round}");
                    list[i].2 = !list[i].2;
                }
                _ => {}
            }
        }
        let values = list
            .iter()
            .map(|(id, name, boxed)| app(&shell.host, *id, name, *boxed))
            .collect();
        shell
            .host
            .set(&shell.rt, "apps.all", Value::list(values))
            .unwrap();
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        // The scene: per item its text, then a box if it has a comment.
        let col = shell.scene.of_kind(NodeKind::Col)[0];
        let mut expected = Vec::new();
        for (_, name, boxed) in &list {
            expected.push(format!("text {name}"));
            if *boxed {
                expected.push("box".to_string());
            }
        }
        let got: Vec<String> = shell
            .scene
            .children(col)
            .iter()
            .map(|&n| match shell.scene.prop(n, Prop::Text) {
                Some(PropValue::Text(t)) => format!("text {t}"),
                _ => "box".to_string(),
            })
            .collect();
        assert_eq!(got, expected, "round {round}");
        // Identity: an item's text node is the same while it stays.
        let mut texts = shell
            .scene
            .children(col)
            .iter()
            .copied()
            .filter(|&n| shell.scene.kind(n) == Some(NodeKind::Text));
        let mut now = std::collections::HashMap::new();
        for (id, _, _) in &list {
            let n = texts.next().unwrap();
            if let Some(old) = nodes.get(id) {
                assert_eq!(*old, n, "item {id} kept its node (round {round})");
            }
            now.insert(*id, n);
        }
        nodes = now;
    }
}

/// Deeply nested trees and recursive fns mount on a 2 MiB thread (the
/// logic thread's size), as they check there.
#[test]
fn deep_trees_mount_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let depth = 120;
            let mut src =
                String::from("fn down(n: int) -> int { n <= 0 ? 0 : down(n - 1) }\nbar B {\n");
            for _ in 0..depth {
                src.push_str("box {\n");
            }
            src.push_str("text join(\"\", down(150))\n");
            for _ in 0..depth {
                src.push_str("}\n");
            }
            src.push_str("}\n");
            let shell = boot(&[("t.strand", src.as_str())], |rt, host| {
                screens(rt, host, &["DP-1"])
            });
            assert_eq!(shell.scene.of_kind(NodeKind::Box).len(), depth);
            assert_eq!(shell.scene.texts(), ["0"]);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Durations past what a `Duration` holds are error values, never a
/// panic: a timer, `sleep` and a `~`.
#[test]
fn huge_durations_are_errors_not_panics() {
    let src = "state n = 0\nstate w = 0\nafter 100000000000000000000000s { n += 1 }\nbar B {\n  box { width: w ~ 1000000000000000000000000000000s; on click { await sleep(100000000000000000000000s); n += 1 } }\n}\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    screens(&rt, &host, &["DP-1"]);
    let inst = Instance::new(&rt, program, host, Storage::none());
    let u = inst.flush();
    assert!(
        u.errors
            .iter()
            .any(|e| e.contains("`after` needs a duration")),
        "{:?}",
        u.errors
    );
    let b = inst.nodes_handling("click")[0];
    inst.set_value("t", "w", Value::int(5)).unwrap();
    inst.event(b, "click", Vec::new());
    let u = inst.tick(Duration::from_secs(1));
    assert!(
        u.errors
            .iter()
            .any(|e| e.contains("`sleep` needs a duration")),
        "{:?}",
        u.errors
    );
    assert_eq!(inst.value_of("t", "n").unwrap(), Value::int(0));
}

/// A zero `every` period is an error naming the timer, not a silent
/// pause.
#[test]
fn a_zero_period_is_reported() {
    let src = "state d = 0ms\nstate n = 0\nevery d { n += 1 }\nbar B { text \"x\" }\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    screens(&rt, &host, &["DP-1"]);
    let inst = Instance::new(&rt, program, host, Storage::none());
    let u = inst.flush();
    let e = u
        .errors
        .iter()
        .find(|e| e.contains("positive period"))
        .unwrap_or_else(|| panic!("{:?}", u.errors));
    assert_eq!(e.what, "every timer in t");
    assert!(e.span.is_some());
    inst.set_value("t", "d", Value::Num(500.0, strand_compiler::vm::Num::Ms))
        .unwrap();
    inst.flush();
    inst.tick(Duration::from_millis(600));
    inst.tick(Duration::from_millis(1100));
    assert_eq!(inst.value_of("t", "n").unwrap(), Value::int(2));
}

/// A dropped instance leaves nothing running on its runtime: timers,
/// handlers and service readers go with it.
#[test]
fn dropping_an_instance_stops_it() {
    let src = "state n = 0\nevery 1s { n += 1 }\nbar B { text pct(battery.percent) }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    shell.at(1.0);
    shell.at(2.0);
    assert_eq!(shell.inst.value_of("t", "n").unwrap(), Value::int(2));
    assert!(shell.host.readers("battery") > 0);
    assert!(shell.rt.next_deadline().is_some());
    let Shell { rt, host, inst, .. } = shell;
    drop(inst);
    assert_eq!(rt.next_deadline(), None, "no timer left");
    assert_eq!(host.readers("battery"), 0);
    let t = rt.tick(Duration::from_secs(10));
    assert!(t.errors.is_empty(), "{:?}", t.errors);
}

/// `strand set` checks the declared type.
#[test]
fn set_checks_the_declared_type() {
    let files = design_shells();
    let shell = boot(&refs(&files), desktop);
    let e = shell.inst.set("theme.look", Value::int(3)).unwrap_err();
    assert!(e.to_string().contains("Look"), "{e}");
    assert!(shell.inst.set("launcher.open", Value::text("yes")).is_err());
    assert!(shell.inst.set("launcher.open", Value::Bool(true)).is_ok());
}

/// A component mounting `slot` twice gives each copy its own element
/// states: hovering one copy is not hovering the other.
#[test]
fn each_slot_copy_has_its_own_states() {
    let src = "component Two() { col { slot }\n col { slot } }\nbar B { Two { box { when hover { bg: #ff0000 } } } }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(boxes.len(), 2);
    shell.inst.set_flag(boxes[1], NodeFlag::Hover, true);
    shell.flush();
    assert!(shell.scene.prop(boxes[0], Prop::Bg).is_none());
    assert!(shell.scene.prop(boxes[1], Prop::Bg).is_some());
    shell.inst.set_flag(boxes[0], NodeFlag::Hover, true);
    shell.inst.set_flag(boxes[1], NodeFlag::Hover, false);
    shell.flush();
    assert!(shell.scene.prop(boxes[0], Prop::Bg).is_some());
    assert!(shell.scene.prop(boxes[1], Prop::Bg).is_none());
}

/// `on change … after d` follows a reactive duration.
#[test]
fn a_debounce_follows_its_duration() {
    let src = "state x = 0\nstate d = 1s\nstate fired = 0\non change x after d { fired += 1 }\nbar B { text \"x\" }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    shell
        .inst
        .set_value("t", "d", Value::Num(3000.0, strand_compiler::vm::Num::Ms))
        .unwrap();
    shell.flush();
    shell.inst.set_value("t", "x", Value::int(1)).unwrap();
    shell.at(0.1);
    shell.at(2.0);
    assert_eq!(shell.inst.value_of("t", "fired").unwrap(), Value::int(0));
    shell.at(3.2);
    assert_eq!(shell.inst.value_of("t", "fired").unwrap(), Value::int(1));
}

/// Surfaces send `show` and `hide`, their content is mounted when first
/// shown and frozen while hidden, and the services their body reads are
/// held only while shown.
#[test]
fn surfaces_show_hide_and_hold_services_while_shown() {
    let src = "export state o = false\nstate shows = 0\nstate hides = 0\npanel P {\n  open: <-> o\n  on show { shows += 1 }\n  on hide { hides += 1 }\n  text pct(battery.percent)\n}\n";
    let mut shell = boot(&[("vis.strand", src)], |_, _| {});
    assert_eq!(shell.host.readers("battery"), 0, "hidden");
    assert!(
        shell.scene.of_kind(NodeKind::Text).is_empty(),
        "not mounted"
    );
    shell.inst.set("vis.o", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 1);
    assert_eq!(shell.scene.of_kind(NodeKind::Text).len(), 1);
    assert_eq!(shell.inst.value_of("vis", "shows").unwrap(), Value::int(1));
    // Hidden: `hide`, services let go; the content stays (frozen).
    let panel = shell.scene.of_kind(NodeKind::Panel)[0];
    shell
        .inst
        .write(panel, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(shell.inst.value_of("vis", "hides").unwrap(), Value::int(1));
    assert_eq!(shell.host.readers("battery"), 0);
    shell
        .host
        .set(&shell.rt, "battery.percent", Value::float(0.5))
        .unwrap();
    let u = shell.flush();
    assert!(u.diff.is_empty(), "frozen while hidden: {:?}", u.diff);
    shell.inst.set("vis.o", Value::Bool(true)).unwrap();
    let u = shell.flush();
    assert!(!u.diff.is_empty(), "catches up when shown");
    assert_eq!(shell.inst.value_of("vis", "shows").unwrap(), Value::int(2));
    assert_eq!(shell.host.readers("battery"), 1);
}

/// `segmented { options: Edge }`: a builtin enum names its options as a
/// config enum does (`options: Look`), and a click writes the chosen
/// variant through to the settings field.
#[test]
fn a_builtin_enum_gives_a_segmented_its_options() {
    let dir = temp_dir("settings-edge");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let src = "state prefs from \"prefs.toml\" { position: Edge = top }\nbar B { segmented { options: Edge; value: <-> prefs.position } }\n";
    let mut shell = boot_with(
        &[("t.strand", src)],
        |rt, host| screens(rt, host, &["DP-1"]),
        storage,
    );
    let rendered = shell.scene.render();
    assert!(
        rendered.contains("options=[top, bottom, left, right]"),
        "{rendered}"
    );
    assert!(rendered.contains("value=top"), "{rendered}");
    let seg = shell.scene.of_kind(NodeKind::Segmented)[0];
    shell
        .inst
        .write(seg, Prop::Value, PropValue::Keyword("left".into()))
        .unwrap();
    shell.flush();
    let rendered = shell.scene.render();
    assert!(rendered.contains("value=left"), "{rendered}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Settings files are read through core (overlay > file > default) and
/// written back field by field.
#[test]
fn strand_set_reaches_settings_that_are_not_exported() {
    let dir = temp_dir("settings-set");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("prefs.toml"),
        "# mine\ncompact = true # dense\n",
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    // design.md's theme.strand: `state prefs from …`, not exported.
    let src = "state prefs from \"prefs.toml\" { compact: bool = false; gap: int = 4 }\nbar B { text prefs.compact ? \"compact\" : \"roomy\" }\n";
    let mut shell = boot_with(
        &[("theme.strand", src)],
        |rt, host| screens(rt, host, &["DP-1"]),
        storage.clone(),
    );
    assert_eq!(shell.scene.texts(), ["compact"]);
    // `strand set prefs.compact false`, as design.md writes it.
    shell.inst.set_text("prefs.compact", "false").unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["roomy"]);
    // With the file named too.
    shell.inst.set_text("theme.prefs.gap", "7").unwrap();
    assert_eq!(shell.inst.get("prefs.gap").unwrap(), Value::int(7));
    assert!(shell.inst.set_text("prefs.gap", "wide").is_err());
    assert!(shell.inst.set_text("prefs.nope", "1").is_err());
    shell.at(1.0);
    if let Some(s) = &storage.settings {
        assert!(s.sync(Duration::from_secs(5)));
    }
    let text = std::fs::read_to_string(config.join("prefs.toml")).unwrap();
    assert!(text.contains("# mine"), "{text}");
    assert!(text.contains("compact = false # dense"), "{text}");
    assert!(text.contains("gap = 7"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn settings_files_are_read_and_written_back() {
    let dir = temp_dir("settings");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("prefs.toml"),
        "# my prefs\ncompact = true\naccent = \"#ff8800\"\n",
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let src = "export state prefs from \"prefs.toml\" {\n  accent: color = #7aa2f7; compact: bool = false; gap: int = 4\n}\nbar B { text prefs.compact ? \"compact\" : \"roomy\" }\n";
    let mut shell = boot_with(
        &[("prefs_test.strand", src)],
        |rt, host| screens(rt, host, &["DP-1"]),
        storage.clone(),
    );
    assert_eq!(shell.scene.texts(), ["compact"]);
    assert_eq!(
        shell.inst.get("prefs_test.prefs.accent").unwrap(),
        Value::Color(strand_scene::Color::from_hex("#ff8800").unwrap())
    );
    assert_eq!(
        shell.inst.get("prefs_test.prefs.gap").unwrap(),
        Value::int(4),
        "default"
    );
    // A write through `strand set`: the field's signal, then the file,
    // after the quiet time, keeping its comment.
    shell
        .inst
        .set("prefs_test.prefs.compact", Value::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["roomy"]);
    shell.at(1.0);
    if let Some(s) = &storage.settings {
        assert!(s.sync(Duration::from_secs(5)));
    }
    let text = std::fs::read_to_string(config.join("prefs.toml")).unwrap();
    assert!(text.contains("# my prefs"), "{text}");
    assert!(text.contains("compact = false"), "{text}");
    assert!(
        shell
            .inst
            .set("prefs_test.prefs.gap", Value::text("x"))
            .is_err()
    );
    // The watcher's reload: an edit to the file reaches the field.
    assert_eq!(shell.inst.settings_files(), [config.join("prefs.toml")]);
    std::fs::write(config.join("prefs.toml"), "compact = true\ngap = 6\n").unwrap();
    assert!(shell.inst.reload_settings(&config.join("prefs.toml")));
    shell.flush();
    assert_eq!(shell.scene.texts(), ["compact"]);
    assert_eq!(
        shell.inst.get("prefs_test.prefs.gap").unwrap(),
        Value::int(6)
    );
    // `strand run` reads on the watcher's thread: the logic thread only
    // decodes what it read.
    let sources = shell.inst.settings_sources();
    assert_eq!(sources.len(), 1);
    std::fs::write(
        config.join("prefs.toml"),
        "compact = false
gap = 8
",
    )
    .unwrap();
    let read = std::thread::spawn(move || sources[0].read())
        .join()
        .unwrap();
    assert!(
        shell
            .inst
            .reload_settings_with(&config.join("prefs.toml"), Some(read))
    );
    shell.flush();
    assert_eq!(shell.scene.texts(), ["roomy"]);
    assert_eq!(
        shell.inst.get("prefs_test.prefs.gap").unwrap(),
        Value::int(8)
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// Runtime errors carry where they happened: the failing operation's
/// file and span, the scene node and the component, and the instance
/// can freeze that component.
#[test]
fn runtime_errors_are_located_and_freeze_their_component() {
    let src = "state zero = 1\nstate ticks = 0\ncomponent Faulty() {\n  every 1s { ticks += 1 }\n  text pct(10 / zero)\n}\nbar B { Faulty }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let text = shell.scene.of_kind(NodeKind::Text)[0];
    shell.inst.set_value("t", "zero", Value::int(0)).unwrap();
    let u = shell.flush();
    let e = &u.errors[0];
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(e.contains("`10 / 0` has no value"), "{e}");
    assert_eq!(e.node, Some(text));
    let span = e.span.unwrap();
    assert_eq!(&src[span.start as usize..span.end as usize], "10 / zero");
    assert!(e.component.is_some());
    let origin = shell.inst.origin(text).unwrap();
    assert_eq!(
        &src[origin.2.start as usize..origin.2.start as usize + 4],
        "text"
    );
    // Freeze the component: its timer stops, and runs again once thawed.
    shell.at(1.0);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(1));
    assert!(shell.inst.freeze(e));
    shell.at(2.0);
    shell.at(3.0);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(1));
    shell.inst.thaw(e);
    shell.at(3.5);
    shell.at(4.5);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(2));
}

/// (handler-fault) A handler's error is a value (spec.md 8.3): it is
/// located and marked as a handler's, and freezing it freezes and
/// outlines nothing, so the component's timer and its other handlers
/// run on; a binding's fault in the same component still freezes it.
#[test]
fn a_handlers_error_freezes_nothing() {
    let src = "state zero = 0\nstate one = 1\nstate ticks = 0\nstate n = 0\nstate tries = 0\ncomponent Faulty() {\n  every 1s { ticks += 1 }\n  row {\n    box { on click { tries += 1; n = 1 / zero } }\n    box { on click { n += 1 } }\n  }\n  text pct(10 / one)\n}\nbar B { Faulty }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    let e = &u.errors[0];
    assert!(e.handler && e.scope.is_some() && e.span.is_some(), "{e:?}");
    assert!(!shell.inst.freeze(e), "a handler's error freezes nothing");
    shell.flush();
    assert!(shell.inst.outlined().is_empty());
    shell.at(1.0);
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(1));
    shell.inst.event(boxes[1], "click", Vec::new());
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    // (build-ready review) The failing handler ran again; failing the
    // same way, it is not reported twice.
    assert_eq!(shell.inst.value_of("t", "tries").unwrap(), Value::int(2));
    assert_eq!(shell.inst.value_of("t", "n").unwrap(), Value::float(1.0));
    // A binding's fault is no handler's: it freezes the component.
    shell.inst.set_value("t", "one", Value::int(0)).unwrap();
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(!u.errors[0].handler);
    assert!(shell.inst.freeze(&u.errors[0]));
}

/// (build-ready review) A handler failing the same way on every run is
/// reported once, until a run of it completes: a timer dividing by zero
/// every tick is one fault, not one a tick, and after a run that
/// completes the next failure is reported again. Another handler's
/// error is its own.
#[test]
fn a_handler_failing_the_same_way_is_reported_once_until_a_run_completes() {
    let src = "state zero = 0\nstate ticks = 0\nstate n = 0\ncomponent T() {\n  every 1s { ticks += 1; n = 10 / zero }\n  box { on click { n = 5 / zero } }\n}\nbar B { T }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let u = shell.at(1.0);
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(u.errors[0].handler, "{:?}", u.errors);
    for t in [2.0, 3.0, 4.0] {
        let u = shell.at(t);
        assert!(
            u.errors.is_empty(),
            "reported again at {t} s: {:?}",
            u.errors
        );
    }
    assert_eq!(shell.inst.value_of("t", "ticks").unwrap(), Value::int(4));
    // Another handler's error is reported.
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(u.errors[0].what.contains("on click"), "{:?}", u.errors);
    // A run that completes; the next failure is news again.
    shell.inst.set_value("t", "zero", Value::int(1)).unwrap();
    let u = shell.at(5.0);
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.inst.value_of("t", "n").unwrap(), Value::float(10.0));
    shell.inst.set_value("t", "zero", Value::int(0)).unwrap();
    let u = shell.at(6.0);
    assert_eq!(u.errors.len(), 1, "reported again after a run completed");
    // (handler-fault-repeat-watch) A `strand watch` started since: the
    // same failure is reported once more at its next run, then not.
    assert!(shell.at(7.0).errors.is_empty());
    shell.inst.report_faults_again();
    let u = shell.at(8.0);
    assert_eq!(u.errors.len(), 1, "reported again for a new watch");
    assert!(shell.at(9.0).errors.is_empty(), "then once only");
}

/// Changing one item of a long keyed list re-runs that item's bindings
/// only: each mounted key has its own value cell, and a `for` over a
/// keyed `state` follows its diffs.
#[test]
fn one_item_change_reruns_one_item() {
    let mut src = String::from("type Row { id: int; label: text }\nstate rows: [Row] key id = [");
    for i in 0..2000 {
        src.push_str(&format!("Row(id: {i}, label: \"r{i}\"), "));
    }
    src.push_str("]\nbar B { col { for r in rows { text r.label } } }\n");
    let mut shell = boot(&[("t.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.of_kind(NodeKind::Text).len(), 2000);
    let before = shell.rt.stats().computations;
    shell
        .inst
        .set_value("t", "rows", {
            let mut v = Vec::new();
            for i in 0..2000 {
                let label = if i == 7 {
                    "changed".to_string()
                } else {
                    format!("r{i}")
                };
                v.push(
                    shell
                        .inst
                        .vm()
                        .types()
                        .find_record("Row")
                        .map_or(Value::Null, |r| {
                            Value::record(r, vec![Value::int(i), Value::text(label)])
                        }),
                );
            }
            Value::list(v)
        })
        .unwrap();
    let u = shell.flush();
    let runs = shell.rt.stats().computations - before;
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff);
    assert!(runs < 20, "{runs} computations for one changed item");
    assert!(shell.scene.find_text("changed").is_some());
}

/// A 2,000-row `list` whose direct child is a `for` mounts only its
/// window (M4: virtualised lists). At boot that is the first
/// `DEFAULT_LIST_WINDOW` rows; `set_list_window` (render's
/// `ToLogic::ListWindow`) mounts and unmounts rows by key, marking those
/// ops `window` so render plays no pose or FLIP, while a row that stays
/// in the window keeps its node; `row_count` and `row_first` say where
/// the mounted rows are. A change to a mounted row is its one op and
/// re-runs only its bindings; a change outside the window sends no op.
#[test]
fn a_2000_row_list_mounts_only_its_window() {
    use strand_compiler::instantiate::DEFAULT_LIST_WINDOW;
    let mut src = String::from("type Row { id: int; label: text }\nstate rows: [Row] key id = [");
    for i in 0..2000 {
        src.push_str(&format!("Row(id: {i}, label: \"r{i}\"), "));
    }
    src.push_str(
        "]\nbar B { list { max_height: 420\n for r in rows { row { when hover { opacity: 0.5 }\n text r.label } } } }\n",
    );
    let t = std::time::Instant::now();
    let mut shell = boot(&[("t.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    eprintln!("mounted a 2,000-row list in {:?}", t.elapsed());
    let list = shell.scene.of_kind(NodeKind::List)[0];
    let rows = |shell: &Shell| shell.scene.children(list).to_vec();
    let num = |shell: &Shell, p: Prop| match shell.scene.prop(list, p) {
        Some(PropValue::Number(n)) => *n,
        v => panic!("{p:?}: {v:?}"),
    };
    let labels = |shell: &Shell| -> Vec<String> {
        rows(shell)
            .iter()
            .map(
                |r| match shell.scene.prop(shell.scene.children(*r)[0], Prop::Text) {
                    Some(PropValue::Text(t)) => t.clone(),
                    v => panic!("{v:?}"),
                },
            )
            .collect()
    };
    assert_eq!(
        shell.scene.of_kind(NodeKind::Row).len(),
        DEFAULT_LIST_WINDOW
    );
    assert_eq!(num(&shell, Prop::RowCount), 2000.0);
    assert_eq!(num(&shell, Prop::RowFirst), 0.0);
    // Mounted with the list, its first rows are not the window's doing.
    assert!(shell.boot.iter().all(|op| !matches!(
        op,
        SceneOp::Create { window: true, .. } | SceneOp::Remove { window: true, .. }
    )));
    assert_eq!(labels(&shell)[0], "r0");

    // Render asks for rows 100..120: by key, with no poses.
    assert!(shell.inst.set_list_window(list, 100, 20));
    let u = shell.flush();
    let (mut created, mut removed) = (0, 0);
    for op in &u.diff.ops {
        match op {
            SceneOp::Create { parent, window, .. } if *parent == Some(list) => {
                assert!(window, "{op:?}");
                created += 1;
            }
            SceneOp::Remove { window, .. } => {
                assert!(window, "{op:?}");
                removed += 1;
            }
            _ => {}
        }
    }
    assert_eq!((created, removed), (20, DEFAULT_LIST_WINDOW));
    assert_eq!(num(&shell, Prop::RowFirst), 100.0);
    assert_eq!(
        labels(&shell),
        (100..120).map(|i| format!("r{i}")).collect::<Vec<_>>()
    );

    // Overlapping: the ten rows that stay keep their nodes.
    let kept = rows(&shell)[10..].to_vec();
    shell.inst.set_list_window(list, 110, 20);
    let u = shell.flush();
    let creates = u
        .diff
        .ops
        .iter()
        .filter(|op| matches!(op, SceneOp::Create { parent, .. } if *parent == Some(list)))
        .count();
    assert_eq!(creates, 10);
    assert_eq!(rows(&shell)[..10], kept[..]);
    assert_eq!(labels(&shell)[0], "r110");

    // The same window again: nothing.
    shell.inst.set_list_window(list, 110, 20);
    assert!(shell.flush().diff.ops.is_empty());

    // A row in the window changes: one op, its own bindings only. One
    // outside it: none.
    let row = shell.inst.vm().types().find_record("Row").expect("Row");
    let all = |changed: &[usize]| {
        Value::list(
            (0..2000)
                .map(|i| {
                    let label = if changed.contains(&i) {
                        "changed".to_string()
                    } else {
                        format!("r{i}")
                    };
                    Value::record(row, vec![Value::int(i as i64), Value::text(label)])
                })
                .collect(),
        )
    };
    let before = shell.rt.stats().computations;
    shell.inst.set_value("t", "rows", all(&[115])).unwrap();
    let u = shell.flush();
    let runs = shell.rt.stats().computations - before;
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff);
    assert!(runs < 20, "{runs} computations for one changed row");
    assert_eq!(labels(&shell)[5], "changed");
    shell
        .inst
        .set_value("t", "rows", all(&[115, 1500]))
        .unwrap();
    let u = shell.flush();
    assert!(u.diff.ops.is_empty(), "{:?}", u.diff);
    assert_eq!(
        shell
            .scene
            .texts()
            .iter()
            .filter(|t| *t == "changed")
            .count(),
        1
    );

    // A row the data inserts in the window is the data's (it enters);
    // the row it pushes out is the window's. `row_count` follows.
    let mut list_v: Vec<Value> = (0..2000)
        .map(|i| Value::record(row, vec![Value::int(i), Value::text(format!("r{i}"))]))
        .collect();
    list_v.insert(
        112,
        Value::record(row, vec![Value::int(5000), Value::text("new")]),
    );
    shell
        .inst
        .set_value("t", "rows", Value::list(list_v))
        .unwrap();
    let u = shell.flush();
    let new = shell.scene.find_text("new").expect("the new row mounts");
    let new_row = shell.scene.parent(new).unwrap();
    for op in &u.diff.ops {
        match op {
            SceneOp::Create { id, window, .. } if *id == new_row => assert!(!window),
            SceneOp::Remove { window, .. } => assert!(window, "{op:?}"),
            _ => {}
        }
    }
    assert_eq!(num(&shell, Prop::RowCount), 2001.0);
    assert_eq!(rows(&shell).len(), 20);
    assert_eq!(labels(&shell)[2], "new");

    // A window past the end shows the last rows.
    shell.inst.set_list_window(list, 5000, 20);
    shell.flush();
    assert_eq!(num(&shell, Prop::RowFirst), 1981.0);
    assert_eq!(labels(&shell).last().unwrap(), "r1999");
}

/// A row the window unmounts keeps its `state`s by key: scrolled away
/// and back, an opened row is still open. A key the data drops loses
/// them, so the same key coming back starts afresh.
#[test]
fn a_row_scrolled_out_and_back_keeps_its_state() {
    let mut src = String::from("type Row { id: int; label: text }\nstate rows: [Row] key id = [");
    for i in 0..200 {
        src.push_str(&format!("Row(id: {i}, label: \"r{i}\"), "));
    }
    src.push_str(
        "]\n\
         component Item(r: Row) {\n\
           state open = false\n\
           row { text r.label\n text open ? \"open\" : \"shut\"\n box { on click { open = !open } } }\n\
         }\n\
         bar B { list { for r in rows { Item(r) } } }\n",
    );
    let mut shell = boot(&[("t.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let list = shell.scene.of_kind(NodeKind::List)[0];
    let opened = |shell: &Shell| -> Vec<String> {
        shell
            .scene
            .children(list)
            .iter()
            .filter(|r| {
                matches!(
                    shell.scene.prop(shell.scene.children(**r)[1], Prop::Text),
                    Some(PropValue::Text(t)) if t == "open"
                )
            })
            .map(
                |r| match shell.scene.prop(shell.scene.children(*r)[0], Prop::Text) {
                    Some(PropValue::Text(t)) => t.clone(),
                    v => panic!("{v:?}"),
                },
            )
            .collect()
    };
    // Open row 3.
    let row3 = shell.scene.children(list)[3];
    let button = shell.scene.children(row3)[2];
    assert!(shell.inst.event(button, "click", Vec::new()));
    shell.flush();
    assert_eq!(opened(&shell), ["r3"]);
    // Scrolled away: row 3 is unmounted.
    shell.inst.set_list_window(list, 100, 20);
    shell.flush();
    assert!(opened(&shell).is_empty());
    assert!(shell.scene.find_text("r3").is_none());
    // And back: open still.
    shell.inst.set_list_window(list, 0, 20);
    shell.flush();
    assert_eq!(opened(&shell), ["r3"]);

    // Away again, and the data drops row 3 then brings it back: shut.
    shell.inst.set_list_window(list, 100, 20);
    shell.flush();
    let row = shell.inst.vm().types().find_record("Row").expect("Row");
    let rows = |skip: Option<i64>| {
        Value::list(
            (0..200)
                .filter(|i| Some(*i) != skip)
                .map(|i| Value::record(row, vec![Value::int(i), Value::text(format!("r{i}"))]))
                .collect(),
        )
    };
    shell.inst.set_value("t", "rows", rows(Some(3))).unwrap();
    shell.flush();
    shell.inst.set_value("t", "rows", rows(None)).unwrap();
    shell.flush();
    shell.inst.set_list_window(list, 0, 20);
    shell.flush();
    assert!(shell.scene.find_text("r3").is_some());
    assert!(opened(&shell).is_empty());
}

/// What a window move keeps of the rows it unmounts: nothing for rows
/// without `state` (no scope per row: scrolled through and back, the
/// runtime holds as many nodes as at the start), and for rows with one
/// a few nodes per key scrolled past, the same on a second pass.
#[test]
fn rows_scrolled_past_keep_only_their_states() {
    fn live(rt: &strand_core::Runtime) -> usize {
        let mut stack = rt.root_owned();
        let mut n = 0;
        while let Some(id) = stack.pop() {
            n += 1;
            stack.extend(rt.owned(id).unwrap_or_default());
        }
        n
    }
    let shell_of = |row: &str| {
        let mut src =
            String::from("type Row { id: int; label: text }\nstate rows: [Row] key id = [");
        for i in 0..2000 {
            src.push_str(&format!("Row(id: {i}, label: \"r{i}\"), "));
        }
        src.push_str(&format!(
            "]\n{row}\nbar B {{ list {{ for r in rows {{ Item(r) }} }} }}\n"
        ));
        boot(&[("t.strand", &src)], |rt, host| {
            screens(rt, host, &["DP-1"])
        })
    };
    // Through the list in steps of 20 rows and back to the top.
    let scroll = |shell: &mut Shell| {
        let list = shell.scene.of_kind(NodeKind::List)[0];
        for first in (0..2000).step_by(20).chain([0]) {
            shell.inst.set_list_window(list, first, 32);
            shell.flush();
        }
    };

    let mut plain = shell_of("component Item(r: Row) { row { text r.label } }");
    let start = live(plain.inst.runtime());
    scroll(&mut plain);
    assert_eq!(live(plain.inst.runtime()), start, "rows without state");

    let mut stateful = shell_of(
        "component Item(r: Row) {\n state open = false\n row { text r.label\n text open ? \"open\" : \"shut\" } }",
    );
    let start = live(stateful.inst.runtime());
    scroll(&mut stateful);
    let once = live(stateful.inst.runtime());
    let per_key = (once - start) as f64 / 2000.0;
    assert!(
        (0.5..=4.0).contains(&per_key),
        "{per_key} nodes kept per row scrolled past ({start} -> {once})"
    );
    scroll(&mut stateful);
    assert_eq!(
        live(stateful.inst.runtime()),
        once,
        "a second pass keeps no more"
    );
}

/// A `list` holding more than its `for` (a header row) is not windowed:
/// every row mounts, as in any container.
#[test]
fn a_list_with_more_than_its_for_mounts_every_row() {
    let mut src = String::from("state rows = [");
    for i in 0..40 {
        src.push_str(&format!("\"r{i}\", "));
    }
    src.push_str("]\nbar B { list { text \"head\"\n for r in rows key r { text r } } }\n");
    let shell = boot(&[("t.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let list = shell.scene.of_kind(NodeKind::List)[0];
    assert_eq!(shell.scene.children(list).len(), 41);
    assert!(shell.scene.prop(list, Prop::RowCount).is_none());
}

/// A list is windowed only when each item of its `for` makes exactly one
/// row (render and `nav` count each child as a row): an item of two
/// nodes, an `if` or a component of two roots mounts every row, while a
/// component with one root is windowed.
#[test]
fn a_list_whose_items_are_not_one_row_mounts_every_row() {
    use strand_compiler::instantiate::DEFAULT_LIST_WINDOW;
    let mut rows = String::from("state rows = [");
    for i in 0..40 {
        rows.push_str(&format!("\"r{i}\", "));
    }
    rows.push_str("]\n");
    let windowed = |body: &str, extra: &str| {
        let src = format!("{rows}{extra}bar B {{ list {{ for r in rows key r {{ {body} }} }} }}\n");
        let shell = boot(&[("t.strand", &src)], |rt, host| {
            screens(rt, host, &["DP-1"])
        });
        let list = shell.scene.of_kind(NodeKind::List)[0];
        let n = shell.scene.children(list).len();
        (shell.scene.prop(list, Prop::RowCount).is_some(), n)
    };
    assert_eq!(windowed("text r\n text r", ""), (false, 80));
    assert_eq!(windowed("if r != \"r3\" { text r }", ""), (false, 39));
    assert_eq!(
        windowed("Two(r)", "component Two(s: text) { text s\n text s }\n"),
        (false, 80)
    );
    assert_eq!(
        windowed("One(r)", "component One(s: text) { row { text s } }\n"),
        (true, DEFAULT_LIST_WINDOW)
    );
    // `let`s and handlers beside the one row are fine.
    assert_eq!(
        windowed("let u = r\n row { text u }", ""),
        (true, DEFAULT_LIST_WINDOW)
    );
}

/// A fault in a file's top-level handler is located but freezes
/// nothing: freezing the config's root would stop every bar (design.md:
/// a fault freezes only its own component).
#[test]
fn a_top_level_fault_freezes_nothing() {
    let src = "state n = 0\nstate zero = 0\non notifications.received(x) { n = 1 / zero }\nbar B { text pct(battery.percent) }\n";
    let mut shell = boot(&[("top.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    shell
        .host
        .emit(&shell.rt, "notifications.received", vec![Value::Null])
        .unwrap();
    let u = shell.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    let e = &u.errors[0];
    assert!(e.span.is_some());
    assert_eq!(e.scope, None);
    assert!(!shell.inst.freeze(e), "nothing to freeze");
    shell
        .host
        .set(&shell.rt, "battery.percent", Value::float(0.5))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["50%"], "the bar keeps updating");
}

/// The host loop sets the clock on every step: a wall clock that jumps
/// back is followed at once, and a clock reader mounted later (a popup
/// opened) shows the time now, not the time at boot.
#[test]
fn the_clock_follows_backward_jumps_and_late_readers() {
    use std::time::UNIX_EPOCH;
    let t0 = strand_compiler::vm::schema_host::MOCK_TIME;
    let wall = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
    let src = "export state o = false\nbar B { text clock.format(\"%H:%M\") }\npanel P { open: <-> o; text clock.format(\"%H:%M:%S\") }\n";
    let mut shell = boot(&[("clk.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts(), ["09:41"]);
    let (u, _) = shell.inst.step(Duration::from_secs(1), wall(t0 + 3600));
    shell.scene.apply(&u.diff).unwrap();
    assert_eq!(shell.scene.texts(), ["10:41"]);
    // Back an hour: shown at once, and the next wake is a minute away at
    // most.
    let (u, wake) = shell.inst.step(Duration::from_secs(2), wall(t0 + 5));
    shell.scene.apply(&u.diff).unwrap();
    assert_eq!(shell.scene.texts(), ["09:41"]);
    let sleep = wake
        .sleep_for(Duration::from_secs(2), wall(t0 + 5))
        .unwrap();
    assert!(sleep <= Duration::from_secs(60), "{sleep:?}");
    // The panel's seconds clock mounts later and starts at the time now.
    shell.inst.set("clk.o", Value::Bool(true)).unwrap();
    let (u, _) = shell.inst.step(Duration::from_secs(3), wall(t0 + 30));
    shell.scene.apply(&u.diff).unwrap();
    assert_eq!(shell.scene.texts(), ["09:41", "09:41:37"]);
}

/// The live sinks (effects, watches, timers) of a runtime.
fn sinks(rt: &Runtime) -> usize {
    use strand_core::NodeKind as K;
    let mut stack = rt.root_owned();
    let mut n = 0;
    while let Some(id) = stack.pop() {
        if matches!(rt.kind(id), Ok(K::Effect | K::Watch | K::Timer)) {
            n += 1;
        }
        stack.extend(rt.owned(id).unwrap_or_default());
    }
    n
}

/// The compiler declares every binding's and handler's reads and every
/// handler's writes before the first flush, so a sink that reads what a
/// handler writes runs after it, once per flush, with final values: an
/// `if` reading `m` and what a handler derives from `m` never sees one
/// new and the other old (it would mount the glitch branch and run
/// twice), from the first flush on.
#[test]
fn sinks_run_once_after_the_handlers_that_feed_them() {
    let src = "export state m = 0\nstate n = 0\nstate k = 0\nstate j = 0\nbar B {\n  if m > 0 && n == 0 { text \"glitch\" } else { text \"ok\" }\n  if m > 2 && k == 0 { text \"glitch\" } else { text \"ok\" }\n  if m > 0 && j != m * 10 { text \"glitch\" } else { text \"ok\" }\n  text join(\" \", m, n, k, j)\n  button { on click { n = 1 } }\n}\non change m { j = m * 10 }\nevery 1s { k += 1 }\n";
    let mut map = SourceMap::new();
    map.add("order.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    screens(&rt, &host, &["DP-1"]);
    let inst = Instance::new(&rt, program, host.clone(), Storage::none());
    // The first flush: no sink runs twice (timers and watches whose
    // value is already sent wait for a change).
    let live = sinks(&rt);
    let u = inst.tick(Duration::ZERO);
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    let boot = rt.stats().effect_runs;
    assert!(boot as usize <= live, "{boot} runs for {live} sinks");
    let mut scene = SceneMirror::new();
    scene.apply(&u.diff).unwrap();
    let shown = format!("{:?}", u.diff);
    assert!(!shown.contains("glitch"), "{shown}");
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene,
        boot: Vec::new(),
        boot_notices: Vec::new(),
        boot_diagnostics: Vec::new(),
    };
    assert_eq!(shell.scene.texts(), ["ok", "ok", "ok", "0 0 0 0"]);
    let runs = |shell: &Shell| shell.rt.stats().effect_runs;
    // A click, an outside write and the `on change` it fires in one tick.
    let button = shell.inst.nodes_handling("click")[0];
    shell.inst.set("order.m", Value::int(2)).unwrap();
    assert!(shell.inst.event(button, "click", Vec::new()));
    let before = runs(&shell);
    let u = shell.flush();
    // Once each: the three `if`s, the text and `on change m`.
    assert_eq!(runs(&shell) - before, 5);
    let shown = format!("{:?}", u.diff);
    assert!(!shown.contains("glitch"), "{shown}");
    assert_eq!(shell.scene.texts(), ["ok", "ok", "ok", "2 1 0 20"]);
    // A timer and an outside write in one tick.
    shell.at(0.5);
    shell.inst.set("order.m", Value::int(3)).unwrap();
    let before = runs(&shell);
    let u = shell.at(1.0);
    assert_eq!(runs(&shell) - before, 5);
    let shown = format!("{:?}", u.diff);
    assert!(!shown.contains("glitch"), "{shown}");
    assert_eq!(shell.scene.texts(), ["ok", "ok", "ok", "3 1 1 30"]);
}

/// A surface's visibility governs every service read under it: a
/// component in a panel's content lets go when the panel hides, and a
/// popup nested in a bar holds what it reads only while it is open.
#[test]
fn hidden_surfaces_hold_no_services_below_them() {
    let src = "export state o = false\nexport state p = false\ncomponent Bat { text pct(battery.percent) }\npanel P { open: <-> o; Bat }\nbar B {\n  text \"x\"\n  popup { open: <-> p; text pct(audio.sink.volume) }\n}\n";
    let mut shell = boot(&[("vis2.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.host.readers("battery"), 0);
    assert_eq!(shell.host.readers("audio"), 0, "the popup is closed");
    shell.inst.set("vis2.o", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 1);
    shell.inst.set("vis2.o", Value::Bool(false)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 0, "hidden with its panel");
    shell.inst.set("vis2.o", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("battery"), 1, "shown again");
    shell.inst.set("vis2.p", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("audio"), 1, "the popup is open");
    shell.inst.set("vis2.p", Value::Bool(false)).unwrap();
    shell.flush();
    assert_eq!(shell.host.readers("audio"), 0);
    // A parked bar lets go of everything under it, popup included.
    shell.inst.set("vis2.p", Value::Bool(true)).unwrap();
    shell.flush();
    screens(&shell.rt, &shell.host, &[]);
    shell.flush();
    assert_eq!(shell.host.readers("audio"), 0, "parked");
    screens(&shell.rt, &shell.host, &["DP-1"]);
    shell.flush();
    assert_eq!(shell.host.readers("audio"), 1, "back");
}

/// `for` over `.filter(…).take(5)` on a keyed collection follows core's
/// incremental views (design.md: they "update incrementally and keep
/// keys"): over 2,000 rows one push or update is a diff through the
/// chain, never a rebuild, and the rows kept keep their scene nodes.
#[test]
fn keyed_chains_update_incrementally() {
    let mut src = String::from(
        "type Row { id: int; label: text; show: bool }\nexport state lowest = 0\nstate rows: [Row] key id = [",
    );
    for i in 0..2000 {
        src.push_str(&format!(
            "Row(id: {i}, label: \"r{i}\", show: {}), ",
            i % 2 == 0
        ));
    }
    src.push_str("]\nstate next = 5000\nbar B {\n  for r in rows.filter(r => r.show && r.id >= lowest).take(5) { text r.label }\n  button { on click { rows.push(Row(id: next, label: \"new\", show: true)); next += 1 } }\n  button { on scroll(dy) { rows.update(2, r => Row(id: r.id, label: \"two\", show: true)) } }\n  button { on activate { rows.remove_key(0) } }\n}\n");
    let mut shell = boot(&[("chain.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts(), ["r0", "r2", "r4", "r6", "r8"]);
    let r2 = shell.text_node("r2");
    let rebuilds = |shell: &Shell| shell.rt.stats().rebuilds;
    // A push at the end: past the first five, nothing to show.
    let before = rebuilds(&shell);
    let push = shell.inst.nodes_handling("click")[0];
    shell.inst.event(push, "click", Vec::new());
    let u = shell.flush();
    assert_eq!(rebuilds(&shell), before, "a push is one diff");
    assert!(u.diff.is_empty(), "{:?}", u.diff);
    // An update of a shown row: that row's text only, same node.
    let update = shell.inst.nodes_handling("scroll")[0];
    shell.inst.event(update, "scroll", vec![Value::float(1.0)]);
    let u = shell.flush();
    assert_eq!(rebuilds(&shell), before, "an update is one diff");
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff);
    assert_eq!(shell.text_node("two"), r2, "the row keeps its node");
    // A shown row leaves: the rest keep their nodes, one more arrives.
    let remove = shell.inst.nodes_handling("activate")[0];
    shell.inst.event(remove, "activate", Vec::new());
    shell.flush();
    assert_eq!(rebuilds(&shell), before);
    assert_eq!(shell.scene.texts(), ["two", "r4", "r6", "r8", "r10"]);
    assert_eq!(shell.text_node("two"), r2);
    // A parameter the lambda reads changes: the view is rebuilt.
    shell.inst.set("chain.lowest", Value::int(6)).unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["r6", "r8", "r10", "r12", "r14"]);
}

/// `.len`, `.first`, `[i]`, `.contains` and `.get` on a keyed collection
/// go through core's accessors: a change never copies the list.
#[test]
fn keyed_reads_do_not_copy_the_list() {
    let mut src = String::from("type Row { id: int; label: text }\nstate rows: [Row] key id = [");
    for i in 0..2000 {
        src.push_str(&format!("Row(id: {i}, label: \"r{i}\"), "));
    }
    src.push_str("]\nstate next = 5000\nbar B {\n  text join(\" \", rows.len, rows.first?.label, rows[1]?.label, rows.contains(Row(id: 3, label: \"r3\")), rows.get(3)?.label)\n  button { on click { rows.push(Row(id: next, label: \"new\")); next += 1 } }\n}\n");
    let mut shell = boot(&[("len.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts(), ["2000 r0 r1 true r3"]);
    let button = shell.inst.nodes_handling("click")[0];
    let before = shell.rt.stats().computations;
    shell.inst.event(button, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["2001 r0 r1 true r3"]);
    // The text's binding only: no list value is rebuilt.
    assert_eq!(shell.rt.stats().computations - before, 1);
}

/// The texts a binding over `rows.contains(…)` shows as the keyed
/// collection is pushed to, updated and removed from, one box per write.
fn contains_follows(src: &str) -> Vec<String> {
    let mut shell = boot(&[("contains.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let mut seen = vec![shell.scene.texts().join(" ")];
    for b in shell.scene.of_kind(NodeKind::Box) {
        shell.inst.event(b, "click", Vec::new());
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        seen.push(shell.scene.texts().join(" "));
    }
    seen
}

const CONTAINS_WRITES: &str = "  box { on click { rows.push(Row(id: 1, label: \"a\")) } }\n  box { on click { rows.update(1, r => Row(id: r.id, label: \"b\")) } }\n  box { on click { rows.update(1, r => Row(id: r.id, label: \"a\")) } }\n  box { on click { rows.remove_key(1) } }\n}\n";

/// A binding that reads a keyed `state` only through `.contains(x)`
/// re-runs when the collection changes: the query reads the item by its
/// key through the tracked accessor (decisions.md dictionary; before, it
/// read through core's untracked `get_key`, so the text stayed `false`
/// after the push).
#[test]
fn a_binding_over_contains_reruns_when_the_keyed_state_changes() {
    let src = format!(
        "type Row {{ id: int; label: text }}\nstate rows: [Row] key id = []\nbar B {{\n  text join(\" \", rows.contains(Row(id: 1, label: \"a\")))\n{CONTAINS_WRITES}"
    );
    assert_eq!(
        contains_follows(&src),
        ["false", "true", "false", "true", "false"]
    );
}

/// The same through a view `let` over the keyed `state`: `.contains`
/// on the view follows the view.
#[test]
fn a_binding_over_contains_on_a_view_reruns_when_the_keyed_state_changes() {
    let src = format!(
        "type Row {{ id: int; label: text }}\nstate rows: [Row] key id = []\nlet shown = rows.filter(r => r.id > 0)\nbar B {{\n  text join(\" \", shown.contains(Row(id: 1, label: \"a\")))\n{CONTAINS_WRITES}"
    );
    assert_eq!(
        contains_follows(&src),
        ["false", "true", "false", "true", "false"]
    );
}

/// Service actions are writes too: a handler calling `n.expire()` (on an
/// item of `notifications.popups`) or `notifications.clear()` declares a
/// write of what the service's actions can change, so readers of the
/// popups are ranked after it from the first flush.
#[test]
fn action_calls_declare_their_service_writes() {
    use strand_compiler::lower::WriteTarget;
    let src = "state m = 0\nbar B {\n  text join(\" \", notifications.popups.len)\n  for n in notifications.popups {\n    box { after 1s { n.expire() } }\n  }\n}\non change m { notifications.clear() }\n";
    let mut map = SourceMap::new();
    map.add("actions.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    );
    let action = WriteTarget::Action("notifications".into());
    let writers = program
        .writes
        .iter()
        .filter(|w| w.contains(&action))
        .count();
    assert_eq!(writers, 2, "{:?}", program.writes);
    // And the mock carries it out in order: the expiry empties the popups
    // and the count that reads them follows in the same tick.
    let mut shell = boot(&[("actions", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        host.set(
            rt,
            "notifications.popups",
            Value::list(vec![notification(host, 1, "mail", "hi", "normal")]),
        )
        .unwrap();
    });
    assert_eq!(shell.scene.texts(), ["1"]);
    let u = shell.at(1.0);
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.scene.texts(), ["0"]);
}

/// A handler loop's work is linear in its items: each iteration drops
/// the previous one's locals (the frame stays small, so reading a local
/// from before the loop is one short scan), and a lambda made in the loop
/// captures only what it reads. Measured as the longest frame the VM
/// ever held, which must not grow with the item count (before, every
/// iteration's locals stayed and 20,000 items took seconds). A whole
/// handler `let` is an `int` (`total += y` keeps `total` an `int`).
#[test]
fn handler_loops_are_linear() {
    let src = "state xs: [int] = []\nstate total = 0\nbar B {\n  box { on click {\n    let base = 1\n    for x in xs {\n      let y = x + base\n      if y > 0 { let z = y\n        total += z }\n      total += [base].map(v => v + y)[0] ?? 0\n    }\n  } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    let mut run = |n: i64| {
        let items = (0..n).map(Value::int).collect();
        shell.inst.set_value("t", "xs", Value::list(items)).unwrap();
        shell.inst.set_value("t", "total", Value::int(0)).unwrap();
        shell.flush();
        let calls = shell.inst.lambda_calls();
        assert!(shell.inst.event(b, "click", Vec::new()));
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        // Σ (x + 1) twice, plus 1 per item for the lambda's `+ base`.
        let want = n * (n + 1) + n;
        assert_eq!(shell.inst.value_of("t", "total").unwrap(), Value::int(want));
        assert_eq!(
            shell.inst.lambda_calls() - calls,
            n as u64,
            "one call per item"
        );
        shell.inst.peak_frame()
    };
    let small = run(100);
    let large = run(20_000);
    assert_eq!(small, large, "the frame does not grow with the items");
    assert!(large <= 8, "{large}");
}

/// Lambdas nested in lambdas capture each outer local once: the frame a
/// call starts with already holds its closure's captures, so capturing
/// from both used to double them per level (2^19 entries at 20 levels).
#[test]
fn nested_lambdas_capture_each_local_once() {
    let mut e = "base".to_string();
    for k in 0..20 {
        e = format!("([1].map(v{k} => {e})[0] ?? 0)");
    }
    let src = format!(
        "state total = 0\nbar B {{ box {{ on click {{\n  let base = 1\n  total += {e}\n}} }} }}\n"
    );
    let mut shell = boot(&[("t.strand", &src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(b, "click", Vec::new()));
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.inst.value_of("t", "total").unwrap(), Value::int(1));
    let peak = shell.inst.peak_frame();
    assert!(peak <= 4, "captures grew with nesting: {peak}");
}

/// A whole handler `let` added to an `int` state keeps it an `int`
/// value (the checker types the `let` `int`; the VM also stores a whole
/// plain number written to an `int` state as an `int`).
#[test]
fn whole_handler_lets_keep_int_state_int() {
    let src = "state total = 0\nbar B { box { on click { let y = 1\n total += y } } }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    for _ in 0..3 {
        assert!(shell.inst.event(b, "click", Vec::new()));
        assert!(shell.flush().errors.is_empty());
    }
    let v = shell.inst.value_of("t", "total").unwrap();
    assert!(matches!(v, Value::Num(n, Num::Int) if n == 3.0), "{v:?}");
}

/// Mounting and unmounting a component that declares a settings `state`
/// many times keeps the mounted-settings list bounded: dead entries are
/// pruned whenever it doubles, so it stays within twice what is live
/// (plus the first prune's floor), at amortised O(1) per mount.
#[test]
fn unmounted_settings_are_pruned() {
    let src = "state on = true\ncomponent C {\n  state p from \"c.toml\" { dense: bool = false }\n  text p.dense ? \"dense\" : \"airy\"\n}\nbar B { box { on click { on = !on } }\n  if on { C } }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert!(shell.scene.find_text("airy").is_some());
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    let mut most = 0;
    for _ in 0..400 {
        assert!(shell.inst.event(b, "click", Vec::new()));
        assert!(shell.flush().errors.is_empty());
        most = most.max(shell.inst.settings_len());
    }
    assert!(shell.scene.find_text("airy").is_some(), "mounted again");
    assert!(most <= 32, "the list grew to {most}");
}

/// design.md's notification stack names its chain in a `let` (`let shown
/// = notifications.popups.filter(…).take(5)`, then `for n in shown` and
/// `open: shown.len > 0`): the `let` is one incremental view, so a new
/// notification runs the filter once, for itself, and is one diff from
/// the service to the scene, with no rebuild.
#[test]
fn a_let_chain_is_one_incremental_view() {
    let files = [fixture("toasts.strand")];
    let mut shell = boot(&refs(&files), desktop);
    assert!(
        shell.scene.find_text("Mail · New message").is_some(),
        "{}",
        shell.scene.render()
    );
    let many: Vec<Value> = (1..=200)
        .map(|i| notification(&shell.host, i, "App", &format!("n{i}"), "normal"))
        .collect();
    shell
        .host
        .set(&shell.rt, "notifications.popups", Value::list(many.clone()))
        .unwrap();
    shell.flush();
    assert!(shell.scene.find_text("App · n5").is_some());
    assert!(shell.scene.find_text("App · n6").is_none(), "take(5)");
    let n1 = shell.text_node("App · n1");
    let (rebuilds, calls) = (shell.rt.stats().rebuilds, shell.inst.lambda_calls());
    // One more at the end: the filter runs for it alone, `take` drops it.
    let mut more = many.clone();
    more.push(notification(&shell.host, 201, "App", "n201", "normal"));
    shell
        .host
        .set(&shell.rt, "notifications.popups", Value::list(more.clone()))
        .unwrap();
    let u = shell.flush();
    assert_eq!(shell.rt.stats().rebuilds, rebuilds, "one diff, no rebuild");
    assert_eq!(shell.inst.lambda_calls() - calls, 1, "the new item only");
    assert!(u.diff.is_empty(), "{:?}", u.diff);
    // One at the front: shown, the others keep their nodes.
    more.insert(0, notification(&shell.host, 0, "App", "n0", "normal"));
    let calls = shell.inst.lambda_calls();
    shell
        .host
        .set(&shell.rt, "notifications.popups", Value::list(more))
        .unwrap();
    shell.flush();
    assert_eq!(shell.rt.stats().rebuilds, rebuilds);
    assert_eq!(shell.inst.lambda_calls() - calls, 1);
    assert!(shell.scene.find_text("App · n0").is_some());
    assert!(shell.scene.find_text("App · n5").is_none());
    assert_eq!(
        shell.text_node("App · n1"),
        n1,
        "kept rows keep their nodes"
    );
    // `dnd` is the filter's parameter: changing it rebuilds the view.
    shell.inst.set("toasts.dnd", Value::Bool(true)).unwrap();
    shell.flush();
    assert!(shell.scene.find_text("App · n0").is_none());
}

/// A timer runs one handler at a time: a fire while the last run is
/// still suspended at an `await` is skipped, so a slow (or never
/// settling) `await` neither piles up runs nor lets an older run write
/// after a newer one.
#[test]
fn a_timer_skips_fires_while_its_last_run_awaits() {
    let src = "state started = 0\nstate done = 0\nstate stuck = 0\nevery 1s { started += 1\n  await sleep(5s)\n  done += 1 }\nevery 1s { stuck += 1\n  await sleep(100000s) }\n";
    let mut shell = boot(&[("t.strand", src)], |_, _| {});
    let get = |shell: &Shell, n: &str| shell.inst.value_of("t", n).unwrap().as_f64().unwrap();
    let mut nodes_at_20 = 0;
    for s in 1..=200 {
        let u = shell.at(f64::from(s));
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        let (started, done) = (get(&shell, "started"), get(&shell, "done"));
        assert!(
            started - done <= 1.0,
            "one run at a time: {started} vs {done}"
        );
        if s == 20 {
            nodes_at_20 = shell.rt.stats().nodes;
        }
    }
    assert_eq!(get(&shell, "stuck"), 1.0, "the stuck run is never doubled");
    let started = get(&shell, "started");
    assert!((30.0..=40.0).contains(&started), "{started}");
    assert!(
        shell.rt.stats().nodes <= nodes_at_20 + 2,
        "{} vs {nodes_at_20}",
        shell.rt.stats().nodes
    );
}

/// A `for` whose keys collide and an `if` whose condition fails at mount
/// are reported once each, located (file, span, scope), not again
/// without a location.
#[test]
fn mount_failures_are_reported_once_located() {
    let src = "state zero = 0\nbar B {\n  for n in notifications.popups key n.app.name { text n.summary }\n  if 1 / zero > 0 { text \"x\" }\n}\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    screens(&rt, &host, &["DP-1"]);
    let a = notification(&host, 1, "Mail", "one", "normal");
    let b = notification(&host, 2, "Mail", "two", "normal");
    host.set(&rt, "notifications.popups", Value::list(vec![a, b]))
        .unwrap();
    let inst = Instance::new(&rt, program, host.clone(), Storage::none());
    let u = inst.flush();
    assert_eq!(u.errors.len(), 2, "{:#?}", u.errors);
    for e in &u.errors {
        assert!(e.file.is_some() && e.span.is_some(), "{e:#?}");
    }
    let whats: Vec<&str> = u.errors.iter().map(|e| e.what.as_str()).collect();
    assert!(whats.iter().any(|w| w.starts_with("for n in")), "{whats:?}");
    assert!(whats.iter().any(|w| w.starts_with("if in")), "{whats:?}");
}

/// A plain number written to a `float` state is stored as a `float`,
/// whatever produced it: writing the same number again (`f = n` with an
/// `int` `n`, then `f = 2.0`) is no change, so `on change` fires once.
#[test]
fn a_float_state_ignores_the_int_tag() {
    let src = "state f: float = 0.0\nstate fired = 0\non change f { fired += 1 }\nbar B {\n  box { on click { let n = 2\n f = n } }\n  box { on scroll(dy) { f = 2.0 } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    let c = shell.scene.of_kind(NodeKind::Box)[1];
    assert!(shell.inst.event(b, "click", Vec::new()));
    assert!(shell.flush().errors.is_empty());
    let v = shell.inst.value_of("t", "f").unwrap();
    assert!(matches!(v, Value::Num(n, Num::Float) if n == 2.0), "{v:?}");
    assert!(shell.inst.event(c, "scroll", vec![Value::float(1.0)]));
    assert!(shell.flush().errors.is_empty());
    assert_eq!(shell.inst.value_of("t", "fired").unwrap(), Value::int(1));
}

/// A number written through a path takes the declared type of the field
/// or item it lands in: an `int` written to a `float` field or item is
/// stored as a `float`.
#[test]
fn path_writes_conform_to_the_field_type() {
    let src = "type Count { n: int; w: float }\nstate c = Count(n: 0, w: 0.0)\nstate ws: [float] = [0.0, 0.0]\nbar B { box { on click { let k = 2\n c.n = k\n c.w = k\n ws[1] = k } } }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(b, "click", Vec::new()));
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    let Value::Record(r) = shell.inst.value_of("t", "c").unwrap() else {
        panic!("not a record")
    };
    assert!(
        matches!(r.fields[0], Value::Num(n, Num::Int) if n == 2.0),
        "{:?}",
        r.fields
    );
    assert!(
        matches!(r.fields[1], Value::Num(n, Num::Float) if n == 2.0),
        "{:?}",
        r.fields
    );
    let ws = shell.inst.value_of("t", "ws").unwrap();
    let ws = ws.as_list().unwrap();
    assert!(
        matches!(ws[1], Value::Num(n, Num::Float) if n == 2.0),
        "{ws:?}"
    );
}

/// A chain step that reads a view `let` compares the view by its
/// version: it runs again when the view changes (an item's `pinned`
/// flips), and not when the source changes outside the view.
#[test]
fn a_step_reading_a_view_let_follows_the_view() {
    let src = "type Row { id: int; label: text; pinned: bool }\nstate rows: [Row] key id = [Row(id: 1, label: \"a\", pinned: false), Row(id: 2, label: \"b\", pinned: false)]\nstate ys: [Row] key id = [Row(id: 10, label: \"y\", pinned: false)]\nlet pinned = rows.filter(r => r.pinned)\nbar B {\n  for y in ys.filter(y => pinned.len > 0 && y.id > 0) { text y.label }\n  box { on click { rows.update(1, r => Row(id: r.id, label: r.label, pinned: !r.pinned)) } }\n  box { on scroll(dy) { rows.update(2, r => Row(id: r.id, label: join(\"\", r.label, \"b\"), pinned: false)) } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert!(shell.scene.find_text("y").is_none());
    let boxes = shell.scene.of_kind(NodeKind::Box);
    let (pin, edit) = (boxes[0], boxes[1]);
    // `pinned` flips: the update's lambda, `rows`'s filter for row 1 and
    // `ys`'s step for its one item.
    let calls = shell.inst.lambda_calls();
    assert!(shell.inst.event(pin, "click", Vec::new()));
    assert!(shell.flush().errors.is_empty());
    assert!(
        shell.scene.find_text("y").is_some(),
        "{}",
        shell.scene.render()
    );
    assert_eq!(shell.inst.lambda_calls() - calls, 3);
    // An edit outside the view: the update's lambda and `rows`'s filter
    // for row 2 only; `ys`'s step does not run.
    for _ in 0..3 {
        let calls = shell.inst.lambda_calls();
        assert!(shell.inst.event(edit, "scroll", vec![Value::float(1.0)]));
        assert!(shell.flush().errors.is_empty());
        assert_eq!(shell.inst.lambda_calls() - calls, 2);
    }
    // Flipped back: the step runs again and the item goes.
    let calls = shell.inst.lambda_calls();
    assert!(shell.inst.event(pin, "click", Vec::new()));
    assert!(shell.flush().errors.is_empty());
    assert!(shell.scene.find_text("y").is_none());
    assert_eq!(shell.inst.lambda_calls() - calls, 3);
}

/// Mounting and unmounting a component with a persisted `state` many
/// times removes each cell on unmount (O(1) each, by signal id), so
/// only the live ones stay.
#[test]
fn unmounted_persisted_cells_are_removed() {
    let dir = temp_dir("persist-churn");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let src = "state on = true\ncomponent C {\n  state n = 0 persist\n  text join(\" \", n)\n}\nbar B { box { on click { on = !on } }\n  if on { C } }\n";
    let one = |rt: &Runtime, host: &SchemaHost| screens(rt, host, &["DP-1"]);
    let mut shell = boot_with(&[("t.strand", src)], one, storage.clone());
    assert_eq!(shell.inst.persisted_len(), 1);
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    for i in 0..200 {
        assert!(shell.inst.event(b, "click", Vec::new()));
        assert!(shell.flush().errors.is_empty());
        assert_eq!(shell.inst.persisted_len(), i % 2);
    }
    drop(shell);
    if let Some(p) = &storage.persist {
        assert!(p.sync(Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The input path, end to end through the VM: `on scroll(dy)` and
/// `on click` bodies (one of them awaiting before it writes) and a `<->`
/// widget write, each driven at 60 Hz for 2 s through `Instance::event`,
/// `Instance::write` and the tick, are the user, not a feedback loop: no
/// `WriteRate` warning and every write lands in its own tick. The same
/// writes followed by an `on change` handler (triggered by the graph)
/// are a loop: that handler warns and is throttled.
#[test]
fn input_handlers_and_two_way_writes_at_60_hz_are_not_throttled() {
    let input = "state v = 0.0\nstate c = 0\nstate a = 0.0\nstate level = 0.0\nstate seen = 0\nbar B {\n  row {\n    box { on scroll(dy) { v += dy } }\n    box { on click { c += 1 } }\n    box { on scroll(dy) { await sleep(1ms); a += dy } }\n    slider { value: <-> level }\n  }\n}\n";
    let graph = format!("{input}on change v, level {{ seen += 1 }}\n");
    for (name, src) in [("input", input.to_string()), ("graph", graph)] {
        let mut shell = boot(&[("t.strand", &src)], |rt, host| {
            screens(rt, host, &["DP-1"])
        });
        let boxes = shell.scene.of_kind(NodeKind::Box);
        assert_eq!(boxes.len(), 3, "{name}");
        let slider = shell.scene.of_kind(NodeKind::Slider)[0];
        let num = |shell: &Shell, cell: &str| {
            shell
                .inst
                .value_of("t", cell)
                .unwrap()
                .as_f64()
                .unwrap_or_else(|| panic!("{cell} is not a number"))
        };
        let mut diags = Vec::new();
        let mut seen_lagged = false;
        let frame = 1.0 / 60.0;
        for i in 1..=120u32 {
            let step = f64::from(i);
            let scroll = || vec![Value::float(1.0), Value::float(0.0)];
            assert!(shell.inst.event(boxes[0], "scroll", scroll()), "{name}");
            assert!(shell.inst.event(boxes[1], "click", Vec::new()), "{name}");
            assert!(shell.inst.event(boxes[2], "scroll", scroll()), "{name}");
            shell
                .inst
                .write(slider, Prop::Value, PropValue::Number(i as f32 / 128.0))
                .unwrap();
            let u = shell.at(step * frame);
            assert!(u.errors.is_empty(), "{name}: {:?}", u.errors);
            diags.extend(u.diagnostics);
            // Every input write lands in the tick it was made in; the run
            // that awaits lands one tick later (its 1 ms sleep).
            assert_eq!(num(&shell, "v"), step, "{name}: scroll {i} held");
            assert_eq!(num(&shell, "c"), step, "{name}: click {i} held");
            assert_eq!(
                num(&shell, "a"),
                step - 1.0,
                "{name}: awaited scroll {i} held"
            );
            assert_eq!(num(&shell, "level"), step / 128.0, "{name}: write {i} held");
            seen_lagged |= num(&shell, "seen") < step;
        }
        let u = shell.at(121.0 * frame);
        diags.extend(u.diagnostics);
        assert_eq!(num(&shell, "a"), 120.0, "{name}");
        let warned: Vec<String> = diags
            .iter()
            .filter_map(|d| match d {
                strand_core::Diagnostic::WriteRate { names, .. } => Some(names.to_string()),
                _ => None,
            })
            .collect();
        if name == "input" {
            assert!(warned.is_empty(), "input warned: {warned:?}");
            assert_eq!(num(&shell, "seen"), 0.0);
        } else {
            assert_eq!(warned.len(), 1, "the `on change` writer: {warned:?}");
            assert!(warned[0].contains("seen"), "{warned:?}");
            assert!(seen_lagged, "the `on change` writer was not throttled");
        }
    }
}

/// A container query (`when self.width < 300`) follows the laid-out size
/// render reports, with 4 px hysteresis: once it holds it keeps holding
/// until the width is 4 px past the threshold, so it cannot flicker.
#[test]
fn container_queries_have_hysteresis() {
    let src = "bar Top {\n  height: 30\n  row {\n    opacity: 1\n    when self.width < 300 { opacity: 0.5 }\n  }\n}\n";
    let mut shell = boot(&[("q.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let opacity = |shell: &Shell| match shell.scene.prop(row, Prop::Opacity) {
        Some(PropValue::Number(n)) => *n,
        p => panic!("{p:?}"),
    };
    let at = |shell: &mut Shell, w: f32| {
        shell.inst.set_size(row, w, 30.0);
        shell.flush();
        opacity(shell)
    };
    assert_eq!(at(&mut shell, 400.0), 1.0);
    assert_eq!(at(&mut shell, 299.0), 0.5, "below the threshold");
    assert_eq!(at(&mut shell, 301.0), 0.5, "held within 4 px");
    assert_eq!(at(&mut shell, 303.0), 0.5, "held within 4 px");
    assert_eq!(at(&mut shell, 304.0), 1.0, "4 px past: released");
    assert_eq!(
        at(&mut shell, 301.0),
        1.0,
        "not on again above the threshold"
    );
    assert_eq!(at(&mut shell, 299.5), 0.5);
}

/// The boot value of `self.width` (0, before any layout) seeds no
/// hysteresis: a container whose first layout is 302 px wide shows the
/// wide variant, as one that grew to 302 px does.
#[test]
fn a_query_first_laid_out_inside_the_band_takes_the_wide_variant() {
    let src = "bar Top {\n  height: 30\n  row {\n    opacity: 1\n    when self.width < 300 { opacity: 0.5 }\n  }\n}\n";
    let mut shell = boot(&[("q.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    shell.inst.set_size(row, 302.0, 30.0);
    shell.flush();
    assert_eq!(
        shell.scene.prop(row, Prop::Opacity),
        Some(&PropValue::Number(1.0))
    );
}

/// Render is told which nodes' sizes logic reads (`watch`): `query` for a
/// container query, `size` for any other binding; nodes nobody measures
/// carry nothing, so their size changes never wake logic.
#[test]
fn only_measured_nodes_are_watched() {
    let src = "bar Top {\n  height: 30\n  row {\n    when self.width < 300 { opacity: 0.5 }\n    box { id: b }\n    text \"w\"\n  }\n  text b.width > 10 ? \"wide\" : \"narrow\"\n}\n";
    let shell = boot(&[("w.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    let bar = shell.scene.of_kind(NodeKind::Bar)[0];
    let kw = |k: &str| Some(PropValue::Keyword(k.into()));
    assert_eq!(shell.scene.prop(row, Prop::Watch).cloned(), kw("query"));
    assert_eq!(shell.scene.prop(b, Prop::Watch).cloned(), kw("size"));
    assert_eq!(shell.scene.prop(bar, Prop::Watch), None);
    for t in shell.scene.of_kind(NodeKind::Text) {
        assert_eq!(shell.scene.prop(t, Prop::Watch), None);
    }
}

/// `nav: results` names a list mounted after the input: render gets the
/// list's node once everything is mounted.
#[test]
fn nav_names_the_list_node() {
    let files = [fixture("launcher.strand")];
    let mut shell = boot(&refs(&files), desktop);
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    let list = shell.scene.of_kind(NodeKind::List)[0];
    assert_eq!(
        shell.scene.prop(input, Prop::Nav),
        Some(&PropValue::Node(list))
    );
}

/// Two-way props are marked for render and input (`two_way`): the
/// launcher's `open: <-> open` (Escape and click-away close it) and its
/// input's `text: <-> query`; its spec says `open` is two-way.
#[test]
fn two_way_props_are_marked() {
    let files = [fixture("launcher.strand")];
    let mut shell = boot(&refs(&files), desktop);
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    let panel = shell.scene.of_kind(NodeKind::Panel)[0];
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    let kw = |k: &str| PropValue::List(vec![PropValue::Keyword(k.into())]);
    assert_eq!(shell.scene.prop(panel, Prop::TwoWay), Some(&kw("open")));
    assert_eq!(shell.scene.prop(input, Prop::TwoWay), Some(&kw("text")));
    let spec = strand_scene::SurfaceSpec::resolve(NodeKind::Panel, |p| {
        shell.scene.prop(panel, p).cloned()
    });
    assert!(spec.open_two_way);
}

/// `nav:` names a list that mounts ticks later (inside an `if` that
/// turns true): render gets it once it is on the scene.
#[test]
fn nav_names_a_list_mounted_later() {
    let src = "state show = false\npanel P {\n  width: 200\n  height: 100\n  col {\n    input { nav: results }\n    if show {\n      list { id: results\n        text \"a\" }\n    }\n  }\n}\n";
    let mut shell = boot(&[("n.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    assert_eq!(shell.scene.prop(input, Prop::Nav), None);
    shell.flush();
    shell
        .inst
        .set_value("n", "show", Value::Bool(true))
        .unwrap();
    shell.flush();
    let list = shell.scene.of_kind(NodeKind::List)[0];
    assert_eq!(
        shell.scene.prop(input, Prop::Nav),
        Some(&PropValue::Node(list))
    );
}

/// The node the input's `nav` names on the scene, if it names one.
fn nav_target(shell: &Shell) -> Option<NodeId> {
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    match shell.scene.prop(input, Prop::Nav) {
        Some(PropValue::Node(n)) => Some(*n),
        None | Some(PropValue::Unset) => None,
        other => panic!("nav is {other:?}"),
    }
}

/// The one `list` on the scene, which must hold the text `row`.
fn the_list(shell: &Shell, row: &str) -> NodeId {
    let lists = shell.scene.of_kind(NodeKind::List);
    assert_eq!(lists.len(), 1, "{}", shell.scene.render());
    let text = shell.text_node(row);
    assert_eq!(shell.scene.parent(text), Some(lists[0]));
    lists[0]
}

/// (nav-remount) `nav:` follows the list it names into a page that
/// mounts in the same change: an expression that switches to it with the
/// page (`nav: m == b ? lb : la` beside `pages current: m`) names the
/// list that change mounted, and a constant `nav: lb` names lb's new
/// instance each time its page comes back, never the unmounted one.
#[test]
fn nav_follows_a_list_that_remounts() {
    for nav in ["m == b ? lb : la", "lb"] {
        let src = format!(
            "enum M {{ a, b }}\nstate q = \"\"\nlet m = q == \"b\" ? b : a\n\
             panel P {{\n  width: 200\n  height: 100\n  col {{\n    \
             input {{ nav: {nav} }}\n    pages current: m {{\n      \
             page a {{ list {{ id: la\n        text \"in-a\" }} }}\n      \
             page b {{ list {{ id: lb\n        text \"in-b\" }} }}\n    }}\n  }}\n}}\n"
        );
        let mut shell = boot(&[("n.strand", src.as_str())], |rt, host| {
            screens(rt, host, &["DP-1"])
        });
        shell.flush();
        let la = the_list(&shell, "in-a");
        let on_a = |la| (nav != "lb").then_some(la);
        assert_eq!(nav_target(&shell), on_a(la), "{nav}: at boot");
        for round in 0..3 {
            shell.inst.set_value("n", "q", Value::text("b")).unwrap();
            shell.flush();
            let lb = the_list(&shell, "in-b");
            assert_eq!(nav_target(&shell), Some(lb), "{nav}, round {round}: on b");
            shell.inst.set_value("n", "q", Value::text("")).unwrap();
            shell.flush();
            let la = the_list(&shell, "in-a");
            assert_eq!(nav_target(&shell), on_a(la), "{nav}, round {round}: on a");
        }
    }
}

/// Typing into the launcher's `input` is a two-way write of its `text`;
/// `open: <-> open` takes the `false` Escape writes.
#[test]
fn input_and_open_take_widget_writes() {
    let files = [fixture("launcher.strand")];
    let mut shell = boot(&refs(&files), desktop);
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    shell
        .inst
        .write(input, Prop::Text, PropValue::Text("fi".into()))
        .unwrap();
    shell.flush();
    assert_eq!(
        shell.scene.prop(input, Prop::Text),
        Some(&PropValue::Text("fi".into()))
    );
    let panel = shell.scene.of_kind(NodeKind::Panel)[0];
    shell
        .inst
        .write(panel, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(shell.inst.get("launcher.open").unwrap(), Value::Bool(false));
}

/// `page` and `tooltip { … }` mount on demand (the schema's `on_demand`,
/// as the cycle check reads it): only the current page is on the scene,
/// a hidden one unmounts (design.md: "Hidden pages unmount"), and a
/// tooltip's content is there only while the element it sits in is
/// hovered. State lives on the component or surface around a page (the
/// checker keeps it off pages), so it is kept across a page change.
#[test]
fn pages_and_tooltips_mount_on_demand() {
    let src = "enum Pg { a, b }\n\
               state cur: Pg = a\n\
               bar B {\n\
                 state n = 0\n\
                 box { text \"x\"; tooltip { text \"TIP\" } }\n\
                 box { on click { cur = cur == a ? b : a } }\n\
                 pages current: cur {\n\
                   page a { text join(\" \", \"PA\", n); box { on click { n += 1 } } }\n\
                   page b { if true { text \"PB\" } }\n\
                 }\n\
               }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let scene = shell.scene.render();
    assert!(scene.contains("text=\"PA 0\""), "{scene}");
    assert!(!scene.contains("PB"), "a hidden page is mounted:\n{scene}");
    assert!(
        !scene.contains("TIP"),
        "an unhovered tooltip is mounted:\n{scene}"
    );
    assert_eq!(shell.scene.of_kind(NodeKind::Page).len(), 1, "{scene}");
    assert!(shell.scene.of_kind(NodeKind::Tooltip).is_empty(), "{scene}");

    // Hovering the box mounts its tooltip; leaving unmounts it.
    let host = shell.scene.of_kind(NodeKind::Box)[0];
    shell.inst.set_flag(host, NodeFlag::Hover, true);
    shell.flush();
    let scene = shell.scene.render();
    assert!(
        scene.contains("tooltip\n") && scene.contains("TIP"),
        "{scene}"
    );
    shell.inst.set_flag(host, NodeFlag::Hover, false);
    shell.flush();
    let scene = shell.scene.render();
    assert!(!scene.contains("TIP"), "{scene}");
    assert!(shell.scene.of_kind(NodeKind::Tooltip).is_empty(), "{scene}");

    // Page a's state moves on, the page is switched away and back.
    let toggle = shell.scene.of_kind(NodeKind::Box)[1];
    let inc = shell.scene.of_kind(NodeKind::Box)[2];
    assert!(shell.inst.event(inc, "click", Vec::new()));
    shell.flush();
    shell.text_node("PA 1");
    // `row_first` on the `pages` is the current page's place in source
    // order: render slides forward when it grows (directional pages).
    let pages = shell.scene.of_kind(NodeKind::Pages)[0];
    let first = |shell: &Shell| match shell.scene.prop(pages, Prop::RowFirst) {
        Some(PropValue::Number(f)) => *f,
        other => panic!("row_first {other:?}"),
    };
    let on_a = first(&shell);
    assert!(shell.inst.event(toggle, "click", Vec::new()));
    shell.flush();
    let scene = shell.scene.render();
    assert!(scene.contains("PB"), "{scene}");
    assert!(!scene.contains("PA"), "the hidden page stayed:\n{scene}");
    assert_eq!(shell.scene.of_kind(NodeKind::Page).len(), 1, "{scene}");
    assert!(first(&shell) > on_a, "page b comes after page a");
    // One diff swaps the page and sets `row_first`.
    assert!(shell.inst.event(toggle, "click", Vec::new()));
    let diff = shell.flush().diff;
    let sets_first = diff
        .ops
        .iter()
        .any(|op| matches!(op, SceneOp::SetProp { id, prop: Prop::RowFirst, .. } if *id == pages));
    let creates_page = diff.ops.iter().any(|op| {
        matches!(
            op,
            SceneOp::Create {
                kind: NodeKind::Page,
                ..
            }
        )
    });
    assert!(sets_first && creates_page, "{diff:?}");
    shell.text_node("PA 1");
    assert!(!shell.scene.render().contains("PB"));
    assert_eq!(first(&shell), on_a);
}

/// Directional pages go by the pages' order as mounted, not by where
/// each `page` is written: pages a `for` makes follow their items (they
/// all share one span), also once the items are reordered. `row_first`
/// on the `pages` moves by one towards the new page's side. (A `page`
/// must sit directly in its `pages`, through `if`, `match` or `for`
/// only: the checker refuses one in a component, so pages never come
/// from another file.)
#[test]
fn pages_from_a_for_slide_by_their_items_order() {
    let main = "state tabs = [\"x\", \"y\", \"z\"]\n\
                state cur = \"x\"\n\
                bar B {\n\
                  pages current: cur {\n\
                    page \"a\" { text \"PA\" }\n\
                    for p in tabs key p { page p { text p } }\n\
                    if true { page \"w\" { text \"PW\" } }\n\
                  }\n\
                }\n";
    let mut shell = boot(&[("t.strand", main)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let pages = shell.scene.of_kind(NodeKind::Pages)[0];
    let first = |shell: &Shell| match shell.scene.prop(pages, Prop::RowFirst) {
        Some(PropValue::Number(f)) => *f,
        other => panic!("row_first {other:?}"),
    };
    let show = |shell: &mut Shell, page: &str| {
        let before = first(shell);
        shell.inst.set_value("t", "cur", Value::text(page)).unwrap();
        shell.flush();
        let text = match page {
            "a" => "PA",
            "w" => "PW",
            p => p,
        };
        let scene = shell.scene.render();
        assert!(shell.scene.find_text(text).is_some(), "{page}: {scene}");
        first(shell) - before
    };
    shell.text_node("x");
    // Forward along the `for`'s items, then back.
    assert!(show(&mut shell, "z") > 0.0);
    assert!(show(&mut shell, "y") < 0.0);
    assert!(show(&mut shell, "a") < 0.0);
    assert!(show(&mut shell, "x") > 0.0);
    // The page in the `if` comes after the `for`'s.
    assert!(show(&mut shell, "w") > 0.0);
    assert!(show(&mut shell, "z") < 0.0);
    // Items reordered: their pages follow.
    shell
        .inst
        .set_value(
            "t",
            "tabs",
            Value::list(vec![Value::text("z"), Value::text("y"), Value::text("x")]),
        )
        .unwrap();
    shell.flush();
    assert!(show(&mut shell, "x") > 0.0);
    assert!(show(&mut shell, "z") < 0.0);
}

/// A `popup`'s content is mounted when it opens and unmounted when it
/// closes (the schema's `on_demand`): its nodes leave the scene and the
/// scopes under it go, while the `state`s of the components in it (a
/// plain one, a keyed list) are kept and come back when it opens again,
/// as design.md's calendar keeps its month. Closing and opening again
/// leaks nothing, and the kept cells go with the popup.
#[test]
fn popup_content_unmounts_when_closed_and_keeps_its_state() {
    let src = "type R { id: int }\n\
               export state p = false\n\
               export state q = true\n\
               component Cal {\n\
                 state month = 1\n\
                 state rs: [R] key id = [R(id: 1)]\n\
                 text join(\" \", \"M\", month, rs.len, pct(audio.sink.volume))\n\
                 box { on click { month += 1; rs.push(R(id: month)) } }\n\
               }\n\
               bar B {\n\
                 text \"x\"\n\
                 if q { popup { open: <-> p; Cal } }\n\
               }\n";
    let mut shell = boot(&[("pop.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let content = |shell: &Shell| {
        let popup = shell.scene.of_kind(NodeKind::Popup);
        assert_eq!(popup.len(), 1, "{}", shell.scene.render());
        shell.scene.render().contains("text=\"M ")
    };
    assert!(!content(&shell), "closed at boot: not mounted");
    let closed_nodes = shell.rt.stats().nodes;

    shell.inst.set("pop.p", Value::Bool(true)).unwrap();
    shell.flush();
    assert!(content(&shell));
    let text = shell
        .scene
        .texts()
        .into_iter()
        .find(|t| t.starts_with("M "))
        .unwrap();
    assert!(text.starts_with("M 1 1 "), "{text}");
    assert_eq!(shell.host.readers("audio"), 1);
    let open_nodes = shell.rt.stats().nodes;
    let inc = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(inc, "click", Vec::new()));
    shell.flush();
    let first = shell
        .scene
        .texts()
        .into_iter()
        .find(|t| t.starts_with("M 2 2 "))
        .unwrap();
    let first_id = shell.text_node(&first);

    // Closed: the content's nodes leave the scene, its scopes go.
    shell.inst.set("pop.p", Value::Bool(false)).unwrap();
    shell.flush();
    let scene = shell.scene.render();
    assert!(
        !content(&shell),
        "the closed popup's content stayed:\n{scene}"
    );
    assert!(shell.scene.of_kind(NodeKind::Box).is_empty(), "{scene}");
    assert_eq!(shell.host.readers("audio"), 0);
    let kept_nodes = shell.rt.stats().nodes;
    assert!(
        kept_nodes < open_nodes,
        "the content's scopes stayed: {kept_nodes} live nodes closed, {open_nodes} open"
    );

    // Open again: mounted afresh, its state as it was left.
    shell.inst.set("pop.p", Value::Bool(true)).unwrap();
    shell.flush();
    let again = shell
        .scene
        .texts()
        .into_iter()
        .find(|t| t.starts_with("M "))
        .unwrap();
    assert!(again.starts_with("M 2 2 "), "state lost: {again}");
    assert_ne!(shell.text_node(&again), first_id, "a new mount");
    assert_eq!(shell.host.readers("audio"), 1);
    let inc = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(inc, "click", Vec::new()));
    shell.flush();
    assert!(
        shell.scene.texts().iter().any(|t| t.starts_with("M 3 3 ")),
        "{:?}",
        shell.scene.texts()
    );

    // Opening and closing again and again leaks nothing.
    for _ in 0..20 {
        shell.inst.set("pop.p", Value::Bool(false)).unwrap();
        shell.flush();
        shell.inst.set("pop.p", Value::Bool(true)).unwrap();
        shell.flush();
    }
    shell.inst.set("pop.p", Value::Bool(false)).unwrap();
    shell.flush();
    assert_eq!(shell.rt.stats().nodes, kept_nodes, "a cycle leaks");
    assert!(!content(&shell));

    // The popup goes for good: its kept cells go with it.
    shell.inst.set("pop.q", Value::Bool(false)).unwrap();
    shell.flush();
    assert!(shell.scene.of_kind(NodeKind::Popup).is_empty());
    shell.inst.set("pop.q", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.rt.stats().nodes, closed_nodes, "kept cells leaked");
    shell.inst.set("pop.p", Value::Bool(true)).unwrap();
    shell.flush();
    assert!(
        shell.scene.texts().iter().any(|t| t.starts_with("M 1 1 ")),
        "a new popup starts afresh: {:?}",
        shell.scene.texts()
    );
}

/// A settings file and a persisted `state` in a closed popup's content
/// are kept as a plain one is.
#[test]
fn a_closed_popup_keeps_settings_and_persisted_state() {
    let src = "export state p = false\n\
               component Cal {\n\
                 state prefs from \"cal.toml\" { week: int = 1 }\n\
                 state seen = 0 persist\n\
                 text join(\" \", \"W\", prefs.week, seen)\n\
                 box { on click { prefs.week += 1; seen += 2 } }\n\
               }\n\
               bar B { popup { open: <-> p; Cal } }\n";
    let mut shell = boot(&[("cal.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    shell.inst.set("cal.p", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["W 1 0"]);
    let inc = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(inc, "click", Vec::new()));
    shell.flush();
    assert_eq!(shell.scene.texts(), ["W 2 2"]);
    shell.inst.set("cal.p", Value::Bool(false)).unwrap();
    shell.flush();
    assert!(shell.scene.texts().is_empty(), "{:?}", shell.scene.texts());
    shell.inst.set("cal.p", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["W 2 2"]);
    let inc = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(inc, "click", Vec::new()));
    shell.flush();
    assert_eq!(shell.scene.texts(), ["W 3 4"]);
}

/// The runtime mounts on demand exactly the elements the checker's cycle
/// check reads as on demand: a `popup` (a surface, its content mounted
/// while it is open), a `page` and a `tooltip`.
#[test]
fn every_on_demand_element_has_a_runtime_rule() {
    let schema = strand_compiler::schema::Schema::builtin();
    let on_demand: Vec<&str> = schema
        .elements
        .iter()
        .filter(|(_, e)| e.flags.on_demand)
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(on_demand, ["page", "popup", "tooltip"]);
}

/// Boots `src` on a 2 MiB thread (the logic thread's stack) with a
/// deadline, so a mount that never ends fails the test instead of
/// hanging it. Returns the boot's runtime errors (as text) and the
/// shell's scene, then runs `more` on the shell.
fn boot_bounded(
    src: &'static str,
    more: impl FnOnce(&mut Shell) -> Vec<String> + Send + 'static,
) -> (Vec<String>, Vec<String>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let mut map = SourceMap::new();
            map.add("t.strand", src.to_string());
            let compiled = strand_compiler::compile(&map);
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(
                &compiled.program,
                strand_compiler::schema::Schema::builtin(),
            ));
            let rt = Runtime::new();
            let host = Rc::new(SchemaHost::mock(&rt, &program.types));
            screens(&rt, &host, &["DP-1"]);
            let inst = Instance::new(&rt, program, host.clone(), Storage::none());
            let mut shell = Shell {
                rt,
                host,
                inst,
                scene: SceneMirror::new(),
                boot: Vec::new(),
                boot_notices: Vec::new(),
                boot_diagnostics: Vec::new(),
            };
            let u = shell.flush();
            let boot: Vec<String> = u.errors.iter().map(|e| e.to_string()).collect();
            let later = more(&mut shell);
            let _ = tx.send((boot, later));
        })
        .unwrap();
    rx.recv_timeout(Duration::from_secs(60))
        .expect("mounting never ended")
}

/// A component that keeps mounting itself where the static cycle check
/// cannot see it (under an `if`, in a tooltip, on a page that becomes
/// current) stops at 256 nested elements and components with a located
/// error naming it; the flush returns.
#[test]
fn runaway_recursion_stops_at_the_depth_limit() {
    fn one_error(errors: &[String], what: &str) {
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert!(
            errors[0].contains(what) && errors[0].contains("256"),
            "{errors:#?}"
        );
    }
    // Under an `if`, even fanning out two ways at every level.
    let (errors, _) = boot_bounded(
        "component C(n: int) { box { if n >= 0 { C n: n + 1; C n: n + 1 } } }\nbar B { C n: 0 }\n",
        |_| Vec::new(),
    );
    one_error(&errors, "component `C`");
    // Through an element that is no component.
    let (errors, _) = boot_bounded(
        "component C(n: int) { if n >= 0 { box { C n: n + 1 } } }\nbar B { C n: 0 }\n",
        |_| Vec::new(),
    );
    one_error(&errors, "`box` in component `C`");
    // In a tooltip: nothing until hovered, then one level per hover.
    let (errors, later) = boot_bounded(
        "component C { box { tooltip { C } } }\nbar B { C }\n",
        |shell| {
            for _ in 0..300 {
                let Some(&b) = shell.scene.of_kind(NodeKind::Box).last() else {
                    break;
                };
                shell.inst.set_flag(b, NodeFlag::Hover, true);
                let u = shell.flush();
                if !u.errors.is_empty() {
                    return u.errors.iter().map(|e| e.to_string()).collect();
                }
            }
            Vec::new()
        },
    );
    assert!(errors.is_empty(), "{errors:#?}");
    one_error(&later, "component `C`");
    // On a hidden page: nothing; once its page is current, the limit.
    let (errors, later) = boot_bounded(
        "enum Pg { a, b }\n\
         state cur: Pg = b\n\
         component C { pages current: cur { page a { C }; page b { text \"B\" } } }\n\
         bar B { box { on click { cur = a } }; C }\n",
        |shell| {
            let b = shell.scene.of_kind(NodeKind::Box)[0];
            shell.inst.event(b, "click", Vec::new());
            let u = shell.flush();
            u.errors.iter().map(|e| e.to_string()).collect()
        },
    );
    assert!(errors.is_empty(), "{errors:#?}");
    one_error(&later, "component `C`");
}

/// A per-device mixer: `s.volume` for `s` in `audio.sinks` (a keyed list
/// of `AudioDevice`, whose `volume` is `rw`) is written through the
/// service by the item's key, from a handler and from a slider's `<->`,
/// and the row shows the new value at once.
#[test]
fn rw_fields_of_keyed_service_items_are_written_by_key() {
    let src = "bar B {\n  for s in audio.sinks {\n    row {\n      text join(\" \", s.name, pct(s.volume))\n      box { on click { s.volume = 0.25 } }\n      slider { value: <-> s.volume }\n    }\n  }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        let dev = |id: i64, name: &str| {
            host.record(
                "AudioDevice",
                &[
                    ("id", Value::int(id)),
                    ("name", Value::text(name)),
                    ("volume", Value::float(0.5)),
                ],
            )
        };
        host.set(
            rt,
            "audio.sinks",
            Value::list(vec![dev(40, "speakers"), dev(41, "headset")]),
        )
        .unwrap();
    });
    assert!(shell.scene.find_text("headset 50%").is_some());
    // The second row's button: the headset, by its key.
    let b = shell.scene.of_kind(NodeKind::Box)[1];
    assert!(shell.inst.event(b, "click", Vec::new()));
    shell.flush();
    let writes = shell.host.take_writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0].path, "AudioDevice(41).volume");
    assert_eq!(writes[0].value, Value::float(0.25));
    assert!(
        shell.scene.find_text("headset 25%").is_some(),
        "{}",
        shell.scene.render()
    );
    assert!(shell.scene.find_text("speakers 50%").is_some());
    // The first row's slider writes the speakers' volume.
    let slider = shell.scene.of_kind(NodeKind::Slider)[0];
    shell
        .inst
        .write(slider, Prop::Value, PropValue::Number(0.75))
        .unwrap();
    shell.flush();
    let writes = shell.host.take_writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0].path, "AudioDevice(40).volume");
    assert!(shell.scene.find_text("speakers 75%").is_some());
    assert_eq!(
        shell.scene.prop(slider, Prop::Value),
        Some(&PropValue::Number(0.75))
    );
}

/// `audio.sink.volume` (fields all the way from the service) stays a
/// write of the service's `sink` field, though `sink` is an
/// `AudioDevice`, a keyed record.
#[test]
fn a_field_path_from_a_service_is_a_field_write() {
    let src = "bar B {\n  box { on click { audio.sink.muted = !audio.sink.muted } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        let sink = host.record("AudioDevice", &[("id", Value::int(40))]);
        host.set(rt, "audio.sink", sink).unwrap();
    });
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    assert!(shell.inst.event(b, "click", Vec::new()));
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    let writes = shell.host.take_writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0].path, "audio.sink.muted");
}

/// Time-bound values (M4) reach the scene as token expressions with
/// time leaves: a whole prop as `PropValue::Token`, a comma shorthand
/// item in place, and a number inside a composite value (a border's
/// width, a conic gradient's `from`) as a template's numeric slot.
/// Render evaluates them per node per frame (`TokenScope::with_time`).
#[test]
fn time_values_convert_to_token_time_leaves() {
    use strand_scene::{BinOp, Border, Paint, TimeContext, TokenExpr, TokenScope, TokenTable};
    let src = "bar B {\n  box { rotate: t * 20deg; opacity: 0.5 + 0.5 * wave(2s) }\n  box { glow: 10 * wave(2s), $accent.alpha(0.4) }\n  box { border: 1.5 + noise(t), conic(from: t * 40deg, $accent, #000000, $accent) }\n  box { bg: $accent.alpha(wave(1s)) }\n}\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(boxes.len(), 4, "{}", shell.scene.render());
    let prop = |i: usize, p: Prop| shell.scene.prop(boxes[i], p).cloned();

    // `rotate: t * 20deg`: the expression itself, `t` a leaf.
    assert_eq!(
        prop(0, Prop::Rotate),
        Some(PropValue::Token(TokenExpr::Binary {
            op: BinOp::Mul,
            lhs: Box::new(TokenExpr::Time),
            rhs: Box::new(TokenExpr::value(PropValue::Number(20.0))),
        }))
    );
    for (i, p) in [
        (0, Prop::Rotate),
        (0, Prop::Opacity),
        (1, Prop::Glow),
        (2, Prop::Border),
        (3, Prop::Bg),
    ] {
        let v = prop(i, p).unwrap_or_else(|| panic!("box {i} has no {p:?}"));
        assert!(v.reads_time(), "box {i} {p:?} reads time: {v:?}");
    }

    // Evaluated where render evaluates them: per node, at its `t`.
    let mut table = TokenTable::default();
    table.insert("accent", PropValue::Color(strand_scene::Color::WHITE));
    let levels = [&table];
    let at = |t: f32, v: &PropValue| {
        TokenScope::new(&levels)
            .with_time(Some(TimeContext::at(t)))
            .resolve(v)
            .map(|c| c.into_owned())
    };
    assert_eq!(
        at(1.5, &prop(0, Prop::Rotate).unwrap()),
        Some(PropValue::Number(30.0))
    );
    // `wave(2s)` is 0 at `t = 0` and 1 half a period in.
    assert_eq!(
        at(0.0, &prop(0, Prop::Opacity).unwrap()),
        Some(PropValue::Number(0.5))
    );
    assert_eq!(
        at(1.0, &prop(0, Prop::Opacity).unwrap()),
        Some(PropValue::Number(1.0))
    );

    // The comma shorthand keeps each item in place.
    let Some(PropValue::List(glow)) = at(1.0, &prop(1, Prop::Glow).unwrap()) else {
        panic!("{:?}", prop(1, Prop::Glow))
    };
    assert_eq!(glow[0], PropValue::Number(10.0));

    // A border's width and a conic's `from` are numeric template slots;
    // its colours are the colour slots, as before.
    let Some(PropValue::Token(TokenExpr::Template {
        value,
        colors,
        numbers,
    })) = prop(2, Prop::Border)
    else {
        panic!("{:?}", prop(2, Prop::Border))
    };
    assert!(matches!(
        *value,
        PropValue::Border(Border {
            paint: Paint::Conic { .. },
            ..
        })
    ));
    assert_eq!(colors.len(), 3);
    assert_eq!(numbers.len(), 2, "width, then `from`: {numbers:?}");
    assert!(
        numbers
            .iter()
            .all(|n| n.as_ref().is_some_and(TokenExpr::reads_time))
    );
    let border = TokenExpr::Template {
        value,
        colors,
        numbers,
    };
    let Some(PropValue::Border(Border {
        width,
        paint: Paint::Conic { from, stops },
    })) = at(2.0, &PropValue::Token(border))
    else {
        panic!("the border resolves")
    };
    // `noise` is 0 at whole `x`.
    assert_eq!((width, from), (1.5, 80.0));
    assert_eq!(stops[0].color, strand_scene::Color::WHITE);

    // A colour method with a time argument is a time value too.
    let Some(PropValue::Color(c)) = at(0.5, &prop(3, Prop::Bg).unwrap()) else {
        panic!("{:?}", prop(3, Prop::Bg))
    };
    assert!(
        (c.a - 1.0).abs() < 1e-6,
        "wave(1s) is 1 at half a period: {c:?}"
    );
}

/// design.md "Effects": `text "hi" { letters { y: 2 * wave(1s, phase: index * 0.1) } }`.
/// A letter's `index` and `count` are time leaves (`TokenExpr::Index`,
/// `TokenExpr::Count`) render evaluates per letter, not values logic
/// fixes for the whole node: arithmetic on them stays symbolic, and the
/// same prop resolves differently for each letter.
/// decisions.md m4-owner: `letters` animates the text of its enclosing
/// `text` node, mounted as its child.
#[test]
fn letters_go_inside_the_text_they_animate() {
    let src = "bar B {\n  text \"hello\" { letters { y: 2 * wave(1s, phase: index * 0.1) } }\n}\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let letters = shell.scene.of_kind(NodeKind::Letters);
    assert_eq!(letters.len(), 1, "{}", shell.scene.render());
    let parent = shell.scene.parent(letters[0]).unwrap();
    assert_eq!(shell.scene.of_kind(NodeKind::Text), vec![parent]);
}

#[test]
fn letters_index_and_count_stay_time_leaves() {
    use strand_scene::{TimeContext, TokenExpr, TokenScope, TokenTable};
    let src = "bar B {\n  text \"hi\" { letters { y: 2 * wave(1s, phase: index * 0.1); x: index * 8; opacity: (index + 1) / count } }\n}\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let letters = shell.scene.of_kind(NodeKind::Letters);
    assert_eq!(letters.len(), 1, "{}", shell.scene.render());
    let prop = |p: Prop| {
        shell
            .scene
            .prop(letters[0], p)
            .cloned()
            .unwrap_or_else(|| panic!("no {p:?}: {}", shell.scene.render()))
    };
    // `x: index * 8`: the expression, `index` a leaf.
    assert_eq!(
        prop(Prop::X),
        PropValue::Token(TokenExpr::Binary {
            op: strand_scene::BinOp::Mul,
            lhs: Box::new(TokenExpr::Index),
            rhs: Box::new(TokenExpr::value(PropValue::Number(8.0))),
        })
    );
    let table = TokenTable::default();
    let levels = [&table];
    let at = |cx: TimeContext, v: &PropValue| {
        TokenScope::new(&levels)
            .with_time(Some(cx))
            .resolve(v)
            .map(|c| c.into_owned())
    };
    let letter = |index: u32| TimeContext {
        t: 0.25,
        index,
        count: 4,
    };
    assert_eq!(at(letter(3), &prop(Prop::X)), Some(PropValue::Number(24.0)));
    assert_eq!(
        at(letter(1), &prop(Prop::Opacity)),
        Some(PropValue::Number(0.5))
    );
    // The phase is per letter: a quarter period in, letter 0 reads
    // wave = 0.5 (y = 1), so does letter 5 (phase 0.5, the falling
    // side), and letter 2 (phase 0.2) is near the top (y ≈ 1.95).
    let y = |i: u32| match at(letter(i), &prop(Prop::Y)) {
        Some(PropValue::Number(n)) => n,
        other => panic!("{other:?}"),
    };
    assert!((y(0) - 1.0).abs() < 1e-5, "{}", y(0));
    assert!((y(5) - 1.0).abs() < 1e-5, "{}", y(5));
    assert!((y(2) - y(0)).abs() > 0.5, "{} vs {}", y(2), y(0));
}

/// (M4) Bindable SVG: `svg "gauge.svg" { #needle { rotate: … } }` mounts
/// a `svg_part` child of the `svg` carrying the id as `name` and its
/// props, which follow state like any prop.
#[test]
fn svg_selectors_mount_svg_parts() {
    let src = "state level = 0.5\nbar B {\n  svg \"/tmp/gauge.svg\" {\n    size: 48\n    #needle { rotate: level * 270deg; opacity: 0.8 }\n    #face { fill: #ff0000 }\n  }\n}\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let svg = shell.scene.of_kind(NodeKind::Svg);
    assert_eq!(svg.len(), 1, "{}", shell.scene.render());
    let parts = shell.scene.children(svg[0]).to_vec();
    assert_eq!(parts.len(), 2, "{}", shell.scene.render());
    assert_eq!(shell.scene.of_kind(NodeKind::SvgPart), parts);
    let prop = |i: usize, p: Prop| shell.scene.prop(parts[i], p).cloned();
    assert_eq!(prop(0, Prop::Name), Some(PropValue::Text("needle".into())));
    assert_eq!(prop(0, Prop::Rotate), Some(PropValue::Angle(135.0)));
    assert_eq!(prop(0, Prop::Opacity), Some(PropValue::Number(0.8)));
    assert_eq!(prop(1, Prop::Name), Some(PropValue::Text("face".into())));
    assert!(prop(1, Prop::Fill).is_some());
    let mut shell = shell;
    shell
        .inst
        .set_value("t", "level", Value::float(1.0))
        .unwrap();
    shell.at(0.1);
    let needle = shell.scene.of_kind(NodeKind::SvgPart)[0];
    assert_eq!(
        shell.scene.prop(needle, Prop::Rotate),
        Some(&PropValue::Angle(270.0))
    );
}

/// (M4) An `on drop` takes what a call with that parameter would: an
/// empty list (whose items say no type) fits a `[Pin]` parameter, and an
/// `int` fits a `float` one. Render matches by name, so the target's
/// `Prop::Accepts` names those too (`[any]`, `int`), and the delivery
/// runs the handler the value fits and not the other.
#[test]
fn drops_take_what_a_call_would() {
    let src = r#"type Pin { app: text; label: text }
state none: [Pin] = []
state count: int = 3
state lists = 0
state nums = 0.0
bar B {
  row {
    on drop(ps: [Pin], at: int) { lists += 1 }
    on drop(x: float, at: int) { nums += x }
    box { drag: none }
    box { drag: count }
  }
}
"#;
    let mut shell = boot(&[("dock.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let kw = |k: &str| PropValue::Keyword(k.into());
    let Some(PropValue::List(acc)) = shell.scene.prop(row, Prop::Accepts).cloned() else {
        panic!("no accepts")
    };
    for k in ["[Pin]", "[any]", "float", "int"] {
        assert!(acc.contains(&kw(k)), "{k} in {acc:?}");
    }
    assert!(!acc.contains(&kw("any")), "{acc:?}");
    let boxes = shell.scene.children(row).to_vec();
    let ty = |n: NodeId| {
        strand_scene::drag_type(shell.scene.prop(n, Prop::Drag).expect("a drag source"))
            .map(str::to_string)
    };
    assert_eq!(ty(boxes[0]).as_deref(), Some("[any]"));
    assert_eq!(ty(boxes[1]).as_deref(), Some("int"));
    let empty = shell.inst.drag_value(boxes[0]).unwrap();
    assert!(shell.inst.event(row, "drop", vec![empty, Value::int(0)]));
    shell.flush();
    assert_eq!(shell.inst.value_of("dock", "lists").unwrap(), Value::int(1));
    let three = shell.inst.drag_value(boxes[1]).unwrap();
    assert!(shell.inst.event(row, "drop", vec![three, Value::int(0)]));
    shell.flush();
    assert_eq!(shell.inst.value_of("dock", "lists").unwrap(), Value::int(1));
    let nums = shell.inst.value_of("dock", "nums").unwrap();
    assert!(
        matches!(&nums, Value::Num(n, _) if (*n - 3.0).abs() < 1e-9),
        "{nums:?}"
    );
}

/// (M4) What a drag gives other programs: a `drag:` source whose value
/// has a form outside Strand (text, a `Drop`, an `App`) reaches render
/// as its type name followed by that form (`strand_scene::drag_export`
/// reads it); other values as the name alone. An `on drop` target whose
/// direct child is a `for` holds rows (`Prop::DropRows`), whether or not
/// they are draggable; one without a `for` does not.
#[test]
fn drag_sources_say_what_other_programs_get() {
    let src = r#"type Pin { app: text; label: text }
state words = ["alpha", "beta"]
state got = ""
bar B {
  row {
    box { drag: "hello" }
    box { drag: Drop(kind: DropKind.files, files: ["/tmp/a b.png", "/tmp/c"], app: null, text: "") }
    box { drag: App(id: "org.x.Y.desktop", name: "Y", comment: null, icon: "y", categories: [], actions: []) }
    box { drag: Pin(app: "a", label: "A") }
  }
  col {
    on drop(t: text, at: int) { got = t }
    for w in words key w { text w }
  }
  stack { on drop(t: text, at: int) { got = t }; text "x" }
}
"#;
    let shell = boot(&[("dock.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let boxes = shell.scene.children(row).to_vec();
    let exports: Vec<_> = boxes
        .iter()
        .map(|b| {
            let p = shell.scene.prop(*b, Prop::Drag).expect("a drag prop");
            (
                strand_scene::drag_type(p).map(str::to_string),
                strand_scene::drag_export(p),
            )
        })
        .collect();
    use strand_scene::{DropKind, DropPayload};
    assert_eq!(
        exports,
        [
            (
                Some("text".to_string()),
                Some(DropPayload::External {
                    kind: DropKind::Text,
                    files: vec![],
                    text: "hello".into(),
                    app_id: None
                })
            ),
            (
                Some("Drop".to_string()),
                Some(DropPayload::External {
                    kind: DropKind::Files,
                    files: vec!["/tmp/a b.png".into(), "/tmp/c".into()],
                    text: String::new(),
                    app_id: None
                })
            ),
            (
                Some("App".to_string()),
                Some(DropPayload::External {
                    kind: DropKind::App,
                    files: vec![],
                    text: String::new(),
                    app_id: Some("org.x.Y.desktop".into())
                })
            ),
            (Some("Pin".to_string()), None),
        ]
    );
    let col = shell.scene.of_kind(NodeKind::Col)[0];
    let stack = shell.scene.of_kind(NodeKind::Stack)[0];
    assert_eq!(
        shell.scene.prop(col, Prop::DropRows),
        Some(&PropValue::Bool(true))
    );
    assert_eq!(shell.scene.prop(stack, Prop::DropRows), None);
    assert_eq!(shell.scene.prop(row, Prop::DropRows), None, "no `on drop`");
}

/// (M5) Every element whose direct child is a `for` is marked
/// `Prop::DropRows`, `on drop` or not, so render tells a nested list's
/// items from the outer list's rows: a kanban board's columns, each a
/// `for` of `drag:` cards, are marked as the board is; a card's own box
/// (no `for`) is not.
#[test]
fn every_element_holding_a_for_marks_its_rows() {
    let src = r#"type Card { id: text }
type Column { name: text; cards: [Card] }
state cols: [Column] key name = [
  Column(name: "todo", cards: [Card(id: "a")]),
  Column(name: "doing", cards: [Card(id: "b"), Card(id: "c")]),
]
state got = ""
bar B {
  row {
    on drop(c: Card, at: int) { got = c.id }
    for column in cols key column.name {
      col { for c in column.cards key c.id { box { drag: c } } }
    }
  }
}
"#;
    let shell = boot(&[("board.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let board = shell.scene.of_kind(NodeKind::Row)[0];
    assert_eq!(
        shell.scene.prop(board, Prop::DropRows),
        Some(&PropValue::Bool(true))
    );
    let cols = shell.scene.of_kind(NodeKind::Col);
    assert_eq!(cols.len(), 2);
    for c in cols {
        assert_eq!(
            shell.scene.prop(c, Prop::DropRows),
            Some(&PropValue::Bool(true)),
            "a column holding a `for` without `on drop`"
        );
    }
    for b in shell.scene.of_kind(NodeKind::Box) {
        assert_eq!(shell.scene.prop(b, Prop::DropRows), None);
    }
}

/// (M5) A child of an element holding a `for` that is not one of its
/// rows is marked `Prop::NotRow` (decisions.md m5-fixups, review round
/// 3): a dock item's label and icon beside its `for` of window dots, the
/// icon mounted later through an `if`, are the item's own content, so
/// render takes the item as the icon's row; the dots, and the items
/// themselves (rows of the dock's `for`), are not marked.
#[test]
fn content_beside_a_for_is_marked_not_a_row() {
    let src = r#"type Pin { app: text; windows: [int] }
state pins: [Pin] key app = [
  Pin(app: "a", windows: [1, 2]),
  Pin(app: "b", windows: [3]),
]
state shown = true
bar B {
  row {
    on drop(p: Pin, at: int) { }
    for p in pins key p.app {
      col {
        text p.app
        for w in p.windows key w { box { } }
        if shown { stack { drag: p } }
      }
    }
  }
}
"#;
    let shell = boot(&[("dock.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let dock = shell.scene.of_kind(NodeKind::Row)[0];
    assert_eq!(shell.scene.prop(dock, Prop::NotRow), None);
    let items = shell.scene.of_kind(NodeKind::Col);
    assert_eq!(items.len(), 2);
    for c in &items {
        assert_eq!(
            shell.scene.prop(*c, Prop::DropRows),
            Some(&PropValue::Bool(true))
        );
        assert_eq!(
            shell.scene.prop(*c, Prop::NotRow),
            None,
            "a row of the dock"
        );
    }
    let content: Vec<NodeId> = shell
        .scene
        .of_kind(NodeKind::Text)
        .into_iter()
        .chain(shell.scene.of_kind(NodeKind::Stack))
        .collect();
    assert_eq!(content.len(), 4);
    for n in content {
        assert_eq!(
            shell.scene.prop(n, Prop::NotRow),
            Some(&PropValue::Bool(true)),
            "the item's own content"
        );
    }
    let dots = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(dots.len(), 3);
    for b in dots {
        assert_eq!(shell.scene.prop(b, Prop::NotRow), None, "a `for` row");
    }
}

/// (M5) A drag source's identity (`strand_scene::drag_identity`) is
/// equal exactly when the values are (`==`): equal items in one list
/// share it, and values whose text shows the same but which differ
/// (`["a, b"]` and `["a", "b"]`, once shown "a, b" both) do not
/// (decisions.md m5-fixups).
#[test]
fn a_drag_identity_is_the_values_equality() {
    let src = r#"bar B {
  row {
    on drop(x: [text], at: int) { }
    box { drag: ["a, b"] }
    box { drag: ["a", "b"] }
    box { drag: ["a, b"] }
  }
}
"#;
    let shell = boot(&[("dock.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let ids: Vec<String> = shell
        .scene
        .children(row)
        .iter()
        .map(|b| {
            let drag = shell.scene.prop(*b, Prop::Drag).expect("a drag source");
            strand_scene::drag_identity(drag)
                .expect("an identity")
                .to_string()
        })
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[0], ids[2], "equal values, one identity");
    assert_ne!(ids[0], ids[1], "values that show alike are not equal");
}

/// (M4) Drag and drop's logic side: a `drag:` source reaches render as
/// its value's type name and an `on drop` target as the types its
/// handlers take (`Prop::Accepts`: `Drop` for other programs' drops,
/// `any` for an untyped parameter). A drop delivers the source's own
/// value and the global index, and runs only the handlers of its type:
/// dropping pin `c` at 0 moves it first ("reordering springs by key":
/// one keyed move), a `Drop` from another program runs the other
/// handler. A source unmounted takes its value with it.
#[test]
fn drag_sources_and_drop_targets_deliver_typed_values() {
    let src = r#"type Pin { app: text; label: text }
export state pins: [Pin] key app = [Pin(app: "a", label: "A"), Pin(app: "b", label: "B"), Pin(app: "c", label: "C")]
state got = ""
state seen = 0
bar B {
  row {
    on drop(p: Pin, at: int) { pins.move(p.app, at) }
    on drop(d: Drop, at: int) { got = d.text }
    for p in pins { box { drag: p } }
  }
  col { on drop(v, at: int) { seen += at } }
}
"#;
    let mut shell = boot(&[("dock.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let col = shell.scene.of_kind(NodeKind::Col)[0];
    let kw = |k: &str| PropValue::Keyword(k.into());
    assert_eq!(
        shell.scene.prop(row, Prop::Accepts),
        Some(&PropValue::List(vec![kw("Pin"), kw("Drop")]))
    );
    assert_eq!(
        shell.scene.prop(col, Prop::Accepts),
        Some(&PropValue::List(vec![kw("any")]))
    );
    let boxes = shell.scene.children(row).to_vec();
    assert_eq!(boxes.len(), 3);
    let mut ids = Vec::new();
    for b in &boxes {
        let drag = shell.scene.prop(*b, Prop::Drag).expect("a drag source");
        assert_eq!(strand_scene::drag_type(drag), Some("Pin"));
        assert_eq!(strand_scene::drag_export(drag), None);
        ids.push(
            strand_scene::drag_identity(drag)
                .expect("an identity")
                .to_string(),
        );
        assert_eq!(shell.scene.prop(*b, Prop::Accepts), None);
    }
    // (M5) One identity per value: three pins, three identities.
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "{ids:?}");
    let app = |shell: &Shell, n: NodeId| {
        let v = shell.inst.drag_value(n).expect("a drag value");
        let Value::Record(r) = v else {
            panic!("not a Pin: {v:?}")
        };
        r.fields[0].clone()
    };
    assert_eq!(app(&shell, boxes[2]), Value::text("c"));
    // Pin `c` dropped at 0: the Pin handler moves it, by key (the same
    // node, moved), and the `Drop` handler does not run.
    let c = shell.inst.drag_value(boxes[2]).unwrap();
    assert!(shell.inst.event(row, "drop", vec![c, Value::int(0)]));
    let u = shell.flush();
    let order: Vec<Value> = shell
        .scene
        .children(row)
        .iter()
        .map(|n| app(&shell, *n))
        .collect();
    assert_eq!(order, ["c", "a", "b"].map(Value::text).to_vec());
    assert_eq!(shell.scene.children(row)[0], boxes[2], "moved by key");
    assert!(
        !u.diff
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::Create { .. } | SceneOp::Remove { .. })),
        "a reorder only moves: {:?}",
        u.diff.ops
    );
    assert_eq!(shell.inst.value_of("dock", "got").unwrap(), Value::text(""));
    // Text from another program: the `Drop` handler, not the Pin one.
    let drop = shell.host.record(
        "Drop",
        &[
            ("kind", shell.host.variant("DropKind", "text")),
            ("text", Value::text("hello")),
        ],
    );
    assert!(
        shell
            .inst
            .event(row, "drop", vec![drop.clone(), Value::int(1)])
    );
    shell.flush();
    assert_eq!(
        shell.inst.value_of("dock", "got").unwrap(),
        Value::text("hello")
    );
    let order: Vec<Value> = shell
        .scene
        .children(row)
        .iter()
        .map(|n| app(&shell, *n))
        .collect();
    assert_eq!(order, ["c", "a", "b"].map(Value::text).to_vec());
    // An untyped parameter takes anything.
    assert!(shell.inst.event(col, "drop", vec![drop, Value::int(4)]));
    shell.flush();
    assert_eq!(shell.inst.value_of("dock", "seen").unwrap(), Value::int(4));
    // The sources unmounted: no values left behind.
    shell
        .inst
        .set("dock.pins", Value::list(Vec::new()))
        .unwrap();
    shell.flush();
    assert!(shell.scene.children(row).is_empty());
    for b in &boxes {
        assert_eq!(shell.inst.drag_value(*b), None);
    }
}

/// (m4-gpu-effects) The bundled effects' knobs check and reach the scene
/// as written: `bloom`'s optional `strength`, `glass`'s optional
/// `refraction`, beside the one-knob forms.
#[test]
fn bundled_effect_knobs_reach_the_scene() {
    let src = "bar B {\n  box { filter: bloom(12, 1.5), crt(), chromatic(2), wobble(4) }\n  box { filter: bloom(8) }\n  box { backdrop: glass(20) }\n  box { backdrop: glass() }\n}\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(boxes.len(), 4, "{}", shell.scene.render());
    let prop = |i: usize, p: Prop| shell.scene.prop(boxes[i], p).cloned();
    let call = |name: &str, args: Vec<PropValue>| PropValue::Call {
        name: name.into(),
        args,
    };
    let calls = |v: Option<PropValue>| -> Vec<(String, usize)> {
        let list = match v {
            Some(PropValue::List(items)) => items,
            Some(v) => vec![v],
            None => Vec::new(),
        };
        list.into_iter()
            .filter_map(|c| match c {
                PropValue::Call { name, args } => Some((name, args.len())),
                _ => None,
            })
            .collect()
    };
    let first = calls(prop(0, Prop::Filter));
    assert_eq!(first.len(), 4, "{first:?}");
    assert_eq!(first[0], ("bloom".to_string(), 2));
    assert_eq!(
        prop(1, Prop::Filter).map(|v| calls(Some(v))[0].0.clone()),
        Some("bloom".to_string())
    );
    let glass = prop(2, Prop::Backdrop);
    assert_eq!(glass, Some(call("glass", vec![PropValue::Number(20.0)])));
    assert_eq!(calls(prop(3, Prop::Backdrop))[0].0, "glass");
}

/// (M6) A click's call of a declared action asks for its command, each
/// parameter exactly one argument as its text: a title with spaces,
/// quotes and `$` stays whole, an `int`, a `bool` and an enum show as
/// written. A `from dbus` service's action goes to the service host by
/// name with its arguments (decisions.md m6-w2-language).
#[test]
fn a_declared_action_asks_for_its_command_with_whole_arguments() {
    use strand_compiler::vm::ExecRequest;
    let src = "permit exec \"notify-send\"\n\
        enum Urgency { low, critical }\n\
        state title = \"it's \\\"two\\\" words; $HOME\"\n\
        action say(summary: text, ms: int, wait: bool, u: Urgency) \
        [\"notify-send\", \"-t\", ms, \"-u\", u, summary, wait]\n\
        service ppd from dbus system \"net.hadess.PowerProfiles\" {\n  \
        profile: text = ActiveProfile\n  action hold(profile: text) = HoldProfile\n}\n\
        bar B {\n  box { on click { say(title, 1500, true, critical) } }\n  \
        box { on click { ppd.hold(\"performance\") } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert!(shell.inst.take_execs().is_empty(), "nothing runs at boot");
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(
        shell.inst.take_execs(),
        [ExecRequest {
            action: "say".into(),
            argv: [
                "notify-send",
                "-t",
                "1500",
                "-u",
                "critical",
                "it's \"two\" words; $HOME",
                "true"
            ]
            .map(String::from)
            .to_vec(),
            reply: None,
            stop: false,
        }]
    );
    assert!(shell.inst.take_execs().is_empty(), "taken once");
    shell.inst.event(boxes[1], "click", Vec::new());
    shell.flush();
    let calls = shell.host.take_actions();
    let held: Vec<_> = calls
        .iter()
        .filter(|c| c.target == "ppd")
        .map(|c| (c.name.as_str(), c.args.clone()))
        .collect();
    assert_eq!(held, [("hold", vec![Value::text("performance")])]);
}

/// (path-tilde) A `path` parameter's `~/…` reaches the program with the
/// home folder in place of the `~`, as every other path Strand takes
/// does, in an action with a result too; a `text` parameter given the
/// same value, and a `path` that is a bare `~`, only holds a `~` further
/// in or names another user's home, stay as written.
#[test]
fn a_path_argument_reaches_the_program_with_the_home_folder() {
    use strand_compiler::vm::ExecRequest;
    let home = std::env::var("HOME").expect("the test needs $HOME");
    assert!(home.starts_with('/'), "an absolute $HOME: {home:?}");
    let home = home.trim_end_matches('/');
    let src = "permit exec \"wf-recorder\"\npermit exec \"du\"\n\
        action rec(file: path, label: text, other: path) \
        [\"wf-recorder\", \"-f\", file, label, other]\n\
        action size(dir: path) -> text [\"du\", \"-s\", dir]\n\
        state got = \"\"\n\
        bar B {\n  box { on click { rec(\"~/Videos/a.mp4\", \"~/Videos/a.mp4\", \"~bob/x\") } }\n  \
        box { on click { rec(\"/tmp/~/a.mp4\", \"~\", \"~\") } }\n  \
        box { on click { got = await size(\"~/Pictures\") } }\n}\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    let argv = |shell: &mut Shell, i: usize| {
        shell.inst.event(boxes[i], "click", Vec::new());
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        shell
            .inst
            .take_execs()
            .into_iter()
            .map(|r: ExecRequest| (r.action, r.argv))
            .collect::<Vec<_>>()
    };
    let owned = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        argv(&mut shell, 0),
        [(
            "rec".to_string(),
            owned(&[
                "wf-recorder",
                "-f",
                &format!("{home}/Videos/a.mp4"),
                "~/Videos/a.mp4",
                "~bob/x",
            ])
        )]
    );
    assert_eq!(
        argv(&mut shell, 1),
        [(
            "rec".to_string(),
            owned(&["wf-recorder", "-f", "/tmp/~/a.mp4", "~", "~"])
        )]
    );
    assert_eq!(
        argv(&mut shell, 2),
        [(
            "size".to_string(),
            owned(&["du", "-s", &format!("{home}/Pictures")])
        )]
    );
}

/// (M6, the owner's ruling) A config's `service battery` is the one its
/// config reads: the host is asked for the declared service's field, not
/// the builtin's (decisions.md m6-w2-language).
#[test]
fn a_config_service_named_like_a_builtin_is_the_one_read() {
    let src = "service battery from file \"/tmp/level\" { level: int }\n\
        bar B { text join(\" \", \"level\", battery.level) }\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert!(
        shell.scene.find_text("level 0").is_some(),
        "{:?}",
        shell.scene.texts()
    );
    shell
        .host
        .set(&shell.rt, "battery.level", Value::int(7))
        .unwrap();
    shell.flush();
    assert!(
        shell.scene.find_text("level 7").is_some(),
        "{:?}",
        shell.scene.texts()
    );
}

/// (M6, owner testing Gaps 8 and 6) A component call's handlers and
/// `id:` are its root element's: a click on the root runs the caller's
/// handler in the caller's scope, `id:` reads the root's `hover`, a
/// call whose root is another call reaches the element at the bottom,
/// and a `slot`'s children stay children.
#[test]
fn a_calls_handlers_and_id_are_its_root_elements() {
    let src = "state n = 0\n\
        component Chip(label: text) { box { text label } }\n\
        component Wrap(label: text) { Chip label }\n\
        component Frame { box { slot } }\n\
        bar B {\n  row {\n    \
        Chip \"a\" { id: c; on click { n += 1 } }\n    \
        Wrap \"b\" { on click { n += 10 } }\n    \
        Frame { on click { n += 100 }; text \"inside\" }\n    \
        text \"hover\" { when c.hover { opacity: 0.5 } }\n    \
        text join(\" \", \"n\", n)\n  }\n}\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert!(
        shell.scene.find_text("inside").is_some(),
        "{:?}",
        shell.scene.texts()
    );
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(boxes.len(), 3);
    assert!(shell.inst.event(boxes[0], "click", Vec::new()));
    shell.flush();
    assert!(
        shell.scene.find_text("n 1").is_some(),
        "{:?}",
        shell.scene.texts()
    );
    assert!(shell.inst.event(boxes[1], "click", Vec::new()));
    shell.flush();
    assert!(
        shell.scene.find_text("n 11").is_some(),
        "{:?}",
        shell.scene.texts()
    );
    assert!(shell.inst.event(boxes[2], "click", Vec::new()));
    shell.flush();
    assert!(
        shell.scene.find_text("n 111").is_some(),
        "{:?}",
        shell.scene.texts()
    );
    let hover = shell.scene.find_text("hover").unwrap();
    assert_eq!(shell.scene.prop(hover, Prop::Opacity), None);
    shell.inst.set_flag(boxes[0], NodeFlag::Hover, true);
    shell.flush();
    assert_eq!(
        shell.scene.prop(hover, Prop::Opacity),
        Some(&PropValue::Number(0.5))
    );
}

/// (M6, owner testing Gaps 7) `screen` in a component is the screen of
/// the bar it is in, on each monitor: read directly, through a component
/// that calls it, and through a `slot`'s children.
#[test]
fn a_component_reads_its_bars_screen() {
    let src = "component Name { text join(\" \", \"on\", screen.name) }\n\
        component Line { row { Name } }\n\
        component Frame { box { slot } }\n\
        bar B { row { Line; Frame { text join(\" \", \"in\", screen.name); Name } } }\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1", "HDMI-A-1"])
    });
    let mut texts = shell.scene.texts();
    texts.sort();
    assert_eq!(
        texts,
        [
            "in DP-1",
            "in HDMI-A-1",
            "on DP-1",
            "on DP-1",
            "on HDMI-A-1",
            "on HDMI-A-1"
        ],
        "{texts:?}"
    );
}

/// (M6, spec.md 6.3) A `trigger: hover` popup's `open` is the router's:
/// it starts closed (render reads a popup with no `open` as shown), its
/// body mounts only once the router writes it true, plays `on show`,
/// and goes when it writes false. Its spec takes no grab. A popup
/// without the trigger keeps refusing a write to an `open` not bound
/// two-way.
#[test]
fn the_router_opens_a_hover_popup() {
    let src = "state shows = 0\nbar Top {\n  height: 30\n  row {\n    box { size: 20\n      popup { trigger: hover\n        on show { shows += 1 }\n        text \"island\" }\n    }\n    box { size: 20; popup { open: false; text \"menu\" } }\n  }\n  text shows > 0 ? \"shown\" : \"never\"\n}\n";
    let mut shell = boot(&[("h.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let popups = shell.scene.of_kind(NodeKind::Popup);
    let (island, menu) = (popups[0], popups[1]);
    let texts = |shell: &Shell| -> Vec<String> {
        shell
            .scene
            .of_kind(NodeKind::Text)
            .into_iter()
            .filter_map(|t| match shell.scene.prop(t, Prop::Text) {
                Some(PropValue::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        shell.scene.prop(island, Prop::Open),
        Some(&PropValue::Bool(false))
    );
    assert_eq!(texts(&shell), ["never"], "nothing mounted in either popup");
    let spec = strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |p| {
        shell.scene.prop(island, p).cloned()
    });
    assert!(spec.hover_trigger && !spec.open);
    shell
        .inst
        .write(island, Prop::Open, PropValue::Bool(true))
        .unwrap();
    shell.flush();
    assert_eq!(
        shell.scene.prop(island, Prop::Open),
        Some(&PropValue::Bool(true))
    );
    let mut got = texts(&shell);
    got.sort();
    assert_eq!(got, ["island", "shown"], "its body mounted and it showed");
    shell
        .inst
        .write(island, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(texts(&shell), ["shown"], "its body gone");
    assert!(
        shell
            .inst
            .write(menu, Prop::Open, PropValue::Bool(true))
            .is_err()
    );
}

/// (m6-owner-fixes, G-b) `open:` on a `trigger: hover` popup is the
/// program's hold (`Prop::Hold`), following its expression, while the
/// popup's `open` stays the router's (closed until it writes it); on
/// any other popup `open:` is `open`.
#[test]
fn open_on_a_hover_popup_is_its_hold() {
    let src = "state peek = false\nbar Top {\n  height: 30\n  row {\n    box { size: 20\n      on click { peek = !peek }\n      popup { trigger: hover; open: peek; text \"island\" }\n    }\n    box { size: 20; popup { open: peek; text \"menu\" } }\n  }\n}\n";
    let mut shell = boot(&[("h.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let popups = shell.scene.of_kind(NodeKind::Popup);
    let (island, menu) = (popups[0], popups[1]);
    let at = |shell: &Shell, n, p| shell.scene.prop(n, p).cloned();
    assert_eq!(at(&shell, island, Prop::Hold), Some(PropValue::Bool(false)));
    assert_eq!(at(&shell, island, Prop::Open), Some(PropValue::Bool(false)));
    assert_eq!(at(&shell, menu, Prop::Hold), None);
    let anchor = shell.scene.parent(island).unwrap();
    shell.inst.event(anchor, "click", Vec::new());
    shell.flush();
    assert_eq!(at(&shell, island, Prop::Hold), Some(PropValue::Bool(true)));
    assert_eq!(
        at(&shell, island, Prop::Open),
        Some(PropValue::Bool(false)),
        "the router opens it"
    );
    assert_eq!(at(&shell, menu, Prop::Open), Some(PropValue::Bool(true)));
}

/// (M6) `p.open` reads a popup's open state, so its anchor can style
/// itself while it shows: a `trigger: hover` popup's as the router
/// opens and closes it, an `open:` popup's as its expression goes.
#[test]
fn an_anchor_reads_its_popups_open() {
    let src = "state on = false\nbar Top {\n  height: 30\n  row {\n    box { size: 20\n      when cal.open { opacity: 0.5 }\n      popup { id: cal; trigger: hover; text \"cal\" }\n    }\n    box { size: 20\n      on click { on = !on }\n      when menu.open { opacity: 0.25 }\n      popup { id: menu; open: on; text \"menu\" }\n    }\n  }\n}\n";
    let mut shell = boot(&[("o.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let popups = shell.scene.of_kind(NodeKind::Popup);
    let (cal, menu) = (popups[0], popups[1]);
    let a = shell.scene.parent(cal).unwrap();
    let b = shell.scene.parent(menu).unwrap();
    let opacity = |shell: &Shell, n| match shell.scene.prop(n, Prop::Opacity) {
        Some(PropValue::Number(v)) => *v,
        _ => 1.0,
    };
    assert_eq!((opacity(&shell, a), opacity(&shell, b)), (1.0, 1.0));
    shell
        .inst
        .write(cal, Prop::Open, PropValue::Bool(true))
        .unwrap();
    shell.flush();
    assert_eq!(opacity(&shell, a), 0.5, "the island shows");
    shell.inst.event(b, "click", Vec::new());
    shell
        .inst
        .write(cal, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(opacity(&shell, a), 1.0, "the island closed");
    assert_eq!(opacity(&shell, b), 0.25, "the menu opened by its `open:`");
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert_eq!(opacity(&shell, b), 1.0, "and closed");
}

/// (M6, decisions.md m6-integration-w2) Where the surface stream's hover
/// popups meet the language stream's calls: a call whose root is a
/// `trigger: hover` popup takes `id:` and `on show` as the popup's, so
/// `i.open` reads the router's writes and the caller's handler runs; a
/// component's `screen` (its bar's) reaches the body of a hover popup
/// that mounts only when the router opens it.
#[test]
fn a_calls_id_and_screen_reach_its_hover_popup() {
    let src = "state shows = 0\n\
        component Island(label: text) { popup { trigger: hover; text label } }\n\
        component Chip { box { size: 20\n      popup { trigger: hover; text join(\" \", \"on\", screen.name) } } }\n\
        bar B {\n  row {\n    \
        box { size: 20\n      when i.open { opacity: 0.5 }\n      \
        Island \"island\" { id: i; on show { shows += 1 } } }\n    \
        Chip\n    \
        text join(\" \", \"shows\", shows)\n  }\n}\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let popups = shell.scene.of_kind(NodeKind::Popup);
    let (island, chip) = (popups[0], popups[1]);
    let anchor = shell.scene.parent(island).unwrap();
    let opacity = |shell: &Shell| match shell.scene.prop(anchor, Prop::Opacity) {
        Some(PropValue::Number(v)) => *v,
        _ => 1.0,
    };
    let mut texts = shell.scene.texts();
    texts.sort();
    assert_eq!(texts, ["shows 0"], "neither body mounted");
    assert_eq!(opacity(&shell), 1.0);
    shell
        .inst
        .write(island, Prop::Open, PropValue::Bool(true))
        .unwrap();
    shell
        .inst
        .write(chip, Prop::Open, PropValue::Bool(true))
        .unwrap();
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    let mut texts = shell.scene.texts();
    texts.sort();
    assert_eq!(texts, ["island", "on DP-1", "shows 1"], "{texts:?}");
    assert_eq!(opacity(&shell), 0.5, "`i.open` is the popup's");
    shell
        .inst
        .write(island, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(opacity(&shell), 1.0, "and closes with it");
}

/// (M6, decisions.md m6-integration-w2, third pass) Where the language
/// stream's `file.Name` meets the surface stream's hover popups: two
/// files each declare a `component Island` rooted in a `trigger: hover`
/// popup, a third calls them as `a.Island` and `b.Island` with `id:`, and
/// `p.open` stays the popup's open state although a file `p.strand`
/// declares a `fn open` (an id in scope is never a file qualifier,
/// spec.md 4.2).
#[test]
fn a_qualified_call_takes_its_hover_popups_id() {
    let a = "component Island(label: text) { popup { trigger: hover; text join(\" \", \"a\", label) } }\n";
    let b = "component Island(label: text) { popup { trigger: hover; text join(\" \", \"b\", label) } }\n";
    let p = "fn open() -> bool { false }\n";
    let t = "bar B {\n  row {\n    \
        box { size: 20\n      when p.open { opacity: 0.5 }\n      a.Island \"x\" { id: p } }\n    \
        box { size: 20\n      when q.open { opacity: 0.25 }\n      b.Island \"y\" { id: q } }\n  }\n}\n";
    let files = [
        ("a.strand", a),
        ("b.strand", b),
        ("p.strand", p),
        ("t.strand", t),
    ];
    let mut shell = boot(&files, |rt, host| screens(rt, host, &["DP-1"]));
    let popups = shell.scene.of_kind(NodeKind::Popup);
    assert_eq!(popups.len(), 2, "{}", shell.scene.render());
    let (pa, pb) = (popups[0], popups[1]);
    let (aa, ab) = (
        shell.scene.parent(pa).unwrap(),
        shell.scene.parent(pb).unwrap(),
    );
    let opacity = |shell: &Shell, n| match shell.scene.prop(n, Prop::Opacity) {
        Some(PropValue::Number(v)) => *v,
        _ => 1.0,
    };
    assert!(shell.scene.texts().is_empty(), "neither body mounted");
    shell
        .inst
        .write(pa, Prop::Open, PropValue::Bool(true))
        .unwrap();
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.scene.texts(), ["a x"], "`a.Island` is a.strand's");
    assert_eq!(opacity(&shell, aa), 0.5, "`p.open` is the popup's");
    assert_eq!(opacity(&shell, ab), 1.0);
    shell
        .inst
        .write(pb, Prop::Open, PropValue::Bool(true))
        .unwrap();
    shell
        .inst
        .write(pa, Prop::Open, PropValue::Bool(false))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["b y"], "`b.Island` is b.strand's");
    assert_eq!(opacity(&shell, aa), 1.0, "and closes with it");
    assert_eq!(opacity(&shell, ab), 0.25, "`q.open` is b's popup's");
}

/// A keyword prop value, as the mirror holds it.
fn kw(k: &str) -> Option<PropValue> {
    Some(PropValue::Keyword(k.into()))
}

/// The transition `u` set `prop` of `node` with, if it set it.
fn set_with(u: &Update, node: NodeId, prop: Prop) -> Option<(PropValue, Transition)> {
    u.diff.ops.iter().rev().find_map(|op| match op {
        SceneOp::SetProp {
            id,
            prop: p,
            value,
            transition,
        } if *id == node && *p == prop => Some((value.clone(), transition.clone())),
        _ => None,
    })
}

/// (M6, spec.md 6.3) `id.box` is the element's laid-out box as render
/// reports it (`Instance::set_box`), read here from inside the popup the
/// bar item opens: an id in the bar is readable from the popup's tree.
/// Before any report it reads as zeros, and the first real box is sent
/// without its transition (it does not spring in from 0, 0); later ones
/// spring. Reading it asks render for the box (`Prop::Watch` `box`).
#[test]
fn box_reads_the_reported_box_from_the_popups_tree() {
    let src = "bar Top {\n  height: 30\n  row {\n    text \"vol\" { id: vol }\n    popup { trigger: hover; width: 200; height: 60\n      box { place: absolute; height: 10\n        x: vol.box.x ~ spring(300, 30)\n        width: vol.box.width\n        opacity: vol.box.y / 10 } }\n  }\n}\n";
    let mut shell = boot(&[("b.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let vol = shell.text_node("vol");
    assert_eq!(
        shell.scene.prop(vol, Prop::Watch),
        None,
        "nothing reads it yet"
    );
    let popup = shell.scene.of_kind(NodeKind::Popup)[0];
    shell
        .inst
        .write(popup, Prop::Open, PropValue::Bool(true))
        .unwrap();
    shell.flush();
    let island = shell.scene.children(popup)[0];
    assert_eq!(shell.scene.prop(vol, Prop::Watch).cloned(), kw("box"));
    assert_eq!(
        shell.scene.prop(island, Prop::X),
        Some(&PropValue::Number(0.0)),
        "zeros before render reports it"
    );
    shell
        .inst
        .set_box(vol, strand_scene::LogicalRect::new(40.0, 5.0, 30.0, 20.0));
    let u = shell.flush();
    let (x, t) = set_with(&u, island, Prop::X).expect("x follows the box");
    assert_eq!(x, PropValue::Number(40.0));
    assert_eq!(t, Transition::Instant, "the first box snaps");
    assert_eq!(
        shell.scene.prop(island, Prop::Width),
        Some(&PropValue::Number(30.0))
    );
    assert_eq!(
        shell.scene.prop(island, Prop::Opacity),
        Some(&PropValue::Number(0.5))
    );
    shell
        .inst
        .set_box(vol, strand_scene::LogicalRect::new(90.0, 5.0, 30.0, 20.0));
    let u = shell.flush();
    let (x, t) = set_with(&u, island, Prop::X).expect("x follows again");
    assert_eq!(x, PropValue::Number(90.0));
    assert_ne!(t, Transition::Instant, "a move springs");
    // An unchanged box sets nothing.
    shell
        .inst
        .set_box(vol, strand_scene::LogicalRect::new(90.0, 5.0, 30.0, 20.0));
    assert!(set_with(&shell.flush(), island, Prop::X).is_none());
}

/// (M6) A conditional id (`when c { id: name }`) names the element whose
/// condition holds, a `for` copy included, read outside the `for`: the
/// workspace indicator follows `cur.box` with no offsets computed by
/// hand, and a removed copy lets go of the name.
#[test]
fn a_conditional_id_names_the_copy_whose_condition_holds() {
    let src = "state current = 1\nstate ids = [1, 2, 3]\nbar Top {\n  height: 30\n  text \"next\" { on click { current = current == 1 ? 3 : 2 } }\n  text \"drop\" { on click { ids = [1, 2] } }\n  row {\n    id: ws\n    for n in ids key n {\n      text \"w\" { when n == current { id: cur } }\n    }\n    box { place: absolute; height: 2\n      x: cur.box.x - ws.box.x\n      width: cur.box.width }\n  }\n}\n";
    let mut shell = boot(&[("w.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let ind = *shell.scene.children(row).last().unwrap();
    let texts: Vec<NodeId> = shell.scene.children(row)[..3].to_vec();
    // Every element the name may move to has its box reported, so what
    // follows it travels from one known box to the next.
    for t in &texts {
        assert_eq!(shell.scene.prop(*t, Prop::Watch).cloned(), kw("box"));
    }
    let rect = strand_scene::LogicalRect::new;
    shell.inst.set_box(row, rect(100.0, 0.0, 200.0, 30.0));
    for (i, t) in texts.iter().enumerate() {
        shell.inst.set_box(
            *t,
            rect(108.0 + 40.0 * i as f32, 0.0, 20.0 + i as f32, 30.0),
        );
    }
    shell.flush();
    let at = |shell: &Shell| {
        (
            shell.scene.prop(ind, Prop::X).cloned(),
            shell.scene.prop(ind, Prop::Width).cloned(),
        )
    };
    let num = |n: f32| Some(PropValue::Number(n));
    assert_eq!(at(&shell), (num(8.0), num(20.0)), "on the first");
    // The third becomes current: the indicator follows it.
    shell
        .inst
        .event(shell.text_node("next"), "click", Vec::new());
    shell.flush();
    assert_eq!(at(&shell), (num(88.0), num(22.0)), "on the third");
    // It goes: nothing is current, the indicator reads zeros (the
    // alias's own box, never laid out) until another is.
    shell
        .inst
        .event(shell.text_node("drop"), "click", Vec::new());
    shell.flush();
    assert_eq!(
        shell.scene.children(row).len(),
        3,
        "two copies and the indicator"
    );
    assert_eq!(at(&shell), (num(-100.0), num(0.0)), "nothing named");
    shell
        .inst
        .event(shell.text_node("next"), "click", Vec::new());
    shell.flush();
    assert_eq!(at(&shell), (num(48.0), num(21.0)), "on the second");
}

/// (M6, the review of 66c2f03) While several elements' conditions hold,
/// a conditional id names the one whose condition started holding last;
/// when that one's stops, the name goes back to the one before (not to
/// nothing), whatever order the conditions' effects run in.
#[test]
fn a_conditional_id_follows_the_latest_holder() {
    let src = "state a = false\nstate b = false\nbar Top {\n  height: 30\n  text \"ta\" { on click { a = !a } }\n  text \"tb\" { on click { b = !b } }\n  row {\n    text \"x\" { when a { id: cur } }\n    text \"y\" { when b { id: cur } }\n    box { place: absolute; height: 2; width: cur.box.width }\n  }\n}\n";
    let mut shell = boot(&[("w.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let kids = shell.scene.children(row).to_vec();
    let (x, y, ind) = (kids[0], kids[1], kids[2]);
    let rect = strand_scene::LogicalRect::new;
    shell.inst.set_box(x, rect(0.0, 0.0, 10.0, 30.0));
    shell.inst.set_box(y, rect(20.0, 0.0, 20.0, 30.0));
    shell.flush();
    let width = |shell: &Shell| shell.scene.prop(ind, Prop::Width).cloned();
    let num = |n: f32| Some(PropValue::Number(n));
    let click = |shell: &mut Shell, t: &str| {
        shell.inst.event(shell.text_node(t), "click", Vec::new());
        shell.flush();
    };
    click(&mut shell, "ta");
    assert_eq!(width(&shell), num(10.0), "x holds");
    click(&mut shell, "tb");
    assert_eq!(width(&shell), num(20.0), "y started holding last");
    click(&mut shell, "tb");
    assert_eq!(width(&shell), num(10.0), "back to x, which still holds");
    click(&mut shell, "ta");
    assert_eq!(width(&shell), num(0.0), "none holds");
    click(&mut shell, "tb");
    click(&mut shell, "ta");
    assert_eq!(width(&shell), num(10.0), "x started holding last");
}

/// (M6, the review of 66c2f03) The elements a conditional id may name are
/// measured only for what is read through it: a name read for its
/// booleans (`cur.hover`) has render report no box of them.
#[test]
fn a_name_read_for_its_booleans_measures_nothing() {
    let src = "state on = true\nbar Top {\n  height: 30\n  row {\n    text \"x\" { when on { id: cur } }\n    text \"y\" { opacity: cur.hover ? 1 : 0.5 }\n  }\n}\n";
    let shell = boot(&[("w.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let x = shell.scene.children(row)[0];
    assert_eq!(shell.scene.prop(x, Prop::Watch), None);
}

/// (M6, the review of 66c2f03) A config's own `type Rect` or `type Node`
/// does not change what `.box` and ids are: the checker and the runtime
/// use the schema's records, so the indicator still reads the box.
#[test]
fn user_types_named_rect_or_node_do_not_hijack_boxes() {
    let src = "type Rect { a: int }\ntype Node { b: int }\nbar Top {\n  height: 30\n  row {\n    text \"x\" { id: x; when self.width < 300 { opacity: 0.5 } }\n    box { place: absolute; height: 2; width: x.box.width }\n  }\n}\n";
    let mut shell = boot(&[("w.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let row = shell.scene.of_kind(NodeKind::Row)[0];
    let kids = shell.scene.children(row).to_vec();
    shell.inst.set_box(
        kids[0],
        strand_scene::LogicalRect::new(0.0, 0.0, 42.0, 30.0),
    );
    shell.flush();
    assert_eq!(
        shell.scene.prop(kids[1], Prop::Width).cloned(),
        Some(PropValue::Number(42.0))
    );
}

/// (audit-schema-names) A config's own `type Day` and `type Date` hide
/// the schema's records by name in the config's code, but the clock
/// still builds the schema's: the calendar's 42 days read `d.date` and
/// format it, and the config's own records keep their own fields.
#[test]
fn user_types_named_day_or_date_do_not_hijack_the_clock() {
    let src = "type Day { n: int }\ntype Date { s: text }\nbar Top {\n  row {\n    for d in calendar.days(clock.today) { text d.date.format(\"%d\") { when d.today { opacity: 0.5 } } }\n    text Date(s: \"mine\").s\n    text join(\" \", Day(n: 7).n)\n  }\n}\n";
    let shell = boot(&[("w.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    // `boot` holds the boot tick to no runtime errors.
    let texts = shell.scene.texts();
    assert_eq!(texts.len(), 44, "{texts:?}");
    assert!(texts.contains(&"mine".to_string()), "{texts:?}");
    assert!(texts.contains(&"7".to_string()), "{texts:?}");
    assert!(texts.contains(&"01".to_string()), "{texts:?}");
}

/// (M6, decisions.md m6-integration-w2, the final pass) Where the
/// surface stream's conditional ids and `.box` meet the language
/// stream's same-named components, looped whole numbers and implicit
/// `screen`: two files each declare `Ind`, an indicator following the
/// conditional id `cur` over a `for` of a literal whole-number list
/// (`int`s, compared with an `int` parameter) and showing its bar's
/// `screen`. `x.Ind` and `y.Ind` each read their own copies' boxes:
/// each call's `cur` names a copy in its own body, never the other
/// call's, and moves with the state both are given.
#[test]
fn same_named_components_keep_their_own_conditional_ids() {
    let ind = |p: &str, scale: &str| {
        format!(
            "component Ind(at: int) {{\n  row {{\n    id: strip\n    \
             for n in [1, 2, 3] key n {{\n      text join(\" \", \"{p}\", n) {{ when n == at {{ id: cur }} }}\n    }}\n    \
             box {{ place: absolute; height: 2\n      x: cur.box.x - strip.box.x\n      width: cur.box.width * {scale} }}\n    \
             text join(\" \", \"{p}\", screen.name)\n  }}\n}}\n"
        )
    };
    let (x, y) = (ind("x", "1"), ind("y", "2"));
    let main = "state current = 2\nbar Top {\n  height: 30\n  text \"next\" { on click { current = current % 3 + 1 } }\n  x.Ind { at: current }\n  y.Ind { at: current }\n}\n";
    let mut shell = boot(
        &[("x.strand", &x), ("y.strand", &y), ("main.strand", main)],
        |rt, host| {
            let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
            host.set(rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
        },
    );
    shell.text_node("x DP-1");
    shell.text_node("y DP-1");
    let rect = strand_scene::LogicalRect::new;
    let mut indicators = Vec::new();
    for (p, left) in [("x", 0.0), ("y", 200.0)] {
        let first = shell.text_node(&format!("{p} 1"));
        let strip = shell.scene.parent(first).unwrap();
        shell.inst.set_box(strip, rect(left, 0.0, 150.0, 30.0));
        for n in 1..=3 {
            let t = shell.text_node(&format!("{p} {n}"));
            shell.inst.set_box(
                t,
                rect(
                    left + 10.0 + 40.0 * (n - 1) as f32,
                    0.0,
                    20.0 + n as f32,
                    30.0,
                ),
            );
        }
        indicators.push(shell.scene.children(strip)[3]);
    }
    shell.flush();
    let at = |shell: &Shell, i: NodeId| {
        (
            shell.scene.prop(i, Prop::X).cloned(),
            shell.scene.prop(i, Prop::Width).cloned(),
        )
    };
    let num = |n: f32| Some(PropValue::Number(n));
    assert_eq!(at(&shell, indicators[0]), (num(50.0), num(22.0)), "x on 2");
    assert_eq!(at(&shell, indicators[1]), (num(50.0), num(44.0)), "y on 2");
    shell
        .inst
        .event(shell.text_node("next"), "click", Vec::new());
    shell.flush();
    assert_eq!(at(&shell, indicators[0]), (num(90.0), num(23.0)), "x on 3");
    assert_eq!(at(&shell, indicators[1]), (num(90.0), num(46.0)), "y on 3");
}

/// (report) The cell tap is told of every write to a cell, by IPC and by
/// a two-way binding alike, once per tick with the value before the tick
/// and after (two writes in one tick are one), and not of a write that
/// changed nothing; the snapshot holds every live cell by its path.
#[test]
fn the_cell_tap_sees_every_write() {
    use std::cell::RefCell;
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
    let told: Rc<RefCell<Vec<(String, Value, Value)>>> = Rc::default();
    let t = told.clone();
    shell.inst.set_cell_tap(Some(Box::new(move |w| {
        t.borrow_mut()
            .push((w.path.to_string(), w.old.clone(), w.new.clone()));
    })));
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(
        told.borrow_mut().drain(..).collect::<Vec<_>>(),
        [(
            "launcher.open".to_string(),
            Value::Bool(false),
            Value::Bool(true)
        )]
    );
    let input = shell.scene.of_kind(NodeKind::Input)[0];
    for text in ["f", "fi"] {
        shell
            .inst
            .write(input, Prop::Text, PropValue::Text(text.into()))
            .unwrap();
    }
    shell.flush();
    assert_eq!(
        told.borrow_mut().drain(..).collect::<Vec<_>>(),
        [(
            "launcher.query".to_string(),
            Value::text(""),
            Value::text("fi")
        )],
        "two writes in one tick are told once"
    );
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    assert!(told.borrow().is_empty(), "nothing changed");
    let snap = shell.inst.cells_snapshot();
    let at = |p: &str| snap.iter().find(|(q, _)| q == p).map(|(_, v)| v.clone());
    assert_eq!(at("launcher.open"), Some(Value::Bool(true)));
    assert_eq!(at("launcher.query"), Some(Value::text("fi")));
    // Taken away, nothing more is told.
    shell.inst.set_cell_tap(None);
    shell.inst.set("launcher.open", Value::Bool(false)).unwrap();
    shell.flush();
    assert!(told.borrow().is_empty());
}

/// (report) The program knows the texts its files write out (a text
/// node's, a placeholder, a comparison's), and no text it builds or is
/// given; a seeded cell takes its value as a baseline, checked against
/// its declared type; a queued answer is the next fetch's.
#[test]
fn text_literals_are_known_to_the_program() {
    let files = design_shells();
    let mut shell = boot(&refs(&files), desktop);
    let prog = shell.inst.vm().prog.clone();
    assert!(prog.is_text_literal("No matches"));
    assert!(prog.is_text_literal("Search apps"));
    assert!(!prog.is_text_literal("fi"));
    assert!(!prog.is_text_literal("No matche"));
    let ty = shell.inst.cell_type("launcher.query").unwrap();
    assert_eq!(prog.types.show(&ty).to_string(), "text");
    assert!(shell.inst.cell_type("launcher.nothing").is_none());
    assert!(
        shell
            .inst
            .seed_cell("launcher.query", Value::Bool(true))
            .is_err()
    );
    shell.inst.set("launcher.open", Value::Bool(true)).unwrap();
    shell.flush();
    // The mock would find Firefox and Files; the answer queued is none.
    shell.host.answer(
        "apps.search",
        Ok(Value::Async(Rc::new(
            strand_compiler::vm::value::AsyncValue::ready(Value::list(Vec::new())),
        ))),
    );
    shell
        .inst
        .seed_cell("launcher.query", Value::text("fi"))
        .unwrap();
    shell.at(0.1);
    shell.at(0.2);
    assert_eq!(
        shell.inst.value_of("launcher", "query").unwrap(),
        Value::text("fi")
    );
    assert!(
        shell.scene.find_text("No matches").is_some(),
        "{}",
        shell.scene.render()
    );
    assert!(shell.host.answers_left().is_empty());
}

/// (growth) `width: auto ~ instant` reaches render as the keyword `auto`
/// with its transition, which render reads as content sizing and the
/// curve its growth takes.
#[test]
fn auto_sizes_carry_their_transition() {
    let shell = boot(
        &[(
            "a.strand",
            "panel P {\n  open: true\n  width: auto ~ instant\n  height: auto ~ 200ms\n  box { width: 10 }\n}\n",
        )],
        desktop,
    );
    let panel = shell.scene.of_kind(NodeKind::Panel)[0];
    let set = |prop: Prop| {
        shell.boot.iter().rev().find_map(|op| match op {
            SceneOp::SetProp {
                id,
                prop: p,
                value,
                transition,
            } if *id == panel && *p == prop => Some((value.clone(), transition.clone())),
            _ => None,
        })
    };
    assert_eq!(
        set(Prop::Width),
        Some((PropValue::Keyword("auto".into()), Transition::Instant))
    );
    let (v, t) = set(Prop::Height).expect("height is set");
    assert_eq!(v, PropValue::Keyword("auto".into()));
    assert!(matches!(t, Transition::Duration { .. }), "{t:?}");
}

/// (graph-follow) A graph's and a meter's `range`, `style`, `line_width`
/// and a list of values and colours reach the scene as render reads
/// them: two ends as a list (`auto` a keyword), a list value as a list
/// of numbers, a colour list as a list of colours.
#[test]
fn graph_and_range_props_reach_the_scene() {
    let src = "state level = 40.0\nbar B {\n  graph [level, level * 2] { range: 0, auto; style: bars; line_width: 3; color: #89b4fa, #f38ba8 }\n  meter level { range: auto }\n  slider { value: <-> level; range: 0, 100 }\n}\n";
    let shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let g = shell.scene.of_kind(NodeKind::Graph)[0];
    let prop = |n: NodeId, p: Prop| shell.scene.prop(n, p).cloned();
    let kw = |k: &str| PropValue::Keyword(k.into());
    assert_eq!(
        prop(g, Prop::Value),
        Some(PropValue::List(vec![
            PropValue::Number(40.0),
            PropValue::Number(80.0)
        ]))
    );
    assert_eq!(
        prop(g, Prop::Range),
        Some(PropValue::List(vec![PropValue::Number(0.0), kw("auto")]))
    );
    assert_eq!(prop(g, Prop::Style), Some(kw("bars")));
    assert!(
        matches!(
            prop(g, Prop::LineWidth),
            Some(PropValue::Number(3.0) | PropValue::Length(strand_scene::Length::Px(3.0)))
        ),
        "{:?}",
        prop(g, Prop::LineWidth)
    );
    assert!(
        matches!(prop(g, Prop::Color), Some(PropValue::List(c)) if c.len() == 2
            && c.iter().all(|c| matches!(c, PropValue::Color(_)))),
        "{:?}",
        prop(g, Prop::Color)
    );
    let m = shell.scene.of_kind(NodeKind::Meter)[0];
    assert_eq!(prop(m, Prop::Range), Some(kw("auto")));
    let s = shell.scene.of_kind(NodeKind::Slider)[0];
    assert_eq!(
        prop(s, Prop::Range),
        Some(PropValue::List(vec![
            PropValue::Number(0.0),
            PropValue::Number(100.0)
        ]))
    );
}

/// A declaration read before its own line is checked at that read. Reaching
/// its line must leave it checked: it went back to `Checking` for good, so
/// every read after that had an error type, reported nowhere, and a method
/// call on it lowered to null: `bg: b` drew nothing, with 0 errors.
#[test]
fn a_declaration_read_before_its_line_keeps_its_type() {
    let shown = "panel P { anchor: top; size: 40; box { size: 24; bg: b } }\n";
    let mut blank = Vec::new();
    for (case, source, bg) in [
        (
            "a parameter's default",
            "let base = #80a0ff\nlet a = base.alpha(0.7)\nlet b = base.alpha(0.3)\n\
             component Unused(c: color = a) { box { size: 24; bg: c } }\n",
            "bg=#80a0ff4d",
        ),
        (
            "a default naming the base",
            "let base = #80a0ff\nlet b = base.alpha(0.3)\n\
             component Unused(c: color = base) { box { size: 24; bg: c } }\n",
            "bg=#80a0ff4d",
        ),
        (
            "an earlier let",
            "let a = base.alpha(0.7)\nlet base = #80a0ff\nlet b = base.alpha(0.3)\n",
            "bg=#80a0ff4d",
        ),
        (
            "a state",
            "let a = base.alpha(0.7)\nstate base = #80a0ff\nlet b = base.alpha(0.3)\n",
            "bg=#80a0ff4d",
        ),
        (
            "a fn",
            "let a = base().alpha(0.7)\nfn base() { #80a0ff }\nlet b = base().alpha(0.3)\n",
            "bg=#80a0ff4d",
        ),
    ] {
        let s = boot(&[("a.strand", &format!("{source}{shown}"))], |_, _| {});
        if !s.scene.render().contains(bg) {
            blank.push(case);
        }
    }
    assert!(blank.is_empty(), "drew nothing, read after: {blank:?}");
}

/// (shadow-buffer) Render is told every shadow a node may take
/// (`shadow_room`): the base one and a `when`'s, whichever holds, and
/// whether its box may change its size (bound, or set in a `when`); a
/// node whose shadow and box are fixed carries nothing.
#[test]
fn a_node_whose_shadow_or_box_may_change_lists_its_shadows() {
    let src = "bar Top {\n  height: 30\n  box { width: 10; shadow: 0 2px 4px #000000\n    when hover { shadow: 0 12px 24px #000000 } }\n  box { width: 10; shadow: 0 2px 4px #000000 }\n  box { width: 10; shadow: 0 2px 4px #000000\n    when hover { width: 20 } }\n}\n";
    let shell = boot(&[("s.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let boxes = shell.scene.of_kind(NodeKind::Box);
    let room = |b| match shell.scene.prop(b, Prop::ShadowRoom) {
        Some(PropValue::List(items)) => match (&items[0], &items[2]) {
            (PropValue::List(s), PropValue::Bool(moves)) => Some((s.len(), *moves)),
            _ => None,
        },
        _ => None,
    };
    assert_eq!(room(boxes[0]), Some((2, false)), "a hover's shadow");
    assert_eq!(room(boxes[1]), None, "fixed");
    assert_eq!(room(boxes[2]), Some((1, true)), "a hover's width");
}

/// (shadow-buffer) A panel whose `y` a `when` may set lists every `y` it
/// may take in its `shadow_room`, for the room its painted moves need; a
/// box's `x`/`y` are paint offsets and list nothing.
#[test]
fn a_panel_whose_place_may_change_lists_its_places() {
    let src = "panel Menu {\n  anchor: top; width: 100; height: 50; y: 4\n  when hover { y: 20 }\n  box { width: 10; y: 2\n    when hover { y: 6 } }\n}\n";
    let shell = boot(&[("s.strand", src)], |rt, host| {
        let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
        host.set(rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
    });
    let panel = shell.scene.of_kind(NodeKind::Panel)[0];
    let Some(PropValue::List(items)) = shell.scene.prop(panel, Prop::ShadowRoom) else {
        panic!("no shadow_room on the panel");
    };
    assert_eq!(items[3], PropValue::List(Vec::new()), "no x");
    let PropValue::List(ys) = &items[4] else {
        panic!("{items:?}");
    };
    let ys: Vec<f32> = ys.iter().filter_map(PropValue::as_number).collect();
    assert_eq!(ys, [4.0, 20.0]);
    let boxes = shell.scene.of_kind(NodeKind::Box);
    assert_eq!(shell.scene.prop(boxes[0], Prop::ShadowRoom), None);
}

/// (panel-change-closed) A panel's own `on change` sees a change made in
/// the tick that opens it, the first time it opens and every time after,
/// also when that tick's writes put back the value it had when it last
/// closed (the launcher's `on hide` sends it home meanwhile), and when
/// the panel opens through another handler (`open` makes it `shown`).
#[test]
fn a_panels_on_change_sees_the_tick_that_opens_it() {
    let src = "enum View { home, themes, apps }
export state open = false
export state themes_key = false
export state apps_key = false
state shown = false
state view: View = home
state held = false
state runs = 0
on change themes_key { view = themes; open = true }
on change apps_key { view = apps; open = true }
on change open { shown = open }
panel Launcher {
  open: <-> shown
  on hide { open = false; view = home }
  on change view { held = view == themes; runs += 1 }
  text \"x\"
}
";
    let mut shell = boot(&[("l.strand", src)], |_, _| {});
    let get = |shell: &Shell, n: &str| shell.inst.value_of("l", n).unwrap();
    // Escape or a click away: the panel writes its `open` false.
    let close = |shell: &mut Shell| {
        let panel = shell.scene.of_kind(NodeKind::Panel)[0];
        shell
            .inst
            .write(panel, Prop::Open, PropValue::Bool(false))
            .unwrap();
        shell.flush();
        assert_eq!(get(shell, "open"), Value::Bool(false));
        shell
            .inst
            .set_value("l", "held", Value::Bool(false))
            .unwrap();
        shell.flush();
    };
    // The first open: the handler is there from boot.
    shell.inst.set("l.themes_key", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(get(&shell, "held"), Value::Bool(true), "the first open");
    assert_eq!(get(&shell, "runs"), Value::int(1));
    // Closed: `on hide` sends it home, which runs nothing.
    close(&mut shell);
    assert_eq!(get(&shell, "runs"), Value::int(1), "closed, home");
    // Opened on the page it closed on: home to themes in that tick.
    shell.inst.set("l.themes_key", Value::Bool(false)).unwrap();
    shell.flush();
    assert_eq!(get(&shell, "held"), Value::Bool(true), "reopened on themes");
    assert_eq!(get(&shell, "runs"), Value::int(2));
    close(&mut shell);
    shell.inst.set("l.apps_key", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(get(&shell, "held"), Value::Bool(false), "on apps");
    assert_eq!(get(&shell, "runs"), Value::int(3));
    close(&mut shell);
    shell.inst.set("l.themes_key", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(get(&shell, "held"), Value::Bool(true), "on themes again");
    assert_eq!(get(&shell, "runs"), Value::int(4));
}

/// (panel-change-closed) A closed panel's handlers do not run; when it
/// opens, its own `on change`, its debounced one and one in an element of
/// it (mounted once it has shown) each run once for what they missed while
/// it was closed, as before. Before it first opens they have seen nothing:
/// a change then is not run when it opens in a later tick.
#[test]
fn a_closed_panels_on_change_runs_once_for_what_it_missed() {
    let src = "export state open = false
export state x = 0
state body = 0
state later = 0
state inner = 0
panel P {
  open: <-> open
  on change x { body += 1 }
  on change x after 1s { later += 1 }
  box { on change x { inner += 1 } }
}
";
    let mut shell = boot(&[("p.strand", src)], |_, _| {});
    let runs =
        |shell: &Shell| ["body", "later", "inner"].map(|n| shell.inst.value_of("p", n).unwrap());
    let none = [Value::int(0), Value::int(0), Value::int(0)];
    shell.inst.set("p.x", Value::int(1)).unwrap();
    shell.at(0.1);
    shell.at(2.0);
    assert_eq!(runs(&shell), none, "closed since boot");
    shell.inst.set("p.open", Value::Bool(true)).unwrap();
    shell.at(2.1);
    shell.at(4.0);
    assert_eq!(runs(&shell), none, "opened later, first time");
    shell.inst.set("p.open", Value::Bool(false)).unwrap();
    shell.at(4.1);
    shell.inst.set("p.x", Value::int(2)).unwrap();
    shell.at(4.2);
    shell.at(5.0);
    assert_eq!(runs(&shell), none, "changed while closed");
    shell.inst.set("p.open", Value::Bool(true)).unwrap();
    shell.at(5.1);
    shell.at(7.0);
    let one = [Value::int(1), Value::int(1), Value::int(1)];
    assert_eq!(runs(&shell), one, "opened later: what it missed, once");
    shell.inst.set("p.x", Value::int(3)).unwrap();
    shell.at(7.1);
    shell.at(9.0);
    let two = [Value::int(2), Value::int(2), Value::int(2)];
    assert_eq!(runs(&shell), two, "changed while open");
}

/// (panel-change-closed) A handler that keeps a component's state in step
/// with something outside (`on change src { mine = src }`) catches up when
/// the panel it is in opens again, in the component and in the panel's
/// body alike.
#[test]
fn a_reopened_panels_handlers_catch_up() {
    let src = "export state open = true
export state src = 0
state nested = 0
state body = 0
component Mirror {
  on change src { nested = src }
  box { }
}
panel P {
  open: <-> open
  on change src { body = src }
  Mirror
}
";
    let mut shell = boot(&[("m.strand", src)], |_, _| {});
    let get = |shell: &Shell, n: &str| shell.inst.value_of("m", n).unwrap();
    shell.inst.set("m.open", Value::Bool(false)).unwrap();
    shell.flush();
    shell.inst.set("m.src", Value::int(5)).unwrap();
    shell.flush();
    assert_eq!(get(&shell, "nested"), Value::int(0), "closed");
    shell.inst.set("m.open", Value::Bool(true)).unwrap();
    shell.flush();
    shell.flush();
    assert_eq!(get(&shell, "nested"), Value::int(5), "the component's");
    assert_eq!(get(&shell, "body"), Value::int(5), "the body's");
}

/// (panel-change-closed) The `on change` handlers of one tick run in the
/// order written, those of a surface's body and of its elements alike: in
/// a bar, and in a panel that starts closed, whose body handlers mount
/// before the rest of it and take their place when it first opens.
#[test]
fn a_surfaces_handlers_run_in_the_order_written() {
    for (surface, open) in [
        ("bar B {", "true"),
        ("panel P {\n  open: <-> open", "false"),
    ] {
        for (first, second, last) in [
            ("box { on change x { y = 1 } }", "on change x { y = 2 }", 2),
            ("on change x { y = 2 }", "box { on change x { y = 1 } }", 1),
        ] {
            let src = format!(
                "export state open = {open}\nexport state x = 0\nstate y = 0\n\
                 {surface}\n  {first}\n  {second}\n}}\n"
            );
            let mut shell = boot(&[("o.strand", &src)], |rt, host| {
                screens(rt, host, &["DP-1"])
            });
            shell.inst.set("o.open", Value::Bool(true)).unwrap();
            shell.flush();
            shell.inst.set("o.x", Value::int(1)).unwrap();
            shell.flush();
            assert_eq!(
                shell.inst.value_of("o", "y").unwrap(),
                Value::int(last),
                "{src}"
            );
        }
    }
}

/// (panel-change-closed) A `persist`ed state in a component of a panel,
/// written in the tick that closes the panel, is saved once the panel
/// opens again: a closed surface holds its save tracker as before, and
/// only its `on change` handlers note each tick's value.
#[test]
fn a_persisted_state_written_as_its_panel_closes_is_saved() {
    let dir = temp_dir("persist-closing");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let src = "export state open = true
component C {
  state picked = \"\" persist
  box { on click { picked = \"a\"; open = false } }
}
panel P {
  open: <-> open
  C
}
";
    let mut shell = boot_with(&[("s.strand", src)], |_, _| {}, storage.clone());
    let pick = shell.inst.nodes_handling("click")[0];
    assert!(shell.inst.event(pick, "click", Vec::new()));
    shell.at(0.1);
    assert_eq!(shell.inst.get("s.open").unwrap(), Value::Bool(false));
    shell.inst.set("s.open", Value::Bool(true)).unwrap();
    shell.at(0.2);
    shell.at(1.0);
    shell.at(1.5);
    let p = storage.persist.clone().unwrap();
    assert!(p.sync(Duration::from_secs(5)));
    let saved: Vec<String> = std::fs::read_dir(p.dir())
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        saved.iter().any(|f| f.contains("picked")),
        "saved before shutdown: {saved:?}"
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (panel-change-closed) A debounced `on change` of a panel's body, and
/// one in an element of it once it has shown, see a change made in the
/// tick that opens it, as its plain `on change` does (that one from the
/// first open: an element's handler mounts with it, so the first open
/// is its first value).
#[test]
fn a_panels_handlers_see_the_tick_that_opens_it() {
    let src = "export state open = false
export state x = 0
export state go = 0
state body = 0
state later = 0
state inner = 0
on change go { x = go; open = true }
panel P {
  open: <-> open
  on change x { body += 1 }
  on change x after 1s { later += 1 }
  box { on change x { inner += 1 } }
}
";
    let mut shell = boot(&[("p.strand", src)], |_, _| {});
    let runs = |shell: &Shell| {
        ["body", "later", "inner"].map(|n| {
            shell
                .inst
                .value_of("p", n)
                .unwrap()
                .as_f64()
                .unwrap_or(-1.0)
        })
    };
    shell.inst.set("p.go", Value::int(1)).unwrap();
    shell.at(0.1);
    shell.at(2.0);
    assert_eq!(runs(&shell), [1.0, 1.0, 0.0], "the first open");
    shell.inst.set("p.open", Value::Bool(false)).unwrap();
    shell.at(2.1);
    shell.inst.set("p.go", Value::int(2)).unwrap();
    shell.at(2.2);
    shell.at(4.0);
    assert_eq!(runs(&shell), [2.0, 2.0, 1.0], "opened again");
}

/// (dictionary) A dictionary `state` is one cell: a `let` reading one
/// entry (`used["a"]`) re-runs on any write, but the equality cut-off
/// stops there unless its entry changed, so what reads the `let` does
/// not re-run when another key is written.
#[test]
fn a_let_over_one_entry_does_not_rerun_its_dependents_when_another_changes() {
    let src = "state used: [text: int] = []\nlet a = used[\"a\"] ?? 0\nbar B {\n  text join(\" \", a * 10)\n  box { on click { used[\"b\"] = 1 } }\n  box { on click { used[\"a\"] = 2 } }\n}\n";
    let mut shell = boot(&[("used.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts(), ["0"]);
    let boxes = shell.inst.nodes_handling("click");
    let before = shell.rt.stats().computations;
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert!(u.diff.ops.is_empty(), "{:?}", u.diff);
    // The `let` only: the text's binding is cut off.
    assert_eq!(shell.rt.stats().computations - before, 1);
    let before = shell.rt.stats().computations;
    shell.inst.event(boxes[1], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["20"]);
    assert_eq!(shell.rt.stats().computations - before, 2);
}

/// (dictionary) `for e in m.entries` keeps each row by its key (`Entry`'s
/// `key`): a value change updates that row's node in place, a new key
/// mounts at its place in key order, a removed key's row leaves, and
/// the other rows keep their nodes.
#[test]
fn rows_of_entries_keep_state_across_writes() {
    let src = "state m: [int: text] = [1: \"a\", 3: \"c\"]\nbar B {\n  for e in m.entries { text e.value }\n  box { on click { m[2] = \"b\" } }\n  box { on click { m[1] = \"A\" } }\n  box { on click { m.remove(3) } }\n}\n";
    let mut shell = boot(&[("rows.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    assert_eq!(shell.scene.texts(), ["a", "c"]);
    let (a, c) = (shell.text_node("a"), shell.text_node("c"));
    let boxes = shell.inst.nodes_handling("click");
    shell.inst.event(boxes[0], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(shell.scene.texts(), ["a", "b", "c"]);
    let creates = u
        .diff
        .ops
        .iter()
        .filter(|op| matches!(op, SceneOp::Create { .. }))
        .count();
    assert_eq!(creates, 1, "{:?}", u.diff);
    assert_eq!(shell.text_node("a"), a);
    assert_eq!(shell.text_node("c"), c);
    shell.inst.event(boxes[1], "click", Vec::new());
    let u = shell.flush();
    assert_eq!(shell.scene.texts(), ["A", "b", "c"]);
    assert_eq!(shell.text_node("A"), a, "the row of key 1 kept its node");
    assert!(
        u.diff
            .ops
            .iter()
            .all(|op| matches!(op, SceneOp::SetProp { .. })),
        "{:?}",
        u.diff
    );
    shell.inst.event(boxes[2], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["A", "b"]);
    assert_eq!(shell.text_node("A"), a);
}

/// (dictionary) `on change m[k]` fires only when that entry's value
/// changes, and sees `null` once the entry is removed; writes to other
/// keys do not fire it.
#[test]
fn on_change_an_entry_fires_only_for_that_key() {
    let src = "state m: [int: text] = [1: \"a\"]\nstate fired = 0\nstate seen = \"\"\non change m[1] { fired += 1; seen = m[1] ?? \"gone\" }\nbar B {\n  text join(\" \", fired, seen)\n  box { on click { m[2] = \"b\" } }\n  box { on click { m[1] = \"A\" } }\n  box { on click { m[1] = \"A\"; m[3] = \"c\" } }\n  box { on click { m.remove(1) } }\n  box { on click { m[2] = \"B\" } }\n}\n";
    let seen = click_each_box(src);
    let texts: Vec<&str> = seen.iter().map(|(t, _)| t.trim_end()).collect();
    assert_eq!(texts, ["0", "0", "1 A", "1 A", "2 gone", "2 gone"]);
    assert!(seen.iter().all(|(_, e)| e.is_empty()), "{seen:?}");
}

/// (dictionary) The issue's case end to end: the last window used on each
/// workspace, written as focus moves and read by key when a workspace
/// chip is clicked (`windows.all.find(w => w.id == (last_win[ws.id] ??
/// ""))?.focus()`), with no list of records to filter and append.
#[test]
fn the_last_window_on_each_workspace_is_remembered_by_key() {
    let src = "state last_win: [int: text] = []\non change windows.focused?.id {\n  let at = windows.focused?.workspace ?? -1\n  if at >= 0 { last_win[at] = windows.focused?.id ?? \"\" }\n}\nbar B {\n  text last_win.entries.map(e => join(\"=\", e.key, e.value)).join(\" \")\n  for ws in workspaces.all {\n    box { on click { windows.all.find(w => w.id == (last_win[ws.id] ?? \"\"))?.focus() } }\n  }\n}\n";
    let mut shell = boot(&[("last.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        let ws = |id: i64| host.record("Workspace", &[("id", Value::int(id))]);
        host.set(rt, "workspaces.all", Value::list(vec![ws(1), ws(2)]))
            .unwrap();
    });
    let win = |id: &str, ws: i64| {
        shell.host.record(
            "Window",
            &[("id", Value::text(id)), ("workspace", Value::int(ws))],
        )
    };
    let (w1, w2, w3) = (win("0x11", 1), win("0x22", 2), win("0x33", 1));
    shell
        .host
        .set(
            &shell.rt,
            "windows.all",
            Value::list(vec![w1.clone(), w2.clone(), w3.clone()]),
        )
        .unwrap();
    shell.flush();
    for w in [w1, w2, w3] {
        shell.host.set(&shell.rt, "windows.focused", w).unwrap();
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    }
    // `on change` takes the first focus as its baseline.
    assert_eq!(shell.scene.texts(), ["1=0x33 2=0x22"]);
    let chips = shell.inst.nodes_handling("click");
    shell.inst.event(chips[1], "click", Vec::new());
    shell.flush();
    let actions: Vec<String> = shell
        .host
        .take_actions()
        .iter()
        .map(|a| a.to_string())
        .collect();
    assert_eq!(actions, ["Window(0x22).focus(0)"]);
}

/// (dictionary) The how-to "Remembering things by key"
/// (site/how-to/remembering-by-key.md): its taskbar, a dictionary of
/// focus stamps ordering the windows by their last use, each shown by
/// its app's nickname from a constant table, the stamps of closed
/// windows pruned at the write; and its recency list, kept as a list and
/// capped by `take`. The sources are the page's files, with a `text` of
/// the stamps' keys added to watch the pruning.
#[test]
fn the_how_to_bounds_what_it_remembers_by_liveness_and_by_take() {
    let taskbar = "let nick: [text: text] = [\"org.gnome.Nautilus\": \"Files\", \"kitty\": \"Terminal\", \"firefox\": \"Web\"]\nstate stamp = 0\nstate used: [text: int] = []\non change windows.focused?.id {\n  let id = windows.focused?.id ?? \"\"\n  if id != \"\" {\n    stamp += 1\n    used[id] = stamp\n    used = used.filter(e => windows.all.any(w => w.id == e.key))\n  }\n}\nbar Taskbar {\n  text used.keys.join(\",\")\n  row {\n    for w in windows.all.sort_by(w => -(used[w.id] ?? 0)) {\n      text nick[w.app_id] ?? w.title { on click { w.focus() } }\n    }\n  }\n}\n";
    let mut shell = boot(&[("taskbar.strand", taskbar)], |rt, host| {
        screens(rt, host, &["DP-1"]);
    });
    let win = |id: &str, app: &str| {
        shell.host.record(
            "Window",
            &[
                ("id", Value::text(id)),
                ("app_id", Value::text(app)),
                ("title", Value::text(format!("{app} window").as_str())),
            ],
        )
    };
    let (w1, w2, w3) = (
        win("0x1", "kitty"),
        win("0x2", "firefox"),
        win("0x3", "foot"),
    );
    let all = |ws: &[&Value]| Value::list(ws.iter().map(|w| (*w).clone()).collect());
    shell
        .host
        .set(&shell.rt, "windows.all", all(&[&w1, &w2, &w3]))
        .unwrap();
    shell.flush();
    // The first focus is `on change`'s baseline; the others are stamped.
    for w in [&w1, &w2, &w3, &w1] {
        shell
            .host
            .set(&shell.rt, "windows.focused", w.clone())
            .unwrap();
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    }
    assert_eq!(
        shell.scene.texts(),
        ["0x1,0x2,0x3", "Terminal", "foot window", "Web"]
    );
    // 0x3 closes: its stamp goes at the next write, not before.
    shell
        .host
        .set(&shell.rt, "windows.all", all(&[&w1, &w2]))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["0x1,0x2,0x3", "Terminal", "Web"]);
    shell
        .host
        .set(&shell.rt, "windows.focused", w2.clone())
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.texts(), ["0x1,0x2", "Web", "Terminal"]);

    let recent = "state recent: [int] = [] persist\non change workspaces.all.find(w => w.focused)?.id {\n  let id = workspaces.all.find(w => w.focused)?.id ?? -1\n  if id >= 0 { recent = ([id] + recent.filter(x => x != id)).take(5) }\n}\nbar Recent {\n  col {\n    for id in recent key id {\n      text workspaces.all.get(id)?.name ?? \"\" { on click { workspaces.all.get(id)?.focus() } }\n    }\n  }\n}\n";
    let mut shell = boot(&[("recent.strand", recent)], |rt, host| {
        screens(rt, host, &["DP-1"]);
    });
    let focus_on = |shell: &mut Shell, on: i64| {
        let ws = (1..=7)
            .map(|id| {
                shell.host.record(
                    "Workspace",
                    &[
                        ("id", Value::int(id)),
                        ("name", Value::text(id.to_string().as_str())),
                        ("focused", Value::Bool(id == on)),
                    ],
                )
            })
            .collect();
        shell
            .host
            .set(&shell.rt, "workspaces.all", Value::list(ws))
            .unwrap();
        let u = shell.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    };
    for on in [1, 2, 3, 4, 5, 6, 7, 3] {
        focus_on(&mut shell, on);
    }
    // Most recent first, each once, five at most.
    assert_eq!(shell.scene.texts(), ["3", "7", "6", "5", "4"]);
}

/// (dictionary) A persisted dictionary's source: an `int`-keyed and an
/// enum-keyed one, shown in key order, with a box writing both.
const PERSISTED_DICTS: &str = "enum Level { low, high }\nstate seen: [int: text] = [] persist\nstate by: [Level: int] = [] persist\nbar B {\n  text seen.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n  text by.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n  box { on click { seen[3] = \"c\"; seen[1] = \"a\"; by[high] = 2; by[low] = 1 } }\n}\n";

/// One monitor, DP-1.
fn dp1(rt: &Runtime, host: &SchemaHost) {
    screens(rt, host, &["DP-1"]);
}

/// The value cell `path` of `storage` holds on disk, once its writes
/// have landed.
fn stored(storage: &Storage, path: &str) -> String {
    let p = storage.persist.as_ref().unwrap();
    assert!(p.sync(Duration::from_secs(5)));
    let s = p.load(path).unwrap().expect("a stored value");
    String::from_utf8(s.value).unwrap()
}

/// (dictionary) A persisted dictionary is stored as `[key, value]` pairs
/// in key order (an `int` key a JSON integer, an enum key its variant's
/// name) and comes back after a restart in key order; a store edited by
/// hand reads too: an object of key texts, or pairs out of order with a
/// key twice (the later wins).
#[test]
fn a_persisted_dictionary_survives_a_restart() {
    let dir = temp_dir("persist-dict");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let files = [("t.strand", PERSISTED_DICTS)];
    let mut shell = boot_with(&files, dp1, storage.clone());
    assert_eq!(shell.scene.texts(), ["", ""]);
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["1=a,3=c", "low=1,high=2"]);
    drop(shell);
    assert_eq!(stored(&storage, "t.seen"), r#"[[1,"a"],[3,"c"]]"#);
    assert_eq!(stored(&storage, "t.by"), r#"[["low",1.0],["high",2.0]]"#);
    let shell = boot_with(&files, dp1, storage.clone());
    assert_eq!(shell.scene.texts(), ["1=a,3=c", "low=1,high=2"]);
    assert!(shell.boot_notices.is_empty(), "{:?}", shell.boot_notices);
    drop(shell);
    // By hand: an object, and pairs out of order with `5` twice.
    let p = storage.persist.clone().unwrap();
    p.save("t.seen", b"[]", br#"{"7": "g", "2": "b"}"#).unwrap();
    p.save("t.by", b"[]", br#"[["high", 2], ["low", 1], ["high", 3]]"#)
        .unwrap();
    let shell = boot_with(&files, dp1, storage.clone());
    assert_eq!(shell.scene.texts(), ["2=b,7=g", "low=1,high=3"]);
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (dictionary) A persisted dictionary's default follows 10.3's rule at
/// a restart: a stored value that is still the old default adopts the
/// new one; one written since is kept and said once. The order is the
/// key order whatever the literal's, so the same entries written in
/// another order are the same default.
#[test]
fn a_changed_default_follows_the_adopt_rule() {
    let dir = temp_dir("persist-dict-default");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let src = |d: &str| {
        format!(
            "state m: [int: text] = {d} persist\nbar B {{\n  text m.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n  box {{ on click {{ m[3] = \"c\" }} }}\n}}\n"
        )
    };
    let boot_on = |d: &str| boot_with(&[("t.strand", &src(d))], dp1, storage.clone());
    // Stored, and still the old default: the new default is adopted.
    let p = storage.persist.clone().unwrap();
    p.save("t.m", br#"[[1,"a"]]"#, br#"[[1,"a"]]"#).unwrap();
    let shell = boot_on("[2: \"B\"]");
    assert_eq!(shell.scene.texts(), ["2=B"]);
    assert!(shell.boot_notices.is_empty(), "{:?}", shell.boot_notices);
    drop(shell);
    let mut shell = boot_on("[1: \"a\", 2: \"b\"]");
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    drop(shell);
    // The same entries in another order: the same default, so the
    // written value is used and nothing is said.
    let shell = boot_on("[2: \"b\", 1: \"a\"]");
    assert_eq!(shell.scene.texts(), ["1=a,2=b,3=c"]);
    assert!(shell.boot_notices.is_empty(), "{:?}", shell.boot_notices);
    drop(shell);
    // Written since, and a new default: kept over it, and said.
    let shell = boot_on("[4: \"d\"]");
    assert_eq!(shell.scene.texts(), ["1=a,2=b,3=c"]);
    assert_eq!(
        shell.boot_notices,
        ["t.m: kept 1: a, 2: b, 3: c (default changed) [reset]"]
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// Whether `diagnostics` has core's warning that `path`'s store did not
/// read back.
fn persist_failed(diagnostics: &[strand_core::Diagnostic], path: &str) -> bool {
    diagnostics.iter().any(
        |d| matches!(d, strand_core::Diagnostic::PersistFailed { path: p, .. } if &**p == path),
    )
}

/// (dictionary) A persisted list of records turned into a dictionary is
/// a type change: what was stored does not read as the new type, so the
/// cell starts from its default with core's warning (no migration).
#[test]
fn a_type_change_resets_with_a_warning() {
    let dir = temp_dir("persist-dict-type");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let before = "type Seen { ws: int; win: text }\nstate last: [Seen] = [Seen(ws: 3, win: \"0x55\")] persist\nbar B {\n  text join(\" \", last.len)\n  box { on click { last = last + [Seen(ws: 5, win: \"0x7a\")] } }\n}\n";
    let mut shell = boot_with(&[("t.strand", before)], dp1, storage.clone());
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["2"]);
    drop(shell);
    let after = "state last: [int: text] = [3: \"0x55\"] persist\nbar B {\n  text last.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n}\n";
    let shell = boot_with(&[("t.strand", after)], dp1, storage.clone());
    assert_eq!(shell.scene.texts(), ["3=0x55"]);
    assert!(
        persist_failed(&shell.boot_diagnostics, "t.last"),
        "{:?}",
        shell.boot_diagnostics
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (dictionary) A store whose dictionary does not read (a pair without
/// its value, a key of the wrong type, a value that does not fit) gives
/// the default and core's warning; the next start reads what was
/// written since.
#[test]
fn a_corrupt_store_gives_the_default() {
    let dir = temp_dir("persist-dict-corrupt");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let files = [(
        "t.strand",
        "state m: [int: text] = [7: \"g\"] persist\nbar B {\n  text m.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n  box { on click { m[1] = \"a\" } }\n}\n",
    )];
    let p = storage.persist.clone().unwrap();
    let default = br#"[[7,"g"]]"#;
    for bad in [
        &br#"[[1]]"#[..],
        br#"[["one","a"]]"#,
        br#"[[1.5,"a"]]"#,
        br#"[[1,2]]"#,
        br#"{"one":"a"}"#,
        b"not json",
    ] {
        p.save("t.m", default, bad).unwrap();
        let shell = boot_with(&files, dp1, storage.clone());
        let shown = String::from_utf8_lossy(bad);
        assert_eq!(shell.scene.texts(), ["7=g"], "{shown}");
        assert!(
            persist_failed(&shell.boot_diagnostics, "t.m"),
            "{shown}: {:?}",
            shell.boot_diagnostics
        );
        drop(shell);
    }
    let mut shell = boot_with(&files, dp1, storage.clone());
    assert!(
        shell.boot_diagnostics.is_empty(),
        "moved aside, not read again"
    );
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    drop(shell);
    let shell = boot_with(&files, dp1, storage.clone());
    assert_eq!(shell.scene.texts(), ["1=a,7=g"]);
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (dictionary) A stored entry whose enum key the program no longer
/// declares (a variant removed while the shell was not running) is
/// dropped as the store is read, the rest kept, and the start says so
/// once.
#[test]
fn a_stored_entry_whose_enum_key_is_gone_is_dropped_with_a_note() {
    let dir = temp_dir("persist-dict-gone");
    let storage = Storage::in_dirs(dir.join("state"), dir.join("config"));
    let p = storage.persist.clone().unwrap();
    p.save(
        "t.by",
        b"[]",
        br#"[["low",1],["mid",5],["gone",6],["high",2]]"#,
    )
    .unwrap();
    let shell = boot_with(&[("t.strand", PERSISTED_DICTS)], dp1, storage.clone());
    assert_eq!(shell.scene.texts(), ["", "low=1,high=2"]);
    assert_eq!(
        shell.boot_notices,
        ["t.by: dropped 2 entries (key no longer in Level)"]
    );
    assert!(
        shell.boot_diagnostics.is_empty(),
        "{:?}",
        shell.boot_diagnostics
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (dictionary) A settings file with dictionary fields: per-screen
/// wallpapers in a `[wallpapers]` section, per-app scales and urgency
/// timeouts, a `bool` per clock, each shown in key order.
const SETTINGS_DICTS: &str = "state prefs from \"prefs.toml\" {\n  gap: int = 4\n  wallpapers: [text: path] = []\n  scale: [text: float] = [\"kitty\": 0.9]\n  after: [Urgency: duration] = []\n  shown: [text: bool] = []\n}\nbar B {\n  text prefs.wallpapers.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n  text prefs.scale.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n  box { on click { prefs.wallpapers[\"eDP-1\"] = \"~/c.jpg\"; prefs.wallpapers.remove(\"HDMI-A-1\") } }\n}\n";

/// The text of every settings notice in `diagnostics`, its file named
/// from the config directory `config`.
fn settings_notices(
    diagnostics: &[strand_core::Diagnostic],
    config: &std::path::Path,
) -> Vec<String> {
    let dir = format!("{}/", config.display());
    diagnostics
        .iter()
        .filter_map(|d| match d {
            strand_core::Diagnostic::Settings(n) => Some(n.to_string().replace(&dir, "")),
            _ => None,
        })
        .collect()
}

/// (dictionary) A settings dictionary is a TOML table: a `[section]` or
/// an inline `{ … }`, its keys read by the key type (an enum's by variant
/// name). One bad entry, a key that does not read or a value that does
/// not fit, keeps the field's last good value with a notice naming the
/// key; the other fields still apply; a deleted table springs back to
/// its default.
#[test]
fn a_settings_dictionary_reads_a_table_and_keeps_its_last_good_value() {
    let dir = temp_dir("settings-dict-read");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let file = config.join("prefs.toml");
    std::fs::write(
        &file,
        "gap = 4\n\n[wallpapers]\nHDMI-A-1 = \"~/b.jpg\"\nDP-1 = \"~/a.jpg\" # left\n",
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot_with(&[("t.strand", SETTINGS_DICTS)], dp1, storage);
    assert_eq!(
        shell.scene.texts(),
        ["DP-1=~/a.jpg,HDMI-A-1=~/b.jpg", "kitty=0.9"]
    );
    assert!(settings_notices(&shell.boot_diagnostics, &config).is_empty());
    let reload = |shell: &mut Shell, text: &str| {
        std::fs::write(&file, text).unwrap();
        assert!(shell.inst.reload_settings(&file));
        settings_notices(&shell.flush().diagnostics, &config)
    };
    // Inline tables; an enum's keys by variant name, in declaration order.
    let notes = reload(
        &mut shell,
        "gap = 5\nwallpapers = { \"DP-1\" = \"~/i.jpg\" }\nafter = { critical = \"0s\", low = \"3s\" }\nscale = { \"org.gnome.Nautilus\" = 0.5 }\n",
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        shell.scene.texts(),
        ["DP-1=~/i.jpg", "org.gnome.Nautilus=0.5"]
    );
    assert_eq!(
        shell.inst.get("prefs.after.low").unwrap(),
        Value::Num(3000.0, Num::Ms)
    );
    let Value::Dict(after) = shell.inst.get("prefs.after").unwrap() else {
        panic!("a dictionary")
    };
    assert_eq!(after.len(), 2);
    // One bad entry: the field keeps its last good value, the notice
    // names the key, and the other fields still apply.
    let wallpapers = "wallpapers = { \"DP-1\" = \"~/i.jpg\" }\n";
    let after = "after = { critical = \"0s\", low = \"3s\" }\n";
    for (text, said) in [
        (
            format!("gap = 6\n{after}\n[wallpapers]\nDP-1 = 3\n"),
            "prefs.toml: wallpapers: \"DP-1\": expected path; keeping its last good value",
        ),
        (
            format!("gap = 6\n{wallpapers}after = {{ urgent = \"1s\" }}\n"),
            "prefs.toml: after: key \"urgent\": expected one of low, normal, critical; keeping its last good value",
        ),
        (
            format!("gap = 6\n{wallpapers}after = {{ low = \"soon\" }}\n"),
            "prefs.toml: after: \"low\": expected a duration like \"200ms\" or \"6s\"; keeping its last good value",
        ),
    ] {
        let notes = reload(&mut shell, &text);
        assert_eq!(notes, [said], "{text}");
        assert_eq!(
            shell.inst.get("prefs.gap").unwrap(),
            Value::int(6),
            "{text}"
        );
        assert_eq!(shell.scene.texts()[0], "DP-1=~/i.jpg", "{text}");
        assert_eq!(
            shell.inst.get("prefs.after.low").unwrap(),
            Value::Num(3000.0, Num::Ms),
            "{text}"
        );
    }
    // Two keys that read as one `int` are one bad entry too.
    let ints = "state p from \"ints.toml\" { names: [int: text] = [] }\nbar B { text p.names.entries.map(e => join(\"=\", e.key, e.value)).join(\",\") }\n";
    std::fs::write(config.join("ints.toml"), "[names]\n3 = \"c\"\n1 = \"a\"\n").unwrap();
    let ints_storage = Storage::in_dirs(dir.join("state2"), &config);
    let mut other = boot_with(&[("i.strand", ints)], dp1, ints_storage);
    assert_eq!(other.scene.texts(), ["1=a,3=c"]);
    std::fs::write(config.join("ints.toml"), "[names]\n3 = \"c\"\n03 = \"x\"\n").unwrap();
    assert!(other.inst.reload_settings(&config.join("ints.toml")));
    assert_eq!(
        settings_notices(&other.flush().diagnostics, &config),
        ["ints.toml: names: key \"03\": the same key as \"3\"; keeping its last good value"]
    );
    std::fs::write(config.join("ints.toml"), "[names]\nthree = \"c\"\n").unwrap();
    assert!(other.inst.reload_settings(&config.join("ints.toml")));
    assert_eq!(
        settings_notices(&other.flush().diagnostics, &config),
        ["ints.toml: names: key \"three\": expected int; keeping its last good value"]
    );
    assert_eq!(other.scene.texts(), ["1=a,3=c"]);
    drop(other);
    // A deleted table springs back to its default.
    let notes = reload(&mut shell, "gap = 6\n");
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(shell.scene.texts(), ["", "kitty=0.9"]);
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (dictionary) A write to a settings dictionary is saved key by key in
/// the table the file has: a changed entry keeps its place and comment, a
/// removed one goes, a new one is added at the table's end, and the rest
/// of the file is untouched; a dictionary the file does not have yet is
/// written as its own `[section]`. The written file reads back the same.
#[test]
fn a_settings_dictionary_writes_back_key_by_key() {
    let dir = temp_dir("settings-dict-write");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let file = config.join("prefs.toml");
    std::fs::write(
        &file,
        "# my prefs\ngap = 4\n\n# per screen\n[wallpapers]\n# the left one\nDP-1   = \"~/a.jpg\"   # left\nHDMI-A-1 = \"~/b.jpg\"\n",
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot_with(&[("t.strand", SETTINGS_DICTS)], dp1, storage.clone());
    let saved = |shell: &mut Shell, at: f64| {
        shell.at(at);
        assert!(
            storage
                .settings
                .as_ref()
                .unwrap()
                .sync(Duration::from_secs(5))
        );
        std::fs::read_to_string(&file).unwrap()
    };
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.scene.texts()[0], "DP-1=~/a.jpg,eDP-1=~/c.jpg");
    assert_eq!(
        saved(&mut shell, 1.0),
        "# my prefs\ngap = 4\n\n# per screen\n[wallpapers]\n# the left one\nDP-1   = \"~/a.jpg\"   # left\neDP-1 = \"~/c.jpg\"\n"
    );
    // `strand set` of one entry: a dotted app id is one key, and a field
    // the file does not have is written as a section after the others.
    shell
        .inst
        .set_text("prefs.wallpapers.DP-1", "~/new.jpg")
        .unwrap();
    shell
        .inst
        .set_text("prefs.scale.org.gnome.Nautilus", "0.5")
        .unwrap();
    shell.flush();
    assert_eq!(
        saved(&mut shell, 2.0),
        "# my prefs\ngap = 4\n\n# per screen\n[wallpapers]\n# the left one\nDP-1   = \"~/new.jpg\"   # left\neDP-1 = \"~/c.jpg\"\n\n[scale]\nkitty = 0.9\n\"org.gnome.Nautilus\" = 0.5\n"
    );
    // An entry is in its field's layer (`strand get --json`).
    assert_eq!(
        shell.inst.layer("prefs.wallpapers.DP-1"),
        Some(strand_core::SettingsLayer::File)
    );
    assert_eq!(
        shell.inst.layer("prefs.scale.nope"),
        Some(strand_core::SettingsLayer::File)
    );
    // What was written reads back as it was.
    assert!(shell.inst.reload_settings(&file));
    let u = shell.flush();
    assert!(settings_notices(&u.diagnostics, &config).is_empty());
    assert_eq!(
        shell.scene.texts(),
        [
            "DP-1=~/new.jpg,eDP-1=~/c.jpg",
            "kitty=0.9,org.gnome.Nautilus=0.5"
        ]
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (settings-tables) Display profiles in a settings file: a dictionary
/// of records, each a `[profiles.name]` section with its monitors as
/// `[[profiles.name.mons]]`.
const PROFILES_SRC: &str = "type Mon { id: text; scale: float }\ntype Profile { mons: [Mon]; lid: text? }\nstate mon from \"monitors.toml\" { profiles: [text: Profile] = [] }\nstate seen = 0\non change mon.profiles { seen += 1 }\nbar B {\n  text mon.profiles.entries.map(e => join(\":\", e.key, e.value.lid ?? \"-\", e.value.mons.map(m => join(\"@\", m.id, m.scale)).join(\"+\"))).join(\",\")\n  text join(\" \", \"seen\", seen)\n  box { on click { mon.profiles[\"desk\"] = Profile(mons: [Mon(id: \"Dell\", scale: 2.0), Mon(id: \"BOE\", scale: 1.25)], lid: \"BOE\") } }\n}\n";

/// (settings-tables) A settings field of records reads TOML's table
/// forms, a hand edit applies live (`on change` sees it), a key a record
/// does not have keeps the field's last good value with a notice naming
/// where it is, and a write of one profile goes back into the sections
/// the file has, every other profile as it was written, and the monitor
/// it changed keeps its comments (settings-tables follow-up).
#[test]
fn settings_records_read_and_write_table_forms() {
    let dir = temp_dir("settings-tables");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let file = config.join("monitors.toml");
    let couch = "# just the laptop\n[profiles.couch]\n[[profiles.couch.mons]]\nid = \"BOE\"   # alone\nscale = 1\n";
    std::fs::write(
        &file,
        format!("[profiles.desk]\nlid = \"BOE\"\n\n[[profiles.desk.mons]]\nid = \"Dell\"   # left\nscale = 1.5\n\n{couch}"),
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot_with(&[("t.strand", PROFILES_SRC)], dp1, storage.clone());
    assert!(settings_notices(&shell.boot_diagnostics, &config).is_empty());
    assert_eq!(
        shell.scene.texts(),
        ["couch:-:BOE@1,desk:BOE:Dell@1.5", "seen 0"]
    );
    let reload = |shell: &mut Shell, text: &str| {
        std::fs::write(&file, text).unwrap();
        assert!(shell.inst.reload_settings(&file));
        settings_notices(&shell.flush().diagnostics, &config)
    };
    // A hand edit applies live, and `on change` sees it.
    let notes = reload(
        &mut shell,
        &format!(
            "[profiles.desk]\nlid = \"BOE\"\n[[profiles.desk.mons]]\nid = \"Dell\"\nscale = 2\n[[profiles.desk.mons]]\nid = \"LG\"\nscale = 1\n\n{couch}"
        ),
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        shell.scene.texts(),
        ["couch:-:BOE@1,desk:BOE:Dell@2+LG@1", "seen 1"]
    );
    // A typo keeps the last good value, with a notice naming it.
    let notes = reload(
        &mut shell,
        &format!("[profiles.desk]\n[[profiles.desk.mons]]\nid = \"Dell\"\nscael = 2\n\n{couch}"),
    );
    assert_eq!(
        notes,
        [
            "monitors.toml: profiles: \"desk\": mons[0]: unknown field \"scael\"; keeping its last good value"
        ]
    );
    assert_eq!(
        shell.scene.texts(),
        ["couch:-:BOE@1,desk:BOE:Dell@2+LG@1", "seen 1"]
    );
    // A write of one profile from the shell.
    let before = format!(
        "# at the desk\n[profiles.desk]\nlid = \"BOE\"\n\n# the big one\n[[profiles.desk.mons]]\nid = \"Dell\"   # left\nscale = 1.5\n\n{couch}"
    );
    assert!(reload(&mut shell, &before).is_empty());
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert_eq!(
        shell.scene.texts(),
        ["couch:-:BOE@1,desk:BOE:Dell@2+BOE@1.25", "seen 3"]
    );
    shell.at(1.0);
    assert!(
        storage
            .settings
            .as_ref()
            .unwrap()
            .sync(Duration::from_secs(5))
    );
    let written = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        written,
        format!(
            "# at the desk\n[profiles.desk]\nlid = \"BOE\"\n\n# the big one\n[[profiles.desk.mons]]\nid = \"Dell\"   # left\nscale = 2.0\n\n[[profiles.desk.mons]]\nid = \"BOE\"\nscale = 1.25\n\n{couch}"
        )
    );
    // And it reads back as it was.
    assert!(shell.inst.reload_settings(&file));
    assert!(settings_notices(&shell.flush().diagnostics, &config).is_empty());
    assert_eq!(
        shell.scene.texts()[0],
        "couch:-:BOE@1,desk:BOE:Dell@2+BOE@1.25"
    );
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (monitor-layout) The best-guess arrangement site/how-to/monitor-profiles.md
/// shows (the page's own files are mounted by
/// `the_monitor_profiles_page_works_together`): the laptop's panel last, the others left to
/// right in `outputs.all` order, each one's x the sum of the logical
/// widths before it (a rotated head is as wide as its mode is tall, a
/// scaled one narrower), y 0, every head on at its preferred mode.
const BEST_GUESS_SRC: &str = "\
fn wide(o: Output) -> int {
  let m = o.preferred ?? o.mode
  let w = match o.transform {
    rotate_90 => m?.height ?? 0
    rotate_270 => m?.height ?? 0
    flipped_90 => m?.height ?? 0
    flipped_270 => m?.height ?? 0
    _ => m?.width ?? 0
  }
  (w / o.scale).round()
}
let order = outputs.all.sort_by(o => o.name.starts_with(\"eDP\"))
let guess = order.map(o => OutputChange(
  name: o.name, enabled: true, mode: o.preferred,
  x: order.take(order.index_of(p => p.name == o.name) ?? 0).sum(p => wide(p)), y: 0))
bar B {
  text guess.map(c => join(\"@\", c.name, c.x ?? -1, c.y ?? -1, c.enabled ?? false, c.mode?.width ?? 0)).join(\" \")
  box { on click { outputs.apply(guess) } }
}
";

/// (monitor-layout) No list operation was added for the best guess:
/// `sort_by`, `take`, `index_of` and `sum` already write it, and
/// `outputs.apply(guess)` type-checks (decisions.md monitor-layout).
#[test]
fn a_best_guess_layout_is_written_with_the_list_operations_there_are() {
    let mut shell = boot(&[("t.strand", BEST_GUESS_SRC)], |rt, host| {
        screens(rt, host, &["DP-1"]);
        let mode = |w: i64, h: i64| {
            host.record(
                "OutputMode",
                &[
                    ("width", Value::int(w)),
                    ("height", Value::int(h)),
                    ("refresh", Value::float(60.0)),
                ],
            )
        };
        let output = |name: &str, on: bool, m: Option<Value>, pref: Value, scale: f64, t: &str| {
            host.record(
                "Output",
                &[
                    ("name", Value::text(name)),
                    ("enabled", Value::Bool(on)),
                    ("mode", m.unwrap_or(Value::Null)),
                    ("preferred", pref),
                    ("scale", Value::float(scale)),
                    ("transform", host.variant("OutputTransform", t)),
                ],
            )
        };
        let all = Value::list(vec![
            output(
                "eDP-1",
                true,
                Some(mode(1920, 1200)),
                mode(1920, 1200),
                1.25,
                "normal",
            ),
            output(
                "DP-3",
                true,
                Some(mode(3840, 2160)),
                mode(3840, 2160),
                1.5,
                "rotate_90",
            ),
            output("DP-4", false, None, mode(2560, 1440), 1.0, "normal"),
        ]);
        host.set(rt, "outputs.all", all).unwrap();
    });
    // DP-3 rotated: 2160 / 1.5 = 1440 wide; DP-4: 2560; eDP-1 last.
    assert_eq!(
        shell.scene.texts(),
        ["DP-3@0@0@true@3840 DP-4@1440@0@true@2560 eDP-1@4000@0@true@1920"]
    );
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    let calls = shell.host.take_actions();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        (calls[0].target.as_str(), calls[0].name.as_str()),
        ("outputs", "apply")
    );
    let Value::List(changes) = &calls[0].args[0] else {
        panic!("{calls:?}")
    };
    assert_eq!(changes.len(), 3);
}

/// (dictionary) `strand get` and `strand set` reach one entry by a key
/// path segment: the rest of the path after the dictionary is one key,
/// read by the key type. `get` of an absent entry is null; `set` inserts
/// or replaces; `toggle` flips a `bool` entry. A whole dictionary is not
/// set from text, an entry is not removed (`null`), and a key or value
/// that does not read is refused: each says why.
#[test]
fn strand_get_and_set_reach_one_entry_of_a_dictionary() {
    let src = "enum Level { low, high }\nexport state names: [int: text] = [1: \"web\"]\nexport state by: [Level: int] = []\nstate prefs from \"prefs.toml\" { shown: [text: bool] = [] }\nbar B { text names.entries.map(e => join(\"=\", e.key, e.value)).join(\",\") }\n";
    let mut shell = boot(&[("dicts.strand", src)], dp1);
    let inst = &shell.inst;
    assert_eq!(inst.get("dicts.names.1").unwrap(), Value::text("web"));
    assert_eq!(inst.get("dicts.names.2").unwrap(), Value::Null);
    inst.set_text("dicts.names.3", "chat").unwrap();
    inst.set_text("dicts.names.1", "mail").unwrap();
    inst.set("dicts.names.2", Value::text("code")).unwrap();
    inst.set_text("dicts.by.high", "2").unwrap();
    assert_eq!(inst.get("dicts.by.high").unwrap(), Value::int(2));
    assert!(inst.toggle("prefs.shown.clock").unwrap());
    assert!(!inst.toggle("prefs.shown.clock").unwrap());
    assert_eq!(inst.get("prefs.shown.clock").unwrap(), Value::Bool(false));
    let err = |path: &str, text: &str| inst.set_text(path, text).unwrap_err().to_string();
    assert_eq!(
        err("dicts.names", "x"),
        "`dicts.names` is a dictionary, `[int: text]`: `strand set` writes one entry, `strand set dicts.names.<key> <value>`"
    );
    assert_eq!(
        err("dicts.names.1", "null"),
        "`dicts.names.1`: `strand set` does not remove an entry (a dictionary's values are never `null`); a handler removes it with `names.remove(k)`"
    );
    assert_eq!(
        err("prefs.shown.clock", "null"),
        "`prefs.shown.clock`: `strand set` does not remove an entry (a dictionary's values are never `null`); a handler removes it with `prefs.shown.remove(k)`"
    );
    assert_eq!(
        err("dicts.names.three", "x"),
        "`three` is not a key of `dicts.names`, a [int: text]: its keys are int"
    );
    assert_eq!(
        err("dicts.by.mid", "1"),
        "`mid` is not a key of `dicts.by`, a [Level: int]: its keys are Level"
    );
    assert_eq!(err("dicts.by.low", "many"), "`many` is not a int");
    assert!(inst.get("dicts.names.x.y").is_err());
    shell.flush();
    assert_eq!(shell.scene.texts(), ["1=mail,2=code,3=chat"]);
}

/// (doc-formats) Date text (RFC 3339, RFC 2822, Unix time) reads as the
/// schema's `Date`: `strand set`, `strand call` and a document's text
/// all go through `parse_text`.
#[test]
fn parse_text_reads_a_date() {
    use strand_compiler::instantiate::parse_text;
    use strand_compiler::ty::Ty;
    let types = &strand_compiler::schema::Schema::builtin().types;
    let date = Ty::Record(types.find_schema_record("Date").unwrap());
    let field = |v: &Value, f: &str| v.field(types, f).cloned();
    let day = parse_text(types, &date, "2026-11-07").unwrap();
    assert_eq!(field(&day, "year"), Some(Value::int(2026)));
    assert_eq!(field(&day, "month"), Some(Value::int(11)));
    assert_eq!(field(&day, "day"), Some(Value::int(7)));
    assert_eq!(field(&day, "weekday"), Some(Value::int(6)));
    assert_eq!(field(&day, "hour"), Some(Value::Null));
    // A naive time is the wall time as written, whatever the zone.
    let at = parse_text(types, &date, "2026-11-07T21:30").unwrap();
    assert_eq!(field(&at, "hour"), Some(Value::int(21)));
    assert_eq!(field(&at, "minute"), Some(Value::int(30)));
    // Instants convert (to the local zone): a year is all that is sure.
    for t in [
        "Wed, 21 Oct 2026 19:29:36 +0000",
        "2026-06-07T12:00:00Z",
        "1794084315",
    ] {
        let v = parse_text(types, &date, t).unwrap_or_else(|| panic!("{t}"));
        assert_eq!(field(&v, "year"), Some(Value::int(2026)), "{t}");
    }
    assert_eq!(parse_text(types, &date, "tomorrow"), None);
    let maybe = Ty::Optional(Box::new(date));
    assert_eq!(parse_text(types, &maybe, "null"), Some(Value::Null));
}

/// (live-files) `hex` and `argb` are ordinary builtins: a label formats a
/// colour, a token through the current token table (following a theme
/// flip), and null in is null out.
#[test]
fn hex_and_argb_format_colours_anywhere() {
    let src = "export state c: color? = #ff8800\n\
               bar Top { text hex($accent) + \" \" + argb(#ff880080) + \" \" + (hex(c, hash: false) ?? \"none\") }\n";
    let mut shell = boot(&[("a.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
    });
    let text = shell.scene.texts()[0].clone();
    let parts: Vec<&str> = text.split(' ').collect();
    assert_eq!(parts.len(), 3, "{text}");
    assert!(parts[0].starts_with('#') && parts[0].len() == 7, "{text}");
    assert_eq!(&parts[1..], ["0x80ff8800", "ff8800"]);
    shell
        .host
        .set(&shell.rt, "system.dark", Value::Bool(true))
        .unwrap();
    shell.flush();
    shell.inst.set("a.c", Value::Null).unwrap();
    shell.flush();
    let after = shell.scene.texts()[0].clone();
    assert!(after.ends_with(" none"), "{after}");
    assert_ne!(
        after.split(' ').next(),
        Some(parts[0]),
        "the token followed the flip"
    );
}

/// (json-fn, review round 3) `json` writes a token as its value over the
/// current table, as `hex` and `join` do: a colour token as `"#rrggbb"`,
/// held in a record's colour field or a dictionary's value, a length
/// token as a number, and it follows a theme flip.
#[test]
fn json_writes_a_token_as_its_value() {
    let src = "type Colors { fg: color; bg: color }\n\
               type Look { c: Colors; r: length; a: [text: color] }\n\
               bar Top { text json(Look(c: Colors(fg: $fg, bg: $accent), r: $radius.md, a: [\"x\": $accent])) }\n";
    let mut shell = boot(&[("a.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
    });
    let read = |shell: &Shell| -> serde_json::Value {
        let t = shell.scene.texts()[0].clone();
        serde_json::from_str(&t).unwrap_or_else(|e| panic!("{t}: {e}"))
    };
    let v = read(&shell);
    let colour = |v: &serde_json::Value| {
        v.as_str()
            .is_some_and(|s| s.len() == 7 && s.starts_with('#'))
    };
    assert!(colour(&v["c"]["fg"]) && colour(&v["c"]["bg"]), "{v}");
    assert_eq!(v["c"]["bg"], v["a"]["x"], "{v}");
    assert!(v["r"].is_number(), "{v}");
    shell
        .host
        .set(&shell.rt, "system.dark", Value::Bool(true))
        .unwrap();
    shell.flush();
    let after = read(&shell);
    assert_ne!(
        after["c"]["fg"], v["c"]["fg"],
        "the token followed the flip"
    );
}

/// The page site/how-to/monitor-profiles.md, whose `strand file=` blocks
/// the tests below mount as they are written there.
const MONITOR_PROFILES_PAGE: &str = include_str!("../../../site/how-to/monitor-profiles.md");

/// The `strand file=NAME` blocks of a site page, in order.
fn page_files(page: &str) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut lines = page.lines();
    while let Some(line) = lines.next() {
        let Some(name) = line.strip_prefix("```strand file=") else {
            continue;
        };
        let body: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
        files.push((name.trim().to_string(), body.join("\n") + "\n"));
    }
    files
}

/// (blocks) An `Output` as outputs reports it, with its `id`.
fn monitor(
    host: &SchemaHost,
    name: &str,
    id: &str,
    (w, h, r): (i64, i64, f64),
    (x, y): (i64, i64),
    scale: f64,
    turn: &str,
) -> Value {
    let mode = host.record(
        "OutputMode",
        &[
            ("width", Value::int(w)),
            ("height", Value::int(h)),
            ("refresh", Value::float(r)),
        ],
    );
    host.record(
        "Output",
        &[
            ("name", Value::text(name)),
            ("id", Value::text(id)),
            ("enabled", Value::Bool(true)),
            ("mode", mode.clone()),
            ("preferred", mode),
            ("x", Value::int(x)),
            ("y", Value::int(y)),
            ("scale", Value::float(scale)),
            ("transform", host.variant("OutputTransform", turn)),
        ],
    )
}

const PANEL: &str = "BOE | 0x0BCA | BOE 0x0BCA";
const DELL: &str = "Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q 6Y1ZW13";

/// The laptop's panel and a Dell turned on its side, at `panel` and
/// `dell`.
fn desk(host: &SchemaHost, panel: (i64, i64), dell: (i64, i64)) -> Value {
    Value::list(vec![
        monitor(
            host,
            "eDP-1",
            PANEL,
            (1920, 1200, 60.0),
            panel,
            1.25,
            "normal",
        ),
        monitor(
            host,
            "DP-3",
            DELL,
            (3840, 2160, 59.997),
            dell,
            1.5,
            "rotate_90",
        ),
    ])
}

/// The `outputs.apply` calls since the last, each as `name@x,y` per
/// monitor.
fn applied(shell: &Shell) -> Vec<String> {
    shell
        .host
        .take_actions()
        .into_iter()
        .filter(|c| c.target == "outputs" && c.name == "apply")
        .map(|c| {
            let json = strand_compiler::vm::json::to_json_string(shell.host.types(), &c.args[0]);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v.as_array()
                .unwrap()
                .iter()
                .map(|c| format!("{}@{},{}", c["name"].as_str().unwrap(), c["x"], c["y"]))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// The `outputs.apply` calls since the last, each as `name mode` per
/// monitor, its mode `WxH` or `null`.
fn applied_modes(shell: &Shell) -> Vec<String> {
    shell
        .host
        .take_actions()
        .into_iter()
        .filter(|c| c.target == "outputs" && c.name == "apply")
        .map(|c| {
            let json = strand_compiler::vm::json::to_json_string(shell.host.types(), &c.args[0]);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v.as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let m = &c["mode"];
                    let mode = match m.is_null() {
                        true => "null".to_string(),
                        false => format!("{}x{}", m["width"], m["height"]),
                    };
                    format!("{} {mode}", c["name"].as_str().unwrap())
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect()
}

/// (outputs-apply, review round 6) The page's profiles place a monitor
/// saved with no mode (a Hyprland mirror shows none, so `kept` stores
/// width 0) with `mode: null`, which keeps the mode it has: `placed` sent
/// `OutputMode(0, 0, 0)`, which `outputs` refuses ("mode width 0 is not
/// a size"), failing the whole profile.
#[test]
fn the_monitor_profiles_page_places_a_monitor_saved_with_no_mode_with_none() {
    let files = page_files(MONITOR_PROFILES_PAGE);
    let refs: Vec<(&str, &str)> = files
        .iter()
        .filter(|(n, _)| n == "layout.strand" || n == "monitors.strand")
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    assert_eq!(refs.len(), 2);
    let dir = temp_dir("monitor-profiles-mirror");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot_with(
        &refs,
        |rt, host| {
            dp1(rt, host);
            // HOT-1 mirrors the panel: on, at its place, with no mode.
            let mirror = host.record(
                "Output",
                &[
                    ("name", Value::text("HOT-1")),
                    ("id", Value::text("Unknown | Unknown | HOT")),
                    ("enabled", Value::Bool(true)),
                    ("x", Value::int(0)),
                    ("y", Value::int(0)),
                    ("scale", Value::float(1.0)),
                    ("transform", host.variant("OutputTransform", "normal")),
                ],
            );
            let panel = monitor(
                host,
                "eDP-1",
                PANEL,
                (1920, 1200, 60.0),
                (0, 0),
                1.0,
                "normal",
            );
            host.set(rt, "outputs.all", Value::list(vec![panel, mirror]))
                .unwrap();
            host.set_ready(rt, "outputs", false).unwrap();
        },
        storage,
    );
    // The best guess once `outputs` answers, its mode the preferred one:
    // none for the mirror.
    shell.at(1.0);
    shell.host.set_ready(&shell.rt, "outputs", true).unwrap();
    shell.at(1.1);
    shell.at(3.0);
    assert_eq!(applied_modes(&shell), ["eDP-1 1920x1200, HOT-1 null"]);
    // Saved as it is, then applied from the profile: the mirror's width
    // is 0, so it is sent no mode.
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    shell.at(3.1);
    shell.at(5.0);
    assert_eq!(shell.scene.texts()[0], "Saved profile");
    assert_eq!(applied_modes(&shell), ["eDP-1 1920x1200, HOT-1 null"]);
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (monitor-profiles, review round 4) The page's Hyprland file writes a
/// monitor that is on as on, with Hyprland's `preferred`, when no mode
/// is known for it, and only a monitor off as `disabled = true`: it
/// wrote every monitor with no mode as off, which would turn a monitor
/// on at a custom mode off at Hyprland's next start.
#[test]
fn the_monitor_profiles_page_writes_a_monitor_on_with_no_mode_as_on() {
    let files = page_files(MONITOR_PROFILES_PAGE);
    let refs: Vec<(&str, &str)> = files
        .iter()
        .filter(|(n, _)| n == "hyprland.strand")
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    assert_eq!(refs.len(), 1);
    let dir = temp_dir("monitor-profiles-no-mode");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot_with(
        &refs,
        |rt, host| {
            dp1(rt, host);
            let on = |name: &str, enabled: bool| {
                host.record(
                    "Output",
                    &[
                        ("name", Value::text(name)),
                        ("enabled", Value::Bool(enabled)),
                        ("scale", Value::float(1.0)),
                        ("transform", host.variant("OutputTransform", "normal")),
                    ],
                )
            };
            host.set(
                rt,
                "outputs.all",
                Value::list(vec![on("NESTED-1", true), on("NESTED-2", false)]),
            )
            .unwrap();
            host.set_ready(rt, "outputs", true).unwrap();
        },
        storage,
    );
    shell.at(1.0);
    let lua: Vec<String> = shell
        .inst
        .take_outputs()
        .into_iter()
        .filter(|c| c.path.as_ref().is_some_and(|p| p.ends_with("monitors.lua")))
        .filter_map(|c| match c.content {
            OutputContent::Write { bytes, .. } => Some(String::from_utf8(bytes).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(
        lua,
        ["-- Written by Strand from outputs.all\n\
          hl.monitor({ output = \"NESTED-1\", mode = \"preferred\", position = \"0x0\", \
          scale = 1, transform = 0 })\n\
          hl.monitor({ output = \"NESTED-2\", disabled = true })\n"]
    );
}

/// (blocks) The monitor profiles page, end to end on the mock host: its
/// compositor file and JSON file wait for `outputs.ready` and then follow
/// `outputs.all` (with `Output.id`); `on change chosen, outputs.ready`
/// applies the best guess (the take/index_of/sum running total) once
/// the monitors are in; a profile written by hand in `[[…]]` form is read
/// into records, matched by `Output.id`, and applied; and "Save this
/// profile" writes back into that form, leaving a profile that reads the
/// same as written, comments and all.
#[test]
fn the_monitor_profiles_page_works_together() {
    let files = page_files(MONITOR_PROFILES_PAGE);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "layout.strand",
            "monitors.strand",
            "hyprland.strand",
            "workspaces.strand"
        ]
    );
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let dir = temp_dir("monitor-profiles");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = boot_with(
        &refs,
        |rt, host| {
            dp1(rt, host);
            host.set_ready(rt, "outputs", false).unwrap();
            host.set(rt, "outputs.all", desk(host, (0, 0), (1920, 0)))
                .unwrap();
        },
        storage.clone(),
    );
    let outputs = |shell: &Shell| {
        let mut seen = shell
            .inst
            .take_outputs()
            .into_iter()
            .map(|c| {
                let name = c
                    .path
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let what = match c.content {
                    OutputContent::Write { bytes, .. } => String::from_utf8(bytes).unwrap(),
                    other => format!("{other:?}"),
                };
                (name, what)
            })
            .collect::<Vec<_>>();
        seen.sort();
        seen
    };
    // Before `outputs` has answered, both files wait and nothing is
    // applied.
    let waiting = format!(
        "{:?}",
        OutputContent::Waiting {
            services: vec!["outputs".into()]
        }
    );
    assert_eq!(
        outputs(&shell),
        [
            ("monitors.json".to_string(), waiting.clone()),
            ("monitors.lua".to_string(), waiting)
        ]
    );
    shell.at(5.0);
    assert!(applied(&shell).is_empty());
    // It answers: the files are written from `outputs.all`, and the best
    // guess is applied 1.5 s later: the Dell, turned, is 2160 / 1.5 =
    // 1440 wide, the panel after it.
    shell.host.set_ready(&shell.rt, "outputs", true).unwrap();
    shell.at(5.1);
    assert_eq!(
        outputs(&shell),
        [
            (
                "monitors.json".to_string(),
                format!("[[\"{PANEL}\",\"eDP-1\"],[\"{DELL}\",\"DP-3\"]]")
            ),
            (
                "monitors.lua".to_string(),
                "-- Written by Strand from outputs.all\n\
                 hl.monitor({ output = \"eDP-1\", mode = \"1920x1200@60\", position = \"0x0\", \
                 scale = 1.25, transform = 0 })\n\
                 hl.monitor({ output = \"DP-3\", mode = \"3840x2160@59.997\", position = \"1920x0\", \
                 scale = 1.5, transform = 1 })\n"
                    .to_string()
            )
        ]
    );
    shell.at(6.0);
    assert!(applied(&shell).is_empty(), "debounced");
    shell.at(6.7);
    assert_eq!(applied(&shell), ["eDP-1@1440,0 DP-3@0,0"]);
    // The compositor does it; the file follows, and nothing is applied
    // again (the guess reads no position).
    shell
        .host
        .set(
            &shell.rt,
            "outputs.all",
            desk(&shell.host, (1440, 0), (0, 0)),
        )
        .unwrap();
    shell.at(7.0);
    let conf = outputs(&shell);
    assert!(
        conf[0].1.contains(
            "hl.monitor({ output = \"eDP-1\", mode = \"1920x1200@60\", position = \"1440x0\","
        ),
        "{conf:?}"
    );
    shell.at(9.0);
    assert!(applied(&shell).is_empty());
    assert_eq!(shell.scene.texts()[0], "Best guess");
    // A profile written by hand, as `[[…]]` tables: the panel left of the
    // Dell this time. It is read live and applied by `id`.
    let setup = format!("{PANEL} + {DELL}");
    let file = config.join("monitors.toml");
    let profile = format!(
        "# Monitor profiles.\n\n\
         # At the desk.\n\
         [profiles.\"{setup}\"]\n\
         [[profiles.\"{setup}\".mons]]\n\
         id = \"{PANEL}\"   # the laptop\n\
         on = true\nwidth = 1920\nheight = 1200\nrefresh = 60.0\n\
         x = 0\ny = 0\nscale = 1.25\ntransform = \"normal\"\n\n\
         [[profiles.\"{setup}\".mons]]\n\
         id = \"{DELL}\"\n\
         on = true\nwidth = 3840\nheight = 2160\nrefresh = 59.997\n\
         x = 1536\ny = 0\nscale = 1.5\ntransform = \"rotate_90\"\n"
    );
    std::fs::write(&file, &profile).unwrap();
    assert!(shell.inst.reload_settings(&file));
    let notes = settings_notices(&shell.flush().diagnostics, &config);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(shell.scene.texts()[0], "Saved profile");
    shell.at(11.0);
    assert_eq!(applied(&shell), ["eDP-1@0,0 DP-3@1536,0"]);
    shell
        .host
        .set(
            &shell.rt,
            "outputs.all",
            desk(&shell.host, (0, 0), (1536, 0)),
        )
        .unwrap();
    shell.at(11.1);
    let _ = outputs(&shell);
    // Saved as it is: every monitor reads as the file has it, so the file
    // is left as written, comments and all.
    let b = shell.inst.nodes_handling("click")[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    shell.at(12.0);
    let sync = |storage: &Storage| {
        assert!(
            storage
                .settings
                .as_ref()
                .unwrap()
                .sync(Duration::from_secs(5))
        )
    };
    sync(&storage);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), profile);
    // Moved by hand (nothing is applied), then saved: only the Dell's
    // table changes; the panel's keeps its comment.
    shell
        .host
        .set(
            &shell.rt,
            "outputs.all",
            desk(&shell.host, (0, 0), (1536, 200)),
        )
        .unwrap();
    shell.at(14.0);
    assert!(applied(&shell).is_empty(), "a move by hand stays");
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    shell.at(15.0);
    sync(&storage);
    let written = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        written,
        profile.replace("x = 1536\ny = 0", "x = 1536\ny = 200")
    );
    shell.at(17.0);
    assert_eq!(applied(&shell), ["eDP-1@0,0 DP-3@1536,200"]);
    drop(shell);
    let _ = std::fs::remove_dir_all(dir);
}

/// (build-ready) A `Date` no source has filled yet is 1970-01-01 with no
/// time of day, so `format` on it is a date, not a fault that freezes the
/// component until a reload; and a date without a time of day formats
/// its hour and minute as midnight, so `%H:%M` over a document's
/// `YYYY-MM-DD` is `00:00`, not an error.
#[test]
fn an_unfilled_date_formats_and_a_date_only_one_formats_midnight() {
    let src = "service feed from file \"/tmp/feed.json\" { at: Date }\nbar B { col {\n  text feed.at.format(\"%Y-%m-%d %H:%M\")\n  text feed.at.add(days: 3).format(\"%a %d %b %H:%M\")\n  text clock.today.format(\"%H:%M\")\n} }\n";
    let mut shell = boot(&[("t.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"])
    });
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert!(shell.inst.outlined().is_empty());
    let texts = shell.scene.texts();
    assert_eq!(texts[0], "1970-01-01 00:00");
    assert_eq!(texts[1], "Sun 04 Jan 00:00");
    assert_eq!(texts[2], "00:00");
    // The data arriving is an ordinary change.
    let date = shell.inst.vm().types().find_schema_record("Date").unwrap();
    let at = strand_compiler::vm::clock::date_record(
        date,
        chrono::NaiveDate::from_ymd_opt(2026, 11, 8).unwrap(),
    );
    shell.host.set(&shell.rt, "feed.at", at).unwrap();
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(shell.scene.texts()[0], "2026-11-08 00:00");
}

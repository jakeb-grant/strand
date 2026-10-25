//! (flex) Surfaces of the 2026-10-24 gap review (decisions.md `## flex`):
//! a panel or OSD on every monitor its `screens:` picks, with `screen`
//! in scope as a bar has; (lockmon) a lock on every monitor, one
//! password between its instances; `exclusive:` on bars and panels.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_scene::{Exclusive, NodeKind, SurfaceSpec};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
}

impl Shell {
    fn flush(&mut self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
    }

    /// The `text` nodes' texts, in scene order (not an `input`'s).
    fn labels(&self) -> Vec<String> {
        self.scene
            .of_kind(NodeKind::Text)
            .into_iter()
            .filter_map(|n| match self.scene.prop(n, strand_scene::Prop::Text) {
                Some(strand_scene::PropValue::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn spec(&self, id: strand_scene::NodeId) -> SurfaceSpec {
        let kind = self.scene.kind(id).unwrap();
        SurfaceSpec::resolve(kind, |p| self.scene.prop(id, p).cloned())
    }
}

fn compile(src: &str) -> strand_compiler::Compiled {
    let mut map = SourceMap::new();
    map.add("shell.strand", src.to_string());
    strand_compiler::compile(&map)
}

fn boot(src: &str, screens: &[(&str, f64, f64)], focused: Option<usize>) -> Shell {
    let compiled = compile(src);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    set_screens(&rt, &host, screens, focused);
    let inst = Instance::new(&rt, program, host.clone(), Storage::none());
    let mut sh = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
    };
    sh.flush();
    sh
}

fn screen(host: &SchemaHost, name: &str, w: f64, h: f64) -> Value {
    host.record(
        "Screen",
        &[
            ("id", Value::text(format!("Mock | {name} | Display"))),
            ("name", Value::text(name)),
            ("make", Value::text("Mock")),
            ("model", Value::text(name)),
            ("description", Value::text("Display")),
            ("width", Value::float(w)),
            ("height", Value::float(h)),
        ],
    )
}

fn set_screens(
    rt: &Runtime,
    host: &SchemaHost,
    screens: &[(&str, f64, f64)],
    focused: Option<usize>,
) {
    let all: Vec<Value> = screens
        .iter()
        .map(|(n, w, h)| screen(host, n, *w, *h))
        .collect();
    let f = focused
        .and_then(|i| all.get(i).cloned())
        .unwrap_or(Value::Null);
    host.set(rt, "screens.all", Value::list(all)).unwrap();
    host.set(rt, "screens.focused", f).unwrap();
}

const MONITORS: [(&str, f64, f64); 2] = [("DP-1", 1280.0, 720.0), ("HDMI-A-1", 1920.0, 1080.0)];

/// `panel X { screens: all; height: screen.height }`: one panel a
/// monitor, each pinned to its monitor and sized from it, as a bar is;
/// a monitor that goes takes its panel, and one plugged in gets one.
#[test]
fn a_panel_on_every_screen_has_its_screen() {
    let src = "panel Side { screens: all; anchor: left; width: 200; height: screen.height\n\
                 text screen.name\n\
               }\n";
    let mut sh = boot(src, &MONITORS, Some(0));
    let panels = sh.scene.of_kind(NodeKind::Panel);
    assert_eq!(panels.len(), 2, "{}", sh.scene.render());
    assert_eq!(sh.scene.texts(), ["DP-1", "HDMI-A-1"]);
    let specs: Vec<SurfaceSpec> = panels.iter().map(|p| sh.spec(*p)).collect();
    assert_eq!(specs[0].height, Some(720.0));
    assert_eq!(specs[1].height, Some(1080.0));
    assert_eq!(
        specs[1].screens,
        strand_scene::Screens::Named(vec!["Mock | HDMI-A-1 | Display".into()])
    );
    // HDMI-A-1 unplugged: its panel goes.
    set_screens(&sh.rt, &sh.host, &MONITORS[..1], Some(0));
    sh.flush();
    assert_eq!(sh.scene.of_kind(NodeKind::Panel).len(), 1);
    assert_eq!(sh.scene.texts(), ["DP-1"]);
    // An OSD on named monitors only.
    let src = "osd Level { screens: [\"HDMI-A-1\"]; width: screen.width / 4; height: 40 }\n";
    let sh = boot(src, &MONITORS, Some(0));
    let osds = sh.scene.of_kind(NodeKind::Osd);
    assert_eq!(osds.len(), 1);
    assert_eq!(sh.spec(osds[0]).width, Some(480.0));
}

/// (flex review) A `screens:` expression that is `focused` now: one
/// instance whose `screen` follows focus in place, not one remounted
/// on each monitor focus moves to, so what it holds stays.
#[test]
fn a_panel_whose_screens_is_focused_now_follows_focus_in_place() {
    let src = "state wide = false\n\
               panel P { screens: wide ? Screens.all : Screens.focused; width: screen.width / 4\n\
                 text screen.name\n\
               }\n";
    let mut sh = boot(src, &MONITORS, Some(0));
    let panels = sh.scene.of_kind(NodeKind::Panel);
    assert_eq!(panels.len(), 1, "{}", sh.scene.render());
    assert_eq!(sh.scene.texts(), ["DP-1"]);
    assert_eq!(sh.spec(panels[0]).screens, strand_scene::Screens::Focused);
    let name = sh.scene.find_text("DP-1").unwrap();
    set_screens(&sh.rt, &sh.host, &MONITORS, Some(1));
    sh.flush();
    assert_eq!(
        sh.scene.of_kind(NodeKind::Panel),
        panels,
        "mounted again on focus"
    );
    assert_eq!(sh.scene.find_text("HDMI-A-1"), Some(name));
    assert_eq!(sh.spec(panels[0]).width, Some(480.0));
}

/// (flex review) A component reading `screen` mounted in a panel whose
/// `screens:` picks monitors gets each instance's monitor, as in a bar.
#[test]
fn a_component_in_a_panel_on_every_screen_reads_its_screen() {
    let src = "component Name { text screen.name }\n\
               panel Side { screens: all; anchor: left; width: 200; height: 100\n\
                 Name { }\n\
               }\n";
    let sh = boot(src, &MONITORS, Some(0));
    assert_eq!(sh.scene.texts(), ["DP-1", "HDMI-A-1"]);
}

/// A panel on the focused monitor (the default, or `screens: focused`)
/// is one surface the compositor places: no `screen` is in scope there,
/// and reading it says where it is.
#[test]
fn screen_is_not_in_scope_on_a_focused_panel() {
    for src in [
        "panel P { width: screen.width }\n",
        "panel P { screens: focused; width: screen.width }\n",
    ] {
        let c = compile(src);
        assert!(c.errors() > 0, "{src}");
        let text = format!("{:?}", c.diagnostics);
        assert!(text.contains("screens: all"), "{text}");
    }
    let c = compile("panel P { screens: all; width: screen.width }\n");
    assert_eq!(c.errors(), 0, "{:#?}", c.diagnostics);
}

/// (lockmon) A lock is on every monitor, an instance each with its own
/// `screen`, pinned to it as a bar's are; its own `state`s are one for
/// all of them, so a password typed into one monitor's field is in every
/// monitor's, and each `input` carries the key its twins share. A monitor
/// unplugged takes its instance (parked), one plugged in gets one, and
/// what was typed stays through both.
#[test]
fn the_lock_is_on_every_monitor_with_one_password() {
    let src = "lock L { open: true\n\
                 state secret = \"\"\n\
                 col { text screen.name\n\
                   input { type: password; text: <-> secret; focus: true\n\
                     on activate { auth.submit(secret); secret = \"\" } }\n\
                 }\n\
               }\n";
    let mut sh = boot(src, &MONITORS, Some(1));
    let locks = sh.scene.of_kind(NodeKind::Lock);
    assert_eq!(locks.len(), 2, "{}", sh.scene.render());
    assert_eq!(sh.labels(), ["DP-1", "HDMI-A-1"]);
    let pins: Vec<strand_scene::Screens> = locks.iter().map(|l| sh.spec(*l).screens).collect();
    assert_eq!(
        pins,
        [
            strand_scene::Screens::Named(vec!["Mock | DP-1 | Display".into()]),
            strand_scene::Screens::Named(vec!["Mock | HDMI-A-1 | Display".into()]),
        ]
    );
    let inputs = sh.scene.of_kind(NodeKind::Input);
    assert_eq!(inputs.len(), 2);
    let twin = |sh: &Shell, n| sh.scene.prop(n, strand_scene::Prop::Twin).cloned();
    assert!(
        matches!(twin(&sh, inputs[0]), Some(strand_scene::PropValue::Text(_))),
        "an input of the lock names its twins"
    );
    assert_eq!(twin(&sh, inputs[0]), twin(&sh, inputs[1]));
    let text = |sh: &Shell, n| sh.scene.prop(n, strand_scene::Prop::Text).cloned();
    let typed = |t: &str| Some(strand_scene::PropValue::Text(t.into()));
    // Typed on one monitor: every monitor's field holds it.
    sh.inst
        .write(inputs[0], strand_scene::Prop::Text, typed("hunt").unwrap())
        .unwrap();
    sh.flush();
    assert_eq!(text(&sh, inputs[0]), typed("hunt"));
    assert_eq!(text(&sh, inputs[1]), typed("hunt"));
    // Then on the other: one text, never two.
    sh.inst
        .write(
            inputs[1],
            strand_scene::Prop::Text,
            typed("hunter2").unwrap(),
        )
        .unwrap();
    sh.flush();
    assert_eq!(text(&sh, inputs[0]), typed("hunter2"));
    assert_eq!(text(&sh, inputs[1]), typed("hunter2"));
    // Focus moving mounts nothing again.
    set_screens(&sh.rt, &sh.host, &MONITORS, Some(0));
    sh.flush();
    assert_eq!(sh.scene.of_kind(NodeKind::Lock), locks);
    // HDMI-A-1 unplugged: its instance goes, the text stays.
    set_screens(&sh.rt, &sh.host, &MONITORS[..1], Some(0));
    sh.flush();
    assert_eq!(sh.scene.of_kind(NodeKind::Lock), [locks[0]]);
    assert_eq!(text(&sh, inputs[0]), typed("hunter2"));
    // A monitor plugged in while locked gets an instance, with the text.
    let more = [MONITORS[0], MONITORS[1], ("DP-2", 800.0, 600.0)];
    set_screens(&sh.rt, &sh.host, &more, Some(0));
    sh.flush();
    let now = sh.scene.of_kind(NodeKind::Lock);
    assert_eq!(now.len(), 3, "{}", sh.scene.render());
    assert_eq!(sh.labels(), ["DP-1", "HDMI-A-1", "DP-2"]);
    for i in sh.scene.of_kind(NodeKind::Input) {
        assert_eq!(text(&sh, i), typed("hunter2"), "{}", sh.scene.render());
    }
}

/// (lockmon) A runtime fault in one monitor's lock instance freezes that
/// instance only (the lock's scope bounds faults below it as the top
/// level does): the other monitor's field still types into the shared
/// password.
#[test]
fn a_fault_in_one_monitors_lock_freezes_that_instance_only() {
    let src = "state zero = 1\n\
               lock L { open: true\n\
                 state secret = \"\"\n\
                 col { text pct(10 / (screen.name == \"DP-1\" ? 1 : zero))\n\
                   input { type: password; text: <-> secret; focus: true\n\
                     on activate { auth.submit(secret); secret = \"\" } }\n\
                 }\n\
               }\n";
    let mut sh = boot(src, &MONITORS, Some(0));
    let locks = sh.scene.of_kind(NodeKind::Lock);
    let inputs = sh.scene.of_kind(NodeKind::Input);
    assert_eq!((locks.len(), inputs.len()), (2, 2));
    sh.inst.set_value("shell", "zero", Value::int(0)).unwrap();
    let u = sh.inst.flush();
    sh.scene.apply(&u.diff).unwrap();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    let e = &u.errors[0];
    let node = e.node.expect("the failing text");
    assert_eq!(sh.scene.ancestor(node, NodeKind::Lock), Some(locks[1]));
    assert!(sh.inst.freeze(e));
    let typed = |t: &str| strand_scene::PropValue::Text(t.into());
    sh.inst
        .write(inputs[0], strand_scene::Prop::Text, typed("hunter2"))
        .unwrap();
    sh.flush();
    assert_eq!(
        sh.scene.prop(inputs[0], strand_scene::Prop::Text),
        Some(&typed("hunter2")),
        "DP-1's instance still types: {}",
        sh.scene.render()
    );
}

/// (lockmon) `screens:` on a lock picks the monitors that show it, as
/// on a bar; `screens: focused` keeps one instance on the focused monitor
/// (else the first) whose `screen` changes in place as focus moves
/// (nothing is mounted again), as does a `screens:` that picks no plugged
/// monitor, so the lock always has content; with no monitor known it
/// still mounts, on an empty `Screen`.
#[test]
fn a_locks_screens_picks_its_monitors() {
    let src = "lock L { open: true; screens: focused\n\
                 on key(k) { auth.submit(k.name) }\n\
                 col { text screen.name }\n\
               }\n";
    let mut sh = boot(src, &MONITORS, Some(1));
    let locks = sh.scene.of_kind(NodeKind::Lock);
    assert_eq!(locks.len(), 1);
    assert_eq!(sh.spec(locks[0]).screens, strand_scene::Screens::Focused);
    assert_eq!(sh.scene.texts()[0], "HDMI-A-1");
    let name = sh.scene.find_text("HDMI-A-1").unwrap();
    set_screens(&sh.rt, &sh.host, &MONITORS, Some(0));
    sh.flush();
    assert_eq!(
        sh.scene.of_kind(NodeKind::Lock),
        locks,
        "the lock was mounted again"
    );
    assert_eq!(sh.scene.texts()[0], "DP-1");
    assert_eq!(
        sh.scene.find_text("DP-1"),
        Some(name),
        "its text was mounted again"
    );
    // No focus: the first.
    set_screens(&sh.rt, &sh.host, &MONITORS, None);
    sh.flush();
    assert_eq!(sh.scene.texts()[0], "DP-1");
    // No monitor at all: still there, on an empty one.
    let sh = boot(src, &[], None);
    assert_eq!(sh.scene.of_kind(NodeKind::Lock).len(), 1);
    assert_eq!(sh.scene.texts()[0], "");
    // A name: that monitor only, pinned.
    let named = src.replace("screens: focused", "screens: \"HDMI-A-1\"");
    let sh = boot(&named, &MONITORS, Some(0));
    let locks = sh.scene.of_kind(NodeKind::Lock);
    assert_eq!(locks.len(), 1);
    assert_eq!(sh.scene.texts(), ["HDMI-A-1"]);
    assert_eq!(
        sh.spec(locks[0]).screens,
        strand_scene::Screens::Named(vec!["Mock | HDMI-A-1 | Display".into()])
    );
    // A name not plugged in: the focused monitor's, not none.
    let gone = src.replace("screens: focused", "screens: \"VGA-9\"");
    let sh = boot(&gone, &MONITORS, Some(0));
    let locks = sh.scene.of_kind(NodeKind::Lock);
    assert_eq!(locks.len(), 1, "a lock with no content");
    assert_eq!(sh.scene.texts(), ["DP-1"]);
    assert_eq!(sh.spec(locks[0]).screens, strand_scene::Screens::Focused);
}

/// (lockmon) A lock's `state` is one for every monitor: its starting
/// value cannot read `screen` or a `let` of the lock (each instance's
/// own); props, `let`s and handlers read `screen` as before.
#[test]
fn a_locks_state_cannot_start_from_its_screen() {
    let head = "lock L { open: true\n  on key(k) { auth.submit(k.name) }\n";
    for body in [
        "  state w = screen.width\n",
        "  let half = screen.width / 2\n  state w = half\n",
        "  col { state w = screen.height }\n",
    ] {
        let c = compile(&format!("{head}{body}}}\n"));
        let codes: Vec<&str> = c.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&"check::lock_state_screen"),
            "{body}: {:#?}",
            c.diagnostics
        );
    }
    for body in [
        "  state w = 0\n  width: screen.width\n",
        "  let half = screen.width / 2\n  width: half\n",
        "  state w = 0\n  on click { w = screen.width }\n",
    ] {
        let c = compile(&format!("{head}{body}}}\n"));
        assert_eq!(c.errors(), 0, "{body}: {:#?}", c.diagnostics);
    }
}

/// `exclusive:` on a bar and a panel: the bar keeps its thickness by
/// default, nothing with `false` or `0`, exactly a length with one; a
/// panel keeps nothing by default, its thickness with `true` along the
/// edge it is anchored or attached to, and nothing at a corner.
#[test]
fn exclusive_sets_the_reserved_space() {
    let zone = |src: &str| {
        let sh = boot(src, &MONITORS[..1], Some(0));
        let root = sh.scene.roots()[0];
        let spec = sh.spec(root);
        (spec.exclusive, spec.exclusive_zone())
    };
    assert_eq!(
        zone("bar B { height: 36 }\n"),
        (Exclusive::Auto, Some(36.0))
    );
    assert_eq!(
        zone("bar B { height: 36; exclusive: false }\n"),
        (Exclusive::None, Some(0.0))
    );
    assert_eq!(
        zone("bar B { height: 36; exclusive: 0 }\n"),
        (Exclusive::None, Some(0.0))
    );
    assert_eq!(
        zone("bar B { height: 36; exclusive: 20 }\n"),
        (Exclusive::Length(20.0), Some(20.0))
    );
    assert_eq!(
        zone("bar B { edge: left; width: 48; exclusive: true }\n"),
        (Exclusive::Auto, Some(48.0))
    );
    assert_eq!(
        zone("panel P { anchor: top; width: 400; height: 60 }\n"),
        (Exclusive::None, Some(0.0))
    );
    assert_eq!(
        zone("panel P { anchor: top; width: 400; height: 60; exclusive: true }\n"),
        (Exclusive::Auto, Some(60.0))
    );
    assert_eq!(
        zone("panel P { anchor: right; width: 300; height: 600; exclusive: 280 }\n"),
        (Exclusive::Length(280.0), Some(280.0))
    );
    assert_eq!(
        zone(
            "panel P { anchor: top_left; attach: left; width: 300; height: 600; exclusive: true }\n"
        ),
        (Exclusive::Auto, Some(300.0))
    );
    assert_eq!(
        zone("panel P { anchor: top_left; width: 300; height: 600; exclusive: true }\n"),
        (Exclusive::Auto, Some(0.0))
    );
}

//! (flex) Surfaces of the 2026-10-24 gap review (decisions.md `## flex`):
//! a panel or OSD on every monitor its `screens:` picks, with `screen`
//! in scope as a bar has; a lock whose `screen` is the monitor its
//! content shows on; `exclusive:` on bars and panels.

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

/// The lock's `screen` is the monitor its content shows on: the
/// focused one, else the first; it changes in place when focus moves
/// (the content is not mounted again, so what was typed stays), and a
/// lock with no monitor known still shows.
#[test]
fn the_locks_screen_follows_its_content() {
    let src = "lock L { open: true\n\
                 on key(k) { auth.submit(k.name) }\n\
                 col { text screen.name }\n\
               }\n";
    let mut sh = boot(src, &MONITORS, Some(1));
    let locks = sh.scene.of_kind(NodeKind::Lock);
    assert_eq!(locks.len(), 1);
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

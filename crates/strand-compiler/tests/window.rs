//! (window) The `window` surface (decisions.md `## window`): an
//! `xdg_toplevel` the compositor manages. One instance whatever the
//! monitors, no `screen` in scope; the props that place a layer-shell
//! surface refused at check; `id: w` on it names its root, whose
//! compositor states (`w.maximized` …) render reports as flags and whose
//! actions (`w.maximize()` …) reach the diff for the surface manager;
//! a reload keeps it open with what the compositor said of it.

use std::rc::Rc;

use strand_compiler::instantiate::{Instance, NodeFlag, SceneMirror, Storage, Update};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, compile as compile_map};
use strand_core::Runtime;
use strand_scene::{
    Anchor, Decorations, NodeId, NodeKind, Prop, PropValue, SurfaceSpec, WindowAsk,
};

struct Shell {
    _rt: Runtime,
    _host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
    build: Build,
}

fn build(prev: Option<&Build>, src: &str) -> Build {
    let mut map = SourceMap::new();
    map.add("shell.strand", src.to_string());
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

fn boot(src: &str) -> Shell {
    let build = build(None, src);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    screens(&rt, &host, &["DP-1", "HDMI-A-1"]);
    let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
    let mut sh = Shell {
        _rt: rt,
        _host: host,
        inst,
        scene: SceneMirror::new(),
        build,
    };
    sh.flush();
    sh
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene
            .apply(&u.diff)
            .unwrap_or_else(|e| panic!("{e}\n{:#?}", u.diff.ops));
        u
    }

    fn reload(&mut self, src: &str) {
        let build = build(Some(&self.build), src);
        self.inst.reload(&build);
        self.build = build;
        self.flush();
    }

    fn window(&self) -> NodeId {
        let w = self.scene.of_kind(NodeKind::Window);
        assert_eq!(w.len(), 1, "{}", self.scene.render());
        w[0]
    }

    fn spec(&self, id: NodeId) -> SurfaceSpec {
        let kind = self.scene.kind(id).unwrap();
        SurfaceSpec::resolve(kind, |p| self.scene.prop(id, p).cloned())
    }

    fn box_with(&self, text: &str) -> NodeId {
        let t = self.scene.find_text(text).unwrap();
        self.scene.parent(t).unwrap()
    }
}

fn codes(src: &str) -> Vec<&'static str> {
    let mut map = SourceMap::new();
    map.add("shell.strand", src.to_string());
    compile_map(&map)
        .diagnostics
        .iter()
        .map(|d| d.code)
        .collect()
}

const SETTINGS: &str = "state shown = true\n\
    window Settings { open: <-> shown; title: \"Preferences\"; width: 400; height: 300\n\
      min_width: 200; max_height: 900; decorations: none\n\
      id: w\n\
      text w.maximized ? \"max\" : \"normal\"\n\
      text w.focused ? \"active\" : \"inactive\"\n\
      text w.decorated ? \"ssd\" : \"csd\"\n\
      box { text \"Max\"; on click { w.maximize() } }\n\
      box { text \"Grip\"; on click { w.resize(Anchor.bottom_right) } }\n\
      box { id: other; text \"Other\"; on click { other.maximize() } }\n\
      box { text \"Bye\"; on click { w.close() } }\n\
    }\n";

#[test]
fn a_window_is_one_surface_with_its_props() {
    let sh = boot(SETTINGS);
    // Two monitors, one window: the compositor puts it on one.
    let w = sh.window();
    let spec = sh.spec(w);
    assert_eq!(spec.window_title(), "Preferences");
    assert_eq!(spec.window_app_id(), "strand-Settings");
    assert_eq!(spec.decorations, Decorations::None);
    assert_eq!((spec.width, spec.height), (Some(400.0), Some(300.0)));
    assert_eq!(spec.min_size, (Some(200.0), None));
    assert_eq!(spec.max_size, (None, Some(900.0)));
    assert!(spec.open);
    assert_eq!(
        sh.scene.texts()[..3],
        ["normal", "inactive", "csd"],
        "{}",
        sh.scene.render()
    );
    // Unset, the title is its name, and decorations the server's.
    let sh = boot("window Picker { text \"x\" }\n");
    let spec = sh.spec(sh.window());
    assert_eq!(spec.window_title(), "Picker");
    assert_eq!(spec.window_app_id(), "strand-Picker");
    assert_eq!(spec.decorations, Decorations::Server);
    assert_eq!((spec.width, spec.height), (None, None));
}

#[test]
fn the_compositors_states_read_on_its_id() {
    let mut sh = boot(SETTINGS);
    let w = sh.window();
    sh.inst.set_flag(w, NodeFlag::Maximized, true);
    sh.inst.set_flag(w, NodeFlag::Focused, true);
    sh.inst.set_flag(w, NodeFlag::Decorated, true);
    sh.flush();
    assert_eq!(sh.scene.texts()[..3], ["max", "active", "ssd"]);
    sh.inst.set_flag(w, NodeFlag::Maximized, false);
    sh.flush();
    assert_eq!(sh.scene.texts()[0], "normal");
}

#[test]
fn window_actions_reach_the_diff() {
    let mut sh = boot(SETTINGS);
    let w = sh.window();
    let max = sh.box_with("Max");
    assert!(sh.inst.event(max, "click", vec![]));
    let u = sh.flush();
    assert_eq!(u.diff.window_asks, [(w, WindowAsk::Maximize)]);
    // Asked once: the next diff has none.
    assert!(sh.flush().diff.window_asks.is_empty());
    let grip = sh.box_with("Grip");
    assert!(sh.inst.event(grip, "click", vec![]));
    let u = sh.flush();
    assert_eq!(
        u.diff.window_asks,
        [(w, WindowAsk::Resize(Anchor::BottomRight))]
    );
    // On an element that is no window's root: nothing.
    let other = sh.box_with("Other");
    assert!(sh.inst.event(other, "click", vec![]));
    assert!(sh.flush().diff.window_asks.is_empty());
    let bye = sh.box_with("Bye");
    assert!(sh.inst.event(bye, "click", vec![]));
    assert_eq!(sh.flush().diff.window_asks, [(w, WindowAsk::Close)]);
}

#[test]
fn the_compositors_close_writes_open_false() {
    let mut sh = boot(SETTINGS);
    let w = sh.window();
    // What the router writes on the compositor's close (ClickAway).
    sh.inst
        .write(w, Prop::Open, PropValue::Bool(false))
        .unwrap();
    sh.flush();
    assert_eq!(
        sh.inst.value_of("shell", "shown").unwrap(),
        Value::Bool(false)
    );
    assert!(!sh.spec(w).open);
}

#[test]
fn a_reload_keeps_an_open_window_and_its_states() {
    let mut sh = boot(SETTINGS);
    let w = sh.window();
    sh.inst.set_flag(w, NodeFlag::Maximized, true);
    sh.flush();
    sh.reload(&SETTINGS.replace("Preferences", "Settings"));
    let again = sh.window();
    assert_eq!(again, w, "the same window, not a new one");
    assert!(sh.spec(again).open);
    assert_eq!(sh.spec(again).window_title(), "Settings");
    assert_eq!(sh.scene.texts()[0], "max");
}

#[test]
fn what_places_a_layer_surface_is_refused_on_a_window() {
    for prop in [
        "screens: all",
        "layer: top",
        "anchor: top",
        "margin: 8",
        "attach: top",
        "scrim: #0004",
        "keyboard: exclusive",
    ] {
        let src = format!("window W {{ {prop}; text \"x\" }}\n");
        assert!(
            codes(&src).contains(&"check::window_prop"),
            "{prop}: {:?}",
            codes(&src)
        );
    }
    // In a `when` too.
    let src = "state a = false\nwindow W { when a { layer: overlay }; text \"x\" }\n";
    assert!(
        codes(src).contains(&"check::window_prop"),
        "{:?}",
        codes(src)
    );
    // No `screen` in scope: the compositor picks the monitor.
    let mut map = SourceMap::new();
    map.add(
        "shell.strand",
        "window W { text screen.name }\n".to_string(),
    );
    let out = compile_map(&map);
    assert!(
        out.diagnostics.iter().any(|d| d
            .help
            .as_deref()
            .is_some_and(|h| h.contains("a `window` is one surface"))),
        "{:#?}",
        out.diagnostics
    );
    // What a window takes checks clean.
    assert_eq!(codes(SETTINGS), Vec::<&str>::new());
    let src = "state s = true\n\
        window W { open: <-> s; inhibit_idle: true; bg: $surface; min_height: 100; text \"x\" }\n";
    assert_eq!(codes(src), Vec::<&str>::new());
}

/// (window) A minimum over the maximum, both written as numbers, is a
/// load error (the compositor would end Strand's connection on it); one
/// at most the maximum, or one an expression gives, checks clean (a
/// binding's is raised at run time, `placement::window_config`).
#[test]
fn a_window_minimum_over_its_maximum_is_refused() {
    let refused = |src: &str| codes(src).contains(&"check::window_min_max");
    let s = "state s = true\n";
    assert!(refused(&format!(
        "{s}window W {{ open: <-> s; min_width: 500; max_width: 400; text \"x\" }}\n"
    )));
    assert!(refused(&format!(
        "{s}window W {{ open: <-> s; min_height: 300px; max_height: 200; text \"x\" }}\n"
    )));
    assert!(!refused(&format!(
        "{s}window W {{ open: <-> s; min_width: 400; max_width: 400; text \"x\" }}\n"
    )));
    assert!(!refused(&format!(
        "{s}state m = 500\nwindow W {{ open: <-> s; min_width: m; max_width: 400; text \"x\" }}\n"
    )));
    let mut map = SourceMap::new();
    map.add(
        "shell.strand",
        format!("{s}window W {{ open: <-> s; min_width: 500; max_width: 400; text \"x\" }}\n"),
    );
    let out = compile_map(&map);
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::window_min_max")
        .unwrap();
    assert_eq!(d.severity, strand_compiler::Severity::Error);
    assert_eq!(d.labels.len(), 2, "both values named");
}

/// (window, W1 open item) The compositor's close writes only a two-way
/// `open:`: one unset (always open) or one-way is warned, a two-way one
/// in a `when` is enough.
#[test]
fn a_window_the_compositor_cannot_close_is_warned() {
    let warned = |src: &str| codes(src).contains(&"check::window_open");
    assert!(warned("window W { text \"x\" }\n"));
    assert!(warned("state s = true\nwindow W { open: s; text \"x\" }\n"));
    assert!(!warned(
        "state s = true\nwindow W { open: <-> s; text \"x\" }\n"
    ));
    assert!(!warned(
        "state s = true\nstate a = false\nwindow W { open: false; when a { open: <-> s }; text \"x\" }\n"
    ));
    // A panel's one-way `open:` is not a window's.
    assert!(!warned("state s = true\npanel P { open: s; text \"x\" }\n"));
    let mut map = SourceMap::new();
    map.add("shell.strand", "window W { text \"x\" }\n".to_string());
    let out = compile_map(&map);
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::window_open")
        .unwrap();
    assert_eq!(d.severity, strand_compiler::Severity::Warning);
    assert!(d.help.as_deref().unwrap().contains("open: <-> shown"));
}

#[test]
fn an_id_on_a_surface_names_its_root() {
    // A panel's too: `p.open` on its own id.
    let sh = boot(
        "panel P { id: p; width: 100\n\
           text p.open ? \"open\" : \"closed\"\n\
         }\n",
    );
    assert_eq!(sh.scene.texts(), ["open"]);
    // Named twice in one body: an error.
    let src = "window W { id: w; box { id: w } }\n";
    assert!(
        codes(src).contains(&"check::redeclared"),
        "{:?}",
        codes(src)
    );
}

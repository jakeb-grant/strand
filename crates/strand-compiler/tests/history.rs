//! (history) A `graph` keeps the history of what it shows while it is not
//! mounted (design.md, "Generative, data-driven and media"; decisions.md
//! history): the instance keeps each series a file-scope graph shows,
//! once, as long as the longest `history` asks; its services are kept at
//! the coarse hidden period; a graph mounting gets the backlog; a reload
//! that drops the graph frees the store and lets the service go.

use std::rc::Rc;
use std::time::Duration;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{HistoryInfo, Instance, SceneMirror, Storage, Update};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::{Backlog, NodeId, NodeKind, Prop, PropValue};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
    build: Build,
}

fn compile(prev: Option<&Build>, files: &[(&str, &str)]) -> Build {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.to_string());
    }
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

fn boot(files: &[(&str, &str)]) -> Shell {
    let build = compile(None, files);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
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
    host.set(&rt, "cpu.usage", Value::float(0.1)).unwrap();
    let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
        build,
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        self.scene.apply(&u.diff).unwrap();
        u
    }

    fn at(&mut self, secs: f64) -> Update {
        let u = self.inst.tick(Duration::from_secs_f64(secs));
        self.scene.apply(&u.diff).unwrap();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        u
    }

    /// `cpu.usage` reads `v` from `secs` on.
    fn cpu(&mut self, secs: f64, v: f64) {
        self.at(secs);
        self.host
            .set(&self.rt, "cpu.usage", Value::float(v))
            .unwrap();
        self.at(secs);
    }

    fn open(&mut self, secs: f64, on: bool) {
        self.at(secs);
        self.inst
            .set_value("shell", "open", Value::Bool(on))
            .unwrap();
        self.at(secs);
    }

    fn reload(&mut self, files: &[(&str, &str)]) {
        let build = compile(Some(&self.build), files);
        self.inst.reload(&build);
        self.build = build;
        let u = self.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    }

    fn graphs(&self) -> Vec<NodeId> {
        self.scene.of_kind(NodeKind::Graph)
    }

    fn backlog(&self, graph: NodeId) -> Option<Backlog> {
        match self.scene.prop(graph, Prop::Backlog) {
            Some(PropValue::Backlog(b)) => Some((**b).clone()),
            _ => None,
        }
    }

    fn kept(&self) -> Vec<(String, Duration)> {
        self.host.kept().into_iter().collect()
    }

    fn histories(&self) -> Vec<HistoryInfo> {
        self.inst.histories()
    }
}

fn secs(n: f64) -> Duration {
    Duration::from_secs_f64(n)
}

const ISLAND: &str = "
state open = true
bar Top {
  edge: top; height: 40
  text \"island\"
  popup { open: <-> open; graph cpu.usage { height: 36; history: 60s; smooth: 0.6 } }
}
";

/// A graph in a popup, which mounts only while open. Closed, nothing
/// reads `cpu` and it is kept at the 3 s hidden period of a 60 s
/// history; opened again, the graph mounts onto what it showed before
/// and what came while it was closed (as ramps).
#[test]
fn a_graph_mounting_again_gets_what_it_showed_and_what_came_while_hidden() {
    let mut shell = boot(&[("shell.strand", ISLAND)]);
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(3.0))]);
    let g = shell.graphs()[0];
    let first = shell.backlog(g).expect("a kept graph has a backlog");
    assert_eq!(first.points.len(), 1, "{first:?}");
    assert_eq!(first.points[0].value, 0.1);
    shell.cpu(1.0, 0.3);
    shell.cpu(2.0, 0.5);
    // Closed: no reader, so what comes is a coarse sample.
    shell.open(3.0, false);
    assert!(shell.graphs().is_empty());
    assert_eq!(shell.host.readers("cpu"), 0);
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(3.0))], "still kept");
    shell.cpu(6.0, 0.9);
    shell.cpu(9.0, 0.7);
    shell.open(10.0, true);
    let g = shell.graphs()[0];
    let b = shell.backlog(g).expect("the backlog on mounting");
    let pts: Vec<(f32, f32, f32)> = b.points.iter().map(|p| (p.age, p.value, p.ramp)).collect();
    assert_eq!(
        pts,
        [
            (10.0, 0.1, 0.0),
            (9.0, 0.3, 0.0),
            (8.0, 0.5, 0.0),
            (4.0, 0.9, 3.0),
            (1.0, 0.7, 3.0),
        ]
    );
    // Shown, a new value reaches the mounted graph's backlog as a step.
    shell.cpu(11.0, 0.2);
    let b = shell.backlog(g).unwrap();
    let last = b.points.last().unwrap();
    assert_eq!((last.age, last.value, last.ramp), (0.0, 0.2, 0.0));
    assert_eq!(shell.histories()[0].points, 6);
}

/// The store keeps only what the longest `history` asks: after twice
/// the span the oldest point is the one in force as the span starts.
#[test]
fn the_store_keeps_no_more_than_the_longest_history() {
    let src = "
state open = true
bar Top {
  edge: top; height: 40
  if open { graph cpu.usage { history: 10s } }
  graph cpu.usage { history: 20s }
}
";
    let mut shell = boot(&[("shell.strand", src)]);
    let h = shell.histories();
    assert_eq!(h.len(), 1, "one series for two graphs: {h:?}");
    assert_eq!(h[0].span, secs(20.0));
    // The more demanding (shorter) period wins: 2 s for 10 s and 20 s.
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(2.0))]);
    for i in 1..=60 {
        shell.cpu(f64::from(i), f64::from(i % 7) / 10.0 + 0.05);
    }
    let h = &shell.histories()[0];
    assert!(h.oldest.unwrap() <= secs(21.0), "{h:?}");
    assert!(h.points <= 22, "{h:?}");
    // The graph wanting 20 s gone: the store falls to 10 s at once.
    let shorter = "
state open = true
bar Top {
  edge: top; height: 40
  if open { graph cpu.usage { history: 10s } }
}
";
    shell.reload(&[("shell.strand", shorter)]);
    let h = &shell.histories()[0];
    assert_eq!(h.span, secs(10.0));
    assert!(h.oldest.unwrap() <= secs(11.0), "{h:?}");
    assert!(h.points <= 12, "{h:?}");
}

/// A reload without the graph frees the store and lets the service go; a
/// config without one keeps nothing from the start.
#[test]
fn a_reload_dropping_the_graph_frees_the_store_and_lets_the_service_go() {
    let mut shell = boot(&[("shell.strand", ISLAND)]);
    shell.cpu(1.0, 0.4);
    assert_eq!(shell.histories().len(), 1);
    let plain = "
state open = true
bar Top {
  edge: top; height: 40
  if open { text \"{cpu.usage}\" }
}
";
    shell.reload(&[("shell.strand", plain)]);
    assert!(shell.histories().is_empty());
    assert!(shell.kept().is_empty());
    // Back: a new store, empty but for the value now.
    shell.reload(&[("shell.strand", ISLAND)]);
    let h = shell.histories();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].points, 1, "{h:?}");
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(3.0))]);
    let none = boot(&[("shell.strand", plain)]);
    assert!(none.histories().is_empty());
    assert!(none.kept().is_empty());
}

/// Only values that exist at file scope are kept: an expression over
/// services, file-level `let`s and `state`s is (one series per
/// expression); a list item's, a component parameter's, an element's or
/// time's is not, nor is a graph whose `history` is computed.
#[test]
fn only_file_scope_values_are_kept() {
    let src = "
state open = true
state scale = 2.0
let load = cpu.usage * scale
component Spark(v: float) { graph v { history: 30s } }
bar Top {
  edge: top; height: 40
  if open {
    graph load { history: 30s }
    graph 1.0 - memory.usage { history: 600s }
    graph cpu.usage { history: open ? 10s : 20s }
    graph wave(1s) { history: 10s }
    Spark(cpu.usage)
    for c in [1.0, 2.0] key c { graph cpu.usage * c { history: 10s } }
  }
}
";
    let shell = boot(&[("shell.strand", src)]);
    let h = shell.histories();
    let spans: Vec<Duration> = h.iter().map(|h| h.span).collect();
    assert_eq!(h.len(), 2, "{h:#?}");
    assert!(spans.contains(&secs(30.0)) && spans.contains(&secs(600.0)));
    let cpu = h.iter().find(|h| h.span == secs(30.0)).unwrap();
    assert_eq!(cpu.services, [("cpu".to_string(), Some(secs(2.0)))]);
    let mem = h.iter().find(|h| h.span == secs(600.0)).unwrap();
    assert_eq!(mem.services, [("memory".to_string(), Some(secs(30.0)))]);
    // Graphs not kept get no backlog.
    let with: usize = shell
        .graphs()
        .into_iter()
        .filter(|g| shell.backlog(*g).is_some())
        .count();
    assert_eq!(with, 2);
}

/// A graph under an `if` in a bar: the bar reads `cpu` (it holds what
/// its `if`s read), so the service samples at its shown rate throughout
/// and the graph, mounted again, gets those values as steps.
#[test]
fn a_graph_under_an_if_mounts_again_onto_its_shown_values() {
    let src = "
state open = true
bar Top {
  edge: top; height: 40
  if open { graph cpu.usage { history: 60s } }
}
";
    let mut shell = boot(&[("shell.strand", src)]);
    shell.cpu(1.0, 0.3);
    shell.open(2.0, false);
    assert!(shell.graphs().is_empty());
    shell.cpu(3.0, 0.6);
    shell.open(4.0, true);
    let b = shell.backlog(shell.graphs()[0]).unwrap();
    let pts: Vec<(f32, f32, f32)> = b.points.iter().map(|p| (p.age, p.value, p.ramp)).collect();
    assert_eq!(pts, [(4.0, 0.1, 0.0), (3.0, 0.3, 0.0), (1.0, 0.6, 0.0)]);
}

/// A graph in a panel, which is hidden, not unmounted: its backlog
/// binding catches up when the panel shows again, with what came while
/// it was hidden.
#[test]
fn a_hidden_panel_graph_catches_up_when_shown() {
    let src = "
state open = true
panel Stats { open: <-> open; graph cpu.usage { history: 60s } }
";
    let mut shell = boot(&[("shell.strand", src)]);
    let g = shell.graphs()[0];
    shell.cpu(1.0, 0.3);
    shell.open(2.0, false);
    assert_eq!(shell.host.readers("cpu"), 0);
    shell.cpu(5.0, 0.8);
    shell.open(6.0, true);
    assert_eq!(shell.graphs(), [g], "the same graph");
    let b = shell.backlog(g).unwrap();
    let last = b.points.last().unwrap();
    assert_eq!((last.age, last.value, last.ramp), (1.0, 0.8, 3.0));
}

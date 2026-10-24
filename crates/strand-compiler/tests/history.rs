//! (history) A `graph` keeps the history of what it shows while it is not
//! mounted (design.md, "Generative, data-driven and media"; decisions.md
//! history): the instance keeps each series a file-scope graph shows,
//! once, as long as the longest `history` asks; its services are kept at
//! the hidden period; a graph mounting gets the backlog; a reload that
//! drops the graph frees the store and lets the service go. Points are
//! timed on a fake boot clock here (`T0` plus the logic clock's seconds,
//! plus any time "slept").

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{HistoryInfo, Instance, SceneMirror, Storage, Update};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::backlog::Clocks;
use strand_scene::{Backlog, BacklogPoint, NodeId, NodeKind, Prop, PropValue};

/// The fake boot clock's seconds at the logic clock's zero.
const T0: f64 = 1000.0;

thread_local! {
    /// The fake clocks: the boot clock and the awake clock.
    static CLOCKS: Cell<Clocks> = const { Cell::new(Clocks { boot: Duration::ZERO, awake: Duration::ZERO }) };
}

fn fake() -> Clocks {
    CLOCKS.with(Cell::get)
}

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
    build: Build,
    /// The logic clock's seconds.
    now: f64,
    /// Seconds slept so far (the boot clock is ahead by that much).
    slept: f64,
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
    // cpu and memory sample every second while shown, as the real ones.
    host.set_polls("cpu", Some(Duration::from_secs(1)));
    host.set_polls("memory", Some(Duration::from_secs(1)));
    let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
        build,
        now: 0.0,
        slept: 0.0,
    };
    shell.clocks();
    shell.inst.set_history_clock(fake);
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell
}

impl Shell {
    fn clocks(&self) {
        CLOCKS.with(|c| {
            c.set(Clocks {
                boot: Duration::from_secs_f64(T0 + self.now + self.slept),
                awake: Duration::from_secs_f64(T0 + self.now),
            })
        });
    }

    /// The fake boot clock now.
    fn boot_now(&self) -> f64 {
        T0 + self.now + self.slept
    }

    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        self.scene.apply(&u.diff).unwrap();
        u
    }

    fn at(&mut self, secs: f64) -> Update {
        self.now = secs;
        self.clocks();
        let u = self.inst.tick(Duration::from_secs_f64(secs));
        self.scene.apply(&u.diff).unwrap();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        u
    }

    /// The machine sleeps `secs`: the boot clock runs on, the awake and
    /// logic clocks do not.
    fn sleep(&mut self, secs: f64) {
        self.slept += secs;
        self.clocks();
    }

    /// `path` reads `v` from `secs` on.
    fn set(&mut self, secs: f64, path: &str, v: f64) {
        self.at(secs);
        self.host.set(&self.rt, path, Value::float(v)).unwrap();
        self.at(secs);
    }

    /// `cpu.usage` reads `v` from `secs` on.
    fn cpu(&mut self, secs: f64, v: f64) {
        self.set(secs, "cpu.usage", v);
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

    fn backlog_arc(&self, graph: NodeId) -> Option<Arc<Backlog>> {
        match self.scene.prop(graph, Prop::Backlog) {
            Some(PropValue::Backlog(b)) => Some(b.clone()),
            _ => None,
        }
    }

    fn backlog(&self, graph: NodeId) -> Option<Backlog> {
        self.backlog_arc(graph).map(|b| (*b).clone())
    }

    /// A graph's backlog as (age now, value, ramp) triples.
    fn points(&self, graph: NodeId) -> Vec<(f64, f32, f32)> {
        let now = self.boot_now();
        self.backlog(graph)
            .expect("a backlog")
            .points
            .iter()
            .map(|p| (now - p.at, p.value, p.ramp))
            .collect()
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
/// and what came while it was closed (as ramps over the 3 s polls).
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
    // Closed: no reader, so what comes is a poll every 3 s.
    shell.open(3.0, false);
    assert!(shell.graphs().is_empty());
    assert_eq!(shell.host.readers("cpu"), 0);
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(3.0))], "still kept");
    shell.cpu(6.0, 0.9);
    shell.cpu(9.0, 0.7);
    shell.open(10.0, true);
    let g = shell.graphs()[0];
    assert_eq!(
        shell.points(g),
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
    let last = *shell.points(g).last().unwrap();
    assert_eq!(last, (0.0, 0.2, 0.0));
    assert_eq!(shell.histories()[0].points, 6);
}

/// (review) Ramps follow how each value was taken: a service that
/// reports its changes (`battery`) steps while hidden; one polling every
/// 10 s ramps over 10 s, not the 3 s of the history's hidden period; and
/// in a series of a shown and a hidden service only the hidden one's
/// changes ramp.
#[test]
fn ramps_follow_how_each_value_was_taken() {
    let src = "
state open = false
bar Top {
  edge: top; height: 40
  text pct(cpu.usage)
  popup { open: <-> open
    graph battery.percent { history: 60s }
    graph disk.usage { history: 60s }
    graph (cpu.usage + memory.usage) / 2.0 { history: 60s }
  }
}
";
    let mut shell = boot(&[("shell.strand", src)]);
    shell.host.set_polls("disk", Some(secs(10.0)));
    shell.set(0.0, "battery.percent", 0.5);
    shell.set(0.0, "disk.usage", 0.5);
    shell.set(0.0, "memory.usage", 0.5);
    shell.set(5.0, "battery.percent", 0.4);
    shell.set(20.0, "disk.usage", 0.6);
    // cpu is shown (the bar's text): its change steps; memory's ramps.
    assert_eq!(
        (shell.host.readers("cpu"), shell.host.readers("memory")),
        (1, 0)
    );
    shell.cpu(22.0, 0.3);
    shell.set(25.0, "memory.usage", 0.7);
    shell.open(26.0, true);
    let mut graphs = shell.graphs();
    graphs.sort();
    let last = |g: NodeId| shell.points(g).last().map(|p| p.2);
    let ramps: Vec<Vec<f32>> = graphs
        .iter()
        .map(|g| shell.points(*g).iter().map(|p| p.2).collect())
        .collect();
    assert_eq!(ramps[0], [0.0, 0.0], "battery steps: {ramps:?}");
    assert_eq!(last(graphs[1]), Some(10.0), "disk ramps over 10 s");
    assert_eq!(
        ramps[2][ramps[2].len() - 2..],
        [0.0, 3.0],
        "cpu steps, memory ramps"
    );
}

/// The store keeps only what the longest `history` asks: after twice
/// the span the oldest point is the one in force as the span starts.
/// Each graph gets its own `history` of it, shared with graphs asking
/// the same.
#[test]
fn the_store_keeps_no_more_than_the_longest_history() {
    let src = "
state open = true
bar Top {
  edge: top; height: 40
  if open { graph cpu.usage { history: 10s } }
  graph cpu.usage { history: 20s }
  graph cpu.usage { history: 10s }
}
";
    let mut shell = boot(&[("shell.strand", src)]);
    let h = shell.histories();
    assert_eq!(h.len(), 1, "one series for three graphs: {h:?}");
    assert_eq!(h[0].span, secs(20.0));
    // The more demanding (shorter) period wins: 2 s for 10 s and 20 s.
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(2.0))]);
    for i in 1..=60 {
        shell.cpu(f64::from(i), f64::from(i % 7) / 10.0 + 0.05);
    }
    let h = &shell.histories()[0];
    assert!(h.oldest.unwrap() <= secs(21.0), "{h:?}");
    assert!(h.points <= 22, "{h:?}");
    // Each graph's backlog is its own `history`; the two of 10 s share one.
    let mut by_span: Vec<(f64, Arc<Backlog>)> = shell
        .graphs()
        .into_iter()
        .map(|g| {
            let b = shell.backlog_arc(g).unwrap();
            (shell.boot_now() - b.points[1].at, b)
        })
        .collect();
    by_span.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert!(by_span[0].0 < 10.0 && by_span[2].0 < 20.0 && by_span[2].0 >= 10.0);
    assert!(Arc::ptr_eq(&by_span[0].1, &by_span[1].1), "shared");
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
  if open { text pct(cpu.usage) }
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
/// time's is not, nor is a graph whose `history` is computed. A
/// `history` that is a constant `let` is constant.
#[test]
fn only_file_scope_values_are_kept() {
    let src = "
state open = true
state scale = 2.0
let load = cpu.usage * scale
let span = 30s
component Spark(v: float) { graph v { history: 30s } }
bar Top {
  edge: top; height: 40
  if open {
    graph load { history: span }
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

/// (review) A component's graph is kept only where the component is used:
/// a declared, unused one keeps nothing and wakes nothing; one used (in
/// another used one) is kept once.
#[test]
fn only_components_in_use_keep_history() {
    let unused = "
component CpuCard { graph cpu.usage { history: 60s } }
bar Top {
  edge: top; height: 40
  text \"x\"
}
";
    let shell = boot(&[("shell.strand", unused)]);
    assert!(shell.histories().is_empty());
    assert!(shell.kept().is_empty(), "nothing woken");
    let used = "
component CpuCard { graph cpu.usage { history: 60s } }
component Card { CpuCard }
bar Top {
  edge: top; height: 40
  Card
  Card
}
";
    let shell = boot(&[("shell.strand", used)]);
    assert_eq!(shell.histories().len(), 1);
    assert_eq!(shell.kept(), [("cpu".to_string(), secs(3.0))]);
}

/// (review) A series is what its expression means: editing a `let` it
/// reads (or a config service's declaration) is another series, not the
/// old one's points under a new meaning.
#[test]
fn editing_what_a_series_reads_is_another_series() {
    let cpu = "
let load = cpu.usage
bar Top {
  edge: top; height: 40
  graph load { history: 60s }
}
";
    let mut shell = boot(&[("shell.strand", cpu)]);
    shell.cpu(1.0, 0.3);
    shell.cpu(2.0, 0.6);
    let before = shell.histories();
    assert_eq!(before[0].points, 3);
    let memory = "
let load = memory.usage
bar Top {
  edge: top; height: 40
  graph load { history: 60s }
}
";
    shell.reload(&[("shell.strand", memory)]);
    let after = shell.histories();
    assert_eq!(after.len(), 1);
    assert_ne!(after[0].key, before[0].key);
    assert!(after[0].points <= 1, "a new store: {after:?}");
    assert_eq!(after[0].services, [("memory".to_string(), Some(secs(3.0)))]);
    assert!(shell.kept().iter().all(|(s, _)| s != "cpu"), "cpu let go");
    let poll = |cmd: &str| {
        format!(
            "permit exec \"sh\"\nservice p from poll [\"sh\", \"-c\", \"{cmd}\"] every 1s {{ n: float }}\nbar Top {{\n  edge: top; height: 40\n  graph p.n {{ history: 60s }}\n}}\n"
        )
    };
    let mut shell = boot(&[("shell.strand", &poll("echo 1"))]);
    let one = shell.histories()[0].key.clone();
    shell.reload(&[("shell.strand", &poll("echo 2"))]);
    assert_ne!(shell.histories()[0].key, one, "the command changed");
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
    let g = shell.graphs()[0];
    assert_eq!(
        shell.points(g),
        [(4.0, 0.1, 0.0), (3.0, 0.3, 0.0), (1.0, 0.6, 0.0)]
    );
}

/// A graph in a panel, which is hidden, not unmounted: its backlog
/// binding catches up when the panel shows again, with what came while
/// it was hidden, timed on the boot clock (no shift however long the
/// graph waited).
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
    shell.open(36.0, true);
    assert_eq!(shell.graphs(), [g], "the same graph");
    let last = *shell.points(g).last().unwrap();
    assert_eq!(last, (31.0, 0.8, 3.0), "taken 31 s before it showed");
}

/// (review) Time the machine slept counts (the boot clock): points from
/// before it age by it, and the slept span is a gap, not the last value
/// held across it.
#[test]
fn a_suspend_is_a_gap_and_ages_what_came_before() {
    let mut shell = boot(&[("shell.strand", ISLAND)]);
    shell.cpu(1.0, 0.3);
    shell.at(2.0);
    shell.sleep(40.0);
    shell.cpu(3.0, 0.6);
    let g = shell.graphs()[0];
    let pts = shell.points(g);
    assert_eq!(pts.len(), 4, "{pts:?}");
    assert_eq!((pts[1].0, pts[1].1), (42.0, 0.3), "aged by the 40 s slept");
    assert!(pts[2].1.is_nan(), "a gap from the last reading before it");
    assert!((pts[2].0 - 41.0).abs() < 0.01, "{pts:?}");
    assert_eq!(pts[3], (0.0, 0.6, 0.0), "no ramp out of a gap");
}

/// (review) A glitch report's replay seeds a kept series with what the
/// live shell held before the replay's window: the points go before
/// the replay's own (one not older than its oldest is dropped), and its
/// graph mounts onto them.
#[test]
fn a_seeded_history_reaches_the_graph() {
    let mut live = boot(&[("shell.strand", ISLAND)]);
    live.cpu(1.0, 0.3);
    live.cpu(2.0, 0.5);
    let h = live.histories();
    // The replay starts 10 s after these, at its own boot.
    let mut replay = boot(&[("shell.strand", ISLAND)]);
    let mut seed: Vec<BacklogPoint> = h[0]
        .ring
        .iter()
        .map(|p| BacklogPoint {
            at: p.at - 10.0,
            ..*p
        })
        .collect();
    seed.push(BacklogPoint {
        at: T0 + 1.0,
        value: 0.9,
        ramp: 0.0,
    });
    assert!(replay.inst.seed_history(&h[0].key, &seed));
    assert!(!replay.inst.seed_history("no such series", &seed));
    replay.flush();
    let g = replay.graphs()[0];
    let values: Vec<(f64, f32)> = replay.points(g).iter().map(|p| (p.0, p.1)).collect();
    assert_eq!(values, [(10.0, 0.1), (9.0, 0.3), (8.0, 0.5), (0.0, 0.1)]);
}

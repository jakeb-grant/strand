//! (history) A run's kept history is timed on its own virtual clock, and
//! a `[seed]`'s `history:<key>` (a glitch report's replay) reaches a
//! graph as it mounts (docs/eval.md, "Replaying a glitch report").

use std::rc::Rc;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_eval::harness::{Driver, RunOptions};
use strand_eval::scenario::Scenario;

const SHELL: &str = "state level = 0.0
bar Top {
  edge: top; height: 40
  row { graph level { width: 100; height: 40; history: 10s; fill: #ff0000 } }
}
";

/// The series key `SHELL`'s graph keeps.
fn key() -> String {
    let mut map = SourceMap::new();
    map.add("shell.strand", SHELL.to_string());
    let build = Build::compile(None, map).unwrap_or_else(|d| panic!("{d:#?}"));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let inst = Instance::from_build(&rt, &build, host, Storage::none());
    let h = inst.histories();
    assert_eq!(h.len(), 1, "{h:?}");
    h[0].key.clone()
}

/// The bar's pixel at `(x, y)` after the steps, BGRA.
fn pixel(seed: Option<&str>, x: usize, y: usize) -> [u8; 4] {
    let seed = seed.map_or_else(String::new, |s| {
        format!(
            "\n[seed]\n{} = {}\n",
            toml_str(&format!("history:{}", key())),
            toml_str(s)
        )
    });
    let text = format!(
        "id = \"history-seed\"\nscreen = {{ width = 200, height = 100, scale = 1.0 }}\n{seed}\n=== shell.strand\n{SHELL}\n=== steps\nframes 30\n"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history-seed.scn");
    std::fs::write(&path, &text).unwrap();
    let sc = Scenario::parse(&text, &path).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let steps = sc.steps.clone();
    let mut d = Driver::boot(sc, &RunOptions::default()).unwrap();
    for step in &steps {
        if let Some(mut run) = d.begin(step).unwrap() {
            while !d.advance(&mut run).unwrap() {}
        }
    }
    let s = &d.surfaces().shown[0];
    let stride = s.size.w as usize * 4;
    let i = y * stride + x * 4;
    [
        s.pixels[i],
        s.pixels[i + 1],
        s.pixels[i + 2],
        s.pixels[i + 3],
    ]
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[test]
fn a_seeded_history_reaches_the_graph_on_the_runs_clock() {
    // Full until 2 s before the run starts: the graph's left columns are
    // filled, the newest (the value now, 0) are not.
    let seed = "[[-9.5, 1.0, 0.0], [-2.0, 0.0, 0.0]]";
    let seeded = pixel(Some(seed), 5, 30);
    let plain = pixel(None, 5, 30);
    assert_ne!(seeded, plain, "the seeded history is drawn");
    assert!(seeded[2] > 128 && seeded[3] > 128, "red fill: {seeded:?}");
    assert_eq!(
        plain,
        pixel(None, 150, 30),
        "nothing before the run: the bar"
    );
    assert_eq!(pixel(Some(seed), 98, 30), pixel(None, 98, 30), "now: 0");
}

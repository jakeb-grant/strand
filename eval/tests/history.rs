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

/// (graph-flake) A click's change is kept at the tick it acts at. The
/// boot clock is 499.91 s at `T0`, so with 100 ms columns a boundary
/// (500 s) lies between tick 5 (499.993 s) and tick 6 (500.010 s), where
/// `frames 6` puts the click. Kept at tick 5's time (the boot clock as
/// the tick before left it), the step landed in the column from 500 s,
/// a column before a live shell that changed it after 500 s: the
/// four-output e2e report drifted by 59 px so. At 500.010 s the column
/// from 500 s shows 0 and the next one 1.
#[test]
fn a_clicks_change_is_kept_at_the_tick_it_acts_at() {
    // Last drawn in the tick from 500.4 s: the column from 500 s is slot
    // 94 (after the column the history starts in, 93), the value now 99.
    assert_eq!(
        clicked_slots("click 10,10").unwrap(),
        [false, false, false, true, true, true, true, true],
        "slots 92 to 99: the column from 500 s (94) shows 0, the step is in 95"
    );
}

/// (step-time) `at` times the click's change between ticks, as live: at
/// 499.995 s on the boot clock (the tick's is 500.01 s), before the
/// column from 500 s, which shows it.
#[test]
fn an_at_step_times_the_next_steps_change() {
    assert_eq!(
        clicked_slots("at 0.085s\nclick 10,10").unwrap(),
        [false, false, true, true, true, true, true, true],
        "slots 92 to 99: the column from 500 s (94) shows the click"
    );
}

/// (step-time) An `at` further than a period from its tick, or before a
/// time step, is refused.
#[test]
fn an_at_step_is_refused_off_its_tick_or_before_time() {
    let far = clicked_slots("at 0.05s\nclick 10,10").unwrap_err();
    assert!(far.contains("from the tick it acts at"), "{far}");
    let time = clicked_slots("at 0.1s\nframes 1\nclick 10,10").unwrap_err();
    assert!(time.contains("not a time step"), "{time}");
}

/// The graph's slots 92 to 99 shown (not the bar's colour) after `steps`
/// at tick 6 (500.01 s on the boot clock) and 27 frames more.
fn clicked_slots(steps: &str) -> Result<Vec<bool>, String> {
    const CLICKED: &str = "state level = 0.0
bar Top {
  edge: top; height: 40
  row {
    box { width: 40; height: 40; bg: #0000ff; on click { level = 1.0 } }
    graph level { width: 100; height: 40; history: 10s; fill: #ff0000 }
  }
}
";
    let text = format!(
        "id = \"history-click\"\nscreen = {{ width = 200, height = 100, scale = 1.0 }}\nboot = 499.91\n\n=== shell.strand\n{CLICKED}\n=== steps\nframes 6\n{steps}\nframes 27\n"
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history-click.scn");
    std::fs::write(&path, &text).unwrap();
    let sc = Scenario::parse(&text, &path).unwrap_or_else(|e| panic!("{e}\n{text}"));
    let steps = sc.steps.clone();
    let mut d = Driver::boot(sc, &RunOptions::default()).unwrap();
    for step in &steps {
        if let Some(mut run) = d.begin(step).map_err(|e| e.to_string())? {
            while !d.advance(&mut run).map_err(|e| e.to_string())? {}
        }
    }
    let s = &d.surfaces().shown[0];
    let stride = s.size.w as usize * 4;
    // The graph's slot `x` (from x 40) at row 10: the bar's colour
    // under a value of 0, the fill or the line (a riser) at 1.
    let px = |x: usize| {
        let i = 10 * stride + (40 + x) * 4;
        [s.pixels[i], s.pixels[i + 1], s.pixels[i + 2]]
    };
    let bar = px(0);
    Ok((92..100).map(|x| px(x) != bar).collect())
}

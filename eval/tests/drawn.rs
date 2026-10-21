//! (replay-owner review) What a glitch report's `drawn.json` is taken
//! from, `Renderer::drawn_nodes`, is the trace's own record of where each
//! node was drawn: on every painted frame of a scrolled list in a
//! clipped, turned box at scale 1.25, while a spring moves its rows and
//! fades them, each node is where the frame's trace has its `visual` box,
//! at its own opacity, in paint order, on the CPU and on a GPU-presented
//! surface (docs/eval.md, "Replaying a glitch report", "The verdict").

use strand_eval::harness::{RunOptions, run};
use strand_eval::scenario::{BackendMode, Scenario};
use strand_eval::{Error, gpu};

const SCN: &str = r#"id     = "drawn-hold"
about  = "drawn_nodes held to the trace (eval/tests/drawn.rs)"
screen = { width = 400, height = 300, scale = 1.25 }
hz     = 60

=== shell.strand
export state go = false
let rows = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]

panel P {
  anchor: top_left; open: true
  col { id: frame; width: 220; height: 160; clip: true; rotate: 7; bg: #1e1e2e; pad: 6
    scroll { id: sc; height: 140; width: 200
      col { gap: 4
        for r in rows key r {
          row { height: 24; width: 180; bg: #313244
                x: go ? 30 : 0
                opacity: go ? 0.5 : 1 }
        }
      }
    }
  }
}

=== steps
rest
pointer 100,60
scroll 45
frames 3
set shell.go true
frames 4
rest
"#;

/// Each leg writes its own copy: the tests run in parallel, and one
/// reading the file while the other rewrites it read a torn header.
fn scenario(leg: &str) -> Scenario {
    let dir = std::env::temp_dir().join(format!("strand-drawn-{}-{leg}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("drawn-hold.scn");
    std::fs::write(&p, SCN).unwrap();
    Scenario::load(&p).unwrap_or_else(|e| panic!("{e}"))
}

fn held(backend: BackendMode) -> Option<strand_eval::harness::RunOutput> {
    let opts = RunOptions {
        backend: Some(backend),
        log: true,
        drawn: true,
        ..RunOptions::default()
    };
    let leg = match backend {
        BackendMode::Gpu => "gpu",
        _ => "cpu",
    };
    match run(&scenario(leg), &opts) {
        Ok(o) => Some(o),
        Err(e @ Error::NoDevice(_)) if !gpu::required() => {
            eprintln!("skipped: {e}");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

fn assert_held(o: &strand_eval::harness::RunOutput, leg: &str) {
    assert!(
        o.log.drawn.is_empty(),
        "{leg}: drawn_nodes parts from the trace:\n{}",
        o.log.drawn.join("\n")
    );
    assert!(
        o.log.drawn_frames > 8,
        "{leg}: {} frames held",
        o.log.drawn_frames
    );
    let t = &o.trace;
    assert_eq!(t.scenario.scale, 1.25, "{leg}");
    let rows = || t.frames.iter().flat_map(|f| f.n.iter());
    assert!(rows().any(|r| r.rotate != 0.0), "{leg}: nothing turned");
    assert!(rows().any(|r| r.clip.is_some()), "{leg}: nothing clipped");
    assert!(
        t.frames
            .iter()
            .any(|f| f.offsets.values().any(|o| *o > 1.0)),
        "{leg}: nothing scrolled"
    );
    assert!(
        rows().any(|r| r.spring[0].abs() > 1.0),
        "{leg}: no spring in flight"
    );
    assert!(
        rows().any(|r| r.own_op > 0.5 && r.own_op < 1.0),
        "{leg}: no fade in flight"
    );
}

#[test]
fn drawn_nodes_are_where_the_trace_drew_them_on_the_cpu() {
    let o = held(BackendMode::Cpu).expect("the CPU leg runs");
    assert_held(&o, "cpu");
}

#[test]
fn drawn_nodes_are_where_the_trace_drew_them_on_the_gpu() {
    let Some(o) = held(BackendMode::Gpu) else {
        return;
    };
    assert_eq!(o.trace.scenario.backend, "gpu");
    assert_held(&o, "gpu");
}

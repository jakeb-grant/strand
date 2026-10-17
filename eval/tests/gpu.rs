//! The GPU leg on a real device (docs/eval.md, "CPU and GPU"): lavapipe
//! in the container and CI (`STRAND_GPU_SOFTWARE=1`); skipped without a
//! device unless `STRAND_REQUIRE_GPU=1`, as crates/strand-render/tests/
//! gpu.rs is.

use strand_eval::checks::{CheckId, Status};
use strand_eval::harness::{RunOptions, run};
use strand_eval::scenario::{BackendMode, Scenario, eval_root};
use strand_eval::{Error, check_scenario, gpu};

fn crossfade() -> Scenario {
    Scenario::load(&eval_root().join("scenarios/curated/cpu-gpu-crossfade.scn"))
        .unwrap_or_else(|e| panic!("{e}"))
}

/// Whether a missing device is a skip here (and says so).
fn skipped(e: &Error) -> bool {
    if matches!(e, Error::NoDevice(_)) && !gpu::required() {
        eprintln!("skipped: {e}");
        return true;
    }
    false
}

/// Every frame of the GPU leg is drawn by the GPU (the renderer rasters
/// nothing: `raster_px` is 0 with damage), the images are its pixels,
/// and the nodes are where the CPU leg has them.
#[test]
fn the_gpu_leg_draws_every_frame_on_the_gpu() {
    let sc = crossfade();
    let opts = |b| RunOptions {
        images: true,
        backend: Some(b),
        ..RunOptions::default()
    };
    let g = match run(&sc, &opts(BackendMode::Gpu)) {
        Ok(g) => g,
        Err(e) if skipped(&e) => return,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(g.trace.scenario.backend, "gpu");
    let damaged: Vec<_> = g
        .trace
        .frames
        .iter()
        .filter(|f| f.cost.values().any(|c| c.damage_px > 0))
        .collect();
    assert!(damaged.len() > 10, "{} frames drew", damaged.len());
    for f in &g.trace.frames {
        for (s, c) in &f.cost {
            assert_eq!(c.raster_px, 0, "k{} surface {s}: rastered on the CPU", f.k);
        }
    }
    assert!(!g.images.is_empty());
    let c = run(&sc, &opts(BackendMode::Cpu)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(c.trace.scenario.backend, "cpu");
    assert!(
        c.trace
            .frames
            .iter()
            .any(|f| f.cost.values().any(|c| c.raster_px > 0)),
        "the CPU leg rasters"
    );
    // The same nodes, frame by frame, at the same places; opacity within
    // 1e-4. A frame with nothing new still presents on the GPU (its
    // target has no age: all of it is damage) and stamps the surface's
    // last painted time, where the CPU's draws nothing and does not: a
    // motion starting at the next frame takes its first step from
    // there, a frame shorter on the GPU (the exit fade after the rest
    // here, 5e-6 apart). docs/decisions.md eval-sandbox, the driver's
    // wakes.
    let rows = |t: &strand_eval::trace::RunTrace| -> Vec<_> {
        t.frames
            .iter()
            .map(|f| {
                let n: Vec<_> =
                    f.n.iter()
                        .map(|r| {
                            let mut r = r.clone();
                            r.op = (r.op * 1e4).round() / 1e4;
                            r.own_op = (r.own_op * 1e4).round() / 1e4;
                            r
                        })
                        .collect();
                (f.k, n)
            })
            .collect()
    };
    assert_eq!(
        rows(&c.trace),
        rows(&g.trace),
        "the same nodes, frame by frame"
    );
}

/// The committed `both` scenario passes `cpu-gpu` (it runs: it is not
/// skipped with a device).
#[test]
fn the_crossfade_scenario_passes_cpu_gpu() {
    let sc = crossfade();
    let c = check_scenario(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    let st = c.report.status(CheckId::CpuGpu).cloned();
    match st {
        Some(Status::Skip(why)) if !gpu::required() && why.contains("no GPU device") => {
            eprintln!("skipped: {why}");
        }
        Some(Status::Pass) => assert!(c.gpu.is_some()),
        other => panic!("{other:?}\n{}", c.report.lines(&c.run.trace).join("\n")),
    }
    assert!(c.report.ok(), "{}", c.report.lines(&c.run.trace).join("\n"));
}

/// `backend gpu` mid-run moves the shown surfaces to the GPU from their
/// next frame, and `backend cpu` back.
#[test]
fn a_backend_step_switches_the_leg_mid_run() {
    let text = r##"id = "switch"
screen = { width = 200, scale = 1.0 }

=== shell.strand
state on = false
bar Top {
  height: 30
  row { width: 200; height: 30; gap: 6; align: center
    box { id: a; width: 20; height: 12; bg: #00ff00 }
    if on { box { id: b; width: 16; height: 16; bg: #ff00ff; enter { opacity: 0 } } }
  }
}

=== steps
rest
backend gpu
set on true
rest
backend cpu
set on false
rest
"##;
    let sc = Scenario::parse(text, &eval_root().join("scenarios/curated/switch.scn"))
        .unwrap_or_else(|e| panic!("{e}"));
    let out = match run(&sc, &RunOptions::default()) {
        Ok(o) => o,
        Err(e) if skipped(&e) => return,
        Err(e) => panic!("{e}"),
    };
    let on_gpu = |f: &strand_eval::trace::Frame| {
        f.cost.values().all(|c| c.raster_px == 0 && c.damage_px > 0)
    };
    let segs = &out.trace.segments;
    let frames = |i: usize| {
        let (s, e) = (segs[i].s.unwrap_or(0), segs[i].e);
        out.trace
            .frames
            .iter()
            .filter(move |f| f.k > s && f.k <= e && f.cost.values().any(|c| c.damage_px > 0))
    };
    assert!(segs.len() >= 3, "{segs:?}");
    assert!(frames(1).count() > 0 && frames(1).all(on_gpu), "on the GPU");
    assert!(
        frames(2).count() > 0 && frames(2).all(|f| !on_gpu(f)),
        "back on the CPU"
    );
}

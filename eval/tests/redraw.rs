//! The `redraw` check (docs/eval.md, "Checks"; docs/decisions.md
//! eval-refs): a partial redraw's buffer against a full repaint of the
//! same frame. Its comparison on synthetic buffers, its findings on a
//! synthetic trace, a real failure (the renderer's damage mutant, which
//! leaves a band of stale pixels around each change), every frame of
//! both legs compared, and the run unchanged by it: equal digests and
//! equal frame images with the check on and off.

use std::collections::BTreeMap;

use strand_eval::checks::{self, CheckId, Sev, Status, redraw};
use strand_eval::harness::{RunOptions, RunOutput, run};
use strand_eval::scenario::{BackendMode, Scenario, eval_root};
use strand_eval::trace::{Frame, RedrawRec, RunTrace, ScenarioInfo, SurfaceInfo};
use strand_eval::{Error, check_scenario, gpu};

fn load(path: &str) -> Scenario {
    Scenario::load(&eval_root().join("scenarios").join(path)).unwrap_or_else(|e| panic!("{e}"))
}

/// Whether a missing device is a skip here (and says so).
fn skipped(e: &Error) -> bool {
    if matches!(e, Error::NoDevice(_)) && !gpu::required() {
        eprintln!("skipped: {e}");
        return true;
    }
    false
}

/// A `w × h` premultiplied BGRA buffer of one colour.
fn buffer(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
    px.repeat((w * h) as usize)
}

fn set(buf: &mut [u8], w: u32, x: u32, y: u32, px: [u8; 4]) {
    let i = ((y * w + x) * 4) as usize;
    buf[i..i + 4].copy_from_slice(&px);
}

#[test]
fn equal_buffers_compare_equal() {
    let a = buffer(8, 6, [10, 20, 30, 255]);
    assert_eq!(redraw::compare(&a, &a.clone(), 8, 6, &[], 0), None);
    assert_eq!(redraw::CPU_TOLERANCE, 0, "docs/eval.md: none on the CPU");
    assert_eq!(
        redraw::GPU_TOLERANCE,
        1,
        "docs/eval.md: one step on the GPU"
    );
}

/// A band one pixel wide around a redrawn rect, left from the frame
/// before: every pixel of it counted, stale, inside the region reported.
#[test]
fn a_band_outside_the_redrawn_rect_is_stale_and_located() {
    let (w, h) = (10, 8);
    let fresh = buffer(w, h, [0, 0, 200, 255]);
    let mut presented = fresh.clone();
    // The frame redrew [3, 2, 4, 3]; the ring around it kept old pixels.
    let redrawn = [3, 2, 4, 3];
    let mut ring = 0;
    for y in 1..6 {
        for x in 2..8 {
            let inside = (3..7).contains(&x) && (2..5).contains(&y);
            if !inside {
                set(&mut presented, w, x, y, [9, 9, 9, 255]);
                ring += 1;
            }
        }
    }
    let r = redraw::compare(&presented, &fresh, w, h, &[redrawn], 0).expect("a difference");
    assert_eq!(r.px, ring);
    assert_eq!(r.any, ring);
    assert_eq!(r.stale, ring, "{r:?}");
    assert_eq!(r.worst, 200 - 9);
    assert_eq!(r.at, [2, 1, 6, 5]);
    assert_eq!(r.redrawn, Some(redrawn));
}

/// A difference inside what the frame redrew is drawn wrong, not left
/// stale: counted, not stale.
#[test]
fn a_difference_inside_the_redrawn_rect_is_not_stale() {
    let (w, h) = (6, 6);
    let fresh = buffer(w, h, [50, 50, 50, 255]);
    let mut presented = fresh.clone();
    set(&mut presented, w, 2, 3, [51, 50, 50, 255]);
    let r = redraw::compare(&presented, &fresh, w, h, &[[0, 0, 6, 6]], 0).expect("a difference");
    assert_eq!((r.px, r.stale, r.worst), (1, 0, 1));
    assert_eq!(r.at, [2, 3, 1, 1]);
}

/// The GPU leg's step: a pixel one step off is recorded but does not
/// count; two steps off counts.
#[test]
fn the_tolerance_lets_one_step_through_and_not_two() {
    let (w, h) = (4, 4);
    let fresh = buffer(w, h, [50, 50, 50, 255]);
    let mut presented = fresh.clone();
    set(&mut presented, w, 1, 1, [51, 50, 50, 255]);
    let full = [[0, 0, 4, 4]];
    let r = redraw::compare(&presented, &fresh, w, h, &full, redraw::GPU_TOLERANCE).expect("seen");
    assert_eq!((r.any, r.px, r.worst), (1, 0, 1));
    assert_eq!(r.at, [1, 1, 1, 1], "the region of what differs at all");
    set(&mut presented, w, 3, 2, [50, 52, 50, 255]);
    let r = redraw::compare(&presented, &fresh, w, h, &full, redraw::GPU_TOLERANCE).expect("seen");
    assert_eq!((r.any, r.px, r.worst), (2, 1, 2));
    assert_eq!(r.at, [3, 2, 1, 1], "the region of what counts");
}

fn synthetic(redraws: Vec<(u32, RedrawRec)>, compared: u32) -> RunTrace {
    let mut t = RunTrace::new(
        ScenarioInfo {
            id: "synth".into(),
            hz: 60,
            scale: 1.0,
            backend: "cpu".into(),
            ..ScenarioInfo::default()
        },
        Default::default(),
    );
    t.surfaces.push(SurfaceInfo {
        id: 1,
        label: "bar Top".into(),
        kind: "bar".into(),
        ..SurfaceInfo::default()
    });
    for k in 0..3 {
        let mut f = Frame {
            k,
            painted: vec![1],
            sizes: BTreeMap::from([(1, [300.0, 30.0])]),
            ..Frame::default()
        };
        for (at, r) in &redraws {
            if *at == k {
                f.redraw.insert(1, r.clone());
            }
        }
        t.frames.push(f);
    }
    t.redraw = compared;
    t.seal();
    t
}

fn scenario() -> Scenario {
    let text = "id = \"synth\"\n=== shell.strand\nbar Top { height: 30 }\n=== steps\nrest\n";
    let file = eval_root().join("scenarios/regressions/synth.scn");
    Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"))
}

/// The check on synthetic traces: a clean one passes, a stale band
/// fails at its tick naming the surface and the region (later ticks of
/// the same surface counted, not repeated), a frame not compared fails,
/// and a run that compared nothing is a skip.
#[test]
fn the_check_fails_a_synthetic_difference_and_names_its_region() {
    let sc = scenario();
    assert!(sc.runs(CheckId::Redraw), "redraw is a default check");
    let clean = synthetic(Vec::new(), 3);
    let r = checks::evaluate(&sc, &clean, None, None, None);
    assert_eq!(r.status(CheckId::Redraw), Some(&Status::Pass));

    let band = RedrawRec {
        px: 22,
        stale: 22,
        any: 22,
        worst: 191,
        at: [2, 1, 6, 5],
        redrawn: Some([3, 2, 4, 3]),
        ..RedrawRec::default()
    };
    let bad = synthetic(vec![(1, band.clone()), (2, band)], 3);
    let r = checks::evaluate(&sc, &bad, None, None, None);
    assert_eq!(r.status(CheckId::Redraw), Some(&Status::Fail));
    let f = r.failures(CheckId::Redraw);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].k, Some(1));
    assert_eq!(f[0].more, 1);
    for want in [
        "surface 1 (bar Top) on the CPU",
        "22 pixels",
        "[2, 1, 6, 5]",
        "22 of them outside what the frame redrew ([3, 2, 4, 3])",
        "stale",
    ] {
        assert!(f[0].msg.contains(want), "{want:?} not in {}", f[0].msg);
    }

    let within = RedrawRec {
        any: 4,
        worst: 1,
        at: [0, 0, 2, 2],
        ..RedrawRec::default()
    };
    let r = checks::evaluate(&sc, &synthetic(vec![(0, within)], 3), None, None, None);
    assert_eq!(r.status(CheckId::Redraw), Some(&Status::Pass));
    assert!(
        r.results
            .iter()
            .flat_map(|x| &x.findings)
            .any(|f| f.check == "redraw" && f.sev == Sev::Info)
    );

    let r = checks::evaluate(&sc, &synthetic(Vec::new(), 2), None, None, None);
    assert_eq!(r.status(CheckId::Redraw), Some(&Status::Fail));
    assert!(r.failures(CheckId::Redraw)[0].msg.contains("only 2 of 3"));

    let r = checks::evaluate(&sc, &synthetic(Vec::new(), 0), None, None, None);
    assert!(matches!(r.status(CheckId::Redraw), Some(Status::Skip(_))));
}

fn opts(redraw: bool) -> RunOptions {
    RunOptions {
        images: true,
        redraw,
        ..RunOptions::default()
    }
}

/// A real failure: the renderer's damage mutant shrinks each frame's own
/// damage by a pixel on every side (as a damage rect that misses a band
/// of what changed would), and the default checks fail `redraw` on the
/// stale band the partial redraws leave, outside what they redrew. The
/// same scenario without the mutant passes.
#[test]
fn a_damage_band_mutant_fails_redraw() {
    let sc = load("curated/island-card-swell.scn");
    let o = RunOptions {
        damage_inset: 1,
        ..opts(false)
    };
    let c = check_scenario(&sc, &o).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        c.report.status(CheckId::Redraw),
        Some(&Status::Fail),
        "{:?}",
        c.report.results
    );
    let f = c.report.failures(CheckId::Redraw);
    assert!(
        f.iter()
            .any(|f| f.msg.contains("outside what the frame redrew") && f.msg.contains("stale")),
        "{f:?}"
    );
    // The region and the repaint's image are in the trace and the run.
    let rec = c
        .run
        .trace
        .frames
        .iter()
        .flat_map(|f| f.redraw.values())
        .find(|r| r.stale > 0)
        .expect("a stale record");
    assert!(rec.at[2] > 0 && rec.at[3] > 0, "{rec:?}");
    let png = rec.png.as_ref().expect("the repaint's image");
    assert!(c.run.images.contains_key(png));

    let c = check_scenario(&sc, &opts(false)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(c.report.status(CheckId::Redraw), Some(&Status::Pass));
}

/// The inspector's overlay (here its damage flash, a tint over each
/// frame's repaint) is drawn into the buffer after the frame's own items:
/// the full repaint draws it too, so a frame with an overlay on compares
/// equal to its repaint.
#[test]
fn a_frame_with_the_inspector_overlay_is_repainted_with_it() {
    let sc = load("curated/island-card-swell.scn");
    let o = RunOptions {
        damage_flash: true,
        ..opts(false)
    };
    let c = check_scenario(&sc, &o).unwrap_or_else(|e| panic!("{e}"));
    // A pass: every painted frame compared, each equal to its repaint.
    assert_eq!(
        c.report.status(CheckId::Redraw),
        Some(&Status::Pass),
        "{:?}",
        c.report.failures(CheckId::Redraw)
    );
    // The flash is on screen: the run's frames differ from one without it.
    let plain = check_scenario(&sc, &opts(false)).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(images(&c.run), images(&plain.run), "the flash drew");
}

/// The frames' images, by tick and surface.
fn images(o: &RunOutput) -> BTreeMap<(u32, u32), Vec<u8>> {
    o.trace
        .frames
        .iter()
        .flat_map(|f| {
            f.png.iter().map(move |(s, name)| {
                let png = o
                    .images
                    .get(name)
                    .map(|i| i.png.clone())
                    .unwrap_or_default();
                ((f.k, *s), png)
            })
        })
        .collect()
}

/// The check changes nothing the run draws: with it on and off the
/// digests are equal and so is every frame's image, on scenarios with
/// partial redraws, a crossfade, popups, scale 1.5 and late text.
#[test]
fn the_check_does_not_perturb_the_run() {
    for path in [
        "curated/island-card-swell.scn",
        "curated/cpu-gpu-crossfade.scn",
        "curated/popup-grow-centred.scn",
        "curated/status-row-scale-1_5.scn",
        "curated/title-pill-cut-late.scn",
        "curated/enter-exit-row.scn",
    ] {
        let sc = load(path);
        let on = run(&sc, &opts(true)).unwrap_or_else(|e| panic!("{path}: {e}"));
        let off = run(&sc, &opts(false)).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(on.trace.digest, off.trace.digest, "{path}");
        assert_eq!(off.trace.redraw, 0, "{path}");
        let painted: usize = on.trace.frames.iter().map(|f| f.painted.len()).sum();
        assert_eq!(
            on.trace.redraw as usize, painted,
            "{path}: a frame not compared"
        );
        assert!(
            images(&on) == images(&off),
            "{path}: the frames' pixels differ"
        );
        assert!(
            on.trace.frames.iter().all(|f| f.redraw.is_empty()),
            "{path}: {:?}",
            on.trace
                .frames
                .iter()
                .find(|f| !f.redraw.is_empty())
                .map(|f| (f.k, &f.redraw))
        );
    }
}

/// The GPU leg: every frame the GPU drew is compared with the same frame
/// drawn fresh on a device of its own, they agree, and the run (digest
/// and images) is the same with the check off.
#[test]
fn the_gpu_leg_compares_every_frame_and_is_unperturbed() {
    let sc = load("curated/cpu-gpu-crossfade.scn");
    let gpu = |redraw| RunOptions {
        backend: Some(BackendMode::Gpu),
        ..opts(redraw)
    };
    let on = match run(&sc, &gpu(true)) {
        Ok(g) => g,
        Err(e) if skipped(&e) => return,
        Err(e) => panic!("{e}"),
    };
    let off = run(&sc, &gpu(false)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(on.trace.digest, off.trace.digest);
    assert!(images(&on) == images(&off), "the GPU frames' pixels differ");
    let painted: usize = on.trace.frames.iter().map(|f| f.painted.len()).sum();
    assert!(painted > 10, "{painted}");
    assert_eq!(on.trace.redraw as usize, painted);
    let past: Vec<_> = on
        .trace
        .frames
        .iter()
        .flat_map(|f| f.redraw.iter().map(move |(s, r)| (f.k, *s, r.clone())))
        .filter(|(_, _, r)| r.px > 0 || !r.gpu)
        .collect();
    assert!(past.is_empty(), "{past:?}");
    assert!(
        redraw::findings(&on.trace)
            .iter()
            .all(|f| f.sev != Sev::Fail)
    );
}

/// A GPU-leg difference reads as one: the leg is named.
#[test]
fn a_gpu_leg_difference_names_the_leg() {
    let rec = RedrawRec {
        px: 3,
        any: 3,
        worst: 40,
        at: [1, 1, 3, 1],
        redrawn: Some([0, 0, 300, 30]),
        gpu: true,
        ..RedrawRec::default()
    };
    let f = redraw::findings(&synthetic(vec![(2, rec)], 3));
    assert_eq!(f.len(), 1);
    assert!(f[0].msg.contains("on the GPU"), "{}", f[0].msg);
    assert!(!f[0].msg.contains("stale"), "{}", f[0].msg);
}

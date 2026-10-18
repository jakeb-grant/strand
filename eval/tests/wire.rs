//! The surface leg (docs/eval.md, "Surface leg"; decisions.md
//! eval-surface-leg): a scenario's frames through the real surface
//! manager to the fake compositor, on the CPU path, the GPU's hand-off,
//! and the promotions between them. The GPU tests need a device
//! (lavapipe in the container and CI), as tests/gpu.rs's do.

use strand_eval::checks::wire::{late_present, regions_on_wire};
use strand_eval::checks::{CheckId, Status};
use strand_eval::harness::{RunOptions, run};
use strand_eval::scenario::{BackendMode, Scenario, eval_root};
use strand_eval::trace::RunTrace;
use strand_eval::wire::{Path, WireLog};
use strand_eval::{Error, check_scenario, gpu};

fn scenario(text: &str) -> Scenario {
    let file = eval_root().join("scenarios/curated/wire.scn");
    Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"))
}

fn bug23() -> Scenario {
    Scenario::load(&eval_root().join("scenarios/regressions/bug23-gpu-regions.scn"))
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

fn go(sc: &Scenario, opts: RunOptions) -> Result<RunTrace, Error> {
    let out = run(sc, &opts)?;
    assert!(out.notes.is_empty(), "{:?}", out.notes);
    Ok(out.trace)
}

fn log(t: &RunTrace) -> &WireLog {
    t.wire.as_ref().expect("the surface leg ran")
}

/// Every frame that drew went out in a commit of the tick it was painted
/// at, shown by that tick's refresh, with the regions it computed.
fn assert_clean(t: &RunTrace) {
    let log = log(t);
    let due: Vec<_> = log.frames.iter().filter(|f| f.due).collect();
    assert!(!due.is_empty(), "no frame drew");
    for f in &due {
        let c = f
            .commit
            .and_then(|i| log.commits.get(i))
            .unwrap_or_else(|| panic!("frame {f:?} has no commit"));
        assert_eq!(c.k, f.k, "{f:?} committed at k{}", c.k);
        assert_eq!(c.shown, Some(f.k), "{f:?} shown at {:?}", c.shown);
    }
    assert_eq!(regions_on_wire(t), [], "regions-on-wire");
    assert_eq!(late_present(t), [], "late-present");
}

const ISLAND: &str = r##"id = "wire-island"
screen = { width = 300, scale = 1.0 }
surface = true

=== shell.strand
state wide = false
bar Top {
  height: 30
  row { id: strip; width: 300; height: 30; gap: 6; align: center
    box { id: before; width: 30; height: 20; bg: #00ff00 }
    box { id: item; width: 40; height: 30; bg: #303030
      popup { id: pop; trigger: hover; delay: 100ms; width: 240; height: 40
        row { id: card; height: 32; radius: 16; bg: #202020; align: center; pad: 0, 10
              width: wide ? 200 : 90 ~ spring(320, 0.7)
          text "Island" { id: label; color: #ffffff }
        }
      }
    }
    box { id: glass; height: 24; radius: 12; bg: #20202080; blur: 24
          width: wide ? 70 : 40 ~ spring(320, 0.7) }
  }
}

=== steps
rest
glide 250,10 -> 50,10 over 100
wait 200
rest
set wide true
rest
"##;

/// On the CPU every frame is the manager's: painted into its shm buffers
/// and committed with its regions; the popup's input region follows the
/// card as it swells and the bar's blur region the pill.
#[test]
fn cpu_frames_reach_the_compositor_with_their_regions() {
    let t = go(&scenario(ISLAND), RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert_clean(&t);
    let log = log(&t);
    assert!(log.commits.iter().all(|c| c.by != Some(Path::Gpu)));
    // The popup's input region changed on the wire as the card swelled.
    let inputs: std::collections::BTreeSet<_> = log
        .commits
        .iter()
        .filter(|c| c.surface == Some(2) && c.buffer.is_some())
        .filter_map(|c| c.input.clone())
        .collect();
    assert!(inputs.len() > 5, "{inputs:?}");
    // So did the bar's blur region, as the pill widened.
    let blurs: std::collections::BTreeSet<_> = log
        .commits
        .iter()
        .filter(|c| c.surface == Some(1) && c.buffer.is_some())
        .map(|c| c.blur.clone())
        .collect();
    assert!(blurs.len() > 5, "{}", blurs.len());
    // One refresh per tick.
    assert!(log.vblanks.windows(2).all(|w| w[1].k == w[0].k + 1));
}

/// On the GPU a surface's first frame is the manager's (it maps the
/// surface: popups open on it), the rest are presented on the handed-off
/// surface with the regions `sync_presented` set: none of them is the
/// last shm frame's.
#[test]
fn gpu_frames_present_with_their_regions() {
    let t = match go(
        &bug23(),
        RunOptions {
            backend: Some(BackendMode::Gpu),
            ..RunOptions::default()
        },
    ) {
        Ok(t) => t,
        Err(e) if skipped(&e) => return,
        Err(e) => panic!("{e}"),
    };
    assert_clean(&t);
    let log = log(&t);
    for s in [1, 2] {
        let by: Vec<_> = log
            .commits
            .iter()
            .filter(|c| c.surface == Some(s) && c.buffer.is_some())
            .map(|c| c.by)
            .collect();
        assert_eq!(by.first(), Some(&Some(Path::Shm)), "surface {s}: {by:?}");
        assert!(by.len() > 5, "surface {s}: {by:?}");
        assert!(
            by[1..].iter().all(|b| *b == Some(Path::Gpu)),
            "surface {s}: {by:?}"
        );
    }
}

/// A surface promoted to the GPU mid-run is handed off; one demoted is
/// taken back and committed by the manager again, from a whole frame.
#[test]
fn promotions_hand_the_surface_over_and_back() {
    let text = format!(
        "{}backend gpu\nset wide false\nwait 150\nbackend cpu\nset wide true\nrest\n",
        ISLAND.replace("set wide true\nrest\n", "")
    );
    let t = match go(&scenario(&text), RunOptions::default()) {
        Ok(t) => t,
        Err(e) if skipped(&e) => return,
        Err(e) => panic!("{e}"),
    };
    assert_clean(&t);
    let log = log(&t);
    let by: Vec<_> = log
        .commits
        .iter()
        .filter(|c| c.surface == Some(2) && c.buffer.is_some())
        .map(|c| (c.by, c.damage.clone(), c.buffer))
        .collect();
    let first_gpu = by.iter().position(|b| b.0 == Some(Path::Gpu));
    let back = first_gpu.and_then(|i| by[i..].iter().position(|b| b.0 == Some(Path::Shm)));
    let (Some(g), Some(b)) = (first_gpu, back) else {
        panic!("no GPU stretch then shm frame: {by:?}");
    };
    // Taken back: the first shm frame repaints the whole buffer.
    let (_, damage, size) = &by[g + b];
    let [w, h] = size.expect("a buffer");
    assert_eq!(damage, &[[0, 0, w as i32, h as i32]], "{by:?}");
}

/// `late-present`: a refresh that never comes leaves the frames of its
/// tick shown a refresh late.
#[test]
fn a_lost_refresh_is_a_late_present() {
    let sc = scenario(ISLAND);
    let clean = go(&sc, RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    // A tick in the middle of the swell, where the popup paints each one.
    let k = log(&clean)
        .frames
        .iter()
        .filter(|f| f.due && f.surface == 2)
        .map(|f| f.k)
        .nth(8)
        .expect("frames of the popup");
    let t = go(
        &sc,
        RunOptions {
            lost_vblanks: vec![k],
            ..RunOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let late = late_present(&t);
    assert!(
        late.iter()
            .any(|f| f.k == Some(k) && f.msg.contains(&format!("frame of k{k}"))),
        "{late:#?}"
    );
    // The regions still match what each commit carries.
    assert_eq!(regions_on_wire(&t), []);
}

/// `surface = true` adds the two checks, and naming one runs the leg; a
/// run without the leg has no log, and the leg runs at scale 1 only.
#[test]
fn the_header_turns_the_leg_on() {
    let sc = scenario(ISLAND);
    assert!(sc.surface);
    assert!(sc.runs(CheckId::RegionsOnWire) && sc.runs(CheckId::LatePresent));
    let named = scenario(&ISLAND.replace("surface = true\n", "checks = [\"late-present\"]\n"));
    assert!(named.surface && named.runs(CheckId::RegionsOnWire));
    let off = scenario(&ISLAND.replace("surface = true\n", ""));
    assert!(!off.surface && !off.runs(CheckId::LatePresent));
    let t = go(&off, RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(t.wire.is_none());
    let file = eval_root().join("scenarios/curated/wire.scn");
    let err = Scenario::parse(&ISLAND.replace("scale = 1.0", "scale = 1.5"), &file)
        .expect_err("the leg at scale 1.5");
    assert!(err.to_string().contains("scale 1"), "{err}");
}

/// The regression scenario passes both legs' surface checks on HEAD (its
/// mutant, eval/mutants/bug23-gpu-regions.patch, fails them).
#[test]
fn bug23_passes_on_both_legs() {
    let c = match check_scenario(&bug23(), &RunOptions::default()) {
        Ok(c) => c,
        Err(e) if skipped(&e) => return,
        Err(e) => panic!("{e}"),
    };
    for check in [CheckId::RegionsOnWire, CheckId::LatePresent] {
        let r = c
            .report
            .results
            .iter()
            .find(|r| r.check == check.name())
            .expect("the check ran");
        assert_eq!(r.status, Status::Pass, "{check:?}: {:#?}", r.findings);
    }
    if let Some(g) = &c.gpu {
        assert!(g.trace.wire.is_some(), "the GPU leg ran the surface leg");
    }
}

/// Held out: every committed curated and regression scenario the leg
/// can run (scale 1 throughout), with the leg turned on, on each of its
/// legs, passes both surface checks and leaves the leg nothing it could
/// not mirror. The checks were written against bug 23's scenario; none
/// of these shaped them, so a finding here is a false positive or a bug.
#[test]
fn every_committed_scenario_passes_the_surface_checks() {
    let mut ran = 0;
    for dir in ["scenarios/curated", "scenarios/regressions"] {
        let mut files: Vec<_> = std::fs::read_dir(eval_root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "scn"))
            .collect();
        files.sort();
        for p in files {
            let sc = Scenario::load(&p).unwrap_or_else(|e| panic!("{e}"));
            if sc.screen.scale != 1.0 {
                continue;
            }
            let legs: &[BackendMode] = match sc.backend {
                BackendMode::Both => &[BackendMode::Cpu, BackendMode::Gpu],
                b => &[if b == BackendMode::Gpu {
                    BackendMode::Gpu
                } else {
                    BackendMode::Cpu
                }],
            };
            for &leg in legs {
                let opts = RunOptions {
                    backend: Some(leg),
                    surface: Some(true),
                    ..RunOptions::default()
                };
                let out = match run(&sc, &opts) {
                    Ok(o) => o,
                    // A `scale` step the leg refuses.
                    Err(Error::Unsupported(why)) if why.contains("surface leg") => continue,
                    Err(e) if skipped(&e) => continue,
                    Err(e) => panic!("{}: {e}", sc.id),
                };
                let leg_notes: Vec<_> = out
                    .notes
                    .iter()
                    .filter(|n| n.starts_with("the surface leg"))
                    .collect();
                assert!(leg_notes.is_empty(), "{} ({leg:?}): {leg_notes:?}", sc.id);
                let t = &out.trace;
                assert_eq!(regions_on_wire(t), [], "{} ({leg:?})", sc.id);
                assert_eq!(late_present(t), [], "{} ({leg:?})", sc.id);
                ran += 1;
            }
        }
    }
    assert!(ran > 15, "{ran} runs");
}

/// (eval-false-alarms) The fake compositor stretches a layer surface
/// over the whole output, so a run in which another surface's exclusive
/// zone sizes one (a left bar under a top bar's zone) is refused with the
/// leg, and runs without it; a zone that only moves a surface (a panel
/// below the bar) is no reason to refuse.
#[test]
fn the_leg_refuses_a_layer_surface_sized_by_another_ones_zone() {
    let bars = |side: &str| {
        format!(
            "id = \"zones\"\nscreen = {{ width = 200, height = 120, scale = 1.0 }}\n\
             === shell.strand\nbar Top {{ height: 20 }}\n{side}\
             panel P {{ anchor: top; open: true; box {{ width: 40; height: 10 }} }}\n\
             === steps\nrest\n"
        )
    };
    let leg = || RunOptions {
        surface: Some(true),
        ..RunOptions::default()
    };
    let sc = scenario(&bars("bar Side { edge: left; width: 30 }\n"));
    match run(&sc, &leg()) {
        Err(Error::Unsupported(why)) => assert!(why.contains("exclusive zone"), "{why}"),
        other => panic!("not refused: {:?}", other.map(|o| o.notes)),
    }
    go(&sc, RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    let sc = scenario(&bars(""));
    let t = go(&sc, leg()).unwrap_or_else(|e| panic!("{e}"));
    assert_clean(&t);
}

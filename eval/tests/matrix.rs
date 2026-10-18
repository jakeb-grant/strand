//! The effect matrix (docs/eval.md, "Matrix"): every effect of
//! effects.toml compiles on the fixture and passes alone, or fails only
//! what known.toml accepts for it; two effects setting the same prop are
//! `n/a`; a known entry that no longer fails is stale; and a failing
//! cell promoted with `--xfail` is a regression the gate reports XFAIL.

use std::collections::BTreeMap;
use std::path::PathBuf;

use strand_eval::checks::Status;
use strand_eval::harness::RunOptions;
use strand_eval::matrix::{self, Effects, Known, Outcome, Plan};
use strand_eval::promote::{self, Ask};
use strand_eval::refs::Refs;
use strand_eval::scenario::{Place, Scenario};
use strand_eval::trace::BuildInfo;

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("matrix-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn set() -> (Effects, String) {
    let dir = matrix::dir();
    let set = Effects::load(&dir.join("effects.toml")).unwrap_or_else(|e| panic!("{e}"));
    let fixture = std::fs::read_to_string(dir.join("fixture.strand.in")).unwrap();
    (set, fixture)
}

fn show(done: &[matrix::Done]) -> String {
    done.iter()
        .filter(|d| d.outcome.gates())
        .map(|d| {
            format!(
                "{} {:?}: {}",
                d.cell.dir_name(),
                d.outcome,
                d.lines.join("; ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_effect_alone_compiles_and_passes_or_is_known() {
    let (set, _) = set();
    // The starting set (plan section 7) and `gpu` (the GPU leg, step 10).
    assert_eq!(set.effects.len(), 23, "the starting set and gpu");
    let mut plan = Plan::new(BuildInfo::default());
    plan.singles = true;
    plan.jobs = 4;
    let done = matrix::run(&plan).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(done.len(), 23);
    assert!(done.iter().all(|d| !d.outcome.gates()), "{}", show(&done));
    // The gpu cell ran both legs and compared them (or skipped without a
    // device where none is required).
    let gpu = done.iter().find(|d| d.cell.name == "gpu").unwrap();
    let checked = gpu.entry.as_ref().unwrap();
    assert!(
        checked
            .report
            .as_ref()
            .and_then(|r| r.status(strand_eval::checks::CheckId::CpuGpu))
            .is_some(),
        "{}",
        show(&done)
    );
    // Each effect does what it says: the motion it is named for shows,
    // in each of the segments named (any frame when none is). The jelly
    // cell's drag is previewed by each pointer motion's wake
    // (`Host::input`) and still squashes (docs/decisions.md jelly-drag).
    // A shared morph goes both ways: the partner unmounting becomes the
    // subject mounting (`on`, from a box painted only by scenes the
    // frames reused: docs/decisions.md shared-morph-on), and back (`off`).
    let has = |name: &str, bit: strand_render::probe::Motions, segments: &[&str]| {
        let d = done.iter().find(|d| d.cell.name == name).unwrap();
        let t = &d.entry.as_ref().unwrap().trace;
        let shows = |within: &dyn Fn(u32) -> bool| {
            t.frames
                .iter()
                .filter(|f| within(f.k))
                .flat_map(|f| &f.n)
                .any(|r| strand_render::probe::Motions(r.emot).contains(bit))
        };
        if segments.is_empty() {
            assert!(shows(&|_| true), "{name}: no {bit:?} in its trace");
        }
        for seg in segments {
            let g = t
                .segments
                .iter()
                .find(|g| g.name == *seg)
                .unwrap_or_else(|| panic!("{name}: no segment {seg}"));
            let (s, e) = (g.s.unwrap_or(0), g.e);
            assert!(
                shows(&|k| k > s && k <= e),
                "{name}: no {bit:?} in its {seg} segment (ticks {s}..={e})"
            );
        }
    };
    use strand_render::probe::Motions as M;
    for (name, bit, segments) in [
        ("enter-fade", M::ENTER, &[][..]),
        ("exit-fade", M::EXIT, &[]),
        ("size-spring", M::SIZE_SPRING, &[]),
        ("shared-morph", M::SHARED_MORPH, &["on", "off"]),
        ("reveal", M::REVEAL, &[]),
        ("stagger", M::STAGGER, &[]),
        ("lean", M::LEAN, &[]),
        ("jelly", M::JELLY, &[]),
        ("keyframes", M::KEYFRAMES, &[]),
    ] {
        has(name, bit, segments);
    }
}

#[test]
fn effects_that_set_the_same_prop_do_not_pair() {
    let (set, fixture) = set();
    let c = matrix::cell(&set, &fixture, &["enter-fade", "enter-slide"], 60);
    assert!(
        c.na.as_deref()
            .is_some_and(|n| n.contains("enter { opacity }")),
        "{:?}",
        c.na
    );
    let c = matrix::cell(&set, &fixture, &["scale-1.5", "scale-2"], 60);
    assert!(c.na.is_some(), "two screen scales");
    let c = matrix::cell(&set, &fixture, &["late-text-1", "late-text-paint"], 60);
    assert!(c.na.as_deref().is_some_and(|n| n.starts_with("excluded")));
    // A pair that touches different props merges into one block, and
    // the cell is a scenario that parses where a regression would live.
    let c = matrix::cell(&set, &fixture, &["enter-fade", "exit-slide"], 60);
    assert!(c.na.is_none(), "{:?}", c.na);
    assert!(c.text.contains("enter {") && c.text.contains("exit {"));
    assert_eq!(c.id(), "m.enter-fade__exit-slide.60");
    let file = matrix::dir().join("cells").join("x.scn");
    Scenario::parse_in(&c.text, &file, Place::Committed).unwrap_or_else(|e| panic!("{e}"));
    // 23 alone and 253 pairs.
    let names: Vec<&str> = set.effects.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        matrix::cells(&set, &fixture, &names, &[60], false).len(),
        276
    );
}

#[test]
fn a_known_failure_that_passes_is_stale() {
    let (set, fixture) = set();
    let c = matrix::cell(&set, &fixture, &["enter-fade"], 60);
    let known = Known {
        checks: vec!["vanish".into()],
        nodes: vec!["#subject".into()],
        note: "for the test".into(),
        since: "2026-10-17".into(),
    };
    let d = matrix::run_cell(c, Some(&known), &BuildInfo::default(), Refs::default());
    assert_eq!(d.outcome, Outcome::Stale, "{:?}", d.lines);
    assert!(d.outcome.gates());
}

/// A cell with no approved reference, or one it left, is CHANGED: it
/// gates, and its first line says so; failing a check it is not known
/// for stays a failure.
#[test]
fn a_cell_without_its_reference_is_changed() {
    let (set, fixture) = set();
    let missing = || Refs {
        required: true,
        ..Refs::default()
    };
    let c = matrix::cell(&set, &fixture, &["enter-fade"], 60);
    let d = matrix::run_cell(c, None, &BuildInfo::default(), missing());
    assert_eq!(d.outcome, Outcome::Changed, "{:?}", d.lines);
    assert!(d.outcome.gates());
    assert!(
        d.lines[0].starts_with("CHANGED: m.enter-fade.60: no approved reference"),
        "{:?}",
        d.lines
    );
    let known = Known {
        checks: vec!["expect".into()],
        nodes: vec!["#after".into()],
        note: "for the test".into(),
        since: "2026-10-17".into(),
    };
    let d = matrix::run_cell(
        failing(&set, &fixture),
        Some(&known),
        &BuildInfo::default(),
        missing(),
    );
    assert_eq!(d.outcome, Outcome::Changed, "{:?}", d.lines);
    let d = matrix::run_cell(
        failing(&set, &fixture),
        None,
        &BuildInfo::default(),
        missing(),
    );
    assert_eq!(d.outcome, Outcome::Fail, "{:?}", d.lines);
}

/// A known entry accepts its checks on the nodes it names only: the
/// same check failing on another node fails the cell.
#[test]
fn a_known_failure_covers_only_the_nodes_it_names() {
    let (set, fixture) = set();
    let known = |nodes: &[&str]| Known {
        checks: vec!["expect".into()],
        nodes: nodes.iter().map(|n| n.to_string()).collect(),
        note: "for the test".into(),
        since: "2026-10-17".into(),
    };
    let run = |k: Known| {
        matrix::run_cell(
            failing(&set, &fixture),
            Some(&k),
            &BuildInfo::default(),
            Refs::default(),
        )
    };
    let d = run(known(&["#after"]));
    assert_eq!(d.outcome, Outcome::Known, "{:?}", d.lines);
    let d = run(known(&["#subject"]));
    assert_eq!(d.outcome, Outcome::Fail, "{:?}", d.lines);
    assert!(
        d.lines.iter().any(|l| l.contains("on #subject only")),
        "{:?}",
        d.lines
    );
    let d = run(known(&["*"]));
    assert_eq!(d.outcome, Outcome::Known, "{:?}", d.lines);
    // An entry without nodes is refused.
    let dir = scratch("known");
    let file = dir.join("known.toml");
    std::fs::write(
        &file,
        "[\"a\"]\nchecks = [\"vanish\"]\nnote = \"n\"\nsince = \"2026-10-17\"\n",
    )
    .unwrap();
    let e = matrix::load_known(&file).unwrap_err().to_string();
    assert!(e.contains("nodes"), "{e}");
}

/// The fixture's `on` mounts the subject between the bystanders, so
/// `#after` moves: an `expect still` on it fails on purpose.
fn failing(set: &Effects, fixture: &str) -> matrix::Cell {
    let mut set = set.clone();
    let e = set
        .effects
        .iter_mut()
        .find(|e| e.name == "enter-fade")
        .unwrap();
    e.post = vec![
        "mark again".into(),
        "set on true".into(),
        "rest".into(),
        "expect still #after x".into(),
    ];
    matrix::cell(&set, fixture, &["enter-fade"], 60)
}

#[test]
fn a_failing_cell_promoted_with_xfail_is_a_regression_the_gate_reports_xfail() {
    let (set, fixture) = set();
    let out = scratch("promote");
    let d = matrix::run_cell(
        failing(&set, &fixture),
        None,
        &BuildInfo::default(),
        Refs::default(),
    );
    assert_eq!(d.outcome, Outcome::Fail, "{:?}", d.lines);
    assert_eq!(d.failed, vec!["expect".to_string()]);
    let mut plan = Plan::new(BuildInfo::default());
    plan.singles = true;
    let mut done = vec![d];
    matrix::write(&out, &plan, &mut done).unwrap_or_else(|e| panic!("{e}"));
    let grid = std::fs::read_to_string(out.join("index.html")).unwrap();
    assert!(grid.contains("href=\"enter-fade@60/index.html\""), "{grid}");
    assert!(out.join("enter-fade@60/index.html").is_file());
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("matrix.json")).unwrap()).unwrap();
    assert_eq!(json["counts"]["fail"], 1);

    // Without --note, an --xfail is refused.
    let to = out.join("regressions");
    let ask = |xfail: Vec<String>, note: Option<&str>| Ask {
        // The cell named without its rate, as the plan's command does.
        from: out.join("enter-fade"),
        name: "enter-fade-still".into(),
        xfail,
        note: note.map(String::from),
        to: Some(to.clone()),
        force: true,
    };
    assert!(promote::promote(&ask(vec!["expect".into()], None)).is_err());
    let path = promote::promote(&ask(vec!["expect".into()], Some("the subject pushes it")))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(path, to.join("enter-fade-still.scn"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("id = \"enter-fade-still\""), "{text}");
    assert!(text.contains("Promoted from"), "{text}");
    assert!(text.contains("=== shell.strand"), "{text}");
    assert!(text.contains("expect still #after x"), "{text}");
    let sc = Scenario::parse_in(&text, &path, Place::Committed).unwrap_or_else(|e| panic!("{e}"));
    let c = strand_eval::check_scenario(&sc, &RunOptions::default()).unwrap();
    let status: BTreeMap<&str, &Status> = c
        .report
        .results
        .iter()
        .map(|r| (r.check.as_str(), &r.status))
        .collect();
    assert!(
        matches!(status.get("expect"), Some(Status::Xfail(_))),
        "{status:?}"
    );
    assert!(c.report.ok(), "XFAIL does not fail the gate");
}

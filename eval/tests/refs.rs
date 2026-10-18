//! Reference recordings (docs/eval.md, "Reference recordings"): a run's
//! recording round-trips through its text; a run that leaves its approved
//! reference beyond a quantum is CHANGED, naming the first tick, node and
//! field; a missing reference fails where one is required; `approve`
//! writes references with a note it requires; the CI side refuses a hand
//! edit and a recording changed under its old note; the committed
//! references stay small and cover every committed scenario.

use std::path::{Path, PathBuf};

use strand_eval::checks::{self, CheckId, Status};
use strand_eval::harness::RunOptions;
use strand_eval::recording::{self, Recording, Tol, decode, encode};
use strand_eval::refs::{self, Ask, RefFile, Verdict};
use strand_eval::report::{self, Entry, Frames, RunInfo};
use strand_eval::scenario::{Scenario, eval_root};
use strand_eval::trace::{PoseRec, Regions, RunTrace};

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("refs-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A row whose middle box enters and pushes the last one right.
const ROW: &str = r##"id = "refs-row"
screen = { width = 300, scale = 1.0 }

=== shell.strand
state on = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 6; align: center; pad: 0, 10
    box { id: left; width: 20; height: 12; bg: #00ff00 }
    if on { box { id: mid; width: 16; height: 16; bg: #ff00ff; enter { opacity: 0 } } }
    box { id: right; width: 20; height: 12; bg: #0000ff }
  }
}

=== steps
rest
mark on
set on true
rest
"##;

fn scenario(text: &str) -> Scenario {
    let file = eval_root().join("scenarios/curated/refs.scn");
    Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"))
}

fn run(text: &str) -> RunTrace {
    strand_eval::harness::run(&scenario(text), &RunOptions::default())
        .unwrap_or_else(|e| panic!("{e}"))
        .trace
}

fn key(t: &RunTrace, id: &str) -> String {
    t.nodes
        .iter()
        .find(|(_, m)| m.id.as_deref() == Some(id))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| panic!("no #{id}"))
}

/// Writes `text`'s checked run under `out` (as `check` does) and returns
/// its scenario directory.
fn checked_run(text: &str, out: &Path, refs_dir: &Path) -> PathBuf {
    let sc = scenario(text);
    let c = strand_eval::check_with(
        &sc,
        &RunOptions::default(),
        refs::for_scenario(&sc, refs_dir, true),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (r, reference) = report::refs_of(c.refs.as_ref(), &c.report, &c.run.trace);
    let mut entries = vec![Entry {
        id: sc.id.clone(),
        file: sc.file.display().to_string(),
        source: sc.source.clone(),
        trace: c.run.trace,
        report: Some(c.report),
        refs: r,
        reference,
        ..Entry::default()
    }];
    let info = RunInfo {
        command: "check".into(),
        out: out.to_path_buf(),
        frames: Frames::None,
    };
    report::write_run(&info, &mut entries, &[]).unwrap();
    out.join(&sc.id)
}

fn ask(dir: &Path, note: &str) -> Ask {
    Ask {
        verdict: Verdict::Good,
        note: note.into(),
        approver: "tester".into(),
        dir: dir.to_path_buf(),
    }
}

// --- the recording

#[test]
fn a_recording_round_trips_through_its_text() {
    let t = run(ROW);
    let rec = Recording::from_trace(&t);
    assert!(!rec.ticks.is_empty());
    let text = encode(&rec);
    assert_eq!(decode(&text, 1).unwrap_or_else(|e| panic!("{e}")), rec);
    // Everything the text carries besides the defaults: a visual box
    // apart from the drawn one, hidden and text states, a pose, an input
    // region, a surface that goes, fractions of a pixel.
    let mut t2 = t.clone();
    let n = t2.frames.len();
    for f in t2.frames.iter_mut().skip(n / 2) {
        for r in &mut f.n {
            r.visual[0] += 1.25;
            r.drawn[1] += 0.015625;
            r.hidden = Some("opacity0".into());
            r.text = Some("E".into());
            r.scale = [0.5, 0.75];
            r.rotate = 12.5;
            r.op = 0.5;
        }
        for s in f.painted.clone() {
            f.poses.insert(
                s,
                PoseRec {
                    opacity: 0.5,
                    scale: 0.9,
                    x: 3.0,
                    y: -2.5,
                },
            );
            f.regions.insert(
                s,
                Regions {
                    input: Some(vec![[1.0, 2.0, 30.5, 4.0]]),
                    ..Regions::default()
                },
            );
        }
    }
    if let Some(f) = t2.frames.last_mut() {
        f.sizes.clear();
        f.n.clear();
    }
    let rec2 = Recording::from_trace(&t2);
    let text2 = encode(&rec2);
    assert_eq!(decode(&text2, 1).unwrap_or_else(|e| panic!("{e}")), rec2);
    assert!(text2.contains("hid=opacity0"), "{text2}");
    assert!(text2.contains("P=128,0.9,3,-2.5"), "{text2}");
    assert!(text2.contains(" -\n"), "a node or surface leaves: {text2}");
}

#[test]
fn a_recording_is_a_function_of_its_scenario() {
    assert_eq!(
        encode(&Recording::from_trace(&run(ROW))),
        encode(&Recording::from_trace(&run(ROW)))
    );
}

#[test]
fn a_diff_names_the_first_tick_node_and_field() {
    let t = run(ROW);
    let rec = Recording::from_trace(&t);
    assert_eq!(recording::diff(&rec, &rec, Tol::QUANTUM), None);
    let left = key(&t, "left");
    let mut moved = t.clone();
    let from = moved.frames[moved.frames.len() / 2].k;
    for f in moved.frames.iter_mut().filter(|f| f.k >= from) {
        for r in f.n.iter_mut().filter(|r| r.key == left) {
            r.drawn[0] += 3.0;
            r.visual[0] += 3.0;
        }
    }
    let d = recording::diff(&rec, &Recording::from_trace(&moved), Tol::QUANTUM)
        .expect("a moved node differs");
    assert_eq!(d.first.k, from);
    assert_eq!(d.first.key.as_deref(), Some(left.as_str()));
    assert_eq!(d.first.field, "drawn.x");
    assert_eq!(d.first.label, "box#left");
    let was = t
        .frames
        .iter()
        .find(|f| f.k == from)
        .and_then(|f| f.n.iter().find(|r| r.key == left))
        .map(|r| r.drawn[0])
        .unwrap();
    assert_eq!(d.first.reference, format!("{was}"));
    assert_eq!(d.first.run, format!("{}", was + 3.0));
    assert!(
        d.first
            .line()
            .starts_with(&format!("k{from} box#left drawn.x ")),
        "{}",
        d.first.line()
    );
    assert_eq!(d.per_node.len(), 1, "one node: {:?}", d.per_node);
    assert!(d.counts().contains(" 1 node"), "{}", d.counts());
    // A tick painted in one and not the other is named too.
    let mut fewer = t.clone();
    let gone = fewer.frames.remove(2).k;
    let d = recording::diff(&rec, &Recording::from_trace(&fewer), Tol::QUANTUM).unwrap();
    assert_eq!((d.first.k, d.first.field.as_str()), (gone, "painted"));
    assert_eq!(d.first.run, "nothing");
}

#[test]
fn one_quantum_is_the_same_and_two_are_not() {
    let t = run(ROW);
    let rec = Recording::from_trace(&t);
    let right = key(&t, "right");
    let nudged = |dx: f32, dop: f32| {
        let mut m = t.clone();
        for f in &mut m.frames {
            for r in f.n.iter_mut().filter(|r| r.key == right) {
                r.drawn[0] += dx;
                r.op -= dop;
            }
        }
        recording::diff(&rec, &Recording::from_trace(&m), Tol::QUANTUM)
    };
    // 1/64 px and 1/255 opacity are one quantum: the same.
    assert_eq!(nudged(1.0 / 64.0, 0.0), None);
    assert_eq!(nudged(0.0, 1.0 / 255.0), None);
    // A last-bit difference that rounds the other way is the same.
    assert_eq!(nudged(0.5 / 64.0 + 1e-4, 0.0), None);
    // Two quanta are not.
    let d = nudged(2.0 / 64.0, 0.0).expect("two quanta of x");
    assert_eq!(d.first.field, "drawn.x");
    let d = nudged(0.0, 2.0 / 255.0).expect("two quanta of opacity");
    assert_eq!(d.first.field, "opacity");
    // A bad recording is reproduced within `tol` on geometry.
    let mut m = t.clone();
    for f in &mut m.frames {
        for r in f.n.iter_mut().filter(|r| r.key == right) {
            r.drawn[0] += 0.4;
        }
    }
    let m = Recording::from_trace(&m);
    assert!(recording::diff(&rec, &m, Tol::QUANTUM).is_some());
    assert!(recording::diff(&rec, &m, Tol::px(0.5)).is_none());
}

// --- the gate

#[test]
fn a_missing_reference_fails_where_one_is_required() {
    let sc = scenario(ROW);
    let t = run(ROW);
    let empty = scratch("missing");
    let r = refs::for_scenario(&sc, &empty, false);
    let rep = checks::evaluate(&sc, &t, None, None, Some(&r));
    assert!(matches!(
        rep.status(CheckId::Reference),
        Some(Status::Skip(_))
    ));
    assert!(rep.ok(), "no reference is a skip outside the gate");
    let r = refs::for_scenario(&sc, &empty, true);
    let rep = checks::evaluate(&sc, &t, None, None, Some(&r));
    assert_eq!(rep.status(CheckId::Reference), Some(&Status::Changed));
    assert!(!rep.ok());
    let line = rep.changed_line().unwrap();
    assert!(
        line.starts_with("CHANGED: refs-row: no approved reference"),
        "{line}"
    );
    // `check --gate` on a scenario with none fails, and says so.
    let dir = scratch("gate");
    let scn = dir.join("refs-missing.scn");
    std::fs::write(&scn, ROW.replace("\"refs-row\"", "\"refs-missing\"")).unwrap();
    let out = dir.join("out");
    let e = strand_eval::main_with(&[
        "check".into(),
        "--gate".into(),
        scn.display().to_string(),
        "--out".into(),
        out.display().to_string(),
    ])
    .unwrap_err();
    assert!(matches!(e, strand_eval::Error::Failed(_)), "{e}");
    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(
        summary.contains("CHANGED: refs-missing: no approved reference"),
        "{summary}"
    );
    assert!(summary.contains("eval approve "), "{summary}");
}

#[test]
fn approve_round_trips_and_a_changed_run_is_named() {
    let dir = scratch("approve");
    let refs_dir = dir.join("refs");
    // No reference: CHANGED (missing), the page has no B side.
    let first = checked_run(ROW, &dir.join("one"), &refs_dir);
    let done = refs::approve(
        std::slice::from_ref(&first),
        &ask(&refs_dir, "the row as it should be"),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(done.len(), 1);
    assert!(done[0].wrote);
    let file = refs_dir.join("refs-row.ref");
    assert_eq!(done[0].file, file);
    let f = RefFile::load(&file).unwrap().unwrap();
    let b = f.good("refs-row").unwrap();
    assert_eq!(b.note, "the row as it should be");
    assert_eq!(b.approver, "tester");
    assert_eq!(b.date, refs::today());
    assert_eq!(b.scenario, refs::source_hash(&scenario(ROW).source));
    assert!(refs::lint(&refs_dir).unwrap().is_empty());
    // The same run again matches, and approving it writes nothing.
    let again = checked_run(ROW, &dir.join("two"), &refs_dir);
    let e = report::read_entry(&again).unwrap();
    assert_eq!(
        e.report.as_ref().unwrap().status(CheckId::Reference),
        Some(&Status::Pass)
    );
    let done = refs::approve(
        std::slice::from_ref(&again),
        &ask(&refs_dir, "the row as it should be"),
    )
    .unwrap();
    assert!(!done[0].wrote);
    assert!(
        refs::changed(&dir.join("two"), &refs_dir)
            .unwrap()
            .is_empty()
    );
    // A change is CHANGED, names its first difference, and the page puts
    // the reference beside the run.
    let wide = ROW.replace("box { id: left; width: 20", "box { id: left; width: 24");
    let changed = checked_run(&wide, &dir.join("three"), &refs_dir);
    let e = report::read_entry(&changed).unwrap();
    let rep = e.report.as_ref().unwrap();
    assert_eq!(rep.status(CheckId::Reference), Some(&Status::Changed));
    let line = rep.changed_line().unwrap();
    assert!(line.starts_with("CHANGED: refs-row: k"), "{line}");
    assert!(
        line.contains("box#left drawn.w 24 (reference 20)"),
        "{line}"
    );
    assert!(line.contains("the scenario's text changed"), "{line}");
    let page = std::fs::read_to_string(dir.join("three/index.html")).unwrap();
    let data = report::html::embedded(&page).unwrap();
    assert_eq!(data["runs"].as_array().unwrap().len(), 2);
    assert_eq!(data["runs"][1]["reference"], true);
    assert_eq!(data["runs"][0]["scenarios"][0]["refs"]["status"], "changed");
    assert_eq!(data["runs"][0]["scenarios"][0]["status"], "changed");
    assert!(data["ab"]["scenarios"]["refs-row"]["same"] == false);
    assert_eq!(
        refs::changed(&dir.join("three"), &refs_dir).unwrap(),
        vec![changed.clone()]
    );
    // Approving the change under the old note is refused; a new note
    // replaces the reference.
    let e = refs::approve(
        std::slice::from_ref(&changed),
        &ask(&refs_dir, "the row as it should be"),
    )
    .unwrap_err();
    assert!(
        matches!(e, strand_eval::Error::Usage(ref m) if m.contains("note")),
        "{e}"
    );
    let old = std::fs::read_to_string(&file).unwrap();
    let done = refs::approve(
        std::slice::from_ref(&changed),
        &ask(&refs_dir, "the left box is wider now"),
    )
    .unwrap();
    assert!(done[0].wrote);
    let new = std::fs::read_to_string(&file).unwrap();
    assert!(refs::note_rule(Some(&old), &new, "x").is_empty());
    let f = RefFile::load(&file).unwrap().unwrap();
    assert_eq!(f.blocks.len(), 1);
    assert_eq!(
        f.good("refs-row").unwrap().note,
        "the left box is wider now"
    );
    let after = checked_run(&wide, &dir.join("four"), &refs_dir);
    let e = report::read_entry(&after).unwrap();
    assert_eq!(
        e.report.as_ref().unwrap().status(CheckId::Reference),
        Some(&Status::Pass)
    );
    // `--all-changed` takes only what changed.
    let done = strand_eval::main_with(&[
        "approve".into(),
        "--all-changed".into(),
        dir.join("four").display().to_string(),
        "--note".into(),
        "nothing to do".into(),
        "--refs".into(),
        refs_dir.display().to_string(),
    ]);
    assert!(done.is_ok(), "{done:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), new);
}

#[test]
fn a_note_is_required() {
    let dir = scratch("note");
    let one = checked_run(ROW, &dir.join("one"), &dir.join("refs"));
    for note in [None, Some(""), Some("   ")] {
        let mut args = vec![
            "approve".to_string(),
            one.display().to_string(),
            "--refs".into(),
            dir.join("refs").display().to_string(),
        ];
        if let Some(n) = note {
            args.extend(["--note".into(), n.into()]);
        }
        let e = strand_eval::main_with(&args).unwrap_err();
        assert!(
            matches!(e, strand_eval::Error::Usage(ref m) if m.contains("note")),
            "{e}"
        );
    }
    let e = refs::approve(std::slice::from_ref(&one), &ask(&dir.join("refs"), " ")).unwrap_err();
    assert!(matches!(e, strand_eval::Error::Usage(_)), "{e}");
    assert!(!dir.join("refs").exists(), "nothing written without a note");
}

/// A matrix cell's reference lives in its rate's file beside the others;
/// approving one leaves the rest as they were.
#[test]
fn matrix_cells_share_their_rates_file() {
    let dir = scratch("matrix");
    let refs_dir = dir.join("refs");
    let t = run(ROW);
    let cell = |name: &str, out: &Path| {
        let mut e = Entry {
            id: format!("m.{name}.60"),
            file: format!("eval/matrix ({name})"),
            source: ROW.into(),
            trace: t.clone(),
            ..Entry::default()
        };
        e.trace.scenario.id = e.id.clone();
        e.trace.scenario.file = eval_root()
            .join(format!("matrix/cells/{name}@60.scn"))
            .display()
            .to_string();
        report::write_entry(&out.join(format!("{name}@60")), &mut e, Frames::None).unwrap();
        out.join(format!("{name}@60")).join(&e.id)
    };
    let out = dir.join("m");
    let a = cell("a", &out);
    let b = cell("b", &out);
    let done = refs::approve(&[a.clone(), b], &ask(&refs_dir, "first")).unwrap();
    assert!(
        done.iter()
            .all(|d| d.wrote && d.file == refs_dir.join("matrix-60.ref"))
    );
    let f = RefFile::load(&refs_dir.join("matrix-60.ref"))
        .unwrap()
        .unwrap();
    assert_eq!(
        f.blocks.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
        ["m.a.60", "m.b.60"]
    );
    assert!(refs::lint(&refs_dir).unwrap().is_empty());
    // `<matrix>/<cell>@<hz>` names the cell's run.
    assert_eq!(refs::entry_dirs(&out.join("a@60")).unwrap(), vec![a]);
    assert_eq!(refs::entry_dirs(&out).unwrap().len(), 2);
    let before = f.good("m.b.60").unwrap().clone();
    let mut moved = t.clone();
    for f in &mut moved.frames {
        for r in &mut f.n {
            r.drawn[1] += 2.0;
        }
    }
    let mut e = Entry {
        id: "m.a.60".into(),
        source: ROW.into(),
        trace: moved,
        ..Entry::default()
    };
    e.trace.scenario.id = e.id.clone();
    e.trace.scenario.file = eval_root()
        .join("matrix/cells/a@60.scn")
        .display()
        .to_string();
    report::write_entry(&dir.join("m2/a@60"), &mut e, Frames::None).unwrap();
    refs::approve(&[dir.join("m2/a@60/m.a.60")], &ask(&refs_dir, "second")).unwrap();
    let f = RefFile::load(&refs_dir.join("matrix-60.ref"))
        .unwrap()
        .unwrap();
    assert_eq!(f.good("m.a.60").unwrap().note, "second");
    assert_eq!(f.good("m.b.60").unwrap(), &before);
}

// --- the CI side

#[test]
fn a_hand_edit_and_a_change_under_the_old_note_are_refused() {
    let dir = scratch("lint");
    let refs_dir = dir.join("refs");
    let one = checked_run(ROW, &dir.join("one"), &refs_dir);
    refs::approve(std::slice::from_ref(&one), &ask(&refs_dir, "as approved")).unwrap();
    let file = refs_dir.join("refs-row.ref");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(refs::lint(&refs_dir).unwrap().is_empty());
    // A recording edited by hand breaks its seal.
    let edited = text.replacen("w=", "w=1", 1);
    std::fs::write(&file, &edited).unwrap();
    let problems = refs::lint(&refs_dir).unwrap();
    assert!(problems.iter().any(|p| p.contains("seal")), "{problems:?}");
    // A recording that changed under its old note is refused against the
    // base; with a new note it passes.
    let mut f = RefFile::parse(&text).unwrap();
    let b = f.blocks[0].clone();
    let block = |note: &str| {
        let meta = refs::Meta {
            verdict: Verdict::Good,
            note,
            date: "2026-10-18",
            rev: "abc",
            approver: "x",
            scenario: "",
        };
        refs::Block::new(&b.id, &meta, b.body.replacen("w=", "w=1", 1))
    };
    f.blocks[0] = block("as approved");
    let same_note = f.render();
    let problems = refs::note_rule(Some(&text), &same_note, "eval/refs/refs-row.ref");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("note did not"), "{problems:?}");
    f.blocks[0] = block("moved on purpose");
    assert!(refs::note_rule(Some(&text), &f.render(), "x").is_empty());
    // A new file only needs its notes (lint).
    assert!(refs::note_rule(None, &same_note, "x").is_empty());
    // Something else in the directory is named.
    std::fs::write(&file, &text).unwrap();
    std::fs::write(refs_dir.join("stray.json"), "{}").unwrap();
    let problems = refs::lint(&refs_dir).unwrap();
    assert!(
        problems.iter().any(|p| p.contains("stray.json")),
        "{problems:?}"
    );
}

/// The committed references: every committed scenario has an approved
/// one, both matrix rates have their file, every block holds its seal,
/// and all of it stays under its size (a tripwire: a recording that
/// grows past it is a format that stopped being compact, docs/eval.md).
#[test]
fn the_committed_references_cover_every_scenario_and_stay_small() {
    let dir = refs::dir();
    let problems = refs::lint(&dir).unwrap();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    for sub in ["curated", "regressions"] {
        for e in std::fs::read_dir(eval_root().join("scenarios").join(sub)).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_none_or(|x| x != "scn") {
                continue;
            }
            let sc = Scenario::load(&p).unwrap_or_else(|e| panic!("{e}"));
            let r = refs::for_scenario(&sc, &dir, true);
            assert!(r.good.is_some(), "{}: no approved reference", sc.id);
        }
    }
    let mut total = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let len = std::fs::metadata(&p).unwrap().len();
        total += len;
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with("matrix-") {
            let cells = RefFile::load(&p).unwrap().unwrap().blocks.len() as u64;
            assert!(
                len <= cells * refs::MATRIX_CELL_BUDGET,
                "{name}: {} bytes a cell, over {}",
                len / cells.max(1),
                refs::MATRIX_CELL_BUDGET
            );
        } else {
            assert!(
                len <= refs::SCENARIO_FILE_BUDGET,
                "{name}: {len} bytes, over {}",
                refs::SCENARIO_FILE_BUDGET
            );
        }
    }
    for hz in [60, 144] {
        assert!(
            dir.join(format!("matrix-{hz}.ref")).is_file(),
            "matrix-{hz}.ref"
        );
    }
    assert!(
        total <= refs::TOTAL_BUDGET,
        "eval/refs/: {total} bytes, over {}",
        refs::TOTAL_BUDGET
    );
}

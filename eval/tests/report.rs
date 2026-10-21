//! Reports (docs/eval.md, "Reports" and "The viewer"): the page is one
//! self-contained file whose embedded trace round-trips and whose images
//! are thinned to stay under its budget; a failing finding gets a contact
//! sheet, a summary line and an `explain` table; verdicts become
//! references the `reference` check reads; A/B names the first frame two
//! runs differ at.

use std::collections::BTreeMap;
use std::path::PathBuf;

use strand_eval::checks::{self, CheckId, Status};
use strand_eval::harness::{Image, RunOptions};
use strand_eval::report::{self, Entry, Frames, RunInfo, html};
use strand_eval::scenario::{Scenario, eval_root};
use strand_eval::trace::RunTrace;

fn scenario(text: &str) -> Scenario {
    let file = eval_root().join("scenarios/curated/report.scn");
    Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"))
}

/// A fresh directory under cargo's test scratch space.
fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("report-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A row whose count re-enters: the last box glides right to make room,
/// and the `expect still` on it is wrong on purpose, so the run has a
/// failing finding on a node.
const ROW: &str = r##"id = "report-row"
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
shot settled
expect still #right x
"##;

fn checked(text: &str) -> Entry {
    let sc = scenario(text);
    let opts = RunOptions {
        images: true,
        ..RunOptions::default()
    };
    let c = strand_eval::check_scenario(&sc, &opts).unwrap_or_else(|e| panic!("{e}"));
    Entry {
        id: sc.id.clone(),
        file: sc.file.display().to_string(),
        source: sc.source.clone(),
        trace: c.run.trace,
        report: Some(c.report),
        images: c.run.images,
        notes: c.run.notes,
        ..Entry::default()
    }
}

fn info(out: &std::path::Path, frames: Frames) -> RunInfo {
    RunInfo {
        command: "check".into(),
        out: out.to_path_buf(),
        frames,
        replay: None,
    }
}

/// The page names nothing outside itself, has a title, light and dark
/// tokens and a narrow layout, and its embedded trace and checks read
/// back as they were written.
#[test]
fn the_page_is_self_contained_and_its_data_round_trips() {
    let out = scratch("page");
    let mut entries = vec![checked(ROW)];
    report::write_run(&info(&out, Frames::All), &mut entries, &[]).unwrap();
    let page = std::fs::read_to_string(out.join("index.html")).unwrap();
    for scheme in ["http://", "https://", "//cdn", "src=\"http", "url(http"] {
        assert!(!page.contains(scheme), "the page names `{scheme}`");
    }
    assert!(page.contains("<title>strand-eval report-row</title>"));
    assert!(page.contains("prefers-color-scheme: dark"));
    assert!(page.contains(":root[data-theme=\"dark\"]"));
    assert!(page.contains("@media (max-width: 760px)"));
    // The node tree keeps its indentation (spaces, not collapsed), and
    // the ten-column cost table scrolls in its own box at phone width.
    assert!(page.contains(".tree div { padding: 1px 4px; cursor: pointer; white-space: pre;"));
    assert!(page.contains(".scrollx { overflow-x: auto;"));
    assert!(page.contains("el('div', { cls: 'scrollx' }, t)"));
    assert!(page.len() < 16 * 1024 * 1024);
    let data = html::embedded(&page).expect("the page embeds its data");
    let sc = &data["runs"][0]["scenarios"][0];
    let trace: RunTrace = serde_json::from_value(sc["trace"].clone()).unwrap();
    assert_eq!(trace, entries[0].trace, "the trace round-trips");
    let checks: checks::Report = serde_json::from_value(sc["checks"].clone()).unwrap();
    assert_eq!(Some(&checks), entries[0].report.as_ref());
    let images = sc["images"].as_object().unwrap();
    assert_eq!(images.len(), entries[0].images.len(), "every frame kept");
    assert!(
        images
            .values()
            .all(|v| v.as_str().unwrap().starts_with("data:image/png;base64,"))
    );
    assert_eq!(sc["thinned"], false);
    // A `</script>` in the data cannot end the data's element.
    let mut e = checked(ROW);
    e.notes
        .push("a note with </script><script>alert(1)</script> in it".into());
    let p = html::page(&html::Page::run(
        &info(&out, Frames::None),
        std::slice::from_ref(&e),
    ))
    .unwrap();
    let back = html::embedded(&p).unwrap();
    assert_eq!(
        back["runs"][0]["scenarios"][0]["notes"][0],
        "a note with </script><script>alert(1)</script> in it"
    );
}

/// An image of noise (it does not compress).
fn noise(seed: u32, w: u32, h: u32) -> Image {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let rgba: Vec<u8> = (0..w * h * 4)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x & 0xff) as u8
        })
        .collect();
    Image {
        width: w,
        height: h,
        png: report::png::encode(w, h, &rgba).unwrap(),
    }
}

/// Over its budget the page keeps the frames around a failing finding
/// and thins the rest; its trace stays whole.
#[test]
fn the_size_guard_thins_images_and_keeps_the_failing_frames() {
    let mut e = checked(ROW);
    assert!(!e.report.as_ref().unwrap().ok(), "the row's expect fails");
    // Every frame gets its own large image.
    let mut images = BTreeMap::new();
    for (n, f) in e.trace.frames.iter_mut().enumerate() {
        let name = format!("frames/noise{n}.png");
        images.insert(name.clone(), noise(n as u32, 128, 128));
        for v in f.png.values_mut() {
            *v = name.clone();
        }
        if f.png.is_empty() {
            f.png.insert(1, name.clone());
        }
    }
    e.images = images;
    let total: usize = e.images.values().map(|i| i.png.len()).sum();
    let entries = [e];
    let out = scratch("thin");
    let mut page = html::Page::run(&info(&out, Frames::All), &entries);
    page.budget = 600 * 1024 + total / 3;
    let text = html::page(&page).unwrap();
    assert!(
        text.len() <= page.budget,
        "{} > {}",
        text.len(),
        page.budget
    );
    let data = html::embedded(&text).unwrap();
    let sc = &data["runs"][0]["scenarios"][0];
    assert_eq!(sc["thinned"], true);
    let kept = sc["images"].as_object().unwrap();
    assert!(!kept.is_empty() && kept.len() < entries[0].images.len());
    // The frame the expect failed at is among those kept.
    let f = entries[0].failing()[0].1.clone();
    let k = f.k.unwrap();
    let frame = entries[0].trace.frames.iter().rfind(|x| x.k <= k).unwrap();
    let name = frame.png.values().next().unwrap();
    assert!(kept.contains_key(name), "{name} at k{k} was thinned");
    let trace: RunTrace = serde_json::from_value(sc["trace"].clone()).unwrap();
    assert_eq!(trace.frames.len(), entries[0].trace.frames.len());
}

/// A failing finding gets a contact sheet, a line in summary.md naming
/// it, an entry in summary.json, and an `explain` table with its frame
/// marked.
#[test]
fn a_failing_finding_is_explained_headless() {
    let out = scratch("headless");
    let mut entries = vec![checked(ROW)];
    report::write_run(&info(&out, Frames::Failing), &mut entries, &[]).unwrap();
    let dir = out.join("report-row");
    let sheet = dir.join("fail-expect-1.png");
    let (w, h, _) = report::png::decode(&std::fs::read(&sheet).unwrap()).unwrap();
    assert!(w > 100 && h > 40, "{w}×{h}");
    assert!(dir.join("scenario.scn").is_file());
    assert!(std::fs::read_dir(dir.join("frames")).unwrap().count() > 0);
    let md = std::fs::read_to_string(out.join("summary.md")).unwrap();
    assert!(md.contains("**report-row** FAIL"), "{md}");
    assert!(
        md.contains("expect [on] box#right (shell.strand:") && md.contains("fail-expect-1.png"),
        "{md}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("summary.json")).unwrap()).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["scenarios"][0]["findings"][0]["label"], "box#right");
    // explain reads the report back.
    let back = report::read_entry(&dir).unwrap();
    assert_eq!(back.trace, entries[0].trace);
    let findings: Vec<_> = back
        .report
        .iter()
        .flat_map(|r| r.results.iter())
        .flat_map(|r| r.findings.clone())
        .collect();
    let text = report::explain::explain(
        &back.trace,
        &findings,
        &report::explain::Ask {
            check: Some("expect".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(text.contains("box#right"), "{text}");
    assert!(text.lines().any(|l| l.starts_with('*')), "{text}");
    assert!(
        text.contains("GLIDE"),
        "the room it makes is a glide:\n{text}"
    );
    // The CLI says the same.
    strand_eval::main_with(&[
        "explain".into(),
        dir.display().to_string(),
        "--node".into(),
        "#left".into(),
    ])
    .unwrap();
}

/// A bad verdict is a known-bad recording: a run that reproduces it fails
/// `reference`; a verdict needs a note and is good or bad.
#[test]
fn a_bad_verdict_becomes_a_recording_the_check_reads() {
    let out = scratch("verdict");
    let ok = ROW.replace("expect still #right x\n", "expect still #left x\n");
    let mut entries = vec![checked(&ok)];
    report::write_run(&info(&out, Frames::None), &mut entries, &[]).unwrap();
    let refs = out.join("refs");
    strand_eval::main_with(&[
        "verdict".into(),
        out.join("report-row").display().to_string(),
        "bad".into(),
        "--note".into(),
        "the right box should not slide".into(),
        "--refs".into(),
        refs.display().to_string(),
        "--approver".into(),
        "tester".into(),
    ])
    .unwrap();
    let file = strand_eval::refs::RefFile::load(&refs.join("report-row.ref"))
        .unwrap()
        .unwrap();
    let bad = file.bad("report-row");
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].note, "the right box should not slide");
    assert!(file.good("report-row").is_none());
    let sc = scenario(&ok);
    let r = strand_eval::refs::for_scenario(&sc, &refs, false);
    let rep = checks::evaluate(&sc, &entries[0].trace, None, None, Some(&r));
    assert_eq!(rep.status(CheckId::Reference), Some(&Status::Fail));
    assert!(
        rep.failures(CheckId::Reference)[0]
            .msg
            .contains("reproduces a known-bad recording"),
        "{:?}",
        rep.failures(CheckId::Reference)
    );
    for args in [vec!["meh"], vec!["bad"], vec!["bad", "--note", "  "]] {
        let mut v = vec![
            "verdict".to_string(),
            out.join("report-row").display().to_string(),
        ];
        v.extend(args.iter().map(|s| s.to_string()));
        let e = strand_eval::main_with(&v).unwrap_err();
        assert!(matches!(e, strand_eval::Error::Usage(_)), "{e}");
    }
}

/// A/B: two runs of one scenario where a node is drawn 3 px lower from
/// one tick on: the comparison names that node, tick and axis, and the
/// A/B page embeds both runs.
#[test]
fn ab_names_the_first_frame_two_runs_differ_at() {
    let a = checked(ROW);
    let mut b = checked(ROW);
    let key = b
        .trace
        .nodes
        .iter()
        .find(|(_, m)| m.id.as_deref() == Some("left"))
        .map(|(k, _)| k.clone())
        .unwrap();
    let from = b.trace.frames[b.trace.frames.len() / 2].k;
    for f in b.trace.frames.iter_mut().filter(|f| f.k >= from) {
        for r in f.n.iter_mut().filter(|r| r.key == key) {
            r.visual[1] += 3.0;
        }
    }
    b.trace.seal();
    let same = strand_eval::ab::compare(std::slice::from_ref(&a), std::slice::from_ref(&a));
    assert_eq!(same["scenarios"]["report-row"]["same"], true);
    let cmp = strand_eval::ab::compare(std::slice::from_ref(&a), std::slice::from_ref(&b));
    let d = &cmp["scenarios"]["report-row"];
    assert_eq!(d["same"], false);
    let first = d["diverge"].as_array().unwrap();
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["label"], "box#left");
    assert_eq!(first[0]["what"], "y");
    assert_eq!(first[0]["k"], from);
    assert_eq!(first[0]["uncertain"], false);
    // Through the CLI, from two reports on disk.
    let out = scratch("ab");
    let (da, db) = (out.join("a"), out.join("b"));
    report::write_run(&info(&da, Frames::All), &mut [a], &[]).unwrap();
    report::write_run(&info(&db, Frames::All), &mut [b], &[]).unwrap();
    strand_eval::main_with(&[
        "ab".into(),
        da.display().to_string(),
        db.display().to_string(),
        "-o".into(),
        out.join("page").display().to_string(),
    ])
    .unwrap();
    let page = std::fs::read_to_string(out.join("page/index.html")).unwrap();
    let data = html::embedded(&page).unwrap();
    assert_eq!(data["runs"].as_array().unwrap().len(), 2);
    assert_eq!(
        data["ab"]["scenarios"]["report-row"]["diverge"][0]["what"],
        "y"
    );
    assert!(!page.contains("https://") && !page.contains("http://"));
}

/// `promote` writes only what may be committed: a local scenario's run
/// (the owner's config and fonts) is refused, a committed one's is
/// promoted.
#[test]
fn promote_refuses_a_local_scenarios_run() {
    use strand_eval::promote::{Ask, promote};
    let dir = scratch("promote-local");
    let text = "id = \"mine\"\n=== shell.strand\nbar Top {\n  height: 30\n  box { id: b; width: 20; height: 10; bg: #ff0000 }\n}\n=== steps\nrest\n";
    let ask = |from: PathBuf| Ask {
        from,
        name: "promoted".into(),
        xfail: Vec::new(),
        note: None,
        to: Some(dir.join("regressions")),
        force: true,
    };
    for (place, ok) in [("scenarios/local", false), ("scenarios/curated", true)] {
        let src = dir.join(place);
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("mine.scn");
        std::fs::write(&file, text).unwrap();
        let out = dir.join(format!("out-{}", place.replace('/', "-")));
        let args: Vec<String> = vec![
            "check".into(),
            file.display().to_string(),
            "--out".into(),
            out.display().to_string(),
        ];
        strand_eval::main_with(&args).unwrap_or_else(|e| panic!("{e}"));
        let r = promote(&ask(out.join("mine")));
        match (ok, r) {
            (true, Ok(p)) => assert!(p.is_file()),
            (false, Err(e)) => {
                let e = e.to_string();
                assert!(e.contains("a local scenario"), "{e}");
            }
            (_, r) => panic!("{place}: {r:?}"),
        }
    }
}

/// strand-eval writes nothing outside `--out`: under CI its step summary
/// is the scripts' to append (scripts/eval/ci.sh, matrix.sh), so a test
/// or a mutant run does not add to it.
#[test]
fn a_run_leaves_the_ci_step_summary_alone() {
    let dir = scratch("step-summary");
    let summary = dir.join("step-summary.md");
    let out = dir.join("out");
    let scn = eval_root().join("scenarios/regressions/bug25-hidden-text.scn");
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"))
        .arg("check")
        .arg(&scn)
        .arg("--out")
        .arg(&out)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .status()
        .unwrap();
    assert!(st.success());
    assert!(out.join("summary.md").is_file());
    assert!(!summary.exists(), "strand-eval wrote the step summary");
}

/// (replayfix) A replay's report is packed (docs/eval.md, "A replay's
/// report, packed"): its trace's frames each against the one before and
/// its frames patches of the one before where little changed; read back,
/// the trace is the same trace and every frame's pixels the same pixels,
/// and the page holds the packed trace and every stored file.
#[test]
fn a_replays_report_is_packed_and_reads_back_whole() {
    use strand_eval::report::{pack, png};
    // A fade and a glide: frames that each differ a little from the
    // one before.
    let text = ROW.replace("id = \"report-row\"", "id = \"report-packed\"");
    let e = checked(&text);
    assert!(e.trace.frames.len() > 5, "{} frames", e.trace.frames.len());
    let (trace, images) = (e.trace.clone(), e.images.clone());
    let out = scratch("packed");
    let mut info = info(&out, Frames::All);
    info.replay = Some(serde_json::json!({"verdict": "VERIFIED", "line": "VERIFIED: fixture"}));
    let mut entries = vec![e];
    report::write_run(&info, &mut entries, &[]).unwrap();
    let dir = out.join("report-packed");

    // The trace: packed, smaller, the same trace read back.
    let packed = std::fs::read_to_string(dir.join("trace.json")).unwrap();
    assert!(packed.contains("\"packed\":"), "{}", &packed[..200]);
    assert!(
        packed.len() * 2 < trace.to_json().len(),
        "packed {} of {}",
        packed.len(),
        trace.to_json().len()
    );
    assert_eq!(RunTrace::from_json(&packed).unwrap(), trace);
    assert_eq!(pack::read(&packed).unwrap().0, trace);
    assert_eq!(
        RunTrace::from_json(&trace.to_json()).unwrap(),
        trace,
        "an unpacked trace reads as before"
    );

    // The frames: some patches, fewer bytes, every pixel back.
    let patches: Vec<_> = std::fs::read_dir(dir.join("frames"))
        .unwrap()
        .flatten()
        .filter(|f| f.file_name().to_string_lossy().ends_with(".patch.png"))
        .collect();
    assert!(!patches.is_empty(), "no frame was stored as a patch");
    let stored: u64 = std::fs::read_dir(dir.join("frames"))
        .unwrap()
        .flatten()
        .map(|f| f.metadata().unwrap().len())
        .sum();
    let whole: usize = images.values().map(|im| im.png.len()).sum();
    assert!((stored as usize) < whole, "stored {stored} of {whole}");
    let back = report::read_entry(&dir).unwrap();
    assert_eq!(back.trace, trace);
    assert_eq!(back.images.len(), images.len());
    for (name, im) in &images {
        let got = back
            .images
            .get(name)
            .unwrap_or_else(|| panic!("{name} not read back"));
        assert_eq!(
            png::decode(&got.png).unwrap(),
            png::decode(&im.png).unwrap(),
            "{name}'s pixels"
        );
    }

    // An agent's whole frames: `explain --png`.
    let pngs = out.join("whole");
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"))
        .arg("explain")
        .arg(&dir)
        .arg("--png")
        .arg(&pngs)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for f in &trace.frames {
        for (sid, name) in &f.png {
            let got = std::fs::read(pngs.join(format!("k{}-s{sid}.png", f.k))).unwrap();
            assert_eq!(
                png::decode(&got).unwrap(),
                png::decode(&images[name].png).unwrap(),
                "k{} s{sid}",
                f.k
            );
        }
    }

    // The page: the packed trace, the frame fields the viewer draws,
    // the patches, every stored file.
    let page = std::fs::read_to_string(out.join("index.html")).unwrap();
    let data = html::embedded(&page).unwrap();
    let sc = &data["runs"][0]["scenarios"][0];
    assert!(sc["trace"]["packed"].is_object());
    let (t, _) = pack::unpack_json(&serde_json::to_string(&sc["trace"]).unwrap()).unwrap();
    let whole: serde_json::Value = serde_json::from_str(&trace.to_json()).unwrap();
    let frames = t["frames"].as_array().unwrap();
    assert_eq!(frames.len(), trace.frames.len());
    for (got, want) in frames.iter().zip(whole["frames"].as_array().unwrap()) {
        let got = got.as_object().unwrap();
        for f in pack::VIEW_FIELDS {
            assert_eq!(got.get(f), want.get(f), "k{} {f}", want["k"]);
        }
        assert!(got.keys().all(|k| pack::VIEW_FIELDS.contains(&k.as_str())));
    }
    for k in ["nodes", "surfaces", "events", "segments", "cols"] {
        assert_eq!(t[k], whole[k], "{k}");
    }
    let p = pack::read(&packed).unwrap().1;
    assert_eq!(sc["patches"].as_object().unwrap().len(), p.len());
    assert_eq!(sc["thinned"], false);
    let shown = sc["images"].as_object().unwrap();
    for name in images.keys() {
        let empty = p.get(name).is_some_and(|q| q.boxes.is_empty());
        assert!(empty || shown.contains_key(name), "{name} not on the page");
    }
    // Smaller than the same page unpacked.
    let plain = checked(&text);
    let unpacked = html::page(&html::Page::run(&info, std::slice::from_ref(&plain))).unwrap();
    assert!(
        page.len() < unpacked.len(),
        "packed {} of {}",
        page.len(),
        unpacked.len()
    );
}

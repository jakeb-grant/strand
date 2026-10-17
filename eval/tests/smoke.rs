//! Scenarios run end to end through the driver (docs/eval.md, "Driver"):
//! a config written in the scenario boots, steps act on virtual time and
//! the trace holds every painted frame's nodes by stable keys.

use strand_eval::harness::{RunOptions, run};
use strand_eval::scenario::{Scenario, eval_root};
use strand_eval::trace::{Frame, Row, RunTrace};
use strand_render::probe::{Axes, Motions};

fn scenario(text: &str) -> Scenario {
    let file = eval_root().join("scenarios/curated/smoke.scn");
    Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"))
}

fn go(text: &str) -> RunTrace {
    let sc = scenario(text);
    let out = run(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(out.notes.is_empty(), "{:?}", out.notes);
    out.trace
}

/// The key of the node with `id:` `id`.
fn key(t: &RunTrace, id: &str) -> String {
    t.nodes
        .iter()
        .find(|(_, m)| m.id.as_deref() == Some(id))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| panic!("no #{id} in {:?}", t.nodes.keys()))
}

fn row<'a>(f: &'a Frame, key: &str) -> Option<&'a Row> {
    f.n.iter().find(|r| r.key == key)
}

/// crates/strand-render/tests/morph.rs's FOCUS_OFF (owner-testing.md bug
/// 25), with `id:`s.
const FOCUS_OFF: &str = r##"id = "focus-off"
screen = { width = 300, scale = 1.0 }

=== shell.strand
state dnd = true
bar Top {
  height: 30
  split { width: 300; height: 30; pad: 0, 14
    end { align: center
      row { gap: 6; align: center; shrink: 0
        box { id: bars; width: 15; height: 12; bg: #00ff00 }
        if dnd { box { id: focus; width: 14; height: 14; bg: #ff00ff } }
        if !dnd {
          row { id: count; gap: 3; align: center; shrink: 0; enter { opacity: 0; x: -6 }
            text "iiii" { width: 14; shrink: 0; color: #ff00ff }
          }
        }
        box { id: last; width: 20; height: 10; bg: #0000ff }
      }
    }
  }
}

=== steps
rest
mark focus-off
set dnd false
rest max 120
expect still #bars y
expect visible #count
expect hidden #focus
"##;

#[test]
fn a_scenario_runs_end_to_end() {
    let t = go(FOCUS_OFF);
    assert_eq!(t.schema, 1);
    assert_eq!(t.scenario.id, "focus-off");
    assert_eq!(t.surfaces.len(), 1, "{:?}", t.surfaces);
    assert_eq!(t.surfaces[0].kind, "bar");
    assert_eq!(t.surfaces[0].label, "bar Top");
    // Boot, then the marked segment.
    assert_eq!(t.segments.len(), 2, "{:#?}", t.segments);
    assert_eq!(t.segments[0].name, "boot");
    assert!(t.segments[0].settled);
    let seg = &t.segments[1];
    assert_eq!(seg.name, "focus-off");
    assert!(seg.settled, "{seg:?}");
    let s = seg.s.expect("a frame before the change");
    assert!(seg.e > s + 3, "{seg:?}");
    assert_eq!(t.rests.len(), 2);
    assert!(t.rests.iter().all(|r| r.settled), "{:?}", t.rests);
    // Keys are `id|origin|instance#sibling`.
    let bars = key(&t, "bars");
    assert_eq!(bars, "bars|shell.strand:7:9|[Mock | DP-1]#0");
    assert_eq!(t.nodes[&bars].label, "box#bars");
    assert_eq!(t.nodes[&bars].origin.as_deref(), Some("shell.strand:7:9"));
    let count = key(&t, "count");
    let pose = t.nodes[&count].enter.expect("its enter pose");
    assert_eq!((pose.opacity, pose.x), (Some(0.0), Some(-6.0)));
    // The count enters authored on x and opacity; the bars never author.
    let entering: Vec<&Row> = t.frames.iter().filter_map(|f| row(f, &count)).collect();
    assert!(entering.len() > 3);
    assert!(
        Motions(entering[0].mot).contains(Motions::ENTER),
        "{:?}",
        entering[0]
    );
    assert!(Axes(entering[0].auth).contains(Axes::X | Axes::OPACITY));
    for f in &t.frames {
        if let Some(b) = row(f, &bars) {
            assert_eq!(b.auth, 0, "frame {}: {b:?}", f.k);
        }
    }
    // The text under the count draws shaped once shown; its effective
    // opacity follows the row's.
    let last = t.frames.last().unwrap();
    let text = t
        .nodes
        .iter()
        .find(|(_, m)| m.kind == "text")
        .map(|(k, _)| k.clone())
        .unwrap();
    let tr = row(last, &text).expect("the text at rest");
    assert_eq!(tr.text.as_deref(), Some("S"));
    assert_eq!(tr.op, 1.0);
    // Expectations are recorded with their keys and segment.
    assert_eq!(t.expects.len(), 3);
    assert_eq!(t.expects[0].keys, std::slice::from_ref(&bars));
    assert_eq!(t.expects[0].segment, Some(1));
    // The digest is sealed and the trace round-trips through JSON.
    assert!(t.digest.starts_with("blake3:"));
    let back = RunTrace::from_json(&t.to_json()).unwrap();
    assert_eq!(back, t);
    assert_eq!(back.compute_digest(), t.digest);
}

#[test]
fn a_run_is_a_function_of_its_scenario() {
    let a = go(FOCUS_OFF);
    let b = go(FOCUS_OFF);
    assert_eq!(a.digest, b.digest);
    assert_eq!(a.frames.len(), b.frames.len());
}

/// A hover popup: its delay and grace run on virtual time
/// (crates/strand-render/tests/input.rs's hover islands, reduced).
const HOVER: &str = r##"id = "hover"
screen = { width = 300 }

=== shell.strand
bar Top {
  height: 30
  row { width: 300; height: 30
    box { id: item; width: 40; height: 30; bg: #303030
      popup { id: pop; trigger: hover; delay: 200ms; width: 120; height: 40
        box { id: card; width: 120; height: 40; bg: #ff0000 }
      }
    }
  }
}

=== steps
rest
pointer 10,10
wait 150
mark opened
wait 100
rest
pointer 200,10
rest
"##;

#[test]
fn a_hover_popup_opens_after_its_delay_on_virtual_time() {
    let sc = scenario(HOVER);
    let out = run(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    let t = out.trace;
    assert!(out.notes.is_empty(), "{:?}", out.notes);
    let pointer_k = t
        .events
        .iter()
        .find(|e| e.text.starts_with("pointer 10"))
        .unwrap()
        .k;
    let popup = t
        .surfaces
        .iter()
        .find(|s| s.kind == "popup")
        .unwrap_or_else(|| panic!("{:?}", t.surfaces));
    // 200 ms at 60 Hz is 12 ticks: never before them.
    let ticks = popup.attached - pointer_k;
    assert!(
        (12..=14).contains(&ticks),
        "opened {ticks} ticks after the pointer"
    );
    let gone = popup.detached.expect("it closes after its grace");
    assert!(gone > popup.attached);
    assert!(t.rests.iter().all(|r| r.settled), "{:?}", t.rests);
}

/// A text held back is drawn by its estimate until its release.
#[test]
fn a_late_text_is_drawn_by_its_estimate_for_its_ticks() {
    let t = go(r##"id = "late"
=== shell.strand
state show = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 4; align: center
    if show { text "iiii" { id: t; color: #ff00ff } }
    box { id: b; width: 20; height: 10; bg: #0000ff }
  }
}
=== steps
rest
text late 3 where "iiii"
set show true
rest
"##);
    let tk = key(&t, "t");
    let drawn: Vec<(u32, String)> = t
        .frames
        .iter()
        .filter_map(|f| Some((f.k, row(f, &tk)?.text.clone()?)))
        .collect();
    let estimate: Vec<u32> = drawn
        .iter()
        .filter(|(_, d)| d == "E")
        .map(|(k, _)| *k)
        .collect();
    assert_eq!(estimate.len(), 3, "{drawn:?}");
    assert_eq!(drawn.last().unwrap().1, "S", "{drawn:?}");
    let held: Vec<usize> = t.frames.iter().map(|f| f.held_text.len()).collect();
    assert_eq!(held.iter().filter(|h| **h > 0).count(), 3, "{held:?}");
}

#[test]
fn a_config_that_does_not_compile_names_its_line() {
    let sc = scenario(
        "id = \"bad\"\n=== shell.strand\nbar Top {\n  height: 30\n  wobble: 3\n}\n=== steps\nrest\n",
    );
    let e = run(&sc, &RunOptions::default()).unwrap_err().to_string();
    assert!(
        e.contains("shell.strand:3 (scenario line 5): unknown prop"),
        "{e}"
    );
}

#[test]
fn a_selector_matching_nothing_is_an_error_at_its_line() {
    let text = FOCUS_OFF.replace("expect visible #count", "expect visible #nothing");
    let line = text.lines().position(|l| l.contains("#nothing")).unwrap() + 1;
    let e = run(&scenario(&text), &RunOptions::default())
        .unwrap_err()
        .to_string();
    let want = format!("smoke.scn:{line}: `#nothing` matches no node mounted now or seen before");
    assert!(e.ends_with(&want), "{e}");
}

/// On main (bugs 24 and 25 fixed) the focus-off scenario passes every
/// check it runs, its twin included.
#[test]
fn the_focus_off_scenario_passes_its_checks() {
    let sc = scenario(FOCUS_OFF);
    let c = strand_eval::check_scenario(&sc, &RunOptions::default()).unwrap();
    let lines = c.report.lines(&c.run.trace);
    assert!(c.report.ok(), "{}", lines.join("\n"));
    assert!(c.twin.is_some());
    use strand_eval::checks::{CheckId, Status};
    for check in [
        CheckId::Bystander,
        CheckId::Envelope,
        CheckId::Teleport,
        CheckId::Vanish,
        CheckId::Twin,
        CheckId::Expect,
        CheckId::Settle,
        CheckId::Determinism,
    ] {
        assert_eq!(
            c.report.status(check),
            Some(&Status::Pass),
            "{check:?}\n{}",
            lines.join("\n")
        );
    }
}

/// `strand-eval check --gate` runs a committed regression and writes its
/// report; a gate over nothing, or over a scenario that fails, fails.
#[test]
fn the_check_command_gates_and_writes_its_report() {
    let out = std::env::temp_dir().join(format!("strand-eval-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let scn = eval_root().join("scenarios/regressions/bug25-hidden-text.scn");
    let args = |extra: &[&str]| -> Vec<String> {
        let mut v = vec!["check".to_string(), "--gate".into()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v.extend(["--out".into(), out.display().to_string()]);
        v
    };
    strand_eval::main_with(&args(&[scn.to_str().unwrap()])).unwrap_or_else(|e| panic!("{e}"));
    let summary = std::fs::read_to_string(out.join("summary.txt")).unwrap();
    assert!(
        summary.contains("bug25-hidden-text bystander: pass"),
        "{summary}"
    );
    assert!(summary.ends_with("1 scenarios: all pass\n"), "{summary}");
    let trace = std::fs::read_to_string(out.join("bug25-hidden-text/trace.json")).unwrap();
    assert!(RunTrace::from_json(&trace).is_ok());
    assert!(out.join("bug25-hidden-text/checks.json").is_file());
    assert!(out.join("summary.md").is_file());
    // Nothing to gate is an error, not a pass.
    let empty = out.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let e = strand_eval::main_with(&args(&[empty.to_str().unwrap()])).unwrap_err();
    assert!(matches!(e, strand_eval::Error::Usage(_)), "{e}");
    // A failing scenario fails the command.
    let bad = out.join("bad.scn");
    let text = FOCUS_OFF.replace("expect hidden #focus", "expect visible #focus");
    std::fs::write(&bad, text.replace("id = \"focus-off\"", "id = \"bad\"")).unwrap();
    let e = strand_eval::main_with(&args(&[bad.to_str().unwrap()])).unwrap_err();
    assert!(matches!(e, strand_eval::Error::Failed(_)), "{e}");
    let _ = std::fs::remove_dir_all(&out);
}

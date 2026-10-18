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

/// (eval-coverage) `intend #count enters` written before the step that
/// mounts `#count` names the node once it shows: the segment's intend
/// holds its key, and the run goes on.
#[test]
fn an_intend_names_a_node_its_segment_mounts_later() {
    let text = FOCUS_OFF.replace("set dnd false\n", "intend #count enters\nset dnd false\n");
    assert_ne!(text, FOCUS_OFF);
    let t = go(&text);
    let count = key(&t, "count");
    let seg = t.segments.iter().find(|s| s.name == "focus-off").unwrap();
    let rec = seg.intends.iter().find(|r| r.what == "enters").unwrap();
    assert_eq!(rec.keys, vec![count], "{:#?}", seg.intends);
}

/// (eval-coverage) An intend that matches no node by the end of its
/// segment is an error at its line.
#[test]
fn an_intend_that_never_matches_is_an_error_at_its_line() {
    let text = FOCUS_OFF.replace("set dnd false\n", "intend #nothing enters\nset dnd false\n");
    let line = text.lines().position(|l| l.contains("#nothing")).unwrap() + 1;
    let e = run(&scenario(&text), &RunOptions::default())
        .unwrap_err()
        .to_string();
    let want = format!("smoke.scn:{line}: `#nothing` matched no node before its segment ended");
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

/// The ticks at which `t` painted a surface, and the most paints any
/// surface had in one tick.
fn paints(t: &RunTrace) -> u32 {
    t.frames
        .iter()
        .flat_map(|f| f.cost.values())
        .map(|c| c.paints)
        .max()
        .unwrap_or(0)
}

/// A tooltip's delay runs on the virtual clock (the renderer's timers
/// read it through `Renderer::set_now`): 250 ms at 60 Hz is 15 ticks,
/// however fast the run goes in wall time, and the scene is not at rest
/// while the tooltip waits.
#[test]
fn a_tooltip_shows_after_its_delay_on_virtual_time() {
    let text = |wait: u32| {
        format!(
            r##"id = "tooltip"
screen = {{ width = 300 }}
tooltip_delay = 250

=== shell.strand
bar Top {{
  height: 30
  row {{ width: 300; height: 30
    box {{ id: tip; width: 40; height: 30; bg: #303030; tooltip: "hello" }}
  }}
}}

=== steps
rest
pointer 10,10
wait {wait}
"##
        )
    };
    let t = go(&text(300));
    let pointer_k = t
        .events
        .iter()
        .find(|e| e.text.starts_with("pointer"))
        .unwrap()
        .k;
    let tip = t
        .surfaces
        .iter()
        .find(|s| s.kind == "popup")
        .unwrap_or_else(|| panic!("no tooltip: {:?}", t.surfaces));
    let ticks = tip.attached - pointer_k;
    assert!(
        (15..=16).contains(&ticks),
        "shown {ticks} ticks after the pointer"
    );
    // Before its delay it does not show.
    let t = go(&text(200));
    assert!(
        t.surfaces.iter().all(|s| s.kind != "popup"),
        "{:?}",
        t.surfaces
    );
    // Waiting for it is not rest.
    let t = go(&text(10).replace("wait 10", "rest"));
    assert!(
        t.surfaces.iter().any(|s| s.kind == "popup"),
        "{:?}",
        t.surfaces
    );
}

/// A container query (`when self.width < 60`): what a layout pass
/// before the frame reads is answered before it (the renderer holds an
/// idle surface's frame for logic), what the frame itself lays out (a
/// late text shaped at the paint) is answered after it and shows in the
/// next frame; every surface paints once a tick.
#[test]
fn layout_facts_reach_logic_between_frames() {
    let t = go(r##"id = "query"
screen = { width = 300 }

=== shell.strand
state wide = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 4; align: center
    row { id: probe; shrink: 0; align: center
      opacity: 1
      when self.width < 60 { opacity: 0.5 }
      text wide ? "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii" : "ii" { id: label; shrink: 0; color: #ffffff }
    }
    box { id: after; width: 20; height: 10; bg: #0000ff }
  }
}

=== steps
rest
mark widen
text late 1 where "iiiiiiii" via paint
set wide true
rest
"##);
    assert_eq!(paints(&t), 1, "a surface painted twice in a tick");
    let probe = key(&t, "probe");
    let seen: Vec<(u32, f32, f32)> = t
        .frames
        .iter()
        .filter_map(|f| {
            let r = row(f, &probe)?;
            Some((f.k, r.laid[2], r.own_op))
        })
        .collect();
    // At boot the narrow row is answered before its first frame.
    assert_eq!(seen[0].2, 0.5, "{seen:?}");
    // The frame that first lays it out wide still has the old answer;
    // the next has logic's.
    let wide = seen
        .iter()
        .position(|(_, w, _)| *w >= 60.0)
        .unwrap_or_else(|| panic!("{seen:?}"));
    assert_eq!(seen[wide].2, 0.5, "{seen:?}");
    // (Its opacity springs to logic's 1 from there.)
    assert!(seen[wide + 1].2 > 0.5, "{seen:?}");
    assert_eq!(seen[wide + 1].0, seen[wide].0 + 1, "{seen:?}");
}

/// `system.reduced_motion` is what logic and render read, as in the
/// shell: setting it reaches render through logic's diff, and a glide
/// it would have played snaps.
#[test]
fn system_reduced_motion_reaches_render_through_logic() {
    let text = |reduce: &str| {
        format!(
            r##"id = "reduce"
screen = {{ width = 300 }}

=== shell.strand
state on = false
bar Top {{
  height: 30
  row {{ width: 300; height: 30; gap: 6; align: center
    if on {{ box {{ id: a; width: 40; height: 10; bg: #00ff00 }} }}
    box {{ id: b; width: 20; height: 10; bg: #0000ff }}
    text system.reduced_motion ? "still" : "moving" {{ id: says; color: #ffffff }}
  }}
}}

=== steps
rest
{reduce}
rest
mark grow
set on true
rest
"##
        )
    };
    let glides = |t: &RunTrace| {
        let b = key(t, "b");
        t.frames
            .iter()
            .filter_map(|f| row(f, &b))
            .any(|r| Motions(r.mot).contains(Motions::GLIDE))
    };
    let moving = go(&text("log full motion"));
    assert!(glides(&moving), "b glides with full motion");
    for step in ["set system.reduced_motion true", "reduce-motion on"] {
        let t = go(&text(step));
        assert!(!glides(&t), "{step}: b snaps");
        let grow = t.segments.iter().find(|s| s.name == "grow").unwrap();
        assert!(grow.reduced_motion, "{step}: {grow:?}");
        // Logic reads it too.
        let says = key(&t, "says");
        let last = t.frames.last().unwrap();
        let w = row(last, &says).unwrap().laid[2];
        let before = row(&moving.frames[0], &key(&moving, "says")).unwrap().laid[2];
        assert!(
            w < before,
            "{step}: logic still says moving ({w} vs {before})"
        );
    }
}

/// A parent's authored size is not its children's: a card whose width
/// springs is laid out each frame, and its children's drawn boxes are
/// held to their layouts. Its offset, opacity and transform are theirs.
#[test]
fn only_an_ancestors_offset_fade_and_transform_author_its_children() {
    use strand_eval::harness::inherited;
    assert_eq!(inherited(Axes::W | Axes::H), Axes::NONE);
    assert_eq!(inherited(Axes::X | Axes::W), Axes::X);
    assert_eq!(inherited(Axes::OPACITY), Axes::OPACITY);
    assert_eq!(inherited(Axes::SCALE), Axes::GEOMETRY);
    assert_eq!(inherited(Axes::ROTATE), Axes::GEOMETRY);
    // Under a card whose width springs, the dot at its end moves with
    // it in layout, never authored, and every check passes.
    let sc = scenario(
        r##"id = "card"
screen = { width = 300 }

=== shell.strand
state wide = false
bar Top {
  height: 30
  row { width: 300; height: 30; align: center
    row { id: card; width: wide ? 200 : 80 ~ spring(700, 0.7); height: 24; bg: #303030; align: center; justify: end
      box { id: dot; width: 10; height: 10; bg: #ff00ff }
    }
  }
}

=== steps
rest
mark grow
set wide true
rest
"##,
    );
    let c =
        strand_eval::check_scenario(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    let lines = c.report.lines(&c.run.trace);
    assert!(c.report.ok(), "{}", lines.join("\n"));
    let t = &c.run.trace;
    let dot = key(t, "dot");
    let laid: Vec<f32> = t
        .frames
        .iter()
        .filter_map(|f| row(f, &dot))
        .map(|r| r.laid[0])
        .collect();
    assert!(
        laid.iter().any(|x| (x - laid[0]).abs() > 20.0),
        "the dot moves with the card: {laid:?}"
    );
    for f in &t.frames {
        if let Some(r) = row(f, &dot) {
            assert_eq!(r.auth, 0, "frame {}: {r:?}", f.k);
        }
    }
}

/// A row written at `x: 20` carries its children by 20 at rest, none of
/// it in flight; while it glides (a box mounted before it), its glide
/// is what of it is in flight.
#[test]
fn the_inherited_offset_says_what_of_it_is_in_flight() {
    let t = go(r##"id = "flight"
screen = { width = 300 }

=== shell.strand
state more = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 4; align: center
    if more { box { width: 30; height: 10; bg: #ff00ff } }
    row { id: carrier; x: 20; gap: 2; align: center
      box { id: inner; width: 10; height: 12; bg: #00ff00 }
    }
  }
}

=== steps
rest
set more true
rest
"##);
    let inner = key(&t, "inner");
    let rows: Vec<&Row> = t.frames.iter().filter_map(|f| row(f, &inner)).collect();
    assert_eq!(rows[0].inherited, [20.0, 0.0], "{:?}", rows[0]);
    assert_eq!(rows[0].flight, [0.0, 0.0], "{:?}", rows[0]);
    let gliding = rows
        .iter()
        .find(|r| r.flight[0] != 0.0)
        .unwrap_or_else(|| panic!("never in flight: {rows:?}"));
    assert!(gliding.flight[0] < -1.0, "{gliding:?}");
    assert!(
        (gliding.inherited[0] - (20.0 + gliding.flight[0])).abs() < 0.05,
        "{gliding:?}"
    );
    let last = rows.last().unwrap();
    assert_eq!(last.flight, [0.0, 0.0], "{last:?}");
}

/// A row of a trace from before the in-flight columns, from before the
/// text's `fit` and `ink`, from before its paint, from before its
/// layout's overhang, or from before its clip (an A/B against an earlier
/// revision), still reads, with nothing in flight, no ink, no paint, no
/// overhang and no clip; any other width is refused.
#[test]
fn a_row_without_the_flight_columns_still_reads() {
    let t = go(FOCUS_OFF);
    let mut r = t.frames[0].n[0].clone();
    assert!(r.paint.is_some(), "{r:?}");
    r.ov = Some([0.5, 2.0]);
    r.clip = Some([1.0, 2.0, 3.0, 4.0]);
    let mut v = serde_json::to_value(&r).unwrap();
    let a = v.as_array_mut().unwrap();
    assert_eq!(a.len(), strand_eval::trace::COLS.len());
    let back: Row = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.clip, Some([1.0, 2.0, 3.0, 4.0]));
    let n = strand_eval::trace::COLS.len();
    v.as_array_mut().unwrap().truncate(n - 1);
    let back: Row = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.clip, None);
    assert_eq!(back.ov, Some([0.5, 2.0]));
    v.as_array_mut().unwrap().truncate(n - 2);
    let back: Row = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.ov, None);
    assert_eq!(back.paint, r.paint);
    v.as_array_mut().unwrap().truncate(n - 3);
    let back: Row = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.paint, None);
    assert_eq!((back.fit, back.ink), (r.fit, r.ink));
    v.as_array_mut().unwrap().truncate(n - 5);
    let back: Row = serde_json::from_value(v.clone()).unwrap();
    assert_eq!((back.fit, back.ink), (None, None));
    assert_eq!(back.flight, r.flight);
    v.as_array_mut().unwrap().truncate(n - 7);
    let back: Row = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.flight, [0.0, 0.0]);
    assert_eq!(back.inherited, r.inherited);
    v.as_array_mut().unwrap().pop();
    let e = serde_json::from_value::<Row>(v).unwrap_err().to_string();
    assert!(e.contains(&format!("has 33 columns, not {n}")), "{e}");
}

/// A `scale` step changes the screen's scale mid-run: each frame records
/// the scale it was drawn at (the report's crops and the CPU/GPU pixel
/// masks read it), and an edge's give is one physical pixel at the
/// coarsest of them.
#[test]
fn each_frame_records_the_scale_it_was_drawn_at() {
    let text = r##"id = "rescale"
screen = { width = 300, scale = 1.0 }

=== shell.strand
bar Top {
  height: 30
  row { width: 300; height: 30; align: center
    box { id: b; width: 20; height: 10; bg: #ff0000 }
  }
}

=== steps
rest
scale 2
rest
"##;
    let sc = scenario(text);
    let out = run(
        &sc,
        &RunOptions {
            images: true,
            ..RunOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let t = &out.trace;
    let scales: Vec<f64> = t.frames.iter().map(|f| f.scale).collect();
    assert_eq!(scales.first(), Some(&1.0), "{scales:?}");
    assert_eq!(scales.last(), Some(&2.0), "{scales:?}");
    assert_eq!(t.min_scale(), 1.0);
    // The last frame's image is drawn at 2x: 600 px wide.
    let last = t.frames.iter().rev().find(|f| !f.png.is_empty()).unwrap();
    let name = last.png.values().next().unwrap();
    let png = &out.images[name].png;
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    assert_eq!(w, 600);
    assert_eq!(t.scale_at(last), 2.0);
    let c = strand_eval::checks::Cx::new(&sc, t);
    assert_eq!(c.tol, 1.0, "one physical pixel at scale 1");
}

/// (eval-coverage-2) A list's rows: what a wheel step and a swipe do to
/// them as the trace says. A wheel step springs the offset (`SCROLL`, no
/// `FLING`); a fast swipe lifts into a fling (`SCROLL` and `FLING`); a
/// row the list's viewport hides while it is still on its surface is
/// `clipped`, its own children traced beside it.
const LIST: &str = r##"id = "list-hooks"
screen = { width = 300, height = 300, scale = 1.0 }

=== shell.strand
let rows = ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10", "a11", "a12"]
panel Picker {
  anchor: top_left; open: true
  col { width: 200; height: 220; pad: 10; bg: #1e1e2e
    list { id: picks; height: 100; gap: 4
      for r in rows key r {
        row { height: 30; bg: #313244
          box { width: 20; height: 10; bg: #ffffff }
        }
      }
    }
  }
}

=== steps
rest
pointer 100,50
mark wheel
scroll 1
rest
mark fling
swipe 60 over 80ms
rest
"##;

#[test]
fn a_wheel_springs_a_swipe_flings_and_a_viewport_clips() {
    let t = go(LIST);
    let seg = |name: &str| {
        t.segments
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no segment {name}"))
    };
    let in_seg = |name: &str| {
        let s = seg(name);
        t.frames
            .iter()
            .filter(move |f| f.k > s.s.unwrap_or(0) && f.k <= s.e)
            .flat_map(|f| f.n.iter())
    };
    let mot = |r: &Row| Motions(r.mot | r.emot);
    assert!(in_seg("wheel").any(|r| mot(r).contains(Motions::SCROLL)));
    assert!(!in_seg("wheel").any(|r| mot(r).contains(Motions::FLING)));
    assert!(in_seg("fling").any(|r| mot(r).contains(Motions::SCROLL | Motions::FLING)));
    // At rest after the fling every row is on the panel's surface; those
    // past the list's 100 px are clipped, and their boxes are traced too.
    let last = t.frames.last().unwrap_or_else(|| panic!("no frames"));
    let clipped: Vec<&Row> = last
        .n
        .iter()
        .filter(|r| r.hidden.as_deref() == Some("clipped"))
        .collect();
    assert!(!clipped.is_empty(), "{:?}", last.n);
    let shown = last.n.iter().filter(|r| r.hidden.is_none()).count();
    assert!(shown > 0);
    assert!(
        clipped.iter().any(|r| last.n.iter().any(|c| t
            .nodes
            .get(&c.key)
            .and_then(|m| m.parent.as_deref())
            == Some(r.key.as_str()))),
        "a clipped row's children are traced: {:?}",
        clipped.iter().map(|r| &r.key).collect::<Vec<_>>()
    );
}

/// (eval-rules-2) What `continuity`, `contain`'s held arm and
/// `coherence`'s drag read: each spring's samples (a change starting, a
/// rest), the clip a collapsing row sets, an interrupted roll, the
/// pointer and a slider's drag.
const MOTION_TRACE: &str = r##"id = "motion-trace"
screen = { width = 400, scale = 1.0 }

=== shell.strand
state right = false
state n = "41"
state shown = true
state vol = 0.3
bar Top {
  height: 40
  row { height: 40; gap: 6; align: center; pad: 0, 8
    slider { id: knob; width: 100; value: <-> vol }
    box { id: pill; width: 30; height: 12; bg: #3a7bd5; x: right ? 120 : 0 }
    text n { id: num; roll: true ~ 300ms; color: #ffffff }
    if shown {
      row { id: toast; height: 30; align: center; exit { height: 0 }
        text "Mail" { id: mail; color: #ffffff }
      }
    }
  }
}

=== steps
rest
mark go
set right true
set n "42"
frames 2
set n "43"
set right false
set shown false
rest
mark drag
drag 40,20 -> 80,20 over 100
rest
"##;

#[test]
fn the_trace_holds_springs_clips_rolls_and_drags() {
    let t = go(MOTION_TRACE);
    let pill = key(&t, "pill");
    let samples: Vec<_> = t
        .frames
        .iter()
        .flat_map(|f| f.sp.iter())
        .filter(|r| r.key == pill && r.of == "x")
        .collect();
    assert!(samples.len() > 4, "{samples:?}");
    // Two changes, the first from rest, the second in flight.
    let starts: Vec<_> = samples.iter().filter(|r| r.start.is_some()).collect();
    assert_eq!(starts.len(), 2, "{samples:?}");
    assert!(
        starts[0].rest.is_some() && starts[1].rest.is_none(),
        "{starts:?}"
    );
    assert_eq!(starts[1].to, vec![0.0]);
    // A 60 Hz output's refresh.
    let period = t.frames.iter().find_map(|f| f.period.get(&1).copied());
    assert!(
        period.is_some_and(|p| (16_600_000..16_700_000).contains(&p)),
        "{period:?}"
    );
    // The row collapsing clips its text.
    let mail = key(&t, "mail");
    assert!(
        t.frames
            .iter()
            .any(|f| row(f, &mail).is_some_and(|r| r.clip.is_some())),
        "no clip on the collapsing row's text"
    );
    // The roll the second change interrupted.
    let num = key(&t, "num");
    assert!(
        t.frames
            .iter()
            .flat_map(|f| f.rolls.iter())
            .any(|r| r.key == num && r.was.is_some()),
        "no interrupted roll"
    );
    // The drag: the pointer and the slider it holds.
    let knob = key(&t, "knob");
    let dragged: Vec<_> = t
        .frames
        .iter()
        .filter(|f| f.drags.contains_key(&knob))
        .collect();
    assert!(!dragged.is_empty(), "no drag traced");
    assert!(dragged.iter().all(|f| f.ptr.is_some_and(|p| p.s == 1)));
    // And the checks that read them pass the renderer's own motion.
    let sc = scenario(MOTION_TRACE);
    let r = strand_eval::checks::evaluate(&sc, &t, None, None, None);
    for c in [
        strand_eval::checks::CheckId::Continuity,
        strand_eval::checks::CheckId::Contain,
        strand_eval::checks::CheckId::Coherence,
    ] {
        assert_eq!(
            r.status(c),
            Some(&strand_eval::checks::Status::Pass),
            "{}",
            r.lines(&t).join("\n")
        );
    }
}

/// (eval-false-alarms) The sandbox places surfaces as the compositor
/// does, from strand-surface's placement: a bar at its edge reserves its
/// zone, a centred content-sized panel is centred in what the bar leaves
/// and centred again when it grows, a corner panel keeps its margins
/// from the zone and the edge, and a popup wider than the room beside
/// its anchor is shifted onto the screen (placement.rs `popup_shift`).
const PLACED: &str = r##"id = "placed"
screen = { width = 300, height = 400, scale = 1.0 }

=== shell.strand
state wide = false
state open = false
bar Top {
  height: 30
  row { height: 30
    box { id: anchor; width: 20; height: 30
      popup { id: pop; open: open
        box { id: menu; width: 120; height: 40 }
      }
    }
  }
}
panel Middle {
  anchor: center; open: true
  box { id: mid; width: wide ? 140 : 100; height: 40 }
}
panel Corner {
  anchor: top_right; margin: 8, 8, 0, 0; open: true
  box { id: corner; width: 60; height: 20 }
}

=== steps
rest
mark widen
set wide true
rest
mark open
set open true
rest
"##;

#[test]
fn surfaces_are_placed_as_the_compositor_places_them() {
    let t = go(PLACED);
    let id = |label: &str| {
        t.surfaces
            .iter()
            .find(|s| s.label == label)
            .map(|s| s.id)
            .unwrap_or_else(|| panic!("no {label} in {:?}", t.surfaces))
    };
    let at = |seg: &str, s: u32| {
        let e = t.segments.iter().find(|g| g.name == seg).unwrap().e;
        let f = t.frames.iter().rev().find(|f| f.k <= e).unwrap();
        f.origins.get(&s).copied()
    };
    let (bar, mid, corner) = (id("bar Top"), id("panel Middle"), id("panel Corner"));
    assert_eq!(at("boot", bar), Some([0.0, 0.0]));
    // Centred in the 300 × 370 px left below the bar's zone.
    assert_eq!(at("boot", mid), Some([100.0, 195.0]));
    assert_eq!(at("widen", mid), Some([80.0, 195.0]));
    // 8 px from the right edge and from the bar's zone.
    assert_eq!(at("boot", corner), Some([232.0, 38.0]));
    // Centred under its 20 px anchor it would start at -50; shifted on.
    let pop = t
        .surfaces
        .iter()
        .find(|s| s.kind == "popup")
        .map(|s| s.id)
        .unwrap_or_else(|| panic!("no popup in {:?}", t.surfaces));
    assert_eq!(at("open", pop), Some([0.0, 36.0]));
}

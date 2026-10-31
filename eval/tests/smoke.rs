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

/// (eval-rules-5) A header's `reduce_motion = true` is where motion
/// starts: a `reduce-motion off` step turns it off, and what changes
/// after it moves again (held-out set G's `g-reduced-motion-panel`). A
/// reduced re-run (the twin, `RunOptions::reduced_motion`) stays reduced
/// whatever its steps say.
/// (integration-r7) Where a parent places a child on x, as declared: a
/// row by `justify:` (start when unset), a column by `align:` (stretched
/// when unset: unknown), a box by `align:` (a sized child unaligned at
/// the start); a child with a margin is unknown.
#[test]
fn a_childs_x_share_is_its_parents_declared_alignment() {
    let text = r##"id = "x-share"
screen = { width = 300 }

=== shell.strand
bar Top {
  height: 120
  row { width: 300; height: 120; gap: 4
    row { width: 60; height: 20; justify: center
      text "a" { id: rc }
    }
    row { width: 60; height: 20
      text "b" { id: rs }
    }
    col { width: 60; height: 40; align: end
      text "c" { id: ce }
    }
    col { width: 60; height: 40
      text "d" { id: cs }
      text "e" { id: cm; margin: 2 }
    }
    box { width: 60; height: 20
      box { id: bx; width: 10; height: 10; bg: #ff0000 }
    }
  }
}

=== steps
rest
"##;
    let t = go(text);
    let share = |id: &str| t.nodes.get(&key(&t, id)).and_then(|m| m.x_share);
    assert_eq!(share("rc"), Some(0.5));
    assert_eq!(share("rs"), Some(0.0));
    assert_eq!(share("ce"), Some(1.0));
    assert_eq!(share("cs"), None, "stretched");
    assert_eq!(share("cm"), None, "a margin");
    assert_eq!(share("bx"), Some(0.0));
}

#[test]
fn a_reduce_motion_off_step_turns_off_the_headers_reduced_motion() {
    let text = r##"id = "reduce-off"
screen = { width = 300 }
reduce_motion = true

=== shell.strand
state on = false
bar Top {
  height: 30
  row { width: 300; height: 30; gap: 6; align: center
    if on { box { id: a; width: 40; height: 10; bg: #00ff00 } }
    box { id: b; width: 20; height: 10; bg: #0000ff }
  }
}

=== steps
rest
mark reduced
set on true
rest
mark back-on
reduce-motion off
rest
mark moving
set on false
rest
"##;
    let glides_in = |t: &RunTrace, seg: &str| {
        let b = key(t, "b");
        let s = t.segments.iter().find(|s| s.name == seg).unwrap();
        t.frames
            .iter()
            .filter(|f| s.s.is_some_and(|k| f.k > k) && f.k <= s.e)
            .filter_map(|f| row(f, &b))
            .any(|r| Motions(r.mot).contains(Motions::GLIDE))
    };
    let t = go(text);
    assert!(!glides_in(&t, "reduced"), "b snaps while motion is reduced");
    assert!(glides_in(&t, "moving"), "b glides once motion is back on");
    let seg = |t: &RunTrace, name: &str| t.segments.iter().find(|s| s.name == name).cloned();
    assert!(seg(&t, "reduced").unwrap().reduced_motion);
    assert!(!seg(&t, "moving").unwrap().reduced_motion);
    assert!(t.scenario.reduced_motion, "reduced from the start");
    assert!(!t.reduced_throughout());
    let sc = scenario(text);
    let twin = run(
        &sc,
        &RunOptions {
            reduced_motion: true,
            ..RunOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .trace;
    assert!(!glides_in(&twin, "moving"), "the twin stays reduced");
    assert!(twin.reduced_throughout());
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

const THEME_SWAP: &str = r##"id = "palette-trace"
screen = { width = 300, scale = 1.0 }

=== shell.strand
state dark = true
let scheme: Palette = dark ? import("catppuccin:mocha") : import("catppuccin:latte")
tokens base {
  motion { spatial: spring(3000, 0.75); effects: spring(500, 1); bouncy: spring(380, 0.75) }
}
use tokens base, palette scheme
bar Top {
  height: 30
  bg: $surface
  box { id: pill; width: 40; height: 14; bg: $accent }
}

=== steps
rest
mark swap
set dark false
rest
"##;

/// (eval-rules-3) A theme swap's palette roots are in the trace: each
/// root's samples keyed `$` and its path, `palette`, along the curve the
/// swap's table gives a colour (the shell's own `$motion.effects`), which
/// each records as asked, and followed to rest.
#[test]
fn the_trace_holds_a_theme_swaps_palette_roots() {
    use strand_eval::trace::CurveRec;
    let t = go(THEME_SWAP);
    let accent: Vec<_> = t
        .frames
        .iter()
        .flat_map(|f| f.sp.iter())
        .filter(|r| r.key == "$accent" && r.of == "palette")
        .collect();
    assert!(accent.len() > 4, "{accent:?}");
    let own = CurveRec::Spring { k: 500.0, z: 1.0 };
    assert!(
        accent
            .iter()
            .all(|r| r.c == own && r.ask == Some(own) && r.v.len() == 4),
        "{accent:?}"
    );
    assert_eq!(accent.iter().filter(|r| r.start.is_some()).count(), 1);
    let r = strand_eval::checks::evaluate(&scenario(THEME_SWAP), &t, None, None, None);
    for c in [
        strand_eval::checks::CheckId::Continuity,
        strand_eval::checks::CheckId::Colour,
    ] {
        assert_eq!(
            r.status(c),
            Some(&strand_eval::checks::Status::Pass),
            "{c:?}"
        );
    }
}

const DECLARED: &str = r#"id = "declared"
screen = { width = 400, height = 300, scale = 1.5 }
=== shell.strand
bar Top {
  height: 30
  split { pad: 0, 8
    start { text "A window title long enough to be cut" { id: title; width: 120; ellipsis: end } }
    center { box { id: mid; width: 40; height: 16; bg: #89b4fa } }
    end { text "12:00" }
  }
}
panel Card {
  layer: top; anchor: top_right; margin: 8, 12; keyboard: none
  open: true
  box { width: 120; height: 40; bg: #313244 }
}
=== steps
rest
"#;

/// (eval-rules-4) The trace holds what `placement` and `fills` read: each
/// surface's placement as declared (a bar's edge, a panel's anchor and
/// margin), a split's padding and its sections' margins, and a cut
/// text's stops at the frame's scale; and the checks pass it.
#[test]
fn the_trace_holds_declared_placements_insets_and_cut_lines() {
    let t = go(DECLARED);
    let bar = t.surfaces.iter().find(|s| s.kind == "bar").unwrap();
    let p = bar.places.first().unwrap();
    assert_eq!(p.edge.as_deref(), Some("top"));
    assert_eq!(p.size[1], Some(30.0));
    let card = t.surfaces.iter().find(|s| s.kind == "panel").unwrap();
    let p = card.places.first().unwrap();
    assert_eq!(p.anchor, "top_right");
    assert_eq!(p.margin, [8.0, 12.0, 8.0, 12.0]);
    assert!(p.margin_set);
    let split = t.nodes.values().find(|m| m.kind == "split").unwrap();
    assert_eq!(split.inset, Some([0.0, 8.0, 0.0, 8.0]));
    let title = key(&t, "title");
    let lines: Vec<_> = t
        .frames
        .iter()
        .flat_map(|f| f.lines.iter())
        .filter(|l| l.key == title)
        .collect();
    assert!(!lines.is_empty());
    let l = lines[0];
    assert_eq!(l.ellipsis.as_deref(), Some("end"));
    assert!(l.natural.len() > 10 && !l.stops.is_empty(), "{l:?}");
    let r = strand_eval::checks::evaluate(&scenario(DECLARED), &t, None, None, None);
    for c in [
        strand_eval::checks::CheckId::Placement,
        strand_eval::checks::CheckId::Ink,
    ] {
        assert_eq!(
            r.status(c),
            Some(&strand_eval::checks::Status::Pass),
            "{c:?}: {:?}",
            r.lines(&t)
        );
    }
}

/// (builtin-shadow) A step the shell refuses says the config's warnings:
/// set G's g-bar-module-swap wrote `state module = network` beside
/// `enum Module { network, battery }`, where the bare name reads the
/// builtin service (spec.md 4.2), and its `set module battery` was
/// refused far away with "`battery` is not a network" alone.
#[test]
fn a_refused_step_says_the_configs_warnings() {
    let sc = scenario(
        r##"id = "module-swap"
screen = { width = 200, scale = 1.0 }

=== bar.strand
enum Module { network, battery }
state module = network
bar Top { height: 20; text module == network ? "wifi" : "bat" }

=== steps
rest
set module battery
rest
"##,
    );
    let e = match run(&sc, &RunOptions::default()) {
        Ok(_) => panic!("`set module battery` is refused"),
        Err(e) => e.to_string(),
    };
    assert!(e.contains("is not a network"), "{e}");
    assert!(
        e.contains("bar.strand:2 (scenario line 6): `network` here reads the builtin service `network`, not the variant `Module.network`"),
        "{e}"
    );
}

/// (builtin-name-diagnostics) A config that does not load says its
/// warnings after its errors: set J's workspace dots declared `state
/// focused`, which every element's own `focused` hides, and saw only the
/// type errors at its uses.
#[test]
fn a_config_that_does_not_load_says_its_warnings() {
    let sc = scenario(
        r##"id = "dots"
screen = { width = 200, scale = 1.0 }

=== bar.strand
state focused = 1
bar Top { height: 20
  row { for d in [1, 2] key d { box { size: 8; when d == focused { width: 24 } } } }
}

=== steps
rest
"##,
    );
    let e = match run(&sc, &RunOptions::default()) {
        Ok(_) => panic!("the config does not load"),
        Err(e) => e.to_string(),
    };
    assert!(e.contains("cannot compare"), "{e}");
    assert!(
        e.contains("the config's warnings:")
            && e.contains("bar.strand:1 (scenario line 5): this `state` is hidden by the node boolean `focused`"),
        "{e}"
    );
}

/// (integration-r7) Each decode drawn names the file it was made from:
/// an image swap under `transition:` draws the old file's decode under
/// the new one, and `placement`'s `fit` placed both by the node's new
/// `source` (eval-coverage-4's media-pill-art-morph-1_5 failed `fit` on
/// the old art's 3:2 decode, judged as the new 3:4 file's).
#[test]
fn an_image_swap_names_each_decodes_own_file() {
    const SWAP: &str = r#"
id = "image-swap-files"
screen = { width = 200, height = 120, scale = 1.0 }

=== shell.strand
state track = 0
panel Art {
  anchor: top_left; margin: 8; width: 120; height: 100
  image track == 0 ? "../../fixtures/images/art.png" : "../../fixtures/images/app.png" {
    id: art; width: 96; height: 80; fit: cover; transition: dissolve }
}

=== steps
rest
set track 1
rest
"#;
    let t = go(SWAP);
    let art = key(&t, "art");
    let both = t
        .frames
        .iter()
        .find(|f| f.imgs.iter().filter(|i| i.key == art).count() == 2)
        .unwrap_or_else(|| panic!("no frame draws the swap's two decodes"));
    let files: Vec<_> = both
        .imgs
        .iter()
        .filter(|i| i.key == art)
        .map(|i| i.file)
        .collect();
    // art.png is 60 x 40 and app.png 24 x 32: the old one under the new.
    assert_eq!(files, vec![Some([60, 40]), Some([24, 32])], "{both:?}");
    let r = strand_eval::checks::evaluate(&scenario(SWAP), &t, None, None, None);
    assert_eq!(
        r.status(strand_eval::checks::CheckId::Placement),
        Some(&strand_eval::checks::Status::Pass),
        "{:?}",
        r.lines(&t)
    );
}

/// (report) Whether the last frame draws the node with `id:` `id`.
fn drawn_last(t: &RunTrace, id: &str) -> bool {
    let Some((k, _)) = t.nodes.iter().find(|(_, m)| m.id.as_deref() == Some(id)) else {
        return false;
    };
    t.frames.last().is_some_and(|f| row(f, k).is_some())
}

/// (report) `[seed]` sets its cells and service fields as baselines: the
/// first frame shows them and no `on change` fires for them; a later
/// `set` of the same cell does fire.
#[test]
fn a_seed_fires_no_on_change() {
    let text = |extra: &str| {
        format!(
            r##"id = "seed"
screen = {{ width = 300, scale = 1.0 }}

[seed]
"shell.n" = "5"
"battery.percent" = "42"

=== shell.strand
state n = 0
state fired = 0
state service_fired = 0
on change n {{ fired = fired + 1 }}
on change battery.percent {{ service_fired = service_fired + 1 }}
bar Top {{
  height: 30
  row {{
    if n == 5 {{ box {{ id: five; width: 10; height: 10; bg: #ff0000 }} }}
    if battery.percent == 42 {{ box {{ id: pct; width: 10; height: 10; bg: #ffff00 }} }}
    if fired > 0 {{ box {{ id: nfired; width: 10; height: 10; bg: #00ff00 }} }}
    if service_fired > 0 {{ box {{ id: sfired; width: 10; height: 10; bg: #0000ff }} }}
  }}
}}

=== steps
rest
{extra}
"##
        )
    };
    let t = go(&text(""));
    let first = &t.frames[0];
    for id in ["five", "pct"] {
        let k = key(&t, id);
        assert!(row(first, &k).is_some(), "#{id} on the first frame");
    }
    assert!(!drawn_last(&t, "nfired"), "a seed fired `on change n`");
    assert!(
        !drawn_last(&t, "sfired"),
        "a seed fired `on change battery.percent`"
    );
    // A step that sets it afterwards is a change.
    let t = go(&text("set n 6\nset battery.percent 50\nrest"));
    assert!(drawn_last(&t, "nfired"));
    assert!(drawn_last(&t, "sfired"));
    assert!(!drawn_last(&t, "five"));
}

/// (replayfix) A seeded cell of a component inside a popup not open at
/// boot (the owner's dashboard calendar) waits for the popup's mount,
/// rather than stopping the run as a cell that does not exist.
#[test]
fn a_seed_waits_for_a_popup_to_mount_its_cell() {
    let t = go(r##"id = "seed-popup"
screen = { width = 300, scale = 1.0 }

[seed]
"Cal.dir" = "1"

=== shell.strand
export state show = false
component Cal() {
  state dir = 0
  box { width: 40; height: 20; bg: #202020
    if dir == 1 { box { id: one; width: 10; height: 10; bg: #ff0000 } }
  }
}
bar Top {
  height: 30
  box { width: 40; height: 30; bg: #303030
    popup { open: show; width: 60; height: 30
      Cal {}
    }
  }
}

=== steps
rest
set shell.show true
rest
"##);
    assert!(
        drawn_last(&t, "one"),
        "the popup's cell seeded as it mounted"
    );
}

/// (replayfix) The header's screen may be the live output (a replay's):
/// `screens.all` names it so, data per screen (`workspaces.slots`)
/// finds its workspaces, and a cell kept per screen is seeded under its
/// id. The owner's replay drew no workspace dot on the bar of `eDP-1`,
/// the sandbox's screen being `DP-1`.
#[test]
fn the_screen_can_be_the_live_output() {
    let text = |who: &str| {
        format!(
            r##"id = "screen-as"
screen = {{ width = 300, scale = 1.0{who} }}

[seed]
"Top[LG | X | LG X].n" = "5"
"workspaces.all" = "[{{\"active\":true,\"focused\":true,\"id\":11,\"name\":\"11\",\"occupied\":false,\"screen\":\"eDP-1\",\"urgent\":false,\"windows\":[]}}]"

=== shell.strand
bar Top {{
  state n = 0
  height: 30
  row {{
    if screen.name == "eDP-1" && screen.make == "LG" {{ box {{ id: named; width: 10; height: 10; bg: #ff0000 }} }}
    if n == 5 {{ box {{ id: kept; width: 10; height: 10; bg: #00ff00 }} }}
    for ws in workspaces.slots(screen) {{ box {{ id: dot; width: 10; height: 10; bg: #0000ff }} }}
  }}
}}

=== steps
rest
"##
        )
    };
    let t = go(&text(
        ", name = \"eDP-1\", id = \"LG | X | LG X\", make = \"LG\", model = \"X\", description = \"LG X\"",
    ));
    for id in ["named", "kept", "dot"] {
        assert!(drawn_last(&t, id), "#{id}");
    }
    // The mock's screen by default: none of it.
    let sc = scenario(&text(""));
    let out = run(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(sc.identity.id, "Mock | DP-1");
    for id in ["named", "kept", "dot"] {
        assert!(!drawn_last(&out.trace, id), "#{id}");
    }
    assert!(
        out.notes
            .iter()
            .any(|n| n.contains("`Top[LG | X | LG X].n`: no such cell")),
        "{:?}",
        out.notes
    );
}

/// (report) The keyboard steps type into the input of the surface with
/// the keyboard: a press on an `on_demand` panel gives it the keyboard
/// (the compositor's rule), `type` types placeholder `x`s at once or over
/// time, `paste` pastes them into the input that asked, and `unfocus`
/// takes the keyboard away until `focus` gives it back.
#[test]
fn typed_placeholders_reach_the_focused_input() {
    let text = |steps: &str| {
        format!(
            r##"id = "typed"
screen = {{ width = 300, scale = 1.0 }}

=== shell.strand
export state q = ""
bar Top {{ height: 30 }}
panel Ask {{
  anchor: center; keyboard: on_demand
  col {{ width: 200; pad: 8
    input {{ id: field; text: <-> q; focus: true }}
    if q == "xxx" {{ box {{ id: three; width: 10; height: 10; bg: #00ff00 }} }}
    if q == "xxxxx" {{ box {{ id: five; width: 10; height: 10; bg: #00ff00 }} }}
    if q == "xxxxxxxxx" {{ box {{ id: nine; width: 10; height: 10; bg: #00ff00 }} }}
  }}
}}

=== steps
rest
{steps}
"##
        )
    };
    let t = go(&text("click 20,16 on #field\ntype 3\nrest"));
    assert!(drawn_last(&t, "three"), "typed into the panel pressed");
    let t = go(&text(
        "click 20,16 on #field\ntype 3\ntype 2 over 100\nrest",
    ));
    assert!(drawn_last(&t, "five"), "typed over time");
    let t = go(&text(
        "click 20,16 on #field\ntype 3\ntype 2 over 100\nunfocus\nfocus #field\npaste 4\nrest",
    ));
    assert!(
        drawn_last(&t, "nine"),
        "pasted after the keyboard came back"
    );
    // No surface has the keyboard before the press: a typed error.
    let sc = scenario(&text("type 3\nrest"));
    let e = run(&sc, &RunOptions::default()).unwrap_err().to_string();
    assert!(e.contains("no surface has the keyboard"), "{e}");
}

/// (report) A `keyboard: exclusive` panel takes the keyboard as it maps,
/// with no `focus` step, and gives it back when it closes.
#[test]
fn an_exclusive_panel_takes_focus_as_it_maps() {
    let text = r##"id = "exclusive"
screen = { width = 300, scale = 1.0 }

=== shell.strand
export state open = false
export state q = ""
bar Top { height: 30 }
panel Ask {
  anchor: center; keyboard: exclusive; open: open
  col { width: 200; pad: 8
    input { text: <-> q; focus: true }
  }
}
bar Probe {
  edge: bottom; height: 30
  row { if q == "xx" { box { id: two; width: 10; height: 10; bg: #00ff00 } } }
}

=== steps
rest
set open true
rest
type 2
rest
set open false
rest
"##;
    let t = go(text);
    assert!(drawn_last(&t, "two"), "typed into the panel that mapped");
    // Closed, it gave the keyboard back to no surface.
    let sc = scenario(&format!("{text}type 1\n"));
    let e = run(&sc, &RunOptions::default()).unwrap_err().to_string();
    assert!(e.contains("no surface has the keyboard"), "{e}");
}

/// (report) `answer` queues the next call's answer ahead of the mock's:
/// the mock's apps search finds no app, the answer given finds one.
#[test]
fn an_answer_is_given_to_the_next_call() {
    let text = |answer: &str| {
        format!(
            r##"id = "answer"
screen = {{ width = 300, scale = 1.0 }}

=== shell.strand
export state query = ""
let hits = apps.search(query)
bar Top {{
  height: 30
  row {{
    if query != "" && !hits.pending && hits.len == 0 {{ box {{ id: none; width: 10; height: 10; bg: #ff0000 }} }}
    if query != "" && !hits.pending && hits.len > 0 {{ box {{ id: some; width: 10; height: 10; bg: #00ff00 }} }}
  }}
}}

=== steps
rest
{answer}
set query fi
rest
"##
        )
    };
    let t = go(&text(""));
    assert!(drawn_last(&t, "none") && !drawn_last(&t, "some"));
    let hit = r#"[{"app": {"id": "firefox", "name": "Firefox", "comment": null, "icon": "firefox", "categories": []}, "score": 1.0, "ranges": []}]"#;
    let t = go(&text(&format!("answer apps.search {hit}")));
    assert!(drawn_last(&t, "some") && !drawn_last(&t, "none"));
    // An answer the method's type does not hold is refused at its line.
    let sc = scenario(&text("answer apps.search {\"no\": 1}"));
    let e = run(&sc, &RunOptions::default()).unwrap_err().to_string();
    assert!(e.contains("is no answer of `apps.search`"), "{e}");
}

/// (flex review) The sandbox runs no command: a result action's call is
/// answered by `answer action <name> …` (a recording's answer), else
/// with an error saying so, so its `.error` shows rather than a call
/// pending for ever.
#[test]
fn a_result_actions_call_is_answered_by_its_answer() {
    let text = |answer: &str| {
        format!(
            r##"id = "action-answer"
screen = {{ width = 300, scale = 1.0 }}

=== shell.strand
permit exec "qalc"
action calc(q: text) -> text ["qalc", "-t", q]
export state query = ""
let r = calc(query)
bar Top {{
  height: 30
  row {{
    if query != "" && r.error != null {{ box {{ id: failed; width: 10; height: 10; bg: #ff0000 }} }}
    if query != "" && (r ?? "") == "4" {{ box {{ id: four; width: 10; height: 10; bg: #00ff00 }} }}
  }}
}}

=== steps
rest
{answer}
set query "2+2"
rest
"##
        )
    };
    let t = go(&text(""));
    assert!(drawn_last(&t, "failed") && !drawn_last(&t, "four"));
    let t = go(&text("answer action calc \"4\""));
    assert!(drawn_last(&t, "four") && !drawn_last(&t, "failed"));
    let t = go(&text("answer action calc error \"no qalc\""));
    assert!(drawn_last(&t, "failed"));
    // Only an action with a result is answered.
    let sc = scenario(&text("answer action nope \"4\""));
    let e = run(&sc, &RunOptions::default()).unwrap_err().to_string();
    assert!(e.contains("no action `nope` with a result"), "{e}");
}

/// (flex review) Render's pointer facts reach logic before an input's
/// flags, as the shell's main thread sends them: `on change
/// self.pressed` reads where the press went down (the grip of
/// crates/strand-compiler/tests/fixtures/flex/floating.strand), on a
/// press whose pointer arrived in the same tick.
#[test]
fn the_pointer_reaches_logic_before_the_press() {
    let text = r##"id = "grip"
screen = { width = 400, scale = 1.0 }

=== shell.strand
state gx = -1
state gy = -1
bar Top {
  height: 40
  col {
    box { id: grip; width: 200; height: 20; bg: #5b8def
      on change self.pressed {
        if self.pressed { gx = self.pointer?.x ?? -1; gy = self.pointer?.y ?? -1 }
      }
    }
    box { id: wx; width: gx + 100; height: 10; bg: #ff0000 }
    box { id: wy; width: gy + 100; height: 10; bg: #00ff00 }
  }
}

=== steps
rest
drag 60,12 -> 160,12 over 100ms on #grip
rest
"##;
    let t = go(text);
    let last = t.frames.last().unwrap();
    let w = |id: &str| row(last, &key(&t, id)).unwrap().laid[2] - 100.0;
    assert_eq!((w("wx"), w("wy")), (60.0, 12.0));
}

/// (report) `warmup` runs its ticks and paints them, but the trace (and
/// so every check) starts after it, from the state it left: boot ends at
/// the first traced frame.
#[test]
fn the_warmup_is_run_but_not_traced() {
    let text = |warmup: u32| {
        format!(
            r##"id = "warm"
screen = {{ width = 300, scale = 1.0 }}
warmup = {warmup}

=== shell.strand
export state wide = false
bar Top {{
  height: 30
  row {{ box {{ id: b; width: wide ? 100 : 20; height: 10; bg: #ff0000 }} }}
}}

=== steps
rest
set wide true
wait 1500
"##
        )
    };
    let full = go(&text(0));
    let warm = go(&text(1000));
    let t0 = 1_000_000_000u64;
    assert!(full.frames[0].t_ns < t0 + 1_000_000_000);
    assert!(warm.frames.iter().all(|f| f.t_ns >= t0 + 1_000_000_000));
    assert!(warm.frames.len() < full.frames.len());
    // It starts from the width the warm-up reached.
    let k = key(&warm, "b");
    let first = row(&warm.frames[0], &k).unwrap();
    let last = row(full.frames.last().unwrap(), &key(&full, "b")).unwrap();
    assert_eq!(first.visual, last.visual, "the warm-up's end is the run's");
    assert_eq!(warm.segments[0].name, "boot");
    assert_eq!(warm.segments[0].e, warm.frames[0].k);
    // And it checks clean from there.
    let sc = scenario(&text(1000));
    let c = strand_eval::check_scenario(&sc, &RunOptions::default()).unwrap();
    assert!(c.report.ok(), "{:?}", c.report.lines(&c.run.trace));
}

/// (action-error) `end action <name> failed "<why>"` is what `rec.error`
/// reads, as a recording's failure replays; the next call clears it.
#[test]
fn an_actions_error_is_what_its_end_says() {
    let text = |steps: &str| {
        format!(
            r##"id = "action-error"
screen = {{ width = 300, scale = 1.0 }}

=== shell.strand
permit exec "wf-recorder"
action rec() ["wf-recorder"]
export state go = 0
on change go {{ rec() }}
bar Top {{
  height: 30
  row {{
    if rec.error != null {{ box {{ id: failed; width: 10; height: 10; bg: #ff0000 }} }}
    if rec.error == null {{ box {{ id: fine; width: 10; height: 10; bg: #00ff00 }} }}
  }}
}}

=== steps
rest
set go 1
rest
{steps}
rest
"##
        )
    };
    let t = go(&text("end action rec"));
    assert!(drawn_last(&t, "fine") && !drawn_last(&t, "failed"));
    let t = go(&text("end action rec failed \"no such file\""));
    assert!(drawn_last(&t, "failed") && !drawn_last(&t, "fine"));
    let t = go(&text("end action rec failed \"no\"\nrest\nset go 2\nrest"));
    assert!(drawn_last(&t, "fine"), "a new call clears it");
}

/// (lockmon) The sandbox runs no command: a detached action's command
/// counts as running from its call until `end action <name>` (a
/// recording's end), so `rec.running` reads as the shell's did. (lockmon
/// review) As the shell's runner, it runs one command of an action at a
/// time with one re-run queued behind it (a call while one runs replaces
/// the queued one), and `rec.stop()` drops the queued re-run while the
/// stopped command runs until its end; the stop is in the trace's
/// commands.
#[test]
fn an_actions_run_lasts_until_its_end() {
    let text = |steps: &str| {
        format!(
            r##"id = "action-run"
screen = {{ width = 300, scale = 1.0 }}

=== shell.strand
permit exec "wf-recorder"
action rec() ["wf-recorder"]
export state go = 0
export state halt = 0
export state again = 0
on change go {{ rec() }}
on change halt {{ rec.stop() }}
on change again {{ rec.stop(); rec() }}
bar Top {{
  height: 30
  row {{
    if rec.running {{ box {{ id: on; width: 10; height: 10; bg: #ff0000 }} }}
    if !rec.running {{ box {{ id: off; width: 10; height: 10; bg: #00ff00 }} }}
  }}
}}

=== steps
rest
set go 1
rest
{steps}
rest
"##
        )
    };
    let t = go(&text(""));
    assert!(drawn_last(&t, "on") && !drawn_last(&t, "off"));
    let t = go(&text("end action rec"));
    assert!(drawn_last(&t, "off") && !drawn_last(&t, "on"));
    // Two calls: one end leaves the queued re-run running.
    let t = go(&text("set go 2\nrest\nend action rec"));
    assert!(drawn_last(&t, "on"));
    // Three calls are two commands, as the runner makes them: two ends
    // end them.
    let t = go(&text(
        "set go 2\nrest\nset go 3\nrest\nend action rec\nrest\nend action rec",
    ));
    assert!(drawn_last(&t, "off"));
    // A stop: the command runs until its end, and nothing after it.
    let t = go(&text("set halt 1"));
    assert!(drawn_last(&t, "on"));
    assert!(
        t.commands.iter().any(|c| c.text == "stop rec"),
        "{:?}",
        t.commands
    );
    let t = go(&text("set go 2\nrest\nset halt 1\nrest\nend action rec"));
    assert!(drawn_last(&t, "off"), "the queued re-run was dropped");
    // A stop then a call (`{ rec.stop(); rec(f) }`): running through the
    // stopped command's end, then the new one's.
    let t = go(&text("set again 1\nrest\nend action rec"));
    assert!(
        drawn_last(&t, "on"),
        "the new call runs after the stopped one"
    );
    let t = go(&text(
        "set again 1\nrest\nend action rec\nrest\nend action rec",
    ));
    assert!(drawn_last(&t, "off"));
    let sc = scenario(&text("end action nope"));
    let e = run(&sc, &RunOptions::default()).unwrap_err().to_string();
    assert!(e.contains("no action `nope`"), "{e}");
}

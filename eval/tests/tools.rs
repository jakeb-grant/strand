//! `strand-eval new` (a scenario skeleton, shots-keys steps mapped one
//! to one) and `strand-eval bench` (a scenario's cost table), docs/eval.md
//! "Scaffolding" and "Cost".

use std::collections::BTreeMap;
use std::path::PathBuf;

use strand_eval::bench::{self, Samples};
use strand_eval::scaffold::{self, Ask, KEPT};
use strand_eval::scenario::{Place, Scenario};
use strand_eval::steps;
use strand_eval::trace::{Cost, Frame, RunTrace};

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tools-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// --- new

#[test]
fn the_shots_keys_verbs_map_one_to_one() {
    let got = scaffold::map_steps(
        "# open the menu\n\
         ipc set ui.open true\n\
         pause 250\n\
         pause 1.5s\n\
         burst 30\n\
         pointer 120,15\n\
         pointer 200 15 over 300ms\n\
         pointer click 40,10 right\n\
         notify \"Hello\" \"a body\"\n",
    );
    assert_eq!(
        got,
        [
            "# open the menu",
            "set ui.open true",
            "wait 250",
            "wait 1500",
            "frames 30",
            "pointer 120,15",
            "glide 120,15 -> 200,15 over 300",
            "click 40,10 right",
            "notify \"Hello\" \"a body\"",
        ]
    );
    // Every mapped line is a step.
    let text = got.join("\n");
    assert!(steps::parse(&text, 1).is_ok(), "{text}");
}

#[test]
fn a_line_it_does_not_understand_is_kept_as_a_comment() {
    let got = scaffold::map_steps(
        "shot menu-open\n\
         pause soon\n\
         pointer nowhere\n\
         pointer 10,10 over 200\n\
         burst 12 8ms\n",
    );
    assert_eq!(got[0], format!("{KEPT} shot menu-open"));
    assert_eq!(got[1], format!("{KEPT} pause soon"));
    assert_eq!(got[2], format!("{KEPT} pointer nowhere"));
    // No place to glide from: the pointer is put there, and the line,
    // whose time the step drops, is kept above it.
    assert_eq!(got[3], format!("{KEPT} pointer 10,10 over 200"));
    assert_eq!(got[4], "pointer 10,10");
    assert_eq!(got[5], format!("{KEPT} burst 12 8ms"));
    assert_eq!(got[6], "frames 12");
    assert_eq!(got.len(), 7);
}

#[test]
fn new_writes_a_skeleton_that_parses_and_never_replaces_one() {
    let dir = scratch("new");
    let steps = dir.join("menu.steps");
    std::fs::write(&steps, "ipc set ui.open true\nburst 10\nwhatever this is\n").unwrap();
    let ask = Ask {
        id: "menu-open".into(),
        from_steps: Some(steps.clone()),
        to: Some(dir.clone()),
        ..Ask::default()
    };
    let path = scaffold::scaffold(&ask).unwrap();
    assert_eq!(path, dir.join("menu-open.scn"));
    let text = std::fs::read_to_string(&path).unwrap();
    let sc = Scenario::parse_in(&text, &path, Place::Local).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(sc.id, "menu-open");
    assert!(text.contains("=== shell.strand\n"), "{text}");
    assert!(
        text.ends_with(&format!(
            "=== steps\nrest\nset ui.open true\nframes 10\n{KEPT} whatever this is\nrest\n"
        )),
        "{text}"
    );
    // It is there now: a second scaffold refuses.
    assert!(scaffold::scaffold(&ask).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    // A config is named, not written in.
    let named = Ask {
        id: "named".into(),
        config: Some("${STRAND_EVAL_CONFIG}/shell.strand".into()),
        to: Some(dir.clone()),
        ..Ask::default()
    };
    let text = std::fs::read_to_string(scaffold::scaffold(&named).unwrap()).unwrap();
    assert!(text.contains("config = \"${STRAND_EVAL_CONFIG}/shell.strand\"\n"));
    assert!(!text.contains("=== shell.strand"));
    // Not an id.
    let bad = Ask {
        id: "../up".into(),
        to: Some(dir),
        ..Ask::default()
    };
    assert!(scaffold::scaffold(&bad).is_err());
}

// --- bench

#[test]
fn percentiles_are_nearest_rank() {
    let mut v: Vec<u64> = (1..=100).rev().collect();
    assert_eq!(bench::percentiles(&mut v), Some([50, 95, 100]));
    assert_eq!(bench::percentiles(&mut [7]), Some([7, 7, 7]));
    assert_eq!(bench::percentiles(&mut []), None);
}

fn costs(items: &[u32], raster_us: u64) -> RunTrace {
    let mut t = RunTrace::default();
    for (k, i) in items.iter().enumerate() {
        t.frames.push(Frame {
            k: k as u32,
            painted: vec![1],
            cost: BTreeMap::from([(
                1,
                Cost {
                    paints: 1,
                    items: *i,
                    damage_px: 100 * u64::from(*i),
                    raster_us,
                    ..Cost::default()
                },
            )]),
            ..Frame::default()
        });
    }
    // A frame that painted nothing is no sample.
    t.frames.push(Frame {
        k: items.len() as u32,
        ..Frame::default()
    });
    t
}

#[test]
fn bench_tables_each_scenarios_costs_and_marks_wall_times_advisory() {
    let mut s = Samples::default();
    bench::sample(&mut s, &costs(&[1, 2, 3, 40], 500));
    bench::sample(&mut s, &costs(&[1, 2, 3, 40], 900));
    assert_eq!((s.paints, s.runs), (8, 2));
    let table = bench::table(&BTreeMap::from([("menu".to_string(), s)]));
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[0], "| scenario | metric | p50 | p95 | max |");
    assert!(
        lines.contains(&"| menu (8 paints, 2 runs) | damage_px | 200 | 4000 | 4000 |"),
        "{table}"
    );
    assert!(lines.contains(&"|  | items | 2 | 40 | 40 |"), "{table}");
    assert!(
        lines.contains(&"|  | raster_us (advisory) | 500 | 900 | 900 |"),
        "{table}"
    );
    // Every metric, deterministic first.
    let at = |m: &str| lines.iter().position(|l| l.contains(m)).unwrap();
    assert!(at("text_held") < at("flatten_us"));
}

// --- new --explore (docs/eval.md, "Local configs and fonts")

/// A config of each thing a starter exercises, written for this test: a
/// bool and an enum state, a hover popup whose content has a click
/// handler only on screen once it opens, and a button outside it.
const EXPLORED: &str = r#"enum Look { calm, loud }
state on = false
state look = calm

bar Top {
  height: 30
  row { width: 300; height: 30; gap: 6; align: center
    box { id: tip; width: 40; height: 30; bg: #303030
      popup { id: pop; trigger: hover; delay: 100ms; width: 120; height: 40
        if pop.open {
          box { id: inner; width: 120; height: 40; bg: look == loud ? #ff0000 : #00ff00
            on click { on = !on }
          }
        }
      }
    }
    box { id: btn; width: on ? 60 : 30; height: 20; bg: #0000ff
      on click { look = look == calm ? loud : calm }
    }
  }
}
"#;

#[test]
fn an_explored_starter_exercises_what_the_config_declares() {
    let dir = scratch("explore");
    let config = dir.join("shell.strand");
    std::fs::write(&config, EXPLORED).unwrap();
    let ask = Ask {
        id: "explored".into(),
        config: Some(config.display().to_string()),
        to: Some(dir.clone()),
        explore: true,
        ..Ask::default()
    };
    let path = scaffold::scaffold(&ask).unwrap_or_else(|e| panic!("{e}"));
    let text = std::fs::read_to_string(&path).unwrap();
    let sc = Scenario::parse_in(&text, &path, Place::Local).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(sc.id, "explored");
    assert!(text.contains("fonts  = \"local\""), "{text}");
    // Each state's other values, then the one it booted at.
    let at = |s: &str| {
        text.find(s)
            .unwrap_or_else(|| panic!("no `{s}` in\n{text}"))
    };
    assert!(at("set on true\nrest\nset on false\nrest") > at("mark state-on"));
    assert!(at("set look loud\nrest\nset look calm\nrest") > at("mark state-look"));
    // The popup's anchor is hovered, and the click inside the popup it
    // opened is exercised from there, before the button outside it.
    let hover = at("# shell.strand:8:5");
    let inner = at("# shell.strand:11:11: on click");
    let btn = at("# shell.strand:17:5: on click");
    assert!(hover < inner && inner < btn, "{text}");
    assert!(text.contains("on @shell.strand:9:7"), "{text}");
    // It runs through, every rest arriving.
    assert!(!text.contains("# this starter stops"), "{text}");
    assert!(!text.contains("never came"), "{text}");
    assert!(!text.contains("never on screen"), "{text}");
    let run = strand_eval::harness::run(&sc, &Default::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(run.trace.rests.iter().all(|r| r.settled), "{text}");
    // Exploring needs a config, and does not map a steps file too.
    let bare = Ask {
        id: "bare".into(),
        to: Some(dir),
        explore: true,
        ..Ask::default()
    };
    assert!(scaffold::scaffold(&bare).is_err());
}

/// `fonts = "local"` shapes with the machine's fonts (fontconfig's): a
/// text asking for DejaVu Sans, which the image has (CI's apt list), is
/// laid out at DejaVu's width, not the bundled Liberation Sans standing in.
#[test]
fn local_fonts_are_the_machines() {
    let dir = scratch("fonts");
    let width = |fonts: &str| {
        let text = format!(
            "id = \"f\"\nfonts = \"{fonts}\"\n=== shell.strand\nbar Top {{\n  height: 30\n  \
             row {{ text \"Wide wandering words\" {{ id: t; font: \"DejaVu Sans\" 20px }} }}\n}}\n\
             === steps\nrest\n"
        );
        let path = dir.join(format!("{fonts}.scn"));
        let sc = Scenario::parse_in(&text, &path, Place::Local).unwrap_or_else(|e| panic!("{e}"));
        let run =
            strand_eval::harness::run(&sc, &Default::default()).unwrap_or_else(|e| panic!("{e}"));
        let last = run.trace.frames.last().unwrap();
        last.n
            .iter()
            .find(|r| r.key.starts_with("t|"))
            .map(|r| r.laid[2])
            .unwrap_or_else(|| panic!("no #t"))
    };
    let (local, bundled) = (width("local"), width("bundled"));
    assert!(
        (local - bundled).abs() > 4.0,
        "local {local} px, bundled {bundled} px"
    );
}

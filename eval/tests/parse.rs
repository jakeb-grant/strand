//! The scenario format (docs/eval.md, "Scenario format"): every header
//! key and verb parses, mistakes are typed errors at `file:line`, and a
//! committed scenario may not name anything outside `eval/`.

use std::path::{Path, PathBuf};

use strand_eval::Error;
use strand_eval::scenario::{
    BackendMode, CheckId, Config, Fonts, Place, Scenario, TextMode, eval_root, place_of,
};
use strand_eval::select::{PathSeg, Selector};
use strand_eval::steps::{Axis, Button, ExpectKind, Intention, Point, StepKind, Via};
use strand_render::probe::Axes;
use strand_scene::NodeKind;

fn file() -> PathBuf {
    eval_root().join("scenarios/curated/t.scn")
}

fn parse(text: &str) -> Scenario {
    Scenario::parse(text, &file()).unwrap_or_else(|e| panic!("{e}"))
}

/// The error's line and message.
fn fails(text: &str) -> (u32, String) {
    fails_at(text, &file())
}

fn fails_at(text: &str, at: &Path) -> (u32, String) {
    match Scenario::parse(text, at) {
        Err(Error::Scenario { line, msg, file }) => {
            assert_eq!(file, at.display().to_string());
            (line, msg)
        }
        Err(e) => panic!("not a scenario error: {e}"),
        Ok(_) => panic!("parsed: {text}"),
    }
}

const MIN: &str = "id = \"t\"\n=== shell.strand\nbar Top { height: 30 }\n=== steps\nrest\n";

#[test]
fn the_plan_example_parses() {
    let text = r##"id     = "bug25-hidden-text"
about  = """owner-testing.md bug 25: Focus off; the unread count re-enters with
enter { opacity: 0; x: -6 }, its text far from its estimate. Siblings stay on their level."""
screen = { width = 300, scale = 1.0 }    # one Screen in screens.all
hz     = 60
text   = "inline"
backend = "cpu"
fonts  = "bundled"
checks = ["settle","bystander","envelope","teleport","vanish","twin"]

=== shell.strand
state dnd = true
bar Top {
  height: 30
}

=== steps
rest                     # boot; paint until rest
mark focus-off           # opens a segment; labels the timeline
set dnd false
rest max 120
expect still #bars y     # inline assertion over the current segment
expect still #last y
"##;
    let s = parse(text);
    assert_eq!(s.id, "bug25-hidden-text");
    assert!(s.about.contains("bug 25"));
    assert_eq!((s.screen.width, s.screen.scale, s.hz), (300, 1.0, 60));
    assert_eq!(
        (s.text, s.backend, s.fonts),
        (TextMode::Inline, BackendMode::Cpu, Fonts::Bundled)
    );
    assert_eq!(s.place, Place::Committed);
    // `expect` always runs.
    assert_eq!(
        s.checks,
        [
            CheckId::Settle,
            CheckId::Bystander,
            CheckId::Envelope,
            CheckId::Teleport,
            CheckId::Vanish,
            CheckId::Twin,
            CheckId::Expect
        ]
    );
    let Config::Inline(sections) = &s.config else {
        panic!("{:?}", s.config)
    };
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].path, "shell.strand");
    assert_eq!(sections[0].line, 12, "its text starts after the `===` line");
    assert!(sections[0].text.starts_with("state dnd = true\n"));
    assert!(!sections[0].text.contains("==="));
    let lines: Vec<u32> = s.steps.iter().map(|s| s.line).collect();
    assert_eq!(lines, [18, 19, 20, 21, 22, 23]);
    assert_eq!(s.steps[0].kind, StepKind::Rest { max: 600 });
    assert_eq!(s.steps[0].text, "rest", "the comment is dropped");
    assert_eq!(s.steps[1].kind, StepKind::Mark("focus-off".into()));
    assert_eq!(
        s.steps[2].kind,
        StepKind::Set {
            path: "dnd".into(),
            value: "false".into()
        }
    );
    assert_eq!(s.steps[3].kind, StepKind::Rest { max: 120 });
    assert_eq!(
        s.steps[4].kind,
        StepKind::Expect {
            what: ExpectKind::Still,
            sel: Selector::Id("bars".into()),
            axis: Some(Axis::Y),
            from: 0
        }
    );
}

#[test]
fn defaults_fill_an_empty_header() {
    let s = parse(MIN);
    assert_eq!(
        (s.screen.width, s.screen.height, s.screen.scale),
        (300, 1080, 1.0)
    );
    assert_eq!(s.hz, 60);
    assert_eq!(s.checks, CheckId::DEFAULT);
    assert!(s.expect_fail.is_empty() && s.allow.is_empty() && s.services.is_empty());
    assert!(s.budget.is_empty());
    assert_eq!(s.tolerance.edge_px, 0.5);
    assert_eq!(s.tolerance.overshoot, 0.35);
    assert_eq!(s.tolerance.teleport_px, 32.0);
    assert!(!s.twin_pixels && !s.reduce_motion);
}

#[test]
fn every_header_key_parses() {
    let text = r##"id = "keys"
about = "every key"
screen = { width = 640, height = 400, scale = 1.5 }
hz = 144
text = "worker"
backend = "both"
checks = ["settle", "regions"]
expect_fail = { settle = "it never rests yet, issue 9" }
twin_pixels = true
tooltip_delay = 250
reduce_motion = true

[services]
"notifications.count" = "3"
"battery.percent" = 40
"audio.muted" = true

[allow]
envelope = [{ node = "#spinner", why = "a spinner turns" }]

[tolerance]
edge_px = 1.0
overshoot = 0.5
teleport_px = 48

[budget]
items = 200
damage_px = 20000
records = 30
layout_nodes = 12
layout_passes = 3

=== shell.strand
bar Top { height: 30 }
=== lib/more.strand
let x = 1
=== steps
rest
"##;
    let s = parse(text);
    assert_eq!(
        (s.screen.width, s.screen.height, s.screen.scale),
        (640, 400, 1.5)
    );
    assert_eq!(s.hz, 144);
    assert_eq!(s.text, TextMode::Worker);
    assert_eq!(s.backend, BackendMode::Both);
    assert_eq!(
        s.checks,
        [
            CheckId::Settle,
            CheckId::Regions,
            CheckId::Expect,
            CheckId::CpuGpu
        ],
        "both adds cpu-gpu"
    );
    assert_eq!(
        s.expect_fail[&CheckId::Settle],
        "it never rests yet, issue 9"
    );
    assert!(s.twin_pixels && s.reduce_motion);
    assert_eq!(s.tooltip_delay, Some(250));
    assert_eq!(
        s.services,
        [
            ("audio.muted".to_string(), "true".to_string()),
            ("battery.percent".into(), "40".into()),
            ("notifications.count".into(), "3".into()),
        ]
    );
    let allow = &s.allow[&CheckId::Envelope];
    assert_eq!(allow[0].node, Selector::Id("spinner".into()));
    assert_eq!(allow[0].why, "a spinner turns");
    assert_eq!(s.tolerance.edge_px, 1.0);
    assert_eq!(s.tolerance.teleport_px, 48.0);
    assert_eq!(s.budget.items, Some(200));
    assert_eq!(s.budget.damage_px, Some(20000));
    assert_eq!(s.budget.layout_passes, Some(3));
    let Config::Inline(sec) = &s.config else {
        panic!()
    };
    assert_eq!(
        sec.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
        ["shell.strand", "lib/more.strand"]
    );
}

#[test]
fn every_verb_parses() {
    let steps = r#"set dnd false
set title "a b  c"
set $accent #ff8800
set notifications.count 3
toggle dnd
emit notifications.received 4 "x y"
notify "Hello" "A body" app=mail urgency=critical
notify "Just a summary"
pointer 10,5
pointer 1.5,2 on #pop
press
press right
release middle
click 12,3
click 12,3 right on bar>popup
scroll -3
swipe 60 over 80ms
swipe -4.5 over 200
leave
text hold
text hold where "iiii"
text release where "iiii" via waker
text release
text late 2 where "iiii" via paint
text late 1
drop 3
drop 2 surface=#pop
jitter 1,1,2,1
reduce-motion on
reduce-motion off
scale 1.5
hz 144
backend gpu
mark focus off
shot the pill
log anything at all # but not this
intend #subject moves x
intend @shell.strand:7:9[1] enters
intend text("iiii") changes opacity scale
expect still #bars y
expect visible #count
expect hidden #focus
expect moves bar>split>end>row>box[2] x from +3
expect still #bars
frames 12
wait 100
wait 40ms
rest
rest max 90
glide 0,5 -> 100,5 over 200
drag 10,10 -> 40,10 over 120ms on #card
"#;
    let s = parse(&format!(
        "id = \"v\"\n=== shell.strand\nbar Top {{ height: 30 }}\n=== steps\n{steps}"
    ));
    let k: Vec<&StepKind> = s.steps.iter().map(|s| &s.kind).collect();
    assert_eq!(s.steps.len(), steps.lines().count());
    assert_eq!(s.steps[0].line, 5);
    let p = |x, y| Point { x, y };
    let expected = vec![
        StepKind::Set {
            path: "dnd".into(),
            value: "false".into(),
        },
        StepKind::Set {
            path: "title".into(),
            value: "a b  c".into(),
        },
        StepKind::Set {
            path: "$accent".into(),
            value: "#ff8800".into(),
        },
        StepKind::Set {
            path: "notifications.count".into(),
            value: "3".into(),
        },
        StepKind::Toggle { path: "dnd".into() },
        StepKind::Emit {
            path: "notifications.received".into(),
            args: vec!["4".into(), "x y".into()],
        },
        StepKind::Notify {
            summary: "Hello".into(),
            body: Some("A body".into()),
            app: Some("mail".into()),
            urgency: Some("critical".into()),
        },
        StepKind::Notify {
            summary: "Just a summary".into(),
            body: None,
            app: None,
            urgency: None,
        },
        StepKind::Pointer {
            at: p(10.0, 5.0),
            on: None,
        },
        StepKind::Pointer {
            at: p(1.5, 2.0),
            on: Some(Selector::Id("pop".into())),
        },
        StepKind::Press {
            button: Button::Left,
        },
        StepKind::Press {
            button: Button::Right,
        },
        StepKind::Release {
            button: Button::Middle,
        },
        StepKind::Click {
            at: p(12.0, 3.0),
            on: None,
            button: Button::Left,
        },
        StepKind::Click {
            at: p(12.0, 3.0),
            on: Some(Selector::Path(vec![
                PathSeg {
                    kind: NodeKind::Bar,
                    nth: None,
                },
                PathSeg {
                    kind: NodeKind::Popup,
                    nth: None,
                },
            ])),
            button: Button::Right,
        },
        StepKind::Scroll { dy: -3.0 },
        StepKind::Swipe { dy: 60.0, ms: 80 },
        StepKind::Swipe { dy: -4.5, ms: 200 },
        StepKind::Leave,
        StepKind::TextHold { filter: None },
        StepKind::TextHold {
            filter: Some("iiii".into()),
        },
        StepKind::TextRelease {
            filter: Some("iiii".into()),
            via: Via::Waker,
        },
        StepKind::TextRelease {
            filter: None,
            via: Via::Paint,
        },
        StepKind::TextLate {
            n: 2,
            filter: Some("iiii".into()),
            via: Via::Paint,
        },
        StepKind::TextLate {
            n: 1,
            filter: None,
            via: Via::Paint,
        },
        StepKind::Drop {
            n: 3,
            surface: None,
        },
        StepKind::Drop {
            n: 2,
            surface: Some(Selector::Id("pop".into())),
        },
        StepKind::Jitter(vec![1.0, 1.0, 2.0, 1.0]),
        StepKind::ReduceMotion(true),
        StepKind::ReduceMotion(false),
        StepKind::Scale(1.5),
        StepKind::Hz(144),
        StepKind::Backend(BackendMode::Gpu),
        StepKind::Mark("focus off".into()),
        StepKind::Shot("the pill".into()),
        StepKind::Log("anything at all".into()),
        StepKind::Intend {
            sel: Selector::Id("subject".into()),
            what: Intention::Moves,
            axes: Axes::X,
        },
        StepKind::Intend {
            sel: Selector::Origin {
                file: "shell.strand".into(),
                line: 7,
                col: Some(9),
                nth: Some(1),
            },
            what: Intention::Enters,
            axes: Axes::ALL,
        },
        StepKind::Intend {
            sel: Selector::Text("iiii".into()),
            what: Intention::Changes,
            axes: Axes::OPACITY | Axes::SCALE,
        },
        StepKind::Expect {
            what: ExpectKind::Still,
            sel: Selector::Id("bars".into()),
            axis: Some(Axis::Y),
            from: 0,
        },
        StepKind::Expect {
            what: ExpectKind::Visible,
            sel: Selector::Id("count".into()),
            axis: None,
            from: 0,
        },
        StepKind::Expect {
            what: ExpectKind::Hidden,
            sel: Selector::Id("focus".into()),
            axis: None,
            from: 0,
        },
        StepKind::Expect {
            what: ExpectKind::Moves,
            sel: Selector::Path(vec![
                PathSeg {
                    kind: NodeKind::Bar,
                    nth: None,
                },
                PathSeg {
                    kind: NodeKind::Split,
                    nth: None,
                },
                PathSeg {
                    kind: NodeKind::End,
                    nth: None,
                },
                PathSeg {
                    kind: NodeKind::Row,
                    nth: None,
                },
                PathSeg {
                    kind: NodeKind::Box,
                    nth: Some(2),
                },
            ]),
            axis: Some(Axis::X),
            from: 3,
        },
        StepKind::Expect {
            what: ExpectKind::Still,
            sel: Selector::Id("bars".into()),
            axis: None,
            from: 0,
        },
        StepKind::Frames(12),
        StepKind::Wait(100),
        StepKind::Wait(40),
        StepKind::Rest { max: 600 },
        StepKind::Rest { max: 90 },
        StepKind::Glide {
            from: p(0.0, 5.0),
            to: p(100.0, 5.0),
            ms: 200,
            on: None,
        },
        StepKind::Drag {
            from: p(10.0, 10.0),
            to: p(40.0, 10.0),
            ms: 120,
            on: Some(Selector::Id("card".into())),
        },
    ];
    assert_eq!(k.len(), expected.len());
    for (i, (got, want)) in k.iter().zip(&expected).enumerate() {
        assert_eq!(*got, want, "step {i}: {}", s.steps[i].text);
    }
    // Which steps move time and which open a segment.
    let time: Vec<bool> = s.steps.iter().map(|s| s.kind.is_time()).collect();
    assert_eq!(time.iter().filter(|t| **t).count(), 9);
    assert!(s.steps[0].kind.opens_segment());
    assert!(!s.steps[33].kind.opens_segment(), "mark only names it");
    assert!(
        s.steps[17].kind.is_time() && s.steps[17].kind.opens_segment(),
        "a swipe"
    );
    let leave = s.steps.iter().find(|s| s.kind == StepKind::Leave).unwrap();
    assert!(leave.kind.opens_segment(), "input opens one");
}

#[test]
fn selectors_print_as_they_parse() {
    for s in [
        "#bars",
        "@shell.strand:7",
        "@shell.strand:7:9",
        "@lib/x.strand:3:1[2]",
        "text(\"iiii\")",
        "bar>split>end>row>box[2]",
    ] {
        let sel = Selector::parse(s).unwrap();
        assert_eq!(sel.to_string(), s);
    }
    for bad in [
        "#",
        "#a-b",
        "@shell.strand",
        "@shell.strand:0",
        "@shell.strand:x",
        "@shell:3",
        "text(iiii)",
        "bar>nope",
        "box[x]",
        "box]",
    ] {
        assert!(Selector::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn mistakes_name_their_line() {
    let with_steps = |steps: &str| {
        format!("id = \"m\"\n=== shell.strand\nbar Top {{ height: 30 }}\n=== steps\nrest\n{steps}")
    };
    // Line 6 is the second step.
    for (steps, want) in [
        ("jump 3", "`jump` is not a step"),
        ("frames many", "not a whole number"),
        ("rest max", "`rest [max N]`"),
        ("pointer 3", "not a point"),
        ("press thumb", "not a button"),
        ("leave #bar", "unexpected `#bar` after `leave`"),
        ("expect sideways #a", "`expect sideways`"),
        ("expect visible #a x", "takes no axis"),
        ("expect still #a from 3", "from +K"),
        ("intend #a wobbles", "moves, enters, exits or changes"),
        ("intend #a moves sideways", "not an axis"),
        ("text late x", "not a whole number"),
        ("text release via bus", "`via waker` or `via paint`"),
        ("text hold via waker", "unexpected `via`"),
        ("jitter 1,0", "above 0"),
        ("reduce-motion maybe", "on or off"),
        ("scale 0", "not a scale"),
        ("hz 0", "above 0"),
        ("backend vulkan", "not a backend"),
        ("glide 0,0 to 3,3 over 9", "X,Y -> X2,Y2"),
        ("swipe 60", "`swipe DY over MS`"),
        ("swipe far over 80", "not a distance"),
        ("swipe 60 over 0", "not milliseconds"),
        ("swipe 60 over 80 now", "unexpected `now`"),
        ("drop 2 on #x", "surface=<selector>"),
        ("set dnd", "a path and a value"),
        ("set \"unclosed", "not closed"),
        ("expect still #a-b", "an id is a name"),
    ] {
        let (line, msg) = fails(&with_steps(steps));
        assert_eq!(line, 6, "{steps}: {msg}");
        assert!(msg.contains(want), "{steps}: {msg}");
    }
}

#[test]
fn header_mistakes_name_their_line() {
    let at = |header: &str| {
        fails(&format!(
            "{header}\n=== shell.strand\nbar Top {{ height: 30 }}\n=== steps\nrest\n"
        ))
    };
    let (line, msg) = at("id = \"h\"\nhz = 60\nwobble = 3");
    assert_eq!(line, 3, "{msg}");
    assert!(msg.contains("wobble"), "{msg}");
    let (line, msg) = at("id = \"h\"\nhz = \"fast\"");
    assert_eq!(line, 2, "{msg}");
    let (line, msg) = at("id = \"h\"\ntext = \"lazy\"");
    assert_eq!((line, msg.contains("inline or worker")), (2, true), "{msg}");
    let (line, msg) = at("id = \"h\"\nchecks = [\"settle\", \"vibes\"]");
    assert_eq!(
        (line, msg.contains("`vibes` is not a check")),
        (2, true),
        "{msg}"
    );
    let (line, msg) = at("id = \"h\"\n\n[expect_fail]\ntwin = \"\"");
    assert_eq!((line, msg.contains("needs a reason")), (3, true), "{msg}");
    let (line, msg) = at("id = \"h\"\n[allow]\nvanish = [{ node = \"#a\", why = \" \" }]");
    assert_eq!((line, msg.contains("`why` is empty")), (2, true), "{msg}");
    let (_, msg) = at("id = \"h\"\n[allow]\nvanish = [{ node = \"#a\" }]");
    assert!(msg.contains("why"), "`why` is mandatory: {msg}");
    let (line, msg) = at("id = \"Bad Id\"");
    assert_eq!((line, msg.contains("lowercase")), (1, true), "{msg}");
    let (line, msg) = at("id = \"h\"\nscreen = { width = 300, scale = 0 }");
    assert_eq!((line, msg.contains("0 to 4")), (2, true), "{msg}");
    let (line, msg) = at("id = \"h\"\n[tolerance]\nedge_px = -1.0");
    assert_eq!((line, msg.contains("0 or more")), (2, true), "{msg}");
    let (_, msg) = at("hz = 60");
    assert!(msg.contains("id"), "id is required: {msg}");
}

#[test]
fn sections_are_checked() {
    let (line, msg) = fails("id = \"s\"\n=== shell.strand\nbar T { height: 30 }\n");
    assert!(msg.contains("no `=== steps`"), "{line}: {msg}");
    let (line, msg) = fails("id = \"s\"\n=== steps\nrest\n=== shell.strand\nbar T {}\n");
    assert_eq!((line, msg.contains("last section")), (4, true), "{msg}");
    let (line, msg) = fails("id = \"s\"\n=== ../x.strand\nbar T {}\n=== steps\nrest\n");
    assert_eq!((line, msg.contains("relative")), (2, true), "{msg}");
    let (line, msg) = fails("id = \"s\"\n=== notes.txt\nhi\n=== steps\nrest\n");
    assert_eq!((line, msg.contains("<path>.strand")), (2, true), "{msg}");
    let (line, msg) = fails(
        "id = \"s\"\n=== shell.strand\nbar T {}\n=== shell.strand\nbar U {}\n=== steps\nrest\n",
    );
    assert_eq!((line, msg.contains("twice")), (4, true), "{msg}");
    let (_, msg) = fails("id = \"s\"\n=== steps\nrest\n");
    assert!(msg.contains("no config"), "{msg}");
    let (_, msg) = fails(
        "id = \"s\"\nconfig = \"../../fixtures/a.strand\"\n=== shell.strand\nbar T {}\n=== steps\nrest\n",
    );
    assert!(msg.contains("not both"), "{msg}");
}

#[test]
fn a_committed_scenario_names_only_files_inside_eval() {
    let named = |config: &str| format!("id = \"c\"\nconfig = \"{config}\"\n=== steps\nrest\n");
    // Inside: fixtures, relative to the scenario's directory.
    let s = parse(&named("../../fixtures/status-row.strand"));
    assert_eq!(
        s.config,
        Config::Path(eval_root().join("fixtures/status-row.strand"))
    );
    // Outside: the owner's configs, the home directory, the repository.
    for outside in [
        "../../../target/try/island",
        "/home/someone/.config/strand",
        "../../../crates/strand/tests/fixtures/bar.strand",
        "../../fixtures/../../docs",
    ] {
        let (line, msg) = fails(&named(outside));
        assert_eq!(line, 2, "{msg}");
        assert!(msg.contains("outside eval/"), "{outside}: {msg}");
    }
    // Local fonts and `config_dir` are the owner's alone.
    let (line, msg) = fails(&format!("fonts = \"local\"\n{MIN}"));
    assert_eq!(
        (line, msg.contains("only in eval/scenarios/local/")),
        (1, true)
    );
    let (_, msg) = fails("id = \"c\"\nconfig_dir = \"${HOME}\"\n=== steps\nrest\n");
    assert!(msg.contains("only in eval/scenarios/local/"), "{msg}");
    // A regression is held to the same rule.
    let reg = eval_root().join("scenarios/regressions/r.scn");
    let (_, msg) = fails_at(&named("/tmp/x.strand"), &reg);
    assert!(msg.contains("outside eval/"), "{msg}");
}

#[test]
fn a_local_scenario_may_name_the_owners_files() {
    let local = eval_root().join("scenarios/local/mine.scn");
    assert_eq!(place_of(&local), Place::Local);
    assert_eq!(place_of(&file()), Place::Committed);
    let s = Scenario::parse(
        "id = \"l\"\nfonts = \"local\"\nconfig = \"/srv/config/shell.strand\"\n=== steps\nrest\n",
        &local,
    )
    .unwrap();
    assert_eq!(s.fonts, Fonts::Local);
    assert_eq!(s.config, Config::Path("/srv/config/shell.strand".into()));
    // `config_dir` expands variables, and names the one unset.
    let (line, msg) = fails_at(
        "id = \"l\"\nconfig_dir = \"${STRAND_EVAL_SURELY_UNSET_VAR}\"\n=== steps\nrest\n",
        &local,
    );
    assert_eq!(line, 2);
    assert!(
        msg.contains("STRAND_EVAL_SURELY_UNSET_VAR is not set"),
        "{msg}"
    );
    // So does `config` (docs/eval.md, "Local configs and fonts"); cargo
    // sets CARGO_MANIFEST_DIR for the tests it runs.
    let s = Scenario::parse(
        "id = \"l\"\nconfig = \"${CARGO_MANIFEST_DIR}/fixtures/status-row.strand\"\n=== steps\nrest\n",
        &local,
    )
    .unwrap();
    assert_eq!(
        s.config,
        Config::Path(eval_root().join("fixtures/status-row.strand"))
    );
    let (line, msg) = fails_at(
        "id = \"l\"\nconfig = \"${STRAND_EVAL_SURELY_UNSET_VAR}/shell.strand\"\n=== steps\nrest\n",
        &local,
    );
    assert_eq!(line, 2);
    assert!(
        msg.contains("STRAND_EVAL_SURELY_UNSET_VAR is not set"),
        "{msg}"
    );
    // A committed scenario does not expand it.
    let (_, msg) = fails(
        "id = \"c\"\nconfig = \"${CARGO_MANIFEST_DIR}/fixtures/status-row.strand\"\n=== steps\nrest\n",
    );
    assert!(msg.contains("only in eval/scenarios/local/"), "{msg}");
}

#[test]
fn hash_is_a_comment_only_on_its_own() {
    let s = parse(
        "id = \"h\"\n=== shell.strand\nbar T { height: 30 }\n=== steps\n# a whole-line comment\nset $accent #ff0000 # the accent\nexpect still #bars   #   why\n",
    );
    assert_eq!(s.steps.len(), 2);
    assert_eq!(
        s.steps[0].kind,
        StepKind::Set {
            path: "$accent".into(),
            value: "#ff0000".into()
        }
    );
    assert_eq!(s.steps[0].line, 6);
    assert_eq!(s.steps[1].text, "expect still #bars");
}

/// Every committed scenario parses as committed (nothing outside eval/,
/// bundled fonts), its ids are its file names and unique, and each says
/// what it is about.
#[test]
fn every_committed_scenario_parses() {
    let dirs = [
        eval_root().join("scenarios/curated"),
        eval_root().join("scenarios/regressions"),
    ];
    let files = strand_eval::cli::collect(&dirs).unwrap();
    assert!(files.len() >= 11, "{files:?}");
    let mut ids = Vec::new();
    for f in &files {
        let s = Scenario::load(f).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.place, Place::Committed, "{}", f.display());
        assert_eq!(s.fonts, Fonts::Bundled, "{}", f.display());
        assert_eq!(
            f.file_stem().and_then(|x| x.to_str()),
            Some(s.id.as_str()),
            "{}",
            f.display()
        );
        assert!(!s.about.trim().is_empty(), "{}: no about", f.display());
        assert!(!ids.contains(&s.id), "{} twice", s.id);
        ids.push(s.id);
    }
    for want in [
        "bug24-flip-open",
        "bug24-flip-open-late1",
        "bug24-flip-open-min",
        "bug25-hidden-text",
    ] {
        assert!(ids.iter().any(|i| i == want), "{want} missing: {ids:?}");
    }
}

/// eval/mutants/mutants.txt names patches and scenarios that exist, and
/// checks that do (or, eval-finish, a selector the run stops at:
/// `unmatched:#id`, which the scenario names).
#[test]
fn every_mutant_line_names_what_exists() {
    let root = eval_root();
    let text = std::fs::read_to_string(root.join("mutants/mutants.txt")).unwrap();
    let mut n = 0;
    for line in text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        let mut it = line.split_whitespace();
        let (patch, scn) = (it.next().unwrap(), it.next().unwrap());
        assert!(root.join("mutants").join(patch).is_file(), "{patch}");
        let s = Scenario::load(&root.join("scenarios").join(scn)).unwrap();
        assert!(
            s.expect_fail.is_empty(),
            "{scn}: a mutant's scenario passes on HEAD outright"
        );
        let specs: Vec<&str> = it.collect();
        assert!(!specs.is_empty(), "{line}");
        for spec in specs {
            if let Some(sel) = spec.strip_prefix("unmatched:") {
                let text = std::fs::read_to_string(root.join("scenarios").join(scn)).unwrap();
                assert!(
                    text.lines().any(|l| l.split_whitespace().any(|w| w == sel)),
                    "{scn} names no {sel}"
                );
                continue;
            }
            let check = spec.split(':').next().unwrap();
            assert!(CheckId::parse(check).is_some(), "{spec}");
        }
        n += 1;
    }
    assert_eq!(n, 49);
}

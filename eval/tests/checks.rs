//! Every check against a synthetic trace it passes and one it fails
//! (docs/eval.md, "Checks"), the shapes of owner-testing.md bugs 24 and
//! 25 among them, and the motion a correct shell has that no check may
//! flag: an authored enter pose, a glide's overshoot, reduced motion's
//! snaps.

use std::collections::BTreeMap;

use strand_eval::checks::{self, CheckId, Reference, Report, Sev, Status};
use strand_eval::scenario::{Scenario, eval_root};
use strand_eval::trace::{
    Cost, ExpectRec, Frame, IntendRec, NodeMeta, Pose, RestRec, Row, RunTrace, ScenarioInfo,
    Segment,
};
use strand_render::probe::{Axes, Motions};

const BAR: [f32; 2] = [300.0, 30.0];

fn scenario(header: &str) -> Scenario {
    let text = format!(
        "id = \"synth\"\n{header}\n=== shell.strand\nbar Top {{ height: 30 }}\n=== steps\nrest\n"
    );
    let file = eval_root().join("scenarios/regressions/synth.scn");
    Scenario::parse(&text, &file).unwrap_or_else(|e| panic!("{e}"))
}

fn row(key: &str, rect: [f32; 4]) -> Row {
    Row {
        key: key.into(),
        surface: 1,
        drawn: rect,
        visual: rect,
        laid: rect,
        op: 1.0,
        own_op: 1.0,
        scale: [1.0, 1.0],
        ..Row::default()
    }
}

trait Shape {
    /// Drawn at `x, y` (laid where it was).
    fn at(self, x: f32, y: f32) -> Row;
    fn laid_at(self, x: f32, y: f32) -> Row;
    fn mot(self, m: Motions) -> Row;
    fn auth(self, a: Axes) -> Row;
    fn op(self, op: f32) -> Row;
    fn spring(self, x: f32, y: f32) -> Row;
}

impl Shape for Row {
    fn at(mut self, x: f32, y: f32) -> Row {
        self.visual[0] = x;
        self.visual[1] = y;
        self.drawn[0] = x;
        self.drawn[1] = y;
        self
    }
    fn laid_at(mut self, x: f32, y: f32) -> Row {
        self.laid[0] = x;
        self.laid[1] = y;
        self
    }
    fn mot(mut self, m: Motions) -> Row {
        self.mot = m.0;
        self.emot = m.0;
        self
    }
    fn auth(mut self, a: Axes) -> Row {
        self.auth = a.0;
        self
    }
    fn op(mut self, op: f32) -> Row {
        self.op = op;
        self.own_op = op;
        self
    }
    fn spring(mut self, x: f32, y: f32) -> Row {
        self.spring = [x, y];
        self
    }
}

fn meta(key: &str, parent: Option<&str>) -> NodeMeta {
    NodeMeta {
        label: format!("box#{key}"),
        kind: "box".into(),
        origin: Some("shell.strand:1:1".into()),
        parent: parent.map(String::from),
        id: Some(key.into()),
        ..NodeMeta::default()
    }
}

/// A trace of ticks `0..=end`: boot at rest by tick 0, then one segment
/// from tick 0 (the frame before its change) to its rest at `end`; tick
/// `k` draws `rows(k)`.
fn shape(end: u32, rows: impl Fn(u32) -> Vec<Row>) -> RunTrace {
    let mut t = RunTrace::new(
        ScenarioInfo {
            id: "synth".into(),
            hz: 60,
            scale: 1.0,
            ..ScenarioInfo::default()
        },
        Default::default(),
    );
    t.segments = vec![
        Segment {
            name: "boot".into(),
            s: None,
            e: 0,
            settled: true,
            hz: 60,
            ..Segment::default()
        },
        Segment {
            name: "change".into(),
            line: 2,
            s: Some(0),
            e: end,
            settled: true,
            hz: 60,
            ..Segment::default()
        },
    ];
    t.rests = vec![
        RestRec {
            line: 1,
            k: 0,
            ticks: 1,
            max: 600,
            settled: true,
        },
        RestRec {
            line: 3,
            k: 1,
            ticks: end,
            max: 600,
            settled: true,
        },
    ];
    for k in 0..=end {
        let n = rows(k);
        for r in &n {
            t.nodes
                .entry(r.key.clone())
                .or_insert_with(|| meta(&r.key, None));
        }
        let damage = if k == 0 { 9000 } else { 100 };
        t.frames.push(Frame {
            k,
            t_ns: 1_000_000_000 + u64::from(k) * 16_666_667,
            painted: vec![1],
            sizes: BTreeMap::from([(1, BAR)]),
            cost: BTreeMap::from([(
                1,
                Cost {
                    paints: 1,
                    damage_px: damage,
                    items: n.len() as u32 + 1,
                    records: 1,
                    ..Cost::default()
                },
            )]),
            n,
            ..Frame::default()
        });
    }
    t.seal();
    t
}

fn check(sc: &Scenario, t: &RunTrace) -> Report {
    checks::evaluate(sc, t, None, None, None)
}

fn status(r: &Report, c: CheckId) -> Status {
    r.status(c)
        .cloned()
        .unwrap_or(Status::Skip("absent".into()))
}

fn fails_on(r: &Report, c: CheckId, key: &str, axis: &str) -> bool {
    r.failures(c)
        .iter()
        .any(|f| f.key.as_deref() == Some(key) && f.axis.as_deref() == Some(axis))
}

fn show(r: &Report, t: &RunTrace) -> String {
    r.lines(t).join("\n")
}

/// A smooth step from `a` to `b` over ticks `1..=n`.
fn ease(a: f32, b: f32, k: u32, n: u32) -> f32 {
    let p = (k.min(n) as f32 / n as f32).clamp(0.0, 1.0);
    a + (b - a) * (1.0 - (1.0 - p).powi(3))
}

// --- settle

#[test]
fn settle_passes_a_rest_reached_and_fails_one_that_is_not() {
    let sc = scenario("");
    let t = shape(4, |_| vec![row("a", [10.0, 10.0, 10.0, 10.0])]);
    assert_eq!(status(&check(&sc, &t), CheckId::Settle), Status::Pass);
    let mut t = t;
    t.rests[1].settled = false;
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Settle),
        Status::Fail,
        "{}",
        show(&r, &t)
    );
    assert!(r.failures(CheckId::Settle)[0].msg.contains("line 3"));
    assert!(!r.ok());
}

// --- bug 24

/// (owner-testing.md bug 24) Text mounted beside the bars moves them
/// left; the glide carried the estimate's offset, so they dropped out of
/// the island (y 9 → 21 → 9, laid y unchanged) and went past their way
/// left (x to 190 on the way from 225 to 211).
fn bug_24(fixed: bool) -> RunTrace {
    shape(12, |k| {
        let bars = row("bars", [225.0, 9.0, 15.0, 12.0]);
        if k == 0 {
            return vec![bars];
        }
        let (x, y) = if fixed {
            (ease(225.0, 211.0, k, 10), 9.0)
        } else {
            let y = match k {
                1..=3 => 21.0,
                4 => 15.0,
                _ => 9.0,
            };
            (ease(190.0, 211.0, k - 1, 9), y)
        };
        let glide = if k < 11 { Motions::GLIDE } else { Motions(0) };
        vec![bars.laid_at(211.0, 9.0).at(x, y).mot(glide)]
    })
}

#[test]
fn the_bug_24_shape_fails_bystander_and_envelope() {
    let sc = scenario("");
    let t = bug_24(false);
    let r = check(&sc, &t);
    let lines = show(&r, &t);
    assert_eq!(status(&r, CheckId::Bystander), Status::Fail, "{lines}");
    assert!(fails_on(&r, CheckId::Bystander, "bars", "y"), "{lines}");
    assert_eq!(status(&r, CheckId::Envelope), Status::Fail, "{lines}");
    assert!(fails_on(&r, CheckId::Envelope, "bars", "x"), "{lines}");
    // One finding per node and axis, the rest counted.
    let y = r
        .failures(CheckId::Bystander)
        .into_iter()
        .find(|f| f.axis.as_deref() == Some("y"))
        .unwrap();
    assert_eq!((y.k, y.more), (Some(1), 3), "{y:?}");
    assert!(
        lines.contains(
            "bystander [change] box#bars (shell.strand:1:1) k1 y 21.00 not in [8.00, 10.00]"
        ),
        "{lines}"
    );
}

#[test]
fn the_bug_24_fix_passes_every_check() {
    let sc = scenario("");
    let t = bug_24(true);
    let r = check(&sc, &t);
    assert!(r.ok(), "{}", show(&r, &t));
    for c in [
        CheckId::Bystander,
        CheckId::Envelope,
        CheckId::Teleport,
        CheckId::Vanish,
    ] {
        assert_eq!(status(&r, c), Status::Pass, "{c:?}: {}", show(&r, &t));
    }
}

// --- bug 25

/// (owner-testing.md bug 25) Focus turns off: its glyph goes and the
/// count enters faded out (`enter { opacity: 0; x: -6 }`) with a text
/// laid by its estimate, so the bars and the last box, neither moving in
/// layout, fell (y 9 → 14 and 10 → 15) and came back.
fn bug_25(fixed: bool) -> RunTrace {
    let mut t = shape(14, |k| {
        let bars = row("bars", [225.0, 9.0, 15.0, 12.0]);
        let last = row("last", [266.0, 10.0, 20.0, 10.0]);
        if k == 0 {
            return vec![bars, row("focus", [246.0, 8.0, 14.0, 14.0]), last];
        }
        let fall = if fixed || k > 6 {
            0.0
        } else {
            5.0 * (7 - k) as f32 / 6.0
        };
        let p = ease(0.0, 1.0, k, 10);
        let entering = k < 14;
        let m = if entering {
            Motions::ENTER | Motions::PROP_SPRING | Motions::OPACITY_SPRING
        } else {
            Motions(0)
        };
        let a = if entering {
            Axes::X | Axes::OPACITY
        } else {
            Axes(0)
        };
        let mut text = row("text", [246.0, 8.0, 14.0, 15.0])
            .at(240.0 + 6.0 * p, 8.0)
            .op(p);
        text.text = Some(if !fixed && k < 3 { "E" } else { "S" }.into());
        text.auth = a.0;
        text.emot = m.0;
        vec![
            bars.at(225.0, 9.0 + fall),
            row("count", [246.0, 8.0, 14.0, 15.0])
                .at(240.0 + 6.0 * p, 8.0)
                .op(p)
                .spring(-6.0 * (1.0 - p), 0.0)
                .mot(m)
                .auth(a),
            text,
            last.at(266.0, 10.0 + fall),
        ]
    });
    let count = t.nodes.get_mut("count").unwrap();
    count.enter = Some(Pose {
        opacity: Some(0.0),
        x: Some(-6.0),
        ..Pose::default()
    });
    t.nodes.get_mut("text").unwrap().parent = Some("count".into());
    t.seal();
    t
}

#[test]
fn the_bug_25_shape_fails_bystander_on_the_bars_and_the_last_box() {
    let sc = scenario("");
    let t = bug_25(false);
    let r = check(&sc, &t);
    let lines = show(&r, &t);
    assert_eq!(status(&r, CheckId::Bystander), Status::Fail, "{lines}");
    assert!(fails_on(&r, CheckId::Bystander, "bars", "y"), "{lines}");
    assert!(fails_on(&r, CheckId::Bystander, "last", "y"), "{lines}");
    // The count's authored entrance is no finding.
    assert!(
        r.results
            .iter()
            .flat_map(|c| &c.findings)
            .all(|f| f.key.as_deref() != Some("count")),
        "{lines}"
    );
}

#[test]
fn the_bug_25_fix_with_its_authored_enter_passes() {
    let sc = scenario("");
    let t = bug_25(true);
    let r = check(&sc, &t);
    assert!(r.ok(), "{}", show(&r, &t));
    assert_eq!(status(&r, CheckId::Vanish), Status::Pass);
}

// --- an authored enter

/// `enter { opacity: 0; x: -6 }` mounted after boot: its first frame
/// already one frame into its springs (as the driver draws it: opacity
/// 0.14, x −4.44 on focus-off), sliding 6 px right while it fades in.
fn entering(pose_played: bool) -> RunTrace {
    let mut t = shape(12, |k| {
        let mut v = vec![row("a", [10.0, 10.0, 10.0, 10.0])];
        if k >= 1 {
            let p = if pose_played {
                ease(0.14, 1.0, k - 1, 9)
            } else {
                1.0
            };
            let (m, a) = if pose_played && k < 12 {
                (
                    Motions::ENTER | Motions::PROP_SPRING,
                    Axes::X | Axes::OPACITY,
                )
            } else {
                (Motions(0), Axes(0))
            };
            v.push(
                row("pill", [40.0, 10.0, 30.0, 10.0])
                    .at(40.0 - 6.0 * (1.0 - p), 10.0)
                    .spring(-6.0 * (1.0 - p), 0.0)
                    .op(p)
                    .mot(m)
                    .auth(a),
            );
        }
        v
    });
    t.nodes.get_mut("pill").unwrap().enter = Some(Pose {
        opacity: Some(0.0),
        x: Some(-6.0),
        ..Pose::default()
    });
    t.seal();
    t
}

#[test]
fn an_authored_enter_x_passes() {
    let sc = scenario("");
    let t = entering(true);
    let r = check(&sc, &t);
    assert!(r.ok(), "{}", show(&r, &t));
    for c in [
        CheckId::Bystander,
        CheckId::Envelope,
        CheckId::Teleport,
        CheckId::Vanish,
    ] {
        assert_eq!(status(&r, c), Status::Pass, "{c:?}");
    }
}

#[test]
fn an_enter_pose_skipped_is_a_pop_in() {
    let sc = scenario("");
    let t = entering(false);
    let r = check(&sc, &t);
    let lines = show(&r, &t);
    assert_eq!(status(&r, CheckId::Vanish), Status::Fail, "{lines}");
    let f = &r.failures(CheckId::Vanish)[0];
    assert!(f.msg.starts_with("pop-in"), "{f:?}");
    assert!(f.msg.contains("without playing its pose"), "{f:?}");
    assert_eq!(f.key.as_deref(), Some("pill"));
}

// --- envelope: a glide's overshoot

fn overshoot(share: f32) -> RunTrace {
    // 0 → 100 on x, past it by `share` of the way at tick 4, then back.
    shape(12, move |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        if k == 0 {
            return vec![a];
        }
        let x = match k {
            1 => 30.0,
            2 => 70.0,
            3 => 100.0 + 80.0 * share,
            4 => 100.0 + 100.0 * share,
            5 => 100.0 + 60.0 * share,
            6 => 100.0 + 20.0 * share,
            _ => 100.0,
        };
        let m = if k < 10 { Motions::GLIDE } else { Motions(0) };
        vec![a.laid_at(100.0, 0.0).at(x, 0.0).mot(m)]
    })
}

#[test]
fn a_30_percent_spring_overshoot_passes() {
    let sc = scenario("");
    let t = overshoot(0.3);
    let r = check(&sc, &t);
    assert!(r.ok(), "{}", show(&r, &t));
    assert_eq!(status(&r, CheckId::Envelope), Status::Pass);
    assert_eq!(status(&r, CheckId::Teleport), Status::Pass);
}

#[test]
fn an_overshoot_past_the_tolerance_fails_envelope() {
    let sc = scenario("");
    let t = overshoot(0.4);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Envelope, "a", "x"),
        "{}",
        show(&r, &t)
    );
    // The scenario may give more.
    let sc = scenario("[tolerance]\novershoot = 0.45");
    assert_eq!(status(&check(&sc, &t), CheckId::Envelope), Status::Pass);
}

#[test]
fn authored_motion_is_out_of_the_envelope() {
    let sc = scenario("");
    let mut t = overshoot(0.8);
    for f in &mut t.frames {
        for r in &mut f.n {
            r.mot = Motions::PROP_SPRING.0;
            r.emot = r.mot;
            r.auth = Axes::X.0;
        }
    }
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Envelope),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    // Keyframes are, whatever they author.
    for f in &mut t.frames {
        for r in &mut f.n {
            r.emot = Motions::KEYFRAMES.0;
            r.auth = 0;
        }
    }
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Envelope),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

// --- reduced motion

/// Motion reduced: the mover snaps 0 → 100 in one frame, and a
/// bystander is drawn off its level for a frame.
fn reduced(bystander_moves: bool) -> RunTrace {
    let mut t = shape(3, move |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        let b = row("b", [200.0, 5.0, 20.0, 20.0]);
        if k == 0 {
            return vec![a, b];
        }
        let by = if bystander_moves && k == 1 { 9.0 } else { 5.0 };
        vec![a.laid_at(100.0, 0.0).at(100.0, 0.0), b.at(200.0, by)]
    });
    t.segments[1].reduced_motion = true;
    t.seal();
    t
}

#[test]
fn a_reduced_motion_snap_passes_teleport() {
    let sc = scenario("");
    let t = reduced(false);
    let r = check(&sc, &t);
    assert!(r.ok(), "{}", show(&r, &t));
    assert_eq!(status(&r, CheckId::Teleport), Status::Pass);
    assert!(
        r.results.iter().all(|c| c.findings.is_empty()),
        "{}",
        show(&r, &t)
    );
}

#[test]
fn a_bystander_moved_under_reduced_motion_still_fails() {
    let sc = scenario("");
    let t = reduced(true);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    assert!(
        fails_on(&r, CheckId::Bystander, "b", "y"),
        "{}",
        show(&r, &t)
    );
    // A run reduced from the start skips teleport and twin outright.
    let mut t = t;
    t.scenario.reduced_motion = true;
    let r = check(&sc, &t);
    assert!(matches!(status(&r, CheckId::Teleport), Status::Skip(_)));
    assert!(matches!(status(&r, CheckId::Twin), Status::Skip(_)));
    assert_eq!(status(&r, CheckId::Bystander), Status::Fail);
}

// --- teleport

#[test]
fn a_glide_that_jumps_fails_teleport() {
    let sc = scenario("");
    let t = shape(6, |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        match k {
            0 => vec![a],
            1 => vec![a.laid_at(100.0, 0.0).at(5.0, 0.0).mot(Motions::GLIDE)],
            2 => vec![a.laid_at(100.0, 0.0).at(95.0, 0.0).mot(Motions::GLIDE)],
            _ => vec![a.laid_at(100.0, 0.0).at(100.0, 0.0)],
        }
    });
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
    let f = &r.failures(CheckId::Teleport)[0];
    assert_eq!(f.k, Some(2));
    assert!(f.msg.contains("jumped 90.0 px"), "{f:?}");
}

#[test]
fn a_drag_released_far_from_home_glides_back_without_teleport() {
    // Dragged 90 px away and released: no net travel over the segment,
    // the glide closes its 90 px offset in a few fast ticks. A tick that
    // closes all of what was left in one go still fails.
    let sc = scenario("");
    let glide = |x: f32, left: f32| {
        let mut r = row("a", [0.0, 0.0, 20.0, 20.0])
            .laid_at(0.0, 0.0)
            .at(x, 0.0)
            .mot(Motions::GLIDE);
        r.glide = [left, 0.0];
        r
    };
    let path = [0.0, 30.0, 60.0, 90.0, 72.0, 35.0, 11.0, 0.0];
    let t = shape(9, |k| match path.get(k as usize) {
        Some(x) if k > 0 && k < 8 => vec![glide(*x, *x)],
        _ => vec![row("a", [0.0, 0.0, 20.0, 20.0])],
    });
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let path = [0.0, 30.0, 60.0, 90.0, 0.5, 0.2, 0.1, 0.0];
    let t = shape(9, |k| match path.get(k as usize) {
        Some(x) if k > 0 && k < 8 => vec![glide(*x, *x)],
        _ => vec![row("a", [0.0, 0.0, 20.0, 20.0])],
    });
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
}

#[test]
fn a_layout_jump_with_no_motion_is_noted_not_failed() {
    let sc = scenario("");
    let t = shape(2, |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        if k == 0 {
            vec![a]
        } else {
            vec![a.laid_at(100.0, 0.0).at(100.0, 0.0)]
        }
    });
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let tp = r.results.iter().find(|c| c.check == "teleport").unwrap();
    assert_eq!(tp.findings.len(), 1);
    assert_eq!(tp.findings[0].sev, Sev::Info);
    assert!(tp.findings[0].msg.starts_with("jump"));
}

#[test]
fn a_size_layout_snaps_under_a_glide_is_noted_not_failed() {
    // A late text's estimate corrected: its box widens 41 px with its
    // layout while it glides a pixel on y. The glide moves it, the width
    // is layout's: noted. A size spring on it would make it a failure.
    let sc = scenario("");
    let make = |m: Motions| {
        shape(4, move |k| {
            let a = row("a", [0.0, 10.0, 58.0, 16.0]);
            match k {
                0 | 1 => vec![a],
                2 => vec![a.laid_at(0.0, 9.0).at(0.0, 9.7).mot(m)],
                _ => vec![a.laid_at(0.0, 9.0).at(0.0, 9.0)],
            }
            .into_iter()
            .map(|mut r| {
                if k >= 2 {
                    r.visual[2] = 99.0;
                    r.drawn[2] = 99.0;
                    r.laid[2] = 99.0;
                }
                r
            })
            .collect()
        })
    };
    let t = make(Motions::GLIDE);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let tp = r.results.iter().find(|c| c.check == "teleport").unwrap();
    assert!(
        tp.findings
            .iter()
            .any(|f| f.sev == Sev::Info && f.msg.starts_with("jump")),
        "{}",
        show(&r, &t)
    );
    let t = make(Motions::GLIDE | Motions::SIZE_SPRING);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "w"),
        "{}",
        show(&r, &t)
    );
}

#[test]
fn keyframes_on_x_alone_do_not_make_a_width_snap_a_jump() {
    // A `play` shaking it on x while its late text's width lands: the
    // width is layout's (noted). Keyframes on w size it: a failure.
    let sc = scenario("");
    let make = |a: Axes| {
        shape(4, move |k| {
            let r = row("a", [0.0, 10.0, 58.0, 16.0]);
            let mut r = match k {
                0 | 1 => r,
                2 => r.at(3.0, 10.0).mot(Motions::KEYFRAMES).auth(a),
                _ => r,
            };
            if k >= 2 {
                r.visual[2] = 99.0;
                r.drawn[2] = 99.0;
                r.laid[2] = 99.0;
            }
            vec![r]
        })
    };
    let t = make(Axes::X);
    let r = check(&sc, &t);
    assert!(
        !fails_on(&r, CheckId::Teleport, "a", "w"),
        "{}",
        show(&r, &t)
    );
    let t = make(Axes::X | Axes::W);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "w"),
        "{}",
        show(&r, &t)
    );
}

#[test]
fn a_drag_released_glides_back_from_where_the_drag_left_it() {
    // Lifted 90 px by a drag (LIFT), released: the first glide frame
    // closes 35 of the 90 px it was left, within 0.75 of it.
    let sc = scenario("");
    // The drag's offset is reported as its glide part (as the probe
    // does: m.jelly.144 k52, LIFT with glide 94.5).
    let lifted = |x: f32| {
        let mut r = row("a", [0.0, 0.0, 20.0, 20.0])
            .laid_at(0.0, 0.0)
            .at(x, 0.0)
            .mot(Motions::LIFT);
        r.glide = [x, 0.0];
        r
    };
    let glide = |x: f32| {
        let mut r = row("a", [0.0, 0.0, 20.0, 20.0])
            .laid_at(0.0, 0.0)
            .at(x, 0.0)
            .mot(Motions::GLIDE);
        r.glide = [x, 0.0];
        r
    };
    let t = shape(9, |k| match k {
        1..=3 => vec![lifted(30.0 * k as f32)],
        4 => vec![glide(55.0)],
        5 => vec![glide(25.0)],
        6 => vec![glide(8.0)],
        _ => vec![row("a", [0.0, 0.0, 20.0, 20.0])],
    });
    let r = check(&sc, &t);
    assert!(
        !fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
}

#[test]
fn a_glide_retargeted_from_its_layout_not_where_it_was_drawn_fails() {
    // Gliding 0 → 40; at tick 3 its layout moves to 30 and the drawn box
    // jumps with it instead of starting where it was.
    let sc = scenario("");
    let t = shape(10, |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        let g = Motions::GLIDE;
        match k {
            0 => vec![a],
            1 => vec![a.laid_at(40.0, 0.0).at(10.0, 0.0).mot(g)],
            2 => vec![a.laid_at(40.0, 0.0).at(18.0, 0.0).mot(g)],
            3 => vec![a.laid_at(30.0, 0.0).at(8.0, 0.0).mot(g)],
            4 => vec![a.laid_at(30.0, 0.0).at(16.0, 0.0).mot(g)],
            5 => vec![a.laid_at(30.0, 0.0).at(24.0, 0.0).mot(g)],
            _ => vec![a.laid_at(30.0, 0.0).at(30.0, 0.0)],
        }
    });
    let r = check(&sc, &t);
    let lines = show(&r, &t);
    let f = r.failures(CheckId::Teleport);
    assert_eq!(f.len(), 1, "{lines}");
    assert!(f[0].msg.starts_with("retarget"), "{lines}");
    assert_eq!(f[0].k, Some(3));
    // Started where it was drawn, it passes.
    let t = shape(10, |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        let g = Motions::GLIDE;
        match k {
            0 => vec![a],
            1 => vec![a.laid_at(40.0, 0.0).at(10.0, 0.0).mot(g)],
            2 => vec![a.laid_at(40.0, 0.0).at(18.0, 0.0).mot(g)],
            3 => vec![a.laid_at(30.0, 0.0).at(22.0, 0.0).mot(g)],
            4 => vec![a.laid_at(30.0, 0.0).at(26.0, 0.0).mot(g)],
            5 => vec![a.laid_at(30.0, 0.0).at(29.0, 0.0).mot(g)],
            _ => vec![a.laid_at(30.0, 0.0).at(30.0, 0.0)],
        }
    });
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

// --- vanish

fn blink(gone: impl Fn(u32) -> bool, m: Motions) -> RunTrace {
    shape(6, move |k| {
        let a = row("a", [10.0, 10.0, 10.0, 10.0]);
        if gone(k) {
            vec![a.op(0.0).mot(m)]
        } else {
            vec![a.mot(if k == 0 { Motions(0) } else { m })]
        }
    })
}

#[test]
fn a_node_gone_for_a_frame_and_back_vanishes() {
    let sc = scenario("");
    let t = blink(|k| k == 2, Motions(0));
    let r = check(&sc, &t);
    let f = r.failures(CheckId::Vanish);
    assert_eq!(f.len(), 1, "{}", show(&r, &t));
    assert!(f[0].msg.starts_with("vanish"));
    assert_eq!(f[0].k, Some(2));
    // Taken out by its exit and put back by its enter, it does not.
    let t = blink(|k| k == 2, Motions::EXIT);
    assert_eq!(status(&check(&sc, &t), CheckId::Vanish), Status::Pass);
    // Away for longer than three ticks it does not either.
    let t = blink(|k| (2..=5).contains(&k), Motions(0));
    assert_eq!(status(&check(&sc, &t), CheckId::Vanish), Status::Pass);
}

#[test]
fn a_node_shown_for_one_frame_flashes() {
    let sc = scenario("");
    let t = blink(|k| k != 3, Motions(0));
    let r = check(&sc, &t);
    let f = r.failures(CheckId::Vanish);
    assert!(
        f.iter()
            .any(|f| f.msg.starts_with("flash") && f.k == Some(3)),
        "{}",
        show(&r, &t)
    );
    let t = blink(|k| k != 3, Motions::KEYFRAMES);
    assert_eq!(status(&check(&sc, &t), CheckId::Vanish), Status::Pass);
}

#[test]
fn a_damaged_frame_that_drew_nothing_is_blank() {
    let sc = scenario("");
    let mut t = shape(3, |_| vec![row("a", [10.0, 10.0, 10.0, 10.0])]);
    t.frames[2].cost.get_mut(&1).unwrap().items = 0;
    let r = check(&sc, &t);
    let f = r.failures(CheckId::Vanish);
    assert!(
        f.iter()
            .any(|f| f.msg.starts_with("blank") && f.k == Some(2)),
        "{}",
        show(&r, &t)
    );
}

// --- twin

#[test]
fn twin_compares_each_rest_with_the_reduced_run() {
    let sc = scenario("");
    let t = bug_24(true);
    let mut twin = shape(1, |k| {
        let b = row("bars", [225.0, 9.0, 15.0, 12.0]);
        if k == 0 {
            vec![b]
        } else {
            vec![b.laid_at(211.0, 9.0).at(211.0, 9.0)]
        }
    });
    twin.segments[1].e = 1;
    twin.scenario.reduced_motion = true;
    let r = checks::evaluate(&sc, &t, Some(&twin), None, None);
    assert_eq!(status(&r, CheckId::Twin), Status::Pass, "{}", show(&r, &t));
    // A run that rests with an offset stuck on it does not match.
    let mut stuck = t.clone();
    stuck.frames.last_mut().unwrap().n[0].visual[0] = 214.0;
    stuck.seal();
    let r = checks::evaluate(&sc, &stuck, Some(&twin), None, None);
    assert!(
        fails_on(&r, CheckId::Twin, "bars", "x"),
        "{}",
        show(&r, &stuck)
    );
    // Nor one that rests with a node the reduced run does not show.
    let mut extra = t.clone();
    extra
        .frames
        .last_mut()
        .unwrap()
        .n
        .push(row("ghost", [0.0, 0.0, 5.0, 5.0]));
    extra.seal();
    let r = checks::evaluate(&sc, &extra, Some(&twin), None, None);
    let f = r.failures(CheckId::Twin);
    assert!(
        f.iter().any(|f| f.key.as_deref() == Some("ghost")),
        "{}",
        show(&r, &extra)
    );
    // A tilt held toward the pointer at rest is the run's alone: reduced
    // motion turns lean off by design (effects/lean.rs), so a leaning
    // node is left out; the same box without LEAN is a difference.
    let mut lean = t.clone();
    if let Some(last) = lean.frames.last_mut() {
        last.n[0].visual[2] = 18.0;
        last.n[0].mot = Motions::LEAN.0;
        last.n[0].emot = Motions::LEAN.0;
    }
    lean.seal();
    let r = checks::evaluate(&sc, &lean, Some(&twin), None, None);
    assert_eq!(
        status(&r, CheckId::Twin),
        Status::Pass,
        "{}",
        show(&r, &lean)
    );
    let mut wide = lean.clone();
    if let Some(last) = wide.frames.last_mut() {
        last.n[0].mot = 0;
        last.n[0].emot = 0;
    }
    wide.seal();
    let r = checks::evaluate(&sc, &wide, Some(&twin), None, None);
    assert!(
        fails_on(&r, CheckId::Twin, "bars", "w"),
        "{}",
        show(&r, &wide)
    );
}

// --- determinism

#[test]
fn determinism_compares_digests_and_names_the_first_tick_apart() {
    let sc = scenario("");
    let a = bug_24(true);
    let r = checks::evaluate(&sc, &a, None, Some(&a.clone()), None);
    assert_eq!(status(&r, CheckId::Determinism), Status::Pass);
    let mut b = a.clone();
    b.frames[5].n[0].visual[0] += 0.25;
    b.seal();
    assert_ne!(a.digest, b.digest);
    let r = checks::evaluate(&sc, &a, None, Some(&b), None);
    assert_eq!(status(&r, CheckId::Determinism), Status::Fail);
    assert_eq!(r.failures(CheckId::Determinism)[0].k, Some(5));
    // One run: said, not passed.
    assert!(matches!(
        status(&check(&sc, &a), CheckId::Determinism),
        Status::Skip(_)
    ));
}

// --- budget

#[test]
fn budget_holds_each_frame_to_its_limits() {
    let t = bug_24(true);
    let sc = scenario("[budget]\nitems = 4\ndamage_px = 10000");
    assert_eq!(status(&check(&sc, &t), CheckId::Budget), Status::Pass);
    let sc = scenario("[budget]\ndamage_px = 1000");
    let r = check(&sc, &t);
    let f = r.failures(CheckId::Budget);
    assert_eq!(f.len(), 1, "{}", show(&r, &t));
    assert_eq!((f[0].k, f[0].axis.as_deref()), (Some(0), Some("damage_px")));
    assert!(matches!(
        status(&check(&scenario(""), &t), CheckId::Budget),
        Status::Skip(_)
    ));
}

// --- reference

#[test]
fn a_run_matching_a_bad_reference_fails_and_one_leaving_a_good_one_is_changed() {
    let sc = scenario("");
    let bad = bug_24(false);
    let good = bug_24(true);
    let reference = |t: &RunTrace, verdict: &str, by_path: bool| Reference {
        verdict: verdict.into(),
        note: "the bars drop".into(),
        rev: "abc123".into(),
        date: "2026-10-17".into(),
        digest: if by_path {
            String::new()
        } else {
            t.digest.clone()
        },
        path: if by_path {
            checks::reference::paths(t)
        } else {
            BTreeMap::new()
        },
    };
    let rf = reference(&bad, "bad", false);
    let r = checks::evaluate(&sc, &bad, None, None, Some(&rf));
    assert_eq!(status(&r, CheckId::Reference), Status::Fail);
    // By path, within tol.
    let rf = reference(&bad, "bad", true);
    let r = checks::evaluate(&sc, &bad, None, None, Some(&rf));
    assert_eq!(status(&r, CheckId::Reference), Status::Fail);
    let r = checks::evaluate(&sc, &good, None, None, Some(&rf));
    assert_eq!(status(&r, CheckId::Reference), Status::Pass);
    // Good: matching passes; differing is CHANGED, which never gates.
    let rf = reference(&good, "good", true);
    assert_eq!(
        status(
            &checks::evaluate(&sc, &good, None, None, Some(&rf)),
            CheckId::Reference
        ),
        Status::Pass
    );
    let mut moved = good.clone();
    moved.frames[3].n[0].visual[0] += 4.0;
    moved.seal();
    let r = checks::evaluate(&sc, &moved, None, None, Some(&rf));
    assert_eq!(status(&r, CheckId::Reference), Status::Changed);
    assert!(r.ok());
}

// --- expect

fn with_expect(mut t: RunTrace, what: &str, key: &str, axis: Option<&str>, k: u32) -> RunTrace {
    t.expects.push(ExpectRec {
        k,
        line: 9,
        text: format!("expect {what} #{key}"),
        what: what.into(),
        keys: vec![key.into()],
        axis: axis.map(String::from),
        from: 0,
        segment: Some(1),
    });
    t
}

#[test]
fn expect_steps_hold_over_their_segment() {
    let sc = scenario("");
    let t = bug_24(false);
    // The bars' y: still on the fix, not on the bug.
    let r = check(
        &sc,
        &with_expect(bug_24(true), "still", "bars", Some("y"), 12),
    );
    assert_eq!(
        status(&r, CheckId::Expect),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let bad = with_expect(t.clone(), "still", "bars", Some("y"), 12);
    let r = check(&sc, &bad);
    let f = r.failures(CheckId::Expect);
    assert_eq!(f.len(), 1, "{}", show(&r, &bad));
    assert!(
        f[0].msg
            .contains("line 9 `expect still #bars`: it moved on y"),
        "{:?}",
        f[0]
    );
    assert_eq!(f[0].k, Some(1));
    // They move on x; they do not on h.
    let r = check(&sc, &with_expect(t.clone(), "moves", "bars", Some("x"), 12));
    assert_eq!(status(&r, CheckId::Expect), Status::Pass);
    let r = check(&sc, &with_expect(t.clone(), "moves", "bars", Some("h"), 12));
    assert_eq!(status(&r, CheckId::Expect), Status::Fail);
    // Shown at the step, or not.
    let r = check(&sc, &with_expect(t.clone(), "visible", "bars", None, 12));
    assert_eq!(status(&r, CheckId::Expect), Status::Pass);
    let r = check(&sc, &with_expect(t.clone(), "hidden", "bars", None, 12));
    assert_eq!(status(&r, CheckId::Expect), Status::Fail);
    // None written: skipped, said.
    assert!(matches!(
        status(&check(&sc, &t), CheckId::Expect),
        Status::Skip(_)
    ));
}

// --- expect_fail, [allow] and intend

#[test]
fn expect_fail_turns_a_failure_into_xfail_and_a_pass_into_xpass() {
    let sc = scenario("expect_fail = { bystander = \"bug 24 until the fix, issue 24\" }");
    let r = check(&sc, &bug_24(false));
    assert!(matches!(status(&r, CheckId::Bystander), Status::Xfail(_)));
    assert_eq!(status(&r, CheckId::Envelope), Status::Fail);
    let r = check(&sc, &bug_24(true));
    assert!(matches!(status(&r, CheckId::Bystander), Status::Xpass(_)));
    assert!(!r.ok(), "an XPASS fails the gate");
    assert!(show(&r, &bug_24(true)).contains("XPASS: now passes, remove expect_fail"));
}

#[test]
fn an_allow_entry_exempts_its_node_and_subtree_and_must_name_one() {
    let t = bug_25(false);
    let sc = scenario(
        "[allow]\nbystander = [{ node = \"#bars\", why = \"synthetic\" }, { node = \"#last\", why = \"synthetic\" }]",
    );
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Bystander),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let sc = scenario("[allow]\nbystander = [{ node = \"#nothing\", why = \"stale\" }]");
    let r = check(&sc, &bug_25(true));
    let f = r.failures(CheckId::Bystander);
    assert_eq!(f.len(), 1);
    assert!(f[0].msg.contains("#nothing names no node"), "{:?}", f[0]);
}

#[test]
fn an_intend_exempts_its_axes_in_its_segment() {
    let sc = scenario("");
    let mut t = bug_24(false);
    t.segments[1].intends.push(IntendRec {
        line: 4,
        what: "moves".into(),
        keys: vec!["bars".into()],
        axes: Axes::Y.0,
    });
    t.seal();
    let r = check(&sc, &t);
    assert!(
        !fails_on(&r, CheckId::Bystander, "bars", "y"),
        "{}",
        show(&r, &t)
    );
    // x is not intended: the envelope still fails.
    assert!(
        fails_on(&r, CheckId::Envelope, "bars", "x"),
        "{}",
        show(&r, &t)
    );
}

#[test]
fn a_report_round_trips_through_json() {
    let sc = scenario("");
    let r = check(&sc, &bug_24(false));
    let back: Report = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(back, r);
}

// --- what scenarios on the product showed (docs/decisions.md eval-sandbox)

/// The box after an exiting one (`exit` mirroring its `enter`): the
/// ghost holds its slot while it leaves and two boxes mount before it,
/// so its layout goes 58 → 102 for a few frames, then to 80 once the
/// ghost has left. It glides after its layout, past where it rests, but
/// never past where a frame laid it out.
fn ghost_slot(peak: f32) -> RunTrace {
    shape(14, |k| {
        let right = row("right", [58.0, 9.0, 20.0, 12.0]);
        match k {
            0 => vec![right],
            1..=5 => vec![
                right
                    .laid_at(102.0, 9.0)
                    .at(ease(58.0, peak, k, 5), 9.0)
                    .mot(Motions::GLIDE),
            ],
            6..=12 => vec![
                right
                    .laid_at(80.0, 9.0)
                    .at(ease(peak, 80.0, k - 5, 7), 9.0)
                    .mot(Motions::GLIDE),
            ],
            _ => vec![right.laid_at(80.0, 9.0).at(80.0, 9.0)],
        }
    })
}

#[test]
fn layout_that_moves_and_comes_back_holds_a_node_to_the_layouts_drawn() {
    let sc = scenario("");
    let t = ghost_slot(97.0);
    let r = check(&sc, &t);
    for c in [CheckId::Bystander, CheckId::Envelope] {
        assert_eq!(status(&r, c), Status::Pass, "{c:?}: {}", show(&r, &t));
    }
    // Past the furthest layout drawn (102, give 15.4) it is not.
    let t = ghost_slot(125.0);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Envelope, "right", "x"),
        "{}",
        show(&r, &t)
    );
}

/// A layout a frame laid out and threw away (the estimate's pass before
/// the shaped text's) is not one drawn: the bars' laid `y` is 9 on every
/// frame, so a drop is still a bystander's (bug 24 in one frame).
#[test]
fn a_layout_no_frame_drew_does_not_widen_the_way() {
    let sc = scenario("");
    let t = shape(6, |k| {
        let bars = row("bars", [225.0, 9.0, 15.0, 12.0]);
        match k {
            0 => vec![bars],
            1 => vec![bars.at(225.0, 2.0).mot(Motions::GLIDE)],
            _ => vec![bars],
        }
    });
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Bystander, "bars", "y"),
        "{}",
        show(&r, &t)
    );
}

/// A card whose written `x:` offset springs from 280 to 50 (owner-testing
/// bug 17's swell): laid at the same place throughout, drawn elsewhere at
/// rest. Authored while it springs, at rest after: a mover, not a
/// bystander drawn off its level.
#[test]
fn a_node_whose_written_offset_changed_is_a_mover() {
    let sc = scenario("");
    let t = shape(12, |k| {
        let card = row("card", [280.0, 0.0, 420.0, 32.0]);
        match k {
            0 => vec![card],
            1..=8 => vec![
                card.at(ease(280.0, 50.0, k, 8), 0.0)
                    .mot(Motions::PROP_SPRING)
                    .auth(Axes::X),
            ],
            _ => vec![card.at(50.0, 0.0)],
        }
    });
    let r = check(&sc, &t);
    for c in [CheckId::Bystander, CheckId::Envelope] {
        assert_eq!(status(&r, c), Status::Pass, "{c:?}: {}", show(&r, &t));
    }
}

/// Three segments of one tick each (motion reduced: each change snaps):
/// a node unmounted by one step and mounted again by the next is not a
/// blink, and the one shown between them is not a flash.
#[test]
fn gone_and_back_across_steps_is_not_a_blink() {
    let sc = scenario("");
    let mut t = shape(2, |k| match k {
        1 => vec![row("focus", [246.0, 8.0, 14.0, 14.0])],
        _ => vec![row("count", [246.0, 8.0, 14.0, 15.0])],
    });
    t.segments[1].e = 1;
    t.segments.push(Segment {
        name: "back".into(),
        line: 4,
        s: Some(1),
        e: 2,
        settled: true,
        hz: 60,
        ..Segment::default()
    });
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    // In one segment the same frames blink.
    let mut one = t.clone();
    one.segments.truncate(2);
    one.segments[1].e = 2;
    one.seal();
    let r = check(&sc, &one);
    let f = r.failures(CheckId::Vanish);
    assert!(
        f.iter().any(|f| f.msg.starts_with("vanish"))
            && f.iter().any(|f| f.msg.starts_with("flash")),
        "{}",
        show(&r, &one)
    );
}

/// A popup's first frame shows its content at its `enter` pose's
/// `opacity: 0`, which draws nothing: not a blank frame. The same frame
/// with nothing posed is.
#[test]
fn a_frame_drawing_only_a_pose_at_opacity_0_is_not_blank() {
    let sc = scenario("");
    let posed = |m: Motions| {
        let mut t = shape(3, |k| {
            let card = row("card", [0.0, 0.0, 120.0, 40.0]);
            if k == 1 {
                let mut c = card.op(0.0).mot(m);
                c.hidden = Some("opacity0".into());
                vec![c]
            } else {
                vec![card]
            }
        });
        t.frames[1].cost.get_mut(&1).unwrap().items = 0;
        t.seal();
        t
    };
    let t = posed(Motions::ENTER);
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("blank")),
        "{}",
        show(&r, &t)
    );
    let t = posed(Motions(0));
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("blank") && f.k == Some(1)),
        "{}",
        show(&r, &t)
    );
}

/// A surface detached with no frame after it (a popup closed by its
/// grace once its content had left) shows nothing from the tick it went.
#[test]
fn a_detached_surface_shows_nothing_after_it_goes() {
    let mut t = shape(2, |_| vec![row("bar", [0.0, 0.0, 300.0, 30.0])]);
    let mut card = row("card", [0.0, 0.0, 120.0, 40.0]);
    card.surface = 2;
    t.frames[2].n.push(card);
    t.frames[2].painted.push(2);
    t.frames[2].sizes.insert(2, [120.0, 40.0]);
    t.surfaces = vec![
        strand_eval::trace::SurfaceInfo {
            id: 1,
            ..Default::default()
        },
        strand_eval::trace::SurfaceInfo {
            id: 2,
            attached: 2,
            detached: Some(5),
            ..Default::default()
        },
    ];
    let tl = checks::Timeline::new(&t);
    let at = |k: u32| tl.at(k).unwrap();
    assert!(tl.row(at(4), "card").is_some());
    assert!(tl.row(at(5), "card").is_none());
    assert!(tl.row(at(9), "card").is_none());
    assert!(tl.row(at(9), "bar").is_some());
}

// --- cpu-gpu

/// A one-surface image of `w` × `h` filled with `px` (straight RGBA),
/// with `dot` at `(x, y)`.
fn image(
    w: u32,
    h: u32,
    px: [u8; 4],
    dot: Option<(u32, u32, [u8; 4])>,
) -> strand_eval::harness::Image {
    let mut rgba: Vec<u8> = (0..w * h).flat_map(|_| px).collect();
    if let Some((x, y, c)) = dot {
        let i = ((y * w + x) * 4) as usize;
        rgba[i..i + 4].copy_from_slice(&c);
    }
    strand_eval::harness::Image {
        width: w,
        height: h,
        png: strand_eval::report::png::encode(w, h, &rgba).unwrap(),
    }
}

/// Both legs' traces of a node gliding, every frame showing `frame.png`
/// (the CPU's image, or the GPU's).
fn legs() -> (RunTrace, RunTrace) {
    let t = shape(6, |k| {
        vec![row("a", [10.0 + k as f32, 10.0, 10.0, 10.0]).mot(Motions::GLIDE)]
    });
    let mut t = t;
    for f in &mut t.frames {
        f.png.insert(1, "frame.png".into());
    }
    (t.clone(), t)
}

fn images(img: strand_eval::harness::Image) -> BTreeMap<String, strand_eval::harness::Image> {
    BTreeMap::from([("frame.png".to_string(), img)])
}

#[test]
fn cpu_gpu_passes_legs_that_agree_within_the_gpu_tolerance() {
    let sc = scenario("backend = \"both\"");
    let (cpu, gpu) = legs();
    let cx = checks::Cx::new(&sc, &cpu);
    let a = images(image(20, 10, [200, 40, 40, 255], None));
    // Every channel 5 off (within GPU_TOLERANCE 6), one pixel far off
    // (an edge: within EDGE_SHARE).
    let b = images(image(
        20,
        10,
        [205, 45, 35, 255],
        Some((3, 3, [0, 0, 0, 255])),
    ));
    let f = checks::cpu_gpu::run(&cx, &gpu, &a, &b);
    assert!(f.is_empty(), "{f:?}");
}

#[test]
fn cpu_gpu_fails_a_node_the_gpu_draws_elsewhere_and_pixels_past_the_tolerance() {
    let sc = scenario("backend = \"both\"");
    let (cpu, mut gpu) = legs();
    gpu.frames[3].n[0].visual[1] += 2.0;
    let cx = checks::Cx::new(&sc, &cpu);
    let a = images(image(20, 10, [200, 40, 40, 255], None));
    let f = checks::cpu_gpu::run(&cx, &gpu, &a, &a);
    assert!(
        f.iter().any(|f| f.key.as_deref() == Some("a")
            && f.axis.as_deref() == Some("y")
            && f.k == Some(3)),
        "{f:?}"
    );
    // Pixels: every channel 7 off fails.
    let (cpu, gpu) = legs();
    let cx = checks::Cx::new(&sc, &cpu);
    let b = images(image(20, 10, [207, 47, 33, 255], None));
    let f = checks::cpu_gpu::run(&cx, &gpu, &a, &b);
    assert!(
        f.iter().any(|f| f.axis.as_deref() == Some("pixels")),
        "{f:?}"
    );
    // A frame painted on one leg only.
    let (cpu, mut gpu) = legs();
    gpu.frames.remove(2);
    let cx = checks::Cx::new(&sc, &cpu);
    let f = checks::cpu_gpu::run(&cx, &gpu, &a, &a);
    assert!(
        f.iter()
            .any(|f| f.msg.contains("not on the GPU") && f.k == Some(2)),
        "{f:?}"
    );
}

#[test]
fn cpu_gpu_masks_an_allowed_node_out_of_both_comparisons() {
    let sc = scenario(
        "backend = \"both\"\n[allow]\ncpu-gpu = [{ node = \"#a\", why = \"a dithered gradient\" }]",
    );
    let (cpu, mut gpu) = legs();
    gpu.frames[3].n[0].visual[1] += 2.0;
    let cx = checks::Cx::new(&sc, &cpu);
    // Far off only inside #a's box (x from 10 + k, 10 wide; y 10 to
    // 20) on every frame: (19, 15). Unmasked, one pixel of 40 × 30 is
    // within EDGE_SHARE, so the box is made of them.
    let a = images(image(40, 30, [200, 40, 40, 255], None));
    let mut rgba: Vec<u8> = (0..40 * 30).flat_map(|_| [200u8, 40, 40, 255]).collect();
    for y in 10..20 {
        for x in 16..20 {
            let i = (y * 40 + x) * 4;
            rgba[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
        }
    }
    let b = images(strand_eval::harness::Image {
        width: 40,
        height: 30,
        png: strand_eval::report::png::encode(40, 30, &rgba).unwrap(),
    });
    let unmasked = checks::cpu_gpu::run(
        &checks::Cx::new(&scenario("backend = \"both\""), &cpu),
        &cpu,
        &a,
        &b,
    );
    assert!(
        unmasked.iter().any(|f| f.axis.as_deref() == Some("pixels")),
        "{unmasked:?}"
    );
    let f = checks::cpu_gpu::run(&cx, &gpu, &a, &b);
    assert!(f.is_empty(), "{f:?}");
}

#[test]
fn cpu_gpu_compares_the_regions_each_leg_reports() {
    let sc = scenario("backend = \"both\"");
    let (cpu, mut gpu) = legs();
    gpu.frames[4].regions.insert(
        1,
        strand_eval::trace::Regions {
            opaque: vec![[0, 0, 300, 30]],
            ..Default::default()
        },
    );
    let cx = checks::Cx::new(&sc, &cpu);
    let a = images(image(20, 10, [200, 40, 40, 255], None));
    let f = checks::cpu_gpu::run(&cx, &gpu, &a, &a);
    assert!(
        f.iter()
            .any(|f| f.msg.contains("regions") && f.k == Some(4)),
        "{f:?}"
    );
}

// --- regions

/// A hover popup (surface 1, root `pop`) holding a card `card` with a
/// label `label`, drawn at `card_x`; the surface reports `input` as its
/// region.
fn island(card_x: impl Fn(u32) -> f32, input: impl Fn(u32) -> Option<Vec<[f32; 4]>>) -> RunTrace {
    let mut t = shape(5, |k| {
        let x = card_x(k);
        vec![
            row("pop", [0.0, 0.0, 300.0, 30.0]),
            row("card", [x, 5.0, 60.0, 20.0]).mot(Motions::GLIDE),
            row("label", [x + 5.0, 8.0, 40.0, 14.0]),
        ]
    });
    t.surfaces.push(strand_eval::trace::SurfaceInfo {
        id: 1,
        label: "popup #pop".into(),
        kind: "popup".into(),
        root: "pop".into(),
        attached: 0,
        detached: None,
    });
    t.nodes.insert("card".into(), meta("card", Some("pop")));
    let mut label = meta("label", Some("card"));
    label.kind = "text".into();
    t.nodes.insert("label".into(), label);
    for f in &mut t.frames {
        f.regions.insert(
            1,
            strand_eval::trace::Regions {
                input: input(f.k),
                ..Default::default()
            },
        );
    }
    t
}

#[test]
fn regions_pass_an_input_region_that_follows_the_drawn_card() {
    let sc = scenario("checks = [\"regions\"]");
    let x = |k: u32| 20.0 + 10.0 * k as f32;
    let t = island(x, |k| Some(vec![[x(k), 5.0, 60.0, 20.0]]));
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Regions),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    // A surface keeping the manager's region is not held to it.
    let t = island(x, |_| None);
    assert_eq!(status(&check(&sc, &t), CheckId::Regions), Status::Pass);
    // Opt-in: not run unless asked for.
    let t = island(x, |_| Some(vec![]));
    assert!(matches!(
        status(&check(&scenario(""), &t), CheckId::Regions),
        Status::Skip(_)
    ));
}

#[test]
fn regions_fail_a_region_left_where_the_card_was() {
    let sc = scenario("checks = [\"regions\"]");
    let x = |k: u32| 20.0 + 10.0 * k as f32;
    // The region lags two frames (20 px) behind the card: neither the
    // card nor its label is inside it.
    let t = island(x, |k| Some(vec![[x(k.saturating_sub(2)), 5.0, 60.0, 20.0]]));
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Regions),
        Status::Fail,
        "{}",
        show(&r, &t)
    );
    let f = r.failures(CheckId::Regions);
    assert!(
        f.iter()
            .any(|f| f.msg.starts_with("covered") && f.key.as_deref() == Some("card")),
        "{f:?}"
    );
    assert!(f.iter().any(|f| f.msg.starts_with("follows")), "{f:?}");
}

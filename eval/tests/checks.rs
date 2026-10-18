//! Every check against a synthetic trace it passes and one it fails
//! (docs/eval.md, "Checks"), the shapes of owner-testing.md bugs 24 and
//! 25 among them, and the motion a correct shell has that no check may
//! flag: an authored enter pose, a glide's overshoot, reduced motion's
//! snaps.

use std::collections::BTreeMap;

use strand_eval::checks::{self, CheckId, Report, Sev, Status};
use strand_eval::scenario::{Scenario, eval_root};
use strand_eval::trace::{
    Cost, EventRec, ExpectRec, Frame, IntendRec, NodeMeta, Pose, PoseRec, RestRec, Row, RunTrace,
    ScenarioInfo, Segment, SurfaceInfo,
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

/// A node carried by a gliding ancestor that also sits at a written
/// `x: 100`: what it has left to go is the glide in flight, not the
/// ancestor's resting offset, and a jump of 70 px in one tick fails
/// (0.75 of the 104 px inherited would have let it through).
#[test]
fn a_static_ancestor_offset_is_not_glide_left_to_go() {
    let sc = scenario("");
    let carried = |x: f32, flight: f32| {
        let mut r = row("a", [0.0, 0.0, 20.0, 20.0])
            .laid_at(100.0, 0.0)
            .at(x, 0.0);
        r.emot = Motions::GLIDE.0;
        r.inherited = [100.0 + flight, 0.0];
        r.flight = [flight, 0.0];
        r
    };
    let t = shape(6, |k| match k {
        0 => vec![
            row("a", [0.0, 0.0, 20.0, 20.0])
                .laid_at(100.0, 0.0)
                .at(100.0, 0.0),
        ],
        1 => vec![carried(104.0, 4.0)],
        2 => vec![carried(34.0, 2.0)],
        3 => vec![carried(66.0, 1.0)],
        4 => vec![carried(98.0, 0.5)],
        _ => vec![
            row("a", [0.0, 0.0, 20.0, 20.0])
                .laid_at(100.0, 0.0)
                .at(100.0, 0.0),
        ],
    });
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
    assert_eq!(r.failures(CheckId::Teleport)[0].k, Some(2));
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

/// (eval-coverage) A node that enters in the segment travels from where it
/// first shows: an enter pose sliding 89 px in on a spring, 46 px in its
/// largest step, passes as the same slide of a node already there does;
/// one that shows at its pose and jumps the whole way home in one tick
/// still fails.
#[test]
fn an_enter_pose_slide_is_held_as_a_slide_of_a_node_already_there() {
    let sc = scenario("");
    let slide = |xs: [f32; 4]| {
        shape(8, move |k| {
            let anchor = row("anchor", [0.0, 0.0, 10.0, 10.0]);
            let a = row("a", [8.0, 4.0, 220.0, 20.0]);
            let m = Motions(Motions::PROP_SPRING.0 | Motions::ENTER.0);
            match k {
                0 => vec![anchor],
                1..=4 => vec![anchor, a.at(xs[k as usize - 1], 4.0).mot(m).auth(Axes::X)],
                _ => vec![anchor, a],
            }
        })
    };
    let t = slide([97.0, 51.0, 21.0, 8.0]);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = slide([97.0, 97.0, 8.0, 8.0]);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
}

/// (eval-coverage) A node gliding across while a sibling's size spring
/// lays it out anew down each frame follows its layout down, as the
/// spring's other neighbours do: no glide of its own was retargeted on y.
/// The same follow with a glide in flight on y is a retarget.
#[test]
fn a_glide_across_under_a_size_spring_follows_its_layout_down() {
    let sc = scenario("");
    let go = |gy: f32| {
        shape(8, move |k| {
            let banner = row("banner", [0.0, 0.0, 120.0, 2.0]);
            let a = row("a", [133.0, 4.0, 50.0, 10.0]);
            let ys = [4.0f32, 7.0, 12.0, 15.0, 16.0];
            let xs = [133.0f32, 130.0, 127.0, 124.0, 123.0];
            let i = (k as usize).min(4);
            let mut b = banner;
            b.drawn[3] = 2.0 + (ys[i] - 4.0);
            b.visual[3] = b.drawn[3];
            b.laid[3] = b.drawn[3];
            if k == 0 {
                return vec![b, a];
            }
            let b = b.mot(Motions::SIZE_SPRING).auth(Axes::H);
            let mut a = a.laid_at(123.0, ys[i]).at(xs[i], ys[i]);
            if k < 4 {
                a = a.mot(Motions::GLIDE);
                a.glide = [xs[i] - 123.0, if k < 3 { gy } else { 0.0 }];
            }
            vec![b, a]
        })
    };
    let t = go(0.0);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = go(4.0);
    let r = check(&sc, &t);
    let f = r.failures(CheckId::Teleport);
    assert!(
        f.iter()
            .any(|f| f.msg.starts_with("retarget") && f.axis.as_deref() == Some("y")),
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
    use strand_eval::recording::{Recording, encode};
    use strand_eval::refs::{Block, Meta, Refs, Verdict};
    let sc = scenario("");
    let bad = bug_24(false);
    let good = bug_24(true);
    let block = |t: &RunTrace, verdict: Verdict| {
        let meta = Meta {
            verdict,
            note: "the bars drop",
            date: "2026-10-17",
            rev: "abc123",
            approver: "test",
            scenario: "",
        };
        Block::new(&sc.id, &meta, encode(&Recording::from_trace(t)))
    };
    let refs = |good: Option<Block>, bad: Vec<Block>| Refs {
        good,
        bad,
        ..Refs::default()
    };
    // Bad: the run that reproduces it fails; another does not.
    let rf = refs(None, vec![block(&bad, Verdict::Bad)]);
    let r = checks::evaluate(&sc, &bad, None, None, Some(&rf));
    assert_eq!(status(&r, CheckId::Reference), Status::Fail);
    let r = checks::evaluate(&sc, &good, None, None, Some(&rf));
    assert!(
        matches!(status(&r, CheckId::Reference), Status::Skip(_)),
        "no approved reference, none required"
    );
    // Good: matching passes; differing is CHANGED, which gates.
    let rf = refs(Some(block(&good, Verdict::Good)), Vec::new());
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
    assert!(!r.ok(), "CHANGED gates");
    assert!(r.only_changed());
    let line = r.changed_line().unwrap();
    assert!(
        line.starts_with(&format!("CHANGED: {}: k{} ", sc.id, moved.frames[3].k)),
        "{line}"
    );
    assert!(line.contains("visual.x"), "{line}");
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
        // The ghost holding its slot while it exits.
        let mut ghost = row("ghost", [80.0, 9.0, 20.0, 12.0]).mot(Motions::EXIT);
        ghost.hidden = Some("inert".into());
        match k {
            0 => vec![right],
            1..=5 => vec![
                right
                    .laid_at(102.0, 9.0)
                    .at(ease(58.0, peak, k, 5), 9.0)
                    .mot(Motions::GLIDE),
                ghost,
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

/// The same excursion with no ghost on the surface, and no change from
/// logic or input when it comes back: a layout drawn wrong. It does not
/// widen the way, and the glide toward it is outside it.
#[test]
fn an_excursion_with_no_reason_does_not_widen_the_way() {
    let sc = scenario("");
    let mut t = ghost_slot(97.0);
    for f in &mut t.frames {
        f.n.retain(|r| r.key != "ghost");
    }
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Envelope, "right", "x"),
        "{}",
        show(&r, &t)
    );
    // Logic's diff (or input) at the frame it came back on is a reason
    // (a second step of the segment took it back).
    let back = t.frames.iter().position(|f| f.k == 6).unwrap();
    t.frames[back].driven = true;
    t.digest = t.compute_digest();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Envelope),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// A node laid out one frame 9 px up and back, its drawn box with it,
/// nothing on its surface to lay it out anew: a bystander drawn off its
/// level (a wrong layout does not make it a mover). A sibling text drawn
/// by its estimate on that frame is a reason: then it is a mover held to
/// the layouts drawn.
#[test]
fn a_wrong_layout_drawn_does_not_exempt_a_bystander() {
    let sc = scenario("");
    let bars = |text: Option<&str>| {
        shape(6, move |k| {
            let b = row("bars", [225.0, 9.0, 15.0, 12.0]);
            let mut label = row("label", [100.0, 8.0, 40.0, 14.0]);
            label.text = Some(if k == 1 { text.unwrap_or("S") } else { "S" }.into());
            match k {
                1 => vec![b.laid_at(225.0, 0.0).at(225.0, 0.0), label],
                _ => vec![b, label],
            }
        })
    };
    let t = bars(None);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Bystander, "bars", "y"),
        "{}",
        show(&r, &t)
    );
    let f = &r.failures(CheckId::Bystander)[0];
    assert!(f.msg.contains("laid out elsewhere and back"), "{f:?}");
    let t = bars(Some("E"));
    let r = check(&sc, &t);
    assert!(
        !fails_on(&r, CheckId::Bystander, "bars", "y"),
        "{}",
        show(&r, &t)
    );
    assert_eq!(
        status(&r, CheckId::Envelope),
        Status::Pass,
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
/// `opacity: 0`, and its last, as its content's exit ends, at the exit's:
/// it shows nothing and draws nothing, which is not a blank frame. The
/// same frame drawing nothing while what it holds shows is, and the
/// surface's root showing counts for nothing (the trace cannot tell
/// whether the surface itself paints).
#[test]
fn a_frame_drawing_nothing_is_blank_only_where_something_inside_shows() {
    let sc = scenario("");
    let posed = |m: Motions, card_shows: bool| {
        let mut t = shape(3, |k| {
            let root = row("root", [0.0, 0.0, 300.0, 30.0]);
            let card = row("card", [0.0, 0.0, 120.0, 30.0]);
            if k == 1 && !card_shows {
                let mut c = card.op(0.0).mot(m);
                c.hidden = Some("opacity0".into());
                vec![root, c]
            } else {
                vec![root, card]
            }
        });
        t.surfaces = vec![SurfaceInfo {
            id: 1,
            root: "root".into(),
            ..Default::default()
        }];
        t.nodes.get_mut("card").unwrap().parent = Some("root".into());
        t.frames[1].cost.get_mut(&1).unwrap().items = 0;
        t.seal();
        t
    };
    for m in [Motions::ENTER, Motions::EXIT, Motions(0)] {
        let t = posed(m, false);
        let r = check(&sc, &t);
        assert!(
            !r.failures(CheckId::Vanish)
                .iter()
                .any(|f| f.msg.starts_with("blank")),
            "{}",
            show(&r, &t)
        );
    }
    let t = posed(Motions(0), true);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("blank on surface 1") && f.k == Some(1)),
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
        SurfaceInfo {
            id: 1,
            ..Default::default()
        },
        SurfaceInfo {
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
    t.surfaces.push(SurfaceInfo {
        id: 1,
        label: "popup #pop".into(),
        kind: "popup".into(),
        root: "pop".into(),
        attached: 0,
        detached: None,
        ..SurfaceInfo::default()
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

// --- coherence

/// `t` with each `(child, parent)` pair's parent set.
fn with_parents(mut t: RunTrace, pairs: &[(&str, &str)]) -> RunTrace {
    for (c, p) in pairs {
        t.nodes.get_mut(*c).unwrap().parent = Some((*p).into());
    }
    t.seal();
    t
}

fn coherence_fails(r: &Report, key: &str, rule: &str) -> bool {
    r.failures(CheckId::Coherence)
        .iter()
        .any(|f| f.key.as_deref() == Some(key) && f.msg.starts_with(rule))
}

/// A parent gliding 12 px back home; its child carried with it, or left
/// at its laid place (the glide drawn on the parent alone).
fn gliding_parent(carried: bool) -> RunTrace {
    let t = shape(4, move |k| {
        let g = if k == 0 { 0.0 } else { 12.0 - 3.0 * k as f32 };
        let mut p = row("p", [20.0, 0.0, 60.0, 30.0])
            .at(20.0 + g, 0.0)
            .mot(Motions::GLIDE);
        p.glide = [g, 0.0];
        let c = row("c", [24.0, 4.0, 10.0, 10.0]);
        let c = if carried { c.at(24.0 + g, 4.0) } else { c };
        vec![p, c]
    });
    with_parents(t, &[("c", "p")])
}

#[test]
fn inherit_holds_a_child_to_its_parents_paint_offset() {
    let sc = scenario("");
    let t = gliding_parent(true);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Coherence),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = gliding_parent(false);
    let r = check(&sc, &t);
    assert!(coherence_fails(&r, "c", "inherit"), "{}", show(&r, &t));
    assert!(!coherence_fails(&r, "p", "inherit"), "{}", show(&r, &t));
    // Its own offsets are its own: a child springing 5 px on top of what
    // it is carried by is coherent.
    let t = with_parents(
        shape(4, |k| {
            let g = if k == 0 { 0.0 } else { 12.0 - 3.0 * k as f32 };
            let mut p = row("p", [20.0, 0.0, 60.0, 30.0])
                .at(20.0 + g, 0.0)
                .mot(Motions::GLIDE);
            p.glide = [g, 0.0];
            let c = row("c", [24.0, 4.0, 10.0, 10.0])
                .at(24.0 + g + 5.0, 4.0)
                .spring(5.0, 0.0)
                .mot(Motions::PROP_SPRING);
            vec![p, c]
        }),
        &[("c", "p")],
    );
    let r = check(&sc, &t);
    assert!(!coherence_fails(&r, "c", "inherit"), "{}", show(&r, &t));
}

/// A parent at half scale with a child; each turned about its box's
/// centre, or about the parent's top-left.
fn scaled(about_centre: bool) -> RunTrace {
    let t = shape(2, move |_| {
        let mut p = row("p", [0.0, 0.0, 100.0, 40.0]);
        p.scale = [0.5, 0.5];
        p.visual = if about_centre {
            [25.0, 10.0, 50.0, 20.0]
        } else {
            [0.0, 0.0, 50.0, 20.0]
        };
        let mut c = row("c", [10.0, 10.0, 20.0, 20.0]);
        c.visual = if about_centre {
            [30.0, 15.0, 10.0, 10.0]
        } else {
            [5.0, 5.0, 10.0, 10.0]
        };
        vec![p, c]
    });
    with_parents(t, &[("c", "p")])
}

#[test]
fn pivot_turns_a_node_and_its_subtree_about_the_box_centre() {
    let sc = scenario("");
    let t = scaled(true);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Coherence),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = scaled(false);
    let r = check(&sc, &t);
    assert!(coherence_fails(&r, "p", "pivot"), "{}", show(&r, &t));
    assert!(coherence_fails(&r, "c", "pivot"), "{}", show(&r, &t));
}

/// `old` named `m` drawn at a 20 px square; `new` of the same name
/// mounted at a 40 px square 100 px right, its first frame drawn with
/// the morph's translate and scale taken from the old box's centre (as
/// anim/morph.rs does), or from its top-left.
fn morphing(from_centre: bool) -> RunTrace {
    let mut t = shape(4, move |k| {
        if k == 0 {
            return vec![row("old", [0.0, 0.0, 20.0, 20.0])];
        }
        let own = row("new", [100.0, 0.0, 40.0, 40.0]);
        if k > 1 {
            return vec![own];
        }
        let d = if from_centre {
            [-110.0, -10.0]
        } else {
            [-100.0, 0.0]
        };
        let mut r = own.at(100.0 + d[0], d[1]).mot(Motions::SHARED_MORPH);
        r.morph = d;
        r.scale = [0.5, 0.5];
        let c = [r.drawn[0] + 20.0, r.drawn[1] + 20.0];
        r.visual = [c[0] - 10.0, c[1] - 10.0, 20.0, 20.0];
        vec![r]
    });
    for k in ["old", "new"] {
        t.nodes.get_mut(k).unwrap().morph = Some("m".into());
    }
    t.seal();
    t
}

#[test]
fn a_shared_morph_starts_from_the_box_its_name_was_drawn_at() {
    let sc = scenario("");
    let t = morphing(true);
    let r = check(&sc, &t);
    assert!(
        !coherence_fails(&r, "new", "morph start"),
        "{}",
        show(&r, &t)
    );
    let t = morphing(false);
    let r = check(&sc, &t);
    assert!(
        coherence_fails(&r, "new", "morph start"),
        "{}",
        show(&r, &t)
    );
}

/// (shared-morph-on) A node with an `enter` pose that mounts morphing
/// plays the morph in place of its pose (anim/morph.rs): no pop-in. The
/// same first frame at rest without the morph skipped its pose.
#[test]
fn a_morph_in_place_of_an_enter_pose_is_no_pop_in() {
    let sc = scenario("");
    let posed = |t: &mut RunTrace| {
        t.nodes.get_mut("new").unwrap().enter = Some(Pose {
            opacity: Some(0.0),
            ..Pose::default()
        });
        t.seal();
    };
    let mut t = morphing(true);
    posed(&mut t);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let mut t = shape(4, |k| {
        let mut v = Vec::new();
        if k == 0 {
            v.push(row("old", [0.0, 0.0, 20.0, 20.0]));
        } else {
            v.push(row("new", [100.0, 0.0, 40.0, 40.0]));
        }
        v
    });
    posed(&mut t);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Fail,
        "{}",
        show(&r, &t)
    );
    assert!(
        r.failures(CheckId::Vanish)[0].msg.starts_with("pop-in"),
        "{}",
        show(&r, &t)
    );
}

/// Siblings `a` and `b` under `p`, overlapping; `a` springing 5 px over
/// `b` and drawn after it, until tick 2 draws it first: below `b`.
/// `swap` also swaps their laid places then (a reorder, FLIP).
fn stacked(pops: bool, swap: bool) -> RunTrace {
    let t = shape(4, move |k| {
        let p = row("p", [0.0, 0.0, 100.0, 30.0]);
        let (ax, bx) = if swap && k >= 2 {
            (20.0, 10.0)
        } else {
            (10.0, 20.0)
        };
        let s = if k == 0 { 0.0 } else { 5.0 };
        let a = row("a", [ax, 0.0, 20.0, 20.0])
            .at(ax + s, 0.0)
            .spring(s, 0.0)
            .mot(Motions::PROP_SPRING);
        let b = row("b", [bx, 0.0, 20.0, 20.0]);
        if pops && k >= 2 {
            vec![p, a, b]
        } else {
            vec![p, b, a]
        }
    });
    with_parents(t, &[("a", "p"), ("b", "p")])
}

#[test]
fn a_node_in_flight_keeps_its_stacking_until_it_lands() {
    let sc = scenario("");
    let t = stacked(false, false);
    let r = check(&sc, &t);
    assert!(!coherence_fails(&r, "a", "stacking"), "{}", show(&r, &t));
    let t = stacked(true, false);
    let r = check(&sc, &t);
    assert!(coherence_fails(&r, "a", "stacking"), "{}", show(&r, &t));
    assert!(!coherence_fails(&r, "b", "stacking"), "{}", show(&r, &t));
    // A reorder that swaps their places swaps their stacking too.
    let t = stacked(true, true);
    let r = check(&sc, &t);
    assert!(!coherence_fails(&r, "a", "stacking"), "{}", show(&r, &t));
}

// --- ink

fn text_row(key: &str, w: f32, ink_w: f32) -> Row {
    let mut r = row(key, [0.0, 0.0, w, 15.0]);
    r.ink = Some([1.0, 2.0, ink_w, 11.0]);
    r.text = Some("I".into());
    r
}

#[test]
fn ink_spilling_past_its_box_fails() {
    let sc = scenario("");
    let t = shape(2, |_| vec![text_row("t", 50.0, 48.0)]);
    let r = check(&sc, &t);
    assert_eq!(status(&r, CheckId::Ink), Status::Pass, "{}", show(&r, &t));
    // A stand-in wider than its box drawn unclipped.
    let t = shape(2, |k| {
        vec![text_row("t", 50.0, if k == 2 { 70.0 } else { 48.0 })]
    });
    let r = check(&sc, &t);
    assert!(fails_on(&r, CheckId::Ink, "t", "w"), "{}", show(&r, &t));
    assert!(r.failures(CheckId::Ink)[0].msg.starts_with("spill"));
}

/// (eval-r3) A text drawn by the layout shaped for its own box may reach
/// past it by its font's own overhang (an icon glyph wider than its
/// advance: the layout's ink past the layout's box, `ov`), and no more:
/// the same layout drawn 6 px off where its box puts it spills.
#[test]
fn ink_gives_a_shaped_text_its_fonts_overhang_and_no_more() {
    let sc = scenario("");
    let shaped = |dx: f32| {
        let mut r = row("icon", [10.0, 0.0, 14.0, 15.0]);
        r.text = Some("S".into());
        // The glyph overhangs its 14 px advance by 4 px on the right.
        r.ink = Some([10.0 + dx, 2.0, 18.0, 11.0]);
        r.ov = Some([0.0, 4.0]);
        r
    };
    let t = shape(2, |_| vec![shaped(0.0)]);
    let r = check(&sc, &t);
    assert_eq!(status(&r, CheckId::Ink), Status::Pass, "{}", show(&r, &t));
    let t = shape(2, |k| vec![shaped(if k == 2 { 6.0 } else { 0.0 })]);
    let r = check(&sc, &t);
    assert!(fails_on(&r, CheckId::Ink, "icon", "w"), "{}", show(&r, &t));
    // Without the layout's own overhang known, the 4 px is a spill.
    let t = shape(2, |_| {
        let mut r = shaped(0.0);
        r.ov = None;
        vec![r]
    });
    let r = check(&sc, &t);
    assert!(fails_on(&r, CheckId::Ink, "icon", "w"), "{}", show(&r, &t));
}

/// An ellipsised text whose box widens 100 → 120 px: its ink `before`
/// px wide, then `after`.
fn widening(before: f32, after: f32) -> RunTrace {
    let mut t = shape(2, move |k| {
        let (w, ink) = if k < 2 {
            (100.0, before)
        } else {
            (120.0, after)
        };
        let mut r = text_row("t", w, ink);
        r.fit = Some(w);
        vec![r]
    });
    t.nodes.get_mut("t").unwrap().ellipsis = true;
    t.seal();
    t
}

#[test]
fn an_ellipsised_text_never_draws_less_of_itself_as_its_box_widens() {
    let sc = scenario("");
    // A word more fits: it draws more.
    let t = widening(80.0, 110.0);
    let r = check(&sc, &t);
    assert_eq!(status(&r, CheckId::Ink), Status::Pass, "{}", show(&r, &t));
    // The narrowest stand-in drawn in a wider box: less of it.
    let t = widening(80.0, 60.0);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Ink)
            .iter()
            .any(|f| f.msg.starts_with("over-cut") && f.k == Some(2)),
        "{}",
        show(&r, &t)
    );
    // Ink reaching its box's edge before may have been a wider layout
    // clipped there: a wider box need not draw it again.
    let t = widening(98.0, 90.0);
    let r = check(&sc, &t);
    assert_eq!(status(&r, CheckId::Ink), Status::Pass, "{}", show(&r, &t));
}

// --- teleport: an out-and-back way, a glide over a size snap

/// A width springing out toward 300 then, sent back at tick 4 (a second
/// step), to its start of 100, `jump` px at most a tick on its way out.
fn out_and_back(jump: f32) -> RunTrace {
    let ws = [
        100.0, 145.0, 190.0, 220.0, 235.0, 200.0, 160.0, 125.0, 105.0, 100.0,
    ];
    let mut t = shape(9, move |k| {
        let mut w = ws[k as usize];
        if k == 1 {
            w = 100.0 + jump;
        }
        let laid = if (1..4).contains(&k) { 300.0 } else { 100.0 };
        let mut r = row("pill", [0.0, 0.0, laid, 20.0]).mot(if k == 0 || k == 9 {
            Motions(0)
        } else {
            Motions::SIZE_SPRING
        });
        r.visual[2] = w;
        r.drawn[2] = w;
        vec![r]
    });
    t.frames[1].driven = true;
    t.frames[4].driven = true;
    t.seal();
    t
}

#[test]
fn an_out_and_back_way_travels_its_excursion() {
    let sc = scenario("");
    let t = out_and_back(45.0);
    let r = check(&sc, &t);
    assert!(
        !fails_on(&r, CheckId::Teleport, "pill", "w"),
        "{}",
        show(&r, &t)
    );
    // More than 0.75 of the whole excursion in one tick still jumps.
    let t = out_and_back(170.0);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "pill", "w"),
        "{}",
        show(&r, &t)
    );
}

/// A glide moves a box and never sizes it: a width layout snaps while
/// the box glides is not a glide retargeted from its layout.
#[test]
fn a_size_snap_under_a_glide_is_no_retarget() {
    let sc = scenario("");
    let t = shape(4, |k| {
        let g = if k == 0 { 0.0 } else { 8.0 - 2.0 * k as f32 };
        let w = if k >= 2 { 120.0 } else { 100.0 };
        let mut r = row("card", [10.0, 0.0, w, 20.0])
            .at(10.0 + g, 0.0)
            .mot(Motions::GLIDE);
        r.glide = [g, 0.0];
        vec![r]
    });
    let r = check(&sc, &t);
    assert!(
        !fails_on(&r, CheckId::Teleport, "card", "w"),
        "{}",
        show(&r, &t)
    );
}

// --- vanish: each surface a popup comes onto, the pose a closing one commits

/// `pill` with `enter { opacity: 0 }` in a popup opened at tick 1
/// (surface 2) and, after it closed, again at tick 7 (surface 3),
/// playing its pose the second time or not.
fn reopened(plays_again: bool) -> RunTrace {
    let mut t = shape(12, move |k| {
        let mut v = vec![row("bar", [0.0, 0.0, 300.0, 30.0])];
        let (s, from) = match k {
            1..=5 => (2, 1),
            7..=12 => (3, 7),
            _ => return v,
        };
        let i = k - from;
        let played = s == 2 || plays_again;
        let op = if played { ease(0.1, 1.0, i, 4) } else { 1.0 };
        let mut p = row("pill", [10.0, 0.0, 80.0, 20.0])
            .op(op)
            .mot(if played && i < 4 {
                Motions::ENTER | Motions::OPACITY_SPRING
            } else {
                Motions(0)
            })
            .auth(Axes::OPACITY);
        p.surface = s;
        v.push(p);
        v
    });
    for f in &mut t.frames {
        let s = match f.k {
            1..=5 => 2,
            7..=12 => 3,
            _ => continue,
        };
        f.painted.push(s);
        f.sizes.insert(s, [100.0, 20.0]);
    }
    t.surfaces = vec![
        SurfaceInfo {
            id: 1,
            ..Default::default()
        },
        SurfaceInfo {
            id: 2,
            attached: 1,
            detached: Some(6),
            ..Default::default()
        },
        SurfaceInfo {
            id: 3,
            attached: 7,
            ..Default::default()
        },
    ];
    t.nodes.get_mut("pill").unwrap().enter = Some(Pose {
        opacity: Some(0.0),
        ..Pose::default()
    });
    t.seal();
    t
}

#[test]
fn a_popup_opened_again_plays_its_enter_pose_again() {
    let sc = scenario("");
    let t = reopened(true);
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pop-in")),
        "{}",
        show(&r, &t)
    );
    let t = reopened(false);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pop-in") && f.k == Some(7)),
        "{}",
        show(&r, &t)
    );
}

/// A popup (surface 2, root `pop`) closing by its exit with the
/// compositor applying its pose: opacity 0.8 down to 0 over ticks 1-4,
/// and at tick 5, its last frame, `last` (none: rest).
fn closing_posed(last: Option<f32>) -> RunTrace {
    let mut t = shape(5, |k| {
        let mut p = row("pop", [0.0, 0.0, 100.0, 40.0]).mot(if k == 0 {
            Motions(0)
        } else {
            Motions::EXIT
        });
        p.surface = 2;
        vec![p]
    });
    let ops = [0.8, 0.5, 0.2, 0.0];
    for f in &mut t.frames {
        f.painted = vec![2];
        f.sizes.insert(2, [100.0, 40.0]);
        f.cost = BTreeMap::from([(
            2,
            Cost {
                paints: 1,
                items: 1,
                ..Cost::default()
            },
        )]);
        let op = match f.k {
            1..=4 => Some(ops[f.k as usize - 1]),
            5 => last,
            _ => None,
        };
        if let Some(op) = op {
            f.poses.insert(
                2,
                PoseRec {
                    opacity: op,
                    scale: 1.0,
                    x: 0.0,
                    y: 0.0,
                },
            );
        }
    }
    t.surfaces = vec![SurfaceInfo {
        id: 2,
        root: "pop".into(),
        detached: Some(6),
        ..Default::default()
    }];
    t.seal();
    t
}

#[test]
fn a_closing_surface_never_commits_a_pose_back_toward_rest() {
    let sc = scenario("");
    let t = closing_posed(Some(0.0));
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pose back")),
        "{}",
        show(&r, &t)
    );
    // Its last frame commits rest's pose: the compositor shows it whole.
    let t = closing_posed(None);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pose back") && f.k == Some(5)),
        "{}",
        show(&r, &t)
    );
}

// --- on screen: surfaces the compositor moves or poses

#[test]
fn rows_move_with_their_surface_and_take_its_pose() {
    // A popup re-centred 30 px left at tick 2 as it grows, its content
    // drawn 30 px right on it then: still on screen.
    let mut t = shape(3, |k| {
        let mut r = row("card", [0.0, 0.0, 100.0, 40.0]);
        if k >= 2 {
            r = r.at(30.0, 0.0);
        }
        r.surface = 2;
        vec![r]
    });
    for f in &mut t.frames {
        let x = if f.k >= 2 { 70.0 } else { 100.0 };
        f.origins.insert(2, [x, 36.0]);
    }
    t.seal();
    assert!(t.clone().on_screen().is_some());
    let s = t.on_screen().unwrap();
    assert!(s.on_screen);
    for f in &s.frames {
        assert_eq!(f.n[0].drawn[0], 0.0, "k{}", f.k);
        assert_eq!(f.n[0].visual[0], 0.0, "k{}", f.k);
    }
    // Nothing moved, nothing posed: the trace as it is.
    let still = shape(2, |_| vec![row("a", [0.0, 0.0, 10.0, 10.0])]);
    assert!(still.on_screen().is_none());
    // A pose's opacity multiplies in; its scale and offset map every
    // visual box (about the surface's top-left) and the root's scale.
    let mut t = shape(1, |_| {
        let mut root = row("root", [0.0, 0.0, 100.0, 40.0]);
        root.surface = 2;
        let mut c = row("c", [10.0, 10.0, 20.0, 20.0]);
        c.surface = 2;
        vec![root, c]
    });
    t.surfaces = vec![SurfaceInfo {
        id: 2,
        root: "root".into(),
        ..Default::default()
    }];
    t.nodes.get_mut("c").unwrap().parent = Some("root".into());
    for f in &mut t.frames {
        f.poses.insert(
            2,
            PoseRec {
                opacity: 0.5,
                scale: 0.9,
                x: 0.0,
                y: 3.0,
            },
        );
    }
    t.seal();
    let s = t.on_screen().unwrap();
    let f = &s.frames[1];
    assert_eq!(f.n[0].op, 0.5);
    assert_eq!(f.n[0].scale, [0.9, 0.9]);
    assert_eq!(f.n[1].visual, [9.0, 12.0, 18.0, 18.0]);
    assert!(Axes(f.n[1].auth).contains(Axes::OPACITY | Axes::SCALE));
    // The pose turns the subtree about the surface's top-left: coherent.
    let r = check(&scenario(""), &t);
    assert!(
        !r.failures(CheckId::Coherence)
            .iter()
            .any(|f| f.msg.starts_with("pivot")),
        "{}",
        show(&r, &t)
    );
}

// --- twin: time-driven nodes at rest

#[test]
fn twin_compares_a_time_driven_node_at_rest() {
    let sc = scenario("");
    let run = |x: f32| {
        shape(2, move |k| {
            vec![
                row("dot", [10.0, 0.0, 10.0, 10.0])
                    .at(if k == 0 { 10.0 } else { x }, 0.0)
                    .mot(Motions::TIME)
                    .auth(Axes::X),
            ]
        })
    };
    let mut twin = run(10.0);
    twin.scenario.reduced_motion = true;
    let t = run(10.0);
    let r = checks::evaluate(&sc, &t, Some(&twin), None, None);
    assert_eq!(status(&r, CheckId::Twin), Status::Pass, "{}", show(&r, &t));
    // Its clock stopped elsewhere than t = 0.
    let t = run(13.0);
    let r = checks::evaluate(&sc, &t, Some(&twin), None, None);
    assert!(fails_on(&r, CheckId::Twin, "dot", "x"), "{}", show(&r, &t));
}

/// Blank frames on two surfaces are two findings, each in its segment.
#[test]
fn blank_frames_are_told_apart_by_surface() {
    let sc = scenario("");
    let mut t = shape(3, |_| {
        let a = row("a", [10.0, 10.0, 10.0, 10.0]);
        let mut b = row("b", [0.0, 0.0, 10.0, 10.0]);
        b.surface = 2;
        vec![a, b]
    });
    for f in &mut t.frames {
        f.painted.push(2);
        f.sizes.insert(2, [50.0, 20.0]);
        let mut c = f.cost[&1];
        if f.k == 2 {
            c.items = 0;
            f.cost.get_mut(&1).unwrap().items = 0;
        }
        f.cost.insert(2, c);
    }
    t.seal();
    let r = check(&sc, &t);
    let blanks: Vec<_> = r
        .failures(CheckId::Vanish)
        .into_iter()
        .filter(|f| f.msg.starts_with("blank"))
        .collect();
    assert_eq!(blanks.len(), 2, "{}", show(&r, &t));
    assert!(
        blanks
            .iter()
            .all(|f| f.segment.as_deref() == Some("change"))
    );
}

// --- (eval-rules) colour, contain, tiling, stagger, pop-out

/// `c` (straight-alpha channels in 0..=1) as the recording writes it.
fn hex(c: [f64; 4]) -> String {
    c.iter()
        .map(|v| format!("{:02x}", (v.clamp(0.0, 1.0) * 255.0).round() as u8))
        .collect()
}

/// The colour `s` of the way from `a` to `b` in premultiplied OKLab, as
/// a spring draws it.
fn on_way(a: [f64; 4], b: [f64; 4], s: f64) -> [f64; 4] {
    use strand_eval::checks::colour::{premultiplied, srgb};
    let (p, q) = (premultiplied(a), premultiplied(b));
    let m: [f64; 4] = std::array::from_fn(|i| p[i] + s * (q[i] - p[i]));
    if m[3] <= 0.0 {
        return [0.0; 4];
    }
    let [r, g, bl] = srgb([m[0] / m[3], m[1] / m[3], m[2] / m[3]]);
    [r, g, bl, m[3]]
}

/// A box whose `bg` springs over ticks 1..=10 through `way(s)`, its own
/// fill spring's bit set when `own`.
fn fill(way: impl Fn(f64) -> [f64; 4], own: bool) -> RunTrace {
    let mot = if own {
        Motions::PROP_SPRING | Motions::FILL_SPRING
    } else {
        Motions::PROP_SPRING
    };
    shape(12, |k| {
        let s = 1.0 - (1.0 - f64::from(k.min(10)) / 10.0).powi(3);
        let mut b = row("chip", [10.0, 6.0, 40.0, 18.0]);
        b.paint = Some(strand_eval::paint::PaintRec {
            bg: vec![hex(way(s))],
            ..Default::default()
        });
        vec![if (1..10).contains(&k) { b.mot(mot) } else { b }]
    })
}

const CLEAR: [f64; 4] = [0.0, 0.0, 0.0, 0.0];
const GOLD: [f64; 4] = [
    0xe5 as f64 / 255.0,
    0xc0 as f64 / 255.0,
    0x7b as f64 / 255.0,
    1.0,
];
const BLUE: [f64; 4] = [0x33 as f64 / 255.0, 0x66 as f64 / 255.0, 1.0, 1.0];
const YELLOW: [f64; 4] = [1.0, 0xcc as f64 / 255.0, 0.0, 1.0];

/// A fill springing from transparent black to gold keeps gold's hue (its
/// alpha alone rises), and blue to yellow passes OKLab's light middle.
#[test]
fn colour_passes_a_spring_on_its_way_in_premultiplied_oklab() {
    let sc = scenario("");
    for (a, b) in [(CLEAR, GOLD), (GOLD, CLEAR), (BLUE, YELLOW)] {
        let t = fill(|s| on_way(a, b, s), true);
        let r = check(&sc, &t);
        assert_eq!(
            status(&r, CheckId::Colour),
            Status::Pass,
            "{}",
            show(&r, &t)
        );
    }
}

/// Straight alpha: a fade from transparent black passes through a dark
/// olive gold never has (strand-scene's premultiplication dropped).
#[test]
fn colour_fails_a_fade_through_a_darker_colour() {
    use strand_eval::checks::colour::{oklab, srgb};
    let sc = scenario("");
    let lab = oklab([GOLD[0], GOLD[1], GOLD[2]]);
    let t = fill(
        |s| {
            let [r, g, b] = srgb(lab.map(|v| v * s));
            [r, g, b, s]
        },
        true,
    );
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Colour, "chip", "bg"),
        "{}",
        show(&r, &t)
    );
}

/// sRGB interpolation: blue to yellow through a dull grey.
#[test]
fn colour_fails_a_change_through_srgb_grey() {
    let sc = scenario("");
    let t = fill(
        |s| std::array::from_fn(|i| BLUE[i] + s * (YELLOW[i] - BLUE[i])),
        true,
    );
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Colour, "chip", "bg"),
        "{}",
        show(&r, &t)
    );
}

/// A colour with no spring of its own (the token graph deriving it each
/// frame, as a theme swap does) and one retargeted in flight are not
/// held to a straight way.
#[test]
fn colour_holds_only_its_own_spring_unretargeted() {
    let sc = scenario("");
    let srgb_way =
        |s: f64| -> [f64; 4] { std::array::from_fn(|i| BLUE[i] + s * (YELLOW[i] - BLUE[i])) };
    let t = fill(srgb_way, false);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Colour),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let mut t = fill(srgb_way, true);
    t.frames[4].driven = true;
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Colour),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// (eval-r3) A colour still springing when its segment starts (an
/// earlier segment set it moving, and this one's first change sends it
/// to a third colour) keeps its velocity, so its way bends off the line
/// from where it was drawn: not held. The same way from rest fails.
#[test]
fn colour_does_not_hold_a_colour_already_springing_when_its_segment_starts() {
    let sc = scenario("");
    let bent =
        |s: f64| -> [f64; 4] { std::array::from_fn(|i| BLUE[i] + s * (YELLOW[i] - BLUE[i])) };
    let mut t = fill(bent, true);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Colour, "chip", "bg"),
        "from rest it is held: {}",
        show(&r, &t)
    );
    for r in &mut t.frames[0].n {
        if r.key == "chip" {
            r.mot = (Motions::PROP_SPRING | Motions::FILL_SPRING).0;
            r.emot = r.mot;
        }
    }
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Colour),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// A node exiting over ticks 1..=`last`, drawn by `at(k)` and gone
/// after; its exit pose is `pose`.
fn exiting(pose: Pose, last: u32, at: impl Fn(u32) -> Row) -> RunTrace {
    let mut t = shape(last + 4, |k| {
        let mut v = vec![row("lead", [8.0, 6.0, 16.0, 12.0])];
        if k == 0 {
            v.push(row("leave", [30.0, 6.0, 16.0, 12.0]));
        } else if k <= last {
            v.push(at(k).mot(Motions::OPACITY_SPRING | Motions::PROP_SPRING | Motions::EXIT));
        }
        v
    });
    t.nodes.get_mut("leave").unwrap().exit = Some(pose);
    t.seal();
    t
}

/// An exit that ends at its pose, faded out or (unfaded) dropped 24 px,
/// unmounts without a pop.
#[test]
fn pop_out_passes_an_exit_that_reaches_its_pose() {
    let sc = scenario("");
    let fade = Pose {
        opacity: Some(0.0),
        y: Some(6.0),
        ..Pose::default()
    };
    let t = exiting(fade, 10, |k| {
        let p = 1.0 - (1.0 - k as f32 / 10.0).powi(3);
        row("leave", [30.0, 6.0, 16.0, 12.0])
            .at(30.0, 6.0 + 6.0 * p)
            .spring(0.0, 6.0 * p)
            .op(1.0 - p)
    });
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let drop = Pose {
        y: Some(24.0),
        ..Pose::default()
    };
    let t = exiting(drop, 10, |k| {
        let p = 1.0 - (1.0 - k as f32 / 10.0).powi(3);
        row("leave", [30.0, 6.0, 16.0, 12.0])
            .at(30.0, 6.0 + 24.0 * p)
            .spring(0.0, 24.0 * p)
    });
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// An exit cut short: unmounted at a fifth of its opacity (its motion
/// declared settled early), or at a quarter of its drop.
#[test]
fn pop_out_fails_an_exit_unmounted_short_of_its_pose() {
    let sc = scenario("");
    let fade = Pose {
        opacity: Some(0.0),
        ..Pose::default()
    };
    let t = exiting(fade, 8, |k| {
        row("leave", [30.0, 6.0, 16.0, 12.0]).op(1.0 - k as f32 / 10.0)
    });
    let r = check(&sc, &t);
    let pops: Vec<_> = r
        .failures(CheckId::Vanish)
        .into_iter()
        .filter(|f| f.msg.starts_with("pop-out") && f.k == Some(8))
        .collect();
    assert_eq!(pops.len(), 1, "{}", show(&r, &t));
    let drop = Pose {
        y: Some(24.0),
        ..Pose::default()
    };
    let t = exiting(drop, 1, |_| {
        row("leave", [30.0, 6.0, 16.0, 12.0])
            .at(30.0, 12.0)
            .spring(0.0, 6.0)
    });
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pop-out") && f.msg.contains("offset 6.00 on y")),
        "{}",
        show(&r, &t)
    );
}

/// Not held where nothing exits (a node taken away at once has no pose
/// to reach) or motion is reduced (a pose snaps).
#[test]
fn pop_out_holds_only_an_exit_played() {
    let fade = Pose {
        opacity: Some(0.0),
        ..Pose::default()
    };
    let sc = scenario("");
    let mut t = exiting(fade, 4, |k| {
        row("leave", [30.0, 6.0, 16.0, 12.0]).op(1.0 - k as f32 / 10.0)
    });
    for f in &mut t.frames {
        for r in &mut f.n {
            r.mot = 0;
            r.emot = 0;
        }
    }
    t.seal();
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pop-out")),
        "{}",
        show(&r, &t)
    );
    let mut t = exiting(fade, 4, |k| {
        row("leave", [30.0, 6.0, 16.0, 12.0]).op(1.0 - k as f32 / 10.0)
    });
    t.segments[1].reduced_motion = true;
    t.seal();
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pop-out")),
        "{}",
        show(&r, &t)
    );
}

/// Three children mounting at tick 1 under a 30 ms stagger, child `n`
/// leaving its enter pose (opacity 0) at the tick `start(n)`.
fn staggered(start: impl Fn(usize) -> u32) -> RunTrace {
    staggered_slow(start, |_| 0)
}

/// As [`staggered`], each child let go of its turn at `start(n)` but
/// drawn within a quantum of its pose for `slow(n)` frames after it (an
/// enter on a curve that starts slowly), before it shows moving.
fn staggered_slow(start: impl Fn(usize) -> u32, slow: impl Fn(usize) -> u32) -> RunTrace {
    let keys = ["c0", "c1", "c2"];
    let mut t = shape(14, |k| {
        let mut v = vec![row("list", [0.0, 0.0, 300.0, 30.0])];
        if k >= 1 {
            for (n, key) in keys.iter().enumerate() {
                let s = start(n);
                let x = 10.0 + 40.0 * n as f32;
                let r = row(key, [x, 6.0, 30.0, 18.0]);
                let lag = slow(n);
                v.push(if k < s {
                    r.op(0.0).mot(Motions::ENTER | Motions::STAGGER)
                } else if k < s + lag {
                    // Begun, but less than a quantum off its pose.
                    r.op(0.5 / 255.0)
                        .mot(Motions::OPACITY_SPRING | Motions::ENTER)
                } else {
                    let p = (((k - s - lag + 1) as f32) / 6.0).min(1.0);
                    r.op(p).mot(Motions::OPACITY_SPRING | Motions::ENTER)
                });
            }
        }
        v
    });
    t.nodes.get_mut("list").unwrap().stagger = Some(30.0);
    for key in keys {
        let m = t.nodes.get_mut(key).unwrap();
        m.parent = Some("list".into());
        m.enter = Some(Pose {
            opacity: Some(0.0),
            ..Pose::default()
        });
    }
    t.seal();
    t
}

/// Children 30 ms apart at 60 Hz start on the first frame at or after
/// their turn: ticks 1, 2 and 4.
#[test]
fn stagger_passes_children_starting_a_step_apart() {
    let sc = scenario("");
    let t = staggered(|n| [1, 2, 4][n]);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Stagger),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// Numbered by their place among all the container's children (one
/// already there before them), every child starts a step late; numbered
/// backwards, the first starts last.
#[test]
fn stagger_fails_children_numbered_wrong() {
    let sc = scenario("");
    let t = staggered(|n| [2, 4, 6][n]);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Stagger)
            .iter()
            .any(|f| f.key.as_deref() == Some("c0") && f.msg.starts_with("start")),
        "{}",
        show(&r, &t)
    );
    let t = staggered(|n| [4, 2, 1][n]);
    let r = check(&sc, &t);
    let late: Vec<_> = r
        .failures(CheckId::Stagger)
        .into_iter()
        .filter_map(|f| f.key.clone())
        .collect();
    assert_eq!(late, ["c0", "c2"], "{}", show(&r, &t));
}

/// (eval-r3) An enter whose curve starts slowly (`ease(in, …)`,
/// `in_back`, `emphasized_accelerate`) stays within a quantum of its pose
/// for frames after its turn: its start is the frame it is let go of its
/// turn, so it is on time. Drawn moving while it still waits for its
/// turn, it starts early.
#[test]
fn stagger_takes_a_slow_start_from_its_turn_not_its_first_visible_frame() {
    let sc = scenario("");
    let t = staggered_slow(|n| [1, 2, 4][n], |_| 3);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Stagger),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    // The rule before (the first frame off its pose) failed it: the
    // children leave their pose three frames after their turn.
    let mut t = staggered(|n| [1, 2, 4][n]);
    for f in &mut t.frames {
        for r in &mut f.n {
            if r.key == "c2" && f.k < 4 {
                // Waiting for its turn, yet drawn half in.
                r.op = 0.5;
                r.own_op = 0.5;
            }
        }
    }
    t.seal();
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Stagger)
            .iter()
            .any(|f| f.key.as_deref() == Some("c2") && f.msg.starts_with("start")),
        "{}",
        show(&r, &t)
    );
}

/// Under reduced motion every child shows at once: not held.
#[test]
fn stagger_is_not_held_under_reduced_motion() {
    let sc = scenario("");
    let mut t = staggered(|n| [4, 2, 1][n]);
    t.scenario.reduced_motion = true;
    t.seal();
    let r = check(&sc, &t);
    assert!(
        matches!(status(&r, CheckId::Stagger), Status::Skip(_)),
        "{}",
        show(&r, &t)
    );
}

/// Two boxes laid flush at x 43 drawn at `a_right` and `b_left`.
fn flush(a_right: f32, b_left: f32) -> RunTrace {
    shape(2, |_| {
        let mut a = row("a", [10.0, 6.0, 33.0, 18.0]);
        a.visual[2] = a_right - 10.0;
        let mut b = row("b", [43.0, 6.0, 34.0, 18.0]);
        b.visual[0] = b_left;
        b.visual[2] = 77.0 - b_left;
        vec![a, b]
    })
}

/// Edges snapped from where they fall share a pixel (at 1.25x, 43 px is
/// drawn at 43.2 on both sides).
#[test]
fn tiling_passes_flush_boxes_sharing_their_edge() {
    let sc = scenario("");
    let t = flush(43.2, 43.2);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Tiling),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// A size rounded apart from its origin leaves a pixel's seam; a box
/// covering every pixel it touches draws one pixel twice.
#[test]
fn tiling_fails_a_seam_or_an_overlap() {
    let sc = scenario("");
    for (a, b) in [(42.4, 43.2), (43.2, 42.4)] {
        let t = flush(a, b);
        let r = check(&sc, &t);
        assert!(
            fails_on(&r, CheckId::Tiling, "a", "x") || fails_on(&r, CheckId::Tiling, "b", "x"),
            "{}",
            show(&r, &t)
        );
    }
}

/// A box in motion (its layer placed by its spring's whole pixels) is
/// not held to its neighbour's edge.
#[test]
fn tiling_skips_a_box_in_motion() {
    let sc = scenario("");
    let mut t = flush(42.4, 43.2);
    for f in &mut t.frames {
        f.n[0].mot = Motions::PROP_SPRING.0;
    }
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Tiling),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// A content-sized popup (surface 2, sized by its content's height)
/// whose card springs from 80 px to 40 px tall over ticks 1..=6 while
/// the surface is `size(k)` tall.
fn shrinking(size: impl Fn(u32) -> f32, h: impl Fn(u32) -> f32) -> RunTrace {
    let mut t = shape(8, |k| {
        let mut pop = row("pop", [0.0, 0.0, 120.0, size(k)]);
        pop.surface = 2;
        let mut card = row("card", [0.0, 0.0, 120.0, 40.0]);
        card.surface = 2;
        card.visual[3] = h(k);
        card.drawn[3] = h(k);
        if k == 0 {
            card.laid[3] = 80.0;
        }
        vec![row("bar", [0.0, 0.0, 300.0, 30.0]), pop, card]
    });
    for f in &mut t.frames {
        f.painted.push(2);
        f.sizes.insert(2, [120.0, size(f.k)]);
        let c = f.cost[&1];
        f.cost.insert(2, c);
    }
    t.surfaces = vec![
        SurfaceInfo {
            id: 1,
            root: "bar".into(),
            ..Default::default()
        },
        SurfaceInfo {
            id: 2,
            root: "pop".into(),
            kind: "popup".into(),
            content: [false, true],
            ..Default::default()
        },
    ];
    t.nodes.get_mut("card").unwrap().parent = Some("pop".into());
    t.seal();
    t
}

fn card_h(k: u32) -> f32 {
    if k == 0 {
        80.0
    } else {
        40.0 + 40.0 * (1.0 - k.min(6) as f32 / 6.0).powi(2)
    }
}

/// Held at its larger size while its content springs smaller, the
/// surface cuts nothing; a spring's overshoot past where it started and
/// rests is not the surface's.
#[test]
fn contain_passes_a_surface_held_while_its_content_shrinks() {
    let sc = scenario("");
    let t = shrinking(|k| if k < 7 { 80.0 } else { 40.0 }, card_h);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Contain),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = shrinking(
        |k| if k < 7 { 80.0 } else { 40.0 },
        |k| if k == 2 { 36.0 } else { card_h(k) },
    );
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Contain),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// The surface taking its content's laid size at once, mid-shrink: the
/// card is drawn past its bottom edge.
#[test]
fn contain_fails_a_surface_that_cuts_its_shrinking_content() {
    let sc = scenario("");
    let t = shrinking(|k| if k == 0 { 80.0 } else { 40.0 }, card_h);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Contain, "card", "y"),
        "{}",
        show(&r, &t)
    );
    // The same cut on a surface sized by its own `height` is the author's.
    let mut t = shrinking(|k| if k == 0 { 80.0 } else { 40.0 }, card_h);
    t.surfaces[1].content = [false, false];
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Contain),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// A bar row holding a pill (`pill`, its ellipsised title `title` laid
/// from 40 to `right`) and a status text (`status`, rigid: plain text
/// keeps its width).
fn titled(right: f32) -> RunTrace {
    let mut t = shape(2, |_| {
        let mut title = row("title", [40.0, 5.0, right - 40.0, 15.0]);
        title.text = Some("S".into());
        vec![
            row("strip", [0.0, 0.0, 300.0, 24.0]),
            row("pill", [24.0, 5.0, right - 20.0, 15.0]),
            title,
            row("status", [right + 10.0, 5.0, 33.0, 15.0]),
        ]
    });
    let parents = [("pill", "strip"), ("title", "pill"), ("status", "strip")];
    for (k, p) in parents {
        t.nodes.get_mut(k).unwrap().parent = Some(p.into());
    }
    t.nodes.get_mut("title").unwrap().ellipsis = true;
    t.nodes.get_mut("status").unwrap().rigid = true;
    t.seal();
    t
}

/// An ellipsised title that gives way inside the bar passes; one laid
/// past the bar's edge, with nothing rigid putting it there, fails.
#[test]
fn contain_holds_an_ellipsised_text_to_give_way() {
    let sc = scenario("");
    let t = titled(249.0);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Contain),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = titled(340.0);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Contain, "title", "w"),
        "{}",
        show(&r, &t)
    );
}

/// Past the edge because something cannot give way (the pill has a
/// width of its own) or inside a clipping ancestor: not the text's.
#[test]
fn contain_excuses_a_text_put_past_the_edge_by_what_cannot_give_way() {
    let sc = scenario("");
    let mut t = titled(340.0);
    t.nodes.get_mut("pill").unwrap().rigid = true;
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Contain),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let mut t = titled(340.0);
    t.nodes.get_mut("pill").unwrap().clips = true;
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Contain),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

// (eval-surface-leg) The surface leg's checks over synthetic logs.

use strand_eval::checks::wire::{late_present, regions_on_wire};
use strand_eval::wire::{Path, WireCommit, WireFrame, WireLog, normal};

/// A trace whose surface leg sent `frames` in `commits`.
fn wired(frames: Vec<WireFrame>, commits: Vec<WireCommit>) -> RunTrace {
    RunTrace {
        wire: Some(WireLog {
            frames,
            commits,
            ..WireLog::default()
        }),
        ..RunTrace::default()
    }
}

/// Popup frame `k` taking input in a card `w` wide, carried by commit `c`.
fn card_frame(k: u32, w: i32, c: usize) -> WireFrame {
    WireFrame {
        k,
        surface: 2,
        path: Some(Path::Gpu),
        size: [240, 40],
        due: true,
        input: Some(vec![[0, 0, w, 32]]),
        opaque: vec![],
        blur: vec![[0, 0, w, 32]],
        posed: false,
        commit: Some(c),
    }
}

/// A commit at `k` of a 240x40 buffer, holding input and blur `w` wide.
fn card_commit(k: u32, w: i32) -> WireCommit {
    WireCommit {
        k,
        surface: Some(2),
        by: Some(Path::Gpu),
        frame: Some(k),
        buffer: Some([240, 40]),
        scale: 1,
        damage: vec![[0, 0, 240, 40]],
        input: Some(vec![[0, 0, w, 32]]),
        blur: vec![[0, 0, w, 32]],
        callbacks: 1,
        shown: Some(k),
        ..WireCommit::default()
    }
}

#[test]
fn regions_on_wire_passes_regions_that_follow_their_frames() {
    let t = wired(
        vec![card_frame(1, 90, 0), card_frame(2, 120, 1)],
        vec![card_commit(1, 90), card_commit(2, 120)],
    );
    assert_eq!(regions_on_wire(&t), []);
    // The same pixels sent as other rectangles are the same region: the
    // leg records each region in its normal form.
    let mut split = card_commit(2, 120);
    split.input = Some(normal([(true, [0, 0, 60, 32]), (true, [60, 0, 60, 32])]));
    let t = wired(
        vec![card_frame(1, 90, 0), card_frame(2, 120, 1)],
        vec![card_commit(1, 90), split],
    );
    assert_eq!(regions_on_wire(&t), []);
}

/// Bug 23's shape: the GPU frame grew, the compositor kept the last shm
/// frame's regions.
#[test]
fn regions_on_wire_fails_a_present_that_keeps_the_last_regions() {
    let t = wired(
        vec![
            card_frame(1, 90, 0),
            card_frame(2, 120, 1),
            card_frame(3, 150, 2),
        ],
        vec![card_commit(1, 90), card_commit(2, 90), card_commit(3, 90)],
    );
    let found = regions_on_wire(&t);
    let input: Vec<_> = found
        .iter()
        .filter(|f| f.msg.contains("input region"))
        .collect();
    assert_eq!(input.len(), 1, "{found:#?}");
    assert_eq!(input[0].k, Some(2));
    assert_eq!(input[0].more, 1, "k3 is counted in");
    assert_eq!(input[0].sev, Sev::Fail);
    assert!(
        found.iter().any(|f| f.msg.contains("blurs behind")),
        "{found:#?}"
    );
    // A bare commit after a frame that puts the old region back fails too.
    let mut bare = card_commit(2, 90);
    bare.buffer = None;
    bare.by = None;
    bare.frame = None;
    let t = wired(
        vec![card_frame(1, 90, 0), card_frame(2, 120, 1)],
        vec![card_commit(1, 90), card_commit(2, 120), bare],
    );
    let found = regions_on_wire(&t);
    assert!(
        found.iter().any(|f| f.msg.contains("(bare, k2)")),
        "{found:#?}"
    );
    // A buffer of the wrong size.
    let mut wrong = card_commit(2, 120);
    wrong.buffer = Some([200, 40]);
    let t = wired(
        vec![card_frame(1, 90, 0), card_frame(2, 120, 1)],
        vec![card_commit(1, 90), wrong],
    );
    assert!(
        regions_on_wire(&t)
            .iter()
            .any(|f| f.msg.contains("200x40 buffer"))
    );
}

/// A frame the compositor poses at a scale sends its regions in the
/// pose's coordinates: not compared.
#[test]
fn regions_on_wire_leaves_a_posed_frame_alone() {
    let mut posed = card_frame(2, 120, 1);
    posed.posed = true;
    let t = wired(
        vec![card_frame(1, 90, 0), posed],
        vec![card_commit(1, 90), card_commit(2, 60)],
    );
    assert_eq!(regions_on_wire(&t), []);
}

#[test]
fn late_present_passes_frames_shown_at_their_refresh() {
    let mut idle = card_frame(3, 120, 2);
    idle.due = false;
    idle.commit = None;
    let t = wired(
        vec![card_frame(1, 90, 0), card_frame(2, 120, 1), idle],
        vec![card_commit(1, 90), card_commit(2, 120)],
    );
    assert_eq!(late_present(&t), []);
}

#[test]
fn late_present_fails_a_frame_late_replaced_or_never_shown() {
    let mut late = card_commit(2, 120);
    late.k = 3;
    late.shown = Some(3);
    let mut replaced = card_commit(3, 150);
    replaced.superseded = true;
    let t = wired(
        vec![
            card_frame(1, 90, 0),
            card_frame(2, 120, 1),
            card_frame(3, 150, 2),
            WireFrame {
                commit: None,
                ..card_frame(4, 160, 0)
            },
        ],
        vec![card_commit(1, 90), late, replaced],
    );
    let found = late_present(&t);
    let at = |k: u32| {
        found
            .iter()
            .find(|f| f.k == Some(k))
            .map(|f| f.msg.as_str())
    };
    assert_eq!(at(1), None);
    assert!(
        at(2).is_some_and(|m| m.contains("1 refresh late")),
        "{found:#?}"
    );
    assert!(at(3).is_some_and(|m| m.contains("replaced")), "{found:#?}");
    assert!(
        at(4).is_some_and(|m| m.contains("never shown")),
        "{found:#?}"
    );
}

#[test]
fn the_surface_checks_skip_without_the_leg() {
    let sc = scenario("surface = true\nscreen = { width = 300, scale = 1.0 }");
    let r = check(&sc, &RunTrace::default());
    for c in [CheckId::RegionsOnWire, CheckId::LatePresent] {
        assert!(matches!(status(&r, c), Status::Skip(_)), "{c:?}");
    }
}

/// (eval-r3) The manager's throttle gives up on real time, not the
/// leg's clock: a frame replaced by the repaint of a tick it gave up in
/// (a run slow enough to wait a second) is not judged, nor the next
/// tick's, while one replaced elsewhere still fails.
#[test]
fn late_present_leaves_the_ticks_the_manager_gave_up_in_to_the_machine() {
    let mut late = card_commit(2, 90);
    late.superseded = true;
    let frames = vec![card_frame(1, 90, 0), card_frame(2, 90, 1)];
    let t = wired(frames.clone(), vec![card_commit(1, 90), late.clone()]);
    assert_eq!(late_present(&t).len(), 1, "replaced: late");
    let mut t = wired(frames, vec![card_commit(1, 90), late]);
    if let Some(w) = t.wire.as_mut() {
        w.gave_up = vec![1];
    }
    assert_eq!(late_present(&t), [], "the tick after a give-up");
    if let Some(w) = t.wire.as_mut() {
        w.gave_up = vec![5];
    }
    assert_eq!(late_present(&t).len(), 1, "a give-up elsewhere");
}

// --- eval-local: what a real shell showed the checks misreading

/// A card mounting in the segment (a popup's content) at 40 × 20 and
/// springing its size to 400 × 200 over ticks 2..=10; `jump` lands it
/// there from its first frame in one tick instead.
fn mounting_card(jump: bool) -> RunTrace {
    shape(14, move |k| {
        let mut v = vec![row("a", [0.0, 0.0, 10.0, 10.0])];
        if k >= 2 {
            let p = if jump {
                if k == 2 { 0.0 } else { 1.0 }
            } else {
                ease(0.0, 1.0, k - 2, 8)
            };
            let (w, h) = (40.0 + 360.0 * p, 20.0 + 180.0 * p);
            let mut c = row("card", [200.0 - w / 2.0, 0.0, w, h]);
            if k < 12 {
                c = c.mot(Motions::SIZE_SPRING).auth(Axes::W | Axes::H);
            }
            v.push(c);
        }
        v
    })
}

#[test]
fn a_node_mounted_in_the_segment_travels_from_its_first_frame() {
    let sc = scenario("");
    let t = mounting_card(false);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    // Its whole way in one tick is still a jump.
    let t = mounting_card(true);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "card", "w"),
        "{}",
        show(&r, &t)
    );
}

/// `child` (`enter { opacity: 0; x: -6 }`) first drawn at tick 4, at rest
/// and without `ENTER`, inside `veil`; `veil_hidden` draws the veil
/// transparent on ticks 1..=3 (its subtree not drawn), else the veil
/// shows from tick 1 and the child appears under it.
fn under_veil(veil_hidden: bool) -> RunTrace {
    let mut t = shape(10, move |k| {
        let mut v = vec![row("a", [0.0, 0.0, 10.0, 10.0])];
        if k >= 1 {
            let op = if veil_hidden && k < 4 { 0.0 } else { 1.0 };
            let mut veil = row("veil", [20.0, 0.0, 100.0, 30.0]).op(op);
            if op == 0.0 {
                veil.hidden = Some("opacity0".into());
            }
            v.push(veil);
        }
        if k >= 4 {
            v.push(row("child", [20.0, 0.0, 50.0, 20.0]));
        }
        v
    });
    t.nodes.get_mut("child").unwrap().enter = Some(Pose {
        opacity: Some(0.0),
        x: Some(-6.0),
        ..Pose::default()
    });
    with_parents(t, &[("child", "veil")])
}

#[test]
fn an_enter_dropped_under_a_transparent_parent_is_no_pop_in() {
    let sc = scenario("");
    let t = under_veil(true);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    // Under a parent that shows, the same first frame is a pop-in.
    let t = under_veil(false);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.key.as_deref() == Some("child") && f.msg.starts_with("pop-in")),
        "{}",
        show(&r, &t)
    );
}

/// `card` mounted at tick 2 at rest with no `ENTER`, its pose `pose`;
/// `sized` gives its first frames a size spring.
fn posed(pose: Pose, sized: bool) -> RunTrace {
    let mut t = shape(8, move |k| {
        let mut v = vec![row("a", [0.0, 0.0, 10.0, 10.0])];
        if k >= 2 {
            let mut c = row("card", [20.0, 0.0, 100.0, 30.0]);
            if sized && k < 6 {
                c = c.mot(Motions::SIZE_SPRING).auth(Axes::W | Axes::H);
            }
            v.push(c);
        }
        v
    });
    t.nodes.get_mut("card").unwrap().enter = Some(pose);
    t.seal();
    t
}

#[test]
fn a_pose_with_nothing_to_play_and_a_size_pose_are_no_pop_in() {
    let sc = scenario("");
    let pop = |t: &RunTrace| {
        check(&sc, t)
            .failures(CheckId::Vanish)
            .iter()
            .any(|f| f.key.as_deref() == Some("card") && f.msg.starts_with("pop-in"))
    };
    // `enter { opacity: 1 }`: where it rests.
    let still = Pose {
        opacity: Some(1.0),
        ..Pose::default()
    };
    assert!(!pop(&posed(still, false)));
    // `enter { width: 56; height: 6 }`: the trace reads no axis of it,
    // and its size springs from its first frame.
    assert!(!pop(&posed(Pose::default(), true)));
    // The same size pose not playing, and a pose of opacity 0.5 skipped.
    assert!(pop(&posed(Pose::default(), false)));
    let half = Pose {
        opacity: Some(0.5),
        ..Pose::default()
    };
    assert!(pop(&posed(half, false)));
}

#[test]
fn a_shaped_text_overhanging_its_box_is_the_fonts_not_a_spill() {
    let sc = scenario("");
    // An icon glyph 4 px wider than its advance, drawn by its own layout,
    // whose ink reaches that far past the layout's box (eval-r3: the
    // overhang the probe records, `ov`).
    let t = shape(2, |_| {
        let mut r = text_row("t", 20.0, 24.0);
        r.text = Some("S".into());
        r.ov = Some([0.0, 5.0]);
        vec![r]
    });
    let r = check(&sc, &t);
    assert_eq!(status(&r, CheckId::Ink), Status::Pass, "{}", show(&r, &t));
    // The same ink from a layout standing in spills.
    let t = shape(2, |_| vec![text_row("t", 20.0, 24.0)]);
    let r = check(&sc, &t);
    assert!(fails_on(&r, CheckId::Ink, "t", "w"), "{}", show(&r, &t));
}

/// A ghost springing 120 px left over ticks 1..=8 as it exits, gone
/// from tick 9; `jump` sends it there in one tick.
fn leaving(jump: bool) -> RunTrace {
    shape(12, move |k| {
        let mut v = vec![row("a", [0.0, 0.0, 10.0, 10.0])];
        if k <= 8 {
            let p = if jump {
                if k == 0 { 0.0 } else { 1.0 }
            } else {
                ease(0.0, 1.0, k, 8)
            };
            let mut g = row("ghost", [100.0, 0.0, 50.0, 20.0])
                .at(100.0 - 120.0 * p, 0.0)
                .spring(-120.0 * p, 0.0);
            if k >= 1 {
                g = g.mot(Motions::EXIT | Motions::PROP_SPRING).auth(Axes::X);
                g.hidden = Some("inert".into());
            }
            v.push(g);
        }
        v
    })
}

#[test]
fn a_node_leaving_in_the_segment_travels_to_its_last_frame() {
    let sc = scenario("");
    let t = leaving(false);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = leaving(true);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "ghost", "x"),
        "{}",
        show(&r, &t)
    );
}

/// `child` (`enter { opacity: 0 }`) first drawn at tick 4 inside `card`;
/// `clipped`: the card is too short to reach it until then (its height
/// springing open, the child clipped out of it), else the card already
/// covers it. Drawn at rest, without `ENTER`. `clips`: the card clips
/// what it holds (eval-r3: a card that does not draws the child past its
/// box, so a frame does not drop its pose for being outside it).
fn under_clip(clipped: bool) -> RunTrace {
    under_card(clipped, true)
}

fn under_card(clipped: bool, clips: bool) -> RunTrace {
    let mut t = shape(10, move |k| {
        // The bar's root, which holds the card (and is its surface's
        // root: the surface ends at its box).
        let mut v = vec![
            row("a", [0.0, 0.0, 10.0, 10.0]),
            row("root", [0.0, 0.0, 300.0, 30.0]),
        ];
        if k >= 1 {
            let h = if clipped {
                (5.0 * k as f32).min(30.0)
            } else {
                30.0
            };
            v.push(row("card", [20.0, 0.0, 200.0, h]).mot(Motions::SIZE_SPRING));
        }
        if k >= 4 {
            // Within the bar (30 px tall), below the card's 15 px at k3.
            v.push(row("child", [20.0, 18.0, 100.0, 8.0]));
        }
        v
    });
    t.nodes.get_mut("child").unwrap().enter = Some(Pose {
        opacity: Some(0.0),
        ..Pose::default()
    });
    t.nodes.get_mut("card").unwrap().clips = clips;
    with_parents(t, &[("child", "card"), ("card", "root")])
}

#[test]
fn an_enter_dropped_out_of_view_is_no_pop_in() {
    let sc = scenario("");
    let pop = |t: &RunTrace| {
        check(&sc, t)
            .failures(CheckId::Vanish)
            .iter()
            .any(|f| f.key.as_deref() == Some("child") && f.msg.starts_with("pop-in"))
    };
    assert!(!pop(&under_clip(true)));
    let t = under_clip(false);
    assert!(pop(&t), "{}", show(&check(&sc, &t), &t));
    // (eval-r3) Outside a card that does not clip it is drawn all the
    // same: its first frame at rest skipped its pose.
    let t = under_card(true, false);
    assert!(pop(&t), "{}", show(&check(&sc, &t), &t));
}

/// `enter { opacity: 0 }` played (its first frame at 0.14, rising), the
/// node gone (hidden by its parent) before the segment's rest.
#[test]
fn an_enter_played_by_a_node_gone_before_the_rest_is_no_pop_in() {
    let sc = scenario("");
    let mut t = shape(10, |k| {
        let mut v = vec![row("a", [0.0, 0.0, 10.0, 10.0])];
        if (2..6).contains(&k) {
            v.push(
                row("n", [20.0, 0.0, 40.0, 20.0])
                    .op(ease(0.14, 1.0, k - 2, 8))
                    .mot(Motions::ENTER | Motions::OPACITY_SPRING)
                    .auth(Axes::OPACITY),
            );
        }
        v
    });
    t.nodes.get_mut("n").unwrap().enter = Some(Pose {
        opacity: Some(0.0),
        ..Pose::default()
    });
    t.seal();
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("pop-in")),
        "{}",
        show(&r, &t)
    );
}

/// (eval-r3) A frame that drew nothing has no buffer commit, but its
/// regions go out with the bare commit after it: that commit is held to
/// it (a ghost's last frame asking for no input), not to the last frame
/// a buffer carried, and one that keeps the old regions still fails.
#[test]
fn regions_on_wire_holds_a_bare_commit_to_the_frame_that_drew_nothing() {
    let mut quiet = card_frame(2, 120, 0);
    quiet.commit = None;
    quiet.due = false;
    let bare = |w: i32| WireCommit {
        k: 2,
        surface: Some(2),
        input: Some(vec![[0, 0, w, 32]]),
        blur: vec![[0, 0, w, 32]],
        shown: Some(2),
        ..WireCommit::default()
    };
    let t = wired(
        vec![card_frame(1, 90, 0), quiet.clone()],
        vec![card_commit(1, 90), bare(120)],
    );
    assert_eq!(regions_on_wire(&t), []);
    let t = wired(
        vec![card_frame(1, 90, 0), quiet],
        vec![card_commit(1, 90), bare(90)],
    );
    let found = regions_on_wire(&t);
    assert!(
        found.iter().any(|f| f.msg.contains("input region")),
        "{found:?}"
    );
}

// --- eval-coverage-2

/// (eval-coverage-2) An authored offset spring sent out and back in one
/// segment (a page slid in and, switched away mid-flight, out again)
/// travels its excursion: 50 px steps of a 100 px excursion that ends
/// 10 px from where it started pass. A 90 px step of it still fails.
#[test]
fn an_authored_spring_out_and_back_travels_its_excursion() {
    let sc = scenario("");
    let out_and_back = |xs: [f32; 5]| {
        shape(8, move |k| {
            let a = row("a", [0.0, 0.0, 20.0, 20.0]);
            let m = Motions::PROP_SPRING;
            match k {
                0 => vec![a],
                1..=5 => vec![a.at(xs[k as usize - 1], 0.0).mot(m).auth(Axes::X)],
                _ => vec![a.at(10.0, 0.0)],
            }
        })
    };
    let t = out_and_back([50.0, 100.0, 60.0, 20.0, 10.0]);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = out_and_back([90.0, 100.0, 60.0, 20.0, 10.0]);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
}

/// (eval-coverage-2) A node that leaves its surface and comes back in one
/// segment (a branch shown again, morphing from where another was drawn)
/// travels from where it came back: back at 0 and morphing to 100, where
/// it started the segment, its 45 px steps pass; back at 0 and home in
/// one tick still fails.
#[test]
fn a_node_that_leaves_and_comes_back_travels_from_where_it_came_back() {
    let sc = scenario("");
    let back = |xs: [f32; 3]| {
        shape(10, move |k| {
            let a = row("a", [100.0, 0.0, 20.0, 20.0]).laid_at(100.0, 0.0);
            let m = Motions::SHARED_MORPH;
            match k {
                0 => vec![a],
                1 => vec![a.mot(m)],
                2 => vec![],
                3 => vec![a.at(0.0, 0.0).mot(m)],
                4..=6 => vec![a.at(xs[k as usize - 4], 0.0).mot(m)],
                _ => vec![a],
            }
        })
    };
    let t = back([45.0, 80.0, 100.0]);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Teleport),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = back([100.0, 100.0, 100.0]);
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Teleport, "a", "x"),
        "{}",
        show(&r, &t)
    );
}

/// (eval-coverage-2) A list's rows laid out at its offset each frame while
/// it scrolls by itself (`SCROLL`, a wheel step's spring, or `FLING`, a
/// lift's fling, into the list's end) jump as layout does: noted, not
/// failed.
#[test]
fn a_rows_jump_under_a_scroll_is_noted_not_failed() {
    for fling in [false, true] {
        rows_jump_under_a_scroll(fling);
    }
}

fn rows_jump_under_a_scroll(fling: bool) {
    let sc = scenario("");
    let t = shape(4, move |k| {
        let a = row("a", [0.0, 0.0, 20.0, 20.0]);
        let x = [0.0, 0.0, 150.0, 150.0, 150.0][k as usize];
        let m = match (k, fling) {
            (1, true) => Motions(Motions::SCROLL.0 | Motions::FLING.0),
            (1..=3, _) => Motions::SCROLL,
            _ => Motions::NONE,
        };
        vec![a.at(x, 0.0).laid_at(x, 0.0).mot(m)]
    });
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
}

/// (eval-coverage-2) A gliding box whose own size snaps while its parent
/// keeps it by its centre (a card shrinking in a held, centred surface)
/// moves with its size: not a retarget. The same move with no change of
/// size still is one (`a_glide_retargeted_from_its_layout_...`).
#[test]
fn a_glide_moved_by_its_own_size_about_its_centre_is_not_a_retarget() {
    let sc = scenario("");
    let t = shape(10, |k| {
        let a = row("a", [0.0, 0.0, 100.0, 20.0]);
        let g = Motions::GLIDE;
        let narrow = |mut r: Row| {
            for b in [&mut r.laid, &mut r.visual, &mut r.drawn] {
                b[2] = 60.0;
            }
            r
        };
        match k {
            0 => vec![a],
            1 => vec![a.laid_at(40.0, 0.0).at(10.0, 0.0).mot(g)],
            2 => vec![a.laid_at(40.0, 0.0).at(18.0, 0.0).mot(g)],
            3 => vec![narrow(a.laid_at(60.0, 0.0).at(38.0, 0.0).mot(g))],
            4 => vec![narrow(a.laid_at(60.0, 0.0).at(50.0, 0.0).mot(g))],
            5 => vec![narrow(a.laid_at(60.0, 0.0).at(58.0, 0.0).mot(g))],
            _ => vec![narrow(a.laid_at(60.0, 0.0).at(60.0, 0.0))],
        }
    });
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Teleport)
            .iter()
            .any(|f| f.msg.starts_with("retarget")),
        "{}",
        show(&r, &t)
    );
}

/// The reduced-motion twin of `shape`: its rest at `end`.
fn twin_of(end: u32, rows: impl Fn(u32) -> Vec<Row>) -> RunTrace {
    let mut t = shape(end, rows);
    t.scenario.reduced_motion = true;
    t
}

/// (eval-coverage-2) A node made again while the one it replaces still
/// exits takes the key with `~2`; with motion reduced the exit is over at
/// once and it takes the plain key. Shown alone under that base in both
/// runs, the two are compared as one node; shown beside a node of the
/// plain key, the `~2` one is its own.
#[test]
fn twin_compares_a_key_made_again_by_its_base() {
    let sc = scenario("");
    let at_rest = [10.0, 5.0, 20.0, 10.0];
    let t = shape(2, |k| match k {
        0 => vec![],
        _ => vec![row("label~2", at_rest)],
    });
    let twin = twin_of(2, |k| match k {
        0 => vec![],
        _ => vec![row("label", at_rest)],
    });
    let r = checks::evaluate(&sc, &t, Some(&twin), None, None);
    assert_eq!(status(&r, CheckId::Twin), Status::Pass, "{}", show(&r, &t));
    let both = shape(2, |k| match k {
        0 => vec![],
        _ => vec![
            row("label", at_rest),
            row("label~2", [40.0, 5.0, 20.0, 10.0]),
        ],
    });
    let r = checks::evaluate(&sc, &both, Some(&twin), None, None);
    assert!(
        r.failures(CheckId::Twin)
            .iter()
            .any(|f| f.key.as_deref() == Some("label~2")),
        "{}",
        show(&r, &both)
    );
}

/// (eval-coverage-2) `reduced_motion` flings nothing, so a surface whose
/// scroll flung rests elsewhere by design and is left out from the
/// fling on; the same rest with no fling is a difference.
#[test]
fn twin_leaves_out_a_surface_whose_scroll_flung() {
    let sc = scenario("");
    let rows = |fling: bool| {
        move |k: u32| {
            let a = row("row", [0.0, 0.0, 20.0, 20.0]);
            let m = match (k, fling) {
                (1 | 2, true) => Motions(Motions::SCROLL.0 | Motions::FLING.0),
                _ => Motions::NONE,
            };
            let x = [0.0, 40.0, 70.0, 80.0][k as usize];
            vec![a.at(x, 0.0).laid_at(x, 0.0).mot(m)]
        }
    };
    let twin = twin_of(3, |_| vec![row("row", [0.0, 0.0, 20.0, 20.0])]);
    let t = shape(3, rows(true));
    let r = checks::evaluate(&sc, &t, Some(&twin), None, None);
    assert_eq!(status(&r, CheckId::Twin), Status::Pass, "{}", show(&r, &t));
    let t = shape(3, rows(false));
    let r = checks::evaluate(&sc, &t, Some(&twin), None, None);
    assert!(fails_on(&r, CheckId::Twin, "row", "x"), "{}", show(&r, &t));
}

/// (eval-coverage-2) A node clipped out by an ancestor (`hidden:
/// clipped`) does not show: clipped for one frame and back, it vanished.
#[test]
fn a_node_clipped_for_a_frame_and_back_vanishes() {
    let sc = scenario("");
    let t = shape(6, |k| {
        let mut a = row("a", [10.0, 10.0, 10.0, 10.0]);
        if k == 2 {
            a.hidden = Some("clipped".into());
        }
        vec![a]
    });
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("vanish") && f.k == Some(2)),
        "{}",
        show(&r, &t)
    );
}

/// (eval-coverage-2) A row a scroll carries past its viewport by itself
/// (`SCROLL`) may show for one frame; and a step that acts inside a
/// segment, not only its first, takes a node out (a second `scroll`).
#[test]
fn a_flash_under_a_scroll_or_a_later_step_passes() {
    let t = blink(|k| k != 3, Motions::SCROLL);
    assert_eq!(
        status(&check(&scenario(""), &t), CheckId::Vanish),
        Status::Pass
    );
    let text = "id = \"synth\"\n=== shell.strand\nbar Top { height: 30 }\n=== steps\nrest\npointer 5,5\nscroll 1\nscroll -1\nrest\n";
    let file = eval_root().join("scenarios/regressions/synth.scn");
    let sc = Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"));
    let line = |s: &str| sc.steps.iter().find(|st| st.text == s).unwrap().line;
    let mut t = blink(|k| k != 3, Motions(0));
    assert!(
        check(&sc, &t)
            .failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("flash")),
        "a flash with no step"
    );
    for (k, s) in [(3, "scroll 1"), (4, "scroll -1")] {
        t.events.push(EventRec {
            k,
            line: line(s),
            text: s.into(),
            segment: Some(1),
        });
    }
    t.seal();
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
}

/// (eval-coverage-2) A list's rows scrolling (`SCROLL`, inherited) are
/// laid out at the offset each frame, laid where drawn: still held to
/// each other's edges.
#[test]
fn tiling_holds_rows_under_a_scroll() {
    let sc = scenario("");
    let mut t = flush(42.4, 43.2);
    for f in &mut t.frames {
        for r in &mut f.n {
            r.emot = Motions::SCROLL.0;
        }
    }
    t.seal();
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Tiling, "a", "x") || fails_on(&r, CheckId::Tiling, "b", "x"),
        "{}",
        show(&r, &t)
    );
}

/// (eval-coverage-2) A step that takes time acts on every tick of its
/// span: a node gone at the swipe's second tick and back at its third (a
/// 50 ms swipe is three input ticks at 60 Hz, after the tick it starts
/// at) was moved by the fingers, not lost; the same blink once the swipe
/// has ended still vanishes.
#[test]
fn a_timed_step_acts_over_its_whole_span() {
    let text = "id = \"synth\"\n=== shell.strand\nbar Top { height: 30 }\n=== steps\nrest\npointer 5,5\nswipe 60 over 50ms\nrest\n";
    let file = eval_root().join("scenarios/regressions/synth.scn");
    let sc = Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"));
    let line = sc
        .steps
        .iter()
        .find(|st| st.text.starts_with("swipe"))
        .unwrap()
        .line;
    let with_swipe = |gone: u32| {
        let mut t = blink(move |k| k == gone, Motions(0));
        t.events.push(EventRec {
            k: 1,
            line,
            text: "swipe 60 over 50ms".into(),
            segment: None,
        });
        t.seal();
        t
    };
    let t = with_swipe(3);
    let r = check(&sc, &t);
    assert_eq!(
        status(&r, CheckId::Vanish),
        Status::Pass,
        "{}",
        show(&r, &t)
    );
    let t = with_swipe(5);
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Vanish)
            .iter()
            .any(|f| f.msg.starts_with("vanish")),
        "{}",
        show(&r, &t)
    );
}

// --- (eval-rules-2) continuity, contain's held and shadow, coherence's drag

use std::time::Duration;

use strand_eval::paint::PaintRec;
use strand_eval::trace::{CurveRec, PtrRec, RollRec, SpringRec, StartRec};
use strand_scene::motion::{Curve, Motion, MotionView, Spring};

/// A frame's time in [`shape`]'s traces.
fn tick(k: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 + u64::from(k) * 16_666_667)
}

/// What the probe records of a sample: the motion before it (`pre`) and
/// the value drawn.
fn sample_rec(key: &str, of: &str, pre: &MotionView<1>, v: [f32; 1]) -> SpringRec {
    let (to, c) = match pre.pending {
        Some((t, c, _)) => (t, c),
        None => (pre.target, pre.curve),
    };
    SpringRec {
        key: key.into(),
        s: 1,
        of: of.into(),
        v: v.to_vec(),
        to: to.to_vec(),
        c: CurveRec::of(&c),
        eps: pre.eps,
        rest: (pre.curve == Curve::Instant).then(|| pre.target.to_vec()),
        start: pre.pending.map(|(_, _, shift)| StartRec {
            last: pre.last.map(|l| l.as_nanos() as u64),
            shift: if shift == [0.0] {
                Vec::new()
            } else {
                shift.to_vec()
            },
        }),
        ..SpringRec::default()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// A retarget that starts again from rest where it was.
    FromRest,
    /// A change started `START_LEAD` before its first frame on a 144 Hz
    /// surface (as at 60 Hz).
    LeadMax,
    /// The way in flight dropped for a fresh one at rest at zero.
    Dropped,
    /// The last frame drawn one frame short of settling.
    Short,
}

/// A pill's `x` springing to 180 from tick 1, sent back to 0 at tick 4,
/// sampled as the renderer samples it (`Motion::sample_frame`, each
/// tick's surface period `period`), drawn wrong as `fault` says.
fn pill_springs(fault: Fault, period: Duration) -> RunTrace {
    let curve = Curve::Spring(Spring::new(170.0, 0.8).unwrap());
    let mut m = Motion::rest([0.0], 0.05).sampled_at(Some(tick(0)));
    let end = 240;
    let mut recs: Vec<(SpringRec, bool)> = Vec::new();
    for k in 0..=end {
        match k {
            1 => m.retarget([180.0], curve),
            4 => m.retarget([0.0], curve),
            _ => {}
        }
        let pre = m.view();
        let lead = if fault == Fault::LeadMax && k == 1 {
            Duration::from_micros(16_667)
        } else {
            period
        };
        if k == 4 && fault == Fault::FromRest {
            // The velocity thrown away: a fresh way from where it is.
            let here = m.peek(tick(3));
            m = Motion::rest(here, 0.05).sampled_at(Some(tick(3)));
            m.retarget([0.0], curve);
        }
        if k == 4 && fault == Fault::Dropped {
            m = Motion::rest([0.0], 0.05).sampled_at(Some(tick(3)));
        }
        let pre = if k == 4 && fault == Fault::Dropped {
            m.view()
        } else {
            pre
        };
        let mut v = m.sample_frame(tick(k), lead);
        let moving = !m.is_settled(tick(k));
        let mut rec = sample_rec("pill", "x", &pre, v);
        if fault == Fault::Short && !moving && pre.curve != Curve::Instant {
            v = [v[0] + 0.3];
            rec.v = v.to_vec();
        }
        recs.push((rec, moving));
        if !moving && k > 4 {
            break;
        }
    }
    assert!(!recs.last().unwrap().1, "settles within the trace");
    let n = recs.len() as u32 - 1;
    let mut t = shape(n, |_| vec![row("pill", [10.0, 8.0, 40.0, 12.0])]);
    for (f, (rec, moving)) in t.frames.iter_mut().zip(recs) {
        f.period.insert(1, period.as_nanos() as u64);
        if moving {
            f.wants = vec![1];
        }
        if f.k > 0 {
            f.sp.push(rec);
        }
    }
    t.frames[0].sp.clear();
    t.seal();
    t
}

fn continuity_fails_with(t: &RunTrace, word: &str) -> bool {
    let sc = scenario("");
    check(&sc, t)
        .failures(CheckId::Continuity)
        .iter()
        .any(|f| f.key.as_deref() == Some("pill") && f.msg.starts_with(word))
}

/// The way a renderer samples, retargeted mid-flight and settled, is
/// the way the check works out, at 60 and 144 Hz.
#[test]
fn continuity_passes_a_spring_retargeted_mid_flight() {
    let sc = scenario("");
    for period in [
        Duration::from_nanos(16_666_667),
        Duration::from_nanos(6_944_444),
    ] {
        let t = pill_springs(Fault::None, period);
        assert!(t.frames.iter().filter(|f| !f.sp.is_empty()).count() > 10);
        let r = check(&sc, &t);
        assert_eq!(
            status(&r, CheckId::Continuity),
            Status::Pass,
            "{}",
            show(&r, &t)
        );
    }
}

/// design.md: "Retargeting mid-flight keeps velocity, so interrupted
/// animations never jolt".
#[test]
fn continuity_fails_a_retarget_that_starts_again_from_rest() {
    let t = pill_springs(Fault::FromRest, Duration::from_nanos(16_666_667));
    assert!(continuity_fails_with(&t, "velocity"), "{}", {
        let sc = scenario("");
        show(&check(&sc, &t), &t)
    });
}

/// A way in flight replaced by one at rest is a jump, not a retarget.
#[test]
fn continuity_fails_a_way_dropped_in_flight() {
    let t = pill_springs(Fault::Dropped, Duration::from_nanos(16_666_667));
    assert!(continuity_fails_with(&t, "velocity"), "{}", {
        let sc = scenario("");
        show(&check(&sc, &t), &t)
    });
}

/// decisions.md first-frame-lead: a change starts one refresh of its
/// surface before its first frame, at 144 Hz 6.9 ms, not 16.7.
#[test]
fn continuity_fails_a_change_started_a_60_hz_frame_early_at_144_hz() {
    let t = pill_springs(Fault::LeadMax, Duration::from_nanos(6_944_444));
    assert!(continuity_fails_with(&t, "start"), "{}", {
        let sc = scenario("");
        show(&check(&sc, &t), &t)
    });
}

/// A surface that stops wanting frames shows its last frame: a motion
/// drawn short of its target there stays short.
#[test]
fn continuity_fails_a_motion_left_short_of_its_target() {
    let t = pill_springs(Fault::Short, Duration::from_nanos(16_666_667));
    let sc = scenario("");
    let r = check(&sc, &t);
    assert!(
        r.failures(CheckId::Continuity)
            .iter()
            .any(|f| f.msg.contains("short of its target")),
        "{}",
        show(&r, &t)
    );
}

/// A node drawn for the first time springs only what its enter pose sets
/// (decisions.md m4-effects-finish).
#[test]
fn continuity_fails_a_new_node_springing_a_prop_its_pose_does_not_set() {
    let sc = scenario("");
    let curve = Curve::Spring(Spring::new(170.0, 0.8).unwrap());
    for (pose_sets_it, fails) in [(true, false), (false, true)] {
        let mut m = Motion::rest([0.0], 0.002).sampled_at(Some(tick(0)));
        m.retarget([1.0], curve);
        let pre = m.view();
        let v = m.sample_frame(tick(1), Duration::from_nanos(16_666_667));
        let mut rec = sample_rec("pill", "opacity", &pre, v);
        rec.enter = Some(pose_sets_it);
        let mut t = shape(1, |_| vec![row("pill", [10.0, 8.0, 40.0, 12.0])]);
        t.frames[1].sp.push(rec);
        t.frames[1].wants = vec![1];
        t.seal();
        let r = check(&sc, &t);
        assert_eq!(
            r.failures(CheckId::Continuity)
                .iter()
                .any(|f| f.msg.starts_with("first frame")),
            fails,
            "{}",
            show(&r, &t)
        );
    }
}

/// An interrupted roll rolls on from the nearer of its two texts
/// (design.md "Rolling numbers", effects/roll.rs).
#[test]
fn continuity_holds_an_interrupted_roll_to_the_text_on_screen() {
    let sc = scenario("");
    let roll = |p_before: f32, was: f32, from: &str| {
        let mut t = shape(2, |_| vec![row("num", [10.0, 8.0, 20.0, 14.0])]);
        t.frames[1].rolls.push(RollRec {
            key: "num".into(),
            from: Some("41".into()),
            to: Some("42".into()),
            p: p_before,
            was: None,
        });
        t.frames[2].rolls.push(RollRec {
            key: "num".into(),
            from: Some(from.into()),
            to: Some("43".into()),
            p: 0.1,
            was: Some((Some("41".into()), Some("42".into()), was)),
        });
        t.seal();
        check(&sc, &t)
    };
    // A fifth of the way: "41" is on screen.
    assert_eq!(
        status(&roll(0.2, 0.2, "41"), CheckId::Continuity),
        Status::Pass
    );
    let r = roll(0.2, 0.2, "42");
    assert!(
        r.failures(CheckId::Continuity)
            .iter()
            .any(|f| f.msg.starts_with("roll")),
        "{:?}",
        r.failures(CheckId::Continuity)
    );
    // Nine tenths: "42".
    assert_eq!(
        status(&roll(0.85, 0.9, "42"), CheckId::Continuity),
        Status::Pass
    );
    assert!(
        !roll(0.85, 0.9, "41")
            .failures(CheckId::Continuity)
            .is_empty()
    );
    // The frame before showed the other side of half way: either text.
    assert_eq!(
        status(&roll(0.45, 0.55, "41"), CheckId::Continuity),
        Status::Pass
    );
}

/// A row collapsing (its height springing 30 to 6) over a 20 px text:
/// clipped, or not.
fn collapsing(clipped: bool, exit_pose: bool, rotate: f32) -> RunTrace {
    let mut t = shape(6, |k| {
        let h = 30.0 - 4.0 * k as f32;
        let mut toast = row("toast", [10.0, 0.0, 200.0, h]);
        toast.rotate = rotate;
        let m = if exit_pose {
            Motions::EXIT
        } else {
            Motions::SIZE_SPRING
        };
        toast = toast.mot(m);
        let mut text = row("text", [20.0, 5.0, 80.0, 20.0]);
        if clipped {
            text.clip = Some([10.0, 0.0, 200.0, h]);
        }
        text.mot = if exit_pose { Motions::EXIT.0 } else { 0 };
        vec![toast, text]
    });
    t.nodes.get_mut("text").unwrap().parent = Some("toast".into());
    if exit_pose {
        t.nodes.get_mut("toast").unwrap().exit = Some(Pose {
            h: Some(0.0),
            ..Pose::default()
        });
    }
    t.seal();
    t
}

/// decisions.md wave3-pixels p2: "a node whose size springs clips its
/// content while it does (a collapsing toast hides what no longer
/// fits)", and exit-height0: one exiting to a size clips until it goes.
#[test]
fn contain_holds_what_a_collapsing_box_lays_out_inside_it() {
    let sc = scenario("");
    for exit_pose in [false, true] {
        let t = collapsing(true, exit_pose, 0.0);
        let r = check(&sc, &t);
        assert_eq!(
            status(&r, CheckId::Contain),
            Status::Pass,
            "{}",
            show(&r, &t)
        );
        let t = collapsing(false, exit_pose, 0.0);
        let r = check(&sc, &t);
        assert!(
            r.failures(CheckId::Contain)
                .iter()
                .any(|f| f.key.as_deref() == Some("text") && f.msg.starts_with("held")),
            "{}",
            show(&r, &t)
        );
    }
    // Turned, its clip reaches the trace as a bounding box: not held.
    let t = collapsing(false, false, 8.0);
    let r = check(&sc, &t);
    assert!(
        !r.failures(CheckId::Contain)
            .iter()
            .any(|f| f.msg.starts_with("held")),
        "{}",
        show(&r, &t)
    );
}

/// A card at rest with a shadow reaching `reach` px past its left edge
/// on a bar whose buffer starts at the card's left edge.
fn shadowed(reach: f32, cuts: u8, m: Motions) -> RunTrace {
    let mut t = shape(2, |_| {
        let mut card = row("card", [0.0, 4.0, 100.0, 20.0]).mot(m);
        card.paint = Some(PaintRec {
            sh: vec![format!("00000080@{},0,{},28", -reach, 100.0 + 2.0 * reach)],
            ..PaintRec::default()
        });
        vec![card]
    });
    for f in &mut t.frames {
        if cuts != 0 {
            f.cuts.insert(1, cuts);
        }
    }
    t.seal();
    t
}

/// design.md: "Shadows enlarge the buffer but not the input region": a
/// shadow at rest is drawn inside its buffer, but on a side the
/// author's geometry lays at the output's edge.
#[test]
fn contain_holds_a_shadow_at_rest_inside_its_buffer() {
    let sc = scenario("");
    let r = check(&sc, &shadowed(0.0, 0, Motions(0)));
    assert_eq!(status(&r, CheckId::Contain), Status::Pass);
    let t = shadowed(6.0, 0, Motions(0));
    let r = check(&sc, &t);
    assert!(
        fails_on(&r, CheckId::Contain, "card", "left"),
        "{}",
        show(&r, &t)
    );
    // Its left side anchored where the margin caps the overhang.
    let r = check(&sc, &shadowed(6.0, 8, Motions(0)));
    assert_eq!(status(&r, CheckId::Contain), Status::Pass);
    // Gliding: the overhang is the one at rest (m6-owner-fixes bug 9).
    let r = check(&sc, &shadowed(6.0, 0, Motions::GLIDE));
    assert_eq!(status(&r, CheckId::Contain), Status::Pass);
}

/// decisions.md wave3-pixels p3: a dragged slider draws the drag, its
/// knob under the pointer.
#[test]
fn coherence_holds_a_dragged_knob_under_the_pointer() {
    let sc = scenario("");
    let slider = |knob_x: f32, pointer: f32| {
        let mut t = shape(2, |_| {
            let mut s = row("knob", [10.0, 7.0, 120.0, 16.0]);
            s.paint = Some(PaintRec {
                pt: vec![
                    "0,6,120,4".into(),
                    "0,6,40,4".into(),
                    format!("{knob_x},0,16,16"),
                ],
                ..PaintRec::default()
            });
            vec![s]
        });
        for f in &mut t.frames {
            f.ptr = Some(PtrRec {
                s: 1,
                at: [pointer, 15.0],
            });
            f.drags.insert("knob".into(), 0.3);
        }
        t.seal();
        check(&sc, &t)
    };
    // Knob centre 10 + 32 + 8 = 50 under a pointer at 50.
    assert_eq!(
        status(&slider(32.0, 50.0), CheckId::Coherence),
        Status::Pass
    );
    let r = slider(26.0, 50.0);
    assert!(
        r.failures(CheckId::Coherence)
            .iter()
            .any(|f| f.msg.starts_with("drag")),
        "{:?}",
        r.failures(CheckId::Coherence)
    );
    // Past the end, the knob stops a radius in.
    assert_eq!(
        status(&slider(104.0, 200.0), CheckId::Coherence),
        Status::Pass
    );
}

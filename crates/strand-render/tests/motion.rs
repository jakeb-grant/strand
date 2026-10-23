//! Animation offline: springs sampled at fixed presentation timestamps
//! (deterministic, so frames compare with reference PNGs), retargeting
//! that keeps velocity, size springs, FLIP glides, enter and exit poses,
//! the snap rules and true idle once everything settles (design.md,
//! "Layout, animation and input"; "Testing": deterministic springs).

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::motion::{EFFECTS, channels_color, color_channels};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

/// The presentation time of frame `k` after `T0` at 60 Hz.
fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

fn pose(props: Vec<(Prop, PropValue)>) -> PropValue {
    PropValue::Pose(props)
}

/// The peak overshoot of the default `$motion.spatial`, spring(3000,
/// 0.75), released from rest: `exp(-ζπ/√(1-ζ²))` = 2.84%, plus a small
/// epsilon (decisions.md m6-owner, 2026-10-14).
const SPATIAL_OVERSHOOT: f32 = 0.0284 + 0.002;

/// Whole-pixel positions of an edge springing from `from` to `to` along
/// `$motion.spatial`: it ends at `to`, passes it by no more than the
/// spring's peak overshoot (and a pixel of rounding), and turns back at
/// most once.
fn assert_springs_to(xs: &[i32], from: i32, to: i32) {
    assert_eq!(xs.last(), Some(&to), "ends at its target: {xs:?}");
    let reach = ((to - from).abs() as f32 * SPATIAL_OVERSHOOT).ceil() as i32 + 1;
    let past = |x: i32| if to >= from { x - to } else { to - x };
    assert!(
        xs.iter().all(|&x| past(x) <= reach),
        "overshoots by more than {reach}: {xs:?}"
    );
    let turns = xs
        .windows(2)
        .map(|w| (w[1] - w[0]).signum())
        .filter(|s| *s != 0)
        .collect::<Vec<_>>()
        .windows(2)
        .filter(|s| s[0] != s[1])
        .count();
    assert!(turns <= 1, "turns back {turns} times: {xs:?}");
}

struct Stage {
    r: Renderer,
    buf: Buffer,
    root: NodeId,
}

impl Stage {
    /// A `w × h` panel holding what `build` adds, painted once at `T0`
    /// (so it is on screen with a clock: changes from now on animate).
    fn new(w: u32, h: u32, build: impl FnOnce(&mut Builder, NodeId)) -> Self {
        Self::with(renderer(), w, h, build)
    }

    /// [`Stage::new`] on the renderer given.
    fn with(mut r: Renderer, w: u32, h: u32, build: impl FnOnce(&mut Builder, NodeId)) -> Self {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Width, num(w as f32)),
                (Prop::Height, num(h as f32)),
                (Prop::Bg, color("#1e1e2e")),
            ],
        );
        build(&mut b, root);
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(w, h, Scale::ONE);
        assert!(!buf.paint_at(&mut r, S, 0, T0).is_empty());
        assert!(!r.wants_frame(S), "at rest after the first frame");
        Stage { r, buf, root }
    }

    fn apply(&mut self, d: SceneDiff) {
        assert!(self.r.apply(d).is_empty());
    }

    fn paint(&mut self, t: Duration) -> Damage {
        self.buf.paint_at(&mut self.r, S, 1, t)
    }

    /// Paints frames from `k` on until nothing moves; returns the frame
    /// after the last one painted.
    fn settle(&mut self, mut k: u32) -> u32 {
        while self.r.wants_frame(S) {
            self.paint(frame(k));
            k += 1;
            assert!(k < 400, "never settled");
        }
        k
    }

    /// The first column of row `y` whose red channel is over 128 (a red
    /// box on the dark background).
    fn red_from(&self, y: u32) -> Option<u32> {
        (0..self.buf.size.w).find(|x| self.buf.px(*x, y)[2] > 128)
    }

    /// The first row of column `x` whose red channel is over 128.
    fn red_top(&self, x: u32) -> Option<u32> {
        (0..self.buf.size.h).find(|y| self.buf.px(x, *y)[2] > 128)
    }

    fn rect(&self, id: NodeId) -> LogicalRect {
        self.r.boxes(S).unwrap().rects[&id]
    }
}

fn red_box(b: &mut Builder, parent: NodeId, extra: Vec<(Prop, PropValue)>) -> NodeId {
    let mut props = vec![(Prop::Bg, color("#ff0000")), (Prop::Size, num(20.0))];
    props.extend(extra);
    b.node(NodeKind::Box, Some(parent), props)
}

/// Movement springs with `$motion.spatial`, colour with
/// `$motion.effects` (the design's springs when the shell names no
/// `motion` tokens); frames at fixed timestamps are the same every run
/// and match the reference filmstrip.
#[test]
fn springs_sampled_at_fixed_timestamps_match_reference() {
    let mut id = None;
    let mut st = Stage::new(160, 24, |b, root| {
        id = Some(red_box(b, root, vec![(Prop::Margin, num(2.0))]));
    });
    let id = id.unwrap();
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(110.0))
        .set(id, Prop::Bg, color("#3060ff"))
        .set(id, Prop::Radius, num(10.0));
    st.apply(d);
    assert!(st.r.wants_frame(S), "a change asks for frames");
    let frames = [1, 2, 3, 5, 8, 13, 21, 60];
    let mut strip = Buffer::new(160, 24 * frames.len() as u32, Scale::ONE);
    let mut xs = Vec::new();
    for (i, k) in frames.iter().enumerate() {
        st.paint(frame(*k));
        let row = 160 * 24 * 4;
        strip.pixels[i * row..(i + 1) * row].copy_from_slice(&st.buf.pixels);
        // The box's left edge: wherever red or blue is strong.
        let x = (0..160u32)
            .find(|x| {
                let p = st.buf.px(*x, 12);
                p[0] > 100 || p[2] > 100
            })
            .unwrap();
        xs.push(x);
    }
    // From x 2 to 112: past it by at most the spring's peak overshoot,
    // turning back once, and at rest on it.
    let xs: Vec<i32> = xs.iter().map(|x| *x as i32).collect();
    assert!(xs[0] > 2, "the first frame already moves: {xs:?}");
    assert_springs_to(&xs, 2, 112);
    assert!(!st.r.wants_frame(S), "settled by frame 60");
    assert_matches_ref("motion_springs", &strip, 3);

    // The same changes at the same timestamps on a fresh renderer give
    // the same pixels.
    let mut again = Stage::new(160, 24, |b, root| {
        red_box(b, root, vec![(Prop::Margin, num(2.0))]);
    });
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(110.0))
        .set(id, Prop::Bg, color("#3060ff"))
        .set(id, Prop::Radius, num(10.0));
    again.apply(d);
    for (i, k) in frames.iter().enumerate() {
        again.paint(frame(*k));
        let row = 160 * 24 * 4;
        assert!(
            again.buf.pixels == strip.pixels[i * row..(i + 1) * row],
            "frame {k} differs"
        );
    }
}

/// An animation interrupted mid-flight keeps its velocity: it carries on
/// the way it was going for a moment, then turns, never jumping.
#[test]
fn retargeting_mid_flight_keeps_velocity() {
    let mut id = None;
    let mut st = Stage::new(200, 20, |b, root| id = Some(red_box(b, root, vec![])));
    let id = id.unwrap();
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(150.0));
    st.apply(d);
    // Two frames in, $motion.spatial is 64% of the way (from 26% at the
    // first): mid-flight.
    let mut xs = Vec::new();
    for k in 1..=2 {
        st.paint(frame(k));
        xs.push(st.red_from(10).unwrap() as i32);
    }
    // Moving right fast.
    assert!(xs[1] - xs[0] >= 5, "{xs:?}");
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(20.0));
    st.apply(d);
    st.paint(frame(3));
    let after = st.red_from(10).unwrap() as i32;
    assert!(
        after > xs[1],
        "kept going right after the retarget: {xs:?} then {after}"
    );
    // No jolt: the step is no bigger than the last one before it.
    assert!(after - xs[1] <= xs[1] - xs[0] + 1);
    st.settle(4);
    assert_eq!(st.red_from(10), Some(20));
}

/// After the last spring settles, an idle surface does no work: it asks
/// for no frames, a paint draws nothing and nothing is laid out.
#[test]
fn an_idle_bar_does_no_work_after_animations_settle() {
    let mut id = None;
    let mut st = Stage::new(120, 24, |b, root| {
        id = Some(red_box(b, root, vec![(Prop::Margin, num(2.0))]));
    });
    let id = id.unwrap();
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(60.0))
        .set(id, Prop::Opacity, num(0.5))
        .set(id, Prop::Bg, color("#00ff00"))
        .set(id, Prop::Width, num(40.0));
    st.apply(d);
    let end = st.settle(1);
    assert!(end < 120, "settled in {end} frames");
    assert!(!st.r.animating(S));
    let passes = st.r.layout_passes();
    for k in 0..30 {
        st.r.update();
        assert!(!st.r.wants_frame(S));
        assert!(st.r.frame_deadline(S).is_none());
        let damage = st.paint(frame(end + k));
        assert!(damage.is_empty(), "frame {k}: {damage:?}");
    }
    assert_eq!(st.r.layout_passes(), passes, "no layout once settled");
}

/// Paint-only props (`x`, `y`, `scale`, `rotate`, `opacity`) spring
/// without a single layout pass.
#[test]
fn paint_only_springs_never_relayout() {
    let mut id = None;
    let mut st = Stage::new(120, 40, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![]);
        id = Some(red_box(b, row, vec![]));
    });
    let id = id.unwrap();
    let passes = st.r.layout_passes();
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(50.0))
        .set(id, Prop::Y, num(10.0))
        .set(id, Prop::Scale, num(1.5))
        .set(id, Prop::Rotate, PropValue::Angle(30.0))
        .set(id, Prop::Opacity, num(0.4));
    st.apply(d);
    let mut moved = 0;
    for k in 1..40 {
        if !st.paint(frame(k)).is_empty() {
            moved += 1;
        }
    }
    assert!(moved > 10, "{moved} frames drew");
    assert_eq!(
        st.r.layout_passes(),
        passes,
        "a paint-only spring relaid out"
    );
}

/// `width: 24` on an 8 px dot springs its laid-out size; each frame lays
/// out only the subtree under its nearest size-stable ancestor (a box of
/// fixed width and height), not the whole surface.
#[test]
fn size_springs_relayout_only_under_the_nearest_size_stable_ancestor() {
    // The bar's Dot: a fixed-size cell holding the dot and a sibling,
    // then six labels.
    let build = || {
        let mut ids = Vec::new();
        let st = Stage::new(240, 40, |b, root| {
            let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Gap, num(4.0))]);
            let cell = b.node(
                NodeKind::Row,
                Some(row),
                vec![(Prop::Width, num(60.0)), (Prop::Height, num(30.0))],
            );
            ids.push(b.node(
                NodeKind::Box,
                Some(cell),
                vec![(Prop::Size, num(8.0)), (Prop::Bg, color("#ff0000"))],
            ));
            ids.push(b.node(NodeKind::Box, Some(cell), vec![(Prop::Size, num(8.0))]));
            for _ in 0..6 {
                let c = b.node(NodeKind::Col, Some(row), vec![]);
                ids.push(b.node(NodeKind::Text, Some(c), vec![(Prop::Text, text("label"))]));
            }
        });
        (st, ids)
    };
    let (mut st, ids) = build();
    let (dot, sib, label) = (ids[0], ids[1], ids[2]);
    let full = st.r.boxes(S).unwrap().rects.len();
    let label_at = st.rect(label);
    let mut d = SceneDiff::new();
    d.set(dot, Prop::Width, num(24.0));
    st.apply(d);
    // The same change on a twin that lays the whole surface out every
    // frame (a layout length set to what it is forces a full pass).
    let (mut twin, _) = build();
    let mut d = SceneDiff::new();
    d.set(dot, Prop::Width, num(24.0));
    twin.apply(d);
    let mut widths = Vec::new();
    let mut partial = Vec::new();
    for k in 1..=12 {
        st.paint(frame(k));
        let mut d = SceneDiff::new();
        d.set(label, Prop::Pad, num(0.0));
        twin.apply(d);
        twin.paint(frame(k));
        widths.push(st.rect(dot).w);
        partial.push(st.r.last_layout_nodes());
        // The sibling follows the dot inside the cell, and the subtree
        // laid out alone matches the full layout.
        assert_eq!(st.rect(sib).x, st.rect(dot).x + st.rect(dot).w, "frame {k}");
        for id in [dot, sib, label] {
            let (a, b) = (st.rect(id), twin.rect(id));
            assert!(
                (a.x - b.x).abs() < 0.01
                    && (a.y - b.y).abs() < 0.01
                    && (a.w - b.w).abs() < 0.01
                    && (a.h - b.h).abs() < 0.01,
                "frame {k}, {id:?}: {a:?} vs full {b:?}"
            );
        }
        assert_eq!(st.rect(label), label_at, "outside the cell nothing moves");
    }
    assert!(widths[0] > 8.0 && widths[0] < 24.0, "{widths:?}");
    assert!(widths[5] > widths[0], "{widths:?}");
    // After the first frame (which lays the whole surface out once for
    // the change), each frame lays out the cell's subtree only (the cell
    // and its two children, once: the springs' targets are kept).
    assert!(
        partial[2..].iter().all(|n| *n <= 3 && *n < full),
        "{partial:?} of {full}"
    );
    st.settle(13);
    assert_eq!(st.rect(dot).w, 24.0);
    assert_eq!(
        st.rect(sib).x,
        st.rect(dot).x + 24.0,
        "the sibling moved with it"
    );
    assert!(!st.r.wants_frame(S));
}

/// A keyed reorder glides each item from its old slot to its new one
/// (FLIP); the boxes are laid out at once, the paint offsets spring.
#[test]
fn keyed_reorder_glides_to_new_slots() {
    let mut ids = Vec::new();
    let mut st = Stage::new(40, 80, |b, root| {
        let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Gap, num(4.0))]);
        ids.push(red_box(b, col, vec![]));
        ids.push(b.node(
            NodeKind::Box,
            Some(col),
            vec![(Prop::Size, num(20.0)), (Prop::Bg, color("#00ff00"))],
        ));
        ids.push(b.node(
            NodeKind::Box,
            Some(col),
            vec![(Prop::Size, num(20.0)), (Prop::Bg, color("#0000ff"))],
        ));
    });
    let col = st.r.tree().get(ids[0]).unwrap().parent.unwrap();
    assert_eq!(st.red_top(10), Some(0));
    // The red box moves to the end.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Move {
        id: ids[0],
        parent: Some(col),
        index: 2,
    });
    st.apply(d);
    st.paint(frame(1));
    assert_eq!(st.rect(ids[0]).y, 48.0, "laid out in its new slot at once");
    // The first frame is one refresh along $motion.spatial, 26% of the
    // 48 px move (12.5 px): drawn from its old slot, not in its new one.
    let first = st.red_top(10).unwrap();
    assert!(first <= 13, "still near its old slot: {first}");
    let mut tops = vec![first as i32];
    let mut k = 2;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        tops.push(st.red_top(10).unwrap() as i32);
        k += 1;
        assert!(k < 400, "never settled");
    }
    assert_springs_to(&tops, 0, 48);
    assert_eq!(st.red_top(10), Some(48));
}

/// (decisions.md first-frame-lead) A change starts one refresh of its
/// surface before the first frame that shows it: on a 144 Hz surface
/// that sat idle, a glide's first frame shows one 144 Hz frame of
/// progress, not START_LEAD's 16.7 ms (2.4 frames in one). And it does
/// not matter whether the idle frames drew nothing (buffer age 1, the
/// CPU's damage-free frames) or the whole frame again (age 0, as a
/// GPU-presented surface paints): both draw the same first frames.
#[test]
fn a_glide_after_idle_frames_leads_by_one_refresh_at_any_buffer_age() {
    let period = Duration::from_nanos(1_000_000_000 / 144);
    let tick = |k: u32| T0 + period * k;
    // The red box's top, to a fraction of a pixel: the coverage-weighted
    // centre of its red in column 40 (where only it and the background,
    // red 0x1e, ever are), less half the box.
    let top = |st: &Stage| {
        let (mut sum, mut weight) = (0.0f64, 0.0f64);
        for y in 0..st.buf.size.h {
            let w = (f64::from(st.buf.px(40, y)[2]) - 30.0) / 225.0;
            sum += (f64::from(y) + 0.5) * w;
            weight += w;
        }
        (sum / weight - 10.0) as f32
    };
    let run = |age: u8| {
        let mut ids = Vec::new();
        let mut st = Stage::new(60, 80, |b, root| {
            let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Gap, num(4.0))]);
            ids.push(red_box(b, col, vec![(Prop::Width, num(50.0))]));
            for c in ["#00ff00", "#0000ff"] {
                ids.push(b.node(
                    NodeKind::Box,
                    Some(col),
                    vec![(Prop::Size, num(20.0)), (Prop::Bg, color(c))],
                ));
            }
        });
        let col = st.r.tree().get(ids[0]).unwrap().parent.unwrap();
        // Idle frames at 144 Hz: the loop runs, nothing changes.
        for k in 1..=12 {
            st.buf.paint_at(&mut st.r, S, age, tick(k));
        }
        let mut d = SceneDiff::new();
        d.push(SceneOp::Move {
            id: ids[0],
            parent: Some(col),
            index: 2,
        });
        st.apply(d);
        (13..=18)
            .map(|k| {
                st.buf.paint_at(&mut st.r, S, age, tick(k));
                top(&st)
            })
            .collect::<Vec<_>>()
    };
    let (drawn_nothing, drawn_again) = (run(1), run(0));
    assert_eq!(
        drawn_nothing, drawn_again,
        "the idle frames' buffer age changes the glide"
    );
    // The glide (48 px back to its new slot along $motion.spatial),
    // started one refresh before the first frame; drawn at whole pixels.
    let mut glide = Motion::<1>::rest([0.0], 0.01).sampled_at(Some(tick(12)));
    glide.shift([-48.0], Curve::Spring(motion::SPATIAL));
    let want: Vec<f32> = (13..=18).map(|k| 48.0 + glide.sample(tick(k))[0]).collect();
    for (k, (got, at)) in drawn_nothing.iter().zip(&want).enumerate() {
        assert!(
            (got - at).abs() <= 0.5,
            "frame {k}: drawn at {got}, one refresh's lead puts it at {at} \
             ({drawn_nothing:?} against {want:?})"
        );
    }
}

/// A node created on a shown surface plays its `enter` pose; a removed
/// one plays `exit` (here collapsing to `height: 0`, so the sibling
/// below slides up to fill the gap), then unmounts. Its id is dead at
/// once: logic reuses the slot while the ghost still plays.
#[test]
fn enter_and_exit_poses_and_siblings_slide_to_fill_the_gap() {
    let mut ids = Vec::new();
    let mut st = Stage::new(80, 100, |b, root| {
        let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Gap, num(4.0))]);
        for c in ["#00ff00", "#0000ff", "#ff0000"] {
            ids.push(b.node(
                NodeKind::Box,
                Some(col),
                vec![
                    (Prop::Height, num(20.0)),
                    (Prop::Bg, color(c)),
                    (
                        Prop::Exit,
                        pose(vec![
                            (Prop::X, num(80.0)),
                            (Prop::Opacity, num(0.0)),
                            (Prop::Height, num(0.0)),
                        ]),
                    ),
                ],
            ));
        }
    });
    let col = st.r.tree().get(ids[0]).unwrap().parent.unwrap();
    assert_eq!(st.red_top(10), Some(48));
    // Remove the middle (blue) one.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: ids[1],
        window: false,
    });
    st.apply(d);
    assert!(st.r.tree().is_ghost(ids[1]), "it plays its exit");
    // Logic reuses the slot at once with a new generation.
    let reused = NodeId::new(ids[1].index, ids[1].generation + 1);
    let mut d = SceneDiff::new();
    d.create(reused, NodeKind::Box, Some(col), 3)
        .set(reused, Prop::Height, num(10.0));
    st.apply(d);
    let mut tops = Vec::new();
    for k in 1..=10 {
        st.paint(frame(k));
        tops.push(st.red_top(10).unwrap());
    }
    assert!(tops[0] < 48 && tops[0] > 24, "{tops:?}");
    assert!(tops.windows(2).all(|w| w[0] >= w[1]), "slides up: {tops:?}");
    let end = st.settle(11);
    assert_eq!(st.r.tree().ghost_count(), 0, "unmounted after its exit");
    assert_eq!(st.red_top(10), Some(24));

    // Enter: a node created now starts from its pose.
    let new = NodeId::new(20, 0);
    let mut d = SceneDiff::new();
    d.create(new, NodeKind::Box, Some(col), 0)
        .set(new, Prop::Height, num(20.0))
        .set(new, Prop::Bg, color("#ffffff"))
        .set(
            new,
            Prop::Enter,
            pose(vec![(Prop::X, num(60.0)), (Prop::Opacity, num(0.0))]),
        );
    st.apply(d);
    st.paint(frame(end));
    let white = |st: &Stage| {
        (0..80).find(|x| {
            let p = st.buf.px(*x, 10);
            p[0] > 60 && p[1] > 60 && p[2] > 60
        })
    };
    let first = white(&st);
    assert!(
        first.is_none_or(|x| x > 40),
        "starts at its pose: {first:?}"
    );
    // Frame by frame towards its place (not a jump to rest).
    let mut xs = vec![first.unwrap_or(80)];
    for k in 1..=5 {
        st.paint(frame(end + k));
        xs.push(white(&st).unwrap_or(80));
    }
    // (Faint at first: its opacity springs from 0 too.) $motion.spatial
    // is 89% of the way three refreshes in, so it moves on every frame
    // up to the second and is still short of its place there.
    assert!(xs.windows(2).all(|w| w[1] <= w[0]), "glides in: {xs:?}");
    assert!(xs[..3].windows(2).all(|w| w[1] < w[0]), "glides in: {xs:?}");
    assert!(xs[2] > 0, "still moving: {xs:?}");
    st.settle(end + 6);
    assert_eq!(white(&st), Some(0));
}

/// (eval-coverage-3) Two neighbouring rows leaving at once to `height:
/// 0` fold both gaps beside them, and their column, sized by what it
/// holds, keeps that size from the frame their springs settle until they
/// unmount: before, each zero-height slot dropped the gap it gave up
/// (taffy drops the margin of an item whose basis is zero when it sizes
/// a parent to its content), so the column grew back by both gaps for
/// the frames the rows faded at zero, and shrank by them as they went.
#[test]
fn neighbours_collapsing_together_keep_their_gaps_folded_until_they_unmount() {
    let mut ids = Vec::new();
    let mut col = None;
    let mut st = Stage::new(120, 160, |b, root| {
        // A column of fixed size holding it, so it is sized by what it
        // holds on its main axis.
        let holder = b.node(
            NodeKind::Col,
            Some(root),
            vec![(Prop::Width, num(100.0)), (Prop::Height, num(140.0))],
        );
        let c = b.node(
            NodeKind::Col,
            Some(holder),
            vec![(Prop::Gap, num(6.0)), (Prop::Bg, color("#303030"))],
        );
        col = Some(c);
        for _ in 0..3 {
            ids.push(b.node(
                NodeKind::Box,
                Some(c),
                vec![
                    (Prop::Width, num(80.0)),
                    (Prop::Height, num(20.0)),
                    (Prop::Bg, color("#ff0000")),
                    (
                        Prop::Exit,
                        pose(vec![(Prop::Opacity, num(0.0)), (Prop::Height, num(0.0))]),
                    ),
                ],
            ));
        }
    });
    let col = col.unwrap();
    assert_eq!(st.rect(col).h, 72.0);
    let mut d = SceneDiff::new();
    d.remove(ids[1]).remove(ids[2]);
    st.apply(d);
    let mut hs = Vec::new();
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        hs.push(st.rect(col).h);
        k += 1;
        assert!(k < 400, "never settled");
    }
    assert_eq!(st.r.tree().ghost_count(), 0, "unmounted after their exits");
    assert_eq!(hs.last(), Some(&20.0), "{hs:?}");
    // Down to the first row alone, and never back up on the way.
    assert!(
        hs.windows(2).all(|w| w[1] <= w[0] + 0.01),
        "the column only shrinks: {hs:?}"
    );
}

/// (integration-r6) Rows waiting at zero to unmount take nothing, not
/// even the hair they are laid at: a column of what stays, centred in a
/// holder whose free space halves to a half pixel, rests where it ends
/// while they wait to unmount (the hair, 1/1024 px in each, rounded it a
/// pixel taller and higher until they went, as unhold-flip's card).
#[test]
fn rows_waiting_at_zero_leave_a_centred_column_where_it_is() {
    let mut ids = Vec::new();
    let mut col = None;
    let mut st = Stage::new(120, 160, |b, root| {
        let holder = b.node(
            NodeKind::Col,
            Some(root),
            vec![
                (Prop::Width, num(100.0)),
                (Prop::Height, num(141.0)),
                (Prop::Justify, kw("center")),
            ],
        );
        let c = b.node(NodeKind::Col, Some(holder), vec![(Prop::Gap, num(6.0))]);
        col = Some(c);
        for _ in 0..3 {
            ids.push(b.node(
                NodeKind::Box,
                Some(c),
                vec![
                    (Prop::Width, num(80.0)),
                    (Prop::Height, num(20.0)),
                    (Prop::Bg, color("#ff0000")),
                    (Prop::Exit, pose(vec![(Prop::Height, num(0.0))])),
                ],
            ));
        }
    });
    let col = col.unwrap();
    let mut d = SceneDiff::new();
    d.remove(ids[1]).remove(ids[2]);
    st.apply(d);
    // The column's place and height each frame, and whether both rows
    // leaving are laid at nothing (or gone).
    let mut seen = Vec::new();
    let mut k = 1;
    loop {
        st.paint(frame(k));
        let rects = &st.r.boxes(S).unwrap().rects;
        let zero = ids[1..]
            .iter()
            .all(|id| rects.get(id).is_none_or(|r| r.h == 0.0));
        let r = st.rect(col);
        seen.push((r.y, r.h, zero));
        k += 1;
        if !st.r.wants_frame(S) {
            break;
        }
        assert!(k < 400, "never settled");
    }
    assert_eq!(st.r.tree().ghost_count(), 0, "unmounted after their exits");
    let end = *seen.last().unwrap();
    assert_eq!((end.0, end.1), (60.5f32.round(), 20.0), "{seen:?}");
    // Once it holds still over two frames with the rows at nothing (their
    // springs at rest, waiting to unmount), it never moves again.
    let still = seen
        .windows(2)
        .position(|w| w[0].2 && w[1].2 && (w[0].0, w[0].1) == (w[1].0, w[1].1))
        .unwrap();
    assert!(
        seen[still..].iter().all(|s| (s.0, s.1) == (end.0, end.1)),
        "the column moves as the rows at zero unmount: {seen:?}"
    );
}

/// (integration-r6) A column sized by what it holds, with a gap of
/// `gap`, holding a row per height in `hs` (each leaving on `exit {
/// height: 0 }`), painted at rest: the stage, the column and its rows.
fn fold_stage(hs: &[f32], gap: f32) -> (Stage, NodeId, Vec<NodeId>) {
    let mut col = None;
    let mut rows = Vec::new();
    let st = Stage::new(160, 640, |b, root| {
        let outer = b.node(NodeKind::Col, Some(root), vec![]);
        let c = b.node(NodeKind::Col, Some(outer), vec![(Prop::Gap, num(gap))]);
        for h in hs {
            rows.push(b.node(
                NodeKind::Box,
                Some(c),
                vec![
                    (Prop::Height, num(*h)),
                    (Prop::Width, num(100.0)),
                    (Prop::Bg, color("#ff0000")),
                    (Prop::Exit, pose(vec![(Prop::Height, num(0.0))])),
                ],
            ));
        }
        col = Some(c);
    });
    (st, col.unwrap(), rows)
}

fn remove(st: &mut Stage, ids: &[NodeId]) {
    let mut d = SceneDiff::new();
    for id in ids {
        d.push(SceneOp::Remove {
            id: *id,
            window: false,
        });
    }
    st.apply(d);
}

/// (integration-r6) However rows leave, their gaps fold with them, and
/// a column sized by what it holds only shrinks, frame by frame, to the
/// rows that stay: rows of different heights leaving together, with a
/// row staying before them or with none, and a second row leaving once
/// the first is under half a gap (two toasts timing out apart). A fold that
/// switched how the gaps were shared as the set of folding rows changed
/// grew the column back by up to a gap there.
#[test]
fn rows_leaving_apart_or_unequal_only_shrink_their_column() {
    // (heights, rows leaving at once, rows leaving once the first of
    // those is under a pixel)
    let cases: [(&[f32], &[usize], &[usize]); 6] = [
        (&[30.0, 40.0, 20.0], &[1, 2], &[]),
        (&[40.0, 20.0], &[0, 1], &[]),
        (&[30.0, 30.0, 30.0], &[1], &[2]),
        (&[12.0, 30.0, 40.0], &[0], &[1]),
        (&[40.0, 20.0], &[0], &[1]),
        (&[40.0, 8.0], &[0], &[1]),
    ];
    for (hs, first, then) in cases {
        let (mut st, col, rows) = fold_stage(hs, 6.0);
        let ids = |ix: &[usize]| ix.iter().map(|i| rows[*i]).collect::<Vec<_>>();
        remove(&mut st, &ids(first));
        let mut seen = vec![st.rect(col).h];
        let mut pending = !then.is_empty();
        let mut k = 1u64;
        // Painted every 2 ms, so a step the fold makes shows apart from
        // the rows' own motion.
        while st.r.wants_frame(S) || pending {
            st.paint(T0 + Duration::from_millis(2 * k));
            seen.push(st.rect(col).h);
            let lead =
                st.r.boxes(S)
                    .unwrap()
                    .rects
                    .get(&rows[first[0]])
                    .map(|r| r.h);
            if pending && lead.is_none_or(|h| h < 3.0) {
                remove(&mut st, &ids(then));
                pending = false;
            }
            k += 1;
            assert!(k < 3000, "never settled");
        }
        let stay: Vec<f32> = (0..hs.len())
            .filter(|i| !first.contains(i) && !then.contains(i))
            .map(|i| hs[i])
            .collect();
        let rest = stay.iter().sum::<f32>() + 6.0 * stay.len().saturating_sub(1) as f32;
        assert_eq!(seen.last(), Some(&rest), "{hs:?}: {seen:?}");
        assert!(
            seen.windows(2).all(|w| w[1] <= w[0] + 0.01),
            "{hs:?} {first:?} then {then:?}: the column grows back: {seen:?}"
        );
    }
}

/// (integration-r6) Rows entering an empty column from `enter { height:
/// 0 }`, of different heights, together or one after another, open
/// their gaps as they grow: frame by frame the column moves by no more
/// than the rows' own growth and the gaps it opens (each gap at most what
/// its row grew), and never steps by a gap at once. A fold that switched
/// how the gaps were shared as the set of folding rows changed stepped
/// here by most of two gaps.
#[test]
fn rows_entering_from_zero_open_their_gaps_as_they_grow() {
    // Every millisecond painted: a row coming every `every` paints (0:
    // all at once).
    for every in [0u64, 40] {
        // Tall rows and a wide gap, so a step by a gap stands clear of
        // the rows' motion in a millisecond and of the rounding of their
        // and the column's boxes to whole pixels.
        let (mut st, col, _) = fold_stage(&[], 60.0);
        let hs = [200.0f32, 100.0, 150.0];
        let ids: Vec<NodeId> = (0..3).map(|i| NodeId::new(900 + i, 0)).collect();
        let mut seen: Vec<(f32, f32)> = vec![(st.rect(col).h, 0.0)];
        let mut k = 1u64;
        loop {
            let due: Vec<usize> = (0..3).filter(|i| 1 + every * *i as u64 == k).collect();
            if !due.is_empty() {
                let mut d = SceneDiff::new();
                for i in due {
                    d.create(ids[i], NodeKind::Box, Some(col), i as u32)
                        .set(ids[i], Prop::Height, num(hs[i]))
                        .set(ids[i], Prop::Width, num(100.0))
                        .set(ids[i], Prop::Bg, color("#ff0000"))
                        .set(ids[i], Prop::Enter, pose(vec![(Prop::Height, num(0.0))]));
                }
                st.apply(d);
            }
            st.paint(T0 + Duration::from_millis(k));
            let rects = &st.r.boxes(S).unwrap().rects;
            let rows: f32 = ids.iter().filter_map(|id| rects.get(id)).map(|r| r.h).sum();
            seen.push((st.rect(col).h, rows));
            k += 1;
            if k > 120 && !st.r.wants_frame(S) {
                break;
            }
            assert!(k < 6000, "never settled");
        }
        let rest = hs.iter().sum::<f32>() + 120.0;
        assert_eq!(seen.last().map(|s| s.0), Some(rest), "{every}: {seen:?}");
        // Each box is whole pixels: the column's step is off by at most
        // 1, the three rows' by at most 3, before the gaps double it; and
        // the column goes the way its rows go.
        for w in seen.windows(2) {
            let (dc, dr) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            assert!(
                dc.abs() <= 2.0 * (dr.abs() + 3.0) + 1.0,
                "{every}: the column steps {dc} where its rows moved {dr}: {seen:?}"
            );
            assert!(
                !(dr > 4.0 && dc < -1.0 || dr < -4.0 && dc > 1.0),
                "{every}: the column steps {dc} against its rows' {dr}: {seen:?}"
            );
        }
    }
}

/// (bar-blank-on-exit) A pose that squeezes a width leaves the height
/// at rest: a text leaving with `exit { width: 0 }`, and a box holding a
/// text, keep the height of their line while they narrow, so the row
/// around them never grows and the box beside them never moves. Before,
/// the text wrapped letter by letter into a column hundreds of pixels
/// tall and the centred box was carried down with the row's middle.
#[test]
fn a_width_exit_squeezes_its_text_without_rewrapping_it() {
    let exit = || {
        (
            Prop::Exit,
            pose(vec![(Prop::Opacity, num(0.0)), (Prop::Width, num(0.0))]),
        )
    };
    let (mut label, mut badge, mut inner, mut red) = (None, None, None, None);
    let mut st = Stage::new(240, 60, |b, root| {
        let row = b.node(
            NodeKind::Row,
            Some(root),
            vec![
                (Prop::Gap, num(6.0)),
                (Prop::Align, PropValue::Keyword("center".into())),
            ],
        );
        label = Some(b.node(
            NodeKind::Text,
            Some(row),
            vec![
                (Prop::Text, text("plugged in")),
                (Prop::Color, color("#ffffff")),
                exit(),
            ],
        ));
        let bx = b.node(
            NodeKind::Box,
            Some(row),
            vec![(Prop::Bg, color("#00ff00")), exit()],
        );
        badge = Some(bx);
        inner = Some(b.node(
            NodeKind::Text,
            Some(bx),
            vec![(Prop::Text, text("Muted")), (Prop::Color, color("#000000"))],
        ));
        red = Some(red_box(b, row, vec![]));
    });
    let (label, badge, inner, red) = (label.unwrap(), badge.unwrap(), inner.unwrap(), red.unwrap());
    let at_rest = [st.rect(label), st.rect(badge), st.rect(inner), st.rect(red)];
    assert!(at_rest[0].w > 40.0 && at_rest[0].h < 30.0, "{at_rest:?}");
    let mut d = SceneDiff::new();
    d.remove(label).remove(badge);
    st.apply(d);
    let mut k = 1;
    let mut seen = Vec::new();
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        k += 1;
        assert!(k < 200, "never settled");
        let Some(boxes) = st.r.boxes(S) else { continue };
        let (Some(l), Some(b)) = (boxes.rects.get(&label), boxes.rects.get(&badge)) else {
            continue;
        };
        seen.push((l.w, l.h, b.w, b.h));
        assert!(l.h <= at_rest[0].h + 0.5, "the label grew tall: {l:?}");
        assert!(b.h <= at_rest[1].h + 0.5, "the badge grew tall: {b:?}");
        let r = boxes.rects[&red];
        assert!(
            (r.y - at_rest[3].y).abs() < 0.5,
            "the box beside moved: {r:?}"
        );
    }
    assert!(
        seen.iter()
            .any(|s| s.0 < at_rest[0].w / 2.0 && s.2 < at_rest[1].w / 2.0),
        "both narrowed: {seen:?}"
    );
    assert_eq!(st.r.tree().ghost_count(), 0, "unmounted after the exit");
}

/// (clip-blink) A clip carries what it holds, and shows what it showed.
/// A card centred in its panel, clipping six 40 px rows, loses its last
/// three (no exit pose: they leave at once): it snaps 60 px down to its
/// new, smaller box (nothing moved its parent's children), and its first
/// row, first in it still, is drawn on every frame. Before, the row
/// glided from where it was on screen, above the card's new top edge, so
/// the card's clip hid it for the first frames of its glide. Then the
/// first two go: the red row moves up two slots inside a card that shrank
/// to one row. Measured inside the card it would start below it, hidden;
/// it starts where the card shows it instead.
#[test]
fn a_snapped_clip_carries_its_rows_so_none_blinks_out() {
    let mut rows = Vec::new();
    let mut st = Stage::new(80, 400, |b, root| {
        let outer = b.node(
            NodeKind::Col,
            Some(root),
            vec![
                (Prop::Height, num(400.0)),
                (Prop::Justify, PropValue::Keyword("center".into())),
            ],
        );
        let card = b.node(
            NodeKind::Col,
            Some(outer),
            vec![
                (Prop::Width, num(60.0)),
                (Prop::Clip, PropValue::Bool(true)),
            ],
        );
        for c in [
            "#00ff00", "#0000ff", "#ff0000", "#0000ff", "#0000ff", "#0000ff",
        ] {
            rows.push(b.node(
                NodeKind::Box,
                Some(card),
                vec![(Prop::Height, num(40.0)), (Prop::Bg, color(c))],
            ));
        }
    });
    assert_eq!(st.rect(rows[0]).y, 80.0);
    // Whether any pixel of column 30 has channel `ch` (BGRA) lit.
    let lit = |st: &Stage, ch: usize| (0..400).any(|y| st.buf.px(30, y)[ch] > 200);
    let mut k = 1;
    for (gone, (shown, ch)) in [(&rows[3..], (rows[0], 1)), (&rows[..2], (rows[2], 2))] {
        assert!(lit(&st, ch));
        let mut d = SceneDiff::new();
        for r in gone {
            d.remove(*r);
        }
        st.apply(d);
        let start = k;
        while st.r.wants_frame(S) {
            st.paint(frame(k));
            assert!(lit(&st, ch), "{shown:?} blinked out at frame {k}");
            k += 1;
            assert!(k < start + 200, "never settled");
        }
    }
    assert_eq!(st.rect(rows[2]).y, 180.0, "the card is centred again");
}

/// (bar-blank-on-exit) A content-sized panel is measured for the size it
/// asks for with what an exit squeezes at its layout at rest, as it is
/// painted: a chip leaving on `exit { width: 0 }` never makes the panel
/// ask for a taller buffer. Before, that measure wrapped the chip's text
/// into a column and the panel asked for a buffer that tall (1215 px
/// here; on an output, all of its height, its row re-centred in it).
#[test]
fn a_content_sized_panel_keeps_its_height_while_a_chip_squeezes_out() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("right".into())),
        ],
    );
    let row = b.node(
        NodeKind::Row,
        Some(root),
        vec![
            (Prop::Gap, num(6.0)),
            (Prop::Align, PropValue::Keyword("center".into())),
        ],
    );
    let chip = b.node(
        NodeKind::Box,
        Some(row),
        vec![
            (Prop::Bg, color("#00ff00")),
            (
                Prop::Exit,
                pose(vec![(Prop::Width, num(0.0)), (Prop::Opacity, num(0.0))]),
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(chip),
        vec![
            (Prop::Text, text("Do not disturb")),
            (Prop::Color, color("#000000")),
        ],
    );
    red_box(&mut b, row, vec![]);
    let mut h = Host::new(b.diff, root);
    h.run();
    let tall = h.surface.as_ref().unwrap().1.size.h;
    assert!((15..40).contains(&tall), "one line tall: {tall}");
    let mut d = SceneDiff::new();
    d.remove(chip);
    h.apply(d);
    let mut heights = Vec::new();
    loop {
        let Some((id, buf, age)) = h.surface.as_mut() else {
            panic!("closed");
        };
        if !h.r.wants_frame(*id) {
            break;
        }
        buf.paint_at(&mut h.r, *id, *age, frame(h.k));
        *age = 1;
        h.k += 1;
        heights.push(buf.size.h);
        if h.r.has_surface_changes() {
            h.r.update();
            h.sync();
        }
        assert!(heights.len() < 400, "never settled");
    }
    assert!(
        heights.iter().all(|v| *v <= tall),
        "asked for no taller buffer: {heights:?} (was {tall})"
    );
}

/// A surface whose `open` goes false plays its exit pose first: its spec
/// stays open until the pose settles. Opening plays `enter`.
#[test]
fn surfaces_play_their_poses_when_they_open_and_close() {
    let mut st = Stage::new(60, 30, |b, root| {
        red_box(b, root, vec![]);
    });
    let root = st.root;
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(true)).set(
        root,
        Prop::Exit,
        pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.9))]),
    );
    st.apply(d);
    let end = st.settle(1);
    st.r.take_surface_changes();
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(false));
    st.apply(d);
    assert!(st.r.surface_spec(root).unwrap().open, "open while it plays");
    let mut alphas = Vec::new();
    for k in 2..=5 {
        st.paint(frame(end + k));
        alphas.push(st.buf.px(30, 15)[3]);
    }
    assert!(alphas[0] < 250 && alphas[3] > 0, "fading: {alphas:?}");
    assert!(
        alphas.windows(2).all(|w| w[1] < w[0]),
        "frame by frame: {alphas:?}"
    );
    // No `update` here: the frame that ends the pose reports the close
    // (the host is woken by `has_surface_changes`).
    let end = st.settle(end + 6);
    assert!(st.r.has_surface_changes(), "the close wakes the host");
    assert!(
        !st.r.surface_spec(root).unwrap().open,
        "closed once settled"
    );
    assert!(
        st.r.take_surface_changes()
            .iter()
            .any(|(n, c)| *n == root
                && matches!(c, SurfaceChange::Updated { spec, .. } if !spec.open))
    );
    // Opening again plays `enter` (exit mirrors enter, and enter is set
    // here too).
    let mut d = SceneDiff::new();
    d.set(root, Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]))
        .set(root, Prop::Open, PropValue::Bool(true));
    st.apply(d);
    assert!(st.r.surface_spec(root).unwrap().open);
    let mut alphas = Vec::new();
    for k in 1..=5 {
        st.paint(frame(end + k));
        alphas.push(st.buf.px(30, 15)[3]);
    }
    assert!(alphas[0] < 128, "enters from transparent: {alphas:?}");
    assert!(
        alphas.windows(2).all(|w| w[1] > w[0]) && alphas[4] < 255,
        "frame by frame: {alphas:?}"
    );
    st.settle(end + 6);
    assert_eq!(st.buf.px(30, 15)[3], 255);
}

/// A surface that closes with a pose while logic removes its content in
/// the same diff (a popup unmounts its content when it closes) keeps
/// drawing that content, at rest, until the pose ends: it does not leave
/// empty. Opened again mid-pose, the kept content goes and logic's new
/// content shows.
#[test]
fn content_removed_as_its_surface_closes_stays_through_the_pose() {
    let mut boxed = NodeId::new(0, 0);
    let mut st = Stage::new(60, 30, |b, root| {
        boxed = red_box(b, root, vec![]);
    });
    let root = st.root;
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(true)).set(
        root,
        Prop::Exit,
        pose(vec![(Prop::Opacity, num(0.0))]),
    );
    st.apply(d);
    let end = st.settle(1);
    st.r.take_surface_changes();
    // Removed before the close in the same diff, as logic sends it.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: boxed,
        window: false,
    });
    d.set(root, Prop::Open, PropValue::Bool(false));
    st.apply(d);
    assert!(st.r.surface_spec(root).unwrap().open, "open while it plays");
    let red = |st: &Stage| {
        let p = st.buf.px(10, 10);
        p[3] > 0 && p[2] as u32 > 3 * p[1] as u32
    };
    for k in 2..=4 {
        st.paint(frame(end + k));
        assert!(red(&st), "frame {k}: the content left before the pose");
    }
    assert!(st.r.tree().ghost_count() > 0);
    let end = st.settle(end + 5);
    assert!(
        !st.r.surface_spec(root).unwrap().open,
        "closed once settled"
    );
    assert_eq!(st.r.tree().ghost_count(), 0, "the kept content unmounted");

    // Opened, then closed with its content removed, then opened again
    // with new content before the pose ends: the old content goes.
    let again = NodeId::new(100, 0);
    let mut d = SceneDiff::new();
    d.create(again, NodeKind::Box, Some(root), u32::MAX);
    d.set(again, Prop::Bg, color("#ff0000"))
        .set(again, Prop::Size, num(20.0));
    d.set(root, Prop::Open, PropValue::Bool(true));
    st.apply(d);
    let end = st.settle(end + 1);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: again,
        window: false,
    });
    d.set(root, Prop::Open, PropValue::Bool(false));
    st.apply(d);
    st.paint(frame(end + 1));
    assert!(st.r.tree().ghost_count() > 0);
    let third = NodeId::new(101, 0);
    let mut d = SceneDiff::new();
    d.create(third, NodeKind::Box, Some(root), u32::MAX);
    d.set(third, Prop::Bg, color("#00ff00"))
        .set(third, Prop::Size, num(20.0));
    d.set(root, Prop::Open, PropValue::Bool(true));
    st.apply(d);
    assert_eq!(st.r.tree().ghost_count(), 0, "the kept content went");
    st.settle(end + 2);
    assert_eq!(st.r.tree().get(root).unwrap().children, vec![third]);
    let p = st.buf.px(10, 10);
    assert!(p[1] > 128 && p[2] < 64, "the new content shows: {p:?}");
}

/// Colours spring in OKLab with premultiplied alpha, sampled exactly as
/// `strand_scene::motion` says.
#[test]
fn colours_spring_in_oklab() {
    let mut id = None;
    let mut st = Stage::new(20, 20, |b, root| id = Some(red_box(b, root, vec![])));
    let id = id.unwrap();
    let mut d = SceneDiff::new();
    d.set(id, Prop::Bg, color("#0000ff"));
    st.apply(d);
    st.paint(frame(1));
    st.paint(frame(3));
    let got = st.buf.px(10, 10);
    // The renderer's motion: from red, last shown at T0, effects spring.
    let mut m = Motion::rest(color_channels(hex("#ff0000")), 0.002).sampled_at(Some(T0));
    m.retarget(color_channels(hex("#0000ff")), Curve::Spring(EFFECTS));
    m.sample(frame(1));
    let want = channels_color(m.sample(frame(3))).to_rgba8();
    for (g, w) in [(got[2], want[0]), (got[1], want[1]), (got[0], want[2])] {
        assert!(g.abs_diff(w) <= 2, "got {got:?} want {want:?}");
    }
    // Not a per-channel sRGB blend.
    let t = 1.0 - want[0] as f32 / 255.0;
    let srgb_green = 0.0 * t;
    assert!(want[1] as f32 > srgb_green, "OKLab passes through {want:?}");
}

/// (eval-rules-7) A button's `bg` set by a state springs from the fill it
/// draws without one (its default, `$surface.hi`, else its label colour
/// at 10 %) and back to it, not from and to clear: the default fill did
/// not vanish at once while the new one faded in from nothing (a flash
/// toward the panel under it), nor snap back at rest after the fill faded
/// out to the panel. Each channel of a pixel inside it moves steadily from
/// where it was to where it rests, both ways, and lands on the default.
#[test]
fn a_buttons_bg_springs_from_and_to_its_default_fill() {
    let mut id = None;
    let mut st = Stage::new(40, 40, |b, root| {
        id = Some(b.node(
            NodeKind::Button,
            Some(root),
            vec![(Prop::Width, num(30.0)), (Prop::Height, num(30.0))],
        ))
    });
    let id = id.unwrap();
    let start = st.buf.px(15, 15);
    for (to, k0) in [(color("#f0c040"), 1), (PropValue::Unset, 200)] {
        let before = st.buf.px(15, 15);
        let mut d = SceneDiff::new();
        d.set(id, Prop::Bg, to.clone());
        st.apply(d);
        let mut seen = vec![before];
        let mut k = k0;
        while st.r.wants_frame(S) {
            st.paint(frame(k));
            seen.push(st.buf.px(15, 15));
            k += 1;
            assert!(k < k0 + 190, "never settled");
        }
        st.paint(frame(k + 5));
        seen.push(st.buf.px(15, 15));
        let end = *seen.last().unwrap();
        assert!(seen.len() > 4, "it springs over frames: {seen:?}");
        for c in 0..3 {
            for w in seen.windows(2) {
                let (a, b) = (i32::from(w[0][c]), i32::from(w[1][c]));
                let e = i32::from(end[c]);
                assert!(
                    (e - b).abs() <= (e - a).abs() + 2,
                    "to {to:?}, channel {c} moves away from its end {e}: {a} -> {b} in {seen:?}"
                );
            }
        }
    }
    assert_eq!(st.buf.px(15, 15), start, "back at its default fill");
}

/// (border-eps) A border fading out ends where the eye no longer sees it
/// move: its colour settles by a colour's tolerance, not its width's
/// (0.05 px, which snapped the fade off from an alpha of 0x08–0x0e), so
/// the last frame before it rests is within a step or two of the
/// background, on the way in too.
#[test]
fn a_fading_border_settles_without_a_visible_snap() {
    let border = |c: &str| {
        PropValue::Border(Border {
            width: 4.0,
            paint: Paint::Solid(hex(c)),
        })
    };
    let mut id = None;
    let mut st = Stage::new(40, 40, |b, root| {
        id = Some(b.node(
            NodeKind::Box,
            Some(root),
            vec![(Prop::Size, num(30.0)), (Prop::Border, border("#ffffff00"))],
        ))
    });
    let id = id.unwrap();
    for (to, k0) in [("#ffffff", 1), ("#ffffff00", 200)] {
        let mut d = SceneDiff::new();
        d.set(id, Prop::Border, border(to));
        st.apply(d);
        // The green channel of a pixel on the border, frame by frame.
        let mut greens = Vec::new();
        let mut k = k0;
        while st.r.wants_frame(S) {
            st.paint(frame(k));
            greens.push(st.buf.px(1, 15)[1]);
            k += 1;
            assert!(k < k0 + 190, "never settled");
        }
        let n = greens.len();
        assert!(n > 3, "it fades over frames: {greens:?}");
        let (last, rest) = (greens[n - 2], greens[n - 1]);
        assert!(
            last.abs_diff(rest) <= 2,
            "fading to {to}, the frame before rest is {last}, then {rest}: {greens:?}"
        );
    }
}

/// Snap rules: fonts snap; shadow lists of different lengths pad with
/// transparent shadows and spring; layout lengths (`pad`) snap while the
/// boxes they move glide; `reduced_motion` snaps everything; a frame at
/// time zero (no clock) shows every value at rest.
#[test]
fn snap_rules_and_reduced_motion() {
    let mut ids = Vec::new();
    let mut st = Stage::new(120, 60, |b, root| {
        let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Pad, num(0.0))]);
        ids.push(col);
        ids.push(red_box(
            b,
            col,
            vec![(
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 2.0,
                    blur: 4.0,
                    spread: 0.0,
                    color: Color::BLACK,
                }]),
            )],
        ));
        ids.push(b.node(
            NodeKind::Text,
            Some(col),
            vec![
                (Prop::Text, text("Aa")),
                (Prop::Font, PropValue::Font(font(10.0))),
            ],
        ));
    });
    let (bx, label) = (ids[1], ids[2]);
    // Fonts snap: the text is laid out at its new size in the next frame.
    let h0 = st.rect(label).h;
    let mut d = SceneDiff::new();
    d.set(label, Prop::Font, PropValue::Font(font(20.0)));
    st.apply(d);
    st.paint(frame(1));
    assert!(st.rect(label).h > h0 * 1.6, "font snapped");
    st.settle(2);
    // Shadow lists pad: one shadow to two springs without a jump.
    let two = vec![
        Shadow {
            x: 0.0,
            y: 8.0,
            blur: 12.0,
            spread: 0.0,
            color: Color::BLACK,
        },
        Shadow {
            x: 0.0,
            y: 1.0,
            blur: 2.0,
            spread: 0.0,
            color: hex("#ff00ff"),
        },
    ];
    let mut d = SceneDiff::new();
    d.set(bx, Prop::Shadow, PropValue::Shadow(two));
    st.apply(d);
    assert!(!st.paint(frame(40)).is_empty());
    assert!(st.r.animating(S));
    let end = st.settle(41);
    // `pad` snaps: the box is laid out 10 px down at once, and glides.
    // `pad` snaps: the box is laid out 10 px down at once, and glides.
    let mut pad = Stage::new(60, 60, |b, root| {
        let col = b.node(NodeKind::Col, Some(root), vec![]);
        red_box(b, col, vec![]);
    });
    let pcol = NodeId::new(1, 0);
    let pbox = NodeId::new(2, 0);
    assert_eq!(pad.red_top(10), Some(0));
    let mut d = SceneDiff::new();
    d.set(pcol, Prop::Pad, num(10.0));
    pad.apply(d);
    pad.paint(frame(1));
    assert_eq!(pad.rect(pbox).y, 10.0, "laid out at once");
    let top = pad.red_top(15).unwrap();
    assert!(top < 10, "glides from where it was: {top}");
    pad.settle(2);
    assert_eq!(pad.red_top(15), Some(10));
    // reduced_motion turned on mid size spring: it snaps at the next
    // frame, which is the last.
    let mut d = SceneDiff::new();
    d.set(bx, Prop::Width, num(60.0));
    st.apply(d);
    for k in 0..3 {
        st.paint(frame(end + k));
    }
    let mid = st.rect(bx).w;
    assert!(mid > 20.0 && mid < 60.0, "springing: {mid}");
    st.r.set_reduced_motion(true);
    assert!(st.r.wants_frame(S));
    st.paint(frame(end + 3));
    assert_eq!(st.rect(bx).w, 60.0, "snapped");
    assert!(!st.r.wants_frame(S), "and settled");
    let end = end + 4;
    // reduced_motion: everything snaps.
    let mut d = SceneDiff::new();
    d.set(bx, Prop::X, num(50.0))
        .set(bx, Prop::Width, num(40.0));
    st.apply(d);
    st.paint(frame(end));
    let r = st.rect(bx);
    assert_eq!(st.red_from((r.y + 5.0) as u32), Some(r.x as u32 + 50));
    assert!(!st.r.wants_frame(S));
    assert_eq!(st.rect(bx).w, 40.0);
    // The token `motion.reduced` does the same.
    st.r.set_reduced_motion(false);
    let mut table = TokenTable::default();
    table.insert("motion.reduced", PropValue::Bool(true));
    let mut d = SceneDiff::new();
    d.set_tokens(table, Transition::Instant);
    st.apply(d);
    assert!(st.r.reduced_motion());
}

/// Offline at time zero (no clock): changes show at rest at once.
#[test]
fn frames_at_time_zero_show_values_at_rest() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Width, num(60.0)), (Prop::Height, num(20.0))],
    );
    let bx = red_box(&mut b, root, vec![]);
    let mut r = renderer();
    r.apply(b.diff);
    r.attach_surface(S, root);
    let mut buf = Buffer::new(60, 20, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let mut d = SceneDiff::new();
    d.set(bx, Prop::X, num(30.0));
    r.apply(d);
    buf.paint(&mut r, S, 1);
    assert!(buf.px(31, 10)[2] > 128 && buf.px(10, 10)[2] < 128);
    assert!(!r.wants_frame(S));
}

/// A token swap that changes a layout length (`$space`) snaps the length
/// and glides the boxes it moved; `popin` scales a node in about its
/// centre.
#[test]
fn token_swaps_glide_and_popin_scales() {
    let mut table = TokenTable::default();
    table.insert("space.2", num(4.0));
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#1e1e2e")),
        ],
    );
    let row = b.node(
        NodeKind::Row,
        Some(root),
        vec![(Prop::Pad, PropValue::Token(TokenExpr::path("space.2")))],
    );
    let bx = red_box(&mut b, row, vec![]);
    let mut d = SceneDiff::new();
    d.set_tokens(table.clone(), Transition::Instant);
    d.ops.extend(b.diff.ops);
    let mut r = renderer();
    r.apply(d);
    r.attach_surface(S, root);
    let mut buf = Buffer::new(80, 40, Scale::ONE);
    buf.paint_at(&mut r, S, 0, T0);
    let mut st = Stage { r, buf, root };
    assert_eq!(st.red_from(20), Some(4));
    let mut swapped = table;
    swapped.insert("space.2", num(24.0));
    let mut d = SceneDiff::new();
    d.set_tokens(swapped, Transition::Default);
    st.apply(d);
    st.paint(frame(1));
    assert_eq!(st.rect(bx).x, 24.0, "the length snapped");
    let x = st.red_from(20).unwrap();
    assert!(x < 20, "the box glides: {x}");
    let end = st.settle(2);
    assert_eq!(st.red_from(20), Some(24));

    // popin(0.5): starts at half size about its centre.
    let n = NodeId::new(30, 0);
    let mut d = SceneDiff::new();
    d.create(n, NodeKind::Box, Some(row), 9)
        .set(n, Prop::Size, num(20.0))
        .set(n, Prop::Bg, color("#00ff00"))
        .set(
            n,
            Prop::Enter,
            PropValue::Call {
                name: "popin".into(),
                args: vec![num(0.5)],
            },
        );
    st.apply(d);
    st.paint(frame(end));
    let green = |st: &Stage| {
        let r = st.rect(n);
        let y = (r.y + r.h / 2.0) as u32;
        (0..80u32)
            .filter(|x| {
                let p = st.buf.px(*x, y);
                p[1] > 30 && p[2] < 100
            })
            .count()
    };
    // One refresh along $motion.spatial (26%), the scale is about 0.63:
    // 12.6 of its 20 px; two in (64%), about 0.82.
    let first = green(&st);
    assert!(first <= 13, "faint and small at first: {first}");
    st.paint(frame(end + 1));
    let mid = green(&st);
    assert!(mid > first && mid < 20, "scaled down mid-flight: {mid}");
    st.settle(end + 5);
    assert_eq!(green(&st), st.rect(n).w as usize);
}

/// The toasts example: a content-sized panel whose first toast leaves
/// with `exit { x: 420; opacity: 0; height: 0 }`: the one below slides up
/// without a jump, and the surface asks for its smaller size only once
/// everything settled.
#[test]
fn a_leaving_toast_lets_the_next_slide_up_in_a_content_sized_panel() {
    let mut b = Builder::default();
    // In the top-right corner, as the example's (a centred panel's held
    // content shrinks about its centre: decisions.md popup-shrink).
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("top_right".into())),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![(Prop::Width, num(100.0)), (Prop::Gap, num(8.0))],
    );
    let mut toasts = Vec::new();
    for c in ["#00ff00", "#ff0000"] {
        let t = b.node(
            NodeKind::Col,
            Some(col),
            vec![
                // The design's toast: `pad: $space.3; border: 1, $border`.
                (Prop::Pad, num(12.0)),
                (
                    Prop::Border,
                    PropValue::Border(Border {
                        width: 1.0,
                        paint: Paint::Solid(Color::from_rgba8(40, 40, 60, 255)),
                    }),
                ),
                (Prop::Bg, color(c)),
                // `shadow: $elevation.lg`'s first: it sizes the overhang.
                (
                    Prop::Shadow,
                    PropValue::Shadow(vec![Shadow {
                        x: 0.0,
                        y: 8.0,
                        blur: 24.0,
                        spread: 0.0,
                        color: Color::from_rgba8(0, 0, 0, 60),
                    }]),
                ),
                (
                    Prop::Exit,
                    pose(vec![
                        (Prop::X, num(420.0)),
                        (Prop::Opacity, num(0.0)),
                        (Prop::Height, num(0.0)),
                    ]),
                ),
            ],
        );
        b.node(NodeKind::Box, Some(t), vec![(Prop::Height, num(30.0))]);
        toasts.push(t);
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let spec = r.surface_spec(root).unwrap().clone();
    let (w, h) = (spec.width.unwrap() as u32, spec.height.unwrap() as u32);
    assert_eq!(h, 54 + 8 + 54);
    // The buffer takes the shadows' overhang around the content.
    let o = spec.overhang;
    assert!(o.bottom > 0.0);
    let (ox, oy) = (o.left as u32, o.top as u32);
    r.attach_surface(S, root);
    let mut buf = Buffer::new(
        w + (o.left + o.right) as u32,
        h + (o.top + o.bottom) as u32,
        Scale::ONE,
    );
    buf.paint_at(&mut r, S, 0, T0);
    let mut st = Stage { r, buf, root };
    // Its fill starts inside its 1 px border.
    assert_eq!(st.red_top(ox + 50), Some(oy + 63));
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: toasts[0],
        window: false,
    });
    st.apply(d);
    let mut tops = Vec::new();
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        tops.push(st.red_top(ox + 50).unwrap() as i32 - 1 - oy as i32);
        // Held: neither its size nor its overhang changes while the
        // toast leaves (a collapsing slot shrinks its parent fully).
        assert!(st.r.surface_spec(root).unwrap() == &spec || !st.r.animating(S));
        k += 1;
        assert!(k < 200);
    }
    assert_eq!(*tops.last().unwrap(), 0, "{tops:?}");
    // No jump: no step is more than $motion.spatial's largest advance in
    // one refresh, 38% of the way (from 26% to 64%), of the 62 px slot.
    assert!(
        tops.windows(2).all(|p| p[1] <= p[0] && p[0] - p[1] <= 24),
        "{tops:?}"
    );
    // No plateau: the padding and the gap fold with the collapsing
    // slot, so the next toast moves on every frame until it is within
    // a pixel of its slot, and unmounting the ghost moves nothing.
    let near = tops.iter().position(|t| *t <= 1).unwrap();
    assert!(
        tops[..=near].windows(2).all(|p| p[1] < p[0]),
        "moves every frame: {tops:?}"
    );
    st.r.update();
    assert_eq!(
        st.r.surface_spec(root).unwrap().height,
        Some(54.0),
        "shrinks once settled"
    );
}

/// Poses apply wherever logic creates and removes nodes: an `if` branch
/// (a node under any container), a `list` row and a `page` of `pages`
/// all enter from their pose and play their exit before unmounting.
#[test]
fn if_branches_list_rows_and_pages_enter_and_exit() {
    let mut parents = Vec::new();
    let mut st = Stage::new(200, 120, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![]);
        parents.push(b.node(NodeKind::Row, Some(row), vec![(Prop::Width, num(60.0))]));
        parents.push(b.node(
            NodeKind::List,
            Some(row),
            vec![(Prop::Width, num(60.0)), (Prop::Height, num(100.0))],
        ));
        parents.push(b.node(NodeKind::Pages, Some(row), vec![(Prop::Width, num(60.0))]));
    });
    let kinds = [NodeKind::Box, NodeKind::Row, NodeKind::Page];
    let fade = pose(vec![(Prop::Opacity, num(0.0)), (Prop::Y, num(30.0))]);
    let mut made = Vec::new();
    let mut d = SceneDiff::new();
    for (i, (p, k)) in parents.iter().zip(kinds).enumerate() {
        let id = NodeId::new(40 + i as u32, 0);
        d.create(id, k, Some(*p), 0)
            .set(id, Prop::Size, num(20.0))
            .set(id, Prop::Bg, color("#ff0000"))
            .set(id, Prop::Enter, fade.clone());
        made.push(id);
    }
    st.apply(d);
    st.paint(frame(1));
    let alpha = |st: &Stage, id: NodeId| {
        let r = st.rect(id);
        st.buf.px((r.x + 10.0) as u32, (r.y + 10.0) as u32)[2]
    };
    let mut seen: Vec<Vec<u8>> = made.iter().map(|id| vec![alpha(&st, *id)]).collect();
    for k in 2..=5 {
        st.paint(frame(k));
        for (i, id) in made.iter().enumerate() {
            seen[i].push(alpha(&st, *id));
        }
    }
    for (id, a) in made.iter().zip(&seen) {
        assert!(a[0] < 200, "{id:?} enters from its pose: {a:?}");
        assert!(
            a.windows(2).all(|w| w[1] >= w[0]) && a[4] > a[0] && a[4] < 255,
            "{id:?} fades in frame by frame: {a:?}"
        );
    }
    let end = st.settle(6);
    for id in &made {
        assert_eq!(alpha(&st, *id), 255, "{id:?} at rest");
    }
    let mut d = SceneDiff::new();
    for id in &made {
        d.push(SceneOp::Remove {
            id: *id,
            window: false,
        });
    }
    st.apply(d);
    for id in &made {
        assert!(
            st.r.tree().is_ghost(*id),
            "{id:?} plays its exit (mirrored enter)"
        );
    }
    let mut seen: Vec<Vec<u8>> = vec![Vec::new(); made.len()];
    for k in 0..4 {
        st.paint(frame(end + k));
        for (i, id) in made.iter().enumerate() {
            seen[i].push(alpha(&st, *id));
        }
    }
    for (id, a) in made.iter().zip(&seen) {
        assert!(
            a.windows(2).all(|w| w[1] <= w[0]) && a[3] < a[0] && a[0] < 255,
            "{id:?} fades out frame by frame: {a:?}"
        );
    }
    st.settle(end + 4);
    assert_eq!(st.r.tree().ghost_count(), 0);
}

/// Per-prop `~` overrides: `~ instant` snaps, `~ 200ms` ends exactly
/// 200 ms after it starts, `~ $motion.bouncy` overshoots.
#[test]
fn per_prop_transitions_override_the_default_spring() {
    let mut id = None;
    let mut st = Stage::new(200, 20, |b, root| id = Some(red_box(b, root, vec![])));
    let id = id.unwrap();
    let set = |x: f32, transition: Transition| {
        let mut d = SceneDiff::new();
        d.push(SceneOp::SetProp {
            id,
            prop: Prop::X,
            value: num(x),
            transition,
        });
        d
    };
    st.apply(set(50.0, Transition::Instant));
    st.paint(frame(1));
    assert_eq!(st.red_from(10), Some(50));
    assert!(!st.r.wants_frame(S));
    st.apply(set(
        150.0,
        Transition::Duration {
            duration: Duration::from_millis(200),
            easing: Easing::Linear,
        },
    ));
    // Starts one frame before frame 2, at frame 1's time.
    st.paint(frame(2));
    let x = st.red_from(10).unwrap();
    assert!((55..=60).contains(&x), "{x}");
    st.paint(frame(1) + Duration::from_millis(200));
    assert_eq!(st.red_from(10), Some(150));
    st.paint(frame(14));
    assert!(!st.r.wants_frame(S));
    st.apply(set(10.0, Transition::Token("motion.bouncy".into())));
    let mut min = 200;
    for k in 15..80 {
        st.paint(frame(k));
        min = min.min(st.red_from(10).unwrap());
    }
    assert!(min <= 8, "bouncy overshoots: {min}");
    st.settle(80);
    assert_eq!(st.red_from(10), Some(10));
}

/// A model of the surface manager's side of the loop (`strand run`'s
/// host): diffs are applied and their surface changes handed on at once;
/// after a paint the host hears of surface changes only if
/// `has_surface_changes` says so (its wake), and then runs `update`
/// before taking them. A spec that opens creates a surface (attach,
/// configure at the spec's size: `configure_surface` runs `update`), one
/// that closes destroys it (detach), one whose size changes is
/// configured again. A surface is painted only while it wants frames.
struct Host {
    r: Renderer,
    root: NodeId,
    surface: Option<(SurfaceId, Buffer, u8)>,
    made: u32,
    k: u32,
}

impl Host {
    fn new(diff: SceneDiff, root: NodeId) -> Host {
        let mut h = Host {
            r: renderer(),
            root,
            surface: None,
            made: 0,
            k: 1,
        };
        h.apply(diff);
        h
    }

    fn apply(&mut self, d: SceneDiff) {
        assert!(self.r.apply(d).is_empty());
        self.sync();
    }

    fn sync(&mut self) {
        while self.r.has_surface_changes() {
            for (node, change) in self.r.take_surface_changes() {
                if node != self.root {
                    continue;
                }
                let spec = match change {
                    SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } => Some(s),
                    _ => None,
                };
                match spec.filter(|s| s.open) {
                    Some(s) => {
                        let size = Size::new(
                            s.width.unwrap_or(1.0) as u32,
                            s.height.unwrap_or(1.0) as u32,
                        );
                        let fresh = self.surface.is_none();
                        if fresh {
                            self.made += 1;
                            let id = SurfaceId(100 + self.made);
                            self.r.attach_surface(id, node);
                            self.surface = Some((id, Buffer::new(size.w, size.h, Scale::ONE), 0));
                        }
                        let (id, buf, age) = self.surface.as_mut().unwrap();
                        if fresh || buf.size != size {
                            *buf = Buffer::new(size.w, size.h, Scale::ONE);
                            *age = 0;
                            self.r.configure_surface(*id, size, Scale::ONE);
                        }
                    }
                    None => {
                        if let Some((id, ..)) = self.surface.take() {
                            self.r.detach_surface(id);
                        }
                    }
                }
            }
        }
    }

    /// Paints the next frame if the surface wants one, then handles a
    /// wake. Returns false when nothing was painted.
    fn frame(&mut self) -> bool {
        let Some((id, buf, age)) = self.surface.as_mut() else {
            return false;
        };
        if !self.r.wants_frame(*id) {
            return false;
        }
        buf.paint_at(&mut self.r, *id, *age, frame(self.k));
        *age = 1;
        self.k += 1;
        if self.r.has_surface_changes() {
            self.r.update();
            self.sync();
        }
        true
    }

    fn px(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        self.surface.as_ref().map(|(_, b, _)| b.px(x, y))
    }

    fn height(&self) -> Option<f32> {
        self.r.surface_spec(self.root).and_then(|s| s.height)
    }

    /// Paints until nothing wants a frame; returns how many were painted.
    fn run(&mut self) -> u32 {
        let mut n = 0;
        while self.frame() {
            n += 1;
            assert!(n < 400, "never settled");
        }
        n
    }
}

/// A surface's poses under the real host order: opening attaches and
/// configures it after its spec opens (with `update` previewing it at
/// time zero), and `enter` still plays over several frames; closing
/// plays `exit`, and once the pose settles the surface is destroyed
/// with no other input; opening again makes a new surface that plays
/// `enter` again.
#[test]
fn surface_poses_play_under_the_host_loop() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Open, PropValue::Bool(false)),
            (
                Prop::Enter,
                pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.8))]),
            ),
        ],
    );
    let mut h = Host::new(b.diff, root);
    assert!(h.surface.is_none(), "closed: no surface");
    let alphas = |h: &mut Host| {
        let mut out = Vec::new();
        while h.frame() {
            out.push(h.px(30, 15).map_or(0, |p| p[3]));
            assert!(out.len() < 400, "never settled");
        }
        out
    };
    for round in 0..2 {
        let mut d = SceneDiff::new();
        d.set(root, Prop::Open, PropValue::Bool(true));
        h.apply(d);
        assert_eq!(h.made, round + 1, "a new surface each time it opens");
        let a = alphas(&mut h);
        assert!(a.len() >= 5, "enter plays over several frames: {a:?}");
        assert!(a[0] < 128, "starts near transparent: {a:?}");
        assert!(
            a[..5].windows(2).all(|w| w[1] > w[0]),
            "rises frame by frame: {a:?}"
        );
        assert_eq!(*a.last().unwrap(), 255, "{a:?}");
        let mut d = SceneDiff::new();
        d.set(root, Prop::Open, PropValue::Bool(false));
        h.apply(d);
        assert!(h.surface.is_some(), "stays while its exit plays");
        let a = alphas(&mut h);
        assert!(h.surface.is_none(), "destroyed once the exit settled");
        assert!(a.len() >= 4, "exit plays over several frames: {a:?}");
        assert!(
            a[..4].windows(2).all(|w| w[1] < w[0]),
            "fades frame by frame: {a:?}"
        );
        assert!(!h.r.surface_spec(root).unwrap().open);
    }
}

/// (eval-r3, popup-still-pose) A node removed with an exit pose at its
/// own values (`enter { opacity: 1 }`, mirrored) has nothing to move, so
/// the frame `update` previews matches the last one; the host loop still
/// paints the frame that ends its exit, and it unmounts. Removed as its
/// surface closes, the surface (kept open while it leaves) then closes
/// too, with no other input.
#[test]
fn an_exit_that_moves_nothing_still_ends_under_the_host_loop() {
    for close in [false, true] {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Width, num(60.0)),
                (Prop::Height, num(30.0)),
                (Prop::Open, PropValue::Bool(true)),
            ],
        );
        let card = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::Width, num(40.0)),
                (Prop::Height, num(20.0)),
                (Prop::Bg, color("#ff0000")),
                (Prop::Enter, pose(vec![(Prop::Opacity, num(1.0))])),
            ],
        );
        let mut h = Host::new(b.diff, root);
        h.run();
        assert_eq!(h.px(10, 10).map(|p| p[3]), Some(255), "shown");
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: card,
            window: false,
        });
        if close {
            d.set(root, Prop::Open, PropValue::Bool(false));
        }
        h.apply(d);
        assert!(h.r.tree().is_ghost(card), "it plays its exit");
        let n = h.run();
        assert!(n >= 1, "a frame ends the exit (close: {close})");
        assert_eq!(h.r.tree().ghost_count(), 0, "unmounted (close: {close})");
        if close {
            assert!(h.surface.is_none(), "the surface closed with it");
            assert!(!h.r.surface_spec(root).unwrap().open);
        } else {
            assert_eq!(h.px(10, 10).map(|p| p[3]), Some(0), "gone");
        }
    }
}

/// (eval-r3) The surface manager asks for a frame's blur region after
/// the paint, when the frame's end (an exit finishing, its ghost
/// unmounting) has already dropped the scene kept for the next frame:
/// it is still the region of the buffer just painted, not none, until a
/// frame without it is painted.
#[test]
fn the_frame_that_ends_an_exit_keeps_its_blur_region() {
    use strand_scene::Painter;
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Open, PropValue::Bool(true)),
            (Prop::Bg, PropValue::Color(hex("#1e1e2e").with_alpha(0.72))),
            (Prop::Blur, num(12.0)),
        ],
    );
    let card = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::Width, num(40.0)),
            (Prop::Height, num(20.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Exit, pose(vec![(Prop::Opacity, num(0.0))])),
        ],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    let id = h.surface.as_ref().map(|(id, ..)| *id).unwrap();
    assert_eq!(h.r.blur_region(id).len(), 1, "the panel's blur");
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: card,
        window: false,
    });
    h.apply(d);
    let mut n = 0;
    while h.frame() {
        n += 1;
        assert!(n < 400, "never settled");
        assert_eq!(h.r.blur_region(id).len(), 1, "after frame {n}");
    }
    assert!(n >= 2, "the exit played: {n}");
    assert_eq!(h.r.tree().ghost_count(), 0, "unmounted");
}

/// A content-sized panel shrinks after a change that moves nothing (a
/// row removed at the end, a height set `~ instant`), and after one that
/// springs, once it settled, all with no other input.
#[test]
fn a_content_sized_panel_shrinks_when_nothing_moves() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Open, PropValue::Bool(true)),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(40.0))]);
    let first = b.node(NodeKind::Box, Some(col), vec![(Prop::Height, num(40.0))]);
    let last = b.node(NodeKind::Box, Some(col), vec![(Prop::Height, num(50.0))]);
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.height(), Some(90.0));
    // The last row goes: nothing glides.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: last,
        window: false,
    });
    h.apply(d);
    h.run();
    assert_eq!(h.height(), Some(40.0), "a plain removal shrinks");
    // `~ instant`: the size snaps.
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: first,
        prop: Prop::Height,
        value: num(10.0),
        transition: Transition::Instant,
    });
    h.apply(d);
    h.run();
    assert_eq!(h.height(), Some(10.0), "an instant size change shrinks");
    // Grows (`~ instant`), then springs smaller: held at its size while
    // the spring moves, it shrinks once settled.
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: first,
        prop: Prop::Height,
        value: num(60.0),
        transition: Transition::Instant,
    });
    h.apply(d);
    h.run();
    assert_eq!(h.height(), Some(60.0));
    let mut d = SceneDiff::new();
    d.set(first, Prop::Height, num(20.0));
    h.apply(d);
    let mut frames = 0;
    while h.frame() {
        frames += 1;
        if h.r.animating(h.surface.as_ref().unwrap().0) {
            assert_eq!(h.height(), Some(60.0), "held while it springs");
        }
        assert!(frames < 400);
    }
    assert!(frames > 3, "springs over {frames} frames");
    assert_eq!(h.height(), Some(20.0), "a spring shrinks once settled");
}

/// Rows created out of a list's view never draw, so their `enter` pose
/// is dropped: the panel neither keeps asking for frames nor stays held
/// at a larger size.
#[test]
fn entering_rows_out_of_view_do_not_hold_a_content_sized_panel() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Open, PropValue::Bool(true)),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(40.0))]);
    let list = b.node(NodeKind::List, Some(col), vec![(Prop::Height, num(40.0))]);
    let tail = b.node(NodeKind::Box, Some(col), vec![(Prop::Height, num(50.0))]);
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.height(), Some(90.0));
    let mut d = SceneDiff::new();
    for i in 0..10u32 {
        let row = NodeId::new(50 + i, 0);
        d.create(row, NodeKind::Row, Some(list), i)
            .set(row, Prop::Height, num(20.0))
            .set(row, Prop::Bg, color("#ff0000"))
            .set(row, Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]));
    }
    h.apply(d);
    let n = h.run();
    assert!(n > 3 && n < 120, "the visible rows enter, then idle: {n}");
    let mut d = SceneDiff::new();
    d.set(tail, Prop::Height, num(10.0));
    h.apply(d);
    h.run();
    assert_eq!(h.height(), Some(50.0), "shrinks once the tail settled");
}

/// Exits are bounded without frames: rows removed with no frame painted
/// leave at most a few ghosts per parent, and a closing surface whose
/// frames stopped closes after the stall limit.
#[test]
fn exits_stay_bounded_without_frames() {
    let mut col = None;
    let mut st = Stage::new(60, 60, |b, root| {
        col = Some(b.node(NodeKind::Col, Some(root), vec![]));
    });
    let col = col.unwrap();
    let mut prev: Option<NodeId> = None;
    for i in 0..500u32 {
        let row = NodeId::new(10 + i, 0);
        let mut d = SceneDiff::new();
        d.create(row, NodeKind::Box, Some(col), 0)
            .set(row, Prop::Height, num(4.0))
            .set(row, Prop::Exit, pose(vec![(Prop::Opacity, num(0.0))]));
        if let Some(p) = prev {
            d.push(SceneOp::Remove {
                id: p,
                window: false,
            });
        }
        st.apply(d);
        prev = Some(row);
        assert!(
            st.r.tree().ghost_count() <= strand_render::MAX_GHOSTS_PER_PARENT,
            "{} ghosts after {i} removals",
            st.r.tree().ghost_count()
        );
    }
    assert!(st.r.tree().ghost_count() > 0, "the last ones still play");
    // Frames stop: a closing surface closes once the stall limit passes.
    st.r.set_exit_stall(Duration::from_millis(20));
    let root = st.root;
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(true)).set(
        root,
        Prop::Exit,
        pose(vec![(Prop::Opacity, num(0.0))]),
    );
    st.apply(d);
    st.settle(1);
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(false));
    st.apply(d);
    assert!(st.r.surface_spec(root).unwrap().open, "plays its exit");
    // The host's loop is told when to look again with no frame coming.
    let wake = st.r.next_wake().expect("a wake for the stalled exit");
    assert!(wake <= std::time::Instant::now() + Duration::from_millis(20));
    std::thread::sleep(Duration::from_millis(60));
    st.r.update();
    assert_eq!(st.r.next_wake(), None, "nothing left to wake for");
    assert!(
        !st.r.surface_spec(root).unwrap().open,
        "closed with no frame painted"
    );
    assert!(
        st.r.tree().ghost_count() == 0,
        "stalled ghosts unmounted too"
    );
}

/// The host needs no timer of its own for a stalled exit: the renderer's
/// timer thread wakes the loop (the waker the host gave the text worker,
/// whose handler runs `update`) when [`Renderer::next_wake`] comes, and
/// is cancelled once nothing is left to wake for, so an idle shell is
/// not woken again.
#[test]
fn a_stalled_exit_wakes_the_loop_by_itself() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let data = std::fs::read(test_font_path()).unwrap();
    let woken = Arc::new(AtomicUsize::new(0));
    let w2 = woken.clone();
    let worker = TextWorker::spawn_with_waker(
        FontConfig::isolated(vec![Arc::new(data)]),
        Some(Box::new(move || {
            w2.fetch_add(1, Ordering::SeqCst);
        })),
    )
    .unwrap();
    let r = Renderer::new(strand_render::TextBackend::Worker(worker));
    let mut st = Stage::with(r, 60, 60, |_, _| {});
    st.r.set_exit_stall(Duration::from_millis(40));
    let root = st.root;
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(true)).set(
        root,
        Prop::Exit,
        pose(vec![(Prop::Opacity, num(0.0))]),
    );
    st.apply(d);
    st.settle(1);
    let before = woken.load(Ordering::SeqCst);
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(false));
    st.apply(d);
    assert!(st.r.surface_spec(root).unwrap().open, "plays its exit");
    // No frame comes (the output sleeps) and the host calls nothing.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while woken.load(Ordering::SeqCst) == before {
        assert!(std::time::Instant::now() < deadline, "never woken");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The wake's handler.
    st.r.update();
    assert!(
        !st.r.surface_spec(root).unwrap().open,
        "closed with no frame painted"
    );
    assert_eq!(st.r.next_wake(), None);
    // Nothing armed any more: the loop stays asleep.
    let after = woken.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(woken.load(Ordering::SeqCst), after, "woken while idle");
}

/// The start of every wall-clock gate's failure message: the laptop's
/// container suite warns, instead of failing, only on failures that all
/// carry it (`scripts/container/gate-misses.sh`). Functional assertions
/// never carry it.
const GATE_MISS: &str = "timing gate missed";

/// The container suite's gate-miss check reads the marker these gates'
/// failures start with.
#[test]
fn the_gate_miss_marker_is_the_one_the_container_suite_reads() {
    let check = include_str!("../../../scripts/container/gate-misses.sh");
    assert!(check.contains(&format!("index(msg, \"{GATE_MISS}\") == 1")));
}

/// Frame time while the design's animated surfaces move: the launcher's
/// `enter { opacity: 0; scale: 0.96 }` over a 640×480 panel of rows, and
/// a toast leaving a stack of three (slide, fade and collapse while the
/// next slides up). In an optimised build (CI's `timing` job: `cargo
/// test --profile timing -p strand-render --test motion
/// animated_frames_fit_the_refresh_budget`) every frame must fit a 60 Hz
/// refresh (16.7 ms), and the median half of it; the gates are checked
/// once at the end, after the functional assertions, with the
/// `GATE_MISS` marker. A debug build only reports the times.
#[test]
fn animated_frames_fit_the_refresh_budget() {
    let gated = !cfg!(debug_assertions);
    let misses = std::cell::RefCell::new(Vec::<String>::new());
    let check = |name: &str, mut times: Vec<Duration>| {
        assert!(times.len() >= 5, "{name}: only {} frames", times.len());
        times.sort();
        let median = times[times.len() / 2];
        let worst = *times.last().unwrap();
        eprintln!(
            "{name}: {} frames, median {median:?}, worst {worst:?}",
            times.len()
        );
        if gated && median >= Duration::from_micros(8_333) {
            misses
                .borrow_mut()
                .push(format!("{name}: median {median:?} over 8.333 ms"));
        }
        if gated && worst >= Duration::from_micros(16_667) {
            misses
                .borrow_mut()
                .push(format!("{name}: worst {worst:?} over 16.667 ms"));
        }
    };
    let timed = |h: &mut Host| {
        let mut times = Vec::new();
        loop {
            let t = std::time::Instant::now();
            if !h.frame() {
                break;
            }
            times.push(t.elapsed());
            assert!(times.len() < 400);
        }
        times
    };

    // The launcher opening.
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(640.0)),
            (Prop::Height, num(480.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Radius, num(16.0)),
            (Prop::Open, PropValue::Bool(false)),
            (
                Prop::Enter,
                pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.96))]),
            ),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![(Prop::Pad, num(16.0)), (Prop::Gap, num(6.0))],
    );
    for i in 0..12 {
        let row = b.node(
            NodeKind::Row,
            Some(col),
            vec![
                (Prop::Pad, num(6.0)),
                (Prop::Radius, num(8.0)),
                (Prop::Bg, color(if i == 0 { "#45475a" } else { "#313244" })),
            ],
        );
        b.node(
            NodeKind::Text,
            Some(row),
            vec![(Prop::Text, text(&format!("Application number {i}")))],
        );
    }
    let mut h = Host::new(b.diff, root);
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(true));
    h.apply(d);
    let times = timed(&mut h);
    check("launcher enter", times);

    // A toast leaving.
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Open, PropValue::Bool(true))],
    );
    let stack = b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Width, num(380.0)),
            (Prop::Gap, num(8.0)),
            (Prop::Pad, num(12.0)),
        ],
    );
    let mut toasts = Vec::new();
    for i in 0..3 {
        let t = b.node(
            NodeKind::Col,
            Some(stack),
            vec![
                (Prop::Pad, num(12.0)),
                (Prop::Radius, num(12.0)),
                (Prop::Bg, color("#313244")),
                (
                    Prop::Shadow,
                    PropValue::Shadow(vec![Shadow {
                        x: 0.0,
                        y: 4.0,
                        blur: 12.0,
                        spread: 0.0,
                        color: hex("#00000080"),
                    }]),
                ),
                (
                    Prop::Exit,
                    pose(vec![
                        (Prop::X, num(420.0)),
                        (Prop::Opacity, num(0.0)),
                        (Prop::Height, num(0.0)),
                    ]),
                ),
            ],
        );
        b.node(
            NodeKind::Text,
            Some(t),
            vec![(Prop::Text, text(&format!("Mail: message {i} arrived")))],
        );
        toasts.push(t);
    }
    let mut h = Host::new(b.diff, root);
    h.run();
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: toasts[0],
        window: false,
    });
    h.apply(d);
    let times = timed(&mut h);
    check("toast exit", times);
    let misses = misses.into_inner();
    if !misses.is_empty() {
        panic!("{GATE_MISS}: {}", misses.join("\n"));
    }
}

/// Logic creating a node under the id of a ghost still playing its exit
/// (a fresh allocator after a hard reset) gets a plain live node: the
/// ghost unmounts and none of its motion carries over.
#[test]
fn a_node_created_over_a_ghost_id_is_live_and_at_rest() {
    let mut ids = Vec::new();
    let mut st = Stage::new(40, 40, |b, root| {
        let col = b.node(NodeKind::Col, Some(root), vec![]);
        ids.push(col);
        ids.push(red_box(
            b,
            col,
            vec![(Prop::Exit, pose(vec![(Prop::Opacity, num(0.0))]))],
        ));
    });
    let (col, id) = (ids[0], ids[1]);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove { id, window: false });
    st.apply(d);
    st.paint(frame(1));
    assert!(st.r.tree().is_ghost(id));
    let mut d = SceneDiff::new();
    d.create(id, NodeKind::Box, Some(col), 0)
        .set(id, Prop::Size, num(20.0))
        .set(id, Prop::Bg, color("#ff0000"));
    st.apply(d);
    assert!(!st.r.tree().is_ghost(id));
    assert_eq!(st.r.tree().ghost_count(), 0);
    assert_eq!(st.r.tree().get(col).unwrap().children, vec![id]);
    st.settle(2);
    assert_eq!(st.buf.px(10, 10)[3], 255, "drawn at rest");
    assert_eq!(st.r.hit(S, LogicalPoint::new(10.0, 10.0))[0], id, "and hit");
}

/// The design's toasts in their most common case, one toast at a time:
/// `open: shown.len > 0` changes in the same tick as the list. The first
/// toast is created by the diff that opens the panel and still slides in
/// over several frames; the last one is removed by the diff that closes
/// it and plays its exit before the surface goes.
#[test]
fn a_lone_toast_enters_with_its_panel_and_leaves_before_it_closes() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Open, PropValue::Bool(false)),
            (Prop::Anchor, PropValue::Keyword("top_right".into())),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(100.0))]);
    let mut h = Host::new(b.diff, root);
    assert!(h.surface.is_none());
    let toast = NodeId::new(50, 0);
    let mut d = SceneDiff::new();
    d.create(toast, NodeKind::Col, Some(col), 0)
        .set(toast, Prop::Height, num(30.0))
        .set(toast, Prop::Bg, color("#ff0000"))
        .set(
            toast,
            Prop::Enter,
            pose(vec![(Prop::X, num(60.0)), (Prop::Opacity, num(0.0))]),
        )
        .set(
            toast,
            Prop::Exit,
            pose(vec![
                (Prop::X, num(60.0)),
                (Prop::Opacity, num(0.0)),
                (Prop::Height, num(0.0)),
            ]),
        )
        .set(root, Prop::Open, PropValue::Bool(true));
    h.apply(d);
    assert!(h.surface.is_some(), "opened");
    assert_eq!(h.height(), Some(30.0));
    // The red edge on row 15, frame by frame (None: nothing red yet).
    let edge = |h: &Host| (0..100).find(|x| h.px(*x, 15).is_some_and(|p| p[2] > 128));
    let mut edges = Vec::new();
    while h.frame() {
        edges.push(edge(&h));
        assert!(edges.len() < 400, "never settled");
    }
    assert!(edges.len() >= 5, "enters over several frames: {edges:?}");
    assert_ne!(
        edges[0],
        Some(0),
        "the first frame shows the pose: {edges:?}"
    );
    assert_eq!(*edges.last().unwrap(), Some(0), "{edges:?}");
    let seen: Vec<u32> = edges.iter().flatten().copied().collect();
    assert!(seen.len() >= 3, "slides in, visibly: {edges:?}");
    assert!(seen.windows(2).all(|w| w[1] <= w[0]), "{edges:?}");
    // The last toast leaves as the panel closes.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: toast,
        window: false,
    });
    d.set(root, Prop::Open, PropValue::Bool(false));
    h.apply(d);
    assert!(h.surface.is_some(), "stays while the toast leaves");
    assert!(h.r.surface_spec(root).unwrap().open);
    // Read on row 2: its height collapses along $motion.spatial too (64%
    // two refreshes in), so row 15 leaves it after the first frame.
    let edge = |h: &Host| (0..100).find(|x| h.px(*x, 2).is_some_and(|p| p[2] > 128));
    let mut edges = Vec::new();
    while h.surface.is_some() && h.frame() {
        edges.push(edge(&h));
        assert!(edges.len() < 400, "never settled");
    }
    assert!(h.surface.is_none(), "closed once the exit played");
    assert!(!h.r.surface_spec(root).unwrap().open);
    assert!(
        edges.len() >= 4,
        "exit plays over several frames: {edges:?}"
    );
    let seen: Vec<u32> = edges.iter().flatten().copied().collect();
    assert!(
        seen.len() >= 2 && seen.windows(2).all(|w| w[1] >= w[0]),
        "slides out: {edges:?}"
    );
    assert!(seen[0] < 30, "starts where it was: {edges:?}");
    // Opening again with a new toast plays its enter again.
    let again = NodeId::new(51, 0);
    let mut d = SceneDiff::new();
    d.create(again, NodeKind::Col, Some(col), 0)
        .set(again, Prop::Height, num(30.0))
        .set(again, Prop::Bg, color("#ff0000"))
        .set(again, Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]))
        .set(root, Prop::Open, PropValue::Bool(true));
    h.apply(d);
    let mut alphas = Vec::new();
    while h.frame() {
        alphas.push(h.px(50, 15).map_or(0, |p| p[2]));
        assert!(alphas.len() < 400);
    }
    assert!(alphas.len() >= 4 && alphas[0] < 128, "{alphas:?}");
    assert_eq!(*alphas.last().unwrap(), 255);
}

/// `radius: full` and a percentage offset spring like any length: the
/// pill's corner fills in over several frames, and `x: 50%` (of the
/// parent) slides there.
#[test]
fn full_radius_and_percentage_offsets_spring() {
    let mut pill = NodeId::new(0, 0);
    let mut st = Stage::new(200, 40, |b, root| {
        pill = red_box(
            b,
            root,
            vec![
                (Prop::Width, num(60.0)),
                (Prop::Height, num(30.0)),
                (Prop::Radius, PropValue::Keyword("full".into())),
            ],
        );
    });
    let r = st.rect(pill);
    let corner = |st: &Stage| st.buf.px(r.x as u32 + 2, r.y as u32 + 2)[2];
    assert!(corner(&st) < 64, "a pill at rest");
    let mut d = SceneDiff::new();
    d.set(pill, Prop::Radius, num(0.0));
    st.apply(d);
    let mut seen = Vec::new();
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        seen.push(corner(&st));
        k += 1;
        assert!(k < 400);
    }
    assert!(seen.len() >= 4, "springs, not snaps: {seen:?}");
    assert!(seen[0] < 200, "{seen:?}");
    assert!(*seen.last().unwrap() > 200, "{seen:?}");
    assert!(seen.windows(2).all(|w| w[1] >= w[0]), "{seen:?}");
    // `x: 50%` of the 200 px panel: 100 px, gliding there.
    let mut d = SceneDiff::new();
    d.set(pill, Prop::X, PropValue::Length(Length::Percent(50.0)));
    st.apply(d);
    let mut xs = Vec::new();
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        xs.push(st.red_from(r.y as u32 + 15).unwrap());
        k += 1;
        assert!(k < 800);
    }
    let x0 = r.x as u32;
    assert!(xs.len() >= 4 && xs[0] > x0 && xs[0] < x0 + 90, "{xs:?}");
    assert_eq!(*xs.last().unwrap(), x0 + 100, "{xs:?}");
}

/// A centred content-sized panel that grows (an `if` branch entering)
/// is re-centred by the compositor: its rows are drawn where they were
/// on screen and glide to their new place, never jumping by half the
/// growth (and its box springs with them, `growth.rs`).
#[test]
fn a_centred_panel_keeps_its_rows_in_place_as_it_grows() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("center".into())),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![(Prop::Width, num(100.0)), (Prop::Gap, num(4.0))],
    );
    b.node(
        NodeKind::Box,
        Some(col),
        vec![(Prop::Height, num(30.0)), (Prop::Bg, color("#ff0000"))],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.height(), Some(30.0));
    // Screen y of the red row's top: the compositor centres the buffer.
    let screen = |h: &Host| {
        let (_, buf, _) = h.surface.as_ref().unwrap();
        let top = (0..buf.size.h).find(|y| buf.px(50, *y)[2] > 128).unwrap() as f32;
        top - buf.size.h as f32 / 2.0
    };
    assert_eq!(screen(&h), -15.0);
    let branch = NodeId::new(60, 0);
    let mut d = SceneDiff::new();
    d.create(branch, NodeKind::Box, Some(col), 1)
        .set(branch, Prop::Height, num(20.0))
        .set(branch, Prop::Bg, color("#00ff00"))
        .set(branch, Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]));
    h.apply(d);
    assert_eq!(h.height(), Some(54.0));
    let mut ys = Vec::new();
    while h.frame() {
        ys.push(screen(&h));
        assert!(ys.len() < 400);
    }
    assert!(ys.len() >= 4, "{ys:?}");
    // The first frame is one refresh along $motion.spatial: 26% of the
    // 12 px the centred row moves (3.1 px), not a jump.
    assert!(ys[0] >= -19.0, "stays where it was: {ys:?}");
    assert_eq!(*ys.last().unwrap(), -27.0, "{ys:?}");
    // (growth) With the box, which springs over the step: at most its
    // fastest refresh (38% of the 12 px) and a pixel of rounding at
    // either end.
    assert!(
        ys.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] <= 6.6),
        "glides there: {ys:?}"
    );
}

/// (popup-shrink) A centred content-sized panel whose rows spring
/// narrower is held at its larger size while they move, and re-centred
/// by the compositor at its smaller size once they settle: its rows
/// shrink about the screen's centre, laid out where the smaller box will
/// be, and stay there as the buffer shrinks, never jumping by half the
/// shrink.
#[test]
fn a_centred_panel_keeps_its_rows_in_place_as_it_shrinks() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("center".into())),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(100.0))]);
    b.node(
        NodeKind::Box,
        Some(col),
        vec![(Prop::Height, num(30.0)), (Prop::Bg, color("#ff0000"))],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    // Screen x of the red row's centre, twice (whole pixels): the
    // compositor centres the buffer.
    let screen = |h: &Host| {
        let (_, buf, _) = h.surface.as_ref().unwrap();
        let red: Vec<u32> = (0..buf.size.w)
            .filter(|x| buf.px(*x, 15)[2] > 128)
            .collect();
        let (a, z) = (*red.first().unwrap(), *red.last().unwrap());
        (a + z + 1) as i64 - buf.size.w as i64
    };
    assert_eq!(h.surface.as_ref().unwrap().1.size.w, 100);
    assert_eq!(screen(&h), 0);
    let mut d = SceneDiff::new();
    d.set(col, Prop::Width, num(40.0));
    h.apply(d);
    // Read each frame as it is painted, before a resize it asks for
    // replaces the buffer.
    let (mut seen, mut widths) = (Vec::new(), Vec::new());
    loop {
        let Some((id, buf, age)) = h.surface.as_mut() else {
            panic!("closed");
        };
        if !h.r.wants_frame(*id) {
            break;
        }
        buf.paint_at(&mut h.r, *id, *age, frame(h.k));
        *age = 1;
        h.k += 1;
        widths.push(buf.size.w);
        seen.push(screen(&h));
        if h.r.has_surface_changes() {
            h.r.update();
            h.sync();
        }
        assert!(seen.len() < 400, "never settled");
    }
    assert!(seen.len() >= 4, "springs: {seen:?}");
    assert_eq!(widths[0], 100, "held at its larger size: {widths:?}");
    assert_eq!(*widths.last().unwrap(), 40, "then shrinks: {widths:?}");
    assert!(
        seen.iter().all(|c| c.abs() <= 1),
        "stays centred on screen: {seen:?} (buffers {widths:?})"
    );
}

/// (bottom-osd-shrink) A bottom-anchored content-sized OSD whose upper
/// row leaves on an exit pose is held at its larger size while it plays,
/// its last row laid out where the smaller box will be, and shrinks once
/// it settles: the compositor keeps the buffer's bottom edge, so the row
/// stays where it is on screen. Before, the pass in the smaller buffer
/// read the row's move in the buffer (48 px up) as a move, and glided it
/// back from 48 px below its place, under the OSD's bottom edge.
#[test]
fn a_bottom_osd_keeps_its_last_row_in_place_as_it_shrinks() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Osd,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("bottom".into())),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![(Prop::Width, num(60.0)), (Prop::Gap, num(8.0))],
    );
    let extra = b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#0000ff")),
            (
                Prop::Exit,
                pose(vec![(Prop::Height, num(0.0)), (Prop::Opacity, num(0.0))]),
            ),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(col),
        vec![(Prop::Height, num(20.0)), (Prop::Bg, color("#ff0000"))],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    // How far above the buffer's bottom edge, which the compositor
    // keeps, the red row's top is (its first row over half covered: a
    // pixel either way while the exit springs it through fractions).
    let lift = |h: &Host| {
        let (_, buf, _) = h.surface.as_ref().unwrap();
        let top = (0..buf.size.h)
            .find(|y| buf.px(30, *y)[2] > 128)
            .expect("the row is drawn");
        buf.size.h as i64 - top as i64
    };
    assert_eq!(h.surface.as_ref().unwrap().1.size.h, 68);
    assert_eq!(lift(&h), 20);
    let mut d = SceneDiff::new();
    d.remove(extra);
    h.apply(d);
    let (mut seen, mut heights) = (Vec::new(), Vec::new());
    loop {
        let Some((id, buf, age)) = h.surface.as_mut() else {
            panic!("closed");
        };
        if !h.r.wants_frame(*id) {
            break;
        }
        buf.paint_at(&mut h.r, *id, *age, frame(h.k));
        *age = 1;
        h.k += 1;
        heights.push(buf.size.h);
        seen.push(lift(&h));
        if h.r.has_surface_changes() {
            h.r.update();
            h.sync();
        }
        assert!(seen.len() < 400, "never settled");
    }
    assert_eq!(heights[0], 68, "held at its larger size: {heights:?}");
    assert_eq!(*heights.last().unwrap(), 20, "then shrinks: {heights:?}");
    assert!(
        seen.iter().all(|l| (l - 20).abs() <= 1),
        "stays on the bottom edge: {seen:?} (buffers {heights:?})"
    );
}

/// The same at odd sizes, read as the compositor places the buffer: a
/// centred layer surface at `output / 2 - width / 2` in whole pixels (on
/// an output 1920 wide). A 101 px panel shrinking to 40 is laid out 30 px
/// into its held buffer, where the 40 px box will be (50 - 20), not 30.5
/// rounded: its row's left edge is at 940 on the last held frame and on
/// the first frame at its new size. Growing back, the row glides out
/// from where it was in the larger buffer (30 px in, `edge_shift`).
#[test]
fn a_centred_panel_at_odd_sizes_lands_where_the_compositor_puts_it() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("center".into())),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(101.0))]);
    b.node(
        NodeKind::Box,
        Some(col),
        vec![(Prop::Height, num(30.0)), (Prop::Bg, color("#ff0000"))],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    // The red row's left edge on screen.
    let screen = |h: &Host| {
        let (_, buf, _) = h.surface.as_ref().unwrap();
        let a = (0..buf.size.w)
            .find(|x| buf.px(*x, 15)[2] > 128)
            .expect("the row is drawn");
        960 - (buf.size.w / 2) as i64 + a as i64
    };
    assert_eq!(h.surface.as_ref().unwrap().1.size.w, 101);
    assert_eq!(screen(&h), 910);
    let run = |h: &mut Host, w: f32| {
        let mut d = SceneDiff::new();
        d.set(col, Prop::Width, num(w));
        h.apply(d);
        let (mut seen, mut widths) = (Vec::new(), Vec::new());
        loop {
            let Some((id, buf, age)) = h.surface.as_mut() else {
                panic!("closed");
            };
            if !h.r.wants_frame(*id) {
                break;
            }
            buf.paint_at(&mut h.r, *id, *age, frame(h.k));
            *age = 1;
            h.k += 1;
            widths.push(h.surface.as_ref().unwrap().1.size.w);
            seen.push(screen(h));
            if h.r.has_surface_changes() {
                h.r.update();
                h.sync();
            }
            assert!(seen.len() < 400, "never settled");
        }
        (seen, widths)
    };
    let (seen, widths) = run(&mut h, 40.0);
    assert_eq!(*widths.last().unwrap(), 40, "shrinks: {widths:?}");
    let held = widths.iter().rposition(|w| *w == 101).expect("held");
    assert_eq!(
        (seen[held], *seen.last().unwrap()),
        (940, 940),
        "where the 40 px box will be, and stays: {seen:?} (buffers {widths:?})"
    );
    let (seen, widths) = run(&mut h, 101.0);
    assert_eq!(widths[0], 101, "grows at once: {widths:?}");
    assert!(
        seen.windows(2).all(|p| p[1] <= p[0]) && seen[0] <= 940 && seen[0] > 910,
        "glides out from where it was, never back: {seen:?}"
    );
    assert_eq!(*seen.last().unwrap(), 910, "{seen:?}");
}

/// Two toasts in quick succession: the first opens the panel, the second
/// arrives in a later tick, before the panel's first frame (it waits for
/// its configure). Both enter: the panel is still opening.
#[test]
fn a_toast_arriving_before_the_first_frame_enters_too() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Open, PropValue::Bool(false)),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(100.0))]);
    let mut h = Host::new(b.diff, root);
    let enter = pose(vec![(Prop::X, num(60.0)), (Prop::Opacity, num(0.0))]);
    let toast = |d: &mut SceneDiff, id: NodeId, c: &str, i: u32| {
        d.create(id, NodeKind::Col, Some(col), i)
            .set(id, Prop::Height, num(30.0))
            .set(id, Prop::Bg, color(c))
            .set(id, Prop::Enter, enter.clone());
    };
    let (a, b2) = (NodeId::new(50, 0), NodeId::new(51, 0));
    let mut d = SceneDiff::new();
    toast(&mut d, a, "#ff0000", 0);
    d.set(root, Prop::Open, PropValue::Bool(true));
    h.apply(d);
    assert!(h.surface.is_some(), "opened");
    let mut d = SceneDiff::new();
    toast(&mut d, b2, "#00ff00", 1);
    h.apply(d);
    assert_eq!(h.height(), Some(60.0));
    let red = |h: &Host| (0..100).find(|x| h.px(*x, 15).is_some_and(|p| p[2] > 128));
    let green =
        |h: &Host| (0..100).find(|x| h.px(*x, 45).is_some_and(|p| p[1] > 128 && p[2] < 128));
    let mut edges = Vec::new();
    while h.frame() {
        edges.push((red(&h), green(&h)));
        assert!(edges.len() < 400, "never settled");
    }
    assert!(edges.len() >= 5, "{edges:?}");
    assert_ne!(edges[0].0, Some(0), "the first toast enters: {edges:?}");
    assert_ne!(edges[0].1, Some(0), "the second toast enters: {edges:?}");
    assert_eq!(*edges.last().unwrap(), (Some(0), Some(0)), "{edges:?}");
    // Once the panel is shown, a later toast enters as usual, and the
    // opening is over: nothing lingers.
    assert!(!h.r.animating(h.surface.as_ref().unwrap().0));
}

/// (stagger review) A stagger numbers only the children with an `enter`
/// on a surface still opening too: a staggered column's first tile is
/// created with the diff that opens the panel, then a plain box and a
/// second tile land in a later diff before the panel's first frame (both
/// enter, the panel still opening, as a toast does). The plain box is
/// drawn at rest at once and takes no turn, so the second tile starts
/// one step (100 ms) after the first, not two.
#[test]
fn a_plain_child_landing_while_its_surface_opens_takes_no_turn() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#000000")),
            (Prop::Open, PropValue::Bool(false)),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Width, num(100.0)),
            (
                Prop::Stagger,
                PropValue::Duration(Duration::from_millis(100)),
            ),
        ],
    );
    let mut h = Host::new(b.diff, root);
    let fade = pose(vec![(Prop::Opacity, num(0.0))]);
    let child = |d: &mut SceneDiff, id: NodeId, c: &str, i: u32, enter: bool| {
        d.create(id, NodeKind::Box, Some(col), i)
            .set(id, Prop::Height, num(20.0))
            .set(id, Prop::Bg, color(c));
        if enter {
            d.set(id, Prop::Enter, fade.clone());
        }
    };
    let (first, plain, second) = (NodeId::new(50, 0), NodeId::new(51, 0), NodeId::new(52, 0));
    let mut d = SceneDiff::new();
    child(&mut d, first, "#ff0000", 0, true);
    d.set(root, Prop::Open, PropValue::Bool(true));
    h.apply(d);
    assert!(h.surface.is_some(), "opened");
    let mut d = SceneDiff::new();
    child(&mut d, plain, "#0000ff", 1, false);
    child(&mut d, second, "#00ff00", 2, true);
    h.apply(d);
    let shown = |h: &Host, y: u32| h.px(50, y).is_some_and(|p| p[..3] != [0, 0, 0]);
    let mut seen = Vec::new();
    while h.frame() {
        seen.push((shown(&h, 10), shown(&h, 30), shown(&h, 50)));
        assert!(seen.len() < 400, "never settled");
    }
    assert!(seen[0].1, "the plain box at rest at once: {seen:?}");
    assert!(seen.iter().all(|s| s.1), "{seen:?}");
    let start = |i: usize| -> usize {
        seen.iter()
            .position(|s| if i == 0 { s.0 } else { s.2 })
            .unwrap_or(usize::MAX)
    };
    // Frames are 16.7 ms apart: the first tile shows within a frame or
    // two, the second one step later (frame 6 or 7), not two (12 or 13).
    let (a, c) = (start(0), start(2));
    assert!(a <= 2, "the first tile starts at once: {seen:?}");
    assert!(
        (5..=8).contains(&(c - a)),
        "the second tile one step after the first ({a}, {c}): {seen:?}"
    );
    assert!(seen.last().is_some_and(|s| *s == (true, true, true)));
}

/// (eval-coverage) A row mounted at the top of a content-sized panel
/// pushes the rows under it down, and the panel grows to fit: laid out
/// first in the buffer it had (too small, so its rows squeezed) and again
/// once the surface is configured at its new size, the rows glide from
/// where they were drawn to where they rest, together: the gap between
/// two of them never opens or closes on the way (the sandbox's
/// toast-stack: each started from where the squeezed pass put it).
#[test]
fn rows_pushed_down_in_a_growing_content_sized_panel_glide_together() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Anchor, PropValue::Keyword("top".into())),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![(Prop::Width, num(100.0)), (Prop::Gap, num(6.0))],
    );
    for c in ["#00ff00", "#0000ff"] {
        b.node(
            NodeKind::Box,
            Some(col),
            vec![(Prop::Height, num(30.0)), (Prop::Bg, color(c))],
        );
    }
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.height(), Some(66.0));
    let top = |h: &Host, hit: fn([u8; 4]) -> bool| {
        let rows = h.surface.as_ref().map_or(0, |(_, b, _)| b.size.h);
        (0..rows).find(|y| h.px(50, *y).is_some_and(hit))
    };
    let green = |p: [u8; 4]| p[1] > 128 && p[0] < 128 && p[2] < 128;
    // The buffer is BGRA.
    let blue = |p: [u8; 4]| p[0] > 128 && p[1] < 128 && p[2] < 128;
    assert_eq!((top(&h, green), top(&h, blue)), (Some(0), Some(36)));
    let red = NodeId::new(60, 0);
    let mut d = SceneDiff::new();
    d.create(red, NodeKind::Box, Some(col), 0)
        .set(red, Prop::Height, num(30.0))
        .set(red, Prop::Bg, color("#ff0000"));
    // As the runtime does: the diff's preview, then the surface manager's
    // configure at the size it asks for.
    assert!(h.r.apply(d).is_empty());
    h.r.update();
    h.sync();
    assert_eq!(h.height(), Some(102.0));
    let mut tops = Vec::new();
    while h.frame() {
        tops.push((top(&h, green), top(&h, blue)));
        assert!(tops.len() < 400, "never settled");
    }
    assert_eq!(*tops.last().unwrap(), (Some(36), Some(72)), "{tops:?}");
    assert!(tops[0].0 < Some(26), "starts near where it was: {tops:?}");
    assert!(
        tops.iter().all(
            |(g, b)| matches!((g, b), (Some(g), Some(b)) if (*b as i32 - *g as i32 - 36).abs() <= 1)
        ),
        "the gap holds: {tops:?}"
    );
}

/// (held-glides) Paints `h` until nothing moves, calling `read` on each
/// frame as it is painted, before a resize it asks for replaces the
/// buffer.
fn paint_each(h: &mut Host, mut read: impl FnMut(&Buffer)) -> usize {
    let mut n = 0;
    loop {
        let Some((id, buf, age)) = h.surface.as_mut() else {
            panic!("closed");
        };
        if !h.r.wants_frame(*id) {
            return n;
        }
        buf.paint_at(&mut h.r, *id, *age, frame(h.k));
        *age = 1;
        h.k += 1;
        n += 1;
        read(buf);
        if h.r.has_surface_changes() {
            h.r.update();
            h.sync();
        }
        assert!(n < 400, "never settled");
    }
}

/// The buffer is BGRA.
fn is_red(p: [u8; 4]) -> bool {
    p[2] > 128 && p[1] < 128 && p[0] < 128
}

fn is_green(p: [u8; 4]) -> bool {
    p[1] > 128 && p[2] < 128 && p[0] < 128
}

/// (held-glides) Two rows of a centred content-sized panel swap places
/// in the frame its hold lets go: the panel was held at its larger size
/// while its other rows folded away, its content laid out where the
/// smaller box would be (popup-shrink), and the compositor re-centres
/// the smaller buffer as the swap is laid out. Each row glides from the
/// slot it was drawn in on screen to the other: neither moves by where
/// the root was placed in the held buffer (the sandbox's launcher: a row
/// that stayed glided 90 px, starting 67 px below where it was).
#[test]
fn rows_swapped_as_a_centred_panels_hold_lets_go_glide_from_where_they_were() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Anchor, PropValue::Keyword("center".into()))],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(100.0))]);
    let mut rows = Vec::new();
    for (c, w) in [
        ("#ff0000", 50.0),
        ("#00ff00", 100.0),
        ("#0000ff", 100.0),
        ("#0000ff", 100.0),
    ] {
        rows.push(b.node(
            NodeKind::Box,
            Some(col),
            vec![
                (Prop::Width, num(w)),
                (Prop::Height, num(30.0)),
                (Prop::Bg, color(c)),
                (Prop::Exit, pose(vec![(Prop::Height, num(0.0))])),
            ],
        ));
    }
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.height(), Some(120.0));
    let mut d = SceneDiff::new();
    for id in &rows[2..] {
        d.push(SceneOp::Remove {
            id: *id,
            window: false,
        });
    }
    h.apply(d);
    // Folded away in the held buffer, until it lets go.
    let mut n = 0;
    while h.surface.as_ref().unwrap().1.size.h == 120 {
        assert!(h.frame(), "held while they fold");
        n += 1;
        assert!(n < 400, "never let go");
    }
    assert_eq!(h.surface.as_ref().unwrap().1.size.h, 60);
    // The swap, before the first frame in the smaller buffer.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Move {
        id: rows[0],
        parent: Some(col),
        index: 1,
    });
    h.apply(d);
    // Each row's top on screen (twice, whole pixels: the compositor
    // centres the buffer): red was in the upper slot (-60), green in the
    // lower (0). Red is the narrower: it is read left of 50, green right
    // of it, where nothing passes over it.
    let mut seen = Vec::new();
    paint_each(&mut h, |buf| {
        let top = |x: u32, hit: fn([u8; 4]) -> bool| {
            (0..buf.size.h)
                .find(|y| hit(buf.px(x, *y)))
                .map(|y| 2 * y as i64 - buf.size.h as i64)
        };
        seen.push((top(25, is_red), top(75, is_green)));
    });
    assert!(seen.len() >= 4, "they glide: {seen:?}");
    assert_eq!(*seen.last().unwrap(), (Some(0), Some(-60)), "{seen:?}");
    let (red, green) = (seen[0].0.unwrap(), seen[0].1.unwrap());
    // One refresh along $motion.spatial: about a quarter of the 30 px
    // each moves, not more than half (60 and 30 here, counted twice).
    assert!(
        (-60..=-30).contains(&red) && (-30..=0).contains(&green),
        "each starts from its slot: {seen:?}"
    );
    // And glides on along the spring, from the slot it left.
    let reds: Vec<i32> = seen.iter().map(|s| s.0.unwrap() as i32).collect();
    let greens: Vec<i32> = seen.iter().map(|s| s.1.unwrap() as i32).collect();
    assert_springs_to(&reds, -60, 0);
    assert_springs_to(&greens, 0, -60);
}

/// A panel that opens with rows created at boot (reported closed, then
/// opened by a later diff) shows them at rest: only rows born with the
/// opening enter.
#[test]
fn rows_older_than_the_opening_show_at_rest() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Open, PropValue::Bool(false)),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Width, num(40.0))]);
    b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Height, num(20.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))])),
        ],
    );
    let mut h = Host::new(b.diff, root);
    let mut d = SceneDiff::new();
    d.set(root, Prop::Open, PropValue::Bool(true));
    h.apply(d);
    assert!(h.frame());
    assert_eq!(h.px(20, 10).unwrap()[2], 255, "at rest on the first frame");
    assert!(!h.frame(), "nothing moves");
}

/// `rotate` arrives from the compiler as an angle and every in-flight
/// sample is one: a static `rotate: 90deg` turns an 80 × 10 bar upright,
/// and a rotate spring draws it in between while it moves.
#[test]
fn rotate_draws_static_and_in_flight() {
    // Red rows down the centre column.
    let rows = |st: &Stage| (0..100).filter(|y| st.buf.px(50, *y)[2] > 128).count();
    let bar = |b: &mut Builder, root: NodeId, extra: Vec<(Prop, PropValue)>| {
        let mut props = vec![
            (Prop::Place, kw("absolute")),
            (Prop::X, num(10.0)),
            (Prop::Y, num(45.0)),
            (Prop::Width, num(80.0)),
            (Prop::Height, num(10.0)),
            (Prop::Bg, color("#ff0000")),
        ];
        props.extend(extra);
        b.node(NodeKind::Box, Some(root), props)
    };
    let st = Stage::new(100, 100, |b, root| {
        bar(b, root, vec![(Prop::Rotate, PropValue::Angle(90.0))]);
    });
    let n = rows(&st);
    assert!((78..=82).contains(&n), "upright: {n} rows");
    let mut id = None;
    let mut st = Stage::new(100, 100, |b, root| {
        id = Some(bar(b, root, vec![]));
    });
    assert!((9..=11).contains(&rows(&st)), "flat: {}", rows(&st));
    let mut d = SceneDiff::new();
    d.set(id.unwrap(), Prop::Rotate, PropValue::Angle(90.0));
    st.apply(d);
    let mut seen = Vec::new();
    for k in 1..60 {
        st.paint(frame(k));
        seen.push(rows(&st));
    }
    assert!(
        seen.iter().any(|n| (14..=70).contains(n)),
        "drawn between flat and upright: {seen:?}"
    );
    let n = *seen.last().unwrap();
    assert!((78..=82).contains(&n), "{seen:?}");
}

/// A rotated pill is hit on its rounded shape, not on its bounding box.
#[test]
fn a_rotated_pill_is_hit_on_its_shape() {
    let mut id = None;
    let mut st = Stage::new(100, 100, |b, root| {
        id = Some(b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::Place, kw("absolute")),
                (Prop::X, num(10.0)),
                (Prop::Y, num(40.0)),
                (Prop::Width, num(80.0)),
                (Prop::Height, num(20.0)),
                (Prop::Radius, num(10.0)),
                (Prop::Bg, color("#ff0000")),
                (Prop::Rotate, PropValue::Angle(45.0)),
            ],
        ));
    });
    let id = id.unwrap();
    st.paint(frame(1));
    let hit =
        |st: &Stage, x: f32, y: f32| st.r.hit(S, LogicalPoint::new(x, y)).first() == Some(&id);
    assert!(hit(&st, 50.0, 50.0), "centre");
    assert!(hit(&st, 70.0, 70.0), "along its axis");
    assert!(hit(&st, 30.0, 30.0), "along its axis");
    // Inside the bounding box (14.6 … 85.4) but off the shape.
    assert!(!hit(&st, 20.0, 80.0), "a bounding-box corner");
    assert!(!hit(&st, 80.0, 20.0), "a bounding-box corner");
    assert!(!hit(&st, 50.0, 67.0), "beside it");
    // The rounded end: past the cap along the axis.
    assert!(!hit(&st, 19.0, 19.0), "past the rounded end");
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// One root on two outputs of different widths (`screens: all`): a row
/// leaving where only the wide one shows it still plays its exit there,
/// though the narrow one's frames do not draw it.
#[test]
fn an_exit_seen_on_one_of_two_outputs_plays_there() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    // As tall as its output: the short one shows two rows.
    let list = b.node(
        NodeKind::List,
        Some(root),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, PropValue::Length(Length::Percent(100.0))),
        ],
    );
    let mut rows = Vec::new();
    for i in 0..6 {
        rows.push(b.node(
            NodeKind::Box,
            Some(list),
            vec![
                (Prop::Height, num(20.0)),
                (Prop::Bg, color(if i == 4 { "#ff0000" } else { "#203040" })),
                (Prop::Exit, pose(vec![(Prop::Opacity, num(0.0))])),
            ],
        ));
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let (wide, narrow) = (SurfaceId(1), SurfaceId(2));
    r.attach_surface(wide, root);
    r.attach_surface(narrow, root);
    let mut wb = Buffer::new(60, 120, Scale::ONE);
    let mut nb = Buffer::new(60, 40, Scale::ONE);
    wb.paint_at(&mut r, wide, 0, T0);
    nb.paint_at(&mut r, narrow, 0, T0);
    assert!(wb.px(30, 90)[2] > 200, "row 4 shows on the tall output");
    // Row 0, on both, changes colour as row 4 leaves: the short output
    // paints frames too, none of which draws row 4.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: rows[4],
        window: false,
    });
    d.set(rows[0], Prop::Bg, color("#80a0c0"));
    assert!(r.apply(d).is_empty());
    let mut reds = Vec::new();
    let mut narrow_frames = 0;
    for k in 1..60 {
        if r.wants_frame(narrow) {
            nb.paint_at(&mut r, narrow, 1, frame(k));
            narrow_frames += 1;
        }
        if !r.wants_frame(wide) {
            break;
        }
        wb.paint_at(&mut r, wide, 1, frame(k));
        reds.push(wb.px(30, 90)[2]);
    }
    assert!(narrow_frames >= 3, "{narrow_frames}");
    assert!(reds.len() >= 4, "fades over several frames: {reds:?}");
    assert!(reds[0] > 100, "{reds:?}");
    assert!(reds[..4].windows(2).all(|w| w[1] < w[0]), "{reds:?}");
}

/// A list of `n` 30 px red-edged rows in a 200 px viewport.
fn scroll_stage(n: usize, row_props: Vec<(Prop, PropValue)>) -> (Stage, NodeId, Vec<NodeId>) {
    let mut lst = None;
    let mut rows = Vec::new();
    let st = Stage::new(120, 200, |b, root| {
        let l = b.node(NodeKind::List, Some(root), vec![(Prop::Height, num(200.0))]);
        lst = Some(l);
        for _ in 0..n {
            let mut props = vec![(Prop::Height, num(30.0)), (Prop::Bg, color("#ff0000"))];
            props.extend(row_props.iter().cloned());
            rows.push(b.node(NodeKind::Box, Some(l), props));
        }
    });
    (st, lst.unwrap(), rows)
}

fn wheel(dy: f32, time: u32) -> strand_render::ScrollInput {
    strand_render::ScrollInput {
        dy,
        kind: strand_render::ScrollKind::Wheel,
        time,
    }
}

fn touch(dy: f32, time: u32) -> strand_render::ScrollInput {
    strand_render::ScrollInput {
        dy,
        kind: strand_render::ScrollKind::Touch,
        time,
    }
}

/// Wheel steps spring the offset (`$motion.spatial`): the first frame
/// after a step is on its way, every frame moves further without
/// overshooting far, and it settles at the step's sum. A second step in
/// flight retargets from where it is, with its velocity. Rows move by a
/// paint offset: no layout pass runs while the view stays within the
/// rows laid out, and the frames stop once it lands.
#[test]
fn wheel_steps_spring_the_offset() {
    let (mut st, lst, _) = scroll_stage(200, vec![]);
    let at = LogicalPoint::new(60.0, 100.0);
    let passes = st.r.layout_passes();
    assert_eq!(st.r.scroll_input(S, at, wheel(45.0, 0)), Some(lst));
    assert!(st.r.wants_frame(S));
    let mut seen = Vec::new();
    for k in 1..4 {
        st.paint(frame(k));
        seen.push(st.r.scroll_offset(lst).unwrap());
    }
    assert!(seen[0] > 0.0 && seen[0] < 45.0, "{seen:?}");
    assert!(seen.windows(2).all(|w| w[1] > w[0]), "{seen:?}");
    // A second step mid-flight: the target is 90, and the offset keeps
    // going from where it was.
    st.r.scroll_input(S, at, wheel(45.0, 50));
    st.paint(frame(4));
    let now = st.r.scroll_offset(lst).unwrap();
    assert!(now > seen[2] && now < 90.0, "{now} after {seen:?}");
    let mut k = 5;
    let mut top = now;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        top = top.max(st.r.scroll_offset(lst).unwrap());
        k += 1;
        assert!(k < 200, "never settled");
    }
    assert_eq!(st.r.scroll_offset(lst), Some(90.0));
    // `$motion.spatial` is damped 0.9: it may overshoot a hair.
    assert!(top < 92.0, "overshot to {top}");
    assert_eq!(st.r.layout_passes(), passes, "scrolling laid nothing out");
    // The rows are drawn where the offset puts them: row 3 (90..120)
    // at the top.
    assert_eq!(st.red_top(60), Some(0));
    let boxes = st.r.boxes(S).unwrap();
    let row3 = st.r.tree().get(lst).unwrap().children[3];
    assert_eq!(boxes.rects[&row3].y, 0.0);

    // Under `reduced_motion` a step lands at once.
    st.r.set_reduced_motion(true);
    st.r.scroll_input(S, at, wheel(30.0, 400));
    st.paint(frame(k));
    assert_eq!(st.r.scroll_offset(lst), Some(120.0));
    assert!(!st.r.wants_frame(S));
}

/// A touchpad moves the offset with the fingers, and lifting them while
/// moving flings it: the velocity decays (`FLING_DECAY`), each frame
/// moves less than the one before, and it stops by itself, about
/// `velocity × FLING_DECAY` further on. A list scrolled past what it laid
/// out lays out only the rows that came into view, each once.
#[test]
fn a_touchpad_fling_decays_and_stops() {
    let (mut st, lst, _) = scroll_stage(400, vec![]);
    let at = LogicalPoint::new(60.0, 100.0);
    // 10 px every 10 ms: 1,000 px/s.
    for i in 0..4 {
        st.r.scroll_input(S, at, touch(10.0, 1000 + i * 10));
    }
    st.paint(frame(1));
    assert_eq!(st.r.scroll_offset(lst), Some(40.0), "follows the fingers");
    let lift = strand_render::ScrollInput {
        dy: 0.0,
        kind: strand_render::ScrollKind::Lift,
        time: 1030,
    };
    assert_eq!(st.r.scroll_input(S, at, lift), Some(lst));
    let mut offs = vec![40.0];
    let mut k = 2;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        offs.push(st.r.scroll_offset(lst).unwrap());
        k += 1;
        assert!(k < 400, "the fling never stopped");
    }
    let steps: Vec<f32> = offs.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(steps[1] > 0.0, "{offs:?}");
    assert!(
        steps.windows(2).all(|w| w[1] <= w[0] + 0.01),
        "each frame moves less: {steps:?}"
    );
    let travel = offs.last().unwrap() - 40.0;
    let expect = 1000.0 * strand_render::FLING_DECAY;
    assert!(
        (travel - expect).abs() < 0.15 * expect,
        "flung {travel} px, about {expect} expected"
    );
    // Rows came into view and were laid out once each.
    let rows_in = st.r.scroll_rows_laid_out();
    assert!(rows_in > 0 && rows_in <= 400 / 30 + 12, "{rows_in}");
    // A lift with the fingers at rest flings nothing.
    st.r.scroll_input(S, at, touch(5.0, 5000));
    let still = strand_render::ScrollInput { time: 5200, ..lift };
    assert_eq!(st.r.scroll_input(S, at, still), None);
}

/// Rows a list's window mounts and unmounts as it scrolls (`window:
/// true`) play no `enter` or `exit` pose, and their siblings no FLIP: a
/// row mounted that way is drawn at rest on its first frame, one
/// unmounted that way leaves at once with no ghost, and the rows around
/// them stay where their global indexes put them. A row the data adds
/// (`window: false`) still enters.
#[test]
fn window_mounts_do_not_play_poses() {
    let enter = (
        Prop::Enter,
        pose(vec![(Prop::Opacity, num(0.0)), (Prop::X, num(60.0))]),
    );
    let (mut st, lst, rows) = scroll_stage(4, vec![enter.clone()]);
    // A view of four rows, so the mounted rows fill it once it has
    // shrunk (while it shrinks from 200 px, frames show the unmounted
    // rows below: a gap, counted before the window moves).
    let mut d = SceneDiff::new();
    d.set(lst, Prop::RowCount, num(100.0))
        .set(lst, Prop::RowFirst, num(0.0))
        .set(lst, Prop::Height, num(120.0));
    st.apply(d);
    st.settle(1);
    let gaps = st.r.list_frames().gaps;
    // The window moves down a row: row 0 goes, a row comes in below.
    let new = NodeId::new(500, 0);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: rows[0],
        window: true,
    });
    d.push(SceneOp::Create {
        id: new,
        kind: NodeKind::Box,
        parent: Some(lst),
        index: 3,
        window: true,
    });
    d.set(new, Prop::Height, num(30.0))
        .set(new, Prop::Bg, color("#ff0000"))
        .set(new, Prop::Enter, enter.1.clone())
        .set(lst, Prop::RowFirst, num(1.0));
    st.apply(d);
    assert!(st.r.tree().get(rows[0]).is_none(), "no ghost");
    st.paint(frame(30));
    // At rest on its first frame (its enter pose would draw it clear and
    // 60 px to the right), and the rows sit 30 px apart from global row
    // 1 (the view, still asked for the top, shows the mounted rows'
    // first: row 1 at the top).
    let r1 = st.rect(rows[1]);
    let rn = st.rect(new);
    assert_eq!((r1.y, rn.y), (0.0, 90.0), "{r1:?} {rn:?}");
    assert_eq!(st.red_from(100), Some(0));
    assert_eq!(st.buf.px(10, 100), [0, 0, 255, 255]);
    assert_eq!(st.red_from(10), Some(0), "row 1 did not glide");
    assert_eq!(st.r.list_frames().gaps, gaps);
    // A row the data inserts enters.
    let data = NodeId::new(501, 0);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Create {
        id: data,
        kind: NodeKind::Box,
        parent: Some(lst),
        index: 1,
        window: false,
    });
    d.set(data, Prop::Height, num(30.0))
        .set(data, Prop::Bg, color("#ff0000"))
        .set(data, Prop::Enter, enter.1)
        .set(lst, Prop::RowCount, num(101.0));
    st.apply(d);
    st.paint(frame(31));
    assert!(st.r.wants_frame(S), "a data row enters");
}

/// A run of columns, first to last.
type Span = Option<(u32, u32)>;

/// The columns of row `y` showing red (`r`) and green (`g`) at most a
/// little darkened (a page sliding is drawn at full opacity).
fn spans(st: &Stage, y: u32) -> (Span, Span) {
    let mut red: Span = None;
    let mut green: Span = None;
    for x in 0..st.buf.size.w {
        let p = st.buf.px(x, y);
        let grow = |s: &mut Span| {
            *s = Some(s.map_or((x, x), |(a, _)| (a, x)));
        };
        if p[2] > 200 && p[1] < 60 {
            grow(&mut red);
        }
        if p[1] > 200 && p[2] < 60 {
            grow(&mut green);
        }
    }
    (red, green)
}

/// `pages` slides between pages by their source order (`row_first`):
/// going forward the new page comes in from the right while the old one
/// leaves to the left, both moving every frame, and `pages` clips them;
/// going back mirrors it. The pairing is readable while the old page
/// plays out (`page_swap`). A page with its own `enter` plays that, and
/// under `reduced_motion` the swap snaps.
#[test]
fn pages_slide_by_source_order() {
    let red =
        |b: &mut Builder, p| b.node(NodeKind::Page, Some(p), vec![(Prop::Bg, color("#ff0000"))]);
    let mut pages = None;
    let mut a = None;
    let mut st = Stage::new(200, 60, |b, root| {
        let p = b.node(
            NodeKind::Pages,
            Some(root),
            vec![
                (Prop::Width, num(120.0)),
                (Prop::Height, num(60.0)),
                (Prop::RowFirst, num(10.0)),
            ],
        );
        pages = Some(p);
        a = Some(red(b, p));
    });
    let (pages, a) = (pages.unwrap(), a.unwrap());
    assert_eq!(spans(&st, 30), (Some((0, 119)), None));
    let swap = |st: &mut Stage, out: NodeId, inn: NodeId, bg: &str, first: f32, enter: bool| {
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: out,
            window: false,
        });
        d.create(inn, NodeKind::Page, Some(pages), 0)
            .set(inn, Prop::Bg, color(bg));
        if enter {
            d.set(inn, Prop::Enter, PropValue::Keyword("fade".into()));
        }
        d.set(pages, Prop::RowFirst, num(first));
        st.apply(d);
    };

    // Forward: b (green) comes in from the right, a (red) leaves left.
    let b = NodeId::new(500, 0);
    swap(&mut st, a, b, "#00ff00", 20.0, false);
    assert_eq!(
        st.r.page_swap(pages),
        Some(strand_render::PageSwap {
            entering: Some(b),
            leaving: Some(a),
            forward: true
        })
    );
    // $motion.spatial overshoots by up to 2.84% of the 120 px slide, so
    // the new page may pass its place by 4 px (its right edge short of
    // the clip for a frame or two) before it comes back.
    let reach = (120.0 * SPATIAL_OVERSHOOT).ceil() as u32 + 1;
    let mut k = 1;
    let mut greens = vec![120];
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        k += 1;
        let (r, g) = spans(&st, 30);
        let gx = g.map_or(120, |(x0, x1)| {
            assert!(
                x1 == 119 || x1 + reach >= 119 && x0 == 0,
                "the new page's right edge is clipped: {g:?}"
            );
            x0
        });
        if let Some((x0, x1)) = r {
            assert_eq!(x0, 0, "the old page leaves to the left");
            assert!(x1 < gx, "{r:?} {g:?}");
        }
        for x in 120..200 {
            assert_eq!(st.buf.px(x, 30)[1], st.buf.px(199, 30)[1], "clipped at {x}");
        }
        if greens.len() == 1 {
            assert!(gx > 40 && gx < 120, "on its way in: {gx}");
        }
        greens.push(gx);
        assert!(k < 200, "never settled");
    }
    // It moves left, by the spring: past its place by no more than the
    // overshoot, turning back once.
    let greens: Vec<i32> = greens.iter().map(|x| *x as i32).collect();
    assert_springs_to(&greens, 120, 0);
    let frames = greens.len() - 1;
    assert!(frames >= 5, "{frames} frames");
    assert_eq!(spans(&st, 30), (None, Some((0, 119))));
    assert!(st.r.tree().get(a).is_none(), "the old page is gone");
    assert_eq!(st.r.page_swap(pages), None);

    // Back: a2 (red) comes in from the left, b leaves to the right.
    let a2 = NodeId::new(501, 0);
    swap(&mut st, b, a2, "#ff0000", 10.0, false);
    st.paint(frame(k));
    k += 1;
    let (r, g) = spans(&st, 30);
    let (r, g) = (r.unwrap(), g.unwrap());
    assert_eq!((r.0, g.1), (0, 119), "{r:?} {g:?}");
    assert!(r.1 < 80 && g.0 > r.1, "{r:?} {g:?}");
    k = st.settle(k);
    assert_eq!(spans(&st, 30), (Some((0, 119)), None));

    // A page with its own enter plays it (a fade: in place, never to the
    // side), and the old page with no pose of its own still slides out.
    let c = NodeId::new(502, 0);
    swap(&mut st, a2, c, "#00ff00", 30.0, true);
    st.paint(frame(k));
    k += 1;
    let (r, g) = spans(&st, 30);
    assert!(g.is_none(), "fading in, not yet bright: {g:?}");
    let r = r.unwrap();
    assert!(r.0 == 0 && r.1 < 119, "a2 slides out: {r:?}");
    // Behind it, c shows faintly (a sliding page is drawn whole).
    let p = st.buf.px(119, 30);
    assert!(p[1] > 0 && p[1] < 200 && p[2] < 60, "c fades in: {p:?}");
    k = st.settle(k);

    // Under reduced_motion the swap snaps.
    st.r.set_reduced_motion(true);
    let d2 = NodeId::new(503, 0);
    swap(&mut st, c, d2, "#ff0000", 40.0, false);
    st.paint(frame(k));
    assert_eq!(spans(&st, 30), (Some((0, 119)), None));
    assert!(!st.r.wants_frame(S));
}

/// (m4-audit) `transition: none` names no mask, so a `pages` with it
/// slides as one without: the old page is not held back for a mask that
/// never runs, which swapped the pages with no transition at all.
#[test]
fn a_pages_with_transition_none_still_slides() {
    let mut pages = None;
    let mut a = None;
    let mut st = Stage::new(200, 60, |b, root| {
        let p = b.node(
            NodeKind::Pages,
            Some(root),
            vec![
                (Prop::Width, num(120.0)),
                (Prop::Height, num(60.0)),
                (Prop::RowFirst, num(10.0)),
                (Prop::Transition, PropValue::Keyword("none".into())),
            ],
        );
        pages = Some(p);
        a = Some(b.node(NodeKind::Page, Some(p), vec![(Prop::Bg, color("#ff0000"))]));
    });
    let (pages, a) = (pages.unwrap(), a.unwrap());
    let b = NodeId::new(500, 0);
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: a,
        window: false,
    });
    d.create(b, NodeKind::Page, Some(pages), 0)
        .set(b, Prop::Bg, color("#00ff00"));
    d.set(pages, Prop::RowFirst, num(20.0));
    st.apply(d);
    st.paint(frame(1));
    let (r, g) = spans(&st, 30);
    let gx = g.map_or(120, |(x0, _)| x0);
    assert!(
        gx > 40 && gx < 120,
        "the new page on its way in: {r:?} {g:?}"
    );
    assert!(
        r.is_some_and(|(x0, x1)| x0 == 0 && x1 < gx),
        "the old page leaving left: {r:?} {g:?}"
    );
    st.settle(2);
    assert_eq!(spans(&st, 30), (None, Some((0, 119))));
    assert!(st.r.tree().get(a).is_none(), "the old page is gone");
}

/// A node created on a shown surface starts at its props' values: the
/// diff that creates it sets them, but they replace nothing on screen,
/// so nothing springs from the props' defaults (a red box does not fade
/// in from transparent, nor a moved one slide in from 0). Only its enter
/// pose moves, and only the props the pose names.
#[test]
fn a_created_node_starts_at_its_values() {
    let mut ids = Vec::new();
    let mut st = Stage::new(80, 40, |b, root| {
        ids.push(b.node(NodeKind::Row, Some(root), vec![]));
    });
    let row = ids[0];
    let (plain, posed) = (NodeId::new(100, 0), NodeId::new(101, 0));
    let mut d = SceneDiff::new();
    for id in [plain, posed] {
        d.create(id, NodeKind::Box, Some(row), u32::MAX)
            .set(id, Prop::Bg, color("#ff0000"))
            .set(id, Prop::Size, num(20.0));
    }
    d.set(plain, Prop::X, num(4.0))
        .set(posed, Prop::Enter, pose(vec![(Prop::Scale, num(0.5))]));
    st.apply(d);
    st.paint(frame(1));
    // No fade or slide: red and opaque at once, moved by its `x`.
    assert_eq!(st.rect(plain).x, 0.0);
    assert_eq!(st.red_from(10), Some(4), "drawn at its x at once");
    let px = st.buf.px(10, 10);
    assert!(
        px[2] > 250 && px[3] == 255,
        "red at once, not from transparent: {px:?}"
    );
    // The posed box scales in, at full colour all the while.
    let c = st.rect(posed);
    let mid = st
        .buf
        .px((c.x + c.w / 2.0) as u32, (c.y + c.h / 2.0) as u32);
    assert!(mid[2] > 250, "its colour does not spring: {mid:?}");
    assert!(st.r.wants_frame(S), "the enter pose plays");
    st.settle(2);
}

/// (eval-rules-4) Rows leaving together on `exit { height: 0 }` fold
/// their gaps as they collapse, whether a row stays before them (the
/// last two of a stack) or none stays (a list cleared at once): the
/// column holding them never steps when the collapsed rows unmount.
#[test]
fn rows_leaving_together_fold_their_gaps_before_they_unmount() {
    for gone in [vec![1usize, 2], vec![0, 1, 2]] {
        let mut col = None;
        let mut rows = Vec::new();
        let mut st = Stage::new(200, 200, |b, root| {
            // In a column, sized by what it holds.
            let outer = b.node(NodeKind::Col, Some(root), vec![]);
            let c = b.node(NodeKind::Col, Some(outer), vec![(Prop::Gap, num(8.0))]);
            for _ in 0..3 {
                rows.push(b.node(
                    NodeKind::Box,
                    Some(c),
                    vec![
                        (Prop::Height, num(30.0)),
                        (Prop::Width, num(100.0)),
                        (Prop::Bg, color("#ff0000")),
                        (Prop::Exit, pose(vec![(Prop::Height, num(0.0))])),
                    ],
                ));
            }
            col = Some(c);
        });
        let col = col.unwrap();
        assert_eq!(st.rect(col).h, 3.0 * 30.0 + 2.0 * 8.0);
        let mut d = SceneDiff::new();
        for i in &gone {
            d.push(SceneOp::Remove {
                id: rows[*i],
                window: false,
            });
        }
        st.apply(d);
        // The column's height each frame, and whether the leaving rows
        // are still mounted.
        let mut seen = Vec::new();
        let mut k = 1;
        while st.r.wants_frame(S) {
            st.paint(frame(k));
            let mounted = st.r.boxes(S).unwrap().rects.contains_key(&rows[gone[0]]);
            seen.push((st.rect(col).h, mounted));
            k += 1;
            assert!(k < 400, "never settled");
        }
        st.paint(frame(k));
        seen.push((st.rect(col).h, false));
        let left = 3 - gone.len();
        let rest = if left == 0 {
            0.0
        } else {
            left as f32 * 30.0 + (left - 1) as f32 * 8.0
        };
        assert_eq!(seen.last().unwrap().0, rest, "{gone:?}: {seen:?}");
        let at = seen.iter().position(|(_, m)| !m).unwrap();
        assert!(at > 0, "{gone:?}: {seen:?}");
        assert!(
            (seen[at].0 - seen[at - 1].0).abs() <= 0.5,
            "{gone:?}: unmounting steps the column: {seen:?}"
        );
    }
}

/// Where the reddest pixels of `buf` are: the top row and left column
/// with a pixel whose red is over 200 and green and blue under 80.
fn red_corner(buf: &Buffer) -> Option<(u32, u32)> {
    let red = |x: u32, y: u32| {
        let p = buf.px(x, y);
        p[2] > 200 && p[1] < 80 && p[0] < 80
    };
    let top = (0..buf.size.h).find(|y| (0..buf.size.w).any(|x| red(x, *y)))?;
    let left = (0..buf.size.w).find(|x| (0..buf.size.h).any(|y| red(*x, y)))?;
    Some((left, top))
}

/// (launcher-reopen) A panel's content is kept while it is closed, not
/// unmounted: opened again, what it holds comes in as it first came,
/// each node with an `enter` pose playing it. Before, the panel faded
/// in and its card showed at once at full opacity.
#[test]
fn a_panel_opened_again_plays_its_contents_enter_poses() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Open, PropValue::Bool(true)),
        ],
    );
    let card = red_box(
        &mut b,
        root,
        vec![(Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]))],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    let alpha = |h: &Host| {
        let r = h.r.boxes(h.surface.as_ref().unwrap().0).unwrap().rects[&card];
        h.px((r.x + 10.0) as u32, (r.y + 10.0) as u32).unwrap()[2]
    };
    assert_eq!(alpha(&h), 255);
    for _ in 0..2 {
        let mut d = SceneDiff::new();
        d.set(root, Prop::Open, PropValue::Bool(false));
        h.apply(d);
        h.run();
        assert!(h.surface.is_none(), "closed");
        let mut d = SceneDiff::new();
        d.set(root, Prop::Open, PropValue::Bool(true));
        h.apply(d);
        let mut seen = Vec::new();
        for _ in 0..4 {
            assert!(h.frame(), "it plays");
            seen.push(alpha(&h));
        }
        assert!(seen[0] < 200, "enters from its pose: {seen:?}");
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]) && seen[3] > seen[0],
            "fades in frame by frame: {seen:?}"
        );
        h.run();
        assert_eq!(alpha(&h), 255, "at rest");
    }
}

/// (launcher-flip-pass) A change that widens a clipped card on its own
/// springs and mounts a chip in its header is laid out for the change
/// with the card still at its old width, where the chip leaves the red
/// readout too narrow for its text; the host's update and the frame lay
/// it out again (the card's first frame of growth) before anything is
/// drawn. The readout's place in the row on y never changes, and on no
/// frame is it drawn off it. Before, the later passes glided nothing, or
/// started what they added a frame late: the glide measured from the
/// squeezed box drew the readout up to 6 px above its level, springing
/// back.
#[test]
fn a_box_squeezed_in_a_pass_no_frame_drew_stays_on_its_level() {
    let (mut card, mut col, mut head, mut time) = (None, None, None, None);
    let mut st = Stage::new(700, 260, |b, root| {
        b.diff.set(root, Prop::Align, kw("start"));
        let row = b.node(
            NodeKind::Row,
            Some(root),
            vec![(Prop::Width, num(700.0)), (Prop::Justify, kw("center"))],
        );
        let c = b.node(
            NodeKind::Stack,
            Some(row),
            vec![
                (Prop::Clip, PropValue::Bool(true)),
                (Prop::Align, kw("start")),
                (Prop::Width, num(320.0)),
                (Prop::Height, num(160.0)),
                (Prop::Bg, color("#24273a")),
            ],
        );
        card = Some(c);
        let k = b.node(
            NodeKind::Col,
            Some(c),
            vec![(Prop::Width, num(320.0)), (Prop::Height, num(160.0))],
        );
        col = Some(k);
        let hd = b.node(
            NodeKind::Row,
            Some(k),
            vec![
                (Prop::Height, num(56.0)),
                (Prop::Pad, PropValue::List(vec![num(0.0), num(16.0)])),
                (Prop::Gap, num(10.0)),
                (Prop::Align, kw("center")),
                (Prop::Shrink, num(0.0)),
            ],
        );
        head = Some(hd);
        b.node(
            NodeKind::Box,
            Some(hd),
            vec![(Prop::Size, num(28.0)), (Prop::Bg, color("#8aadf4"))],
        );
        b.node(
            NodeKind::Box,
            Some(hd),
            vec![
                (Prop::Width, num(110.0)),
                (Prop::Height, num(20.0)),
                (Prop::Shrink, num(0.0)),
                (Prop::Bg, color("#363a4f")),
            ],
        );
        let tm = b.node(
            NodeKind::Box,
            Some(hd),
            vec![(Prop::Pad, num(4.0)), (Prop::Bg, color("#ff0000"))],
        );
        time = Some(tm);
        b.node(
            NodeKind::Text,
            Some(tm),
            vec![(Prop::Text, text("3:07")), (Prop::Color, color("#ff0000"))],
        );
        let dots = b.node(
            NodeKind::Row,
            Some(hd),
            vec![
                (Prop::Gap, num(6.0)),
                (Prop::Height, num(16.0)),
                (Prop::Align, kw("center")),
                (Prop::Shrink, num(0.0)),
            ],
        );
        for _ in 0..3 {
            b.node(
                NodeKind::Box,
                Some(dots),
                vec![(Prop::Size, num(6.0)), (Prop::Bg, color("#6e738d"))],
            );
        }
    });
    let (card, col, head, time) = (card.unwrap(), col.unwrap(), head.unwrap(), time.unwrap());
    let level = red_corner(&st.buf).expect("the readout is drawn").1;
    let rest = st.rect(time);
    let chip = NodeId::new(90, 0);
    let label = NodeId::new(91, 0);
    let mut d = SceneDiff::new();
    d.create(chip, NodeKind::Box, Some(head), 2)
        .set(chip, Prop::Pad, PropValue::List(vec![num(2.0), num(8.0)]))
        .set(chip, Prop::Bg, color("#a6da95"))
        .set(
            chip,
            Prop::Enter,
            pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.6))]),
        )
        .create(label, NodeKind::Text, Some(chip), 0)
        .set(label, Prop::Text, text("Now playing"))
        .set(label, Prop::Color, color("#181926"));
    // `width: w ~ spring(520, 0.68); height: h ~ spring(900, 0.82)`.
    for n in [card, col] {
        for (prop, v, stiffness, damping) in [
            (Prop::Width, 560.0, 520.0, 0.68),
            (Prop::Height, 240.0, 900.0, 0.82),
        ] {
            d.push(SceneOp::SetProp {
                id: n,
                prop,
                value: num(v),
                transition: Transition::Spring { stiffness, damping },
            });
        }
    }
    st.apply(d);
    // As the host loop turns: the layouts its text worker delivers wake
    // it, and it updates (`host_loop::woke`).
    let turn = |st: &mut Stage| st.r.update();
    turn(&mut st);
    let mut seen = Vec::new();
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        turn(&mut st);
        k += 1;
        assert!(k < 400, "never settled");
        seen.push((red_corner(&st.buf).map(|c| c.1), st.rect(time).y));
    }
    assert!(
        seen.iter().all(|(_, y)| (y - rest.y).abs() < 0.5),
        "laid out on its level on every frame drawn: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|(top, ..)| top.is_some_and(|t| t.abs_diff(level) <= 1)),
        "drawn on its level ({level}) on every frame: {seen:?}"
    );
}

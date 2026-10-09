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
    assert!(xs.windows(2).all(|w| w[0] <= w[1]), "{xs:?}");
    assert!(xs[0] > 2, "the first frame already moves: {xs:?}");
    assert_eq!(*xs.last().unwrap(), 112, "{xs:?}");
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
    let mut xs = Vec::new();
    for k in 1..=5 {
        st.paint(frame(k));
        xs.push(st.red_from(10).unwrap() as i32);
    }
    // Moving right fast.
    assert!(xs[4] - xs[3] >= 5, "{xs:?}");
    let mut d = SceneDiff::new();
    d.set(id, Prop::X, num(20.0));
    st.apply(d);
    st.paint(frame(6));
    let after = st.red_from(10).unwrap() as i32;
    assert!(
        after > xs[4],
        "kept going right after the retarget: {xs:?} then {after}"
    );
    // No jolt: the step is no bigger than the last one before it.
    assert!(after - xs[4] <= xs[4] - xs[3] + 1);
    st.settle(7);
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
    let first = st.red_top(10).unwrap();
    assert!(first < 12, "still near its old slot: {first}");
    let mut tops = vec![first];
    for k in 2..=8 {
        st.paint(frame(k));
        tops.push(st.red_top(10).unwrap());
    }
    assert!(tops.windows(2).all(|w| w[0] <= w[1]), "{tops:?}");
    st.settle(9);
    assert_eq!(st.red_top(10), Some(48));
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
    // (Faint at first: its opacity springs from 0 too.)
    assert!(xs.windows(2).all(|w| w[1] <= w[0]), "glides in: {xs:?}");
    assert!(xs[3..].windows(2).all(|w| w[1] < w[0]), "glides in: {xs:?}");
    assert!(xs[5] > 0, "still moving: {xs:?}");
    st.settle(end + 6);
    assert_eq!(white(&st), Some(0));
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
    let first = green(&st);
    assert!(first < 12, "faint and small at first: {first}");
    st.paint(frame(end + 3));
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
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, color("#1e1e2e"))]);
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
    assert!(
        tops.windows(2).all(|p| p[1] <= p[0] && p[0] - p[1] <= 12),
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

/// Frame time while the design's animated surfaces move (release builds
/// only: `cargo test --release -p strand-render --test motion --
/// --ignored`): the launcher's `enter { opacity: 0; scale: 0.96 }` over a
/// 640×480 panel of rows, and a toast leaving a stack of three (slide,
/// fade and collapse while the next slides up). Every frame must fit a
/// 60 Hz refresh (16.7 ms), and most of them half of it.
#[test]
#[ignore = "release-mode frame-time bench"]
fn animated_frames_fit_the_refresh_budget() {
    fn check(name: &str, mut times: Vec<Duration>) {
        assert!(times.len() >= 5, "{name}: only {} frames", times.len());
        times.sort();
        let median = times[times.len() / 2];
        let worst = *times.last().unwrap();
        eprintln!(
            "{name}: {} frames, median {median:?}, worst {worst:?}",
            times.len()
        );
        assert!(median < Duration::from_micros(8_333), "{name}: {median:?}");
        assert!(worst < Duration::from_micros(16_667), "{name}: {worst:?}");
    }
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
/// growth.
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
    assert!(ys[0] >= -16.0, "stays where it was: {ys:?}");
    assert_eq!(*ys.last().unwrap(), -27.0, "{ys:?}");
    assert!(
        ys.windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] < 6.0),
        "glides there: {ys:?}"
    );
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
    let mut k = 1;
    let mut last_green = 120;
    let mut frames = 0;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        k += 1;
        let (r, g) = spans(&st, 30);
        let gx = g.map_or(120, |(x0, x1)| {
            assert_eq!(x1, 119, "the new page's right edge is clipped");
            x0
        });
        if let Some((x0, x1)) = r {
            assert_eq!(x0, 0, "the old page leaves to the left");
            assert!(x1 < gx, "{r:?} {g:?}");
        }
        for x in 120..200 {
            assert_eq!(st.buf.px(x, 30)[1], st.buf.px(199, 30)[1], "clipped at {x}");
        }
        assert!(gx <= last_green, "moves left: {gx} after {last_green}");
        if frames == 0 {
            assert!(gx > 40 && gx < 120, "on its way in: {gx}");
        }
        last_green = gx;
        frames += 1;
        assert!(k < 200, "never settled");
    }
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

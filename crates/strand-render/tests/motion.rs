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
        let mut r = renderer();
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
    let mut dot = None;
    let mut st = Stage::new(240, 40, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Gap, num(4.0))]);
        let cell = b.node(
            NodeKind::Row,
            Some(row),
            vec![(Prop::Width, num(60.0)), (Prop::Height, num(30.0))],
        );
        dot = Some(b.node(
            NodeKind::Box,
            Some(cell),
            vec![(Prop::Size, num(8.0)), (Prop::Bg, color("#ff0000"))],
        ));
        b.node(NodeKind::Box, Some(cell), vec![(Prop::Size, num(8.0))]);
        for _ in 0..6 {
            let c = b.node(NodeKind::Col, Some(row), vec![]);
            b.node(NodeKind::Text, Some(c), vec![(Prop::Text, text("label"))]);
        }
    });
    let dot = dot.unwrap();
    let full = st.r.boxes(S).unwrap().rects.len();
    let mut d = SceneDiff::new();
    d.set(dot, Prop::Width, num(24.0));
    st.apply(d);
    let mut widths = Vec::new();
    let mut partial = Vec::new();
    for k in 1..=12 {
        st.paint(frame(k));
        widths.push(st.rect(dot).w);
        partial.push(st.r.last_layout_nodes());
    }
    assert!(widths[0] > 8.0 && widths[0] < 24.0, "{widths:?}");
    assert!(widths[5] > widths[0], "{widths:?}");
    // After the first frame (which lays the whole surface out once for
    // the change), each frame lays out the cell's subtree only (the cell
    // and its two children, twice: at rest and in flight).
    assert!(
        partial[2..].iter().all(|n| *n <= 6 && *n < full),
        "{partial:?} of {full}"
    );
    st.settle(13);
    assert_eq!(st.rect(dot).w, 24.0);
    // The sibling after the dot moved with it.
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
    d.push(SceneOp::Remove { id: ids[1] });
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
    st.settle(end + 1);
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
    st.paint(frame(end + 2));
    let mid = st.buf.px(30, 15);
    assert!(mid[3] < 250 && mid[3] > 0, "fading: {mid:?}");
    let end = st.settle(end + 3);
    st.r.update();
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
    st.paint(frame(end + 1));
    assert!(st.buf.px(30, 15)[3] < 128, "enters from transparent");
    st.settle(end + 2);
    assert_eq!(st.buf.px(30, 15)[3], 255);
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
    // reduced_motion: everything snaps.
    st.r.set_reduced_motion(true);
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

//! (M4) Compositor-animated poses offline (design.md, "Compositor-
//! animated poses"): with `Renderer::set_compositor_poses(true)` a
//! surface root's `enter`/`exit` opacity, scale and offset are reported
//! through `Painter::surface_pose` frame by frame while the content
//! paints once, at rest, and later frames return no damage (the surface
//! manager commits them with no buffer). What a placement cannot apply
//! (a centred panel's scale, a popup's offset) still repaints, and with
//! delegation off everything repaints as before.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const T0: Duration = Duration::from_secs(1);

/// The presentation time of frame `k` after `T0` at 60 Hz.
fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// One frame as the surface manager sees it: the damage painted, the
/// pose reported, and a pixel near the panel's right edge (inside it at
/// rest and through a slide from the right scaled from 0.8).
#[derive(Debug)]
struct Seen {
    damage: u64,
    pose: Option<SurfacePose>,
    centre: [u8; 4],
}

/// The host side of the loop for one surface node (as `motion.rs`'s
/// host): a spec that opens attaches and configures a surface, one that
/// closes detaches it; frames are painted while it wants them.
struct Host {
    r: Renderer,
    root: NodeId,
    surface: Option<(SurfaceId, Buffer, u8)>,
    k: u32,
}

impl Host {
    fn new(poses: bool, diff: SceneDiff, root: NodeId) -> Host {
        let mut r = renderer();
        r.set_compositor_poses(poses);
        let mut h = Host {
            r,
            root,
            surface: None,
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
                        let o = s.overhang;
                        let size = Size::new(
                            (s.width.unwrap_or(1.0) + o.left + o.right) as u32,
                            (s.height.unwrap_or(1.0) + o.top + o.bottom) as u32,
                        );
                        if self.surface.is_none() {
                            self.r.attach_surface(SurfaceId(7), node);
                            self.surface = Some((SurfaceId(7), Buffer::new(1, 1, Scale::ONE), 0));
                        }
                        let (id, buf, age) = self.surface.as_mut().unwrap();
                        if buf.size != size {
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

    /// Paints frames until the surface wants none (or is gone).
    fn run(&mut self) -> Vec<Seen> {
        let mut out = Vec::new();
        loop {
            let Some((id, buf, age)) = self.surface.as_mut() else {
                return out;
            };
            let id = *id;
            if !self.r.wants_frame(id) {
                return out;
            }
            let damage = buf.paint_at(&mut self.r, id, *age, frame(self.k));
            if !damage.is_empty() {
                *age = 1;
            }
            out.push(Seen {
                damage: damage.area(),
                pose: self.r.surface_pose(id),
                centre: buf.px(70, 20),
            });
            self.k += 1;
            if self.r.has_surface_changes() {
                self.r.update();
                self.sync();
            }
            assert!(out.len() < 400, "never settled");
        }
    }

    fn open(&mut self, open: bool) {
        let mut d = SceneDiff::new();
        d.set(self.root, Prop::Open, PropValue::Bool(open));
        self.apply(d);
    }
}

/// A closed 80 × 40 red panel at `anchor` with `enter`.
fn panel(anchor: &str, enter: Vec<(Prop, PropValue)>) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Anchor, kw(anchor)),
            (Prop::Open, PropValue::Bool(false)),
            (Prop::Enter, PropValue::Pose(enter)),
        ],
    );
    (b.diff, root)
}

fn slide_in() -> Vec<(Prop, PropValue)> {
    vec![
        (Prop::Opacity, num(0.0)),
        (Prop::Scale, num(0.8)),
        (Prop::X, num(40.0)),
    ]
}

/// The design's case: a corner panel slides, scales and fades in, and
/// out again, by the compositor alone. The first frame paints the panel
/// at rest (opaque red, at full size) and later frames paint nothing
/// while the pose rises; closing plays the exit the same way, and the
/// surface goes once it settles.
#[test]
fn a_corner_panel_enters_and_exits_by_the_compositor() {
    let (diff, root) = panel("top_right", slide_in());
    let mut h = Host::new(true, diff, root);
    h.open(true);
    let seen = h.run();
    assert!(seen.len() >= 5, "enter plays over several frames: {seen:?}");
    // Content at rest from the first frame: opaque red, no fade or scale
    // in the pixels.
    assert!(seen[0].damage > 0, "the first frame is painted");
    assert_eq!(seen[0].centre, [0, 0, 255, 255], "{seen:?}");
    let first = seen[0].pose.expect("a pose from the first frame");
    assert!(first.opacity < 0.5, "{first:?}");
    assert!(first.scale < 0.95, "{first:?}");
    // Offset: the 40 px slide plus the corner's move that keeps the
    // centre (40, 20) in place, both shrinking towards rest.
    assert!(first.offset.x > 20.0, "{first:?}");
    for w in seen.windows(2).take(4) {
        let (a, b) = (w[0].pose.unwrap(), w[1].pose.unwrap());
        assert!(b.opacity > a.opacity, "fades in frame by frame: {seen:?}");
        assert!(
            b.offset.x < a.offset.x,
            "slides in frame by frame: {seen:?}"
        );
    }
    assert!(
        seen[1..].iter().all(|s| s.damage == 0),
        "nothing is repainted while the compositor animates: {seen:?}"
    );
    assert_eq!(
        seen.last().unwrap().pose,
        Some(SurfacePose::IDENTITY),
        "settles at rest"
    );
    // The exit mirrors the enter, delegated too, then the surface goes.
    h.open(false);
    let seen = h.run();
    assert!(h.surface.is_none(), "closed once the exit settled");
    assert!(seen.len() >= 4, "exit plays over several frames: {seen:?}");
    assert!(seen.iter().all(|s| s.damage == 0), "{seen:?}");
    let opacities: Vec<f32> = seen.iter().map(|s| s.pose.unwrap().opacity).collect();
    assert!(
        opacities.windows(2).take(4).all(|w| w[1] < w[0]),
        "{opacities:?}"
    );
}

/// The frame a delegated enter paints is the panel at rest, pixel for
/// pixel the frame it settles on without delegation (PNG
/// `surface_pose_at_rest.png`).
#[test]
fn delegated_content_is_painted_at_rest() {
    let (diff, root) = panel("top_right", slide_in());
    let mut h = Host::new(true, diff, root);
    h.open(true);
    let _ = h.run();
    let (_, delegated, _) = h.surface.as_ref().unwrap();
    assert_matches_ref("surface_pose_at_rest", delegated, 0);
    let (diff, root) = panel("top_right", slide_in());
    let mut plain = Host::new(false, diff, root);
    plain.open(true);
    let _ = plain.run();
    let (_, painted, _) = plain.surface.as_ref().unwrap();
    assert_eq!(delegated.to_rgba(), painted.to_rgba());
}

/// Without the protocols nothing is delegated: every frame of the enter
/// repaints, and the pixels fade in.
#[test]
fn without_the_protocols_poses_repaint() {
    let (diff, root) = panel("top_right", slide_in());
    let mut h = Host::new(false, diff, root);
    h.open(true);
    let seen = h.run();
    assert!(seen.len() >= 5, "{seen:?}");
    assert!(seen.iter().all(|s| s.pose.is_none()), "{seen:?}");
    assert!(seen.iter().take(10).all(|s| s.damage > 0), "{seen:?}");
    let alphas: Vec<u8> = seen.iter().map(|s| s.centre[3]).collect();
    assert!(
        alphas[1..6].windows(2).all(|w| w[1] > w[0]) && alphas[3] < 255,
        "fades in its pixels: {alphas:?}"
    );
}

/// A centred panel delegates only its opacity: a margin cannot move it
/// and the compositor would scale it from the wrong corner, so its scale
/// repaints (every frame has damage) while the fade is the compositor's.
#[test]
fn a_centred_panel_delegates_only_its_fade() {
    let (diff, root) = panel("center", slide_in());
    let mut h = Host::new(true, diff, root);
    h.open(true);
    let seen = h.run();
    assert!(seen.len() >= 5, "{seen:?}");
    let first = seen[0].pose.unwrap();
    assert!(first.opacity < 0.5, "{first:?}");
    assert_eq!(
        (first.scale, first.offset.x, first.offset.y),
        (1.0, 0.0, 0.0)
    );
    // Scale and x paint: frames repaint, and the content is opaque (the
    // fade is not in the pixels).
    assert!(seen.iter().take(4).all(|s| s.damage > 0), "{seen:?}");
    assert_eq!(seen[0].centre[3], 255, "{seen:?}");
}

/// Turning delegation off mid-way repaints the pose into the content:
/// the next frame is a full one, and no pose is reported.
#[test]
fn turning_delegation_off_repaints() {
    let (diff, root) = panel("top_right", vec![(Prop::Opacity, num(0.0))]);
    let mut h = Host::new(true, diff, root);
    h.open(true);
    let (id, buf, _) = h.surface.as_mut().unwrap();
    let id = *id;
    assert!(!buf.paint_at(&mut h.r, id, 0, frame(1)).is_empty());
    assert!(h.r.surface_pose(id).is_some_and(|p| p.opacity < 0.5));
    assert!(buf.paint_at(&mut h.r, id, 1, frame(2)).is_empty());
    h.r.set_compositor_poses(false);
    assert!(h.r.wants_frame(id));
    let d = buf.paint_at(&mut h.r, id, 1, frame(3));
    assert!(!d.is_empty(), "the fade is painted again");
    assert_eq!(h.r.surface_pose(id), None);
    assert!(buf.px(40, 20)[3] < 255, "{:?}", buf.px(40, 20));
}

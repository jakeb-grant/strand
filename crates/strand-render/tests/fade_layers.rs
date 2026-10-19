//! (m6-owner-fixes, owner-testing.md bug 17) Fade layers: an opacity
//! group whose content only moves (an `enter` or `exit` pose of
//! `opacity` and `y`) is rastered once into an offscreen layer and
//! composited at its opacity on each frame (when its content costs the
//! raster more than a layer: here a shadow and a gradient). Every frame
//! must match the frame that rasters the group's content again, under a
//! rounded clip, with shadows, borders, gradients, text, a nested
//! translucent box and a blur region, at scales 1 and 1.5.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test fade_layers`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

/// The presentation time of frame `k` after `T0` at 60 Hz.
fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

/// Pixels of a layered frame may differ from the re-rastered one by
/// this much per channel (the layer's pixels are stored as 8-bit before
/// they are composited at the group's opacity): the PNG tests'
/// tolerance.
const TOLERANCE: u8 = 2;

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn shadow(y: f32, blur: f32, alpha: f32) -> Shadow {
    Shadow {
        x: 0.0,
        y,
        blur,
        spread: 0.0,
        color: Color::BLACK.with_alpha(alpha),
    }
}

fn linear(a: &str, b: &str) -> PropValue {
    PropValue::Paint(Paint::Linear {
        angle: 135.0,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: hex(a),
            },
            GradientStop {
                offset: 1.0,
                color: hex(b),
            },
        ],
    })
}

fn border(w: f32, c: &str) -> PropValue {
    PropValue::Border(Border {
        width: w,
        paint: Paint::Solid(hex(c)),
    })
}

const W: f32 = 360.0;
const H: f32 = 240.0;

/// The panel and its card: a rounded, clipping card with a gradient and
/// a shadow, which the fading panel enters.
fn stage(b: &mut Builder) -> NodeId {
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Bg, color("#1e1e2e")), (Prop::Pad, num(12.0))],
    );
    b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Width, num(W - 24.0)),
            (Prop::Height, num(H - 24.0)),
            (Prop::Radius, num(18.0)),
            (Prop::Clip, PropValue::Bool(true)),
            (Prop::Pad, num(10.0)),
            (Prop::Bg, linear("#181825", "#313244")),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![shadow(4.0, 16.0, 0.4)]),
            ),
        ],
    );
    root
}

/// The panel that fades in with `pose` (the owner's dashboard panel,
/// reduced): its content created under node `card` from id 100 on.
fn panel(card: NodeId, pose: Vec<(Prop, PropValue)>, label: &str) -> SceneDiff {
    let mut d = SceneDiff::new();
    let mut next = 100u32;
    let mut node = |d: &mut SceneDiff, kind, parent, props: Vec<(Prop, PropValue)>| {
        let id = NodeId::new(next, 0);
        next += 1;
        d.create(id, kind, Some(parent), u32::MAX);
        for (p, v) in props {
            d.set(id, p, v);
        }
        id
    };
    let p = node(
        &mut d,
        NodeKind::Col,
        card,
        vec![
            (Prop::Width, num(300.0)),
            (Prop::Gap, num(8.0)),
            (Prop::Pad, num(10.0)),
            (Prop::Radius, num(12.0)),
            (Prop::Bg, color("#313244")),
            (Prop::Border, border(1.0, "#585b70")),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![shadow(4.0, 12.0, 0.3)]),
            ),
            (Prop::Blur, num(12.0)),
            (Prop::Enter, PropValue::Pose(pose.clone())),
            (Prop::Exit, PropValue::Pose(pose)),
        ],
    );
    for (i, t) in [label, "Tuesday 14 October", "3 events, 2 reminders"]
        .into_iter()
        .enumerate()
    {
        node(
            &mut d,
            NodeKind::Text,
            p,
            vec![
                (Prop::Text, text(t)),
                (Prop::Font, PropValue::Font(font(13.0 + i as f32))),
                (Prop::Color, color("#cdd6f4")),
            ],
        );
    }
    let row = node(&mut d, NodeKind::Row, p, vec![(Prop::Gap, num(6.0))]);
    for c in ["#89b4fa", "#f38ba8", "#a6e3a1", "#f9e2af", "#cba6f7"] {
        node(
            &mut d,
            NodeKind::Box,
            row,
            vec![
                (Prop::Width, num(27.0)),
                (Prop::Height, num(21.0)),
                (Prop::Radius, num(6.0)),
                (Prop::Bg, color(c)),
            ],
        );
    }
    // A gradient pill, and a translucent box of its own.
    node(
        &mut d,
        NodeKind::Box,
        p,
        vec![
            (Prop::Height, num(14.0)),
            (Prop::Radius, kw("full")),
            (Prop::Bg, linear("#89b4fa", "#cba6f7")),
        ],
    );
    let inner = node(
        &mut d,
        NodeKind::Row,
        p,
        vec![
            (Prop::Height, num(30.0)),
            (Prop::Radius, num(9.0)),
            (Prop::Clip, PropValue::Bool(true)),
            (Prop::Opacity, num(0.6)),
            (Prop::Bg, color("#45475a")),
        ],
    );
    // Wider than its clip.
    node(
        &mut d,
        NodeKind::Box,
        inner,
        vec![
            (Prop::Width, num(400.0)),
            (Prop::Height, num(40.0)),
            (Prop::Radius, num(20.0)),
            (Prop::Bg, color("#fab387")),
            (Prop::Border, border(2.0, "#11111b")),
        ],
    );
    d
}

/// Two renderers fed the same frames, one with fade layers and one
/// without.
struct Pair {
    on: Renderer,
    off: Renderer,
    a: Buffer,
    b: Buffer,
    card: NodeId,
}

impl Pair {
    fn new(scale: Scale) -> Pair {
        let mut b = Builder::default();
        let root = stage(&mut b);
        let card = NodeId::new(1, 0);
        let mut on = renderer();
        let mut off = renderer();
        off.set_fade_layers(false);
        let size = scale.physical_size(LogicalSize::new(W, H));
        let (mut a, mut bb) = (
            Buffer::new(size.w, size.h, scale),
            Buffer::new(size.w, size.h, scale),
        );
        for (r, buf) in [(&mut on, &mut a), (&mut off, &mut bb)] {
            assert!(r.apply(b.diff.clone()).is_empty());
            r.attach_surface(S, root);
            buf.paint_at(r, S, 0, T0);
        }
        Pair {
            on,
            off,
            a,
            b: bb,
            card,
        }
    }

    fn apply(&mut self, d: SceneDiff) {
        assert!(self.on.apply(d.clone()).is_empty());
        assert!(self.off.apply(d).is_empty());
    }

    /// Paints frame `k` on both and returns the largest difference of a
    /// channel between them.
    fn paint(&mut self, k: u32) -> u8 {
        let da = self.a.paint_at(&mut self.on, S, 1, frame(k));
        let db = self.b.paint_at(&mut self.off, S, 1, frame(k));
        assert_eq!(da, db, "frame {k}: both damage the same pixels");
        self.a
            .pixels
            .iter()
            .zip(&self.b.pixels)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0)
    }

    /// Paints from frame `k` until both rest; returns the frame after the
    /// last and the largest difference seen.
    fn settle(&mut self, mut k: u32, worst: &mut u8) -> u32 {
        while self.on.wants_frame(S) || self.off.wants_frame(S) {
            *worst = (*worst).max(self.paint(k));
            assert!(*worst <= TOLERANCE, "frame {k}: off by {worst}");
            k += 1;
            assert!(k < 400, "never settled");
        }
        k
    }
}

/// The panel entering and leaving with `opacity: 0; y: -12`, drawn from
/// a fade layer on the frames between, matches the frames that raster it
/// whole, at scales 1 and 1.5; its layers are built once per half-pixel
/// phase and drawn on the frames after; and frame 4 of the entry matches
/// its reference with and without layers.
#[test]
fn a_fading_panel_drawn_from_its_layer_matches_rastering_it() {
    for (scale, name) in [
        (Scale::ONE, "fade_layer_enter_1x"),
        (Scale::new(180).unwrap(), "fade_layer_enter_1_5x"),
    ] {
        let mut p = Pair::new(scale);
        let pose = vec![(Prop::Opacity, num(0.0)), (Prop::Y, num(-12.0))];
        p.apply(panel(p.card, pose, "Today"));
        let mut worst = 0;
        for k in 1..=4 {
            worst = worst.max(p.paint(k));
        }
        assert!(worst <= TOLERANCE, "{scale:?}: entering, off by {worst}");
        assert_matches_ref(name, &p.a, TOLERANCE);
        assert_matches_ref(name, &p.b, TOLERANCE);
        let end = p.settle(5, &mut worst);
        let (built, drawn) = p.on.fade_layers();
        assert!(built >= 1, "{scale:?}: the panel's layer is built");
        assert!(
            built <= 2,
            "{scale:?}: once per half-pixel phase, not per frame: {built} built"
        );
        assert!(
            drawn >= built + 4,
            "{scale:?}: the layers are drawn on the frames after: {built} built, {drawn} drawn"
        );
        assert_eq!(p.off.fade_layers(), (0, 0), "off draws none");
        // At rest the group is gone: both draw the same pixels.
        assert_eq!(p.a.pixels, p.b.pixels, "{scale:?}: at rest");

        // Its exit: the same layers again (cached).
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: NodeId::new(100, 0),
            window: false,
        });
        p.apply(d);
        p.settle(end, &mut worst);
        let (again, drawn_after) = p.on.fade_layers();
        assert!(drawn_after > drawn + 4, "{scale:?}: the exit draws layers");
        assert!(
            again <= built + 2,
            "{scale:?}: the exit builds at most a layer per phase: {again} built"
        );
        assert_eq!(p.a.pixels, p.b.pixels, "{scale:?}: after the exit");
        eprintln!("{scale:?}: worst {worst}, {again} built, {drawn_after} drawn");
    }
}

/// A fading group whose content changes on every frame (a label
/// rewritten as it fades) is drawn as before once two of its layers went
/// unused, and still matches.
#[test]
fn a_fade_whose_content_changes_every_frame_stops_building_layers() {
    let mut p = Pair::new(Scale::ONE);
    let pose = vec![(Prop::Opacity, num(0.0))];
    p.apply(panel(p.card, pose, "0"));
    let label = NodeId::new(101, 0);
    let mut worst = 0;
    let mut k = 1;
    let mut early = 0;
    while p.on.wants_frame(S) {
        let mut d = SceneDiff::new();
        d.set(label, Prop::Text, text(&format!("{k} {k} {k}")));
        p.apply(d);
        worst = worst.max(p.paint(k));
        if k == 4 {
            early = p.on.fade_layers().0;
        }
        k += 1;
        assert!(k < 400, "never settled");
    }
    assert!(k > 8, "it fades over frames: {k}");
    assert!(worst <= TOLERANCE, "off by {worst}");
    // The panel's first two layers go unused, and it is drawn as before
    // (its translucent row, flat fills only, is never layered).
    let (built, _) = p.on.fade_layers();
    assert!(built <= 2, "{built} built");
    assert_eq!(built, early, "none built after the fourth frame");
}

/// Random edits around and inside fading groups (their opacity, their
/// offset, their content, entering and leaving with a pose, the rounded
/// clip around them), painted incrementally into rotating buffers with
/// fade layers, always match a full repaint without them: no layer is
/// drawn stale, and its damage is the group's.
#[test]
fn random_edits_to_fading_groups_match_rastering_them() {
    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        for seed in [0x2545_f491_4f6c_dd1d, 0x9e37_79b9_7f4a_7c15] {
            random_fades(seed, scale, 140);
        }
    }
}

fn random_fades(seed: u64, scale: Scale, frames: u32) {
    let mut seed = seed;
    let mut rnd = move |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as u32
    };
    let palette = ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af", "#45475a"];
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Font, PropValue::Font(font(12.0))),
            (Prop::Color, color("#ffffff")),
        ],
    );
    let card = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(4.0)),
            (Prop::Width, num(180.0)),
            (Prop::Height, num(70.0)),
            (Prop::Radius, num(12.0)),
            (Prop::Clip, PropValue::Bool(true)),
            (Prop::Bg, color("#313244")),
        ],
    );
    let group = |b: &mut Builder, x: f32| {
        let g = b.node(
            NodeKind::Col,
            Some(card),
            vec![
                (Prop::X, num(x)),
                (Prop::Y, num(6.0)),
                (Prop::Width, num(70.0)),
                (Prop::Pad, num(4.0)),
                (Prop::Gap, num(3.0)),
                (Prop::Radius, num(8.0)),
                (Prop::Bg, color("#585b70")),
                (Prop::Opacity, num(0.5)),
                (Prop::Shadow, PropValue::Shadow(vec![shadow(2.0, 6.0, 0.5)])),
                (
                    Prop::Enter,
                    PropValue::Pose(vec![(Prop::Opacity, num(0.0)), (Prop::Y, num(-6.0))]),
                ),
                (
                    Prop::Exit,
                    PropValue::Pose(vec![(Prop::Opacity, num(0.0)), (Prop::Y, num(8.0))]),
                ),
            ],
        );
        let t = b.node(NodeKind::Text, Some(g), vec![(Prop::Text, text("fade"))]);
        let kids: Vec<NodeId> = (0..3)
            .map(|i| {
                b.node(
                    NodeKind::Box,
                    Some(g),
                    vec![
                        (Prop::Height, num(8.0)),
                        (Prop::Radius, num(3.0)),
                        (Prop::Bg, color(palette[i])),
                    ],
                )
            })
            .collect();
        (g, t, kids)
    };
    let (g1, t1, k1) = group(&mut b, 0.0);
    let (g2, t2, k2) = group(&mut b, 90.0);
    let size = scale.physical_size(LogicalSize::new(200.0, 80.0));
    let (w, h) = (size.w, size.h);
    let mut reference = renderer();
    reference.set_fade_layers(false);
    // (shadow-redraw) The same frames with layers, each repainted in
    // full: a partial repaint must leave exactly what it draws.
    let mut layered = renderer();
    let mut r = renderer();
    let mut bufs = [
        Buffer::new(w, h, scale),
        Buffer::new(w, h, scale),
        Buffer::new(w, h, scale),
    ];
    for x in [&mut reference, &mut layered, &mut r] {
        assert!(x.apply(b.diff.clone()).is_empty());
        x.attach_surface(S, root);
    }
    bufs[0].paint_at(&mut r, S, 0, T0);
    Buffer::new(w, h, scale).paint_at(&mut reference, S, 0, T0);
    Buffer::new(w, h, scale).paint_at(&mut layered, S, 0, T0);
    let mut commits = 1usize;
    let mut painted_at: [Option<usize>; 3] = [Some(1), None, None];
    let mut front = 0;
    let mut log: Vec<String> = Vec::new();
    let mut edge_misses = 0usize;
    let mut edge_worst = 0u8;
    // The card's radius targets, by the frame that set them.
    let mut radii: Vec<(u32, f32)> = vec![(0, 12.0)];
    // The second group is removed and made again (its exit, then its
    // enter) under fresh ids.
    let mut live2 = Some((g2, t2, k2));
    let mut next = 1000u32;
    for k in 1..frames {
        let mut d = SceneDiff::new();
        for _ in 0..1 + rnd(2) {
            let (g, t, kids) = match &live2 {
                Some((g, t, kids)) if rnd(2) == 0 => (*g, *t, kids.clone()),
                _ => (g1, t1, k1.clone()),
            };
            match rnd(10) {
                0 => {
                    d.set(g, Prop::Opacity, num([0.3, 0.6, 0.9, 1.0][rnd(4) as usize]));
                }
                1 => {
                    d.set(g, Prop::Y, num(rnd(12) as f32 - 2.0));
                }
                2 => {
                    d.set(g, Prop::X, num(rnd(110) as f32));
                }
                3 => {
                    d.set(
                        kids[rnd(3) as usize],
                        Prop::Bg,
                        color(palette[rnd(5) as usize]),
                    );
                }
                4 => {
                    d.set(t, Prop::Text, text(&rnd(100).to_string()));
                }
                5 => {
                    d.set(card, Prop::Width, num(120.0 + rnd(70) as f32));
                }
                6 => {
                    let v = rnd(16) as f32;
                    radii.push((k, v));
                    d.set(card, Prop::Radius, num(v));
                }
                7 => {
                    d.set(
                        kids[rnd(3) as usize],
                        Prop::Height,
                        num(4.0 + rnd(8) as f32),
                    );
                }
                _ => match live2.take() {
                    Some((g, ..)) => {
                        d.push(SceneOp::Remove {
                            id: g,
                            window: false,
                        });
                    }
                    None => {
                        let id = |n: &mut u32| {
                            *n += 1;
                            NodeId::new(*n, 0)
                        };
                        let g = id(&mut next);
                        d.create(g, NodeKind::Col, Some(card), u32::MAX)
                            .set(g, Prop::X, num(90.0))
                            .set(g, Prop::Y, num(10.0))
                            .set(g, Prop::Width, num(60.0))
                            .set(g, Prop::Pad, num(4.0))
                            .set(g, Prop::Radius, num(8.0))
                            .set(g, Prop::Bg, color("#585b70"))
                            .set(
                                g,
                                Prop::Enter,
                                PropValue::Pose(vec![
                                    (Prop::Opacity, num(0.0)),
                                    (Prop::Y, num(-6.0)),
                                ]),
                            )
                            .set(
                                g,
                                Prop::Exit,
                                PropValue::Pose(vec![(Prop::Opacity, num(0.0))]),
                            );
                        let t = id(&mut next);
                        d.create(t, NodeKind::Text, Some(g), u32::MAX).set(
                            t,
                            Prop::Text,
                            text("again"),
                        );
                        let kids: Vec<NodeId> = (0..3)
                            .map(|i| {
                                let c = id(&mut next);
                                d.create(c, NodeKind::Box, Some(g), u32::MAX)
                                    .set(c, Prop::Height, num(6.0))
                                    .set(c, Prop::Radius, num(3.0))
                                    .set(c, Prop::Bg, color(palette[i]));
                                c
                            })
                            .collect();
                        live2 = Some((g, t, kids));
                    }
                },
            }
        }
        log.push(format!("{d:?}"));
        reference.apply(d.clone());
        layered.apply(d.clone());
        r.apply(d);
        let i = rnd(3) as usize;
        if r.wants_frame(S) {
            let age = painted_at[i].map_or(0, |c| (commits + 1 - c).min(255) as u8);
            let dmg = bufs[i].paint_at(&mut r, S, age, frame(k));
            log.push(format!("frame {k} buf {i} age {age} damage {dmg:?}"));
            if !dmg.is_empty() {
                commits += 1;
                painted_at[i] = Some(commits);
                front = i;
            }
        }
        let mut full = Buffer::new(w, h, scale);
        full.paint_at(&mut reference, S, 0, frame(k));
        let mut whole = Buffer::new(w, h, scale);
        whole.paint_at(&mut layered, S, 0, frame(k));
        let stale: Vec<(u32, u32)> = bufs[front]
            .pixels
            .chunks(4)
            .zip(whole.pixels.chunks(4))
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| (i as u32 % w, i as u32 / w))
            .collect();
        if !stale.is_empty() {
            for l in &log[log.len().saturating_sub(10)..] {
                eprintln!("{l}");
            }
            panic!(
                "scale {scale:?} frame {k}: {} pixels differ from the frame repainted in full with layers: {:?}",
                stale.len(),
                &stale[..stale.len().min(20)]
            );
        }
        // Past the tolerance only along the card's clip edge while it
        // springs between pixels: vello composites a group through a
        // clip's antialiased edge a little differently from the shapes
        // in it one by one. The edge is the clip's outline, its rounded
        // corners included, at any radius the card's springs between
        // the radii set in the last 120 frames and the one before:
        // pixels within 1.5 px of it. What a layered frame draws there is
        // held exactly to the layered full repaint above.
        let edge = {
            // Its laid box, then its paint offset (`x: 10; y: 4`).
            let c = r.boxes(S).unwrap().rects[&card];
            let (cx, cy) = (c.x + 10.0, c.y + 4.0);
            let f = scale.as_f64() as f32;
            let (mx, my) = ((cx + c.w / 2.0) * f, (cy + c.h / 2.0) * f);
            let (hw, hh) = (c.w / 2.0 * f, c.h / 2.0 * f);
            let held = radii.iter().rposition(|(at, _)| *at + 120 < k).unwrap_or(0);
            let (lo, hi) = radii[held..]
                .iter()
                .fold((f32::MAX, 0.0f32), |(a, b), (_, v)| (a.min(*v), b.max(*v)));
            move |x: u32, y: u32| {
                let (x, y) = (x as f32 + 0.5, y as f32 + 0.5);
                // The rounded rect's signed distance at radius `rad`.
                let at = |rad: f32| {
                    let rad = (rad * f).min(hw).min(hh);
                    let qx = (x - mx).abs() - hw + rad;
                    let qy = (y - my).abs() - hh + rad;
                    qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad
                };
                let steps = ((hi - lo) * 4.0).ceil() as u32;
                (0..=steps)
                    .any(|i| at(lo + (hi - lo) * i as f32 / steps.max(1) as f32).abs() <= 1.5)
            }
        };
        let bad: Vec<(u32, u32, u8)> = bufs[front]
            .pixels
            .chunks(4)
            .zip(full.pixels.chunks(4))
            .enumerate()
            .filter_map(|(i, (a, b))| {
                let d = a
                    .iter()
                    .zip(b)
                    .map(|(x, y)| x.abs_diff(*y))
                    .max()
                    .unwrap_or(0);
                (d > TOLERANCE).then_some((i as u32 % w, i as u32 / w, d))
            })
            .collect();
        edge_misses += bad.len();
        edge_worst = bad.iter().fold(edge_worst, |w, (.., d)| w.max(*d));
        // (integration-r4) On the edge the two differ only in how the
        // clip's coverage `c` meets the group (an image at its alpha
        // against a group composited): a mix of `c` and `c²` of what lies
        // across the edge, never more than c(1 - c) <= 1/4 of the
        // contrast there. The contrast is the reference's own: the widest
        // channel range in the 5 x 5 pixels around (measured over 204
        // runs, 24 seeds and 100 more at both scales: at most 0.255 of
        // it, within the tolerance).
        let contrast = |x: u32, y: u32| {
            let mut range = 0u8;
            for c in 0..4 {
                let (mut lo, mut hi) = (u8::MAX, 0u8);
                for yy in y.saturating_sub(2)..(y + 3).min(h) {
                    for xx in x.saturating_sub(2)..(x + 3).min(w) {
                        let v = full.pixels[((yy * w + xx) * 4) as usize + c];
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                }
                range = range.max(hi - lo);
            }
            range
        };
        let off_edge: Vec<_> = bad
            .iter()
            .filter(|(x, y, d)| !edge(*x, *y) || *d > contrast(*x, *y) / 4 + TOLERANCE)
            .collect();
        if !off_edge.is_empty() {
            for l in &log[log.len().saturating_sub(10)..] {
                eprintln!("{l}");
            }
            panic!(
                "scale {scale:?} frame {k}: {} pixels differ by more than {TOLERANCE} off the card's edge, or by more than a quarter of the contrast across it on it: {:?}",
                off_edge.len(),
                &off_edge[..off_edge.len().min(20)]
            );
        }
    }
    eprintln!(
        "{scale:?}: {edge_misses} pixels over the tolerance on the clip's edge (worst {edge_worst})"
    );
    let (built, drawn) = r.fade_layers();
    assert!(
        drawn > built && built > 0,
        "{scale:?}: {built} built, {drawn} drawn"
    );
}

/// (shadow-redraw) A translucent panel with shadows that faded in and
/// rests drawn from its cached layer, whose label then changes: the
/// partial repaint, drawn only around the label, leaves the buffer
/// exactly as a renderer that never cached a layer paints that frame in
/// full, at scales 1 and 1.5. Before, the repaint rastered the group
/// directly where it was damaged (its new key not cached, and the frame
/// not repainting it whole), and the layer's pixels around it, 8-bit
/// before they were composited, were a step off what the full repaint
/// rastered: whether a group was drawn from a layer depended on what
/// earlier frames had cached.
#[test]
fn a_resting_layered_panel_repaints_a_change_exactly() {
    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        let pose = vec![(Prop::Opacity, num(0.0))];
        let translucent = |label: &str| {
            let mut d = panel(NodeId::new(1, 0), pose.clone(), label);
            d.set(NodeId::new(100, 0), Prop::Opacity, num(0.7));
            d
        };
        let size = scale.physical_size(LogicalSize::new(W, H));
        let mut b = Builder::default();
        let root = stage(&mut b);
        let mut r = renderer();
        assert!(r.apply(b.diff.clone()).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(size.w, size.h, scale);
        buf.paint_at(&mut r, S, 0, T0);
        assert!(r.apply(translucent("12:00")).is_empty());
        let mut k = 1;
        while r.wants_frame(S) {
            buf.paint_at(&mut r, S, 1, frame(k));
            k += 1;
            assert!(k < 400, "never settled");
        }
        let (built, drawn) = r.fade_layers();
        assert!(built >= 1, "{scale:?}: the fade builds the panel's layer");
        assert!(drawn > built, "{scale:?}: and draws it again");

        let mut d = SceneDiff::new();
        d.set(NodeId::new(101, 0), Prop::Text, text("12:01"));
        assert!(r.apply(d).is_empty());
        let passes = r.fade_passes();
        let damage = buf.paint_at(&mut r, S, 1, frame(k));
        assert!(
            r.fade_passes() > passes,
            "{scale:?}: the changed panel is drawn from a layer rastered where it repaints"
        );
        let whole = Rect::new(0, 0, size.w, size.h);
        assert!(
            damage.bounds().is_some() && !damage.covers(whole),
            "{scale:?}: the label repaints only around itself: {damage:?}"
        );

        // A renderer that never drew the panel before: the same frame in
        // full, with nothing cached.
        let mut b = Builder::default();
        let root = stage(&mut b);
        let mut fresh = renderer();
        assert!(fresh.apply(b.diff.clone()).is_empty());
        assert!(fresh.apply(translucent("12:01")).is_empty());
        fresh.attach_surface(S, root);
        let mut full = Buffer::new(size.w, size.h, scale);
        full.paint_at(&mut fresh, S, 0, frame(k));
        let differ = buf
            .pixels
            .chunks_exact(4)
            .zip(full.pixels.chunks_exact(4))
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            differ, 0,
            "{scale:?}: {differ} pixels of the partial repaint differ from a full one"
        );
    }
}

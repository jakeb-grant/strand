//! (M4) Effect layers: group opacity, blend modes and masks drawn through
//! vello_cpu layers, partial repaints under them equal to full ones, and
//! damage grown by each effect's reach (`crate::layers`).

mod common;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

/// Four 40 px boxes in a 240×60 bar: two overlapping squares under one
/// group opacity, a square multiplied onto a yellow box, a green box
/// fading out toward its bottom and a mauve box revealed in a circle.
/// Returns the scene, the effects per node and the faded box.
fn scene(fade_bg: &str) -> (SceneDiff, Vec<(NodeId, Vec<Effect>)>, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let place = |x: f32, more: Vec<(Prop, PropValue)>| {
        let mut p = vec![
            (Prop::X, num(x)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
        ];
        p.extend(more);
        p
    };
    let group = b.node(NodeKind::Box, Some(root), place(10.0, vec![]));
    let sq = |c: &str, x: f32, y: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(y)),
            (Prop::Size, num(24.0)),
            (Prop::Bg, color(c)),
        ]
    };
    b.node(NodeKind::Box, Some(group), sq("#f38ba8", 0.0, 0.0));
    b.node(NodeKind::Box, Some(group), sq("#89b4fa", 12.0, 12.0));
    let yellow = b.node(
        NodeKind::Box,
        Some(root),
        place(70.0, vec![(Prop::Bg, color("#f9e2af"))]),
    );
    let multiplied = b.node(NodeKind::Box, Some(yellow), sq("#89b4fa", 20.0, 20.0));
    let faded = b.node(
        NodeKind::Box,
        Some(root),
        place(130.0, vec![(Prop::Bg, color(fade_bg))]),
    );
    let revealed = b.node(
        NodeKind::Box,
        Some(root),
        place(190.0, vec![(Prop::Bg, color("#cba6f7"))]),
    );
    let effects = vec![
        (group, vec![Effect::Opacity(0.5)]),
        (multiplied, vec![Effect::Blend(BlendMode::Multiply)]),
        (
            faded,
            vec![Effect::Mask(strand_scene::Mask::Fade {
                edge: Edge::Bottom,
                len: 20.0,
            })],
        ),
        (
            revealed,
            vec![Effect::Mask(strand_scene::Mask::Radial {
                at: Anchor::Center,
                size: 16.0,
            })],
        ),
    ];
    (b.diff, effects, faded)
}

fn drawn(diff: SceneDiff, effects: &[(NodeId, Vec<Effect>)], scale: Scale) -> (Renderer, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    for (id, e) in effects {
        r.set_layer_effects(*id, e.clone());
    }
    r.attach_surface(S, r.tree().roots()[0]);
    let (w, h) = (
        (240.0 * scale.as_f32()).round() as u32,
        (60.0 * scale.as_f32()).round() as u32,
    );
    let mut buf = Buffer::new(w, h, scale);
    buf.paint(&mut r, S, 0);
    (r, buf)
}

/// design.md "Effects": group opacity (the overlap is no darker than
/// either square), `blend: multiply`, `mask: fade(bottom, 20)` and
/// `mask: radial(center, 16)`, at 1× and 2× (ref `layers.png`,
/// `layers_2x.png`).
#[test]
fn effect_layers_draw_opacity_blend_and_masks() {
    let (diff, effects, _) = scene("#a6e3a1");
    let (_, buf) = drawn(diff, &effects, Scale::ONE);
    assert_matches_ref("layers", &buf, 1);
    let (diff, effects, _) = scene("#a6e3a1");
    let (_, buf) = drawn(diff, &effects, Scale::new(240).unwrap());
    assert_matches_ref("layers_2x", &buf, 1);
}

/// A change under a mask layer repaints only its damage, and the result
/// equals a full repaint.
#[test]
fn partial_repaints_under_layers_match_full() {
    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        let (diff, effects, faded) = scene("#a6e3a1");
        let (mut r, mut buf) = drawn(diff, &effects, scale);
        let mut d = SceneDiff::new();
        d.set(faded, Prop::Bg, color("#fab387"));
        assert!(r.apply(d).is_empty());
        let damage = buf.paint(&mut r, S, 1);
        assert!(!damage.is_empty());
        assert!(
            damage.area() < (buf.size.w * buf.size.h / 4) as u64,
            "{scale:?}: only the faded box repaints: {damage:?}"
        );
        let (diff, effects, _) = scene("#fab387");
        let (_, full) = drawn(diff, &effects, scale);
        assert!(
            buf.pixels == full.pixels,
            "{scale:?}: partial differs from full"
        );
    }
}

/// design.md: a layer's damage grows by its effects' reach. A child of
/// a box under `blur(2)` (3σ = 6 px) that changes damages its box grown
/// by 6 px; the same box with no effect damages only itself.
#[test]
fn damage_grows_by_each_effects_reach() {
    for (effects, grow) in [
        (vec![], 0),
        (vec![Effect::Blur { radius: 2.0 }], 6),
        (vec![Effect::Blur { radius: 2.0 }, Effect::Opacity(0.5)], 6),
    ] {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let outer = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(60.0)),
                (Prop::Y, num(10.0)),
                (Prop::Size, num(40.0)),
            ],
        );
        let inner = b.node(
            NodeKind::Box,
            Some(outer),
            vec![
                (Prop::X, num(8.0)),
                (Prop::Y, num(8.0)),
                (Prop::Size, num(16.0)),
                (Prop::Bg, color("#f38ba8")),
            ],
        );
        let (mut r, mut buf) = drawn(b.diff, &[(outer, effects.clone())], Scale::ONE);
        let mut d = SceneDiff::new();
        d.set(inner, Prop::Bg, color("#89b4fa"));
        assert!(r.apply(d).is_empty());
        let damage = buf.paint(&mut r, S, 1);
        let rect = r.boxes(S).unwrap().rects[&inner];
        // Drawn at its box moved by its own and its parent's `x`, `y`.
        let x = (rect.x + 68.0) as i32;
        let y = (rect.y + 18.0) as i32;
        let want = Rect::new(
            x - grow,
            y - grow,
            16 + 2 * grow as u32,
            16 + 2 * grow as u32,
        );
        assert!(
            damage.covers(want),
            "{effects:?}: {damage:?} misses {want:?}"
        );
        assert!(
            damage.area() <= want.area() + 4 * (16 + 2 * grow as u64 + 2),
            "{effects:?}: {damage:?} is more than {want:?}"
        );
    }
}

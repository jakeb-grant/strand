//! (shadow-part, decisions.md) A large shadow, too large to cache at
//! first sight (`EAGER_ENTRY_BYTES`) and too short for its blur to be
//! sliced, is drawn from its pixmap's pixels on every frame: from the
//! cache once a second whole frame has admitted it, else from the cells
//! of the pixmap's fixed grid the frame shows, rastered uncached. So a
//! card at 2× scaling in (`enter { opacity: 0; scale: 0.96 }`) and then
//! shrinking, every frame a partial redraw over the last, ends exactly
//! as a renderer that never drew it paints it in one frame (the docs
//! launcher's `redraw` failure: 26,732 pixels a level off while it
//! scaled, 32 along its corners at rest). Regenerate the reference with
//! `STRAND_BLESS=1 cargo test -p strand-render --test shadow_part`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);
const W: f32 = 680.0;
const H: f32 = 330.0;
const CARD: NodeId = NodeId::new(100, 0);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

fn stage(b: &mut Builder) -> NodeId {
    b.node(NodeKind::Bar, None, vec![(Prop::Pad, num(40.0))])
}

/// The launcher's card (site/how-to/launcher.md): translucent, rounded,
/// clipping, with `$elevation.xl`'s shadow, `height` tall.
fn card(root: NodeId, height: f32) -> SceneDiff {
    let mut d = SceneDiff::new();
    d.create(CARD, NodeKind::Col, Some(root), u32::MAX);
    for (p, v) in [
        (Prop::Width, num(600.0)),
        (Prop::Height, num(height)),
        (Prop::Radius, num(20.0)),
        (Prop::Clip, PropValue::Bool(true)),
        (Prop::Bg, color("#eff1f5d1")),
        (
            Prop::Shadow,
            PropValue::Shadow(vec![Shadow {
                x: 0.0,
                y: 16.0,
                blur: 48.0,
                spread: 0.0,
                color: hex("#00000066"),
            }]),
        ),
        (
            Prop::Enter,
            PropValue::Pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.96))]),
        ),
    ] {
        d.set(CARD, p, v);
    }
    d
}

/// Paints frames from `k` on, each over the last (age 1), until the
/// surface settles; returns the next frame.
fn settle(r: &mut Renderer, buf: &mut Buffer, mut k: u32) -> u32 {
    while r.wants_frame(S) {
        buf.paint_at(r, S, 1, frame(k));
        k += 1;
        assert!(k < 600, "never settled");
    }
    k
}

#[test]
fn a_large_shadow_entering_and_shrinking_repaints_as_fresh() {
    let scale = Scale::from_integer(2).unwrap();
    let size = scale.physical_size(LogicalSize::new(W, H));
    let mut b = Builder::default();
    let root = stage(&mut b);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    let mut buf = Buffer::new(size.w, size.h, scale);
    buf.paint_at(&mut r, S, 0, T0);
    assert!(r.apply(card(root, 151.0)).is_empty());
    let k = settle(&mut r, &mut buf, 1);
    assert!(k > 5, "the card scales in over frames: {k}");
    let mut d = SceneDiff::new();
    d.set(CARD, Prop::Height, num(103.0));
    assert!(r.apply(d).is_empty());
    let k = settle(&mut r, &mut buf, k);

    // A renderer that never drew the card: its last frame in full.
    let mut b = Builder::default();
    let root = stage(&mut b);
    let mut fresh = renderer();
    assert!(fresh.apply(b.diff).is_empty());
    assert!(fresh.apply(card(root, 103.0)).is_empty());
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
        "{differ} pixels of the partial redraws differ from a full paint"
    );
    assert_matches_ref("shadow_part_2x", &buf, 1);
}

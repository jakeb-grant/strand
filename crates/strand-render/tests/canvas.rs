//! (M4) `canvas { draw: }`: a recorded draw list painted as the renderer
//! paints boxes (design.md, "Canvas: the same paint API the renderer
//! uses"), offline against a PNG reference, at 1× and 1.5×; a themed
//! paint is resolved in the node's token scope; a new list repaints only
//! the canvas.

mod canvas_scene;
mod common;

use std::sync::Arc;

use canvas_scene::{canvas_scene, chart};
use common::*;
use strand_scene::canvas::DrawOp;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;

fn drawn(scale: Scale) -> Buffer {
    let mut r = renderer();
    let (diff, _) = canvas_scene(chart());
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let w = (160.0 * scale.as_f64()).round() as u32;
    let h = (60.0 * scale.as_f64()).round() as u32;
    let mut buf = Buffer::new(w, h, scale);
    buf.paint(&mut r, S, 0);
    buf
}

#[test]
fn a_canvas_draws_its_list_like_boxes() {
    assert_matches_ref("canvas_chart", &drawn(Scale::ONE), TOLERANCE);
}

#[test]
fn a_canvas_draws_at_the_surface_scale() {
    assert_matches_ref(
        "canvas_chart_1_5x",
        &drawn(Scale::from_f64(1.5).unwrap()),
        TOLERANCE,
    );
}

#[test]
fn a_themed_paint_resolves_and_a_new_list_repaints_the_canvas_only() {
    let accent = PropValue::Token(TokenExpr::path("accent"));
    let mut r = renderer();
    let (mut diff, node) = canvas_scene(vec![DrawOp::FillThemed(accent.clone())]);
    let mut tokens = TokenTable::default();
    tokens.insert("accent", PropValue::Color(hex("#ff0000")));
    // On the bar (the root, the first node built).
    diff.set(
        NodeId::new(0, 0),
        Prop::Tokens,
        PropValue::Tokens(Box::new(tokens)),
    );
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(160, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    // BGRA: the box is the accent.
    assert_eq!(buf.px(50, 30), [0, 0, 255, 255]);
    assert_eq!(buf.px(5, 5), [0x2e, 0x1e, 0x1e, 255]);

    let mut d = SceneDiff::default();
    d.set(
        node,
        Prop::Draw,
        PropValue::DrawList(Arc::from(vec![
            DrawOp::Rect {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
            },
            DrawOp::Fill(Paint::Solid(hex("#00ff00"))),
        ])),
    );
    assert!(r.apply(d).is_empty());
    let damage = buf.paint(&mut r, S, 1);
    let rects: Vec<Rect> = damage.rects().to_vec();
    assert!(
        rects
            .iter()
            .all(|d| d.x >= 10 && d.y >= 10 && d.x + d.w as i32 <= 110 && d.y + d.h as i32 <= 50),
        "{rects:?}"
    );
    assert_eq!(buf.px(15, 15), [0, 255, 0, 255]);
}

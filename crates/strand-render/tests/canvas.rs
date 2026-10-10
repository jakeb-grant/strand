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

/// `c.text(t, x, y)` (design.md, "Canvas"): text in the node's font and
/// colour with its baseline starting at (`x`, `y`), drawn in order with
/// the shapes (a later fill covers it) and clipped to the box, at 1× and
/// 2× (refs `canvas_text.png`, `canvas_text_2x.png`).
#[test]
fn a_canvas_draws_its_texts() {
    let solid = |c: &str| Paint::Solid(hex(c));
    let text = |t: &str, x, y| DrawOp::Text {
        text: t.into(),
        x,
        y,
    };
    let ops = vec![
        DrawOp::Fill(solid("#313244")),
        text("CPU 42%", 6.0, 18.0),
        text("hidden", 6.0, 34.0),
        DrawOp::Rect {
            x: 0.0,
            y: 24.0,
            w: 40.0,
            h: 16.0,
        },
        DrawOp::Fill(solid("#45475a")),
        text("clipped past the box", 60.0, 34.0),
    ];
    for (scale, name) in [
        (Scale::ONE, "canvas_text"),
        (Scale::new(240).unwrap(), "canvas_text_2x"),
    ] {
        let mut r = renderer();
        let (mut diff, node) = canvas_scene(ops.clone());
        diff.set(node, Prop::Font, PropValue::Font(font(13.0)));
        diff.set(node, Prop::Color, color("#cdd6f4"));
        assert!(r.apply(diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let k = scale.as_f64();
        let mut buf = Buffer::new((160.0 * k) as u32, (60.0 * k) as u32, scale);
        buf.paint(&mut r, S, 0);
        // Shaped inline: drawn by the next frame at the latest.
        buf.paint(&mut r, S, 0);
        let px = |x: f64, y: f64| buf.px((x * k) as u32, (y * k) as u32);
        let lit = |x0: f64, x1: f64, y0: f64, y1: f64| {
            let mut n = 0;
            let (mut x, step) = (x0, 1.0 / k);
            while x < x1 {
                let mut y = y0;
                while y < y1 {
                    let [b, g, r, _] = px(x, y);
                    if r > 0x90 && g > 0x90 && b > 0x90 {
                        n += 1;
                    }
                    y += step;
                }
                x += step;
            }
            n
        };
        // The first text sits above its baseline at y = 18 (+10 for the box).
        assert!(lit(16.0, 70.0, 16.0, 28.0) > 20, "{name}: the label drawn");
        assert_eq!(
            lit(16.0, 70.0, 28.5, 33.0),
            0,
            "{name}: nothing below its baseline"
        );
        // The later fill covers the second text where it overlaps.
        assert_eq!(lit(10.0, 50.0, 34.0, 50.0), 0, "{name}: covered");
        // The third is clipped at the box's right edge.
        assert_eq!(lit(110.5, 160.0, 0.0, 60.0), 0, "{name}: clipped");
        assert!(
            lit(70.0, 110.0, 34.0, 50.0) > 10,
            "{name}: drawn in the box"
        );
        assert_matches_ref(name, &buf, TOLERANCE);
    }
}

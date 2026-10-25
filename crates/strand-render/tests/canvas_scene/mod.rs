//! The canvas scene `canvas.rs` and `gpu.rs` draw.

#![allow(dead_code)]

use std::sync::Arc;

use strand_scene::canvas::DrawOp;
use strand_scene::*;

use super::common::*;

/// A 160×60 bar with a 100×40 canvas at (10, 10) showing `ops`.
pub fn canvas_scene(ops: Vec<DrawOp>) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let node = b.node(
        NodeKind::Canvas,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Width, num(100.0)),
            (Prop::Height, num(40.0)),
            (Prop::Draw, PropValue::DrawList(Arc::from(ops))),
        ],
    );
    (b.diff, node)
}

/// A small chart: a background, bars, a polyline and a dot.
pub fn chart() -> Vec<DrawOp> {
    let solid = |c: &str| Paint::Solid(hex(c));
    let mut ops = vec![DrawOp::Fill(solid("#313244"))];
    for (i, h) in [12.0, 24.0, 18.0, 30.0].into_iter().enumerate() {
        ops.push(DrawOp::Rect {
            x: 6.0 + i as f32 * 14.0,
            y: 36.0 - h,
            w: 10.0,
            h,
        });
    }
    ops.push(DrawOp::Fill(Paint::Linear {
        angle: 180.0,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: hex("#89b4fa"),
            },
            GradientStop {
                offset: 1.0,
                color: hex("#cba6f7"),
            },
        ],
    }));
    let pts = [(64.0, 30.0), (74.0, 12.0), (84.0, 22.0), (96.0, 6.0)];
    for w in pts.windows(2) {
        ops.push(DrawOp::Line {
            x1: w[0].0,
            y1: w[0].1,
            x2: w[1].0,
            y2: w[1].1,
        });
    }
    ops.push(DrawOp::Stroke {
        paint: solid("#a6e3a1"),
        width: 2.0,
    });
    ops.push(DrawOp::Circle {
        x: 96.0,
        y: 6.0,
        r: 3.0,
    });
    ops.push(DrawOp::Fill(solid("#f9e2af")));
    // Off the box: clipped.
    ops.push(DrawOp::Circle {
        x: 100.0,
        y: 40.0,
        r: 8.0,
    });
    ops.push(DrawOp::Fill(solid("#f38ba8")));
    ops
}

/// (flex) Paths drawn point by point: two pie slices (`move`, `arc`,
/// `close`), an even-odd ring, a stroked half circle over the top and a
/// triangle from `to` alone.
pub fn paths() -> Vec<DrawOp> {
    let solid = |c: &str| Paint::Solid(hex(c));
    let arc = |x, y, r, from, to| DrawOp::Arc { x, y, r, from, to };
    let circle = |x, y, r| DrawOp::Circle { x, y, r };
    vec![
        DrawOp::Fill(solid("#313244")),
        // A pie: 12 o'clock to 4 o'clock red, the rest blue.
        DrawOp::MoveTo { x: 20.0, y: 20.0 },
        arc(20.0, 20.0, 16.0, -90.0, 30.0),
        DrawOp::Close,
        DrawOp::Fill(solid("#ff0000")),
        DrawOp::MoveTo { x: 20.0, y: 20.0 },
        arc(20.0, 20.0, 16.0, 30.0, 270.0),
        DrawOp::Close,
        DrawOp::Fill(solid("#0000ff")),
        // A ring: the inner circle cuts a hole.
        circle(55.0, 20.0, 15.0),
        circle(55.0, 20.0, 8.0),
        DrawOp::FillEvenOdd(solid("#00ff00")),
        // From 9 o'clock clockwise to 3 o'clock: over the top.
        arc(85.0, 20.0, 12.0, 180.0, 360.0),
        DrawOp::Stroke {
            paint: solid("#ffff00"),
            width: 3.0,
        },
        // A triangle from `to` alone: the first `to` starts it.
        DrawOp::LineTo { x: 79.0, y: 39.0 },
        DrawOp::LineTo { x: 91.0, y: 39.0 },
        DrawOp::LineTo { x: 85.0, y: 33.0 },
        DrawOp::Close,
        DrawOp::Fill(solid("#ffffff")),
    ]
}

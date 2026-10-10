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

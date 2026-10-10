//! (M4) What a `canvas`'s `draw: (c) => …` records: the renderer's own
//! paint vocabulary, in a canvas-2D-like state model. The VM records the
//! ops on the logic thread while running `draw` (and again when what it
//! read changes); render draws them with vello_cpu. They travel as
//! [`crate::PropValue::DrawList`] on [`crate::Prop::Draw`].
//!
//! Coordinates are logical pixels from the canvas box's top-left corner
//! (`c.width` and `c.height` come from layout facts). Shapes add to the
//! current path; [`DrawOp::Fill`] and [`DrawOp::Stroke`] paint it and
//! start a new one. The variants are the ones builtin.schema's `Canvas`
//! record offers today; transforms and clips are added with the canvas
//! work, so other crates match with a wildcard arm.

use crate::protocol::{Paint, PropValue};

/// One recorded canvas call.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DrawOp {
    /// `c.line(x1, y1, x2, y2)`: a segment added to the current path.
    Line { x1: f32, y1: f32, x2: f32, y2: f32 },
    /// `c.rect(x, y, w, h)`: a rectangle added to the current path.
    Rect { x: f32, y: f32, w: f32, h: f32 },
    /// `c.circle(x, y, r)`: a circle added to the current path.
    Circle { x: f32, y: f32, r: f32 },
    /// `c.fill(paint)`: fills the current path (non-zero), then clears it.
    Fill(Paint),
    /// `c.stroke(paint, width)`: strokes the current path, then clears it.
    Stroke { paint: Paint, width: f32 },
    /// `c.text(t, x, y)`: text in the node's font and colour, its
    /// baseline's start at (`x`, `y`).
    Text { text: String, x: f32, y: f32 },
    /// (M4) `c.fill(paint)` with a paint that names theme tokens
    /// (`c.fill($accent)`): render resolves it in the node's token scope
    /// when it draws, as it does a box's `bg`.
    FillThemed(PropValue),
    /// (M4) `c.stroke(paint, width)` with a themed paint.
    StrokeThemed { paint: PropValue, width: f32 },
}

impl DrawOp {
    /// Every canvas method an op records, in declaration order: the
    /// functions of builtin.schema's `Canvas` record (the catalogue test
    /// keeps the two equal).
    pub const METHODS: &'static [&'static str] =
        &["line", "rect", "circle", "fill", "stroke", "text"];

    /// The canvas method that records this op, as builtin.schema names it.
    pub fn method(&self) -> &'static str {
        match self {
            DrawOp::Line { .. } => "line",
            DrawOp::Rect { .. } => "rect",
            DrawOp::Circle { .. } => "circle",
            DrawOp::Fill(_) | DrawOp::FillThemed(_) => "fill",
            DrawOp::Stroke { .. } | DrawOp::StrokeThemed { .. } => "stroke",
            DrawOp::Text { .. } => "text",
        }
    }

    /// True for ops that paint (and end) the current path.
    pub fn paints(&self) -> bool {
        matches!(
            self,
            DrawOp::Fill(_)
                | DrawOp::Stroke { .. }
                | DrawOp::FillThemed(_)
                | DrawOp::StrokeThemed { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Color, Prop, PropValue};

    #[test]
    fn a_recorded_list_travels_as_a_draw_prop() {
        let ops: Arc<[DrawOp]> = Arc::from(vec![
            DrawOp::Rect {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 4.0,
            },
            DrawOp::Circle {
                x: 5.0,
                y: 2.0,
                r: 1.0,
            },
            DrawOp::Fill(Paint::Solid(Color::WHITE)),
            DrawOp::Line {
                x1: 0.0,
                y1: 0.0,
                x2: 10.0,
                y2: 4.0,
            },
            DrawOp::Stroke {
                paint: Paint::Solid(Color::BLACK),
                width: 1.0,
            },
            DrawOp::Text {
                text: "42".into(),
                x: 1.0,
                y: 3.0,
            },
        ]);
        let v = PropValue::DrawList(ops.clone());
        // Cloning shares the list; equality compares the ops.
        assert_eq!(v.clone(), PropValue::DrawList(Arc::from(ops.to_vec())));
        assert!(!v.has_tokens());
        assert_eq!(Prop::Draw.name(), "draw");
        let methods: Vec<_> = ops.iter().map(DrawOp::method).collect();
        assert_eq!(
            methods,
            ["rect", "circle", "fill", "line", "stroke", "text"]
        );
        assert_eq!(ops.iter().filter(|o| o.paints()).count(), 2);
        let mut sorted = methods.clone();
        sorted.sort_unstable();
        let mut all = DrawOp::METHODS.to_vec();
        all.sort_unstable();
        assert_eq!(sorted, all, "every variant's method is listed");
    }
}

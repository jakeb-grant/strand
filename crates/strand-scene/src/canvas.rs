//! (M4) What a `canvas`'s `draw: (c) => …` records: the renderer's own
//! paint vocabulary, in a canvas-2D-like state model. The VM records the
//! ops on the logic thread while running `draw` (and again when what it
//! read changes); render draws them with vello_cpu. They travel as
//! [`crate::PropValue::DrawList`] on [`crate::Prop::Draw`].
//!
//! Coordinates are logical pixels from the canvas box's top-left corner
//! (`c.width` and `c.height` come from layout facts). Shapes add to the
//! current path; [`DrawOp::Fill`] and [`DrawOp::Stroke`] paint it and
//! start a new one. (flex) A path is also drawn point by point:
//! [`DrawOp::MoveTo`] starts a subpath, [`DrawOp::LineTo`] and
//! [`DrawOp::Arc`] continue the open one (or start one at their first
//! point when none is open), and [`DrawOp::Close`] closes it; a fill may
//! take the even-odd rule ([`DrawOp::FillEvenOdd`]), so a shape drawn
//! inside another cuts a hole. The variants are the ones builtin.schema's
//! `Canvas` record offers today; more may come, so other crates match
//! with a wildcard arm.

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
    /// (flex) `c.move(x, y)`: starts a subpath at (`x`, `y`).
    MoveTo { x: f32, y: f32 },
    /// (flex) `c.to(x, y)`: a line from the current point to (`x`, `y`);
    /// with no subpath open, it starts one there.
    LineTo { x: f32, y: f32 },
    /// (flex) `c.arc(x, y, r, from, to)`: an arc of the circle centred
    /// at (`x`, `y`) with radius `r`, from angle `from` to angle `to`,
    /// degrees, 0 pointing right and growing clockwise (y grows down);
    /// it turns clockwise when `to > from`, at most one whole turn. A line
    /// joins the open subpath's current point to the arc's start; with
    /// none open, the arc starts a subpath.
    Arc {
        x: f32,
        y: f32,
        r: f32,
        from: f32,
        to: f32,
    },
    /// (flex) `c.close()`: a line back to the open subpath's start,
    /// closing it.
    Close,
    /// (flex) `c.fill(paint, evenodd: true)`: fills the current path by
    /// the even-odd rule (a region inside an even number of its subpaths
    /// is not filled), then clears it.
    FillEvenOdd(Paint),
    /// (flex) [`DrawOp::FillEvenOdd`] with a themed paint.
    FillThemedEvenOdd(PropValue),
}

impl DrawOp {
    /// Every canvas method an op records, in declaration order: the
    /// functions of builtin.schema's `Canvas` record (the catalogue test
    /// keeps the two equal).
    pub const METHODS: &'static [&'static str] = &[
        "line", "rect", "circle", "fill", "stroke", "text", "move", "to", "arc", "close",
    ];

    /// The canvas method that records this op, as builtin.schema names it.
    pub fn method(&self) -> &'static str {
        match self {
            DrawOp::Line { .. } => "line",
            DrawOp::Rect { .. } => "rect",
            DrawOp::Circle { .. } => "circle",
            DrawOp::Fill(_)
            | DrawOp::FillThemed(_)
            | DrawOp::FillEvenOdd(_)
            | DrawOp::FillThemedEvenOdd(_) => "fill",
            DrawOp::Stroke { .. } | DrawOp::StrokeThemed { .. } => "stroke",
            DrawOp::Text { .. } => "text",
            DrawOp::MoveTo { .. } => "move",
            DrawOp::LineTo { .. } => "to",
            DrawOp::Arc { .. } => "arc",
            DrawOp::Close => "close",
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
                | DrawOp::FillEvenOdd(_)
                | DrawOp::FillThemedEvenOdd(_)
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
            DrawOp::MoveTo { x: 5.0, y: 2.0 },
            DrawOp::Arc {
                x: 5.0,
                y: 2.0,
                r: 2.0,
                from: 0.0,
                to: 90.0,
            },
            DrawOp::LineTo { x: 1.0, y: 1.0 },
            DrawOp::Close,
            DrawOp::FillEvenOdd(Paint::Solid(Color::WHITE)),
        ]);
        let v = PropValue::DrawList(ops.clone());
        // Cloning shares the list; equality compares the ops.
        assert_eq!(v.clone(), PropValue::DrawList(Arc::from(ops.to_vec())));
        assert!(!v.has_tokens());
        assert_eq!(Prop::Draw.name(), "draw");
        let methods: Vec<_> = ops.iter().map(DrawOp::method).collect();
        assert_eq!(
            methods,
            [
                "rect", "circle", "fill", "line", "stroke", "text", "move", "arc", "to", "close",
                "fill"
            ]
        );
        assert_eq!(ops.iter().filter(|o| o.paints()).count(), 3);
        let mut sorted = methods.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let mut all = DrawOp::METHODS.to_vec();
        all.sort_unstable();
        assert_eq!(sorted, all, "every variant's method is listed");
    }
}

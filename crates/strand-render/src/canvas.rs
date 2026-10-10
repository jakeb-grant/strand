//! (M4) A `canvas`'s recorded draw list (`Prop::Draw`, what `draw: (c) =>
//! …` recorded; `strand_scene::canvas`) as display items: the same
//! fills the renderer paints boxes with, so the CPU rasteriser and the
//! GPU lowering draw a canvas like any node (design.md, "Canvas: the
//! same paint API the renderer uses").
//!
//! Coordinates are logical pixels from the box's top-left corner. Shapes
//! add to the current path; `fill` paints it (non-zero) and `stroke`
//! paints its outline (`kurbo::stroke`, round joins and caps), each then
//! starting a new path. `fill` with no path paints the whole box (the
//! grammar's `c.fill($accent)` before drawing on it). A themed paint is
//! resolved in the node's token scope, as a `bg` is. Everything is
//! clipped to the box. `text(t, x, y)` draws `t` in the node's font and
//! colour with its first baseline starting at (`x`, `y`): each text op is
//! one of the node's texts (`TextSpec::part` 1 + its index among the
//! node's text ops), shaped by the text worker like a `segmented`'s
//! labels, and drawn once its layout is back ([`CanvasText`]).

use strand_scene::canvas::DrawOp;
use strand_scene::{Paint, PropValue};
use vello_cpu::kurbo::{self, Affine, BezPath, Shape};

use crate::flatten::{FillShape, Item};

/// Curve flattening tolerance, physical pixels (as flatten's).
const TOLERANCE: f64 = 0.1;

/// Largest coordinate or size a canvas op may use, logical pixels:
/// non-finite and huge values from a script do not reach the rasteriser.
const MAX: f64 = 1e6;

fn sane(v: f32) -> Option<f64> {
    let v = f64::from(v);
    v.is_finite().then(|| v.clamp(-MAX, MAX))
}

/// Most text ops of one canvas drawn (a text's part is a `u8`, part 0
/// being a node's own text).
pub(crate) const MAX_TEXTS: usize = 254;

/// How a canvas's texts are drawn: `text(i, t, x, y)` gives the item of
/// its `i`-th text op (`t` with its baseline's start at physical (`x`,
/// `y`)) and the rectangle it can touch, or `None` until it is shaped.
pub(crate) type CanvasText<'a> =
    dyn FnMut(usize, &str, f64, f64) -> Option<(Item, kurbo::Rect)> + 'a;

/// The items drawing `ops` in `frame` (the box, physical pixels) at
/// `scale`, each with the physical rectangle it can touch; inside a clip
/// to the box when there is anything to draw.
/// `themed` resolves a paint that names tokens, `text` draws a text op.
pub(crate) fn items(
    ops: &[DrawOp],
    frame: kurbo::Rect,
    scale: f64,
    themed: &dyn Fn(&PropValue) -> Option<Paint>,
    text: &mut CanvasText<'_>,
) -> Vec<(Item, kurbo::Rect)> {
    let mut texts = 0;
    let to_phys = Affine::translate((frame.x0, frame.y0)) * Affine::scale(scale);
    let mut out = Vec::new();
    let mut path = BezPath::new();
    for op in ops {
        match op {
            DrawOp::Line { x1, y1, x2, y2 } => {
                if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                    (sane(*x1), sane(*y1), sane(*x2), sane(*y2))
                {
                    path.move_to((x1, y1));
                    path.line_to((x2, y2));
                }
            }
            DrawOp::Rect { x, y, w, h } => {
                if let (Some(x), Some(y), Some(w), Some(h)) =
                    (sane(*x), sane(*y), sane(*w), sane(*h))
                {
                    path.extend(kurbo::Rect::new(x, y, x + w, y + h).path_elements(TOLERANCE));
                }
            }
            DrawOp::Circle { x, y, r } => {
                if let (Some(x), Some(y), Some(r)) = (sane(*x), sane(*y), sane(*r))
                    && r > 0.0
                {
                    path.extend(kurbo::Circle::new((x, y), r).path_elements(TOLERANCE));
                }
            }
            DrawOp::Fill(_) | DrawOp::FillThemed(_) => {
                let p = std::mem::take(&mut path);
                let paint = match op {
                    DrawOp::FillThemed(v) => themed(v),
                    DrawOp::Fill(p) => Some(p.clone()),
                    _ => None,
                };
                let Some(paint) = paint else {
                    continue;
                };
                let shape = if p.is_empty() {
                    frame.to_path(TOLERANCE)
                } else {
                    to_phys * p
                };
                push_fill(&mut out, shape, &paint, frame);
            }
            DrawOp::Stroke { width, .. } | DrawOp::StrokeThemed { width, .. } => {
                let p = std::mem::take(&mut path);
                let paint = match op {
                    DrawOp::StrokeThemed { paint, .. } => themed(paint),
                    DrawOp::Stroke { paint, .. } => Some(paint.clone()),
                    _ => None,
                };
                let Some(paint) = paint else {
                    continue;
                };
                let Some(width) = sane(*width).filter(|w| *w > 0.0) else {
                    continue;
                };
                if p.is_empty() {
                    continue;
                }
                let style = kurbo::Stroke::new(width * scale)
                    .with_caps(kurbo::Cap::Round)
                    .with_join(kurbo::Join::Round);
                let outline = kurbo::stroke(
                    to_phys * p,
                    &style,
                    &kurbo::StrokeOpts::default(),
                    TOLERANCE,
                );
                push_fill(&mut out, outline, &paint, frame);
            }
            DrawOp::Text { text: t, x, y } => {
                let i = texts;
                texts += 1;
                if i >= MAX_TEXTS || t.is_empty() {
                    continue;
                }
                let (Some(x), Some(y)) = (sane(*x), sane(*y)) else {
                    continue;
                };
                let at = to_phys * kurbo::Point::new(x, y);
                if let Some((item, reach)) = text(i, t, at.x, at.y)
                    && let Some(reach) = Some(reach.intersect(frame))
                    && reach.width() > 0.0
                    && reach.height() > 0.0
                {
                    out.push((item, reach));
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        return out;
    }
    let mut clipped = Vec::with_capacity(out.len() + 2);
    clipped.push((Item::PushClip(frame.to_path(TOLERANCE)), frame));
    clipped.extend(out);
    clipped.push((Item::PopClip, frame));
    clipped
}

fn push_fill(out: &mut Vec<(Item, kurbo::Rect)>, path: BezPath, paint: &Paint, frame: kurbo::Rect) {
    let reach = path.bounding_box().intersect(frame);
    if reach.width() <= 0.0 || reach.height() <= 0.0 {
        return;
    }
    out.push((
        Item::Fill {
            shape: FillShape::Path(path),
            paint: paint.clone(),
            frame,
        },
        reach,
    ));
}

#[cfg(test)]
mod tests {
    use strand_scene::Color;

    use super::*;

    #[test]
    fn shapes_fill_and_stroke_in_the_box_at_the_scale() {
        let frame = kurbo::Rect::new(100.0, 50.0, 300.0, 150.0);
        let ops = [
            DrawOp::Rect {
                x: 10.0,
                y: 10.0,
                w: 20.0,
                h: 10.0,
            },
            DrawOp::Fill(Paint::Solid(Color::WHITE)),
            DrawOp::Line {
                x1: 0.0,
                y1: 0.0,
                x2: 1000.0,
                y2: 0.0,
            },
            DrawOp::Stroke {
                paint: Paint::Solid(Color::BLACK),
                width: 2.0,
            },
        ];
        let items = items(&ops, frame, 2.0, &|_| None, &mut |_, _, _, _| None);
        assert_eq!(items.len(), 4, "{items:?}");
        assert!(matches!(items[0].0, Item::PushClip(_)));
        assert!(matches!(items[3].0, Item::PopClip));
        let (Item::Fill { shape, .. }, reach) = &items[1] else {
            panic!("{:?}", items[1]);
        };
        let FillShape::Path(p) = shape else {
            panic!("a path");
        };
        assert_eq!(p.bounding_box(), kurbo::Rect::new(120.0, 70.0, 160.0, 90.0));
        assert_eq!(*reach, kurbo::Rect::new(120.0, 70.0, 160.0, 90.0));
        // The stroke is clipped to the box.
        let (_, reach) = &items[2];
        assert_eq!(reach.x1, frame.x1);
        assert!((reach.y0 - 48.0).abs() < 0.5 || reach.y0 == frame.y0);
    }

    #[test]
    fn nonsense_values_draw_nothing() {
        let frame = kurbo::Rect::new(0.0, 0.0, 10.0, 10.0);
        let ops = [
            DrawOp::Circle {
                x: f32::NAN,
                y: 1.0,
                r: 3.0,
            },
            DrawOp::Circle {
                x: 1.0,
                y: 1.0,
                r: -3.0,
            },
            DrawOp::Fill(Paint::Solid(Color::WHITE)),
            DrawOp::Rect {
                x: 1.0,
                y: 1.0,
                w: 2.0,
                h: 2.0,
            },
            DrawOp::Stroke {
                paint: Paint::Solid(Color::WHITE),
                width: f32::INFINITY,
            },
        ];
        // (The first fill paints the box: no shape was valid, so it had
        // no path.)
        let ops = &ops[3..];
        assert!(items(ops, frame, 1.0, &|_| None, &mut |_, _, _, _| None).is_empty());
    }

    #[test]
    fn a_fill_with_no_path_paints_the_box_and_themed_paints_resolve() {
        let frame = kurbo::Rect::new(0.0, 0.0, 10.0, 10.0);
        let accent = PropValue::Token(strand_scene::TokenExpr::path("accent"));
        let ops = [DrawOp::FillThemed(accent.clone())];
        let red = Paint::Solid(Color::rgb(1.0, 0.0, 0.0));
        let resolve = |v: &PropValue| (*v == accent).then(|| red.clone());
        let items = items(&ops, frame, 1.0, &resolve, &mut |_, _, _, _| None);
        let (Item::Fill { paint, .. }, reach) = &items[1] else {
            panic!("{items:?}");
        };
        assert_eq!(*paint, red);
        assert_eq!(*reach, frame);
        // Unresolved: nothing drawn.
        assert!(super::items(&ops, frame, 1.0, &|_| None, &mut |_, _, _, _| None).is_empty());
    }

    #[test]
    fn texts_are_drawn_in_order_at_their_baseline_in_the_box() {
        let frame = kurbo::Rect::new(100.0, 50.0, 300.0, 150.0);
        let text = |t: &str, x, y| DrawOp::Text {
            text: t.into(),
            x,
            y,
        };
        let ops = [
            text("a", 10.0, 20.0),
            DrawOp::Fill(Paint::Solid(Color::WHITE)),
            text("", 0.0, 0.0),
            text("b", f32::NAN, 0.0),
            text("c", 1000.0, 20.0),
            text("d", 4.0, 8.0),
        ];
        let mut asked = Vec::new();
        let items = items(&ops, frame, 2.0, &|_| None, &mut |i, t, x, y| {
            asked.push((i, t.to_string(), x, y));
            let r = kurbo::Rect::new(x, y - 10.0, x + 10.0, y);
            Some((Item::PopClip, r))
        });
        // Each op's index counts every text op; empty and nonsense ones
        // ask for nothing.
        assert_eq!(
            asked,
            vec![
                (0, "a".into(), 120.0, 90.0),
                (3, "c".into(), 2100.0, 90.0),
                (4, "d".into(), 108.0, 66.0),
            ]
        );
        // "a", the fill, "d" (outside the box, "c" draws nothing), clipped.
        assert_eq!(items.len(), 5, "{items:?}");
        assert!(matches!(items[2].0, Item::Fill { .. }));
        assert_eq!(items[1].1, kurbo::Rect::new(120.0, 80.0, 130.0, 90.0));
    }
}

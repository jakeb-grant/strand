//! (M4) A `canvas`'s recorded draw list (`Prop::Draw`, what `draw: (c) =>
//! …` recorded; `strand_scene::canvas`) as display items: the same
//! fills the renderer paints boxes with, so the CPU rasteriser and the
//! GPU lowering draw a canvas like any node (design.md, "Canvas: the
//! same paint API the renderer uses").
//!
//! Coordinates are logical pixels from the box's top-left corner. Shapes
//! add to the current path; `fill` paints it (non-zero, or even-odd when
//! asked) and `stroke` paints its outline (`kurbo::stroke`, round joins
//! and caps), each then starting a new path. (flex) `move`, `to`, `arc`
//! and `close` draw a path point by point: `to` and `arc` continue the
//! open subpath, or start one at their first point when none is open (at
//! the start, after `close` or a paint, and after a `rect` or `circle`,
//! which are closed); an `arc` is joined to the open subpath by a line to
//! its start, and turns at most one whole turn. `fill` with no path paints the whole box (the
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

/// The path being built: the shapes since the last paint, and whether
/// a subpath is open for `to`, `arc` and `close` to continue.
#[derive(Default)]
struct Path {
    path: BezPath,
    open: bool,
}

impl Path {
    /// Continues the open subpath with a line to `p`, or starts one at
    /// `p`.
    fn join(&mut self, p: kurbo::Point) {
        if self.open {
            self.path.line_to(p);
        } else {
            self.path.move_to(p);
            self.open = true;
        }
    }

    fn take(&mut self) -> BezPath {
        self.open = false;
        std::mem::take(&mut self.path)
    }
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
    let mut path = Path::default();
    for op in ops {
        match op {
            DrawOp::Line { x1, y1, x2, y2 } => {
                if let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                    (sane(*x1), sane(*y1), sane(*x2), sane(*y2))
                {
                    path.path.move_to((x1, y1));
                    path.path.line_to((x2, y2));
                    path.open = true;
                }
            }
            DrawOp::Rect { x, y, w, h } => {
                if let (Some(x), Some(y), Some(w), Some(h)) =
                    (sane(*x), sane(*y), sane(*w), sane(*h))
                {
                    path.path
                        .extend(kurbo::Rect::new(x, y, x + w, y + h).path_elements(TOLERANCE));
                    path.open = false;
                }
            }
            DrawOp::Circle { x, y, r } => {
                if let (Some(x), Some(y), Some(r)) = (sane(*x), sane(*y), sane(*r))
                    && r > 0.0
                {
                    path.path
                        .extend(kurbo::Circle::new((x, y), r).path_elements(TOLERANCE));
                    path.open = false;
                }
            }
            DrawOp::MoveTo { x, y } => {
                if let (Some(x), Some(y)) = (sane(*x), sane(*y)) {
                    path.path.move_to((x, y));
                    path.open = true;
                }
            }
            DrawOp::LineTo { x, y } => {
                if let (Some(x), Some(y)) = (sane(*x), sane(*y)) {
                    path.join(kurbo::Point::new(x, y));
                }
            }
            DrawOp::Arc { x, y, r, from, to } => {
                if let (Some(x), Some(y), Some(r), Some(from), Some(to)) =
                    (sane(*x), sane(*y), sane(*r), sane(*from), sane(*to))
                    && r >= 0.0
                {
                    let start = from.to_radians();
                    let sweep = (to - from).clamp(-360.0, 360.0).to_radians();
                    let arc = kurbo::Arc::new((x, y), (r, r), start, sweep, 0.0);
                    path.join(kurbo::Point::new(x + r * start.cos(), y + r * start.sin()));
                    if r > 0.0 && sweep != 0.0 {
                        path.path.extend(arc.append_iter(TOLERANCE));
                    }
                }
            }
            DrawOp::Close => {
                if path.open {
                    path.path.close_path();
                    path.open = false;
                }
            }
            DrawOp::Fill(_)
            | DrawOp::FillThemed(_)
            | DrawOp::FillEvenOdd(_)
            | DrawOp::FillThemedEvenOdd(_) => {
                let p = path.take();
                let paint = match op {
                    DrawOp::FillThemed(v) | DrawOp::FillThemedEvenOdd(v) => themed(v),
                    DrawOp::Fill(p) | DrawOp::FillEvenOdd(p) => Some(p.clone()),
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
                let even_odd = matches!(op, DrawOp::FillEvenOdd(_) | DrawOp::FillThemedEvenOdd(_));
                push_fill(&mut out, shape, even_odd, &paint, frame);
            }
            DrawOp::Stroke { width, .. } | DrawOp::StrokeThemed { width, .. } => {
                let p = path.take();
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
                push_fill(&mut out, outline, false, &paint, frame);
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

/// A fill of `path` (`even_odd`: by the even-odd rule, else non-zero).
fn push_fill(
    out: &mut Vec<(Item, kurbo::Rect)>,
    path: BezPath,
    even_odd: bool,
    paint: &Paint,
    frame: kurbo::Rect,
) {
    let reach = path.bounding_box().intersect(frame);
    if reach.width() <= 0.0 || reach.height() <= 0.0 {
        return;
    }
    out.push((
        Item::Fill {
            shape: if even_odd {
                FillShape::EvenOdd(path)
            } else {
                FillShape::Path(path)
            },
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

    /// (flex) `to` and `arc` continue the open subpath or start one;
    /// `close`, a paint, a `rect` and a `circle` leave none open; an
    /// even-odd fill keeps its rule.
    #[test]
    fn paths_continue_the_open_subpath_or_start_one() {
        use kurbo::PathEl;
        let frame = kurbo::Rect::new(0.0, 0.0, 100.0, 100.0);
        let white = || Paint::Solid(Color::WHITE);
        let path_of = |ops: &[DrawOp]| -> (Vec<PathEl>, bool) {
            let items = items(ops, frame, 1.0, &|_| None, &mut |_, _, _, _| None);
            match &items[1].0 {
                Item::Fill {
                    shape: FillShape::Path(p),
                    ..
                } => (p.elements().to_vec(), false),
                Item::Fill {
                    shape: FillShape::EvenOdd(p),
                    ..
                } => (p.elements().to_vec(), true),
                other => panic!("{other:?}"),
            }
        };
        let pt = kurbo::Point::new;
        // `to` with nothing open starts the path; `close` closes it; the
        // next `to` starts another.
        let (els, even_odd) = path_of(&[
            DrawOp::LineTo { x: 10.0, y: 10.0 },
            DrawOp::LineTo { x: 20.0, y: 10.0 },
            DrawOp::Close,
            DrawOp::LineTo { x: 30.0, y: 30.0 },
            DrawOp::LineTo { x: 40.0, y: 30.0 },
            DrawOp::FillEvenOdd(white()),
        ]);
        assert!(even_odd);
        assert_eq!(
            els,
            [
                PathEl::MoveTo(pt(10.0, 10.0)),
                PathEl::LineTo(pt(20.0, 10.0)),
                PathEl::ClosePath,
                PathEl::MoveTo(pt(30.0, 30.0)),
                PathEl::LineTo(pt(40.0, 30.0)),
            ]
        );
        // An arc joins the open subpath with a line to its start (at 0deg,
        // the right of its circle) and ends a quarter turn clockwise,
        // below its centre.
        let (els, even_odd) = path_of(&[
            DrawOp::MoveTo { x: 50.0, y: 50.0 },
            DrawOp::Arc {
                x: 50.0,
                y: 50.0,
                r: 10.0,
                from: 0.0,
                to: 90.0,
            },
            DrawOp::Fill(white()),
        ]);
        assert!(!even_odd);
        assert_eq!(els[0], PathEl::MoveTo(pt(50.0, 50.0)));
        assert_eq!(els[1], PathEl::LineTo(pt(60.0, 50.0)));
        let end = els.last().and_then(|e| e.end_point()).unwrap();
        assert!(
            (end.x - 50.0).abs() < 1e-6 && (end.y - 60.0).abs() < 1e-6,
            "{end:?}"
        );
        // After a circle (closed) an arc starts a subpath of its own.
        let (els, _) = path_of(&[
            DrawOp::Circle {
                x: 20.0,
                y: 20.0,
                r: 5.0,
            },
            DrawOp::Arc {
                x: 50.0,
                y: 50.0,
                r: 10.0,
                from: 180.0,
                to: 0.0,
            },
            DrawOp::Fill(white()),
        ]);
        let moves: Vec<_> = els
            .iter()
            .filter_map(|e| match e {
                PathEl::MoveTo(p) => Some(*p),
                _ => None,
            })
            .collect();
        assert_eq!(moves.len(), 2, "{els:?}");
        assert_eq!(moves[1], pt(40.0, 50.0));
        // A sweep of more than a turn is one turn.
        let (els, _) = path_of(&[
            DrawOp::Arc {
                x: 50.0,
                y: 50.0,
                r: 10.0,
                from: 0.0,
                to: 7200.0,
            },
            DrawOp::Fill(white()),
        ]);
        let end = els.last().and_then(|e| e.end_point()).unwrap();
        assert!(
            (end.x - 60.0).abs() < 1e-6 && (end.y - 50.0).abs() < 1e-6,
            "{end:?}"
        );
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

//! (M4) Goo merge (design.md, "Shape and geometry": `merge 10 { … }`
//! around siblings, "children melt together", "Ok, via marching
//! squares"; "CPU raster nodes for … goo fields").
//!
//! A `merge d` node draws, under its children, the goo between them:
//! the smooth union of their boxes (each a rounded rectangle, as drawn,
//! paint offsets and springs included). Boxes within `d` logical pixels
//! of each other grow a bridge that thickens as they near and fills in
//! as they meet. Boxes further apart stay as they are. The goo takes
//! each child's `bg`, blended where two meet. The children then draw as
//! usual on top, so their content stays sharp and only the bridges are
//! new.
//!
//! The field is evaluated per pixel: a signed distance to each box,
//! combined by a polynomial smooth minimum of width `2d` (so the gap
//! between two boxes `d` apart just closes), with an antialiased edge
//! at zero. On the GPU this would be marching squares
//! over the same field. It is a CPU raster node cached by its children's
//! boxes, so it is redrawn only when one of them moves.

use std::hash::{DefaultHasher, Hash, Hasher};

use strand_scene::{Color, TimeContext};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo;

use super::builtin::Canvas;
use crate::clock::Rate;
use crate::offscreen::RasterSource;

/// Largest merge distance, logical pixels.
pub(crate) const MAX_DISTANCE: f32 = 200.0;

/// One child's box in the goo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Blob {
    /// Physical pixels, relative to the merge node's box.
    pub rect: kurbo::Rect,
    pub radius: f64,
    pub color: Color,
}

/// A merge node's goo: its children's boxes and the merge distance
/// (physical pixels).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Goo {
    pub blobs: Vec<Blob>,
    pub distance: f64,
}

/// Signed distance from `p` to the rounded rectangle `r` with corner
/// radius `radius` (negative inside).
fn rounded_box(p: kurbo::Point, r: kurbo::Rect, radius: f64) -> f64 {
    let c = r.center();
    let half = (r.width() / 2.0, r.height() / 2.0);
    let radius = radius.clamp(0.0, half.0.min(half.1));
    let q = (
        (p.x - c.x).abs() - (half.0 - radius),
        (p.y - c.y).abs() - (half.1 - radius),
    );
    let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
    outside + q.0.max(q.1).min(0.0) - radius
}

/// The polynomial smooth minimum of `a` and `b` over width `k`.
fn smooth_min(a: f64, b: f64, k: f64) -> f64 {
    if k <= 0.0 {
        return a.min(b);
    }
    let h = (k - (a - b).abs()).max(0.0) / k;
    a.min(b) - h * h * k * 0.25
}

impl Goo {
    /// What the pixmap depends on, for its cache key.
    pub(crate) fn config(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.distance.to_bits().hash(&mut h);
        for b in &self.blobs {
            for v in [b.rect.x0, b.rect.y0, b.rect.x1, b.rect.y1, b.radius] {
                v.to_bits().hash(&mut h);
            }
            [b.color.r, b.color.g, b.color.b, b.color.a]
                .map(f32::to_bits)
                .hash(&mut h);
        }
        h.finish()
    }

    /// The field's distance and colour at `p`.
    fn at(&self, p: kurbo::Point) -> (f64, Color) {
        // Twice the distance: two boxes `distance` apart meet in the
        // middle (the smooth minimum dips by at most a quarter of it).
        let k = self.distance * 2.0;
        let mut d = f64::INFINITY;
        let mut nearest = f64::INFINITY;
        let ds: Vec<f64> = self
            .blobs
            .iter()
            .map(|b| rounded_box(p, b.rect, b.radius))
            .collect();
        for &di in &ds {
            d = if d.is_infinite() {
                di
            } else {
                smooth_min(d, di, k)
            };
            nearest = nearest.min(di);
        }
        // Each box's colour weighs by how near it is to the nearest.
        let mut sum = [0.0f32; 4];
        let mut total = 0.0f32;
        for (b, di) in self.blobs.iter().zip(&ds) {
            let w = ((k - (di - nearest)).max(0.0) / k.max(1e-6)).powi(2) as f32 + 1e-6;
            let w = if (*di - nearest).abs() < 1e-9 {
                w.max(1e-3)
            } else {
                w
            };
            sum[0] += b.color.r * w;
            sum[1] += b.color.g * w;
            sum[2] += b.color.b * w;
            sum[3] += b.color.a * w;
            total += w;
        }
        let c = if total > 0.0 {
            Color::new(
                sum[0] / total,
                sum[1] / total,
                sum[2] / total,
                sum[3] / total,
            )
        } else {
            Color::TRANSPARENT
        };
        (d, c)
    }
}

impl RasterSource for Goo {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, _scale: f32, _time: TimeContext) {
        if self.blobs.is_empty() {
            return;
        }
        // Only where the goo can reach.
        let mut reach = self.blobs[0].rect;
        for b in &self.blobs[1..] {
            reach = reach.union(b.rect);
        }
        let reach = reach.inflate(1.0, 1.0);
        let (x0, y0) = (
            reach.x0.floor().max(0.0) as u32,
            reach.y0.floor().max(0.0) as u32,
        );
        let (x1, y1) = (
            (reach.x1.ceil().max(0.0) as u32).min(w),
            (reach.y1.ceil().max(0.0) as u32).min(h),
        );
        let mut canvas = Canvas { px: pixels, w, h };
        for y in y0..y1 {
            for x in x0..x1 {
                let (d, c) = self.at(kurbo::Point::new(x as f64 + 0.5, y as f64 + 0.5));
                let alpha = (0.5 - d).clamp(0.0, 1.0) as f32;
                if alpha > 0.0 {
                    canvas.over(x as i32, y as i32, c, alpha);
                }
            }
        }
    }

    fn rate(&self) -> Rate {
        Rate::Refresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(x0: f64, x1: f64) -> Blob {
        Blob {
            rect: kurbo::Rect::new(x0, 10.0, x1, 30.0),
            radius: 10.0,
            color: Color::WHITE,
        }
    }

    #[test]
    fn near_boxes_bridge_and_far_ones_do_not() {
        // Two discs 6 px apart with a merge distance of 10: the gap's
        // middle is inside the goo (it closes below 10).
        let near = Goo {
            blobs: vec![blob(0.0, 20.0), blob(26.0, 46.0)],
            distance: 10.0,
        };
        assert!(near.at(kurbo::Point::new(23.0, 20.0)).0 < 0.0);
        // 30 px apart: the middle stays outside.
        let far = Goo {
            blobs: vec![blob(0.0, 20.0), blob(50.0, 70.0)],
            distance: 10.0,
        };
        assert!(far.at(kurbo::Point::new(35.0, 20.0)).0 > 0.0);
        // A lone box is its own outline.
        let one = Goo {
            blobs: vec![blob(0.0, 20.0)],
            distance: 10.0,
        };
        assert!(one.at(kurbo::Point::new(10.0, 20.0)).0 < 0.0);
        assert!((one.at(kurbo::Point::new(20.5, 20.0)).0 - 0.5).abs() < 1e-6);
        assert_ne!(near.config(), far.config());
    }
}

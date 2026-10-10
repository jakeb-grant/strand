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
//! The field is a signed distance to each box, combined by a polynomial
//! smooth minimum of width `2d` (so the gap between two boxes `d` apart
//! just closes). It is contoured by marching squares: sampled, with its
//! colour, at the corners of a [`GRID`] px grid, its zero crossing on each
//! cell edge found by linear interpolation, and each cell's segments (two
//! for a saddle, joined or split by the cell's centre) giving the outline
//! there. Each pixel is covered by its distance to its cell's outline
//! (antialiased over a pixel) and coloured by the corners' colours,
//! interpolated. A cell with every corner on one side is wholly in or
//! out. The field is evaluated once per grid corner rather than per
//! pixel. It is a CPU raster node cached by its children's boxes, so it
//! is redrawn only when one of them moves.

use std::hash::{DefaultHasher, Hash, Hasher};

use strand_scene::{Color, TimeContext};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo;

use super::builtin::Canvas;
use crate::clock::Rate;
use crate::offscreen::RasterSource;

/// The marching-squares grid, physical pixels.
pub(crate) const GRID: usize = 2;

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

/// One marching-squares cell's outline: its segments, each a line with
/// the inside on its negative side, and how they combine.
#[derive(Debug)]
enum Outline {
    /// Every corner inside.
    Full,
    /// One segment.
    One(Line),
    /// A saddle whose inside is joined through the centre (inside both
    /// lines) or split into two corners (inside either).
    Joined(Line, Line),
    Split(Line, Line),
}

/// A line through `a` with the unit `normal` pointing outside.
#[derive(Clone, Copy, Debug)]
struct Line {
    a: kurbo::Point,
    normal: kurbo::Vec2,
}

impl Line {
    /// The line through `a` and `b`, oriented so that `corner` (field
    /// value `d`) is on the side its sign says. `None` if `a` and `b`
    /// meet.
    fn through(a: kurbo::Point, b: kurbo::Point, corner: kurbo::Point, d: f64) -> Option<Line> {
        let t = b - a;
        let len = t.hypot();
        if len < 1e-9 {
            return None;
        }
        let mut normal = kurbo::Vec2::new(t.y, -t.x) / len;
        if (normal.dot(corner - a) < 0.0) != (d < 0.0) {
            normal = -normal;
        }
        Some(Line { a, normal })
    }

    fn distance(&self, p: kurbo::Point) -> f64 {
        self.normal.dot(p - self.a)
    }
}

impl Outline {
    /// The outline of a cell at `origin`, `size` px wide, with the field
    /// `d` at its corners (top left, top right, bottom right, bottom
    /// left). `None` with every corner outside.
    fn of(d: [f64; 4], origin: kurbo::Point, size: f64) -> Option<Outline> {
        let inside = d.map(|v| v < 0.0);
        if inside.iter().all(|&i| !i) {
            return None;
        }
        if inside.iter().all(|&i| i) {
            return Some(Outline::Full);
        }
        let pos = [
            origin,
            origin + (size, 0.0),
            origin + (size, size),
            origin + (0.0, size),
        ];
        // The zero crossing on edge `e` (from corner `e` to the next).
        let cross = |e: usize| {
            let (a, b) = (e, (e + 1) % 4);
            if inside[a] == inside[b] {
                return None;
            }
            let t = (d[a] / (d[a] - d[b])).clamp(0.0, 1.0);
            Some(pos[a].lerp(pos[b], t))
        };
        let c = [cross(0), cross(1), cross(2), cross(3)];
        // A corner sharing edges `e - 1` and `e` is cut off by the
        // segment between their crossings.
        let cut = |k: usize| -> Option<Line> {
            let (p, q) = (c[(k + 3) % 4]?, c[k]?);
            Line::through(p, q, pos[k], d[k])
        };
        let found: Vec<usize> = (0..4).filter(|&e| c[e].is_some()).collect();
        if found.len() == 2 {
            let (p, q) = (c[found[0]]?, c[found[1]]?);
            // Orient by the corner furthest from the outline.
            let k = (0..4)
                .max_by(|&a, &b| d[a].abs().total_cmp(&d[b].abs()))
                .unwrap_or(0);
            return match Line::through(p, q, pos[k], d[k]) {
                Some(l) => Some(Outline::One(l)),
                // The outline touches a corner only: decided by the rest.
                None => (inside.iter().filter(|&&i| i).count() > 2).then_some(Outline::Full),
            };
        }
        // A saddle: the centre decides whether the inside corners join.
        let centre = d.iter().sum::<f64>() / 4.0;
        let (k0, k1) = if (centre < 0.0) == inside[0] {
            // Corners 1 and 3 are cut off.
            (1, 3)
        } else {
            (0, 2)
        };
        let (l0, l1) = (cut(k0)?, cut(k1)?);
        Some(if centre < 0.0 {
            Outline::Joined(l0, l1)
        } else {
            Outline::Split(l0, l1)
        })
    }

    /// Signed distance from `p` to the outline (negative inside).
    fn distance(&self, p: kurbo::Point) -> f64 {
        match self {
            Outline::Full => f64::NEG_INFINITY,
            Outline::One(l) => l.distance(p),
            Outline::Joined(a, b) => a.distance(p).max(b.distance(p)),
            Outline::Split(a, b) => a.distance(p).min(b.distance(p)),
        }
    }
}

/// `c` (top left, top right, bottom right, bottom left) interpolated at
/// `(u, v)` across the cell.
fn bilinear(c: [Color; 4], u: f32, v: f32) -> Color {
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let ch =
        |f: fn(&Color) -> f32| lerp(lerp(f(&c[0]), f(&c[1]), u), lerp(f(&c[3]), f(&c[2]), u), v);
    Color::new(ch(|c| c.r), ch(|c| c.g), ch(|c| c.b), ch(|c| c.a))
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
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        // The field at the grid's corners over the reach (a corner past
        // its last pixel included).
        let g = GRID;
        let cols = (x1 - x0) as usize / g + 2;
        let rows = (y1 - y0) as usize / g + 2;
        let corners: Vec<(f64, Color)> = (0..rows)
            .flat_map(|j| (0..cols).map(move |i| (i, j)))
            .map(|(i, j)| {
                self.at(kurbo::Point::new(
                    x0 as f64 + (i * g) as f64,
                    y0 as f64 + (j * g) as f64,
                ))
            })
            .collect();
        let mut canvas = Canvas { px: pixels, w, h };
        for j in 0..rows - 1 {
            for i in 0..cols - 1 {
                let corner = |di: usize, dj: usize| corners[(j + dj) * cols + i + di];
                // Top left, top right, bottom right, bottom left.
                let cell = [corner(0, 0), corner(1, 0), corner(1, 1), corner(0, 1)];
                let origin =
                    kurbo::Point::new(x0 as f64 + (i * g) as f64, y0 as f64 + (j * g) as f64);
                let Some(outline) = Outline::of(cell.map(|c| c.0), origin, g as f64) else {
                    continue;
                };
                for py in 0..g {
                    for px in 0..g {
                        let (x, y) = (origin.x as i64 + px as i64, origin.y as i64 + py as i64);
                        if x >= x1 as i64 || y >= y1 as i64 {
                            continue;
                        }
                        let p = kurbo::Point::new(x as f64 + 0.5, y as f64 + 0.5);
                        let alpha = (0.5 - outline.distance(p)).clamp(0.0, 1.0) as f32;
                        if alpha > 0.0 {
                            let (u, v) = ((p.x - origin.x) / g as f64, (p.y - origin.y) / g as f64);
                            let c = bilinear(cell.map(|c| c.1), u as f32, v as f32);
                            canvas.over(x as i32, y as i32, c, alpha);
                        }
                    }
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

    /// The marching-squares drawing follows the field: every pixel more
    /// than a pixel inside is covered, every one more than a pixel
    /// outside is not, a bridge included.
    #[test]
    fn marching_squares_follow_the_field() {
        let goo = Goo {
            blobs: vec![blob(0.0, 20.0), blob(26.0, 46.0)],
            distance: 10.0,
        };
        let (w, h) = (50u32, 40u32);
        let mut px = vec![PremulRgba8::from_u32(0); (w * h) as usize];
        goo.draw(&mut px, w, h, 1.0, TimeContext::default());
        let mut edge = 0;
        for y in 0..h {
            for x in 0..w {
                let p = kurbo::Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let d = goo.at(p).0;
                let a = px[(y * w + x) as usize].a;
                if d < -1.0 {
                    assert_eq!(a, 255, "({x}, {y}) inside at {d}");
                } else if d > 1.0 {
                    assert_eq!(a, 0, "({x}, {y}) outside at {d}");
                } else if a > 0 && a < 255 {
                    edge += 1;
                }
            }
        }
        assert!(edge > 0, "antialiased");
        assert_eq!(px[(20 * w + 23) as usize].a, 255, "the bridge");
    }
}

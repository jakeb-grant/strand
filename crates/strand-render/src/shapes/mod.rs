//! (M4) The shape library (design.md, "Shape and geometry"): `shape:
//! cookie` draws a node's box as one of builtin.schema's `enum Shape`
//! (Material 3 Expressive's cookie, clover, burst and friends, the
//! regular polygons, a heart, and the plain `rect`, `circle` and `pill`),
//! and a change morphs by spring ([`morph`]). `mask: shape(cookie)` masks
//! a subtree with the same outlines.
//!
//! Every shape is an outline of [`SAMPLES`] points at fixed directions
//! from the box's centre, in normalised box coordinates (−1..1 on each
//! axis), so two shapes morph point by point: the outline at progress `p`
//! is the points lerped (a bouncy spring overshoots past the target and
//! back). A shape at rest draws its exact path (a rect, an ellipse, a
//! stadium) or a smooth closed curve through its points.

pub(crate) mod morph;
pub(crate) mod stroke;

use std::f64::consts::{PI, TAU};

use vello_cpu::kurbo::{self, BezPath, Ellipse, RoundedRect, Shape as _};

/// Points per outline: a multiple of 3, 4, 5, 6, 8 and 12, so the
/// polygons' corners and the lobes of most shapes fall on samples.
pub(crate) const SAMPLES: usize = 120;

/// Curve flattening tolerance, physical pixels.
const TOLERANCE: f64 = 0.1;

/// builtin.schema's `enum Shape`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Shape {
    Rect,
    Circle,
    Pill,
    Cookie,
    Clover,
    Burst,
    Flower,
    Gem,
    Sunny,
    Triangle,
    Pentagon,
    Hexagon,
    Heart,
}

impl Shape {
    #[cfg(test)]
    pub(crate) const ALL: [Shape; 13] = [
        Shape::Rect,
        Shape::Circle,
        Shape::Pill,
        Shape::Cookie,
        Shape::Clover,
        Shape::Burst,
        Shape::Flower,
        Shape::Gem,
        Shape::Sunny,
        Shape::Triangle,
        Shape::Pentagon,
        Shape::Hexagon,
        Shape::Heart,
    ];

    pub(crate) fn from_name(name: &str) -> Option<Shape> {
        Some(match name {
            "rect" => Shape::Rect,
            "circle" => Shape::Circle,
            "pill" => Shape::Pill,
            "cookie" => Shape::Cookie,
            "clover" => Shape::Clover,
            "burst" => Shape::Burst,
            "flower" => Shape::Flower,
            "gem" => Shape::Gem,
            "sunny" => Shape::Sunny,
            "triangle" => Shape::Triangle,
            "pentagon" => Shape::Pentagon,
            "hexagon" => Shape::Hexagon,
            "heart" => Shape::Heart,
            _ => return None,
        })
    }

    /// The radius of a corner to round hit testing and shadows with, for
    /// a box of `w × h`: the shapes inscribed in their box are hit as a
    /// disc (an ellipse's inscribed circle), a rect as its box.
    pub(crate) fn corner(self, w: f64, h: f64) -> f64 {
        match self {
            Shape::Rect => 0.0,
            _ => w.min(h) / 2.0,
        }
    }
}

/// The direction of sample `i`: clockwise from 12 o'clock, as a unit
/// vector in screen coordinates (y down).
fn direction(i: usize) -> (f64, f64) {
    let th = TAU * i as f64 / SAMPLES as f64;
    (th.sin(), -th.cos())
}

/// A regular `n`-gon with a vertex at 12 o'clock, inscribed in the unit
/// circle: its radius in direction `th`.
fn polygon(n: f64, th: f64) -> f64 {
    let seg = TAU / n;
    let a = th.rem_euclid(seg) - seg / 2.0;
    (PI / n).cos() / a.cos()
}

/// The outline of `shape` in normalised box coordinates, for a box of
/// aspect `aspect` (width / height; only a pill depends on it).
pub(crate) fn outline(shape: Shape, aspect: f64) -> Vec<[f32; 2]> {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    let points: Vec<(f64, f64)> = (0..SAMPLES)
        .map(|i| {
            let (dx, dy) = direction(i);
            let th = TAU * i as f64 / SAMPLES as f64;
            match shape {
                Shape::Rect => {
                    let k = 1.0 / dx.abs().max(dy.abs());
                    (dx * k, dy * k)
                }
                Shape::Pill => pill_point(dx, dy, aspect),
                // Rays from a point inside the heart, 0.12 above the
                // middle of its height (it spans −1.24 to 1, y down).
                Shape::Heart => {
                    let (cx, cy) = (0.0, -0.12);
                    let r = heart_from(cx, cy, dx, dy);
                    (cx + dx * r, cy + dy * r)
                }
                _ => {
                    let r = radial(shape, th);
                    (dx * r, dy * r)
                }
            }
        })
        .collect();
    let fit = !matches!(shape, Shape::Rect | Shape::Pill | Shape::Circle);
    let (cx, cy, k) = if fit {
        fit_box(&points)
    } else {
        (0.0, 0.0, 1.0)
    };
    points
        .into_iter()
        .map(|(x, y)| [((x - cx) * k) as f32, ((y - cy) * k) as f32])
        .collect()
}

/// The centre of `points`' bounding box and the uniform scale that makes
/// its larger half-extent 1: a shape fills its box on one axis and keeps
/// its proportions (a clover's diagonal leaves reach the box's edges).
fn fit_box(points: &[(f64, f64)]) -> (f64, f64, f64) {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in points {
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    let half = ((x1 - x0) / 2.0).max((y1 - y0) / 2.0);
    if !(half.is_finite() && half > 0.0) {
        return (0.0, 0.0, 1.0);
    }
    ((x0 + x1) / 2.0, (y0 + y1) / 2.0, 1.0 / half)
}

/// The edge of the implicit heart `(x² + y² − 1)³ − x²y³ = 0` (y up)
/// along `(dx, dy)` from the point `(cx, cy)` of its own coordinates
/// (screen, y down), as a distance, by bisection. The heart reaches about
/// 1.14 sideways, 1 down and 1.25 up.
fn heart_from(cx: f64, cy: f64, dx: f64, dy: f64) -> f64 {
    let inside = |x: f64, y: f64| {
        let y = -y;
        let q = x * x + y * y - 1.0;
        q * q * q - x * x * y * y * y < 0.0
    };
    let (mut lo, mut hi) = (0.0, 2.6);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if inside(cx + dx * mid, cy + dy * mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The radius, in the unit circle, of the shapes drawn round their
/// centre, in direction `th` (radians clockwise from 12 o'clock).
fn radial(shape: Shape, th: f64) -> f64 {
    match shape {
        Shape::Circle => 1.0,
        // Nine soft scallops.
        Shape::Cookie => 0.9 + 0.1 * (9.0 * th).cos(),
        // Four leaves on the diagonals, pinched between them.
        Shape::Clover => 0.5 + 0.5 * (2.0 * (th - PI / 4.0)).cos().abs().sqrt(),
        // Twelve soft points.
        Shape::Burst => 0.72 + 0.28 * ((1.0 + (12.0 * th).cos()) / 2.0).powf(1.5),
        // Eight round petals.
        Shape::Flower => 0.62 + 0.38 * (4.0 * th).cos().abs().sqrt(),
        // Eight gentle rays.
        Shape::Sunny => 0.88 + 0.12 * (8.0 * th).cos(),
        // A diamond with well-rounded corners.
        Shape::Gem => 0.65 * polygon(4.0, th) + 0.35,
        Shape::Triangle => 0.92 * polygon(3.0, th) + 0.08,
        Shape::Pentagon => 0.94 * polygon(5.0, th) + 0.06,
        Shape::Hexagon => 0.95 * polygon(6.0, th) + 0.05,
        Shape::Rect | Shape::Pill | Shape::Heart => 1.0,
    }
}

/// Where a ray from the centre along `(dx, dy)` (normalised) leaves a
/// stadium filling a box of aspect `aspect`, normalised.
fn pill_point(dx: f64, dy: f64, aspect: f64) -> (f64, f64) {
    // In box units with height 2: half-width `aspect`, half-height 1.
    let (hw, hh) = (aspect, 1.0);
    let r = hw.min(hh);
    let (ex, ey) = (dx * hw, dy * hh);
    let len = (ex * ex + ey * ey).sqrt();
    if len == 0.0 {
        return (0.0, 0.0);
    }
    let (ux, uy) = (ex / len, ey / len);
    // Distance to the stadium: the straight sides, else the end disc.
    let (cx, cy) = (hw - r, hh - r);
    let mut t = f64::INFINITY;
    if uy.abs() > 1e-12 {
        let ts = hh / uy.abs();
        if (ux * ts).abs() <= cx + 1e-9 {
            t = ts;
        }
    }
    if ux.abs() > 1e-12 {
        let ts = hw / ux.abs();
        if (uy * ts).abs() <= cy + 1e-9 {
            t = t.min(ts);
        }
    }
    if !t.is_finite() {
        // The rounded end: |p − c| = r with c the end disc's centre.
        let (ccx, ccy) = (cx * ux.signum(), cy * uy.signum());
        let b = ux * ccx + uy * ccy;
        let c = ccx * ccx + ccy * ccy - r * r;
        t = b + (b * b - c).max(0.0).sqrt();
    }
    ((ux * t) / hw, (uy * t) / hh)
}

/// What a shaped node draws: a shape at rest, or a morph's points.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outline {
    Shape(Shape),
    Points(Vec<[f32; 2]>),
}

impl Outline {
    /// The radius hit testing and shadows round the box with.
    pub(crate) fn corner(&self, w: f64, h: f64) -> f64 {
        match self {
            Outline::Shape(s) => s.corner(w, h),
            Outline::Points(_) => w.min(h) / 2.0,
        }
    }
}

/// The path of `outline` filling `frame` (physical pixels).
pub(crate) fn path(outline: &Outline, frame: kurbo::Rect) -> BezPath {
    match outline {
        Outline::Shape(Shape::Rect) => frame.to_path(TOLERANCE),
        Outline::Shape(Shape::Circle) => Ellipse::from_rect(frame).to_path(TOLERANCE),
        Outline::Shape(Shape::Pill) => {
            let r = frame.width().min(frame.height()) / 2.0;
            RoundedRect::from_rect(frame, r).to_path(TOLERANCE)
        }
        Outline::Shape(s) => smooth(&outline_points(*s, frame), frame),
        Outline::Points(p) => smooth(p, frame),
    }
}

fn outline_points(s: Shape, frame: kurbo::Rect) -> Vec<[f32; 2]> {
    outline(s, aspect(frame))
}

/// A box's width over its height (1 when empty).
pub(crate) fn aspect(frame: kurbo::Rect) -> f64 {
    if frame.height() > 0.0 {
        frame.width() / frame.height()
    } else {
        1.0
    }
}

/// Normalised point `p` in `frame`.
fn place(p: [f32; 2], frame: kurbo::Rect) -> kurbo::Point {
    let c = frame.center();
    kurbo::Point::new(
        c.x + p[0] as f64 * frame.width() / 2.0,
        c.y + p[1] as f64 * frame.height() / 2.0,
    )
}

/// A closed Catmull-Rom curve through `points` placed in `frame`.
fn smooth(points: &[[f32; 2]], frame: kurbo::Rect) -> BezPath {
    let n = points.len();
    let mut path = BezPath::new();
    if n < 3 {
        return path;
    }
    let p = |i: usize| place(points[i % n], frame);
    path.move_to(p(0));
    for i in 0..n {
        let (p0, p1, p2, p3) = (p(i + n - 1), p(i), p(i + 1), p(i + 2));
        let c1 = p1 + (p2 - p0) / 6.0;
        let c2 = p2 - (p3 - p1) / 6.0;
        path.curve_to(c1, c2, p2);
    }
    path.close_path();
    path
}

/// A shape's outline as a polygon in physical pixels, for masks: its
/// coverage of a pixel centre, antialiased over a pixel.
#[derive(Clone, Debug)]
pub(crate) struct Polygon {
    points: Vec<kurbo::Point>,
}

impl Polygon {
    pub(crate) fn new(o: &Outline, frame: kurbo::Rect) -> Polygon {
        let pts = match o {
            Outline::Shape(s) => outline(*s, aspect(frame)),
            Outline::Points(p) => p.clone(),
        };
        Polygon {
            points: pts.into_iter().map(|p| place(p, frame)).collect(),
        }
    }

    /// How much of a pixel centred at `p` the shape covers: 1 inside, 0
    /// outside, a ramp one pixel wide across the edge.
    pub(crate) fn coverage(&self, p: kurbo::Point) -> f64 {
        let n = self.points.len();
        if n < 3 {
            return 0.0;
        }
        let mut inside = false;
        let mut d2 = f64::INFINITY;
        for i in 0..n {
            let a = self.points[i];
            let b = self.points[(i + 1) % n];
            if (a.y > p.y) != (b.y > p.y) {
                let x = a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x);
                if p.x < x {
                    inside = !inside;
                }
            }
            let ab = b - a;
            let len2 = ab.hypot2();
            let t = if len2 > 0.0 {
                ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            d2 = d2.min((p - (a + ab * t)).hypot2());
        }
        let d = d2.sqrt();
        let signed = if inside { d } else { -d };
        (signed + 0.5).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_schema_enum() {
        let schema = "rect, circle, pill, cookie, clover, burst, flower, gem, sunny, triangle, pentagon, hexagon, heart";
        let names: Vec<&str> = schema.split(", ").collect();
        assert_eq!(names.len(), Shape::ALL.len());
        for (n, s) in names.iter().zip(Shape::ALL) {
            assert_eq!(Shape::from_name(n), Some(s));
        }
        assert_eq!(Shape::from_name("blob"), None);
    }

    /// Every outline has its samples, stays in its box, and reaches it
    /// on some side (the shapes fill their boxes).
    #[test]
    fn outlines_fill_their_boxes() {
        for s in Shape::ALL {
            let o = outline(s, 1.5);
            assert_eq!(o.len(), SAMPLES, "{s:?}");
            let max = o
                .iter()
                .map(|p| p[0].abs().max(p[1].abs()))
                .fold(0.0f32, f32::max);
            assert!(max <= 1.0 + 1e-3, "{s:?} leaves its box: {max}");
            assert!(max >= 0.95, "{s:?} does not reach its box: {max}");
        }
        // A rect's corners are on samples; a pill's ends are round.
        let rect = outline(Shape::Rect, 2.0);
        assert!(rect.contains(&[1.0, -1.0]));
        let pill = outline(Shape::Pill, 2.0);
        assert!((pill[0][1] + 1.0).abs() < 1e-4, "top is flat");
        let right = pill[SAMPLES / 4];
        assert!((right[0] - 1.0).abs() < 1e-4 && right[1].abs() < 1e-4);
    }

    #[test]
    fn polygon_coverage_is_antialiased() {
        let frame = kurbo::Rect::new(0.0, 0.0, 100.0, 100.0);
        let disc = Polygon::new(&Outline::Shape(Shape::Circle), frame);
        assert_eq!(disc.coverage(kurbo::Point::new(50.0, 50.0)), 1.0);
        assert_eq!(disc.coverage(kurbo::Point::new(2.0, 2.0)), 0.0);
        let edge = disc.coverage(kurbo::Point::new(50.0, 0.25));
        assert!(edge > 0.0 && edge < 1.0, "{edge}");
        let cookie = Polygon::new(&Outline::Shape(Shape::Cookie), frame);
        // A scallop's tip at 12 o'clock reaches the edge, the dip between
        // two does not.
        assert_eq!(cookie.coverage(kurbo::Point::new(50.0, 2.0)), 1.0);
        let dip = TAU / 18.0;
        let (x, y) = (50.0 + 48.0 * dip.sin(), 50.0 - 48.0 * dip.cos());
        assert_eq!(cookie.coverage(kurbo::Point::new(x, y)), 0.0);
    }
}

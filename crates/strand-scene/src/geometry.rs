//! Geometry in physical pixels (`i32`/`u32`) and logical pixels (`f32`), and
//! the fractional output scale that converts between them.

/// A point in physical pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A size in physical pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl Size {
    pub const fn new(w: u32, h: u32) -> Self {
        Self { w, h }
    }

    pub const fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }

    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// A rectangle in physical pixels: origin `(x, y)`, extent `w × h`.
///
/// Edges are half-open: the rectangle covers columns `x..x + w` and rows
/// `y..y + h`. Edge arithmetic is done in `i64` so no combination of `i32`
/// origin and `u32` extent overflows.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// The rectangle at the origin covering `size`.
    pub const fn from_size(size: Size) -> Self {
        Self::new(0, 0, size.w, size.h)
    }

    /// Builds a rectangle from half-open edges, clamping to the `i32`/`u32`
    /// range. Inverted edges give an empty rectangle at `(x0, y0)`.
    pub fn from_edges(x0: i64, y0: i64, x1: i64, y1: i64) -> Self {
        let cx0 = x0.clamp(i32::MIN as i64, i32::MAX as i64);
        let cy0 = y0.clamp(i32::MIN as i64, i32::MAX as i64);
        let w = (x1 - cx0).clamp(0, u32::MAX as i64) as u32;
        let h = (y1 - cy0).clamp(0, u32::MAX as i64) as u32;
        Self::new(cx0 as i32, cy0 as i32, w, h)
    }

    pub const fn left(self) -> i64 {
        self.x as i64
    }

    pub const fn top(self) -> i64 {
        self.y as i64
    }

    /// One past the last covered column.
    pub const fn right(self) -> i64 {
        self.x as i64 + self.w as i64
    }

    /// One past the last covered row.
    pub const fn bottom(self) -> i64 {
        self.y as i64 + self.h as i64
    }

    pub const fn origin(self) -> Point {
        Point::new(self.x, self.y)
    }

    pub const fn size(self) -> Size {
        Size::new(self.w, self.h)
    }

    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub const fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// The overlap of two rectangles, or `None` if they do not overlap.
    pub fn intersect(self, other: Rect) -> Option<Rect> {
        let x0 = self.left().max(other.left());
        let y0 = self.top().max(other.top());
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        (x0 < x1 && y0 < y1).then(|| Rect::from_edges(x0, y0, x1, y1))
    }

    pub fn intersects(self, other: Rect) -> bool {
        self.intersect(other).is_some()
    }

    /// The bounding box of both rectangles. An empty rectangle contributes
    /// nothing.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect::from_edges(
            self.left().min(other.left()),
            self.top().min(other.top()),
            self.right().max(other.right()),
            self.bottom().max(other.bottom()),
        )
    }

    /// True if every pixel of `other` is inside `self`. Empty rectangles are
    /// contained in everything.
    pub fn contains_rect(self, other: Rect) -> bool {
        other.is_empty()
            || (self.left() <= other.left()
                && self.top() <= other.top()
                && self.right() >= other.right()
                && self.bottom() >= other.bottom())
    }

    pub fn contains(self, p: Point) -> bool {
        let (x, y) = (p.x as i64, p.y as i64);
        x >= self.left() && x < self.right() && y >= self.top() && y < self.bottom()
    }

    /// Grows the rectangle by `n` pixels on every side.
    pub fn inflate(self, n: u32) -> Rect {
        let n = n as i64;
        Rect::from_edges(
            self.left() - n,
            self.top() - n,
            self.right() + n,
            self.bottom() + n,
        )
    }

    pub fn translate(self, dx: i32, dy: i32) -> Rect {
        Rect::from_edges(
            self.left() + dx as i64,
            self.top() + dy as i64,
            self.right() + dx as i64,
            self.bottom() + dy as i64,
        )
    }
}

/// A point in logical pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LogicalPoint {
    pub x: f32,
    pub y: f32,
}

impl LogicalPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A size in logical pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LogicalSize {
    pub w: f32,
    pub h: f32,
}

impl LogicalSize {
    pub const fn new(w: f32, h: f32) -> Self {
        Self { w, h }
    }
}

/// A rectangle in logical pixels.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl LogicalRect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    pub fn is_empty(self) -> bool {
        self.w.is_nan() || self.h.is_nan() || self.w <= 0.0 || self.h <= 0.0
    }

    pub fn union(self, other: LogicalRect) -> LogicalRect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        LogicalRect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    pub fn intersect(self, other: LogicalRect) -> Option<LogicalRect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        (x0 < x1 && y0 < y1).then(|| LogicalRect::new(x0, y0, x1 - x0, y1 - y0))
    }

    pub fn translate(self, dx: f32, dy: f32) -> LogicalRect {
        LogicalRect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn inflate(self, n: f32) -> LogicalRect {
        LogicalRect::new(self.x - n, self.y - n, self.w + 2.0 * n, self.h + 2.0 * n)
    }
}

/// The fractional output scale as `numerator / 120`, the representation
/// `wp_fractional_scale_v1` sends (`preferred_scale`). `Scale(120)` is 1.0,
/// `Scale(150)` is 1.25, `Scale(180)` is 1.5 and `Scale(240)` is 2.0.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Scale(u32);

impl Default for Scale {
    fn default() -> Self {
        Self::ONE
    }
}

impl Scale {
    /// The protocol's denominator.
    pub const DENOMINATOR: u32 = 120;
    pub const ONE: Scale = Scale(120);

    /// A scale of `numerator / 120`; zero is rejected.
    pub const fn new(numerator: u32) -> Option<Self> {
        if numerator == 0 {
            None
        } else {
            Some(Self(numerator))
        }
    }

    /// An integer scale such as the legacy `wl_output.scale`.
    pub const fn from_integer(factor: u32) -> Option<Self> {
        Self::new(factor.saturating_mul(Self::DENOMINATOR))
    }

    /// The nearest representable scale to a float factor (1.25 → 150/120).
    pub fn from_f64(factor: f64) -> Option<Self> {
        if !factor.is_finite() || factor <= 0.0 {
            return None;
        }
        Self::new((factor * Self::DENOMINATOR as f64).round() as u32)
    }

    pub const fn numerator(self) -> u32 {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        self.0 as f64 / Self::DENOMINATOR as f64
    }

    pub fn as_f32(self) -> f32 {
        self.as_f64() as f32
    }

    pub const fn is_integer(self) -> bool {
        self.0 % Self::DENOMINATOR == 0
    }

    /// A logical length in physical pixels, unrounded.
    pub fn to_physical(self, logical: f32) -> f64 {
        logical as f64 * self.0 as f64 / Self::DENOMINATOR as f64
    }

    /// A physical length in logical pixels.
    pub fn to_logical(self, physical: f64) -> f32 {
        (physical * Self::DENOMINATOR as f64 / self.0 as f64) as f32
    }

    /// Rounds a logical coordinate to the nearest physical pixel edge,
    /// halves away from zero (the rounding `wp_fractional_scale_v1`
    /// prescribes for buffer sizes).
    pub fn round(self, logical: f32) -> i64 {
        let v = self.to_physical(logical);
        if v.is_finite() { v.round() as i64 } else { 0 }
    }

    /// Buffer size for a surface of logical size `size`: each dimension is
    /// `round(logical × scale)`, halves away from zero.
    pub fn physical_size(self, size: LogicalSize) -> Size {
        let w = self.round(size.w).clamp(0, u32::MAX as i64) as u32;
        let h = self.round(size.h).clamp(0, u32::MAX as i64) as u32;
        Size::new(w, h)
    }

    /// Logical size of a buffer of physical size `size`.
    pub fn logical_size(self, size: Size) -> LogicalSize {
        LogicalSize::new(
            self.to_logical(size.w as f64),
            self.to_logical(size.h as f64),
        )
    }

    /// Snaps a logical rectangle to physical pixels by rounding each edge
    /// independently. Adjacent logical rectangles stay adjacent and every
    /// edge lands on a pixel boundary, so fills stay crisp at 1.25 or 1.5.
    pub fn snap_rect(self, r: LogicalRect) -> Rect {
        Rect::from_edges(
            self.round(r.x),
            self.round(r.y),
            self.round(r.right()),
            self.round(r.bottom()),
        )
    }

    /// The smallest physical rectangle covering every pixel the logical
    /// rectangle touches (floor of the origin, ceiling of the far edges).
    /// Used for damage, which must never under-cover.
    pub fn cover_rect(self, r: LogicalRect) -> Rect {
        if r.is_empty() {
            return Rect::default();
        }
        let f = |v: f32| self.to_physical(v);
        let fin = |v: f64| if v.is_finite() { v } else { 0.0 };
        Rect::from_edges(
            fin(f(r.x)).floor() as i64,
            fin(f(r.y)).floor() as i64,
            fin(f(r.right())).ceil() as i64,
            fin(f(r.bottom())).ceil() as i64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_edges_and_intersection() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(b), Some(Rect::new(5, 5, 5, 5)));
        assert_eq!(a.union(b), Rect::new(0, 0, 15, 15));
        assert_eq!(a.intersect(Rect::new(10, 0, 5, 5)), None);
        assert!(a.contains_rect(Rect::new(2, 2, 3, 3)));
        assert!(!a.contains_rect(b));
        assert!(a.contains(Point::new(9, 9)));
        assert!(!a.contains(Point::new(10, 9)));
    }

    #[test]
    fn rect_extremes_do_not_overflow() {
        let r = Rect::new(i32::MAX - 1, 0, u32::MAX, 1);
        assert_eq!(r.right(), i32::MAX as i64 - 1 + u32::MAX as i64);
        assert_eq!(r.inflate(4).x, i32::MAX - 5);
    }

    #[test]
    fn scale_conversions() {
        let s = Scale::new(150).unwrap();
        assert_eq!(s.as_f64(), 1.25);
        assert_eq!(Scale::from_f64(1.5), Scale::new(180));
        assert_eq!(Scale::new(0), None);
        // 1366 × 1.25 = 1707.5 rounds away from zero.
        assert_eq!(
            s.physical_size(LogicalSize::new(1366.0, 36.0)),
            Size::new(1708, 45)
        );
        assert_eq!(s.logical_size(Size::new(2560, 45)).w, 2048.0);
    }

    #[test]
    fn snap_keeps_neighbours_adjacent() {
        let s = Scale::new(180).unwrap();
        let a = s.snap_rect(LogicalRect::new(0.0, 0.0, 10.3, 5.0));
        let b = s.snap_rect(LogicalRect::new(10.3, 0.0, 7.1, 5.0));
        assert_eq!(a.right(), b.left());
    }

    #[test]
    fn cover_never_undercovers() {
        let s = Scale::new(150).unwrap();
        let r = LogicalRect::new(1.1, 2.2, 3.3, 4.4);
        let c = s.cover_rect(r);
        assert!(c.left() as f64 <= s.to_physical(r.x));
        assert!(c.right() as f64 >= s.to_physical(r.right()));
        assert!(c.contains_rect(s.snap_rect(r)));
    }
}

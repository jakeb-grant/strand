//! Damage: up to [`MAX_RECTS`] physical rectangles that need repainting.

use crate::geometry::{Rect, Size};

/// The most rectangles a [`Damage`] holds (design: "up to 8 dirty
/// rectangles per frame").
pub const MAX_RECTS: usize = 8;

/// A set of at most [`MAX_RECTS`] physical rectangles.
///
/// Adding a rectangle that is already covered by one rectangle is a no-op,
/// and rectangles the new one covers are dropped. When a ninth rectangle
/// would be needed, the pair whose bounding box grows the covered area least
/// is merged, so damage only ever grows: every pixel added stays covered.
#[derive(Copy, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    rects: [Rect; MAX_RECTS],
    len: u8,
}

impl std::fmt::Debug for Damage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.rects()).finish()
    }
}

impl Damage {
    pub const fn new() -> Self {
        Self {
            rects: [Rect::new(0, 0, 0, 0); MAX_RECTS],
            len: 0,
        }
    }

    /// Damage covering one rectangle.
    pub fn from_rect(r: Rect) -> Self {
        let mut d = Self::new();
        d.add(r);
        d
    }

    /// Damage covering a whole buffer of `size`.
    pub fn full(size: Size) -> Self {
        Self::from_rect(Rect::from_size(size))
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Bounding box of all rectangles.
    pub fn bounds(&self) -> Option<Rect> {
        let mut it = self.rects().iter();
        let first = *it.next()?;
        Some(it.fold(first, |acc, r| acc.union(*r)))
    }

    /// Adds a rectangle. Empty rectangles are ignored.
    pub fn add(&mut self, r: Rect) {
        if r.is_empty() || self.rects().iter().any(|e| e.contains_rect(r)) {
            return;
        }
        let mut buf = [Rect::default(); MAX_RECTS + 1];
        let mut n = 0;
        for e in self.rects() {
            if !r.contains_rect(*e) {
                buf[n] = *e;
                n += 1;
            }
        }
        buf[n] = r;
        n += 1;
        while n > MAX_RECTS {
            n = merge_cheapest(&mut buf, n);
        }
        self.rects[..n].copy_from_slice(&buf[..n]);
        self.len = n as u8;
    }

    /// Adds every rectangle of `other`.
    pub fn union(&mut self, other: &Damage) {
        for r in other.rects() {
            self.add(*r);
        }
    }

    /// Adds every rectangle of `other`, clipped to `bounds`.
    pub fn union_clipped(&mut self, other: &Damage, bounds: Rect) {
        for r in other.rects() {
            if let Some(c) = r.intersect(bounds) {
                self.add(c);
            }
        }
    }

    /// Clips every rectangle to `bounds`, dropping those outside it.
    pub fn clip(&mut self, bounds: Rect) {
        let mut out = Damage::new();
        out.union_clipped(self, bounds);
        *self = out;
    }

    /// The damage clipped to `bounds`.
    pub fn clipped(mut self, bounds: Rect) -> Self {
        self.clip(bounds);
        self
    }

    /// True if `r` is entirely covered by the union of the rectangles.
    pub fn covers(&self, r: Rect) -> bool {
        if r.is_empty() {
            return true;
        }
        let mut d = Damage::new();
        for e in self.rects() {
            if let Some(c) = e.intersect(r) {
                d.rects[d.len as usize] = c;
                d.len += 1;
            }
        }
        d.area() == r.area()
    }

    /// Exact area in pixels of the union of the rectangles (overlaps are
    /// counted once). This is what the M0 exit criterion of ≤2,000 px² per
    /// clock tick is measured on.
    pub fn area(&self) -> u64 {
        union_area(self.rects())
    }
}

/// Exact union area of a handful of rectangles by coordinate compression.
fn union_area(rects: &[Rect]) -> u64 {
    match rects {
        [] => return 0,
        [r] => return r.area(),
        _ => {}
    }
    let mut xs = [0i64; 2 * (MAX_RECTS + 1)];
    let mut ys = [0i64; 2 * (MAX_RECTS + 1)];
    let n = rects.len().min(MAX_RECTS + 1);
    for (i, r) in rects.iter().take(n).enumerate() {
        xs[2 * i] = r.left();
        xs[2 * i + 1] = r.right();
        ys[2 * i] = r.top();
        ys[2 * i + 1] = r.bottom();
    }
    let xs = &mut xs[..2 * n];
    let ys = &mut ys[..2 * n];
    xs.sort_unstable();
    ys.sort_unstable();
    let mut area = 0u64;
    for xw in xs.windows(2) {
        let (x0, x1) = (xw[0], xw[1]);
        if x0 == x1 {
            continue;
        }
        for yw in ys.windows(2) {
            let (y0, y1) = (yw[0], yw[1]);
            if y0 == y1 {
                continue;
            }
            let covered = rects
                .iter()
                .take(n)
                .any(|r| r.left() <= x0 && r.right() >= x1 && r.top() <= y0 && r.bottom() >= y1);
            if covered {
                area += ((x1 - x0) * (y1 - y0)) as u64;
            }
        }
    }
    area
}

/// Merges the pair of rectangles whose bounding box adds the least area over
/// what the pair already covers; removes rectangles the merged box covers.
/// Returns the new count.
fn merge_cheapest(buf: &mut [Rect; MAX_RECTS + 1], n: usize) -> usize {
    let mut best = (0, 1);
    let mut best_cost = u64::MAX;
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (buf[i], buf[j]);
            let overlap = a.intersect(b).map_or(0, Rect::area);
            let covered = a.area() + b.area() - overlap;
            let cost = a.union(b).area() - covered;
            if cost < best_cost {
                best_cost = cost;
                best = (i, j);
            }
        }
    }
    let merged = buf[best.0].union(buf[best.1]);
    let mut out = 0;
    for k in 0..n {
        if k == best.0 || k == best.1 || merged.contains_rect(buf[k]) {
            continue;
        }
        buf[out] = buf[k];
        out += 1;
    }
    buf[out] = merged;
    out + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn contained_rects_are_absorbed() {
        let mut d = Damage::new();
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(2, 2, 3, 3));
        assert_eq!(d.len(), 1);
        d.add(Rect::new(-1, -1, 20, 20));
        assert_eq!(d.rects(), &[Rect::new(-1, -1, 20, 20)]);
        d.add(Rect::new(0, 0, 0, 5));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn ninth_rect_merges_cheapest_pair() {
        let mut d = Damage::new();
        for i in 0..8 {
            d.add(Rect::new(i * 100, 0, 10, 10));
        }
        assert_eq!(d.len(), 8);
        // Adjacent to the first rect: merging those two costs nothing.
        d.add(Rect::new(10, 0, 10, 10));
        assert_eq!(d.len(), 8);
        assert!(d.rects().contains(&Rect::new(0, 0, 20, 10)));
        assert_eq!(d.area(), 9 * 100);
    }

    #[test]
    fn area_counts_overlap_once() {
        let mut d = Damage::new();
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(5, 5, 10, 10));
        assert_eq!(d.area(), 175);
        assert!(d.covers(Rect::new(0, 0, 10, 10)));
        assert!(!d.covers(Rect::new(0, 0, 15, 15)));
    }

    #[test]
    fn clipping() {
        let mut d = Damage::from_rect(Rect::new(-5, -5, 10, 10));
        d.add(Rect::new(100, 100, 4, 4));
        d.clip(Rect::new(0, 0, 50, 50));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 5, 5)]);
        let mut e = Damage::new();
        e.union_clipped(&Damage::full(Size::new(80, 80)), Rect::new(0, 0, 50, 10));
        assert_eq!(e.area(), 500);
    }

    fn rect() -> impl Strategy<Value = Rect> {
        (-50i32..200, -50i32..200, 0u32..80, 0u32..80)
            .prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
    }

    proptest! {
        #[test]
        fn merged_damage_covers_inputs(rs in proptest::collection::vec(rect(), 0..40)) {
            let mut d = Damage::new();
            for r in &rs {
                d.add(*r);
                prop_assert!(d.len() <= MAX_RECTS);
            }
            for r in &rs {
                prop_assert!(d.covers(*r), "{r:?} not covered by {d:?}");
            }
            let bounds = rs.iter().fold(Rect::default(), |a, r| a.union(*r));
            prop_assert!(d.area() <= bounds.area());
            let exact = union_area_brute(&rs);
            prop_assert!(d.area() >= exact);
        }

        #[test]
        fn union_and_clip_keep_invariants(
            a in proptest::collection::vec(rect(), 0..12),
            b in proptest::collection::vec(rect(), 0..12),
            clip in rect(),
        ) {
            let mut da = Damage::new();
            a.iter().for_each(|r| da.add(*r));
            let mut db = Damage::new();
            b.iter().for_each(|r| db.add(*r));
            let mut u = da;
            u.union(&db);
            prop_assert!(u.len() <= MAX_RECTS);
            for r in a.iter().chain(&b) {
                prop_assert!(u.covers(*r));
            }
            let c = u.clipped(clip);
            for r in c.rects() {
                prop_assert!(clip.contains_rect(*r));
            }
            for r in a.iter().chain(&b) {
                if let Some(i) = r.intersect(clip) {
                    prop_assert!(c.covers(i));
                }
            }
        }

        #[test]
        fn area_matches_brute_force(rs in proptest::collection::vec(rect(), 0..8)) {
            let mut d = Damage::new();
            rs.iter().for_each(|r| d.add(*r));
            prop_assert_eq!(d.area(), union_area_brute(d.rects()));
        }
    }

    /// Exact union area for any number of rectangles (test oracle).
    fn union_area_brute(rs: &[Rect]) -> u64 {
        let mut xs: Vec<i64> = rs.iter().flat_map(|r| [r.left(), r.right()]).collect();
        let mut ys: Vec<i64> = rs.iter().flat_map(|r| [r.top(), r.bottom()]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let mut n = 0;
        for xw in xs.windows(2) {
            for yw in ys.windows(2) {
                let p = crate::geometry::Point::new(xw[0] as i32, yw[0] as i32);
                if rs.iter().any(|r| r.contains(p)) {
                    n += ((xw[1] - xw[0]) * (yw[1] - yw[0])) as u64;
                }
            }
        }
        n
    }
}

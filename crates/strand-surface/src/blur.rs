//! The blur ladder's first rung (design.md, "Blur ladder"): the region
//! `ext-background-effect-v1` blurs, following the rounded shapes of the
//! nodes with `blur` ([`strand_scene::Painter::blur_region`]).
//!
//! A `wl_region` is a union of rectangles in surface-local logical
//! pixels, so a rounded box becomes its middle as one rectangle plus
//! about one-pixel bands down each rounded corner. Every band lies
//! inside the shape (the compositor never blurs past a rounded corner,
//! where the surface is transparent), and rows of the same width merge,
//! so a box with square corners is one rectangle. Pure, so it is tested
//! without a compositor; the manager caches the result per surface and
//! sends it only when it changes (`manager/commit.rs`).

use strand_scene::{BlurRegion, Scale};

/// A rectangle of a blur region: `x, y, w, h` in surface-local logical
/// pixels.
pub type BlurRect = (i32, i32, i32, i32);

/// The rectangles covering `regions` (buffer pixels, as render reports
/// them) on a surface painted at `scale`, in logical pixels: inside
/// every rounded shape, one rectangle per run of rows of equal width,
/// in order (top to bottom, then left to right).
pub fn region_rects(regions: &[BlurRegion], scale: Scale) -> Vec<BlurRect> {
    let s = scale.as_f64();
    let s = if s.is_finite() && s > 0.0 { s } else { 1.0 };
    let mut out = Vec::new();
    for r in regions {
        shape_rects(r, s, &mut out);
    }
    out.sort_unstable_by_key(|&(x, y, w, h)| (y, x, w, h));
    out.dedup();
    out
}

/// The largest magnitude a coordinate takes, logical pixels: far past
/// any output, small enough that sums never overflow.
const LIMIT: f64 = (1 << 20) as f64;

fn shape_rects(r: &BlurRegion, s: f64, out: &mut Vec<BlurRect>) {
    if r.rect.w == 0 || r.rect.h == 0 {
        return;
    }
    let x0 = f64::from(r.rect.x) / s;
    let y0 = f64::from(r.rect.y) / s;
    let x1 = x0 + f64::from(r.rect.w) / s;
    let y1 = y0 + f64::from(r.rect.h) / s;
    let (w, h) = (x1 - x0, y1 - y0);
    // Radii in logical pixels, scaled down together until opposite
    // corners fit along each side (as render draws them).
    // `radius: full` may arrive infinite: a pill.
    let rad = |v: f32| {
        let v = f64::from(v) / s;
        if v == f64::INFINITY {
            w.max(h)
        } else if v.is_finite() && v > 0.0 {
            v
        } else {
            0.0
        }
    };
    let [mut tl, mut tr, mut br, mut bl] = r.radii.map(rad);
    let fit = |side: f64, a: f64, b: f64| if a + b > side { side / (a + b) } else { 1.0 };
    let f = fit(w, tl, tr)
        .min(fit(w, bl, br))
        .min(fit(h, tl, bl))
        .min(fit(h, tr, br));
    for v in [&mut tl, &mut tr, &mut br, &mut bl] {
        *v *= f;
    }
    // How far a corner of radius `rad` cuts into a row whose nearest
    // point to the corner's edge is `t` away from it.
    let inset = |rad: f64, t: f64| {
        if t >= rad {
            0.0
        } else {
            let d = rad - t.max(0.0);
            rad - (rad * rad - d * d).max(0.0).sqrt()
        }
    };
    let top = y0.ceil().clamp(-LIMIT, LIMIT) as i32;
    let bottom = y1.floor().clamp(-LIMIT, LIMIT) as i32;
    let mut run: Option<(i32, i32, i32, i32)> = None;
    for y in top..bottom {
        let (ry0, ry1) = (f64::from(y), f64::from(y) + 1.0);
        // The row's edge farthest into each corner cuts deepest.
        let (from_top, from_bottom) = (ry0 - y0, y1 - ry1);
        let left = x0 + inset(tl, from_top).max(inset(bl, from_bottom));
        let right = x1 - inset(tr, from_top).max(inset(br, from_bottom));
        let (l, rr) = (
            left.ceil().clamp(-LIMIT, LIMIT) as i32,
            right.floor().clamp(-LIMIT, LIMIT) as i32,
        );
        if rr <= l {
            if let Some(done) = run.take() {
                out.push(done);
            }
            continue;
        }
        match &mut run {
            Some((rx, _, rw, rh)) if *rx == l && *rw == rr - l => *rh += 1,
            _ => {
                if let Some(done) = run.take() {
                    out.push(done);
                }
                run = Some((l, y, rr - l, 1));
            }
        }
    }
    if let Some(done) = run {
        out.push(done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::Rect;

    fn region(x: i32, y: i32, w: u32, h: u32, r: f32) -> BlurRegion {
        BlurRegion {
            rect: Rect::new(x, y, w, h),
            radii: [r; 4],
            radius: 24.0,
        }
    }

    fn inside(rects: &[BlurRect], x: i32, y: i32) -> bool {
        rects
            .iter()
            .any(|&(rx, ry, rw, rh)| x >= rx && y >= ry && x < rx + rw && y < ry + rh)
    }

    /// Square corners: one rectangle, the box.
    #[test]
    fn a_square_box_is_one_rectangle() {
        let rects = region_rects(&[region(10, 20, 300, 40, 0.0)], Scale::ONE);
        assert_eq!(rects, [(10, 20, 300, 40)]);
        assert!(region_rects(&[], Scale::ONE).is_empty());
        assert!(region_rects(&[region(0, 0, 0, 40, 4.0)], Scale::ONE).is_empty());
    }

    /// A rounded box: the middle is one rectangle, each rounded corner
    /// about one band per pixel of radius, and every pixel of every band
    /// is inside the rounded shape (the compositor never blurs past a
    /// corner), while the box's centre and its straight edges are in.
    #[test]
    fn rounded_corners_become_one_pixel_bands_inside_the_shape() {
        let r = 16.0;
        let rects = region_rects(&[region(0, 0, 200, 100, r as f32)], Scale::ONE);
        // Middle plus at most r bands at the top and r at the bottom.
        assert!(rects.len() <= 1 + 2 * r as usize, "{}", rects.len());
        assert!(rects.len() > 8, "corners are banded: {rects:?}");
        assert!(rects.contains(&(0, 16, 200, 68)), "the middle: {rects:?}");
        // A point of the 200×100 box with radius-16 corners.
        let in_shape = |x: f64, y: f64| {
            let cx = x.clamp(r, 200.0 - r);
            let cy = y.clamp(r, 100.0 - r);
            (x - cx).powi(2) + (y - cy).powi(2) <= r * r + 1e-9
        };
        for &(x, y, w, h) in &rects {
            for py in y..y + h {
                for px in [x, x + w - 1] {
                    let (fx, fy) = (f64::from(px), f64::from(py));
                    for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                        assert!(
                            in_shape(fx + dx, fy + dy),
                            "pixel ({px}, {py}) reaches outside the rounded shape"
                        );
                    }
                }
            }
        }
        assert!(inside(&rects, 100, 50));
        assert!(inside(&rects, 100, 0) && inside(&rects, 0, 50));
        assert!(!inside(&rects, 0, 0) && !inside(&rects, 199, 99));
        // A pill: every row is a band.
        let pill = region_rects(&[region(0, 0, 120, 32, f32::INFINITY)], Scale::ONE);
        assert!(pill.len() >= 16, "{pill:?}");
        assert!(!inside(&pill, 0, 0) && !inside(&pill, 3, 3) && inside(&pill, 60, 16));
    }

    /// Buffer pixels become logical ones, rounded inward: at 2× a box of
    /// 400×200 buffer pixels at (20, 20) is 200×100 at (10, 10); at 1.5×
    /// an odd edge rounds inside the box.
    #[test]
    fn regions_are_logical_and_rounded_inward() {
        let rects = region_rects(
            &[region(20, 20, 400, 200, 0.0)],
            Scale::from_integer(2).unwrap(),
        );
        assert_eq!(rects, [(10, 10, 200, 100)]);
        let s = Scale::new(180).unwrap(); // 1.5
        let rects = region_rects(&[region(1, 1, 301, 151, 0.0)], s);
        assert_eq!(rects, [(1, 1, 200, 100)]);
    }

    /// Two nodes with blur: both shapes, and the same shape twice once;
    /// the order is stable, so the cache compares equal.
    #[test]
    fn several_shapes_are_one_stable_list() {
        let a = region(0, 0, 100, 30, 0.0);
        let b = region(0, 50, 100, 30, 0.0);
        let one = region_rects(&[b, a, a], Scale::ONE);
        assert_eq!(one, [(0, 0, 100, 30), (0, 50, 100, 30)]);
        assert_eq!(one, region_rects(&[a, b], Scale::ONE));
    }

    /// Values a `.strand` file can hold, however large, neither overflow
    /// nor loop for long: the rows are bounded by the logical limit.
    #[test]
    fn huge_and_odd_values_are_safe() {
        let nan = BlurRegion {
            rect: Rect::new(0, 0, 50, 50),
            radii: [f32::NAN, -3.0, f32::INFINITY, 0.0],
            radius: 1.0,
        };
        let rects = region_rects(&[nan], Scale::ONE);
        assert!(!rects.is_empty());
        let wide = region(i32::MAX - 10, 0, 10, 4, 2.0);
        let _ = region_rects(&[wide], Scale::ONE);
    }
}

//! (eval-rules-6) A `border:` ring's shape, measured and worked out
//! apart (docs/eval.md, `coherence`'s `ring`).
//!
//! A border is drawn inside its box (decisions.md 2026-10-05 render:
//! "Borders are inside the box, width rounded to whole physical pixels
//! (≥1)"), and its inner edge is the outer rounded box inset by the
//! border's width with each radius reduced by it, as CSS draws a border
//! (CSS Backgrounds 3, "Corner Shaping": the padding edge's radii are
//! the border edge's less the border's width, never below 0). The outer
//! radii are the `radius:` asked for, shrunk as CSS shrinks them so
//! adjacent corners never overlap (one factor for all four, the least
//! of each side's length over the sum of its two radii; `radius: full`
//! a pill), and the CSS shorthand gives one to four values clockwise
//! from the top left (design.md `radius: 14, 14, 0, 0`).
//!
//! The harness measures the ring the flattener filled
//! (`probe::RingDrawn::path`, even-odd) along a few rays: each corner's
//! bisector, where a wrong inner radius moves the inner edge furthest
//! (`√2 − 1` of the radius's error), and each edge's midpoint, where
//! the ring's width shows. [`expected`] works out where those rays cross
//! the shape the props ask for, from the box, the radii and the width
//! alone, never from the renderer's own radii or paths.

use serde::{Deserialize, Serialize};
use strand_scene::{Length, PropValue};

/// A radius that stands for `full`: larger than any box, so CSS's
/// shrink makes it half the shorter side.
pub const FULL: f64 = 1e9;

/// How far outside the box each ray starts, device px: a ray through a
/// square corner then meets the corner, not starts on it.
const OUTSIDE: f64 = 2.0;

/// One ring as drawn in a frame: its node, its surface, the box it is
/// drawn in (device px, `x0, y0, x1, y1`), the device scale, the
/// border's width and each corner's radius as the props ask (logical px,
/// clockwise from the top left, [`FULL`] for `full`), and where each of
/// [`rays`] crosses the outline drawn (device px from the ray's start).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RingRec {
    pub key: String,
    pub s: u32,
    pub frame: [f64; 4],
    pub scale: f64,
    pub width: f32,
    pub radius: [f64; 4],
    pub drawn: Vec<Vec<f32>>,
}

/// A ray: where it starts, its direction (unit) and its length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ray {
    pub at: [f64; 2],
    pub dir: [f64; 2],
    pub len: f64,
}

/// The rays a ring is measured along, for its box `frame`: each
/// corner's bisector clockwise from the top left, from outside the
/// corner to the middle of the shorter side's reach, then each edge's
/// midpoint, top, right, bottom, left, inward to the box's middle.
pub fn rays(frame: [f64; 4]) -> Vec<Ray> {
    let [x0, y0, x1, y1] = frame;
    let (w, h) = (x1 - x0, y1 - y0);
    let d = std::f64::consts::FRAC_1_SQRT_2;
    let reach = OUTSIDE + w.min(h) / 2.0 * std::f64::consts::SQRT_2;
    let corner = |px: f64, py: f64, sx: f64, sy: f64| Ray {
        at: [px - sx * OUTSIDE * d, py - sy * OUTSIDE * d],
        dir: [sx * d, sy * d],
        len: reach,
    };
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    vec![
        corner(x0, y0, 1.0, 1.0),
        corner(x1, y0, -1.0, 1.0),
        corner(x1, y1, -1.0, -1.0),
        corner(x0, y1, 1.0, -1.0),
        Ray {
            at: [cx, y0 - OUTSIDE],
            dir: [0.0, 1.0],
            len: OUTSIDE + h / 2.0,
        },
        Ray {
            at: [x1 + OUTSIDE, cy],
            dir: [-1.0, 0.0],
            len: OUTSIDE + w / 2.0,
        },
        Ray {
            at: [cx, y1 + OUTSIDE],
            dir: [0.0, -1.0],
            len: OUTSIDE + h / 2.0,
        },
        Ray {
            at: [x0 - OUTSIDE, cy],
            dir: [1.0, 0.0],
            len: OUTSIDE + w / 2.0,
        },
    ]
}

/// Points this close along a ray are one crossing (a ray through a
/// vertex meets both segments there).
const SAME: f64 = 1e-3;

fn dedup(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < SAME);
    v
}

/// Where `ray` crosses `path`, device px from its start, in order.
pub fn crossings(path: &kurbo::BezPath, ray: Ray) -> Vec<f64> {
    use kurbo::ParamCurve;
    let line = kurbo::Line::new(
        (ray.at[0], ray.at[1]),
        (
            ray.at[0] + ray.dir[0] * ray.len,
            ray.at[1] + ray.dir[1] * ray.len,
        ),
    );
    let mut out = Vec::new();
    for seg in path.segments() {
        for i in seg.intersect_line(line) {
            // kurbo's cubic solver can return a root that is not on the
            // ray where an arc's chord runs along it (its end, for a
            // corner arc parallel to a bisector): a crossing is kept
            // only where the outline's point lies on the ray, measured
            // where it projects.
            let p = seg.eval(i.segment_t);
            let (dx, dy) = (p.x - ray.at[0], p.y - ray.at[1]);
            let along = dx * ray.dir[0] + dy * ray.dir[1];
            let off = (dx * ray.dir[1] - dy * ray.dir[0]).abs();
            if off <= ON_RAY && (-ON_RAY..=ray.len + ON_RAY).contains(&along) {
                out.push(along);
            }
        }
    }
    dedup(out)
}

/// How far from a ray a point of the outline may lie and count as on
/// it, device px: f64 slack, far below a pixel.
const ON_RAY: f64 = 1e-3;

/// A `radius:` as the props give it, per corner clockwise from the top
/// left (logical px; [`FULL`] for `full`), on a box of logical `size`;
/// `None` for a form this does not read (it is not judged).
pub fn radii(v: Option<&PropValue>, size: [f32; 2]) -> Option<[f64; 4]> {
    let short = f64::from(size[0].min(size[1]).max(0.0));
    let one = |v: &PropValue| -> Option<f64> {
        let r = match v {
            PropValue::Number(n) | PropValue::Length(Length::Px(n)) => f64::from(*n),
            PropValue::Length(Length::Percent(p)) => short * f64::from(*p) / 100.0,
            PropValue::Keyword(k) if k == "full" => FULL,
            _ => return None,
        };
        Some(clean(r))
    };
    let Some(v) = v else {
        return Some([0.0; 4]);
    };
    match v {
        PropValue::Corners(c) => Some(
            [c.top_left, c.top_right, c.bottom_right, c.bottom_left].map(|r| clean(f64::from(r))),
        ),
        PropValue::List(items) => {
            let v: Vec<f64> = items.iter().map(one).collect::<Option<_>>()?;
            // CSS's shorthand, clockwise from the top left.
            Some(match v[..] {
                [a] => [a, a, a, a],
                [a, b] => [a, b, a, b],
                [a, b, c] => [a, b, c, b],
                [a, b, c, d] => [a, b, c, d],
                _ => return None,
            })
        }
        v => one(v).map(|r| [r; 4]),
    }
}

/// A radius as drawn: infinite is `full`, negative or not a number is
/// square.
fn clean(r: f64) -> f64 {
    if r == f64::INFINITY {
        FULL
    } else if r.is_finite() && r > 0.0 {
        r.min(FULL)
    } else {
        0.0
    }
}

/// The border's width in device px: its width at the scale, rounded to
/// whole pixels, at least one (decisions.md 2026-10-05 render).
pub fn border_px(width: f32, scale: f64) -> f64 {
    (f64::from(width) * scale).round().max(1.0)
}

/// The outer radii in device px on a box `w` × `h`: scaled, then shrunk
/// by CSS's one factor so no two corners on a side overlap.
pub fn outer_radii(r: [f64; 4], scale: f64, w: f64, h: f64) -> [f64; 4] {
    let r = r.map(|v| v * scale);
    let ratio = |side: f64, a: f64, b: f64| {
        if a + b > side && a + b > 0.0 {
            side.max(0.0) / (a + b)
        } else {
            1.0
        }
    };
    let f = ratio(w, r[0], r[1])
        .min(ratio(w, r[3], r[2]))
        .min(ratio(h, r[0], r[3]))
        .min(ratio(h, r[1], r[2]));
    r.map(|v| v * f)
}

/// Where `ray` crosses the boundary of the rounded box `b`
/// (`x0, y0, x1, y1`) with radii `r` (clockwise from the top left,
/// fitting the box), device px from the ray's start.
pub fn rrect_crossings(b: [f64; 4], r: [f64; 4], ray: Ray) -> Vec<f64> {
    let [x0, y0, x1, y1] = b;
    let mut out = Vec::new();
    let [ax, ay] = ray.at;
    let [dx, dy] = ray.dir;
    let within = |t: f64| (-SAME..=ray.len + SAME).contains(&t);
    // The straight parts of the edges: (fixed coordinate, along x?,
    // from, to).
    let edges = [
        (y0, true, x0 + r[0], x1 - r[1]),
        (x1, false, y0 + r[1], y1 - r[2]),
        (y1, true, x0 + r[3], x1 - r[2]),
        (x0, false, y0 + r[0], y1 - r[3]),
    ];
    for (c, horizontal, lo, hi) in edges {
        if hi < lo - SAME {
            continue;
        }
        let (dc, ac, da, aa) = if horizontal {
            (dy, ay, dx, ax)
        } else {
            (dx, ax, dy, ay)
        };
        if dc.abs() < 1e-12 {
            continue;
        }
        let t = (c - ac) / dc;
        let along = aa + da * t;
        if within(t) && along >= lo - SAME && along <= hi + SAME {
            out.push(t);
        }
    }
    // The corners' quarter circles: centre, radius, and the quadrant
    // (signs of the point's offset from the centre).
    let corners = [
        (x0 + r[0], y0 + r[0], r[0], -1.0, -1.0),
        (x1 - r[1], y0 + r[1], r[1], 1.0, -1.0),
        (x1 - r[2], y1 - r[2], r[2], 1.0, 1.0),
        (x0 + r[3], y1 - r[3], r[3], -1.0, 1.0),
    ];
    for (cx, cy, rr, sx, sy) in corners {
        if rr <= 0.0 {
            continue;
        }
        // |a + t d − c|² = rr², d unit.
        let (px, py) = (ax - cx, ay - cy);
        let bq = px * dx + py * dy;
        let cq = px * px + py * py - rr * rr;
        let disc = bq * bq - cq;
        if disc < 0.0 {
            continue;
        }
        for t in [-bq - disc.sqrt(), -bq + disc.sqrt()] {
            let (qx, qy) = (px + dx * t, py + dy * t);
            if within(t) && qx * sx >= -SAME && qy * sy >= -SAME {
                out.push(t);
            }
        }
    }
    out.retain(|t| within(*t));
    dedup(out)
}

/// Where each of [`rays`] should cross the ring of `rec`: its outer
/// rounded box and, when the border leaves room inside, that box inset
/// by the border's width with each radius reduced by it.
pub fn expected(rec: &RingRec) -> Vec<Vec<f64>> {
    let [x0, y0, x1, y1] = rec.frame;
    let (w, h) = (x1 - x0, y1 - y0);
    let bw = border_px(rec.width, rec.scale);
    let outer = outer_radii(rec.radius, rec.scale, w, h);
    let inner_box = [x0 + bw, y0 + bw, x1 - bw, y1 - bw];
    let has_inner = inner_box[2] - inner_box[0] > 0.0 && inner_box[3] - inner_box[1] > 0.0;
    let inner = outer.map(|r| (r - bw).max(0.0));
    rays(rec.frame)
        .into_iter()
        .map(|ray| {
            let mut v = rrect_crossings(rec.frame, outer, ray);
            if has_inner {
                v.extend(rrect_crossings(inner_box, inner, ray));
            }
            dedup(v)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corner arc whose chord runs along a bisector (a 48 × 18 chip,
    /// radius 6, a 2 px border: the inner top-left arc from 32,9 to
    /// 36,5) gives kurbo's solver a root off the ray; it is not a
    /// crossing.
    #[test]
    fn a_root_off_the_ray_is_no_crossing() {
        use kurbo::{Rect, RoundedRect, RoundedRectRadii, Shape};
        let frame = [30.0, 3.0, 78.0, 21.0];
        let mut p = RoundedRect::from_rect(
            Rect::new(30.0, 3.0, 78.0, 21.0),
            RoundedRectRadii::from_single_radius(6.0),
        )
        .to_path(0.1);
        p.extend(
            RoundedRect::from_rect(
                Rect::new(32.0, 5.0, 76.0, 19.0),
                RoundedRectRadii::from_single_radius(4.0),
            )
            .to_path(0.1),
        );
        let rec = RingRec {
            frame,
            scale: 1.0,
            width: 2.0,
            radius: [6.0; 4],
            ..RingRec::default()
        };
        let want = expected(&rec);
        for (ray, w) in rays(frame).into_iter().zip(want) {
            let got = crossings(&p, ray);
            assert_eq!(got.len(), w.len(), "{got:?} {w:?}");
            for (g, w) in got.iter().zip(&w) {
                assert!((g - w).abs() < 0.1, "{got:?} {w:?}");
            }
        }
    }

    fn rrect_path(b: [f64; 4], r: f64) -> kurbo::BezPath {
        use kurbo::Shape;
        kurbo::RoundedRect::new(b[0], b[1], b[2], b[3], r).to_path(0.1)
    }

    #[test]
    fn a_ray_meets_a_rounded_box_where_its_arc_lies() {
        let b = [0.0, 0.0, 100.0, 40.0];
        let rays = rays(b);
        // The top-left bisector meets the arc at r(√2 − 1) past the
        // corner, OUTSIDE before it.
        let want = OUTSIDE + 10.0 * (std::f64::consts::SQRT_2 - 1.0);
        let got = rrect_crossings(b, [10.0; 4], rays[0]);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!((got[0] - want).abs() < 1e-9, "{got:?} {want}");
        let drawn = crossings(&rrect_path(b, 10.0), rays[0]);
        assert_eq!(drawn.len(), 1);
        assert!((drawn[0] - want).abs() < 0.05, "{drawn:?} {want}");
        // A square corner: the corner itself, once.
        let sq = crossings(&rrect_path(b, 0.0), rays[0]);
        assert_eq!(sq.len(), 1, "{sq:?}");
        assert!((sq[0] - OUTSIDE).abs() < 1e-9);
        assert_eq!(rrect_crossings(b, [0.0; 4], rays[0]).len(), 1);
        // The top edge's midpoint ray meets the edge.
        let top = rrect_crossings(b, [10.0; 4], rays[4]);
        assert_eq!(top, vec![OUTSIDE]);
    }

    #[test]
    fn radii_read_the_css_shorthand_and_full() {
        let n = |v: f32| PropValue::Number(v);
        assert_eq!(
            radii(Some(&PropValue::List(vec![n(14.0), n(0.0)])), [10.0, 10.0]),
            Some([14.0, 0.0, 14.0, 0.0])
        );
        assert_eq!(
            radii(
                Some(&PropValue::List(vec![n(1.0), n(2.0), n(3.0)])),
                [10.0, 10.0]
            ),
            Some([1.0, 2.0, 3.0, 2.0])
        );
        assert_eq!(
            radii(
                Some(&PropValue::Length(Length::Percent(50.0))),
                [40.0, 20.0]
            ),
            Some([10.0; 4])
        );
        let full = radii(Some(&PropValue::Keyword("full".into())), [10.0, 10.0]).unwrap();
        assert_eq!(outer_radii(full, 1.5, 90.0, 36.0), [18.0; 4]);
        assert_eq!(radii(None, [1.0, 1.0]), Some([0.0; 4]));
    }

    #[test]
    fn a_css_ring_is_as_wide_at_its_corners_as_along_its_edges() {
        let rec = RingRec {
            frame: [0.0, 0.0, 120.0, 60.0],
            scale: 1.0,
            width: 2.0,
            radius: [16.0; 4],
            ..RingRec::default()
        };
        let e = expected(&rec);
        // Each bisector: outer then inner, 2 px apart (the radii are
        // concentric); each edge: 2 px apart.
        for v in &e {
            assert_eq!(v.len(), 2, "{e:?}");
            assert!((v[1] - v[0] - 2.0).abs() < 1e-9, "{e:?}");
        }
    }
}

//! Paint and shape helpers: colours and gradients from props, opacity,
//! the blur tint fallback, corner radii and squircles, rect conversions.

use strand_scene::{Color, Corners, Damage, Length, Paint, PropValue, Rect};
use vello_cpu::kurbo::{self, BezPath, RoundedRect, RoundedRectRadii, Shape};

use super::{MAX_LOGICAL, TOLERANCE, finite_or_zero};

pub(super) fn opaque_paint(p: &Paint) -> bool {
    match p {
        Paint::Solid(c) => c.clamped().a >= 1.0,
        Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. } => {
            !stops.is_empty() && stops.iter().all(|s| s.color.clamped().a >= 1.0)
        }
    }
}

/// The fully opaque part of a filled rounded rect: the box minus its
/// corner squares, as a horizontal and a vertical band.
pub(super) fn opaque_bands(phys: Rect, r: &RoundedRectRadii) -> Damage {
    let c = |v: f64| v.ceil().clamp(0.0, u32::MAX as f64) as i64;
    let (l, t, rr, b) = (phys.left(), phys.top(), phys.right(), phys.bottom());
    let top = c(r.top_left.max(r.top_right));
    let bottom = c(r.bottom_left.max(r.bottom_right));
    let left = c(r.top_left.max(r.bottom_left));
    let right = c(r.top_right.max(r.bottom_right));
    let mut d = Damage::new();
    d.add(Rect::from_edges(l, t + top, rr, b - bottom));
    d.add(Rect::from_edges(l + left, t, rr - right, b));
    d
}

/// `blur`'s tint fallback: every colour's alpha up by 0.15.
pub(super) fn tinted(mut p: Paint) -> Paint {
    let up = |c: &mut Color| c.a = (c.a + BLUR_TINT).min(1.0);
    match &mut p {
        Paint::Solid(c) => up(c),
        Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. } => {
            stops.iter_mut().for_each(|s| up(&mut s.color))
        }
    }
    p
}

/// How much `blur`'s tint fallback raises the background's alpha.
pub const BLUR_TINT: f32 = 0.15;

pub(super) fn paint_of(v: Option<&PropValue>) -> Option<Paint> {
    match v? {
        PropValue::Color(c) => Some(Paint::Solid(*c)),
        PropValue::Paint(p) => Some(p.clone()),
        _ => None,
    }
}

/// Corner radii in logical pixels for a box of `w × h` logical pixels.
/// `radius: full` arrives as `Keyword("full")` or infinite radii
/// ([`Corners::FULL`]) and becomes the largest finite radius, which the
/// CSS shrink in [`radii`] turns into a pill; a percentage is of the
/// shorter side. The comma shorthand (`radius: $radius.lg, $radius.lg,
/// 0, 0`) is a `List` of one to four of those, expanded like CSS. NaN and
/// negative radii are square.
pub(crate) fn corners_of(v: Option<&PropValue>, w: f32, h: f32) -> Corners {
    let one = |v: &PropValue| match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) => Some(*n),
        PropValue::Length(Length::Percent(p)) => Some(w.min(h) * p / 100.0),
        PropValue::Keyword(k) if k == "full" => Some(f32::INFINITY),
        _ => None,
    };
    let c = match v {
        Some(PropValue::Corners(c)) => *c,
        Some(PropValue::List(items)) => items
            .iter()
            .map(one)
            .collect::<Option<Vec<f32>>>()
            .and_then(|v| Corners::from_values(&v))
            .unwrap_or_default(),
        Some(v) => one(v).map(Corners::all).unwrap_or_default(),
        None => Corners::default(),
    };
    let one = |r: f32| {
        if r == f32::INFINITY {
            MAX_LOGICAL
        } else {
            finite_or_zero(r).max(0.0)
        }
    };
    Corners {
        top_left: one(c.top_left),
        top_right: one(c.top_right),
        bottom_right: one(c.bottom_right),
        bottom_left: one(c.bottom_left),
    }
}

/// Scales radii to physical pixels and shrinks them like CSS so adjacent
/// corners never overlap (`radius: full` becomes a pill).
pub(crate) fn radii(c: Corners, w: f64, h: f64, s: f64) -> RoundedRectRadii {
    let (tl, tr, br, bl) = (
        (c.top_left as f64 * s).max(0.0),
        (c.top_right as f64 * s).max(0.0),
        (c.bottom_right as f64 * s).max(0.0),
        (c.bottom_left as f64 * s).max(0.0),
    );
    let ratio = |side: f64, a: f64, b: f64| if a + b > side { side / (a + b) } else { 1.0 };
    let f = ratio(w, tl, tr)
        .min(ratio(w, bl, br))
        .min(ratio(h, tl, bl))
        .min(ratio(h, tr, br));
    RoundedRectRadii::new(tl * f, tr * f, br * f, bl * f)
}

pub(super) fn radii_zero(r: &RoundedRectRadii) -> bool {
    r.top_left <= 0.0 && r.top_right <= 0.0 && r.bottom_right <= 0.0 && r.bottom_left <= 0.0
}

pub(super) fn shape_path(rect: kurbo::Rect, r: RoundedRectRadii, squircle: bool) -> BezPath {
    if radii_zero(&r) {
        rect.to_path(TOLERANCE)
    } else if squircle {
        squircle_path(rect, r)
    } else {
        RoundedRect::from_rect(rect, r).to_path(TOLERANCE)
    }
}

/// Superellipse exponent of `corners: squircle`.
pub(super) const SQUIRCLE_N: f64 = 5.0;

/// How much further along each edge a squircle corner starts than a
/// circular one of the same radius: the curve eases into the straight
/// edge instead of meeting it at a kink in curvature (the "continuous
/// corner" of iOS and Material 3 Expressive).
pub(super) const SQUIRCLE_REACH: f64 = 1.6;

/// A rounded rect whose corners are superellipse quadrants
/// (`|x|^n + |y|^n = 1`, n = 5) reaching `SQUIRCLE_REACH` × the radius
/// along each edge (capped at half the side), flattened to lines.
/// Hit testing and blurred shadows keep the circular shape of the same
/// radii, which a squircle stays within a pixel or so of.
pub(super) fn squircle_path(rect: kurbo::Rect, r: RoundedRectRadii) -> BezPath {
    let (w, h) = (rect.width(), rect.height());
    let cap = (w.min(h) / 2.0).max(0.0);
    let reach = |v: f64| (v * SQUIRCLE_REACH).min(cap).max(0.0);
    // Corners clockwise from top-left: (corner point, x dir, y dir).
    let corners = [
        (rect.x0, rect.y0, 1.0, 1.0, reach(r.top_left)),
        (rect.x1, rect.y0, -1.0, 1.0, reach(r.top_right)),
        (rect.x1, rect.y1, -1.0, -1.0, reach(r.bottom_right)),
        (rect.x0, rect.y1, 1.0, -1.0, reach(r.bottom_left)),
    ];
    let mut path = BezPath::new();
    let e = 2.0 / SQUIRCLE_N;
    for (i, &(cx, cy, dx, dy, rr)) in corners.iter().enumerate() {
        // Points from the edge before the corner to the edge after it,
        // going clockwise.
        let steps = ((rr.sqrt() * 4.0).ceil() as usize).clamp(4, 64);
        let pt = |t: f64| {
            let (s, c) = t.sin_cos();
            // At t = 0 on the edge before, at π/2 on the edge after.
            let a = rr - rr * c.abs().powf(e);
            let b = rr - rr * s.abs().powf(e);
            match i {
                0 => kurbo::Point::new(cx + dx * a, cy + dy * b),
                1 => kurbo::Point::new(cx + dx * b, cy + dy * a),
                2 => kurbo::Point::new(cx + dx * a, cy + dy * b),
                _ => kurbo::Point::new(cx + dx * b, cy + dy * a),
            }
        };
        for k in 0..=steps {
            let t = std::f64::consts::FRAC_PI_2 * k as f64 / steps as f64;
            let p = pt(t);
            if i == 0 && k == 0 {
                path.move_to(p);
            } else {
                path.line_to(p);
            }
        }
    }
    path.close_path();
    path
}

pub(super) fn kurbo_rect(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        r.left() as f64,
        r.top() as f64,
        r.right() as f64,
        r.bottom() as f64,
    )
}

/// Smallest pixel rectangle covering a float rectangle.
pub(super) fn cover(r: kurbo::Rect) -> Rect {
    Rect::from_edges(
        r.x0.floor() as i64,
        r.y0.floor() as i64,
        r.x1.ceil() as i64,
        r.y1.ceil() as i64,
    )
}

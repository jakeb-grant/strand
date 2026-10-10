//! (M4) Stroke styles (design.md, "Shape and geometry": "Stroke styles:
//! dash, trim, caps, wavy", `stroke: 3, $accent { trim: 0, progress;
//! wave: 2, 18px; cap: round }`), and the centre lines of `arc` gauges and
//! wavy meters, as fill paths.
//!
//! A stroke follows a centre line: a box's outline inset by half the
//! stroke's width (so it is drawn inside the box, as a border is), an
//! arc, a meter's midline. The line is flattened to points; a closed one
//! starts at the top of its box's centre and runs clockwise, so `trim: 0,
//! p` is a progress ring filling clockwise from 12 o'clock. Then, in
//! order:
//!
//! - `wave: amplitude, wavelength` moves each point along the line's
//!   normal by `amplitude · sin(2π s / wavelength)` (`s` the distance along
//!   it); a closed line fits a whole number of waves, so it has no seam;
//! - `trim: from, to` keeps the part between those fractions of its
//!   length (after the wave, so a trimmed wave keeps its shape);
//! - `dash: on, off` and `cap: butt | round | square` are kurbo's stroker
//!   (round joins), which turns the line into the path that is filled.
//!
//! Lengths here are physical pixels.

use strand_scene::{Prop, PropValue};
use vello_cpu::kurbo::{self, BezPath, PathEl, Point, Shape, Vec2};

use crate::effects::{keyword, number};

/// Flattening tolerance, device pixels.
const TOLERANCE: f64 = 0.1;

/// Most points a styled line keeps (a huge wavy box stays bounded).
const MAX_POINTS: usize = 20_000;

/// Line caps.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cap {
    Butt,
    Round,
    Square,
}

impl Cap {
    pub(crate) fn from_name(name: &str) -> Option<Cap> {
        match name {
            "butt" => Some(Cap::Butt),
            "round" => Some(Cap::Round),
            "square" => Some(Cap::Square),
            _ => None,
        }
    }

    fn kurbo(self) -> kurbo::Cap {
        match self {
            Cap::Butt => kurbo::Cap::Butt,
            Cap::Round => kurbo::Cap::Round,
            Cap::Square => kurbo::Cap::Square,
        }
    }
}

/// How a line is stroked, physical pixels.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Style {
    pub width: f64,
    pub cap: Cap,
    /// Fractions of the length kept, `0 ≤ from ≤ to ≤ 1`.
    pub trim: Option<(f64, f64)>,
    /// Amplitude and wavelength.
    pub wave: Option<(f64, f64)>,
    /// Dash and gap lengths.
    pub dash: Option<(f64, f64)>,
}

impl Style {
    /// A plain stroke of `width` with `cap`.
    pub(crate) fn plain(width: f64, cap: Cap) -> Style {
        Style {
            width,
            cap,
            trim: None,
            wave: None,
            dash: None,
        }
    }
}

/// `stroke:`'s default wavelength when `wave:` gives only an amplitude,
/// logical pixels (design.md's example: `wave: 2, 18px`).
pub(crate) const WAVELENGTH: f32 = 18.0;

/// The style a node's resolved props (`get`) give a stroke of `width`
/// physical pixels on a surface at `scale`: `trim: from, to` (or `trim:
/// to`), `wave: amplitude, wavelength` (or an amplitude alone), `dash: on,
/// off` (or `on`, gaps as long), and `cap:` (`default` without one).
pub(crate) fn style_of<'v>(
    get: impl Fn(Prop) -> Option<&'v PropValue>,
    width: f64,
    scale: f64,
    default: Cap,
) -> Style {
    let pair = |v: Option<&PropValue>| -> Option<(f32, Option<f32>)> {
        match v? {
            PropValue::List(items) => Some((
                items.first().and_then(number)?,
                items.get(1).and_then(number),
            )),
            other => Some((number(other)?, None)),
        }
    };
    let trim = pair(get(Prop::Trim)).map(|(a, b)| {
        let (a, b) = match b {
            Some(b) => (a, b),
            None => (0.0, a),
        };
        let (a, b) = (a.clamp(0.0, 1.0) as f64, b.clamp(0.0, 1.0) as f64);
        (a.min(b), a.max(b))
    });
    let wave = pair(get(Prop::Wave))
        .map(|(a, l)| {
            (
                a.clamp(-1000.0, 1000.0) as f64 * scale,
                l.unwrap_or(WAVELENGTH).clamp(1.0, 10_000.0) as f64 * scale,
            )
        })
        .filter(|(a, _)| *a != 0.0);
    let dash = pair(get(Prop::Dash))
        .map(|(on, off)| {
            let on = on.clamp(0.0, 10_000.0) as f64 * scale;
            let off = off.map_or(on, |o| o.clamp(0.0, 10_000.0) as f64 * scale);
            (on, off)
        })
        .filter(|(on, _)| *on > 0.0);
    let cap = get(Prop::Cap)
        .and_then(keyword)
        .and_then(Cap::from_name)
        .unwrap_or(default);
    Style {
        width,
        cap,
        trim,
        wave,
        dash,
    }
}

/// One flattened subpath.
#[derive(Clone, Debug)]
struct Line {
    points: Vec<Point>,
    closed: bool,
}

/// `path` as flattened lines.
fn lines(path: &BezPath) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    kurbo::flatten(path.iter(), TOLERANCE, |el| match el {
        PathEl::MoveTo(p) => out.push(Line {
            points: vec![p],
            closed: false,
        }),
        PathEl::LineTo(p) => {
            if let Some(l) = out.last_mut()
                && l.points.last() != Some(&p)
            {
                l.points.push(p);
            }
        }
        PathEl::ClosePath => {
            if let Some(l) = out.last_mut() {
                l.closed = true;
                if l.points.len() > 1 && l.points.first() == l.points.last() {
                    l.points.pop();
                }
            }
        }
        _ => {}
    });
    // A subpath that ends where it began is closed (kurbo's ellipse path
    // has no `ClosePath`).
    for l in &mut out {
        if !l.closed
            && l.points.len() > 2
            && let (Some(a), Some(b)) = (l.points.first(), l.points.last())
            && a.distance(*b) < 1e-6
        {
            l.closed = true;
            l.points.pop();
        }
    }
    out.retain(|l| l.points.len() > 1);
    out
}

/// Twice the signed area of a closed polygon (positive: clockwise on
/// screen, y down).
fn area2(p: &[Point]) -> f64 {
    let n = p.len();
    (0..n)
        .map(|i| {
            let (a, b) = (p[i], p[(i + 1) % n]);
            a.x * b.y - b.x * a.y
        })
        .sum()
}

/// A closed line made to start nearest the top of its bounding box's
/// centre and run clockwise.
fn from_top(mut l: Line) -> Line {
    if !l.closed {
        return l;
    }
    if area2(&l.points) < 0.0 {
        l.points.reverse();
    }
    let (x0, x1) = l
        .points
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.x), b.max(p.x)));
    let y0 = l.points.iter().fold(f64::MAX, |a, p| a.min(p.y));
    let cx = (x0 + x1) / 2.0;
    // The highest crossing of the vertical centre line: the line is split
    // there and starts from it.
    let n = l.points.len();
    let mut best: Option<(usize, Point)> = None;
    for i in 0..n {
        let (a, b) = (l.points[i], l.points[(i + 1) % n]);
        if (a.x - cx) * (b.x - cx) > 0.0 || a.x == b.x {
            continue;
        }
        let t = (cx - a.x) / (b.x - a.x);
        let p = a.lerp(b, t);
        if best.is_none_or(|(_, q)| p.y < q.y) {
            best = Some((i, p));
        }
    }
    let Some((i, p)) = best else {
        // No crossing (degenerate): the topmost point.
        let i = l
            .points
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (a.y - y0).abs().total_cmp(&(b.y - y0).abs()))
            .map_or(0, |(i, _)| i);
        l.points.rotate_left(i);
        return l;
    };
    l.points.rotate_left((i + 1) % n);
    if l.points.last() != Some(&p) && l.points.first() != Some(&p) {
        l.points.insert(0, p);
    } else if l.points.last() == Some(&p) {
        l.points.rotate_right(1);
    }
    l
}

/// The cumulative length at each point (closed: the closing edge adds
/// one more entry, the total).
fn lengths(l: &Line) -> Vec<f64> {
    let mut acc = 0.0;
    let mut out = Vec::with_capacity(l.points.len() + 1);
    out.push(0.0);
    let n = l.points.len();
    let edges = if l.closed { n } else { n - 1 };
    for i in 0..edges {
        acc += l.points[i].distance(l.points[(i + 1) % n]);
        out.push(acc);
    }
    out
}

/// The point at distance `s` along `l` (`acc` from [`lengths`]).
fn at(l: &Line, acc: &[f64], s: f64) -> Point {
    let n = l.points.len();
    let i = match acc.binary_search_by(|v| v.total_cmp(&s)) {
        Ok(i) => i.min(acc.len() - 2),
        Err(i) => i.saturating_sub(1).min(acc.len() - 2),
    };
    let (a, b) = (l.points[i % n], l.points[(i + 1) % n]);
    let seg = acc[i + 1] - acc[i];
    let t = if seg > 0.0 {
        ((s - acc[i]) / seg).clamp(0.0, 1.0)
    } else {
        0.0
    };
    a.lerp(b, t)
}

/// `l` moved along its normal by a sine of `amp` and wavelength `len`.
fn wave(l: &Line, amp: f64, len: f64) -> Line {
    let acc = lengths(l);
    let total = acc.last().copied().unwrap_or(0.0);
    if total <= 0.0 || amp == 0.0 || len <= 0.0 {
        return l.clone();
    }
    // A closed line fits whole waves.
    let len = if l.closed {
        total / (total / len).round().max(1.0)
    } else {
        len
    };
    // Sixteen samples a wave, at most a device pixel apart.
    let step = (len / 16.0).min(1.0).max(total / MAX_POINTS as f64);
    let n = (total / step).ceil().max(2.0) as usize;
    let count = if l.closed { n } else { n + 1 };
    let points = (0..count)
        .map(|k| {
            let s = total * k as f64 / n as f64;
            let p = at(l, &acc, s);
            let d = (at(l, &acc, (s + step / 2.0).min(total))
                - at(l, &acc, (s - step / 2.0).max(0.0)))
            .normalize();
            let normal = if d.is_finite() {
                Vec2::new(d.y, -d.x)
            } else {
                Vec2::ZERO
            };
            p + normal * amp * (std::f64::consts::TAU * s / len).sin()
        })
        .collect();
    Line {
        points,
        closed: l.closed,
    }
}

/// The part of `l` between fractions `a` and `b` of its length (open).
fn trim(l: &Line, a: f64, b: f64) -> Option<Line> {
    let acc = lengths(l);
    let total = acc.last().copied().unwrap_or(0.0);
    let (sa, sb) = (total * a, total * b);
    if sb - sa <= 1e-6 {
        return None;
    }
    let n = l.points.len();
    let mut points = vec![at(l, &acc, sa)];
    for (i, s) in acc.iter().enumerate().skip(1) {
        if *s > sa && *s < sb {
            points.push(l.points[i % n]);
        }
    }
    points.push(at(l, &acc, sb));
    Some(Line {
        points,
        closed: false,
    })
}

/// The lines of `center` styled (wave, then trim) as a path.
fn styled(center: &BezPath, style: &Style) -> BezPath {
    let mut path = BezPath::new();
    for l in lines(center) {
        let mut l = from_top(l);
        if let Some((amp, len)) = style.wave {
            l = wave(&l, amp, len);
        }
        if let Some((a, b)) = style.trim {
            match trim(&l, a, b) {
                Some(t) => l = t,
                None => continue,
            }
        }
        let mut pts = l.points.iter();
        if let Some(p) = pts.next() {
            path.move_to(*p);
        }
        for p in pts {
            path.line_to(*p);
        }
        if l.closed {
            path.close_path();
        }
    }
    path
}

/// The fill path of `center` stroked in `style`; `None` when it draws
/// nothing.
pub(crate) fn outline(center: &BezPath, style: &Style) -> Option<BezPath> {
    if !(style.width.is_finite() && style.width > 0.0) {
        return None;
    }
    let line = styled(center, style);
    if line.elements().is_empty() {
        return None;
    }
    let mut stroke = kurbo::Stroke::new(style.width)
        .with_caps(style.cap.kurbo())
        .with_join(kurbo::Join::Round);
    if let Some((on, off)) = style.dash
        && on > 0.0
        && off >= 0.0
        && (on + off) >= 0.5
    {
        stroke = stroke.with_dashes(0.0, [on, off]);
    }
    let out = kurbo::stroke(
        line.iter(),
        &stroke,
        &kurbo::StrokeOpts::default(),
        TOLERANCE,
    );
    (!out.elements().is_empty() && out.bounding_box().area() > 0.0).then_some(out)
}

/// A circular arc's centre line: centre `c`, radius `r`, from `start`
/// degrees clockwise (0 is 3 o'clock, y down) through `sweep` degrees.
pub(crate) fn arc(c: Point, r: f64, start: f64, sweep: f64) -> BezPath {
    let a = kurbo::Arc::new(c, (r, r), start.to_radians(), sweep.to_radians(), 0.0);
    let mut p = BezPath::new();
    p.move_to(c + Vec2::from_angle(start.to_radians()) * r);
    a.append_iter(TOLERANCE).for_each(|el| p.push(el));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> BezPath {
        kurbo::Rect::new(0.0, 0.0, 100.0, 100.0).to_path(0.1)
    }

    fn covers(p: &BezPath, x: f64, y: f64) -> bool {
        p.winding(Point::new(x, y)) != 0
    }

    #[test]
    fn a_closed_line_starts_at_the_top_and_runs_clockwise() {
        let l = from_top(lines(&square()).remove(0));
        assert!(l.closed);
        // It starts at the top centre, and runs right along the top.
        assert_eq!(l.points[0], Point::new(50.0, 0.0));
        assert_eq!(l.points[1], Point::new(100.0, 0.0));
        assert!(area2(&l.points) > 0.0);
        // A circle's start is at 12 o'clock (an ellipse's path does not
        // close, but ends where it began).
        let circle =
            kurbo::Ellipse::from_rect(kurbo::Rect::new(10.0, 10.0, 90.0, 90.0)).to_path(0.1);
        let l = from_top(lines(&circle).remove(0));
        assert!((l.points[0].x - 50.0).abs() < 3.0 && (l.points[0].y - 10.0).abs() < 1.0);
        assert!(l.points[3].x > l.points[0].x, "clockwise: rightwards first");
    }

    #[test]
    fn trim_keeps_a_fraction_from_twelve_oclock() {
        let circle = kurbo::Circle::new((50.0, 50.0), 40.0).to_path(0.1);
        let half = Style {
            trim: Some((0.0, 0.5)),
            ..Style::plain(4.0, Cap::Butt)
        };
        let p = outline(&circle, &half).unwrap();
        // The right half of the ring, not the left.
        assert!(covers(&p, 90.0, 50.0));
        assert!(!covers(&p, 10.0, 50.0));
        assert!(
            outline(
                &circle,
                &Style {
                    trim: Some((0.3, 0.3)),
                    ..half.clone()
                }
            )
            .is_none()
        );
    }

    #[test]
    fn caps_dashes_and_waves() {
        let mut line = BezPath::new();
        line.move_to((10.0, 50.0));
        line.line_to((90.0, 50.0));
        let butt = outline(&line, &Style::plain(6.0, Cap::Butt)).unwrap();
        let round = outline(&line, &Style::plain(6.0, Cap::Round)).unwrap();
        assert!(!covers(&butt, 8.0, 50.0) && covers(&round, 8.0, 50.0));
        let dashed = outline(
            &line,
            &Style {
                dash: Some((10.0, 10.0)),
                ..Style::plain(6.0, Cap::Butt)
            },
        )
        .unwrap();
        assert!(covers(&dashed, 15.0, 50.0) && !covers(&dashed, 25.0, 50.0));
        // A wave of amplitude 8 and wavelength 40: a quarter wave in, the
        // line is 8 px off the midline.
        let wavy = outline(
            &line,
            &Style {
                wave: Some((8.0, 40.0)),
                ..Style::plain(2.0, Cap::Butt)
            },
        )
        .unwrap();
        assert!(!covers(&wavy, 20.0, 50.0));
        assert!(covers(&wavy, 20.0, 42.0) || covers(&wavy, 20.0, 58.0));
        let b = wavy.bounding_box();
        assert!((b.height() - 18.0).abs() < 0.5, "{b:?}");
    }

    #[test]
    fn props_give_the_style() {
        let props = [
            (
                Prop::Trim,
                PropValue::List(vec![PropValue::Number(0.75), PropValue::Number(0.25)]),
            ),
            (Prop::Wave, PropValue::Number(2.0)),
            (Prop::Dash, PropValue::Number(3.0)),
            (Prop::Cap, PropValue::Keyword("round".into())),
        ];
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v);
        assert_eq!(
            style_of(get, 6.0, 2.0, Cap::Butt),
            Style {
                width: 6.0,
                cap: Cap::Round,
                trim: Some((0.25, 0.75)),
                wave: Some((4.0, 36.0)),
                dash: Some((6.0, 6.0)),
            }
        );
        assert_eq!(
            style_of(|_| None, 1.0, 1.0, Cap::Butt),
            Style::plain(1.0, Cap::Butt)
        );
    }

    #[test]
    fn arcs_run_clockwise_from_their_start() {
        // 270° from 135° (bottom left) round the top to 45°.
        let a = arc(Point::new(50.0, 50.0), 40.0, 135.0, 270.0);
        let p = outline(&a, &Style::plain(4.0, Cap::Butt)).unwrap();
        assert!(covers(&p, 50.0, 10.0), "through the top");
        assert!(!covers(&p, 50.0, 90.0), "the gap at the bottom");
    }
}

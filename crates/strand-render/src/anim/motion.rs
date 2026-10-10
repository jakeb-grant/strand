//! One prop's motion: its value encoded as channels a spring moves, and
//! the spring itself.

use std::time::Duration;

use strand_scene::motion::{channels_color, color_channels};
use strand_scene::{Border, Color, Corners, Curve, Length, Motion, Paint, Prop, PropValue, Shadow};

use super::eps;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Enc {
    One([f32; 1]),
    Four([f32; 4]),
    Five([f32; 5]),
    Shadows(Vec<[f32; 8]>),
    /// (M4) A `shader` node's `uniforms:`.
    Uniforms(Uniforms),
}

/// (M4) A `shader` node's `uniforms:` as channels: each entry's numbers
/// in order (a colour's four channels, a list's items), with the
/// entries' names and kinds as its shape. Springing between two values
/// of the same shape moves every channel; another shape snaps.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Uniforms {
    shape: Vec<(String, Kind)>,
    values: Vec<f32>,
}

/// What a uniform's channels decode to.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    Number,
    Px,
    Percent,
    Angle,
    Color,
    List(Vec<Kind>),
}

/// `v`'s kind, its channels added to `out`; `None` for a value that
/// cannot spring (a duration, a bool: it snaps).
fn kind_of(v: &PropValue, out: &mut Vec<f32>) -> Option<Kind> {
    let finite = |n: f32| n.is_finite().then_some(n);
    Some(match v {
        PropValue::Number(n) => {
            out.push(finite(*n)?);
            Kind::Number
        }
        PropValue::Length(Length::Px(n)) => {
            out.push(finite(*n)?);
            Kind::Px
        }
        PropValue::Length(Length::Percent(n)) => {
            out.push(finite(*n)?);
            Kind::Percent
        }
        PropValue::Angle(n) => {
            out.push(finite(*n)?);
            Kind::Angle
        }
        PropValue::Color(c) | PropValue::Paint(Paint::Solid(c)) => {
            out.extend(color_channels(*c));
            Kind::Color
        }
        PropValue::List(items) => Kind::List(
            items
                .iter()
                .map(|i| kind_of(i, out))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

/// The value of kind `k` from the channels `it` gives next.
fn of_kind(k: &Kind, it: &mut impl Iterator<Item = f32>) -> PropValue {
    let mut next = || it.next().unwrap_or(0.0);
    match k {
        Kind::Number => PropValue::Number(next()),
        Kind::Px => PropValue::Length(Length::Px(next())),
        Kind::Percent => PropValue::Length(Length::Percent(next())),
        Kind::Angle => PropValue::Angle(next()),
        Kind::Color => {
            let c = [next(), next(), next(), next()];
            PropValue::Color(channels_color(c))
        }
        Kind::List(items) => PropValue::List(items.iter().map(|k| of_kind(k, it)).collect()),
    }
}

impl Uniforms {
    fn of(entries: &[(String, PropValue)]) -> Option<Uniforms> {
        let mut values = Vec::new();
        let shape = entries
            .iter()
            .map(|(n, v)| Some((n.clone(), kind_of(v, &mut values)?)))
            .collect::<Option<_>>()?;
        Some(Uniforms { shape, values })
    }

    /// `self` moved `u` of the way to `to`, if they are of one shape.
    pub(super) fn mix(&self, to: &Uniforms, u: f32) -> Option<Uniforms> {
        (self.shape == to.shape && self.values.len() == to.values.len()).then(|| Uniforms {
            shape: self.shape.clone(),
            values: self
                .values
                .iter()
                .zip(&to.values)
                .map(|(a, b)| a + (b - a) * u)
                .collect(),
        })
    }

    fn value(&self) -> PropValue {
        let mut it = self.values.iter().copied();
        PropValue::Uniforms(
            self.shape
                .iter()
                .map(|(n, k)| (n.clone(), of_kind(k, &mut it)))
                .collect(),
        )
    }
}

#[derive(Clone, Debug)]
pub(super) enum PropMotion {
    One(Motion<1>),
    Four(Motion<4>),
    Five(Motion<5>),
    /// Padded to the longer of the lists it moves between; `len` is the
    /// target's own length.
    Shadows {
        m: Vec<Motion<8>>,
        len: usize,
    },
    /// One spring per channel, all of one shape.
    Uniforms {
        shape: Vec<(String, Kind)>,
        m: Vec<Motion<1>>,
    },
}

pub(super) fn number(v: &PropValue) -> Option<f32> {
    match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) | PropValue::Angle(n) => {
            n.is_finite().then_some(*n)
        }
        _ => None,
    }
}

pub(super) fn shadow_channels(s: &Shadow) -> [f32; 8] {
    let c = color_channels(s.color);
    let f = |v: f32| if v.is_finite() { v } else { 0.0 };
    [
        f(s.x),
        f(s.y),
        f(s.blur),
        f(s.spread),
        c[0],
        c[1],
        c[2],
        c[3],
    ]
}

/// The boxes a value is resolved against: the node's own laid-out box
/// (`radius: full`, a percentage radius) and its parent's (a percentage
/// `x`/`y`, as flatten resolves them).
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Extents {
    pub own: (f32, f32),
    pub parent: (f32, f32),
}

/// The colour a lone-radius `glow: r` glows in, as flatten draws it at
/// rest: the node's own solid `color` in `props`, else `inh` (what it
/// inherits).
pub(super) fn glow_color(props: &[(Prop, std::borrow::Cow<'_, PropValue>)], inh: Color) -> Color {
    match props
        .iter()
        .find(|(q, _)| *q == Prop::Color)
        .map(|(_, v)| v.as_ref())
    {
        Some(PropValue::Color(c) | PropValue::Paint(Paint::Solid(c))) => *c,
        _ => inh,
    }
}

/// A prop value as channels (`None` value: the prop's default, `inh` for
/// `color`), lengths resolved against `b`, so `radius: full` and
/// percentage offsets spring in pixels. `None` when it cannot
/// interpolate (a gradient): it snaps.
pub(super) fn encode(p: Prop, v: Option<&PropValue>, inh: Color, b: Extents) -> Option<Enc> {
    let solid = |v: &PropValue| match v {
        PropValue::Color(c) | PropValue::Paint(Paint::Solid(c)) => Some(*c),
        _ => None,
    };
    let half = (b.own.0.min(b.own.1) / 2.0).max(0.0);
    // A radius in pixels: `full` (a pill) is half the shorter side, as
    // the CSS shrink of an infinite radius draws it.
    let radius = |v: &PropValue| match v {
        PropValue::Length(Length::Percent(q)) => Some(b.own.0.min(b.own.1) * q / 100.0),
        PropValue::Keyword(k) if k == "full" => Some(half),
        v => number(v),
    };
    Some(match (p, v) {
        (Prop::X | Prop::Y | Prop::Rotate | Prop::Value, None) => Enc::One([0.0]),
        (Prop::Value, Some(v)) => Enc::One([number(v)?]),
        (Prop::Track, None) => Enc::Four(color_channels(Color::TRANSPARENT)),
        (Prop::Track, Some(v)) => Enc::Four(color_channels(solid(v)?)),
        (Prop::Opacity | Prop::Scale, None) => Enc::One([1.0]),
        (Prop::X, Some(PropValue::Length(Length::Percent(q)))) => {
            Enc::One([b.parent.0 * q / 100.0]).finite()?
        }
        (Prop::Y, Some(PropValue::Length(Length::Percent(q)))) => {
            Enc::One([b.parent.1 * q / 100.0]).finite()?
        }
        (Prop::X | Prop::Y | Prop::Opacity | Prop::Scale | Prop::Rotate, Some(v)) => {
            Enc::One([number(v)?])
        }
        (Prop::Bg, None) => Enc::Four(color_channels(Color::TRANSPARENT)),
        (Prop::Color, None) => Enc::Four(color_channels(inh)),
        (Prop::Bg | Prop::Color, Some(v)) => Enc::Four(color_channels(solid(v)?)),
        // (M4) A text's solid `fill:` (no fill draws in `color`, which is
        // no colour to spring from: it snaps).
        (Prop::Fill, Some(v)) => Enc::Four(color_channels(solid(v)?)),
        // (M4) `trim: from, to`; `wave: amplitude[, wavelength]`; `glow:
        // radius, colour` (a lone radius glows in `inh`, which callers set
        // to the node's colour, [`glow_color`]).
        (Prop::Trim, None) => Enc::Four([0.0, 1.0, 0.0, 0.0]),
        (Prop::Trim, Some(PropValue::List(items))) => {
            let a = number(items.first()?)?;
            let b = number(items.get(1)?)?;
            Enc::Four([a, b, 0.0, 0.0])
        }
        (Prop::Wave, None) => Enc::One([0.0]),
        (Prop::Wave, Some(PropValue::List(items))) => {
            let a = number(items.first()?)?;
            let l = number(items.get(1)?)?;
            Enc::Four([a, l, 0.0, 0.0])
        }
        (Prop::Wave, Some(v)) => Enc::One([number(v)?]),
        (Prop::Glow, None) => {
            let c = color_channels(Color::TRANSPARENT);
            Enc::Five([0.0, c[0], c[1], c[2], c[3]])
        }
        (Prop::Glow, Some(v)) => {
            let (r, c) = match v {
                PropValue::List(items) => (
                    number(items.first()?)?,
                    items.get(1).map_or(Some(inh), solid)?,
                ),
                v => (number(v)?, inh),
            };
            let c = color_channels(c);
            Enc::Five([r, c[0], c[1], c[2], c[3]])
        }
        (Prop::Border | Prop::Stroke, None) => {
            let c = color_channels(Color::TRANSPARENT);
            Enc::Five([0.0, c[0], c[1], c[2], c[3]])
        }
        (Prop::Border | Prop::Stroke, Some(PropValue::Border(Border { width, paint }))) => {
            let c = color_channels(solid(&PropValue::Paint(paint.clone()))?);
            Enc::Five([width.is_finite().then_some(*width)?, c[0], c[1], c[2], c[3]])
        }
        (Prop::Uniforms, Some(PropValue::Uniforms(entries))) => {
            Enc::Uniforms(Uniforms::of(entries)?)
        }
        (Prop::Shadow, None) => Enc::Shadows(Vec::new()),
        (Prop::Shadow, Some(PropValue::Shadow(list))) => {
            Enc::Shadows(list.iter().map(shadow_channels).collect())
        }
        (Prop::Radius, None) => Enc::Four([0.0; 4]),
        (Prop::Radius, Some(v)) => {
            let c = match v {
                PropValue::Corners(c) => *c,
                PropValue::List(items) => {
                    let n: Option<Vec<f32>> = items.iter().map(radius).collect();
                    Corners::from_values(&n?)?
                }
                v => Corners::all(radius(v)?),
            };
            let all = [c.top_left, c.top_right, c.bottom_right, c.bottom_left]
                .map(|r| if r == f32::INFINITY { half } else { r });
            if all.iter().any(|v| !v.is_finite() || *v > 1e5) {
                return None;
            }
            Enc::Four(all)
        }
        _ => return None,
    })
}

impl Enc {
    pub(super) fn finite(self) -> Option<Enc> {
        match &self {
            Enc::One([v]) if !v.is_finite() => None,
            _ => Some(self),
        }
    }
}

pub(super) fn decode(p: Prop, e: &Enc) -> PropValue {
    match (p, e) {
        (Prop::Rotate, Enc::One([v])) => PropValue::Angle(*v),
        (_, Enc::One([v])) => PropValue::Number(*v),
        (Prop::Trim | Prop::Wave, Enc::Four(c)) => {
            PropValue::List(vec![PropValue::Number(c[0]), PropValue::Number(c[1])])
        }
        (Prop::Glow, Enc::Five(b)) => PropValue::List(vec![
            PropValue::Number(b[0].max(0.0)),
            PropValue::Color(channels_color([b[1], b[2], b[3], b[4]])),
        ]),
        (Prop::Radius, Enc::Four(c)) => PropValue::Corners(Corners {
            top_left: c[0].max(0.0),
            top_right: c[1].max(0.0),
            bottom_right: c[2].max(0.0),
            bottom_left: c[3].max(0.0),
        }),
        (_, Enc::Four(c)) => PropValue::Color(channels_color(*c)),
        (_, Enc::Five(b)) => PropValue::Border(Border {
            width: b[0].max(0.0),
            paint: Paint::Solid(channels_color([b[1], b[2], b[3], b[4]])),
        }),
        (_, Enc::Uniforms(u)) => u.value(),
        (_, Enc::Shadows(list)) => PropValue::Shadow(
            list.iter()
                .map(|s| Shadow {
                    x: s[0],
                    y: s[1],
                    blur: s[2].max(0.0),
                    spread: s[3],
                    color: channels_color([s[4], s[5], s[6], s[7]]),
                })
                .collect(),
        ),
    }
}

/// A shadow's geometry with no colour: what a shorter list is padded with.
pub(super) fn clear(s: [f32; 8]) -> [f32; 8] {
    [s[0], s[1], s[2], s[3], 0.0, 0.0, 0.0, 0.0]
}

impl PropMotion {
    pub(super) fn rest(p: Prop, e: &Enc, last: Option<Duration>) -> PropMotion {
        let k = eps(p);
        match e {
            Enc::One(v) => PropMotion::One(Motion::rest(*v, k).sampled_at(last)),
            Enc::Four(v) => PropMotion::Four(Motion::rest(*v, k).sampled_at(last)),
            Enc::Five(v) => PropMotion::Five(Motion::rest(*v, k).sampled_at(last)),
            Enc::Shadows(list) => PropMotion::Shadows {
                m: list
                    .iter()
                    .map(|s| Motion::rest(*s, 0.01).sampled_at(last))
                    .collect(),
                len: list.len(),
            },
            Enc::Uniforms(u) => PropMotion::Uniforms {
                shape: u.shape.clone(),
                m: u.values
                    .iter()
                    .map(|v| Motion::rest([*v], k).sampled_at(last))
                    .collect(),
            },
        }
    }

    pub(super) fn target(&self) -> Enc {
        match self {
            PropMotion::One(m) => Enc::One(m.target()),
            PropMotion::Four(m) => Enc::Four(m.target()),
            PropMotion::Five(m) => Enc::Five(m.target()),
            PropMotion::Shadows { m, len } => {
                Enc::Shadows(m.iter().take(*len).map(Motion::target).collect())
            }
            PropMotion::Uniforms { shape, m } => Enc::Uniforms(Uniforms {
                shape: shape.clone(),
                values: m.iter().map(|m| m.target()[0]).collect(),
            }),
        }
    }

    /// Retargets; false if `e` is of another shape (the caller snaps).
    pub(super) fn retarget(&mut self, e: &Enc, curve: Curve, last: Option<Duration>) -> bool {
        match (self, e) {
            (PropMotion::One(m), Enc::One(v)) => m.retarget(*v, curve),
            (PropMotion::Four(m), Enc::Four(v)) => m.retarget(*v, curve),
            (PropMotion::Five(m), Enc::Five(v)) => m.retarget(*v, curve),
            (PropMotion::Shadows { m, len }, Enc::Shadows(list)) => {
                // Padded to equal length: a missing shadow is the other's
                // geometry, fully transparent (design.md, snap rules).
                for (i, motion) in m.iter_mut().enumerate() {
                    let t = list
                        .get(i)
                        .copied()
                        .unwrap_or_else(|| clear(motion.target()));
                    motion.retarget(t, curve);
                }
                for s in list.iter().skip(m.len()) {
                    let mut motion = Motion::rest(clear(*s), 0.01).sampled_at(last);
                    motion.retarget(*s, curve);
                    m.push(motion);
                }
                *len = list.len();
            }
            (PropMotion::Uniforms { shape, m }, Enc::Uniforms(u))
                if *shape == u.shape && m.len() == u.values.len() =>
            {
                for (m, v) in m.iter_mut().zip(&u.values) {
                    m.retarget([*v], curve);
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn value(&mut self, at: Duration, commit: bool) -> Enc {
        fn one<const N: usize>(m: &mut Motion<N>, at: Duration, commit: bool) -> [f32; N] {
            if commit { m.sample(at) } else { m.peek(at) }
        }
        match self {
            PropMotion::One(m) => Enc::One(one(m, at, commit)),
            PropMotion::Four(m) => Enc::Four(one(m, at, commit)),
            PropMotion::Five(m) => Enc::Five(one(m, at, commit)),
            PropMotion::Shadows { m, .. } => {
                Enc::Shadows(m.iter_mut().map(|m| one(m, at, commit)).collect())
            }
            PropMotion::Uniforms { shape, m } => Enc::Uniforms(Uniforms {
                shape: shape.clone(),
                values: m.iter_mut().map(|m| one(m, at, commit)[0]).collect(),
            }),
        }
    }

    pub(super) fn settled(&self, at: Duration) -> bool {
        match self {
            PropMotion::One(m) => m.is_settled(at),
            PropMotion::Four(m) => m.is_settled(at),
            PropMotion::Five(m) => m.is_settled(at),
            PropMotion::Shadows { m, .. } => m.iter().all(|m| m.is_settled(at)),
            PropMotion::Uniforms { m, .. } => m.iter().all(|m| m.is_settled(at)),
        }
    }
}

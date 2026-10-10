//! (M4) Light on a node (design.md, "Paint and light"): `glow:`,
//! `inner_shadow:` and `rim:`, as display items the flattener pushes.
//!
//! - `glow: r, color` reaches about `r` logical pixels: the shape grown
//!   by σ = r/4 and blurred by σ, so it starts near full strength at the
//!   edge. A box glows as a shadow with no offset and a spread of σ under
//!   it (the shadow cache's 9-slice); text, icons and images glow their
//!   own pixels: a copy of what they drew, tinted to the colour, blurred
//!   by σ and its coverage doubled (standing in for the growth), in an
//!   offscreen group under them (cached until the content changes).
//! - `inner_shadow:` takes the shadow syntax: inside the box, the shadow a
//!   hole the box's shape casts, offset, spread inwards and blurred (CSS
//!   `inset`), drawn as a blurred offscreen ring clipped to the box.
//! - `rim: edge, color`: a lit edge, one logical pixel (at least one
//!   device pixel) along the inside of that edge, following the box's
//!   corners and thinning out around them.

use std::sync::Arc;

use strand_scene::{Color, Edge, Effect, Paint, PropValue, Rect, Shadow};
use vello_cpu::kurbo::{self, Affine, BezPath, Shape};

use super::{color, keyword, number};
use crate::flatten::{DisplayItem, Item};
use crate::layers::Layer;

/// Largest glow, logical pixels.
const MAX_GLOW: f32 = 250.0;

/// A node's `glow:`: its reach in logical pixels and its colour.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Glow {
    pub radius: f32,
    pub color: Color,
}

impl Glow {
    /// `glow: r, color` (`glow: r` glows in `fallback`, the node's
    /// colour). `None` for no glow, a zero radius or a clear colour.
    pub(crate) fn of(v: Option<&PropValue>, fallback: Color) -> Option<Glow> {
        let (radius, color) = match v? {
            PropValue::List(items) => (
                items.first().and_then(number)?,
                items.get(1).and_then(color).unwrap_or(fallback),
            ),
            other => (number(other)?, fallback),
        };
        let radius = radius.min(MAX_GLOW);
        (radius > 0.0 && color.a > 0.0 && color.a.is_finite()).then_some(Glow { radius, color })
    }

    /// The glow's standard deviation, logical pixels.
    fn sigma(self) -> f32 {
        self.radius / 4.0
    }

    /// The shadow a box glows as: no offset, a spread of σ and a CSS blur
    /// radius of 2σ.
    pub(crate) fn shadow(self) -> Shadow {
        Shadow {
            x: 0.0,
            y: 0.0,
            blur: self.sigma() * 2.0,
            spread: self.sigma(),
            color: self.color,
        }
    }

    /// The effects of a content glow's group: every pixel in the colour
    /// (its coverage times the colour's alpha), blurred, its coverage
    /// doubled.
    pub(crate) fn effects(self) -> Arc<[Effect]> {
        let mut gain = strand_scene::effect::IDENTITY_MATRIX;
        gain[18] = 2.0;
        Arc::from([
            Effect::ColorMatrix(super::filter::solid(self.color)),
            Effect::Blur {
                radius: self.sigma(),
            },
            Effect::ColorMatrix(gain),
        ])
    }
}

/// `text_stroke: width, paint`'s group effects: the letters' coverage
/// grown by about `width` (blurred by 1.48 × width, then every pixel
/// over a quarter covered made opaque: a Gaussian's quarter level lies
/// 0.674 σ out) in the paint's colour (a gradient's first stop).
pub(crate) fn text_stroke(v: Option<&PropValue>) -> Option<Arc<[Effect]>> {
    let Some(PropValue::Border(strand_scene::Border { width, paint })) = v else {
        return None;
    };
    let w = (*width).min(100.0);
    if !(w.is_finite() && w > 0.0) {
        return None;
    }
    let c = match paint {
        Paint::Solid(c) => *c,
        Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. } => {
            stops.first()?.color
        }
    };
    let mut grow = strand_scene::effect::IDENTITY_MATRIX;
    grow[18] = 8.0;
    grow[19] = -1.0;
    Some(Arc::from([
        Effect::ColorMatrix(super::filter::solid(c)),
        Effect::Blur { radius: w * 1.48 },
        Effect::ColorMatrix(grow),
    ]))
}

/// Draws a copy of the items drawn since `start` (text, an icon, an
/// image) through `effects` in a group just before them (a glow, a text
/// stroke). `frame`, `scale` and `xform` describe the node's box as
/// [`Layer`] does; items' bounds are already in surface space. Returns
/// the group's bounds, and the effects for the node's signature.
pub(crate) fn under_content(
    items: &mut Vec<DisplayItem>,
    start: usize,
    effects: Arc<[Effect]>,
    layer: (kurbo::Rect, f32, Affine),
    surface: Rect,
) -> Option<(Rect, Arc<[Effect]>)> {
    let content = items
        .get(start..)?
        .iter()
        .fold(Rect::default(), |acc, d| match d.item {
            Item::PushClip(_)
            | Item::PushOpacity(_)
            | Item::PushTransform(_)
            | Item::PushLayer(_)
            | Item::PopClip
            | Item::PopOpacity
            | Item::PopTransform
            | Item::PopLayer => acc,
            _ => acc.union(d.bounds),
        });
    if content.is_empty() {
        return None;
    }
    let (frame, scale, xform) = layer;
    let reach = crate::layers::reach_px(&effects, scale);
    let bounds = content.inflate(reach).intersect(surface)?;
    let mut group = Vec::with_capacity(items.len() - start + 2);
    group.push(DisplayItem {
        item: Item::PushLayer(Arc::new(Layer {
            effects: effects.clone(),
            frame,
            scale,
            xform,
        })),
        bounds,
    });
    group.extend(items[start..].iter().cloned());
    group.push(DisplayItem {
        item: Item::PopLayer,
        bounds,
    });
    items.splice(start..start, group);
    Some((bounds, effects))
}

/// What a box's light is drawn around: its frame and outline in physical
/// pixels (in the group's space), the surface's scale and transform.
#[derive(Copy, Clone, Debug)]
pub(crate) struct BoxLight<'a> {
    pub frame: kurbo::Rect,
    pub path: &'a BezPath,
    pub scale: f64,
    pub xform: Affine,
}

/// The pixels `r` covers (physical, untransformed).
fn cover(r: kurbo::Rect) -> Rect {
    let r = r.expand();
    Rect::from_edges(r.x0 as i64, r.y0 as i64, r.x1 as i64, r.y1 as i64)
}

/// `inner_shadow:`'s items, each with the pixels it may touch (in the
/// box's untransformed space): per shadow, a blurred ring around the box's
/// shape (moved by the offset, shrunk by the spread) clipped to the box.
pub(crate) fn inner_shadows(list: &[Shadow], b: BoxLight) -> Vec<(Item, Rect)> {
    let s = b.scale;
    let mut out = Vec::new();
    let (w, h) = (b.frame.width(), b.frame.height());
    if w <= 0.0 || h <= 0.0 {
        return out;
    }
    let phys = cover(b.frame);
    for sh in list {
        if !(sh.color.a.is_finite() && sh.color.a > 0.0) {
            continue;
        }
        let f = |v: f32| if v.is_finite() { v as f64 } else { 0.0 };
        let (dx, dy) = (f(sh.x).clamp(-1e4, 1e4) * s, f(sh.y).clamp(-1e4, 1e4) * s);
        let spread = (f(sh.spread) * s).clamp(0.0, w.min(h) / 2.0);
        // CSS blur radius is twice the Gaussian's standard deviation.
        let sigma = (f(sh.blur).clamp(0.0, super::MAX_RADIUS as f64) / 2.0) as f32;
        // The hole: the box's shape moved by the offset and shrunk by the
        // spread about its centre.
        let c = b.frame.center();
        let k = ((w - 2.0 * spread) / w, (h - 2.0 * spread) / h);
        let hole = Affine::translate((c.x + dx, c.y + dy))
            * Affine::scale_non_uniform(k.0, k.1)
            * Affine::translate((-c.x, -c.y));
        // The ring reaches past the box (moved) by the blur's reach, so
        // the blur sees colour all round the box's edge.
        let pad = (3.0 * sigma as f64 * s).ceil() + 2.0;
        let moved = b.frame.with_origin((b.frame.x0 + dx, b.frame.y0 + dy));
        let outer = b.frame.union(moved).inflate(pad, pad);
        let mut ring = outer.to_path(0.1);
        ring.extend(hole * b.path.clone());
        let region = cover(outer.inflate(pad, pad));
        out.push((Item::PushClip(b.path.clone()), phys));
        let blurred = sigma > 0.0;
        if blurred {
            out.push((
                Item::PushLayer(Arc::new(Layer {
                    effects: Arc::from([Effect::Blur { radius: sigma }]),
                    frame: b.frame,
                    scale: s as f32,
                    xform: b.xform,
                })),
                region,
            ));
        }
        out.push((
            Item::Border {
                path: ring,
                paint: Paint::Solid(sh.color),
                frame: b.frame,
            },
            region,
        ));
        if blurred {
            out.push((Item::PopLayer, region));
        }
        out.push((Item::PopClip, phys));
    }
    out
}

/// `rim: edge, color`'s items: the band between the box's outline and the
/// outline moved one logical pixel inwards from `edge`, clipped to the
/// box.
pub(crate) fn rim(v: Option<&PropValue>, b: BoxLight) -> Vec<(Item, Rect)> {
    let Some(PropValue::List(items)) = v else {
        return Vec::new();
    };
    let edge = items.first().and_then(keyword).and_then(Edge::from_name);
    let c = items.get(1).and_then(color);
    let (Some(edge), Some(c)) = (edge, c) else {
        return Vec::new();
    };
    if !(c.a.is_finite() && c.a > 0.0) || b.frame.width() <= 0.0 || b.frame.height() <= 0.0 {
        return Vec::new();
    }
    let bw = b.scale.round().max(1.0);
    let (dx, dy) = match edge {
        Edge::Top => (0.0, bw),
        Edge::Bottom => (0.0, -bw),
        Edge::Left => (bw, 0.0),
        Edge::Right => (-bw, 0.0),
    };
    let mut band = b.path.clone();
    band.extend(Affine::translate((dx, dy)) * b.path.clone());
    let phys = cover(b.frame);
    vec![
        (Item::PushClip(b.path.clone()), phys),
        (
            Item::Border {
                path: band,
                paint: Paint::Solid(c),
                frame: b.frame,
            },
            phys,
        ),
        (Item::PopClip, phys),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red(a: f32) -> Color {
        Color::from_hex("#ff0000").unwrap().with_alpha(a)
    }

    #[test]
    fn glow_reads_a_radius_and_a_colour() {
        let white = Color::from_hex("#ffffff").unwrap();
        let v = PropValue::List(vec![PropValue::Number(12.0), PropValue::Color(red(0.6))]);
        assert_eq!(
            Glow::of(Some(&v), white),
            Some(Glow {
                radius: 12.0,
                color: red(0.6)
            })
        );
        // A lone radius glows in the node's colour; none, zero or a clear
        // colour is no glow.
        assert_eq!(
            Glow::of(Some(&PropValue::Number(6.0)), white).map(|g| g.color),
            Some(white)
        );
        assert_eq!(Glow::of(None, white), None);
        assert_eq!(Glow::of(Some(&PropValue::Number(0.0)), white), None);
        let clear = PropValue::List(vec![PropValue::Number(4.0), PropValue::Color(red(0.0))]);
        assert_eq!(Glow::of(Some(&clear), white), None);
        // σ = r/4: the box's shadow grows by σ and blurs by 2σ (CSS), the
        // group blurs by σ and reaches 3σ.
        let g = Glow::of(Some(&v), white).unwrap();
        assert_eq!((g.shadow().spread, g.shadow().blur), (3.0, 6.0));
        assert_eq!(crate::layers::reach_px(&g.effects(), 1.0), 9);
    }

    #[test]
    fn rim_and_inner_shadow_clip_to_the_box() {
        let frame = kurbo::Rect::new(10.0, 10.0, 50.0, 30.0);
        let path = frame.to_path(0.1);
        let b = BoxLight {
            frame,
            path: &path,
            scale: 2.0,
            xform: Affine::IDENTITY,
        };
        let v = PropValue::List(vec![
            PropValue::Keyword("top".into()),
            PropValue::Color(red(0.15)),
        ]);
        let items = rim(Some(&v), b);
        assert_eq!(items.len(), 3);
        let Item::Border { path: band, .. } = &items[1].0 else {
            panic!("{items:?}");
        };
        // The band is the top two device pixels at scale 2.
        let inside = |x, y| {
            let p = kurbo::Point::new(x, y);
            band.segments().count() > 0
                && path.contains(p)
                && !(Affine::translate((0.0, 2.0)) * path.clone()).contains(p)
        };
        assert!(inside(30.0, 11.0) && !inside(30.0, 13.0));
        assert!(rim(Some(&PropValue::Number(1.0)), b).is_empty());

        let sh = Shadow {
            x: 0.0,
            y: 2.0,
            blur: 6.0,
            spread: 0.0,
            color: red(0.5),
        };
        let items = inner_shadows(&[sh], b);
        let kinds: Vec<&str> = items
            .iter()
            .map(|(i, _)| match i {
                Item::PushClip(_) => "clip",
                Item::PushLayer(_) => "layer",
                Item::Border { .. } => "ring",
                Item::PopLayer => "/layer",
                Item::PopClip => "/clip",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, ["clip", "layer", "ring", "/layer", "/clip"]);
        // An unblurred one needs no group.
        let sharp = Shadow { blur: 0.0, ..sh };
        assert_eq!(inner_shadows(&[sharp], b).len(), 3);
    }
}

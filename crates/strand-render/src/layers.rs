//! (M4) Effect layers (design.md, "Runtime changes these need", items
//! 1–4): a node's group effects ([`strand_scene::Effect`]) wrap its
//! subtree in a layer, `Item::PushLayer` … `Item::PopLayer` in the display
//! list, like its clip, opacity and transform groups.
//!
//! - Damage: a layer's group bounds, and the records of every node in it,
//!   grow by the effects' reach ([`Effect::reach_of`]), so a change under
//!   a blur repaints all the pixels the blur spreads it to.
//! - vello_cpu lowering ([`push`]): the raster draws per cell, so only
//!   effects that work on one cell's pixels at a time lower to a cell's
//!   `push_layer`: `Opacity` (multiplied), `Blend` (the last one wins) and
//!   `Mask` (`fade`, `radial`: a cell-sized alpha mask, in device pixels
//!   through the group's transform). Effects that read neighbouring
//!   pixels (`Blur`) or a whole group (`ColorMatrix`) are drawn by an
//!   offscreen group ([`crate::offscreen`]), then drawn into each cell as
//!   an image under the cell-local part. The CPU draws a `Shader` pass's
//!   group unfiltered, and `Mask::Shape` is opaque until the shape
//!   library lands (S-effects).
//!
//! Effects are built from props by S-effects (`filter:`, `blend:`,
//! `mask:`, …). Until then the renderer takes them per node from
//! [`crate::Renderer::set_layer_effects`].

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{Anchor, BlendMode, Edge, Effect, Mask, NodeId};
use vello_cpu::RenderContext;
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{self, Compose, Mix};

/// One node's effect group as the display list carries it.
#[derive(Clone, Debug)]
pub struct Layer {
    pub effects: Arc<[Effect]>,
    /// The node's box in physical pixels, in the group's own space (under
    /// its `scale`/`rotate` transform): what masks are relative to.
    pub frame: kurbo::Rect,
    /// The surface's scale: mask lengths are logical.
    pub scale: f32,
    /// The transform in force at the group (its node's and its
    /// ancestors' `scale`/`rotate`): an offscreen group is drawn under it.
    pub xform: Affine,
}

impl Layer {
    /// True if a cell can draw the layer with `push_layer` alone; else
    /// it is an offscreen group ([`crate::offscreen`]).
    pub fn cell_local(&self) -> bool {
        self.effects
            .iter()
            .all(|e| !matches!(e, Effect::Blur { .. } | Effect::ColorMatrix(_)))
    }
}

/// Effects attached to nodes until S-effects builds them from props.
pub type NodeEffects = HashMap<NodeId, Arc<[Effect]>>;

/// How far `effects` spread a group's damage, in physical pixels on a
/// surface at `scale`.
pub fn reach_px(effects: &[Effect], scale: f32) -> u32 {
    let r = Effect::reach_of(effects, scale).top * scale;
    if r.is_finite() {
        r.max(0.0).ceil().min(10_000.0) as u32
    } else {
        0
    }
}

/// Hashes `effects` for damage signatures.
pub fn hash_effects(h: &mut impl Hasher, effects: &[Effect]) {
    // Every effect is plain data whose `Debug` names each field and
    // value: equal effects print equally, and changed ones differ.
    format!("{effects:?}").hash(h);
}

/// The `push_layer` that draws `layer` in one cell of `ctx`; `cur` maps
/// the group's space to the cell (for masks).
pub fn push(ctx: &mut RenderContext, layer: &Layer, cur: Affine) {
    let mut opacity: Option<f32> = None;
    let mut blend = None;
    let mut masks = Vec::new();
    for e in layer.effects.iter() {
        match e {
            Effect::Opacity(o) => {
                let o = if o.is_finite() {
                    o.clamp(0.0, 1.0)
                } else {
                    1.0
                };
                opacity = Some(opacity.unwrap_or(1.0) * o);
            }
            Effect::Blend(b) => blend = Some(blend_mode(*b)),
            Effect::Mask(m @ (Mask::Fade { .. } | Mask::Radial { .. })) => masks.push(m),
            // Drawn by an offscreen group, unfiltered on the CPU, or
            // pending (see the module docs).
            Effect::Mask(Mask::Shape(_))
            | Effect::Blur { .. }
            | Effect::ColorMatrix(_)
            | Effect::Shader(_) => {}
        }
    }
    let mask = (!masks.is_empty()).then(|| cell_mask(ctx, layer, &masks, cur));
    ctx.push_layer(None, blend, opacity, mask, None);
}

/// `blend:` as vello's blend mode: a mix composited source-over, and
/// `add` the Porter-Duff plus. Every mode is per channel, so the raster's
/// swapped red and blue do not change it.
fn blend_mode(b: BlendMode) -> peniko::BlendMode {
    match b {
        BlendMode::Screen => peniko::BlendMode::new(Mix::Screen, Compose::SrcOver),
        BlendMode::Add => peniko::BlendMode::new(Mix::Normal, Compose::Plus),
        BlendMode::Multiply => peniko::BlendMode::new(Mix::Multiply, Compose::SrcOver),
        BlendMode::Overlay => peniko::BlendMode::new(Mix::Overlay, Compose::SrcOver),
        BlendMode::Difference => peniko::BlendMode::new(Mix::Difference, Compose::SrcOver),
    }
}

/// The alpha of `masks` (multiplied) at each pixel of `ctx`'s cell.
fn cell_mask(ctx: &RenderContext, layer: &Layer, masks: &[&Mask], cur: Affine) -> vello_cpu::Mask {
    let (w, h) = (ctx.width(), ctx.height());
    let inv = if cur.determinant().abs() > 1e-12 {
        cur.inverse()
    } else {
        Affine::IDENTITY
    };
    let mut data = Vec::with_capacity(w as usize * h as usize);
    for y in 0..h {
        for x in 0..w {
            // The pixel's centre in the group's space.
            let p = inv * kurbo::Point::new(x as f64 + 0.5, y as f64 + 0.5);
            let a = masks
                .iter()
                .map(|m| mask_alpha(m, layer, p))
                .product::<f64>();
            data.push((a.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    vello_cpu::Mask::from_parts(data, w, h)
}

/// One mask's alpha at `p` (the group's space, physical pixels).
pub fn mask_alpha(m: &Mask, layer: &Layer, p: kurbo::Point) -> f64 {
    let f = layer.frame;
    let s = if layer.scale.is_finite() && layer.scale > 0.0 {
        layer.scale as f64
    } else {
        1.0
    };
    match m {
        // Transparent at the edge, opaque `len` in.
        Mask::Fade { edge, len } => {
            let len = if len.is_finite() {
                *len as f64 * s
            } else {
                0.0
            };
            let d = match edge {
                Edge::Top => p.y - f.y0,
                Edge::Bottom => f.y1 - p.y,
                Edge::Left => p.x - f.x0,
                Edge::Right => f.x1 - p.x,
            };
            if len <= 0.0 {
                if d >= 0.0 { 1.0 } else { 0.0 }
            } else {
                (d / len).clamp(0.0, 1.0)
            }
        }
        // Opaque within `size` of the anchor, an antialiased edge.
        Mask::Radial { at, size } => {
            let size = if size.is_finite() {
                *size as f64 * s
            } else {
                0.0
            };
            let (cx, cy) = anchor_point(*at, f);
            let d = ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt();
            (size - d + 0.5).clamp(0.0, 1.0)
        }
        Mask::Shape(_) => 1.0,
    }
}

/// The point `at` names in `f`.
fn anchor_point(at: Anchor, f: kurbo::Rect) -> (f64, f64) {
    let c = f.center();
    match at {
        Anchor::Center => (c.x, c.y),
        Anchor::Top => (c.x, f.y0),
        Anchor::Bottom => (c.x, f.y1),
        Anchor::Left => (f.x0, c.y),
        Anchor::Right => (f.x1, c.y),
        Anchor::TopLeft => (f.x0, f.y0),
        Anchor::TopRight => (f.x1, f.y0),
        Anchor::BottomLeft => (f.x0, f.y1),
        Anchor::BottomRight => (f.x1, f.y1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(effects: Vec<Effect>) -> Layer {
        Layer {
            effects: effects.into(),
            frame: kurbo::Rect::new(10.0, 10.0, 110.0, 50.0),
            scale: 2.0,
            xform: Affine::IDENTITY,
        }
    }

    #[test]
    fn masks_fade_from_their_edge_and_reveal_a_circle() {
        let fade = Mask::Fade {
            edge: Edge::Bottom,
            len: 10.0,
        };
        let l = layer(vec![]);
        let at = |m: &Mask, x, y| mask_alpha(m, &l, kurbo::Point::new(x, y));
        assert_eq!(at(&fade, 50.0, 50.0), 0.0, "transparent at the edge");
        assert_eq!(at(&fade, 50.0, 40.0), 0.5, "10 logical px = 20 physical");
        assert_eq!(at(&fade, 50.0, 20.0), 1.0);
        let radial = Mask::Radial {
            at: Anchor::TopLeft,
            size: 10.0,
        };
        assert_eq!(at(&radial, 10.0, 10.0), 1.0);
        assert_eq!(at(&radial, 29.0, 10.0), 1.0);
        assert_eq!(at(&radial, 40.0, 10.0), 0.0);
        assert_eq!(at(&Mask::Shape("cookie".into()), 0.0, 0.0), 1.0);
    }

    #[test]
    fn reach_is_physical_and_only_spatial_effects_leave_the_cell() {
        let blur = vec![Effect::Blur { radius: 2.0 }, Effect::Opacity(0.5)];
        assert_eq!(reach_px(&blur, 2.0), 12, "3σ of 2 logical px at 2×");
        assert_eq!(reach_px(&[Effect::Opacity(0.5)], 2.0), 0);
        assert_eq!(reach_px(&[Effect::Blur { radius: f32::NAN }], 1.0), 0);
        assert!(!layer(blur).cell_local());
        assert!(layer(vec![Effect::Blend(BlendMode::Screen)]).cell_local());
        let gray = Effect::ColorMatrix(strand_scene::effect::IDENTITY_MATRIX);
        assert!(!layer(vec![gray]).cell_local());
    }
}

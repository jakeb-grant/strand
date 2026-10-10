//! (M4) `backdrop:`: a filter over the surface's own content behind a
//! node (design.md, "Filters and compositing": `backdrop: blur(16)`
//! "inside a surface", "drawn at quarter scale, promoted to GPU above
//! about 0.2 Mpx"; "Bundled GPU effects": `backdrop: glass()` "without a
//! usable GPU … glass becomes blur and tint").
//!
//! The flattener puts an empty effect layer, clipped to the node's
//! outline, just under its background: its one effect is a shader pass
//! whose input is [`ShaderInput::Backdrop`] (`Bundled::BackdropBlur`, or
//! `Bundled::Glass`). On the CPU the layer is an offscreen group
//! ([`crate::offscreen`]) whose pixels are not its own items but what the
//! display list drew before it ([`behind`]): drawn at reduced scale
//! (a quarter; half or full for blurs too small to survive it), blurred
//! there, scaled back up, and tinted for glass. The group is cached by
//! the hash of what is behind it, and the node's damage signature hashes
//! the same items ([`hash_behind`]), so a change behind it repaints it.
//!
//! What is behind is the earlier items over the node's box grown by the
//! blur's reach. Groups still open at the node (its ancestors' clips,
//! transforms and layers) are left open: their items draw as they are.
//! A backdrop inside an offscreen ancestor shows nothing behind it (the
//! ancestor is drawn before backdrops are).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{
    Bundled, Color, Effect, PropValue, Rect, Scale, ShaderInput, ShaderPass, ShaderRef,
};
use vello_cpu::kurbo::Affine;
use vello_cpu::{
    Pixmap, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources, TargetInit,
};

use crate::cache::PaintCache;
use crate::flatten::{DisplayItem, Item};
use crate::layers::Layer;
use crate::offscreen::{Drawn, layer_key};
use crate::raster::AtlasMirror;

/// `glass()`'s refraction at its rim when not given, logical pixels.
pub(crate) const GLASS_REFRACTION: f32 = 12.0;

/// `glass()`'s CPU fallback: a blur of this radius (logical pixels)…
pub(crate) const GLASS_BLUR: f32 = 16.0;

/// …under white at this alpha.
pub(crate) const GLASS_TINT: f32 = 0.12;

/// The effect `backdrop:`'s value gives a node on a surface at `scale`:
/// `blur(r)` or `glass()`. `None` for anything else (the checker reports
/// it) or a blur of nothing.
pub(crate) fn effect(v: &PropValue, scale: f32) -> Option<Effect> {
    let PropValue::Call { name, args } = v else {
        return None;
    };
    let (code, uniforms) = match name.as_str() {
        "blur" => {
            let r = args
                .first()
                .and_then(crate::effects::number)?
                .clamp(0.0, crate::effects::MAX_RADIUS);
            if r <= 0.0 {
                return None;
            }
            (Bundled::BackdropBlur, vec![r * scale])
        }
        // `glass(refraction = 12)`: the GPU's knob (its CPU fallback is
        // one blur and tint, `GLASS_BLUR`, `GLASS_TINT`).
        "glass" => {
            let refraction = args
                .first()
                .and_then(crate::effects::number)
                .unwrap_or(GLASS_REFRACTION)
                .clamp(0.0, 200.0);
            (Bundled::Glass, vec![refraction * scale])
        }
        _ => return None,
    };
    Some(Effect::Shader(ShaderPass {
        code: ShaderRef::Bundled(code),
        uniforms: uniforms.into(),
        input: ShaderInput::Backdrop,
    }))
}

/// A backdrop pass as the CPU draws it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Pass {
    /// The blur's standard deviation, physical pixels.
    pub sigma: f32,
    /// Drawn over the blur (glass).
    pub tint: Option<Color>,
}

impl Pass {
    /// How far the blur reads past the box, physical pixels.
    fn reach(self) -> u32 {
        (3.0 * self.sigma).ceil().clamp(0.0, 10_000.0) as u32
    }

    /// The scale it is drawn at: a quarter (design.md), or half or full
    /// size for a blur under 8 or 2 device pixels, which a quarter-scale
    /// pass would turn into blocks.
    fn reduction(self) -> u32 {
        if self.sigma >= 8.0 {
            4
        } else if self.sigma >= 2.0 {
            2
        } else {
            1
        }
    }
}

/// True if `e` is a backdrop pass.
pub(crate) fn is_backdrop(e: &Effect) -> bool {
    matches!(
        e,
        Effect::Shader(ShaderPass {
            input: ShaderInput::Backdrop,
            ..
        })
    )
}

/// The CPU pass of a layer that draws a backdrop, if it does.
pub(crate) fn pass(layer: &Layer, scale: Scale) -> Option<Pass> {
    let s = scale.as_f32();
    layer.effects.iter().find_map(|e| match e {
        Effect::Shader(ShaderPass {
            code: ShaderRef::Bundled(Bundled::BackdropBlur),
            uniforms,
            input: ShaderInput::Backdrop,
        }) => Some(Pass {
            sigma: uniforms
                .first()
                .copied()
                .filter(|v| v.is_finite())?
                .max(0.0),
            tint: None,
        }),
        Effect::Shader(ShaderPass {
            code: ShaderRef::Bundled(Bundled::Glass),
            input: ShaderInput::Backdrop,
            ..
        }) => Some(Pass {
            sigma: GLASS_BLUR * s,
            tint: Some(Color::WHITE.with_alpha(GLASS_TINT)),
        }),
        _ => None,
    })
}

/// The reach a node's backdrop effect reads past its box, physical
/// pixels (`effect` from [`effect`]).
pub(crate) fn reach_of(e: &Effect, scale: Scale) -> u32 {
    let layer = Layer {
        effects: Arc::from([e.clone()]),
        frame: Default::default(),
        scale: scale.as_f32(),
        xform: Affine::IDENTITY,
        gpu: None,
    };
    pass(&layer, scale).map_or(0, Pass::reach)
}

/// The items before `end` that draw over `region`, with the groups still
/// open at `end` left open (their push markers dropped): a balanced list.
pub(crate) fn behind(items: &[DisplayItem], end: usize, region: Rect) -> Vec<DisplayItem> {
    let end = end.min(items.len());
    let mut open: Vec<usize> = Vec::new();
    let mut keep = vec![true; end];
    for (i, d) in items[..end].iter().enumerate() {
        match d.item {
            Item::PushClip(_)
            | Item::PushOpacity(_)
            | Item::PushTransform(_)
            | Item::PushLayer(_) => open.push(i),
            Item::PopClip | Item::PopOpacity | Item::PopTransform | Item::PopLayer
                if open.pop().is_none() =>
            {
                keep[i] = false;
            }
            _ => {}
        }
    }
    for i in open {
        keep[i] = false;
    }
    items[..end]
        .iter()
        .zip(keep)
        .filter(|(d, k)| *k && (is_marker(&d.item) || d.bounds.intersects(region)))
        .map(|(d, _)| d.clone())
        .collect()
}

fn is_marker(i: &Item) -> bool {
    matches!(
        i,
        Item::PushClip(_)
            | Item::PushOpacity(_)
            | Item::PushTransform(_)
            | Item::PushLayer(_)
            | Item::PopClip
            | Item::PopOpacity
            | Item::PopTransform
            | Item::PopLayer
    )
}

/// Hashes the items before `end` that draw over `region` (a node's damage
/// signature: a change behind its backdrop repaints it).
pub(crate) fn hash_behind(items: &[DisplayItem], end: usize, region: Rect, h: &mut impl Hasher) {
    for d in items[..end.min(items.len())].iter() {
        if !is_marker(&d.item) && d.bounds.intersects(region) {
            crate::flatten::hash_item(h, &d.item);
            (d.bounds.x, d.bounds.y, d.bounds.w, d.bounds.h).hash(h);
        }
    }
}

/// The cache key of the backdrop group at `i` over `region`.
pub(crate) fn key(
    items: &[DisplayItem],
    i: usize,
    pass: Pass,
    region: Rect,
    scale: Scale,
    groups: &HashMap<usize, Drawn>,
) -> u64 {
    let mut h = DefaultHasher::new();
    (
        "backdrop",
        region.x,
        region.y,
        region.w,
        region.h,
        scale.as_f32().to_bits(),
        pass.sigma.to_bits(),
        pass.tint.map(|c| [c.r, c.g, c.b, c.a].map(f32::to_bits)),
    )
        .hash(&mut h);
    let read = region.inflate(pass.reach());
    for d in behind(items, i, read) {
        crate::flatten::hash_item(&mut h, &d.item);
        if let Item::PushLayer(l) = &d.item
            && let Some(n) = groups.get(&layer_key(l))
        {
            (Arc::as_ptr(&n.pixmap) as usize).hash(&mut h);
        }
    }
    h.finish()
}

/// Draws the backdrop group at `i` (its layer `layer`) over `region` of a
/// surface `surface`: what is behind, blurred at reduced scale, scaled up
/// to `region`, tinted. `None` when the region is empty or too large.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    items: &[DisplayItem],
    i: usize,
    pass: Pass,
    region: Rect,
    surface: Rect,
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    groups: &HashMap<usize, Drawn>,
) -> Option<Drawn> {
    let read = region.inflate(pass.reach()).intersect(surface)?;
    let k = pass.reduction();
    let (sw, sh) = (read.w.div_ceil(k).max(1), read.h.div_ceil(k).max(1));
    let (sw16, sh16) = (u16::try_from(sw).ok()?, u16::try_from(sh).ok()?);
    let below = behind(items, i, read);
    let settings = RenderSettings {
        num_threads: 0,
        ..RenderSettings::default()
    };
    let mut ctx = RenderContext::new_with(sw16, sh16, settings);
    let base =
        Affine::scale(1.0 / k as f64) * Affine::translate((-(read.x as f64), -(read.y as f64)));
    ctx.set_transform(base);
    crate::raster::draw_group(
        &mut ctx,
        &below,
        read,
        atlas,
        cache,
        scale,
        base,
        Affine::IDENTITY,
        groups,
    );
    ctx.flush();
    let mut small = Pixmap::new(sw16, sh16);
    let mut resources = Resources::new();
    ctx.render_with(
        small.as_mut(),
        &mut resources,
        RasterizerSettings {
            target_init: TargetInit::SrcOver,
            render_mode: RenderMode::OptimizeQuality,
            ..RasterizerSettings::default()
        },
    );
    drop(ctx);
    crate::offscreen::blur(
        small.data_as_u8_slice_mut(),
        sw as usize,
        sh as usize,
        pass.sigma / k as f32,
    );
    let (w16, h16) = (u16::try_from(region.w).ok()?, u16::try_from(region.h).ok()?);
    if w16 == 0 || h16 == 0 {
        return None;
    }
    let mut out = Pixmap::new(w16, h16);
    upscale(
        small.data_as_u8_slice(),
        (sw as usize, sh as usize),
        out.data_as_u8_slice_mut(),
        (region.w as usize, region.h as usize),
        (
            (region.x - read.x) as f64 / k as f64,
            (region.y - read.y) as f64 / k as f64,
        ),
        k as f64,
    );
    if let Some(c) = pass.tint {
        tint_over(out.data_as_u8_slice_mut(), c);
    }
    Some(Drawn {
        pixmap: Arc::new(out),
        x: region.x,
        y: region.y,
    })
}

/// Bilinear upscaling by `k` of `src` (premultiplied, `sw × sh`) into
/// `dst` (`dw × dh`), whose origin is at `origin` in `src`'s pixels.
fn upscale(
    src: &[u8],
    (sw, sh): (usize, usize),
    dst: &mut [u8],
    (dw, dh): (usize, usize),
    origin: (f64, f64),
    k: f64,
) {
    if sw == 0 || sh == 0 {
        return;
    }
    let px =
        |x: usize, y: usize, c: usize| src[(y.min(sh - 1) * sw + x.min(sw - 1)) * 4 + c] as f64;
    for y in 0..dh {
        // Pixel centres.
        let fy = (origin.1 + (y as f64 + 0.5) / k - 0.5).max(0.0);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        for x in 0..dw {
            let fx = (origin.0 + (x as f64 + 0.5) / k - 0.5).max(0.0);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let o = (y * dw + x) * 4;
            for c in 0..4 {
                let top = px(x0, y0, c) * (1.0 - tx) + px(x0 + 1, y0, c) * tx;
                let bottom = px(x0, y0 + 1, c) * (1.0 - tx) + px(x0 + 1, y0 + 1, c) * tx;
                dst[o + c] = (top * (1.0 - ty) + bottom * ty).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// Draws `c` over every pixel of `px` (premultiplied BGRA).
fn tint_over(px: &mut [u8], c: Color) {
    let c = c.clamped();
    let a = c.a;
    let src = [c.b * a, c.g * a, c.r * a, a].map(|v| v * 255.0);
    for p in px.chunks_exact_mut(4) {
        for i in 0..4 {
            p[i] = (src[i] + p[i] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_values_become_backdrop_passes() {
        let call = |name: &str, args| PropValue::Call {
            name: name.into(),
            args,
        };
        let s = Scale::new(240).unwrap();
        let blur = effect(&call("blur", vec![PropValue::Number(16.0)]), 2.0).unwrap();
        assert!(is_backdrop(&blur));
        let layer = |e: Effect| Layer {
            effects: Arc::from([e]),
            frame: Default::default(),
            scale: 2.0,
            xform: Affine::IDENTITY,
            gpu: None,
        };
        assert_eq!(
            pass(&layer(blur.clone()), s),
            Some(Pass {
                sigma: 32.0,
                tint: None
            })
        );
        assert_eq!(reach_of(&blur, s), 96);
        let glass = effect(&call("glass", vec![]), 2.0).unwrap();
        // The GPU's refraction knob, 12 when not given, in buffer pixels.
        let refraction = |e: &Effect| match e {
            Effect::Shader(p) => p.uniforms.to_vec(),
            _ => Vec::new(),
        };
        assert_eq!(refraction(&glass), vec![24.0]);
        let deep = effect(&call("glass", vec![PropValue::Number(20.0)]), 2.0).unwrap();
        assert_eq!(refraction(&deep), vec![40.0]);
        let p = pass(&layer(glass), s).unwrap();
        assert_eq!(p.sigma, 32.0);
        assert!(p.tint.is_some());
        assert_eq!(
            effect(&call("blur", vec![PropValue::Number(0.0)]), 1.0),
            None
        );
        assert_eq!(
            effect(&call("grayscale", vec![PropValue::Number(1.0)]), 1.0),
            None
        );
        // A quarter scale for real blurs, finer for small ones.
        assert_eq!(
            Pass {
                sigma: 16.0,
                tint: None
            }
            .reduction(),
            4
        );
        assert_eq!(
            Pass {
                sigma: 3.0,
                tint: None
            }
            .reduction(),
            2
        );
        assert_eq!(
            Pass {
                sigma: 1.0,
                tint: None
            }
            .reduction(),
            1
        );
    }

    #[test]
    fn behind_leaves_open_groups_open() {
        let d = |item| DisplayItem {
            item,
            bounds: Rect::new(0, 0, 10, 10),
        };
        let path = vello_cpu::kurbo::BezPath::new();
        let items = vec![
            d(Item::PushClip(path.clone())),
            d(Item::PushOpacity(0.5)),
            d(Item::PopOpacity),
            d(Item::PushClip(path)),
        ];
        let got = behind(&items, 4, Rect::new(0, 0, 10, 10));
        // The first and last clips are open at the end: dropped.
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0].item, Item::PushOpacity(_)));
    }

    #[test]
    fn upscale_is_bilinear() {
        // Two pixels, black and white (opaque), doubled: the inner two
        // are between.
        let src = [0, 0, 0, 255, 255, 255, 255, 255];
        let mut dst = [0u8; 16];
        upscale(&src, (2, 1), &mut dst, (4, 1), (0.0, 0.0), 2.0);
        let v: Vec<u8> = dst.chunks(4).map(|p| p[0]).collect();
        assert_eq!(v[0], 0);
        assert_eq!(v[3], 255);
        assert!(v[1] > 0 && v[1] < v[2] && v[2] < 255, "{v:?}");
    }
}

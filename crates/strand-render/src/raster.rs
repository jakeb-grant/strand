//! vello_cpu backend: paints a display list into a `wl_shm` buffer, only
//! inside the damage.
//!
//! vello_cpu writes premultiplied RGBA8; `wl_shm` ARGB8888 little-endian is
//! premultiplied BGRA8 in memory. Compositing is per channel, so painting
//! every colour with red and blue swapped produces BGRA bytes directly,
//! without a conversion pass. The damaged rectangles are cleared and the
//! scene is drawn source-over under a clip of exactly those rectangles, so
//! pixels outside the damage keep the buffer's previous contents.

use std::collections::HashMap;
use std::sync::Arc;

use strand_scene::{Color, Damage, Paint, PaintTarget, Rect, Scale};
use strand_text::{AtlasUpload, PageId};
use vello_cpu::color::{AlphaColor, ColorSpaceTag, PremulRgba8, Srgb};
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{Extend, Fill, Gradient, ImageQuality, ImageSampler};
use vello_cpu::{
    Image, ImageSource, PaintType, Pixmap, PixmapMut, RasterizerSettings, RenderContext,
    RenderMode, RenderSettings, Resources, TargetInit, Tint, TintMode,
};

use crate::flatten::{DisplayItem, FillShape, Item};

/// The render thread's copy of the text worker's glyph atlases, as vello
/// pixmaps (white, premultiplied, coverage in every channel).
#[derive(Debug, Default)]
pub struct AtlasMirror {
    pages: HashMap<(Scale, u32), MirrorPage>,
}

#[derive(Debug)]
struct MirrorPage {
    generation: u32,
    pixmap: Arc<Pixmap>,
}

impl AtlasMirror {
    pub fn apply(&mut self, up: &AtlasUpload) {
        let size = up.page_size;
        let page = self
            .pages
            .entry((up.page.scale, up.page.index))
            .or_insert_with(|| MirrorPage {
                generation: up.page.generation,
                pixmap: Arc::new(Pixmap::new(size, size)),
            });
        if page.generation != up.page.generation
            || page.pixmap.width() != size
            || page.pixmap.height() != size
        {
            page.generation = up.page.generation;
            page.pixmap = Arc::new(Pixmap::new(size, size));
        }
        let pm = Arc::make_mut(&mut page.pixmap);
        let data = pm.data_mut();
        let (x, y, w, h) = (up.x as usize, up.y as usize, up.w as usize, up.h as usize);
        let stride = size as usize;
        if x + w > stride || y + h > stride || up.alpha.len() < w * h {
            return;
        }
        for row in 0..h {
            let src = &up.alpha[row * w..row * w + w];
            let dst = &mut data[(y + row) * stride + x..(y + row) * stride + x + w];
            for (d, &a) in dst.iter_mut().zip(src) {
                *d = PremulRgba8 {
                    r: a,
                    g: a,
                    b: a,
                    a,
                };
            }
        }
    }

    pub fn page(&self, id: PageId) -> Option<&Arc<Pixmap>> {
        self.pages
            .get(&(id.scale, id.index))
            .filter(|p| p.generation == id.generation)
            .map(|p| &p.pixmap)
    }

    /// Drops pages of scales no surface uses any more.
    pub fn retain_scales(&mut self, keep: impl Fn(Scale) -> bool) {
        self.pages.retain(|(s, _), _| keep(*s));
    }
}

/// A colour for vello with red and blue swapped (see the module docs).
fn bgra(c: Color) -> AlphaColor<Srgb> {
    let c = c.clamped();
    AlphaColor::new([c.b, c.g, c.r, c.a])
}

fn stops(stops: &[strand_scene::GradientStop]) -> Vec<(f32, AlphaColor<Srgb>)> {
    stops
        .iter()
        .map(|s| (s.offset.clamp(0.0, 1.0), bgra(s.color)))
        .collect()
}

fn gradient(mut g: Gradient, s: &[strand_scene::GradientStop]) -> PaintType {
    // Interpolate per channel in sRGB: symmetric in red and blue, so the
    // swap stays valid. (A perceptual space would mix the swapped channels.)
    g.interpolation_cs = ColorSpaceTag::Srgb;
    g.extend = Extend::Pad;
    PaintType::Gradient(g.with_stops(stops(s).as_slice()))
}

fn paint_type(p: &Paint, frame: kurbo::Rect) -> PaintType {
    let center = frame.center();
    match p {
        Paint::Solid(c) => PaintType::Solid(bgra(*c)),
        Paint::Linear { stops: s, .. }
        | Paint::Radial { stops: s }
        | Paint::Conic { stops: s, .. }
            if s.len() < 2 =>
        {
            PaintType::Solid(
                s.first()
                    .map_or(AlphaColor::TRANSPARENT, |st| bgra(st.color)),
            )
        }
        Paint::Linear { angle, stops: s } => {
            // CSS: 0deg points up, angles run clockwise, and the gradient
            // line is long enough for the corners to hit the end colours.
            let a = (*angle as f64).to_radians();
            let (sin, cos) = a.sin_cos();
            let len = (frame.width() * sin).abs() + (frame.height() * cos).abs();
            let d = kurbo::Vec2::new(sin, -cos) * (len / 2.0);
            gradient(Gradient::new_linear(center - d, center + d), s)
        }
        Paint::Radial { stops: s } => {
            let r = (frame.width() / 2.0).hypot(frame.height() / 2.0);
            gradient(Gradient::new_radial(center, r as f32), s)
        }
        Paint::Conic { from, stops: s } => {
            // CSS conic angles start at the top; kurbo's at +x.
            let start = (*from - 90.0).to_radians();
            gradient(
                Gradient::new_sweep(center, start, start + std::f32::consts::TAU),
                s,
            )
        }
    }
}

/// Render contexts kept for distinct buffer sizes.
const MAX_CONTEXTS: usize = 4;

/// Owns the vello contexts and scratch space between frames.
#[derive(Debug, Default)]
pub struct Raster {
    /// One context per buffer size in use (outputs of different sizes
    /// would otherwise reallocate it every frame), most recent first.
    contexts: Vec<RenderContext>,
    resources: Option<Resources>,
    scratch: Vec<u8>,
}

impl Raster {
    /// Paints `items` into `target`, touching only pixels inside `damage`.
    /// Returns false if the target is too large for vello_cpu (dimensions
    /// above `u16::MAX`).
    pub fn paint(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        atlas: &AtlasMirror,
        scale: Scale,
        target: &mut PaintTarget<'_>,
    ) -> bool {
        let (Ok(w), Ok(h)) = (u16::try_from(target.size.w), u16::try_from(target.size.h)) else {
            return false;
        };
        if w == 0 || h == 0 || damage.is_empty() || target.validate().is_err() {
            return true;
        }
        let damage = damage.clipped(target.bounds());
        match self
            .contexts
            .iter()
            .position(|c| c.width() == w && c.height() == h)
        {
            Some(i) => {
                let c = self.contexts.remove(i);
                self.contexts.insert(0, c);
            }
            None => {
                self.contexts
                    .insert(0, RenderContext::new_with(w, h, RenderSettings::default()));
                self.contexts.truncate(MAX_CONTEXTS);
            }
        }
        let ctx = &mut self.contexts[0];
        ctx.reset();
        let resources = self.resources.get_or_insert_with(Resources::new);

        clear(target, &damage);

        // Clip to exactly the damage, one disjoint rectangle at a time.
        // Pixel-aligned rect clips leave coverage inside them untouched, so
        // a partial repaint is bit-identical to a full one; a multi-rect clip
        // path would round differently where layers composite through it.
        for clip in disjoint(&damage) {
            ctx.push_clip_rect(&to_kurbo(clip));
            draw(ctx, items, clip, atlas, scale);
            ctx.pop_clip();
        }
        ctx.flush();

        // The f32 pipeline: the u8 one rounds differently where a shape is
        // composited through a clip edge than where it is not, so a partial
        // repaint would differ from a full one by ±1 near damage borders.
        let settings = RasterizerSettings {
            target_init: TargetInit::SrcOver,
            render_mode: RenderMode::OptimizeQuality,
            ..RasterizerSettings::default()
        };
        let row = w as usize * 4;
        let stride = target.stride as usize;
        if stride == row {
            let len = row * h as usize;
            if let Some(pm) = PixmapMut::new(w, h, &mut target.pixels[..len]) {
                ctx.render_with(pm, resources, settings);
            }
        } else {
            // Padded rows: render through a tightly packed copy.
            self.scratch.resize(row * h as usize, 0);
            for y in 0..h as usize {
                self.scratch[y * row..(y + 1) * row]
                    .copy_from_slice(&target.pixels[y * stride..y * stride + row]);
            }
            if let Some(pm) = PixmapMut::new(w, h, &mut self.scratch) {
                ctx.render_with(pm, resources, settings);
            }
            for r in damage.rects() {
                let (x0, x1) = (r.left() as usize * 4, r.right() as usize * 4);
                for y in r.top() as usize..r.bottom() as usize {
                    target.pixels[y * stride + x0..y * stride + x1]
                        .copy_from_slice(&self.scratch[y * row + x0..y * row + x1]);
                }
            }
        }
        // Release image references so atlas pages can be updated in place.
        ctx.reset();
        true
    }
}

/// Encodes the display items that touch `clip`.
fn draw(
    ctx: &mut RenderContext,
    items: &[DisplayItem],
    clip: Rect,
    atlas: &AtlasMirror,
    scale: Scale,
) {
    let touches = |b: &Rect| clip.intersects(*b);
    for d in items {
        match &d.item {
            Item::PushClip(p) => ctx.push_clip_path(p),
            Item::PopClip => ctx.pop_clip(),
            Item::PushOpacity(o) => ctx.push_opacity_layer(*o),
            Item::PopOpacity => ctx.pop_layer(),
            _ if !touches(&d.bounds) => {}
            Item::Shadow {
                rect,
                radius,
                std_dev,
                color,
                clip,
            } => {
                ctx.set_fill_rule(Fill::EvenOdd);
                ctx.push_clip_path(clip);
                ctx.set_fill_rule(Fill::NonZero);
                ctx.set_paint(bgra(*color));
                ctx.fill_blurred_rounded_rect(rect, *radius, *std_dev, false);
                ctx.pop_clip();
            }
            Item::Fill {
                shape,
                paint,
                frame,
            } => {
                ctx.set_paint(paint_type(paint, *frame));
                match shape {
                    FillShape::Rect(r) => ctx.fill_rect(r),
                    FillShape::Path(p) => ctx.fill_path(p),
                }
            }
            Item::Border { path, paint, frame } => {
                ctx.set_paint(paint_type(paint, *frame));
                ctx.set_fill_rule(Fill::EvenOdd);
                ctx.fill_path(path);
                ctx.set_fill_rule(Fill::NonZero);
            }
            Item::Glyphs {
                x,
                y,
                layout,
                color,
            } => {
                ctx.set_tint(Some(Tint {
                    color: bgra(*color),
                    mode: TintMode::AlphaMask,
                }));
                // A layout shaped for another scale (the surface moved
                // to a different output) is drawn resampled until the
                // re-shaped one arrives.
                let k = scale.as_f64() / layout.scale.as_f64();
                let quality = if k == 1.0 {
                    ImageQuality::Low
                } else {
                    ImageQuality::Medium
                };
                ctx.set_transform(Affine::translate((*x as f64, *y as f64)) * Affine::scale(k));
                for g in layout.glyphs() {
                    let Some(page) = atlas.page(g.slot.page) else {
                        continue;
                    };
                    let gb = Rect::new(
                        *x + (g.x as f64 * k).floor() as i32,
                        *y + (g.y as f64 * k).floor() as i32,
                        (g.slot.w as f64 * k).ceil() as u32 + 1,
                        (g.slot.h as f64 * k).ceil() as u32 + 1,
                    );
                    if !touches(&gb) {
                        continue;
                    }
                    ctx.set_paint(PaintType::Image(Image {
                        image: ImageSource::Pixmap(page.clone()),
                        sampler: ImageSampler {
                            x_extend: Extend::Pad,
                            y_extend: Extend::Pad,
                            quality,
                            alpha: 1.0,
                        },
                    }));
                    ctx.set_paint_transform(Affine::translate((
                        (g.x - g.slot.x as i32) as f64,
                        (g.y - g.slot.y as i32) as f64,
                    )));
                    ctx.fill_rect(&kurbo::Rect::new(
                        g.x as f64,
                        g.y as f64,
                        (g.x + g.slot.w as i32) as f64,
                        (g.y + g.slot.h as i32) as f64,
                    ));
                }
                ctx.reset_tint();
                ctx.reset_paint_transform();
                ctx.reset_transform();
            }
        }
    }
}

fn to_kurbo(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        r.left() as f64,
        r.top() as f64,
        r.right() as f64,
        r.bottom() as f64,
    )
}

/// Splits damage into disjoint rectangles covering exactly the same
/// pixels: horizontal bands between rectangle edges, each band's covered
/// x-intervals merged, and vertically adjacent identical spans joined.
fn disjoint(damage: &Damage) -> Vec<Rect> {
    let rects = damage.rects();
    if rects.len() <= 1 {
        return rects.to_vec();
    }
    let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r.top(), r.bottom()]).collect();
    ys.sort_unstable();
    ys.dedup();
    let mut out: Vec<Rect> = Vec::new();
    // Spans of the previous band, as indices into `out`, for joining.
    let mut open: Vec<usize> = Vec::new();
    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        let mut spans: Vec<(i64, i64)> = rects
            .iter()
            .filter(|r| r.top() <= y0 && r.bottom() >= y1)
            .map(|r| (r.left(), r.right()))
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (a, b) in spans {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let mut next_open = Vec::new();
        for (a, b) in merged {
            let joined = open.iter().copied().find(|&i| {
                let r = out[i];
                r.left() == a && r.right() == b && r.bottom() == y0
            });
            match joined {
                Some(i) => {
                    out[i] = Rect::from_edges(a, out[i].top(), b, y1);
                    next_open.push(i);
                }
                None => {
                    out.push(Rect::from_edges(a, y0, b, y1));
                    next_open.push(out.len() - 1);
                }
            }
        }
        open = next_open;
    }
    out
}

/// Zeroes the damaged pixels (transparent) before drawing over them.
fn clear(target: &mut PaintTarget<'_>, damage: &Damage) {
    let stride = target.stride as usize;
    for r in damage.rects() {
        let (x0, x1) = (r.left() as usize * 4, r.right() as usize * 4);
        for y in r.top() as usize..r.bottom() as usize {
            target.pixels[y * stride + x0..y * stride + x1].fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_covers_exactly_without_overlap() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move |n: i64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as i64
        };
        for _ in 0..500 {
            let mut d = Damage::new();
            for _ in 0..rnd(12) {
                d.add(Rect::new(
                    rnd(60) as i32,
                    rnd(60) as i32,
                    rnd(30) as u32,
                    rnd(30) as u32,
                ));
            }
            let parts = disjoint(&d);
            let sum: u64 = parts.iter().map(|r| r.area()).sum();
            assert_eq!(sum, d.area(), "{d:?} -> {parts:?}");
            for (i, a) in parts.iter().enumerate() {
                assert!(d.covers(*a));
                for b in &parts[i + 1..] {
                    assert!(!a.intersects(*b), "{a:?} overlaps {b:?}");
                }
            }
        }
    }
}

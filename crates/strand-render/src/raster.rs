//! vello_cpu backend: paints a display list into a `wl_shm` buffer, only
//! inside the damage.
//!
//! vello_cpu writes premultiplied RGBA8; `wl_shm` ARGB8888 little-endian is
//! premultiplied BGRA8 in memory. Compositing is per channel, so painting
//! every colour with red and blue swapped produces BGRA bytes directly,
//! without a conversion pass. The damaged rectangles are cleared and the
//! scene is drawn source-over under a clip of exactly those rectangles, one
//! fixed-grid cell at a time, so pixels outside the damage keep the
//! buffer's previous contents and cost follows the damage.

use std::collections::HashMap;
use std::sync::Arc;

use strand_scene::{Color, Damage, GradientStop, Paint, PaintTarget, Rect, Scale};
use strand_text::{AtlasUpload, PageId};
use vello_cpu::color::{AlphaColor, ColorSpaceTag, PremulRgba8, Srgb};
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{Extend, Fill, Gradient, ImageQuality, ImageSampler};
use vello_cpu::{
    Image, ImageSource, PaintType, Pixmap, PixmapMut, RasterizerSettings, RenderContext,
    RenderMode, RenderSettings, Resources, TargetInit, Tint, TintMode,
};

use crate::cache::{PaintCache, ShadowShape, draw_shadow, gradient_key, image_paint, shadow_key};
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

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Drops this scale's pages that are not in `live` (the worker trimmed
    /// or reset them; see `TextLayout::atlas_pages`).
    pub fn retain_pages(&mut self, scale: Scale, live: &[PageId]) {
        self.pages.retain(|(s, index), p| {
            *s != scale
                || live
                    .iter()
                    .any(|id| id.index == *index && id.generation == p.generation)
        });
    }

    /// Bytes of mirrored pixels held for `scale`.
    pub fn bytes(&self, scale: Scale) -> usize {
        self.pages
            .iter()
            .filter(|((s, _), _)| *s == scale)
            .map(|(_, p)| p.pixmap.width() as usize * p.pixmap.height() as usize * 4)
            .sum()
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

/// Samples between adjacent stops, so vello's per-channel sRGB
/// interpolation follows the OKLab ramp design asks for.
const GRADIENT_SAMPLES: usize = 8;

/// Gradient stops for vello. Colours are interpolated in premultiplied
/// OKLab by sampling each segment, then handed over with red and blue
/// swapped; vello interpolates per channel in sRGB between the samples,
/// which is symmetric in red and blue, so the swap stays valid.
fn stops(stops: &[GradientStop]) -> Vec<(f32, AlphaColor<Srgb>)> {
    let clean: Vec<GradientStop> = stops
        .iter()
        .map(|s| GradientStop {
            offset: if s.offset.is_finite() {
                s.offset.clamp(0.0, 1.0)
            } else {
                0.0
            },
            color: s.color.clamped(),
        })
        .collect();
    let mut out = Vec::with_capacity(clean.len() * GRADIENT_SAMPLES);
    for (i, s) in clean.iter().enumerate() {
        out.push((s.offset, bgra(s.color)));
        let Some(next) = clean.get(i + 1) else { break };
        if next.offset <= s.offset {
            continue;
        }
        for k in 1..GRADIENT_SAMPLES {
            let t = k as f32 / GRADIENT_SAMPLES as f32;
            let offset = s.offset + (next.offset - s.offset) * t;
            out.push((offset, bgra(s.color.lerp_oklab(next.color, t))));
        }
    }
    out
}

fn gradient(mut g: Gradient, s: &[GradientStop]) -> PaintType {
    g.interpolation_cs = ColorSpaceTag::Srgb;
    g.extend = Extend::Pad;
    PaintType::Gradient(g.with_stops(stops(s).as_slice()))
}

fn finite_angle(deg: f32) -> f64 {
    if deg.is_finite() {
        (deg % 360.0) as f64
    } else {
        0.0
    }
}

/// The vello paint for `p` filling `frame`, and the paint transform it
/// needs (relative to the current transform).
fn paint_type(p: &Paint, frame: kurbo::Rect) -> (PaintType, Affine) {
    let center = frame.center();
    let paint = match p {
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
            let a = finite_angle(*angle).to_radians();
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
            // A full turn starting at +x; the paint transform turns that
            // start to CSS's `from`, measured clockwise from the top (y
            // grows down, so a positive rotation is clockwise on screen).
            let g = gradient(Gradient::new_sweep(center, 0.0, std::f32::consts::TAU), s);
            let turn = (finite_angle(*from) - 90.0).to_radians();
            return (g, Affine::rotate_about(turn, center));
        }
    };
    (paint, Affine::IDENTITY)
}

/// Width of a raster cell: one vello wide tile.
pub const CELL_W: u32 = 256;
/// Height of a raster cell.
pub const CELL_H: u32 = 64;

/// Render contexts kept for distinct cell sizes by default (edge cells are
/// smaller, so a surface uses up to four sizes).
const MIN_CONTEXTS: usize = 8;

/// Owns the vello contexts and scratch space between frames.
#[derive(Debug)]
pub struct Raster {
    /// One context per cell size in use, most recent first.
    contexts: Vec<RenderContext>,
    /// Contexts kept; see [`Raster::set_surfaces`].
    max_contexts: usize,
    resources: Option<Resources>,
    scratch: Vec<u8>,
    /// Pixels the last `paint` handed to vello (cells × cell area).
    rasterised: u64,
    /// Dithered gradients and blurred shadows, drawn from pixmaps.
    cache: PaintCache,
}

impl Default for Raster {
    fn default() -> Self {
        Self {
            contexts: Vec::new(),
            max_contexts: MIN_CONTEXTS,
            resources: None,
            scratch: Vec::new(),
            rasterised: 0,
            cache: PaintCache::default(),
        }
    }
}

impl Raster {
    /// Keeps enough contexts for `n` surfaces (four cell sizes each: full
    /// cells, the right column, the bottom row and the corner), so several
    /// outputs of different sizes never rebuild contexts every frame.
    pub fn set_surfaces(&mut self, n: usize) {
        self.max_contexts = (4 * n).max(MIN_CONTEXTS);
        self.contexts.truncate(self.max_contexts);
    }

    /// Render contexts currently kept.
    #[cfg(test)]
    pub fn contexts(&self) -> usize {
        self.contexts.len()
    }

    /// The cache of dithered gradients and shadows.
    pub fn cache(&self) -> &PaintCache {
        &self.cache
    }

    /// Pixels the last [`Raster::paint`] rasterised. Tracks damage, not
    /// buffer size.
    pub fn rasterised(&self) -> u64 {
        self.rasterised
    }

    fn context(&mut self, w: u16, h: u16) -> &mut RenderContext {
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
                // Single-threaded explicitly: feature unification could
                // otherwise turn on vello's worker threads, and its filters
                // panic when rendering multi-threaded.
                let settings = RenderSettings {
                    num_threads: 0,
                    ..RenderSettings::default()
                };
                self.contexts
                    .insert(0, RenderContext::new_with(w, h, settings));
                self.contexts.truncate(self.max_contexts);
            }
        }
        &mut self.contexts[0]
    }

    /// Paints `items` into `target`, touching only pixels inside `damage`.
    ///
    /// The buffer is split into a fixed grid of [`CELL_W`] × [`CELL_H`]
    /// cells, and only cells the damage touches are rasterised, each by a
    /// context of the cell's size with the scene translated by the cell's
    /// origin. vello's cost is proportional to the pixmap it renders into,
    /// so a clock tick costs the same on a 4K lock screen as on a bar.
    /// Because the grid is fixed, a pixel is always rasterised by the same
    /// cell with the same translation, which keeps a partial repaint
    /// bit-identical to a full one.
    pub fn paint(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        atlas: &AtlasMirror,
        scale: Scale,
        target: &mut PaintTarget<'_>,
    ) {
        self.rasterised = 0;
        if target.size.is_empty() || target.validate().is_err() {
            return;
        }
        let damage = damage.clipped(target.bounds());
        let Some(bbox) = damage.bounds() else {
            return;
        };
        self.prepare(items, &damage);
        // Pixel-aligned rect clips leave coverage inside them untouched; a
        // multi-rect clip path would round differently where layers
        // composite through it.
        let parts = disjoint(&damage);
        let (cw, ch) = (CELL_W as i64, CELL_H as i64);
        let settings = RasterizerSettings {
            target_init: TargetInit::SrcOver,
            // The f32 pipeline: the u8 one rounds differently where a shape
            // is composited through a clip edge than where it is not.
            render_mode: RenderMode::OptimizeQuality,
            ..RasterizerSettings::default()
        };
        for cy in bbox.top().div_euclid(ch)..(bbox.bottom() + ch - 1).div_euclid(ch) {
            for cx in bbox.left().div_euclid(cw)..(bbox.right() + cw - 1).div_euclid(cw) {
                let Some(cell) = Rect::from_edges(cx * cw, cy * ch, (cx + 1) * cw, (cy + 1) * ch)
                    .intersect(target.bounds())
                else {
                    continue;
                };
                let clips: Vec<Rect> = parts.iter().filter_map(|p| p.intersect(cell)).collect();
                if clips.is_empty() {
                    continue;
                }
                // Cells are at most CELL_W × CELL_H.
                let (w, h) = (cell.w as u16, cell.h as u16);
                self.rasterised += cell.area();
                let base = Affine::translate((-(cell.x as f64), -(cell.y as f64)));
                self.context(w, h);
                let ctx = &mut self.contexts[0];
                ctx.reset();
                ctx.set_transform(base);
                for clip in &clips {
                    ctx.push_clip_rect(&to_kurbo(*clip));
                    draw(ctx, items, *clip, atlas, &self.cache, scale, base);
                    ctx.pop_clip();
                }
                ctx.flush();
                let row = cell.w as usize * 4;
                self.scratch.clear();
                self.scratch.resize(row * cell.h as usize, 0);
                let resources = self.resources.get_or_insert_with(Resources::new);
                let ctx = &mut self.contexts[0];
                if let Some(pm) = PixmapMut::new(w, h, &mut self.scratch) {
                    ctx.render_with(pm, resources, settings);
                }
                // Release image references so atlas pages can be updated
                // in place.
                ctx.reset();
                let stride = target.stride as usize;
                for c in &clips {
                    let lx0 = (c.left() - cell.left()) as usize * 4;
                    let lx1 = (c.right() - cell.left()) as usize * 4;
                    let (x0, x1) = (c.left() as usize * 4, c.right() as usize * 4);
                    for y in c.top()..c.bottom() {
                        let ly = (y - cell.top()) as usize;
                        let y = y as usize;
                        target.pixels[y * stride + x0..y * stride + x1]
                            .copy_from_slice(&self.scratch[ly * row + lx0..ly * row + lx1]);
                    }
                }
            }
        }
    }
}

impl Raster {
    /// Builds the cached pixmaps of the gradients and shadows that touch
    /// `damage` (see [`crate::cache`]).
    fn prepare(&mut self, items: &[DisplayItem], damage: &Damage) {
        self.cache.begin_frame();
        let touches = |b: &Rect| damage.rects().iter().any(|r| r.intersects(*b));
        for d in items {
            if !touches(&d.bounds) {
                continue;
            }
            match &d.item {
                Item::Fill { paint, frame, .. } | Item::Border { paint, frame, .. } => {
                    if let Some((w, h)) = frame_px(*frame) {
                        self.cache.gradient(paint, w, h);
                    }
                }
                Item::Shadow {
                    rect,
                    radii,
                    std_dev,
                    color,
                    extent,
                    ..
                } => {
                    self.cache.shadow(&ShadowShape {
                        rect: *rect,
                        radii: *radii,
                        std_dev: *std_dev,
                        color: *color,
                        extent: *extent,
                    });
                }
                _ => {}
            }
        }
    }
}

/// A gradient frame's size in whole pixels.
fn frame_px(f: kurbo::Rect) -> Option<(u32, u32)> {
    let (w, h) = (f.width().round(), f.height().round());
    (w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0 && w <= 65535.0 && h <= 65535.0)
        .then_some((w as u32, h as u32))
}

/// The paint of `p` over `frame`: a cached dithered pixmap for a
/// gradient, else vello's own paint.
fn paint_for(p: &Paint, frame: kurbo::Rect, cache: &PaintCache) -> (PaintType, Affine) {
    if !matches!(p, Paint::Solid(_))
        && let Some((w, h)) = frame_px(frame)
        && let Some(pm) = cache.peek(gradient_key(p, w, h))
    {
        return image_paint(pm, frame.x0.round(), frame.y0.round(), false);
    }
    paint_type(p, frame)
}

/// Index just past the pop matching the push at `i`.
fn skip_group(items: &[DisplayItem], i: usize) -> usize {
    let mut depth = 0usize;
    for (j, d) in items.iter().enumerate().skip(i) {
        match d.item {
            Item::PushClip(_) | Item::PushOpacity(_) | Item::PushTransform(_) => depth += 1,
            Item::PopClip | Item::PopOpacity | Item::PopTransform => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
    }
    items.len()
}

/// Encodes the display items that touch `clip`. `base` maps surface
/// coordinates to the cell being rasterised.
fn draw(
    ctx: &mut RenderContext,
    items: &[DisplayItem],
    clip: Rect,
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    base: Affine,
) {
    let touches = |b: &Rect| clip.intersects(*b);
    // The transform in force (`scale`, `rotate` groups), and the ones
    // their pops return to.
    let mut cur = base;
    let mut saved: Vec<Affine> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let d = &items[i];
        i += 1;
        match &d.item {
            Item::PushClip(_) | Item::PushOpacity(_) | Item::PushTransform(_)
                if !touches(&d.bounds) =>
            {
                i = skip_group(items, i - 1);
            }
            Item::PushTransform(a) => {
                saved.push(cur);
                cur = base * *a;
                ctx.set_transform(cur);
            }
            Item::PopTransform => {
                cur = saved.pop().unwrap_or(base);
                ctx.set_transform(cur);
            }
            Item::PushClip(p) => ctx.push_clip_path(p),
            Item::PopClip => ctx.pop_clip(),
            Item::PushOpacity(o) => ctx.push_opacity_layer(*o),
            Item::PopOpacity => ctx.pop_layer(),
            _ if !touches(&d.bounds) => {}
            Item::Shadow {
                rect,
                radii,
                std_dev,
                color,
                clip,
                extent,
            } => {
                ctx.set_fill_rule(Fill::EvenOdd);
                ctx.push_clip_path(clip);
                ctx.set_fill_rule(Fill::NonZero);
                let shape = ShadowShape {
                    rect: *rect,
                    radii: *radii,
                    std_dev: *std_dev,
                    color: *color,
                    extent: *extent,
                };
                match cache.peek(shadow_key(&shape)) {
                    Some(pm) => {
                        let (ox, oy) = (extent.x0.floor(), extent.y0.floor());
                        let (p, t) = image_paint(pm, ox, oy, false);
                        ctx.set_paint(p);
                        ctx.set_paint_transform(t);
                        ctx.fill_rect(&kurbo::Rect::new(
                            ox,
                            oy,
                            ox + pm.width() as f64,
                            oy + pm.height() as f64,
                        ));
                        ctx.reset_paint_transform();
                    }
                    None => draw_shadow(ctx, &shape),
                }
                ctx.pop_clip();
            }
            Item::Fill {
                shape,
                paint,
                frame,
            } => {
                let (p, t) = paint_for(paint, *frame, cache);
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                match shape {
                    FillShape::Rect(r) => ctx.fill_rect(r),
                    FillShape::Path(p) => ctx.fill_path(p),
                }
                ctx.reset_paint_transform();
            }
            Item::Border { path, paint, frame } => {
                let (p, t) = paint_for(paint, *frame, cache);
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                ctx.set_fill_rule(Fill::EvenOdd);
                ctx.fill_path(path);
                ctx.set_fill_rule(Fill::NonZero);
                ctx.reset_paint_transform();
            }
            Item::Image { pixmap, rect, tint } => {
                // Decoded at this size: drawn pixel for pixel unless a
                // transform scales or turns it.
                let smooth = cur != base;
                let (p, t) = image_paint(pixmap, rect.x0, rect.y0, smooth);
                if let Some(c) = tint {
                    ctx.set_tint(Some(Tint {
                        color: bgra(*c),
                        mode: TintMode::AlphaMask,
                    }));
                }
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                ctx.fill_rect(rect);
                ctx.reset_paint_transform();
                ctx.reset_tint();
            }
            Item::Glyphs {
                x,
                y,
                layout,
                color,
            } => {
                // A layout shaped for another scale (the surface moved
                // to a different output) is drawn resampled until the
                // re-shaped one arrives.
                let k = scale.as_f64() / layout.scale.as_f64();
                let transformed = cur != base;
                let quality = if k == 1.0 && !transformed {
                    ImageQuality::Low
                } else {
                    ImageQuality::Medium
                };
                ctx.set_transform(
                    cur * Affine::translate((*x as f64, *y as f64)) * Affine::scale(k),
                );
                for (g, run_color) in layout
                    .runs
                    .iter()
                    .flat_map(|r| r.glyphs.iter().map(move |g| (g, r.color)))
                {
                    let Some(page) = atlas.page(g.slot.page) else {
                        continue;
                    };
                    let gb = Rect::new(
                        *x + (g.x as f64 * k).floor() as i32,
                        *y + (g.y as f64 * k).floor() as i32,
                        (g.slot.w as f64 * k).ceil() as u32 + 1,
                        (g.slot.h as f64 * k).ceil() as u32 + 1,
                    );
                    // Under a transform glyph boxes are not in surface
                    // pixels: the group's bounds already matched.
                    if !transformed && !touches(&gb) {
                        continue;
                    }
                    // Marks and markup spans paint in their own colour.
                    ctx.set_tint(Some(Tint {
                        color: bgra(run_color.unwrap_or(*color)),
                        mode: TintMode::AlphaMask,
                    }));
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
                ctx.set_transform(cur);
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

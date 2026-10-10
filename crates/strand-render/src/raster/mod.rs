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

use strand_scene::{Damage, PaintTarget, Rect, Scale};
use vello_cpu::kurbo::Affine;
use vello_cpu::{
    PixmapMut, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources, TargetInit,
};

use crate::cache::{PaintCache, ShadowShape};
use crate::flatten::{DisplayItem, Item};

mod atlas;
mod draw;
mod paint;

pub use atlas::AtlasMirror;
use draw::{disjoint, draw, frame_px, to_kurbo};
pub(crate) use draw::{draw as draw_group, skip_group};

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
    /// (M4) Offscreen groups (blurred, colour-filtered subtrees).
    offscreen: crate::offscreen::Offscreen,
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
            offscreen: crate::offscreen::Offscreen::default(),
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

    /// (M4) The paint cache and the offscreen groups, for lowering a frame
    /// to the GPU.
    #[cfg(feature = "gpu")]
    pub(crate) fn gpu_parts(&mut self) -> (&mut PaintCache, &mut crate::offscreen::Offscreen) {
        (&mut self.cache, &mut self.offscreen)
    }

    /// The offscreen group cache.
    pub fn offscreen(&self) -> &crate::offscreen::Offscreen {
        &self.offscreen
    }

    /// Frees the paint cache's and offscreen groups' idle entries without
    /// painting.
    pub fn trim_idle(&mut self, now: std::time::Instant) {
        self.cache.trim_idle(now);
        self.offscreen.trim_idle(now);
    }

    /// Frees the paint cache's entries and offscreen groups no frame has
    /// used since `since`.
    pub fn trim_unused_since(&mut self, since: std::time::Instant) {
        self.cache.trim_unused_since(since);
        self.offscreen.trim_unused_since(since);
    }

    /// Shortens (tests) how long an unused paint cache entry or
    /// offscreen group lives.
    pub fn set_idle_free(&mut self, idle: std::time::Duration) {
        self.cache.set_idle_free(idle);
        self.offscreen.set_idle_free(idle);
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
        let now = std::time::Instant::now();
        self.cache.trim_idle(now);
        self.offscreen
            .prepare(items, &damage, target.bounds(), atlas, &self.cache, scale);
        self.offscreen.trim_idle(now);
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
                    draw(
                        ctx,
                        items,
                        *clip,
                        atlas,
                        &self.cache,
                        scale,
                        base,
                        Affine::IDENTITY,
                        self.offscreen.current(),
                    );
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

#[cfg(test)]
mod tests;

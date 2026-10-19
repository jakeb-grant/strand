//! (m6-owner-fixes, owner-testing.md bug 17) Fade layers: an opacity
//! group whose content is unchanged but for where it is drawn (an `enter`
//! or `exit` pose of `opacity` and `x`/`y`, a `when` fade, a translucent
//! row scrolling) is rastered once into an offscreen layer, a cached
//! offscreen group (design.md, "Runtime changes these need", item 2), and
//! each frame draws that layer as an image at the group's opacity (its
//! alpha: no group is composited) and position instead of rastering its
//! content again.
//!
//! - **Which groups.** An opacity group the flattener tagged with its
//!   node ([`crate::flatten::Fade`]), outside every transform and
//!   offscreen group (its pixels are then whole surface pixels), whose
//!   content holds no offscreen group (a blur, a colour matrix, a
//!   backdrop: those read pixels around them) and something that costs
//!   the raster more per pixel than a layer's image does (a shadow, a
//!   gradient, text filled with a paint: [`costly`]). Flat fills,
//!   borders, plain text and images cost vello_cpu's fine stage about
//!   what the image would, so a group of only those is drawn as before
//!   (measured: `tests/morph.rs`,
//!   `a_fading_dashboard_draws_its_panels_from_layers`). The outermost
//!   such group is layered; one nested in it is drawn into its layer.
//! - **The key.** The group's items hashed from the top-left pixel of
//!   what they draw ([`crate::flatten::Shift`]), with the layer's size
//!   and the scale: the same content moved by whole pixels has the same
//!   key, and anything else (a glyph shaped anew, a colour, a box moved
//!   within the group, a half-pixel phase at a fractional scale) has
//!   another. Its pixels are those of its items drawn at their place, so
//!   a key that matches is exact.
//! - **When it is kept.** A layer is cached when built on a frame that
//!   repaints the group whole (it always does while its opacity or
//!   offset changes), when its node is in flight
//!   ([`crate::flatten::Fade::moving`]) or the same key was drawn in the
//!   node's recent frames. A node whose last two layers were never drawn
//!   again (content changing every frame) keeps no more until it rests.
//!   A layer that is cached is drawn whatever the damage.
//! - **Always a layer.** A group that qualifies is drawn from its layer
//!   on every frame: one not cached is rastered uncached, only where the
//!   frame repaints, on the layer's own fixed grid of cells
//!   ([`render_layer`]), so that part is bit-identical to the whole
//!   layer. Its pixels never depend on what earlier frames cached: a
//!   layer's pixels are 8-bit before they are composited, so a group
//!   drawn from a layer on one frame and rastered as a group on another
//!   differed by a step, and a partial repaint by a full one
//!   (decisions.md, shadow-redraw).
//! - **What it costs.** The group's pixels once per key, in the offscreen
//!   groups' budget ([`super::OFFSCREEN_BYTES`], least recently used
//!   first, freed when idle): at a fractional scale a fade moving by
//!   whole logical pixels has two keys, one per half-pixel phase.
//! - **Damage** is unchanged: the group's pixels repaint as before; what
//!   is saved is rastering its content.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use strand_scene::{Damage, NodeId, Rect, Scale};
use vello_cpu::kurbo::Affine;

use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo;
use vello_cpu::{
    Pixmap, PixmapMut, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources,
    TargetInit,
};

use super::{Drawn, Offscreen, pins};
use crate::cache::PaintCache;
use crate::flatten::{DisplayItem, Item, Shift, hash_item_from};
use crate::raster::{AtlasMirror, CELL_H, CELL_W, disjoint, draw_group, skip_group};

/// Frames (paints of any surface) a node's recent keys count for.
const RECENT: u64 = 6;

/// Layers built and never drawn again before a node builds no more.
const WASTED: u8 = 2;

/// Largest side of a layer, pixels.
const MAX_SIDE: u32 = 4096;

/// The fade layers' state in [`Offscreen`].
#[derive(Debug)]
pub(super) struct Fades {
    pub(super) enabled: bool,
    /// This frame's layers, by their group's push item (its address).
    pub(super) current: HashMap<usize, Drawn>,
    nodes: HashMap<NodeId, History>,
    pub(super) builds: u64,
    pub(super) draws: u64,
    /// Layers rastered uncached, where a frame repaints (see
    /// [`render_layer`]).
    pub(super) passes: u64,
    /// What [`render_layer`] rasters with, kept between calls.
    raster: LayerRaster,
}

/// Render contexts kept for a layer's distinct cell sizes: a layer's
/// cells are full but for its right column, bottom row and corner, so
/// two layers of different sizes use up to eight.
const LAYER_CONTEXTS: usize = 8;

/// (integration-r4) The contexts, resources and scratch [`render_layer`]
/// rasters with, kept between calls as the surface raster keeps its own
/// (`Raster::context`): a fade group that is not cached is rastered on
/// every frame it repaints, and making a context per cell size, the
/// resources and the scratch anew on each was measured (decisions.md
/// shadow-redraw). Freed with the offscreen groups when idle.
#[derive(Debug, Default)]
struct LayerRaster {
    /// One context per cell size in use, most recent first.
    contexts: Vec<RenderContext>,
    resources: Resources,
    scratch: Vec<u8>,
    /// When it last rastered.
    used: Option<Instant>,
}

impl LayerRaster {
    /// The context for a cell of `w` × `h`, first in the list.
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
                // Single-threaded, as the surface raster's (feature
                // unification could turn on vello's worker threads).
                let settings = RenderSettings {
                    num_threads: 0,
                    ..RenderSettings::default()
                };
                self.contexts
                    .insert(0, RenderContext::new_with(w, h, settings));
                self.contexts.truncate(LAYER_CONTEXTS);
            }
        }
        &mut self.contexts[0]
    }

    /// Frees it all when no layer was rastered for `idle`.
    fn trim_idle(&mut self, now: Instant, idle: Duration) {
        if self
            .used
            .is_some_and(|t| now.saturating_duration_since(t) >= idle)
        {
            *self = LayerRaster::default();
        }
    }
}

impl Fades {
    /// (integration-r4) Frees the layer raster's contexts when idle.
    pub(super) fn trim_idle(&mut self, now: Instant, idle: Duration) {
        self.raster.trim_idle(now, idle);
    }
}

impl Default for Fades {
    fn default() -> Self {
        Self {
            enabled: true,
            current: HashMap::new(),
            nodes: HashMap::new(),
            builds: 0,
            draws: 0,
            passes: 0,
            raster: LayerRaster::default(),
        }
    }
}

/// A node's recent fade groups.
#[derive(Debug, Default)]
struct History {
    /// The frame it was last seen.
    seen: u64,
    /// Its last keys, newest first.
    keys: [u64; 2],
    /// Its last layer built, and whether a frame drew it since.
    built: Option<(u64, bool)>,
    /// Layers built and never drawn again, in a row.
    wasted: u8,
}

/// Whether drawing `item` costs the raster more per pixel than drawing a
/// layer's pixels does (see the module docs).
fn costly(item: &Item) -> bool {
    match item {
        Item::Shadow { .. } => true,
        Item::Fill { paint, .. } | Item::Border { paint, .. } => {
            !matches!(paint, strand_scene::Paint::Solid(_))
        }
        Item::Glyphs { fill, .. } => fill.is_some(),
        _ => false,
    }
}

/// The key of the push item `d` in [`Fades::current`].
pub(crate) fn item_key(d: &DisplayItem) -> usize {
    d as *const DisplayItem as usize
}

impl Offscreen {
    /// Finds or builds this frame's fade layers among `items` (see the
    /// module docs); `whole` tells if the damage repaints a rectangle
    /// whole.
    pub(crate) fn prepare_fades(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        surface: Rect,
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
    ) {
        self.fades.current.clear();
        if !self.fades.enabled {
            return;
        }
        let frame = self.frame;
        // Groups inside a transform or an offscreen group are not drawn in
        // whole surface pixels: each open transform and layer, and
        // whether it is one of those.
        let mut open: Vec<bool> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let d = &items[i];
            match &d.item {
                Item::PushTransform(_) => open.push(true),
                Item::PushLayer(l) => open.push(!l.cell_local()),
                Item::PopTransform | Item::PopLayer => {
                    open.pop();
                }
                Item::PushOpacity(alpha, Some(fade))
                    if !open.contains(&true) && *alpha > 0.0 && d.bounds.intersects(surface) =>
                {
                    let end = skip_group(items, i);
                    if let Some(g) = self.fade_layer(
                        &items[i + 1..end.saturating_sub(1).max(i + 1)],
                        d.bounds,
                        fade.node,
                        fade.moving,
                        (damage, surface),
                        atlas,
                        cache,
                        scale,
                        frame,
                    ) {
                        self.fades.current.insert(item_key(d), g);
                        i = end;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.fades.nodes.retain(|_, h| h.seen + RECENT * 4 >= frame);
    }

    /// The layer of one fade group (`inner` its items, `bounds` its push
    /// item's), if one is cached or worth building now.
    #[allow(clippy::too_many_arguments)]
    fn fade_layer(
        &mut self,
        inner: &[DisplayItem],
        bounds: Rect,
        node: NodeId,
        moving: bool,
        (damage, surface): (&Damage, Rect),
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
        frame: u64,
    ) -> Option<Drawn> {
        let leaf = |d: &&DisplayItem| {
            !matches!(
                d.item,
                Item::PushClip(_)
                    | Item::PopClip
                    | Item::PushOpacity(..)
                    | Item::PopOpacity
                    | Item::PushTransform(_)
                    | Item::PopTransform
                    | Item::PushLayer(_)
                    | Item::PopLayer
            )
        };
        // What reads pixels around it cannot be drawn alone, and content
        // that costs the raster no more than a layer does is drawn as
        // before.
        if inner
            .iter()
            .any(|d| matches!(&d.item, Item::PushLayer(l) if !l.cell_local()))
            || !inner.iter().any(|d| costly(&d.item))
        {
            return None;
        }
        // Everything it draws, unclipped: its leaves' bounds (a push
        // item's bounds are its group's, clipped by what is around it).
        let extent = inner
            .iter()
            .filter(leaf)
            .fold(Rect::default(), |a, d| a.union(d.bounds));
        if extent.is_empty()
            || extent.w > MAX_SIDE
            || extent.h > MAX_SIDE
            || extent.area() * 4 > (super::OFFSCREEN_BYTES / 4) as u64
        {
            return None;
        }
        let o = Shift {
            x: extent.left(),
            y: extent.top(),
        };
        let mut h = DefaultHasher::new();
        ("fade", extent.w, extent.h, scale.as_f32().to_bits()).hash(&mut h);
        // A push item's bounds are its group's as clipped around the
        // fade group, which is not drawn into the layer: only what is
        // drawn and the leaves' bounds count.
        for d in inner {
            hash_item_from(&mut h, &d.item, o);
            if leaf(&d) {
                (
                    d.bounds.left() - o.x,
                    d.bounds.top() - o.y,
                    d.bounds.w,
                    d.bounds.h,
                )
                    .hash(&mut h);
            }
        }
        let key = h.finish();
        // A cached layer is drawn where its content is this frame.
        let at = |g: Drawn| Drawn {
            pixmap: g.pixmap,
            x: extent.x,
            y: extent.y,
        };
        let recent = {
            let hist = self.fades.nodes.entry(node).or_default();
            if hist.seen + RECENT < frame {
                *hist = History::default();
            }
            let recent = hist.seen > 0 && hist.keys.contains(&key);
            hist.seen = frame;
            if hist.keys[0] != key {
                hist.keys = [key, hist.keys[0]];
            }
            recent
        };
        if let Some(g) = self.get(key) {
            let hist = self.fades.nodes.entry(node).or_default();
            if let Some((_, used)) = &mut hist.built {
                *used = true;
            }
            hist.wasted = 0;
            self.fades.draws += 1;
            return Some(at(g));
        }
        // Drawn whole: no group in it is passed over for being clipped
        // away around the fade group (it may show on a later frame).
        let whole: Vec<DisplayItem> = inner
            .iter()
            .map(|d| DisplayItem {
                item: d.item.clone(),
                bounds: if leaf(&d) { d.bounds } else { extent },
            })
            .collect();
        // Cached only when built on a frame that repaints all of it that
        // shows, while its node is in flight or its key recurs, and while
        // its layers are drawn again. Otherwise its layer is drawn all the
        // same, rastered uncached where this frame repaints: whether a
        // group is drawn from a layer depends on the frame alone, never
        // on what earlier frames cached or built, so a partial repaint
        // draws exactly what a full one does (decisions.md, shadow-redraw).
        let shows = bounds.intersect(surface)?;
        let admit = damage.covers(shows) && {
            let hist = self.fades.nodes.entry(node).or_default();
            if let Some((_, false)) = hist.built {
                hist.wasted = hist.wasted.saturating_add(1);
                hist.built = None;
            }
            hist.wasted < WASTED && (moving || recent)
        };
        if !admit {
            let parts: Vec<Rect> = disjoint(damage)
                .into_iter()
                .filter_map(|r| r.intersect(extent))
                .collect();
            let drawn = render_layer(
                &mut self.fades.raster,
                &whole,
                extent,
                &parts,
                atlas,
                cache,
                scale,
            )?;
            self.fades.passes += 1;
            return Some(drawn);
        }
        let hist = self.fades.nodes.entry(node).or_default();
        hist.built = Some((key, false));
        let drawn = render_layer(
            &mut self.fades.raster,
            &whole,
            extent,
            &[extent],
            atlas,
            cache,
            scale,
        )?;
        self.fades.builds += 1;
        self.fades.draws += 1;
        let pins = pins(inner, &HashMap::new());
        Some(self.insert(key, drawn, pins))
    }
}

/// A fade group's layer over `extent`, rastered where `parts` (disjoint,
/// inside `extent`) lie, into a pixmap of the cells they touch. The layer
/// is cut into a fixed grid of [`CELL_W`] × [`CELL_H`] cells from its
/// top-left, each rastered by a context of its size with the items
/// translated by the cell's origin, and clipped to the parts inside it
/// by pixel-aligned rects, which leave the coverage inside them
/// untouched. So a pixel of the layer is always rastered by the same
/// cell with the same translation: the part a partial repaint rasters is
/// bit-identical to that part of the whole layer a full one builds, as
/// the surface's own cells are (`Raster::paint`). `None` when no part
/// touches it or a side is too large.
fn render_layer(
    lr: &mut LayerRaster,
    items: &[DisplayItem],
    extent: Rect,
    parts: &[Rect],
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
) -> Option<Drawn> {
    let (cw, ch) = (CELL_W as i64, CELL_H as i64);
    let (ex, ey) = (extent.left(), extent.top());
    // The cells any part touches, and the bounds of them all.
    let mut cells: Vec<(Rect, Vec<Rect>)> = Vec::new();
    for gy in 0..(extent.h as i64 + ch - 1) / ch {
        for gx in 0..(extent.w as i64 + cw - 1) / cw {
            let x0 = ex + gx * cw;
            let y0 = ey + gy * ch;
            let Some(cell) = Rect::from_edges(x0, y0, x0 + cw, y0 + ch).intersect(extent) else {
                continue;
            };
            let clips: Vec<Rect> = parts.iter().filter_map(|p| p.intersect(cell)).collect();
            if !clips.is_empty() {
                cells.push((cell, clips));
            }
        }
    }
    let area = cells.iter().map(|(c, _)| *c).reduce(|a, c| a.union(c))?;
    let (w, h) = (u16::try_from(area.w).ok()?, u16::try_from(area.h).ok()?);
    let mut pm = Pixmap::new(w, h);
    let raster = RasterizerSettings {
        target_init: TargetInit::SrcOver,
        render_mode: RenderMode::OptimizeQuality,
        ..RasterizerSettings::default()
    };
    lr.used = Some(Instant::now());
    let mut resources = std::mem::take(&mut lr.resources);
    let mut scratch = std::mem::take(&mut lr.scratch);
    for (cell, clips) in &cells {
        // Cells are at most CELL_W × CELL_H.
        let (cw, ch) = (cell.w as u16, cell.h as u16);
        let ctx = lr.context(cw, ch);
        ctx.reset();
        let base = Affine::translate((-(cell.x as f64), -(cell.y as f64)));
        ctx.set_transform(base);
        for clip in clips {
            ctx.push_clip_rect(&kurbo::Rect::new(
                clip.left() as f64,
                clip.top() as f64,
                clip.right() as f64,
                clip.bottom() as f64,
            ));
            draw_group(
                ctx,
                items,
                *clip,
                atlas,
                cache,
                scale,
                base,
                Affine::IDENTITY,
                &HashMap::new(),
                &HashMap::new(),
            );
            ctx.pop_clip();
        }
        ctx.flush();
        let row = cell.w as usize * 4;
        scratch.clear();
        scratch.resize(row * cell.h as usize, 0);
        if let Some(target) = PixmapMut::new(cw, ch, &mut scratch) {
            ctx.render_with(target, &mut resources, raster);
        }
        ctx.reset();
        let data = pm.data_mut();
        let ox = (cell.left() - area.left()) as usize;
        for y in 0..cell.h as usize {
            let dy = (cell.top() - area.top()) as usize + y;
            for (x, px) in scratch[y * row..(y + 1) * row].chunks_exact(4).enumerate() {
                data[dy * area.w as usize + ox + x] = PremulRgba8 {
                    r: px[0],
                    g: px[1],
                    b: px[2],
                    a: px[3],
                };
            }
        }
    }
    lr.resources = resources;
    lr.scratch = scratch;
    Some(Drawn {
        pixmap: std::sync::Arc::new(pm),
        x: area.x,
        y: area.y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (integration-r4) The layer raster keeps one context per cell size
    /// between calls, most recent first, at most [`LAYER_CONTEXTS`], and
    /// frees them all once idle.
    #[test]
    fn layer_contexts_are_kept_between_calls_and_freed_when_idle() {
        let mut lr = LayerRaster::default();
        let full = lr.context(CELL_W as u16, CELL_H as u16) as *const RenderContext;
        lr.context(40, CELL_H as u16);
        assert_eq!(lr.contexts.len(), 2);
        let again = lr.context(CELL_W as u16, CELL_H as u16) as *const RenderContext;
        assert_eq!(full, again, "the full cell's context is the one made first");
        assert_eq!(lr.contexts.len(), 2);
        for w in 1..=20 {
            lr.context(w, 10);
        }
        assert_eq!(lr.contexts.len(), LAYER_CONTEXTS);
        let t = Instant::now();
        lr.used = Some(t);
        lr.trim_idle(t + Duration::from_secs(1), Duration::from_secs(30));
        assert_eq!(lr.contexts.len(), LAYER_CONTEXTS, "not idle yet");
        lr.trim_idle(t + Duration::from_secs(31), Duration::from_secs(30));
        assert!(lr.contexts.is_empty() && lr.scratch.capacity() == 0 && lr.used.is_none());
    }
}

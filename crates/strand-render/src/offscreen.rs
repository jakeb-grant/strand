//! (M4) Cached offscreen groups and CPU raster nodes (design.md, "Runtime
//! changes these need", items 2 and 3).
//!
//! **Offscreen groups.** An effect layer whose effects read more than one
//! cell's pixels (`Blur`, `ColorMatrix`: [`crate::layers::Layer::cell_local`]
//! is false) is drawn whole into a pixmap of its group's bounds (its
//! effects' reach included), filtered there, and drawn into each cell as
//! an image under the layer's cell-local part (opacity, blend, masks).
//! The pixmap is cached by the hash of what the group draws, so it is
//! redrawn only when its content changes; the cache has its own byte
//! budget ([`OFFSCREEN_BYTES`], least recently used dropped first) and
//! frees entries no frame used for [`crate::cache::IDLE_FREE`], like the
//! paint cache. A group larger than the budget is drawn each frame it is
//! damaged, uncached.
//!
//! **Raster nodes.** A node with a [`RasterSource`] (particles, grain,
//! graphs, spectrum, animated frames: S-effects) draws into a pixmap of
//! its box at its clock's rate, kept per node and redrawn only when its
//! tick or size changes; the display list carries it as `Item::Raster`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};
use std::time::Duration;
use strand_scene::time::Instant;

use strand_scene::{Damage, Effect, NodeId, Rect, Scale, TimeContext};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo::Affine;
use vello_cpu::{
    Pixmap, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources, TargetInit,
};

use crate::cache::{IDLE_FREE, PaintCache};
use crate::clock::Rate;
use crate::flatten::{DisplayItem, Item};
use crate::raster::AtlasMirror;

mod fade;
pub(crate) use fade::item_key as fade_key;

/// Bytes of offscreen group pixmaps kept (design.md: "about 32 MB"): a
/// blurred or filtered group of a 2× launcher or a 1440p overlay
/// (decisions.md, m5-speed-caches: over the old 4 MB such a group was
/// drawn and filtered again on every frame that touched it, 125 ms
/// against 8 ms a frame for a 1400 × 1000 px group).
pub const OFFSCREEN_BYTES: usize = 32 << 20;

/// A group's filtered pixels and where they go on the surface.
#[derive(Clone, Debug)]
pub struct Drawn {
    pub pixmap: Arc<Pixmap>,
    pub x: i32,
    pub y: i32,
    /// (lock-int) Surface pixels per pixel of `pixmap` along each axis:
    /// 1 drawn pixel for pixel, `k` for a blur kept at its reduction
    /// (1 / k² the bytes) and scaled up bilinearly, the edge pixels
    /// repeated, where it is drawn (`upscale`, the GPU's smooth image).
    pub k: u32,
    /// Where the surface rect's corner falls in `pixmap`, in its pixels
    /// (a backdrop read from past its region).
    pub origin: (f64, f64),
    /// Its size on the surface: the pixmap's own at `k` 1.
    pub w: u32,
    pub h: u32,
}

impl Drawn {
    /// `pixmap` drawn pixel for pixel at `(x, y)`.
    pub fn at(pixmap: Arc<Pixmap>, x: i32, y: i32) -> Drawn {
        let (w, h) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
        Drawn {
            pixmap,
            x,
            y,
            k: 1,
            origin: (0.0, 0.0),
            w,
            h,
        }
    }

    /// The surface rect it covers.
    pub fn rect(&self) -> vello_cpu::kurbo::Rect {
        let (x, y) = (f64::from(self.x), f64::from(self.y));
        vello_cpu::kurbo::Rect::new(x, y, x + f64::from(self.w), y + f64::from(self.h))
    }

    /// Its pixmap's transform onto the surface (an image paint's).
    pub fn transform(&self) -> Affine {
        Affine::translate((f64::from(self.x), f64::from(self.y)))
            * Affine::scale(f64::from(self.k))
            * Affine::translate((-self.origin.0, -self.origin.1))
    }

    /// Whether it is drawn scaled (sampled bilinearly).
    pub fn scaled(&self) -> bool {
        self.k > 1
    }

    /// Its pixels at their size on the surface (scaled up when kept
    /// reduced: a GPU pass reads its input pixel for pixel).
    #[cfg(feature = "gpu")]
    pub fn full(&self) -> Option<Arc<Pixmap>> {
        let whole = (
            u32::from(self.pixmap.width()),
            u32::from(self.pixmap.height()),
        );
        if self.k == 1 && self.origin == (0.0, 0.0) && whole == (self.w, self.h) {
            return Some(self.pixmap.clone());
        }
        let (w16, h16) = (u16::try_from(self.w).ok()?, u16::try_from(self.h).ok()?);
        let mut out = Pixmap::new(w16, h16);
        upscale(
            self.pixmap.data_as_u8_slice(),
            (
                usize::from(self.pixmap.width()),
                usize::from(self.pixmap.height()),
            ),
            out.data_as_u8_slice_mut(),
            (self.w as usize, self.h as usize),
            self.origin,
            f64::from(self.k),
        );
        Some(Arc::new(out))
    }
}

#[derive(Debug)]
struct Entry {
    drawn: Drawn,
    /// The pixmaps its key names by address (an `image`'s, a GPU pass's,
    /// a nested group's): a `Weak` keeps each address allocated while the
    /// entry lives, so no new pixmap with other pixels can take it and
    /// match a stale entry (decisions.md, m4-audit).
    pins: Vec<Weak<Pixmap>>,
    bytes: usize,
    used: u64,
    frame: u64,
    at: Instant,
}

/// The offscreen group cache.
#[derive(Debug, Default)]
pub struct Offscreen {
    entries: HashMap<u64, Entry>,
    bytes: usize,
    tick: u64,
    frame: u64,
    builds: u64,
    /// (blur-ready) Wall-clock time spent drawing groups so far
    /// (advisory; measurements read it).
    build_time: Duration,
    idle: Option<Duration>,
    /// This frame's groups, by their layer (its `Arc`'s address).
    current: HashMap<usize, Drawn>,
    /// (m6-owner-fixes, bug 17) Fade layers ([`fade`]).
    fades: fade::Fades,
}

/// The key of a layer in [`Offscreen::current`].
pub fn layer_key(l: &Arc<crate::layers::Layer>) -> usize {
    Arc::as_ptr(l) as usize
}

impl Offscreen {
    /// Bytes kept.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Groups drawn so far (tests: a frame reusing a group draws none).
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// (blur-ready) Wall-clock time spent drawing groups so far.
    pub fn build_time(&self) -> Duration {
        self.build_time
    }

    /// Groups kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// This frame's group pixmaps by layer.
    pub fn current(&self) -> &HashMap<usize, Drawn> {
        &self.current
    }

    /// (m6-owner-fixes, bug 17) This frame's fade layers, by their
    /// opacity group's push item ([`fade_key`]).
    pub fn fades(&self) -> &HashMap<usize, Drawn> {
        &self.fades.current
    }

    /// Turns fade layers on (the default) or off (tests: the frame they
    /// replace).
    pub fn set_fade_layers(&mut self, on: bool) {
        self.fades.enabled = on;
    }

    /// (eval-sandbox) Whether fade layers are on.
    #[cfg(feature = "eval-hooks")]
    pub(crate) fn fade_layers_on(&self) -> bool {
        self.fades.enabled
    }

    /// Fade layers built so far.
    pub fn fade_builds(&self) -> u64 {
        self.fades.builds
    }

    /// Fade layers drawn so far, built or reused (a frame drawing a
    /// group from its layer counts one).
    pub fn fade_draws(&self) -> u64 {
        self.fades.draws
    }

    /// Fade layers rastered uncached so far, only where their frame
    /// repainted (a group whose layer is neither cached nor kept).
    pub fn fade_passes(&self) -> u64 {
        self.fades.passes
    }

    /// Shortens (tests) how long an unused group lives.
    pub fn set_idle_free(&mut self, idle: Duration) {
        self.idle = Some(idle);
    }

    /// Frees groups no frame has used for the idle time.
    pub fn trim_idle(&mut self, now: Instant) {
        let idle = self.idle.unwrap_or(IDLE_FREE);
        self.entries
            .retain(|_, e| now.saturating_duration_since(e.at) < idle);
        self.fades.trim_idle(now, idle);
        self.recount();
    }

    fn recount(&mut self) {
        self.bytes = self.entries.values().map(|e| e.bytes).sum();
    }

    /// Draws (or finds) the offscreen groups among `items` that touch
    /// `damage`, and every group nested in one of those, inner groups
    /// first, for this frame's cells.
    pub fn prepare(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        surface: Rect,
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
    ) {
        self.prepare_except(items, damage, surface, atlas, cache, scale, &|_| false);
    }

    /// (perf-gpu-retain) [`Self::prepare`], leaving out the groups pushed
    /// at the indices `skip` holds: the backdrops a GPU frame draws
    /// itself (`Op::Backdrop`), which nothing drawn here reads.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_except(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        surface: Rect,
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
        skip: &dyn Fn(usize) -> bool,
    ) {
        self.frame += 1;
        self.current.clear();
        let touches = |b: &Rect| damage.rects().iter().any(|r| r.intersects(*b));
        let offscreen = |d: &DisplayItem| matches!(&d.item, Item::PushLayer(l) if !l.cell_local());
        // Every group the damage touches, and every group inside one: an
        // outer group is drawn whole, so its inner groups must be ready
        // (their pixels drawn filtered) whether or not damage meets them.
        let mut groups: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            if offscreen(&items[i]) && touches(&items[i].bounds) {
                let end = crate::raster::skip_group(items, i);
                groups.extend((i..end.min(items.len())).filter(|&j| offscreen(&items[j])));
                i = end.max(i + 1);
            } else {
                i += 1;
            }
        }
        // Each group after every group it shows: by where it ends, so
        // the groups inside one come before it (it is drawn whole), and a
        // backdrop (an empty group) comes after the groups closed before
        // it, which it shows behind it, and before the groups still open
        // around it, which then show its pixels.
        let mut order: Vec<(usize, usize)> = groups
            .iter()
            .map(|&i| (crate::raster::skip_group(items, i), i))
            .collect();
        order.sort_unstable();
        for (end, i) in order {
            let Item::PushLayer(layer) = &items[i].item else {
                continue;
            };
            if skip(i) {
                continue;
            }
            // (M4) Drawn by the GPU: its pixels as they came back.
            if let Some(g) = &layer.gpu {
                self.current.insert(layer_key(layer), g.clone());
                continue;
            }
            let Some(region) = items[i].bounds.intersect(surface).filter(|r| !r.is_empty()) else {
                continue;
            };
            if let Some(pass) = crate::backdrop::pass(layer, scale) {
                let key = crate::backdrop::key(items, i, pass, region, scale, &self.current);
                let drawn = match self.get(key) {
                    Some(d) => d,
                    None => {
                        let behind = crate::backdrop::read(items, i, pass, region);
                        let pins = pins(&behind, &self.current);
                        let started = std::time::Instant::now();
                        let Some(drawn) = crate::backdrop::render(
                            items,
                            i,
                            pass,
                            region,
                            surface,
                            atlas,
                            cache,
                            scale,
                            &self.current,
                        ) else {
                            continue;
                        };
                        self.builds += 1;
                        self.build_time += started.elapsed();
                        self.insert(key, drawn, pins)
                    }
                };
                self.current.insert(layer_key(layer), drawn);
                continue;
            }
            let inner = &items[i + 1..end.saturating_sub(1).max(i + 1)];
            let mut h = DefaultHasher::new();
            (
                region.x,
                region.y,
                region.w,
                region.h,
                scale.as_f32().to_bits(),
            )
                .hash(&mut h);
            crate::layers::hash_effects(&mut h, &layer.effects);
            (
                layer.mosaic,
                layer.frame.x0.to_bits(),
                layer.frame.y0.to_bits(),
            )
                .hash(&mut h);
            for v in layer.xform.as_coeffs() {
                v.to_bits().hash(&mut h);
            }
            for d in inner {
                crate::flatten::hash_item(&mut h, &d.item);
                // A nested group's own pixels (drawn just before).
                if let Item::PushLayer(l) = &d.item
                    && let Some(n) = self.current.get(&layer_key(l))
                {
                    (Arc::as_ptr(&n.pixmap) as usize).hash(&mut h);
                }
            }
            let key = h.finish();
            let drawn = match self.get(key) {
                Some(d) => d,
                None => {
                    let started = std::time::Instant::now();
                    let Some(drawn) = render_group(
                        inner,
                        layer,
                        region,
                        cut_sides(region, surface),
                        atlas,
                        cache,
                        scale,
                        &self.current,
                        true,
                    ) else {
                        continue;
                    };
                    self.builds += 1;
                    self.build_time += started.elapsed();
                    let pins = pins(inner, &self.current);
                    self.insert(key, drawn, pins)
                }
            };
            self.current.insert(layer_key(layer), drawn);
        }
    }

    fn get(&mut self, key: u64) -> Option<Drawn> {
        self.tick += 1;
        let (tick, frame) = (self.tick, self.frame);
        // A pinned address is never reused, so a key that matches names
        // the same, live, pixmaps; one gone means the hash collided.
        let e = self.entries.get_mut(&key)?;
        if e.pins.iter().any(|p| p.strong_count() == 0) {
            if let Some(e) = self.entries.remove(&key) {
                self.bytes -= e.bytes;
            }
            return None;
        }
        e.used = tick;
        e.frame = frame;
        e.at = Instant::now();
        Some(e.drawn.clone())
    }

    /// Keeps `drawn` within the budget (evicting the least recently used
    /// groups this frame does not use); a group over the budget is not
    /// kept.
    fn insert(&mut self, key: u64, drawn: Drawn, pins: Vec<Weak<Pixmap>>) -> Drawn {
        let bytes = drawn.pixmap.width() as usize * drawn.pixmap.height() as usize * 4;
        if bytes > OFFSCREEN_BYTES {
            return drawn;
        }
        while self.bytes + bytes > OFFSCREEN_BYTES {
            let frame = self.frame;
            let Some((&old, _)) = self
                .entries
                .iter()
                .filter(|(_, e)| e.frame != frame)
                .min_by_key(|(_, e)| e.used)
            else {
                return drawn;
            };
            if let Some(e) = self.entries.remove(&old) {
                self.bytes -= e.bytes;
            }
        }
        self.tick += 1;
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                drawn: drawn.clone(),
                pins,
                bytes,
                used: self.tick,
                frame: self.frame,
                at: Instant::now(),
            },
        );
        drawn
    }
}

/// The pixmaps that `hash_item` (and a key's nested groups) name by
/// address among `items`, to pin in the entry keyed by them.
fn pins(items: &[DisplayItem], groups: &HashMap<usize, Drawn>) -> Vec<Weak<Pixmap>> {
    let mut out = Vec::new();
    for d in items {
        match &d.item {
            Item::Image { pixmap, .. } => out.push(Arc::downgrade(pixmap)),
            Item::PushLayer(l) => {
                if let Some(g) = &l.gpu {
                    out.push(Arc::downgrade(&g.pixmap));
                }
                if let Some(n) = groups.get(&layer_key(l)) {
                    out.push(Arc::downgrade(&n.pixmap));
                }
            }
            _ => {}
        }
    }
    out
}

/// The sides ([left, top, right, bottom]) where a group drawn over
/// `region` meets the edge of `surface`: what it holds may go on past
/// there unseen (its bounds are clipped to the surface), and a group
/// that ends there has only its blur's faded tail at the edge.
pub(crate) fn cut_sides(region: Rect, surface: Rect) -> [bool; 4] {
    let end = |p: i32, l: u32| i64::from(p) + i64::from(l);
    [
        region.x <= surface.x,
        region.y <= surface.y,
        end(region.x, region.w) >= end(surface.x, surface.w),
        end(region.y, region.h) >= end(surface.y, surface.h),
    ]
}

/// Draws a group's items into a pixmap of `region` and applies its
/// spatial and colour effects; `bundled`: its bundled passes' CPU
/// versions too (false for a GPU pass's input, which the GPU filters).
/// `cut`: the sides ([`cut_sides`]) where the surface cut the group,
/// past which its blur reads the edge repeated. A large blur is kept at
/// its reduction ([`Drawn::k`]). `None` if the region is too large for
/// one context (the group then draws unfiltered).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_group(
    items: &[DisplayItem],
    layer: &crate::layers::Layer,
    region: Rect,
    cut: [bool; 4],
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    groups: &HashMap<usize, Drawn>,
    bundled: bool,
) -> Option<Drawn> {
    let s = scale.as_f32();
    // (lock-blur) A large blur is drawn at its reduction, filtered there
    // and (lock-int) kept so, scaled up where it is drawn: a full-screen
    // still under `blur(36)` took over a second at full size, and its
    // upscaled pixels were 16 times the bytes, which a lock on three large
    // monitors outgrew the budget with, rebuilding every frame.
    let k = group_reduction(&layer.effects, layer.mosaic.is_some(), s);
    let mut pm = rasterise_at(items, layer.xform, region, k, atlas, cache, scale, groups)?;
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let kf = k as f32;
    let bytes = pm.data_as_u8_slice_mut();
    if let Some(cell) = layer.mosaic {
        // The cells start at the box's corner on the surface.
        let corner = layer.xform * layer.frame.origin();
        let phase = (
            (corner.x - region.x as f64).round() as i64,
            (corner.y - region.y as f64).round() as i64,
        );
        mosaic(bytes, w, h, cell as usize, phase);
    }
    // (lock-int) What the surface cut off a group is unseen, not absent:
    // its blur reads the edge repeated there, as a backdrop's does, so a
    // full-screen still under `blur(36)` does not fade to what is under
    // it at the screen's edges. A reduced pixel half past the region's
    // end (a size that is no multiple of `k`) is the one before it.
    let edges = cut.map(|c| if c { Edge::Repeat } else { Edge::Transparent });
    if k > 1
        && layer
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Blur { .. }))
    {
        if cut[2] && !region.w.is_multiple_of(k) && w > 1 {
            for y in 0..h {
                let o = (y * w + w - 1) * 4;
                bytes.copy_within(o - 4..o, o);
            }
        }
        if cut[3] && !region.h.is_multiple_of(k) && h > 1 {
            let o = (h - 1) * w * 4;
            bytes.copy_within(o - w * 4..o, o);
        }
    }
    for e in layer.effects.iter() {
        match e {
            Effect::Blur { radius } => blur_edge(bytes, w, h, radius * s / kf, edges),
            Effect::ColorMatrix(m) => color_matrix(bytes, m),
            // `bloom(r)` without a GPU: a glow of the group's own pixels,
            // reaching its radius (3σ; the uniform is in buffer pixels).
            Effect::Shader(pass) if bundled && crate::layers::cpu_glow(e) => {
                let r = pass.uniforms.first().copied().unwrap_or(0.0);
                crate::effects::glow::glow_under(bytes, w, h, r / 3.0 / kf, None);
            }
            _ => {}
        }
    }
    Some(Drawn {
        pixmap: Arc::new(pm),
        x: region.x,
        y: region.y,
        k,
        origin: (0.0, 0.0),
        w: region.w,
        h: region.h,
    })
}

/// Draws `items` under `xform` into a pixmap of `region` (surface
/// pixels), unfiltered. `None` if the region is too large for one
/// context. (Only the GPU backend's passes ask for it unreduced.)
#[cfg(feature = "gpu")]
pub(crate) fn rasterise(
    items: &[DisplayItem],
    xform: Affine,
    region: Rect,
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    groups: &HashMap<usize, Drawn>,
) -> Option<Pixmap> {
    rasterise_at(items, xform, region, 1, atlas, cache, scale, groups)
}

/// [`rasterise`] at `1 / k` of the region's size (rounded up), its
/// origin at the region's corner.
#[allow(clippy::too_many_arguments)]
fn rasterise_at(
    items: &[DisplayItem],
    xform: Affine,
    region: Rect,
    k: u32,
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    groups: &HashMap<usize, Drawn>,
) -> Option<Pixmap> {
    let k = k.max(1);
    let (w, h) = (
        u16::try_from(region.w.div_ceil(k)).ok()?,
        u16::try_from(region.h.div_ceil(k)).ok()?,
    );
    let settings = RenderSettings {
        num_threads: 0,
        ..RenderSettings::default()
    };
    let mut ctx = RenderContext::new_with(w, h, settings);
    let base = Affine::scale(1.0 / f64::from(k))
        * Affine::translate((-(region.x as f64), -(region.y as f64)));
    ctx.set_transform(base * xform);
    crate::raster::draw_group(
        &mut ctx,
        items,
        region,
        atlas,
        cache,
        scale,
        base,
        xform,
        groups,
        &HashMap::new(),
    );
    ctx.flush();
    // Drawn straight into the pixmap that is kept, and filtered there:
    // the transient heap is the context and the blur's few rows (design.md
    // bounds the groups at about 32 MB; a group over the budget is drawn on
    // every damaged frame).
    let mut pm = Pixmap::new(w, h);
    let mut resources = Resources::new();
    ctx.render_with(
        pm.as_mut(),
        &mut resources,
        RasterizerSettings {
            target_init: TargetInit::SrcOver,
            render_mode: RenderMode::OptimizeQuality,
            ..RasterizerSettings::default()
        },
    );
    drop(ctx);
    Some(pm)
}

/// `pixelate`: every `cell × cell` square of premultiplied pixels (`w × h`,
/// the grid starting at `phase`, which may lie outside) becomes the
/// average of its pixels, as if drawn at one sample per cell and sampled
/// back up by nearest neighbour.
pub fn mosaic(px: &mut [u8], w: usize, h: usize, cell: usize, phase: (i64, i64)) {
    if cell <= 1 || px.len() < w * h * 4 {
        return;
    }
    let c = cell as i64;
    // The first cell's start: the grid line at or before 0.
    let start = |p: i64| -> i64 { -(-p).rem_euclid(c) };
    let (sx, sy) = (start(phase.0), start(phase.1));
    let mut y0 = sy;
    while y0 < h as i64 {
        let (ya, yb) = (y0.max(0) as usize, ((y0 + c) as usize).min(h));
        let mut x0 = sx;
        while x0 < w as i64 {
            let (xa, xb) = (x0.max(0) as usize, ((x0 + c) as usize).min(w));
            let mut sum = [0u32; 4];
            for y in ya..yb {
                for p in px[(y * w + xa) * 4..(y * w + xb) * 4].chunks_exact(4) {
                    for k in 0..4 {
                        sum[k] += p[k] as u32;
                    }
                }
            }
            let n = ((yb - ya) * (xb - xa)).max(1) as u32;
            let avg = sum.map(|v| ((v + n / 2) / n) as u8);
            for y in ya..yb {
                for p in px[(y * w + xa) * 4..(y * w + xb) * 4].chunks_exact_mut(4) {
                    p.copy_from_slice(&avg);
                }
            }
            x0 += c;
        }
        y0 += c;
    }
}

/// What a blur reads past the pixels it has.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    /// Nothing: a group fades out past its edge (`filter: blur`, where
    /// the group itself ends).
    Transparent,
    /// The edge pixels, repeated: what is behind a backdrop goes on past
    /// what was read of it, as the GPU's pass samples it (lock-blur).
    Repeat,
}

/// A Gaussian blur of standard deviation `sigma` pixels over premultiplied
/// pixels (separable, out to 3σ; transparent past the edges).
pub fn blur(px: &mut [u8], w: usize, h: usize, sigma: f32) {
    blur_edge(px, w, h, sigma, [Edge::Transparent; 4]);
}

/// [`blur`], reading `edges` past the pixels' edges ([left, top, right,
/// bottom]).
pub(crate) fn blur_edge(px: &mut [u8], w: usize, h: usize, sigma: f32, edges: [Edge; 4]) {
    blur_edge_lanes(px, w, h, sigma, edges);
}

/// (blur-ready) Work, in pixels times taps, from which a blur is split
/// into bands of rows on the raster's helpers ([`crate::raster::share`]):
/// about a millisecond of one core's, ten times a helper's start.
const BANDED_WORK: usize = 1 << 22;

/// [`blur_edge`]; returns the threads that ran it (1: on the calling
/// thread alone).
///
/// (blur-ready) A large blur runs in bands of output rows on the
/// raster's helpers, each band as the whole blur runs (its rows' row
/// passes into a ring, then each output row's column pass in kernel
/// order), so every pixel's sums are the same, in the same order, and
/// the pixels are the same byte for byte. A band also reads the `r`
/// rows above and below it, which the bands beside it overwrite: those
/// come from a copy taken before the bands start. The helpers are
/// joined before it returns (idle is zero work), and the heap is each
/// band's ring and the copied rows, freed on return.
pub(crate) fn blur_edge_lanes(
    px: &mut [u8],
    w: usize,
    h: usize,
    sigma: f32,
    edges: [Edge; 4],
) -> usize {
    if !(sigma.is_finite() && sigma > 0.0) || w == 0 || h == 0 || px.len() < w * h * 4 {
        return 1;
    }
    let r = (3.0 * sigma).ceil() as usize;
    let kernel: Vec<f32> = {
        let k: Vec<f32> = (0..=2 * r)
            .map(|i| {
                let x = i as f32 - r as f32;
                (-(x * x) / (2.0 * sigma * sigma)).exp()
            })
            .collect();
        let sum: f32 = k.iter().sum();
        k.into_iter().map(|v| v / sum).collect()
    };
    let stride = w * 4;
    let px = &mut px[..h * stride];
    // Asked once, on this thread, for every band.
    #[cfg(target_arch = "x86_64")]
    let wide = wide();
    #[cfg(not(target_arch = "x86_64"))]
    let wide = false;
    // Bands of at least 2r rows (each also passes the 2r rows around
    // it), one per thread that may run.
    #[cfg(test)]
    let worth = w * h * (2 * r + 1) >= BANDED_WORK || ALWAYS_BAND.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let worth = w * h * (2 * r + 1) >= BANDED_WORK;
    let lanes = if worth {
        crate::raster::threads().min(h / (2 * r).max(1))
    } else {
        1
    };
    if lanes < 2 {
        blur_band(px, &[], &[], (0, h), w, h, r, &kernel, edges, wide);
        return 1;
    }
    let rows = h.div_ceil(lanes);
    let bands: Vec<(usize, usize)> = (0..h)
        .step_by(rows)
        .map(|y0| (y0, (y0 + rows).min(h)))
        .collect();
    // The rows each band reads that the others write, copied first.
    let halos: Vec<(Vec<u8>, Vec<u8>)> = bands
        .iter()
        .map(|&(y0, y1)| {
            let (lo, hi) = (y0.saturating_sub(r), (y1 + r).min(h));
            (
                px[lo * stride..y0 * stride].to_vec(),
                px[y1 * stride..hi * stride].to_vec(),
            )
        })
        .collect();
    let slices: Vec<std::sync::Mutex<&mut [u8]>> = px
        .chunks_mut(rows * stride)
        .map(std::sync::Mutex::new)
        .collect();
    let mut states = vec![(); bands.len()];
    #[cfg(test)]
    let tids = std::sync::Mutex::new(Vec::new());
    let ran = crate::raster::share(&mut states, bands.len(), |_, i| {
        #[cfg(test)]
        if let Ok(mut t) = tids.lock() {
            // SAFETY: gettid has no preconditions.
            t.push(unsafe { libc::gettid() } as i64);
        }
        let mut band = slices[i]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (above, below) = &halos[i];
        blur_band(
            &mut band, above, below, bands[i], w, h, r, &kernel, edges, wide,
        );
    });
    #[cfg(test)]
    LAST_TIDS.with(|t| *t.borrow_mut() = tids.into_inner().unwrap_or_default());
    ran
}

#[cfg(test)]
thread_local! {
    /// (tests) The threads the last banded filter called on this thread
    /// ran its bands on.
    static LAST_TIDS: RefCell<Vec<i64>> = const { RefCell::new(Vec::new()) };
    /// (tests) This thread's filters split whatever their work, so small
    /// buffers test the bands.
    static ALWAYS_BAND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// (tests) This thread's colour matrices use their table whatever
    /// their size, so small buffers test it.
    static ALWAYS_TABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// (tests) This thread's blurs run the build for any x86-64 CPU,
    /// not the AVX2 one.
    static NARROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// (tests) The colour matrix tables this thread has built.
    static TABLES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The blur of output rows `y0..y1` (`band`, whose input pixels it
/// overwrites in place), reading the input rows above it from `above`
/// (rows `y0 − r..y0`, clamped) and below it from `below` (rows
/// `y1..y1 + r`, clamped): the whole blur with `y0 = 0` and `y1 = h`.
///
/// (blur-ready review) On a CPU with AVX2 it runs compiled for it: the
/// same multiplies and adds (never fused) in the same order, eight lanes
/// at a time rather than four, so the same bytes
/// (`the_avx2_blur_is_the_blur`).
#[allow(clippy::too_many_arguments)]
fn blur_band(
    band: &mut [u8],
    above: &[u8],
    below: &[u8],
    rows: (usize, usize),
    w: usize,
    h: usize,
    r: usize,
    kernel: &[f32],
    edges: [Edge; 4],
    wide: bool,
) {
    #[cfg(target_arch = "x86_64")]
    if wide {
        // SAFETY: `wide` is true only when this CPU has AVX2 ([`wide`]).
        unsafe { blur_band_avx2(band, above, below, rows, w, h, r, kernel, edges) };
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = wide;
    blur_band_body::<32>(band, above, below, rows, w, h, r, kernel, edges);
}

/// Whether the blur runs its AVX2 build: the CPU has it (and, in tests,
/// the calling thread has not turned it off).
#[cfg(target_arch = "x86_64")]
fn wide() -> bool {
    #[cfg(test)]
    if NARROW.with(std::cell::Cell::get) {
        return false;
    }
    std::arch::is_x86_feature_detected!("avx2")
}

/// [`blur_band_body`] compiled for AVX2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
fn blur_band_avx2(
    band: &mut [u8],
    above: &[u8],
    below: &[u8],
    rows: (usize, usize),
    w: usize,
    h: usize,
    r: usize,
    kernel: &[f32],
    edges: [Edge; 4],
) {
    blur_band_body::<64>(band, above, below, rows, w, h, r, kernel, edges);
}

/// [`blur_band`]'s work, inlined into each build of it, summing `L`
/// floats at a time ([`convolve`]).
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn blur_band_body<const L: usize>(
    band: &mut [u8],
    above: &[u8],
    below: &[u8],
    (y0, y1): (usize, usize),
    w: usize,
    h: usize,
    r: usize,
    kernel: &[f32],
    edges: [Edge; 4],
) {
    let stride = w * 4;
    let (lo, hi) = (y0.saturating_sub(r), (y1 + r).min(h));
    // Rows are blurred into a ring of the `2r + 1` rows the column pass
    // reads (f32, as the columns need them), and each output row is
    // written back in place once its rows are in: a row read for the ring
    // is always below every row written so far. The heap is the ring,
    // not a copy of the whole group.
    let ring_rows = (2 * r + 1).min(hi - lo);
    let mut ring = vec![0f32; ring_rows * stride];
    let mut out = vec![0f32; stride];
    // (lock-blur) A row padded by `r` pixels each side (zero, or its edge
    // pixel repeated), so the row pass runs tap by tap over whole rows,
    // which the compiler vectorises: the same sums in the same order as
    // one pixel at a time (a tap past a transparent edge adds zero).
    // (blur-ready review) Both passes sum a few dozen outputs through
    // every tap in registers ([`convolve`]), in the same order.
    let mut padded = vec![0f32; stride + 2 * r * 4];
    // The taps of the row being summed: weight and where its row starts.
    let mut taps: Vec<(f32, usize)> = Vec::with_capacity(2 * r + 1);
    let mut next = lo;
    for y in y0..y1 {
        let last = (y + r).min(h - 1);
        while next <= last {
            let row = if next < y0 {
                &above[(next - lo) * stride..(next - lo + 1) * stride]
            } else if next >= y1 {
                &below[(next - y1) * stride..(next - y1 + 1) * stride]
            } else {
                &band[(next - y0) * stride..(next - y0 + 1) * stride]
            };
            for (d, s) in padded[r * 4..r * 4 + stride].iter_mut().zip(row) {
                *d = *s as f32;
            }
            // (Zero where transparent: `padded` is only ever written
            // inside the row or by a repeating side.)
            let (first, end) = (&row[..4], &row[stride - 4..]);
            for x in 0..r {
                for c in 0..4 {
                    if edges[0] == Edge::Repeat {
                        padded[x * 4 + c] = first[c] as f32;
                    }
                    if edges[2] == Edge::Repeat {
                        padded[(r + w + x) * 4 + c] = end[c] as f32;
                    }
                }
            }
            let slot = &mut ring[(next % ring_rows) * stride..(next % ring_rows + 1) * stride];
            taps.extend(kernel.iter().enumerate().map(|(i, k)| (*k, i * 4)));
            convolve::<L>(slot, &padded, &taps);
            taps.clear();
            next += 1;
        }
        // The column pass for row `y`, tap by tap in kernel order (the
        // same sums, in the same order, as one pixel at a time).
        for (i, k) in kernel.iter().enumerate() {
            let sy = y as isize + i as isize - r as isize;
            let past = match sy {
                _ if sy < 0 => Some(edges[1]),
                _ if sy >= h as isize => Some(edges[3]),
                _ => None,
            };
            if past == Some(Edge::Transparent) {
                continue;
            }
            let sy = sy.clamp(0, h as isize - 1) as usize;
            taps.push((*k, (sy % ring_rows) * stride));
        }
        convolve::<L>(&mut out, &ring, &taps);
        taps.clear();
        let dst = &mut band[(y - y0) * stride..(y - y0 + 1) * stride];
        for (d, acc) in dst.chunks_exact_mut(4).zip(out.chunks_exact(4)) {
            let a = acc[3].round().clamp(0.0, 255.0);
            d[3] = a as u8;
            for c in 0..3 {
                // Premultiplied: a channel never exceeds its alpha.
                d[c] = acc[c].round().clamp(0.0, a) as u8;
            }
        }
    }
}

/// `out[j] = Σ k · src[at + j]` over `taps` (`(k, at)`), in their order,
/// for every `j` in `out`: `L` outputs at a time, each summed in a
/// register through every tap before it is stored, so the sums are the
/// tap-by-tap sums over whole rows (from 0, in tap order) and each output
/// is stored once rather than read and written at every tap.
///
/// The inner loop indexes rather than zips: as fast optimised, and as
/// fast as the loops it replaced in the debug builds the lock VM's tests
/// run, where zipping an array and a slice cost a quarter more.
#[inline(always)]
#[allow(clippy::needless_range_loop)]
fn convolve<const L: usize>(out: &mut [f32], src: &[f32], taps: &[(f32, usize)]) {
    let n = out.len();
    let whole = n - n % L;
    for (c, chunk) in out[..whole].chunks_exact_mut(L).enumerate() {
        let j = c * L;
        let mut acc = [0f32; L];
        for &(k, at) in taps {
            let s = &src[at + j..at + j + L];
            for t in 0..L {
                acc[t] += k * s[t];
            }
        }
        chunk.copy_from_slice(&acc);
    }
    for (j, o) in out.iter_mut().enumerate().skip(whole) {
        let mut a = 0f32;
        for &(k, at) in taps {
            a += k * src[at + j];
        }
        *o = a;
    }
}

/// The scale a backdrop blur of standard deviation `sigma` device pixels
/// is drawn at (its reduction: drawn at `1 / k`, blurred there, scaled
/// back up), on either backend: a quarter, as design.md has it, half or
/// full size for a blur under 8 or 2 device pixels, which a quarter-scale
/// pass would turn into blocks, and (lock-blur) an eighth from 64, where
/// it is still 8 of its pixels wide.
pub(crate) fn reduction(sigma: f32) -> u32 {
    if sigma >= 64.0 {
        8
    } else if sigma >= 8.0 {
        4
    } else if sigma >= 2.0 {
        2
    } else {
        1
    }
}

/// The reduction a `filter: blur` of `sigma` device pixels is drawn at:
/// half size from 8 device pixels, a quarter from 32 and an eighth from
/// 64, so the blur is at least 4 of its pixels wide at half size and 8
/// at a quarter or less. A group is drawn at reduced scale by sampling,
/// not averaging: an image under a blur 4 of its pixels wide at a quarter
/// came out up to 9 levels off a full-size blur, and 2 at 8 wide
/// (lock-blur).
pub(crate) fn filter_reduction(sigma: f32) -> u32 {
    if sigma >= 64.0 {
        8
    } else if sigma >= 32.0 {
        4
    } else if sigma >= 8.0 {
        2
    } else {
        1
    }
}

/// The reduction a group of `effects` is drawn at on a surface at
/// scale `s` ([`filter_reduction`], the smallest of its blurs'): full
/// size under a mosaic, whose cells the scaling up would soften, and
/// when a colour matrix after a blur changes alpha (a glow's doubled
/// coverage, a stroke's edge), which would sharpen what the scaling up
/// smoothed. A colour-only matrix after it (`blur(36) brightness(0.7)`)
/// keeps the reduction: with alpha kept it is linear in premultiplied
/// pixels (but for its clamping), so it gives the same before or after
/// the scaling up, as it does before the blur.
pub(crate) fn group_reduction(effects: &[Effect], mosaic: bool, s: f32) -> u32 {
    let reshaped = effects
        .iter()
        .skip_while(|e| !matches!(e, Effect::Blur { .. }))
        .any(|e| matches!(e, Effect::ColorMatrix(m) if m[15..] != [0.0, 0.0, 0.0, 1.0, 0.0]));
    if mosaic || reshaped {
        return 1;
    }
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Blur { radius } => Some(filter_reduction(radius * s)),
            _ => None,
        })
        .min()
        .unwrap_or(1)
}

/// Bilinear upscaling by `k` of `src` (premultiplied, `sw × sh`) into
/// `dst` (`dw × dh`), whose origin is at `origin` in `src`'s pixels,
/// sampled from pixel centres with the edge pixels repeated, as the
/// GPU draws a reduced backdrop back up (`Op::Backdrop`'s `reduce`).
/// (lock-blur) Each source row is interpolated across once, and each
/// destination row is a blend of two of those.
#[cfg(any(feature = "gpu", test))]
pub(crate) fn upscale(
    src: &[u8],
    (sw, sh): (usize, usize),
    dst: &mut [u8],
    (dw, dh): (usize, usize),
    origin: (f64, f64),
    k: f64,
) {
    if sw == 0 || sh == 0 || src.len() < sw * sh * 4 || dst.len() < dw * dh * 4 {
        return;
    }
    // A pixel centre's place in `src`: its two neighbours and the share
    // of the second.
    let place = |o: f64, i: usize, n: usize| {
        let f = (o + (i as f64 + 0.5) / k - 0.5).max(0.0);
        let a = (f.floor() as usize).min(n - 1);
        (a, (a + 1).min(n - 1), f.fract() as f32)
    };
    let cols: Vec<(usize, usize, f32)> = (0..dw).map(|x| place(origin.0, x, sw)).collect();
    let across = |j: usize, out: &mut Vec<f32>| {
        let row = &src[j * sw * 4..(j + 1) * sw * 4];
        out.clear();
        for &(a, b, t) in &cols {
            for c in 0..4 {
                let (p, q) = (row[a * 4 + c] as f32, row[b * 4 + c] as f32);
                out.push(p + (q - p) * t);
            }
        }
    };
    // The two source rows the last destination row read, across.
    let (mut top, mut bottom) = (Vec::with_capacity(dw * 4), Vec::with_capacity(dw * 4));
    let mut have: Option<(usize, usize)> = None;
    for y in 0..dh {
        let (a, b, t) = place(origin.1, y, sh);
        if have != Some((a, b)) {
            match have {
                // Moving down a row: the old bottom is the new top.
                Some((_, ob)) if ob == a => std::mem::swap(&mut top, &mut bottom),
                _ => across(a, &mut top),
            }
            across(b, &mut bottom);
            have = Some((a, b));
        }
        let out = &mut dst[y * dw * 4..(y + 1) * dw * 4];
        for ((d, p), q) in out.iter_mut().zip(&top).zip(&bottom) {
            *d = (p + (q - p) * t).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Applies a straight-alpha colour matrix (rows r, g, b, a; the fifth
/// column an offset in 0..1) to premultiplied pixels whose red and blue
/// are swapped (the raster's BGRA order).
pub fn color_matrix(px: &mut [u8], m: &[f32; 20]) {
    color_matrix_lanes(px, m);
}

/// (blur-ready) Pixels from which a colour matrix is split into bands
/// on the raster's helpers: about a millisecond of one core's.
const MATRIX_BANDED: usize = 1 << 18;

/// (blur-ready review) Pixels in a band from which a colour matrix looks
/// its straight channels up in a table (256 KB, zeroed, its rows filled
/// as alphas show up): with every alpha present, filling it costs what
/// about 22,000 pixels worked out one at a time do.
const MATRIX_TABLE: usize = 1 << 15;

/// [`color_matrix`]; returns the threads that ran it.
///
/// (blur-ready) Each pixel goes through the same float operations as
/// before, so the pixels are the same byte for byte; what changed is how
/// often they run. A straight channel `c / 255 / a` is looked up in a
/// table of the same expression, filled one alpha at a time as the
/// pixels show it (a blurred still is mostly opaque: one row of 256); a
/// transparent pixel, whose result depends only on the matrix's offsets,
/// is worked out once; and a large buffer is split into bands on the
/// raster's helpers, joined before it returns.
pub(crate) fn color_matrix_lanes(px: &mut [u8], m: &[f32; 20]) -> usize {
    let n = px.len() / 4;
    let px = &mut px[..n * 4];
    let mut clear = [0u8; 4];
    matrix_pixel(&mut clear, m, None);
    #[cfg(test)]
    let worth = n >= MATRIX_BANDED || ALWAYS_BAND.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let worth = n >= MATRIX_BANDED;
    let lanes = if worth {
        crate::raster::threads().min(n.max(1))
    } else {
        1
    };
    let per = n.div_ceil(lanes) * 4;
    // The table pays for itself once a band's pixels outnumber what its
    // rows cost to fill (256 divisions an alpha, three a pixel without
    // it) with every alpha shown: below that, a small group (an icon
    // under `grayscale(1)`, a glow's matrix on a line of text) works
    // each pixel out as it always did.
    #[cfg(test)]
    let table = per / 4 >= MATRIX_TABLE || ALWAYS_TABLE.with(std::cell::Cell::get);
    #[cfg(not(test))]
    let table = per / 4 >= MATRIX_TABLE;
    if lanes < 2 {
        matrix_band(px, m, clear, table);
        return 1;
    }
    let slices: Vec<std::sync::Mutex<&mut [u8]>> =
        px.chunks_mut(per).map(std::sync::Mutex::new).collect();
    let mut states = vec![(); slices.len()];
    crate::raster::share(&mut states, slices.len(), |_, i| {
        let mut band = slices[i]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matrix_band(&mut band, m, clear, table);
    })
}

/// [`color_matrix`] over `px` on this thread, `clear` its result for a
/// transparent pixel, with the table of `c / 255 / a` when `table`.
fn matrix_band(px: &mut [u8], m: &[f32; 20], clear: [u8; 4], table: bool) {
    if !table {
        for p in px.chunks_exact_mut(4) {
            if p[3] == 0 {
                p.copy_from_slice(&clear);
            } else {
                matrix_pixel(p, m, None);
            }
        }
        return;
    }
    #[cfg(test)]
    TABLES.with(|t| t.set(t.get() + 1));
    // `c / 255 / a` by alpha and channel, each alpha's row filled when a
    // pixel first shows it.
    let mut table = vec![0f32; 256 * 256];
    let mut filled = [false; 256];
    for p in px.chunks_exact_mut(4) {
        let alpha = p[3] as usize;
        if alpha == 0 {
            p.copy_from_slice(&clear);
            continue;
        }
        let row = &mut table[alpha * 256..(alpha + 1) * 256];
        if !filled[alpha] {
            let a = p[3] as f32 / 255.0;
            for (c, v) in row.iter_mut().enumerate() {
                *v = c as f32 / 255.0 / a;
            }
            filled[alpha] = true;
        }
        matrix_pixel(p, m, Some(row));
    }
}

/// One pixel of [`color_matrix`]: `un`, its alpha's row of `c / 255 /
/// a` (computed here when `None`).
#[inline(always)]
fn matrix_pixel(p: &mut [u8], m: &[f32; 20], un: Option<&[f32]>) {
    let a = p[3] as f32 / 255.0;
    let un = |c: u8| match un {
        Some(row) if a > 0.0 => row[c as usize],
        _ if a > 0.0 => c as f32 / 255.0 / a,
        _ => 0.0,
    };
    // Stored b, g, r, a.
    let v = [un(p[2]), un(p[1]), un(p[0]), a];
    let row = |i: usize| {
        let r = &m[i * 5..i * 5 + 5];
        (r[0] * v[0] + r[1] * v[1] + r[2] * v[2] + r[3] * v[3] + r[4]).clamp(0.0, 1.0)
    };
    let (r, g, b, na) = (row(0), row(1), row(2), row(3));
    let q = |c: f32| (c * na * 255.0).round().clamp(0.0, 255.0) as u8;
    p[0] = q(b);
    p[1] = q(g);
    p[2] = q(r);
    p[3] = (na * 255.0).round() as u8;
}

/// A CPU raster node's pixels (S-effects: particles, grain, graphs,
/// spectrum, animated image frames).
pub trait RasterSource: std::fmt::Debug + Send + Sync {
    /// Draws the node into `pixels` (premultiplied RGBA, row-major,
    /// `w × h` physical pixels at `scale`), at `time`.
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, time: TimeContext);

    /// (blur-ready) [`RasterSource::draw`] in the raster's order, red and
    /// blue swapped (each pixel's `r` holds its blue): by default drawn,
    /// then swapped. A source that can write that order directly saves
    /// the pass over its pixels.
    fn draw_bgra(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, time: TimeContext) {
        self.draw(pixels, w, h, scale, time);
        for p in pixels {
            std::mem::swap(&mut p.r, &mut p.b);
        }
    }

    /// (blur-ready) What its pixels are drawn from, apart from the node
    /// and its size: nodes whose sources name the same content, drawn at
    /// the same size and scale, share one pixmap (two `thumbnail`s of one
    /// screen's still). `None` (the default): its own.
    fn content(&self) -> Option<RasterContent> {
        None
    }

    /// How often it changes: its clock's rate.
    fn rate(&self) -> Rate;

    /// (M4, S-effects) Its clock, if it has one: `rate()` by default. A
    /// source whose pixels change only when fed (a spectrum) has none,
    /// so it costs no frame between feeds.
    fn clock(&self) -> Option<Rate> {
        Some(self.rate())
    }

    /// (M4, S-effects) Reads the node's resolved props for the frame
    /// being drawn and returns a hash of everything besides size, scale
    /// and time that its pixels depend on (its props, what it was fed):
    /// the pixmap is drawn again when it changes. Called on every
    /// flatten that draws the node, so it must not count calls.
    fn state(&self, _props: &RasterProps<'_>) -> u64 {
        0
    }
}

/// (blur-ready) What a [`RasterSource`]'s pixels are drawn from
/// ([`RasterSource::content`]): a hash of it, and the bytes it names by
/// address, if any, held weakly so the address is not handed to other
/// bytes while a pixmap is shared under it.
#[derive(Clone, Debug)]
pub struct RasterContent {
    pub key: u64,
    pub pin: Option<Weak<[u8]>>,
}

/// (M4) What a [`RasterSource`] reads of its node: its props as resolved
/// for this frame (tokens and springs applied) and the colour it
/// inherits.
pub struct RasterProps<'a> {
    pub get: &'a dyn Fn(strand_scene::Prop) -> Option<&'a strand_scene::PropValue>,
    pub color: strand_scene::Color,
    /// (M4) An `svg`'s `#id { … }` parts: each id and its props as
    /// resolved for this frame (empty for every other node).
    pub parts: &'a [(String, Vec<(strand_scene::Prop, strand_scene::PropValue)>)],
    /// (M4) Where a source's file jobs run (the image store's worker, or
    /// inline): `None` runs them inline.
    pub files: Option<&'a crate::image::ImageStore>,
    /// (graph-follow) The frame being drawn on the boot clock
    /// (`Animator::frame_boot`; zero in a frame with no clock): where a
    /// `graph`'s columns fall, and (review) when it saw its value.
    pub boot: std::time::Duration,
    /// (review) How early a tick counts as reached: half a frame, as a
    /// capped clock's (`Animator::slack`).
    pub slack: std::time::Duration,
    /// (graph-follow) Time stands still in this frame (`reduced_motion`).
    pub still: bool,
    /// (review) This frame will be painted, not a preview flattened at
    /// the last painted frame's time (`Animator::commits`).
    pub commit: bool,
}

impl RasterProps<'_> {
    /// Runs the file job `key` ([`crate::image::ImageStore::load_file`]):
    /// its result, or `None` while it is on the worker.
    pub(crate) fn load_file(
        &self,
        key: &str,
        work: impl FnOnce() -> crate::image::Loaded + Send + 'static,
    ) -> Option<crate::image::Loaded> {
        match self.files {
            Some(store) => store.load_file(key, work),
            None => Some(work()),
        }
    }
}

impl std::fmt::Debug for RasterProps<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RasterProps")
            .field("color", &self.color)
            .finish_non_exhaustive()
    }
}

/// (grain-over) Which of a node's raster pixmaps: its own pixels (a
/// source of its own, or one its props build: an `effect`, `particles`,
/// a merge's goo), or its `grain:` over them. Each keeps its last pixmap
/// apart, so drawing one never throws the other's away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RasterLayer {
    Own,
    Grain,
}

/// A raster pixmap kept: the key it was drawn at, its pixels, and (a
/// pixmap shared by content) the pin of the bytes it was drawn from.
type Kept = (u64, Arc<Pixmap>, Option<Arc<Pin>>);

/// (blur-ready) The bytes a shared pixmap was drawn from, by a `Weak` of
/// them ([`RasterContent::pin`]), so their address is not handed to
/// other bytes while it is shared under it. Held by the nodes that keep
/// the pixmap (their [`Kept`]) and by nothing else: a `Weak` keeps its
/// target's whole allocation (a 4K still's 33 MB) after the last strong
/// reference goes, so it is dropped with the last node, never left in
/// the map until a later paint prunes it.
#[derive(Debug)]
struct Pin(Option<Weak<[u8]>>);

/// (blur-ready) A pixmap shared by content: the pin of the bytes it was
/// drawn from, and the pixmap, both weakly (a dead entry keeps neither).
#[derive(Debug)]
struct Shared {
    pin: Weak<Pin>,
    pixmap: Weak<Pixmap>,
}

impl Shared {
    /// Its pixmap and pin, while some node keeps them and the bytes it
    /// was drawn from are the ones `pin` names.
    fn take(&self, pin: &Option<Weak<[u8]>>) -> Option<(Arc<Pixmap>, Arc<Pin>)> {
        let held = self.pin.upgrade()?;
        let same = match (&held.0, pin) {
            (None, None) => true,
            (Some(a), Some(b)) => Weak::ptr_eq(a, b) && a.strong_count() > 0,
            _ => false,
        };
        same.then(|| self.pixmap.upgrade().map(|p| (p, held)))
            .flatten()
    }

    /// Whether it can still be shared: a node keeps it and its bytes are
    /// alive.
    fn live(&self) -> bool {
        self.pixmap.strong_count() > 0
            && self
                .pin
                .upgrade()
                .is_some_and(|p| p.0.as_ref().is_none_or(|b| b.strong_count() > 0))
    }
}

/// Raster sources by node, and each node's last pixmaps.
#[derive(Debug, Default)]
pub struct RasterNodes {
    sources: HashMap<NodeId, Arc<dyn RasterSource>>,
    drawn: RefCell<HashMap<(NodeId, RasterLayer), Kept>>,
    /// (blur-ready) Pixmaps drawn from a source's content
    /// ([`RasterSource::content`]), by that content, size and scale: a
    /// node whose source names it takes the pixmap another drew. Weak,
    /// so a pixmap lives exactly as long as a node keeps it.
    shared: RefCell<HashMap<u64, Shared>>,
    builds: RefCell<u64>,
    /// (blur-ready) Wall-clock time spent drawing pixmaps so far
    /// (advisory; measurements read it).
    build_time: std::cell::Cell<Duration>,
}

impl RasterNodes {
    /// Sets (or with `None` removes) `node`'s source: a source with state
    /// of its own, fed from outside the props (the media stream's
    /// graphs, spectrum and animated frames); sources built from props go
    /// through [`RasterNodes::pixmap_from`].
    pub fn set(&mut self, node: NodeId, source: Option<Arc<dyn RasterSource>>) {
        match source {
            Some(s) => {
                self.sources.insert(node, s);
            }
            None => {
                self.sources.remove(&node);
            }
        }
        self.drawn.get_mut().remove(&(node, RasterLayer::Own));
        self.shared.get_mut().retain(|_, s| s.live());
    }

    /// Drops nodes `keep` rejects.
    pub fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.sources.retain(|id, _| keep(*id));
        self.drawn.get_mut().retain(|(id, _), _| keep(*id));
        self.shared.get_mut().retain(|_, s| s.live());
    }

    /// Drops `node`'s last pixmap of `layer` when nothing draws it: its
    /// own when it has no source of its own (it builds none from its
    /// props any more), its grain when it has none.
    pub fn unused(&self, node: NodeId, layer: RasterLayer) {
        if layer == RasterLayer::Grain || !self.sources.contains_key(&node) {
            // Only a pixmap let go can leave a shared entry with no node.
            if self.drawn.borrow_mut().remove(&(node, layer)).is_some() {
                self.shared.borrow_mut().retain(|_, s| s.live());
            }
        }
    }

    /// `node`'s clock rate, if it is a raster node with a clock.
    pub fn rate(&self, node: NodeId) -> Option<Rate> {
        self.sources.get(&node).and_then(|s| s.clock())
    }

    /// Pixmaps drawn so far (tests).
    pub fn builds(&self) -> u64 {
        *self.builds.borrow()
    }

    /// (blur-ready) Wall-clock time spent drawing pixmaps so far.
    pub fn build_time(&self) -> Duration {
        self.build_time.get()
    }

    /// Bytes of the pixmaps kept (a pixmap kept by several nodes once).
    pub fn bytes(&self) -> usize {
        let drawn = self.drawn.borrow();
        let mut seen = std::collections::HashSet::new();
        drawn
            .values()
            .filter(|(_, p, _)| seen.insert(Arc::as_ptr(p) as usize))
            .map(|(_, p, _)| p.width() as usize * p.height() as usize * 4)
            .sum()
    }

    /// (blur-ready) Pixmaps shared by content that can still be shared
    /// (tests).
    #[cfg(test)]
    fn shared(&self) -> usize {
        self.shared.borrow().values().filter(|s| s.live()).count()
    }

    /// `node`'s own pixels at `w × h` and `time`: the last pixmap if its key
    /// (state, size, scale, time) is unchanged, else drawn anew. Its key too,
    /// for the node's damage signature.
    pub fn pixmap(
        &self,
        node: NodeId,
        w: u32,
        h: u32,
        scale: f32,
        time: TimeContext,
        props: &RasterProps<'_>,
    ) -> Option<(u64, Arc<Pixmap>)> {
        let source = self.sources.get(&node)?;
        // Its state (props, what it was fed) is its config.
        let state = source.state(props);
        self.pixmap_from(
            (node, RasterLayer::Own),
            source.as_ref(),
            state,
            w,
            h,
            scale,
            time,
        )
    }

    /// `node`'s pixels of `layer` drawn by `source`, built from its props
    /// (`config` says from what: a change draws anew), as
    /// [`RasterNodes::pixmap`].
    #[allow(clippy::too_many_arguments)]
    pub fn pixmap_from(
        &self,
        (node, layer): (NodeId, RasterLayer),
        source: &dyn RasterSource,
        config: u64,
        w: u32,
        h: u32,
        scale: f32,
        time: TimeContext,
    ) -> Option<(u64, Arc<Pixmap>)> {
        let (w16, h16) = (u16::try_from(w).ok()?, u16::try_from(h).ok()?);
        if w16 == 0 || h16 == 0 {
            return None;
        }
        let mut k = DefaultHasher::new();
        (
            config,
            w,
            h,
            scale.to_bits(),
            time.t.to_bits(),
            time.index,
            time.count,
        )
            .hash(&mut k);
        let key = k.finish();
        if let Some((old, pm, _)) = self.drawn.borrow().get(&(node, layer))
            && *old == key
        {
            return Some((key, pm.clone()));
        }
        // (blur-ready) Content another node drew at this size: its pixmap.
        let content = source.content().map(|c| {
            let mut k = DefaultHasher::new();
            (c.key, w, h, scale.to_bits()).hash(&mut k);
            (k.finish(), c.pin)
        });
        if let Some((shared, pin)) = &content {
            let found = self.shared.borrow().get(shared).and_then(|s| s.take(pin));
            if let Some((pm, held)) = found {
                self.drawn
                    .borrow_mut()
                    .insert((node, layer), (key, pm.clone(), Some(held)));
                return Some((key, pm));
            }
        }
        let started = std::time::Instant::now();
        let mut pm = Pixmap::new(w16, h16);
        // The raster draws red and blue swapped (BGRA buffers).
        source.draw_bgra(pm.data_mut(), w, h, scale, time);
        let pm = Arc::new(pm);
        *self.builds.borrow_mut() += 1;
        self.build_time
            .set(self.build_time.get() + started.elapsed());
        let content = content.map(|(shared, pin)| (shared, Arc::new(Pin(pin))));
        let old = self.drawn.borrow_mut().insert(
            (node, layer),
            (key, pm.clone(), content.as_ref().map(|(_, p)| p.clone())),
        );
        if let Some((shared, held)) = content {
            let mut map = self.shared.borrow_mut();
            map.insert(
                shared,
                Shared {
                    pin: Arc::downgrade(&held),
                    pixmap: Arc::downgrade(&pm),
                },
            );
            // The pixmap it kept before goes now, and with it any entry
            // it was the last to keep.
            drop(old);
            map.retain(|_, s| s.live());
        }
        Some((key, pm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (m4-audit) A group's key names its images by address. A pixmap
    /// freed after the group was cached must not hand its address to a
    /// new pixmap with other pixels, which would hit the stale group
    /// (the allocator gives a just-freed block of the same size back
    /// first): the entry pins every address it names.
    #[test]
    fn a_freed_images_address_never_hits_a_stale_group() {
        use strand_scene::Size;
        let surface = Rect::new(0, 0, 16, 16);
        let damage = Damage::full(Size::new(16, 16));
        let scale = Scale::default();
        let full = vello_cpu::kurbo::Rect::new(0.0, 0.0, 16.0, 16.0);
        let layer = Arc::new(crate::layers::Layer {
            effects: Arc::from([Effect::Blur { radius: 1.0 }]),
            frame: full,
            scale: 1.0,
            xform: Affine::IDENTITY,
            mosaic: None,
            gpu: None,
        });
        let image = |px: [u8; 4]| {
            let mut pm = Pixmap::new(16, 16);
            for p in pm.data_as_u8_slice_mut().chunks_exact_mut(4) {
                p.copy_from_slice(&px);
            }
            Arc::new(pm)
        };
        let items = |pm: &Arc<Pixmap>| {
            let at = |item| DisplayItem {
                item,
                bounds: surface,
            };
            vec![
                at(Item::PushLayer(layer.clone())),
                at(Item::Image {
                    pixmap: pm.clone(),
                    rect: full,
                    dest: full,
                    tint: None,
                }),
                at(Item::PopLayer),
            ]
        };
        let (atlas, cache) = (AtlasMirror::default(), PaintCache::default());
        let mut off = Offscreen::default();
        let centre = |off: &Offscreen| {
            let d = &off.current()[&layer_key(&layer)];
            let (w, h) = (d.pixmap.width() as usize, d.pixmap.height() as usize);
            let o = ((h / 2) * w + w / 2) * 4;
            d.pixmap.data_as_u8_slice()[o..o + 4].to_vec()
        };
        let red = image([255, 0, 0, 255]);
        let addr = Arc::as_ptr(&red) as usize;
        off.prepare(&items(&red), &damage, surface, &atlas, &cache, scale);
        assert_eq!(off.builds(), 1);
        assert!(centre(&off)[0] > 200, "{:?}", centre(&off));
        drop(red);
        let blue = image([0, 0, 255, 255]);
        let others: Vec<_> = (0..64).map(|_| image([0, 0, 0, 0])).collect();
        assert!(
            std::iter::once(&blue)
                .chain(&others)
                .all(|p| Arc::as_ptr(p) as usize != addr),
            "a cached group's image address was handed out again"
        );
        off.prepare(&items(&blue), &damage, surface, &atlas, &cache, scale);
        assert_eq!(off.builds(), 2, "the stale group was hit");
        assert!(centre(&off)[2] > 200, "{:?}", centre(&off));
    }

    #[test]
    fn blur_spreads_and_keeps_coverage() {
        let (w, h) = (21, 1);
        let mut px = vec![0u8; w * h * 4];
        px[10 * 4..10 * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
        blur(&mut px, w, h, 1.0);
        let alpha: Vec<u8> = px.chunks(4).map(|p| p[3]).collect();
        assert!(alpha[10] < 255 && alpha[10] > 0);
        assert!(alpha[11] > 0 && alpha[9] == alpha[11], "symmetric");
        assert_eq!(alpha[0], 0, "3σ away: nothing");
        for p in px.chunks(4) {
            assert!(p[0] <= p[3], "premultiplied");
        }
    }

    /// The whole-group two-pass blur the ring replaced (an f32 copy of
    /// every row pass).
    fn blur_whole(px: &mut [u8], w: usize, h: usize, sigma: f32) {
        let r = (3.0 * sigma).ceil() as usize;
        let k: Vec<f32> = (0..=2 * r)
            .map(|i| {
                let x = i as f32 - r as f32;
                (-(x * x) / (2.0 * sigma * sigma)).exp()
            })
            .collect();
        let sum: f32 = k.iter().sum();
        let kernel: Vec<f32> = k.into_iter().map(|v| v / sum).collect();
        let mut tmp = vec![0f32; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                for (i, k) in kernel.iter().enumerate() {
                    let sx = x as isize + i as isize - r as isize;
                    if (0..w as isize).contains(&sx) {
                        for c in 0..4 {
                            tmp[(y * w + x) * 4 + c] +=
                                k * px[(y * w + sx as usize) * 4 + c] as f32;
                        }
                    }
                }
            }
        }
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0f32; 4];
                for (i, k) in kernel.iter().enumerate() {
                    let sy = y as isize + i as isize - r as isize;
                    if (0..h as isize).contains(&sy) {
                        for c in 0..4 {
                            acc[c] += k * tmp[(sy as usize * w + x) * 4 + c];
                        }
                    }
                }
                let o = (y * w + x) * 4;
                let a = acc[3].round().clamp(0.0, 255.0);
                px[o + 3] = a as u8;
                for c in 0..3 {
                    px[o + c] = acc[c].round().clamp(0.0, a) as u8;
                }
            }
        }
    }

    /// The in-place ring blur gives exactly the whole-group blur, for
    /// groups taller and shorter than its ring.
    #[test]
    fn ring_blur_matches_the_whole_group_blur() {
        for (w, h, sigma) in [(37, 53, 2.0), (29, 5, 3.0), (16, 40, 0.7), (1, 9, 4.0)] {
            let mut px = vec![0u8; w * h * 4];
            let mut seed = 0x2545_f491_u32;
            for p in px.chunks_exact_mut(4) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let a = (seed >> 24) as u8;
                for (c, v) in p[..3].iter_mut().enumerate() {
                    *v = ((seed >> (c * 8)) as u8).min(a);
                }
                p[3] = a;
            }
            let mut whole = px.clone();
            blur_whole(&mut whole, w, h, sigma);
            blur(&mut px, w, h, sigma);
            assert!(px == whole, "{w}×{h} σ {sigma}");
        }
    }

    /// The blur as it was before blur-ready: the whole of it on one
    /// thread, a ring of row passes and each output row's column pass.
    fn blur_edge_serial(px: &mut [u8], w: usize, h: usize, sigma: f32, edges: [Edge; 4]) {
        if !(sigma.is_finite() && sigma > 0.0) || w == 0 || h == 0 || px.len() < w * h * 4 {
            return;
        }
        let r = (3.0 * sigma).ceil() as usize;
        let kernel: Vec<f32> = {
            let k: Vec<f32> = (0..=2 * r)
                .map(|i| {
                    let x = i as f32 - r as f32;
                    (-(x * x) / (2.0 * sigma * sigma)).exp()
                })
                .collect();
            let sum: f32 = k.iter().sum();
            k.into_iter().map(|v| v / sum).collect()
        };
        let ring_rows = (2 * r + 1).min(h);
        let stride = w * 4;
        let mut ring = vec![0f32; ring_rows * stride];
        let mut out = vec![0f32; stride];
        let mut padded = vec![0f32; stride + 2 * r * 4];
        let mut next = 0;
        for y in 0..h {
            let last = (y + r).min(h - 1);
            while next <= last {
                let row = &px[next * stride..(next + 1) * stride];
                for (d, s) in padded[r * 4..r * 4 + stride].iter_mut().zip(row) {
                    *d = *s as f32;
                }
                let (first, end) = (&row[..4], &row[stride - 4..]);
                for x in 0..r {
                    for c in 0..4 {
                        if edges[0] == Edge::Repeat {
                            padded[x * 4 + c] = first[c] as f32;
                        }
                        if edges[2] == Edge::Repeat {
                            padded[(r + w + x) * 4 + c] = end[c] as f32;
                        }
                    }
                }
                let slot = &mut ring[(next % ring_rows) * stride..(next % ring_rows + 1) * stride];
                slot.fill(0.0);
                for (i, k) in kernel.iter().enumerate() {
                    let src = &padded[i * 4..i * 4 + stride];
                    for (o, v) in slot.iter_mut().zip(src) {
                        *o += k * v;
                    }
                }
                next += 1;
            }
            out.fill(0.0);
            for (i, k) in kernel.iter().enumerate() {
                let sy = y as isize + i as isize - r as isize;
                let past = match sy {
                    _ if sy < 0 => Some(edges[1]),
                    _ if sy >= h as isize => Some(edges[3]),
                    _ => None,
                };
                if past == Some(Edge::Transparent) {
                    continue;
                }
                let sy = sy.clamp(0, h as isize - 1) as usize;
                let slot = sy % ring_rows;
                let src = &ring[slot * stride..(slot + 1) * stride];
                for (o, v) in out.iter_mut().zip(src) {
                    *o += k * v;
                }
            }
            let dst = &mut px[y * stride..(y + 1) * stride];
            for (d, acc) in dst.chunks_exact_mut(4).zip(out.chunks_exact(4)) {
                let a = acc[3].round().clamp(0.0, 255.0);
                d[3] = a as u8;
                for c in 0..3 {
                    d[c] = acc[c].round().clamp(0.0, a) as u8;
                }
            }
        }
    }

    /// Premultiplied noise (`w × h`), some pixels transparent.
    fn noise(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut seed = seed | 1;
        let mut px = vec![0u8; w * h * 4];
        for p in px.chunks_exact_mut(4) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let a = if seed & 7 == 0 { 0 } else { (seed >> 24) as u8 };
            for (c, v) in p[..3].iter_mut().enumerate() {
                *v = ((seed >> (c * 8)) as u8).min(a);
            }
            p[3] = a;
        }
        px
    }

    /// (blur-ready) The blur split into bands on the raster's helpers is
    /// the serial blur byte for byte: every edge mode, sizes from 1 × 1
    /// to a reduced 4K group (the lock's: 960 × 540 at σ 13.5), heights
    /// below the ring and just past the bands' threshold, odd widths,
    /// sigmas from half a pixel to 64. Small buffers are split as large
    /// ones are, whatever their work.
    #[test]
    fn banded_blur_is_the_serial_blur() {
        crate::raster::unbudgeted();
        ALWAYS_BAND.with(|b| b.set(true));
        let small = [0.5f32, 1.9, 3.0, 13.5, 27.0, 64.0];
        let cases: [((usize, usize), &[f32]); 9] = [
            ((1, 1), &small),
            ((3, 1000), &small),
            ((1000, 3), &small),
            ((31, 50), &small),
            // 2r rows a band at σ 13.5 (r = 41): two bands, then three,
            // and at the other sigmas up to 4 (a band of 1 row at 0.5).
            ((17, 164), &[13.5]),
            ((19, 247), &[13.5]),
            ((33, 333), &[0.5, 1.9, 3.0, 13.5, 27.0, 64.0]),
            ((1, 400), &[3.0, 13.5]),
            ((960, 540), &[13.5]),
        ];
        let modes = [Edge::Transparent, Edge::Repeat];
        let mut banded = 0;
        for (n, ((w, h), sigmas)) in cases.into_iter().enumerate() {
            let src = noise(w, h, 0x2545_f491 ^ n as u32);
            // Every edge combination where bands meet them; the large
            // group with none and all repeating.
            let combos: Vec<usize> = if (w, h) == (960, 540) {
                vec![0, 15]
            } else {
                (0..16).collect()
            };
            for &sigma in sigmas {
                for &e in &combos {
                    let edges: [Edge; 4] = std::array::from_fn(|i| modes[(e >> i) & 1]);
                    let mut want = src.clone();
                    blur_edge_serial(&mut want, w, h, sigma, edges);
                    let mut got = src.clone();
                    let ran = blur_edge_lanes(&mut got, w, h, sigma, edges);
                    banded += usize::from(ran > 1);
                    let first = want.iter().zip(&got).position(|(a, b)| a != b);
                    assert_eq!(
                        first.map(|i| (i / 4 % w, i / 4 / w, want[i], got[i])),
                        None,
                        "{w}×{h} σ {sigma} edges {edges:?} on {ran} threads"
                    );
                }
            }
        }
        ALWAYS_BAND.with(|b| b.set(false));
        if crate::raster::threads() > 1 {
            assert!(banded > 100, "only {banded} blurs ran in bands");
        }
    }

    /// (blur-ready review) The blur's AVX2 build is its build for any
    /// x86-64 CPU byte for byte, whole and in bands, at every edge mode.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_avx2_blur_is_the_blur() {
        if !std::arch::is_x86_feature_detected!("avx2") {
            return;
        }
        crate::raster::unbudgeted();
        let modes = [Edge::Transparent, Edge::Repeat];
        for (n, (w, h, sigma)) in [
            (1, 1, 0.5f32),
            (7, 300, 3.0),
            (131, 97, 13.5),
            (960, 540, 13.5),
        ]
        .into_iter()
        .enumerate()
        {
            let src = noise(w, h, 0x51ed_270b ^ n as u32);
            for e in [0usize, 5, 10, 15] {
                let edges: [Edge; 4] = std::array::from_fn(|i| modes[(e >> i) & 1]);
                for split in [false, true] {
                    ALWAYS_BAND.with(|b| b.set(split));
                    let mut narrow = src.clone();
                    NARROW.with(|b| b.set(true));
                    blur_edge_lanes(&mut narrow, w, h, sigma, edges);
                    NARROW.with(|b| b.set(false));
                    let mut wide = src.clone();
                    blur_edge_lanes(&mut wide, w, h, sigma, edges);
                    let first = narrow.iter().zip(&wide).position(|(a, b)| a != b);
                    assert_eq!(
                        first, None,
                        "{w}×{h} σ {sigma} edges {edges:?} split {split}"
                    );
                }
            }
        }
        ALWAYS_BAND.with(|b| b.set(false));
    }

    /// (blur-ready) A large blur (the lock's group: 960 × 540 at σ 13.5)
    /// runs on the raster's helpers; a small one on the calling thread.
    /// Once it returns, none of the helpers is left.
    #[test]
    fn a_large_blur_runs_on_the_raster_lanes() {
        crate::raster::unbudgeted();
        let mut px = noise(960, 540, 7);
        let ran = blur_edge_lanes(&mut px, 960, 540, 13.5, [Edge::Repeat; 4]);
        let want = crate::raster::threads().min(540 / 82);
        assert_eq!(ran, want, "the lock's group's blur");
        let tids = LAST_TIDS.with(|t| t.borrow().clone());
        // SAFETY: gettid has no preconditions.
        let me = unsafe { libc::gettid() } as i64;
        for tid in tids.into_iter().filter(|t| *t != me) {
            let task = format!("/proc/self/task/{tid}");
            let gone = (0..200).any(|_| {
                let gone = !std::path::Path::new(&task).exists();
                if !gone {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                gone
            });
            assert!(gone, "helper {tid} outlived the blur");
        }
        let mut px = noise(64, 64, 9);
        assert_eq!(blur_edge_lanes(&mut px, 64, 64, 13.5, [Edge::Repeat; 4]), 1);
    }

    /// The colour matrix as it was before blur-ready, pixel by pixel.
    fn color_matrix_serial(px: &mut [u8], m: &[f32; 20]) {
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as f32 / 255.0;
            let un = |c: u8| if a > 0.0 { c as f32 / 255.0 / a } else { 0.0 };
            let v = [un(p[2]), un(p[1]), un(p[0]), a];
            let row = |i: usize| {
                let r = &m[i * 5..i * 5 + 5];
                (r[0] * v[0] + r[1] * v[1] + r[2] * v[2] + r[3] * v[3] + r[4]).clamp(0.0, 1.0)
            };
            let (r, g, b, na) = (row(0), row(1), row(2), row(3));
            let q = |c: f32| (c * na * 255.0).round().clamp(0.0, 255.0) as u8;
            p[0] = q(b);
            p[1] = q(g);
            p[2] = q(r);
            p[3] = (na * 255.0).round() as u8;
        }
    }

    /// (blur-ready) The colour matrix with its table, its transparent
    /// pixel worked out once and its bands is the per-pixel one byte for
    /// byte: every alpha, channels at and under it and a few over it,
    /// for brightness, grayscale, saturate, a glow's alpha gain and a
    /// matrix with offsets, on one thread and split.
    #[test]
    fn color_matrix_is_the_reference_for_every_alpha() {
        use crate::effects::filter;
        crate::raster::unbudgeted();
        let mut gain = strand_scene::effect::IDENTITY_MATRIX;
        gain[18] = 2.0;
        let mut offsets = filter::saturate(1.4);
        offsets[4] = 0.1;
        offsets[9] = -0.05;
        offsets[14] = 0.3;
        offsets[19] = 0.2;
        let matrices = [
            filter::brightness(0.6),
            filter::grayscale(1.0),
            filter::saturate(2.0),
            gain,
            offsets,
        ];
        let mut px = Vec::new();
        let mut seed = 0x9e37_79b9u32;
        for a in 0..=255u32 {
            for _ in 0..64 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let c = |s: u32| (s % (a + 1)) as u8;
                px.extend_from_slice(&[c(seed), c(seed >> 8), c(seed >> 16), a as u8]);
            }
            // At and over the alpha (a compositor can send them).
            let over = (a + 1).min(255) as u8;
            px.extend_from_slice(&[a as u8, a as u8, a as u8, a as u8]);
            px.extend_from_slice(&[over, 0, 255, a as u8]);
        }
        for m in &matrices {
            let mut want = px.clone();
            color_matrix_serial(&mut want, m);
            for (split, table) in [(false, false), (false, true), (true, false), (true, true)] {
                ALWAYS_BAND.with(|b| b.set(split));
                ALWAYS_TABLE.with(|b| b.set(table));
                let mut got = px.clone();
                let ran = color_matrix_lanes(&mut got, m);
                let first = want.iter().zip(&got).position(|(a, b)| a != b);
                assert_eq!(
                    first.map(|i| (&px[i / 4 * 4..i / 4 * 4 + 4], want[i], got[i])),
                    None,
                    "{m:?} on {ran} threads, table {table}"
                );
            }
            ALWAYS_BAND.with(|b| b.set(false));
            ALWAYS_TABLE.with(|b| b.set(false));
        }
    }

    /// (blur-ready) A full-screen colour matrix (3840 × 2160) runs on the
    /// raster's helpers; a small one on the calling thread.
    #[test]
    fn a_full_screen_colour_matrix_runs_on_the_raster_lanes() {
        crate::raster::unbudgeted();
        let m = crate::effects::filter::brightness(0.6);
        let mut px = vec![255u8; 3840 * 2160 * 4];
        assert_eq!(color_matrix_lanes(&mut px, &m), crate::raster::threads());
        let mut px = vec![255u8; 64 * 64 * 4];
        assert_eq!(color_matrix_lanes(&mut px, &m), 1);
    }

    /// (blur-ready review) A small colour matrix (an icon, a line of
    /// text's glow) builds no table, whose 256 KB and row fills would
    /// cost it more than its pixels do; a large one builds one.
    #[test]
    fn a_small_colour_matrix_builds_no_table() {
        let m = crate::effects::filter::grayscale(1.0);
        let tables = || TABLES.with(std::cell::Cell::get);
        let before = tables();
        for (w, h) in [(16, 16), (48, 48), (200, 40), (128, 128)] {
            let mut px = noise(w, h, 5);
            assert_eq!(color_matrix_lanes(&mut px, &m), 1);
        }
        assert_eq!(tables(), before, "a small matrix built a table");
        let mut px = noise(256, 256, 5);
        assert_eq!(color_matrix_lanes(&mut px, &m), 1);
        assert_eq!(tables(), before + 1, "a 256 × 256 matrix uses its table");
    }

    /// A source of one colour, drawn from `bytes` (its content).
    #[derive(Debug)]
    struct Fill {
        bytes: Arc<[u8]>,
    }

    impl RasterSource for Fill {
        fn draw(&self, pixels: &mut [PremulRgba8], _: u32, _: u32, _: f32, _: TimeContext) {
            for p in pixels {
                *p = PremulRgba8 {
                    r: self.bytes[0],
                    g: 0,
                    b: 0,
                    a: 255,
                };
            }
        }

        fn rate(&self) -> Rate {
            Rate::Refresh
        }

        fn content(&self) -> Option<RasterContent> {
            Some(RasterContent {
                key: Arc::as_ptr(&self.bytes) as *const u8 as usize as u64,
                pin: Some(Arc::downgrade(&self.bytes)),
            })
        }
    }

    /// (blur-ready) Nodes whose sources name one content share one
    /// pixmap at one size (drawn once, kept once), and draw their own at
    /// another; the pixmap lives as long as a node keeps it, and once
    /// none does, nothing keeps the content's bytes either.
    #[test]
    fn a_shared_pixmap_is_freed_with_its_last_node() {
        let bytes: Arc<[u8]> = Arc::from(vec![200u8; 4]);
        let mut nodes = RasterNodes::default();
        let (a, b, c) = (NodeId::new(1, 0), NodeId::new(2, 0), NodeId::new(3, 0));
        for n in [a, b, c] {
            nodes.set(
                n,
                Some(Arc::new(Fill {
                    bytes: bytes.clone(),
                })),
            );
        }
        let t = TimeContext::default();
        let get = |_: strand_scene::Prop| None;
        let props = RasterProps {
            get: &get,
            color: strand_scene::Color::WHITE,
            parts: &[],
            files: None,
            boot: Duration::ZERO,
            slack: Duration::ZERO,
            still: false,
            commit: true,
        };
        let (_, pa) = nodes.pixmap(a, 40, 30, 1.0, t, &props).unwrap();
        let (_, pb) = nodes.pixmap(b, 40, 30, 1.0, t, &props).unwrap();
        let (_, pc) = nodes.pixmap(c, 20, 30, 1.0, t, &props).unwrap();
        assert!(Arc::ptr_eq(&pa, &pb), "one pixmap at one size");
        assert!(!Arc::ptr_eq(&pa, &pc), "its own at another");
        assert_eq!(nodes.builds(), 2);
        assert_eq!(nodes.bytes(), (40 * 30 + 20 * 30) * 4, "kept once");
        assert_eq!(pa.data()[0].b, 200, "in the raster's order");
        let shared = Arc::downgrade(&pa);
        drop((pa, pb, pc));
        nodes.retain(|n| n == b);
        assert!(shared.upgrade().is_some(), "kept while a node keeps it");
        assert_eq!(nodes.shared(), 1);
        nodes.retain(|_| false);
        assert!(shared.upgrade().is_none(), "freed with its last node");
        assert_eq!(nodes.bytes(), 0);
        assert_eq!(nodes.shared(), 0);
        assert!(nodes.shared.borrow().is_empty(), "no dead entry is left");
        assert_eq!(Arc::weak_count(&bytes), 0, "nothing pins the content");
    }

    /// (blur-ready review) A shared pixmap that outlives its nodes (the
    /// last display list or a group's pin keeps it while the nodes go)
    /// leaves no pin on its content: once the frame's bytes are dropped
    /// they are freed, with no paint after (an idle shell after an
    /// unlock), not kept whole by a `Weak` until one.
    #[test]
    fn a_shared_pixmap_kept_past_its_nodes_pins_no_content() {
        let bytes: Arc<[u8]> = Arc::from(vec![200u8; 4]);
        let mut nodes = RasterNodes::default();
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        for n in [a, b] {
            nodes.set(
                n,
                Some(Arc::new(Fill {
                    bytes: bytes.clone(),
                })),
            );
        }
        let t = TimeContext::default();
        let get = |_: strand_scene::Prop| None;
        let props = RasterProps {
            get: &get,
            color: strand_scene::Color::WHITE,
            parts: &[],
            files: None,
            boot: Duration::ZERO,
            slack: Duration::ZERO,
            still: false,
            commit: true,
        };
        let (_, pa) = nodes.pixmap(a, 40, 30, 1.0, t, &props).unwrap();
        let (_, pb) = nodes.pixmap(b, 40, 30, 1.0, t, &props).unwrap();
        assert!(Arc::ptr_eq(&pa, &pb));
        drop(pb);
        // The nodes go while the pixmap is still kept elsewhere.
        nodes.retain(|_| false);
        // Nothing but the frame's own holders keeps its bytes: dropped,
        // they are freed with no call on `nodes` after.
        assert_eq!(Arc::weak_count(&bytes), 0, "no pin outlives the nodes");
        assert_eq!(nodes.shared(), 0, "nothing is shared without a node");
        drop(pa);
    }

    #[test]
    fn color_matrix_reads_bgra() {
        // grayscale-ish: r' = b (straight), others kept.
        let mut m = strand_scene::effect::IDENTITY_MATRIX;
        m[0] = 0.0;
        m[2] = 1.0;
        // A half-transparent pure blue, stored b, g, r, a premultiplied.
        let mut px = vec![128, 0, 0, 128];
        color_matrix(&mut px, &m);
        assert_eq!(px, vec![128, 0, 128, 128], "red now equals blue");
    }

    /// Each cell becomes its pixels' average, the grid starting at the
    /// phase (cut cells at the edges average what they hold).
    #[test]
    fn mosaic_averages_each_cell_from_its_phase() {
        let (w, h) = (5, 2);
        // A ramp: pixel x has value 10·x in every channel.
        let mut px: Vec<u8> = (0..w * h).flat_map(|i| [(10 * (i % w)) as u8; 4]).collect();
        mosaic(&mut px, w, h, 2, (1, 0));
        let row: Vec<u8> = px[..w * 4].chunks(4).map(|p| p[0]).collect();
        // Cells [0], [1, 2], [3, 4].
        assert_eq!(row, vec![0, 15, 15, 35, 35]);
        assert_eq!(px[..w * 4], px[w * 4..], "rows of one cell agree");
        // A cell of 1 changes nothing.
        let before = px.clone();
        mosaic(&mut px, w, h, 1, (0, 0));
        assert_eq!(px, before);
    }

    /// (lock-blur review) A colour-only matrix after a blur keeps the
    /// group's reduction, whichever order the filter is written in; one
    /// that changes alpha after it (a glow's, a stroke edge's), or a
    /// mosaic, keeps it at full size.
    #[test]
    fn a_colour_matrix_after_a_blur_keeps_its_reduction() {
        use crate::effects::filter;
        use strand_scene::effect::IDENTITY_MATRIX;
        let blur = Effect::Blur { radius: 36.0 };
        let dim = Effect::ColorMatrix(filter::brightness(0.7));
        let k = |e: &[Effect]| group_reduction(e, false, 1.0);
        assert_eq!(k(std::slice::from_ref(&blur)), 4);
        assert_eq!(k(&[dim.clone(), blur.clone()]), 4);
        assert_eq!(k(&[blur.clone(), dim.clone()]), 4);
        for m in [
            filter::grayscale(1.0),
            filter::saturate(1.4),
            filter::contrast(1.2),
            filter::invert(1.0),
            filter::tint(strand_scene::Color::new(0.5, 0.6, 0.9, 0.8)),
        ] {
            assert_eq!(k(&[blur.clone(), Effect::ColorMatrix(m)]), 4, "{m:?}");
        }
        let mut gain = IDENTITY_MATRIX;
        gain[18] = 2.0;
        assert_eq!(k(&[blur.clone(), Effect::ColorMatrix(gain)]), 1, "a glow's");
        let mut grow = IDENTITY_MATRIX;
        grow[18] = 8.0;
        grow[19] = -1.0;
        assert_eq!(
            k(&[blur.clone(), Effect::ColorMatrix(grow)]),
            1,
            "an edge's"
        );
        assert_eq!(
            group_reduction(&[blur.clone(), dim], true, 1.0),
            1,
            "mosaic"
        );
        // The reduction is of device pixels.
        assert_eq!(group_reduction(&[blur], false, 2.0), 8);
    }
}

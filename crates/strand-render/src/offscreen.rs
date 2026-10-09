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
//! frees entries no frame used for [`crate::cache::IDLE_FREE`] or since
//! the frame loop stopped, like the paint cache. A group larger than the
//! budget is drawn each frame it is damaged, uncached.
//!
//! **Raster nodes.** A node with a [`RasterSource`] (particles, grain,
//! graphs, spectrum, animated frames: S-effects) draws into a pixmap of
//! its box at its clock's rate, kept per node and redrawn only when its
//! tick or size changes; the display list carries it as `Item::Raster`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{Damage, Effect, NodeId, Rect, Scale, TimeContext};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo::Affine;
use vello_cpu::{
    Pixmap, PixmapMut, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources,
    TargetInit,
};

use crate::cache::{IDLE_FREE, PaintCache};
use crate::clock::Rate;
use crate::flatten::{DisplayItem, Item};
use crate::raster::AtlasMirror;

/// Bytes of offscreen group pixmaps kept (design.md: "about 4 MB").
pub const OFFSCREEN_BYTES: usize = 4 << 20;

/// A group's filtered pixels and where they go on the surface.
#[derive(Clone, Debug)]
pub struct Drawn {
    pub pixmap: Arc<Pixmap>,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug)]
struct Entry {
    drawn: Drawn,
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
    idle: Option<Duration>,
    /// This frame's groups, by their layer (its `Arc`'s address).
    current: HashMap<usize, Drawn>,
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

    /// Groups kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// This frame's group pixmaps by layer.
    pub fn current(&self) -> &HashMap<usize, Drawn> {
        &self.current
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
        self.recount();
    }

    /// Frees groups no frame has used since `since` (the loop stopped).
    pub fn trim_unused_since(&mut self, since: Instant) {
        self.entries.retain(|_, e| e.at >= since);
        self.recount();
    }

    fn recount(&mut self) {
        self.bytes = self.entries.values().map(|e| e.bytes).sum();
    }

    /// Draws (or finds) the offscreen groups among `items` that touch
    /// `damage`, inner groups first, for this frame's cells.
    pub fn prepare(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        surface: Rect,
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
    ) {
        self.frame += 1;
        self.current.clear();
        let touches = |b: &Rect| damage.rects().iter().any(|r| r.intersects(*b));
        let groups: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                matches!(&d.item, Item::PushLayer(l) if !l.cell_local()) && touches(&d.bounds)
            })
            .map(|(i, _)| i)
            .collect();
        for &i in groups.iter().rev() {
            let Item::PushLayer(layer) = &items[i].item else {
                continue;
            };
            let Some(region) = items[i].bounds.intersect(surface).filter(|r| !r.is_empty()) else {
                continue;
            };
            let end = crate::raster::skip_group(items, i);
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
                    let Some(drawn) =
                        render_group(inner, layer, region, atlas, cache, scale, &self.current)
                    else {
                        continue;
                    };
                    self.builds += 1;
                    self.insert(key, drawn)
                }
            };
            self.current.insert(layer_key(layer), drawn);
        }
    }

    fn get(&mut self, key: u64) -> Option<Drawn> {
        self.tick += 1;
        let (tick, frame) = (self.tick, self.frame);
        self.entries.get_mut(&key).map(|e| {
            e.used = tick;
            e.frame = frame;
            e.at = Instant::now();
            e.drawn.clone()
        })
    }

    /// Keeps `drawn` within the budget (evicting the least recently used
    /// groups this frame does not use); a group over the budget is not
    /// kept.
    fn insert(&mut self, key: u64, drawn: Drawn) -> Drawn {
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
                bytes,
                used: self.tick,
                frame: self.frame,
                at: Instant::now(),
            },
        );
        drawn
    }
}

/// Draws a group's items into a pixmap of `region` and applies its
/// spatial and colour effects. `None` if the region is too large for
/// one context (the group then draws unfiltered).
fn render_group(
    items: &[DisplayItem],
    layer: &crate::layers::Layer,
    region: Rect,
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    groups: &HashMap<usize, Drawn>,
) -> Option<Drawn> {
    let (w, h) = (u16::try_from(region.w).ok()?, u16::try_from(region.h).ok()?);
    let settings = RenderSettings {
        num_threads: 0,
        ..RenderSettings::default()
    };
    let mut ctx = RenderContext::new_with(w, h, settings);
    let base = Affine::translate((-(region.x as f64), -(region.y as f64)));
    ctx.set_transform(base * layer.xform);
    crate::raster::draw_group(
        &mut ctx,
        items,
        region,
        atlas,
        cache,
        scale,
        base,
        layer.xform,
        groups,
    );
    ctx.flush();
    let mut bytes = vec![0u8; w as usize * h as usize * 4];
    let mut resources = Resources::new();
    if let Some(pm) = PixmapMut::new(w, h, &mut bytes) {
        ctx.render_with(
            pm,
            &mut resources,
            RasterizerSettings {
                target_init: TargetInit::SrcOver,
                render_mode: RenderMode::OptimizeQuality,
                ..RasterizerSettings::default()
            },
        );
    }
    let s = scale.as_f32();
    for e in layer.effects.iter() {
        match e {
            Effect::Blur { radius } => blur(&mut bytes, w as usize, h as usize, radius * s),
            Effect::ColorMatrix(m) => color_matrix(&mut bytes, m),
            _ => {}
        }
    }
    let mut pm = Pixmap::new(w, h);
    for (d, s) in pm.data_mut().iter_mut().zip(bytes.chunks_exact(4)) {
        *d = PremulRgba8 {
            r: s[0],
            g: s[1],
            b: s[2],
            a: s[3],
        };
    }
    Some(Drawn {
        pixmap: Arc::new(pm),
        x: region.x,
        y: region.y,
    })
}

/// A Gaussian blur of standard deviation `sigma` pixels over premultiplied
/// pixels (separable, out to 3σ; transparent past the edges).
pub fn blur(px: &mut [u8], w: usize, h: usize, sigma: f32) {
    if !(sigma.is_finite() && sigma > 0.0) || w == 0 || h == 0 {
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
    let mut tmp = vec![0f32; w * h * 4];
    // Rows into `tmp`.
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 4];
            for (i, k) in kernel.iter().enumerate() {
                let sx = x as isize + i as isize - r as isize;
                if sx < 0 || sx >= w as isize {
                    continue;
                }
                let o = (y * w + sx as usize) * 4;
                for c in 0..4 {
                    acc[c] += k * px[o + c] as f32;
                }
            }
            tmp[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&acc);
        }
    }
    // Columns back into `px`.
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 4];
            for (i, k) in kernel.iter().enumerate() {
                let sy = y as isize + i as isize - r as isize;
                if sy < 0 || sy >= h as isize {
                    continue;
                }
                let o = (sy as usize * w + x) * 4;
                for c in 0..4 {
                    acc[c] += k * tmp[o + c];
                }
            }
            let o = (y * w + x) * 4;
            let a = acc[3].round().clamp(0.0, 255.0);
            px[o + 3] = a as u8;
            for c in 0..3 {
                // Premultiplied: a channel never exceeds its alpha.
                px[o + c] = acc[c].round().clamp(0.0, a) as u8;
            }
        }
    }
}

/// Applies a straight-alpha colour matrix (rows r, g, b, a; the fifth
/// column an offset in 0..1) to premultiplied pixels whose red and blue
/// are swapped (the raster's BGRA order).
pub fn color_matrix(px: &mut [u8], m: &[f32; 20]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as f32 / 255.0;
        let un = |c: u8| if a > 0.0 { c as f32 / 255.0 / a } else { 0.0 };
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
}

/// A CPU raster node's pixels (S-effects: particles, grain, graphs,
/// spectrum, animated image frames).
pub trait RasterSource: std::fmt::Debug + Send + Sync {
    /// Draws the node into `pixels` (premultiplied RGBA, row-major,
    /// `w × h` physical pixels at `scale`), at `time`.
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, time: TimeContext);

    /// How often it changes: its clock's rate.
    fn rate(&self) -> Rate;
}

/// Raster sources by node, and each node's last pixmap.
#[derive(Debug, Default)]
pub struct RasterNodes {
    sources: HashMap<NodeId, Arc<dyn RasterSource>>,
    drawn: RefCell<HashMap<NodeId, (u64, Arc<Pixmap>)>>,
    builds: RefCell<u64>,
}

impl RasterNodes {
    /// Sets (or with `None` removes) `node`'s source.
    pub fn set(&mut self, node: NodeId, source: Option<Arc<dyn RasterSource>>) {
        match source {
            Some(s) => {
                self.sources.insert(node, s);
            }
            None => {
                self.sources.remove(&node);
            }
        }
        self.drawn.get_mut().remove(&node);
    }

    /// Drops nodes `keep` rejects.
    pub fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.sources.retain(|id, _| keep(*id));
        let sources = &self.sources;
        self.drawn
            .get_mut()
            .retain(|id, _| sources.contains_key(id));
    }

    /// `node`'s clock rate, if it is a raster node.
    pub fn rate(&self, node: NodeId) -> Option<Rate> {
        self.sources.get(&node).map(|s| s.rate())
    }

    /// Pixmaps drawn so far (tests).
    pub fn builds(&self) -> u64 {
        *self.builds.borrow()
    }

    /// Bytes of the pixmaps kept.
    pub fn bytes(&self) -> usize {
        self.drawn
            .borrow()
            .values()
            .map(|(_, p)| p.width() as usize * p.height() as usize * 4)
            .sum()
    }

    /// `node`'s pixels at `w × h` and `time`: the last pixmap if its key
    /// (size, scale, time) is unchanged, else drawn anew. Its key too,
    /// for the node's damage signature.
    pub fn pixmap(
        &self,
        node: NodeId,
        w: u32,
        h: u32,
        scale: f32,
        time: TimeContext,
    ) -> Option<(u64, Arc<Pixmap>)> {
        let source = self.sources.get(&node)?;
        let (w16, h16) = (u16::try_from(w).ok()?, u16::try_from(h).ok()?);
        if w16 == 0 || h16 == 0 {
            return None;
        }
        let mut k = DefaultHasher::new();
        (
            w,
            h,
            scale.to_bits(),
            time.t.to_bits(),
            time.index,
            time.count,
        )
            .hash(&mut k);
        let key = k.finish();
        if let Some((old, pm)) = self.drawn.borrow().get(&node)
            && *old == key
        {
            return Some((key, pm.clone()));
        }
        let mut pm = Pixmap::new(w16, h16);
        source.draw(pm.data_mut(), w, h, scale, time);
        // The raster draws red and blue swapped (BGRA buffers).
        for p in pm.data_mut() {
            std::mem::swap(&mut p.r, &mut p.b);
        }
        let pm = Arc::new(pm);
        *self.builds.borrow_mut() += 1;
        self.drawn.borrow_mut().insert(node, (key, pm.clone()));
        Some((key, pm))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

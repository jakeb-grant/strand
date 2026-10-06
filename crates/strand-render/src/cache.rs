//! Cached paint: dithered gradients and blurred shadows rendered once
//! into pixmaps and drawn from there (design.md, "Paint and light":
//! banding removed by automatic dithering; "Runtime changes": cached
//! offscreen groups, about 4 MB).
//!
//! Every entry is a pure function of its key, and whether a key is
//! cached at all depends only on the key (its size), so a partial
//! repaint draws exactly what a full one does.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{Color, GradientStop, Paint};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{Extend, Fill, ImageQuality, ImageSampler};
use vello_cpu::{
    Image, ImageSource, PaintType, Pixmap, PixmapMut, RasterizerSettings, RenderContext,
    RenderMode, RenderSettings, Resources, TargetInit,
};

/// Bytes of cached pixmaps kept, least recently used dropped first.
pub const PAINT_CACHE_BYTES: usize = 4 << 20;

/// Largest single cached pixmap; larger gradients are dithered cell by
/// cell, uncached, and larger shadows draw directly (decided by size
/// alone).
pub const MAX_ENTRY_BYTES: usize = 2 << 20;

/// Entries in a gradient's colour table.
const LUT: usize = 1024;

/// 8 × 8 Bayer matrix: thresholds 0..64.
const BAYER: [u8; 64] = [
    0, 32, 8, 40, 2, 34, 10, 42, 48, 16, 56, 24, 50, 18, 58, 26, 12, 44, 4, 36, 14, 46, 6, 38, 60,
    28, 52, 20, 62, 30, 54, 22, 3, 35, 11, 43, 1, 33, 9, 41, 51, 19, 59, 27, 49, 17, 57, 25, 15,
    47, 7, 39, 13, 45, 5, 37, 63, 31, 55, 23, 61, 29, 53, 21,
];

/// The dither offset of pixel `(x, y)`, in `-0.5..0.5` of one level.
fn dither(x: u32, y: u32) -> f32 {
    (BAYER[((y & 7) * 8 + (x & 7)) as usize] as f32 + 0.5) / 64.0 - 0.5
}

#[derive(Debug)]
struct Entry {
    pixmap: Arc<Pixmap>,
    bytes: usize,
    used: u64,
    /// The frame that last used it: never evicted during that frame, so
    /// a frame needing more than the budget still draws every entry
    /// from its pixmap.
    frame: u64,
}

/// Pixmaps of gradients and shadows by key, with an LRU byte budget.
#[derive(Debug, Default)]
pub struct PaintCache {
    entries: HashMap<u64, Entry>,
    bytes: usize,
    tick: u64,
    builds: u64,
    frame: u64,
}

/// A blurred rounded rect, relative to the pixmap's origin.
#[derive(Clone, Debug)]
pub struct ShadowShape {
    pub rect: kurbo::Rect,
    pub radii: [f32; 4],
    pub std_dev: f32,
    pub color: Color,
    pub extent: kurbo::Rect,
}

fn hash_f64(h: &mut impl Hasher, v: f64) {
    v.to_bits().hash(h);
}

/// The key of a gradient filling a `w × h` frame.
pub fn gradient_key(paint: &Paint, w: u32, h: u32) -> u64 {
    let mut h_ = DefaultHasher::new();
    1u8.hash(&mut h_);
    (w, h).hash(&mut h_);
    let stops = |h: &mut DefaultHasher, s: &[GradientStop]| {
        for st in s {
            st.offset.to_bits().hash(h);
            for c in [st.color.r, st.color.g, st.color.b, st.color.a] {
                c.to_bits().hash(h);
            }
        }
    };
    match paint {
        Paint::Solid(_) => 0u8.hash(&mut h_),
        Paint::Linear { angle, stops: s } => {
            1u8.hash(&mut h_);
            angle.to_bits().hash(&mut h_);
            stops(&mut h_, s);
        }
        Paint::Radial { stops: s } => {
            2u8.hash(&mut h_);
            stops(&mut h_, s);
        }
        Paint::Conic { from, stops: s } => {
            3u8.hash(&mut h_);
            from.to_bits().hash(&mut h_);
            stops(&mut h_, s);
        }
    }
    h_.finish()
}

/// The key of a shadow's pixmap.
pub fn shadow_key(s: &ShadowShape) -> u64 {
    let mut h = DefaultHasher::new();
    2u8.hash(&mut h);
    // Relative to the whole-pixel origin the pixmap is drawn from (see
    // `render_shadow`): a shadow moved by whole pixels (a sliding toast,
    // a FLIP glide) reuses its pixmap.
    let (ox, oy) = (s.extent.x0.floor(), s.extent.y0.floor());
    for v in [
        s.rect.x0 - ox,
        s.rect.y0 - oy,
        s.rect.x1 - ox,
        s.rect.y1 - oy,
    ] {
        hash_f64(&mut h, v);
    }
    for v in [
        s.extent.x0 - ox,
        s.extent.y0 - oy,
        s.extent.x1 - ox,
        s.extent.y1 - oy,
    ] {
        hash_f64(&mut h, v);
    }
    for r in s.radii {
        r.to_bits().hash(&mut h);
    }
    s.std_dev.to_bits().hash(&mut h);
    for c in [s.color.r, s.color.g, s.color.b, s.color.a] {
        c.to_bits().hash(&mut h);
    }
    h.finish()
}

/// True if a `w × h` pixmap may be cached.
pub fn cacheable(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= u16::MAX as u32 && h <= u16::MAX as u32 && {
        (w as usize) * (h as usize) * 4 <= MAX_ENTRY_BYTES
    }
}

impl PaintCache {
    /// Bytes of pixmaps held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Pixmaps built so far (tests: a second frame builds none).
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Starts a frame: entries used from now on stay until the next.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    fn get(&mut self, key: u64) -> Option<Arc<Pixmap>> {
        self.tick += 1;
        let (tick, frame) = (self.tick, self.frame);
        self.entries.get_mut(&key).map(|e| {
            e.used = tick;
            e.frame = frame;
            e.pixmap.clone()
        })
    }

    /// A cached pixmap, without touching the LRU order.
    pub fn peek(&self, key: u64) -> Option<&Arc<Pixmap>> {
        self.entries.get(&key).map(|e| &e.pixmap)
    }

    fn insert(&mut self, key: u64, pixmap: Pixmap) -> Arc<Pixmap> {
        let bytes = pixmap.width() as usize * pixmap.height() as usize * 4;
        while self.bytes + bytes > PAINT_CACHE_BYTES {
            let frame = self.frame;
            let Some((&old, _)) = self
                .entries
                .iter()
                .filter(|(_, e)| e.frame != frame)
                .min_by_key(|(_, e)| e.used)
            else {
                break;
            };
            if let Some(e) = self.entries.remove(&old) {
                self.bytes -= e.bytes;
            }
        }
        self.tick += 1;
        self.builds += 1;
        let pixmap = Arc::new(pixmap);
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                pixmap: pixmap.clone(),
                bytes,
                used: self.tick,
                frame: self.frame,
            },
        );
        pixmap
    }

    /// The dithered pixmap of `paint` over a `w × h` frame (`None` for a
    /// solid paint or one too large to cache).
    pub fn gradient(&mut self, paint: &Paint, w: u32, h: u32) -> Option<Arc<Pixmap>> {
        if matches!(paint, Paint::Solid(_)) || !cacheable(w, h) {
            return None;
        }
        let key = gradient_key(paint, w, h);
        if let Some(p) = self.get(key) {
            return Some(p);
        }
        let pm = render_gradient(paint, w, h)?;
        Some(self.insert(key, pm))
    }

    /// The pixmap of a blurred rounded rect, its origin at the shape's
    /// `extent` corner rounded down (`None` when too large to cache).
    pub fn shadow(&mut self, s: &ShadowShape) -> Option<Arc<Pixmap>> {
        let (w, h) = shadow_size(s);
        if !cacheable(w, h) {
            return None;
        }
        let key = shadow_key(s);
        if let Some(p) = self.get(key) {
            return Some(p);
        }
        let pm = render_shadow(s, w as u16, h as u16);
        Some(self.insert(key, pm))
    }
}

/// Colours of a gradient at `LUT` evenly spaced offsets, premultiplied,
/// in 0..255, with red and blue swapped (BGRA, as the raster draws),
/// interpolated in OKLab between stops.
fn lut(stops: &[GradientStop]) -> Vec<[f32; 4]> {
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
    (0..LUT)
        .map(|i| {
            let t = i as f32 / (LUT - 1) as f32;
            let c = color_at(&clean, t);
            let a = c.a;
            [c.b * a * 255.0, c.g * a * 255.0, c.r * a * 255.0, a * 255.0]
        })
        .collect()
}

fn color_at(stops: &[GradientStop], t: f32) -> Color {
    let Some(first) = stops.first() else {
        return Color::TRANSPARENT;
    };
    if t <= first.offset {
        return first.color;
    }
    for w in stops.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if t <= b.offset {
            if b.offset <= a.offset {
                return b.color;
            }
            return a
                .color
                .lerp_oklab(b.color, (t - a.offset) / (b.offset - a.offset));
        }
    }
    stops.last().map_or(Color::TRANSPARENT, |s| s.color)
}

/// Where a frame pixel centre falls along a gradient, 0 to 1.
type Geometry = Box<dyn Fn(f64, f64) -> f64>;

/// Renders `paint` over a `w × h` frame with ordered dithering: each
/// channel is offset by an 8 × 8 Bayer threshold before rounding, so a
/// slow ramp alternates between neighbouring levels instead of banding.
/// Geometry follows CSS, as `raster::paint_type`: a linear gradient's
/// line runs through the centre at `angle` (0deg up, clockwise) and is
/// long enough for the corners to reach the end colours; a radial one
/// runs from the centre to the farthest corner; a conic one turns
/// clockwise from `from` (0deg up).
pub fn render_gradient(paint: &Paint, w: u32, h: u32) -> Option<Pixmap> {
    render_gradient_part(paint, w, h, (0, 0, w, h))
}

/// The part `(x0, y0, pw, ph)` of [`render_gradient`]'s `w × h` frame:
/// the same pixels, since each is a pure function of its position in the
/// frame. A gradient too large to cache draws, dithered, only the part a
/// cell shows.
pub fn render_gradient_part(
    paint: &Paint,
    w: u32,
    h: u32,
    (x0, y0, pw, ph): (u32, u32, u32, u32),
) -> Option<Pixmap> {
    if pw == 0 || ph == 0 || pw > u16::MAX as u32 || ph > u16::MAX as u32 {
        return None;
    }
    let (stops, geo): (&[GradientStop], Geometry) = match paint {
        Paint::Solid(_) => return None,
        Paint::Linear { angle, stops } => {
            let a = finite_angle(*angle).to_radians();
            let (sin, cos) = a.sin_cos();
            let len = (w as f64 * sin).abs() + (h as f64 * cos).abs();
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let (dx, dy) = (sin, -cos);
            (
                stops,
                Box::new(move |x, y| {
                    if len <= 0.0 {
                        return 0.0;
                    }
                    ((x - cx) * dx + (y - cy) * dy) / len + 0.5
                }),
            )
        }
        Paint::Radial { stops } => {
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let r = cx.hypot(cy);
            (
                stops,
                Box::new(move |x, y| {
                    if r <= 0.0 {
                        0.0
                    } else {
                        (x - cx).hypot(y - cy) / r
                    }
                }),
            )
        }
        Paint::Conic { from, stops } => {
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let from = finite_angle(*from).to_radians();
            (
                stops,
                Box::new(move |x, y| {
                    // Clockwise from the top: atan2 of (x, -y).
                    let a = (x - cx).atan2(-(y - cy)) - from;
                    a.rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU
                }),
            )
        }
    };
    let table = lut(stops);
    let opaque = stops.iter().all(|s| s.color.clamped().a >= 1.0);
    let mut pm = Pixmap::new(pw as u16, ph as u16);
    let data = pm.data_mut();
    for py in 0..ph {
        for px in 0..pw {
            let (x, y) = (x0 + px, y0 + py);
            let t = geo(x as f64 + 0.5, y as f64 + 0.5).clamp(0.0, 1.0);
            let f = t as f32 * (LUT - 1) as f32;
            let i = (f as usize).min(LUT - 2);
            let k = f - i as f32;
            let (a, b) = (table[i], table[i + 1]);
            let d = dither(x, y);
            let ch = |n: usize| a[n] + (b[n] - a[n]) * k;
            let q = |v: f32| (v + d).round().clamp(0.0, 255.0) as u8;
            let alpha = q(ch(3));
            let c = |n: usize| q(ch(n)).min(alpha);
            data[(py * pw + px) as usize] = PremulRgba8 {
                r: c(0),
                g: c(1),
                b: c(2),
                a: alpha,
            };
        }
    }
    pm.set_may_have_transparency(!opaque);
    Some(pm)
}

fn finite_angle(deg: f32) -> f64 {
    if deg.is_finite() {
        (deg % 360.0) as f64
    } else {
        0.0
    }
}

/// A colour for vello with red and blue swapped (see `raster`).
fn bgra(c: Color) -> vello_cpu::color::AlphaColor<vello_cpu::color::Srgb> {
    let c = c.clamped();
    vello_cpu::color::AlphaColor::new([c.b, c.g, c.r, c.a])
}

/// Draws a blurred rounded rect into `ctx` (in its current transform).
/// With differing radii each quadrant is drawn with its own corner's
/// radius (vello blurs one radius per rect), split on whole pixels so
/// the seams do not antialias.
pub fn draw_shadow(ctx: &mut RenderContext, s: &ShadowShape) {
    ctx.set_paint(bgra(s.color));
    let (rect, radii, extent) = (&s.rect, &s.radii, &s.extent);
    if radii.iter().all(|r| *r == radii[0]) {
        ctx.fill_blurred_rounded_rect(rect, radii[0], s.std_dev, false);
        return;
    }
    let (mx, my) = (rect.center().x.round(), rect.center().y.round());
    let quads = [
        kurbo::Rect::new(extent.x0, extent.y0, mx, my),
        kurbo::Rect::new(mx, extent.y0, extent.x1, my),
        kurbo::Rect::new(mx, my, extent.x1, extent.y1),
        kurbo::Rect::new(extent.x0, my, mx, extent.y1),
    ];
    for (q, r) in quads.iter().zip(radii) {
        if q.width() <= 0.0 || q.height() <= 0.0 {
            continue;
        }
        ctx.push_clip_rect(q);
        ctx.fill_blurred_rounded_rect(rect, *r, s.std_dev, false);
        ctx.pop_clip();
    }
}

/// The pixel size of a shadow's pixmap: its extent, out to whole pixels.
pub fn shadow_size(s: &ShadowShape) -> (u32, u32) {
    let e = &s.extent;
    let span = |a: f64, b: f64| {
        let v = b.ceil() - a.floor();
        if v.is_finite() && v > 0.0 {
            v.min(u32::MAX as f64) as u32
        } else {
            0
        }
    };
    (span(e.x0, e.x1), span(e.y0, e.y1))
}

/// Renders a shadow into a pixmap whose origin is its extent's corner
/// rounded down.
fn render_shadow(s: &ShadowShape, w: u16, h: u16) -> Pixmap {
    let (ox, oy) = (s.extent.x0.floor(), s.extent.y0.floor());
    let settings = RenderSettings {
        num_threads: 0,
        ..RenderSettings::default()
    };
    let mut ctx = RenderContext::new_with(w, h, settings);
    ctx.set_transform(Affine::translate((-ox, -oy)));
    ctx.set_fill_rule(Fill::NonZero);
    draw_shadow(&mut ctx, s);
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
    let mut pm = Pixmap::new(w, h);
    for (d, s) in pm.data_mut().iter_mut().zip(bytes.chunks_exact(4)) {
        *d = PremulRgba8 {
            r: s[0],
            g: s[1],
            b: s[2],
            a: s[3],
        };
    }
    pm
}

/// The image paint drawing `pm` with its origin at `(x, y)`, and the
/// paint transform placing it there.
pub fn image_paint(pm: &Arc<Pixmap>, x: f64, y: f64, smooth: bool) -> (PaintType, Affine) {
    (
        PaintType::Image(Image {
            image: ImageSource::Pixmap(pm.clone()),
            sampler: ImageSampler {
                x_extend: Extend::Pad,
                y_extend: Extend::Pad,
                quality: if smooth {
                    ImageQuality::Medium
                } else {
                    ImageQuality::Low
                },
                alpha: 1.0,
            },
        }),
        Affine::translate((x, y)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(a: &str, b: &str) -> Paint {
        Paint::Linear {
            angle: 90.0,
            stops: vec![
                GradientStop {
                    offset: 0.0,
                    color: Color::from_hex(a).unwrap(),
                },
                GradientStop {
                    offset: 1.0,
                    color: Color::from_hex(b).unwrap(),
                },
            ],
        }
    }

    /// A slow ramp over 256 px (8 levels) has no 32 px bands: the
    /// longest run of one value along a row is short, and the row's
    /// mean still rises steadily.
    #[test]
    fn slow_ramps_are_dithered() {
        let pm = render_gradient(&ramp("#202020", "#282828"), 256, 8).unwrap();
        let row: Vec<u8> = (0..256).map(|x| pm.sample(x, 3).g).collect();
        let mut longest = 0;
        let mut run = 1;
        for w in row.windows(2) {
            if w[0] == w[1] {
                run += 1;
            } else {
                longest = longest.max(run);
                run = 1;
            }
        }
        longest = longest.max(run);
        assert!(longest <= 16, "longest run {longest}");
        let mean = |x0: u16| -> f32 {
            (0..8)
                .flat_map(|y| (x0..x0 + 32).map(move |x| (x, y)))
                .map(|(x, y)| pm.sample(x, y).g as f32)
                .sum::<f32>()
                / 256.0
        };
        assert!(mean(0) < mean(112) && mean(112) < mean(224));
        assert!((mean(0) - 0x20 as f32).abs() < 1.0);
    }

    #[test]
    fn the_cache_reuses_and_evicts() {
        let mut c = PaintCache::default();
        let p = ramp("#000000", "#ffffff");
        let a = c.gradient(&p, 100, 100).unwrap();
        let b = c.gradient(&p, 100, 100).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(c.builds(), 1);
        // Too large to cache: drawn by vello instead.
        assert!(c.gradient(&p, 2000, 2000).is_none());
        for i in 0..200 {
            c.begin_frame();
            c.gradient(&p, 200 + i, 200);
        }
        assert!(c.bytes() <= PAINT_CACHE_BYTES);
    }

    #[test]
    fn conic_starts_at_from() {
        let p = Paint::Conic {
            from: 90.0,
            stops: vec![
                GradientStop {
                    offset: 0.0,
                    color: Color::from_hex("#ff0000").unwrap(),
                },
                GradientStop {
                    offset: 1.0,
                    color: Color::from_hex("#0000ff").unwrap(),
                },
            ],
        };
        let pm = render_gradient(&p, 64, 64).unwrap();
        // Just clockwise of 90deg (right of centre, a little below) is
        // the start colour: red, which is in the `b` slot (BGRA).
        let px = pm.sample(60, 34);
        assert!(px.b > 200 && px.r < 60, "{px:?}");
    }
}

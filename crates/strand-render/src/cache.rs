//! Cached paint: dithered gradients and blurred shadows rendered once
//! into pixmaps and drawn from there (design.md, "Paint and light":
//! banding removed by automatic dithering; "Runtime changes": cached
//! offscreen groups, about 32 MB).
//!
//! Every entry is a pure function of its key, and whether a key is
//! cached at all depends only on the key (its size), so a partial
//! repaint draws exactly what a full one does. A shadow on whole pixels
//! whose box is large enough for its blur is drawn from slice tiles that
//! do not depend on the box's size (`slice_plan`), so a box whose size
//! springs reuses them on every frame (decisions.md, shadow-slice).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;
use strand_scene::time::Instant;

use strand_scene::{Color, GradientStop, Paint};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{Extend, Fill, ImageQuality, ImageSampler};
use vello_cpu::{
    Image, ImageSource, PaintType, Pixmap, PixmapMut, RasterizerSettings, RenderContext,
    RenderMode, RenderSettings, Resources, TargetInit,
};

/// Bytes of cached pixmaps kept, least recently used dropped first.
pub const PAINT_CACHE_BYTES: usize = 32 << 20;

/// Largest single cached pixmap (16 MB: a 2× launcher's gradient and
/// shadow, a full-height panel at 2×, a 1440p overlay); larger gradients
/// are dithered cell by cell, uncached, and larger shadows draw directly
/// (decided by size alone). At 2 MB a 2× launcher's gradient and shadow
/// were drawn again on every full repaint: 39 ms a frame against 6.5 ms
/// (decisions.md, m5-speed-caches).
pub const MAX_ENTRY_BYTES: usize = 16 << 20;

/// Entries up to this size are cached as soon as a frame draws them (a
/// shadow) or a second frame does (a gradient). A larger one is cached
/// only when a second frame repaints all of it: building it costs more
/// than drawing the part a frame shows, so a launcher's first frame and
/// the partial frames after it (typing, hover) draw it directly, and only
/// repeated full repaints (a fade or scale where the compositor cannot
/// do it, a resize) pay the build once (decisions.md, m5-speed-caches).
pub const EAGER_ENTRY_BYTES: usize = 2 << 20;

/// Entries no frame has used for this long are freed (design.md: the
/// cached offscreen groups are "freed when idle") at the next paint or
/// wake of the render loop, never at a wake of their own. A surface
/// shown again within it (a launcher reopened) draws from what its last
/// showing cached: 6–7 ms against 37–40 ms for a 2× launcher's first
/// frame (decisions.md, m5-speed-caches).
pub const IDLE_FREE: Duration = Duration::from_secs(30);

/// Most gradient keys remembered as drawn once, uncached (see
/// [`PaintCache::gradient`]); past it the record starts over.
const SEEN_MAX: usize = 4096;

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

/// Multiple of the standard deviation past which vello's blurred
/// rounded rect is flat: its erf of 4 is within 3e-6 of 1, under a
/// thousandth of a level in a u8 channel, so a box this many σ from an
/// edge draws the same field whatever lies beyond (decisions.md,
/// shadow-slice).
const FLAT: f64 = 4.0;

/// Bytes of corner tiles held uncached (see [`PaintCache::shadow_slices`])
/// before the oldest are dropped; the current frame's are never dropped.
/// A tile waits here until a later frame asks for it again, however many
/// other surfaces paint in between (each paint is a frame of the shared
/// cache): 4 MB holds a few dozen of the launcher's 1.5× corner tiles
/// (about 100–250 kB each), many frames of a radius springing on every
/// surface at once.
const RECENT_BYTES: usize = 4 << 20;

/// Shadows blurred less than this are not sliced: f32 rounding in where
/// a pixel falls (a part of the box's size) moves a near-step edge.
const MIN_SLICED_STD_DEV: f32 = 0.25;

#[derive(Debug)]
struct Entry {
    pixmap: Arc<Pixmap>,
    bytes: usize,
    used: u64,
    /// The frame that last used it: never evicted during that frame, so
    /// a frame needing more than the budget still draws every entry
    /// from its pixmap.
    frame: u64,
    /// When a frame last used it.
    at: Instant,
}

/// Pixmaps of gradients and shadows by key, with an LRU byte budget.
#[derive(Debug, Default)]
pub struct PaintCache {
    entries: HashMap<u64, Entry>,
    bytes: usize,
    tick: u64,
    builds: u64,
    frame: u64,
    /// Gradients drawn once, uncached: key to the frame and time that
    /// drew them. A gradient is cached only when a later frame asks for
    /// it again.
    seen: HashMap<u64, (u64, Instant)>,
    /// How long an unused entry lives (`IDLE_FREE` unless a test
    /// shortens it).
    idle: Option<Duration>,
    /// Sliced shadows' corner tiles built uncached: cached when a later
    /// frame asks again (see [`PaintCache::shadow_slices`]), the oldest
    /// dropped past [`RECENT_BYTES`] or when idle.
    recent: HashMap<u64, Recent>,
    /// Bytes of `recent` (counted in [`PaintCache::bytes`]).
    recent_bytes: usize,
    /// Pixels of blurred shadow rendered into pixmaps (whole shadows and
    /// tiles), plus the extents frames drew directly (tests).
    blurred: u64,
    /// (redraw) This frame's parts of shadows not cached, by display
    /// item ([`PaintCache::shadow_part`]); dropped when the frame ends.
    parts: HashMap<usize, ShadowPart>,
    /// (redraw) What large shadows' cells are rastered with (behind a
    /// lock only so that the cache stays `Sync` for the raster's threads,
    /// which read it; only `&mut self` takes it).
    lanes: std::sync::Mutex<CellLanes>,
}

/// A corner tile built uncached, held for a later frame to cache.
#[derive(Debug)]
struct Recent {
    pixmap: Arc<Pixmap>,
    bytes: usize,
    /// The frame that built it.
    frame: u64,
    /// When it was built, in `tick` order (oldest dropped first).
    tick: u64,
    at: Instant,
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

/// One tile of a sliced shadow: the blurred field of a box `local`, in
/// tile pixels, with corner radius `radius`, over a `w × h` pixmap whose
/// origin is `origin` in item coordinates (whole pixels).
#[derive(Clone, Debug)]
struct TileSpec {
    key: u64,
    origin: (i64, i64),
    w: u16,
    h: u16,
    local: kurbo::Rect,
    radius: f32,
    /// A quadrant's base tile (cached at once), not its corner tile.
    base: bool,
}

/// Where one quadrant of a sliced shadow draws from: `area` (whole
/// pixels, item coordinates) is drawn from `base`, the field of a
/// square corner padded past its edges, except `corner_area`, drawn from
/// `corner`, the field of the quadrant's own corner radius.
#[derive(Clone, Debug)]
struct QuadPlan {
    area: [i64; 4],
    corner_area: [i64; 4],
    base: TileSpec,
    corner: TileSpec,
}

/// A shadow cut into tiles that do not depend on its box's size.
#[derive(Clone, Debug)]
struct SlicePlan {
    quads: [QuadPlan; 4],
}

/// One axis of one corner: the tile's span `[a, b)` in whole pixels for
/// an edge at `c` reaching `out` pixels outward and `inner` inward (`low`
/// for a left or top edge), the edge's place in tile pixels, and the
/// virtual box's extent along the axis there (`big` inward).
fn corner_span(c: f64, low: bool, out: i64, inner: i64, big: f64) -> ((i64, i64), (f64, f64)) {
    let f = c.floor();
    let (fi, frac) = (f as i64, c - f);
    if low {
        let e = frac + out as f64;
        ((fi - out, fi + inner + 1), (e, e + big))
    } else {
        let e = frac + (inner + 1) as f64;
        ((fi - inner - 1, fi + out + 1), (e - big, e))
    }
}

fn tile_key(
    s: &ShadowShape,
    corner: usize,
    radius: Option<f32>,
    frac: (f64, f64),
    reach: (i64, i64),
) -> u64 {
    let mut h = DefaultHasher::new();
    3u8.hash(&mut h);
    corner.hash(&mut h);
    radius.map(f32::to_bits).hash(&mut h);
    hash_f64(&mut h, frac.0);
    hash_f64(&mut h, frac.1);
    reach.hash(&mut h);
    s.std_dev.to_bits().hash(&mut h);
    for c in [s.color.r, s.color.g, s.color.b, s.color.a] {
        c.to_bits().hash(&mut h);
    }
    h.finish()
}

/// How a shadow is cut into tiles, or `None` when its field depends on
/// its box's size (a box under about `2 × FLAT` σ across, or one whose
/// corners are so round that vello clamps them to the box) or it is too
/// sharp to slice exactly.
///
/// vello's blurred rounded rect (`fill_blurred_rounded_rect`) is a
/// distance field: at a point `u` outside a vertical edge and `v`
/// outside a horizontal one (negative inside), past a corner's reach
/// (`r1 = hypot(r, 2σ)` from each edge) its distance is `v`, `u` or, deep
/// inside, `max(u, v)`, whatever the radius, and its other terms are
/// flat for a box at least `2 × FLAT` σ across. So each quadrant is the
/// field of a square corner (`base`, out to `FLAT` σ inside, padded
/// beyond: the edges' profiles and the flat middle) with its own corner
/// tile (`corner`, out to `r1`) over it: tiles keyed by radius, blur,
/// colour and the edges' sub-pixel phase, never by the size, so a box
/// whose size springs draws from the same tiles on every frame, and one
/// whose radius springs builds only its corner tiles. Each quadrant is
/// the field of its own corner's radius, as [`draw_shadow`] draws it.
fn slice_plan(s: &ShadowShape) -> Option<SlicePlan> {
    let sd = s.std_dev;
    if !(sd.is_finite() && sd >= MIN_SLICED_STD_DEV) {
        return None;
    }
    let r = s.rect;
    let e = s.extent;
    if ![r.x0, r.y0, r.x1, r.y1, e.x0, e.y0, e.x1, e.y1]
        .iter()
        .all(|v| v.is_finite() && v.abs() < 1e7)
    {
        return None;
    }
    let (w, h) = (r.width(), r.height());
    if !(w > 0.0 && h > 0.0) {
        return None;
    }
    let sdf = sd as f64;
    let (short, long) = (w.min(h), w.max(h));
    if short < 2.0 * FLAT * sdf {
        return None;
    }
    // vello's terms (`EncodeExt for BlurredRoundedRectangle`): a radius
    // or `r1` clamped to half the short side, or a scale term short of
    // flat, follows the box's size.
    let mut inner_k = [0i64; 4];
    for (k, &rad) in inner_k.iter_mut().zip(&s.radii) {
        if !(rad.is_finite() && rad >= 0.0) {
            return None;
        }
        let r1 = rad.hypot(sd * 2.0) as f64;
        if r1 > 0.5 * short || 0.5 * (long - 0.5 * rad as f64) < FLAT * sdf {
            return None;
        }
        *k = r1.ceil() as i64 + 2;
    }
    let out = (2.5 * sdf).ceil() as i64 + 2;
    let flat = (FLAT * sdf).ceil() as i64 + 2;
    let px = |a: f64, b: f64| (a.floor() as i64, b.ceil() as i64);
    let (ex, ey) = (px(e.x0, e.x1), px(e.y0, e.y1));
    // The quadrants meet between the corners' reach, where every
    // quadrant's field is the same (the edges' profiles).
    let reach = |i: usize| flat.max(inner_k[i]) + 1;
    let split = |lo: f64, hi: f64, a: [usize; 2], b: [usize; 2], c: f64| {
        let lo = lo.floor() as i64 + reach(a[0]).max(reach(a[1]));
        let hi = hi.floor() as i64 - reach(b[0]).max(reach(b[1]));
        (lo <= hi).then(|| (c.round() as i64).clamp(lo, hi))
    };
    let mx = split(r.x0, r.x1, [0, 3], [1, 2], r.center().x)?;
    let my = split(r.y0, r.y1, [0, 1], [3, 2], r.center().y)?;
    let quad = |i: usize| -> Option<QuadPlan> {
        let (left, top) = (matches!(i, 0 | 3), matches!(i, 0 | 1));
        let (cx, cy) = (
            if left { r.x0 } else { r.x1 },
            if top { r.y0 } else { r.y1 },
        );
        let tile = |radius: Option<f32>, inner: i64| -> Option<TileSpec> {
            let rad = radius.unwrap_or(0.0);
            let big = (2 * (inner.max(flat) + out + 8)) as f64 + rad.ceil() as f64;
            let ((xa, xb), (lx0, lx1)) = corner_span(cx, left, out, inner, big);
            let ((ya, yb), (ly0, ly1)) = corner_span(cy, top, out, inner, big);
            let (tw, th) = (u16::try_from(xb - xa).ok()?, u16::try_from(yb - ya).ok()?);
            // A tile over `EAGER_ENTRY_BYTES` (a blur of over about 110
            // px, or a huge radius) is not sliced: the whole shadow keeps
            // the large-entry rule, built only by a second whole frame.
            if !eager(tw as u32, th as u32) {
                return None;
            }
            Some(TileSpec {
                key: tile_key(
                    s,
                    i,
                    radius,
                    (cx - cx.floor(), cy - cy.floor()),
                    (out, inner),
                ),
                origin: (xa, ya),
                w: tw,
                h: th,
                local: kurbo::Rect::new(lx0, ly0, lx1, ly1),
                radius: rad,
                base: radius.is_none(),
            })
        };
        let base = tile(None, flat)?;
        let corner = tile(Some(s.radii[i]), inner_k[i])?;
        // The quadrant: from the extent's edge (or the tiles', past which
        // the field is zero) to the split.
        let (qx0, qx1) = if left {
            (ex.0.max(corner.origin.0), mx)
        } else {
            (mx, ex.1.min(corner.origin.0 + corner.w as i64))
        };
        let (qy0, qy1) = if top {
            (ey.0.max(corner.origin.1), my)
        } else {
            (my, ey.1.min(corner.origin.1 + corner.h as i64))
        };
        let (kx0, kx1) = if left {
            (qx0, corner.origin.0 + corner.w as i64)
        } else {
            (corner.origin.0, qx1)
        };
        let (ky0, ky1) = if top {
            (qy0, corner.origin.1 + corner.h as i64)
        } else {
            (corner.origin.1, qy1)
        };
        Some(QuadPlan {
            area: [qx0, qy0, qx1, qy1],
            corner_area: [kx0.max(qx0), ky0.max(qy0), kx1.min(qx1), ky1.min(qy1)],
            base,
            corner,
        })
    };
    Some(SlicePlan {
        quads: [quad(0)?, quad(1)?, quad(2)?, quad(3)?],
    })
}

/// A sliced shadow ready to draw: each part an image placed with its
/// origin at `origin` (item coordinates), filling `dest`.
#[derive(Clone, Debug, Default)]
pub struct Slices {
    pub parts: Vec<SlicePart>,
}

#[derive(Clone, Debug)]
pub struct SlicePart {
    pub dest: kurbo::Rect,
    pub pixmap: Arc<Pixmap>,
    pub origin: (f64, f64),
}

impl Slices {
    fn from_plan(
        plan: &SlicePlan,
        mut tile: impl FnMut(&TileSpec) -> Option<Arc<Pixmap>>,
    ) -> Option<Self> {
        let mut parts = Vec::with_capacity(12);
        let rect = |[x0, y0, x1, y1]: [i64; 4]| {
            (x1 > x0 && y1 > y0)
                .then(|| kurbo::Rect::new(x0 as f64, y0 as f64, x1 as f64, y1 as f64))
        };
        for q in &plan.quads {
            let base = tile(&q.base)?;
            let corner = tile(&q.corner)?;
            let [qx0, qy0, qx1, qy1] = q.area;
            let [kx0, ky0, kx1, ky1] = q.corner_area;
            let at = |t: &TileSpec| (t.origin.0 as f64, t.origin.1 as f64);
            if let Some(dest) = rect(q.corner_area) {
                parts.push(SlicePart {
                    dest,
                    pixmap: corner,
                    origin: at(&q.corner),
                });
            }
            // The rest of the quadrant: the rows beside the corner tile,
            // then the full-height columns beside it.
            let (cols, rows) = (
                if kx0 > qx0 { [qx0, kx0] } else { [kx1, qx1] },
                if ky0 > qy0 { [qy0, ky0] } else { [ky1, qy1] },
            );
            for dest in [
                rect([kx0, rows[0], kx1, rows[1]]),
                rect([cols[0], qy0, cols[1], qy1]),
            ]
            .into_iter()
            .flatten()
            {
                parts.push(SlicePart {
                    dest,
                    pixmap: base.clone(),
                    origin: at(&q.base),
                });
            }
        }
        Some(Self { parts })
    }
}

/// Draws a sliced shadow into `ctx` (in its current transform, which
/// must keep pixels whole), only the parts that touch `region`.
pub fn draw_slices(ctx: &mut RenderContext, slices: &Slices, region: kurbo::Rect) {
    for p in &slices.parts {
        let dest = p.dest.intersect(region);
        if dest.width() <= 0.0 || dest.height() <= 0.0 {
            continue;
        }
        let (paint, t) = image_paint(&p.pixmap, p.origin.0, p.origin.1, false);
        ctx.set_paint(paint);
        ctx.set_paint_transform(t);
        ctx.fill_rect(&dest);
        ctx.reset_paint_transform();
    }
}

/// True if a `w × h` pixmap may be cached.
pub fn cacheable(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && w <= u16::MAX as u32 && h <= u16::MAX as u32 && {
        (w as usize) * (h as usize) * 4 <= MAX_ENTRY_BYTES
    }
}

/// True if a `w × h` pixmap is small enough to cache without a second
/// whole repaint asking for it ([`EAGER_ENTRY_BYTES`]).
fn eager(w: u32, h: u32) -> bool {
    (w as usize) * (h as usize) * 4 <= EAGER_ENTRY_BYTES
}

impl PaintCache {
    /// Bytes of pixmaps held: cached entries and corner tiles held
    /// uncached.
    pub fn bytes(&self) -> usize {
        self.bytes + self.recent_bytes
    }

    /// Pixmaps built so far (tests: a second frame builds none).
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// The lanes large shadows' cells are rastered on.
    fn lanes(&mut self) -> &mut CellLanes {
        self.lanes
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts a frame: entries used from now on stay until the next.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.parts.clear();
    }

    /// Ends a frame: drops its parts of shadows ([`Self::shadow_part`]).
    pub fn end_frame(&mut self) {
        self.parts.clear();
    }

    fn get(&mut self, key: u64) -> Option<Arc<Pixmap>> {
        self.tick += 1;
        let (tick, frame) = (self.tick, self.frame);
        self.entries.get_mut(&key).map(|e| {
            e.used = tick;
            e.frame = frame;
            e.at = Instant::now();
            e.pixmap.clone()
        })
    }

    /// Frees the entries no frame has used since `now - IDLE_FREE` (a
    /// frame touches what it draws first, so a paint's own entries are
    /// never idle). Called at every paint and at every wake of the render
    /// loop (`Renderer::update`); nothing wakes just for it.
    pub fn trim_idle(&mut self, now: Instant) {
        let idle = self.idle_free();
        let before = self.entries.len();
        self.entries
            .retain(|_, e| now.saturating_duration_since(e.at) < idle);
        if self.entries.len() != before {
            self.bytes = self.entries.values().map(|e| e.bytes).sum();
        }
        self.seen
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < idle);
        self.recent
            .retain(|_, t| now.saturating_duration_since(t.at) < idle);
        self.recent_bytes = self.recent.values().map(|t| t.bytes).sum();
        let lanes = self.lanes();
        if lanes
            .used
            .is_some_and(|t| now.saturating_duration_since(t) >= idle)
        {
            *lanes = CellLanes::default();
        }
        trim_luts(now, idle);
    }

    /// How long an unused entry lives.
    pub fn idle_free(&self) -> Duration {
        self.idle.unwrap_or(IDLE_FREE)
    }

    /// Shortens (tests) how long an unused entry lives.
    pub fn set_idle_free(&mut self, idle: Duration) {
        self.idle = Some(idle);
    }

    /// A cached pixmap, without touching the LRU order (corner tiles
    /// built uncached by this frame or the last count too: see
    /// [`Self::shadow_slices`]).
    pub fn peek(&self, key: u64) -> Option<&Arc<Pixmap>> {
        self.entries
            .get(&key)
            .map(|e| &e.pixmap)
            .or_else(|| self.recent.get(&key).map(|t| &t.pixmap))
    }

    fn insert(&mut self, key: u64, pixmap: Pixmap) -> Arc<Pixmap> {
        self.insert_arc(key, Arc::new(pixmap))
    }

    fn insert_arc(&mut self, key: u64, pixmap: Arc<Pixmap>) -> Arc<Pixmap> {
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
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                pixmap: pixmap.clone(),
                bytes,
                used: self.tick,
                frame: self.frame,
                at: Instant::now(),
            },
        );
        pixmap
    }

    /// The dithered pixmap of `paint` over a `w × h` frame (`None` for a
    /// solid paint, one too large to cache, or one no earlier frame drew:
    /// the raster then dithers it cell by cell). As [`Self::gradient_in`]
    /// for a frame that repaints all of it (tests).
    #[cfg(test)]
    pub fn gradient(&mut self, paint: &Paint, w: u32, h: u32) -> Option<Arc<Pixmap>> {
        self.gradient_in(paint, w, h, true)
    }

    /// [`Self::gradient`] in a frame that repaints all of it (`whole`) or
    /// only part: one over [`EAGER_ENTRY_BYTES`] is admitted only by a
    /// second frame repainting all of it, and a partial frame neither
    /// builds nor records it.
    pub fn gradient_in(
        &mut self,
        paint: &Paint,
        w: u32,
        h: u32,
        whole: bool,
    ) -> Option<Arc<Pixmap>> {
        if matches!(paint, Paint::Solid(_)) || !cacheable(w, h) {
            return None;
        }
        let key = gradient_key(paint, w, h);
        if let Some(p) = self.get(key) {
            return Some(p);
        }
        if !whole && !eager(w, h) {
            return None;
        }
        // Admitted on a second frame's use: a gradient whose paint or
        // size changes every frame (a conic ring turning with `t`, a box
        // whose size springs, however many share a size) never repeats a
        // key, so it is drawn cell by cell (`raster::paint_for`, the same
        // pixels) and never evicts the shadows beside it (design.md:
        // "only the ring repaints").
        if !self.seen_before(key) {
            return None;
        }
        let pm = render_gradient(paint, w, h)?;
        self.builds += 1;
        Some(self.insert(key, pm))
    }

    /// Whether an earlier frame asked for `key` (consuming that record);
    /// otherwise records this frame's ask.
    fn seen_before(&mut self, key: u64) -> bool {
        let frame = self.frame;
        match self.seen.get(&key) {
            Some(&(f, _)) if f != frame => {
                self.seen.remove(&key);
                true
            }
            Some(_) => false,
            None => {
                if self.seen.len() >= SEEN_MAX {
                    self.seen.clear();
                }
                self.seen.insert(key, (frame, Instant::now()));
                false
            }
        }
    }

    /// The pixmap of a blurred rounded rect, its origin at the shape's
    /// `extent` corner rounded down (`None` when too large to cache),
    /// built at first sight whatever its size (tests; GPU frames draw
    /// shadows themselves, `Op::BlurredRect`, perf-gpu-retain).
    #[cfg(test)]
    pub fn shadow(&mut self, s: &ShadowShape) -> Option<Arc<Pixmap>> {
        self.shadow_with(s, |_, _| true)
    }

    /// [`Self::shadow`] for the CPU raster, in a frame that repaints all
    /// of it (`whole`) or only part: one up to [`EAGER_ENTRY_BYTES`] is
    /// cached at once, a larger one only by a second frame repainting all
    /// of it (the raster draws it directly until then).
    pub fn shadow_in(&mut self, s: &ShadowShape, whole: bool) -> Option<Arc<Pixmap>> {
        self.shadow_with(s, |c, key| whole && c.seen_before(key))
    }

    fn shadow_with(
        &mut self,
        s: &ShadowShape,
        admit_large: impl FnOnce(&mut Self, u64) -> bool,
    ) -> Option<Arc<Pixmap>> {
        let (w, h) = shadow_size(s);
        if !cacheable(w, h) {
            return None;
        }
        let key = shadow_key(s);
        if let Some(p) = self.get(key) {
            return Some(p);
        }
        if !eager(w, h) && !admit_large(self, key) {
            return None;
        }
        // A large one is rastered on its fixed grid of cells, as a part
        // of it is when it is not cached (`shadow_part`): the same pixels.
        let pm = if eager(w, h) {
            render_shadow(s, w as u16, h as u16)
        } else {
            let all = vec![true; part_grid(w, h).len()];
            let whole = kurbo::Rect::new(0.0, 0.0, f64::from(w), f64::from(h));
            render_shadow_cells(self.lanes(), s, (w, h), &all, whole)
        };
        self.built(&pm);
        Some(self.insert(key, pm))
    }

    /// (redraw) The part of `s`'s pixmap that a frame needs, for a shadow
    /// [`Self::shadow_in`] neither had cached nor built: the cells of the
    /// pixmap's fixed grid ([`PART_CELL_W`] × [`PART_CELL_H`], from its
    /// origin) that `damage` (surface px) reaches under `to_surface` (the
    /// transform the item is drawn under), less those wholly inside the
    /// box `clip` cuts out of the shadow, rastered uncached into a pixmap
    /// of the whole's size, for the raster's draw of `item` (the display
    /// item's address) this frame ([`Self::peek_part`]). A large entry is
    /// rastered whole on the same grid, so the part's pixels are the
    /// whole pixmap's: a frame draws a shadow from its pixmap's pixels
    /// whether or not an earlier frame cached it, and a partial redraw
    /// equals a full one (decisions.md, shadow-part).
    pub fn shadow_part(
        &mut self,
        item: usize,
        s: &ShadowShape,
        clip: &kurbo::BezPath,
        to_surface: Affine,
        damage: &[kurbo::Rect],
    ) {
        let (w, h) = shadow_size(s);
        if !cacheable(w, h) || to_surface.determinant().abs() < 1e-12 {
            return;
        }
        let cells = part_grid(w, h);
        let (ox, oy) = (s.extent.x0.floor(), s.extent.y0.floor());
        let back = to_surface.inverse();
        // Pixmap pixels one on from what each damaged pixel samples.
        let mut need = vec![false; cells.len()];
        for d in damage {
            let q = back.transform_rect_bbox(*d) + kurbo::Vec2::new(-ox, -oy);
            let q = q.inflate(2.0, 2.0);
            for (i, c) in cells.iter().enumerate() {
                need[i] |= overlaps(c, &q);
            }
        }
        // A device pixel the clip lets through samples the pixmap at most
        // about one device pixel inside the box, plus bilinear's pixel.
        let reach = 2.0 + 1.5 * device_px(back);
        let segs: Vec<kurbo::Rect> = clip
            .segments()
            .map(|g| kurbo::ParamCurveExtrema::bounding_box(&g))
            .collect();
        let hidden = |c: &kurbo::Rect| {
            let c = (*c + kurbo::Vec2::new(ox, oy)).inflate(reach, reach);
            !segs.iter().any(|b| overlaps(b, &c))
                && kurbo::Shape::winding(clip, c.center()) % 2 == 0
        };
        let mut want = vec![false; cells.len()];
        let mut ok = vec![false; cells.len()];
        for (i, c) in cells.iter().enumerate() {
            if hidden(c) {
                ok[i] = true;
            } else if need[i] {
                want[i] = true;
                ok[i] = true;
            }
        }
        // Drawn pixel for pixel (no transform), only the cells rastered
        // need a pixmap; under a transform it is sampled between pixels and
        // filled over its whole rect, so it is the whole's size and place.
        let window = if to_surface == Affine::IDENTITY {
            cells
                .iter()
                .zip(&want)
                .filter_map(|(c, w)| w.then_some(*c))
                .reduce(|a, c| a.union(c))
        } else {
            Some(kurbo::Rect::new(0.0, 0.0, f64::from(w), f64::from(h)))
        };
        let pixmap = window.filter(|_| want.iter().any(|w| *w)).map(|window| {
            let pm = render_shadow_cells(self.lanes(), s, (w, h), &want, window);
            let px: u64 = cells
                .iter()
                .zip(&want)
                .filter(|(_, w)| **w)
                .map(|(c, _)| (c.width() * c.height()) as u64)
                .sum();
            self.blurred += px;
            (Arc::new(pm), (window.x0, window.y0))
        });
        self.parts.insert(
            item,
            ShadowPart {
                pixmap,
                size: (w, h),
                ok,
            },
        );
    }

    /// (redraw) This frame's part of a shadow for display item `item`
    /// ([`Self::shadow_part`]).
    pub fn peek_part(&self, item: usize) -> Option<&ShadowPart> {
        self.parts.get(&item)
    }

    /// Counts a pixmap of blurred shadow rendered.
    fn built(&mut self, pm: &Pixmap) {
        self.builds += 1;
        self.blurred += pm.width() as u64 * pm.height() as u64;
    }

    /// Notes that a frame draws a shadow of `extent` directly (tests: see
    /// [`Self::blurred`]).
    pub fn drew_directly(&mut self, extent: kurbo::Rect) {
        let (w, h) = whole_px(&extent);
        self.blurred += w as u64 * h as u64;
    }

    /// Pixels of blurred shadow rendered so far: whole pixmaps and slice
    /// tiles built, and the extents of shadows frames drew directly
    /// (tests: a box whose size springs renders none after its first
    /// frame).
    pub fn blurred(&self) -> u64 {
        self.blurred
    }

    /// The tiles of `s` sliced (see `slice_plan`), built as needed, or
    /// `None` when its field depends on its box's size (draw it whole).
    /// Each quadrant's base tile is cached at once: it depends on the blur,
    /// colour and sub-pixel phase only. A corner tile, which also depends
    /// on its radius, is held uncached (`recent`, at most
    /// [`RECENT_BYTES`]) and cached when any later frame asks for it,
    /// however many other surfaces painted in between, so a radius
    /// springing frame by frame builds a corner tile a frame and never
    /// evicts what the cache holds, and a still one is built once.
    pub fn shadow_slices(&mut self, s: &ShadowShape) -> Option<Slices> {
        let plan = slice_plan(s)?;
        self.build_tiles(&plan, s);
        Slices::from_plan(&plan, |t| Some(self.tile(t, s)))
    }

    /// (perf-cpu-raster) Builds the tiles of `plan` that neither the
    /// cache nor this frame holds, on a thread each (a radius springing
    /// builds its four corner tiles every frame: 2 ms of a 1× frame and
    /// 6.7 ms of a 1.5× one, one after another), then keeps them as
    /// [`Self::tile`] would have, in its order: a base tile cached, a
    /// corner tile held. The tiles the cache holds are touched first, as
    /// `tile` would before building the next quadrant's, so no
    /// insertion evicts them. Each tile is the same pure function of its
    /// spec on whichever thread builds it.
    fn build_tiles(&mut self, plan: &SlicePlan, s: &ShadowShape) {
        let frame = self.frame;
        let mut missing: Vec<&TileSpec> = Vec::new();
        for t in plan.quads.iter().flat_map(|q| [&q.base, &q.corner]) {
            if self.entries.contains_key(&t.key) {
                self.get(t.key);
            } else if self.recent.contains_key(&t.key) || missing.iter().any(|m| m.key == t.key) {
                // Held (`tile` caches it if an earlier frame built it),
                // or already to be built.
            } else {
                missing.push(t);
            }
        }
        if missing.len() < 2 {
            return;
        }
        let built: Vec<std::sync::Mutex<Option<Pixmap>>> = missing
            .iter()
            .map(|_| std::sync::Mutex::new(None))
            .collect();
        let threads = crate::raster::threads().min(missing.len());
        crate::raster::share(&mut vec![(); threads], missing.len(), |_, i| {
            let pm = render_tile(missing[i], s);
            *built[i]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pm);
        });
        let built = built.into_iter().map(|b| {
            b.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        for (t, pm) in missing.into_iter().zip(built) {
            // Every task ran (a panic is this call's).
            let Some(pm) = pm else { continue };
            self.built(&pm);
            if t.base {
                self.insert(t.key, pm);
            } else {
                self.hold_recent(t.key, Arc::new(pm));
            }
        }
        debug_assert_eq!(self.frame, frame);
    }

    /// The tiles of `s` sliced, if this frame or the cache holds all of
    /// them (the raster's draw, after [`Self::shadow_slices`]).
    pub fn peek_slices(&self, s: &ShadowShape) -> Option<Slices> {
        let plan = slice_plan(s)?;
        Slices::from_plan(&plan, |t| self.peek(t.key).cloned())
    }

    fn tile(&mut self, t: &TileSpec, s: &ShadowShape) -> Arc<Pixmap> {
        if let Some(p) = self.get(t.key) {
            return p;
        }
        let frame = self.frame;
        match self.recent.get(&t.key) {
            Some(r) if r.frame == frame => return r.pixmap.clone(),
            Some(_) => {
                if let Some(r) = self.recent.remove(&t.key) {
                    self.recent_bytes -= r.bytes;
                    return self.insert_arc(t.key, r.pixmap);
                }
            }
            None => {}
        }
        let pm = render_tile(t, s);
        self.built(&pm);
        if t.base {
            return self.insert(t.key, pm);
        }
        let pm = Arc::new(pm);
        self.hold_recent(t.key, pm.clone());
        pm
    }

    /// Holds a corner tile this frame built uncached, dropping the
    /// oldest other frames' tiles past [`RECENT_BYTES`].
    fn hold_recent(&mut self, key: u64, pixmap: Arc<Pixmap>) {
        let bytes = pixmap.width() as usize * pixmap.height() as usize * 4;
        while self.recent_bytes + bytes > RECENT_BYTES {
            let frame = self.frame;
            let Some((&old, _)) = self
                .recent
                .iter()
                .filter(|(_, r)| r.frame != frame)
                .min_by_key(|(_, r)| r.tick)
            else {
                break;
            };
            if let Some(r) = self.recent.remove(&old) {
                self.recent_bytes -= r.bytes;
            }
        }
        self.tick += 1;
        self.recent_bytes += bytes;
        let old = self.recent.insert(
            key,
            Recent {
                pixmap,
                bytes,
                frame: self.frame,
                tick: self.tick,
                at: Instant::now(),
            },
        );
        if let Some(r) = old {
            self.recent_bytes -= r.bytes;
        }
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

/// Colour tables kept (each 16 kB, so at most 4 MB); past it the record
/// starts over.
const LUTS_KEPT: usize = 256;

/// The colour tables kept, by their stops' bits, and when one was last
/// asked for.
struct Luts {
    tables: HashMap<Box<[u32]>, Arc<[[f32; 4]]>>,
    used: Instant,
}

static LUTS: std::sync::Mutex<Option<Luts>> = std::sync::Mutex::new(None);

fn luts() -> std::sync::MutexGuard<'static, Option<Luts>> {
    LUTS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// (perf-cpu-raster review) Frees every colour table when none has been
/// asked for since `now - idle`: a gradient whose colours spring leaves
/// a table per frame no later frame asks for. Called with the paint
/// cache's own trim ([`PaintCache::trim_idle`]), at a paint or a wake
/// of the render loop, never at a wake of its own.
fn trim_luts(now: Instant, idle: Duration) {
    let mut l = luts();
    if l.as_ref()
        .is_some_and(|l| now.saturating_duration_since(l.used) >= idle)
    {
        *l = None;
    }
}

/// (perf-cpu-raster) [`lut`] of `stops`, kept by their exact bits: a
/// gradient drawn smoothly or too large or changing to cache dithers its
/// part in every cell of every frame, and building its 1024 OKLab
/// samples cost more than the part (a staggered grid's 24 tiles: 2.7
/// ms a frame of the 3.8 ms raster). Shared by every thread (the
/// raster's draw them in parallel, `raster::draw_parallel`).
fn lut_cached(stops: &[GradientStop]) -> Arc<[[f32; 4]]> {
    let key: Box<[u32]> = stops
        .iter()
        .flat_map(|s| {
            [
                s.offset.to_bits(),
                s.color.r.to_bits(),
                s.color.g.to_bits(),
                s.color.b.to_bits(),
                s.color.a.to_bits(),
            ]
        })
        .collect();
    if let Some(l) = luts().as_mut()
        && let Some(t) = l.tables.get(&key).cloned()
    {
        l.used = Instant::now();
        return t;
    }
    let table: Arc<[[f32; 4]]> = lut(stops).into();
    let mut l = luts();
    // The clock read under the lock: a trim between the miss and here
    // never sees a use older than itself.
    let now = Instant::now();
    let l = l.get_or_insert_with(|| Luts {
        tables: HashMap::new(),
        used: now,
    });
    if l.tables.len() >= LUTS_KEPT {
        l.tables.clear();
    }
    l.tables.insert(key, table.clone());
    l.used = now;
    table
}

#[cfg(test)]
thread_local! {
    /// Gradient parts this thread dithered (tests).
    pub(crate) static PARTS_DITHERED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Colour tables held (tests: bounded).
#[cfg(test)]
pub(crate) fn luts_held() -> usize {
    luts().as_ref().map_or(0, |l| l.tables.len())
}

/// (perf-cpu-raster) The part `(x0, y0, pw, ph)` of a gradient's whole
/// pixmap `whole` (as [`render_gradient`] drew it): the pixels
/// [`render_gradient_part`] would dither for that part, copied.
pub fn crop_part(whole: &Pixmap, (x0, y0, pw, ph): (u32, u32, u32, u32)) -> Option<Pixmap> {
    let (w, h) = (whole.width() as u32, whole.height() as u32);
    if pw == 0 || ph == 0 || x0 + pw > w || y0 + ph > h {
        return None;
    }
    let mut pm = Pixmap::new(pw as u16, ph as u16);
    let src = whole.data();
    let dst = pm.data_mut();
    for y in 0..ph {
        let from = ((y0 + y) * w + x0) as usize;
        dst[(y * pw) as usize..][..pw as usize].copy_from_slice(&src[from..from + pw as usize]);
    }
    pm.set_may_have_transparency(whole.may_have_transparency());
    Some(pm)
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

/// (perf-cpu-raster) Pixels of a gradient part past which its rows are
/// dithered on several threads (a 720 × 380 card's gradient took 14 ms
/// on one).
const PARALLEL_GRADIENT_PX: u32 = 1 << 16;

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
    part: (u32, u32, u32, u32),
) -> Option<Pixmap> {
    let (_, _, pw, ph) = part;
    if pw == 0 || ph == 0 || pw > u16::MAX as u32 || ph > u16::MAX as u32 {
        return None;
    }
    #[cfg(test)]
    PARTS_DITHERED.with(|n| n.set(n.get() + 1));
    let stops: &[GradientStop] = match paint {
        Paint::Solid(_) => return None,
        Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. } => stops,
    };
    let table = lut_cached(stops);
    // Claimed opaque when its stops are (opaque-fast-path): every pixel
    // then is (their alpha is 255 exactly, which a dither under half a
    // level rounds back to 255). Drawn pixel for pixel its samples are
    // its pixels, and one drawn smoothly is isolated in a layer of its
    // own (`raster::draw`'s `fill_with`), as `crate::image`'s
    // `to_pixmap` says.
    let opaque = !stops.is_empty() && stops.iter().all(|s| s.color.clamped().a >= 1.0);
    let mut pm = Pixmap::new(pw as u16, ph as u16);
    // Where a frame pixel centre falls along the gradient, 0 to 1: each
    // geometry its own loop (no call through a pointer per pixel).
    let data = pm.data_mut();
    match paint {
        Paint::Solid(_) => return None,
        Paint::Linear { angle, .. } => {
            let a = finite_angle(*angle).to_radians();
            let (sin, cos) = a.sin_cos();
            let len = (w as f64 * sin).abs() + (h as f64 * cos).abs();
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let (dx, dy) = (sin, -cos);
            dither_part(data, part, &table, move |x, y| {
                if len <= 0.0 {
                    return 0.0;
                }
                ((x - cx) * dx + (y - cy) * dy) / len + 0.5
            });
        }
        Paint::Radial { .. } => {
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let r = cx.hypot(cy);
            dither_part(data, part, &table, move |x, y| {
                if r <= 0.0 {
                    0.0
                } else {
                    (x - cx).hypot(y - cy) / r
                }
            });
        }
        Paint::Conic { from, .. } => {
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let from = finite_angle(*from).to_radians();
            dither_part(data, part, &table, move |x, y| {
                // Clockwise from the top: atan2 of (x, -y).
                let a = (x - cx).atan2(-(y - cy)) - from;
                a.rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU
            });
        }
    }
    pm.set_may_have_transparency(!opaque);
    Some(pm)
}

/// Dithers the part `(x0, y0, pw, ph)` of a gradient whose geometry is
/// `geo` into `data` (`pw × ph`): a large part's rows shared among
/// threads ([`PARALLEL_GRADIENT_PX`]), each pixel the same pure
/// function of its place whichever thread draws it.
fn dither_part(
    data: &mut [PremulRgba8],
    (x0, y0, pw, ph): (u32, u32, u32, u32),
    table: &[[f32; 4]],
    geo: impl Fn(f64, f64) -> f64 + Sync,
) {
    let threads = crate::raster::threads().min(ph as usize);
    if pw * ph < PARALLEL_GRADIENT_PX || threads <= 1 {
        dither_rows(data, (x0, y0, pw), table, &geo);
        return;
    }
    let rows = (ph as usize).div_ceil(threads);
    let chunks: Vec<std::sync::Mutex<Option<&mut [PremulRgba8]>>> = data
        .chunks_mut(rows * pw as usize)
        .map(|c| std::sync::Mutex::new(Some(c)))
        .collect();
    crate::raster::share(&mut vec![(); threads], chunks.len(), |_, i| {
        let chunk = chunks[i]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(chunk) = chunk {
            dither_rows(chunk, (x0, y0 + (i * rows) as u32, pw), table, &geo);
        }
    });
}

/// Dithers `data`'s rows (`pw` wide) of a gradient part whose first row
/// is the frame's row `y0`, starting at its column `x0`.
fn dither_rows(
    data: &mut [PremulRgba8],
    (x0, y0, pw): (u32, u32, u32),
    table: &[[f32; 4]],
    geo: &impl Fn(f64, f64) -> f64,
) {
    for (py, row) in data.chunks_mut(pw as usize).enumerate() {
        for (px, out) in row.iter_mut().enumerate() {
            let (x, y) = (x0 + px as u32, y0 + py as u32);
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
            *out = PremulRgba8 {
                r: c(0),
                g: c(1),
                b: c(2),
                a: alpha,
            };
        }
    }
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
    whole_px(&s.extent)
}

/// The pixel size of `e` out to whole pixels.
fn whole_px(e: &kurbo::Rect) -> (u32, u32) {
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
    render_blurred(w, h, |ctx| {
        ctx.set_transform(Affine::translate((-ox, -oy)));
        draw_shadow(ctx, s);
    })
}

/// Renders one tile of a sliced shadow.
fn render_tile(t: &TileSpec, s: &ShadowShape) -> Pixmap {
    render_blurred(t.w, t.h, |ctx| {
        ctx.set_paint(bgra(s.color));
        ctx.fill_blurred_rounded_rect(&t.local, t.radius, s.std_dev, false);
    })
}

/// Renders what `draw` draws into a `w × h` pixmap.
fn render_blurred(w: u16, h: u16, draw: impl FnOnce(&mut RenderContext)) -> Pixmap {
    let settings = RenderSettings {
        num_threads: 0,
        ..RenderSettings::default()
    };
    let mut ctx = RenderContext::new_with(w, h, settings);
    ctx.set_fill_rule(Fill::NonZero);
    draw(&mut ctx);
    ctx.flush();
    // (perf-cpu-raster) Rendered straight into the pixmap: its pixels
    // are premultiplied RGBA8 bytes in order, as vello writes them.
    let mut pm = Pixmap::new(w, h);
    let mut resources = Resources::new();
    if let Some(target) = PixmapMut::new(w, h, pm.data_as_u8_slice_mut()) {
        ctx.render_with(
            target,
            &mut resources,
            RasterizerSettings {
                target_init: TargetInit::SrcOver,
                render_mode: RenderMode::OptimizeQuality,
                ..RasterizerSettings::default()
            },
        );
    }
    pm
}

/// Sides of the cells a large shadow's pixmap is rastered in, whole or in
/// part ([`PaintCache::shadow_part`]): each pixel is always rastered by a
/// context of its cell's size with the shadow translated by the cell's
/// origin, so a part is bit for bit the whole pixmap there (vello's
/// blurred rect is evaluated in f32 relative to the context's origin, and
/// one context of the whole size rounds otherwise).
pub const PART_CELL_W: u32 = 128;
/// See [`PART_CELL_W`].
pub const PART_CELL_H: u32 = 64;

/// (redraw) What [`PaintCache::shadow_part`] rastered of a shadow's
/// pixmap for one frame.
#[derive(Debug)]
pub struct ShadowPart {
    /// A pixmap holding the cells rastered, and where its origin lies in
    /// the whole's (`None`: every cell the frame needs lies inside the
    /// box, so none shows).
    pixmap: Option<(Arc<Pixmap>, (f64, f64))>,
    size: (u32, u32),
    /// Per cell of the grid: rastered, or wholly inside the box.
    ok: Vec<bool>,
}

impl ShadowPart {
    /// The pixmap, and its origin from the shadow's whole-pixel origin,
    /// where a cached one is drawn: (0, 0), and the whole's size, when the
    /// shadow is drawn under a transform.
    pub fn pixmap(&self) -> Option<(&Arc<Pixmap>, (f64, f64))> {
        self.pixmap.as_ref().map(|(p, at)| (p, *at))
    }

    /// Whether it holds every pixel `area` (pixmap px) may sample, one on
    /// around it (bilinear) included.
    pub fn covers(&self, area: kurbo::Rect) -> bool {
        let area = area.inflate(1.0, 1.0);
        part_grid(self.size.0, self.size.1)
            .iter()
            .zip(&self.ok)
            .all(|(c, ok)| *ok || !overlaps(c, &area))
    }
}

/// The cells of a `w × h` pixmap's part grid, row by row.
fn part_grid(w: u32, h: u32) -> Vec<kurbo::Rect> {
    let mut cells = Vec::new();
    for y in (0..h).step_by(PART_CELL_H as usize) {
        for x in (0..w).step_by(PART_CELL_W as usize) {
            cells.push(kurbo::Rect::new(
                f64::from(x),
                f64::from(y),
                f64::from((x + PART_CELL_W).min(w)),
                f64::from((y + PART_CELL_H).min(h)),
            ));
        }
    }
    cells
}

/// Whether `a` and `b` overlap or touch (a line's zero-area box counts).
fn overlaps(a: &kurbo::Rect, b: &kurbo::Rect) -> bool {
    a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1
}

/// How many item pixels one device pixel spans at most under `back`
/// (surface to item coordinates).
fn device_px(back: Affine) -> f64 {
    let [a, b, c, d, _, _] = back.as_coeffs();
    a.hypot(b).max(c.hypot(d))
}

/// (redraw) A thread's render contexts for [`render_shadow_cells`], one
/// per cell size in use (a grid has at most four), kept between frames
/// in [`PaintCache`] and freed when idle.
#[derive(Debug, Default)]
struct CellLane {
    contexts: Vec<RenderContext>,
    resources: Option<Resources>,
}

impl CellLane {
    /// Rasters `s` into the pixmap cell `c` (pixmap px), its pixmap's
    /// origin at `(ox, oy)`, as [`render_blurred`] would with a context of
    /// its own (the same settings, translation and drawing).
    fn render(&mut self, s: &ShadowShape, c: kurbo::Rect, ox: f64, oy: f64) -> Pixmap {
        let (w, h) = (c.width() as u16, c.height() as u16);
        let at = match self
            .contexts
            .iter()
            .position(|x| x.width() == w && x.height() == h)
        {
            Some(i) => i,
            None => {
                let settings = RenderSettings {
                    num_threads: 0,
                    ..RenderSettings::default()
                };
                self.contexts.push(RenderContext::new_with(w, h, settings));
                self.contexts.len() - 1
            }
        };
        let ctx = &mut self.contexts[at];
        ctx.reset();
        ctx.set_fill_rule(Fill::NonZero);
        ctx.set_transform(Affine::translate((-(ox + c.x0), -(oy + c.y0))));
        draw_shadow(ctx, s);
        ctx.flush();
        let mut pm = Pixmap::new(w, h);
        let resources = self.resources.get_or_insert_with(Resources::new);
        if let Some(target) = PixmapMut::new(w, h, pm.data_as_u8_slice_mut()) {
            ctx.render_with(
                target,
                resources,
                RasterizerSettings {
                    target_init: TargetInit::SrcOver,
                    render_mode: RenderMode::OptimizeQuality,
                    ..RasterizerSettings::default()
                },
            );
        }
        ctx.reset();
        pm
    }
}

/// (redraw) The lanes [`render_shadow_cells`] rasters on, and when they
/// last did.
#[derive(Debug, Default)]
struct CellLanes {
    lanes: Vec<CellLane>,
    used: Option<Instant>,
}

/// `s`'s pixmap of `w × h`, or the part of it `window` (pixmap px, on
/// whole pixels) covers, with only the cells of its part grid that `want`
/// marks rastered (the others transparent), each by a context of its
/// cell's size, on as many threads as there are cells to share.
fn render_shadow_cells(
    lanes: &mut CellLanes,
    s: &ShadowShape,
    (w, h): (u32, u32),
    want: &[bool],
    window: kurbo::Rect,
) -> Pixmap {
    let cells: Vec<kurbo::Rect> = part_grid(w, h)
        .into_iter()
        .zip(want)
        .filter_map(|(c, w)| w.then_some(c))
        .collect();
    let (ox, oy) = (s.extent.x0.floor(), s.extent.y0.floor());
    let done: Vec<std::sync::Mutex<Option<Pixmap>>> =
        cells.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let threads = crate::raster::threads().min(cells.len()).max(1);
    while lanes.lanes.len() < threads {
        lanes.lanes.push(CellLane::default());
    }
    lanes.used = Some(Instant::now());
    crate::raster::share(&mut lanes.lanes[..threads], cells.len(), |lane, i| {
        let pm = lane.render(s, cells[i], ox, oy);
        *done[i]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pm);
    });
    let (wx, wy) = (window.x0 as usize, window.y0 as usize);
    let mut pm = Pixmap::new(window.width() as u16, window.height() as u16);
    let row = window.width() as usize;
    let data = pm.data_mut();
    for (c, cell) in cells.iter().zip(done) {
        let cell = cell
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Every task ran (a panic is this call's).
        let Some(cell) = cell else { continue };
        let (x0, y0, cw) = (
            c.x0 as usize - wx,
            c.y0 as usize - wy,
            cell.width() as usize,
        );
        for (y, src) in cell.data().chunks_exact(cw).enumerate() {
            let at = (y0 + y) * row + x0;
            data[at..at + cw].copy_from_slice(src);
        }
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

    /// (opaque-fast-path) A gradient's dithered pixmap claims to be opaque
    /// exactly when its stops are, and then every pixel is: the dither,
    /// under half a level, rounds an alpha of 255 back to 255.
    #[test]
    fn a_gradient_pixmap_claims_opacity_with_opaque_stops() {
        let pm = render_gradient_part(&ramp("#1e66f5", "#ea76cb"), 97, 61, (0, 0, 97, 61)).unwrap();
        assert!(!pm.may_have_transparency());
        assert!(pm.data().iter().all(|p| p.a == 255));
        let pm =
            render_gradient_part(&ramp("#1e66f5", "#ea76cb80"), 97, 61, (0, 0, 97, 61)).unwrap();
        assert!(pm.may_have_transparency());
        let none = Paint::Linear {
            angle: 0.0,
            stops: Vec::new(),
        };
        let pm = render_gradient_part(&none, 9, 9, (0, 0, 9, 9)).unwrap();
        assert!(pm.may_have_transparency(), "no stops: transparent");
    }

    /// (perf-cpu-raster) A part copied from a gradient's whole pixmap is
    /// the part dithered on its own, for every paint and parts at the
    /// edges, the corners and inside; a part past the pixmap is refused.
    #[test]
    fn a_part_cropped_from_the_whole_is_the_part_dithered() {
        let paints = [
            ramp("#1e66f5", "#ea76cb"),
            ramp("#1e66f580", "#ea76cb"),
            Paint::Radial {
                stops: vec![
                    GradientStop {
                        offset: 0.2,
                        color: Color::from_hex("#f38ba8").unwrap(),
                    },
                    GradientStop {
                        offset: 0.9,
                        color: Color::from_hex("#89b4fa00").unwrap(),
                    },
                ],
            },
            Paint::Conic {
                from: 30.0,
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: Color::from_hex("#a6e3a1").unwrap(),
                    },
                    GradientStop {
                        offset: 1.0,
                        color: Color::from_hex("#cba6f7").unwrap(),
                    },
                ],
            },
        ];
        let (w, h) = (173u32, 91u32);
        for p in &paints {
            let whole = render_gradient(p, w, h).unwrap();
            for part in [
                (0, 0, w, h),
                (0, 0, 17, 9),
                (w - 30, h - 20, 30, 20),
                (41, 13, 66, 58),
                (100, 0, 73, 91),
                (5, 77, 1, 1),
            ] {
                let a = crop_part(&whole, part).unwrap();
                let b = render_gradient_part(p, w, h, part).unwrap();
                assert!(a.data() == b.data(), "{p:?} {part:?}");
                assert_eq!(a.may_have_transparency(), b.may_have_transparency());
            }
            assert!(crop_part(&whole, (w - 5, 0, 6, 4)).is_none());
            assert!(crop_part(&whole, (0, 0, 0, 4)).is_none());
        }
    }

    /// (perf-cpu-raster) A large gradient, dithered on several threads,
    /// is the bytes of its strips dithered one by one on this one (each
    /// under [`PARALLEL_GRADIENT_PX`]), for every geometry.
    #[test]
    fn a_large_gradient_on_threads_is_its_strips_on_one() {
        let paints = [
            ramp("#1e66f5", "#ea76cb80"),
            Paint::Radial {
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: Color::from_hex("#f38ba8").unwrap(),
                    },
                    GradientStop {
                        offset: 1.0,
                        color: Color::from_hex("#11111b").unwrap(),
                    },
                ],
            },
            Paint::Conic {
                from: 200.0,
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: Color::from_hex("#a6e3a1").unwrap(),
                    },
                    GradientStop {
                        offset: 1.0,
                        color: Color::from_hex("#cba6f7").unwrap(),
                    },
                ],
            },
        ];
        let (w, h) = (731u32, 413u32);
        assert!(w * h > 4 * PARALLEL_GRADIENT_PX);
        for p in &paints {
            let whole = render_gradient(p, w, h).unwrap();
            let strip = 50;
            for y in (0..h).step_by(strip as usize) {
                let sh = strip.min(h - y);
                assert!(w * sh < PARALLEL_GRADIENT_PX);
                let part = render_gradient_part(p, w, h, (0, y, w, sh)).unwrap();
                let from = (y * w) as usize;
                assert!(
                    whole.data()[from..from + (w * sh) as usize] == *part.data(),
                    "{p:?} rows {y}.."
                );
            }
            // And a large part off the frame's corner.
            let part = (37, 21, 600, 300);
            assert!(
                crop_part(&whole, part).unwrap().data()
                    == render_gradient_part(p, w, h, part).unwrap().data()
            );
        }
    }

    /// (perf-cpu-raster) The colour tables kept are each the table built
    /// afresh, and their number stays bounded however many gradients
    /// pass (each a new spring step of a colour).
    #[test]
    fn colour_tables_are_kept_exact_and_bounded() {
        let s = ramp("#313244", "#45475a");
        let Paint::Linear { stops, .. } = &s else {
            unreachable!()
        };
        assert!(*lut_cached(stops) == *lut(stops));
        assert!(*lut_cached(stops) == *lut(stops), "from the cache");
        for i in 0..3 * LUTS_KEPT {
            let c = Color {
                r: i as f32 / (3 * LUTS_KEPT) as f32,
                g: 0.5,
                b: 0.25,
                a: 1.0,
            };
            let stops = [
                GradientStop {
                    offset: 0.0,
                    color: c,
                },
                GradientStop {
                    offset: 1.0,
                    color: Color::WHITE,
                },
            ];
            assert!(*lut_cached(&stops) == *lut(&stops));
            assert!(luts_held() <= LUTS_KEPT);
        }
    }

    /// (perf-cpu-raster review) Stops that differ only in an offset, or
    /// only in one channel of a colour, are each their own table: asked
    /// for alternately, each is the table built afresh, and each draws
    /// its own pixels (a stop sliding along a gradient, or a colour's
    /// alpha springing, never draws the last frame's).
    #[test]
    fn stops_differing_in_one_field_keep_their_own_tables() {
        let stops = |mid: f32, alpha: f32| {
            vec![
                GradientStop {
                    offset: 0.0,
                    color: Color {
                        r: 0.9,
                        g: 0.2,
                        b: 0.1,
                        a: 1.0,
                    },
                },
                GradientStop {
                    offset: mid,
                    color: Color {
                        r: 0.1,
                        g: 0.7,
                        b: 0.3,
                        a: alpha,
                    },
                },
                GradientStop {
                    offset: 1.0,
                    color: Color {
                        r: 0.2,
                        g: 0.1,
                        b: 0.8,
                        a: 1.0,
                    },
                },
            ]
        };
        // Offsets only, then alpha only, then one more channel.
        let mut green = stops(0.5, 0.75);
        green[1].color.g = 0.71;
        let pairs = [
            (stops(0.5, 0.75), stops(0.625, 0.75)),
            (stops(0.5, 0.75), stops(0.5, 0.5)),
            (stops(0.5, 0.75), green),
        ];
        for (a, b) in &pairs {
            for _ in 0..3 {
                assert!(*lut_cached(a) == *lut(a));
                assert!(*lut_cached(b) == *lut(b));
            }
            assert!(*lut(a) != *lut(b), "the pair's tables are the same");
            let paint = |stops: &Vec<GradientStop>| Paint::Linear {
                angle: 90.0,
                stops: stops.clone(),
            };
            let pa = render_gradient(&paint(a), 300, 20).unwrap();
            let pb = render_gradient(&paint(b), 300, 20).unwrap();
            assert_ne!(pa.data_as_u8_slice(), pb.data_as_u8_slice());
            // Alternately once more, each from the cache.
            let qa = render_gradient(&paint(a), 300, 20).unwrap();
            assert_eq!(pa.data_as_u8_slice(), qa.data_as_u8_slice());
        }
    }

    /// (perf-cpu-raster review) The colour tables are freed once none
    /// has been asked for for the paint cache's idle time, with the
    /// paint cache's own idle entries, and kept while asked for.
    #[test]
    fn colour_tables_are_freed_when_idle() {
        let s = ramp("#123456", "#654321");
        let Paint::Linear { stops, .. } = &s else {
            unreachable!()
        };
        lut_cached(stops);
        assert!(luts_held() >= 1);
        let mut c = PaintCache::default();
        // Not idle yet: kept (whatever other tests ask for meanwhile).
        c.trim_idle(Instant::now() + IDLE_FREE / 2);
        let after = Instant::now();
        // Idle past IDLE_FREE from every use so far: freed, unless a
        // test running beside this one asked for a table since.
        c.trim_idle(after + IDLE_FREE + Duration::from_secs(1));
        let used = luts().as_ref().map(|l| l.used);
        assert!(
            used.is_none_or(|u| u > after),
            "the tables outlived their idle time"
        );
        // And built again, the same, when asked for after.
        assert!(*lut_cached(stops) == *lut(stops));
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

    fn shadow() -> ShadowShape {
        ShadowShape {
            rect: kurbo::Rect::new(20.0, 20.0, 620.0, 220.0),
            radii: [20.0; 4],
            std_dev: 12.0,
            color: Color::from_hex("#00000060").unwrap(),
            extent: kurbo::Rect::new(0.0, 0.0, 640.0, 240.0),
        }
    }

    /// A gradient or shadow over `EAGER_ENTRY_BYTES` is built only by a
    /// second frame repainting all of it: a first frame and partial
    /// frames draw it directly (decisions.md, m5-speed-caches). The GPU
    /// backend's `shadow` still builds at first sight.
    #[test]
    fn large_entries_wait_for_a_second_whole_frame() {
        let big = ShadowShape {
            rect: kurbo::Rect::new(48.0, 48.0, 1248.0, 1048.0),
            extent: kurbo::Rect::new(0.0, 0.0, 1300.0, 1100.0),
            ..shadow()
        };
        let (w, h) = shadow_size(&big);
        assert!(!eager(w, h) && cacheable(w, h));
        let ramp = Paint::Linear {
            angle: 160.0,
            stops: stops("#1e1e2e", "#313244"),
        };
        let mut c = PaintCache::default();
        c.begin_frame();
        assert!(c.shadow_in(&big, true).is_none(), "first whole frame");
        assert!(c.gradient_in(&ramp, w, h, true).is_none());
        for _ in 0..3 {
            c.begin_frame();
            assert!(c.shadow_in(&big, false).is_none(), "partial frame");
            assert!(c.gradient_in(&ramp, w, h, false).is_none());
        }
        assert_eq!((c.builds(), c.bytes()), (0, 0));
        c.begin_frame();
        assert!(c.shadow_in(&big, true).is_some(), "second whole frame");
        assert!(c.gradient_in(&ramp, w, h, true).is_some());
        assert_eq!(c.builds(), 2);
        c.begin_frame();
        assert!(c.shadow_in(&big, false).is_some(), "cached for any frame");
        // Small entries keep their rule: a shadow at once, a gradient on a
        // second frame's use, partial or not.
        c.begin_frame();
        assert!(c.shadow_in(&shadow(), false).is_some());
        assert!(c.gradient_in(&ramp, 100, 100, false).is_none());
        c.begin_frame();
        assert!(c.gradient_in(&ramp, 100, 100, false).is_some());
        // The GPU backend's frames are whole: built at first sight.
        let mut g = PaintCache::default();
        assert!(g.shadow(&big).is_some());
    }

    fn stops(a: &str, b: &str) -> Vec<GradientStop> {
        vec![
            GradientStop {
                offset: 0.0,
                color: Color::from_hex(a).unwrap(),
            },
            GradientStop {
                offset: 1.0,
                color: Color::from_hex(b).unwrap(),
            },
        ]
    }

    /// design.md's rice `border: 1.5, conic(from: t * 40deg, …)`: a
    /// gradient whose paint changes every frame is never cached, so it
    /// never evicts the shadow beside it; nor do two such rings of the
    /// same size, nor a gradient box whose size springs.
    #[test]
    fn an_animated_conic_border_never_evicts_a_cached_shadow() {
        let mut c = PaintCache::default();
        let key = shadow_key(&shadow());
        c.begin_frame();
        assert!(c.shadow(&shadow()).is_some());
        let builds = c.builds();
        // 240 frames of two 480 × 480 rings turning together (900 KB
        // each, 432 MB uncached) and a 300-wide box growing a pixel a
        // frame.
        for i in 0..240 {
            c.begin_frame();
            for (n, colors) in [("#89b4fa", "#cba6f7"), ("#a6e3a1", "#f9e2af")]
                .into_iter()
                .enumerate()
            {
                let ring = Paint::Conic {
                    from: i as f32 * 0.67 + n as f32 * 90.0,
                    stops: stops(colors.0, colors.1),
                };
                assert!(c.gradient(&ring, 480, 480).is_none());
            }
            let grown = ramp("#000000", "#ffffff");
            assert!(c.gradient(&grown, 300 + i, 200).is_none());
            assert!(c.peek(key).is_some(), "the shadow was evicted at frame {i}");
        }
        assert_eq!(c.builds(), builds);
        assert!(c.bytes() <= PAINT_CACHE_BYTES);
        // A still gradient of the same size is cached once a second frame
        // draws it.
        let still = ramp("#000000", "#ffffff");
        c.begin_frame();
        assert!(c.gradient(&still, 480, 480).is_none(), "first drawn");
        assert!(c.gradient(&still, 480, 480).is_none(), "same frame");
        c.begin_frame();
        assert!(c.gradient(&still, 480, 480).is_some());
        c.begin_frame();
        assert!(c.gradient(&still, 480, 480).is_some());
        assert_eq!(c.builds(), builds + 1);
    }

    /// Entries no frame used for `IDLE_FREE` are freed; the current
    /// frame's stay.
    #[test]
    fn idle_entries_are_freed() {
        let mut c = PaintCache::default();
        let p = ramp("#000000", "#ffffff");
        c.begin_frame();
        assert!(c.gradient(&p, 100, 100).is_none());
        c.begin_frame();
        c.shadow(&shadow()).unwrap();
        c.gradient(&p, 100, 100).unwrap();
        let now = Instant::now();
        c.trim_idle(now);
        assert!(c.bytes() > 0, "used this frame");
        c.begin_frame();
        c.trim_idle(now + IDLE_FREE / 2);
        assert!(c.bytes() > 0, "not idle long enough");
        c.trim_idle(now + IDLE_FREE + Duration::from_millis(1));
        assert_eq!(c.bytes(), 0);
        assert!(c.peek(shadow_key(&shadow())).is_none());
        assert!(c.seen.is_empty());
    }

    /// A paint reads the trim's clock before it touches its entries
    /// (`Raster::paint`): an entry touched after that instant is never
    /// idle, however long the frame's preparation took.
    #[test]
    fn entries_touched_after_the_trim_clock_stay() {
        let mut c = PaintCache::default();
        c.set_idle_free(Duration::from_millis(1));
        let now = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        c.begin_frame();
        c.shadow(&shadow()).unwrap();
        c.trim_idle(now);
        assert!(c.bytes() > 0, "built after the clock was read");
    }

    #[test]
    fn the_cache_reuses_and_evicts() {
        let mut c = PaintCache::default();
        let p = ramp("#000000", "#ffffff");
        c.begin_frame();
        assert!(c.gradient(&p, 100, 100).is_none());
        c.begin_frame();
        let a = c.gradient(&p, 100, 100).unwrap();
        c.begin_frame();
        let b = c.gradient(&p, 100, 100).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(c.builds(), 1);
        // Too large to cache: drawn by vello instead.
        assert!(c.gradient(&p, 2000, 2000).is_none());
        for _ in 0..2 {
            for i in 0..200 {
                c.begin_frame();
                c.gradient(&p, 200 + i, 200);
            }
        }
        assert!(c.builds() > 1);
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

    /// A shadow as `flatten` makes one: `blur` logical pixels at `scale`
    /// over a box already in physical pixels.
    fn cast(rect: kurbo::Rect, radii: [f32; 4], blur: f64, scale: f64, color: &str) -> ShadowShape {
        let sd = blur * scale / 2.0;
        let reach = (3.0 * sd).ceil() + 1.0;
        ShadowShape {
            rect,
            radii,
            std_dev: sd as f32,
            color: Color::from_hex(color).unwrap(),
            extent: rect.inflate(reach, reach),
        }
    }

    /// `s` drawn from its slices into a pixmap the size and origin of its
    /// whole one.
    fn sliced(s: &ShadowShape, c: &mut PaintCache) -> Pixmap {
        let sl = c.shadow_slices(s).expect("sliced");
        let (w, h) = shadow_size(s);
        let (ox, oy) = (s.extent.x0.floor(), s.extent.y0.floor());
        render_blurred(w as u16, h as u16, |ctx| {
            ctx.set_transform(Affine::translate((-ox, -oy)));
            draw_slices(ctx, &sl, s.extent.inflate(1.0, 1.0));
        })
    }

    /// The largest channel difference and how many pixels differ.
    fn diff(a: &Pixmap, b: &Pixmap) -> (u8, usize) {
        assert_eq!((a.width(), a.height()), (b.width(), b.height()));
        let mut worst = 0u8;
        let mut n = 0;
        for (p, q) in a.data().iter().zip(b.data()) {
            let d = [
                p.r.abs_diff(q.r),
                p.g.abs_diff(q.g),
                p.b.abs_diff(q.b),
                p.a.abs_diff(q.a),
            ];
            let m = d.into_iter().max().unwrap_or(0);
            worst = worst.max(m);
            n += (m > 0) as usize;
        }
        (worst, n)
    }

    /// (shadow-slice) A sliced shadow is the direct blurred rect, to
    /// rounding: uniform and per-corner radii, several sizes, fractional
    /// edges, at scale 1 and 1.5 (the field is f32 relative to the box,
    /// so a pixel's place rounds a little differently in a tile than in
    /// the whole box).
    #[test]
    fn sliced_shadows_match_the_direct_blur() {
        let mut worst = (0u8, 0.0f64, String::new());
        let mut cases = 0;
        for scale in [1.0, 1.5] {
            for (blur, w, h) in [
                (8.0, 120.0, 90.0),
                (24.0, 300.0, 200.0),
                (48.0, 600.0, 420.0),
            ] {
                let (w, h) = (w * scale, h * scale);
                for (fx, fy, fw, fh) in [
                    (0.0, 0.0, 0.0, 0.0),
                    (0.5, 0.5, 0.0, 0.0),
                    (0.25, 0.75, 0.5, 0.25),
                    (0.7, 0.1, 0.3, 0.9),
                ] {
                    let rect = kurbo::Rect::new(
                        30.0 + fx,
                        17.0 + fy,
                        30.0 + fx + w + fw,
                        17.0 + fy + h + fh,
                    );
                    let k = scale as f32;
                    for radii in [
                        [0.0; 4],
                        [12.0 * k; 4],
                        [18.0 * k, 18.0 * k, 32.0 * k, 32.0 * k],
                        [0.0, 40.0 * k, 6.5 * k, 9.25 * k],
                    ] {
                        let s = cast(rect, radii, blur, scale, "#1e66f566");
                        let direct =
                            render_shadow(&s, shadow_size(&s).0 as u16, shadow_size(&s).1 as u16);
                        let mut c = PaintCache::default();
                        c.begin_frame();
                        let lit = direct.data().iter().filter(|p| p.a > 0).count();
                        assert!(lit > direct.data().len() / 4, "{s:?}");
                        let (m, n) = diff(&sliced(&s, &mut c), &direct);
                        let share = n as f64 / direct.data().len() as f64;
                        if (m, share) > (worst.0, worst.1) {
                            worst = (m, share, format!("{s:?}"));
                        }
                        assert!(
                            m <= 1 && share < 0.01,
                            "{m} {n} of {}: {s:?}",
                            direct.data().len()
                        );
                        cases += 1;
                    }
                }
            }
        }
        eprintln!(
            "{cases} cases; worst {} ({:.4}% of pixels): {}",
            worst.0,
            worst.1 * 100.0,
            worst.2
        );
    }

    /// (shadow-slice) A box whose field follows its size is drawn whole,
    /// as before: one under `2 × FLAT` σ across, one so round that vello
    /// clamps its corners to the box (a pill), and an unblurred one.
    #[test]
    fn small_round_or_sharp_boxes_are_not_sliced() {
        let card = kurbo::Rect::new(10.0, 10.0, 410.0, 310.0);
        let ok = cast(card, [16.0; 4], 24.0, 1.0, "#00000066");
        assert!(slice_plan(&ok).is_some());
        // 8σ is the shortest side sliced: 12σ = 144 px here.
        let narrow = kurbo::Rect::new(10.0, 10.0, 410.0, 150.0);
        assert!(slice_plan(&cast(narrow, [16.0; 4], 24.0, 1.0, "#00000066")).is_some());
        let narrower = kurbo::Rect::new(10.0, 10.0, 410.0, 100.0);
        let s = cast(narrower, [16.0; 4], 24.0, 1.0, "#00000066");
        assert!(slice_plan(&s).is_none());
        let mut c = PaintCache::default();
        c.begin_frame();
        assert!(c.shadow_slices(&s).is_none());
        assert!(
            c.shadow_in(&s, true).is_some(),
            "drawn from its whole pixmap"
        );
        // A pill: r1 = hypot(r, 2σ) is clamped to half its height.
        let pill = kurbo::Rect::new(10.0, 10.0, 610.0, 160.0);
        assert!(slice_plan(&cast(pill, [75.0; 4], 24.0, 1.0, "#00000066")).is_none());
        assert!(slice_plan(&cast(pill, [40.0; 4], 24.0, 1.0, "#00000066")).is_some());
        // Unblurred: a near-step edge.
        assert!(slice_plan(&cast(card, [16.0; 4], 0.0, 1.0, "#00000066")).is_none());
        // A tile over `EAGER_ENTRY_BYTES`: σ = 125 makes a base tile about
        // 820 px square (2.7 MB), so the shadow keeps the large-entry rule;
        // σ = 100 makes one of about 655 px (1.7 MB).
        let wide = kurbo::Rect::new(10.0, 10.0, 2410.0, 1810.0);
        assert!(slice_plan(&cast(wide, [16.0; 4], 250.0, 1.0, "#00000066")).is_none());
        let plan = slice_plan(&cast(wide, [16.0; 4], 200.0, 1.0, "#00000066")).unwrap();
        assert!(
            plan.quads
                .iter()
                .all(|q| eager(q.base.w as u32, q.base.h as u32)
                    && eager(q.corner.w as u32, q.corner.h as u32))
        );
    }

    /// (shadow-slice) A box whose size springs draws every frame from the
    /// tiles its first frame built; one whose corner radius springs builds
    /// that corner's tiles each frame without caching them, and caches
    /// them once a second frame asks for the same radius.
    #[test]
    fn a_springing_box_draws_from_the_same_tiles() {
        let mut c = PaintCache::default();
        let at = |i: usize, r: f32| {
            let (w, h) = (450.0 + 7.0 * i as f64, 330.0 + 3.0 * i as f64);
            cast(
                kurbo::Rect::new(40.0, 40.0, 40.0 + w, 40.0 + h),
                [27.0, 27.0, r, r],
                48.0,
                1.5,
                "#00000066",
            )
        };
        c.begin_frame();
        assert!(c.shadow_slices(&at(0, 48.0)).is_some());
        let (builds, blurred, bytes) = (c.builds(), c.blurred(), c.bytes);
        assert_eq!(builds, 8, "four base and four corner tiles");
        for i in 1..30 {
            c.begin_frame();
            assert!(c.shadow_slices(&at(i, 48.0)).is_some());
            assert_eq!((c.builds(), c.blurred()), (builds, blurred), "frame {i}");
        }
        // Only the base tiles were cached at once; the corner tiles a
        // frame later. The tiles are small next to the whole shadow.
        assert!(c.bytes > bytes);
        assert_eq!(c.recent_bytes, 0);
        let whole = {
            let (w, h) = shadow_size(&at(29, 48.0));
            w as usize * h as usize * 4
        };
        assert!(
            c.bytes() < whole,
            "{} B of tiles, {whole} B whole",
            c.bytes()
        );
        eprintln!("tiles {} B, whole shadow {whole} B", c.bytes());
        let held = c.bytes();
        // The bottom radius springs: two corner tiles a frame, uncached
        // (held aside, at most `RECENT_BYTES` of them).
        for i in 0..40 {
            c.begin_frame();
            assert!(c.shadow_slices(&at(29, 50.5 + 2.5 * i as f32)).is_some());
            assert_eq!(c.builds(), builds + 2 * (i + 1), "frame {i}");
            assert_eq!(c.bytes, held, "a springing radius caches nothing");
            assert!(c.recent_bytes <= RECENT_BYTES, "frame {i}");
            assert_eq!(c.bytes(), c.bytes + c.recent_bytes);
        }
        // It settles: the next frame caches the tiles the last one built.
        c.begin_frame();
        let s = at(29, 50.5 + 2.5 * 39.0);
        assert!(c.shadow_slices(&s).is_some());
        assert_eq!(c.builds(), builds + 80);
        assert!(c.bytes > held);
        c.begin_frame();
        let (m, _) = diff(
            &sliced(&s, &mut c),
            &render_shadow(&s, shadow_size(&s).0 as u16, shadow_size(&s).1 as u16),
        );
        assert!(m <= 1);
        assert_eq!(c.builds(), builds + 80, "cached");
    }

    /// (shadow-slice) The cache's frames are every surface's paints (CPU
    /// prepares and GPU lowerings): a still card's corner tiles are built
    /// once and cached by its next frame however many other surfaces
    /// paint in between, here a bar drawing its own shadow and an OSD
    /// whose radius springs, and its springing radius settles the same.
    #[test]
    fn other_surfaces_painting_between_frames_do_not_lose_corner_tiles() {
        let card = |r: f32| {
            cast(
                kurbo::Rect::new(40.0, 40.0, 940.0, 670.0),
                [27.0, 27.0, r, r],
                48.0,
                1.5,
                "#00000066",
            )
        };
        let bar = cast(
            kurbo::Rect::new(10.0, 10.0, 1900.0, 60.0),
            [0.0; 4],
            4.0,
            1.0,
            "#00000040",
        );
        let osd = |i: usize| {
            cast(
                kurbo::Rect::new(20.0, 20.0, 320.0, 140.0),
                [10.0 + i as f32 * 0.5; 4],
                6.0,
                1.0,
                "#00000080",
            )
        };
        let mut c = PaintCache::default();
        let others = |c: &mut PaintCache, i: usize| {
            for _ in 0..1 + i % 3 {
                c.begin_frame();
                c.shadow_slices(&bar).expect("bar sliced");
                c.begin_frame();
                c.shadow_slices(&osd(i)).expect("osd sliced");
            }
        };
        c.begin_frame();
        c.shadow_slices(&card(48.0)).unwrap();
        for i in 0..12 {
            others(&mut c, i);
            let before = c.builds();
            c.begin_frame();
            c.shadow_slices(&card(48.0)).unwrap();
            assert_eq!(c.builds(), before, "frame {i} rebuilt the card's tiles");
        }
        // Its radius springs, then settles at 60.
        for i in 0..10 {
            others(&mut c, 12 + i);
            let before = c.builds();
            c.begin_frame();
            c.shadow_slices(&card(50.0 + i as f32)).unwrap();
            assert_eq!(c.builds(), before + 2, "springing frame {i}");
        }
        for i in 0..6 {
            others(&mut c, 22 + i);
            let before = c.builds();
            c.begin_frame();
            c.shadow_slices(&card(59.0)).unwrap();
            assert_eq!(c.builds(), before, "settled frame {i} rebuilt");
        }
        assert!(c.recent_bytes <= RECENT_BYTES);
    }

    /// (shadow-slice) At the slicing bound the flat terms are tightest:
    /// the shortest box `slice_plan` takes for a blur, and the roundest
    /// corner it takes for a box, still match the direct blur, at 1.5×
    /// with fractional edges, each way round.
    #[test]
    fn shadows_at_the_slicing_bound_match_the_direct_blur() {
        let mut cases = 0;
        let mut worst = (0u8, 0usize);
        let mut check = |s: &ShadowShape| {
            let (w, h) = shadow_size(s);
            let direct = render_shadow(s, w as u16, h as u16);
            let mut c = PaintCache::default();
            c.begin_frame();
            let (m, n) = diff(&sliced(s, &mut c), &direct);
            worst = worst.max((m, n));
            assert!(
                m <= 1 && (n as f64) < 0.01 * direct.data().len() as f64,
                "{m} {n}: {s:?}"
            );
            cases += 1;
        };
        let scale = 1.5;
        for blur in [8.0, 24.0, 48.0] {
            let sd = blur * scale / 2.0;
            for (fx, fy) in [(0.25, 0.75), (0.6, 0.15)] {
                for tall in [false, true] {
                    let shape = |short: f64, radii: [f32; 4]| {
                        let (w, h) = if tall {
                            (short, 400.0 * scale)
                        } else {
                            (400.0 * scale, short)
                        };
                        cast(
                            kurbo::Rect::new(30.0 + fx, 17.0 + fy, 30.0 + fx + w, 17.0 + fy + h),
                            radii,
                            blur,
                            scale,
                            "#1e66f566",
                        )
                    };
                    // The shortest side sliced, in quarter pixels from 8σ.
                    for radii in [[0.0; 4], [6.0, 18.0, 27.0, 3.0]] {
                        let short = (0..4000)
                            .map(|q| 8.0 * sd + q as f64 * 0.25)
                            .find(|&v| slice_plan(&shape(v, radii)).is_some())
                            .expect("some size slices");
                        assert!(slice_plan(&shape(short - 0.25, radii)).is_none());
                        check(&shape(short, radii));
                        check(&shape(short + 0.5, radii));
                    }
                    // The roundest corner sliced on a box 12σ across,
                    // alone and on all four corners.
                    let short = (12.0 * sd).round();
                    for one in [true, false] {
                        let radii = |r: f32| if one { [0.0, r, 0.0, 0.0] } else { [r; 4] };
                        let r = (0..4000)
                            .map(|q| q as f32 * 0.25)
                            .take_while(|&r| slice_plan(&shape(short, radii(r))).is_some())
                            .last()
                            .expect("a square corner slices");
                        assert!(r > 0.0, "blur {blur}");
                        check(&shape(short, radii(r)));
                    }
                }
            }
        }
        eprintln!("{cases} cases at the bound; worst {worst:?}");
    }
}

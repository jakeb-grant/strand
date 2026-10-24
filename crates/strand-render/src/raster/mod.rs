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

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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
pub(crate) use draw::{disjoint, draw as draw_group, skip_group};
use draw::{draw, frame_px, to_kurbo};
#[cfg(feature = "gpu")]
pub(crate) use paint::ramp;

/// Width of a raster cell: one vello wide tile.
pub const CELL_W: u32 = 256;
/// Height of a raster cell.
pub const CELL_H: u32 = 64;

/// Render contexts kept for distinct cell sizes by default (edge cells are
/// smaller, so a surface uses up to four sizes).
const MIN_CONTEXTS: usize = 8;

/// (perf-cpu-raster) Cells a paint draws before it shares them among
/// threads: under it the threads' start costs more than they save.
pub const PARALLEL_CELLS: usize = 8;

/// Most threads that draw one paint's cells, the render thread with
/// them (a performance core each on a 4 + 8 hybrid laptop; more land
/// on efficiency cores, which finish a cell slower than they help).
pub const MAX_THREADS: usize = 4;

/// (perf-cpu-raster review) How long the other threads' contexts and
/// scratch (`Raster::lanes[1..]`) are kept after the last paint that
/// drew on them: freed at the first paint or wake of the render loop
/// after it (never at a wake of their own). An animation's frames come
/// milliseconds apart, so its lanes are kept while it runs; a bar's
/// first paint (two rows of cells, drawn on four threads) leaves about
/// 1 MB in its three lanes, which an idle shell does not keep. Building
/// them again costs a large paint a few contexts.
pub const LANE_IDLE: Duration = Duration::from_secs(1);

/// (perf-bench) Where the last [`Raster::paint`] spent its time: the
/// paint cache's gradients and shadows built (`prepare`), offscreen
/// groups and fade layers drawn (`offscreen`), the display list encoded
/// into the cells' vello contexts (`encode`), vello's rasterising of
/// them (`fine`), and the cells copied into the buffer (`copy`); and the
/// cells drawn. Wall-clock, advisory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RasterTimes {
    pub prepare: std::time::Duration,
    pub offscreen: std::time::Duration,
    pub encode: std::time::Duration,
    pub fine: std::time::Duration,
    pub copy: std::time::Duration,
    pub cells: u32,
    /// (perf-cpu-raster) The threads that drew the cells (1: the render
    /// thread alone). With more, `encode`, `fine` and `copy` share the
    /// parallel pass's wall-clock time in proportion to the time the
    /// threads spent in each.
    pub threads: u32,
}

/// One thread's vello contexts and scratch space, kept between frames.
#[derive(Debug, Default)]
struct Lane {
    /// One context per cell size in use, most recent first.
    contexts: Vec<RenderContext>,
    resources: Option<Resources>,
    scratch: Vec<u8>,
    /// This paint's time in each part.
    encode: std::time::Duration,
    fine: std::time::Duration,
    copy: std::time::Duration,
    /// The OS thread that last drew with it (tests: gone once the paint
    /// returns).
    #[cfg(test)]
    tid: i64,
}

/// One cell a paint draws: where, the damage's disjoint parts in it,
/// and its row of cells (its band of the target).
#[derive(Debug)]
struct CellJob {
    cell: Rect,
    clips: Vec<Rect>,
    band: usize,
}

/// What every cell of a paint draws from.
struct Shared<'a> {
    items: &'a [DisplayItem],
    atlas: &'a AtlasMirror,
    cache: &'a PaintCache,
    scale: Scale,
    groups: &'a std::collections::HashMap<usize, crate::offscreen::Drawn>,
    fades: &'a std::collections::HashMap<usize, crate::offscreen::Drawn>,
    settings: RasterizerSettings,
    max_contexts: usize,
    stride: usize,
    origin: (i64, i64),
}

/// Owns the vello contexts and scratch space between frames.
#[derive(Debug)]
pub struct Raster {
    /// The render thread's contexts and scratch (`lanes[0]`), and the
    /// other threads' that share large paints ([`PARALLEL_CELLS`]).
    lanes: Vec<Lane>,
    /// Contexts kept; see [`Raster::set_surfaces`].
    max_contexts: usize,
    /// Most threads a paint's cells are drawn on ([`threads`]; tests set
    /// 1 to draw on the render thread alone).
    max_threads: usize,
    /// Pixels the last `paint` handed to vello (cells × cell area).
    rasterised: u64,
    /// Dithered gradients and blurred shadows, drawn from pixmaps.
    cache: PaintCache,
    /// (M4) Offscreen groups (blurred, colour-filtered subtrees).
    offscreen: crate::offscreen::Offscreen,
    /// (perf-bench) The last paint's parts.
    times: RasterTimes,
    /// (perf-cpu-raster review) When a paint last drew on `lanes[1..]`.
    shared_at: Option<Instant>,
}

impl Default for Raster {
    fn default() -> Self {
        Self {
            lanes: vec![Lane::default()],
            max_contexts: MIN_CONTEXTS,
            max_threads: threads(),
            rasterised: 0,
            cache: PaintCache::default(),
            offscreen: crate::offscreen::Offscreen::default(),
            times: RasterTimes::default(),
            shared_at: None,
        }
    }
}

impl Raster {
    /// Keeps enough contexts for `n` surfaces (four cell sizes each: full
    /// cells, the right column, the bottom row and the corner), so several
    /// outputs of different sizes never rebuild contexts every frame.
    pub fn set_surfaces(&mut self, n: usize) {
        self.max_contexts = (4 * n).max(MIN_CONTEXTS);
        for lane in &mut self.lanes {
            lane.contexts.truncate(self.max_contexts);
        }
    }

    /// Render contexts the render thread keeps.
    #[cfg(test)]
    pub fn contexts(&self) -> usize {
        self.lanes[0].contexts.len()
    }

    /// (perf-cpu-raster) Draws a paint's cells on at most `n` threads
    /// (tests: 1 draws them all on the calling thread).
    #[cfg(test)]
    pub(crate) fn set_max_threads(&mut self, n: usize) {
        self.max_threads = n.max(1);
    }

    /// (perf-cpu-raster) The OS threads other than the caller's that
    /// drew the last paint's cells (tests).
    #[cfg(test)]
    pub(crate) fn worker_tids(&self) -> Vec<i64> {
        let threads = (self.times.threads as usize).min(self.lanes.len());
        self.lanes[1..threads.max(1)]
            .iter()
            .map(|l| l.tid)
            .collect()
    }

    /// (perf-cpu-raster) Threads that have drawn a paint's cells so far,
    /// the render thread with them (tests).
    #[cfg(test)]
    pub(crate) fn lanes(&self) -> usize {
        self.lanes.len()
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

    /// (m6-owner-fixes, bug 17) Turns fade layers on or off (tests).
    pub fn set_fade_layers(&mut self, on: bool) {
        self.offscreen.set_fade_layers(on);
    }

    /// Frees the paint cache's and offscreen groups' idle entries without
    /// painting.
    pub fn trim_idle(&mut self, now: std::time::Instant) {
        self.cache.trim_idle(now);
        self.offscreen.trim_idle(now);
        crate::hugepage::trim_idle(now, self.cache.idle_free());
        self.trim_lanes(now);
    }

    /// (perf-cpu-raster review) Frees the other threads' lanes (their
    /// contexts, vello resources and scratch) once no paint has drawn on
    /// them for [`LANE_IDLE`] (or the shorter idle time a test sets with
    /// [`Raster::set_idle_free`]).
    fn trim_lanes(&mut self, now: Instant) {
        if self.lanes.len() <= 1 {
            return;
        }
        let idle = LANE_IDLE.min(self.cache.idle_free());
        if self
            .shared_at
            .is_none_or(|t| now.saturating_duration_since(t) >= idle)
        {
            self.lanes.truncate(1);
            self.lanes.shrink_to_fit();
            self.shared_at = None;
        }
    }

    /// Shortens (tests) how long an unused paint cache entry or
    /// offscreen group lives.
    pub fn set_idle_free(&mut self, idle: std::time::Duration) {
        self.cache.set_idle_free(idle);
        self.offscreen.set_idle_free(idle);
    }

    /// (eval-sandbox) A raster with nothing cached, nothing drawn before
    /// and this one's settings (fade layers on or off): what draws a
    /// frame as into a fresh buffer, for the `redraw` check
    /// (`Renderer::repaint_fresh`).
    #[cfg(feature = "eval-hooks")]
    pub(crate) fn fresh(&self) -> Raster {
        let mut r = Raster::default();
        r.offscreen.set_fade_layers(self.offscreen.fade_layers_on());
        r
    }

    /// Pixels the last [`Raster::paint`] rasterised. Tracks damage, not
    /// buffer size.
    pub fn rasterised(&self) -> u64 {
        self.rasterised
    }

    /// (perf-bench) Where the last paint spent its time.
    pub fn times(&self) -> RasterTimes {
        self.times
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
        let surface = target.bounds();
        self.paint_window(items, damage, atlas, scale, target, surface, (0, 0));
    }

    /// [`Raster::paint`] into a buffer that holds only a window of the
    /// surface: `target`'s pixel (0, 0) is the surface's `origin`, and
    /// `surface` is the whole surface (offscreen groups are drawn over
    /// it as in a full paint). The same cells rasterise each pixel, so
    /// the window is bit-identical to that part of a full paint (the
    /// drag icon, [`crate::Renderer::drag_image`]).
    #[allow(clippy::too_many_arguments)]
    pub fn paint_window(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        atlas: &AtlasMirror,
        scale: Scale,
        target: &mut PaintTarget<'_>,
        surface: Rect,
        origin: (i64, i64),
    ) {
        self.rasterised = 0;
        self.times = RasterTimes::default();
        if target.size.is_empty() || target.validate().is_err() {
            return;
        }
        let window = Rect::from_edges(
            origin.0,
            origin.1,
            origin.0 + target.size.w as i64,
            origin.1 + target.size.h as i64,
        );
        let Some(window) = window.intersect(surface) else {
            return;
        };
        let damage = damage.clipped(window);
        let Some(bbox) = damage.bounds() else {
            return;
        };
        // The trim's clock is read before this frame touches its
        // entries, so none it builds or reuses counts as idle however
        // long `prepare` takes (a slow runner took over the 20 ms a test
        // sets, and freed a shadow the frame had just built).
        let now = Instant::now();
        self.prepare(items, &damage, window);
        self.cache.trim_idle(now);
        let mark = Instant::now();
        self.times.prepare = mark.saturating_duration_since(now);
        self.offscreen
            .prepare(items, &damage, surface, atlas, &self.cache, scale);
        self.offscreen
            .prepare_fades(items, &damage, surface, atlas, &self.cache, scale);
        self.offscreen.trim_idle(now);
        crate::hugepage::trim_idle(now, self.cache.idle_free());
        self.trim_lanes(now);
        self.times.offscreen = mark.elapsed();
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
        let mut jobs: Vec<CellJob> = Vec::new();
        // Each row of cells' rows of the target, `[top, bottom)`.
        let mut bands: Vec<(i64, i64)> = Vec::new();
        for cy in bbox.top().div_euclid(ch)..(bbox.bottom() + ch - 1).div_euclid(ch) {
            for cx in bbox.left().div_euclid(cw)..(bbox.right() + cw - 1).div_euclid(cw) {
                let Some(cell) = Rect::from_edges(cx * cw, cy * ch, (cx + 1) * cw, (cy + 1) * ch)
                    .intersect(surface)
                else {
                    continue;
                };
                let clips: Vec<Rect> = parts.iter().filter_map(|p| p.intersect(cell)).collect();
                if clips.is_empty() {
                    continue;
                }
                self.rasterised += cell.area();
                let rows = (
                    cell.top().max(window.top()) - origin.1,
                    cell.bottom().min(window.bottom()) - origin.1,
                );
                if bands.last() != Some(&rows) {
                    bands.push(rows);
                }
                jobs.push(CellJob {
                    cell,
                    clips,
                    band: bands.len() - 1,
                });
            }
        }
        self.times.cells = jobs.len() as u32;
        let shared = Shared {
            items,
            atlas,
            cache: &self.cache,
            scale,
            groups: self.offscreen.current(),
            fades: self.offscreen.fades(),
            settings,
            max_contexts: self.max_contexts,
            stride: target.stride as usize,
            origin,
        };
        let mut threads = if jobs.len() >= PARALLEL_CELLS {
            self.max_threads.min(jobs.len())
        } else {
            1
        };
        let lanes_used = threads;
        while self.lanes.len() < threads {
            self.lanes.push(Lane::default());
        }
        for lane in &mut self.lanes {
            lane.encode = Duration::ZERO;
            lane.fine = Duration::ZERO;
            lane.copy = Duration::ZERO;
        }
        let start = Instant::now();
        if threads <= 1 {
            let lane = &mut self.lanes[0];
            for job in &jobs {
                lane.draw(job, &shared);
                lane.copy_into(job, &shared, target.pixels, 0);
            }
        } else {
            threads = draw_parallel(
                &mut self.lanes[..lanes_used],
                &jobs,
                &bands,
                &shared,
                target,
            );
        }
        let wall = start.elapsed();
        if lanes_used > 1 {
            self.shared_at = Some(Instant::now());
        }
        self.times.threads = threads as u32;
        let lanes = &self.lanes[..lanes_used];
        let (enc, fine, copy) = lanes.iter().fold(
            (Duration::ZERO, Duration::ZERO, Duration::ZERO),
            |(e, f, c), l| (e + l.encode, f + l.fine, c + l.copy),
        );
        if threads <= 1 {
            (self.times.encode, self.times.fine, self.times.copy) = (enc, fine, copy);
        } else {
            // The pass's wall-clock time, shared as the threads spent
            // theirs.
            let total = (enc + fine + copy).as_secs_f64();
            let part = |d: Duration| {
                if total > 0.0 {
                    wall.mul_f64(d.as_secs_f64() / total)
                } else {
                    Duration::ZERO
                }
            };
            (self.times.encode, self.times.fine, self.times.copy) =
                (part(enc), part(fine), part(copy));
        }
    }
}

/// Threads a large paint may share its cells among: the cores this
/// process may run on, at most [`MAX_THREADS`] (asked once: the
/// question reads cgroup files).
pub(crate) fn threads() -> usize {
    static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, MAX_THREADS)
    })
}

/// (perf-cpu-raster) Draws `jobs` on up to `lanes.len()` threads
/// ([`share`]; it returns how many drew), the render thread one of
/// them, started for this paint and joined before it returns (so
/// nothing runs, or wakes, between paints). Each thread
/// takes the next cell not yet taken, draws it alone in its own
/// context as the render thread would (every cell is drawn on its own,
/// so which thread draws it changes no pixel), and copies it into its
/// band of `target`: the rows of its row of cells, held by a lock while
/// it copies, since the cells of a row share them.
fn draw_parallel(
    lanes: &mut [Lane],
    jobs: &[CellJob],
    bands: &[(i64, i64)],
    shared: &Shared<'_>,
    target: &mut PaintTarget<'_>,
) -> usize {
    let stride = shared.stride;
    let mut slices = Vec::with_capacity(bands.len());
    let mut rest: &mut [u8] = target.pixels;
    let mut at = 0usize;
    for &(top, bottom) in bands {
        let (top, bottom) = (
            top.max(0) as usize * stride,
            bottom.max(0) as usize * stride,
        );
        let skip = top.saturating_sub(at).min(rest.len());
        let (_, tail) = std::mem::take(&mut rest).split_at_mut(skip);
        let len = bottom.saturating_sub(top).min(tail.len());
        let (band, tail) = tail.split_at_mut(len);
        slices.push(Mutex::new(band));
        rest = tail;
        at = top + len;
    }
    share(lanes, jobs.len(), |lane, i| {
        #[cfg(test)]
        {
            // SAFETY: gettid has no preconditions.
            lane.tid = unsafe { libc::gettid() } as i64;
        }
        let job = &jobs[i];
        lane.draw(job, shared);
        let mut band = slices[job.band]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lane.copy_into(job, shared, &mut band, bands[job.band].0);
    })
}

/// Helper threads running in this process ([`share`]): at most
/// [`MAX_THREADS`] − 1 at once, however many renderers paint together
/// (an eval sandbox, tests), so the raster never oversubscribes the
/// cores the rest of the process (the text worker) needs.
static HELPERS: AtomicUsize = AtomicUsize::new(0);

/// Takes up to `want` of [`HELPERS`]' free places; the guard gives them
/// back.
fn reserve_helpers(want: usize) -> Helpers {
    let mut held = HELPERS.load(Ordering::Relaxed);
    loop {
        let n = want.min((MAX_THREADS - 1).saturating_sub(held));
        if n == 0 {
            return Helpers(0);
        }
        match HELPERS.compare_exchange_weak(held, held + n, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return Helpers(n),
            Err(now) => held = now,
        }
    }
}

/// Places in [`HELPERS`], given back when dropped (a panic included).
struct Helpers(usize);

impl Drop for Helpers {
    fn drop(&mut self) {
        if self.0 > 0 {
            HELPERS.fetch_sub(self.0, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// A test thread whose `share` calls start every helper they ask for,
    /// outside [`HELPERS`]: tests that count a paint's threads, which
    /// other tests' paints would otherwise take places from.
    static UNBUDGETED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// (tests) This thread's `share` calls start every helper they ask for
/// and hold no place in [`HELPERS`].
#[cfg(test)]
pub(crate) fn unbudgeted() {
    UNBUDGETED.with(|u| u.set(true));
}

/// (perf-cpu-raster) Runs `work(state, i)` for every `i` in `0..tasks`
/// on up to `states.len()` threads, the calling thread one of them with
/// `states[0]`: each takes the next task not yet taken. The others are
/// started for this call and joined before it returns, so nothing runs
/// or wakes between calls (idle is zero work); they are as many as
/// [`HELPERS`] has room for, and one that cannot start leaves its tasks
/// to the rest. A task's panic is the caller's. Returns the threads
/// that ran, the caller's included.
pub(crate) fn share<S: Send>(
    states: &mut [S],
    tasks: usize,
    work: impl Fn(&mut S, usize) + Sync,
) -> usize {
    let next = AtomicUsize::new(0);
    let run = |state: &mut S| {
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= tasks {
                break;
            }
            work(state, i);
        }
    };
    let Some((first, others)) = states.split_first_mut() else {
        return 0;
    };
    let want = others.len().min(tasks.saturating_sub(1));
    #[cfg(test)]
    let (_helpers, want) = if UNBUDGETED.with(|u| u.get()) {
        (Helpers(0), want)
    } else {
        let h = reserve_helpers(want);
        let n = h.0;
        (h, n)
    };
    #[cfg(not(test))]
    let (_helpers, want) = {
        let h = reserve_helpers(want);
        let n = h.0;
        (h, n)
    };
    std::thread::scope(|s| {
        let started: Vec<_> = others[..want]
            .iter_mut()
            .filter_map(|state| {
                std::thread::Builder::new()
                    .name("strand-raster".into())
                    .spawn_scoped(s, || run(state))
                    .ok()
            })
            .collect();
        let ran = 1 + started.len();
        run(first);
        // Joined, not only finished: the threads have exited when the
        // call returns.
        for t in started {
            if let Err(e) = t.join() {
                std::panic::resume_unwind(e);
            }
        }
        ran
    })
}

impl Lane {
    fn context(&mut self, w: u16, h: u16, max: usize) -> &mut RenderContext {
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
                self.contexts.truncate(max);
            }
        }
        &mut self.contexts[0]
    }

    /// Rasterises `job`'s cell into `scratch`: the scene translated by the
    /// cell's origin, clipped to the damage's parts in it.
    fn draw(&mut self, job: &CellJob, sh: &Shared<'_>) {
        let mark = Instant::now();
        let cell = job.cell;
        // Cells are at most CELL_W × CELL_H.
        let (w, h) = (cell.w as u16, cell.h as u16);
        let base = Affine::translate((-(cell.x as f64), -(cell.y as f64)));
        let ctx = self.context(w, h, sh.max_contexts);
        ctx.reset();
        ctx.set_transform(base);
        for clip in &job.clips {
            ctx.push_clip_rect(&to_kurbo(*clip));
            draw(
                ctx,
                sh.items,
                *clip,
                sh.atlas,
                sh.cache,
                sh.scale,
                base,
                Affine::IDENTITY,
                sh.groups,
                sh.fades,
            );
            ctx.pop_clip();
        }
        ctx.flush();
        let encoded = Instant::now();
        self.encode += encoded.saturating_duration_since(mark);
        let row = cell.w as usize * 4;
        self.scratch.clear();
        self.scratch.resize(row * cell.h as usize, 0);
        let resources = self.resources.get_or_insert_with(Resources::new);
        let ctx = &mut self.contexts[0];
        if let Some(pm) = PixmapMut::new(w, h, &mut self.scratch) {
            ctx.render_with(pm, resources, sh.settings);
        }
        // Release image references so atlas pages can be updated in
        // place.
        ctx.reset();
        self.fine += encoded.elapsed();
    }

    /// Copies `job`'s clips from `scratch` into `dst`: the target's rows
    /// from `row0` on.
    fn copy_into(&mut self, job: &CellJob, sh: &Shared<'_>, dst: &mut [u8], row0: i64) {
        let mark = Instant::now();
        let cell = job.cell;
        let row = cell.w as usize * 4;
        let (stride, origin) = (sh.stride, sh.origin);
        for c in &job.clips {
            let lx0 = (c.left() - cell.left()) as usize * 4;
            let lx1 = (c.right() - cell.left()) as usize * 4;
            let x0 = (c.left() - origin.0) as usize * 4;
            let x1 = (c.right() - origin.0) as usize * 4;
            for y in c.top()..c.bottom() {
                let ly = (y - cell.top()) as usize;
                let y = (y - origin.1 - row0) as usize;
                dst[y * stride + x0..y * stride + x1]
                    .copy_from_slice(&self.scratch[ly * row + lx0..ly * row + lx1]);
            }
        }
        self.copy += mark.elapsed();
    }
}

impl Raster {
    /// Builds the cached pixmaps of the gradients and shadows that touch
    /// `damage` (see [`crate::cache`]), telling the cache which ones this
    /// frame repaints whole (all of their part inside `window`).
    fn prepare(&mut self, items: &[DisplayItem], damage: &Damage, window: Rect) {
        self.cache.begin_frame();
        let touches = |b: &Rect| damage.rects().iter().any(|r| r.intersects(*b));
        let whole = |b: &Rect| b.intersect(window).is_some_and(|b| damage.covers(b));
        // Whether each open `scale`/`rotate` group transforms (a sliced
        // shadow draws only on whole pixels).
        let mut turned: Vec<bool> = Vec::new();
        for d in items {
            match &d.item {
                Item::PushTransform(a) => turned.push(*a != Affine::IDENTITY),
                Item::PopTransform => {
                    turned.pop();
                }
                _ => {}
            }
            if !touches(&d.bounds) {
                continue;
            }
            match &d.item {
                Item::Fill { paint, frame, .. } | Item::Border { paint, frame, .. } => {
                    if let Some((w, h)) = frame_px(*frame) {
                        self.cache.gradient_in(paint, w, h, whole(&d.bounds));
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
                    let shape = ShadowShape {
                        rect: *rect,
                        radii: *radii,
                        std_dev: *std_dev,
                        color: *color,
                        extent: *extent,
                    };
                    let sliced = !turned.last().copied().unwrap_or(false)
                        && self.cache.shadow_slices(&shape).is_some();
                    if !sliced && self.cache.shadow_in(&shape, whole(&d.bounds)).is_none() {
                        self.cache.drew_directly(*extent);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests;

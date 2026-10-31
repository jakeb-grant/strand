//! Frames: when a surface wants one or holds it (for text, a container
//! query, a configure), damage between frames' node records, and
//! [`Painter`](strand_scene::Painter) itself.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;
use strand_scene::time::Instant;

use strand_scene::{
    Damage, NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, Rect, SurfaceId,
};
use vello_cpu::kurbo;

use super::{DAMAGE_HISTORY, MAX_HOLD_PAUSE, Renderer};
use crate::flatten::{NodeRecord, RoundBox};

impl Renderer {
    /// How long a newly configured surface holds its first frame for text
    /// still being shaped ([`FIRST_FRAME_TEXT_WAIT`] by default).
    pub fn set_first_frame_wait(&mut self, wait: Duration) {
        self.first_frame_wait = wait;
    }

    /// How long a painted surface holds a frame for text that has no
    /// glyphs to show yet ([`NEW_TEXT_WAIT`] by default; zero: never).
    pub fn set_new_text_wait(&mut self, wait: Duration) {
        self.new_text_wait = wait;
    }

    /// How long laying out waits for the text worker to shape new text
    /// ([`ESTIMATE_WAIT`] by default; zero: never, it is laid out by an
    /// estimate until it arrives).
    ///
    /// [`ESTIMATE_WAIT`]: super::ESTIMATE_WAIT
    pub fn set_estimate_wait(&mut self, wait: Duration) {
        self.estimate_wait = wait;
    }

    /// Texts laying out has waited for the worker to shape so far (see
    /// [`Renderer::set_estimate_wait`]).
    pub fn estimate_waits(&self) -> u64 {
        self.estimate_waits
    }

    /// Estimate waits laying out has opened so far: one a surface's
    /// frame, however many layout passes it takes and shared by its
    /// `update` and its `paint`, each later wait of the frame taking only
    /// what is left of it (owner-testing.md bug 17). A frame that waits
    /// for nothing opens none.
    pub fn estimate_windows(&self) -> u64 {
        self.estimate_windows
    }

    /// How long laying out has waited for the worker so far (at most
    /// the estimate wait a frame, and what taking in the layouts that
    /// arrived costs).
    pub fn estimate_waited(&self) -> Duration {
        self.estimate_waited
    }

    /// How recently a surface must have painted to count as in motion,
    /// so that it holds no frame for new text ([`BUSY_WINDOW`] by
    /// default; tests set it so they do not depend on the clock).
    pub fn set_busy_window(&mut self, window: Duration) {
        self.busy_window = window;
    }

    /// When a surface that is holding a frame for text will want it
    /// anyway: the loop should wake by then (a calloop timer) and check
    /// [`Painter::wants_frame`] again. `None` when it is not waiting.
    ///
    /// A first frame waits for all of its text (up to the first-frame
    /// wait); a later one only for text with nothing to show yet, a node
    /// just added (up to [`NEW_TEXT_WAIT`]), and only on a surface that
    /// is idle (not painted within [`BUSY_WINDOW`]). Changed text keeps showing
    /// its old layout meanwhile, so it holds nothing.
    pub fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        let s = self.surfaces.get(&surface)?;
        let until = match s.painted {
            false => s.awaiting_text.then_some(s.wait_until).flatten(),
            true => s.new_text_until,
        };
        // (wave-cost) Unless something else wants the frame.
        let quiet = s.quiet_until.filter(|_| {
            !s.dirty && s.valid && !self.pointer_due(surface) && !self.inspect.wants(surface)
        });
        let now = Instant::now();
        // (M4) A pass or readback in flight.
        #[cfg(feature = "gpu")]
        let gpu = self.gpu_hold(surface);
        #[cfg(not(feature = "gpu"))]
        let gpu = None;
        [
            until,
            s.query_hold.map(|(_, t)| t),
            s.size_hold.map(|(_, t)| t),
            gpu,
            quiet,
        ]
        .into_iter()
        .flatten()
        .filter(|t| now < *t)
        .min()
    }

    /// Presentation time of the last frame painted for `surface`.
    pub fn frame_time(&self, surface: SurfaceId) -> Option<Duration> {
        self.surfaces.get(&surface).map(|s| s.time)
    }

    /// Pixels the last paint rasterised (it follows the damage, not the
    /// buffer size).
    pub fn last_raster_pixels(&self) -> u64 {
        self.raster.rasterised()
    }

    /// Where the last `paint` spent its time.
    pub fn last_paint_times(&self) -> PaintTimes {
        self.paint_times
    }

    /// The damage the last `paint` returned for `surface`.
    pub fn last_damage(&self, surface: SurfaceId) -> Option<Damage> {
        self.last_damage.get(&surface).copied()
    }
}

/// (wave-cost) The longest a surface in motion goes unasked for a frame
/// before it looks again at what moves.
const QUIET_MOST: Duration = Duration::from_millis(250);

/// (wave-cost) What the wakes of frames asked for later are rounded up to,
/// so that surfaces due at about the same time wake the loop once.
const QUIET_GRID: Duration = Duration::from_millis(4);

/// Above this many changed glyph cells (old or new), the change is
/// damaged as the box around them, not cell by cell.
pub(super) const GLYPH_CELLS_COMPARED: usize = 64;

/// Damage for a text whose glyphs changed from `a` to `b` (cells in
/// layout order): the cells that differ, where they were and where they
/// are. The common prefix and suffix are skipped (a clock tick, an edit
/// at the end of a long body), so the cost is linear in the glyphs; a
/// middle longer than [`GLYPH_CELLS_COMPARED`] is damaged as one box per
/// side.
pub(super) fn glyph_damage<C: GlyphCell>(a: &[C], b: &[C], d: &mut Damage) {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[prefix..], &b[prefix..]);
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a, b) = (&a[..a.len() - suffix], &b[..b.len() - suffix]);
    if a.len().max(b.len()) > GLYPH_CELLS_COMPARED {
        for side in [a, b] {
            if let Some(u) = side
                .iter()
                .map(GlyphCell::rect)
                .filter(|r| !r.is_empty())
                .reduce(|u, r| u.union(r))
            {
                d.add(u);
            }
        }
        return;
    }
    for c in a {
        if !b.contains(c) {
            d.add(c.rect());
        }
    }
    for c in b {
        if !a.contains(c) {
            d.add(c.rect());
        }
    }
}

/// A glyph cell [`glyph_damage`] compares: a text's cells are
/// `(where, glyph)`; the tests count its comparisons through their own.
pub(super) trait GlyphCell: PartialEq {
    fn rect(&self) -> strand_scene::Rect;
}

impl GlyphCell for (strand_scene::Rect, u64) {
    fn rect(&self) -> strand_scene::Rect {
        self.0
    }
}

/// Damage between two frames' node records: a changed node damages where it
/// was and where it is; added and removed nodes damage their one place.
pub(super) fn diff_records(
    old: &BTreeMap<NodeId, NodeRecord>,
    new: &BTreeMap<NodeId, NodeRecord>,
    d: &mut Damage,
) {
    for (id, n) in new {
        match old.get(id) {
            Some(o) if o == n => {}
            Some(o) => {
                // Glyphs and rings are compared under the same clips.
                let clipped_alike =
                    o.geom.as_ref().map(|g| &g.clips) == n.geom.as_ref().map(|g| &g.clips);
                match (&o.glyphs, &n.glyphs, &o.ring, &n.ring) {
                    // Only its glyphs changed: the glyphs that differ, where
                    // they were and where they are.
                    (Some(a), Some(b), ..) if clipped_alike && a.rest == b.rest => {
                        glyph_damage(&a.cells, &b.cells, d)
                    }
                    // Only its border changed, in the same place: its ring.
                    (_, _, Some(a), Some(b))
                        if clipped_alike
                            && a.rest == b.rest
                            && a.cells == b.cells
                            && o.bounds == n.bounds =>
                    {
                        for c in b.cells {
                            d.add(c);
                        }
                    }
                    _ if geometry_damage(o, n, d) => {}
                    _ => {
                        d.add(o.bounds);
                        d.add(n.bounds);
                    }
                }
            }
            None => d.add(n.bounds),
        }
    }
    for (id, o) in old {
        if !new.contains_key(id) {
            d.add(o.bounds);
        }
    }
}

/// Above this many comparisons (moved nodes times the nodes drawn in
/// both frames), [`order_damage`] damages each moved node's whole
/// bounds, where it was and where it is, instead of what it shares with
/// each node it passed.
pub(super) const ORDER_PAIRS_COMPARED: usize = 1 << 16;

/// (dock-drag) Damage where nodes drawn in both frames changed paint
/// order: a lifted child raised above its siblings or let back among
/// them, a keyed reorder. A pixel two nodes both draw takes the colour
/// of the later one, so where a pair swapped places the colour can
/// change though neither node's record did (and a node that only moved
/// damages no more than its edges, `geometry_damage`). Each pair that
/// swapped damages what both draw, where they were and where they are.
///
/// Every swapped pair holds a node outside a longest run kept in order
/// (the nodes kept in order never swap among themselves), so only those
/// moved nodes are compared with the rest.
pub(super) fn order_damage(
    (old_order, old): (&[NodeId], &BTreeMap<NodeId, NodeRecord>),
    (new_order, new): (&[NodeId], &BTreeMap<NodeId, NodeRecord>),
    d: &mut Damage,
) {
    if old_order == new_order {
        return;
    }
    // A node recorded twice paints where its record says, at its last
    // place.
    let place = |order: &[NodeId]| {
        let mut at = std::collections::HashMap::with_capacity(order.len());
        for (i, id) in order.iter().enumerate() {
            at.insert(*id, i);
        }
        at
    };
    let was = place(old_order);
    let is = place(new_order);
    // The nodes drawn in both, in their new order, with their old places.
    let mut both: Vec<(usize, usize, NodeId)> = is
        .iter()
        .filter_map(|(id, n)| was.get(id).map(|o| (*n, *o, *id)))
        .collect();
    both.sort_unstable();
    if both.windows(2).all(|w| w[0].1 < w[1].1) {
        return;
    }
    let moved = moved_out_of_order(&both);
    let whole = moved.len().saturating_mul(both.len()) > ORDER_PAIRS_COMPARED;
    let bounds = |id: NodeId| {
        (
            old.get(&id).map(|r| r.bounds),
            new.get(&id).map(|r| r.bounds),
        )
    };
    for &m in &moved {
        let (_, mo, mid) = both[m];
        let (ma, mb) = bounds(mid);
        if whole {
            d.add(ma.unwrap_or_default());
            d.add(mb.unwrap_or_default());
            continue;
        }
        for (j, &(_, xo, xid)) in both.iter().enumerate() {
            if j == m || (j < m) == (xo < mo) {
                continue;
            }
            let (xa, xb) = bounds(xid);
            for (p, q) in [(ma, xa), (mb, xb)] {
                if let (Some(p), Some(q)) = (p, q)
                    && let Some(r) = p.intersect(q)
                    && !r.is_empty()
                {
                    d.add(r);
                }
            }
        }
    }
}

/// The indices of `both` (sorted by new place) left out of a longest run
/// whose old places increase.
fn moved_out_of_order(both: &[(usize, usize, NodeId)]) -> Vec<usize> {
    // Patience sorting: `tails[l]` ends the best run of length l + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; both.len()];
    for (i, &(_, o, _)) in both.iter().enumerate() {
        let l = tails.partition_point(|&t| both[t].1 < o);
        if l > 0 {
            prev[i] = tails[l - 1];
        }
        if l == tails.len() {
            tails.push(i);
        } else {
            tails[l] = i;
        }
    }
    let mut kept = vec![false; both.len()];
    let mut at = tails.last().copied().unwrap_or(usize::MAX);
    while at != usize::MAX {
        kept[at] = true;
        at = prev[at];
    }
    (0..both.len()).filter(|i| !kept[*i]).collect()
}

/// Where a CPU paint spent its time ([`Renderer::last_paint_times`]).
/// Wall-clock spans, advisory: each is a few clock reads a frame (a few
/// per raster cell), never a wakeup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaintTimes {
    /// Laying out and flattening (zero when it reused the scene
    /// flattened before), waiting for the text worker included.
    pub flatten: Duration,
    /// Of that, waiting for the text worker to shape new text.
    pub text_wait: Duration,
    /// (perf-bench) Of `flatten`, laying out (every pass the frame ran).
    pub layout: Duration,
    /// (perf-bench) Working out the frame's damage from the node records
    /// (`diff_records`, `order_damage`).
    pub diff: Duration,
    /// (perf-bench) A GPU-drawn frame's display list lowered to
    /// `strand-gpu`'s ops and uploads (zero for a CPU frame).
    pub lower: Duration,
    /// Drawing.
    pub raster: Duration,
    /// (perf-bench) Of `raster`, its parts ([`RasterTimes`]).
    pub raster_parts: crate::raster::RasterTimes,
    /// (perf-bench) A theme crossfade's snapshot taken and the frame
    /// blended over it.
    pub blend: Duration,
}

/// Only where it is changed (`NodeRecord::geom`): its own box moved or
/// resized, or a clip above it did, and it draws the same. The pixels
/// along the boxes' edges and corners that changed, where it was and
/// where it is. False if more changed.
fn geometry_damage(o: &NodeRecord, n: &NodeRecord, d: &mut Damage) -> bool {
    let (Some(a), Some(b)) = (&o.geom, &n.geom) else {
        return false;
    };
    if a.rest != b.rest || a.clips.len() != b.clips.len() {
        return false;
    }
    let mut changed = Vec::new();
    let mut add = |r: Rect| changed.push(r);
    match (a.own, b.own) {
        (Some(x), Some(y)) if x != y => RoundBox::changed(x, y, &mut add),
        (Some(_), Some(_)) => {}
        (None, None) if a.ink == b.ink => {}
        _ => return false,
    }
    for (x, y) in a.clips.iter().zip(b.clips.iter()) {
        if x != y {
            RoundBox::changed(*x, *y, &mut add);
        }
    }
    for r in changed {
        for at in [o.bounds, n.bounds] {
            if let Some(c) = r.intersect(at) {
                d.add(c);
            }
        }
    }
    true
}

impl Painter for Renderer {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let started = Instant::now();
        // (gpu-first review 2) Under `STRAND_LOG=debug`, a paint slower
        // than `SLOW_FRAME` is logged with this thread's time on a core,
        // waiting for one and its major faults, beside the GPU thread's
        // own slow-frame line: a stall under load is told apart from
        // work of ours.
        #[cfg(feature = "gpu")]
        let sched = log::log_enabled!(log::Level::Debug)
            .then(strand_gpu::thread_stats)
            .flatten();
        let damage = self.paint_frame(surface, target);
        self.plan_quiet(surface, target.time, damage.is_empty());
        #[cfg(feature = "gpu")]
        if started.elapsed() >= strand_gpu::SLOW_FRAME && log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "a slow paint of {surface:?}: {:.1} ms:{} flatten={:.1} raster={:.1}",
                started.elapsed().as_secs_f64() * 1e3,
                strand_gpu::thread_stats_since(sched),
                self.paint_times.flatten.as_secs_f64() * 1e3,
                self.paint_times.raster.as_secs_f64() * 1e3,
            );
        }
        let placed = self.placed_root(surface);
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.posed = s.cache.as_ref().and_then(|f| f.pose);
            s.painted_regions = s.cache.as_ref().map(|f| super::PaintedRegions {
                blur: f.blur.clone(),
                input: f.input.clone(),
                root_paints: f.root_paints,
                placed,
            });
            // The next frame waits for its own new text.
            s.text_deadline = None;
        }
        self.pause_holds(surface, started);
        // (M5) `strand report`'s frame timing (`summary.rs`). A frame
        // sent to the GPU is timed when the GPU thread's reply comes.
        #[cfg(feature = "gpu")]
        let sent = self.gpu_frame_sent(surface, started);
        #[cfg(not(feature = "gpu"))]
        let sent = false;
        if !sent && self.surfaces.contains_key(&surface) {
            self.frame_stats
                .record(surface, started.elapsed(), !damage.is_empty());
        }
        // Exits the frame finished: ghosts unmount (their siblings glide
        // into the gap from the next frame), closing surfaces close, and
        // a surface that settled shrinks: the specs are refreshed here so
        // the host sees the change at once.
        // (flex) The pointer over the nodes logic reads it of.
        self.report_pointer(surface);
        self.process_finished();
        self.release_holds();
        self.arm_timer();
        // When the frame loop stops, the paint cache and the offscreen
        // groups keep what its frames did not use: a surface shown again
        // within `IDLE_FREE` (a launcher or popup reopened) draws its
        // first frame from them. They go at the first paint or wake
        // after `IDLE_FREE` (design.md: "freed when idle"), never at a
        // wake of their own (decisions.md, m5-speed-caches).
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.wants(surface)
    }

    fn blur_region(&self, surface: SurfaceId) -> Vec<strand_scene::BlurRegion> {
        let Some(s) = self.surfaces.get(&surface) else {
            return Vec::new();
        };
        // The last painted frame's (eval-r3), else the scene about to be.
        match (&s.painted_regions, &s.cache) {
            (Some(p), _) => p.blur.clone(),
            (None, Some(f)) => f.blur.clone(),
            (None, None) => Vec::new(),
        }
    }

    fn surface_pose(&self, surface: SurfaceId) -> Option<strand_scene::SurfacePose> {
        self.delegated_pose(surface)
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.surfaces
            .get(&surface)
            .map_or_else(Damage::new, |s| s.opaque)
    }

    /// (M6) A `trigger: hover` popup that draws nothing itself (no
    /// background, border, shadow, blur or backdrop of its own) takes
    /// input only in the boxes of its top-level elements that drew
    /// anything in the last frame (a blur or backdrop counts), as drawn
    /// (spec.md 6.7): an island moving inside a wide popup lets the
    /// pointer through everywhere else. Every other surface (but for
    /// growth's, below) keeps the surface manager's region: a popup
    /// opened by `open:` takes clicks in its gaps as it always did (spec.md 14, decisions.md
    /// m6-w2-surface). While a press that went down on the popup is
    /// held (a slider's drag) it takes its whole box: a compositor that
    /// does not keep the pointer on the pressed surface (Hyprland with no
    /// keyboard focus anywhere) would otherwise send it elsewhere, and
    /// end the drag, as soon as it leaves the drawn content
    /// (decisions.md m6-owner-fixes, owner-testing.md bug 15).
    ///
    /// (growth) A content-sized panel or popup whose box was drawn
    /// inside its buffer short of filling it (its box springs over a step
    /// in the larger buffer, or a shrink holds the buffer) takes input in
    /// that box alone: the margin it has not reached, or has left, draws
    /// nothing and passes the pointer to what is behind it. An OSD or a
    /// tooltip takes none (`Renderer::placed_root`).
    ///
    /// (hit-through) A surface with `hit: none` on its root takes input
    /// only where what is on it does: the hit boxes of its last frame
    /// (`hit_region`), so the pointer passes through the rest of it to
    /// what is behind, a spanned panel's margin included. A held press
    /// takes the whole box as on a hover popup.
    fn input_region(&self, surface: SurfaceId) -> Option<Vec<strand_scene::LogicalRect>> {
        let s = self.surfaces.get(&surface)?;
        let hover = self.specs.get(&s.root).is_some_and(|sp| sp.hover_trigger);
        // The last painted frame's (eval-r3: not the cache, which the
        // frame's end may have dropped), else the scene about to be.
        let (input, root_paints, placed) = match (&s.painted_regions, &s.cache) {
            (Some(p), _) => (&p.input, p.root_paints, p.placed),
            (None, Some(f)) => (&f.input, f.root_paints, None),
            (None, None) => return None,
        };
        let held = self.extras.widgets.pressed.contains(&s.root);
        if held {
            return None;
        }
        if hover && !root_paints {
            return Some(input.clone());
        }
        // (hit-through) `hit: none` on its root: where what is on it takes
        // input, and the pointer passes through the rest.
        if self.hit_through(s.root) {
            return Some(hit_region(&s.hits, s.scale.as_f64()));
        }
        placed.map(|r| vec![r])
    }
}

/// (hit-through) Where the hit boxes of a frame take the pointer, logical:
/// each one's bounds as drawn, cut by its clip. Paint order has a parent
/// first, so a box inside one already kept adds nothing (a list's rows).
fn hit_region(hits: &[crate::flatten::HitBox], k: f64) -> Vec<strand_scene::LogicalRect> {
    let mut kept: Vec<kurbo::Rect> = Vec::new();
    for h in hits {
        let drawn = match h.inverse {
            Some(inv) => inv.inverse().transform_rect_bbox(h.rect),
            None => h.rect,
        };
        let c = h.clip;
        let clip = kurbo::Rect::new(
            c.left() as f64,
            c.top() as f64,
            c.right() as f64,
            c.bottom() as f64,
        );
        let r = drawn.intersect(clip);
        let inside = |o: &kurbo::Rect| r.x0 >= o.x0 && r.y0 >= o.y0 && r.x1 <= o.x1 && r.y1 <= o.y1;
        // Not finite: a degenerate transform, never hit.
        if r.area() > 0.0 && r.is_finite() && !kept.iter().any(inside) {
            kept.push(r);
        }
    }
    kept.into_iter()
        .map(|r| {
            strand_scene::LogicalRect::new(
                (r.x0 / k) as f32,
                (r.y0 / k) as f32,
                (r.width() / k) as f32,
                (r.height() / k) as f32,
            )
        })
        .collect()
}

impl Renderer {
    /// (hit-through) Whether the surface node `root` passes the pointer
    /// through by `hit: none`. Not a lock, which never lets the pointer
    /// reach what it hides, nor an OSD or a tooltip, which take none.
    pub(super) fn hit_through(&self, root: NodeId) -> bool {
        let Some(spec) = self.specs.get(&root) else {
            return false;
        };
        !matches!(spec.kind, NodeKind::Lock | NodeKind::Osd)
            && !spec.tooltip
            && self.tree.get(root).is_some_and(
                |n| matches!(n.get(Prop::Hit), Some(PropValue::Keyword(k)) if k == "none"),
            )
    }

    /// (m6-owner-fixes) Whether `surface` takes the pointer at `at`
    /// (logical, on the surface) as its last frame left it: inside the
    /// region [`Painter::input_region`] asks for, or else inside its box
    /// within the shadow overhang (the surface manager's region).
    pub fn takes_input_at(&self, surface: SurfaceId, at: strand_scene::LogicalPoint) -> bool {
        let inside = |r: &strand_scene::LogicalRect| {
            at.x >= r.x && at.y >= r.y && at.x < r.x + r.w && at.y < r.y + r.h
        };
        if let Some(rects) = Painter::input_region(self, surface) {
            return rects.iter().any(inside);
        }
        let Some(s) = self.surfaces.get(&surface) else {
            return false;
        };
        let k = s.scale.as_f64() as f32;
        let o = self
            .specs
            .get(&s.root)
            .map(|sp| sp.overhang)
            .unwrap_or_default();
        let (w, h) = (s.size.w as f32 / k, s.size.h as f32 / k);
        inside(&strand_scene::LogicalRect::new(
            o.left,
            o.top,
            w - o.left - o.right,
            h - o.top - o.bottom,
        ))
    }

    pub(super) fn paint_frame(
        &mut self,
        surface: SurfaceId,
        target: &mut PaintTarget<'_>,
    ) -> Damage {
        if target.validate().is_err() {
            return Damage::new();
        }
        #[cfg(feature = "eval-hooks")]
        let probe_passes = self.layout_passes;
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        s.time = target.time;
        s.quiet_until = None;
        let gap = std::mem::take(&mut s.quiet_gap);
        if gap {
            self.clocks
                .frame_after_gap(surface, target.time, self.now());
        } else {
            self.clocks.frame(surface, target.time, self.now());
        }
        self.anim.set_period(self.clocks.period(surface));
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        if s.resize(target.size, target.scale) {
            self.prune_scales();
        }
        self.poll_text();
        if !self.spec_dirty.is_empty() {
            // Text delivered just now may resize a content-sized surface:
            // its spec is refreshed before painting, and a frame at the
            // old size is held for the configure at the new one (the
            // surface manager arms the deadline; the change reaches it
            // with the next sync, which the text worker's waker runs).
            let before = self.surfaces.get(&surface).and_then(|s| s.size_hold);
            self.refresh_specs();
            let now = Instant::now();
            if let Some(s) = self.surfaces.get(&surface)
                && s.size_hold != before
                && s.size_hold.is_some_and(|(_, t)| now < t)
            {
                return Damage::new();
            }
        }
        // Scrolls move to this frame's offsets (lists.rs).
        self.advance_scrolls(surface, target.time);
        // Springs sample this frame's time; a scene flattened earlier
        // (by `update`) is stale while anything moves.
        let swapping = self.swap_moving(surface) || self.scrolling(surface);
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        let root = s.root;
        // A scene flattened by `update` is drawn at rest: not while
        // anything moves, nor while a motion waits to start (an `enter`
        // pose a time-zero preview could not play: a surface just
        // attached, a node just created, a surface just opened whose
        // `morph` names may start: cross-surface-morph).
        let pending = self.anim.busy(&self.tree, root) || self.anim.opening(root) || swapping;
        // A node reading time repaints every frame of its clock.
        let ticking = self.clocks.running(surface);
        // (growth) Nor while a step it may take waits for this frame.
        // (wave-cost) Nor over a size set since, which only a layout shows
        // (`SurfaceState::mark_sized` keeps the scene for `kept_scene`).
        let cached = if s.animating || pending || ticking || s.step_due || s.anim_layout {
            None
        } else {
            // A scene with time-bound nodes was evaluated at another
            // frame's time (or a preview's): never reused.
            s.cache.take().filter(|f| f.clocks.is_empty())
        };
        let prev = s.painted_time;
        let fresh = cached.is_none();
        if fresh {
            // Palette roots in flight take this frame's values.
            self.sample_tokens(target.time, true);
        }
        self.anim.begin(target.time, prev, true);
        let now = self.now();
        self.anim.date_hidden(now);
        // (eval-rules-3) The palette roots' samples join the frame's.
        #[cfg(feature = "eval-hooks")]
        if fresh {
            let traced = self.take_palette_traces(root);
            self.anim.springs.extend(traced);
        }
        let flatten_start = Instant::now();
        let waited = self.estimate_waited;
        let laid = self.layout_spent;
        // (wave-cost) No box moved: the last frame's scene.
        let kept = match (&cached, swapping || ticking) {
            (None, false) => self.kept_scene(surface),
            _ => None,
        };
        let held = kept.is_some();
        let mut f = match cached.or(kept) {
            Some(f) => f,
            None => self.flatten_surface(surface),
        };
        // (shared-morph-on) Where each `morph` name is on screen now, the
        // scene flattened for this frame or reused: a node at rest on a
        // surface that only reuses scenes is still a morph's start.
        self.anim.shared_painted(root, target.time, &f.morphs);
        self.anim.surface_shown(root);
        self.was_shown.insert(root);
        self.paint_times = PaintTimes {
            flatten: flatten_start.elapsed(),
            text_wait: self.estimate_waited.saturating_sub(waited),
            layout: self.layout_spent.saturating_sub(laid),
            ..PaintTimes::default()
        };
        // (M4) Its passes (drawn with what they have).
        #[cfg(feature = "gpu")]
        self.gpu_passes(surface, &f.items, &f.passes, false);
        if fresh {
            // Its clocks run while it draws them: not frozen (reduced
            // motion, a frame with no clock), and not after it detached.
            let frozen = self.anim.reduced() || target.time.is_zero();
            let clocks: &[crate::clock::Clock] = if frozen { &[] } else { &f.clocks };
            self.clocks.drawn(surface, clocks);
            let surfaces = &self.surfaces;
            self.clocks.retain(|s| surfaces.contains_key(&s));
        }
        // A theme crossfade on this surface paints every frame in full,
        // over the old frame taken before the first one is drawn.
        let snapping = Instant::now();
        self.take_snapshot(surface, target);
        self.paint_times.blend = snapping.elapsed();
        let fade = self.fade_frame(surface, target.time, target.size);
        // A cached scene was drawn at rest.
        let animating =
            fresh && (self.anim.active() || self.swap_moving(surface) || self.scrolling(surface));
        // Every painted frame, cached or not, is checked for a list
        // showing a gap or held at its mounted rows.
        self.check_list_gaps(surface);
        // (M4) Clocked passes a GPU-drawn surface runs in every frame.
        #[cfg(feature = "gpu")]
        let clocked = self.gpu_clocked_damage(surface, &f.items, &f.passes);
        if fresh && !held {
            // Exits under this surface it did not draw (a row scrolled
            // out of view) end: nobody sees them, unless another surface
            // showing the same root drew it in its last frame.
            if let Some(s) = self.surfaces.get_mut(&surface) {
                s.drawn = self.anim.drawn().clone();
            }
            let tree = &self.tree;
            let surfaces = &self.surfaces;
            let (now, stall) = (Instant::now(), self.exit_stall);
            // Drawn there: reached by its last frame (a record), or
            // sampled by it (an exit faded to nothing has no record).
            // Only a surface still getting frames counts: an output
            // asleep (occluded, DPMS off) never samples its motions, so
            // its stale frame would keep the exit from ending.
            let elsewhere = |id: NodeId| {
                surfaces.iter().any(|(sid, o)| {
                    *sid != surface
                        && o.root == root
                        && o.painted_at
                            .is_some_and(|t| now.saturating_duration_since(t) < stall)
                        && (o.drawn.contains(&id) || o.records.contains_key(&id))
                })
            };
            let unseen = |id: NodeId| tree.root_of(id) == Some(root) && !elsewhere(id);
            self.anim.finish_undrawn(unseen);
            self.anim.drop_undrawn_enters(unseen);
        }
        #[cfg(feature = "gpu")]
        let presenting =
            self.quiet_frames && self.backend(surface) == strand_scene::Backend::GpuPresent;
        #[cfg(not(feature = "gpu"))]
        let presenting = false;
        #[cfg(all(feature = "eval-hooks", feature = "gpu"))]
        let probe_backend = self.backend(surface);
        #[cfg(all(feature = "eval-hooks", not(feature = "gpu")))]
        let probe_backend = strand_scene::Backend::Cpu;
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        s.animating = animating;
        let bounds = target.bounds();
        // A frame blended with a crossfade's snapshot may be translucent
        // where the new one alone is opaque: it claims nothing.
        s.opaque = if matches!(fade, super::swap::FadeFrame::Blend(_)) {
            Damage::new()
        } else {
            f.opaque
        };
        s.dirty = false;

        // This frame's changes.
        let mut frame = Damage::new();
        let diffing = Instant::now();
        if s.valid && !fade.full() {
            diff_records(&s.records, &f.records, &mut frame);
            order_damage((&s.order, &s.records), (&f.order, &f.records), &mut frame);
            frame.clip(bounds);
            // (eval-sandbox) The `redraw` check's mutant: damage that
            // misses a band around what changed.
            #[cfg(feature = "eval-hooks")]
            if self.probe.damage_inset > 0 {
                frame = inset(&frame, self.probe.damage_inset);
            }
        } else {
            frame = Damage::full(target.size);
        }
        self.paint_times.diff = diffing.elapsed();
        #[cfg(feature = "gpu")]
        frame.union_clipped(&clocked, bounds);
        // (M5) The inspector's outline and repaint flash, drawn after the
        // frame's own items: where they changed is repainted too, and
        // only the frame's own repaint (`frame` so far) is tinted.
        let lock = self
            .tree
            .get(root)
            .is_some_and(|n| n.kind == strand_scene::NodeKind::Lock);
        let overlay =
            self.inspect
                .overlay(surface, &f.hits, lock, &frame, target.time, s.scale, bounds);
        #[cfg(feature = "gpu")]
        let inspected = overlay.as_ref().is_some_and(|o| !o.damage.is_empty());
        if let Some(o) = &overlay {
            frame.union_clipped(&o.damage, bounds);
        }
        // (M4) Promotion's input, and whether a GPU-drawn surface must
        // send this frame.
        #[cfg(feature = "gpu")]
        let stats = (
            frame.area(),
            animating,
            animating
                || inspected
                || !clocked.is_empty()
                || s.records != f.records
                || s.order != f.order,
        );
        s.records = f.records.clone();
        s.order.clone_from(&f.order);
        // (still-pointer) Hit boxes that moved under a pointer on this
        // surface: the frame's reaction routes it again. Compared in place
        // of the copy that equal boxes no longer need.
        if s.hits != f.hits {
            s.hits_moved |= self.extras.pointers.contains_key(&s.root);
            s.hits.clone_from(&f.hits);
        }
        // (eval-sandbox) Every paint, empty damage included.
        #[cfg(feature = "eval-hooks")]
        self.probe.frame(
            surface,
            target.time,
            probe_backend,
            &f,
            fresh && !held,
            frame.area(),
            self.layout_passes.saturating_sub(probe_passes),
            s.boxes.as_ref().map_or(0, |b| b.rects.len()),
            self.pending.len(),
            self.paint_times,
            fade.weight(),
            &self
                .scrolls
                .iter()
                .filter(|(_, st)| st.moving())
                .map(|(id, st)| (*id, st.flinging()))
                .collect::<Vec<_>>(),
            self.anim.period(),
            std::mem::take(&mut self.anim.springs),
        );

        // Widen by the buffer's age: it misses the last `age - 1` frames.
        let age = target.age as usize;
        let mut total = frame;
        // (wave-cost) A presented frame has no buffer age: every frame is
        // whole, and one where nothing changed is not sent. (A host that
        // does not skip frames asks for each as it always did.)
        if presenting && s.valid {
            // As it is.
        } else if !s.valid || age == 0 || age > s.history.len() + 1 {
            total = Damage::full(target.size);
        } else {
            for d in s.history.iter().take(age - 1) {
                total.union(d);
            }
        }
        total.clip(bounds);
        // (launcher-flip-pass) What this frame laid out is drawn now.
        s.drawn_boxes = None;
        s.flip_shifts = None;
        // What this frame laid out is what the surface shows now, but
        // for what it drew nothing of (late-text-dip): a text still on
        // its way, and a box laid out empty because such a text is under
        // it (it takes no room). Neither has a place it was drawn to
        // glide from when the text arrives and lays them out whole. A
        // box laid out empty for any other reason, and a node no longer
        // laid out, leave the set.
        if !s.unpainted.is_empty() {
            let shaping = super::text::still_shaping(&self.texts, &s.wanted);
            let mut waiting: HashSet<NodeId> = HashSet::new();
            for t in &shaping {
                let mut at = self.tree.get(*t).and_then(|n| n.parent);
                while let Some(p) = at {
                    if !waiting.insert(p) {
                        break;
                    }
                    at = self.tree.get(p).and_then(|n| n.parent);
                }
            }
            let boxes = s.boxes.as_ref();
            s.unpainted.retain(|n| {
                shaping.contains(n)
                    || (waiting.contains(n)
                        && boxes
                            .and_then(|b| b.rects.get(n))
                            .is_some_and(|r| r.w <= 0.0 || r.h <= 0.0))
            });
        }
        if total.is_empty() {
            // Nothing drawn: no frame, so nothing enters the history and
            // the caller does not commit (see `Painter::paint`).
            // (eval-sandbox) The buffer must still show this scene.
            #[cfg(feature = "eval-hooks")]
            self.probe
                .keep_frame(surface, &f, target.size, target.scale, fade.weight());
            s.cache = Some(f);
            self.last_damage.insert(surface, total);
            #[cfg(feature = "gpu")]
            self.gpu_frame_stats(surface, stats.0, stats.1);
            return total;
        }

        s.history.push_front(frame);
        s.history.truncate(DAMAGE_HISTORY);
        s.painted_time = Some(target.time);
        if !target.time.is_zero() {
            // Shown with a clock: it is no longer opening.
            self.opening.remove(&root);
        }
        s.valid = true;
        s.painted = true;
        // (eval-coverage) The change's FLIP scope was drawn: it closes
        // now if its texts are shaped, or at the flatten they are.
        s.flip_drawn = true;
        if s.flip_settled {
            s.flip_open = None;
        }
        s.query_hold = None;
        s.query_held = false;
        let scale = s.scale;
        // The inspector's items join the frame's for drawing only: the
        // flattened scene kept for the next frame is the frame's own.
        let own = f.items.len();
        if let Some(o) = overlay {
            f.items.extend(o.items);
        }
        // (M4) A promoted surface: its pixels come from the GPU (or the
        // CPU draws it in full until they do). A presented frame is
        // lowered, crossfade included, so nothing is drawn into `target`.
        #[cfg(feature = "gpu")]
        let (total, drawn, lowered) = match self.backend(surface) {
            strand_scene::Backend::GpuReadback => {
                match self.gpu_readback_paint(surface, &f.items, &f.passes, stats.2, target) {
                    Some(d) => (d, true, false),
                    None => (Damage::full(target.size), false, false),
                }
            }
            strand_scene::Backend::GpuPresent => {
                let w = match fade {
                    super::swap::FadeFrame::Blend(w) => Some(w),
                    _ => None,
                };
                (
                    self.gpu_present_paint(surface, &f.items, &f.passes, target.size, scale, w),
                    true,
                    true,
                )
            }
            strand_scene::Backend::Cpu => (total, false, false),
        };
        #[cfg(not(feature = "gpu"))]
        let (drawn, lowered) = (false, false);
        let raster_start = Instant::now();
        if !drawn {
            self.raster
                .paint(&f.items, &total, &self.atlas, scale, target);
            self.paint_times.raster_parts = self.raster.times();
        }
        self.paint_times.raster = raster_start.elapsed();
        #[cfg(feature = "eval-hooks")]
        if !drawn {
            let px = self.raster.rasterised();
            self.probe.rastered(surface, px, self.paint_times);
        }
        if let super::swap::FadeFrame::Blend(w) = fade
            && !lowered
        {
            let blending = Instant::now();
            self.blend_fade(surface, w, target);
            self.paint_times.blend += blending.elapsed();
        }
        // (eval-sandbox) Kept as drawn: the inspector's overlay
        // included, which the buffer shows too.
        #[cfg(feature = "eval-hooks")]
        self.probe
            .keep_frame(surface, &f, target.size, target.scale, fade.weight());
        f.items.truncate(own);
        // Nothing changed since flattening: the next paint can reuse it.
        // Stamped once the frame is drawn: a slow raster does not age the
        // surface into looking idle.
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.cache = Some(f);
            s.painted_at = Some(Instant::now());
        }
        self.last_damage.insert(surface, total);
        #[cfg(feature = "gpu")]
        self.gpu_frame_stats(surface, stats.0, stats.1);
        total
    }

    /// Other surfaces' first-frame and size holds wait for the text
    /// worker and for the compositor's configure, which the loop cannot
    /// take while it paints: a hold still running when this paint of
    /// `painted` started is moved on by the time it took. Under load a
    /// first frame painting another surface for longer than the hold
    /// (half a second for a debug build's launcher beside a bar) showed
    /// at its estimated size, with the configure at its real one already
    /// on its way. A hold is moved on by at most [`MAX_HOLD_PAUSE`] in
    /// all: one whose configure never matches (a compositor clamping the
    /// size) still runs out while other surfaces paint back to back.
    pub(super) fn pause_holds(&mut self, painted: SurfaceId, started: Instant) {
        let spent = started.elapsed();
        let on = |t: &mut Instant, paused: &mut Duration| {
            if *t > started {
                let by = spent.min(MAX_HOLD_PAUSE.saturating_sub(*paused));
                *t += by;
                *paused += by;
            }
        };
        for (id, s) in &mut self.surfaces {
            if *id == painted {
                continue;
            }
            if !s.painted
                && let Some(t) = &mut s.wait_until
            {
                on(t, &mut s.wait_paused);
            }
            if let Some((_, t)) = &mut s.size_hold {
                on(t, &mut s.size_paused);
            }
        }
    }

    /// (wave-cost) Whether frames that cannot show anything are skipped:
    /// not asked for from the compositor at all, the loop woken only by the
    /// next that can ([`Renderer::frame_deadline`], so
    /// [`Painter::wants_frame`] is false until then). For a host whose loop
    /// keeps real time; a host that paints while `wants_frame` holds, on its
    /// own clock (a test, a replay), draws every frame. Every frame painted
    /// shows the motion exactly where it is at that frame's time.
    pub fn set_quiet_frames(&mut self, on: bool) {
        // (eval-sandbox) Eval's checks read every frame.
        self.quiet_frames = on && !cfg!(feature = "eval-hooks");
    }

    /// (wave-cost) Decides, after a paint of `surface` at `time`, when it
    /// next needs a frame if that is not the next one. Only size springs
    /// move there, and a box lands only on a whole logical pixel: a spring
    /// that will not have moved [`SIZE_STEP`] by the next frame can show at
    /// most that fraction of a pixel's travel late. So a slow spring costs
    /// frames by how far it moves, not by the refresh rate of each monitor
    /// showing it, and one that moves that far each frame is drawn at
    /// refresh as ever.
    ///
    /// [`SIZE_STEP`]: crate::anim::SIZE_STEP
    fn plan_quiet(&mut self, surface: SurfaceId, time: Duration, empty: bool) {
        if !self.quiet_frames || time.is_zero() {
            return;
        }
        let Some(s) = self.surfaces.get(&surface) else {
            return;
        };
        if !s.animating || s.dirty {
            return;
        }
        let root = s.root;
        let period = self.clocks.period(surface);
        let alone = !s.layout_dirty
            && !s.step_due
            && s.cache
                .as_ref()
                .is_some_and(|f| !f.moving && f.clocks.is_empty())
            && !self.clocks.running(surface)
            && !self.swap_moving(surface)
            && !self.scrolling(surface)
            && self.anim.sizes_alone(&self.tree, root);
        let frames = if alone {
            let most = (QUIET_MOST.as_nanos() / period.as_nanos().max(1)).min(u128::from(u32::MAX));
            self.anim
                .frames_unmoved(&self.tree, root, time, period, most as u32)
        } else {
            1
        };
        // (M4) A presented surface's frame that drew nothing sent nothing,
        // so no reply wakes the loop for the next one.
        #[cfg(feature = "gpu")]
        let wake = empty && self.backend(surface) == strand_scene::Backend::GpuPresent;
        #[cfg(not(feature = "gpu"))]
        let wake = {
            let _ = empty;
            false
        };
        if frames < 2 && !wake {
            return;
        }
        let at = self.clocks.instant_of(surface, time + period * frames);
        // Deadlines close together wake the loop once.
        let at = at.map(|t| {
            let since = t.saturating_duration_since(self.quiet_epoch).as_nanos() as u64;
            let grid = QUIET_GRID.as_nanos() as u64;
            t + Duration::from_nanos((grid - since % grid) % grid)
        });
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.quiet_until = at;
            s.quiet_gap = frames >= 2;
        }
    }

    pub(super) fn wants(&self, surface: SurfaceId) -> bool {
        // Dirty or never painted. Text still being shaped does not count:
        // its delivery marks the surface dirty (the worker's waker makes
        // the loop call `update`), so waiting costs no frames. A surface
        // not painted yet holds its first frame for its text, up to its
        // deadline.
        self.frame_deadline(surface).is_none()
            && self.surfaces.get(&surface).is_some_and(|s| {
                s.dirty
                    || !s.valid
                    || self.pointer_due(surface)
                    || s.animating
                    || self.clocks.wants(surface)
                    || self.inspect.wants(surface)
            })
    }
}

/// (eval-sandbox) `d` with each rect shrunk by `px` on every side (those
/// it empties dropped): `Renderer::set_damage_inset`'s mutant.
#[cfg(feature = "eval-hooks")]
fn inset(d: &Damage, px: u32) -> Damage {
    let n = i64::from(px);
    let mut out = Damage::new();
    for r in d.rects() {
        let r = Rect::from_edges(r.left() + n, r.top() + n, r.right() - n, r.bottom() - n);
        if !r.is_empty() {
            out.add(r);
        }
    }
    out
}

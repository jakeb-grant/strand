//! Frames: when a surface wants one or holds it (for text, a container
//! query, a configure), damage between frames' node records, and
//! [`Painter`](strand_scene::Painter) itself.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use strand_scene::{Damage, NodeId, PaintTarget, Painter, SurfaceId};

use super::{DAMAGE_HISTORY, Renderer};
use crate::flatten::NodeRecord;

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

    /// The damage the last `paint` returned for `surface`.
    pub fn last_damage(&self, surface: SurfaceId) -> Option<Damage> {
        self.last_damage.get(&surface).copied()
    }
}

/// Above this many changed glyph cells (old or new), the change is
/// damaged as the box around them, not cell by cell.
pub(super) const GLYPH_CELLS_COMPARED: usize = 64;

/// Damage for a text whose glyphs changed from `a` to `b` (cells in
/// layout order): the cells that differ, where they were and where they
/// are. The common prefix and suffix are skipped (a clock tick, an edit
/// at the end of a long body), so the cost is linear in the glyphs; a
/// middle longer than [`GLYPH_CELLS_COMPARED`] is damaged as one box per
/// side.
pub(super) fn glyph_damage(
    a: &[(strand_scene::Rect, u64)],
    b: &[(strand_scene::Rect, u64)],
    d: &mut Damage,
) {
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
                .map(|c| c.0)
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
            d.add(c.0);
        }
    }
    for c in b {
        if !a.contains(c) {
            d.add(c.0);
        }
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
            // Only its glyphs changed: the glyphs that differ, where
            // they were and where they are.
            Some(o) => match (&o.glyphs, &n.glyphs) {
                (Some(a), Some(b)) if a.rest == b.rest => glyph_damage(&a.cells, &b.cells, d),
                _ => {
                    d.add(o.bounds);
                    d.add(n.bounds);
                }
            },
            None => d.add(n.bounds),
        }
    }
    for (id, o) in old {
        if !new.contains_key(id) {
            d.add(o.bounds);
        }
    }
}

impl Painter for Renderer {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let burst = *self.burst.get_or_insert_with(Instant::now);
        let damage = self.paint_frame(surface, target);
        // Exits the frame finished: ghosts unmount (their siblings glide
        // into the gap from the next frame), closing surfaces close, and
        // a surface that settled shrinks: the specs are refreshed here so
        // the host sees the change at once.
        self.process_finished();
        self.release_holds();
        self.arm_timer();
        // The frame loop stops (no surface wants another frame, and no
        // capped clock waits on a wake for its next tick): the paint
        // cache's entries none of its frames used are idle, and go now
        // rather than at a wake of their own (design.md: "freed when
        // idle"; the M0 gate: no wakeup between ticks). Between a capped
        // clock's ticks the loop has not stopped, so nothing the tick
        // left alone is freed (`IDLE_FREE` still applies).
        if !self.surfaces.keys().any(|s| self.wants(*s)) && self.clocks.next_wake().is_none() {
            self.raster.trim_unused_since(burst);
            self.burst = None;
        }
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.wants(surface)
    }

    fn blur_region(&self, surface: SurfaceId) -> Vec<strand_scene::BlurRegion> {
        self.surfaces
            .get(&surface)
            .and_then(|s| s.cache.as_ref())
            .map(|f| f.blur.clone())
            .unwrap_or_default()
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.surfaces
            .get(&surface)
            .map_or_else(Damage::new, |s| s.opaque)
    }
}

impl Renderer {
    pub(super) fn paint_frame(
        &mut self,
        surface: SurfaceId,
        target: &mut PaintTarget<'_>,
    ) -> Damage {
        if target.validate().is_err() {
            return Damage::new();
        }
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        s.time = target.time;
        self.clocks.frame(surface, target.time, Instant::now());
        self.anim.set_slack(self.clocks.slack(surface));
        self.glide_origin(surface, target.size, target.scale);
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
        // attached, a node just created).
        let pending = self.anim.busy(&self.tree, root) || swapping;
        // A node reading time repaints every frame of its clock.
        let ticking = self.clocks.running(surface);
        let cached = if s.animating || pending || ticking {
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
        let f = match cached {
            Some(f) => f,
            None => self.flatten_surface(surface),
        };
        // (M4) Its `shader` nodes' passes (drawn with what they have).
        #[cfg(feature = "gpu")]
        self.gpu_passes(surface, &f.passes, false);
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
        self.take_snapshot(surface, target);
        let fade = self.fade_frame(surface, target.time, target.size);
        // A cached scene was drawn at rest.
        let animating =
            fresh && (self.anim.active() || self.swap_moving(surface) || self.scrolling(surface));
        // Every painted frame, cached or not, is checked for a list
        // showing a gap or held at its mounted rows.
        self.check_list_gaps(surface);
        if fresh {
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
        if s.valid && !fade.full() {
            diff_records(&s.records, &f.records, &mut frame);
            frame.clip(bounds);
        } else {
            frame = Damage::full(target.size);
        }
        // (M4) Promotion's input, and whether a GPU-drawn surface must
        // send this frame.
        #[cfg(feature = "gpu")]
        let stats = (frame.area(), animating, animating || s.records != f.records);
        s.records = f.records.clone();
        s.hits = f.hits.clone();

        // Widen by the buffer's age: it misses the last `age - 1` frames.
        let age = target.age as usize;
        let mut total = frame;
        if !s.valid || age == 0 || age > s.history.len() + 1 {
            total = Damage::full(target.size);
        } else {
            for d in s.history.iter().take(age - 1) {
                total.union(d);
            }
        }
        total.clip(bounds);
        if total.is_empty() {
            // Nothing drawn: no frame, so nothing enters the history and
            // the caller does not commit (see `Painter::paint`).
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
        s.query_hold = None;
        s.query_held = false;
        let scale = s.scale;
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
        if !drawn {
            self.raster
                .paint(&f.items, &total, &self.atlas, scale, target);
        }
        if let super::swap::FadeFrame::Blend(w) = fade
            && !lowered
        {
            self.blend_fade(surface, w, target);
        }
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

    pub(super) fn wants(&self, surface: SurfaceId) -> bool {
        // Dirty or never painted. Text still being shaped does not count:
        // its delivery marks the surface dirty (the worker's waker makes
        // the loop call `update`), so waiting costs no frames. A surface
        // not painted yet holds its first frame for its text, up to its
        // deadline.
        self.frame_deadline(surface).is_none()
            && self
                .surfaces
                .get(&surface)
                .is_some_and(|s| s.dirty || !s.valid || s.animating || self.clocks.wants(surface))
    }
}

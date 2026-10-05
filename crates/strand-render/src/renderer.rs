//! The render thread's entry point: applies scene diffs, keeps text layouts
//! flowing, diffs damage and implements [`Painter`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{
    Damage, NodeId, NodeKind, PaintTarget, Painter, Prop, Scale, SceneDiff, SceneOp, Size,
    SurfaceId,
};
use strand_text::{TextEngine, TextError, TextKey, TextLayout, TextRequest, TextWorker};

use crate::flatten::{Flattened, NodeRecord, TextSpec, flatten};
use crate::raster::{AtlasMirror, Raster};
use crate::tree::{SceneError, SceneTree};

/// How many past frames' damage is kept for buffer-age widening. A buffer
/// older than this is repainted in full.
pub const DAMAGE_HISTORY: usize = 4;

/// Where text layouts come from.
#[derive(Debug)]
pub enum TextBackend {
    /// The text worker thread (the runtime configuration).
    Worker(TextWorker),
    /// Shape synchronously on the calling thread: deterministic, for
    /// offline rendering, tests and benchmarks.
    Inline(Box<TextEngine>),
}

impl TextBackend {
    /// Withdraws a request that is no longer wanted.
    fn cancel(&self, key: TextKey) {
        if let TextBackend::Worker(w) = self {
            // A gone worker has nothing queued.
            let _ = w.cancel(key);
        }
    }

    /// Frees the glyph atlas for a scale no surface uses any more.
    fn drop_scale(&mut self, scale: Scale) {
        match self {
            TextBackend::Worker(w) => {
                let _ = w.drop_scale(scale);
            }
            TextBackend::Inline(e) => e.drop_scale(scale),
        }
    }
}

#[derive(Debug, Default)]
struct TextState {
    /// The layout being drawn (the last one delivered).
    layout: Option<Arc<TextLayout>>,
    /// What `layout` was shaped from.
    shaped: Option<TextSpec>,
    /// The request in flight, if any.
    requested: Option<(TextKey, TextSpec)>,
}

#[derive(Debug)]
struct SurfaceState {
    root: NodeId,
    size: Size,
    scale: Scale,
    /// The scale this surface showed before its last rescale. Its text
    /// layouts are kept (drawn resampled) until every text on the surface
    /// has a layout at the new scale; then it is pruned.
    prev_scale: Option<Scale>,
    records: BTreeMap<NodeId, NodeRecord>,
    /// Damage of the most recent frames, newest first.
    history: VecDeque<Damage>,
    /// False until the first paint, and after size or scale changes.
    valid: bool,
    dirty: bool,
    /// Fully opaque part of the last painted frame.
    opaque: Damage,
    /// Presentation time of the last painted frame (springs sample it
    /// from M2).
    time: Duration,
    /// The flattened scene for the current state, reused by `paint` after
    /// `update`; `None` once anything it depends on changes.
    cache: Option<Flattened>,
}

impl SurfaceState {
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.cache = None;
    }

    /// Takes a new size and scale; returns true if the scale changed.
    fn resize(&mut self, size: Size, scale: Scale) -> bool {
        if self.size == size && self.scale == scale {
            return false;
        }
        let rescaled = self.scale != scale;
        if rescaled && self.size != Size::default() && self.prev_scale.is_none() {
            self.prev_scale = Some(self.scale);
        }
        self.size = size;
        self.scale = scale;
        self.valid = false;
        self.mark_dirty();
        rescaled
    }
}

/// Retained scene, damage tracking and painting for every surface.
#[derive(Debug)]
pub struct Renderer {
    tree: SceneTree,
    surfaces: BTreeMap<SurfaceId, SurfaceState>,
    text: TextBackend,
    /// Text per node and scale: a node shown on outputs of different scales
    /// keeps one layout per scale.
    texts: HashMap<(NodeId, Scale), TextState>,
    pending: HashMap<TextKey, (NodeId, Scale)>,
    next_key: u64,
    /// Scales text was requested at whose atlases may still exist.
    text_scales: BTreeSet<Scale>,
    atlas: AtlasMirror,
    raster: Raster,
    last_damage: HashMap<SurfaceId, Damage>,
}

impl Renderer {
    pub fn new(text: TextBackend) -> Self {
        Self {
            tree: SceneTree::new(),
            surfaces: BTreeMap::new(),
            text,
            texts: HashMap::new(),
            pending: HashMap::new(),
            next_key: 1,
            text_scales: BTreeSet::new(),
            atlas: AtlasMirror::default(),
            raster: Raster::default(),
            last_damage: HashMap::new(),
        }
    }

    /// The retained tree, read-only (inspector, tests).
    pub fn tree(&self) -> &SceneTree {
        &self.tree
    }

    /// Paints the subtree under `root` (a surface node) into `surface`.
    pub fn attach_surface(&mut self, surface: SurfaceId, root: NodeId) {
        self.surfaces.insert(
            surface,
            SurfaceState {
                root,
                size: Size::default(),
                scale: Scale::ONE,
                prev_scale: None,
                records: BTreeMap::new(),
                history: VecDeque::new(),
                valid: false,
                dirty: true,
                opaque: Damage::new(),
                time: Duration::ZERO,
                cache: None,
            },
        );
    }

    /// Forces a full repaint of `surface` on its next paint (for example
    /// after a painted buffer could not be committed).
    pub fn invalidate(&mut self, surface: SurfaceId) {
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.valid = false;
            s.dirty = true;
        }
    }

    /// Tells the renderer a surface's buffer size and scale before its first
    /// paint, so text can be shaped ahead of the first frame.
    pub fn configure_surface(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.resize(size, scale);
        }
        self.prune_scales();
        self.update();
    }

    pub fn detach_surface(&mut self, surface: SurfaceId) {
        self.surfaces.remove(&surface);
        self.last_damage.remove(&surface);
        self.prune_scales();
    }

    /// Frees everything held for scales no surface uses any more: text
    /// layouts, requests in flight, the atlas mirror's pages and the text
    /// worker's atlas. The worker and the mirror are dropped together, so
    /// a scale that comes back (a monitor replugged) re-rasterises and
    /// re-uploads its glyphs.
    ///
    /// A surface's previous scale counts as used until its text has been
    /// re-shaped at the new scale, so a rescale never blanks text.
    fn prune_scales(&mut self) {
        let used: BTreeSet<Scale> = self
            .surfaces
            .values()
            .flat_map(|s| [Some(s.scale), s.prev_scale])
            .flatten()
            .collect();
        let gone: Vec<Scale> = self.text_scales.difference(&used).copied().collect();
        if gone.is_empty() {
            return;
        }
        for s in self.surfaces.values_mut() {
            s.cache = None;
        }
        for scale in gone {
            self.text_scales.remove(&scale);
            self.atlas.retain_scales(|s| s != scale);
            let text = &self.text;
            self.texts.retain(|(_, s), t| {
                if *s == scale
                    && let Some((k, _)) = t.requested.take()
                {
                    text.cancel(k);
                }
                *s != scale
            });
            self.pending.retain(|_, (_, s)| *s != scale);
            self.text.drop_scale(scale);
        }
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

    /// Applies one tick's diff. Failed ops are returned; the rest apply.
    pub fn apply(&mut self, diff: SceneDiff) -> Vec<SceneError> {
        let mut errors = Vec::new();
        // Surface roots whose subtree an op touches; `None` means all.
        let mut touched: Option<Vec<NodeId>> = Some(Vec::new());
        for op in diff.ops {
            // Where the node painted before the op, and the node whose
            // root to look up after it.
            let (before, after) = match &op {
                SceneOp::Create { id, .. } => (None, Some(*id)),
                SceneOp::Remove { id } => (self.tree.root_of(*id), None),
                SceneOp::SetProp { id, .. } => (self.tree.root_of(*id), None),
                SceneOp::Move { id, .. } => (self.tree.root_of(*id), Some(*id)),
                SceneOp::SetTokens { .. } => {
                    touched = None;
                    (None, None)
                }
            };
            match self.tree.apply_op(op) {
                Ok(()) => {
                    if let Some(t) = &mut touched {
                        t.extend(before);
                        t.extend(after.and_then(|id| self.tree.root_of(id)));
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        // Drop text state of nodes that are gone or no longer show text.
        let tree = &self.tree;
        let text = &self.text;
        self.texts.retain(|(id, _), t| {
            let keep = tree.get(*id).is_some_and(|n| {
                matches!(n.kind, NodeKind::Text | NodeKind::Button) && n.get(Prop::Text).is_some()
            });
            if !keep && let Some((k, _)) = t.requested.take() {
                text.cancel(k);
            }
            keep
        });
        let texts = &self.texts;
        self.pending.retain(|_, slot| texts.contains_key(slot));
        // A surface nested in a touched one (a popup in a bar) inherits
        // from it, so it is touched too; `update` clears it again if
        // nothing it draws changed.
        let tree = &self.tree;
        for s in self.surfaces.values_mut() {
            if touched
                .as_ref()
                .is_none_or(|t| t.iter().any(|r| tree.is_ancestor(*r, s.root)))
            {
                s.mark_dirty();
            }
        }
        self.update();
        errors
    }

    /// Collects finished text layouts and sends shaping requests for text
    /// that changed. Never blocks. Call after the text worker's waker fires.
    ///
    /// A dirty surface whose flattened scene turns out identical to its
    /// last painted frame (only text still being shaped changed) stops
    /// being dirty, so it asks for no frame until the layout arrives.
    pub fn update(&mut self) {
        self.poll_text();
        let ids: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.dirty && s.cache.is_none() && s.size != Size::default())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let f = self.flatten_surface(id);
            if let Some(s) = self.surfaces.get_mut(&id) {
                if s.valid && s.records == f.records && s.opaque == f.opaque {
                    s.dirty = false;
                }
                s.cache = Some(f);
            }
        }
    }

    /// True while text requests are in flight.
    pub fn text_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Blocks until every text request in flight has been delivered or
    /// `timeout` passes (offline rendering and tests). Returns true if
    /// nothing is pending any more.
    pub fn wait_for_text(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.pending.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            let TextBackend::Worker(w) = &self.text else {
                break;
            };
            match w.recv_timeout(left) {
                Ok(Some(l)) => self.deliver(l),
                Ok(None) | Err(_) => break,
            }
            // A delivered layout can change sizes and so re-request.
            self.update();
        }
        self.pending.is_empty()
    }

    /// The damage the last `paint` returned for `surface`.
    pub fn last_damage(&self, surface: SurfaceId) -> Option<Damage> {
        self.last_damage.get(&surface).copied()
    }

    fn poll_text(&mut self) {
        loop {
            let TextBackend::Worker(w) = &self.text else {
                return;
            };
            match w.try_recv() {
                Ok(Some(l)) => self.deliver(l),
                Ok(None) => return,
                Err(TextError::WorkerGone | TextError::Spawn(_)) => {
                    // Keep drawing the last layouts; nothing more will come.
                    self.pending.clear();
                    return;
                }
            }
        }
    }

    fn deliver(&mut self, layout: TextLayout) {
        if layout.is_reset() {
            self.reset_text();
            return;
        }
        // Atlas uploads apply even when the layout itself is stale, except
        // for scales already pruned (their pages would never be freed).
        for up in &layout.uploads {
            if self.text_scales.contains(&up.page.scale) {
                self.atlas.apply(up);
            }
        }
        let Some(slot) = self.pending.remove(&layout.key) else {
            return;
        };
        let Some(state) = self.texts.get_mut(&slot) else {
            return;
        };
        if state.requested.as_ref().map(|(k, _)| *k) != Some(layout.key) {
            return;
        }
        if let Some((_, spec)) = state.requested.take() {
            state.shaped = Some(spec);
            state.layout = Some(Arc::new(layout));
        }
        // Surfaces at other scales may draw it resampled meanwhile.
        let root = self.tree.root_of(slot.0);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_dirty();
            }
        }
    }

    /// The text worker restarted its engine: every mirrored page and every
    /// layout drawing from one is stale. Forget them all and re-request.
    fn reset_text(&mut self) {
        self.atlas = AtlasMirror::default();
        for (_, t) in self.texts.drain() {
            if let Some((k, _)) = t.requested {
                self.text.cancel(k);
            }
        }
        self.pending.clear();
        for s in self.surfaces.values_mut() {
            s.valid = false;
            s.mark_dirty();
        }
    }

    /// The layouts to draw at `scale`: each node's layout for that scale,
    /// or, while that is being shaped, one for another scale (resampled).
    fn layouts_for(&self, scale: Scale) -> HashMap<NodeId, Arc<TextLayout>> {
        let mut out: HashMap<NodeId, Arc<TextLayout>> = HashMap::new();
        for ((id, s), t) in &self.texts {
            let Some(l) = &t.layout else { continue };
            if *s == scale || !out.contains_key(id) {
                out.insert(*id, l.clone());
            }
        }
        out
    }

    /// Flattens a surface, issuing text requests for changed text. With the
    /// inline backend new layouts are available at once, so it flattens
    /// again until text is stable.
    fn flatten_surface(&mut self, id: SurfaceId) -> Flattened {
        let mut stable = None;
        for _ in 0..4 {
            let Some(s) = self.surfaces.get(&id) else {
                return Flattened::default();
            };
            let layouts = self.layouts_for(s.scale);
            let flat = flatten(&self.tree, s.root, s.size, s.scale, &layouts);
            if !self.request_text(&flat.text) {
                stable = Some(flat);
                break;
            }
        }
        let f = match stable {
            Some(f) => f,
            None => {
                let Some(s) = self.surfaces.get(&id) else {
                    return Flattened::default();
                };
                let layouts = self.layouts_for(s.scale);
                flatten(&self.tree, s.root, s.size, s.scale, &layouts)
            }
        };
        // Once every text here has a layout at this scale, the previous
        // scale's layouts are no longer needed as stand-ins.
        let texts = &self.texts;
        let settled = f.text.iter().all(|(node, spec)| {
            texts
                .get(&(*node, spec.scale))
                .is_some_and(|t| t.requested.is_none())
        });
        if settled
            && let Some(s) = self.surfaces.get_mut(&id)
            && s.prev_scale.take().is_some()
        {
            self.prune_scales();
        }
        f
    }

    /// Sends requests for text whose spec changed. Returns true if a layout
    /// was delivered synchronously (inline backend).
    fn request_text(&mut self, needs: &[(NodeId, TextSpec)]) -> bool {
        let mut delivered = false;
        for (node, spec) in needs {
            let slot = (*node, spec.scale);
            let state = self.texts.entry(slot).or_default();
            if state.shaped.as_ref() == Some(spec) {
                // Reverted to what is shown: a request in flight for
                // something else must not replace it on arrival.
                if let Some((old, _)) = state.requested.take() {
                    self.pending.remove(&old);
                    self.text.cancel(old);
                }
                continue;
            }
            if state.requested.as_ref().is_some_and(|(_, s)| s == spec) {
                continue;
            }
            let key = TextKey(self.next_key);
            self.next_key += 1;
            if let Some((old, _)) = state.requested.take() {
                self.pending.remove(&old);
                self.text.cancel(old);
            }
            self.text_scales.insert(spec.scale);
            state.requested = Some((key, spec.clone()));
            let req = TextRequest {
                key,
                text: spec.text.clone(),
                style: spec.style.clone(),
                max_width: spec.max_width,
                scale: spec.scale,
            };
            match &mut self.text {
                TextBackend::Worker(w) => {
                    if w.request(req).is_ok() {
                        self.pending.insert(key, slot);
                    } else if let Some(state) = self.texts.get_mut(&slot) {
                        // No worker: keep the last layout, stop asking.
                        state.shaped = Some(spec.clone());
                        state.requested = None;
                    }
                }
                TextBackend::Inline(engine) => {
                    let layout = engine.layout(&req);
                    self.pending.insert(key, slot);
                    self.deliver(layout);
                    delivered = true;
                }
            }
        }
        delivered
    }
}

/// Damage between two frames' node records: a changed node damages where it
/// was and where it is; added and removed nodes damage their one place.
fn diff_records(
    old: &BTreeMap<NodeId, NodeRecord>,
    new: &BTreeMap<NodeId, NodeRecord>,
    d: &mut Damage,
) {
    for (id, n) in new {
        match old.get(id) {
            Some(o) if o == n => {}
            Some(o) => {
                d.add(o.bounds);
                d.add(n.bounds);
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

impl Painter for Renderer {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        if target.validate().is_err() {
            return Damage::new();
        }
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        s.time = target.time;
        if s.resize(target.size, target.scale) {
            self.prune_scales();
        }
        self.poll_text();
        let cached = self.surfaces.get_mut(&surface).and_then(|s| s.cache.take());
        let f = match cached {
            Some(f) => f,
            None => self.flatten_surface(surface),
        };
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        let bounds = target.bounds();
        s.opaque = f.opaque;
        s.dirty = false;

        // This frame's changes.
        let mut frame = Damage::new();
        if s.valid {
            diff_records(&s.records, &f.records, &mut frame);
            frame.clip(bounds);
        } else {
            frame = Damage::full(target.size);
        }
        s.records = f.records.clone();

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
            return total;
        }

        s.history.push_front(frame);
        s.history.truncate(DAMAGE_HISTORY);
        s.valid = true;
        let scale = s.scale;
        self.raster
            .paint(&f.items, &total, &self.atlas, scale, target);
        // Nothing changed since flattening: the next paint can reuse it.
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.cache = Some(f);
        }
        self.last_damage.insert(surface, total);
        total
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        // Dirty or never painted. Text still being shaped does not count:
        // its delivery marks the surface dirty (the worker's waker makes
        // the loop call `update`), so waiting costs no frames.
        self.surfaces
            .get(&surface)
            .is_some_and(|s| s.dirty || !s.valid)
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.surfaces
            .get(&surface)
            .map_or_else(Damage::new, |s| s.opaque)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::{Color, PropValue};
    use strand_text::{FontConfig, test_font_path};

    fn engine() -> TextEngine {
        let data = std::fs::read(test_font_path()).unwrap();
        TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]))
    }

    fn renderer() -> Renderer {
        Renderer::new(TextBackend::Inline(Box::new(engine())))
    }

    fn paint(r: &mut Renderer, px: &mut [u8], age: u8) -> Damage {
        let mut t = PaintTarget::new(px, Size::new(80, 20), 320, Scale::ONE, age).unwrap();
        r.paint(SurfaceId(1), &mut t)
    }

    /// After the worker restarts its engine, every mirrored page is stale:
    /// the renderer drops them with all layouts, repaints in full and
    /// re-requests its text, ending up with the same pixels.
    #[test]
    fn engine_reset_drops_layouts_and_reshapes() {
        let mut r = renderer();
        let (root, txt) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let mut d = SceneDiff::new();
        d.create(root, NodeKind::Bar, None, 0)
            .set(root, Prop::Color, PropValue::Color(Color::WHITE))
            .create(txt, NodeKind::Text, Some(root), 0)
            .set(txt, Prop::Text, PropValue::Text("12:59".into()));
        assert!(r.apply(d).is_empty());
        r.attach_surface(SurfaceId(1), root);
        let mut before = vec![0u8; 80 * 20 * 4];
        paint(&mut r, &mut before, 0);
        assert!(before.iter().any(|b| *b != 0));
        assert!(!r.wants_frame(SurfaceId(1)));

        // What the worker does: a fresh engine, then the reset marker.
        r.text = TextBackend::Inline(Box::new(engine()));
        r.deliver(TextLayout::reset(TextKey(0), Scale::ONE));
        assert!(r.texts.is_empty() && r.atlas.is_empty());
        assert!(r.wants_frame(SurfaceId(1)));
        let mut after = vec![0u8; 80 * 20 * 4];
        let d = paint(&mut r, &mut after, 1);
        assert_eq!(d, Damage::full(Size::new(80, 20)));
        assert!(before == after);
    }
}

//! The render thread's entry point: applies scene diffs, keeps text layouts
//! flowing, diffs damage and implements [`Painter`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{Damage, NodeId, PaintTarget, Painter, Scale, SceneDiff, Size, SurfaceId};
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
    records: BTreeMap<NodeId, NodeRecord>,
    /// Damage of the most recent frames, newest first.
    history: VecDeque<Damage>,
    /// False until the first paint, and after size or scale changes.
    valid: bool,
    dirty: bool,
}

/// Retained scene, damage tracking and painting for every surface.
#[derive(Debug)]
pub struct Renderer {
    tree: SceneTree,
    surfaces: BTreeMap<SurfaceId, SurfaceState>,
    text: TextBackend,
    texts: HashMap<NodeId, TextState>,
    pending: HashMap<TextKey, NodeId>,
    next_key: u64,
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
                records: BTreeMap::new(),
                history: VecDeque::new(),
                valid: false,
                dirty: true,
            },
        );
    }

    /// Tells the renderer a surface's buffer size and scale before its first
    /// paint, so text can be shaped ahead of the first frame.
    pub fn configure_surface(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        if let Some(s) = self.surfaces.get_mut(&surface)
            && (s.size != size || s.scale != scale)
        {
            s.size = size;
            s.scale = scale;
            s.valid = false;
            s.dirty = true;
        }
        self.update();
    }

    pub fn detach_surface(&mut self, surface: SurfaceId) {
        self.surfaces.remove(&surface);
        self.last_damage.remove(&surface);
        let scales: Vec<Scale> = self.surfaces.values().map(|s| s.scale).collect();
        self.atlas.retain_scales(|s| scales.contains(&s));
    }

    /// Applies one tick's diff. Failed ops are returned; the rest apply.
    pub fn apply(&mut self, diff: SceneDiff) -> Vec<SceneError> {
        let errors = self.tree.apply(diff);
        let tree = &self.tree;
        self.texts.retain(|id, _| tree.contains(*id));
        self.pending.retain(|_, id| tree.contains(*id));
        for s in self.surfaces.values_mut() {
            s.dirty = true;
        }
        self.update();
        errors
    }

    /// Collects finished text layouts and sends shaping requests for text
    /// that changed. Never blocks. Call after the text worker's waker fires.
    pub fn update(&mut self) {
        self.poll_text();
        let ids: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.dirty && s.size != Size::default())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.flatten_surface(id);
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
        // Atlas uploads apply even when the layout itself is stale.
        for up in &layout.uploads {
            self.atlas.apply(up);
        }
        let Some(node) = self.pending.remove(&layout.key) else {
            return;
        };
        let Some(state) = self.texts.get_mut(&node) else {
            return;
        };
        if state.requested.as_ref().map(|(k, _)| *k) != Some(layout.key) {
            return;
        }
        if let Some((_, spec)) = state.requested.take() {
            state.shaped = Some(spec);
            state.layout = Some(Arc::new(layout));
        }
        for s in self.surfaces.values_mut() {
            s.dirty = true;
        }
    }

    /// Flattens a surface, issuing text requests for changed text. With the
    /// inline backend new layouts are available at once, so it flattens
    /// again until text is stable.
    fn flatten_surface(&mut self, id: SurfaceId) -> Flattened {
        for _ in 0..4 {
            let Some(s) = self.surfaces.get(&id) else {
                return Flattened::default();
            };
            let layouts: HashMap<NodeId, Arc<TextLayout>> = self
                .texts
                .iter()
                .filter_map(|(id, t)| Some((*id, t.layout.clone()?)))
                .collect();
            let f = flatten(&self.tree, s.root, s.size, s.scale, &layouts);
            if !self.request_text(&f.text) {
                return f;
            }
        }
        let Some(s) = self.surfaces.get(&id) else {
            return Flattened::default();
        };
        let layouts: HashMap<NodeId, Arc<TextLayout>> = self
            .texts
            .iter()
            .filter_map(|(id, t)| Some((*id, t.layout.clone()?)))
            .collect();
        flatten(&self.tree, s.root, s.size, s.scale, &layouts)
    }

    /// Sends requests for text whose spec changed. Returns true if a layout
    /// was delivered synchronously (inline backend).
    fn request_text(&mut self, needs: &[(NodeId, TextSpec)]) -> bool {
        let mut delivered = false;
        for (node, spec) in needs {
            let state = self.texts.entry(*node).or_default();
            if state.shaped.as_ref() == Some(spec)
                || state.requested.as_ref().is_some_and(|(_, s)| s == spec)
            {
                continue;
            }
            let key = TextKey(self.next_key);
            self.next_key += 1;
            if let Some((old, _)) = state.requested.take() {
                self.pending.remove(&old);
            }
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
                        self.pending.insert(key, *node);
                    } else if let Some(state) = self.texts.get_mut(node) {
                        // No worker: keep the last layout, stop asking.
                        state.shaped = Some(spec.clone());
                        state.requested = None;
                    }
                }
                TextBackend::Inline(engine) => {
                    let layout = engine.layout(&req);
                    self.pending.insert(key, *node);
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
        if s.size != target.size || s.scale != target.scale {
            s.size = target.size;
            s.scale = target.scale;
            s.valid = false;
        }
        self.poll_text();
        let f = self.flatten_surface(surface);
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        let bounds = target.bounds();
        let records: BTreeMap<NodeId, NodeRecord> = f.records.into_iter().collect();

        // This frame's changes.
        let mut frame = Damage::new();
        if s.valid {
            diff_records(&s.records, &records, &mut frame);
            frame.clip(bounds);
        } else {
            frame = Damage::full(target.size);
        }

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

        s.history.push_front(frame);
        s.history.truncate(DAMAGE_HISTORY);
        s.records = records;
        s.valid = true;
        s.dirty = false;
        let scale = s.scale;

        if !self
            .raster
            .paint(&f.items, &total, &self.atlas, scale, target)
        {
            return Damage::new();
        }
        self.last_damage.insert(surface, total);
        total
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.surfaces
            .get(&surface)
            .is_some_and(|s| s.dirty || !s.valid)
            || !self.pending.is_empty()
    }
}

//! The render thread's entry point: applies scene diffs, keeps text layouts
//! flowing, diffs damage and implements [`Painter`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{
    Damage, NodeId, NodeKind, PaintTarget, Painter, Prop, Scale, SceneDiff, SceneOp, Size,
    SurfaceChange, SurfaceId, SurfaceSpec, TokenScope,
};
use strand_text::{TextEngine, TextError, TextKey, TextLayout, TextRequest, TextWorker};

use crate::flatten::{Flattened, NodeRecord, TextSpec, flatten, scope_tables};
use crate::raster::{AtlasMirror, Raster};
use crate::tree::{SceneError, SceneTree};

/// How many past frames' damage is kept for buffer-age widening. A buffer
/// older than this is repainted in full.
pub const DAMAGE_HISTORY: usize = 4;

/// How long a newly configured surface waits for its text before its first
/// frame (see [`Renderer::frame_deadline`]).
pub const FIRST_FRAME_TEXT_WAIT: Duration = Duration::from_millis(50);

/// How often a layout that came back incomplete (no atlas room) is asked
/// for again before waiting for other text to change or go.
pub const MAX_TEXT_RETRIES: u8 = 2;

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
    /// Shaping `shaped` crashed the text worker's engine: it is not asked
    /// for again (nothing is drawn) until the spec changes.
    poisoned: bool,
    /// Retries left for an incomplete layout are `MAX_TEXT_RETRIES -
    /// retries`.
    retries: u8,
}

impl TextState {
    /// The layout lacks glyphs for want of atlas room.
    fn incomplete(&self) -> bool {
        self.layout.as_ref().is_some_and(|l| l.is_incomplete())
    }
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
    /// Painted at least once since it was attached.
    painted: bool,
    /// Until when the first frame waits for text being shaped (set when
    /// the surface is first configured).
    wait_until: Option<Instant>,
    /// Some text on the surface has no layout at any scale yet.
    awaiting_text: bool,
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
    /// Resolved spec of every surface-kind node, as of the last `apply`.
    specs: BTreeMap<NodeId, SurfaceSpec>,
    surface_changes: Vec<(NodeId, SurfaceChange)>,
    first_frame_wait: Duration,
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
            specs: BTreeMap::new(),
            surface_changes: Vec::new(),
            first_frame_wait: FIRST_FRAME_TEXT_WAIT,
        }
    }

    /// How long a newly configured surface holds its first frame for text
    /// still being shaped ([`FIRST_FRAME_TEXT_WAIT`] by default).
    pub fn set_first_frame_wait(&mut self, wait: Duration) {
        self.first_frame_wait = wait;
    }

    /// The resolved surface parameters of a surface-kind node (as of the
    /// last [`Renderer::apply`]): what `strand-surface` creates its layer
    /// surface from.
    pub fn surface_spec(&self, node: NodeId) -> Option<&SurfaceSpec> {
        self.specs.get(&node)
    }

    /// Surface-kind nodes created, changed or removed since the last call,
    /// in order. Token changes that move a resolved value (`margin:
    /// $space.2`) are reported as updates.
    pub fn take_surface_changes(&mut self) -> Vec<(NodeId, SurfaceChange)> {
        std::mem::take(&mut self.surface_changes)
    }

    /// Bytes of atlas pixels the render thread mirrors for `scale`.
    pub fn atlas_mirror_bytes(&self, scale: Scale) -> usize {
        self.atlas.bytes(scale)
    }

    /// Re-resolves every surface spec and records what changed.
    fn refresh_specs(&mut self) {
        let tree = &self.tree;
        let mut live = BTreeSet::new();
        for id in tree.surface_nodes() {
            let Some(node) = tree.get(id) else { continue };
            let tables = scope_tables(tree, id);
            let scope = TokenScope::new(&tables);
            let spec =
                SurfaceSpec::resolve(node.kind, |p| node.get(p).and_then(|v| scope.resolve(v)));
            live.insert(id);
            let change = match self.specs.get(&id) {
                None => Some(SurfaceChange::Created(spec.clone())),
                Some(old) if *old != spec => Some(SurfaceChange::Updated {
                    recreate: old.needs_recreate(&spec),
                    spec: spec.clone(),
                }),
                Some(_) => None,
            };
            if let Some(c) = change {
                self.surface_changes.push((id, c));
                self.specs.insert(id, spec);
            }
        }
        let gone: Vec<NodeId> = self
            .specs
            .keys()
            .filter(|id| !live.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.specs.remove(&id);
            self.surface_changes.push((id, SurfaceChange::Removed));
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
                painted: false,
                wait_until: None,
                awaiting_text: false,
                dirty: true,
                opaque: Damage::new(),
                time: Duration::ZERO,
                cache: None,
            },
        );
        self.raster.set_surfaces(self.surfaces.len());
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
    ///
    /// Until its first paint, a surface whose text is still being shaped
    /// does not ask for a frame (no frame without its text at boot or on
    /// hotplug) for up to the first-frame wait; see
    /// [`Renderer::frame_deadline`].
    pub fn configure_surface(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        let wait = self.first_frame_wait;
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.resize(size, scale);
            if !s.painted && s.wait_until.is_none() && size != Size::default() {
                s.wait_until = Some(Instant::now() + wait);
            }
        }
        self.prune_scales();
        self.update();
    }

    pub fn detach_surface(&mut self, surface: SurfaceId) {
        self.surfaces.remove(&surface);
        self.last_damage.remove(&surface);
        self.raster.set_surfaces(self.surfaces.len());
        self.prune_scales();
    }

    /// When a surface that is holding its first frame for text will want
    /// it anyway: the loop should wake by then (a calloop timer) and check
    /// [`Painter::wants_frame`] again. `None` when it is not waiting.
    pub fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        let s = self.surfaces.get(&surface)?;
        (!s.painted && s.awaiting_text)
            .then_some(s.wait_until)
            .flatten()
            .filter(|t| Instant::now() < *t)
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
        let before = self.texts.len();
        self.texts.retain(|(id, _), t| {
            let keep = tree.get(*id).is_some_and(|n| {
                matches!(n.kind, NodeKind::Text | NodeKind::Button) && n.get(Prop::Text).is_some()
            });
            if !keep && let Some((k, _)) = t.requested.take() {
                text.cancel(k);
            }
            keep
        });
        if self.texts.len() != before {
            // Dropped layouts may have freed atlas pages.
            self.refresh_retries();
        }
        let texts = &self.texts;
        self.pending.retain(|_, slot| texts.contains_key(slot));
        // A surface nested in a touched one (a popup in a bar) inherits
        // from it, so it is touched too; `update` clears it again if
        // nothing it draws changed.
        // A surface whose root went with an ancestor (a popup in a removed
        // bar) is touched too: its stale frame clears until it is
        // detached.
        let tree = &self.tree;
        for s in self.surfaces.values_mut() {
            if !tree.contains(s.root)
                || touched
                    .as_ref()
                    .is_none_or(|t| t.iter().any(|r| tree.is_ancestor(*r, s.root)))
            {
                s.mark_dirty();
            }
        }
        self.refresh_specs();
        self.update();
        errors
    }

    /// Gives every incomplete layout its retries back and wakes its
    /// surfaces, after something happened that can free atlas pages (a
    /// layout replaced for new text, text removed, a scale dropped).
    /// Retries themselves never refresh, so this cannot loop.
    fn refresh_retries(&mut self) {
        let mut roots = BTreeSet::new();
        for ((id, _), t) in self.texts.iter_mut() {
            if t.incomplete() {
                t.retries = 0;
                roots.extend(self.tree.root_of(*id));
            }
        }
        for s in self.surfaces.values_mut() {
            if roots.contains(&s.root) {
                s.mark_dirty();
            }
        }
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
            // Text with a request in flight and no layout at any scale.
            let mut shown = HashSet::new();
            let mut asked = HashSet::new();
            for ((node, _), t) in &self.texts {
                if t.layout.is_some() {
                    shown.insert(*node);
                } else if t.requested.is_some() {
                    asked.insert(*node);
                }
            }
            let awaiting = f
                .text
                .iter()
                .any(|(node, _)| asked.contains(node) && !shown.contains(node));
            if let Some(s) = self.surfaces.get_mut(&id) {
                if s.valid && s.records == f.records && s.opaque == f.opaque {
                    s.dirty = false;
                }
                s.awaiting_text = awaiting;
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
            // The request answered by the reset crashed the engine: keep
            // it from being asked for again, or the worker would restart
            // its engine and every text would reshape, forever.
            let culprit = self.pending.get(&layout.key).and_then(|slot| {
                let (_, spec) = self.texts.get(slot)?.requested.as_ref()?;
                Some((*slot, spec.clone()))
            });
            self.reset_text();
            if let Some((slot, spec)) = culprit {
                self.texts.insert(
                    slot,
                    TextState {
                        shaped: Some(spec),
                        poisoned: true,
                        ..TextState::default()
                    },
                );
            }
            return;
        }
        // Atlas uploads apply even when the layout itself is stale, except
        // for scales already pruned (their pages would never be freed).
        if self.text_scales.contains(&layout.scale) {
            for up in &layout.uploads {
                if up.page.scale == layout.scale {
                    self.atlas.apply(up);
                }
            }
            // Pages the worker trimmed or reset since are dropped.
            if let Some(live) = layout.atlas_pages() {
                self.atlas.retain_pages(layout.scale, live);
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
        let mut replaced = false;
        if let Some((_, spec)) = state.requested.take() {
            // A retry (same spec) does not refresh anyone's retries.
            replaced = state.shaped.as_ref() != Some(&spec);
            if replaced {
                state.retries = 0;
            }
            state.shaped = Some(spec);
            state.poisoned = false;
            state.layout = Some(Arc::new(layout));
        }
        if replaced {
            // The layout it replaced released its atlas pages.
            self.refresh_retries();
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
        let text = &self.text;
        self.texts.retain(|_, t| {
            if let Some((k, _)) = t.requested.take() {
                text.cancel(k);
            }
            // Earlier culprits stay poisoned.
            t.layout = None;
            t.poisoned
        });
        self.pending.clear();
        // Repaint in full, but keep showing the old frame (rather than one
        // without text) until the text is back, as for a first frame.
        let until = Instant::now() + self.first_frame_wait;
        for s in self.surfaces.values_mut() {
            s.valid = false;
            s.painted = false;
            s.wait_until = Some(until);
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
            if state.requested.as_ref().is_some_and(|(_, s)| s == spec) {
                continue;
            }
            if state.shaped.as_ref() == Some(spec) {
                // Reverted to what is shown: a request in flight for
                // something else must not replace it on arrival.
                if let Some((old, _)) = state.requested.take() {
                    self.pending.remove(&old);
                    self.text.cancel(old);
                }
                // Glyphs left out for want of atlas room: ask again, a
                // bounded number of times.
                if !(state.incomplete() && state.retries < MAX_TEXT_RETRIES) {
                    continue;
                }
                state.retries += 1;
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
                        state.retries = MAX_TEXT_RETRIES;
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
        s.painted = true;
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
        // the loop call `update`), so waiting costs no frames. A surface
        // not painted yet holds its first frame for its text, up to its
        // deadline.
        self.frame_deadline(surface).is_none()
            && self
                .surfaces
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
    use std::time::Duration;
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

    fn texts_diff(texts: &[(u32, &str)]) -> (SceneDiff, NodeId) {
        let root = NodeId::new(0, 0);
        let mut d = SceneDiff::new();
        d.create(root, NodeKind::Bar, None, 0).set(
            root,
            Prop::Color,
            PropValue::Color(Color::WHITE),
        );
        for (i, t) in texts {
            let id = NodeId::new(*i, 0);
            d.create(id, NodeKind::Text, Some(root), u32::MAX).set(
                id,
                Prop::Text,
                PropValue::Text((*t).into()),
            );
        }
        (d, root)
    }

    fn worker() -> TextBackend {
        let data = std::fs::read(test_font_path()).unwrap();
        TextBackend::Worker(
            strand_text::TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap(),
        )
    }

    /// A request that crashes the worker's engine every time is not asked
    /// for again after the reset, so the worker does not restart its
    /// engine (and every text reshape) in a loop.
    #[test]
    fn crashing_requests_are_not_retried() {
        let mut r = Renderer::new(worker());
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let (d, root) = texts_diff(&[(1, "crash"), (2, "fine")]);
        assert!(r.apply(d).is_empty());
        r.attach_surface(SurfaceId(1), root);
        r.configure_surface(SurfaceId(1), Size::new(80, 20), Scale::ONE);
        let key_of =
            |r: &Renderer, n: NodeId| r.texts[&(n, Scale::ONE)].requested.as_ref().unwrap().0;
        // Flattening without polling the worker: what `update` would ask.
        let ask = |r: &mut Renderer| {
            r.surfaces.get_mut(&SurfaceId(1)).unwrap().cache = None;
            r.flatten_surface(SurfaceId(1));
        };
        let ka = key_of(&r, a);
        r.deliver(TextLayout::reset(ka, Scale::ONE));
        ask(&mut r);
        assert!(r.texts[&(a, Scale::ONE)].poisoned);
        assert!(
            r.texts[&(a, Scale::ONE)].requested.is_none(),
            "not asked again"
        );
        // A second crash (here: the other text) keeps the first culprit
        // poisoned too.
        let kb = key_of(&r, b);
        r.deliver(TextLayout::reset(kb, Scale::ONE));
        ask(&mut r);
        assert!(r.pending.is_empty(), "{:?}", r.pending);
        assert!(r.texts[&(a, Scale::ONE)].poisoned && r.texts[&(b, Scale::ONE)].poisoned);
        // New text for the culprit is asked for.
        let mut d = SceneDiff::new();
        d.set(a, Prop::Text, PropValue::Text("other".into()));
        r.apply(d);
        assert!(r.wait_for_text(Duration::from_secs(10)));
        assert!(r.texts[&(a, Scale::ONE)].layout.is_some());
        assert!(!r.texts[&(a, Scale::ONE)].poisoned);
    }

    /// A layout missing glyphs for want of atlas room is retried a bounded
    /// number of times, and again once other text frees pages.
    #[test]
    fn incomplete_text_retries_without_looping() {
        let data = std::fs::read(test_font_path()).unwrap();
        let mut cfg = FontConfig::isolated(vec![Arc::new(data)]);
        cfg.atlas = strand_text::AtlasConfig {
            page_size: 64,
            max_pages: 1,
            max_bytes: 64 * 64,
        };
        let mut r = Renderer::new(TextBackend::Inline(Box::new(TextEngine::new(cfg))));
        let (b, a) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let (mut d, root) = texts_diff(&[(1, "WXYZ"), (2, "ABCD")]);
        let big = PropValue::Font(strand_scene::Font {
            size: 40.0,
            ..strand_scene::Font::default()
        });
        d.set(root, Prop::Font, big);
        assert!(r.apply(d).is_empty());
        r.attach_surface(SurfaceId(1), root);
        let mut px = vec![0u8; 200 * 40 * 4];
        let mut t = PaintTarget::new(&mut px, Size::new(200, 40), 800, Scale::ONE, 0).unwrap();
        r.paint(SurfaceId(1), &mut t);
        let state = |r: &Renderer, n| r.texts[&(n, Scale::ONE)].incomplete();
        let n = |r: &Renderer, x| {
            r.texts[&(x, Scale::ONE)]
                .layout
                .as_ref()
                .unwrap()
                .glyphs()
                .count()
        };
        assert!(
            !state(&r, b) && state(&r, a),
            "B fills the only page: {} {} {} {}",
            state(&r, b),
            state(&r, a),
            n(&r, b),
            n(&r, a)
        );
        // Two retries, then nothing more however often it is flattened.
        let keys = r.next_key;
        assert_eq!(keys, 1 + 2 + MAX_TEXT_RETRIES as u64);
        for _ in 0..3 {
            r.invalidate(SurfaceId(1));
            r.surfaces.get_mut(&SurfaceId(1)).unwrap().cache = None;
            r.update();
        }
        assert_eq!(r.next_key, keys, "no hot loop");
        // B goes: its page frees and A completes.
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove { id: b });
        r.apply(d);
        assert!(!state(&r, a));
        assert_eq!(
            r.texts[&(a, Scale::ONE)]
                .layout
                .as_ref()
                .unwrap()
                .glyphs()
                .count(),
            4
        );
    }

    /// Several outputs of different sizes keep a context per cell size
    /// instead of rebuilding edge-cell contexts every frame.
    #[test]
    fn contexts_cover_several_outputs() {
        let mut r = renderer();
        let mut d = SceneDiff::new();
        let sizes = [Size::new(300, 70), Size::new(280, 40), Size::new(270, 100)];
        for i in 0..3u32 {
            let id = NodeId::new(i, 0);
            d.create(id, NodeKind::Bar, None, i)
                .set(id, Prop::Bg, PropValue::Color(Color::WHITE));
            r.attach_surface(SurfaceId(i), id);
        }
        r.apply(d);
        for _ in 0..2 {
            for (i, size) in sizes.iter().enumerate() {
                let mut px = vec![0u8; (size.w * size.h * 4) as usize];
                let mut t = PaintTarget::new(&mut px, *size, size.w * 4, Scale::ONE, 0).unwrap();
                r.paint(SurfaceId(i as u32), &mut t);
            }
        }
        assert_eq!(r.raster.contexts(), 9, "every cell size is kept");
        r.detach_surface(SurfaceId(2));
        r.detach_surface(SurfaceId(1));
        assert!(r.raster.contexts() <= 8);
    }
}

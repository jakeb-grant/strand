//! The render thread's entry point: applies scene diffs, keeps text layouts
//! flowing, diffs damage and implements [`Painter`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{
    Anchor, Damage, Edge, Insets, LogicalPoint, LogicalSize, NodeId, NodeKind, PaintTarget,
    Painter, Prop, PropValue, Scale, SceneDiff, SceneOp, Size, SurfaceChange, SurfaceId,
    SurfaceSpec, TokenScope,
};
use strand_text::{TextEngine, TextError, TextKey, TextLayout, TextRequest, TextWorker};

use crate::flatten::{
    Flattened, HitBox, NodeRecord, Shaped, TextSpec, flatten, natural_texts, pick, scope_tables,
};
use crate::layout::{Boxes, MAX_CONTENT_SIZE, RootSize, ScrollState, TextSizes, layout};
use crate::raster::{AtlasMirror, Raster};
use crate::tree::{SceneError, SceneTree};

/// How many past frames' damage is kept for buffer-age widening. A buffer
/// older than this is repainted in full.
pub const DAMAGE_HISTORY: usize = 4;

/// How long a newly configured surface waits for its text before its first
/// frame (see [`Renderer::frame_deadline`]).
pub const FIRST_FRAME_TEXT_WAIT: Duration = Duration::from_millis(50);

/// How long a painted surface holds a frame that would show a text with
/// no glyphs yet (a node just added: no layout at any scale or width to
/// stand in): about one refresh at 60 Hz. Shaping takes about a
/// millisecond, so the frame usually goes out with its text instead of
/// one refresh later; past this the frame is painted without it (see
/// [`Renderer::frame_deadline`]).
pub const NEW_TEXT_WAIT: Duration = Duration::from_millis(16);

/// A surface that painted a frame this recently is in motion (an
/// animation, a stream of changes: two refreshes at 60 Hz). It never
/// holds a frame for new text: it paints at once and the glyphs follow a
/// frame later, so nothing else on it stalls (design.md: the renderer
/// never waits). The springs that land in M2 replace this with their own
/// in-flight state (decisions.md, wave2-exit).
pub const BUSY_WINDOW: Duration = Duration::from_millis(34);

/// How often a layout that came back incomplete (no atlas room) is asked
/// for again before waiting for other text to change or go.
pub const MAX_TEXT_RETRIES: u8 = 2;

/// How long `strand run` holds a frame for logic's answer to a container
/// query whose size changed (`when self.width < 300`): logic answers in
/// well under a millisecond, so the frame shows the settled variant; a
/// logic thread that is busy costs at most this.
pub const QUERY_WAIT: Duration = Duration::from_millis(30);

/// How long a content-sized surface holds a frame after its size changed
/// (its content grew or its text arrived) for the compositor's configure
/// at the new size: one round trip. Painting meanwhile would show a frame
/// at the old size, then the new one (a one-frame size pop).
pub const RESIZE_WAIT: Duration = Duration::from_millis(50);

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

/// What one text layout is shaped for: a node at one scale, in a line box
/// of one width. Alignment happens inside the line box, so a node shown on
/// two surfaces of the same scale but different widths needs two layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TextSlot {
    node: NodeId,
    scale: Scale,
    /// `max_width` as bits (`None` when unbounded).
    width: Option<u32>,
}

impl TextSlot {
    fn of(node: NodeId, spec: &TextSpec) -> Self {
        Self {
            node,
            scale: spec.scale,
            width: spec.max_width.map(f32::to_bits),
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
    /// Some text on the surface is being shaped and has no layout for
    /// the surface's own scale and width yet.
    awaiting_text: bool,
    /// Until when a painted surface holds a frame for text with no layout
    /// at all (a new node): set when such text shows up, cleared when
    /// none is left.
    new_text_until: Option<Instant>,
    /// When it last painted a frame (see [`BUSY_WINDOW`]).
    painted_at: Option<Instant>,
    dirty: bool,
    /// Fully opaque part of the last painted frame.
    opaque: Damage,
    /// Presentation time of the last painted frame (springs sample it
    /// from M2).
    time: Duration,
    /// The flattened scene for the current state, reused by `paint` after
    /// `update`; `None` once anything it depends on changes.
    cache: Option<Flattened>,
    /// The text slots the last flatten wanted.
    wanted: Vec<TextSlot>,
    /// The last layout pass, reused until something layout reads changes.
    boxes: Option<Boxes>,
    /// Something layout reads changed since `boxes` was computed.
    layout_dirty: bool,
    /// Hit shapes of the last painted frame, in paint order.
    hits: Vec<HitBox>,
    /// A layout pass changed the size of a node a container query reads:
    /// the frame waits for logic to have seen fact batch `.0` (its
    /// answer, see [`SceneDiff::layout_seen`]) or until `.1`.
    query_hold: Option<(u64, Instant)>,
    /// A frame was held for a query since the last paint: at most one
    /// hold (one extra pass) per frame.
    query_held: bool,
    /// How far shadows reach past the root's box (from its spec).
    overhang: strand_scene::Insets,
    /// A content-sized surface whose spec asks for another size than it
    /// is configured at: the buffer size it waits for, and until when.
    size_hold: Option<(LogicalSize, Instant)>,
}

impl SurfaceState {
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.cache = None;
    }

    /// Something layout reads changed: lay out again before painting.
    fn mark_layout(&mut self) {
        self.mark_dirty();
        self.layout_dirty = true;
    }

    /// Holds the next frame for logic's answer to fact batch `seq` (a
    /// container query's size changed), once per frame, and only on a
    /// surface that is idle: one in motion (a size animating) paints on,
    /// and the answer lands a frame later, so logic never sits on the
    /// path of an animation (design.md: the renderer never waits).
    fn hold_for_query(&mut self, seq: u64, wait: Duration, window: Duration) {
        let now = Instant::now();
        let idle = self
            .painted_at
            .is_none_or(|t| now.saturating_duration_since(t) >= window);
        if !wait.is_zero() && !self.query_held && idle {
            self.query_hold = Some((seq, now + wait));
            self.query_held = true;
        }
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
        self.mark_layout();
        rescaled
    }
}

/// Retained scene, damage tracking and painting for every surface.
#[derive(Debug)]
pub struct Renderer {
    tree: SceneTree,
    surfaces: BTreeMap<SurfaceId, SurfaceState>,
    text: TextBackend,
    /// Text per node, scale and line box width: a node shown on outputs of
    /// different scales or widths keeps one layout for each.
    texts: HashMap<TextSlot, TextState>,
    pending: HashMap<TextKey, TextSlot>,
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
    new_text_wait: Duration,
    busy_window: Duration,
    /// Scroll offsets and list row heights, by `scroll`/`list` node.
    scrolls: HashMap<NodeId, ScrollState>,
    /// Layout passes run (tests: paint-only changes run none).
    layout_passes: u64,
    /// Laid-out sizes not yet handed to logic (`self.width`), and the
    /// last size each surface laid a watched node out at (a node shown on
    /// two surfaces of different sizes reports a change of either, never
    /// flip-flops between them).
    facts: Vec<(NodeId, f32, f32)>,
    facts_sent: HashMap<(SurfaceId, NodeId), (f32, f32)>,
    /// Fact batches handed out ([`Renderer::layout_seq`]).
    facts_seq: u64,
    /// How long a frame waits for logic's answer to a container query
    /// (zero: never; no logic thread offline).
    query_wait: Duration,
    /// Text slots content sizing of surfaces asks for (kept by pruning).
    spec_wanted: HashMap<NodeId, Vec<TextSlot>>,
    /// Surface nodes whose content size or overhang may have changed.
    spec_dirty: BTreeSet<NodeId>,
    /// Surface nodes sized by their content (as of the last refresh).
    content_sized: BTreeSet<NodeId>,
    /// How long a content-sized surface holds a frame for the configure
    /// at its new size (zero: never; offline there is no compositor).
    resize_wait: Duration,
    /// The logical size of the output each surface is on, when known: a
    /// content-sized surface is never larger (see [`Renderer::set_surface_bounds`]).
    bounds: HashMap<SurfaceId, LogicalSize>,
}

/// The overhang a surface asks for: on an axis its anchor leaves centred
/// (both axes for `center`, the cross axis for an edge, none for a bar
/// or a corner), the larger side on both sides, since the compositor
/// centres the whole buffer and an uneven overhang would move the box off
/// centre.
fn centred_overhang(spec: &SurfaceSpec, o: Insets) -> Insets {
    if spec.kind == NodeKind::Bar {
        return o;
    }
    let (h, v) = match spec.anchor {
        Anchor::Center => (true, true),
        Anchor::Top | Anchor::Bottom => (true, false),
        Anchor::Left | Anchor::Right => (false, true),
        _ => (false, false),
    };
    let mut o = o;
    if h {
        let m = o.left.max(o.right);
        (o.left, o.right) = (m, m);
    }
    if v {
        let m = o.top.max(o.bottom);
        (o.top, o.bottom) = (m, m);
    }
    o
}

/// Delivered text layouts as layout measures them.
struct TextInfo<'a> {
    shaped: &'a HashMap<NodeId, Vec<Shaped>>,
    scale: Scale,
}

impl TextSizes for TextInfo<'_> {
    fn natural(&self, node: NodeId) -> Option<strand_scene::LogicalSize> {
        pick(self.shaped.get(&node)?, self.scale, None).map(|l| l.size)
    }

    fn fitted(&self, node: NodeId, width: f32) -> Option<strand_scene::LogicalSize> {
        pick(self.shaped.get(&node)?, self.scale, Some(width)).map(|l| l.size)
    }
}

/// True for props layout reads: a change to one relayouts its surface;
/// any other prop (colours, `x`/`y`, opacity, shadows) only repaints.
pub fn affects_layout(prop: Prop) -> bool {
    use Prop::*;
    matches!(
        prop,
        Width
            | Height
            | Size
            | MinWidth
            | MaxWidth
            | MinHeight
            | MaxHeight
            | Pad
            | Margin
            | Gap
            | Grow
            | Shrink
            | Align
            | Justify
            | Place
            | Columns
            | Edge
            | Text
            | Font
            | Weight
            | Ellipsis
            | MaxLines
            | Markup
            | Marks
            | Tokens
    )
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
            new_text_wait: NEW_TEXT_WAIT,
            busy_window: BUSY_WINDOW,
            scrolls: HashMap::new(),
            layout_passes: 0,
            facts: Vec::new(),
            facts_sent: HashMap::new(),
            facts_seq: 0,
            query_wait: Duration::ZERO,
            spec_wanted: HashMap::new(),
            spec_dirty: BTreeSet::new(),
            content_sized: BTreeSet::new(),
            bounds: HashMap::new(),
            resize_wait: Duration::ZERO,
        }
    }

    /// How long a content-sized surface whose size changed holds its
    /// frame for the compositor's configure at the new size (see
    /// [`RESIZE_WAIT`]); zero, the default, never holds (offline
    /// rendering has no compositor to wait for).
    pub fn set_resize_wait(&mut self, wait: Duration) {
        self.resize_wait = wait;
        self.refresh_size_holds();
    }

    /// The logical size of the output `surface` is on: a content-sized
    /// surface is laid out no larger than it, less its margins (the
    /// surface manager clamps the layer size the same way), so it waits
    /// for no configure the compositor would never send.
    pub fn set_surface_bounds(&mut self, surface: SurfaceId, size: Option<LogicalSize>) {
        match size {
            Some(b) => self.bounds.insert(surface, b),
            None => self.bounds.remove(&surface),
        };
        self.refresh_size_holds();
    }

    /// The buffer size, logical pixels, the spec of `root` asks a
    /// surface of it for, on the content-sized axes (`None`: not sized by
    /// its content on that axis).
    fn wanted_size(&self, surface: SurfaceId, root: NodeId) -> Option<(Option<f32>, Option<f32>)> {
        if !self.content_sized.contains(&root) {
            return None;
        }
        let spec = self.specs.get(&root)?;
        let (o, m) = (spec.overhang, spec.margin);
        let b = self.bounds.get(&surface);
        let fit = |v: Option<f32>, bound: Option<f32>, margins: f32| {
            v.map(|v| match bound {
                Some(b) => v.min((b - margins).max(1.0)),
                None => v,
            })
        };
        let w = fit(spec.width, b.map(|b| b.w), m.left + m.right).map(|w| w + o.left + o.right);
        let h = fit(spec.height, b.map(|b| b.h), m.top + m.bottom).map(|h| h + o.top + o.bottom);
        Some(match (spec.kind, spec.edge) {
            (NodeKind::Bar, Some(Edge::Left | Edge::Right)) => (w, None),
            (NodeKind::Bar, _) => (None, h),
            _ => (w, h),
        })
    }

    /// Holds the frames of content-sized surfaces configured at another
    /// size than their spec asks for, until the configure (or
    /// [`RESIZE_WAIT`]): whatever the order of the spec change and the
    /// configure (a spec that changed before the surface's first
    /// configure arrived holds its first frame too). Each size asked for
    /// holds at most once, so a compositor that configures another size
    /// costs one wait, not a stall.
    fn refresh_size_holds(&mut self) {
        let ids: Vec<(SurfaceId, NodeId)> =
            self.surfaces.iter().map(|(i, s)| (*i, s.root)).collect();
        let now = Instant::now();
        for (id, root) in ids {
            let wanted = self.wanted_size(id, root);
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let holds = !self.resize_wait.is_zero() && s.size != Size::default();
            let Some((w, h)) = wanted.filter(|_| holds) else {
                s.size_hold = None;
                continue;
            };
            let have = s.scale.logical_size(s.size);
            let off =
                |want: Option<f32>, have: f32| want.is_some_and(|v| (v.round() - have).abs() > 1.0);
            if !(off(w, have.w) || off(h, have.h)) {
                // Configured at what it asked for.
                s.size_hold = None;
                continue;
            }
            let target = LogicalSize::new(w.unwrap_or(have.w), h.unwrap_or(have.h));
            if s.size_hold.is_none_or(|(t, _)| t != target) {
                // A first frame waits as long as it would for its text:
                // nothing shows meanwhile, and a busy main thread (other
                // surfaces painting at boot) can delay the configure
                // past one round trip.
                let wait = if s.painted {
                    self.resize_wait
                } else {
                    self.resize_wait.max(self.first_frame_wait)
                };
                s.size_hold = Some((target, now + wait));
            }
        }
    }

    /// Layout passes run so far (paint-only changes run none).
    pub fn layout_passes(&self) -> u64 {
        self.layout_passes
    }

    /// The last layout of `surface`: every laid-out node's box in
    /// surface logical pixels (before paint offsets).
    pub fn boxes(&self, surface: SurfaceId) -> Option<&Boxes> {
        self.surfaces.get(&surface)?.boxes.as_ref()
    }

    /// Laid-out sizes that changed since the last call, `(node, width,
    /// height)` in logical pixels, of the nodes logic reads them of
    /// (`Prop::Watch`): what `self.width` and container queries read. A
    /// node shown on several surfaces reports the size of the last one
    /// laid out. A non-empty batch gets the next [`Renderer::layout_seq`].
    pub fn take_layout_facts(&mut self) -> Vec<(NodeId, f32, f32)> {
        let facts = std::mem::take(&mut self.facts);
        if !facts.is_empty() {
            self.facts_seq += 1;
        }
        facts
    }

    /// The sequence number of the last fact batch taken: logic echoes it
    /// as [`SceneDiff::layout_seen`].
    pub fn layout_seq(&self) -> u64 {
        self.facts_seq
    }

    /// How long a frame whose layout changed a size a container query
    /// reads waits for logic's answer (see [`QUERY_WAIT`]); zero, the
    /// default, never waits.
    pub fn set_query_wait(&mut self, wait: Duration) {
        self.query_wait = wait;
    }

    /// Scrolls the innermost `scroll` or `list` under `point` of
    /// `surface` (in its last frame) that can move by `dy` logical
    /// pixels: one already at its end passes the scroll outward. Returns
    /// the node scrolled, if any moved.
    pub fn scroll(&mut self, surface: SurfaceId, point: LogicalPoint, dy: f32) -> Option<NodeId> {
        let chain = self.hit(surface, point);
        // The innermost that can still move that way: one at its end
        // hands the wheel to the one around it.
        let id = chain.into_iter().find(|n| {
            self.tree
                .get(*n)
                .is_some_and(|n| matches!(n.kind, NodeKind::Scroll | NodeKind::List))
                && self.scrolls.entry(*n).or_default().scroll_by(dy)
        })?;
        let root = self.tree.root_of(id);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_layout();
            }
        }
        Some(id)
    }

    /// Scrolls list or scroll `id` so that its child `row` is in view.
    pub fn scroll_into_view(&mut self, id: NodeId, row: NodeId) {
        let Some(node) = self.tree.get(id) else {
            return;
        };
        let st = self.scrolls.entry(id).or_default();
        let est = if st.heights.is_empty() {
            crate::layout::LIST_ROW_ESTIMATE
        } else {
            st.heights.values().sum::<f32>() / st.heights.len() as f32
        };
        let gap = node
            .get(Prop::Gap)
            .and_then(PropValue::as_number)
            .unwrap_or(0.0)
            .max(0.0);
        let mut y = 0.0;
        for c in &node.children {
            let h = st.heights.get(c).copied().unwrap_or(est);
            if *c == row {
                let before = st.offset;
                if y < st.offset {
                    st.offset = y;
                } else if y + h > st.offset + st.viewport {
                    st.offset = y + h - st.viewport;
                }
                if st.offset != before {
                    let root = self.tree.root_of(id);
                    for s in self.surfaces.values_mut() {
                        if Some(s.root) == root {
                            s.mark_layout();
                        }
                    }
                }
                return;
            }
            y += h + gap;
        }
    }

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

    /// The text backend (the runtime's text worker, or inline shaping).
    pub fn text(&self) -> &TextBackend {
        &self.text
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

    /// True if surface changes are waiting for
    /// [`Renderer::take_surface_changes`]: a host that called in from the
    /// surface manager (a configure, a paint) wakes its loop to hand
    /// them over at once, as a resized surface's frame is held for them.
    pub fn has_surface_changes(&self) -> bool {
        !self.surface_changes.is_empty()
    }

    /// Text layouts kept or asked for, over all nodes, scales and widths
    /// (tests: a virtualised list shapes only the rows in view).
    pub fn text_slots(&self) -> usize {
        self.texts.len()
    }

    /// Bytes of atlas pixels the render thread mirrors for `scale`.
    pub fn atlas_mirror_bytes(&self, scale: Scale) -> usize {
        self.atlas.bytes(scale)
    }

    /// Re-resolves every surface spec and records what changed. A
    /// surface with no size of its own (a panel, OSD or popup without
    /// `width`/`height`, a bar without a thickness) is sized by its
    /// content, laid out on its own; every surface reports how far its
    /// shadows reach past its box (`SurfaceSpec::overhang`).
    ///
    /// The content pass runs only where it decides something: never for
    /// a closed surface (it is sized when it opens), and for a surface of
    /// a fixed size only until it is shown (then the overhang comes from
    /// the pass that lays it out for painting, see `flatten_now`).
    fn refresh_specs(&mut self) {
        let dirty = std::mem::take(&mut self.spec_dirty);
        let layouts = (!dirty.is_empty()).then(|| self.shaped());
        let ids: Vec<NodeId> = self.tree.surface_nodes().collect();
        let mut live = BTreeSet::new();
        let mut requests = Vec::new();
        for id in ids {
            let Some(node) = self.tree.get(id) else {
                continue;
            };
            let tables = scope_tables(&self.tree, id);
            let scope = TokenScope::new(&tables);
            let mut spec =
                SurfaceSpec::resolve(node.kind, |p| node.get(p).and_then(|v| scope.resolve(v)));
            live.insert(id);
            let bar = spec.kind == NodeKind::Bar;
            let vertical = matches!(spec.edge, Some(Edge::Left | Edge::Right));
            let content_sized = if bar {
                if vertical {
                    spec.width.is_none()
                } else {
                    spec.height.is_none()
                }
            } else {
                spec.width.is_none() || spec.height.is_none()
            };
            if content_sized {
                self.content_sized.insert(id);
            } else {
                self.content_sized.remove(&id);
            }
            // A surface already showing this node: its scale, and for a bar
            // the length the compositor gave it.
            let shown = self.surfaces.values().find(|s| s.root == id);
            let scale = shown.map_or(Scale::ONE, |s| s.scale);
            let old = self.specs.get(&id);
            let laid_out = shown.is_some_and(|s| s.boxes.is_some());
            let pass = spec.open && (content_sized || !laid_out);
            if !spec.open {
                // Nothing to shape for a surface nobody sees.
                self.spec_wanted.remove(&id);
            }
            let (size, overhang) = match (&layouts, old) {
                (Some(layouts), _) if dirty.contains(&id) && pass => {
                    let along = shown.map(|s| {
                        let l = s.scale.logical_size(s.size);
                        let o = s.overhang;
                        if vertical {
                            l.h - o.top - o.bottom
                        } else {
                            l.w - o.left - o.right
                        }
                    });
                    let (w, h) = match (bar, vertical) {
                        (true, false) => (along, spec.height),
                        (true, true) => (spec.width, along),
                        _ => (spec.width, spec.height),
                    };
                    let info = TextInfo {
                        shaped: layouts,
                        scale,
                    };
                    let root_size = RootSize::Content {
                        width: w,
                        height: h,
                    };
                    let mut b = layout(&self.tree, id, root_size, &info, &mut self.scrolls);
                    self.layout_passes += 1;
                    if b.unsettled {
                        // A list measured rows it had only estimated: its
                        // size (and so the surface's) comes from the
                        // second pass, as the painted one does.
                        b = layout(&self.tree, id, root_size, &info, &mut self.scrolls);
                        self.layout_passes += 1;
                    }
                    if content_sized {
                        let r = natural_texts(&self.tree, id, scale, &b.rects);
                        self.spec_wanted
                            .insert(id, r.iter().map(|(n, t)| TextSlot::of(*n, t)).collect());
                        requests.extend(r);
                    } else {
                        self.spec_wanted.remove(&id);
                    }
                    (b.size, b.overhang)
                }
                (_, Some(old)) => (
                    LogicalSize::new(old.width.unwrap_or(0.0), old.height.unwrap_or(0.0)),
                    old.overhang,
                ),
                _ => (LogicalSize::default(), Insets::default()),
            };
            spec.overhang = centred_overhang(&spec, overhang);
            if content_sized {
                // Capped: content taller than any output (a list with no
                // `max_height`, a long body) never asks for a buffer of
                // its full size; the output's own size caps it further
                // where the surface is placed.
                let cap = |v: f32| v.ceil().clamp(1.0, MAX_CONTENT_SIZE);
                let (w, h) = (cap(size.w), cap(size.h));
                match (bar, vertical) {
                    (true, false) => spec.height = Some(h),
                    (true, true) => spec.width = Some(w),
                    _ => {
                        spec.width.get_or_insert(w);
                        spec.height.get_or_insert(h);
                    }
                }
            }
            self.record_spec(id, spec);
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
        self.content_sized.retain(|id| live.contains(id));
        self.spec_wanted.retain(|id, _| live.contains(id));
        if !requests.is_empty() && self.request_text(&requests) {
            // Shaped inline: size the surfaces with it at once.
            self.spec_dirty
                .extend(requests.iter().filter_map(|(n, _)| self.tree.root_of(*n)));
            self.refresh_specs();
        }
        self.refresh_size_holds();
    }

    /// Records `spec` as the spec of surface node `id`, reporting a
    /// change; shown surfaces lay out again inside a new overhang.
    fn record_spec(&mut self, id: NodeId, spec: SurfaceSpec) {
        let change = match self.specs.get(&id) {
            None => SurfaceChange::Created(spec.clone()),
            Some(old) if *old != spec => SurfaceChange::Updated {
                recreate: old.needs_recreate(&spec),
                spec: spec.clone(),
            },
            Some(_) => return,
        };
        for s in self.surfaces.values_mut() {
            if s.root == id && s.overhang != spec.overhang {
                s.overhang = spec.overhang;
                s.mark_layout();
            }
        }
        self.surface_changes.push((id, change));
        self.specs.insert(id, spec);
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
                new_text_until: None,
                painted_at: None,
                dirty: true,
                opaque: Damage::new(),
                time: Duration::ZERO,
                cache: None,
                wanted: Vec::new(),
                boxes: None,
                layout_dirty: true,
                hits: Vec::new(),
                query_hold: None,
                query_held: false,
                size_hold: None,
                overhang: self
                    .specs
                    .get(&root)
                    .map(|s| s.overhang)
                    .unwrap_or_default(),
            },
        );
        self.raster.set_surfaces(self.surfaces.len());
        self.refresh_size_holds();
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
        self.bounds.remove(&surface);
        self.facts_sent.retain(|(s, _), _| *s != surface);
        self.last_damage.remove(&surface);
        self.raster.set_surfaces(self.surfaces.len());
        self.prune_scales();
        self.prune_texts();
    }

    /// Drops text no surface wants any more: slots for a width or scale
    /// a surface has left. A slot drawn as a stand-in (its node's wanted
    /// slot has no layout yet) is kept until the wanted one arrives; a
    /// poisoned slot never gets one, so it keeps no stand-ins.
    ///
    /// A surface whose wanted slot has no layout may have drawn a dropped
    /// slot as its stand-in: it is marked dirty (its cache cleared) so it
    /// stops drawing it. Returns those surfaces.
    fn prune_texts(&mut self) -> Vec<SurfaceId> {
        let mut keep: HashSet<TextSlot> = self.spec_wanted.values().flatten().copied().collect();
        let mut standing_in: HashSet<NodeId> = HashSet::new();
        for s in self.surfaces.values() {
            for slot in &s.wanted {
                keep.insert(*slot);
                if self
                    .texts
                    .get(slot)
                    .is_none_or(|t| t.layout.is_none() && !t.poisoned)
                {
                    standing_in.insert(slot.node);
                }
            }
        }
        let mut dropped: HashSet<NodeId> = HashSet::new();
        let text = &self.text;
        self.texts.retain(|slot, t| {
            let k = keep.contains(slot) || standing_in.contains(&slot.node);
            if !k {
                if let Some((key, _)) = t.requested.take() {
                    text.cancel(key);
                }
                if t.layout.is_some() {
                    dropped.insert(slot.node);
                }
            }
            k
        });
        let mut marked = Vec::new();
        if dropped.is_empty() {
            return marked;
        }
        let texts = &self.texts;
        self.pending.retain(|_, slot| texts.contains_key(slot));
        for (id, s) in &mut self.surfaces {
            let drew_stand_in = s.wanted.iter().any(|w| {
                dropped.contains(&w.node) && texts.get(w).is_none_or(|t| t.layout.is_none())
            });
            if drew_stand_in {
                s.mark_dirty();
                marked.push(*id);
            }
        }
        self.refresh_retries();
        marked
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
        [
            until,
            s.query_hold.map(|(_, t)| t),
            s.size_hold.map(|(_, t)| t),
        ]
        .into_iter()
        .flatten()
        .filter(|t| now < *t)
        .min()
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
            self.texts.retain(|slot, t| {
                if slot.scale == scale
                    && let Some((k, _)) = t.requested.take()
                {
                    text.cancel(k);
                }
                slot.scale != scale
            });
            self.pending.retain(|_, slot| slot.scale != scale);
            self.text.drop_scale(scale);
        }
    }

    /// The nodes under the logical `point` of `surface` in its last
    /// painted frame, innermost first, ending at the surface's root (just
    /// the root when nothing is there). A node is hit inside its laid-out
    /// rounded box, grown by `hit: grow(n)` and cut by the clip of a
    /// `scroll`, `list` or `clip: true` ancestor; the topmost in paint
    /// order wins (children over their parent, later siblings over
    /// earlier ones). Shadows are not part of a node's shape. Empty for an
    /// unknown surface.
    pub fn hit(&self, surface: SurfaceId, point: LogicalPoint) -> Vec<NodeId> {
        let Some(s) = self.surfaces.get(&surface) else {
            return Vec::new();
        };
        let k = s.scale.as_f64();
        let (x, y) = (point.x as f64 * k, point.y as f64 * k);
        let best = s
            .hits
            .iter()
            .rev()
            .find(|h| h.contains(x, y))
            .map(|h| h.node);
        let mut chain = Vec::new();
        let mut cur = best.unwrap_or(s.root);
        loop {
            chain.push(cur);
            if cur == s.root {
                break;
            }
            match self.tree.get(cur).and_then(|n| n.parent) {
                Some(p) => cur = p,
                None => break,
            }
        }
        chain
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
        // `relayout`: those whose layout it may change (anything but a
        // paint-only prop).
        let mut touched: Option<Vec<NodeId>> = Some(Vec::new());
        let mut relayout: Option<Vec<NodeId>> = Some(Vec::new());
        // Logic took in the facts a held frame waits for: it answered.
        if let Some(seen) = diff.layout_seen {
            for s in self.surfaces.values_mut() {
                if s.query_hold.is_some_and(|(seq, _)| seq <= seen) {
                    s.query_hold = None;
                }
            }
        }
        let mut watched = Vec::new();
        for op in diff.ops {
            if let SceneOp::SetProp {
                id,
                prop: Prop::Watch,
                ..
            } = &op
            {
                watched.push(*id);
            }
            // Where the node painted before the op, and the node whose
            // root to look up after it.
            let (before, after, shapes) = match &op {
                SceneOp::Create { id, .. } => (None, Some(*id), true),
                SceneOp::Remove { id } => (self.tree.root_of(*id), None, true),
                SceneOp::SetProp { id, prop, .. } => {
                    (self.tree.root_of(*id), None, affects_layout(*prop))
                }
                SceneOp::Move { id, .. } => (self.tree.root_of(*id), Some(*id), true),
                SceneOp::SetTokens { .. } => {
                    touched = None;
                    relayout = None;
                    (None, None, true)
                }
            };
            // `open` sizes a content-sized surface when it opens.
            let open = matches!(
                &op,
                SceneOp::SetProp {
                    prop: Prop::Open,
                    ..
                }
            );
            // A shadow, or the coordinates of a `place: absolute` node
            // casting one, changes only the overhang a surface asks for
            // (from its layout pass, so it lays out again). A flow node's
            // paint offset never does: the overhang is taken at rest, so
            // an enter or exit animation never resizes the buffer.
            let shadow = match &op {
                SceneOp::SetProp {
                    prop: Prop::Shadow, ..
                } => true,
                SceneOp::SetProp {
                    id,
                    prop: Prop::X | Prop::Y,
                    ..
                } => self.tree.get(*id).is_some_and(|n| {
                    n.get(Prop::Shadow).is_some()
                        && matches!(n.get(Prop::Place), Some(PropValue::Keyword(k)) if k == "absolute")
                }),
                _ => false,
            };
            match self.tree.apply_op(op) {
                Ok(()) => {
                    let roots: Vec<NodeId> = before
                        .into_iter()
                        .chain(after.and_then(|id| self.tree.root_of(id)))
                        .collect();
                    if let Some(t) = &mut touched {
                        t.extend(roots.iter().copied());
                    }
                    if (shapes || shadow)
                        && let Some(t) = &mut relayout
                    {
                        t.extend(roots.iter().copied());
                    }
                    if shapes || shadow || open {
                        self.spec_dirty.extend(roots);
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        if relayout.is_none() {
            self.spec_dirty.extend(self.tree.surface_nodes());
        }
        // Drop text state of nodes that are gone or no longer show text.
        let tree = &self.tree;
        let text = &self.text;
        let before = self.texts.len();
        self.texts.retain(|slot, t| {
            let keep = tree.get(slot.node).is_some_and(|n| match n.kind {
                NodeKind::Text | NodeKind::Button => n.get(Prop::Text).is_some(),
                NodeKind::Input => {
                    n.get(Prop::Text).is_some() || n.get(Prop::Placeholder).is_some()
                }
                _ => false,
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
        let tree = &self.tree;
        self.facts_sent.retain(|(_, n), _| tree.contains(*n));
        // Newly watched nodes: their size as laid out now, if it is (on
        // the first surface showing it). A query that starts reading one
        // holds that surface's next frame for the answer, as a size
        // change would.
        let (wait, window) = (self.query_wait, self.busy_window);
        for id in watched {
            self.facts_sent.retain(|(_, n), _| *n != id);
            let query = match self.tree.get(id).and_then(|n| n.get(Prop::Watch)) {
                None => continue,
                Some(w) => matches!(w, PropValue::Keyword(k) if k == "query"),
            };
            let seq = self.facts_seq + 1;
            let mut first = true;
            for (sid, s) in self.surfaces.iter_mut() {
                let Some(r) = s.boxes.as_ref().and_then(|b| b.rects.get(&id)).copied() else {
                    continue;
                };
                self.facts_sent.insert((*sid, id), (r.w, r.h));
                if !first {
                    continue;
                }
                first = false;
                if query {
                    s.hold_for_query(seq, wait, window);
                }
                self.facts.push((id, r.w, r.h));
            }
        }
        self.scrolls.retain(|n, _| tree.contains(*n));
        // A surface nested in a touched one (a popup in a bar) inherits
        // from it, so it is touched too; `update` clears it again if
        // nothing it draws changed.
        // A surface whose root went with an ancestor (a popup in a removed
        // bar) is touched too: its stale frame clears until it is
        // detached.
        let tree = &self.tree;
        for s in self.surfaces.values_mut() {
            let hit = |t: &Option<Vec<NodeId>>| {
                t.as_ref()
                    .is_none_or(|t| t.iter().any(|r| tree.is_ancestor(*r, s.root)))
            };
            if !tree.contains(s.root) || hit(&relayout) {
                s.mark_layout();
            } else if hit(&touched) {
                s.mark_dirty();
            }
        }
        // `update` refreshes the specs (after collecting delivered text,
        // so nothing asked for here can already be answered).
        self.update();
        errors
    }

    /// Gives every incomplete layout its retries back and wakes its
    /// surfaces, after something happened that can free atlas pages (a
    /// layout replaced for new text, text removed, a scale dropped).
    /// Retries themselves never refresh, so this cannot loop.
    fn refresh_retries(&mut self) {
        let mut roots = BTreeSet::new();
        for (slot, t) in self.texts.iter_mut() {
            if t.incomplete() {
                t.retries = 0;
                roots.extend(self.tree.root_of(slot.node));
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
        self.refresh_specs();
        let ids: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.dirty && s.cache.is_none() && s.size != Size::default())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let f = self.flatten_surface(id);
            // Text with a request in flight and no layout for this
            // surface's scale and width yet. A stand-in from another scale
            // or width does not count: a first frame drawn with one would
            // be repainted as soon as the right layout lands.
            let texts = &self.texts;
            let waiting: Vec<NodeId> = f
                .text
                .iter()
                .filter(|(node, spec)| {
                    texts
                        .get(&TextSlot::of(*node, spec))
                        .is_some_and(|t| t.layout.is_none() && t.requested.is_some())
                })
                .map(|(node, _)| *node)
                .collect();
            // Of those, text with no layout anywhere to stand in.
            let blank = !waiting.is_empty() && {
                let shown: HashSet<NodeId> = texts
                    .iter()
                    .filter(|(_, t)| t.layout.is_some())
                    .map(|(slot, _)| slot.node)
                    .collect();
                waiting.iter().any(|n| !shown.contains(n))
            };
            let wait = self.new_text_wait;
            let window = self.busy_window;
            if let Some(s) = self.surfaces.get_mut(&id) {
                if s.valid && s.records == f.records && s.opaque == f.opaque {
                    s.dirty = false;
                }
                s.awaiting_text = !waiting.is_empty();
                // A surface in motion holds nothing (see BUSY_WINDOW).
                let now = Instant::now();
                let idle = s
                    .painted_at
                    .is_none_or(|t| now.saturating_duration_since(t) >= window);
                s.new_text_until = match (blank, s.new_text_until) {
                    (false, _) => None,
                    (true, Some(t)) => Some(t),
                    (true, None) if s.painted && idle && !wait.is_zero() => Some(now + wait),
                    (true, None) => None,
                };
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
        // Surfaces at other scales may draw it resampled meanwhile; its
        // size feeds layout, and a content-sized surface's size.
        let root = self.tree.root_of(slot.node);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_layout();
            }
        }
        self.spec_dirty.extend(root);
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
            s.mark_layout();
        }
    }

    /// Every delivered layout per node, with the width it was shaped for.
    /// `flatten` draws the one for the surface's scale and width or, while
    /// that is being shaped, a stand-in (resampled and re-aligned).
    fn shaped(&self) -> HashMap<NodeId, Vec<Shaped>> {
        let mut out: HashMap<NodeId, Vec<Shaped>> = HashMap::new();
        for (slot, t) in &self.texts {
            let Some(l) = &t.layout else { continue };
            out.entry(slot.node).or_default().push(Shaped {
                layout: l.clone(),
                max_width: slot.width.map(f32::from_bits),
            });
        }
        // Stand-in choice must not depend on hash order.
        for v in out.values_mut() {
            v.sort_by(|a, b| {
                (a.layout.scale, a.max_width.map(f32::to_bits))
                    .cmp(&(b.layout.scale, b.max_width.map(f32::to_bits)))
            });
        }
        out
    }

    /// Flattens a surface, issuing text requests for changed text. With the
    /// inline backend new layouts are available at once, so it flattens
    /// again until text is stable.
    fn flatten_surface(&mut self, id: SurfaceId) -> Flattened {
        let mut stable = None;
        for _ in 0..6 {
            let flat = self.flatten_now(id);
            if !self.request_text(&flat.text) {
                stable = Some(flat);
                break;
            }
        }
        let f = match stable {
            Some(f) => f,
            None => self.flatten_now(id),
        };
        // Once every text here has a layout at this scale, the previous
        // scale's layouts are no longer needed as stand-ins.
        let texts = &self.texts;
        let settled = f.text.iter().all(|(node, spec)| {
            texts
                .get(&TextSlot::of(*node, spec))
                .is_some_and(|t| t.requested.is_none())
        });
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.wanted = f
                .text
                .iter()
                .map(|(node, spec)| TextSlot::of(*node, spec))
                .collect();
            if settled && s.prev_scale.take().is_some() {
                self.prune_scales();
            }
        }
        if self.prune_texts().contains(&id) {
            // `f` drew a stand-in that is gone now.
            return self.flatten_now(id);
        }
        f
    }

    /// Lays `id` out if anything layout reads changed (at most one extra
    /// pass, when a list measured rows it had only estimated), then
    /// flattens it.
    fn flatten_now(&mut self, id: SurfaceId) -> Flattened {
        let layouts = self.shaped();
        let Some(s) = self.surfaces.get(&id) else {
            return Flattened::default();
        };
        if s.layout_dirty || s.boxes.is_none() {
            let logical = s.scale.logical_size(s.size);
            let o = s.overhang;
            let frame = strand_scene::LogicalRect::new(
                o.left,
                o.top,
                (logical.w - o.left - o.right).max(0.0),
                (logical.h - o.top - o.bottom).max(0.0),
            );
            let info = TextInfo {
                shaped: &layouts,
                scale: s.scale,
            };
            let root = s.root;
            let mut boxes = layout(
                &self.tree,
                root,
                RootSize::Fixed(frame),
                &info,
                &mut self.scrolls,
            );
            self.layout_passes += 1;
            if boxes.unsettled {
                boxes = layout(
                    &self.tree,
                    root,
                    RootSize::Fixed(frame),
                    &info,
                    &mut self.scrolls,
                );
                self.layout_passes += 1;
            }
            // A surface of a fixed size takes its overhang from this
            // pass (no content pass runs for it once shown): a change is
            // reported, and it lays out again inside the new one.
            if !self.content_sized.contains(&root)
                && let Some(mut spec) = self.specs.get(&root).cloned()
                && spec.open
            {
                let o = centred_overhang(&spec, boxes.overhang);
                if o != spec.overhang {
                    spec.overhang = o;
                    self.record_spec(root, spec);
                    let frame = strand_scene::LogicalRect::new(
                        o.left,
                        o.top,
                        (logical.w - o.left - o.right).max(0.0),
                        (logical.h - o.top - o.bottom).max(0.0),
                    );
                    boxes = layout(
                        &self.tree,
                        root,
                        RootSize::Fixed(frame),
                        &info,
                        &mut self.scrolls,
                    );
                    self.layout_passes += 1;
                }
            }
            // Sizes logic reads go to it; one a query reads holds the
            // frame for its answer (once per frame).
            let mut query = false;
            for (node, r) in &boxes.rects {
                let Some(watch) = self.tree.get(*node).and_then(|n| n.get(Prop::Watch)) else {
                    continue;
                };
                let size = (r.w, r.h);
                if self.facts_sent.get(&(id, *node)) != Some(&size) {
                    self.facts_sent.insert((id, *node), size);
                    self.facts.push((*node, r.w, r.h));
                    query |= matches!(watch, PropValue::Keyword(k) if k == "query");
                }
            }
            let (wait, window) = (self.query_wait, self.busy_window);
            let seq = self.facts_seq + 1;
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.boxes = Some(boxes);
                s.layout_dirty = false;
                if query {
                    s.hold_for_query(seq, wait, window);
                }
            }
        }
        let Some(s) = self.surfaces.get(&id) else {
            return Flattened::default();
        };
        let empty = Boxes::default();
        let boxes = s.boxes.as_ref().unwrap_or(&empty);
        flatten(&self.tree, s.root, s.size, s.scale, &layouts, boxes)
    }

    /// Sends requests for text whose spec changed. Returns true if a layout
    /// was delivered synchronously (inline backend).
    fn request_text(&mut self, needs: &[(NodeId, TextSpec)]) -> bool {
        let mut delivered = false;
        for (node, spec) in needs {
            let slot = TextSlot::of(*node, spec);
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
            return total;
        }

        s.history.push_front(frame);
        s.history.truncate(DAMAGE_HISTORY);
        s.valid = true;
        s.painted = true;
        s.query_hold = None;
        s.query_held = false;
        let scale = s.scale;
        self.raster
            .paint(&f.items, &total, &self.atlas, scale, target);
        // Nothing changed since flattening: the next paint can reuse it.
        // Stamped once the frame is drawn: a slow raster does not age the
        // surface into looking idle.
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.cache = Some(f);
            s.painted_at = Some(Instant::now());
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

    /// The text state of `n` at scale 1 (tests show one width per node).
    fn text_at(r: &Renderer, n: NodeId) -> &TextState {
        r.texts
            .iter()
            .find(|(s, _)| s.node == n && s.scale == Scale::ONE)
            .map(|(_, t)| t)
            .unwrap()
    }

    /// A bar with one full-width text aligned `align`.
    fn aligned_text(text: &str, align: &str) -> (SceneDiff, NodeId, NodeId) {
        let (root, txt) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let mut d = SceneDiff::new();
        d.create(root, NodeKind::Bar, None, 0)
            .set(root, Prop::Color, PropValue::Color(Color::WHITE))
            .create(txt, NodeKind::Text, Some(root), 0)
            .set(txt, Prop::Text, PropValue::Text(text.into()))
            .set(
                txt,
                Prop::Width,
                PropValue::Length(strand_scene::Length::Percent(100.0)),
            )
            .set(txt, Prop::Align, PropValue::Keyword(align.into()));
        (d, root, txt)
    }

    fn paint_sized(r: &mut Renderer, id: u32, size: Size, age: u8) -> (Vec<u8>, Damage) {
        let mut px = vec![0u8; size.w as usize * size.h as usize * 4];
        let mut t = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, age).unwrap();
        let d = r.paint(SurfaceId(id), &mut t);
        (px, d)
    }

    /// The frame a renderer showing only this surface paints from scratch.
    fn alone(diffs: &[SceneDiff], root: NodeId, size: Size) -> Vec<u8> {
        let mut r = renderer();
        for d in diffs {
            assert!(r.apply(d.clone()).is_empty());
        }
        r.attach_surface(SurfaceId(1), root);
        r.configure_surface(SurfaceId(1), size, Scale::ONE);
        paint_sized(&mut r, 1, size, 0).0
    }

    /// One text node on two surfaces of the same scale but different widths
    /// (a 2560 and a 1920 monitor at scale 1): alignment happens in the
    /// line box, so each surface needs its own layout. Every frame on each
    /// equals that surface painted alone, from the first and after a change.
    #[test]
    fn one_text_on_two_widths_at_one_scale_aligns_on_each() {
        for align in ["center", "end"] {
            let (wide, narrow) = (Size::new(200, 20), Size::new(120, 20));
            let (d, root, txt) = aligned_text("12:59", align);
            let mut r = renderer();
            assert!(r.apply(d.clone()).is_empty());
            r.attach_surface(SurfaceId(1), root);
            r.attach_surface(SurfaceId(2), root);
            r.configure_surface(SurfaceId(1), wide, Scale::ONE);
            r.configure_surface(SurfaceId(2), narrow, Scale::ONE);
            let (a, _) = paint_sized(&mut r, 1, wide, 0);
            let (b, _) = paint_sized(&mut r, 2, narrow, 0);
            assert!(
                a == alone(std::slice::from_ref(&d), root, wide),
                "{align}: wide boot"
            );
            assert!(
                b == alone(std::slice::from_ref(&d), root, narrow),
                "{align}: narrow boot"
            );
            assert_eq!(r.texts.len(), 1, "one unbounded layout for both widths");

            let mut tick = SceneDiff::new();
            tick.set(txt, Prop::Text, PropValue::Text("13:00".into()));
            assert!(r.apply(tick.clone()).is_empty());
            // Paint the narrow one first this time.
            let mut nb = b.clone();
            let mut t = PaintTarget::new(&mut nb, narrow, narrow.w * 4, Scale::ONE, 1).unwrap();
            let dn = r.paint(SurfaceId(2), &mut t);
            let mut na = a.clone();
            let mut t = PaintTarget::new(&mut na, wide, wide.w * 4, Scale::ONE, 1).unwrap();
            let dw = r.paint(SurfaceId(1), &mut t);
            let both = [d.clone(), tick];
            assert!(na == alone(&both, root, wide), "{align}: wide tick");
            assert!(nb == alone(&both, root, narrow), "{align}: narrow tick");
            assert!(dn.area() < narrow.w as u64 * 20 && dw.area() < wide.w as u64 * 20);
            assert_eq!(r.texts.len(), 1, "replaced layouts are not kept");
            r.detach_surface(SurfaceId(2));
            assert_eq!(r.texts.len(), 1);
        }
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

    /// Text the worker delivers while a content-sized surface is being
    /// painted resizes it: the paint refreshes its spec first and draws
    /// nothing at the old size, holding the frame for the configure.
    #[test]
    fn text_collected_while_painting_holds_a_resized_surface() {
        let (root, txt) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let mut d = SceneDiff::new();
        d.create(root, NodeKind::Panel, None, 0)
            .set(root, Prop::Color, PropValue::Color(Color::WHITE))
            .create(txt, NodeKind::Text, Some(root), 0)
            .set(txt, Prop::Text, PropValue::Text("short".into()));
        let mut r = Renderer::new(worker());
        r.set_resize_wait(Duration::from_secs(30));
        assert!(r.apply(d).is_empty());
        assert!(r.wait_for_text(Duration::from_secs(10)));
        r.update();
        let size = |r: &Renderer| {
            let s = r.surface_spec(root).unwrap();
            Size::new(s.width.unwrap() as u32, s.height.unwrap() as u32)
        };
        let first = size(&r);
        r.attach_surface(SurfaceId(1), root);
        r.configure_surface(SurfaceId(1), first, Scale::ONE);
        assert!(r.wait_for_text(Duration::from_secs(10)));
        r.update();
        assert!(!paint_sized(&mut r, 1, first, 0).1.is_empty());
        r.take_surface_changes();
        let mut d = SceneDiff::new();
        d.set(
            txt,
            Prop::Text,
            PropValue::Text("a much longer line of text".into()),
        );
        r.apply(d);
        // Shaped meanwhile, collected by the paint.
        std::thread::sleep(Duration::from_millis(300));
        let (_, damage) = paint_sized(&mut r, 1, first, 1);
        assert!(damage.is_empty(), "painted at the old size");
        assert!(r.frame_deadline(SurfaceId(1)).is_some());
        assert!(size(&r).w > first.w);
        assert!(!r.take_surface_changes().is_empty());
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
        let key_of = |r: &Renderer, n: NodeId| text_at(r, n).requested.as_ref().unwrap().0;
        // Flattening without polling the worker: what `update` would ask.
        let ask = |r: &mut Renderer| {
            r.surfaces.get_mut(&SurfaceId(1)).unwrap().cache = None;
            r.flatten_surface(SurfaceId(1));
        };
        let ka = key_of(&r, a);
        r.deliver(TextLayout::reset(ka, Scale::ONE));
        ask(&mut r);
        assert!(text_at(&r, a).poisoned);
        assert!(text_at(&r, a).requested.is_none(), "not asked again");
        // A second crash (here: the other text) keeps the first culprit
        // poisoned too.
        let kb = key_of(&r, b);
        r.deliver(TextLayout::reset(kb, Scale::ONE));
        ask(&mut r);
        assert!(r.pending.is_empty(), "{:?}", r.pending);
        assert!(text_at(&r, a).poisoned && text_at(&r, b).poisoned);
        // New text for the culprit is asked for.
        let mut d = SceneDiff::new();
        d.set(a, Prop::Text, PropValue::Text("other".into()));
        r.apply(d);
        assert!(r.wait_for_text(Duration::from_secs(10)));
        assert!(text_at(&r, a).layout.is_some());
        assert!(!text_at(&r, a).poisoned);
    }

    /// A poisoned slot never gets a layout, so it does not hold on to the
    /// node's layouts at other widths as stand-ins: when the other surface
    /// goes, its layout goes too.
    #[test]
    fn a_poisoned_slot_keeps_no_stand_ins() {
        let mut r = Renderer::new(worker());
        let (mut d, root, txt) = aligned_text(
            "a window title much too long to fit on either of the two bars",
            "center",
        );
        d.set(txt, Prop::Ellipsis, PropValue::Keyword("end".into()));
        assert!(r.apply(d).is_empty());
        r.attach_surface(SurfaceId(1), root);
        r.attach_surface(SurfaceId(2), root);
        r.configure_surface(SurfaceId(1), Size::new(200, 20), Scale::ONE);
        r.configure_surface(SurfaceId(2), Size::new(120, 20), Scale::ONE);
        // The unbounded layout first: then each width asks for its own.
        let start = std::time::Instant::now();
        while r
            .texts
            .iter()
            .all(|(s, t)| s.width.is_some() || t.layout.is_none())
        {
            r.poll_text();
            std::thread::sleep(Duration::from_millis(1));
            assert!(start.elapsed() < Duration::from_secs(10));
        }
        for id in [1, 2] {
            r.surfaces.get_mut(&SurfaceId(id)).unwrap().cache = None;
            r.flatten_surface(SurfaceId(id));
        }
        // The engine crashes on the wide one's request.
        let wide = Some(200f32.to_bits());
        let key = r
            .texts
            .iter()
            .find(|(s, _)| s.node == txt && s.width == wide)
            .and_then(|(_, t)| t.requested.as_ref())
            .unwrap()
            .0;
        r.deliver(TextLayout::reset(key, Scale::ONE));
        for id in [1, 2] {
            r.surfaces.get_mut(&SurfaceId(id)).unwrap().cache = None;
            r.flatten_surface(SurfaceId(id));
        }
        assert!(r.wait_for_text(Duration::from_secs(10)));
        let slot = |r: &Renderer, w: f32| {
            r.texts
                .iter()
                .find(|(s, _)| s.width == Some(w.to_bits()))
                .map(|(_, t)| (t.poisoned, t.layout.is_some()))
        };
        assert_eq!(slot(&r, 200.0), Some((true, false)));
        assert_eq!(slot(&r, 120.0), Some((false, true)));
        r.update();
        let s1 = &r.surfaces[&SurfaceId(1)];
        assert!(s1.cache.is_some());
        r.detach_surface(SurfaceId(2));
        assert_eq!(slot(&r, 120.0), None, "kept as a stand-in for nothing");
        assert_eq!(
            r.texts.len(),
            2,
            "the unbounded layout and the poisoned one"
        );
        // Surface 1 drew that layout as its stand-in: it must not keep
        // showing glyphs that are gone.
        let s1 = &r.surfaces[&SurfaceId(1)];
        assert!(s1.dirty && s1.cache.is_none(), "stale stand-in kept");
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
        let state = |r: &Renderer, n| text_at(r, n).incomplete();
        let n = |r: &Renderer, x| text_at(r, x).layout.as_ref().unwrap().glyphs().count();
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
        assert_eq!(text_at(&r, a).layout.as_ref().unwrap().glyphs().count(), 4);
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

//! The render thread's entry point: applies scene diffs, keeps text layouts
//! flowing, diffs damage and implements [`Painter`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{
    Damage, LogicalPoint, LogicalSize, NodeId, NodeKind, Prop, PropValue, Scale, Size,
    SurfaceChange, SurfaceId, SurfaceSpec,
};
use strand_text::TextKey;

use crate::anim::Animator;
use crate::flatten::{Extras, Flattened, HitBox, NodeRecord};
use crate::layout::{Boxes, ScrollState};
use crate::raster::{AtlasMirror, Raster};
use crate::tree::SceneTree;

mod apply;
#[cfg(feature = "gpu")]
pub(crate) mod backend;

mod feed;
mod frame;
mod layout_pass;
mod lists;
mod pose;
mod specs;
mod surfaces;
mod swap;
mod text;
mod tooltip;
mod wake;

#[cfg(feature = "gpu")]
pub use backend::GPU_WAIT;

pub use feed::{FeedDemand, FeedKind, THUMBNAIL_STEP};
pub use lists::{ListFrames, ScrollInput, ScrollKind};
pub use text::TextBackend;
use text::{TextSlot, TextState};
use tooltip::Tooltip;
use wake::LoopWaker;

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

/// How long an exit pose waits for frames: an exit older than this on a
/// surface that painted nothing for as long (its output asleep or
/// covered, so no frame callback comes) ends at once, so removed subtrees
/// and closing surfaces never pile up. Any exit ends after
/// `MAX_MOTION` plus this.
pub const EXIT_STALL: Duration = Duration::from_secs(1);

/// Ghosts (removed nodes playing their exit pose) one parent keeps at
/// most: a new one ends the oldest's exit.
pub const MAX_GHOSTS_PER_PARENT: usize = 8;

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
    /// Something on it is moving (a spring, a pose, a glide): it wants
    /// frames until everything settles.
    animating: bool,
    /// Size springs are moving: the next frame lays out again (only the
    /// subtrees under their nearest size-stable ancestors).
    anim_layout: bool,
    /// Parents whose children changed (created, removed, moved, a
    /// layout length snapped): the next layout pass glides their
    /// children from where they were (FLIP). `flip_all`: every node
    /// (a token swap).
    flip: HashSet<NodeId>,
    flip_all: bool,
    /// Presentation time of the last frame it painted.
    painted_time: Option<Duration>,
    /// Nodes with motion state its last fresh frame drew. With
    /// `records`, what it shows: a node another surface showing the same
    /// root (`screens: all`) does not draw keeps its motions while this
    /// one still does.
    drawn: HashSet<NodeId>,
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
    /// (M4) List windows asked of logic, touchpad scrolls, gap stats
    /// (`lists.rs`).
    lists: lists::Lists,
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
    /// Every spring, pose and glide.
    anim: Animator,
    /// `reduced_motion` as the host set it ([`Renderer::set_reduced_motion`]).
    reduced_motion: bool,
    /// When the frame loop's current run of paints began (none while
    /// nothing paints): what the paint cache holds unused since then is
    /// freed when the run ends.
    burst: Option<Instant>,
    /// Surfaces whose closing pose finished: their spec reports them
    /// closed until they open again.
    closed: HashSet<NodeId>,
    /// Content-sized surface nodes whose spec was held at a larger size
    /// than their content while something on them moved: they shrink
    /// once nothing does (see [`Renderer::release_holds`]).
    held: BTreeSet<NodeId>,
    /// See [`EXIT_STALL`].
    exit_stall: Duration,
    /// Content logic removed from a surface playing its closing pose (a
    /// popup unmounts its content when it closes), by surface node: kept
    /// as ghosts, drawn at rest, until the surface closes or opens again.
    closing_content: BTreeMap<NodeId, Vec<NodeId>>,
    /// Surface nodes the diff being applied closes (`open: false`).
    closing_now: BTreeSet<NodeId>,
    /// Nodes the diff being applied creates on a surface not shown: if
    /// the same diff opens that surface (the first toast and `open:
    /// shown.len > 0`), they play their `enter` pose on its first frame.
    born: Vec<NodeId>,
    /// Surface nodes that opened (reported closed, then open) and have
    /// not painted a clocked frame yet: nodes created under them in the
    /// meantime (a second toast in the configure round trip) enter too.
    opening: BTreeSet<NodeId>,
    /// Nodes laid out by the last layout step (tests: a size spring lays
    /// out only the subtree under its nearest size-stable ancestor).
    laid_out_nodes: usize,
    /// Theme swaps: palette roots in flight, crossfades (`swap.rs`).
    swap: swap::Swap,
    /// What flattening reads besides the tree (compositor blur, widget
    /// state, images).
    extras: Extras,
    /// The tooltip waiting to show or shown (`tooltip: expr`).
    tooltip: Option<Tooltip>,
    tooltip_delay: Duration,
    /// Tooltips shown so far (their overlay ids' generation).
    tooltip_seq: u32,
    /// Wakes the render loop from another thread (the text worker's
    /// waker), so a tooltip shows when its delay ends.
    waker: Option<LoopWaker>,
    /// One timer thread for the render loop's own wakes (a tooltip's
    /// delay, a stalled exit's end: [`Renderer::next_wake`]), re-armed
    /// with the earliest due time and cancelled with `None` (started on
    /// first use).
    timer: Option<std::sync::mpsc::Sender<Option<Instant>>>,
    /// The due time last sent to `timer`.
    timer_due: Option<Instant>,
    /// (M4) Per-surface clocks: nodes reading time each surface drew.
    clocks: crate::clock::Clocks,
    /// (M4) The GPU backend: promotion, the device, lowering, passes.
    #[cfg(feature = "gpu")]
    gpu: backend::GpuState,
}

/// How long the pointer rests on a node before its `tooltip` shows.
pub const TOOLTIP_DELAY: Duration = Duration::from_millis(600);

impl Renderer {
    pub fn new(text: TextBackend) -> Self {
        // With a text worker, images decode on a worker of their own that
        // wakes the render loop through the same waker.
        let images = match &text {
            TextBackend::Worker(w) => {
                crate::image::ImageWorker::spawn(crate::image::IconTheme::system(), w.waker())
                    .map(crate::image::ImageBackend::Worker)
                    .unwrap_or_else(|_| {
                        crate::image::ImageBackend::Inline(crate::image::IconTheme::system())
                    })
            }
            TextBackend::Inline(_) => {
                crate::image::ImageBackend::Inline(crate::image::IconTheme::system())
            }
        };
        let extras = Extras {
            images: crate::image::ImageStore::new(images),
            ..Extras::default()
        };
        let waker = match &text {
            TextBackend::Worker(w) => w
                .waker()
                .map(|w| LoopWaker(Arc::new(std::sync::Mutex::new(w)))),
            TextBackend::Inline(_) => None,
        };
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
            lists: lists::Lists::default(),
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
            anim: Animator::default(),
            reduced_motion: false,
            burst: None,
            closed: HashSet::new(),
            held: BTreeSet::new(),
            exit_stall: EXIT_STALL,
            closing_content: BTreeMap::new(),
            closing_now: BTreeSet::new(),
            born: Vec::new(),
            opening: BTreeSet::new(),
            laid_out_nodes: 0,
            swap: swap::Swap::default(),
            extras,
            tooltip: None,
            tooltip_delay: TOOLTIP_DELAY,
            tooltip_seq: 0,
            timer: None,
            timer_due: None,
            waker,
            clocks: crate::clock::Clocks::default(),
            #[cfg(feature = "gpu")]
            gpu: backend::GpuState::default(),
        }
    }

    /// How long a cached shadow or gradient nothing draws lives
    /// (10 s; tests shorten it).
    pub fn set_paint_cache_idle(&mut self, idle: Duration) {
        self.raster.set_idle_free(idle);
    }

    /// (M4) The offscreen group cache: bytes kept, groups drawn so far,
    /// and groups kept (`crate::offscreen`).
    pub fn offscreen_cache(&self) -> (usize, u64, usize) {
        let o = self.raster.offscreen();
        (o.bytes(), o.builds(), o.len())
    }

    /// (M4) Notices for CPU fallbacks drawn in place of GPU effects (a
    /// still aurora, particles capped at 1,000), each once a run: the host
    /// logs them and sends them to `strand watch`.
    pub fn take_effect_notices(&mut self) -> Vec<String> {
        self.extras.fallbacks.take()
    }

    /// (M4) Pixmaps the raster nodes drew so far, and their bytes kept.
    pub fn raster_nodes(&self) -> (u64, usize) {
        (self.extras.rasters.builds(), self.extras.rasters.bytes())
    }

    /// Decodes images and icons inline with icons from `theme` (offline
    /// renders and tests; a worker-backed renderer decodes off-thread).
    pub fn set_icon_theme_inline(&mut self, theme: &str) {
        self.extras
            .images
            .set_backend(crate::image::ImageBackend::Inline(
                crate::image::IconTheme::named(theme),
            ));
    }

    /// The icon theme changed (`index.theme`, a theme installed or
    /// switched, an icon added: the watcher's `CacheKind::Icons`): the
    /// shared lookup ([`strand_icons::invalidate`]) and every icon decode
    /// are forgotten, and the surfaces drawing icons repaint with them
    /// looked up afresh.
    pub fn icons_changed(&mut self) {
        strand_icons::invalidate();
        for id in self.extras.images.invalidate_icons() {
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.mark_dirty();
            }
        }
    }

    /// The installed fonts changed (a fontconfig directory: the watcher's
    /// `CacheKind::Fonts`): the text engine looks them up afresh and every
    /// text is shaped again (the old frame stays on screen until the new
    /// text is in, as for a first frame).
    pub fn fonts_changed(&mut self) {
        match &mut self.text {
            TextBackend::Worker(w) => {
                // Its reset layout (key 0) arrives through `update`, which
                // forgets every layout and asks again. A gone worker keeps
                // the last layouts (as `update` does).
                let _ = w.reload_fonts();
            }
            TextBackend::Inline(engine) => {
                engine.reload_fonts();
                self.reset_text();
            }
        }
    }

    /// Text layouts or image decodes are on their workers: a frame
    /// painted now is not the last one its content asks for (the
    /// arrival repaints).
    pub fn work_pending(&self) -> bool {
        self.text_pending() || self.extras.images.pending() > 0
    }

    /// Bytes of decoded images held (at most
    /// [`crate::image::IMAGE_CACHE_BYTES`] beyond what one frame draws).
    pub fn image_bytes(&self) -> usize {
        self.extras.images.bytes()
    }

    /// The compositor blurs behind surfaces (`ext-background-effect-v1`,
    /// M4): `blur` stops drawing its tint fallback (alpha + 0.15).
    pub fn set_compositor_blur(&mut self, on: bool) {
        if self.extras.compositor_blur != on {
            self.extras.compositor_blur = on;
            for s in self.surfaces.values_mut() {
                s.mark_dirty();
            }
        }
    }

    /// (M4) The pointer on `surface` (`Router::pointer`; `None` once it
    /// left): `parallax` and `tilt` follow it, so a surface drawing them
    /// repaints when it moves.
    pub fn set_pointer(&mut self, surface: SurfaceId, at: Option<strand_scene::LogicalPoint>) {
        let Some(root) = self.surfaces.get(&surface).map(|s| s.root) else {
            return;
        };
        let changed = match at {
            Some(p) => self.extras.pointers.insert(root, p) != Some(p),
            None => self.extras.pointers.remove(&root).is_some(),
        };
        let tree = &self.tree;
        // (M4) A pass that reads the pointer (a shader's
        // `strand.pointer`, glass's highlight) follows it too.
        #[cfg(feature = "gpu")]
        let reads = self.gpu.pointer_users.contains(&surface);
        #[cfg(not(feature = "gpu"))]
        let reads = false;
        if changed
            && (reads || self.anim.leans(|id| tree.root_of(id) == Some(root)))
            && let Some(s) = self.surfaces.get_mut(&surface)
        {
            s.mark_dirty();
        }
    }

    /// (M4) Where `surface` lies on its output (`None`: not known), from
    /// the surface manager's placement (`SurfaceHost::surface_placed`):
    /// a shared-element morph into a node on it can start from a box last
    /// drawn on another surface of the same output. A surface given no
    /// output name (a popup) takes its parent surface's, looked up again
    /// whenever any surface is placed, moves or goes: a popup placed
    /// before its parent gets an output once the parent is placed, and
    /// follows it to another monitor.
    pub fn set_surface_origin(
        &mut self,
        surface: SurfaceId,
        origin: Option<(Option<String>, strand_scene::LogicalPoint)>,
    ) {
        let Some(root) = self.surfaces.get(&surface).map(|s| s.root) else {
            return;
        };
        match origin {
            Some(o) => {
                self.extras.placed.insert(root, o);
            }
            None => {
                self.extras.placed.remove(&root);
            }
        }
        self.resolve_origins();
    }

    /// Rebuilds `extras.origins` from what the host placed, each surface
    /// with no output of its own taking the nearest placed ancestor
    /// surface's (a popup of a popup walks up to the layer surface).
    pub(crate) fn resolve_origins(&mut self) {
        let tree = &self.tree;
        let placed = &self.extras.placed;
        let output_of = |root: NodeId| -> Option<String> {
            let mut at = root;
            // A surface tree is shallow; the bound only guards a cycle.
            for _ in 0..16 {
                if let Some(o) = placed.get(&at).and_then(|(o, _)| o.clone()) {
                    return Some(o);
                }
                let parent = tree.get(at)?.parent?;
                at = tree.root_of(parent)?;
            }
            None
        };
        self.extras.origins = placed
            .iter()
            .filter_map(|(root, (_, at))| {
                let output = output_of(*root)?;
                Some((*root, crate::flatten::SurfaceOrigin { output, at: *at }))
            })
            .collect();
    }

    /// (M4) Where `surface` lies on its output, as last set.
    pub fn surface_origin(&self, surface: SurfaceId) -> Option<&crate::flatten::SurfaceOrigin> {
        let root = self.surfaces.get(&surface)?.root;
        self.extras.origins.get(&root)
    }

    /// Hover, press, focus, carets and slider drags as the input router
    /// last set them.
    pub fn widgets(&self) -> &crate::widgets::Widgets {
        &self.extras.widgets
    }

    /// Repaints the surfaces showing `node`.
    fn mark_node_dirty(&mut self, node: NodeId) {
        let root = self.tree.root_of(node);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_dirty();
            }
        }
    }

    /// `node`'s input `flag` (see [`crate::InputScene::set_flag`]).
    pub fn set_widget_flag(&mut self, node: NodeId, flag: crate::Flag, on: bool) {
        let w = &mut self.extras.widgets;
        let set = match flag {
            crate::Flag::Hover => &mut w.hovered,
            crate::Flag::Pressed => &mut w.pressed,
            crate::Flag::Focused => &mut w.focused,
            crate::Flag::Selected => return,
        };
        let changed = if on {
            set.insert(node)
        } else {
            set.remove(&node)
        };
        // Only widgets draw these states.
        let draws = self.tree.get(node).is_some_and(|n| {
            matches!(
                n.kind,
                NodeKind::Button | NodeKind::Slider | NodeKind::Input | NodeKind::Segmented
            )
        });
        if changed && draws {
            self.mark_node_dirty(node);
        }
        if changed && matches!(flag, crate::Flag::Hover | crate::Flag::Pressed) {
            self.refresh_tooltip();
        }
    }

    /// An `input`'s caret and selection (`None`: at the end of its text).
    pub fn set_caret(&mut self, node: NodeId, caret: Option<crate::widgets::Caret>) {
        let w = &mut self.extras.widgets;
        let old = match caret {
            Some(c) => w.carets.insert(node, c),
            None => w.carets.remove(&node),
        };
        if old != caret {
            self.mark_node_dirty(node);
        }
    }

    /// A slider's value while dragged; `None` ends the drag (it draws its
    /// `value` again).
    pub fn set_drag(&mut self, slider: NodeId, value: Option<f32>) {
        let w = &mut self.extras.widgets;
        let old = match value {
            Some(v) => w.drags.insert(slider, v),
            None => w.drags.remove(&slider),
        };
        if old != value {
            self.mark_node_dirty(slider);
        }
    }

    /// The byte offset in `input`'s text nearest to the surface-logical
    /// point `at`, as its last frame drew it.
    pub fn caret_at(&self, surface: SurfaceId, input: NodeId, at: LogicalPoint) -> Option<usize> {
        let s = self.surfaces.get(&surface)?;
        let text = match self.tree.get(input)?.get(Prop::Text) {
            Some(PropValue::Text(t)) => t.as_str(),
            _ => "",
        };
        let Some((l, x0)) = s.cache.as_ref().and_then(|f| f.inputs.get(&input)) else {
            // Nothing typed (or not drawn yet): the start.
            return Some(0);
        };
        let shown = crate::flatten::caret_index(l, at.x - x0);
        let password = matches!(
            self.tree.get(input)?.get(Prop::InputType),
            Some(PropValue::Keyword(k)) if k == "password"
        );
        if !password {
            return Some(shown.min(text.len()));
        }
        // Bullets: one per character.
        let i = shown / '•'.len_utf8();
        Some(text.char_indices().nth(i).map_or(text.len(), |(b, _)| b))
    }

    /// Bytes of cached gradient and shadow pixmaps (at most
    /// [`crate::PAINT_CACHE_BYTES`] plus what one frame needs), and how
    /// many were built so far.
    pub fn paint_cache(&self) -> (usize, u64) {
        let c = self.raster.cache();
        (c.bytes(), c.builds())
    }

    /// `reduced_motion` (from the system or a setting): every spring,
    /// pose and glide snaps. The global token `motion.reduced: true` turns
    /// it on too.
    pub fn set_reduced_motion(&mut self, on: bool) {
        self.reduced_motion = on;
        self.refresh_reduced();
    }

    /// True while motion is reduced (by the host or the token table).
    pub fn reduced_motion(&self) -> bool {
        self.anim.reduced()
    }

    fn refresh_reduced(&mut self) {
        let token = matches!(
            self.tree.tokens.get("motion.reduced"),
            Some(PropValue::Bool(true))
        );
        let on = self.reduced_motion || token;
        if on != self.anim.reduced() {
            // Time signals stop at `t = 0` (or run again): every surface
            // flattens afresh (one frame each).
            for s in self.surfaces.values_mut() {
                s.mark_dirty();
            }
        }
        if on && !self.anim.reduced() {
            // Springs in flight snap at the next frame: sizes too, which
            // only a layout pass lets go of.
            for s in self.surfaces.values_mut() {
                if self.anim.busy(&self.tree, s.root) {
                    s.mark_layout();
                }
            }
        }
        self.anim.set_reduced(on);
    }

    /// Nodes laid out by the last layout step of any surface.
    pub fn last_layout_nodes(&self) -> usize {
        self.laid_out_nodes
    }

    /// True while something on `surface` is moving (springs unsettled):
    /// it wants frames until it settles.
    pub fn animating(&self, surface: SurfaceId) -> bool {
        self.surfaces.get(&surface).is_some_and(|s| s.animating)
    }

    /// True while any surface is moving: frames are still to come.
    pub fn in_motion(&self) -> bool {
        self.surfaces.values().any(|s| s.animating)
    }

    /// True if the surface node `root` is on screen with a clock: a
    /// change there animates (a frame at time zero, offline, snaps).
    fn shown(&self, root: Option<NodeId>) -> bool {
        root.is_some_and(|r| {
            self.surfaces
                .values()
                .any(|s| s.root == r && s.painted && s.painted_time.is_some_and(|t| !t.is_zero()))
        })
    }

    /// The surfaces showing surface node `root` lay out again.
    fn mark_layout_of(&mut self, root: NodeId) {
        for s in self.surfaces.values_mut() {
            if s.root == root {
                s.mark_layout();
            }
        }
    }

    /// The children of `parent` change place: the next layout of its
    /// surfaces glides them (FLIP).
    fn flip(&mut self, parent: Option<NodeId>) {
        let Some(p) = parent else { return };
        let root = self.tree.root_of(p);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.flip.insert(p);
            }
        }
    }

    /// Something on surface node `id` moves or is about to (a spring, a
    /// pose, a glide still to start).
    fn moving(&self, id: NodeId) -> bool {
        self.anim.busy(&self.tree, id)
            || self
                .surfaces
                .values()
                .any(|s| s.root == id && (s.animating || s.flip_all || !s.flip.is_empty()))
    }

    /// (M4) Why the GPU is not drawing: this build has no GPU backend.
    #[cfg(not(feature = "gpu"))]
    pub fn gpu_status(&self) -> strand_scene::GpuStatus {
        strand_scene::GpuStatus::Unavailable {
            reason: strand_scene::GpuStatus::NOT_BUILT.into(),
        }
    }

    /// (M4) True while a surface shows a `shader` node (logic hears the
    /// status only then).
    #[cfg(not(feature = "gpu"))]
    pub fn gpu_in_demand(&self) -> bool {
        self.surfaces.values().any(|s| {
            s.records.keys().any(|id| {
                self.tree
                    .get(*id)
                    .is_some_and(|n| n.kind == NodeKind::Shader)
            })
        })
    }

    /// The retained tree, read-only (inspector, tests).
    pub fn tree(&self) -> &SceneTree {
        &self.tree
    }
}

#[cfg(test)]
mod tests;

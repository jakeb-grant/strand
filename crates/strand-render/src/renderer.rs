//! The render thread's entry point: applies scene diffs, keeps text layouts
//! flowing, diffs damage and implements [`Painter`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{
    Anchor, Damage, Edge, Insets, LogicalPoint, LogicalRect, LogicalSize, NodeId, NodeKind,
    PaintTarget, Painter, Prop, PropValue, Scale, SceneDiff, SceneOp, Size, SurfaceChange,
    SurfaceId, SurfaceSpec, TokenScope,
};
use strand_text::{TextEngine, TextError, TextKey, TextLayout, TextRequest, TextWorker};

use crate::anim::{Animator, ExitKind, SizeMap, exit_pose, is_pose};
use crate::flatten::{
    Extras, Flattened, HitBox, NodeRecord, Shaped, TextSpec, flatten, natural_texts, pick,
    scope_tables,
};
use crate::layout::{Boxes, MAX_CONTENT_SIZE, RootSize, ScrollState, TextSizes, layout};
use crate::raster::{AtlasMirror, Raster};
use crate::tree::{SceneError, SceneTree};
use strand_scene::Curve;

mod swap;

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
    /// See [`TextSpec::part`].
    part: u8,
}

impl TextSlot {
    fn of(node: NodeId, spec: &TextSpec) -> Self {
        Self {
            node,
            scale: spec.scale,
            width: spec.max_width.map(f32::to_bits),
            part: spec.part,
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
    /// Surfaces whose closing pose finished: their spec reports them
    /// closed until they open again.
    closed: HashSet<NodeId>,
    /// Content-sized surface nodes whose spec was held at a larger size
    /// than their content while something on them moved: they shrink
    /// once nothing does (see [`Renderer::release_holds`]).
    held: BTreeSet<NodeId>,
    /// See [`EXIT_STALL`].
    exit_stall: Duration,
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
    /// One timer thread for tooltip delays, re-armed with the latest due
    /// time (started on first use).
    tooltip_timer: Option<std::sync::mpsc::Sender<Instant>>,
}

/// Starts the thread that wakes the render loop at the latest due time
/// it was sent; a newer one replaces the one waited for. It ends when the
/// renderer (the sender) goes.
fn spawn_tooltip_timer(waker: LoopWaker) -> Option<std::sync::mpsc::Sender<Instant>> {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel::<Instant>();
    std::thread::Builder::new()
        .name("strand-tooltip".into())
        .spawn(move || {
            let mut due: Option<Instant> = None;
            loop {
                let next = match due {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
                };
                match next {
                    Ok(d) => due = Some(d),
                    Err(RecvTimeoutError::Timeout) => {
                        due = None;
                        if let Ok(w) = waker.0.lock() {
                            w();
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .ok()?;
    Some(tx)
}

/// The render loop's waker, callable from any thread.
#[derive(Clone)]
struct LoopWaker(Arc<std::sync::Mutex<strand_text::Waker>>);

impl std::fmt::Debug for LoopWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoopWaker")
    }
}

/// How long the pointer rests on a node before its `tooltip` shows.
pub const TOOLTIP_DELAY: Duration = Duration::from_millis(600);

/// A tooltip: the node it describes, its text, when it shows, and its
/// render-owned popup once shown.
#[derive(Clone, Debug)]
struct Tooltip {
    target: NodeId,
    text: String,
    due: Instant,
    popup: Option<NodeId>,
}

/// The overhang a surface asks for: on an axis its anchor leaves centred
/// (both axes for `center`, the cross axis for an edge, none for a bar
/// or a corner), the larger side on both sides, since the compositor
/// centres the whole buffer and an uneven overhang would move the box off
/// centre.
fn centred_overhang(spec: &SurfaceSpec, o: Insets) -> Insets {
    // A popup's box is its window geometry, which the compositor places:
    // its overhang may be uneven.
    if matches!(spec.kind, NodeKind::Bar | NodeKind::Popup) {
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

    fn part(&self, node: NodeId, part: u8) -> Option<strand_scene::LogicalSize> {
        crate::flatten::pick_part(self.shaped.get(&node)?, part, self.scale, None).map(|l| l.size)
    }
}

/// Layout lengths that snap to a new value while the boxes they move
/// glide there (design.md, snap rules); `width`, `height` and `size`
/// spring instead.
fn snaps_and_glides(prop: Prop) -> bool {
    use Prop::*;
    matches!(
        prop,
        MinWidth
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
    )
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
            closed: HashSet::new(),
            held: BTreeSet::new(),
            exit_stall: EXIT_STALL,
            born: Vec::new(),
            opening: BTreeSet::new(),
            laid_out_nodes: 0,
            swap: swap::Swap::default(),
            extras,
            tooltip: None,
            tooltip_delay: TOOLTIP_DELAY,
            tooltip_seq: 0,
            tooltip_timer: None,
            waker,
        }
    }

    /// How long the pointer rests before a tooltip shows (tests shorten
    /// it).
    pub fn set_tooltip_delay(&mut self, delay: Duration) {
        self.tooltip_delay = delay;
    }

    /// The render-owned popup of the tooltip shown, if one is.
    pub fn tooltip_popup(&self) -> Option<NodeId> {
        self.tooltip.as_ref().and_then(|t| t.popup)
    }

    /// The deepest hovered node with a `tooltip`, and its text; none
    /// while a button is held.
    fn tooltip_wanted(&self) -> Option<(NodeId, String)> {
        let w = &self.extras.widgets;
        if !w.pressed.is_empty() {
            return None;
        }
        let depth = |mut n: NodeId| {
            let mut d = 0;
            while let Some(p) = self.tree.get(n).and_then(|x| x.parent) {
                d += 1;
                n = p;
            }
            d
        };
        w.hovered
            .iter()
            .filter(|n| self.tree.contains_live(**n))
            .filter_map(|n| {
                let node = self.tree.get(*n)?;
                let v = node.get(Prop::Tooltip)?;
                let tables = scope_tables(&self.tree, *n);
                let scope = TokenScope::new(&tables);
                match scope.resolve(v).as_deref() {
                    Some(PropValue::Text(t)) if !t.trim().is_empty() => Some((*n, t.clone())),
                    _ => None,
                }
            })
            .max_by_key(|(n, _)| (depth(*n), *n))
    }

    /// Hover or a press changed: the tooltip that should show changes
    /// with it (shown after `tooltip_delay` of rest, hidden at once).
    fn refresh_tooltip(&mut self) {
        let want = self.tooltip_wanted();
        if let (Some(t), Some((n, text))) = (&mut self.tooltip, &want)
            && t.target == *n
        {
            if t.text != *text {
                t.text = text.clone();
                // Shown: its label takes the new text in place, and its
                // popup lays out and resizes (no new surface: a value
                // changing while hovered does not flicker).
                if let Some(p) = t.popup {
                    let label = self.tree.overlay_children(p).first().copied();
                    if let Some(l) = label {
                        self.tree
                            .set_overlay_prop(l, Prop::Text, PropValue::Text(text.clone()));
                    }
                    for s in self.surfaces.values_mut().filter(|s| s.root == p) {
                        s.mark_layout();
                    }
                    self.spec_dirty.insert(p);
                    self.refresh_specs();
                }
            }
            return;
        }
        self.hide_tooltip();
        let Some((target, text)) = want else {
            return;
        };
        let due = Instant::now() + self.tooltip_delay;
        self.tooltip = Some(Tooltip {
            target,
            text,
            due,
            popup: None,
        });
        if self.tooltip_timer.is_none() {
            self.tooltip_timer = self.waker.clone().and_then(spawn_tooltip_timer);
        }
        if let Some(tx) = &self.tooltip_timer
            && tx.send(due).is_err()
        {
            self.tooltip_timer = None;
        }
    }

    fn hide_tooltip(&mut self) {
        if let Some(t) = self.tooltip.take()
            && let Some(p) = t.popup
        {
            self.tree.remove_overlay(p);
            self.refresh_specs();
        }
    }

    /// Shows the waiting tooltip once its delay has passed: a popup under
    /// its node holding its text, styled by the theme's inverse surface
    /// (`$inverse_surface`, `$inverse_on_surface`, `$radius.sm`,
    /// `$font.caption` when the table has them).
    fn show_tooltip(&mut self) {
        let Some(t) = &self.tooltip else {
            return;
        };
        if t.popup.is_some() || Instant::now() < t.due {
            return;
        }
        if !self.tree.contains_live(t.target) {
            self.tooltip = None;
            return;
        }
        let (target, text) = (t.target, t.text.clone());
        self.tooltip_seq = self.tooltip_seq.wrapping_add(1);
        let g = self.tooltip_seq;
        let popup = NodeId::new(crate::tree::OVERLAY_INDEX, g);
        let label = NodeId::new(crate::tree::OVERLAY_INDEX + 1, g);
        let tables = scope_tables(&self.tree, target);
        let scope = TokenScope::new(&tables);
        let token = |path: &str, fallback: PropValue| {
            if scope.lookup(path).is_some() {
                PropValue::Token(strand_scene::TokenExpr::path(path))
            } else {
                fallback
            }
        };
        let entry = |prop, value| crate::tree::PropEntry {
            prop,
            value,
            transition: strand_scene::Transition::Instant,
        };
        let mut props = vec![
            entry(Prop::Name, PropValue::Text("tooltip".into())),
            entry(
                Prop::Bg,
                token(
                    "inverse_surface",
                    PropValue::Color(strand_scene::Color::from_rgba8(30, 30, 36, 240)),
                ),
            ),
            entry(
                Prop::Color,
                token(
                    "inverse_on_surface",
                    PropValue::Color(strand_scene::Color::WHITE),
                ),
            ),
            entry(Prop::Radius, token("radius.sm", PropValue::Number(6.0))),
            entry(
                Prop::Pad,
                PropValue::List(vec![PropValue::Number(4.0), PropValue::Number(8.0)]),
            ),
        ];
        if scope.lookup("font.caption").is_some() {
            props.push(entry(
                Prop::Font,
                PropValue::Token(strand_scene::TokenExpr::path("font.caption")),
            ));
        }
        self.tree.add_overlay(vec![
            crate::tree::Node {
                id: popup,
                kind: NodeKind::Popup,
                parent: Some(target),
                children: vec![label],
                props,
                epoch: 0,
            },
            crate::tree::Node {
                id: label,
                kind: NodeKind::Text,
                parent: Some(popup),
                children: Vec::new(),
                props: vec![entry(Prop::Text, PropValue::Text(text))],
                epoch: 0,
            },
        ]);
        if let Some(t) = &mut self.tooltip {
            t.popup = Some(popup);
        }
        self.spec_dirty.insert(popup);
        self.refresh_specs();
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

    /// `node`'s laid-out box on `surface`, in surface logical pixels
    /// (before paint offsets).
    pub fn node_rect(&self, surface: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        self.surfaces
            .get(&surface)?
            .boxes
            .as_ref()?
            .rects
            .get(&node)
            .copied()
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

    /// Ghosts under surface node `root` still play their exit.
    fn ghosts_under(&self, root: NodeId) -> bool {
        self.anim
            .exits()
            .any(|(id, k)| k == ExitKind::Ghost && self.tree.root_of(id) == Some(root))
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

    /// Unmounts ghosts whose exit finished and closes surfaces whose
    /// closing pose did.
    fn process_finished(&mut self) {
        for (id, kind) in self.anim.take_finished() {
            match kind {
                ExitKind::Ghost => {
                    let parent = self.tree.get(id).and_then(|n| n.parent);
                    let root = self.tree.root_of(id);
                    self.tree.drop_ghost(id);
                    self.flip(parent);
                    for s in self.surfaces.values_mut() {
                        if Some(s.root) == root {
                            s.mark_layout();
                        }
                    }
                    self.spec_dirty.extend(root);
                }
                ExitKind::Close => {
                    self.closed.insert(id);
                    self.spec_dirty.insert(id);
                }
            }
        }
        let tree = &self.tree;
        self.anim.retain(|id| tree.contains(id));
    }

    /// How long an exit waits for frames before it ends at once (see
    /// [`EXIT_STALL`]; tests shorten it).
    pub fn set_exit_stall(&mut self, stall: Duration) {
        self.exit_stall = stall;
    }

    /// Ends exits no frame samples: older than the stall limit on a
    /// surface that painted nothing for as long, or older than any motion
    /// may run.
    fn expire_exits(&mut self) {
        let now = Instant::now();
        let stall = self.exit_stall;
        for (id, started) in self.anim.exit_times() {
            let age = now.saturating_duration_since(started);
            let root = self.tree.root_of(id);
            let painting = self.surfaces.values().any(|s| {
                Some(s.root) == root
                    && s.painted_at
                        .is_some_and(|t| now.saturating_duration_since(t) < stall)
            });
            if age >= strand_scene::motion::MAX_MOTION + stall || (age >= stall && !painting) {
                self.anim.finish_now(id);
            }
        }
    }

    /// When the host's loop must run [`Renderer::update`] even if
    /// nothing else happens: the earliest instant an exit in flight is
    /// ended for want of frames (its output asleep, see [`EXIT_STALL`]),
    /// so a closing surface whose frames stopped still closes and its
    /// ghosts unmount. `None` when no exit is in flight. Arm a timer at
    /// it after every `apply`, `update` and paint.
    pub fn next_wake(&self) -> Option<Instant> {
        let stall = self.exit_stall;
        let tooltip = self
            .tooltip
            .as_ref()
            .filter(|t| t.popup.is_none())
            .map(|t| t.due);
        let exits = self
            .anim
            .exits()
            .filter_map(|(id, _)| {
                let started = self.anim.exit_started(id)?;
                let root = self.tree.root_of(id);
                let last = self
                    .surfaces
                    .values()
                    .filter(|s| Some(s.root) == root)
                    .filter_map(|s| s.painted_at)
                    .max();
                let asleep = last.map_or(started, |t| t.max(started)) + stall;
                Some(asleep.min(started + strand_scene::motion::MAX_MOTION + stall))
            })
            .min();
        match (exits, tooltip) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
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

    /// Content-sized surfaces held at a larger size while things moved
    /// ask for their own size once nothing does, and any spec left dirty
    /// (an exit finished, a surface closed) is refreshed, so the host
    /// sees the change ([`Renderer::has_surface_changes`]) without
    /// waiting for an unrelated event.
    fn release_holds(&mut self) {
        let ready: Vec<NodeId> = self
            .held
            .iter()
            .copied()
            .filter(|id| !self.moving(*id))
            .collect();
        for id in ready {
            self.held.remove(&id);
            self.spec_dirty.insert(id);
        }
        if !self.spec_dirty.is_empty() {
            self.refresh_specs();
        }
    }

    /// Ends exits nobody can see any more (their surface went or never
    /// showed, or it gets no frames).
    fn reap_exits(&mut self) {
        self.expire_exits();
        let mut shown: HashSet<NodeId> = HashSet::new();
        for s in self.surfaces.values() {
            if s.painted {
                shown.insert(s.root);
            }
        }
        let tree = &self.tree;
        self.anim
            .finish_undrawn(|id| tree.root_of(id).is_none_or(|r| !shown.contains(&r)));
        self.process_finished();
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
            if node.kind == NodeKind::Popup {
                // Nested in the surface of the element it is declared in,
                // anchored to that element's laid-out box there; shown only
                // while that surface is.
                let anchor = node.parent;
                let parent = anchor.and_then(|a| self.tree.root_of(a));
                spec.parent = parent;
                spec.tooltip = self.tree.is_overlay(id);
                spec.anchor_rect = anchor.zip(parent).and_then(|(a, p)| {
                    self.surfaces
                        .values()
                        .filter(|s| s.root == p)
                        .find_map(|s| s.boxes.as_ref()?.rects.get(&a).copied())
                });
                if parent.is_some_and(|p| self.specs.get(&p).is_some_and(|ps| !ps.open)) {
                    spec.open = false;
                }
            }
            // Surface poses: `enter` plays when it opens, `exit` before it
            // closes (it stays open until the pose settles).
            let reported = self.specs.get(&id).map(|s| s.open);
            let reduced = self.anim.reduced();
            if spec.open {
                self.closed.remove(&id);
                if self.anim.exiting(id) == Some(ExitKind::Close) {
                    self.anim.cancel_exit(id);
                    // Its pose may have sized it: the next layout lets go.
                    self.mark_layout_of(id);
                } else if reported != Some(true) && !reduced && is_pose(node.get(Prop::Enter)) {
                    self.anim.enter(id);
                }
                // Opened by the diff that created these nodes: they
                // enter with it (a surface that was never reported, at
                // boot, shows at rest).
                if reported == Some(false) && !reduced {
                    if !self.shown(Some(id)) {
                        self.opening.insert(id);
                    }
                    for n in &self.born {
                        if self.tree.root_of(*n) == Some(id)
                            && self.tree.contains_live(*n)
                            && self
                                .tree
                                .get(*n)
                                .is_some_and(|c| is_pose(c.get(Prop::Enter)))
                        {
                            self.anim.enter(*n);
                        }
                    }
                }
            } else if reported == Some(true) && !self.closed.contains(&id) {
                if self.anim.exiting(id) == Some(ExitKind::Close) {
                    spec.open = true;
                } else if !reduced && is_pose(exit_pose(node)) && self.shown(Some(id)) {
                    self.anim.exit(id, ExitKind::Close);
                    self.mark_layout_of(id);
                    spec.open = true;
                } else if !reduced && self.ghosts_under(id) && self.shown(Some(id)) {
                    // Rows still leaving keep it open; it closes when the
                    // last ghost unmounts (which refreshes the specs).
                    spec.open = true;
                }
            }
            if !spec.open || self.shown(Some(id)) {
                self.opening.remove(&id);
            }
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
                    // At rest: exiting nodes take their exit pose's size.
                    let rest = self.anim.rest_sizes(&self.tree, id);
                    let mut b = layout(&self.tree, id, root_size, &info, &mut self.scrolls, &rest);
                    self.layout_passes += 1;
                    if b.unsettled {
                        // A list measured rows it had only estimated: its
                        // size (and so the surface's) comes from the
                        // second pass, as the painted one does.
                        b = layout(&self.tree, id, root_size, &info, &mut self.scrolls, &rest);
                        self.layout_passes += 1;
                    }
                    // Never smaller while something on it moves (a toast
                    // collapsing, its siblings sliding up): it shrinks
                    // once everything settles.
                    let mut held = false;
                    if let Some(old) = old.filter(|_| self.moving(id)) {
                        let (w, h) = (old.width.unwrap_or(0.0), old.height.unwrap_or(0.0));
                        held = b.size.w.ceil() < w || b.size.h.ceil() < h;
                        b.size.w = b.size.w.max(w);
                        b.size.h = b.size.h.max(h);
                    }
                    if held {
                        self.held.insert(id);
                    } else {
                        self.held.remove(&id);
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
        self.held.retain(|id| live.contains(id));
        self.opening.retain(|id| live.contains(id));
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
                drawn: HashSet::new(),
                cache: None,
                wanted: Vec::new(),
                boxes: None,
                layout_dirty: true,
                hits: Vec::new(),
                query_hold: None,
                query_held: false,
                size_hold: None,
                animating: false,
                anim_layout: false,
                flip: HashSet::new(),
                flip_all: false,
                painted_time: None,
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
        self.glide_origin(surface, size, scale);
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.resize(size, scale);
            if !s.painted && s.wait_until.is_none() && size != Size::default() {
                s.wait_until = Some(Instant::now() + wait);
            }
        }
        self.prune_scales();
        self.update();
    }

    /// A shown content-sized surface is about to grow: where its anchor
    /// moves its origin (centred: half the growth; anchored to the right
    /// or bottom edge: all of it), its content is drawn where it was on
    /// screen and glides to its new place, instead of jumping as the
    /// compositor re-centres the buffer.
    fn glide_origin(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        let Some(s) = self.surfaces.get(&surface) else {
            return;
        };
        if !s.painted || s.scale != scale || s.size == size || s.size == Size::default() {
            return;
        }
        let root = s.root;
        let (old, new) = (s.scale.logical_size(s.size), scale.logical_size(size));
        let Some(spec) = self.specs.get(&root) else {
            return;
        };
        if self.anim.reduced() || spec.kind == NodeKind::Bar || !self.content_sized.contains(&root)
        {
            return;
        }
        let (fx, fy) = match spec.anchor {
            Anchor::Center => (0.5, 0.5),
            Anchor::Top => (0.5, 0.0),
            Anchor::Bottom => (0.5, 1.0),
            Anchor::Left => (0.0, 0.5),
            Anchor::Right => (1.0, 0.5),
            Anchor::TopLeft => (0.0, 0.0),
            Anchor::TopRight => (1.0, 0.0),
            Anchor::BottomLeft => (0.0, 1.0),
            Anchor::BottomRight => (1.0, 1.0),
        };
        // Growth only: content kept in place in a smaller buffer would be
        // cut off at its edge (a shrink comes once everything settled,
        // and steps; decisions.md, wave3-pixels fixer round 3).
        let delta = [(new.w - old.w).max(0.0) * fx, (new.h - old.h).max(0.0) * fy];
        if delta.iter().all(|d| d.abs() < 0.5) {
            return;
        }
        let tables = [&self.tree.tokens];
        let curve = Curve::of(
            &TokenScope::new(&tables).transition(&strand_scene::Transition::Default, Prop::X),
        );
        let kids: Vec<NodeId> = self
            .tree
            .get(root)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for k in kids {
            if self.tree.get(k).is_some_and(|n| !n.kind.is_surface()) {
                self.anim.glide(k, delta, curve);
            }
        }
    }

    pub fn detach_surface(&mut self, surface: SurfaceId) {
        self.surfaces.remove(&surface);
        self.forget_fade(surface);
        self.extras.images.forget(surface);
        self.reap_exits();
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
        // A `tooltip` (or the tokens it reads) changing while one waits
        // or shows: its text follows.
        let tooltips = diff.ops.iter().any(|op| {
            matches!(
                op,
                SceneOp::SetProp {
                    prop: Prop::Tooltip,
                    ..
                } | SceneOp::SetTokens { .. }
            )
        });
        let errors = self.apply_ops(diff);
        // Widget state of nodes logic removed goes with them, and a
        // tooltip with its node.
        let tree = &self.tree;
        self.extras.widgets.retain(|n| tree.contains_live(n));
        if self
            .tooltip
            .as_ref()
            .is_some_and(|t| !self.tree.contains_live(t.target))
        {
            self.hide_tooltip();
        }
        if tooltips && !self.extras.widgets.hovered.is_empty() {
            self.refresh_tooltip();
        }
        errors
    }

    fn apply_ops(&mut self, diff: SceneDiff) -> Vec<SceneError> {
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
        // A theme swap is planned against the table and frames on screen.
        let swap = self.plan_swap(&diff);
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
            let result = if self.animate_op(&op) {
                Ok(())
            } else {
                self.tree.apply_op(op)
            };
            match result {
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
        if let Some(plan) = swap {
            self.install_swap(plan);
        }
        if relayout.is_none() {
            self.spec_dirty.extend(self.tree.surface_nodes());
            self.refresh_reduced();
        }
        let tree = &self.tree;
        self.anim.retain(|id| tree.contains(id));
        self.closed.retain(|id| tree.contains(*id));
        self.reap_exits();
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
        self.born.clear();
        errors
    }

    /// What an op starts moving, before it applies: logic's new value of
    /// an animatable prop springs from the old one, a node created on a
    /// shown surface enters, a removed one with an exit pose becomes a
    /// ghost (returns true: the removal is done), and structural changes
    /// glide their siblings. Nothing animates on a surface not shown
    /// with a clock, or under `reduced_motion`.
    fn animate_op(&mut self, op: &SceneOp) -> bool {
        let reduced = self.anim.reduced();
        match op {
            SceneOp::Create { id, parent, .. } => {
                // Logic reused the id of a ghost: the tree unmounts the
                // ghost, and its motions must not carry over.
                if self.tree.is_ghost(*id) {
                    let old = self.tree.get(*id).and_then(|n| n.parent);
                    self.anim.forget(*id);
                    self.flip(old);
                }
                let root = parent.and_then(|p| self.tree.root_of(p));
                if parent.is_some() && !reduced {
                    // On screen, or opening (its first frame still to
                    // come): it enters.
                    let opening = root.is_some_and(|r| self.opening.contains(&r));
                    if opening || self.shown(root) {
                        self.anim.enter(*id);
                    } else {
                        self.born.push(*id);
                    }
                }
                self.flip(*parent);
            }
            SceneOp::Remove { id } => {
                let Some(node) = self.tree.get(*id).filter(|_| self.tree.contains_live(*id)) else {
                    return false;
                };
                let parent = node.parent;
                let root = self.tree.root_of(*id);
                let laid = self.surfaces.values().any(|s| {
                    Some(s.root) == root
                        && s.boxes.as_ref().is_some_and(|b| b.rects.contains_key(id))
                });
                if !node.kind.is_surface()
                    && !reduced
                    && is_pose(exit_pose(node))
                    && laid
                    && self.shown(root)
                    && self.tree.ghost(*id).is_ok()
                {
                    // At most a few ghosts per parent: rows removed faster
                    // than they leave (or with no frames to leave in) end
                    // the oldest exit.
                    if let Some(p) = parent.and_then(|p| self.tree.get(p)) {
                        let mut ghosts: Vec<(Instant, NodeId)> = p
                            .children
                            .iter()
                            .filter(|c| self.anim.exiting(**c) == Some(ExitKind::Ghost))
                            .filter_map(|c| Some((self.anim.exit_started(*c)?, *c)))
                            .collect();
                        ghosts.sort();
                        let over = (ghosts.len() + 1).saturating_sub(MAX_GHOSTS_PER_PARENT);
                        for (_, g) in ghosts.into_iter().take(over) {
                            self.anim.finish_now(g);
                        }
                    }
                    self.anim.exit(*id, ExitKind::Ghost);
                    return true;
                }
                self.flip(parent);
            }
            SceneOp::Move { id, parent, .. } => {
                let old = self.tree.get(*id).and_then(|n| n.parent);
                self.flip(old);
                self.flip(*parent);
            }
            SceneOp::SetProp { id, prop, .. } => {
                if !self.tree.contains_live(*id) {
                    return false;
                }
                let root = self.tree.root_of(*id);
                let animates = !reduced && self.shown(root);
                if animates && crate::anim::ANIMATED.contains(prop) {
                    let tables = scope_tables(&self.tree, *id);
                    let scope = TokenScope::new(&tables);
                    let old = self
                        .tree
                        .get(*id)
                        .and_then(|n| n.get(*prop))
                        .and_then(|v| scope.resolve(v))
                        .map(std::borrow::Cow::into_owned);
                    self.anim.touch(*id, *prop, old);
                }
                if animates && crate::anim::is_size(*prop) {
                    self.anim.touch_size(*id);
                }
                // Layout lengths snap; the boxes they move glide.
                if snaps_and_glides(*prop) {
                    let parent = self.tree.get(*id).and_then(|n| n.parent);
                    self.flip(Some(*id));
                    self.flip(parent);
                }
            }
            SceneOp::SetTokens { .. } => {
                for s in self.surfaces.values_mut() {
                    s.flip_all = true;
                }
            }
        }
        false
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
        self.show_tooltip();
        self.poll_text();
        self.expire_exits();
        self.process_finished();
        self.refresh_specs();
        let ids: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| s.dirty && s.cache.is_none() && s.size != Size::default())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(s) = self.surfaces.get(&id) {
                // A preview: nothing starts until a frame samples it.
                let (time, prev) = (s.time, s.painted_time);
                self.sample_tokens(time, false);
                self.anim.begin(time, prev, false);
            }
            let f = self.flatten_surface(id);
            let animating = self.anim.active() || self.swap_moving(id);
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
                s.animating = animating;
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
        // A preview that laid out a change moving nothing (a row removed
        // at the end, a size set `~ instant`) lets a held surface shrink.
        self.release_holds();
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
        // Decoded images: the surfaces drawing them repaint.
        let arrived = self.extras.images.poll();
        if !arrived.is_empty() {
            let images = &self.extras.images;
            for (id, s) in self.surfaces.iter_mut() {
                if arrived.iter().any(|k| images.drawn_by(*id, k)) {
                    s.mark_dirty();
                }
            }
        }
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
                part: slot.part,
            });
        }
        // Stand-in choice must not depend on hash order.
        for v in out.values_mut() {
            v.sort_by(|a, b| {
                (a.part, a.layout.scale, a.max_width.map(f32::to_bits)).cmp(&(
                    b.part,
                    b.layout.scale,
                    b.max_width.map(f32::to_bits),
                ))
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
            let text = self.request_text(&flat.text);
            // Decoded inline (offline): draw them at once too.
            // While a size springs here, images at a new size draw their
            // last decode scaled and are decoded once it rests.
            let defer = self
                .surfaces
                .get(&id)
                .is_some_and(|s| self.anim.sizes_moving(&self.tree, s.root));
            let images = self.extras.images.want(id, &flat.images, defer);
            if !text && !images {
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
        // A surface a theme swap crossfades while the roots spring
        // elsewhere is drawn from the new table.
        let held = self.hold_tokens(id);
        let f = self.flatten_tokens(id);
        self.release_tokens(held);
        f
    }

    fn flatten_tokens(&mut self, id: SurfaceId) -> Flattened {
        let layouts = self.shaped();
        self.lay_out(id, &layouts);
        let Some(s) = self.surfaces.get(&id) else {
            return Flattened::default();
        };
        let empty = Boxes::default();
        let boxes = s.boxes.as_ref().unwrap_or(&empty);
        flatten(
            &self.tree,
            s.root,
            s.size,
            s.scale,
            &layouts,
            boxes,
            &mut self.anim,
            &self.extras,
        )
    }

    /// One layout pass of `root` (two when a list measured rows it had
    /// estimated), with `sizes` forced.
    fn run_layout(
        &mut self,
        root: NodeId,
        size: RootSize,
        info: &TextInfo<'_>,
        sizes: &SizeMap,
    ) -> Boxes {
        let mut boxes = layout(&self.tree, root, size, info, &mut self.scrolls, sizes);
        self.layout_passes += 1;
        self.laid_out_nodes += boxes.rects.len();
        if boxes.unsettled {
            boxes = layout(&self.tree, root, size, info, &mut self.scrolls, sizes);
            self.layout_passes += 1;
            self.laid_out_nodes += boxes.rects.len();
        }
        boxes
    }

    /// Lays out surface `id` if anything layout reads changed, or its
    /// size springs moved. Sizes spring from the boxes of the last pass
    /// to the ones laid out at rest; a frame where only size springs
    /// moved lays out just the subtrees under their nearest size-stable
    /// ancestors. A structural change glides the boxes it moved from
    /// where they were (FLIP).
    fn lay_out(&mut self, id: SurfaceId, layouts: &HashMap<NodeId, Vec<Shaped>>) {
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        let full = s.layout_dirty || s.boxes.is_none();
        if !full && !s.anim_layout {
            return;
        }
        self.laid_out_nodes = 0;
        let root = s.root;
        let logical = s.scale.logical_size(s.size);
        let o = s.overhang;
        let frame = strand_scene::LogicalRect::new(
            o.left,
            o.top,
            (logical.w - o.left - o.right).max(0.0),
            (logical.h - o.top - o.bottom).max(0.0),
        );
        let info = TextInfo {
            shaped: layouts,
            scale: s.scale,
        };
        // Replaced below on every path: taken, not copied.
        let old = self.surfaces.get_mut(&id).and_then(|s| s.boxes.take());
        let rest = self.anim.rest_sizes(&self.tree, root);
        if !full
            && let Some(old) = &old
            && let Some(boxes) = self.relayout_sized(root, old, &rest, &info)
        {
            let moving = self.anim.sizes_moving(&self.tree, root);
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.boxes = Some(boxes);
                s.anim_layout = moving;
            }
            self.report_facts(id);
            return;
        }
        let sized = self.anim.size_work(&self.tree, root);
        let mut sizes = rest.clone();
        let mut boxes = if sized && !full && !self.anim.size_pending(&self.tree, root) {
            // Only springs moved: their targets are the ones learnt when
            // they started (nothing layout reads changed since), so one
            // pass with the in-flight sizes is enough.
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            self.run_layout(root, RootSize::Fixed(frame), &info, &sizes)
        } else if sized {
            let at_rest = self.run_layout(root, RootSize::Fixed(frame), &info, &rest);
            let old_rects = old.as_ref().map(|b| &b.rects);
            self.anim
                .start_sizes(&self.tree, root, &at_rest.rects, old_rects, false);
            sizes = self.anim.size_overrides(&self.tree, root, &rest);
            if sizes == rest {
                at_rest
            } else {
                self.run_layout(root, RootSize::Fixed(frame), &info, &sizes)
            }
        } else {
            self.run_layout(root, RootSize::Fixed(frame), &info, &rest)
        };
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
                boxes = self.run_layout(root, RootSize::Fixed(frame), &info, &sizes);
            }
        }
        // FLIP: boxes a structural change moved start where they were.
        if let Some(s) = self.surfaces.get_mut(&id) {
            let flip = std::mem::take(&mut s.flip);
            let all = std::mem::take(&mut s.flip_all);
            if let Some(old) = &old
                && (all || !flip.is_empty())
                && !self.anim.reduced()
            {
                let tables = [&self.tree.tokens];
                let curve = Curve::of(
                    &TokenScope::new(&tables)
                        .transition(&strand_scene::Transition::Default, Prop::X),
                );
                glide_moved(
                    &self.tree,
                    &mut self.anim,
                    root,
                    &old.rects,
                    &boxes.rects,
                    &flip,
                    all,
                    curve,
                );
            }
        }
        let moving = self.anim.sizes_moving(&self.tree, root);
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.boxes = Some(boxes);
            s.layout_dirty = false;
            s.anim_layout = moving;
        }
        self.report_facts(id);
    }

    /// Hands the laid-out sizes of watched nodes that changed to logic,
    /// holding the frame for a container query's answer.
    fn report_facts(&mut self, id: SurfaceId) {
        let Some(boxes) = self.surfaces.get(&id).and_then(|s| s.boxes.as_ref()) else {
            return;
        };
        let mut query = false;
        let mut facts = Vec::new();
        for (node, r) in &boxes.rects {
            if self.tree.is_ghost(*node) {
                continue;
            }
            let Some(watch) = self.tree.get(*node).and_then(|n| n.get(Prop::Watch)) else {
                continue;
            };
            let size = (r.w, r.h);
            if self.facts_sent.get(&(id, *node)) != Some(&size) {
                facts.push((*node, size));
                query |= matches!(watch, PropValue::Keyword(k) if k == "query");
            }
        }
        for (node, size) in facts {
            self.facts_sent.insert((id, node), size);
            self.facts.push((node, size.0, size.1));
        }
        let (wait, window) = (self.query_wait, self.busy_window);
        let seq = self.facts_seq + 1;
        if let Some(s) = self.surfaces.get_mut(&id)
            && query
        {
            s.hold_for_query(seq, wait, window);
        }
    }

    /// Lays out only what size springs move: the subtree under each
    /// moving node's nearest size-stable ancestor (a node of fixed width
    /// and height, or a surface root of fixed size), into a copy of
    /// `old`, once, with the in-flight sizes: their targets were learnt
    /// when they started and nothing layout reads changed since. `None`
    /// when that would be the whole surface, or a spring waits to start
    /// (a full pass learns its target).
    fn relayout_sized(
        &mut self,
        root: NodeId,
        old: &Boxes,
        rest: &SizeMap,
        info: &TextInfo<'_>,
    ) -> Option<Boxes> {
        if self.anim.size_pending(&self.tree, root) {
            return None;
        }
        let moving = self.anim.sized_nodes(&self.tree, root);
        let mut subs: Vec<NodeId> = Vec::new();
        for n in moving {
            let mut a = self.tree.get(n)?.parent?;
            loop {
                if a == root {
                    if self.content_sized.contains(&root) {
                        return None;
                    }
                    break;
                }
                if !rest.contains_key(&a) && crate::layout::size_stable(&self.tree, a) {
                    break;
                }
                a = self.tree.get(a)?.parent?;
            }
            if a == root {
                return None;
            }
            subs.push(a);
        }
        let tree = &self.tree;
        let all = subs.clone();
        subs.retain(|a| !all.iter().any(|b| b != a && tree.is_ancestor(*b, *a)));
        subs.sort();
        subs.dedup();
        let mut frames = Vec::with_capacity(subs.len());
        for a in &subs {
            frames.push(*old.rects.get(a)?);
        }
        let sizes = self.anim.size_overrides(&self.tree, root, rest);
        let mut boxes = old.clone();
        for (a, frame) in subs.iter().zip(frames) {
            let b = self.run_layout(*a, RootSize::Fixed(frame), info, &sizes);
            let mut stack: Vec<NodeId> = self.tree.get(*a)?.children.clone();
            while let Some(n) = stack.pop() {
                boxes.rects.remove(&n);
                if let Some(node) = self.tree.get(n) {
                    stack.extend(node.children.iter().copied());
                }
            }
            for (k, v) in b.rects {
                if k != *a {
                    boxes.rects.insert(k, v);
                }
            }
        }
        Some(boxes)
    }

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
/// FLIP: every node whose parent is in `parents` (all with `all`) and
/// whose box moved starts where it was and glides to its new place.
/// Visited top-down, so a node that moved with its parent adds nothing
/// to the parent's glide.
#[allow(clippy::too_many_arguments)]
fn glide_moved(
    tree: &SceneTree,
    anim: &mut Animator,
    root: NodeId,
    old: &HashMap<NodeId, strand_scene::LogicalRect>,
    new: &HashMap<NodeId, strand_scene::LogicalRect>,
    parents: &HashSet<NodeId>,
    all: bool,
    curve: Curve,
) {
    let mut stack = vec![(root, (0.0f32, 0.0f32))];
    while let Some((n, acc)) = stack.pop() {
        let Some(node) = tree.get(n) else { continue };
        let scope = all || parents.contains(&n);
        for c in &node.children {
            if tree.get(*c).is_none_or(|k| k.kind.is_surface()) {
                continue;
            }
            let mut acc_c = acc;
            if scope && let (Some(o), Some(w)) = (old.get(c), new.get(c)) {
                let adj = (o.x - w.x - acc.0, o.y - w.y - acc.1);
                if adj.0.abs() > 0.5 || adj.1.abs() > 0.5 {
                    anim.glide(*c, [adj.0, adj.1], curve);
                    acc_c = (acc.0 + adj.0, acc.1 + adj.1);
                }
            }
            stack.push((*c, acc_c));
        }
    }
}

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
        let damage = self.paint_frame(surface, target);
        // Exits the frame finished: ghosts unmount (their siblings glide
        // into the gap from the next frame), closing surfaces close, and
        // a surface that settled shrinks: the specs are refreshed here so
        // the host sees the change at once.
        self.process_finished();
        self.release_holds();
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
    fn paint_frame(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        if target.validate().is_err() {
            return Damage::new();
        }
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        s.time = target.time;
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
        // Springs sample this frame's time; a scene flattened earlier
        // (by `update`) is stale while anything moves.
        let swapping = self.swap_moving(surface);
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return Damage::new();
        };
        let root = s.root;
        // A scene flattened by `update` is drawn at rest: not while
        // anything moves, nor while a motion waits to start (an `enter`
        // pose a time-zero preview could not play: a surface just
        // attached, a node just created).
        let pending = self.anim.busy(&self.tree, root) || swapping;
        let cached = if s.animating || pending {
            None
        } else {
            s.cache.take()
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
        // A theme crossfade on this surface paints every frame in full,
        // over the old frame taken before the first one is drawn.
        self.take_snapshot(surface, target);
        let fade = self.fade_frame(surface, target.time, target.size);
        // A cached scene was drawn at rest.
        let animating = fresh && (self.anim.active() || self.swap_moving(surface));
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
        s.opaque = if matches!(fade, swap::FadeFrame::Blend(_)) {
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
        self.raster
            .paint(&f.items, &total, &self.atlas, scale, target);
        if let swap::FadeFrame::Blend(w) = fade {
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
        total
    }

    fn wants(&self, surface: SurfaceId) -> bool {
        // Dirty or never painted. Text still being shaped does not count:
        // its delivery marks the surface dirty (the worker's waker makes
        // the loop call `update`), so waiting costs no frames. A surface
        // not painted yet holds its first frame for its text, up to its
        // deadline.
        self.frame_deadline(surface).is_none()
            && self
                .surfaces
                .get(&surface)
                .is_some_and(|s| s.dirty || !s.valid || s.animating)
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

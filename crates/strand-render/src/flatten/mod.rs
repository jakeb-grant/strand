//! Retained tree → display list for one surface, plus a per-node record of
//! physical bounds and a paint signature for damage diffing.
//!
//! Layout here is the M0 placeholder: nodes are placed at `x`/`y` inside
//! their parent and sized by `width`/`height`/`size` (text nodes by their
//! shaped layout). Flex layout with taffy replaces it in M2.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use strand_scene::{
    BlurRegion, Color, Damage, Font, Length, LogicalRect, NodeId, NodeKind, Paint, Prop, PropValue,
    Rect, Scale, Size, TokenScope, TokenTable,
};
use strand_text::TextLayout;
use vello_cpu::kurbo::{self, BezPath, RoundedRectRadii};

use crate::anim::Animator;
use crate::layout::Boxes;
use crate::tree::{Node, SceneTree};

#[cfg(feature = "gpu")]
mod gpu;
pub(crate) mod hash;
mod image;
mod node;
mod paint;
#[cfg(feature = "eval-hooks")]
mod probe;
mod text;
mod widget;

pub(crate) use hash::{Shift, hash_item, hash_item_from};

pub use paint::BLUR_TINT;
use paint::{cover, kurbo_rect};
// `attach:` fillets resolve their target's radius like a fill does.
pub(crate) use node::late_text_under;
pub(crate) use paint::{corners_of, radii};
use text::sane_font;
pub use text::{Shaped, TextSpec, natural_texts};
pub(crate) use text::{pick, pick_part, slot_color};
pub(crate) use widget::caret_index;

/// Curve flattening tolerance in physical pixels.
const TOLERANCE: f64 = 0.1;

/// Largest magnitude, in logical pixels, of any length or offset read from
/// props. Non-finite values count as unset; finite ones are clamped here so
/// no downstream arithmetic overflows.
const MAX_LOGICAL: f32 = 1e6;

/// Largest shadow blur, logical pixels.
const MAX_BLUR: f32 = 1000.0;

/// One drawing command in physical pixels.
#[derive(Clone, Debug)]
pub enum Item {
    PushClip(BezPath),
    PopClip,
    /// Draws the group at an opacity; the node it fades, when the raster
    /// may draw the group once into a fade layer ([`Fade`]).
    PushOpacity(f32, Option<Fade>),
    PopOpacity,
    /// Draws the group under this transform (physical pixels, from the
    /// surface's origin): `scale` and `rotate` about the node's centre.
    PushTransform(kurbo::Affine),
    PopTransform,
    /// (M4) Draws the group through its node's effects
    /// ([`crate::layers`]): its bounds include their reach.
    PushLayer(Arc<crate::layers::Layer>),
    PopLayer,
    /// (M4) A CPU raster node's pixels ([`crate::offscreen::RasterNodes`])
    /// filling `rect`, drawn at its clock's rate; `key` names the tick
    /// and size they were drawn at.
    Raster {
        node: NodeId,
        key: u64,
        pixmap: Arc<vello_cpu::Pixmap>,
        rect: kurbo::Rect,
    },
    /// A blurred rounded rect, clipped to outside the casting box.
    Shadow {
        rect: kurbo::Rect,
        /// Corner radii clockwise from top-left. When they differ, each
        /// quadrant is drawn with its own corner's radius.
        radii: [f32; 4],
        std_dev: f32,
        color: Color,
        /// Area the shadow may cover minus the casting box (even-odd).
        clip: BezPath,
        /// The area the shadow may cover.
        extent: kurbo::Rect,
    },
    Fill {
        shape: FillShape,
        paint: Paint,
        /// Box the paint's gradient geometry is relative to.
        frame: kurbo::Rect,
    },
    /// An even-odd ring.
    Border {
        path: BezPath,
        paint: Paint,
        frame: kurbo::Rect,
    },
    Glyphs {
        x: i32,
        y: i32,
        layout: Arc<TextLayout>,
        color: Color,
        /// The colours of the layout's span slots ([`span_slots`]): a
        /// run whose colour is slot `i` paints in `spans[i]`.
        spans: Vec<Color>,
        /// (M4) `fill:` on text: the glyphs painted with a paint instead.
        fill: Option<Arc<GlyphFill>>,
    },
    /// A decoded `image` or `icon` filling `rect` (it was decoded at that
    /// size); a symbolic icon is a mask painted in `tint`. The pixmap
    /// maps onto `dest`: `rect` itself, or for a decode at another size
    /// standing in (a size spring), where the fit places it, clipped to
    /// `rect`.
    Image {
        pixmap: Arc<vello_cpu::Pixmap>,
        rect: kurbo::Rect,
        dest: kurbo::Rect,
        tint: Option<Color>,
    },
}

/// (m6-owner-fixes, owner-testing.md bug 17) The node an opacity group
/// fades, for the raster's fade layers ([`crate::offscreen`]): a group
/// whose content is unchanged but for where it is drawn is rastered once
/// into an offscreen layer and composited at its opacity on each frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fade {
    pub node: NodeId,
    /// Its opacity or paint offset is in flight (an `enter` or `exit`
    /// pose, a transition, a `play`): the group is likely drawn again
    /// with the same content on the next frames, so a layer is worth
    /// building on its first frame.
    pub moving: bool,
}

/// (M4) `fill: linear(…)` on text (S-effects): the glyphs' coverage
/// painted with `paint`, whose gradient spans `frame` (the text's box),
/// over `area` (where the glyphs can be), physical pixels.
#[derive(Clone, Debug)]
pub struct GlyphFill {
    pub paint: Paint,
    pub frame: kurbo::Rect,
    pub area: kurbo::Rect,
}

#[derive(Clone, Debug)]
pub enum FillShape {
    Rect(kurbo::Rect),
    Path(BezPath),
    /// (flex) A path filled by the even-odd rule (a canvas's
    /// `c.fill(paint, evenodd: true)`).
    EvenOdd(BezPath),
}

/// A display item with the physical rectangle it can touch. A push item's
/// bounds cover its whole group, so the painter can skip the group (to the
/// matching pop) when it misses the damage.
#[derive(Clone, Debug)]
pub struct DisplayItem {
    pub item: Item,
    pub bounds: Rect,
}

/// What damage diffing remembers about a node between frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub bounds: Rect,
    pub sig: u64,
    /// A text node drawn untransformed at its own scale, with nothing
    /// after its glyphs (no underline or caret): what it draws besides
    /// its glyphs, and each glyph's box, so a text change damages only
    /// the glyphs that changed (a clock tick repaints its last digit,
    /// design.md: "a clock tick repaints about 60×20 px").
    pub glyphs: Option<Arc<GlyphCells>>,
    /// A node with a border along its box: what it draws besides the
    /// border, and the ring's four strips (buffer pixels), so a change of
    /// the border alone (a gradient turning with `t`) damages only the
    /// ring (design.md: "only the ring repaints").
    pub ring: Option<Arc<RingCells>>,
    /// Where it is drawn apart from what it draws: the rounded clips
    /// above it, and its own box when that box is all it draws, so a box
    /// that only moves or resizes (a card swelling) damages the pixels
    /// along its edges and corners, and what it clips damages no more.
    pub geom: Option<Arc<Geometry>>,
}

impl NodeRecord {
    /// For what is drawn into a node after its record is made (a merge's
    /// goo, a GPU pass's pixels): its geometry no longer tells what it
    /// draws, so any change to it, its clips' included, damages its whole
    /// bounds.
    pub(crate) fn drop_geometry(&mut self) {
        if let Some(g) = self.geom.take() {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&self.sig, &mut h);
            for c in g.clips.iter() {
                std::hash::Hash::hash(&(c.rect, c.radii), &mut h);
            }
            self.sig = std::hash::Hasher::finish(&h);
        }
    }
}

/// A rounded box in buffer pixels (see [`NodeRecord::geom`]): its rect,
/// its corners' radii as drawn (bit patterns, so a radius springing by a
/// fraction of a pixel differs), and how far in from its edges what it
/// draws along them reaches (a border, a rim), plus a pixel of
/// antialiasing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundBox {
    pub rect: Rect,
    pub radii: [u64; 4],
    pub edge: u32,
}

impl RoundBox {
    /// The pixels whose cover by the box can differ between `a` and `b`:
    /// their bounds less what both cover whole (both rects shrunk by the
    /// wider edge band), and each corner of both as far as its radius
    /// and that band reach.
    pub fn changed(a: RoundBox, b: RoundBox, out: &mut impl FnMut(Rect)) {
        let e = i64::from(a.edge.max(b.edge));
        let all = a.rect.union(b.rect).inflate(1);
        let core = a
            .rect
            .intersect(b.rect)
            .map(|c| Rect::from_edges(c.left() + e, c.top() + e, c.right() - e, c.bottom() - e));
        let Some(core) = core.filter(|c| !c.is_empty()) else {
            out(all);
            return;
        };
        let (x0, y0, x1, y1) = (all.left(), all.top(), all.right(), all.bottom());
        let (c0, d0, c1, d1) = (core.left(), core.top(), core.right(), core.bottom());
        for r in [
            Rect::from_edges(x0, y0, x1, d0),
            Rect::from_edges(x0, d1, x1, y1),
            Rect::from_edges(x0, d0, c0, d1),
            Rect::from_edges(c1, d0, x1, d1),
        ] {
            if !r.is_empty() {
                out(r);
            }
        }
        for o in [a, b] {
            let (l, t, rr, bb) = (o.rect.left(), o.rect.top(), o.rect.right(), o.rect.bottom());
            for (i, radius) in o.radii.iter().enumerate() {
                let radius = f64::from_bits(*radius);
                if radius.is_nan() || radius <= 0.0 {
                    continue;
                }
                let side = (radius.min(1e6).ceil() as i64).saturating_add(e);
                let (cx, cy) = match i {
                    0 => (l, t),
                    1 => (rr - side, t),
                    2 => (rr - side, bb - side),
                    _ => (l, bb - side),
                };
                let sq = Rect::from_edges(cx - 1, cy - 1, cx + side + 1, cy + side + 1);
                if let Some(sq) = sq.intersect(core) {
                    out(sq);
                }
            }
        }
    }
}

/// A node's geometry for damage diffing (see [`NodeRecord::geom`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// The hash of everything it draws but its own box's geometry (with
    /// its own box) or of everything it draws (without), leaving out the
    /// clips' geometry either way.
    pub rest: u64,
    /// What it draws, before the clips above it cut it.
    pub ink: Rect,
    /// Its own box, when all it draws is that box: a solid fill, border
    /// and rim, untransformed.
    pub own: Option<RoundBox>,
    /// The rounded clips above it, outermost first.
    pub clips: Arc<[RoundBox]>,
}

/// A node's border ring for damage diffing (see [`NodeRecord::ring`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingCells {
    /// The hash of everything the node draws but its border.
    pub rest: u64,
    /// The strips the ring lies in: the top and bottom rows between the
    /// corners, and the left and right columns holding them.
    pub cells: [Rect; 4],
}

/// A text node's glyphs for damage diffing (see [`NodeRecord::glyphs`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlyphCells {
    /// The hash of everything the node draws but its glyphs.
    pub rest: u64,
    /// Each glyph's box (buffer pixels, grown by a pixel) and identity
    /// (its atlas slot and colour).
    pub cells: Vec<(Rect, u64)>,
}

#[derive(Debug, Default)]
pub struct Flattened {
    /// (wave-cost) Flattening it found something moving (a paint prop, a
    /// glide, an effect): the scene does not hold for a later frame even
    /// where no box changed. Set by the renderer; sizes in flight are
    /// layout's, and count only as boxes that move.
    pub moving: bool,
    pub items: Vec<DisplayItem>,
    /// Ordered, so damage is added in a deterministic order.
    pub records: BTreeMap<NodeId, NodeRecord>,
    /// (dock-drag) The recorded nodes in the order they paint: where two
    /// that overlap swap places (a lifted child raised above its
    /// siblings, a keyed reorder) the pixels they share change colour
    /// though neither record does ([`crate::renderer`]'s order damage).
    pub order: Vec<NodeId>,
    /// Text nodes and the shaping they need at this scale.
    pub text: Vec<(NodeId, TextSpec)>,
    /// Where the surface root paints fully opaque pixels.
    pub opaque: Damage,
    /// Every drawn node's hit shape, in paint order (a node before its
    /// children, earlier siblings before later ones).
    pub hits: Vec<HitBox>,
    /// Every image and icon the frame draws, decoded or not.
    pub images: Vec<crate::image::ImageKey>,
    /// Each `input`'s text layout and the surface-logical x of its
    /// origin (a click places the caret from it).
    pub inputs: HashMap<NodeId, (Arc<TextLayout>, f32)>,
    /// Where nodes with `blur` ask the compositor to blur behind the
    /// surface: their rounded boxes in buffer pixels, with the radius
    /// (the blur ladder's first rung, M4).
    pub blur: Vec<BlurRegion>,
    /// (M4) The clocks of drawn, visible nodes that read time (`t`,
    /// `wave(…)`, `noise(t)`) or draw a CPU raster source:
    /// they repaint on every tick of their clock while drawn.
    pub(crate) clocks: Vec<crate::clock::Clock>,
    /// (shared-morph-on) The drawn nodes with a `morph` name and their
    /// boxes, in paint order: where each name is on screen once a frame
    /// paints this scene, flattened for it or reused.
    pub(crate) morphs: Vec<crate::anim::MorphBox>,
    /// (M4) The passes of drawn `shader` nodes and bundled effects.
    #[cfg(feature = "gpu")]
    pub(crate) passes: Vec<crate::renderer::backend::PassWant>,
    /// (M4) The pose the compositor applies to the whole surface this
    /// frame, taken out of the root's props ([`crate::pose`]); `None`
    /// when nothing is delegated.
    pub pose: Option<strand_scene::SurfacePose>,
    /// (M6) The boxes of the root's children whose subtrees draw
    /// something, as drawn (paint offsets in), logical: where a popup
    /// that draws nothing itself takes input ([`Renderer::input_region`]).
    ///
    /// [`Renderer::input_region`]: crate::Renderer
    pub input: Vec<LogicalRect>,
    /// (M6) The surface root draws something itself (a background, a
    /// border): its whole box takes input.
    pub root_paints: bool,
    /// (growth) The surface root's box as drawn (paint offsets in, before
    /// transforms), logical: with its laid-out box, where a content-sized
    /// surface drawn short of its buffer takes input
    /// (`Renderer::placed_root`).
    pub root_box: Option<LogicalRect>,
    /// (replay-owner) Each node as drawn, for a flatten that asked
    /// ([`flatten_drawn`]): a glitch report's on-demand record, never a
    /// frame's (`None` then).
    pub drawn: Option<Vec<crate::DrawnNode>>,
    /// (eval-sandbox) Each node as drawn, when the frame trace is on
    /// (`crate::probe`).
    #[cfg(feature = "eval-hooks")]
    pub trace: Option<Arc<[crate::probe::NodeTrace]>>,
}

/// A node's hit shape: its rounded box in physical pixels, grown by
/// `hit: grow(n)`, inside the clip of its ancestors. Under `scale` or
/// `rotate` the box is in the node's untransformed space and `inverse`
/// maps a surface point into it, so a rotated pill is hit on its rounded
/// shape, not its bounding box.
#[derive(Clone, Debug)]
pub struct HitBox {
    pub node: NodeId,
    pub rect: kurbo::Rect,
    pub radii: RoundedRectRadii,
    pub clip: Rect,
    /// Surface pixels to the node's own space (`None`: identity).
    pub inverse: Option<kurbo::Affine>,
    /// (M5) How far `hit: grow(n)` grew `rect` (physical pixels): the
    /// inspector outlines and reports the box without it.
    pub grow: f64,
}

impl HitBox {
    /// True if the physical point `(x, y)` (a pixel centre) is inside the
    /// rounded shape and the clip.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        let c = self.clip;
        if x < c.left() as f64
            || y < c.top() as f64
            || x >= c.right() as f64
            || y >= c.bottom() as f64
        {
            return false;
        }
        let (x, y) = match self.inverse {
            Some(inv) => {
                let p = inv * kurbo::Point::new(x, y);
                (p.x, p.y)
            }
            None => (x, y),
        };
        let r = self.rect;
        if x < r.x0 || y < r.y0 || x >= r.x1 || y >= r.y1 {
            return false;
        }
        // Outside a corner's quarter circle?
        let corner = |cx: f64, cy: f64, rad: f64| {
            let (dx, dy) = (x - cx, y - cy);
            dx * dx + dy * dy <= rad * rad
        };
        let rr = &self.radii;
        if x < r.x0 + rr.top_left && y < r.y0 + rr.top_left {
            return corner(r.x0 + rr.top_left, r.y0 + rr.top_left, rr.top_left);
        }
        if x > r.x1 - rr.top_right && y < r.y0 + rr.top_right {
            return corner(r.x1 - rr.top_right, r.y0 + rr.top_right, rr.top_right);
        }
        if x > r.x1 - rr.bottom_right && y > r.y1 - rr.bottom_right {
            return corner(
                r.x1 - rr.bottom_right,
                r.y1 - rr.bottom_right,
                rr.bottom_right,
            );
        }
        if x < r.x0 + rr.bottom_left && y > r.y1 - rr.bottom_left {
            return corner(r.x0 + rr.bottom_left, r.y1 - rr.bottom_left, rr.bottom_left);
        }
        true
    }
}

#[derive(Clone)]
struct Inherited<'a> {
    /// The nearest ancestor's `color`; `None` is the theme's `$fg`,
    /// looked up in each node's own scope (so a `set { $fg: … }` or
    /// `set { $surface: … }` subtree, guarded, applies to it).
    color: Option<Color>,
    /// The nearest ancestor's `font`; `None` is `$font.ui`, likewise.
    font: Option<Font>,
    /// A `weight` set below the nearest `font`.
    weight: Option<u16>,
    /// Token tables in scope: the global one, then each ancestor's
    /// `tokens` override.
    tokens: Vec<&'a TokenTable>,
    /// Hash of everything above this node that affects how it paints
    /// (opacity, clips, paint-order epochs).
    ctx: u64,
    /// Accumulated clip in physical pixels.
    clip: Rect,
    /// Accumulated paint offset (`x`, `y`) of the ancestors, logical,
    /// as written (at rest).
    offset: (f32, f32),
    /// (seam-snap) The part of the ancestors' paint offsets in flight
    /// (springs and glides), logical and unrounded: each box adds its
    /// own and rounds its edges from where they fall (`drawn_span`).
    flight: (f32, f32),
    /// (integration-r8) What the ancestors' shared morphs in flight do
    /// to where this node is drawn, logical surface pixels: a box drawn
    /// at `b` rests at `morphs⁻¹(b)` once they settle.
    morphs: kurbo::Affine,
    /// Inside a subtree playing its exit pose: drawn, never hit.
    inert: bool,
    /// (M4) A `tokens` override in scope reads time: every node under it
    /// is evaluated at its own time.
    timed: bool,
    /// (M4) How far the effect layers around this node spread its
    /// pixels, physical: its damage grows by it.
    reach: u32,
    /// (M6) Below a child of the surface root: that child's box takes
    /// this node's input (`Flattened::input`).
    input_done: bool,
    /// The rounded clips above this node, untransformed (`NodeRecord::geom`):
    /// their geometry is left out of `ctx`.
    clips: Arc<[RoundBox]>,
}

/// What flattening reads besides the tree, layout and springs.
#[derive(Debug, Default)]
pub struct Extras {
    /// The compositor blurs behind surfaces (`ext-background-effect-v1`,
    /// M4): `blur` needs no tint fallback.
    pub compositor_blur: bool,
    /// (M4) The compositor applies surface poses (`wp_alpha_modifier_v1`
    /// and the viewporter): a root's opacity, scale and offset go to
    /// it where its placement allows ([`crate::pose`]).
    pub compositor_poses: bool,
    /// (M4) Roots of the surfaces the GPU thread presents: their poses
    /// are painted into their frames, never delegated (architecture.md,
    /// "Surface hand-off").
    #[cfg(feature = "gpu")]
    pub gpu_presented: std::collections::HashSet<NodeId>,
    /// (perf-gpu-retain) Roots of the surfaces the GPU draws (presented
    /// or read back): their frames draw their backdrops themselves, so a
    /// backdrop's pass (the pointer glass follows) is in its damage.
    #[cfg(feature = "gpu")]
    pub gpu_lowered: std::collections::HashSet<NodeId>,
    /// (M4) The compositor draws a layer surface stretched to its
    /// arranged box whatever its viewport (Hyprland): a root's scale is
    /// painted, not delegated.
    pub compositor_pose_scale_off: bool,
    /// Hover, press, focus, carets and slider drags from the input
    /// router.
    pub widgets: crate::widgets::Widgets,
    /// Decoded `image` and `icon` pixels.
    pub images: crate::image::ImageStore,
    /// (M4) CPU raster nodes' sources and pixmaps.
    pub rasters: crate::offscreen::RasterNodes,
    /// (M4) Passes' last pixels (`shader` nodes, bundled effects).
    #[cfg(feature = "gpu")]
    pub shaders: crate::renderer::backend::ShaderResults,
    /// (M4) Bundled effects take their GPU form (3-D tilt, an animated
    /// aurora, particles past the CPU's cap): false while the device is
    /// unavailable.
    #[cfg(feature = "gpu")]
    pub gpu_ok: bool,
    /// (M4) The pointer on each surface it is over, by the surface's
    /// root node, logical pixels (`Router::pointer`, handed over by the
    /// host): `parallax` and `tilt` follow it.
    pub pointers: HashMap<NodeId, strand_scene::LogicalPoint>,
    /// (M4) Where each surface lies on its output, by the surface's root
    /// node (handed over by the host): shared-element morphs across
    /// surfaces start from boxes moved by it.
    pub origins: HashMap<NodeId, SurfaceOrigin>,
    /// (M4) What the host placed, by the surface's root: its own output
    /// (`None` for a popup) and position; `origins` is resolved from it.
    pub placed: HashMap<NodeId, (Option<String>, strand_scene::LogicalPoint)>,
    /// (M4) The CPU fallbacks drawn in place of GPU effects, said once.
    pub fallbacks: crate::effects::raster::Fallbacks,

    /// (M4) Media nodes' sources (graphs, spectra), by node.
    pub media: crate::media::Media,
    /// (eval-sandbox) The frame trace is on: flattening records each
    /// node (`crate::probe`).
    #[cfg(feature = "eval-hooks")]
    pub(crate) trace: bool,
    /// (eval-coverage) The text each layout delivered while the trace is
    /// on was shaped from, by its request's key: the trace names the
    /// glyphs a node draws by it (`crate::probe::GlyphsDrawn`).
    #[cfg(feature = "eval-hooks")]
    pub(crate) texts: HashMap<strand_text::TextKey, crate::probe::ProbeText>,
}

/// (M4) Where a surface's buffer (its top-left corner) lies on an
/// output: the output's name and the position, in the output's logical
/// pixels (`SurfaceHost::surface_placed`).
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceOrigin {
    /// The output it is on (any name that tells outputs apart).
    pub output: String,
    /// Its top-left corner on that output.
    pub at: strand_scene::LogicalPoint,
}

impl Extras {
    /// (M4) Whether the surface whose root is `root` hands its pose to
    /// the compositor: it delegates poses, and the GPU thread does not
    /// present it.
    pub(crate) fn delegates_pose(&self, root: NodeId) -> bool {
        #[cfg(feature = "gpu")]
        if self.gpu_presented.contains(&root) {
            return false;
        }
        let _ = root;
        self.compositor_poses
    }
}

/// Flattens the subtree under `root` for a surface of `size` at `scale`.
/// `layouts` holds the delivered text layouts per node (see [`Shaped`]).
#[allow(clippy::too_many_arguments)]
pub fn flatten(
    tree: &SceneTree,
    root: NodeId,
    size: Size,
    scale: Scale,
    layouts: &HashMap<NodeId, Vec<Shaped>>,
    boxes: &Boxes,
    anim: &mut Animator,
    extras: &Extras,
) -> Flattened {
    flatten_with(tree, root, size, scale, layouts, boxes, anim, extras, false)
}

/// (replay-owner) [`flatten`], also recording each node as drawn
/// ([`Flattened::drawn`]): where the glitch report's live boxes come
/// from (`Renderer::drawn_nodes`), at the points the eval's frame trace
/// records its rows, so the two name the same nodes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn flatten_drawn(
    tree: &SceneTree,
    root: NodeId,
    size: Size,
    scale: Scale,
    layouts: &HashMap<NodeId, Vec<Shaped>>,
    boxes: &Boxes,
    anim: &mut Animator,
    extras: &Extras,
) -> Flattened {
    flatten_with(tree, root, size, scale, layouts, boxes, anim, extras, true)
}

#[allow(clippy::too_many_arguments)]
fn flatten_with(
    tree: &SceneTree,
    root: NodeId,
    size: Size,
    scale: Scale,
    layouts: &HashMap<NodeId, Vec<Shaped>>,
    boxes: &Boxes,
    anim: &mut Animator,
    extras: &Extras,
    drawn: bool,
) -> Flattened {
    let mut out = Flattened::default();
    let Some(node) = tree.get(root) else {
        return out;
    };
    let full = Rect::from_size(size);
    let logical = scale.logical_size(size);
    let mut f = Flattener {
        tree,
        scale,
        surface: full,
        layouts,
        boxes,
        anim,
        extras,
        xform: kurbo::Affine::IDENTITY,
        out: &mut out,
        fillet: crate::fillet::find(tree, boxes, root),
        open_clips: Vec::new(),
        fillet_fill: None,
        item_clips: Vec::new(),
        pointer: extras.pointers.get(&root).copied(),
        #[cfg(feature = "gpu")]
        gpu_frame: extras.gpu_lowered.contains(&root),
        logical: LogicalRect::new(0.0, 0.0, logical.w, logical.h),
        drawn: drawn.then(Vec::new),
        #[cfg(feature = "eval-hooks")]
        trace: extras.trace.then(Vec::new),
        #[cfg(feature = "eval-hooks")]
        probe_images: Vec::new(),
        #[cfg(feature = "eval-hooks")]
        probe_shadows: Vec::new(),
    };
    // Text with no `color` or `font` above it is themed: `$fg` and
    // `$font.ui` when the token table has them (the built-in theme does).
    let mut inh = Inherited {
        color: None,
        font: None,
        weight: None,
        tokens: vec![&tree.tokens],
        ctx: 0,
        clip: full,
        offset: (0.0, 0.0),
        flight: (0.0, 0.0),
        morphs: kurbo::Affine::IDENTITY,
        inert: false,
        timed: false,
        reach: 0,
        input_done: false,
        clips: Arc::from([]),
    };
    // A surface nested in another (a popup in a bar) inherits tokens,
    // colour and font from its ancestors, though it paints on its own.
    let mut ancestors = Vec::new();
    let mut up = node.parent;
    while let Some(a) = up.and_then(|p| tree.get(p)) {
        ancestors.push(a);
        up = a.parent;
    }
    for a in ancestors.into_iter().rev() {
        inherit(a, &mut inh);
    }
    let frame = boxes
        .rects
        .get(&root)
        .copied()
        .unwrap_or(LogicalRect::new(0.0, 0.0, logical.w, logical.h));
    f.node(node, frame, &inh, true);
    let drawn = f.drawn.take();
    #[cfg(feature = "eval-hooks")]
    {
        let trace = f.trace.take();
        out.trace = trace.map(Arc::from);
    }
    out.drawn = drawn;
    out
}

struct Flattener<'a> {
    tree: &'a SceneTree,
    scale: Scale,
    surface: Rect,
    layouts: &'a HashMap<NodeId, Vec<Shaped>>,
    boxes: &'a Boxes,
    anim: &'a mut Animator,
    extras: &'a Extras,
    /// The transform of the group being flattened (identity outside any
    /// `scale` or `rotate`).
    xform: kurbo::Affine,
    out: &'a mut Flattened,
    /// The surface's `attach:` fillet (`crate::fillet`).
    fillet: Option<crate::fillet::Fillet>,
    /// (fillet-clip) The clips open above the node being flattened,
    /// outermost first, so the fillet target's fill passes them at its
    /// fillets (`Flattener::let_fillets_through`).
    open_clips: Vec<OpenClip>,
    /// (fillet-clip) The fillet target's fill item once drawn under a
    /// clip it passes, kept in step with the items inserted before it:
    /// what each such clip holds the rest of its group around
    /// (`Flattener::hold_to_box`).
    fillet_fill: Option<usize>,
    /// (damage-fraction) What the clips a node pushed for its own items
    /// (`Flattener::push`) let through, innermost last: an item drawn
    /// under one paints no further than it.
    item_clips: Vec<kurbo::Rect>,
    /// (M4) The pointer on the surface, and the surface's logical box.
    pointer: Option<strand_scene::LogicalPoint>,
    /// (perf-gpu-retain) The GPU draws the surface's frames
    /// (`Extras::gpu_lowered`).
    #[cfg(feature = "gpu")]
    gpu_frame: bool,
    logical: LogicalRect,
    /// (replay-owner) The nodes drawn so far, for [`flatten_drawn`].
    drawn: Option<Vec<crate::DrawnNode>>,
    /// (eval-sandbox) The nodes drawn so far, when the frame trace is on
    /// (`crate::probe`).
    #[cfg(feature = "eval-hooks")]
    trace: Option<Vec<crate::probe::NodeTrace>>,
    /// (eval-rules-4) The decodes the image nodes drew, by node, until
    /// each node's trace takes its own.
    #[cfg(feature = "eval-hooks")]
    probe_images: Vec<(NodeId, crate::probe::ImageDrawn)>,
    /// (eval-rules-8) The shadows the nodes drew, by node, in paint
    /// order: what each one's props asked (`ShadowDrawn::asked`) and the
    /// box it was cast by, until each node's trace takes its own.
    #[cfg(feature = "eval-hooks")]
    probe_shadows: Vec<(NodeId, [f32; 4], LogicalRect)>,
}

/// (fillet-clip) A clip group open above the node being flattened.
struct OpenClip {
    /// Its `PushClip` item.
    item: usize,
    /// The transform its path is drawn under.
    xform: kurbo::Affine,
    /// Its box (physical pixels, under `xform`): what it lets the
    /// target's fillets through is bounded by it.
    frame: kurbo::Rect,
    /// Its box's largest corner radius (physical pixels): how far its
    /// rounding reaches into the target's square corners.
    radius: f64,
    /// Its box path, kept once the fillet target's fill or a shadow has
    /// widened the clip: what the rest of the group is held to.
    held: Option<BezPath>,
    /// (shadow-pass) It clips only because its box's size springs
    /// (`Animator::sizing`), not for `clip: true`, a `scroll`, `list` or
    /// `pages`: the shadows of the boxes inside it pass it.
    sizing: bool,
    /// (shadow-pass) What the clips above it let through (surface px):
    /// a shadow that passes it shows no further.
    outer: Rect,
    /// (shadow-pass) The shadow items that pass it, kept in step with
    /// the items inserted before them.
    passes: Vec<usize>,
}

/// (damage-fraction) Where `item` paints, from its own geometry
/// (physical pixels, before the transform it is drawn under), when that
/// geometry says: a path's curves, a shadow's extent, a pixmap's rect,
/// a clip's outline. A shape's outline (a Catmull-Rom curve through its
/// points, a morph between two) passes outside its box by a fraction of
/// a pixel, and the raster's antialiasing paints the pixel that fraction
/// covers; damage covers all of what is painted (design.md: exact
/// damage), so a pushed item's bounds take this in, whatever its caller
/// passed. Glyphs are bounded by their layout's ink, which their callers
/// pass.
fn painted(item: &Item) -> Option<kurbo::Rect> {
    use kurbo::Shape;
    let r = match item {
        Item::Fill { shape, .. } => match shape {
            FillShape::Rect(r) => *r,
            FillShape::Path(p) | FillShape::EvenOdd(p) => p.bounding_box(),
        },
        Item::Border { path, .. } | Item::PushClip(path) => path.bounding_box(),
        Item::Shadow { extent, .. } => *extent,
        Item::Image { rect, .. } | Item::Raster { rect, .. } => *rect,
        _ => return None,
    };
    (r.is_finite() && r.width() > 0.0 && r.height() > 0.0).then_some(r)
}

/// How far past a pixel's edge geometry may reach and paint nothing
/// there: vello_cpu rounds a pixel's coverage to 8 bits
/// (`coverage * 255 + 0.5`, truncated), so a sliver of at most this much
/// of a pixel gets an alpha of 0. It absorbs the float error of a
/// curve's extremes (a rounded rect's arcs end a few ulps past its box),
/// which would otherwise grow a box's damage by a row it does not paint,
/// differently wherever it lies.
const UNPAINTED_SLIVER: f64 = 1.0 / 512.0;

/// The pixels geometry inside `r` paints ([`UNPAINTED_SLIVER`]).
fn painted_cover(r: kurbo::Rect) -> Rect {
    let e = UNPAINTED_SLIVER;
    Rect::from_edges(
        (r.x0 + e).floor() as i64,
        (r.y0 + e).floor() as i64,
        (r.x1 - e).ceil() as i64,
        (r.y1 - e).ceil() as i64,
    )
}

/// The pixels `r` covers once drawn under `a`.
fn map_rect(a: kurbo::Affine, r: Rect) -> Rect {
    if a == kurbo::Affine::IDENTITY || r.is_empty() {
        return r;
    }
    cover(a.transform_rect_bbox(kurbo_rect(r))).inflate(1)
}

/// Applies a node's inherited props (`tokens`, `color`, `font`, `weight`)
/// to `inh`, as its children see them.
fn inherit<'a>(node: &'a Node, inh: &mut Inherited<'a>) {
    if let Some(global) = inh.tokens.first() {
        inh.timed |= crate::time::overrides_read_time(node, global);
    }
    if let Some(PropValue::Tokens(t)) = node.get(Prop::Tokens) {
        inh.tokens.push(t);
    }
    // A nested surface's ancestors are read at rest (`t = 0`): their
    // own clocks belong to the surface that draws them.
    let scope = TokenScope::new(&inh.tokens);
    let get = |p: Prop| node.get(p).and_then(|v| scope.resolve(v));
    if let Some(PropValue::Color(c)) = get(Prop::Color).as_deref() {
        inh.color = Some(*c);
    }
    if let Some(PropValue::Font(f)) = get(Prop::Font).as_deref() {
        inh.font = Some(sane_font(f.clone()));
        inh.weight = None;
    }
    if let Some(w) = number(get(Prop::Weight).as_deref()) {
        inh.weight = Some(w.clamp(1.0, 1000.0) as u16);
    }
}

/// The fill a node of `kind` draws when it names no `bg`: a bar's
/// surface (`root`: a bar is a surface's root) is themed `$surface`, a
/// button and a segmented control `$surface.hi` (the label colour
/// `text_color` at 10 % without it). (eval-rules-7) It is the value an
/// unset `bg` springs from and to, as flatten draws it at rest: a button
/// given `bg:` by a state fades its fill from this, not from clear.
pub(crate) fn default_fill(
    kind: NodeKind,
    root: bool,
    scope: &TokenScope,
    text_color: Color,
) -> Option<Color> {
    match kind {
        NodeKind::Bar if root => match scope.lookup("surface") {
            Some(PropValue::Color(c)) => Some(c),
            _ => None,
        },
        NodeKind::Button | NodeKind::Segmented => Some(match scope.lookup("surface.hi") {
            Some(PropValue::Color(c)) => c,
            _ => text_color.alpha(0.1),
        }),
        _ => None,
    }
}

/// The theme's default text colour in `scope`: `$fg`, else black.
pub(crate) fn default_color(scope: &TokenScope) -> Color {
    match scope.lookup("fg") {
        Some(PropValue::Color(c)) => c,
        _ => Color::BLACK,
    }
}

/// The theme's default font in `scope`: `$font.ui`, else the default.
fn default_font(scope: &TokenScope) -> Font {
    match scope.lookup("font.ui") {
        Some(PropValue::Font(f)) => sane_font(f),
        _ => Font::default(),
    }
}

/// A finite value clamped to `±MAX_LOGICAL`; non-finite is `None`.
fn finite(v: f32) -> Option<f32> {
    v.is_finite().then(|| v.clamp(-MAX_LOGICAL, MAX_LOGICAL))
}

/// `finite`, with non-finite values read as 0.
fn finite_or_zero(v: f32) -> f32 {
    finite(v).unwrap_or(0.0)
}

fn number(v: Option<&PropValue>) -> Option<f32> {
    match v? {
        PropValue::Number(n) => finite(*n),
        PropValue::Length(Length::Px(n)) => finite(*n),
        _ => None,
    }
}

/// An angle in degrees: `rotate: 90deg` arrives as [`PropValue::Angle`]
/// (and so does every in-flight rotate sample); a bare number is degrees.
fn angle(v: Option<&PropValue>) -> Option<f32> {
    match v? {
        PropValue::Angle(n) => n.is_finite().then_some(*n),
        other => number(Some(other)),
    }
}

fn length(v: Option<&PropValue>, reference: f32) -> Option<f32> {
    match v? {
        PropValue::Number(n) => finite(*n),
        PropValue::Length(Length::Px(n)) => finite(*n),
        PropValue::Length(Length::Percent(p)) => finite(reference * p / 100.0),
        _ => None,
    }
}

/// The token tables in scope at `id`: the global table, then the
/// `tokens` overrides of its ancestors and of `id` itself.
pub fn scope_tables(tree: &SceneTree, id: NodeId) -> Vec<&TokenTable> {
    let mut chain = Vec::new();
    let mut cur = tree.get(id);
    while let Some(n) = cur {
        if let Some(PropValue::Tokens(t)) = n.get(Prop::Tokens) {
            chain.push(t.as_ref());
        }
        cur = n.parent.and_then(|p| tree.get(p));
    }
    chain.push(&tree.tokens);
    chain.reverse();
    chain
}

#[cfg(test)]
mod tests;

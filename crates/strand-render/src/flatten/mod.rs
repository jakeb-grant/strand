//! Retained tree → display list for one surface, plus a per-node record of
//! physical bounds and a paint signature for damage diffing.
//!
//! Layout here is the M0 placeholder: nodes are placed at `x`/`y` inside
//! their parent and sized by `width`/`height`/`size` (text nodes by their
//! shaped layout). Flex layout with taffy replaces it in M2.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use strand_scene::{
    BlurRegion, Color, Damage, Font, Length, LogicalRect, NodeId, Paint, Prop, PropValue, Rect,
    Scale, Size, TokenScope, TokenTable,
};
use strand_text::TextLayout;
use vello_cpu::kurbo::{self, BezPath, RoundedRectRadii};

use crate::anim::Animator;
use crate::layout::Boxes;
use crate::tree::{Node, SceneTree};

mod hash;
mod image;
mod node;
mod paint;
mod text;
mod widget;

pub use paint::BLUR_TINT;
use paint::{cover, kurbo_rect};
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
    PushOpacity(f32),
    PopOpacity,
    /// Draws the group under this transform (physical pixels, from the
    /// surface's origin): `scale` and `rotate` about the node's centre.
    PushTransform(kurbo::Affine),
    PopTransform,
    /// (M4) Draws the group through its node's effects
    /// ([`crate::layers`]): its bounds include their reach.
    PushLayer(Arc<crate::layers::Layer>),
    PopLayer,
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

#[derive(Clone, Debug)]
pub enum FillShape {
    Rect(kurbo::Rect),
    Path(BezPath),
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
    pub items: Vec<DisplayItem>,
    /// Ordered, so damage is added in a deterministic order.
    pub records: BTreeMap<NodeId, NodeRecord>,
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
    /// `wave(…)`, `noise(t)`) or animate by nature (a built-in `effect`):
    /// they repaint on every tick of their clock while drawn.
    pub(crate) clocks: Vec<crate::clock::Clock>,
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
    /// Accumulated paint offset (`x`, `y`) of the ancestors, logical.
    offset: (f32, f32),
    /// Inside a subtree playing its exit pose: drawn, never hit.
    inert: bool,
    /// (M4) A `tokens` override in scope reads time: every node under it
    /// is evaluated at its own time.
    timed: bool,
    /// (M4) How far the effect layers around this node spread its
    /// pixels, physical: its damage grows by it.
    reach: u32,
}

/// What flattening reads besides the tree, layout and springs.
#[derive(Debug, Default)]
pub struct Extras {
    /// The compositor blurs behind surfaces (`ext-background-effect-v1`,
    /// M4): `blur` needs no tint fallback.
    pub compositor_blur: bool,
    /// Hover, press, focus, carets and slider drags from the input
    /// router.
    pub widgets: crate::widgets::Widgets,
    /// Decoded `image` and `icon` pixels.
    pub images: crate::image::ImageStore,
    /// (M4) Group effects per node ([`crate::layers`]), until S-effects
    /// builds them from props.
    pub effects: crate::layers::NodeEffects,
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
        inert: false,
        timed: false,
        reach: 0,
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
    if let Some(PropValue::Tokens(t)) = node.get(Prop::Tokens) {
        inh.tokens.push(t);
    }
    inh.timed |= crate::time::overrides_read_time(node);
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

/// The theme's default text colour in `scope`: `$fg`, else black.
fn default_color(scope: &TokenScope) -> Color {
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

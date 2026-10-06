//! Retained tree → display list for one surface, plus a per-node record of
//! physical bounds and a paint signature for damage diffing.
//!
//! Layout here is the M0 placeholder: nodes are placed at `x`/`y` inside
//! their parent and sized by `width`/`height`/`size` (text nodes by their
//! shaped layout). Flex layout with taffy replaces it in M2.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{
    Border, Color, Corners, Damage, Font, Length, LogicalRect, NodeId, NodeKind, Paint, Prop,
    PropValue, Rect, Scale, Shadow, Size, TokenScope, TokenTable,
};
use strand_text::{Ellipsis, TextAlign, TextLayout, TextSpan, TextStyle};
use vello_cpu::kurbo::{self, BezPath, RoundedRect, RoundedRectRadii, Shape};

use crate::anim::Animator;
use crate::layout::Boxes;
use crate::tree::{Node, SceneTree};

/// Curve flattening tolerance in physical pixels.
const TOLERANCE: f64 = 0.1;

/// Largest magnitude, in logical pixels, of any length or offset read from
/// props. Non-finite values count as unset; finite ones are clamped here so
/// no downstream arithmetic overflows.
const MAX_LOGICAL: f32 = 1e6;

/// Largest shadow blur, logical pixels.
const MAX_BLUR: f32 = 1000.0;

/// What a text node needs shaped.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpec {
    pub text: String,
    pub style: TextStyle,
    pub max_width: Option<f32>,
    pub scale: Scale,
}

/// A delivered layout of a text node and the line box width it was shaped
/// for. A node shown on several surfaces can have one per scale and width:
/// alignment (`center`, `end`) happens inside the line box, so a layout
/// shaped for one width is wrong on a surface of another.
#[derive(Clone, Debug)]
pub struct Shaped {
    pub layout: Arc<TextLayout>,
    pub max_width: Option<f32>,
}

/// The delivered layout of a node for line box `width` (`None`: shaped
/// unbounded), preferring `scale`, else the same width at another scale
/// (drawn resampled).
pub(crate) fn pick(shaped: &[Shaped], scale: Scale, width: Option<f32>) -> Option<Arc<TextLayout>> {
    let same_w = |c: &&Shaped| c.max_width == width;
    shaped
        .iter()
        .find(|c| same_w(c) && c.layout.scale == scale)
        .or_else(|| shaped.iter().find(same_w))
        .map(|c| c.layout.clone())
}

/// A layout to draw and its logical offset in the node's box.
pub(crate) type PlacedText = (Arc<TextLayout>, f32, f32);

/// Where a text node's glyphs go in its box: the layout to draw and the
/// logical offset of its origin from the box's top-left. A text whose
/// unbounded layout fits its box is drawn from that layout, aligned in
/// the box by `align`; a narrower box draws the layout shaped for its
/// width (wrapped or ellipsised, aligned by the text engine), or the
/// unbounded one while that is being shaped. `align: center` also
/// centres it vertically in a taller box; otherwise it sits at the top.
pub(crate) fn place_text(
    shaped: &[Shaped],
    scale: Scale,
    rect: LogicalRect,
    align: TextAlign,
) -> (Option<f32>, Option<PlacedText>) {
    let natural = pick(shaped, scale, None);
    let fit = natural
        .as_ref()
        .is_some_and(|n| rect.w + 1.0 < n.size.w)
        .then(|| rect.w.round().max(0.0));
    let chosen = match fit {
        Some(w) => pick(shaped, scale, Some(w))
            .map(|l| (l, 0.0))
            .or_else(|| natural.clone().map(|l| (l, 0.0))),
        None => natural.clone().map(|l| {
            let slack = (rect.w - l.size.w).max(0.0);
            let dx = match align {
                TextAlign::Start => 0.0,
                TextAlign::Center => slack / 2.0,
                TextAlign::End => slack,
            };
            (l, dx)
        }),
    }
    // Nothing for this width at all yet: any layout of the node stands in.
    .or_else(|| shaped.first().map(|c| (c.layout.clone(), 0.0)));
    let placed = chosen.map(|(l, dx)| {
        let dy = match align {
            TextAlign::Center => ((rect.h - l.size.h) / 2.0).max(0.0),
            _ => 0.0,
        };
        (l, dx, dy)
    });
    (fit, placed)
}

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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub bounds: Rect,
    pub sig: u64,
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
}

/// Flattens the subtree under `root` for a surface of `size` at `scale`.
/// `layouts` holds the delivered text layouts per node (see [`Shaped`]).
pub fn flatten(
    tree: &SceneTree,
    root: NodeId,
    size: Size,
    scale: Scale,
    layouts: &HashMap<NodeId, Vec<Shaped>>,
    boxes: &Boxes,
    anim: &mut Animator,
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

/// A font safe to shape: non-finite or non-positive sizes fall back to the
/// default size.
fn sane_font(mut f: Font) -> Font {
    if !(f.size.is_finite() && f.size > 0.0) {
        f.size = Font::default().size;
    }
    f.size = f.size.min(MAX_LOGICAL);
    f.weight = f.weight.clamp(1, 1000);
    f.family = with_generic(&f.family);
    f
}

/// Generic CSS families: a list that names one already falls back.
const GENERIC_FAMILIES: [&str; 8] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-sans-serif",
    "ui-monospace",
];

/// `family` ending in a generic family, as CSS falls back: a font that
/// is not installed (`"Inter"`) must not leave the choice of fallback per
/// character to the font library, which can pick a font that cannot draw
/// it (digits from a bitmap emoji font). A family whose name says `Mono`
/// falls back to `monospace`, any other to `sans-serif`.
fn with_generic(family: &str) -> String {
    let has_generic = family
        .split(',')
        .map(|f| f.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
        .any(|f| GENERIC_FAMILIES.contains(&f.as_str()));
    if has_generic || family.trim().is_empty() {
        return family.to_string();
    }
    let generic = if family.to_ascii_lowercase().contains("mono") {
        "monospace"
    } else {
        "sans-serif"
    };
    format!("{family}, {generic}")
}

/// `marks: h.ranges` (a list of `[start, end]` character ranges, end
/// exclusive, as fuzzy matchers report them) as text spans painted in
/// `mark_color` (default `$accent`), or bold when there is no colour.
fn marks(
    text: &str,
    v: Option<&PropValue>,
    color: impl FnOnce() -> Option<Color>,
) -> Vec<TextSpan> {
    let Some(PropValue::List(items)) = v else {
        return Vec::new();
    };
    // Character index → byte offset.
    let bytes: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let at = |n: f32| bytes[(n.max(0.0) as usize).min(bytes.len() - 1)];
    let ranges: Vec<std::ops::Range<usize>> = items
        .iter()
        .filter_map(|r| match r {
            PropValue::List(pair) => match (pair.first(), pair.get(1)) {
                (Some(a), Some(b)) => Some((a.as_number()?, b.as_number()?)),
                _ => None,
            },
            _ => None,
        })
        .filter(|(a, b)| a.is_finite() && b.is_finite() && a < b)
        .take(1024)
        .map(|(a, b)| at(a)..at(b))
        .collect();
    if ranges.is_empty() {
        return Vec::new();
    }
    let color = color();
    ranges
        .into_iter()
        .map(|range| TextSpan {
            range,
            weight: color.is_none().then_some(700),
            italic: false,
            underline: false,
            color,
        })
        .collect()
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

fn opaque_paint(p: &Paint) -> bool {
    match p {
        Paint::Solid(c) => c.clamped().a >= 1.0,
        Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. } => {
            !stops.is_empty() && stops.iter().all(|s| s.color.clamped().a >= 1.0)
        }
    }
}

/// The fully opaque part of a filled rounded rect: the box minus its
/// corner squares, as a horizontal and a vertical band.
fn opaque_bands(phys: Rect, r: &RoundedRectRadii) -> Damage {
    let c = |v: f64| v.ceil().clamp(0.0, u32::MAX as f64) as i64;
    let (l, t, rr, b) = (phys.left(), phys.top(), phys.right(), phys.bottom());
    let top = c(r.top_left.max(r.top_right));
    let bottom = c(r.bottom_left.max(r.bottom_right));
    let left = c(r.top_left.max(r.bottom_left));
    let right = c(r.top_right.max(r.bottom_right));
    let mut d = Damage::new();
    d.add(Rect::from_edges(l, t + top, rr, b - bottom));
    d.add(Rect::from_edges(l + left, t, rr - right, b));
    d
}

fn paint_of(v: Option<&PropValue>) -> Option<Paint> {
    match v? {
        PropValue::Color(c) => Some(Paint::Solid(*c)),
        PropValue::Paint(p) => Some(p.clone()),
        _ => None,
    }
}

/// Corner radii in logical pixels for a box of `w × h` logical pixels.
/// `radius: full` arrives as `Keyword("full")` or infinite radii
/// ([`Corners::FULL`]) and becomes the largest finite radius, which the
/// CSS shrink in [`radii`] turns into a pill; a percentage is of the
/// shorter side. The comma shorthand (`radius: $radius.lg, $radius.lg,
/// 0, 0`) is a `List` of one to four of those, expanded like CSS. NaN and
/// negative radii are square.
fn corners_of(v: Option<&PropValue>, w: f32, h: f32) -> Corners {
    let one = |v: &PropValue| match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) => Some(*n),
        PropValue::Length(Length::Percent(p)) => Some(w.min(h) * p / 100.0),
        PropValue::Keyword(k) if k == "full" => Some(f32::INFINITY),
        _ => None,
    };
    let c = match v {
        Some(PropValue::Corners(c)) => *c,
        Some(PropValue::List(items)) => items
            .iter()
            .map(one)
            .collect::<Option<Vec<f32>>>()
            .and_then(|v| Corners::from_values(&v))
            .unwrap_or_default(),
        Some(v) => one(v).map(Corners::all).unwrap_or_default(),
        None => Corners::default(),
    };
    let one = |r: f32| {
        if r == f32::INFINITY {
            MAX_LOGICAL
        } else {
            finite_or_zero(r).max(0.0)
        }
    };
    Corners {
        top_left: one(c.top_left),
        top_right: one(c.top_right),
        bottom_right: one(c.bottom_right),
        bottom_left: one(c.bottom_left),
    }
}

/// Scales radii to physical pixels and shrinks them like CSS so adjacent
/// corners never overlap (`radius: full` becomes a pill).
fn radii(c: Corners, w: f64, h: f64, s: f64) -> RoundedRectRadii {
    let (tl, tr, br, bl) = (
        (c.top_left as f64 * s).max(0.0),
        (c.top_right as f64 * s).max(0.0),
        (c.bottom_right as f64 * s).max(0.0),
        (c.bottom_left as f64 * s).max(0.0),
    );
    let ratio = |side: f64, a: f64, b: f64| if a + b > side { side / (a + b) } else { 1.0 };
    let f = ratio(w, tl, tr)
        .min(ratio(w, bl, br))
        .min(ratio(h, tl, bl))
        .min(ratio(h, tr, br));
    RoundedRectRadii::new(tl * f, tr * f, br * f, bl * f)
}

fn radii_zero(r: &RoundedRectRadii) -> bool {
    r.top_left <= 0.0 && r.top_right <= 0.0 && r.bottom_right <= 0.0 && r.bottom_left <= 0.0
}

fn shape_path(rect: kurbo::Rect, r: RoundedRectRadii) -> BezPath {
    if radii_zero(&r) {
        rect.to_path(TOLERANCE)
    } else {
        RoundedRect::from_rect(rect, r).to_path(TOLERANCE)
    }
}

fn kurbo_rect(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        r.left() as f64,
        r.top() as f64,
        r.right() as f64,
        r.bottom() as f64,
    )
}

/// Smallest pixel rectangle covering a float rectangle.
fn cover(r: kurbo::Rect) -> Rect {
    Rect::from_edges(
        r.x0.floor() as i64,
        r.y0.floor() as i64,
        r.x1.ceil() as i64,
        r.y1.ceil() as i64,
    )
}

fn hash_f32(h: &mut impl Hasher, v: f32) {
    v.to_bits().hash(h);
}

fn hash_color(h: &mut impl Hasher, c: &Color) {
    for v in [c.r, c.g, c.b, c.a] {
        hash_f32(h, v);
    }
}

fn hash_stops(h: &mut impl Hasher, stops: &[strand_scene::GradientStop]) {
    stops.len().hash(h);
    for st in stops {
        hash_f32(h, st.offset);
        hash_color(h, &st.color);
    }
}

fn hash_paint(h: &mut impl Hasher, p: &Paint) {
    match p {
        Paint::Solid(c) => {
            0u8.hash(h);
            hash_color(h, c);
        }
        Paint::Linear { angle, stops } => {
            1u8.hash(h);
            hash_f32(h, *angle);
            hash_stops(h, stops);
        }
        Paint::Radial { stops } => {
            2u8.hash(h);
            hash_stops(h, stops);
        }
        Paint::Conic { from, stops } => {
            3u8.hash(h);
            hash_f32(h, *from);
            hash_stops(h, stops);
        }
    }
}

fn hash_rect(h: &mut impl Hasher, r: kurbo::Rect) {
    for v in [r.x0, r.y0, r.x1, r.y1] {
        v.to_bits().hash(h);
    }
}

fn hash_path(h: &mut impl Hasher, p: &BezPath) {
    use kurbo::PathEl;
    let mut pt = |p: kurbo::Point| {
        p.x.to_bits().hash(h);
        p.y.to_bits().hash(h);
    };
    for el in p.elements() {
        match *el {
            PathEl::MoveTo(a) => pt(a),
            PathEl::LineTo(a) => pt(a),
            PathEl::QuadTo(a, b) => {
                pt(a);
                pt(b);
            }
            PathEl::CurveTo(a, b, c) => {
                pt(a);
                pt(b);
                pt(c);
            }
            PathEl::ClosePath => pt(kurbo::Point::new(f64::NAN, 0.0)),
        }
    }
}

fn hash_item(h: &mut impl Hasher, item: &Item) {
    match item {
        Item::PushClip(p) => {
            0u8.hash(h);
            hash_path(h, p);
        }
        Item::PopClip => 1u8.hash(h),
        Item::PushOpacity(o) => {
            2u8.hash(h);
            hash_f32(h, *o);
        }
        Item::PopOpacity => 3u8.hash(h),
        Item::PushTransform(a) => {
            8u8.hash(h);
            for v in a.as_coeffs() {
                v.to_bits().hash(h);
            }
        }
        Item::PopTransform => 9u8.hash(h),
        Item::Shadow {
            rect,
            radii,
            std_dev,
            color,
            clip,
            extent,
        } => {
            4u8.hash(h);
            hash_rect(h, *rect);
            for r in radii {
                hash_f32(h, *r);
            }
            hash_f32(h, *std_dev);
            hash_color(h, color);
            hash_path(h, clip);
            hash_rect(h, *extent);
        }
        Item::Fill {
            shape,
            paint,
            frame,
        } => {
            5u8.hash(h);
            match shape {
                FillShape::Rect(r) => hash_rect(h, *r),
                FillShape::Path(p) => hash_path(h, p),
            }
            hash_paint(h, paint);
            hash_rect(h, *frame);
        }
        Item::Border { path, paint, frame } => {
            6u8.hash(h);
            hash_path(h, path);
            hash_paint(h, paint);
            hash_rect(h, *frame);
        }
        Item::Glyphs {
            x,
            y,
            layout,
            color,
        } => {
            7u8.hash(h);
            (x, y, layout.key, layout.scale).hash(h);
            hash_color(h, color);
        }
    }
}

/// The unbounded text request of a text node whose resolved props `get`
/// reads, and its `align`: markup parsed, marks as spans, shaped at
/// `scale` with `font`.
fn natural_spec<'v>(
    get: &impl Fn(Prop) -> Option<&'v PropValue>,
    scope: &TokenScope<'_>,
    font: &Font,
    scale: Scale,
) -> Option<(TextSpec, TextAlign)> {
    let Some(PropValue::Text(text)) = get(Prop::Text) else {
        return None;
    };
    let align = match get(Prop::Align) {
        Some(PropValue::Keyword(k)) if k == "center" => TextAlign::Center,
        Some(PropValue::Keyword(k)) if k == "end" => TextAlign::End,
        _ => TextAlign::Start,
    };
    let ellipsis = match get(Prop::Ellipsis) {
        Some(PropValue::Keyword(k)) => Ellipsis::from_name(k),
        Some(PropValue::Bool(true)) => Some(Ellipsis::End),
        _ => None,
    };
    let max_lines = number(get(Prop::MaxLines))
        .filter(|n| *n >= 1.0)
        .map(|n| n.min(10_000.0) as u32);
    let accent = || match scope.lookup("accent") {
        Some(PropValue::Color(c)) => Some(c),
        _ => None,
    };
    let (shown, mut spans) = match get(Prop::Markup) {
        Some(PropValue::Keyword(k)) if k == "basic" => crate::markup::parse(text, accent()),
        _ => (text.clone(), Vec::new()),
    };
    spans.extend(marks(&shown, get(Prop::Marks), || {
        match get(Prop::MarkColor) {
            Some(PropValue::Color(c)) => Some(*c),
            _ => accent(),
        }
    }));
    Some((
        TextSpec {
            text: shown,
            style: TextStyle {
                font: font.clone(),
                line_height: None,
                align: TextAlign::Start,
                ellipsis,
                max_lines,
                spans,
            },
            max_width: None,
            scale,
        },
        align,
    ))
}

/// The unbounded text requests of the text nodes under the surface node
/// `root` (not in nested surfaces) that its content pass laid out
/// (`laid`), at `scale`: what laying it out by its content measures,
/// before any surface shows it. Rows a virtualised list left out are not
/// walked, so a 2,000-row list asks for its visible rows only.
pub fn natural_texts(
    tree: &SceneTree,
    root: NodeId,
    scale: Scale,
    laid: &HashMap<NodeId, LogicalRect>,
) -> Vec<(NodeId, TextSpec)> {
    let mut out = Vec::new();
    let Some(node) = tree.get(root) else {
        return out;
    };
    let mut inh = Inherited {
        color: None,
        font: None,
        weight: None,
        tokens: vec![&tree.tokens],
        ctx: 0,
        clip: Rect::default(),
        offset: (0.0, 0.0),
        inert: false,
    };
    let mut ancestors = Vec::new();
    let mut up = node.parent;
    while let Some(a) = up.and_then(|p| tree.get(p)) {
        ancestors.push(a);
        up = a.parent;
    }
    for a in ancestors.into_iter().rev() {
        inherit(a, &mut inh);
    }
    fn walk<'a>(
        tree: &'a SceneTree,
        node: &'a Node,
        inh: &Inherited<'a>,
        scale: Scale,
        laid: &HashMap<NodeId, LogicalRect>,
        out: &mut Vec<(NodeId, TextSpec)>,
    ) {
        if !laid.contains_key(&node.id) {
            return;
        }
        let mut inh = inh.clone();
        inherit(node, &mut inh);
        if matches!(node.kind, NodeKind::Text | NodeKind::Button) {
            let scope = TokenScope::new(&inh.tokens);
            let props: Vec<(Prop, Cow<'_, PropValue>)> = node
                .props
                .iter()
                .filter(|e| e.prop != Prop::Tokens)
                .filter_map(|e| scope.resolve(&e.value).map(|v| (e.prop, v)))
                .collect();
            let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());
            let mut font = inh.font.clone().unwrap_or_else(|| default_font(&scope));
            if let Some(w) = inh.weight {
                font.weight = w;
            }
            if let Some((spec, _)) = natural_spec(&get, &scope, &font, scale) {
                out.push((node.id, spec));
            }
        }
        for c in &node.children {
            if let Some(child) = tree.get(*c).filter(|n| !n.kind.is_surface()) {
                walk(tree, child, &inh, scale, laid, out);
            }
        }
    }
    walk(tree, node, &inh, scale, laid, &mut out);
    out
}

impl<'a> Flattener<'a> {
    fn push(&mut self, item: Item, bounds: Rect, sig: &mut DefaultHasher, ink: &mut Rect) {
        let bounds = map_rect(self.xform, bounds);
        hash_item(sig, &item);
        *ink = ink.union(bounds);
        self.out.items.push(DisplayItem { item, bounds });
    }

    /// Pushes a group marker; returns its index so a push marker's bounds
    /// can be set to its group's once known.
    fn marker(&mut self, item: Item) -> usize {
        let bounds = self.surface;
        self.out.items.push(DisplayItem { item, bounds });
        self.out.items.len() - 1
    }

    /// Flattens `node` and its subtree; returns the subtree's ink bounds
    /// (clipped by ancestors).
    fn node(
        &mut self,
        node: &'a Node,
        parent: LogicalRect,
        inh: &Inherited<'a>,
        root: bool,
    ) -> Rect {
        let s = self.scale.as_f64();
        // Token references resolve once per node, against the global table
        // and the `tokens` overrides of this node and its ancestors.
        let mut tokens = inh.tokens.clone();
        if let Some(PropValue::Tokens(t)) = node.get(Prop::Tokens) {
            tokens.push(t);
        }
        let scope = TokenScope::new(&tokens);
        let mut props: Vec<(Prop, Cow<'a, PropValue>)> = node
            .props
            .iter()
            .filter(|e| e.prop != Prop::Tokens)
            .filter_map(|e| scope.resolve(&e.value).map(|v| (e.prop, v)))
            .collect();
        // A node layout did not place (a list row out of view) draws
        // nothing.
        let laid = if root {
            parent
        } else {
            match self.boxes.rects.get(&node.id) {
                Some(r) => *r,
                None => return Rect::default(),
            }
        };
        // Springs: this frame's values of the props in flight.
        let inherited = inh.color.unwrap_or_else(|| default_color(&scope));
        self.anim
            .paint(node, &mut props, &scope, inherited, Some(laid), parent);
        let inert = inh.inert || self.tree.is_ghost(node.id);
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());

        // Inherited props, as the children see them.
        let own_color = match get(Prop::Color) {
            Some(PropValue::Color(c)) => Some(*c),
            _ => inh.color,
        };
        let (own_font, mut weight) = match get(Prop::Font) {
            Some(PropValue::Font(f)) => (Some(sane_font(f.clone())), None),
            _ => (inh.font.clone(), inh.weight),
        };
        if let Some(w) = number(get(Prop::Weight)) {
            weight = Some(w.clamp(1.0, 1000.0) as u16);
        }
        // What this node draws with; theme defaults come from its scope.
        let is_text = matches!(
            node.kind,
            NodeKind::Text | NodeKind::Button | NodeKind::Input
        );
        let color = if is_text {
            own_color.unwrap_or_else(|| default_color(&scope))
        } else {
            Color::BLACK
        };
        let mut font = if is_text {
            own_font.clone().unwrap_or_else(|| default_font(&scope))
        } else {
            Font::default()
        };
        if let Some(w) = weight {
            font.weight = w;
        }

        // Geometry: the laid-out box, moved by the paint offsets (`x`,
        // `y`, and a FLIP glide) of this node and its ancestors. A
        // percentage is of the parent's box, as CSS insets are.
        let glide = self.anim.offset(node.id);
        let own = (
            length(get(Prop::X), parent.w).unwrap_or(0.0) + glide.0,
            length(get(Prop::Y), parent.h).unwrap_or(0.0) + glide.1,
        );
        let offset = (inh.offset.0 + own.0, inh.offset.1 + own.1);
        let rect = LogicalRect::new(
            laid.x + offset.0,
            laid.y + offset.1,
            laid.w.max(0.0),
            laid.h.max(0.0),
        );

        // An `input` shows its `text`, or its `placeholder` in `$fg.muted`
        // while that is empty (plain text: no markup or marks; caret and
        // selection come with the M2 widgets item).
        let input = node.kind == NodeKind::Input;
        let placeholder =
            input && !matches!(get(Prop::Text), Some(PropValue::Text(t)) if !t.is_empty());
        let color = if placeholder {
            match scope.lookup("fg.muted") {
                Some(PropValue::Color(c)) => c,
                _ => color.with_alpha(color.a * 0.6),
            }
        } else {
            color
        };
        let text_get = |p: Prop| match p {
            Prop::Text if placeholder => get(Prop::Placeholder),
            Prop::Markup | Prop::Marks | Prop::Ellipsis | Prop::MaxLines if input => None,
            _ => get(p),
        };
        // Text: the unbounded layout (layout measures with it) and, for a
        // box narrower than it, one shaped for the box's width.
        let mut layout = None;
        // An `input`'s text that is wider than its box is clipped to it.
        let mut clip_text = false;
        if is_text
            && let Some((natural, align)) = natural_spec(&text_get, &scope, &font, self.scale)
        {
            let shaped: &[Shaped] = self.layouts.get(&node.id).map_or(&[], Vec::as_slice);
            let (fit, placed) = if input {
                // One line, never wrapped: wider than the box, it is
                // clipped and shifted so its end (where typing happens)
                // stays in view.
                let placed = pick(shaped, self.scale, None)
                    .or_else(|| shaped.first().map(|c| c.layout.clone()))
                    .map(|l| {
                        let dx = (rect.w - l.size.w).min(0.0);
                        clip_text = dx < 0.0;
                        (l, dx, 0.0)
                    });
                (None, placed)
            } else {
                place_text(shaped, self.scale, rect, align)
            };
            self.out.text.push((node.id, natural.clone()));
            if let Some(w) = fit {
                self.out.text.push((
                    node.id,
                    TextSpec {
                        style: TextStyle {
                            align,
                            ..natural.style
                        },
                        max_width: Some(w),
                        ..natural
                    },
                ));
            }
            layout = placed;
        }

        let phys = self.scale.snap_rect(rect);
        let frame = kurbo_rect(phys);
        let opacity = number(get(Prop::Opacity)).unwrap_or(1.0).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return Rect::default();
        }

        // `scale` and `rotate` about the box's centre: the subtree is drawn
        // under a transform, and its damage is the transformed bounds.
        let zoom = number(get(Prop::Scale)).unwrap_or(1.0).clamp(0.0, 1000.0);
        if zoom <= 0.0 {
            return Rect::default();
        }
        let turn = angle(get(Prop::Rotate)).unwrap_or(0.0) % 360.0;
        let saved = self.xform;
        let transform_group = (zoom != 1.0 || turn != 0.0).then(|| {
            let c = frame.center();
            let local = kurbo::Affine::translate(c.to_vec2())
                * kurbo::Affine::rotate((turn as f64).to_radians())
                * kurbo::Affine::scale(zoom as f64)
                * kurbo::Affine::translate(-c.to_vec2());
            self.xform = saved * local;
            self.marker(Item::PushTransform(self.xform))
        });

        let mut sig = DefaultHasher::new();
        (inh.ctx, node.kind, node.epoch).hash(&mut sig);
        hash_f32(&mut sig, opacity);
        for v in self.xform.as_coeffs() {
            v.to_bits().hash(&mut sig);
        }
        let mut ink = Rect::default();

        let opacity_group = (opacity < 1.0).then(|| self.marker(Item::PushOpacity(opacity)));
        let r = radii(
            corners_of(get(Prop::Radius), rect.w, rect.h),
            frame.width(),
            frame.height(),
            s,
        );
        let box_path = shape_path(frame, r);
        let has_area = !phys.is_empty();

        // Shadows, under the box.
        if has_area && let Some(PropValue::Shadow(list)) = get(Prop::Shadow) {
            for sh in list {
                self.shadow(sh, frame, &r, &box_path, &mut sig, &mut ink);
            }
        }
        // Background. A bar that names none is themed: `$surface` (an
        // edge strip; panels, OSDs and popups stay clear around their
        // content, which carries its own `bg`).
        let bg = match get(Prop::Bg) {
            None if root && node.kind == NodeKind::Bar => match scope.lookup("surface") {
                Some(PropValue::Color(c)) => Some(Paint::Solid(c)),
                _ => None,
            },
            v => paint_of(v),
        };
        if has_area && let Some(paint) = bg {
            if root
                && opacity >= 1.0
                && saved == self.xform
                && self.xform == kurbo::Affine::IDENTITY
                && opaque_paint(&paint)
            {
                self.out.opaque = opaque_bands(phys, &r).clipped(self.surface);
            }
            let shape = if radii_zero(&r) {
                FillShape::Rect(frame)
            } else {
                FillShape::Path(box_path.clone())
            };
            self.push(
                Item::Fill {
                    shape,
                    paint,
                    frame,
                },
                phys,
                &mut sig,
                &mut ink,
            );
        }
        // Border, drawn inside the box.
        if has_area
            && let Some(PropValue::Border(Border { width, paint })) = get(Prop::Border)
            && let Some(width) = finite(*width)
            && width > 0.0
        {
            let bw = (width as f64 * s).round().max(1.0);
            let inner = frame.inflate(-bw, -bw);
            let mut path = box_path.clone();
            if inner.width() > 0.0 && inner.height() > 0.0 {
                let ir = RoundedRectRadii::new(
                    (r.top_left - bw).max(0.0),
                    (r.top_right - bw).max(0.0),
                    (r.bottom_right - bw).max(0.0),
                    (r.bottom_left - bw).max(0.0),
                );
                path.extend(shape_path(inner, ir));
            }
            self.push(
                Item::Border {
                    path,
                    paint: paint.clone(),
                    frame,
                },
                phys,
                &mut sig,
                &mut ink,
            );
        }
        // Text.
        if let Some((l, dx, dy)) = layout {
            // A layout from another scale is drawn resampled (see raster).
            let x = phys.x + (dx as f64 * s).round().clamp(-1e7, 1e7) as i32;
            let y = phys.y + (dy as f64 * s).round().clamp(-1e7, 1e7) as i32;
            let k = self.scale.as_f64() / l.scale.as_f64();
            let bounds = if k == 1.0 {
                l.ink.translate(x, y)
            } else {
                cover(kurbo::Rect::new(
                    x as f64 + l.ink.left() as f64 * k,
                    y as f64 + l.ink.top() as f64 * k,
                    x as f64 + l.ink.right() as f64 * k,
                    y as f64 + l.ink.bottom() as f64 * k,
                ))
                .inflate(1)
            };
            let bounds = if clip_text {
                bounds.intersect(phys).unwrap_or_default()
            } else {
                bounds
            };
            let clip = (clip_text && !bounds.is_empty())
                .then(|| self.marker(Item::PushClip(frame.to_path(0.1))));
            if let Some(i) = clip {
                self.out.items[i].bounds = bounds;
            }
            if !bounds.is_empty() {
                let lines: Vec<(Rect, Color)> = l
                    .runs
                    .iter()
                    .filter_map(|r| Some((r.underline?, r.color.unwrap_or(color))))
                    .collect();
                self.push(
                    Item::Glyphs {
                        x,
                        y,
                        layout: l,
                        color,
                    },
                    bounds,
                    &mut sig,
                    &mut ink,
                );
                for (u, c) in lines {
                    let r = kurbo::Rect::new(
                        x as f64 + u.left() as f64 * k,
                        y as f64 + u.top() as f64 * k,
                        x as f64 + u.right() as f64 * k,
                        y as f64 + u.bottom() as f64 * k,
                    );
                    self.push(
                        Item::Fill {
                            shape: FillShape::Rect(r),
                            paint: Paint::Solid(c),
                            frame: r,
                        },
                        cover(r),
                        &mut sig,
                        &mut ink,
                    );
                }
            }
            if clip.is_some() {
                self.marker(Item::PopClip);
            }
        }

        let bounds = ink.intersect(inh.clip).unwrap_or_default();
        self.out.records.insert(
            node.id,
            NodeRecord {
                bounds,
                sig: sig.finish(),
            },
        );

        // Hit shape: the rounded box, grown by `hit: grow(n)`.
        let grow = match get(Prop::Hit) {
            Some(PropValue::Call { name, args }) if name == "grow" => {
                args.first().and_then(|a| number(Some(a))).unwrap_or(0.0)
            }
            _ => 0.0,
        }
        .clamp(0.0, 1000.0) as f64
            * s;
        if !inert {
            let grown = frame.inflate(grow, grow);
            let radii = RoundedRectRadii::new(
                r.top_left + grow,
                r.top_right + grow,
                r.bottom_right + grow,
                r.bottom_left + grow,
            );
            // Transformed: the untransformed shape, hit through the
            // inverse (a degenerate transform is never hit).
            let inverse = (self.xform != kurbo::Affine::IDENTITY).then(|| {
                if self.xform.determinant().abs() > 1e-12 {
                    self.xform.inverse()
                } else {
                    kurbo::Affine::translate((f64::INFINITY, f64::INFINITY))
                }
            });
            self.out.hits.push(HitBox {
                node: node.id,
                rect: grown,
                radii,
                inverse,
                clip: inh.clip,
            });
        }

        // Children. A `scroll` or `list` always clips its content, and so
        // does a node whose size springs (a toast collapsing to `height:
        // 0`).
        let clips = matches!(get(Prop::Clip), Some(PropValue::Bool(true)))
            || matches!(node.kind, NodeKind::Scroll | NodeKind::List)
            || self.anim.sizing(node.id);
        let mut ctx = DefaultHasher::new();
        (inh.ctx, node.epoch).hash(&mut ctx);
        hash_f32(&mut ctx, opacity);
        let mut child_clip = inh.clip;
        let mut clip_group = None;
        if clips {
            hash_path(&mut ctx, &box_path);
            child_clip = map_rect(self.xform, phys)
                .intersect(inh.clip)
                .unwrap_or_default();
            clip_group = Some(self.marker(Item::PushClip(box_path)));
        }
        let child_inh = Inherited {
            color: own_color,
            font: own_font,
            weight,
            tokens,
            ctx: ctx.finish(),
            clip: child_clip,
            offset,
            inert,
        };
        let mut children = Rect::default();
        if !(clips && child_clip.is_empty()) {
            for c in &node.children {
                // A nested surface (a popup) paints on its own surface.
                if let Some(child) = self.tree.get(*c).filter(|n| !n.kind.is_surface()) {
                    children = children.union(self.node(child, rect, &child_inh, false));
                }
            }
        }
        if let Some(i) = clip_group {
            self.out.items[i].bounds = children;
            self.marker(Item::PopClip);
        }
        let subtree = bounds.union(children);
        if let Some(i) = opacity_group {
            self.out.items[i].bounds = subtree;
            self.marker(Item::PopOpacity);
        }
        if let Some(i) = transform_group {
            self.out.items[i].bounds = subtree;
            self.marker(Item::PopTransform);
            self.xform = saved;
        }
        subtree
    }

    fn shadow(
        &mut self,
        sh: &Shadow,
        frame: kurbo::Rect,
        r: &RoundedRectRadii,
        box_path: &BezPath,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let s = self.scale.as_f64();
        if sh.color.a.is_nan() || sh.color.a <= 0.0 {
            return;
        }
        let spread = finite_or_zero(sh.spread) as f64 * s;
        let (dx, dy) = (
            finite_or_zero(sh.x) as f64 * s,
            finite_or_zero(sh.y) as f64 * s,
        );
        let rect = frame
            .with_origin((frame.x0 + dx, frame.y0 + dy))
            .inflate(spread, spread);
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        let limit = rect.width().min(rect.height()) / 2.0;
        let corner = |v: f64| (v + spread).min(limit).max(0.0) as f32;
        let radii = [
            corner(r.top_left),
            corner(r.top_right),
            corner(r.bottom_right),
            corner(r.bottom_left),
        ];
        // CSS blur radius is twice the Gaussian standard deviation.
        let blur = finite_or_zero(sh.blur).clamp(0.0, MAX_BLUR) as f64;
        let std_dev = blur * s / 2.0;
        let reach = (3.0 * std_dev).ceil() + 1.0;
        let extent = rect.inflate(reach, reach);
        let mut clip = extent.to_path(TOLERANCE);
        clip.extend(box_path.iter());
        self.push(
            Item::Shadow {
                rect,
                radii,
                std_dev: std_dev as f32,
                color: sh.color,
                clip,
                extent,
            },
            cover(extent),
            sig,
            ink,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::{SceneDiff, SceneOp, Transition};

    fn id(i: u32) -> NodeId {
        NodeId::new(i, 0)
    }

    fn tree() -> SceneTree {
        let mut t = SceneTree::new();
        let mut d = SceneDiff::new();
        d.create(id(0), NodeKind::Bar, None, 0)
            .set(id(0), Prop::Bg, PropValue::Color(Color::WHITE))
            .create(id(1), NodeKind::Box, Some(id(0)), 0)
            .set(id(1), Prop::X, PropValue::Number(10.0))
            .set(id(1), Prop::Y, PropValue::Number(4.0))
            .set(id(1), Prop::Size, PropValue::Number(8.0))
            .set(id(1), Prop::Bg, PropValue::Color(Color::BLACK));
        assert!(t.apply(d).is_empty());
        t
    }

    struct NoText;
    impl crate::layout::TextSizes for NoText {
        fn natural(&self, _: NodeId) -> Option<strand_scene::LogicalSize> {
            None
        }
        fn fitted(&self, _: NodeId, _: f32) -> Option<strand_scene::LogicalSize> {
            None
        }
    }

    /// Lays out and flattens the surface `id(0)`.
    fn flat(t: &SceneTree, size: Size, scale: Scale) -> Flattened {
        let l = scale.logical_size(size);
        let boxes = crate::layout::layout(
            t,
            id(0),
            crate::layout::RootSize::Fixed(LogicalRect::new(0.0, 0.0, l.w, l.h)),
            &NoText,
            &mut HashMap::new(),
            &HashMap::new(),
        );
        flatten(
            t,
            id(0),
            size,
            scale,
            &HashMap::new(),
            &boxes,
            &mut Animator::default(),
        )
    }

    /// Text that fits its box is drawn from its unbounded layout, placed
    /// by `align` and centred vertically; a narrower box asks for a layout
    /// of its width and draws the unbounded one until it arrives.
    #[test]
    fn text_is_aligned_in_its_box() {
        use strand_text::{FontConfig, TextEngine, TextKey, TextRequest, test_font_path};
        let data = std::fs::read(test_font_path()).unwrap();
        let mut engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
        let l = Arc::new(engine.layout(&TextRequest {
            key: TextKey(1),
            text: "12:59".into(),
            style: TextStyle::default(),
            max_width: None,
            scale: Scale::ONE,
        }));
        let shaped = vec![Shaped {
            layout: l.clone(),
            max_width: None,
        }];
        let w = l.size.w;
        let rect = LogicalRect::new(0.0, 0.0, 100.0, l.size.h + 10.0);
        for (align, want) in [
            (TextAlign::Start, 0.0),
            (TextAlign::Center, (100.0 - w) / 2.0),
            (TextAlign::End, 100.0 - w),
        ] {
            let (fit, placed) = place_text(&shaped, Scale::ONE, rect, align);
            let (_, dx, dy) = placed.unwrap();
            assert_eq!(fit, None);
            assert!((dx - want).abs() < 1e-3, "{align:?}: {dx} vs {want}");
            let want_dy = if align == TextAlign::Center { 5.0 } else { 0.0 };
            assert!((dy - want_dy).abs() < 1e-3);
        }
        let narrow = LogicalRect::new(0.0, 0.0, (w / 2.0).round(), l.size.h);
        let (fit, placed) = place_text(&shaped, Scale::ONE, narrow, TextAlign::End);
        assert_eq!(fit, Some((w / 2.0).round()));
        assert_eq!(placed.unwrap().1, 0.0, "the stand-in is not aligned");
    }

    #[test]
    fn absolute_layout_and_records() {
        let t = tree();
        let f = flat(&t, Size::new(100, 20), Scale::ONE);
        assert_eq!(f.records[&id(0)].bounds, Rect::new(0, 0, 100, 20));
        assert_eq!(f.records[&id(1)].bounds, Rect::new(10, 4, 8, 8));
        assert_eq!(f.items.len(), 2);
    }

    #[test]
    fn fractional_scale_snaps_edges() {
        let t = tree();
        let s = Scale::new(150).unwrap();
        let f = flat(&t, Size::new(125, 25), s);
        // 10 × 1.25 = 12.5 → 13, 18 × 1.25 = 22.5 → 23.
        assert_eq!(f.records[&id(1)].bounds, Rect::new(13, 5, 10, 10));
    }

    #[test]
    fn signatures_track_paint_changes_only() {
        let mut t = tree();
        let a = flat(&t, Size::new(100, 20), Scale::ONE);
        // Same value again: same signature.
        t.apply_op(SceneOp::SetProp {
            id: id(1),
            prop: Prop::Bg,
            value: PropValue::Color(Color::BLACK),
            transition: Transition::Instant,
        })
        .unwrap();
        let b = flat(&t, Size::new(100, 20), Scale::ONE);
        assert_eq!(a.records, b.records);
        t.apply_op(SceneOp::SetProp {
            id: id(0),
            prop: Prop::Opacity,
            value: PropValue::Number(0.5),
            transition: Transition::Instant,
        })
        .unwrap();
        let c = flat(&t, Size::new(100, 20), Scale::ONE);
        // Parent opacity changes how the child paints.
        assert_ne!(a.records[&id(1)].sig, c.records[&id(1)].sig);
    }

    #[test]
    fn radius_full_is_a_pill_in_every_encoding() {
        let full = [
            PropValue::Keyword("full".into()),
            PropValue::Corners(Corners::FULL),
            PropValue::Number(f32::INFINITY),
            PropValue::Length(Length::Percent(50.0)),
        ];
        for v in full {
            let c = corners_of(Some(&v), 40.0, 10.0);
            let r = radii(c, 40.0, 10.0, 1.0);
            assert_eq!(r.top_left, 5.0, "{v:?}");
            assert_eq!(r.bottom_right, 5.0, "{v:?}");
        }
        for v in [PropValue::Number(f32::NAN), PropValue::Number(-3.0)] {
            assert!(corners_of(Some(&v), 40.0, 10.0).is_zero(), "{v:?}");
        }
    }

    #[test]
    fn radius_lists_expand_like_css() {
        let top = PropValue::List(vec![
            PropValue::Number(14.0),
            PropValue::Number(14.0),
            PropValue::Number(0.0),
            PropValue::Number(0.0),
        ]);
        let c = corners_of(Some(&top), 100.0, 40.0);
        assert_eq!((c.top_left, c.top_right, c.bottom_right), (14.0, 14.0, 0.0));
        let pair = PropValue::List(vec![
            PropValue::Keyword("full".into()),
            PropValue::Length(Length::Percent(10.0)),
        ]);
        let c = corners_of(Some(&pair), 100.0, 40.0);
        assert_eq!(
            (c.top_left, c.top_right, c.bottom_right),
            (MAX_LOGICAL, 4.0, MAX_LOGICAL)
        );
        let bad = PropValue::List(vec![PropValue::Number(1.0); 5]);
        assert!(corners_of(Some(&bad), 100.0, 40.0).is_zero());
    }

    #[test]
    fn marks_become_coloured_spans_on_char_ranges() {
        let pair =
            |a: f32, b: f32| PropValue::List(vec![PropValue::Number(a), PropValue::Number(b)]);
        let v = PropValue::List(vec![pair(0.0, 1.0), pair(2.0, 9.0), pair(3.0, 3.0)]);
        let s = marks("héllo", Some(&v), || Some(Color::WHITE));
        let r: Vec<_> = s.iter().map(|s| s.range.clone()).collect();
        assert_eq!(r, vec![0..1, 3..6]);
        assert!(s.iter().all(|s| s.color == Some(Color::WHITE)));
        let s = marks("héllo", Some(&v), || None);
        assert_eq!(s[0].weight, Some(700));
    }

    #[test]
    fn radii_shrink_like_css() {
        let r = radii(Corners::all(999.0), 40.0, 10.0, 1.0);
        assert_eq!(r.top_left, 5.0);
        let r = radii(
            Corners {
                top_left: 14.0,
                top_right: 14.0,
                bottom_right: 0.0,
                bottom_left: 0.0,
            },
            100.0,
            100.0,
            1.5,
        );
        assert_eq!((r.top_left, r.bottom_left), (21.0, 0.0));
    }
}

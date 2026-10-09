//! Flex layout with taffy, on the render thread (design.md, "Layout,
//! animation and input").
//!
//! Each layout pass builds a taffy tree for one surface's subtree from the
//! resolved props, computes it and reads back one logical box per node, in
//! surface coordinates. Paint-only props (`x`, `y`, `scale`, `rotate`,
//! `opacity`, colours, shadows) are not read here: the flattener applies
//! `x`/`y` as an offset of the subtree, so changing them never relayouts.
//!
//! Containers:
//!
//! - `row`, `start`, `center`, `end`: a flex row; children are centred on
//!   the cross axis unless `align` says otherwise. `start`, `center` and
//!   `end` pack their children to their own side.
//! - `col`, `scroll`: a flex column; children stretch across.
//! - `stack`, `box`, surface roots and any other container: every child in
//!   one cell, stretched where its size is auto.
//! - `grid`: `columns: n` auto-sized columns.
//! - `split`: a grid of `1fr auto 1fr` with `start`, `center` and `end` in
//!   their own column, so the centre is truly centred whatever the sides
//!   hold.
//! - `list`: virtualised. Taffy sees it as one leaf sized from its rows'
//!   heights (measured ones, else an estimate); only the rows inside its
//!   viewport are laid out, each as a taffy root of its own.
//! - `spacer`: grows to take the space left.
//!
//! Text is measured from its delivered layouts (see [`TextSizes`]): never
//! blocking, a text not shaped yet is estimated from its length, and the
//! renderer lays out again when the layout arrives.

use std::collections::{HashMap, HashSet};

use strand_scene::{
    Font, Insets, Length, LogicalRect, LogicalSize, NodeId, NodeKind, Prop, PropValue, Shadow,
    TokenScope, TokenTable,
};
use taffy::LayoutInput;
use taffy::prelude::{
    AlignContent, AlignItems, AvailableSpace, Dimension, LengthPercentageAuto, TaffyTree, auto,
    length, percent,
};

use crate::anim::SizeMap;
use crate::tree::{Node, SceneTree};

mod list;
mod style;
mod text;

use list::rows_height;
pub use list::{
    FLING_DECAY, LAYOUT_OVERSCAN, ListBox, ListWindow, ScrollState, WINDOW_NEED, WINDOW_OVERSCAN,
};
pub(crate) use list::{layout_margin, relayout_list, scrolls_content, translate_under};
pub(crate) use text::TextSizes;
use text::measure_leaf;

/// Width of `1ch` as a fraction of the font size (the `0` advance of
/// common UI fonts is 0.55–0.62 em).
pub const CH_EM: f32 = 0.6;

/// Row height a list assumes for rows it has not laid out yet, before any
/// row was measured.
pub const LIST_ROW_ESTIMATE: f32 = 32.0;

/// Largest size, logical pixels, a content-sized surface takes on either
/// axis (decisions.md, wave3-pixels): past it, `scroll` and `list` take
/// the rest, and a list lays out only the rows this much shows.
pub const MAX_CONTENT_SIZE: f32 = 4096.0;

/// Largest length read from props, logical pixels.
const MAX_LEN: f32 = 1e6;

/// One layout pass's result for a surface.
#[derive(Clone, Debug, Default)]
pub struct Boxes {
    /// Every laid-out node's border box in surface logical pixels, before
    /// paint offsets (`x`, `y`). Rows a list did not lay out are absent.
    pub rects: HashMap<NodeId, LogicalRect>,
    /// The root's size as laid out (its content size for a content-sized
    /// surface).
    pub size: LogicalSize,
    /// How far shadows reach past the root's box.
    pub overhang: Insets,
    /// List rows laid out in this pass, over all lists.
    pub rows_laid_out: usize,
    /// Rows that exist in lists, laid out or not.
    pub rows_total: usize,
    /// A list measured rows whose heights differed from its estimate:
    /// one more pass places everything where it belongs.
    pub(crate) unsettled: bool,
    /// (M4) Each `scroll` and `list` laid out: the offset these boxes
    /// show it at and which of its rows they hold.
    pub lists: HashMap<NodeId, ListBox>,
}

/// How the root is sized.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RootSize {
    /// The surface's box: `frame`.
    Fixed(LogicalRect),
    /// Sized by its content, with an optional definite width (a bar's
    /// output width) and height (a bar's thickness).
    Content {
        width: Option<f32>,
        height: Option<f32>,
    },
}

enum Ctx {
    Text {
        node: NodeId,
        font: f32,
        chars: usize,
        /// Characters in its longest word: its min-content width, as a
        /// share of its natural width.
        word: usize,
        /// `ellipsis` or `max_lines`: it may be narrower than its text.
        shrinks: bool,
        wraps: bool,
        max_lines: Option<u32>,
        /// A `text` showing nothing (`h.app.comment ?? ""`): no line, as
        /// an empty block in CSS.
        empty: bool,
    },
    /// A leaf with a size of its own when its props give none.
    Fixed(f32, f32),
    /// Square, `side` across when its props give no size, else as tall as
    /// it is wide (or as wide as tall): an `image` before its source is
    /// decoded has no size of its own.
    Square(f32),
    /// A `segmented` of `n` options: as wide as its widest label (plus
    /// padding) times `n`.
    Segmented {
        node: NodeId,
        n: usize,
        font: f32,
    },
    List(NodeId),
}

/// The grid row and column a container puts a child of a kind in.
type CellOf = Box<dyn Fn(NodeKind) -> (i16, i16)>;

/// Per-pass state while building the taffy tree.
struct Build<'a> {
    tree: &'a SceneTree,
    taffy: TaffyTree<Ctx>,
    /// taffy node → scene node.
    map: Vec<(taffy::NodeId, NodeId)>,
    lists: Vec<(NodeId, taffy::NodeId, Inh<'a>)>,
    scrolls: Vec<NodeId>,
    /// Shadow lists by node, with the node's offset when it is placed by
    /// coordinates (`place: absolute`: its `x`/`y` are where it is). A
    /// flow node's `x`/`y` are paint offsets (the channels enter and exit
    /// animations spring) and count as zero: the overhang is the
    /// shadows' at rest, so sliding a shadowed toast never resizes the
    /// buffer, and content moved past it is clipped to the surface.
    shadows: Vec<(NodeId, (f32, f32), Vec<Shadow>)>,
    vertical_split: bool,
    /// Sizes springs (and exit poses) force on nodes this pass.
    sizes: &'a SizeMap,
    /// For the child [`Build::node`] is about to build: its parent's
    /// main axis (0 width, 1 height) and the margins added before and
    /// after it there, folding the gap next to a collapsing sibling.
    fold: Option<(usize, [f32; 2])>,
}

/// What a node inherits for layout: token tables and font size.
#[derive(Clone)]
struct Inh<'a> {
    tokens: Vec<&'a TokenTable>,
    font: Font,
}

fn finite(v: f32) -> Option<f32> {
    v.is_finite().then(|| v.clamp(-MAX_LEN, MAX_LEN))
}

fn num(v: Option<&PropValue>) -> Option<f32> {
    match v? {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) => finite(*n),
        _ => None,
    }
}

/// A length prop: logical pixels, a percentage of the parent, or auto.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Len {
    Px(f32),
    Pct(f32),
    Auto,
}

impl Len {
    fn dim(self) -> Dimension {
        match self {
            Len::Px(v) => length(v),
            Len::Pct(p) => percent(p),
            Len::Auto => auto(),
        }
    }

    fn lpa(self) -> LengthPercentageAuto {
        match self {
            Len::Px(v) => LengthPercentageAuto::length(v),
            Len::Pct(p) => LengthPercentageAuto::percent(p),
            Len::Auto => LengthPercentageAuto::auto(),
        }
    }
}

/// A length prop; `ch` against the font size.
fn dim(v: Option<&PropValue>, font: f32) -> Option<Len> {
    Some(match v? {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) => Len::Px(finite(*n)?.max(0.0)),
        PropValue::Length(Length::Percent(p)) => Len::Pct(finite(*p)? / 100.0),
        PropValue::Length(Length::Ch(c)) => Len::Px((finite(*c)? * CH_EM * font).max(0.0)),
        PropValue::Length(Length::Auto) => Len::Auto,
        PropValue::Keyword(k) if k == "auto" => Len::Auto,
        _ => return None,
    })
}

fn insets(v: Option<&PropValue>) -> Option<Insets> {
    let i = v?.insets()?;
    [i.top, i.right, i.bottom, i.left]
        .iter()
        .all(|v| v.is_finite())
        .then_some(i)
}

fn keyword(v: Option<&PropValue>) -> Option<&str> {
    match v? {
        PropValue::Keyword(k) => Some(k),
        _ => None,
    }
}

fn align_items(k: &str) -> Option<AlignItems> {
    Some(match k {
        "start" => AlignItems::FLEX_START,
        "center" => AlignItems::CENTER,
        "end" => AlignItems::FLEX_END,
        "stretch" => AlignItems::STRETCH,
        "baseline" => AlignItems::BASELINE,
        _ => return None,
    })
}

fn justify(k: &str) -> Option<AlignContent> {
    Some(match k {
        "start" => AlignContent::FLEX_START,
        "center" => AlignContent::CENTER,
        "end" => AlignContent::FLEX_END,
        "space_between" => AlignContent::SPACE_BETWEEN,
        "space_around" => AlignContent::SPACE_AROUND,
        "space_evenly" => AlignContent::SPACE_EVENLY,
        _ => return None,
    })
}

/// True for nodes their parent neither lays out nor paints: a nested
/// surface (a popup paints on its own surface) and a `tooltip { … }`
/// element, whose rich content is not drawn yet (the checker says so with
/// `check::not_drawn_yet`; logic mounts it only while its element is
/// hovered).
pub fn out_of_flow(kind: NodeKind) -> bool {
    kind.is_surface() || kind == NodeKind::Tooltip
}

/// True for kinds whose children are not laid out in flow (they draw
/// their own content).
pub fn is_leaf(kind: NodeKind) -> bool {
    use NodeKind::*;
    matches!(
        kind,
        Text | Icon
            | Image
            | Button
            | Slider
            | Input
            | Meter
            | Segmented
            | Spacer
            | Canvas
            | Arc
            | Graph
            | Spectrum
            | Shader
            | Svg
            | Lottie
            | Thumbnail
            | Particles
            | Effect
    )
}

/// True if `id` has a fixed width and height in logical pixels (`width`
/// and `height`, or `size`): its children can be laid out again without
/// it or anything around it moving (a size spring's relayout stops
/// there).
pub(crate) fn size_stable(tree: &SceneTree, id: NodeId) -> bool {
    let Some(node) = tree.get(id) else {
        return false;
    };
    let inh = inherited_at(tree, id);
    let mut tokens = inh.tokens.clone();
    if let Some(PropValue::Tokens(t)) = node.get(Prop::Tokens) {
        tokens.push(t);
    }
    let scope = TokenScope::new(&tokens);
    let get = |p: Prop| node.get(p).and_then(|v| scope.resolve(v));
    let font = inh.font.size;
    let size = dim(get(Prop::Size).as_deref(), font);
    let px = |p: Prop| matches!(dim(get(p).as_deref(), font).or(size), Some(Len::Px(_)));
    px(Prop::Width) && px(Prop::Height)
}

/// Applies `node`'s inherited props for its children.
fn inherit<'a>(node: &'a Node, inh: &mut Inh<'a>) {
    if let Some(PropValue::Tokens(t)) = node.get(Prop::Tokens) {
        inh.tokens.push(t);
    }
    let scope = TokenScope::new(&inh.tokens);
    if let Some(v) = node.get(Prop::Font).and_then(|v| scope.resolve(v))
        && let PropValue::Font(f) = v.as_ref()
        && f.size.is_finite()
        && f.size > 0.0
    {
        inh.font = f.clone();
    }
}

/// The inherited state at `id`: its ancestors' tokens and font.
fn inherited_at<'a>(tree: &'a SceneTree, id: NodeId) -> Inh<'a> {
    let mut chain = Vec::new();
    let mut up = tree.get(id).and_then(|n| n.parent);
    while let Some(a) = up.and_then(|p| tree.get(p)) {
        chain.push(a);
        up = a.parent;
    }
    let mut inh = Inh {
        tokens: vec![&tree.tokens],
        font: Font::default(),
    };
    for a in chain.into_iter().rev() {
        inherit(a, &mut inh);
    }
    inh
}

/// Lays out the subtree under the surface node `root`.
pub(crate) fn layout(
    tree: &SceneTree,
    root: NodeId,
    size: RootSize,
    texts: &dyn TextSizes,
    scrolls: &mut HashMap<NodeId, ScrollState>,
    sizes: &SizeMap,
) -> Boxes {
    let mut out = Boxes::default();
    let Some(node) = tree.get(root) else {
        return out;
    };
    let inh = inherited_at(tree, root);
    // A side bar's `split` runs down; `root` may be a subtree of it.
    let surface = tree.root_of(root).and_then(|r| tree.get(r)).unwrap_or(node);
    let vertical_split = matches!(
        surface.get(Prop::Edge),
        Some(PropValue::Keyword(k)) if k == "left" || k == "right"
    );
    let mut b = Build::new(tree, vertical_split, sizes);
    let Some(t) = b.node(node, &inh, true) else {
        return out;
    };
    let (origin, avail) = match size {
        RootSize::Fixed(frame) => {
            if let Ok(mut s) = b.taffy.style(t).cloned() {
                s.size = taffy::Size {
                    width: length(frame.w.max(0.0)),
                    height: length(frame.h.max(0.0)),
                };
                s.margin = taffy::Rect::zero();
                s.min_size = taffy::Size::auto();
                s.max_size = taffy::Size::auto();
                let _ = b.taffy.set_style(t, s);
            }
            (
                (frame.x, frame.y),
                taffy::Size {
                    width: AvailableSpace::Definite(frame.w),
                    height: AvailableSpace::Definite(frame.h),
                },
            )
        }
        RootSize::Content { width, height } => {
            if let Ok(mut s) = b.taffy.style(t).cloned() {
                if let Some(w) = width {
                    s.size.width = length(w);
                }
                if let Some(h) = height {
                    s.size.height = length(h);
                }
                s.margin = taffy::Rect::zero();
                let _ = b.taffy.set_style(t, s);
            }
            (
                (0.0, 0.0),
                taffy::Size {
                    width: width.map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
                    height: height.map_or(AvailableSpace::MaxContent, AvailableSpace::Definite),
                },
            )
        }
    };
    b.compute(t, avail, texts, scrolls);
    if matches!(size, RootSize::Content { .. })
        && let Ok(l) = b.taffy.layout(t).map(|l| l.size)
        && (l.width > MAX_CONTENT_SIZE || l.height > MAX_CONTENT_SIZE)
        && let Ok(mut s) = b.taffy.style(t).cloned()
    {
        // Taller (or wider) than any output: laid out again at the cap,
        // so what overflows scrolls and a list lays out only what the
        // capped box shows.
        if l.width > MAX_CONTENT_SIZE {
            s.size.width = length(MAX_CONTENT_SIZE);
        }
        if l.height > MAX_CONTENT_SIZE {
            s.size.height = length(MAX_CONTENT_SIZE);
        }
        let _ = b.taffy.set_style(t, s);
        b.compute(t, avail, texts, scrolls);
    }
    b.read_back(t, origin, scrolls, &mut out);
    if let Some(r) = out.rects.get(&root) {
        out.size = LogicalSize::new(r.w, r.h);
    }
    b.place_lists(texts, scrolls, &mut out);
    // Shadow reach past the root's box.
    if let Some(rb) = out.rects.get(&root).copied() {
        let mut o = Insets::default();
        for (id, (ox, oy), list) in &b.shadows {
            let Some(r) = out.rects.get(id) else { continue };
            let r = LogicalRect::new(r.x + ox, r.y + oy, r.w, r.h);
            for sh in list {
                if sh.color.a.is_nan() || sh.color.a <= 0.0 {
                    continue;
                }
                let spread = finite(sh.spread).unwrap_or(0.0);
                let blur = finite(sh.blur).unwrap_or(0.0).clamp(0.0, 1000.0);
                let reach = (1.5 * blur).ceil() + 1.0 + spread;
                let (dx, dy) = (finite(sh.x).unwrap_or(0.0), finite(sh.y).unwrap_or(0.0));
                o.left = o.left.max(rb.x - (r.x + dx - reach));
                o.top = o.top.max(rb.y - (r.y + dy - reach));
                o.right = o.right.max(r.x + r.w + dx + reach - (rb.x + rb.w));
                o.bottom = o.bottom.max(r.y + r.h + dy + reach - (rb.y + rb.h));
            }
        }
        let c = |v: f32| v.max(0.0).ceil();
        out.overhang = Insets {
            top: c(o.top),
            right: c(o.right),
            bottom: c(o.bottom),
            left: c(o.left),
        };
    }
    out
}

impl<'a> Build<'a> {
    fn new(tree: &'a SceneTree, vertical_split: bool, sizes: &'a SizeMap) -> Self {
        // Rounded in `read_back`, from absolute positions: taffy rounds
        // each location relative to its parent, so a box under two
        // half-pixel offsets (the `end` of a `split` whose centre is an odd
        // number of pixels narrower than the bar) lands a pixel off, and
        // moves whenever the centre's width changes parity.
        let mut taffy = TaffyTree::new();
        taffy.disable_rounding();
        Build {
            sizes,
            fold: None,
            tree,
            taffy,
            map: Vec::new(),
            lists: Vec::new(),
            scrolls: Vec::new(),
            shadows: Vec::new(),
            vertical_split,
        }
    }

    /// Computes the tree under `t`, with its lists measured from their
    /// rows.
    fn compute(
        &mut self,
        t: taffy::NodeId,
        avail: taffy::Size<AvailableSpace>,
        texts: &dyn TextSizes,
        scrolls: &mut HashMap<NodeId, ScrollState>,
    ) {
        let mut lists: HashMap<NodeId, f32> = HashMap::new();
        for (id, _, _) in &self.lists {
            let st = scrolls.entry(*id).or_default();
            let gap = self
                .tree
                .get(*id)
                .and_then(|n| num(n.get(Prop::Gap)))
                .unwrap_or(0.0)
                .max(0.0);
            lists.insert(*id, rows_height(self.tree, *id, st, gap));
        }
        let _ = self.taffy.compute_layout_with_measure(
            t,
            avail,
            |inputs: LayoutInput, _, ctx, style| measure_leaf(texts, &lists, inputs, ctx, style),
        );
    }

    /// Reads back the boxes under `t` in surface coordinates, from
    /// `origin`, with scroll offsets applied to scrolled content.
    fn read_back(
        &self,
        t: taffy::NodeId,
        origin: (f32, f32),
        scrolls: &mut HashMap<NodeId, ScrollState>,
        out: &mut Boxes,
    ) {
        let ids: HashMap<taffy::NodeId, NodeId> = self.map.iter().copied().collect();
        let scroll_ids: HashSet<NodeId> = self.scrolls.iter().copied().collect();
        // Each box is snapped to whole logical pixels from its unrounded
        // position relative to `t` (`cx`, `cy`), so its edges depend only
        // on where they fall, never on its ancestors' rounding. `ox`, `oy`
        // carry the origin and scroll offsets, which stay unrounded.
        let mut stack = vec![(t, 0.0f32, 0.0f32, origin.0, origin.1)];
        while let Some((tn, px, py, ox, oy)) = stack.pop() {
            let Ok(l) = self.taffy.layout(tn) else {
                continue;
            };
            let (cx, cy) = (px + l.location.x, py + l.location.y);
            let (x0, y0) = (cx.round(), cy.round());
            let w = (cx + l.size.width).round() - x0;
            let h = (cy + l.size.height).round() - y0;
            let Some(&id) = ids.get(&tn) else { continue };
            out.rects
                .insert(id, LogicalRect::new(ox + x0, oy + y0, w, h));
            let mut dy = 0.0;
            if scroll_ids.contains(&id) {
                let st = scrolls.entry(id).or_default();
                st.viewport = h;
                // The content's extent: its lowest child, plus the bottom
                // pad.
                let bottom = self
                    .taffy
                    .children(tn)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|k| self.taffy.layout(*k).ok())
                    .map(|k| (cy + k.location.y + k.size.height).round() - y0)
                    .fold(0.0f32, f32::max);
                st.content = (bottom + l.padding.bottom.round()).max(h);
                if !st.offset.is_finite() {
                    st.offset = 0.0;
                }
                st.offset = st.offset.clamp(0.0, (st.content - st.viewport).max(0.0));
                st.shown = st.offset;
                dy = st.offset;
                out.lists.insert(
                    id,
                    ListBox {
                        applied: dy,
                        laid: None,
                        content: LogicalRect::new(ox + x0, oy + y0, w, h),
                        bounds: (0.0, (st.content - st.viewport).max(0.0)),
                    },
                );
            }
            if let Ok(kids) = self.taffy.children(tn) {
                for k in kids {
                    stack.push((k, cx, cy, ox, oy - dy));
                }
            }
        }
    }
}

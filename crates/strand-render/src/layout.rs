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
use taffy::prelude::{
    AlignContent, AlignItems, AvailableSpace, Dimension, Display, FlexDirection, GridPlacement,
    LengthPercentage, LengthPercentageAuto, Line, Position, Style, TaffyTree, auto, fr, length,
    line, minmax, percent,
};
use taffy::{LayoutInput, LayoutOutput, Overflow, compute_leaf_layout};

use crate::tree::{Node, SceneTree};

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

/// Delivered text layouts, as layout sees them: sizes in logical pixels.
pub(crate) trait TextSizes {
    /// The size of `node`'s text shaped without a width bound.
    fn natural(&self, node: NodeId) -> Option<LogicalSize>;
    /// The size of `node`'s text shaped for a line box `width` wide
    /// (wrapped or ellipsised).
    fn fitted(&self, node: NodeId, width: f32) -> Option<LogicalSize>;
}

/// Scroll position and row heights of a `scroll` or `list`, kept across
/// layout passes.
#[derive(Clone, Debug, Default)]
pub struct ScrollState {
    /// How far the content is scrolled, logical pixels from the top.
    pub offset: f32,
    /// Measured row heights of a list, by row node.
    pub heights: HashMap<NodeId, f32>,
    /// The visible height and the content height of the last pass.
    pub viewport: f32,
    pub content: f32,
}

impl ScrollState {
    /// Scrolls by `dy` logical pixels, kept within the content. Returns
    /// true if the offset moved.
    pub fn scroll_by(&mut self, dy: f32) -> bool {
        if !dy.is_finite() {
            return false;
        }
        let max = (self.content - self.viewport).max(0.0);
        let new = (self.offset + dy).clamp(0.0, max);
        let moved = new != self.offset;
        self.offset = new;
        moved
    }

    fn estimate(&self) -> f32 {
        if self.heights.is_empty() {
            LIST_ROW_ESTIMATE
        } else {
            self.heights.values().sum::<f32>() / self.heights.len() as f32
        }
    }
}

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
    },
    /// A leaf with a size of its own when its props give none.
    Fixed(f32, f32),
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

impl<'a> Build<'a> {
    fn node(&mut self, node: &'a Node, inh: &Inh<'a>, root: bool) -> Option<taffy::NodeId> {
        if !root && node.kind.is_surface() {
            return None;
        }
        let mut inh = inh.clone();
        inherit(node, &mut inh);
        let scope = TokenScope::new(&inh.tokens);
        let get = |p: Prop| node.get(p).and_then(|v| scope.resolve(v));
        let font = inh.font.size;
        let mut style = Style::default();
        let kind = node.kind;

        // Box sizes.
        let size = dim(get(Prop::Size).as_deref(), font);
        if let Some(w) = dim(get(Prop::Width).as_deref(), font).or(size) {
            style.size.width = w.dim();
        }
        if let Some(h) = dim(get(Prop::Height).as_deref(), font).or(size) {
            style.size.height = h.dim();
        }
        if let Some(d) = dim(get(Prop::MinWidth).as_deref(), font) {
            style.min_size.width = d.lpa();
        }
        if let Some(d) = dim(get(Prop::MinHeight).as_deref(), font) {
            style.min_size.height = d.lpa();
        }
        if let Some(d) = dim(get(Prop::MaxWidth).as_deref(), font) {
            style.max_size.width = d.lpa();
        }
        if let Some(d) = dim(get(Prop::MaxHeight).as_deref(), font) {
            style.max_size.height = d.lpa();
        }
        if let Some(m) = insets(get(Prop::Margin).as_deref()) {
            style.margin = taffy::Rect {
                left: LengthPercentageAuto::length(m.left),
                right: LengthPercentageAuto::length(m.right),
                top: LengthPercentageAuto::length(m.top),
                bottom: LengthPercentageAuto::length(m.bottom),
            };
        }
        if let Some(p) = insets(get(Prop::Pad).as_deref()) {
            let l = |v: f32| LengthPercentage::length(v.max(0.0));
            style.padding = taffy::Rect {
                left: l(p.left),
                right: l(p.right),
                top: l(p.top),
                bottom: l(p.bottom),
            };
        }
        if let Some(g) = num(get(Prop::Gap).as_deref()) {
            let g = LengthPercentage::length(g.max(0.0));
            style.gap = taffy::Size {
                width: g,
                height: g,
            };
        }
        if let Some(g) = num(get(Prop::Grow).as_deref()) {
            style.flex_grow = g.max(0.0);
        }
        if let Some(s) = num(get(Prop::Shrink).as_deref()) {
            style.flex_shrink = s.max(0.0);
        }
        if keyword(get(Prop::Place).as_deref()) == Some("absolute") {
            style.position = Position::Absolute;
            style.inset = taffy::Rect {
                left: LengthPercentageAuto::length(0.0),
                top: LengthPercentageAuto::length(0.0),
                right: LengthPercentageAuto::auto(),
                bottom: LengthPercentageAuto::auto(),
            };
        }
        let align = keyword(get(Prop::Align).as_deref()).and_then(align_items);
        let justify_v = keyword(get(Prop::Justify).as_deref()).and_then(justify);

        // Leaves.
        if is_leaf(kind) {
            let ctx = match kind {
                NodeKind::Text | NodeKind::Button => {
                    let (text, word) = match get(Prop::Text).as_deref() {
                        Some(PropValue::Text(t)) => (
                            t.chars().count(),
                            t.split_whitespace()
                                .map(|w| w.chars().count())
                                .max()
                                .unwrap_or(0),
                        ),
                        _ => (0, 0),
                    };
                    let ellipsis = !matches!(
                        get(Prop::Ellipsis).as_deref(),
                        None | Some(PropValue::Bool(false))
                    ) && keyword(get(Prop::Ellipsis).as_deref()) != Some("none");
                    let max_lines = num(get(Prop::MaxLines).as_deref())
                        .filter(|n| *n >= 1.0)
                        .map(|n| n.min(10_000.0) as u32);
                    if ellipsis {
                        style.min_size.width = LengthPercentageAuto::length(0.0);
                    }
                    Some(Ctx::Text {
                        node: node.id,
                        font,
                        chars: text,
                        word,
                        shrinks: ellipsis || max_lines.is_some(),
                        wraps: !ellipsis || max_lines.is_some_and(|n| n > 1),
                        max_lines,
                    })
                }
                NodeKind::Spacer => {
                    if get(Prop::Grow).is_none() {
                        style.flex_grow = 1.0;
                    }
                    style.flex_basis = length(0.0);
                    None
                }
                NodeKind::Icon => Some(Ctx::Fixed(16.0, 16.0)),
                NodeKind::Slider => Some(Ctx::Fixed(0.0, 16.0)),
                NodeKind::Meter => Some(Ctx::Fixed(0.0, 4.0)),
                NodeKind::Input => Some(Ctx::Fixed(0.0, (font * 1.25).ceil())),
                NodeKind::Segmented => Some(Ctx::Fixed(0.0, (font * 1.8).ceil())),
                _ => None,
            };
            let t = match ctx {
                Some(c) => self.taffy.new_leaf_with_context(style, c).ok()?,
                None => self.taffy.new_leaf(style).ok()?,
            };
            self.map.push((t, node.id));
            self.shadow(node, &get);
            return Some(t);
        }

        // Containers.
        let mut children: Vec<(NodeKind, &'a Node)> = node
            .children
            .iter()
            .filter_map(|c| self.tree.get(*c))
            .filter(|c| !c.kind.is_surface())
            .map(|c| (c.kind, c))
            .collect();
        let row_like = |style: &mut Style, j: AlignContent| {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Row;
            style.align_items = Some(align.unwrap_or(AlignItems::CENTER));
            style.justify_content = Some(justify_v.unwrap_or(j));
        };
        let mut grid_cell: Option<CellOf> = None;
        match kind {
            NodeKind::Row => row_like(&mut style, AlignContent::FLEX_START),
            NodeKind::Start | NodeKind::Center | NodeKind::End if self.vertical_split => {
                style.display = Display::Flex;
                style.flex_direction = FlexDirection::Column;
                style.align_items = Some(align.unwrap_or(AlignItems::CENTER));
                style.justify_content = Some(justify_v.unwrap_or(match kind {
                    NodeKind::Start => AlignContent::FLEX_START,
                    NodeKind::Center => AlignContent::CENTER,
                    _ => AlignContent::FLEX_END,
                }));
            }
            NodeKind::Start => row_like(&mut style, AlignContent::FLEX_START),
            NodeKind::Center => row_like(&mut style, AlignContent::CENTER),
            NodeKind::End => row_like(&mut style, AlignContent::FLEX_END),
            NodeKind::Col | NodeKind::Scroll => {
                style.display = Display::Flex;
                style.flex_direction = FlexDirection::Column;
                style.align_items = Some(align.unwrap_or(AlignItems::STRETCH));
                if let Some(j) = justify_v {
                    style.justify_content = Some(j);
                }
                if kind == NodeKind::Scroll {
                    style.overflow.y = Overflow::Scroll;
                    style.scrollbar_width = 0.0;
                    self.scrolls.push(node.id);
                }
            }
            NodeKind::List => {
                // A leaf to taffy; its rows are laid out after.
                style.overflow.y = Overflow::Scroll;
                style.scrollbar_width = 0.0;
                let t = self
                    .taffy
                    .new_leaf_with_context(style, Ctx::List(node.id))
                    .ok()?;
                self.map.push((t, node.id));
                self.lists.push((node.id, t, inh.clone()));
                self.shadow(node, &get);
                return Some(t);
            }
            NodeKind::Grid => {
                style.display = Display::Grid;
                let n =
                    num(get(Prop::Columns).as_deref()).map_or(1, |n| n.clamp(1.0, 1000.0) as usize);
                style.grid_template_columns = (0..n).map(|_| auto()).collect();
                // Tracks keep their content size: the grid does not spread
                // its cells over a wider box.
                style.justify_content = Some(justify_v.unwrap_or(AlignContent::START));
                style.align_content = Some(AlignContent::START);
                if let Some(a) = align {
                    style.align_items = Some(a);
                    style.justify_items = Some(a);
                }
            }
            NodeKind::Split => {
                style.display = Display::Grid;
                let side = || minmax(length(0.0), fr(1.0));
                let tracks = vec![side(), auto(), side()];
                if self.vertical_split {
                    style.grid_template_rows = tracks;
                    style.grid_template_columns = vec![fr(1.0)];
                } else {
                    style.grid_template_columns = tracks;
                    style.grid_template_rows = vec![fr(1.0)];
                }
                let vertical = self.vertical_split;
                grid_cell = Some(Box::new(move |k| {
                    let i = match k {
                        NodeKind::Start => 1,
                        NodeKind::End => 3,
                        _ => 2,
                    };
                    if vertical { (i, 1) } else { (1, i) }
                }));
                // Sections in source order of start, center, end.
                children.sort_by_key(|(k, _)| match k {
                    NodeKind::Start => 0,
                    NodeKind::Center => 1,
                    NodeKind::End => 2,
                    _ => 1,
                });
            }
            _ => {
                // A stack: every child in one cell. The cell's smallest
                // size is none (`minmax(0, 1fr)`, as `split`'s sides), so
                // content wider than the box wraps or overflows inside it
                // instead of widening it.
                style.display = Display::Grid;
                let cell = || minmax(length(0.0), fr(1.0));
                style.grid_template_columns = vec![cell()];
                style.grid_template_rows = vec![cell()];
                if let Some(a) = align {
                    style.align_items = Some(a);
                    style.justify_items = Some(a);
                }
                grid_cell = Some(Box::new(|_| (1, 1)));
            }
        }
        let mut kids = Vec::with_capacity(children.len());
        let mut child_inh = inh.clone();
        child_inh.font = inh.font.clone();
        for (_, c) in children {
            if let Some(t) = self.node(c, &child_inh, false) {
                // A scroll's content scrolls instead of shrinking.
                if kind == NodeKind::Scroll
                    && c.get(Prop::Shrink).is_none()
                    && let Ok(mut s) = self.taffy.style(t).cloned()
                {
                    s.flex_shrink = 0.0;
                    let _ = self.taffy.set_style(t, s);
                }
                if let Some(cell) = &grid_cell {
                    let (r, col) = cell(c.kind);
                    if let Ok(s) = self.taffy.style(t).cloned() {
                        let mut s = s;
                        s.grid_row = Line {
                            start: line::<GridPlacement>(r),
                            end: GridPlacement::Auto,
                        };
                        s.grid_column = Line {
                            start: line::<GridPlacement>(col),
                            end: GridPlacement::Auto,
                        };
                        let _ = self.taffy.set_style(t, s);
                    }
                }
                kids.push(t);
            }
        }
        let t = self.taffy.new_with_children(style, &kids).ok()?;
        self.map.push((t, node.id));
        self.shadow(node, &get);
        Some(t)
    }

    fn shadow<V: AsRef<PropValue>>(&mut self, node: &Node, get: &impl Fn(Prop) -> Option<V>) {
        if let Some(v) = get(Prop::Shadow)
            && let PropValue::Shadow(list) = v.as_ref()
            && !list.is_empty()
        {
            let placed = keyword(get(Prop::Place).as_ref().map(AsRef::as_ref)) == Some("absolute");
            let off = |p: Prop| {
                get(p)
                    .and_then(|v| v.as_ref().as_number())
                    .and_then(finite)
                    .filter(|_| placed)
                    .unwrap_or(0.0)
            };
            self.shadows
                .push((node.id, (off(Prop::X), off(Prop::Y)), list.clone()));
        }
    }
}

/// A text leaf as measured: its node, font size, length and how it
/// fits a narrower box.
struct TextLeaf {
    node: NodeId,
    font: f32,
    chars: usize,
    word: usize,
    shrinks: bool,
    wraps: bool,
    max_lines: Option<u32>,
    /// `min_width`/`max_width` resolved against the parent: the width the
    /// height is computed for is clamped first, so a text capped by
    /// `max_width` is as tall as its wrapped lines.
    min_w: Option<f32>,
    max_w: Option<f32>,
}

/// The text size taffy is told for a text leaf.
fn measure_text(
    texts: &dyn TextSizes,
    t: &TextLeaf,
    known: taffy::Size<Option<f32>>,
    avail: taffy::Size<AvailableSpace>,
) -> taffy::Size<f32> {
    let TextLeaf {
        node,
        font,
        chars,
        word,
        shrinks,
        wraps,
        max_lines,
        ..
    } = *t;
    let natural = texts.natural(node).unwrap_or_else(|| {
        // Not shaped yet: a guess from its length, replaced when the
        // layout arrives.
        LogicalSize::new(chars as f32 * font * 0.55, (font * 1.2).ceil())
    });
    // Whole pixels up, so rounding the layout never cuts a text that fits.
    let natural = LogicalSize::new(natural.w.ceil(), natural.h.ceil());
    // Wrapping text's smallest width is its longest word (as in CSS),
    // taken as its share of the natural width: a text in a growing column
    // wraps instead of widening it.
    let least = if chars > 0 {
        (natural.w * word as f32 / chars as f32)
            .ceil()
            .min(natural.w)
    } else {
        natural.w
    };
    let w = known.width.unwrap_or_else(|| {
        let w = match avail.width {
            AvailableSpace::MinContent if shrinks => 0.0,
            AvailableSpace::MinContent if wraps => least,
            AvailableSpace::Definite(a) if shrinks => natural.w.min(a.max(0.0)),
            AvailableSpace::Definite(a) if wraps => natural.w.min(a.max(least)),
            _ => natural.w,
        };
        let w = t.max_w.map_or(w, |m| w.min(m));
        t.min_w.map_or(w, |m| w.max(m))
    });
    let h = known.height.unwrap_or_else(|| {
        if w + 1.0 < natural.w && wraps {
            texts.fitted(node, w.round()).map_or_else(
                || {
                    let lines = (natural.w / w.max(1.0)).ceil().max(1.0);
                    let lines = max_lines.map_or(lines, |m| lines.min(m as f32));
                    natural.h * lines
                },
                |s| s.h,
            )
        } else {
            natural.h
        }
    });
    taffy::Size {
        width: w,
        height: h,
    }
}

/// The content height a list's `height` and `max_height` leave for its
/// rows, resolved against its parent. Taffy ignores max sizes when it
/// asks a leaf for its content contribution, so the cap is applied in the
/// measure itself.
fn list_cap(inputs: &LayoutInput, style: &Style) -> Option<f32> {
    use taffy::util::{MaybeResolve, ResolveOrZero};
    let calc = |_: *const (), _: f32| 0.0;
    let parent = inputs.parent_size.height;
    let size: Option<f32> = style.size.height.maybe_resolve(parent, calc);
    let max: Option<f32> = style.max_size.height.maybe_resolve(parent, calc);
    let cap = match (size, max) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    let pad = style
        .padding
        .resolve_or_zero(inputs.parent_size.width, calc);
    let border = style.border.resolve_or_zero(inputs.parent_size.width, calc);
    Some((cap - pad.top - pad.bottom - border.top - border.bottom).max(0.0))
}

/// Lays out the subtree under the surface node `root`.
pub(crate) fn layout(
    tree: &SceneTree,
    root: NodeId,
    size: RootSize,
    texts: &dyn TextSizes,
    scrolls: &mut HashMap<NodeId, ScrollState>,
) -> Boxes {
    let mut out = Boxes::default();
    let Some(node) = tree.get(root) else {
        return out;
    };
    let inh = inherited_at(tree, root);
    let vertical_split = matches!(
        node.get(Prop::Edge),
        Some(PropValue::Keyword(k)) if k == "left" || k == "right"
    );
    let mut b = Build::new(tree, vertical_split);
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

/// What taffy is told for a leaf: text from its layouts, fixed-size
/// widgets, and a list's rows (capped by its `height`/`max_height`).
fn measure_leaf(
    texts: &dyn TextSizes,
    lists: &HashMap<NodeId, f32>,
    inputs: LayoutInput,
    ctx: Option<&mut Ctx>,
    style: &Style,
) -> LayoutOutput {
    let cap = matches!(ctx, Some(Ctx::List(_)))
        .then(|| list_cap(&inputs, style))
        .flatten();
    let width_of = |d: LengthPercentageAuto| -> Option<f32> {
        use taffy::util::MaybeResolve;
        let v: Option<f32> = d.maybe_resolve(inputs.parent_size.width, |_: *const (), _| 0.0);
        v.filter(|v| v.is_finite())
    };
    let max_w = || width_of(style.max_size.width);
    let min_w = || width_of(style.min_size.width);
    compute_leaf_layout(
        inputs,
        style,
        |_, _| 0.0,
        |known, avail| match ctx {
            Some(Ctx::Text {
                node,
                font,
                chars,
                word,
                shrinks,
                wraps,
                max_lines,
            }) => {
                let leaf = TextLeaf {
                    node: *node,
                    font: *font,
                    chars: *chars,
                    word: *word,
                    shrinks: *shrinks,
                    wraps: *wraps,
                    max_lines: *max_lines,
                    min_w: min_w(),
                    max_w: max_w(),
                };
                measure_text(texts, &leaf, known, avail)
            }
            Some(Ctx::Fixed(w, h)) => taffy::Size {
                width: known.width.unwrap_or(*w),
                height: known.height.unwrap_or(*h),
            },
            Some(Ctx::List(id)) => {
                let h = lists.get(id).copied().unwrap_or_default();
                // A scroll container: its rows past `height` or
                // `max_height` scroll, so that is all its parent sees of
                // them, and its smallest size is none.
                let h = match avail.height {
                    AvailableSpace::MinContent => 0.0,
                    _ => cap.map_or(h, |c| h.min(c)),
                };
                taffy::Size {
                    width: known.width.unwrap_or(0.0),
                    height: known.height.unwrap_or(h),
                }
            }
            None => taffy::Size {
                width: known.width.unwrap_or(0.0),
                height: known.height.unwrap_or(0.0),
            },
        },
    )
}

/// The rows' total height a list's measure starts from: measured row
/// heights, else the list's estimate, plus gaps.
fn rows_height(tree: &SceneTree, id: NodeId, st: &ScrollState, gap: f32) -> f32 {
    let est = st.estimate();
    let rows = tree.get(id).map_or(&[][..], |n| &n.children[..]);
    rows.iter()
        .map(|r| st.heights.get(r).copied().unwrap_or(est))
        .sum::<f32>()
        + gap * rows.len().saturating_sub(1) as f32
}

impl<'a> Build<'a> {
    fn new(tree: &'a SceneTree, vertical_split: bool) -> Self {
        Build {
            tree,
            taffy: TaffyTree::new(),
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
        let mut stack = vec![(t, origin.0, origin.1)];
        while let Some((tn, px, py)) = stack.pop() {
            let Ok(l) = self.taffy.layout(tn) else {
                continue;
            };
            let (x, y) = (px + l.location.x, py + l.location.y);
            let Some(&id) = ids.get(&tn) else { continue };
            out.rects
                .insert(id, LogicalRect::new(x, y, l.size.width, l.size.height));
            let mut dy = 0.0;
            if scroll_ids.contains(&id) {
                let st = scrolls.entry(id).or_default();
                st.viewport = l.size.height;
                // The content's extent: its lowest child, plus the bottom
                // pad.
                let bottom = self
                    .taffy
                    .children(tn)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|k| self.taffy.layout(*k).ok())
                    .map(|k| k.location.y + k.size.height)
                    .fold(0.0f32, f32::max);
                st.content = (bottom + l.padding.bottom).max(l.size.height);
                if !st.offset.is_finite() {
                    st.offset = 0.0;
                }
                st.offset = st.offset.clamp(0.0, (st.content - st.viewport).max(0.0));
                dy = st.offset;
            }
            if let Ok(kids) = self.taffy.children(tn) {
                for k in kids {
                    stack.push((k, x, y - dy));
                }
            }
        }
    }

    /// Lays out the rows in view of every list this pass met, nested
    /// lists in those rows included.
    fn place_lists(
        &mut self,
        texts: &dyn TextSizes,
        scrolls: &mut HashMap<NodeId, ScrollState>,
        out: &mut Boxes,
    ) {
        for (id, _, inh) in std::mem::take(&mut self.lists) {
            place_list(self.tree, id, &inh, texts, scrolls, out);
        }
    }
}

/// Lays out the rows of list `id` that its viewport shows.
fn place_list(
    tree: &SceneTree,
    id: NodeId,
    inh: &Inh<'_>,
    texts: &dyn TextSizes,
    scrolls: &mut HashMap<NodeId, ScrollState>,
    out: &mut Boxes,
) {
    let (Some(node), Some(frame)) = (tree.get(id), out.rects.get(&id).copied()) else {
        return;
    };
    let mut inh = inh.clone();
    inherit(node, &mut inh);
    let scope = TokenScope::new(&inh.tokens);
    let get = |p: Prop| node.get(p).and_then(|v| scope.resolve(v));
    let pad = insets(get(Prop::Pad).as_deref()).unwrap_or_default();
    let gap = num(get(Prop::Gap).as_deref()).unwrap_or(0.0).max(0.0);
    let content = LogicalRect::new(
        frame.x + pad.left,
        frame.y + pad.top,
        (frame.w - pad.left - pad.right).max(0.0),
        (frame.h - pad.top - pad.bottom).max(0.0),
    );
    let st = scrolls.entry(id).or_default();
    let est = st.estimate();
    let rows = &node.children;
    out.rows_total += rows.len();
    let total = rows_height(tree, id, st, gap);
    st.viewport = content.h;
    st.content = total;
    if !st.offset.is_finite() {
        st.offset = 0.0;
    }
    st.offset = st.offset.clamp(0.0, (total - content.h).max(0.0));
    let offset = st.offset;
    let mut y = 0.0;
    let mut i = 0;
    // Skip rows above the viewport by their known or estimated heights.
    while i < rows.len() {
        let h = st.heights.get(&rows[i]).copied().unwrap_or(est);
        if y + h > offset {
            break;
        }
        y += h + gap;
        i += 1;
    }
    let mut changed = false;
    while i < rows.len() && y < offset + content.h {
        let Some(row) = tree.get(rows[i]) else {
            i += 1;
            continue;
        };
        let mut b = Build::new(tree, false);
        let Some(t) = b.node(row, &inh, false) else {
            i += 1;
            continue;
        };
        // Rows stretch across the list, as in a column.
        if let Ok(mut s) = b.taffy.style(t).cloned()
            && s.size.width.is_auto()
        {
            s.size.width = length(content.w);
            let _ = b.taffy.set_style(t, s);
        }
        b.compute(
            t,
            taffy::Size {
                width: AvailableSpace::Definite(content.w),
                height: AvailableSpace::MaxContent,
            },
            texts,
            scrolls,
        );
        let h = b.taffy.layout(t).map_or(0.0, |l| l.size.height);
        let st = scrolls.entry(id).or_default();
        // Off what this pass assumed for it: the rows below are misplaced.
        let assumed = st.heights.insert(rows[i], h).unwrap_or(est);
        if (h - assumed).abs() > 0.5 {
            changed = true;
        }
        b.read_back(t, (content.x, content.y + y - offset), scrolls, out);
        // A list or scroll inside the row: its own rows in view.
        b.place_lists(texts, scrolls, out);
        out.rows_laid_out += 1;
        y += h + gap;
        i += 1;
    }
    // Forget heights of rows that are gone.
    let st = scrolls.entry(id).or_default();
    if st.heights.len() > rows.len() {
        let live: HashSet<NodeId> = rows.iter().copied().collect();
        st.heights.retain(|k, _| live.contains(k));
    }
    // Scrolling goes by what was just measured.
    st.content = rows_height(tree, id, st, gap);
    if changed {
        out.unsettled = true;
    }
}

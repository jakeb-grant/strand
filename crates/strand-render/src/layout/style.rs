//! A node's taffy style and children, from its resolved props.

use strand_scene::{Insets, NodeKind, Prop, PropValue, TokenScope};
use taffy::Overflow;
use taffy::prelude::{
    AlignContent, AlignItems, Display, FlexDirection, GridPlacement, LengthPercentage,
    LengthPercentageAuto, Line, Position, Style, auto, fr, length, line, minmax,
};

use super::{
    Build, CellOf, Ctx, Inh, Len, align_items, dim, finite, inherit, insets, is_leaf, justify,
    keyword, num, out_of_flow,
};
use crate::tree::Node;

impl<'a> Build<'a> {
    pub(super) fn node(
        &mut self,
        node: &'a Node,
        inh: &Inh<'a>,
        root: bool,
    ) -> Option<taffy::NodeId> {
        if !root && out_of_flow(node.kind) {
            return None;
        }
        let fold = self.fold.take();
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
        // A size in flight: exactly that, however small its content (a
        // toast collapsing to `height: 0` clips what it holds).
        let forced = self.sizes.get(&node.id).copied().unwrap_or_default();
        if let Some(d) = dim(get(Prop::MinHeight).as_deref(), font) {
            style.min_size.height = d.lpa();
        }
        if let Some(d) = dim(get(Prop::MaxWidth).as_deref(), font) {
            style.max_size.width = d.lpa();
        }
        if let Some(d) = dim(get(Prop::MaxHeight).as_deref(), font) {
            style.max_size.height = d.lpa();
        }
        if let Some(w) = forced.size[0] {
            style.size.width = length(w);
            style.min_size.width = LengthPercentageAuto::length(0.0);
            style.max_size.width = LengthPercentageAuto::auto();
        }
        if let Some(h) = forced.size[1] {
            style.size.height = length(h);
            style.min_size.height = LengthPercentageAuto::length(0.0);
            style.max_size.height = LengthPercentageAuto::auto();
        }
        let mut margin = insets(get(Prop::Margin).as_deref());
        if let Some((axis, [before, after])) = fold {
            let m = margin.get_or_insert_with(Insets::default);
            if axis == 0 {
                m.left += before;
                m.right += after;
            } else {
                m.top += before;
                m.bottom += after;
            }
        }
        if let Some(m) = margin {
            style.margin = taffy::Rect {
                left: LengthPercentageAuto::length(m.left),
                right: LengthPercentageAuto::length(m.right),
                top: LengthPercentageAuto::length(m.top),
                bottom: LengthPercentageAuto::length(m.bottom),
            };
        }
        // A button pads its label unless it says otherwise.
        let pad = insets(get(Prop::Pad).as_deref())
            .or((kind == NodeKind::Button).then_some(crate::widgets::BUTTON_PAD));
        if let Some(mut p) = pad {
            // A forced size smaller than the padding: the padding gives
            // way (taffy never makes a box smaller than its padding), so
            // a collapsing toast reaches zero.
            let fit = |v: f32, a: &mut f32, b: &mut f32| {
                let (x, y) = (a.max(0.0), b.max(0.0));
                if x + y > v {
                    let k = v.max(0.0) / (x + y);
                    (*a, *b) = (x * k, y * k);
                }
            };
            if let Some(w) = forced.size[0] {
                fit(w, &mut p.left, &mut p.right);
            }
            if let Some(h) = forced.size[1] {
                fit(h, &mut p.top, &mut p.bottom);
            }
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
        } else if matches!(kind, NodeKind::Image | NodeKind::Icon) {
            // An image or icon sized in absolute lengths keeps its size in
            // a full row, as a CSS replaced element's automatic minimum
            // keeps it (the launcher's 32 px icon beside a long cut
            // comment); an explicit `shrink:` lets it give way. A
            // percentage size is relative to the row, so it gives way as
            // any box does.
            let w = dim(get(Prop::Width).as_deref(), font).or(size);
            let h = dim(get(Prop::Height).as_deref(), font).or(size);
            let px = |l: Option<Len>| matches!(l, Some(Len::Px(_)));
            let pct = |l: Option<Len>| matches!(l, Some(Len::Pct(_)));
            if (px(w) || px(h)) && !pct(w) && !pct(h) {
                style.flex_shrink = 0.0;
            }
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
                    let empty = kind == NodeKind::Text
                        && match get(Prop::Text).as_deref() {
                            Some(PropValue::Text(t)) => t.is_empty(),
                            _ => true,
                        };
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
                        empty,
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
                // Not sized by its source (unknown until decoded, and a
                // layout that waited on decodes would jump): 16 × 16 like
                // an icon, or square to the side given. A fully sized one
                // keeps no content size (it may shrink as a box does).
                NodeKind::Image
                    if get(Prop::Size).is_none()
                        && (get(Prop::Width).is_none() || get(Prop::Height).is_none()) =>
                {
                    Some(Ctx::Square(16.0))
                }
                NodeKind::Slider => Some(Ctx::Fixed(
                    crate::widgets::SLIDER_WIDTH,
                    crate::widgets::SLIDER_KNOB + 4.0,
                )),
                NodeKind::Meter => Some(Ctx::Fixed(0.0, 4.0)),
                NodeKind::Input => Some(Ctx::Fixed(0.0, (font * 1.25).ceil())),
                NodeKind::Segmented => Some(Ctx::Segmented {
                    node: node.id,
                    n: crate::widgets::options(get(Prop::Options).as_deref()).len(),
                    font,
                }),
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
            .filter(|c| !out_of_flow(c.kind))
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
        // The flex main axis and gap a collapsing child folds.
        let main = match style.display {
            Display::Flex if style.flex_direction == FlexDirection::Row => Some(0),
            Display::Flex => Some(1),
            _ => None,
        };
        let gap = num(get(Prop::Gap).as_deref()).unwrap_or(0.0).max(0.0);
        let count = children.len();
        // A slot collapsing to zero takes the gap beside it along: its
        // share of the gap is never more than its own size. The
        // neighbour across that gap gives it up (a negative margin on the
        // collapsing node itself would not shrink an auto-sized parent:
        // an outer size never goes below zero).
        let mut folds = vec![[0.0f32; 2]; count];
        if let Some(axis) = main.filter(|_| gap > 0.0 && count > 1) {
            for (i, (_, c)) in children.iter().enumerate() {
                let Some(f) = self.sizes.get(&c.id) else {
                    continue;
                };
                let Some(v) = f.size[axis].filter(|v| f.collapse[axis] && gap > *v) else {
                    continue;
                };
                if i + 1 < count {
                    folds[i + 1][0] -= gap - v;
                } else {
                    folds[i - 1][1] -= gap - v;
                }
            }
        }
        for (i, (_, c)) in children.into_iter().enumerate() {
            self.fold = main
                .filter(|_| folds[i] != [0.0, 0.0])
                .map(|axis| (axis, folds[i]));
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

    pub(super) fn shadow<V: AsRef<PropValue>>(
        &mut self,
        node: &Node,
        get: &impl Fn(Prop) -> Option<V>,
    ) {
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

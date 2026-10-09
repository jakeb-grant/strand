//! Virtualised lists and scroll offsets: rows measured or estimated,
//! and only the rows in view laid out.

use std::collections::{HashMap, HashSet};

use strand_scene::{LogicalRect, NodeId, Prop, TokenScope};
use taffy::LayoutInput;
use taffy::prelude::{AvailableSpace, Style, length};

use super::text::TextSizes;
use super::{Boxes, Build, Inh, LIST_ROW_ESTIMATE, inherit, insets, num};
use crate::anim::SizeMap;
use crate::tree::SceneTree;

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

/// The content height a list's `height` and `max_height` leave for its
/// rows, resolved against its parent. Taffy ignores max sizes when it
/// asks a leaf for its content contribution, so the cap is applied in the
/// measure itself.
pub(super) fn list_cap(inputs: &LayoutInput, style: &Style) -> Option<f32> {
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

/// The rows' total height a list's measure starts from: measured row
/// heights, else the list's estimate, plus gaps.
pub(super) fn rows_height(tree: &SceneTree, id: NodeId, st: &ScrollState, gap: f32) -> f32 {
    let est = st.estimate();
    let rows = tree.get(id).map_or(&[][..], |n| &n.children[..]);
    rows.iter()
        .map(|r| st.heights.get(r).copied().unwrap_or(est))
        .sum::<f32>()
        + gap * rows.len().saturating_sub(1) as f32
}

impl<'a> Build<'a> {
    /// Lays out the rows in view of every list this pass met, nested
    /// lists in those rows included.
    pub(super) fn place_lists(
        &mut self,
        texts: &dyn TextSizes,
        scrolls: &mut HashMap<NodeId, ScrollState>,
        out: &mut Boxes,
    ) {
        for (id, _, inh) in std::mem::take(&mut self.lists) {
            place_list(self.tree, id, &inh, texts, scrolls, out, self.sizes);
        }
    }
}

/// Lays out the rows of list `id` that its viewport shows.
pub(super) fn place_list(
    tree: &SceneTree,
    id: NodeId,
    inh: &Inh<'_>,
    texts: &dyn TextSizes,
    scrolls: &mut HashMap<NodeId, ScrollState>,
    out: &mut Boxes,
    sizes: &SizeMap,
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
        let mut b = Build::new(tree, false, sizes);
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
        let h = b.taffy.layout(t).map_or(0.0, |l| l.size.height.round());
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
        // A collapsing row takes its gap along (as in a column).
        let g = match sizes.get(&rows[i]) {
            Some(f) if f.collapse[1] && f.size[1].is_some() => gap.min(h),
            _ => gap,
        };
        y += h + g;
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

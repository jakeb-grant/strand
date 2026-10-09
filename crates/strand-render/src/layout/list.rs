//! Virtualised lists and scroll offsets (design.md, "Layout, animation
//! and input"; M4: smooth 2,000-row scrolling).
//!
//! A `list` is one taffy leaf sized from its rows' heights. Its rows are
//! placed at their global indexes: on a list whose `for` logic windows
//! (`row_count`, `row_first`), the rows that are not mounted keep their
//! extent at the estimated row height, so the mounted ones sit where
//! they would in the whole list. Only the rows in view, plus
//! [`LAYOUT_OVERSCAN`], are laid out, each as a taffy root of its own.
//!
//! Scrolling moves a paint offset: the boxes of everything under the
//! `scroll` or `list` are translated (`Renderer`'s list step), and a
//! list lays out again only the rows that come into its overscan, once
//! each. A list's window (the rows logic is asked to mount) runs
//! [`WINDOW_OVERSCAN`] viewports ahead of what is shown; what is painted
//! never leaves the mounted rows ([`ScrollState::shown`]), so no frame
//! shows a gap where rows are still on their way.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

use strand_scene::{Curve, LogicalRect, Motion, NodeId, NodeKind, Prop, PropValue, TokenScope};
use taffy::LayoutInput;
use taffy::prelude::{AvailableSpace, Style, length};

use super::text::TextSizes;
use super::{Boxes, Build, Inh, LIST_ROW_ESTIMATE, inherit, inherited_at, insets, num};
use crate::anim::SizeMap;
use crate::tree::SceneTree;

/// How far past its viewport a list lays rows out, as a share of the
/// viewport (at least a row, at most [`MAX_LAYOUT_OVERSCAN`]): a scroll
/// of up to half this much translates boxes and lays nothing out.
pub const LAYOUT_OVERSCAN: f32 = 0.25;

/// The most a list lays out past either end of its viewport, logical
/// pixels.
pub const MAX_LAYOUT_OVERSCAN: f32 = 128.0;

/// How far past either end of a `view` tall viewport a list lays rows
/// out.
pub fn layout_margin(view: f32) -> f32 {
    (view * LAYOUT_OVERSCAN).clamp(LIST_ROW_ESTIMATE, MAX_LAYOUT_OVERSCAN)
}

/// How far past its viewport, in viewports, a list asks logic to mount
/// rows ([`ScrollState::window`]): about 25 frames of a fast fling at
/// 60 Hz.
pub const WINDOW_OVERSCAN: f32 = 1.5;

/// How far past its viewport, in viewports, the mounted rows must reach
/// before a list asks for a new window: less than [`WINDOW_OVERSCAN`],
/// so a scroll asks again only every half viewport.
pub const WINDOW_NEED: f32 = 1.0;

/// How a touchpad fling decays: its velocity falls by a factor of `e`
/// every this many seconds.
pub const FLING_DECAY: f32 = 0.35;

/// A fling slower than this, logical pixels per second, stops.
pub const FLING_STOP: f32 = 20.0;

/// Touchpad scroll samples older than this (milliseconds, on the
/// compositor's clock) do not count towards a fling's velocity.
pub const FLING_SAMPLES_MS: u32 = 100;

/// The rows a virtualised list has, and which of them are mounted and
/// wanted, as of its last layout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListWindow {
    /// Every row (`row_count`).
    pub count: u32,
    /// The global index of the first mounted row (`row_first`).
    pub first: u32,
    /// How many rows are mounted.
    pub mounted: u32,
    /// The rows to ask for: the view and [`WINDOW_OVERSCAN`].
    pub want: Range<u32>,
    /// The rows that must be mounted: the view and [`WINDOW_NEED`].
    pub need: Range<u32>,
}

impl ListWindow {
    /// The mounted rows fall short of what is needed, or hold far more
    /// than is wanted.
    pub fn stale(&self) -> bool {
        let mounted = self.first..self.first + self.mounted;
        let covers = self.need.is_empty()
            || (mounted.start <= self.need.start && self.need.end <= mounted.end);
        let wanted = self.want.end - self.want.start;
        !covers || self.mounted > 3 * wanted + 16
    }
}

/// How the offset moves by itself.
#[derive(Clone, Debug, PartialEq)]
enum ScrollMotion {
    /// Wheel steps: the offset springs to the target (`$motion.spatial`).
    Spring(Motion<1>, Curve),
    /// A touchpad lifted while moving: the velocity decays from `v`
    /// (logical pixels per second, at `from`) from the first frame
    /// after the lift.
    Fling {
        v: f32,
        from: f32,
        start: Option<Duration>,
        /// The last frame before the lift.
        last: Option<Duration>,
    },
}

/// Scroll position and row heights of a `scroll` or `list`, kept across
/// layout passes.
#[derive(Clone, Debug, Default)]
pub struct ScrollState {
    /// Where the content is scrolled to, logical pixels from the top.
    pub offset: f32,
    /// Measured row heights of a list, by row node.
    pub heights: HashMap<NodeId, f32>,
    /// The visible height and the content height of the last pass.
    pub viewport: f32,
    pub content: f32,
    /// What is shown: `offset` kept within the mounted rows of a
    /// windowed list (equal to it otherwise).
    pub shown: f32,
    /// A virtualised list's rows (`None` for a `scroll`, or a list logic
    /// mounts in full).
    pub window: Option<ListWindow>,
    motion: Option<ScrollMotion>,
    /// Recent touchpad deltas: compositor time (ms) and pixels.
    samples: Vec<(u32, f32)>,
    /// A row in view and where it was: when rows above it change their
    /// extent (measured where they were estimated, unmounted where they
    /// were measured), the offset follows it, so nothing in view jumps.
    anchor: Option<(NodeId, f32)>,
}

impl ScrollState {
    /// The largest offset.
    fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    /// Scrolls by `dy` logical pixels, kept within the content. Returns
    /// true if the offset moved. A motion in flight stops.
    pub fn scroll_by(&mut self, dy: f32) -> bool {
        if !dy.is_finite() {
            return false;
        }
        let new = (self.offset + dy).clamp(0.0, self.max_offset());
        let moved = new != self.offset;
        self.offset = new;
        self.motion = None;
        moved
    }

    /// Where the offset is heading: a wheel spring's target, else where
    /// it is.
    pub fn target(&self) -> f32 {
        match &self.motion {
            Some(ScrollMotion::Spring(m, _)) => m.target()[0],
            _ => self.offset,
        }
    }

    /// A wheel step of `dy`: the target moves by it (kept within the
    /// content) and the offset springs there along `curve` (it lands at
    /// once with [`Curve::Instant`]). Returns true if the target moved.
    pub fn wheel(&mut self, dy: f32, curve: Curve, last: Option<Duration>) -> bool {
        if !dy.is_finite() {
            return false;
        }
        let from = self.target();
        let to = (from + dy).clamp(0.0, self.max_offset());
        if to == from {
            return false;
        }
        if curve == Curve::Instant {
            self.offset = to;
            self.motion = None;
            return true;
        }
        match &mut self.motion {
            Some(ScrollMotion::Spring(m, c)) => {
                m.retarget([to], curve);
                *c = curve;
            }
            _ => {
                // Started a frame before the next one (as any spring), so
                // that frame already shows it moving.
                let mut m = Motion::rest([self.offset], 0.25).sampled_at(last);
                m.retarget([to], curve);
                self.motion = Some(ScrollMotion::Spring(m, curve));
            }
        }
        true
    }

    /// A touchpad (finger or continuous) moved by `dy` at compositor
    /// time `time` (ms): the offset follows at once. Returns true if it
    /// moved.
    pub fn touch(&mut self, dy: f32, time: u32) -> bool {
        self.samples
            .retain(|(t, _)| time.wrapping_sub(*t) <= FLING_SAMPLES_MS);
        self.samples.push((time, dy));
        self.scroll_by(dy)
    }

    /// The fingers lifted at compositor time `time`: a fling at the
    /// recent speed starts, unless they had stopped. Returns true if
    /// one did.
    pub fn lift(&mut self, time: u32, frame: Option<Duration>) -> bool {
        let recent: Vec<(u32, f32)> = self
            .samples
            .drain(..)
            .filter(|(t, _)| time.wrapping_sub(*t) <= FLING_SAMPLES_MS)
            .collect();
        let (Some(first), Some(last)) = (recent.first(), recent.last()) else {
            return false;
        };
        // Each sample is the motion up to its time, so the first one's
        // happened before the span the samples cover (from the first to
        // the lift, a frame at least); a lone sample spans that frame.
        let ms = time.wrapping_sub(first.0).max(last.0.wrapping_sub(first.0));
        let span = (ms as f32 / 1000.0).max(0.008);
        let skip = usize::from(recent.len() > 1);
        let v = recent[skip..].iter().map(|(_, d)| d).sum::<f32>() / span;
        if !v.is_finite() || v.abs() < FLING_STOP {
            return false;
        }
        self.motion = Some(ScrollMotion::Fling {
            v,
            from: self.offset,
            start: None,
            last: frame,
        });
        true
    }

    /// Stops any motion where it is.
    pub fn stop(&mut self) {
        self.motion = None;
    }

    /// True while the offset moves by itself.
    pub fn moving(&self) -> bool {
        self.motion.is_some()
    }

    /// Advances the motion to frame time `at` (`snap`: no clock, or
    /// `reduced_motion`: it lands at once). Returns true if the offset
    /// changed.
    pub fn advance(&mut self, at: Duration, snap: bool) -> bool {
        let before = self.offset;
        let max = self.max_offset();
        match &mut self.motion {
            None => {}
            Some(ScrollMotion::Spring(m, _)) => {
                if snap {
                    self.offset = m.target()[0];
                    self.motion = None;
                } else {
                    self.offset = m.sample(at)[0];
                    if m.is_settled(at) {
                        self.motion = None;
                    }
                }
            }
            Some(ScrollMotion::Fling {
                v,
                from,
                start,
                last,
            }) => {
                // It starts a frame before the first one that shows it.
                let t0 = *start.get_or_insert_with(|| {
                    let lead = last.map_or(Duration::ZERO, |l| at.saturating_sub(l));
                    at.saturating_sub(lead.min(strand_scene::motion::START_LEAD))
                });
                let t = at.saturating_sub(t0).as_secs_f32();
                let k = (-t / FLING_DECAY).exp();
                let travel = *v * FLING_DECAY;
                self.offset = if snap {
                    *from + travel
                } else {
                    *from + travel * (1.0 - k)
                };
                if snap || (*v * k).abs() < FLING_STOP {
                    self.motion = None;
                }
            }
        }
        if !self.offset.is_finite() {
            self.offset = before;
            self.motion = None;
        }
        // A fling stops at either end.
        if self.offset <= 0.0 || self.offset >= max {
            self.offset = self.offset.clamp(0.0, max);
            if matches!(self.motion, Some(ScrollMotion::Fling { .. })) {
                self.motion = None;
            }
        }
        self.offset != before
    }

    /// Moves the view by `d` logical pixels with the content (the rows
    /// above it changed their extent): the offset, its target and
    /// `shown`.
    fn shift(&mut self, d: f32) {
        self.offset += d;
        self.shown += d;
        match &mut self.motion {
            Some(ScrollMotion::Spring(m, c)) => {
                let to = m.target()[0] + d;
                m.shift([d], *c);
                m.retarget([to], *c);
            }
            Some(ScrollMotion::Fling { from, .. }) => *from += d,
            None => {}
        }
    }

    fn estimate(&self) -> f32 {
        if self.heights.is_empty() {
            LIST_ROW_ESTIMATE
        } else {
            self.heights.values().sum::<f32>() / self.heights.len() as f32
        }
    }
}

/// What a layout pass left for each `scroll` and `list` it placed: the
/// offset its boxes show, and the span of content (logical pixels from
/// the content's top) whose rows are laid out.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ListBox {
    pub applied: f32,
    /// `None` for a `scroll`, whose content is laid out in full.
    pub laid: Option<(f32, f32)>,
    /// The content box (inside its padding), as laid out.
    pub content: LogicalRect,
    /// The offsets it can show: within its content, and for a windowed
    /// list within its mounted rows.
    pub bounds: (f32, f32),
}

/// The rows of a list in the scene: `(row_count, row_first)` for a list
/// logic windows, else every child from 0.
fn window_of(node: &crate::tree::Node) -> (u32, u32) {
    let n = node.children.len() as u32;
    let count = match node.get(Prop::RowCount) {
        Some(PropValue::Number(c)) if c.is_finite() && *c >= 0.0 => *c as u32,
        _ => return (n, 0),
    };
    let first = match node.get(Prop::RowFirst) {
        Some(PropValue::Number(f)) if f.is_finite() && *f >= 0.0 => *f as u32,
        _ => 0,
    };
    let first = first.min(count);
    (count.max(first + n), first)
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
/// heights, else the list's estimate, plus gaps, with the rows logic has
/// not mounted at the estimate.
pub(super) fn rows_height(tree: &SceneTree, id: NodeId, st: &ScrollState, gap: f32) -> f32 {
    let est = st.estimate();
    let Some(node) = tree.get(id) else {
        return 0.0;
    };
    let (count, _) = window_of(node);
    let rows = &node.children;
    let unmounted = count.saturating_sub(rows.len() as u32) as f32;
    let mounted: f32 = rows
        .iter()
        .map(|r| st.heights.get(r).copied().unwrap_or(est))
        .sum();
    (mounted + unmounted * est + gap * count.saturating_sub(1) as f32).max(0.0)
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
            place_list(self.tree, id, &inh, texts, scrolls, out, self.sizes, None);
        }
    }
}

/// Where a list's rows are: each mounted row's top and height, in
/// content pixels, and the content height, with unmounted rows at the
/// estimate.
struct Placement {
    ys: Vec<f32>,
    hs: Vec<f32>,
    /// The mounted rows' span.
    top: f32,
    bottom: f32,
    total: f32,
    /// One unmounted row's extent (its estimate and the gap).
    pitch: f32,
    count: u32,
    first: u32,
}

impl Placement {
    /// The global index of the row at content height `y`.
    fn index_at(&self, y: f32) -> u32 {
        let n = self.ys.len() as u32;
        let i = if y < self.top || n == 0 {
            (y.max(0.0) / self.pitch) as u32
        } else if y >= self.bottom {
            self.first + n + ((y - self.bottom) / self.pitch) as u32
        } else {
            let j = self.ys.partition_point(|t| *t <= y).saturating_sub(1);
            self.first + j as u32
        };
        i.min(self.count)
    }

    /// The rows from content height `a` to `b`.
    fn rows(&self, a: f32, b: f32) -> Range<u32> {
        if self.count == 0 {
            return 0..0;
        }
        let lo = self.index_at(a).min(self.count - 1);
        let hi = (self.index_at(b) + 1).min(self.count);
        lo..hi.max(lo)
    }
}

/// Lays out the rows of list `id` that its viewport and overscan show,
/// at their global indexes. With `reuse` (a scroll that only moved the
/// view: nothing layout reads changed), rows laid out there are moved,
/// not laid out again.
#[allow(clippy::too_many_arguments)]
pub(super) fn place_list(
    tree: &SceneTree,
    id: NodeId,
    inh: &Inh<'_>,
    texts: &dyn TextSizes,
    scrolls: &mut HashMap<NodeId, ScrollState>,
    out: &mut Boxes,
    sizes: &SizeMap,
    reuse: Option<&Boxes>,
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
    let rows = &node.children;
    let windowed = node.get(Prop::RowCount).is_some();
    let (count, first) = window_of(node);
    let place = |st: &ScrollState| {
        let est = st.estimate();
        let pitch = (est + gap).max(1.0);
        let top = first as f32 * pitch;
        let mut ys = Vec::with_capacity(rows.len());
        let mut hs = Vec::with_capacity(rows.len());
        let mut y = top;
        for r in rows {
            let h = st.heights.get(r).copied().unwrap_or(est);
            ys.push(y);
            hs.push(h);
            // A collapsing row takes its gap along (as in a column).
            let g = match sizes.get(r) {
                Some(f) if f.collapse[1] && f.size[1].is_some() => gap.min(h),
                _ => gap,
            };
            y += h + g;
        }
        let after = count.saturating_sub(first + rows.len() as u32) as f32;
        let total = if count == 0 {
            0.0
        } else {
            (y + after * pitch - gap).max(0.0)
        };
        Placement {
            ys,
            hs,
            top,
            bottom: y,
            total,
            pitch,
            count,
            first,
        }
    };
    let st = scrolls.entry(id).or_default();
    let p = place(st);
    // Rows above the one in view changed their extent: the view follows
    // it.
    if let Some((a, was)) = st.anchor
        && let Some(j) = rows.iter().position(|r| *r == a)
    {
        let d = p.ys[j] - was;
        if d.abs() > 0.01 {
            st.shift(d);
        }
    }
    st.viewport = content.h;
    st.content = p.total;
    if !st.offset.is_finite() {
        st.offset = 0.0;
    }
    st.offset = st.offset.clamp(0.0, st.max_offset());
    let bounds = shown_bounds(st, &p, windowed);
    st.shown = st.offset.clamp(bounds.0, bounds.1);
    let shown = st.shown;
    let view = content.h;
    let margin = layout_margin(view);
    let (lo, hi) = (shown - margin, shown + view + margin);
    out.rows_total += count as usize;
    let mut changed = false;
    let mut laid = (f32::MAX, f32::MIN);
    for (j, rid) in rows.iter().enumerate() {
        let (y, h0) = (p.ys[j], p.hs[j]);
        if y + h0 <= lo || y >= hi {
            continue;
        }
        let Some(row) = tree.get(*rid) else {
            continue;
        };
        let at = (content.x, content.y + y - shown);
        // Laid out before and nothing it reads changed: moved there.
        if let Some(old) = reuse
            && let Some(r) = old.rects.get(&row.id)
            && (r.w - content.w).abs() < 0.01
        {
            let (dx, dy) = (at.0 - r.x, at.1 - r.y);
            move_subtree(tree, row.id, old, out, dx, dy);
            laid = (laid.0.min(y), laid.1.max(y + r.h));
            continue;
        }
        let mut b = Build::new(tree, false, sizes);
        let Some(t) = b.node(row, &inh, false) else {
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
        let assumed = st.heights.insert(*rid, h).unwrap_or(h0);
        if (h - assumed).abs() > 0.5 {
            changed = true;
        }
        b.read_back(t, at, scrolls, out);
        // A list or scroll inside the row: its own rows in view.
        b.place_lists(texts, scrolls, out);
        out.rows_laid_out += 1;
        laid = (laid.0.min(y), laid.1.max(y + h));
    }
    let st = scrolls.entry(id).or_default();
    // Forget heights of rows that are gone.
    if st.heights.len() > rows.len() {
        let live: HashSet<NodeId> = rows.iter().copied().collect();
        st.heights.retain(|k, _| live.contains(k));
    }
    // Scrolling goes by what was just measured.
    let p = place(st);
    st.content = p.total;
    st.anchor = (0..rows.len())
        .find(|j| p.ys[*j] + p.hs[*j] > shown)
        .map(|j| (rows[j], p.ys[j]));
    st.window = windowed.then(|| {
        let (a, b) = (st.target().min(st.offset), st.target().max(st.offset));
        ListWindow {
            count,
            first,
            mounted: rows.len() as u32,
            want: p.rows(
                a - view * WINDOW_OVERSCAN,
                b + view * (1.0 + WINDOW_OVERSCAN),
            ),
            need: p.rows(a - view * WINDOW_NEED, b + view * (1.0 + WINDOW_NEED)),
        }
    });
    out.lists.insert(
        id,
        ListBox {
            applied: shown,
            laid: Some(if laid.0 <= laid.1 {
                laid
            } else {
                (shown, shown)
            }),
            content,
            bounds,
        },
    );
    if changed {
        out.unsettled = true;
    }
}

/// The offsets a list can show: its content, and for a windowed list
/// only its mounted rows unless they reach the list's end on that side
/// (a list whose rows are still on their way shows the edge of the ones
/// it has, never a gap).
fn shown_bounds(st: &ScrollState, p: &Placement, windowed: bool) -> (f32, f32) {
    let max = st.max_offset();
    if !windowed || p.ys.is_empty() {
        return (0.0, max);
    }
    let lo = if p.first > 0 { p.top.min(max) } else { 0.0 };
    let all = p.first + p.ys.len() as u32 >= p.count;
    let hi = if all {
        max
    } else {
        (p.bottom - st.viewport).clamp(lo, max.max(lo))
    };
    (lo, hi.max(lo))
}

/// Copies the boxes of `id` and everything under it from `old` into
/// `out`, moved by `(dx, dy)`.
fn move_subtree(tree: &SceneTree, id: NodeId, old: &Boxes, out: &mut Boxes, dx: f32, dy: f32) {
    let mut stack = vec![id];
    while let Some(n) = stack.pop() {
        if let Some(r) = old.rects.get(&n) {
            out.rects
                .insert(n, LogicalRect::new(r.x + dx, r.y + dy, r.w, r.h));
        }
        if let Some(l) = old.lists.get(&n) {
            let mut l = *l;
            l.content =
                LogicalRect::new(l.content.x + dx, l.content.y + dy, l.content.w, l.content.h);
            out.lists.insert(n, l);
        }
        if let Some(node) = tree.get(n) {
            stack.extend(node.children.iter().copied());
        }
    }
}

/// Moves the boxes of everything under `id` (not `id` itself) by `dy`.
pub(crate) fn translate_under(tree: &SceneTree, id: NodeId, boxes: &mut Boxes, dy: f32) {
    let Some(node) = tree.get(id) else {
        return;
    };
    let mut stack: Vec<NodeId> = node.children.clone();
    while let Some(n) = stack.pop() {
        if let Some(r) = boxes.rects.get_mut(&n) {
            r.y += dy;
        }
        if let Some(l) = boxes.lists.get_mut(&n) {
            l.content.y += dy;
        }
        if let Some(node) = tree.get(n) {
            stack.extend(node.children.iter().copied());
        }
    }
}

/// Lays list `id` out again in `boxes` for its new offset, moving the
/// rows laid out there before and laying out only the ones that came
/// into its overscan (nothing layout reads changed). Returns the rows it
/// laid out.
pub(crate) fn relayout_list(
    tree: &SceneTree,
    id: NodeId,
    texts: &dyn TextSizes,
    scrolls: &mut HashMap<NodeId, ScrollState>,
    boxes: &mut Boxes,
    sizes: &SizeMap,
) -> usize {
    if !boxes.rects.contains_key(&id) {
        return 0;
    }
    let old = boxes.clone();
    if let Some(node) = tree.get(id) {
        let mut stack: Vec<NodeId> = node.children.clone();
        while let Some(n) = stack.pop() {
            boxes.rects.remove(&n);
            boxes.lists.remove(&n);
            if let Some(node) = tree.get(n) {
                stack.extend(node.children.iter().copied());
            }
        }
    }
    let before = boxes.rows_laid_out;
    // Its rows are counted again below.
    let (count, _) = tree.get(id).map_or((0, 0), window_of);
    boxes.rows_total = boxes.rows_total.saturating_sub(count as usize);
    let inh = inherited_at(tree, id);
    place_list(tree, id, &inh, texts, scrolls, boxes, sizes, Some(&old));
    boxes.rows_laid_out - before
}

/// True if `id` is a `scroll` or `list`.
pub(crate) fn scrolls_content(tree: &SceneTree, id: NodeId) -> bool {
    tree.get(id)
        .is_some_and(|n| matches!(n.kind, NodeKind::Scroll | NodeKind::List))
}

//! Scrolling `scroll` and `list` nodes (M4: smooth 2,000-row scrolling):
//! wheel steps spring, touchpad flings decay, and every frame moves the
//! boxes under a scrolled node by a paint offset, laying out only the
//! list rows that come into view. A virtualised list asks logic for the
//! rows it wants mounted ([`Renderer::take_list_windows`]), and every
//! painted frame is checked for a gap in a list's view
//! ([`Renderer::list_frames`]).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Duration;

use strand_scene::{
    Curve, LogicalPoint, NodeId, NodeKind, Prop, PropValue, SurfaceId, TokenScope, Transition,
};

use super::Renderer;
use super::layout_pass::TextInfo;
use crate::layout::{layout_margin, relayout_list, scrolls_content, translate_under};

/// What moved a scroll ([`Renderer::scroll_input`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScrollKind {
    /// A wheel step (detents, or a wheel's high-resolution pixels): the
    /// offset springs to its new target.
    Wheel,
    /// A touchpad or other continuous source: the offset follows at once.
    Touch,
    /// The fingers lifted (`axis_stop`): a fling may start.
    Lift,
}

/// One scroll on a surface: `dy` logical pixels (positive scrolls down)
/// and the compositor's time of the event, milliseconds.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollInput {
    pub dy: f32,
    pub kind: ScrollKind,
    pub time: u32,
}

/// Frames painted with a list in view, and those whose view showed
/// content with no row there (rows still being mounted or laid out).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFrames {
    pub frames: u64,
    pub gaps: u64,
}

/// The renderer's list state besides each node's [`ScrollState`].
///
/// [`ScrollState`]: crate::layout::ScrollState
#[derive(Debug, Default)]
pub(crate) struct Lists {
    /// The window last asked for each list, with the mounted rows it was
    /// asked against (`first`, `count`, `mounted`): asked again only
    /// once logic has answered with something else.
    asked: HashMap<NodeId, (Range<u32>, u32, u32, u32)>,
    /// The node a touchpad scrolls on each surface, until it lifts.
    touching: HashMap<SurfaceId, NodeId>,
    stats: ListFrames,
    /// Rows laid out by scroll steps (tests: each row once).
    scroll_rows: usize,
    /// Nodes the diff being applied mounted for a list's window (rows
    /// with `window: true` and everything created under them): they
    /// show at rest, so neither their creation nor their props animate.
    pub(super) still: HashSet<NodeId>,
}

/// The global index of a list's first mounted row (`row_first`).
fn row_first(node: &crate::tree::Node) -> u32 {
    match node.get(Prop::RowFirst) {
        Some(PropValue::Number(f)) if f.is_finite() && *f >= 0.0 => *f as u32,
        _ => 0,
    }
}

impl Renderer {
    /// Scrolls the innermost `scroll` or `list` under `point` of
    /// `surface` (in its last frame) that can move by `dy` logical
    /// pixels, at once: one already at its end passes the scroll outward.
    /// Returns the node scrolled, if any moved.
    pub fn scroll(&mut self, surface: SurfaceId, point: LogicalPoint, dy: f32) -> Option<NodeId> {
        let id = self.scrollable_at(surface, point, dy, false)?;
        self.scrolls.entry(id).or_default().scroll_by(dy);
        self.scrolled(id);
        Some(id)
    }

    /// A scroll from the pointer: wheel steps spring the offset of the
    /// innermost `scroll` or `list` under `point` that can still move
    /// that way (as [`Renderer::scroll`]), touchpad motion moves it at
    /// once and a lift lets it fling. Under `reduced_motion` the wheel
    /// lands at once and nothing flings. Returns the node it moves.
    pub fn scroll_input(
        &mut self,
        surface: SurfaceId,
        point: LogicalPoint,
        input: ScrollInput,
    ) -> Option<NodeId> {
        let reduced = self.anim.reduced();
        match input.kind {
            ScrollKind::Lift => {
                let id = self.lists.touching.remove(&surface)?;
                let last = self.last_frame(surface);
                let st = self.scrolls.get_mut(&id)?;
                if reduced || !st.lift(input.time, last) {
                    return None;
                }
                self.scrolled(id);
                Some(id)
            }
            ScrollKind::Touch => {
                let id = self.scrollable_at(surface, point, input.dy, false)?;
                self.lists.touching.insert(surface, id);
                self.scrolls
                    .entry(id)
                    .or_default()
                    .touch(input.dy, input.time);
                self.scrolled(id);
                Some(id)
            }
            ScrollKind::Wheel => {
                let id = self.scrollable_at(surface, point, input.dy, true)?;
                let curve = if reduced {
                    Curve::Instant
                } else {
                    let tables = [&self.tree.tokens];
                    Curve::of(&TokenScope::new(&tables).transition(&Transition::Default, Prop::Y))
                };
                let last = self.last_frame(surface);
                self.scrolls
                    .entry(id)
                    .or_default()
                    .wheel(input.dy, curve, last);
                self.scrolled(id);
                Some(id)
            }
        }
    }

    /// When `surface` last painted a frame (presentation time).
    fn last_frame(&self, surface: SurfaceId) -> Option<Duration> {
        self.surfaces
            .get(&surface)
            .and_then(|s| s.painted_time)
            .filter(|t| !t.is_zero())
    }

    /// The innermost `scroll` or `list` under `point` that can move by
    /// `dy` (from where it is heading, with `target`).
    fn scrollable_at(
        &mut self,
        surface: SurfaceId,
        point: LogicalPoint,
        dy: f32,
        target: bool,
    ) -> Option<NodeId> {
        if !dy.is_finite() || dy == 0.0 {
            return None;
        }
        let chain = self.hit(surface, point);
        chain.into_iter().find(|n| {
            if !scrolls_content(&self.tree, *n) {
                return false;
            }
            let st = self.scrolls.entry(*n).or_default();
            let from = if target { st.target() } else { st.offset };
            let max = (st.content - st.viewport).max(0.0);
            (from + dy).clamp(0.0, max) != from
        })
    }

    /// Scrolls list or scroll `id` so that its child `row` is in view.
    pub fn scroll_into_view(&mut self, id: NodeId, row: NodeId) {
        let Some(node) = self.tree.get(id) else {
            return;
        };
        let Some(j) = node.children.iter().position(|c| *c == row) else {
            return;
        };
        let index = row_first(node) + j as u32;
        self.reveal_index(id, index);
    }

    /// Scrolls list `id` so that its row at global index `index` is in
    /// view, mounted or not: a row logic has not mounted is placed at
    /// the estimated row height, and the view follows it once it is
    /// measured. It lands at once.
    pub fn reveal_index(&mut self, id: NodeId, index: u32) {
        let Some(node) = self.tree.get(id) else {
            return;
        };
        let rows = node.children.clone();
        let gap = node
            .get(Prop::Gap)
            .and_then(PropValue::as_number)
            .unwrap_or(0.0)
            .max(0.0);
        let first = row_first(node);
        let st = self.scrolls.entry(id).or_default();
        let est = if st.heights.is_empty() {
            crate::layout::LIST_ROW_ESTIMATE
        } else {
            st.heights.values().sum::<f32>() / st.heights.len() as f32
        };
        let h = |r: &NodeId| st.heights.get(r).copied().unwrap_or(est);
        let (y, rh) = if index < first {
            (index as f32 * (est + gap), est)
        } else {
            let j = (index - first) as usize;
            let top = first as f32 * (est + gap);
            if j < rows.len() {
                let y = top + rows[..j].iter().map(|r| h(r) + gap).sum::<f32>();
                (y, h(&rows[j]))
            } else {
                let mounted = rows.iter().map(|r| h(r) + gap).sum::<f32>();
                let past = (j - rows.len()) as f32 * (est + gap);
                (top + mounted + past, est)
            }
        };
        let before = st.offset;
        let mut to = st.offset;
        if y < to {
            to = y;
        } else if y + rh > to + st.viewport {
            to = y + rh - st.viewport;
        }
        st.stop();
        st.offset = to.clamp(0.0, (st.content - st.viewport).max(0.0));
        if st.offset != before {
            self.scrolled(id);
        }
    }

    /// How many rows of list `id` its view shows at once (at the rows'
    /// measured mean height), once it is laid out.
    pub fn rows_in_view(&self, id: NodeId) -> Option<u32> {
        let st = self.scrolls.get(&id)?;
        if st.viewport <= 0.0 {
            return None;
        }
        let gap = self
            .tree
            .get(id)
            .and_then(|n| n.get(Prop::Gap))
            .and_then(PropValue::as_number)
            .unwrap_or(0.0)
            .max(0.0);
        let est = if st.heights.is_empty() {
            crate::layout::LIST_ROW_ESTIMATE
        } else {
            st.heights.values().sum::<f32>() / st.heights.len() as f32
        };
        let n = ((st.viewport + gap) / (est + gap).max(1.0)).floor();
        Some(if n.is_finite() { n.max(1.0) as u32 } else { 1 })
    }

    /// The offset scroll or list `id` shows (logical pixels from the top
    /// of its content).
    pub fn scroll_offset(&self, id: NodeId) -> Option<f32> {
        self.scrolls.get(&id).map(|s| s.shown)
    }

    /// The scroll state of `id` (tests, the inspector).
    pub fn scroll_state(&self, id: NodeId) -> Option<&crate::layout::ScrollState> {
        self.scrolls.get(&id)
    }

    /// The rows of virtualised lists whose mounted window no longer
    /// covers what they show and its overscan (or holds far more): each
    /// list's view plus overscan, as global row indexes, for logic
    /// (`ToLogic::ListWindow`). A window is asked once; it is asked again
    /// when logic's answer (`row_first`, `row_count`, the rows mounted)
    /// changed and still falls short.
    pub fn take_list_windows(&mut self) -> Vec<(NodeId, Range<u32>)> {
        let mut out = Vec::new();
        for (id, st) in &self.scrolls {
            let Some(w) = &st.window else { continue };
            if !w.stale() || !self.tree.contains_live(*id) {
                continue;
            }
            let key = (w.want.clone(), w.first, w.count, w.mounted);
            if self.lists.asked.get(id) == Some(&key) {
                continue;
            }
            self.lists.asked.insert(*id, key);
            out.push((*id, w.want.clone()));
        }
        let tree = &self.tree;
        self.lists.asked.retain(|id, _| tree.contains_live(*id));
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Frames painted with a list in view so far, and how many showed a
    /// gap where its content had rows (the M4 exit's "no frame shows a
    /// gap").
    pub fn list_frames(&self) -> ListFrames {
        self.lists.stats
    }

    /// Rows laid out because a scroll brought them into view (each comes
    /// in once while nothing else changes).
    pub fn scroll_rows_laid_out(&self) -> usize {
        self.lists.scroll_rows
    }

    /// True while a scroll on `surface` moves, or waits to be drawn.
    pub(super) fn scrolling(&self, surface: SurfaceId) -> bool {
        let Some(s) = self.surfaces.get(&surface) else {
            return false;
        };
        let root = s.root;
        let pending = s.boxes.as_ref().is_some_and(|b| {
            b.lists.iter().any(|(id, lb)| {
                self.scrolls.get(id).is_some_and(|st| {
                    let shown = st.offset.clamp(lb.bounds.0, lb.bounds.1);
                    (shown - lb.applied).abs() > 0.01
                })
            })
        });
        pending
            || self
                .scrolls
                .iter()
                .any(|(id, st)| st.moving() && self.tree.root_of(*id) == Some(root))
    }

    /// `id` scrolled: the surfaces showing it want a frame.
    fn scrolled(&mut self, id: NodeId) {
        let root = self.tree.root_of(id);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_dirty();
            }
        }
    }

    /// Before a frame of `surface` at `time`: scroll motions advance, and
    /// the boxes under each scrolled node move to its new offset by a
    /// paint offset. A list whose new view leaves the rows it laid out
    /// lays out the rows that came into its overscan, and only those.
    /// Nothing is moved while a full layout is due: it places them.
    pub(super) fn advance_scrolls(&mut self, surface: SurfaceId, time: Duration) {
        let Some(s) = self.surfaces.get(&surface) else {
            return;
        };
        let root = s.root;
        let snap = time.is_zero() || self.anim.reduced();
        let tree = &self.tree;
        let mut moved = false;
        for (id, st) in self.scrolls.iter_mut() {
            if st.moving() && tree.root_of(*id) == Some(root) {
                moved |= st.advance(time, snap);
            }
        }
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        if moved {
            s.mark_dirty();
        }
        if s.layout_dirty || s.boxes.is_none() {
            return;
        }
        let mut scrolled: Vec<NodeId> = s.boxes.as_ref().map_or_else(Vec::new, |b| {
            b.lists
                .iter()
                .filter(|(id, lb)| {
                    self.scrolls.get(id).is_some_and(|st| {
                        let shown = st.offset.clamp(lb.bounds.0, lb.bounds.1);
                        (shown - lb.applied).abs() > 0.01
                    })
                })
                .map(|(id, _)| *id)
                .collect()
        });
        if scrolled.is_empty() {
            return;
        }
        // Outer ones first: the boxes of a nested one move with them.
        scrolled.sort_by_key(|id| (self.depth(*id), *id));
        let mut layouts = None;
        let rest = self.anim.rest_sizes(&self.tree, root);
        for id in scrolled {
            let Some(s) = self.surfaces.get_mut(&surface) else {
                return;
            };
            let scale = s.scale;
            let (Some(boxes), Some(st)) = (s.boxes.as_mut(), self.scrolls.get_mut(&id)) else {
                continue;
            };
            let Some(lb) = boxes.lists.get(&id).copied() else {
                continue;
            };
            let shown = st.offset.clamp(lb.bounds.0, lb.bounds.1);
            let view = lb.content.h;
            let fits = match lb.laid {
                // A `scroll` holds all of its content.
                None => true,
                Some((a, b)) => {
                    let m = layout_margin(view) * 0.5;
                    (a <= 0.0 || a <= shown - m) && (b >= st.content || b >= shown + view + m)
                }
            };
            if fits {
                translate_under(&self.tree, id, boxes, lb.applied - shown);
                st.shown = shown;
                if let Some(l) = boxes.lists.get_mut(&id) {
                    l.applied = shown;
                }
            } else {
                let layouts = layouts.get_or_insert_with(|| self.shaped());
                let info = TextInfo {
                    shaped: layouts,
                    scale,
                };
                let Some(boxes) = self
                    .surfaces
                    .get_mut(&surface)
                    .and_then(|s| s.boxes.as_mut())
                else {
                    continue;
                };
                self.lists.scroll_rows +=
                    relayout_list(&self.tree, id, &info, &mut self.scrolls, boxes, &rest);
            }
            if let Some(s) = self.surfaces.get_mut(&surface) {
                s.mark_dirty();
            }
        }
    }

    /// How many ancestors `id` has.
    fn depth(&self, id: NodeId) -> usize {
        let mut d = 0;
        let mut up = self.tree.get(id).and_then(|n| n.parent);
        while let Some(p) = up {
            d += 1;
            up = self.tree.get(p).and_then(|n| n.parent);
        }
        d
    }

    /// After a frame of `surface` is flattened: does a list in it show
    /// content with no row laid out there? Counted in
    /// [`Renderer::list_frames`].
    pub(super) fn check_list_gaps(&mut self, surface: SurfaceId) {
        let Some(boxes) = self.surfaces.get(&surface).and_then(|s| s.boxes.as_ref()) else {
            return;
        };
        let mut any = false;
        let mut gap = false;
        for (id, lb) in &boxes.lists {
            let Some(node) = self.tree.get(*id).filter(|n| n.kind == NodeKind::List) else {
                continue;
            };
            let Some(st) = self.scrolls.get(id) else {
                continue;
            };
            let view = lb.content;
            if view.h <= 0.0 || st.content <= 0.0 || node.children.is_empty() {
                continue;
            }
            any = true;
            // The part of the view the content reaches.
            let bottom = view.y + view.h.min(st.content - lb.applied);
            let mut spans: Vec<(f32, f32)> = node
                .children
                .iter()
                .filter_map(|r| boxes.rects.get(r))
                .map(|r| (r.y, r.y + r.h))
                .collect();
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            // Rows stand apart by the list's `gap`; a hole is wider.
            let slack = node
                .get(Prop::Gap)
                .and_then(PropValue::as_number)
                .unwrap_or(0.0)
                .max(0.0)
                + 1.0;
            let mut reached = view.y;
            for (a, b) in spans {
                if a > reached + slack {
                    break;
                }
                reached = reached.max(b);
            }
            if reached + slack < bottom {
                gap = true;
            }
        }
        if any {
            self.lists.stats.frames += 1;
            if gap {
                self.lists.stats.gaps += 1;
            }
        }
    }
}

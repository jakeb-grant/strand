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
use crate::anim::{exit_pose, is_pose, page_slide};
use crate::layout::{
    keep_in_view, layout_margin, list_columns, list_horizontal, relayout_list, row_span,
    scroll_axes, scrolls_content, translate_under,
};

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

/// One scroll on a surface: `dy` logical pixels (positive scrolls down),
/// `dx` (positive scrolls right) and the compositor's time of the event,
/// milliseconds.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollInput {
    pub dx: f32,
    pub dy: f32,
    pub kind: ScrollKind,
    pub time: u32,
}

/// (keyboard) A `scroll`'s or `list`'s position moved from code or a key
/// ([`Renderer::scroll_move`]). `y` alone, on one that runs only along x, is
/// along x, as a wheel is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ScrollMove {
    /// To these offsets, springing.
    To { y: Option<f32>, x: Option<f32> },
    /// By this much from where it is heading, as a wheel step.
    By { dy: Option<f32>, dx: Option<f32> },
    /// By this many views, along the way it runs (y on one that runs both).
    Pages(f32),
    /// Every `scroll` and `list` around the node shows it, each moving at
    /// once and as little as it takes, as for a list's selected row.
    Reveal,
    /// This list shows its child `row` ([`Renderer::scroll_into_view`]).
    RevealRow(NodeId),
    /// This list shows its row at this index ([`Renderer::reveal_index`]).
    RevealIndex(u32),
}

/// The asks waiting for a layout, at most: a timer moving a scroll nobody
/// shows keeps only its last.
const SCROLL_MOVES_HELD: usize = 64;

/// Frames painted with a list in view, those whose view showed content
/// with no row there (rows still being mounted or laid out), and those
/// whose view was held back at the edge of the mounted rows while the
/// scroll went on (logic's rows late): the view stalls there, then
/// jumps when they land. A jump to a row not mounted
/// ([`Renderer::reveal_index`]: a `nav` key, Ctrl+End) is held at the
/// mounted edge until logic mounts it by design and is not a stall,
/// until the pointer scrolls that list again.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFrames {
    pub frames: u64,
    pub gaps: u64,
    pub stalls: u64,
    /// The global index of the first row the last of these frames
    /// showed, in the last list checked (by node id).
    pub top_row: u32,
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
    /// The node a touchpad scrolls on each surface, until it lifts, and
    /// whether along x: (flex) the gesture keeps both from its first
    /// event while the pointer stays over the node.
    touching: HashMap<SurfaceId, (NodeId, bool)>,
    stats: ListFrames,
    /// Lists whose offset a reveal set ([`Renderer::reveal_index`]): held
    /// at the mounted rows' edge until logic answers, which is not a
    /// stall. Cleared when the view reaches the offset or the pointer
    /// scrolls the list.
    revealing: HashSet<NodeId>,
    /// Rows laid out by scroll steps (tests: each row once).
    scroll_rows: usize,
    /// (keyboard) Moves waiting for the layout of the surface showing their
    /// node ([`Renderer::scroll_move`]), in order.
    moves: Vec<(NodeId, ScrollMove)>,
    /// (keyboard) The nodes whose position logic reads (`Prop::Watch` with
    /// `scroll`): only these are reported.
    watched: HashSet<NodeId>,
    /// (keyboard) What it was last told of each, `(x, y)`.
    told: HashMap<NodeId, (f32, f32)>,
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
    /// (M4) [`Renderer::hit`] as if `skip` and its subtree were not
    /// there: what a dragged source, drawn at the pointer, is over.
    pub fn hit_under(&self, surface: SurfaceId, point: LogicalPoint, skip: NodeId) -> Vec<NodeId> {
        let Some(s) = self.surfaces.get(&surface) else {
            return Vec::new();
        };
        let inside = |n: NodeId| {
            let mut cur = Some(n);
            while let Some(c) = cur {
                if c == skip {
                    return true;
                }
                cur = self.tree.get(c).and_then(|x| x.parent);
            }
            false
        };
        let k = s.scale.as_f64();
        let (x, y) = (point.x as f64 * k, point.y as f64 * k);
        let best = s
            .hits
            .iter()
            .rev()
            .find(|h| h.contains(x, y) && !inside(h.node))
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

    /// (M4) Drag and drop: draws `node` (a `drag:` source) `offset` away
    /// from its box, above its siblings, as the pointer drags it; `None`
    /// lets it go, and it springs (`$motion.spatial`, as a FLIP) from
    /// where it was held to its box, or to the box a drop moved it to.
    /// Under `reduced_motion` it is back at once.
    pub fn lift(&mut self, node: NodeId, offset: Option<LogicalPoint>) {
        let curve = if self.anim.reduced() {
            Curve::Instant
        } else {
            let tables = [&self.tree.tokens];
            Curve::of(&TokenScope::new(&tables).transition(&Transition::Default, Prop::X))
        };
        self.anim.lift(node, offset.map(|p| [p.x, p.y]), curve);
        self.mark_node_dirty(node);
    }

    /// Scrolls the innermost `scroll` or `list` under `point` of
    /// `surface` (in its last frame) that can move by `dy` logical
    /// pixels, at once: one already at its end passes the scroll outward.
    /// Returns the node scrolled, if any moved.
    pub fn scroll(&mut self, surface: SurfaceId, point: LogicalPoint, dy: f32) -> Option<NodeId> {
        let (id, x, d) = self.scrollable_at(surface, point, (0.0, dy), false)?;
        self.lists.revealing.remove(&id);
        self.axis_state(id, x).scroll_by(d);
        self.scrolled(id);
        Some(id)
    }

    /// (flex) The scroll state of `id` along x (`x`), else along y. (keyboard)
    /// A list has one, along the way it runs.
    fn axis_state(&mut self, id: NodeId, x: bool) -> &mut crate::layout::ScrollState {
        let list = self.tree.get(id).is_some_and(|n| n.kind == NodeKind::List);
        let st = self.scrolls.entry(id).or_default();
        if x && !list {
            st.across.get_or_insert_with(Default::default)
        } else {
            st
        }
    }

    /// The spring a wheel step (or a snap) moves an offset along: the
    /// default spatial transition, at once under `reduced_motion`.
    fn scroll_curve(&self) -> Curve {
        if self.anim.reduced() {
            Curve::Instant
        } else {
            let tables = [&self.tree.tokens];
            Curve::of(&TokenScope::new(&tables).transition(&Transition::Default, Prop::Y))
        }
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
                let (id, x) = self.lists.touching.remove(&surface)?;
                let last = self.last_frame(surface);
                self.lists.revealing.remove(&id);
                let curve = self.scroll_curve();
                let st = self.axis_state(id, x);
                // (flex) `snap: true`: it settles on a child, flung or not.
                let moved = if st.snaps.is_some() {
                    st.lift_to_snap(input.time, last, curve)
                } else {
                    !reduced && st.lift(input.time, last)
                };
                if !moved {
                    return None;
                }
                self.scrolled(id);
                Some(id)
            }
            ScrollKind::Touch => {
                // (flex) A gesture keeps the node and axis its first event
                // took, so a diagonal swipe on `axis: both` does not step
                // between the axes and its lift settles the axis it moved.
                let held = self
                    .lists
                    .touching
                    .get(&surface)
                    .copied()
                    .filter(|(id, _)| self.hit(surface, point).contains(id));
                if let Some((id, x)) = held {
                    let d = if x { input.dx } else { input.dy };
                    if d.is_finite() && d != 0.0 {
                        self.axis_state(id, x).touch(d, input.time);
                        self.scrolled(id);
                    }
                    return Some(id);
                }
                let (id, x, d) = self.scrollable_at(surface, point, (input.dx, input.dy), false)?;
                self.lists.revealing.remove(&id);
                self.lists.touching.insert(surface, (id, x));
                self.axis_state(id, x).touch(d, input.time);
                self.scrolled(id);
                Some(id)
            }
            ScrollKind::Wheel => {
                let (id, x, d) = self
                    .scrollable_at(surface, point, (input.dx, input.dy), true)
                    .or_else(|| self.sideways_wheel_at(surface, point, (input.dx, input.dy)))?;
                self.lists.revealing.remove(&id);
                let curve = self.scroll_curve();
                let last = self.last_frame(surface);
                self.axis_state(id, x).wheel(d, curve, last);
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

    /// (flex) A plain mouse wheel (`dy` only) that no `scroll` or `list`
    /// under `point` can take along y: the innermost scroll along x
    /// alone that can move by it, sideways, so a strip scrolls with a
    /// wheel while a page around it still takes vertical scrolling
    /// first. Never for a touchpad, whose fingers move along x when
    /// they mean to.
    fn sideways_wheel_at(
        &mut self,
        surface: SurfaceId,
        point: LogicalPoint,
        (dx, dy): (f32, f32),
    ) -> Option<(NodeId, bool, f32)> {
        if dx != 0.0 || dy == 0.0 || !dy.is_finite() {
            return None;
        }
        let chain = self.hit(surface, point);
        for n in chain {
            let Some(node) = self.tree.get(n).filter(|_| scrolls_content(&self.tree, n)) else {
                continue;
            };
            if scroll_axes(node) != [true, false] {
                continue;
            }
            let st = self.axis_state(n, true);
            let from = st.target();
            let max = (st.content - st.viewport).max(0.0);
            if (from + dy).clamp(0.0, max) != from {
                return Some((n, true, dy));
            }
        }
        None
    }

    /// The innermost `scroll` or `list` under `point` that can move by
    /// `(dx, dy)` (from where it is heading, with `target`), whether
    /// along x, and by how much. (flex) Along the larger of the two
    /// first.
    fn scrollable_at(
        &mut self,
        surface: SurfaceId,
        point: LogicalPoint,
        (dx, dy): (f32, f32),
        target: bool,
    ) -> Option<(NodeId, bool, f32)> {
        let dx = if dx.is_finite() { dx } else { 0.0 };
        let dy = if dy.is_finite() { dy } else { 0.0 };
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        let chain = self.hit(surface, point);
        for n in chain {
            let Some(node) = self.tree.get(n).filter(|_| scrolls_content(&self.tree, n)) else {
                continue;
            };
            let [along_x, along_y] = scroll_axes(node);
            let mut tries: Vec<(bool, f32)> = Vec::with_capacity(2);
            if along_y && dy != 0.0 {
                tries.push((false, dy));
            }
            if along_x && dx != 0.0 {
                tries.push((true, dx));
            }
            if dx.abs() > dy.abs() {
                tries.reverse();
            }
            for (x, d) in tries {
                let st = self.axis_state(n, x);
                let from = if target { st.target() } else { st.offset };
                let max = (st.content - st.viewport).max(0.0);
                if (from + d).clamp(0.0, max) != from {
                    return Some((n, x, d));
                }
            }
        }
        None
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
    /// measured. It lands at once, scrolled as little as it takes (to
    /// the row's top if the row is taller than the view), and stands
    /// until the pointer scrolls the list: whenever the list's viewport
    /// changes, the row is brought into view again from the offset it
    /// started from (`ScrollState::revealed`).
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
        let cols = list_columns(node);
        let st = self.scrolls.entry(id).or_default();
        let row = index
            .checked_sub(first)
            .and_then(|j| rows.get(j as usize))
            .copied();
        let (y, rh) = row_span(st, &rows, first, gap, cols, index);
        let before = st.offset;
        let to = keep_in_view(before, y, rh, st.viewport);
        st.stop();
        st.offset = to.clamp(0.0, (st.content - st.viewport).max(0.0));
        // Placed again by the next layout, and whenever the view
        // changes: the view this one went by may be on its way to
        // another size (list-scroll-regrow).
        st.revealed(row, index, before);
        if st.offset != before {
            self.lists.revealing.insert(id);
            self.scrolled(id);
        }
    }

    /// (keyboard) A move of a `scroll` or `list`, or a reveal of any node,
    /// from code or a key. It is made now if what it reads is laid out, and
    /// else once it is, in the order they were asked: a position past the
    /// content a diff just added, and a node it just mounted, need the
    /// layout that diff brings.
    pub fn scroll_move(&mut self, id: NodeId, mv: ScrollMove) {
        if mv == ScrollMove::Reveal && !self.in_a_scroll(id) {
            return;
        }
        // A row's reveal stands whatever layout comes (`ScrollState::revealed`).
        let stands = matches!(mv, ScrollMove::RevealRow(_) | ScrollMove::RevealIndex(_));
        // Moves on one surface keep their order; another's wait holds up nothing.
        let root = self.tree.root_of(id);
        let behind = self
            .lists
            .moves
            .iter()
            .any(|(n, _)| self.tree.root_of(*n) == root);
        if !behind && (stands || self.laid_surface(id).is_some()) {
            self.make_scroll_move(id, mv);
            return;
        }
        if self.lists.moves.len() >= SCROLL_MOVES_HELD {
            self.lists.moves.remove(0);
        }
        self.lists.moves.push((id, mv));
    }

    /// Whether a `scroll` or `list` is around `id`.
    fn in_a_scroll(&self, id: NodeId) -> bool {
        let mut up = self.tree.get(id).and_then(|n| n.parent);
        while let Some(p) = up {
            let Some(n) = self.tree.get(p) else {
                return false;
            };
            if scrolls_content(&self.tree, p) {
                return true;
            }
            if n.kind.is_surface() {
                return false;
            }
            up = n.parent;
        }
        false
    }

    /// A surface showing `id`, if each one showing it has its layout.
    fn laid_surface(&self, id: NodeId) -> Option<SurfaceId> {
        let root = self.tree.root_of(id)?;
        let mut showing = self.surfaces.iter().filter(|(_, s)| s.root == root);
        if showing
            .clone()
            .any(|(_, s)| s.layout_dirty || s.boxes.is_none())
        {
            return None;
        }
        showing.next().map(|(id, _)| *id)
    }

    /// A layout is done: the moves it was waited for.
    pub(super) fn run_scroll_moves(&mut self) {
        if self.lists.moves.is_empty() {
            return;
        }
        let mut waiting: Vec<(NodeId, ScrollMove)> = Vec::new();
        let mut held = HashSet::new();
        for (id, mv) in std::mem::take(&mut self.lists.moves) {
            if !self.tree.contains_live(id) {
                continue;
            }
            let root = self.tree.root_of(id);
            if held.contains(&root) || self.laid_surface(id).is_none() {
                // (No later move of its surface passes it.)
                held.insert(root);
                waiting.push((id, mv));
            } else {
                self.make_scroll_move(id, mv);
            }
        }
        self.lists.moves = waiting;
    }

    fn make_scroll_move(&mut self, id: NodeId, mv: ScrollMove) {
        let (y, x) = match mv {
            ScrollMove::Reveal => return self.reveal_node(id),
            ScrollMove::RevealRow(row) => return self.scroll_into_view(id, row),
            ScrollMove::RevealIndex(i) => return self.reveal_index(id, i),
            ScrollMove::To { y, x } => (y, x),
            ScrollMove::By { dy, dx } => (dy, dx),
            ScrollMove::Pages(n) => (Some(n), None),
        };
        let Some(node) = self
            .tree
            .get(id)
            .filter(|_| scrolls_content(&self.tree, id))
        else {
            return;
        };
        let axes = scroll_axes(node);
        let (y, x) = if axes == [true, false] && x.is_none() {
            (None, y)
        } else {
            (y, x)
        };
        let curve = self.scroll_curve();
        let last = self.laid_surface(id).and_then(|s| self.last_frame(s));
        let mut moved = false;
        for (along_x, value) in [
            (false, y.filter(|_| axes[1])),
            (true, x.filter(|_| axes[0])),
        ] {
            let Some(v) = value else { continue };
            let st = self.axis_state(id, along_x);
            moved |= match mv {
                ScrollMove::To { .. } => st.spring_towards(v, curve, last),
                ScrollMove::Pages(_) => {
                    let view = st.viewport;
                    st.wheel(v * view, curve, last)
                }
                _ => st.wheel(v, curve, last),
            };
        }
        if moved {
            self.lists.revealing.remove(&id);
            self.scrolled(id);
        }
    }

    /// `node` shows: the innermost `scroll` or `list` around it first, then
    /// each out, in view of where the one inside it moved it.
    fn reveal_node(&mut self, node: NodeId) {
        let Some(surface) = self.laid_surface(node) else {
            return;
        };
        let rect_of = |r: &Self, n: NodeId| {
            r.surfaces
                .get(&surface)
                .and_then(|s| s.boxes.as_ref())
                .and_then(|b| b.rects.get(&n).copied())
        };
        let own = rect_of(self, node);
        // How far the moves so far carry it on the surface.
        let (mut sx, mut sy) = (0.0f32, 0.0f32);
        let mut child = node;
        let mut up = self.tree.get(node).and_then(|n| n.parent);
        while let Some(a) = up {
            let Some(n) = self.tree.get(a) else {
                return;
            };
            let (surface_node, parent, kind) = (n.kind.is_surface(), n.parent, n.kind);
            let axes = scroll_axes(n);
            let horizontal = list_horizontal(n);
            let lb = self
                .surfaces
                .get(&surface)
                .and_then(|s| s.boxes.as_ref())
                .and_then(|b| b.lists.get(&a).copied());
            if let Some(lb) = lb {
                match kind {
                    NodeKind::List => {
                        self.scroll_into_view(a, child);
                        let to = self.scrolls.get(&a).map_or(lb.applied, |s| s.offset);
                        if horizontal {
                            sx -= to - lb.applied;
                        } else {
                            sy -= to - lb.applied;
                        }
                    }
                    NodeKind::Scroll => {
                        if let Some(r) = own.or_else(|| rect_of(self, child)) {
                            let mut moved = false;
                            if axes[1] {
                                let y = r.y + sy - lb.content.y + lb.applied;
                                let to = self.reveal_along(a, false, y, r.h, &mut moved);
                                sy -= to - lb.applied;
                            }
                            if axes[0] {
                                let x = r.x + sx - lb.content.x + lb.applied_x;
                                let to = self.reveal_along(a, true, x, r.w, &mut moved);
                                sx -= to - lb.applied_x;
                            }
                            if moved {
                                self.lists.revealing.remove(&a);
                                self.scrolled(a);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if surface_node {
                return;
            }
            child = a;
            up = parent;
        }
    }

    /// `scroll` `id` along x (`x`) or y lands, at once, where content from
    /// `at` for `len` shows, moving as little as it takes. The offset.
    fn reveal_along(&mut self, id: NodeId, x: bool, at: f32, len: f32, moved: &mut bool) -> f32 {
        let st = self.axis_state(id, x);
        let to = keep_in_view(st.target(), at, len, st.viewport)
            .clamp(0.0, (st.content - st.viewport).max(0.0));
        st.stop();
        if st.offset != to {
            st.offset = to;
            *moved = true;
        }
        to
    }

    /// (keyboard) Where the `scroll`s and `list`s logic reads the position of
    /// (`s.scroll_y`) are, those that moved since it was last told: `(node,
    /// x, y)`, what each shows. Nothing, at no cost, when none is read.
    pub fn take_scroll_facts(&mut self) -> Vec<(NodeId, f32, f32)> {
        if self.lists.watched.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for id in &self.lists.watched {
            let Some(st) = self.scrolls.get(id) else {
                continue;
            };
            let (x, y) = if self.tree.get(*id).is_some_and(list_horizontal) {
                (st.shown, 0.0)
            } else {
                (st.across.as_ref().map_or(0.0, |a| a.shown), st.shown)
            };
            if self.lists.told.get(id).copied().unwrap_or((0.0, 0.0)) != (x, y) {
                out.push((*id, x, y));
            }
        }
        for (id, x, y) in &out {
            self.lists.told.insert(*id, (*x, *y));
        }
        out.sort_by_key(|(id, ..)| *id);
        out
    }

    /// (keyboard) The watched nodes changed (a diff set `Prop::Watch`).
    pub(super) fn watch_scrolls(&mut self, ids: &[NodeId]) {
        for id in ids {
            let reads = matches!(
                self.tree.get(*id).and_then(|n| n.get(Prop::Watch)),
                Some(PropValue::Keyword(k)) if k.split('_').any(|p| p == "scroll")
            );
            if reads {
                self.lists.watched.insert(*id);
            } else {
                self.lists.watched.remove(id);
            }
        }
        let tree = &self.tree;
        self.lists.watched.retain(|n| tree.contains_live(*n));
        self.lists.told.retain(|n, _| tree.contains_live(*n));
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

    /// Frames painted with a list in view so far, how many showed a gap
    /// where its content had rows (the M4 exit's "no frame shows a
    /// gap"), and how many held the view back at the mounted rows' edge.
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
                self.scrolls
                    .get(id)
                    .is_some_and(|st| st.unapplied_either(lb))
            })
        });
        pending
            || self
                .scrolls
                .iter()
                .any(|(id, st)| st.any_moving() && self.tree.root_of(*id) == Some(root))
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
        let lead = self.anim.lead();
        let tree = &self.tree;
        let mut moved = false;
        for (id, st) in self.scrolls.iter_mut() {
            if st.any_moving() && tree.root_of(*id) == Some(root) {
                if st.moving() {
                    moved |= st.advance(time, snap, lead);
                }
                if let Some(a) = st.across.as_mut().filter(|a| a.moving()) {
                    moved |= a.advance(time, snap, lead);
                }
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
                    self.scrolls
                        .get(id)
                        .is_some_and(|st| st.unapplied_either(lb))
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
            let view = lb.view();
            let fits = match lb.laid {
                // A `scroll` holds all of its content.
                None => true,
                Some((a, b)) => {
                    let m = layout_margin(view) * 0.5;
                    (a <= 0.0 || a <= shown - m) && (b >= st.content || b >= shown + view + m)
                }
            };
            if fits {
                // (flex) And along x, for a scroll that runs that way.
                let shown_x = st.across.as_mut().map_or(lb.applied_x, |a| {
                    a.shown = a.offset.clamp(lb.bounds_x.0, lb.bounds_x.1);
                    a.shown
                });
                let by = if lb.horizontal {
                    (lb.applied - shown, 0.0)
                } else {
                    (lb.applied_x - shown_x, lb.applied - shown)
                };
                translate_under(&self.tree, id, boxes, by);
                st.shown = shown;
                if let Some(l) = boxes.lists.get_mut(&id) {
                    l.applied = shown;
                    l.applied_x = shown_x;
                }
            } else {
                let layouts = layouts.get_or_insert_with(|| self.shaped());
                let info = TextInfo {
                    shaped: layouts,
                    scale,
                    estimate: false,
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
        let mut stall = false;
        let mut top = None;
        // Lists whose view is where their offset is.
        let mut at_offset = Vec::new();
        let mut ids: Vec<NodeId> = boxes.lists.keys().copied().collect();
        ids.sort();
        for id in &ids {
            let Some(lb) = boxes.lists.get(id) else {
                continue;
            };
            let Some(node) = self.tree.get(*id).filter(|n| n.kind == NodeKind::List) else {
                continue;
            };
            let Some(st) = self.scrolls.get(id) else {
                continue;
            };
            let horizontal = list_horizontal(node);
            let (view_at, view) = if horizontal {
                (lb.content.x, lb.content.w)
            } else {
                (lb.content.y, lb.content.h)
            };
            if view <= 0.0 || st.content <= 0.0 || node.children.is_empty() {
                continue;
            }
            // A box's start and extent along the list.
            let along = |r: &strand_scene::LogicalRect| {
                if horizontal { (r.x, r.w) } else { (r.y, r.h) }
            };
            any = true;
            // Shown short of where the scroll is: held at the mounted
            // rows' edge (a reveal's hold is not a stall).
            let max = (st.content - st.viewport).max(0.0);
            if (st.offset.clamp(0.0, max) - lb.applied).abs() > 0.5 {
                if !self.lists.revealing.contains(id) {
                    stall = true;
                }
            } else {
                at_offset.push(*id);
            }
            // The part of the view the content reaches.
            let bottom = view_at + view.min(st.content - lb.applied);
            let mut spans: Vec<(f32, f32)> = node
                .children
                .iter()
                .filter_map(|r| boxes.rects.get(r))
                .map(|r| {
                    let (a, n) = along(r);
                    (a, a + n)
                })
                .collect();
            if let Some(j) = node.children.iter().position(|r| {
                boxes.rects.get(r).is_some_and(|r| {
                    let (a, n) = along(r);
                    a + n > view_at + 0.5
                })
            }) {
                top = Some(row_first(node) + j as u32);
            }
            spans.sort_by(|a, b| a.0.total_cmp(&b.0));
            // Rows stand apart by the list's `gap`; a hole is wider.
            let slack = node
                .get(Prop::Gap)
                .and_then(PropValue::as_number)
                .unwrap_or(0.0)
                .max(0.0)
                + 1.0;
            let mut reached = view_at;
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
        for id in at_offset {
            self.lists.revealing.remove(&id);
        }
        if any {
            let stats = &mut self.lists.stats;
            stats.frames += 1;
            if gap {
                stats.gaps += 1;
            }
            if stall {
                stats.stalls += 1;
            }
            if let Some(t) = top {
                stats.top_row = t;
            }
        }
    }
}

/// Directional `pages` transitions (see `anim/pages.rs`).
impl Renderer {
    /// True if `node` is a `page` directly under a `pages`.
    pub(super) fn is_page(&self, node: &crate::tree::Node) -> bool {
        node.kind == NodeKind::Page
            && node
                .parent
                .and_then(|p| self.tree.get(p))
                .is_some_and(|p| p.kind == NodeKind::Pages)
    }

    /// Node `id` of `kind` is being created under `parent` and enters.
    pub(super) fn page_created(&mut self, id: NodeId, kind: NodeKind, parent: Option<NodeId>) {
        let Some(p) = parent else {
            return;
        };
        let pages = self.tree.get(p).is_some_and(|n| n.kind == NodeKind::Pages);
        if pages && kind == NodeKind::Page {
            self.anim.pages.created(p, id);
        }
    }

    /// Logic is about to set `row_first` on `id`: on a `pages`, the
    /// current page's place in source order.
    pub(super) fn page_moving(&mut self, id: NodeId) {
        let Some(n) = self.tree.get(id).filter(|n| n.kind == NodeKind::Pages) else {
            return;
        };
        let old = n.get(Prop::RowFirst).cloned();
        self.anim.pages.moving(id, old.as_ref());
    }

    /// The end of a diff: each `pages` whose current page changed slides
    /// the new page in and the old one out, by their order (forward in
    /// from the right). A page with its own `enter` or `exit` plays that;
    /// a `pages` with a `transition:` mask leaves the swap to it
    /// (`transition: none` names no mask: the pages slide); with
    /// no direction (no `row_first` before) or under `reduced_motion`
    /// the old page goes at once.
    pub(super) fn settle_pages(&mut self) {
        let tree = &self.tree;
        let settled = self
            .anim
            .pages
            .settle(|p| match tree.get(p)?.get(Prop::RowFirst) {
                Some(PropValue::Number(f)) if f.is_finite() => Some(*f),
                _ => None,
            });
        let reduced = self.anim.reduced();
        for s in settled {
            let masked = self.tree.get(s.pages).is_some_and(|n| {
                n.get(Prop::Transition)
                    .is_some_and(crate::effects::transition::is_mask)
            });
            let slides = s.moved && !masked && !reduced;
            let forward = s.swap.forward;
            if let Some(e) = s.swap.entering
                && slides
                && !self
                    .tree
                    .get(e)
                    .is_some_and(|n| is_pose(n.get(Prop::Enter)))
            {
                self.anim.set_pose(e, page_slide(true, forward));
            }
            if let Some(l) = s.swap.leaving {
                let own = self.tree.get(l).is_some_and(|n| is_pose(exit_pose(n)));
                if own {
                    continue;
                }
                if slides {
                    self.anim.set_pose(l, page_slide(false, forward));
                } else if !masked {
                    self.anim.finish_now(l);
                }
            }
        }
        let tree = &self.tree;
        self.anim
            .pages
            .prune(|g| tree.is_ghost(g), |p| tree.contains_live(p));
    }

    /// The last swap of `pages`' current page while its old page still
    /// plays out: the page coming in, the page going out and whether it
    /// went forward in source order (transition masks pair them).
    pub fn page_swap(&self, pages: NodeId) -> Option<crate::anim::PageSwap> {
        self.anim
            .pages
            .last(pages)
            .filter(|s| s.leaving.is_some_and(|l| self.tree.is_ghost(l)))
    }
}

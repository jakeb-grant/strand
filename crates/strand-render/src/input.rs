//! Input routing on the main thread (design.md, "Layout, animation and
//! input"): the state machine that turns surface input into what logic
//! hears about — hover and pressed chains, keyboard focus, list
//! selection, `input` edits, `nav`, Escape, click-away and focus loss.
//!
//! [`Router::handle`] takes one [`InputEvent`] and the scene it lands on
//! (the [`Renderer`]'s last painted frame: its hit chains, scroll state
//! and tree) and returns [`Intent`]s: flags to set, events to deliver to
//! the nearest handler, two-way writes. The host turns them into
//! messages for the logic thread; this module never talks to logic, so
//! it is tested on its own, and the inspector (M5), popup grabs and drag
//! and drop (M4) reach the same state.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use strand_scene::input::button;
use strand_scene::{
    AxisSource, ButtonState, InputEvent, KeyInput, LogicalPoint, LogicalRect, Modifiers, NodeId,
    NodeKind, Prop, PropValue, SceneDiff, SceneOp, SurfaceId,
};

use crate::renderer::{Renderer, ScrollInput, ScrollKind};
use crate::tree::SceneTree;
use crate::widgets::{Caret, Edit};

/// A per-node input state logic reads (`hover`, `pressed`, `focused`,
/// `selected`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Flag {
    Hover,
    Pressed,
    Focused,
    Selected,
}

/// An event delivered to a node's nearest handler (innermost first;
/// `propagate()` passes it on).
#[derive(Clone, Debug, PartialEq)]
pub enum NodeEvent {
    /// A left click (`on click`).
    Click,
    /// A right click (`on secondary`).
    Secondary,
    /// A middle click (`on middle`).
    Middle,
    /// A scroll in wheel detents, positive down and right (`on
    /// scroll(dy, dx)`): one notch is 1, so design.md's `volume -= dy *
    /// 0.05` moves 5% a notch; smooth (touchpad) scrolling counts
    /// [`WHEEL_STEP`] logical pixels as one.
    Scroll { dy: f64, dx: f64 },
    /// A list row chosen with Enter or a click (`on activate`).
    Activate,
    /// A key pressed while the node has focus (`on key(k)`).
    Key {
        name: String,
        text: String,
        modifiers: Modifiers,
    },
    /// Escape, a click away or focus loss closed a popup (`on dismiss`).
    Dismiss,
    /// (M4) Something was dropped on the node (`on drop(value, at)`):
    /// `at` is the global row index it landed at. Produced once S-lists
    /// builds drag and drop; until then the Router emits none.
    Drop {
        payload: strand_scene::DropPayload,
        at: u32,
    },
}

/// What input asks of logic.
#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    /// `node`'s `flag` turns `on` or off.
    Flag { node: NodeId, flag: Flag, on: bool },
    /// `event` on `node`, bubbling to its nearest handler.
    Event { node: NodeId, event: NodeEvent },
    /// A two-way write: an `input`'s `text`, a surface's `open`.
    Write {
        node: NodeId,
        prop: Prop,
        value: PropValue,
    },
}

/// What routing asks of the scene: the hit chain under a point,
/// scrolling, and the tree (focus, `nav`, list rows, `open`). The
/// [`Renderer`] implements it; tests can give just a hit function
/// ([`HitOnly`]).
pub trait InputScene {
    /// The nodes under `at`, innermost first, ending at the surface's
    /// root (see [`Renderer::hit`]).
    fn hit(&self, surface: SurfaceId, at: LogicalPoint) -> Vec<NodeId>;
    /// Scrolls the innermost `scroll`/`list` under `at`; the node, if it
    /// moved.
    fn scroll(&mut self, surface: SurfaceId, at: LogicalPoint, dy: f32) -> Option<NodeId> {
        let _ = (surface, at, dy);
        None
    }
    /// (M4) A scroll from the pointer: a wheel step springs, touchpad
    /// motion follows at once, a lift may fling (see
    /// [`Renderer::scroll_input`]). By default a wheel or touch scroll is
    /// [`InputScene::scroll`] and a lift does nothing.
    fn scroll_input(
        &mut self,
        surface: SurfaceId,
        at: LogicalPoint,
        input: ScrollInput,
    ) -> Option<NodeId> {
        match input.kind {
            ScrollKind::Lift => None,
            _ => self.scroll(surface, at, input.dy),
        }
    }
    fn tree(&self) -> Option<&SceneTree> {
        None
    }
    /// Scrolls list `list` so `row` is in view.
    fn reveal(&mut self, list: NodeId, row: NodeId) {
        let _ = (list, row);
    }
    /// (M4) Scrolls virtualised list `list` so its row at global index
    /// `index` is in view, mounted or not (see
    /// [`Renderer::reveal_index`]): its window follows, and the row
    /// mounts.
    fn reveal_index(&mut self, list: NodeId, index: u32) {
        let _ = (list, index);
    }
    /// (M4) How many rows of `list` its view shows at once (Page_Up and
    /// Page_Down move by that many).
    fn rows_in_view(&self, list: NodeId) -> Option<u32> {
        let _ = list;
        None
    }
    /// True if surface node `root` is open with `keyboard: exclusive` and
    /// a two-way `open` (the design's launcher): a press on another
    /// surface is a click away from it.
    fn exclusive_open(&self, root: NodeId) -> bool {
        let _ = root;
        false
    }
    /// `node`'s `flag` turned on or off (widgets draw hover, press and
    /// focus at once, before logic hears of it).
    fn set_flag(&mut self, node: NodeId, flag: Flag, on: bool) {
        let _ = (node, flag, on);
    }
    /// `node`'s laid-out box on `surface`, logical pixels (a slider's
    /// track, a segmented control's options).
    fn node_rect(&self, surface: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        let _ = (surface, node);
        None
    }
    /// An `input`'s caret (`None`: at the end of its text).
    fn caret(&self, node: NodeId) -> Option<Caret> {
        let _ = node;
        None
    }
    fn set_caret(&mut self, node: NodeId, caret: Option<Caret>) {
        let _ = (node, caret);
    }
    /// The byte offset in `input`'s text nearest to `at` (a click placing
    /// the caret).
    fn caret_at(&self, surface: SurfaceId, input: NodeId, at: LogicalPoint) -> Option<usize> {
        let _ = (surface, input, at);
        None
    }
    /// A slider's value while dragged (`None`: the drag ended).
    fn set_drag(&mut self, slider: NodeId, value: Option<f32>) {
        let _ = (slider, value);
    }
}

impl InputScene for Renderer {
    fn hit(&self, surface: SurfaceId, at: LogicalPoint) -> Vec<NodeId> {
        Renderer::hit(self, surface, at)
    }
    fn scroll(&mut self, surface: SurfaceId, at: LogicalPoint, dy: f32) -> Option<NodeId> {
        Renderer::scroll(self, surface, at, dy)
    }
    fn scroll_input(
        &mut self,
        surface: SurfaceId,
        at: LogicalPoint,
        input: ScrollInput,
    ) -> Option<NodeId> {
        Renderer::scroll_input(self, surface, at, input)
    }
    fn tree(&self) -> Option<&SceneTree> {
        Some(Renderer::tree(self))
    }
    fn reveal(&mut self, list: NodeId, row: NodeId) {
        self.scroll_into_view(list, row);
    }
    fn reveal_index(&mut self, list: NodeId, index: u32) {
        Renderer::reveal_index(self, list, index);
    }
    fn rows_in_view(&self, list: NodeId) -> Option<u32> {
        Renderer::rows_in_view(self, list)
    }
    fn exclusive_open(&self, root: NodeId) -> bool {
        self.surface_spec(root).is_some_and(|s| {
            s.open && s.open_two_way && s.keyboard == strand_scene::Keyboard::Exclusive
        })
    }
    fn set_flag(&mut self, node: NodeId, flag: Flag, on: bool) {
        self.set_widget_flag(node, flag, on);
    }
    fn node_rect(&self, surface: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        Renderer::node_rect(self, surface, node)
    }
    fn caret(&self, node: NodeId) -> Option<Caret> {
        self.widgets().carets.get(&node).copied()
    }
    fn set_caret(&mut self, node: NodeId, caret: Option<Caret>) {
        Renderer::set_caret(self, node, caret);
    }
    fn caret_at(&self, surface: SurfaceId, input: NodeId, at: LogicalPoint) -> Option<usize> {
        Renderer::caret_at(self, surface, input, at)
    }
    fn set_drag(&mut self, slider: NodeId, value: Option<f32>) {
        Renderer::set_drag(self, slider, value);
    }
}

/// A scene that only hit-tests, through a function (tests).
pub struct HitOnly<F>(pub F);

impl<F> std::fmt::Debug for HitOnly<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HitOnly")
    }
}

impl<F: Fn(SurfaceId, LogicalPoint) -> Vec<NodeId>> InputScene for HitOnly<F> {
    fn hit(&self, surface: SurfaceId, at: LogicalPoint) -> Vec<NodeId> {
        (self.0)(surface, at)
    }
}

/// How long an `input` trusts the text it last wrote over the text the
/// scene shows (the write is still on its way through logic).
pub const EDIT_IN_FLIGHT: Duration = Duration::from_millis(500);

/// Logical pixels one wheel detent scrolls when an axis frame carries
/// only detents.
pub const WHEEL_STEP: f64 = 15.0;

/// (M4) A drag in flight, as other streams read it ([`Router::drag`]):
/// the drag ghost, `jelly` and list reordering. Drag and drop arrives in
/// M4's wave 2; until then no drag is ever in flight.
#[derive(Clone, Debug, PartialEq)]
pub struct DragView {
    /// The `drag:` source node.
    pub source: NodeId,
    /// The surface the pointer is over.
    pub surface: SurfaceId,
    /// The pointer, logical pixels on `surface`.
    pub pointer: LogicalPoint,
    /// The pointer's velocity, logical pixels per second.
    pub velocity: LogicalPoint,
    /// The node that would take the drop (its `on drop` accepts the
    /// dragged type), if any.
    pub target: Option<NodeId>,
    /// Where in the target list it would land, as a global row index.
    pub index: Option<u32>,
}

/// A list's selection that is not on a mounted row: moved there by a key
/// past the rows logic has mounted (it lands when that row mounts), or
/// left behind when the list's window unmounted the selected row.
#[derive(Clone, Copy, Debug)]
struct Away {
    /// The selected row's global index.
    index: u32,
    /// Return was pressed while it was on its way: it is activated when
    /// it lands.
    activate: bool,
    /// Scrolled into view when it lands (moved by a key, not left
    /// behind by a scroll).
    reveal: bool,
}

/// An `input`'s writes logic has not answered yet.
#[derive(Debug)]
struct InFlight {
    /// The text last written.
    latest: String,
    /// What the scene can show while those writes are on their way: the
    /// text before the first, and each written since. Anything else is
    /// logic's own value (a handler cleared the query), which wins.
    pending: Vec<String>,
    at: Instant,
}

/// The routing state of every surface: what is hovered, pressed and
/// focused, which row each list has selected, and the text each `input`
/// last wrote.
#[derive(Debug, Default)]
pub struct Router {
    /// The node each surface shows.
    surfaces: HashMap<SurfaceId, NodeId>,
    /// The hovered chain per surface (innermost first).
    hovered: HashMap<SurfaceId, Vec<NodeId>>,
    /// The chain the left button went down on, per surface.
    pressed: HashMap<SurfaceId, Vec<NodeId>>,
    /// The chain a right button press went down on, per surface.
    right_down: HashMap<SurfaceId, Vec<NodeId>>,
    /// The chain a middle button press went down on, per surface.
    middle_down: HashMap<SurfaceId, Vec<NodeId>>,
    /// The node with keyboard focus on each surface that has it.
    focus: HashMap<SurfaceId, NodeId>,
    /// The selected row of each list arrows or clicks have moved in.
    selected: HashMap<NodeId, NodeId>,
    /// The global index of each selected row, as of the last routing.
    sel_index: HashMap<NodeId, u32>,
    /// Lists whose selection is a row not mounted now.
    away: HashMap<NodeId, Away>,
    /// The last pointer position on each surface the pointer is over.
    pointer: HashMap<SurfaceId, LogicalPoint>,
    /// The text of a focused input and the rows of its `nav` list when
    /// its selection was last settled: a new query selects the first row
    /// again; rows that change under the same query (late or re-ranked
    /// results) keep a row the user moved to, while it is still there.
    nav_rows: HashMap<NodeId, (String, Vec<NodeId>)>,
    /// `nav` lists whose selection the user moved (an arrow, a click)
    /// since their input's text last changed or the input took focus.
    moved: HashSet<NodeId>,
    /// Each `input`'s writes still on their way through logic.
    edits: HashMap<NodeId, InFlight>,
    /// The slider being dragged on each surface (the left button went
    /// down on it).
    dragging: HashMap<SurfaceId, NodeId>,
    /// The `input` whose text a held left button selects, per surface.
    selecting: HashMap<SurfaceId, NodeId>,
    /// Intents of the event being handled.
    out: Vec<Intent>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// `surface` shows the surface node `node`.
    pub fn attached(&mut self, surface: SurfaceId, node: NodeId) {
        self.surfaces.insert(surface, node);
    }

    /// Logic's next scene diff, before it is applied: an `input` text it
    /// sets answers the writes in flight up to that one (logic answers in
    /// order), and a text that is none of them is logic's own (a handler
    /// cleared the query), which drops the writes in flight, so the next
    /// key builds on it.
    ///
    /// A selected row the diff unmounts for its list's window (`window:
    /// true`: the list scrolled away from it) stays selected by its
    /// global index, and is selected again when the window brings it
    /// back.
    pub fn observe(&mut self, diff: &SceneDiff) {
        for op in &diff.ops {
            if let SceneOp::Remove { id, window: true } = op
                && let Some(list) = self
                    .selected
                    .iter()
                    .find_map(|(l, r)| (*r == *id).then_some(*l))
                && let Some(&index) = self.sel_index.get(&list)
            {
                self.away.entry(list).or_insert(Away {
                    index,
                    activate: false,
                    reveal: false,
                });
            }
        }
        if self.edits.is_empty() {
            return;
        }
        for op in &diff.ops {
            let SceneOp::SetProp {
                id,
                prop: Prop::Text,
                value,
                ..
            } = op
            else {
                continue;
            };
            let Some(e) = self.edits.get_mut(id) else {
                continue;
            };
            let shown = match value {
                PropValue::Text(t) => t.as_str(),
                _ => "",
            };
            match e.pending.iter().position(|p| p == shown) {
                Some(i) if shown != e.latest => {
                    e.pending.drain(..i);
                }
                _ => {
                    self.edits.remove(id);
                }
            }
        }
    }

    /// `surface` is gone: its hover, press and focus go with it.
    pub fn detached(&mut self, surface: SurfaceId) {
        self.surfaces.remove(&surface);
        self.hovered.remove(&surface);
        self.pressed.remove(&surface);
        self.right_down.remove(&surface);
        self.middle_down.remove(&surface);
        self.focus.remove(&surface);
        self.pointer.remove(&surface);
    }

    /// (M4) The last pointer position on `surface`, while the pointer is
    /// over it (`parallax` and `tilt` read it when they flatten).
    pub fn pointer(&self, surface: SurfaceId) -> Option<LogicalPoint> {
        self.pointer.get(&surface).copied()
    }

    /// (M4) The drag in flight. Drag and drop is M4's wave 2: until it
    /// lands this is always `None`.
    pub fn drag(&self) -> Option<DragView> {
        None
    }

    /// The global index of `list`'s selected row, mounted or not (a
    /// virtualised list's selection may be a row its window has not
    /// mounted: it lands when that row mounts).
    pub fn selected_index(&self, list: NodeId) -> Option<u32> {
        match self.away.get(&list) {
            Some(a) => Some(a.index),
            None => self
                .selected
                .contains_key(&list)
                .then(|| self.sel_index.get(&list).copied())
                .flatten(),
        }
    }

    /// The node with keyboard focus on `surface`.
    pub fn focused(&self, surface: SurfaceId) -> Option<NodeId> {
        self.focus.get(&surface).copied()
    }

    /// The selected row of `list`.
    pub fn selected(&self, list: NodeId) -> Option<NodeId> {
        self.selected.get(&list).copied()
    }

    /// The hovered chain of `surface`, innermost first.
    pub fn hovered(&self, surface: SurfaceId) -> &[NodeId] {
        self.hovered.get(&surface).map_or(&[], Vec::as_slice)
    }

    /// Routes one input event; returns what logic hears of it.
    ///
    /// Pointer input goes to the node under the pointer (the scene's hit
    /// chain, innermost first, ending at the surface's node): every node
    /// on the chain is hovered (a row is hovered while a child is), a
    /// left button held marks the chain under it pressed (and latches
    /// hover there until it is released, as a drag does), a release is
    /// `click`/`secondary`/`middle` on the innermost node that both the
    /// press and the release were over (a release with no press on this
    /// surface clicks nothing), and a click on a `list` row also selects
    /// and `activate`s it. A scroll scrolls the innermost `scroll`/`list`
    /// under the pointer that can move (a default action: it always runs)
    /// and is `scroll(dy, dx)` on the innermost node.
    ///
    /// Keyboard input goes to the focused node of the focused surface:
    /// the first node with `focus: true` when the surface gets the
    /// keyboard, or an `input`/`list` clicked (a click on the list an
    /// `input` steers with `nav` leaves the typing on the input). Keys
    /// are `key(k)` on it; an `input` edits its `text` (a two-way write)
    /// and routes arrows to the list its `nav` names, as a focused `list`
    /// takes them itself: Up/Down move the selection (scrolled into
    /// view) and Return `activate`s the selected row. Escape, a click on
    /// the surface's click-away catcher and losing the keyboard write
    /// `open: false` on a surface whose `open` is two-way (a popup also
    /// gets `dismiss`).
    pub fn handle(&mut self, event: &InputEvent, scene: &mut dyn InputScene) -> Vec<Intent> {
        self.route(event, scene);
        self.select_first_rows(scene);
        self.flush(scene)
    }

    /// After logic's diff is applied: a focused `input`'s `nav` list that
    /// has rows and no selected row (its rows just arrived, or the one
    /// selected left), or whose input's text changed since (a new query,
    /// or `on show` cleared it), selects its first, so `selected` shows
    /// which row Return will `activate`. Rows that change under the same
    /// text (an `Async` search's late or re-ranked results) select the
    /// first too, unless the user moved the selection (an arrow, a
    /// click) to a row that is still there. Returns what logic hears of
    /// it.
    ///
    /// A selection moved past a virtualised list's mounted rows lands on
    /// its row once logic mounts it (and Return pressed meanwhile
    /// activates it then).
    pub fn settle(&mut self, scene: &mut dyn InputScene) -> Vec<Intent> {
        self.prune(scene.tree());
        self.land(scene);
        self.select_first_rows(scene);
        self.index_selections(scene.tree());
        self.flush(scene)
    }

    /// Selections that were away from their list's mounted rows and
    /// whose row is mounted now: selected (scrolled into view if a key
    /// moved them there), and activated if Return was pressed meanwhile.
    fn land(&mut self, scene: &mut dyn InputScene) {
        let Some(tree) = scene.tree() else {
            return;
        };
        self.away.retain(|l, _| tree.contains_live(*l));
        let mut landed = Vec::new();
        for (&list, a) in &self.away {
            let w = window_rows(tree, list);
            if w.count == 0 {
                continue;
            }
            let index = a.index.min(w.count - 1);
            if let Some(row) = w.row(index) {
                landed.push((list, row, *a));
            }
        }
        landed.sort_by_key(|(l, _, _)| *l);
        for (list, row, a) in landed {
            self.away.remove(&list);
            self.select_with(scene, list, Some(row), a.reveal);
            if a.activate {
                self.event(row, NodeEvent::Activate);
            }
        }
    }

    /// Remembers each selected row's global index (a window that
    /// unmounts it keeps the selection by it).
    fn index_selections(&mut self, tree: Option<&SceneTree>) {
        let Some(tree) = tree else {
            return;
        };
        for (&list, &row) in &self.selected {
            if let Some(i) = window_rows(tree, list).index_of(row) {
                self.sel_index.insert(list, i);
            }
        }
        let selected = &self.selected;
        self.sel_index.retain(|l, _| selected.contains_key(l));
    }

    /// Widgets draw hover, press, focus and selection at once.
    fn flush(&mut self, scene: &mut dyn InputScene) -> Vec<Intent> {
        for i in &self.out {
            if let Intent::Flag { node, flag, on } = i {
                scene.set_flag(*node, *flag, *on);
            }
        }
        std::mem::take(&mut self.out)
    }

    /// See [`Router::settle`].
    fn select_first_rows(&mut self, scene: &mut dyn InputScene) {
        let Some(tree) = scene.tree() else {
            return;
        };
        let mut want = Vec::new();
        for &f in self.focus.values() {
            let Some(n) = tree.get(f) else {
                continue;
            };
            let (NodeKind::Input, Some(PropValue::Node(list))) = (n.kind, n.get(Prop::Nav)) else {
                continue;
            };
            let rows: Vec<NodeId> = tree.get(*list).map_or_else(Vec::new, |l| {
                l.children
                    .iter()
                    .copied()
                    .filter(|r| tree.contains_live(*r))
                    .collect()
            });
            let text = match n.get(Prop::Text) {
                Some(PropValue::Text(t)) => t.clone(),
                _ => String::new(),
            };
            // A selection on its way to a row not mounted yet (or left
            // behind by a scroll) waits for it, unless a new query
            // starts the results over.
            if self.away.contains_key(list) {
                match self.nav_rows.get(list) {
                    Some((t, _)) if *t != text => {
                        self.away.remove(list);
                        self.moved.remove(list);
                    }
                    _ => {
                        self.nav_rows.insert(*list, (text, rows));
                        continue;
                    }
                }
            }
            if let Some(sel) = self.selected.get(list) {
                match self.nav_rows.get(list) {
                    // A new query: its results start at the top.
                    Some((t, _)) if *t != text => {
                        self.moved.remove(list);
                    }
                    Some((_, seen)) if *seen == rows => continue,
                    // The same query's rows changed (results arriving
                    // late, re-ranked): the row the user moved to stays
                    // selected while it is there.
                    Some(_) if self.moved.contains(list) && rows.contains(sel) => {
                        self.nav_rows.insert(*list, (text, rows));
                        continue;
                    }
                    Some(_) => {}
                    // Selected by a click or an arrow before any settle.
                    None => {
                        self.nav_rows.insert(*list, (text, rows));
                        continue;
                    }
                }
            }
            if let Some(&row) = rows.first() {
                want.push((*list, row));
            }
            self.nav_rows.insert(*list, (text, rows));
        }
        for (list, row) in want {
            self.select(scene, list, Some(row));
        }
    }

    fn emit(&mut self, i: Intent) {
        self.out.push(i);
    }

    fn flag(&mut self, node: NodeId, flag: Flag, on: bool) {
        self.emit(Intent::Flag { node, flag, on });
    }

    fn event(&mut self, node: NodeId, event: NodeEvent) {
        self.emit(Intent::Event { node, event });
    }

    fn route(&mut self, event: &InputEvent, scene: &mut dyn InputScene) {
        let surface = event.surface();
        let Some(&root) = self.surfaces.get(&surface) else {
            return;
        };
        self.prune(scene.tree());
        let chain = |scene: &dyn InputScene, at: LogicalPoint| {
            let c = scene.hit(surface, at);
            if c.is_empty() { vec![root] } else { c }
        };
        match event {
            InputEvent::PointerEnter { position, .. }
            | InputEvent::PointerMotion { position, .. } => {
                self.pointer.insert(surface, *position);
                if self.pressed.contains_key(&surface) {
                    // A drag: the slider follows the pointer, a held
                    // button in an `input` extends the selection.
                    if let Some(&slider) = self.dragging.get(&surface) {
                        self.drag_slider(scene, surface, slider, *position, false);
                    }
                    if let Some(&input) = self.selecting.get(&surface)
                        && let Some(pos) = scene.caret_at(surface, input, *position)
                    {
                        let anchor = scene.caret(input).map_or(pos, |c| c.anchor);
                        scene.set_caret(input, Some(Caret { pos, anchor }));
                    }
                    return;
                }
                let now = chain(scene, *position);
                self.hover(surface, now);
            }
            InputEvent::PointerLeave { .. } => {
                self.pointer.remove(&surface);
                self.end_drags(scene, surface);
                self.release(surface);
                self.hover(surface, Vec::new());
            }
            InputEvent::PointerButton {
                button: b,
                state,
                position,
                ..
            } => {
                self.pointer.insert(surface, *position);
                let under = chain(scene, *position);
                if *b == button::LEFT && *state == ButtonState::Pressed {
                    self.click_away_from_others(surface, root, scene);
                }
                self.button(surface, *b, *state, under, *position, scene);
            }
            InputEvent::PointerAxis {
                horizontal,
                vertical,
                position,
                source,
                time,
                ..
            } => {
                self.pointer.insert(surface, *position);
                // A wheel (detents, or its high-resolution pixels) springs
                // the offset; a touchpad or other continuous source moves
                // it at once, and its lift (`axis_stop`) may fling.
                let kind = match source {
                    Some(AxisSource::Wheel | AxisSource::WheelTilt) => ScrollKind::Wheel,
                    Some(AxisSource::Finger | AxisSource::Continuous) => ScrollKind::Touch,
                    None if vertical.value120 != 0 => ScrollKind::Wheel,
                    None => ScrollKind::Touch,
                };
                let lift = vertical.stop && kind == ScrollKind::Touch;
                // An axis frame with no motion (a finger lifted:
                // `axis_stop` alone) scrolls nothing and wakes no handler.
                // Detents alone (a wheel whose frame carries no pixel
                // value) scroll 15 px each, as libinput's legacy wheel.
                let px = |a: &strand_scene::AxisDelta| {
                    if a.pixels == 0.0 && a.value120 != 0 {
                        f64::from(a.value120) / 120.0 * WHEEL_STEP
                    } else {
                        a.pixels
                    }
                };
                let (dy, dx) = (px(vertical), px(horizontal));
                let lifted = |scene: &mut dyn InputScene| {
                    if lift {
                        let input = ScrollInput {
                            dy: 0.0,
                            kind: ScrollKind::Lift,
                            time: *time,
                        };
                        scene.scroll_input(surface, *position, input);
                    }
                };
                if dy == 0.0 && dx == 0.0 {
                    lifted(scene);
                    return;
                }
                let under = chain(scene, *position);
                if dy != 0.0 {
                    let input = ScrollInput {
                        dy: dy as f32,
                        kind,
                        time: *time,
                    };
                    scene.scroll_input(surface, *position, input);
                }
                lifted(scene);
                // The handler counts detents (a wheel's own, else pixels
                // over the legacy step).
                let notches = |a: &strand_scene::AxisDelta, px: f64| {
                    if a.value120 != 0 {
                        f64::from(a.value120) / 120.0
                    } else {
                        px / WHEEL_STEP
                    }
                };
                let (dy, dx) = (notches(vertical, dy), notches(horizontal, dx));
                self.event(under[0], NodeEvent::Scroll { dy, dx });
            }
            InputEvent::KeyboardEnter { .. } => {
                // Focus held from before (the keyboard back from a popup
                // that grabbed it: no leave came between) stays where Tab
                // or a click had moved it.
                if self.focus.contains_key(&surface) {
                    return;
                }
                let first = scene
                    .tree()
                    .and_then(|t| first_with_focus(t, root))
                    .unwrap_or(root);
                self.set_focus(surface, Some(first));
                // Fresh focus (the surface opened again) starts its
                // `nav` list at the top: the first row is selected again
                // (`settle`), not the one selected when it last closed.
                let nav = scene.tree().and_then(|t| match t.get(first) {
                    Some(n) if n.kind == NodeKind::Input => match n.get(Prop::Nav) {
                        Some(PropValue::Node(list)) => Some(*list),
                        _ => None,
                    },
                    _ => None,
                });
                if let Some(list) = nav {
                    self.moved.remove(&list);
                    self.select(scene, list, None);
                }
            }
            InputEvent::KeyboardLeave { .. } => {
                self.set_focus(surface, None);
                // Focus loss closes a surface bound `open: <-> x`.
                if open_two_way(scene.tree(), root) {
                    self.close(scene, root);
                }
            }
            InputEvent::ClickAway { .. } => {
                if open_two_way(scene.tree(), root) {
                    self.close(scene, root);
                }
            }
            InputEvent::Key { key, .. } => {
                if key.state == ButtonState::Pressed {
                    self.key(surface, root, key, scene);
                }
            }
            // (M4) `wl_data_device` drags: S-lists routes them (targets
            // by `Prop::Accepts`, `NodeEvent::Drop`). Until then they
            // change nothing and emit no intent.
            InputEvent::DragEnter { .. }
            | InputEvent::DragMotion { .. }
            | InputEvent::DragLeave { .. }
            | InputEvent::DragDrop { .. } => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn button(
        &mut self,
        surface: SurfaceId,
        b: u32,
        state: ButtonState,
        under: Vec<NodeId>,
        at: LogicalPoint,
        scene: &mut dyn InputScene,
    ) {
        // The chain the matching press went down on: a release clicks the
        // innermost node on both chains (pressed on one button, released
        // on its sibling: their row), and nothing without a press on this
        // surface.
        let down = match (b, state) {
            (button::LEFT, ButtonState::Released) => self.pressed.get(&surface).cloned(),
            (button::RIGHT, ButtonState::Pressed) => {
                self.right_down.insert(surface, under.clone());
                None
            }
            (button::RIGHT, ButtonState::Released) => self.right_down.remove(&surface),
            (button::MIDDLE, ButtonState::Pressed) => {
                self.middle_down.insert(surface, under.clone());
                None
            }
            (button::MIDDLE, ButtonState::Released) => self.middle_down.remove(&surface),
            _ => None,
        };
        if b == button::LEFT {
            match state {
                ButtonState::Pressed => {
                    self.hover(surface, under.clone());
                    for &n in under.iter().rev() {
                        self.flag(n, Flag::Pressed, true);
                    }
                    self.pressed.insert(surface, under.clone());
                    // A click focuses the `input` or `list` under it.
                    let target = scene.tree().and_then(|tree| {
                        under.iter().copied().find(|n| {
                            tree.get(*n)
                                .is_some_and(|n| matches!(n.kind, NodeKind::Input | NodeKind::List))
                        })
                    });
                    // A click on the list an `input` steers with `nav`
                    // leaves the typing on the input.
                    let steers = |list: NodeId| {
                        let f = self.focus.get(&surface)?;
                        let n = scene.tree()?.get(*f)?;
                        (n.kind == NodeKind::Input
                            && n.get(Prop::Nav) == Some(&PropValue::Node(list)))
                        .then_some(())
                    };
                    if let Some(t) = target
                        && steers(t).is_none()
                    {
                        self.set_focus(surface, Some(t));
                    }
                    let kind_of = |scene: &dyn InputScene, n: NodeId| {
                        scene.tree().and_then(|t| t.get(n)).map(|n| n.kind)
                    };
                    // A press on a slider whose `value` is bound two-way
                    // moves it there and starts a drag (a display-only
                    // one stays put).
                    if let Some(&slider) = under
                        .iter()
                        .find(|n| kind_of(scene, **n) == Some(NodeKind::Slider))
                        && value_two_way(scene.tree(), slider)
                    {
                        self.dragging.insert(surface, slider);
                        self.drag_slider(scene, surface, slider, at, false);
                    }
                    // A press in an `input` puts the caret there and starts
                    // selecting.
                    if let Some(&input) = under
                        .iter()
                        .find(|n| kind_of(scene, **n) == Some(NodeKind::Input))
                        && let Some(pos) = scene.caret_at(surface, input, at)
                    {
                        scene.set_caret(input, Some(Caret::at(pos)));
                        self.selecting.insert(surface, input);
                    }
                }
                ButtonState::Released => {
                    if let Some(&slider) = self.dragging.get(&surface) {
                        self.drag_slider(scene, surface, slider, at, true);
                    }
                    self.end_drags(scene, surface);
                    self.release(surface);
                    self.hover(surface, under.clone());
                }
            }
        }
        if state != ButtonState::Released {
            return;
        }
        let event = match b {
            button::LEFT => NodeEvent::Click,
            button::RIGHT => NodeEvent::Secondary,
            button::MIDDLE => NodeEvent::Middle,
            _ => return,
        };
        let Some(down) = down else {
            return;
        };
        let Some(&node) = under.iter().find(|n| down.contains(n)) else {
            return;
        };
        let activate = if event == NodeEvent::Click {
            list_row(scene.tree(), &under)
        } else {
            None
        };
        // A click on a `segmented` chooses the option under it.
        if event == NodeEvent::Click {
            self.choose(scene, surface, &under, at);
        }
        self.event(node, event);
        if let Some((list, row)) = activate {
            self.moved.insert(list);
            self.select(scene, list, Some(row));
            self.event(row, NodeEvent::Activate);
        }
    }

    /// Forgets selections and edits of nodes that are gone (a list
    /// refilled by its `for`, an `input` in an `if` that turned false,
    /// a reload), and a selection whose row left its list.
    fn prune(&mut self, tree: Option<&SceneTree>) {
        let Some(tree) = tree else {
            return;
        };
        // A node logic removed is gone at once, even while it plays its
        // exit pose (a ghost): it keeps no focus, edit or selection.
        let live = |n: NodeId| tree.contains_live(n);
        self.nav_rows.retain(|list, _| live(*list));
        self.moved.retain(|list| live(*list));
        self.away.retain(|list, _| live(*list));
        self.selected.retain(|list, row| {
            live(*list) && live(*row) && tree.get(*row).is_some_and(|r| r.parent == Some(*list))
        });
        self.edits.retain(|n, _| live(*n));
        self.focus.retain(|_, n| live(*n));
    }

    /// Moves keyboard focus on `surface` to `node` (`focused`).
    fn set_focus(&mut self, surface: SurfaceId, node: Option<NodeId>) {
        let old = match node {
            Some(n) => self.focus.insert(surface, n),
            None => self.focus.remove(&surface),
        };
        if old == node {
            return;
        }
        if let Some(o) = old {
            self.flag(o, Flag::Focused, false);
        }
        if let Some(n) = node {
            self.flag(n, Flag::Focused, true);
        }
    }

    /// `open: false` on the surface node `root` (Escape, click-away, focus
    /// loss); a popup is also told `dismiss`.
    /// A left press on `surface` (showing `root`) is a click away from
    /// every other open `keyboard: exclusive` surface whose `open` is
    /// two-way: the catcher under such a surface covers only the usable
    /// area of its own output, so a press on Strand's own bar (or a
    /// surface on another output) is caught here. The press itself still
    /// goes to what it landed on.
    fn click_away_from_others(&mut self, surface: SurfaceId, root: NodeId, scene: &dyn InputScene) {
        let mut others: Vec<NodeId> = self
            .surfaces
            .iter()
            .filter(|(s, r)| **s != surface && **r != root)
            .map(|(_, r)| *r)
            .filter(|r| scene.exclusive_open(*r))
            .collect();
        others.sort();
        others.dedup();
        for other in others {
            self.close(scene, other);
        }
    }

    fn close(&mut self, scene: &dyn InputScene, root: NodeId) {
        self.emit(Intent::Write {
            node: root,
            prop: Prop::Open,
            value: PropValue::Bool(false),
        });
        if scene
            .tree()
            .and_then(|t| t.get(root))
            .is_some_and(|n| n.kind == NodeKind::Popup)
        {
            self.event(root, NodeEvent::Dismiss);
        }
    }

    /// A key pressed on `surface` (see [`Router::handle`]).
    fn key(
        &mut self,
        surface: SurfaceId,
        root: NodeId,
        key: &KeyInput,
        scene: &mut dyn InputScene,
    ) {
        let focus = self.focus.get(&surface).copied().unwrap_or(root);
        let node = scene.tree().and_then(|t| t.get(focus));
        let kind = node.map(|n| n.kind);
        let nav = match node.and_then(|n| n.get(Prop::Nav)) {
            Some(PropValue::Node(l)) => Some(*l),
            _ => None,
        };
        let text = match node.and_then(|n| n.get(Prop::Text)) {
            Some(PropValue::Text(t)) => t.clone(),
            _ => String::new(),
        };
        let open = open_two_way(scene.tree(), root);
        self.event(
            focus,
            NodeEvent::Key {
                name: key.name.clone(),
                text: key.text.clone(),
                modifiers: key.modifiers,
            },
        );
        if key.name == "Escape" && open {
            self.close(scene, root);
            return;
        }
        let list = match kind {
            Some(NodeKind::List) => Some(focus),
            _ => nav,
        };
        // Submenus (tray menus): Right on a row that holds a popup opens
        // it; Left closes a popup that opened from another popup.
        if kind != Some(NodeKind::Input) {
            let tree = scene.tree();
            match key.name.as_str() {
                "Right" | "KP_Right" => {
                    let rows = list
                        .and_then(|l| self.selected.get(&l))
                        .into_iter()
                        .copied();
                    let menu = tree.and_then(|t| {
                        rows.filter_map(|r| submenu_in(t, r, true))
                            .next()
                            .or_else(|| {
                                self.hovered.get(&surface).and_then(|chain| {
                                    chain
                                        .iter()
                                        .take_while(|n| **n != root)
                                        .find_map(|n| submenu_in(t, *n, false))
                                })
                            })
                    });
                    if let Some(menu) = menu {
                        self.emit(Intent::Write {
                            node: menu,
                            prop: Prop::Open,
                            value: PropValue::Bool(true),
                        });
                        return;
                    }
                }
                "Left" | "KP_Left" if open && tree.is_some_and(|t| nested_popup(t, root)) => {
                    self.close(scene, root);
                    return;
                }
                _ => {}
            }
        }
        if let Some(list) = list {
            let w = scene
                .tree()
                .map(|t| window_rows(t, list))
                .unwrap_or_default();
            let cur = self
                .away
                .get(&list)
                .map(|a| a.index)
                .or_else(|| self.selected.get(&list).and_then(|r| w.index_of(*r)));
            let last = w.count.saturating_sub(1);
            let page = scene.rows_in_view(list).unwrap_or(1).max(1);
            // In an `input`, Home and End move its caret (Shift extends
            // the selection); Ctrl+Home and Ctrl+End go to the list's
            // ends. On a focused list they go there with any modifiers.
            let ends = kind != Some(NodeKind::Input) || key.modifiers.ctrl;
            let to = match key.name.as_str() {
                "Down" | "KP_Down" => Some(cur.map_or(w.first, |i| (i + 1).min(last))),
                "Up" | "KP_Up" => Some(cur.map_or(w.first, |i| i.saturating_sub(1))),
                "Page_Down" | "Next" | "KP_Page_Down" | "KP_Next" => {
                    Some(cur.map_or(w.first, |i| i.saturating_add(page).min(last)))
                }
                "Page_Up" | "Prior" | "KP_Page_Up" | "KP_Prior" => {
                    Some(cur.map_or(w.first, |i| i.saturating_sub(page)))
                }
                "Home" | "KP_Home" if ends => Some(0),
                "End" | "KP_End" if ends => Some(last),
                "Return" | "KP_Enter" => {
                    if let Some(a) = self.away.get_mut(&list) {
                        // On its way: activated when it lands.
                        a.activate = true;
                        if !a.reveal {
                            a.reveal = true;
                            scene.reveal_index(list, a.index);
                        }
                        return;
                    }
                    let row = self
                        .selected
                        .get(&list)
                        .copied()
                        .filter(|r| w.rows.contains(r))
                        .or(w.rows.first().copied());
                    if let Some(row) = row {
                        self.event(row, NodeEvent::Activate);
                    }
                    return;
                }
                _ => None,
            };
            if let Some(to) = to {
                if w.count > 0 {
                    self.moved.insert(list);
                    self.go_to(scene, list, &w, to);
                }
                return;
            }
        }
        if kind == Some(NodeKind::Input) {
            // Keys typed faster than logic answers build on the last
            // write; once the scene shows it, or a text of logic's own,
            // they build on the scene.
            let in_flight = self.edits.get(&focus).filter(|e| {
                e.latest != text && e.at.elapsed() < EDIT_IN_FLIGHT && e.pending.contains(&text)
            });
            // (`observe` already dropped what logic answered.)
            let mut pending = in_flight.map_or_else(|| vec![text.clone()], |e| e.pending.clone());
            let now = in_flight.map_or(text, |e| e.latest.clone());
            let caret = scene
                .caret(focus)
                .unwrap_or(Caret::at(now.len()))
                .clamped(&now);
            match crate::widgets::edit(&now, caret, &key.name, &key.text, key.modifiers) {
                Edit::None => {}
                Edit::Moved(c) => scene.set_caret(focus, Some(c)),
                Edit::Changed(now, c) => {
                    scene.set_caret(focus, Some(c));
                    pending.push(now.clone());
                    self.edits.insert(
                        focus,
                        InFlight {
                            latest: now.clone(),
                            pending,
                            at: Instant::now(),
                        },
                    );
                    self.emit(Intent::Write {
                        node: focus,
                        prop: Prop::Text,
                        value: PropValue::Text(now),
                    });
                }
            }
        }
    }

    /// Moves `slider` to the pointer at `at` (its value from where `at`
    /// falls along its track, 0 to 1) and writes it; `last` ends the drag.
    fn drag_slider(
        &mut self,
        scene: &mut dyn InputScene,
        surface: SurfaceId,
        slider: NodeId,
        at: LogicalPoint,
        last: bool,
    ) {
        let Some(r) = scene.node_rect(surface, slider) else {
            return;
        };
        // Pressed, so drawn with its larger knob: the same span.
        let (x0, x1) = crate::widgets::slider_span(r.x as f64, (r.x + r.w) as f64, true, 1.0);
        let v = (((at.x as f64 - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0)) as f32;
        if !v.is_finite() {
            return;
        }
        scene.set_drag(slider, (!last).then_some(v));
        self.emit(Intent::Write {
            node: slider,
            prop: Prop::Value,
            value: PropValue::Number(v),
        });
    }

    /// Ends a slider drag and a selection by pointer on `surface`.
    fn end_drags(&mut self, scene: &mut dyn InputScene, surface: SurfaceId) {
        if let Some(slider) = self.dragging.remove(&surface) {
            scene.set_drag(slider, None);
        }
        self.selecting.remove(&surface);
    }

    /// A click at `at` on a chain holding a `segmented`: writes the option
    /// whose segment it falls in.
    fn choose(
        &mut self,
        scene: &mut dyn InputScene,
        surface: SurfaceId,
        under: &[NodeId],
        at: LogicalPoint,
    ) {
        let Some(tree) = scene.tree() else {
            return;
        };
        let Some(seg) = under
            .iter()
            .copied()
            .find(|n| tree.get(*n).is_some_and(|n| n.kind == NodeKind::Segmented))
        else {
            return;
        };
        if !value_two_way(Some(tree), seg) {
            return;
        }
        let opts = crate::widgets::options(tree.get(seg).and_then(|n| n.get(Prop::Options)));
        let Some(r) = scene.node_rect(surface, seg) else {
            return;
        };
        if opts.is_empty() || r.w <= 0.0 {
            return;
        }
        let i = (((at.x - r.x) / r.w * opts.len() as f32).floor().max(0.0) as usize)
            .min(opts.len() - 1);
        self.emit(Intent::Write {
            node: seg,
            prop: Prop::Value,
            value: opts[i].clone(),
        });
    }

    /// Moves `list`'s selection to its row at global index `to`: selected
    /// at once if mounted, else on its way (scrolled to, so the window
    /// mounts it; `settle` lands it).
    fn go_to(&mut self, scene: &mut dyn InputScene, list: NodeId, w: &WindowRows, to: u32) {
        if let Some(row) = w.row(to) {
            self.away.remove(&list);
            self.select(scene, list, Some(row));
            return;
        }
        self.select(scene, list, None);
        self.away.insert(
            list,
            Away {
                index: to,
                activate: false,
                reveal: true,
            },
        );
        scene.reveal_index(list, to);
    }

    /// Selects `row` in `list` (`selected`), scrolled into view.
    fn select(&mut self, scene: &mut dyn InputScene, list: NodeId, row: Option<NodeId>) {
        self.select_with(scene, list, row, true);
    }

    /// Selects `row` in `list`, scrolled into view if `reveal`.
    fn select_with(
        &mut self,
        scene: &mut dyn InputScene,
        list: NodeId,
        row: Option<NodeId>,
        reveal: bool,
    ) {
        if let Some(r) = row
            && let Some(i) = scene.tree().and_then(|t| window_rows(t, list).index_of(r))
        {
            self.sel_index.insert(list, i);
        }
        let old = match row {
            Some(r) => self.selected.insert(list, r),
            None => self.selected.remove(&list),
        };
        if old == row {
            return;
        }
        if let Some(o) = old {
            self.flag(o, Flag::Selected, false);
        }
        if let Some(r) = row {
            self.flag(r, Flag::Selected, true);
            if reveal {
                scene.reveal(list, r);
            }
        }
    }

    /// The hovered chain of `surface` becomes `now`: nodes it left lose
    /// `hover` (innermost first), nodes it reached get it (outermost
    /// first).
    fn hover(&mut self, surface: SurfaceId, now: Vec<NodeId>) {
        let was = self.hovered.remove(&surface).unwrap_or_default();
        for &n in &was {
            if !now.contains(&n) {
                self.flag(n, Flag::Hover, false);
            }
        }
        for &n in now.iter().rev() {
            if !was.contains(&n) {
                self.flag(n, Flag::Hover, true);
            }
        }
        if !now.is_empty() {
            self.hovered.insert(surface, now);
        }
    }

    /// The left button is up (or the pointer left): nothing is pressed.
    fn release(&mut self, surface: SurfaceId) {
        for n in self.pressed.remove(&surface).unwrap_or_default() {
            self.flag(n, Flag::Pressed, false);
        }
    }
}

/// The first node under `root` (depth first) with `focus: true`.
fn first_with_focus(tree: &SceneTree, root: NodeId) -> Option<NodeId> {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = tree.get(id)?;
        if matches!(n.get(Prop::Focus), Some(PropValue::Bool(true))) {
            return Some(id);
        }
        stack.extend(n.children.iter().rev());
    }
    None
}

/// True if the surface node `root` has `open` bound two-way (`open: <->
/// x`, as the compiler marks it with `two_way`): only then do Escape,
/// click-away and focus loss write `false` to it.
fn open_two_way(tree: Option<&SceneTree>, root: NodeId) -> bool {
    tree.and_then(|t| t.get(root))
        .is_some_and(|n| strand_scene::is_two_way(n.get(Prop::TwoWay), Prop::Open))
}

/// True if `node`'s `value` is bound two-way (`value: <-> x`): only then
/// does a slider drag or a segmented click write it.
fn value_two_way(tree: Option<&SceneTree>, node: NodeId) -> bool {
    tree.and_then(|t| t.get(node))
        .is_some_and(|n| strand_scene::is_two_way(n.get(Prop::TwoWay), Prop::Value))
}

/// The `list` and its row on a hit chain (innermost first).
fn list_row(tree: Option<&SceneTree>, chain: &[NodeId]) -> Option<(NodeId, NodeId)> {
    let tree = tree?;
    chain.windows(2).find_map(|w| {
        let parent = tree.get(w[1])?;
        (parent.kind == NodeKind::List).then_some((w[1], w[0]))
    })
}

/// The rows a list has mounted, with where they sit among all its rows.
#[derive(Debug, Default)]
struct WindowRows {
    /// The global index of the first mounted row (`row_first`).
    first: u32,
    /// All rows, mounted or not (`row_count`; the mounted ones on a list
    /// logic does not window).
    count: u32,
    /// The mounted rows, in order (ghosts left out).
    rows: Vec<NodeId>,
}

impl WindowRows {
    /// The mounted row at global index `index`.
    fn row(&self, index: u32) -> Option<NodeId> {
        let j = index.checked_sub(self.first)?;
        self.rows.get(j as usize).copied()
    }

    /// The global index of mounted row `row`.
    fn index_of(&self, row: NodeId) -> Option<u32> {
        let j = self.rows.iter().position(|r| *r == row)?;
        Some(self.first + j as u32)
    }
}

/// `list`'s mounted rows and their place among all its rows.
fn window_rows(tree: &SceneTree, list: NodeId) -> WindowRows {
    let Some(n) = tree.get(list) else {
        return WindowRows::default();
    };
    let index = |p: Prop| match n.get(p) {
        Some(PropValue::Number(f)) if f.is_finite() && *f >= 0.0 => Some(*f as u32),
        _ => None,
    };
    let rows: Vec<NodeId> = n
        .children
        .iter()
        .copied()
        .filter(|r| tree.contains_live(*r))
        .collect();
    let first = index(Prop::RowFirst).unwrap_or(0);
    let mounted = first.saturating_add(rows.len() as u32);
    let count = index(Prop::RowCount).map_or(mounted, |c| c.max(mounted));
    WindowRows { first, count, rows }
}

/// A closed `popup` with a two-way `open` that `node` holds: among its
/// direct children, or anywhere below it if `deep` (popups' own content
/// left out).
fn submenu_in(tree: &SceneTree, node: NodeId, deep: bool) -> Option<NodeId> {
    let mut stack: Vec<NodeId> = tree.get(node)?.children.iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        let Some(n) = tree.get(id).filter(|_| tree.contains_live(id)) else {
            continue;
        };
        if n.kind == NodeKind::Popup {
            if strand_scene::is_two_way(n.get(Prop::TwoWay), Prop::Open)
                && !matches!(n.get(Prop::Open), Some(PropValue::Bool(true)))
            {
                return Some(id);
            }
            continue;
        }
        if deep {
            stack.extend(n.children.iter().rev());
        }
    }
    None
}

/// True if surface node `root` is a popup that opened from another popup
/// (a submenu).
fn nested_popup(tree: &SceneTree, root: NodeId) -> bool {
    let Some(n) = tree.get(root).filter(|n| n.kind == NodeKind::Popup) else {
        return false;
    };
    let mut at = n.parent;
    while let Some(p) = at {
        let Some(pn) = tree.get(p) else {
            return false;
        };
        if pn.kind == NodeKind::Popup {
            return true;
        }
        at = pn.parent;
    }
    false
}

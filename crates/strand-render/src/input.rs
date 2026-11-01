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

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;
use strand_scene::time::Instant;

use strand_scene::input::button;
use strand_scene::{
    AxisSource, ButtonState, ClipboardAsk, InputEvent, KeyInput, ListAsk, LogicalPoint,
    LogicalRect, Modifiers, NodeAsk, NodeId, NodeKind, Prop, PropValue, SceneDiff, SceneOp,
    SurfaceId,
};

use crate::renderer::{Renderer, ScrollInput, ScrollKind, ScrollMove};
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
    /// (keyboard) Return or space on a focused node: its own `on activate`
    /// if it has one, else `on click`, which only logic can tell.
    Press,
    /// A key pressed (or repeating) while the node has focus (`on
    /// key(k)`), which only observes.
    Key {
        name: String,
        text: String,
        modifiers: Modifiers,
        repeat: bool,
    },
    /// (keyboard) A key let go while the node has focus (`on
    /// key_release(k)`).
    KeyRelease {
        name: String,
        text: String,
        modifiers: Modifiers,
    },
    /// (keyboard) A key this node claimed (`on key "ctrl+n" { … }`): for
    /// its claim numbered `handler` ([`strand_scene::KeyClaim`]) alone, on
    /// no other handler. `key` is all of it, for `k` and for `propagate()`
    /// to match the next claim out.
    Claim { handler: u32, key: KeyInput },
    /// Escape, a click away or focus loss closed a popup (`on dismiss`).
    Dismiss,
    /// (M4) Something was dropped on the node (`on drop(value, at)`):
    /// a `drag:` source (by node) or another program's files, app or
    /// text; `at` is the global row index it landed at (see
    /// [`Router::handle`]).
    Drop {
        payload: strand_scene::DropPayload,
        at: u32,
        /// (M6) Rows `at` could not see: see [`HiddenRows`].
        hidden: Option<HiddenRows>,
    },
}

/// (M6) The rows of a windowed `list` before its mounted window, which a
/// drop's `at` counted by their number but could not look into (render
/// has only the mounted rows): sent when the drop is a `drag:` source of
/// another list and no copy of the dragged item was left out before the
/// pointer among the mounted rows. Logic, which holds every item, leaves
/// out the first copy among the first `before` items of `list`, if there
/// is one (`at` one less; `Instance::drop_at`), so `at` counts every
/// row as spec.md 6.7 says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HiddenRows {
    /// The windowed `list` whose rows `at` counts.
    pub list: NodeId,
    /// Its rows before the mounted window (`Prop::RowFirst` as render
    /// held it).
    pub before: u32,
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
    /// (keyboard) `list`'s selection is at the row with this global index,
    /// or nowhere (`results.selected_index`).
    SelectedIndex { list: NodeId, index: Option<u32> },
    /// (M6) For the host, not logic: a focused `input` copies to or
    /// pastes from the compositor's clipboard (Ctrl+C/X/V). A paste comes
    /// back as [`InputEvent::Paste`].
    Clipboard(ClipboardAsk),
}

/// (report) What kind of `input` has keyboard focus
/// ([`Router::focused_input`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Text,
    /// `type: password`.
    Password,
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
    /// (still-pointer) `node`'s hit box on `surface` as its last frame drew
    /// it, to tell whether it moved.
    fn hit_box(&self, surface: SurfaceId, node: NodeId) -> Option<crate::flatten::HitBox> {
        let _ = (surface, node);
        None
    }
    /// Scrolls list `list` so `row` is in view.
    fn reveal(&mut self, list: NodeId, row: NodeId) {
        let _ = (list, row);
    }
    /// (keyboard) Moves `scroll` or `list` `node`, or reveals it
    /// ([`Renderer::scroll_move`]).
    fn scroll_move(&mut self, node: NodeId, mv: ScrollMove) {
        let _ = (node, mv);
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
    /// (integration-r8) True if surface node `root` is an open `popup`
    /// whose `open` is two-way and that the pointer does not open and
    /// close itself (no `trigger: hover`): a press on another Strand
    /// surface, which the compositor gives to Strand without ending a
    /// popup grab (xdg-shell ends one only for another client's
    /// surface), is a click away from it.
    fn popup_open(&self, root: NodeId) -> bool {
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
    /// (m6-owner-fixes) `at` moved into `node`'s laid-out box, undoing
    /// the paint offsets and transforms it is drawn with (see
    /// [`Renderer::to_laid`]); by default `at`.
    fn to_laid(&self, surface: SurfaceId, node: NodeId, at: LogicalPoint) -> LogicalPoint {
        let _ = (surface, node);
        at
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
    /// (M4) Drag and drop: `node` (a `drag:` source) is drawn `offset`
    /// away from its box, above its siblings, following the pointer; or,
    /// `None`, it springs back to its box (or to the place a drop moved
    /// it to). See [`Renderer::lift`].
    fn lift(&mut self, node: NodeId, offset: Option<LogicalPoint>) {
        let _ = (node, offset);
    }
    /// (M4) [`InputScene::hit`] past `skip` and its subtree (a dragged
    /// source drawn under the pointer). By default `hit`.
    fn hit_under(&self, surface: SurfaceId, at: LogicalPoint, skip: NodeId) -> Vec<NodeId> {
        let _ = skip;
        self.hit(surface, at)
    }
}

impl InputScene for Renderer {
    fn hit(&self, surface: SurfaceId, at: LogicalPoint) -> Vec<NodeId> {
        Renderer::hit(self, surface, at)
    }
    fn hit_box(&self, surface: SurfaceId, node: NodeId) -> Option<crate::flatten::HitBox> {
        let (_, _, hits) = self.painted_hits(surface)?;
        hits.iter().rev().find(|h| h.node == node).cloned()
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
        Renderer::scroll_move(self, list, ScrollMove::RevealRow(row));
    }
    fn scroll_move(&mut self, node: NodeId, mv: ScrollMove) {
        Renderer::scroll_move(self, node, mv);
    }
    fn reveal_index(&mut self, list: NodeId, index: u32) {
        Renderer::scroll_move(self, list, ScrollMove::RevealIndex(index));
    }
    fn rows_in_view(&self, list: NodeId) -> Option<u32> {
        Renderer::rows_in_view(self, list)
    }
    fn exclusive_open(&self, root: NodeId) -> bool {
        self.surface_spec(root).is_some_and(|s| {
            s.open && s.open_two_way && s.keyboard == strand_scene::Keyboard::Exclusive
        })
    }
    fn popup_open(&self, root: NodeId) -> bool {
        self.surface_spec(root).is_some_and(|s| {
            s.kind == NodeKind::Popup && s.open && s.open_two_way && !s.hover_trigger
        })
    }
    fn set_flag(&mut self, node: NodeId, flag: Flag, on: bool) {
        self.set_widget_flag(node, flag, on);
    }
    fn node_rect(&self, surface: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        Renderer::node_rect(self, surface, node)
    }
    fn to_laid(&self, surface: SurfaceId, node: NodeId, at: LogicalPoint) -> LogicalPoint {
        Renderer::to_laid(self, surface, node, at)
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
    fn lift(&mut self, node: NodeId, offset: Option<LogicalPoint>) {
        Renderer::lift(self, node, offset);
    }
    fn hit_under(&self, surface: SurfaceId, at: LogicalPoint, skip: NodeId) -> Vec<NodeId> {
        Renderer::hit_under(self, surface, at, skip)
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

/// (M4) How far (logical pixels) the pointer moves with the left button
/// held on a `drag:` source before it is a drag rather than a press.
pub const DRAG_THRESHOLD: f32 = 6.0;

/// (M4) A drag in flight, as other streams read it ([`Router::drag`]):
/// `jelly` and list reordering. A press on a `drag:` source is a drag
/// once the pointer has moved [`DRAG_THRESHOLD`] from it.
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

/// (M4) A press on a `drag:` source, and the drag it becomes.
#[derive(Clone, Debug)]
struct Drag {
    source: NodeId,
    /// The dragged value's type (`Prop::Drag`): what targets accept.
    kind: String,
    /// The surface it was pressed on (it is drawn there) and where.
    origin: SurfaceId,
    start: LogicalPoint,
    /// Where the pointer is now, on `surface`.
    surface: SurfaceId,
    pointer: LogicalPoint,
    /// Logical pixels per second, and the last motion's time (ms).
    velocity: LogicalPoint,
    time: Option<u32>,
    /// Past the threshold: a drag, not a press.
    active: bool,
    /// The compositor carries it (`wl_data_device`): the pointer left the
    /// origin surface, and it moves by `Drag*` events now.
    handed: bool,
    /// Where it would land.
    target: Option<(NodeId, u32)>,
}

/// (M4) Another program's drag over one of our surfaces.
#[derive(Clone, Debug)]
struct Offer {
    surface: SurfaceId,
    target: Option<(NodeId, u32)>,
}

/// (M4) What a drag carries, as targets accept it.
#[derive(Clone, Copy, Debug)]
enum Carried<'a> {
    /// A `drag:` source's value of this type (`Prop::Drag`), and the
    /// source itself (never its own target).
    Value(&'a str, NodeId),
    /// Another program's files, app or text (`on drop(d: Drop, …)`).
    Outside,
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

/// (still-pointer) How long hover has to have made no move under a still pointer
/// for the surface to count as having come to rest.
const STILL_REST: std::time::Duration = std::time::Duration::from_secs(1);

/// (still-pointer) How many times hover may make one move, from one node to
/// another, under a still pointer before it stays where it is.
const STILL_MOVES: u8 = 2;

/// (still-pointer) Where hover has gone under a pointer that stays still,
/// since it last moved, took other input, or the surface rested. Hover
/// follows anything once or twice: rows passing, a knob that slid away and
/// comes back; a move made a third time is a node moving itself off the
/// pointer when hovered, and refusing it lets its surface rest.
#[derive(Debug)]
struct StillPointer {
    at: LogicalPoint,
    /// When hover last moved, or a move was refused with its two nodes in
    /// motion.
    moved: Instant,
    /// The moves hover made, by innermost node, and how many times.
    moves: HashMap<(Option<NodeId>, NodeId), u8>,
    /// The move last refused, with its two nodes' boxes then: while neither
    /// has moved, frames of anything else on the surface change nothing.
    refused: Option<StillRefusal>,
}

/// (still-pointer) A move of hover and its two nodes' hit boxes.
type StillRefusal = (
    (Option<NodeId>, NodeId),
    (
        Option<crate::flatten::HitBox>,
        Option<crate::flatten::HitBox>,
    ),
);

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
    /// (keyboard) What logic's last diff asked of nodes (`results.next()`,
    /// `node.focus()`), in order, until it is applied ([`Router::settle`]).
    asks: Vec<(NodeId, NodeAsk)>,
    /// (keyboard) The surfaces that hold the keyboard, whether or not the
    /// node focused on one is still there.
    keyboard: HashSet<SurfaceId>,
    /// (keyboard) The node `node.focus()` named on a surface (by its root)
    /// that did not hold the keyboard: it takes focus when the keyboard
    /// comes.
    focus_wanted: HashMap<NodeId, NodeId>,
    /// (keyboard) The `value`s keys wrote to a slider or `segmented` that
    /// logic has not answered: the next key builds on the last.
    value_flights: HashMap<NodeId, ValueFlight>,
    /// (keyboard) A surface's focus that left a node no longer shown or
    /// hittable, and the node it went to: it comes back when the first shows
    /// again, unless focus moved on meanwhile.
    focus_lost: HashMap<SurfaceId, (NodeId, NodeId)>,
    /// (keyboard) Each list's selection as logic last heard it
    /// ([`Intent::SelectedIndex`]).
    told_index: HashMap<NodeId, u32>,
    /// The last pointer position on each surface the pointer is over.
    pointer: HashMap<SurfaceId, LogicalPoint>,
    /// (still-pointer) Where hover went under a pointer that stays still.
    still: HashMap<SurfaceId, StillPointer>,
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
    /// (lockmon) Each `input` with twins (`Prop::Twin`: the lock's field
    /// on every monitor) and their shared key.
    twins: HashMap<NodeId, String>,
    /// (oi-lock) The twin key of the input that had keyboard focus last,
    /// when it has twins: the keyboard entering another monitor's lock
    /// focuses that monitor's twin, so typing carries on in the field the
    /// user was typing in (focused by a click or Tab, not `focus: true`).
    /// Forgotten once focus moves to anything else.
    focused_twin: Option<String>,
    /// The slider being dragged on each surface (the left button went
    /// down on it).
    dragging: HashMap<SurfaceId, NodeId>,
    /// (flex) Whether each slider being dragged runs upward, as it did
    /// when the press began: a drag does not change axis mid-way.
    drag_upright: HashMap<NodeId, bool>,
    /// The `input` whose text a held left button selects, per surface.
    selecting: HashMap<SurfaceId, NodeId>,
    /// (M4) A press on a `drag:` source, or the drag it became.
    drag: Option<Drag>,
    /// (M4) A drag just ended (dropped or cancelled): the left release
    /// that ends it clicks nothing.
    dragged: bool,
    /// (M4) Another program's drag over one of our surfaces.
    offer: Option<Offer>,
    /// Intents of the event being handled.
    out: Vec<Intent>,
    /// (M5) Pick mode: pointer input goes to the inspector, not to
    /// handlers ([`Router::set_picking`]).
    picking: bool,
    /// (M6) The `trigger: hover` popups the router opens and closes.
    hover_popups: HoverPopups,
    /// (M6) The `input` each surface asked the host to paste into
    /// ([`ClipboardAsk::Paste`]), until the paste arrives.
    pending_paste: HashMap<SurfaceId, NodeId>,
    /// (flex) Nodes whose `focus:` logic's last diff turned true: focus
    /// moves to each when routing settles, on a surface that holds the
    /// keyboard.
    focus_turned: Vec<NodeId>,
    /// The time the router reads in place of the wall clock
    /// ([`Router::set_now`]): a host on a virtual clock's.
    now: Option<Instant>,
}

/// (keyboard) Keys' writes to a widget's `value` on their way through logic.
#[derive(Debug)]
struct ValueFlight {
    /// The last written.
    latest: PropValue,
    /// Those written before it, oldest first.
    pending: Vec<PropValue>,
    at: Instant,
}

/// (keyboard) How far an arrow key moves a focused `scroll`, logical pixels.
const KEY_SCROLL_STEP: f32 = 40.0;

/// (keyboard) The fraction of its range an arrow key moves a `slider` with no
/// `step:`.
const KEY_SLIDER_STEP: f32 = 0.01;

/// (M6) How long a `trigger: hover` popup stays open after the pointer
/// leaves both it and the element it is anchored to (spec.md 6.3): the
/// pointer crosses the gap between a bar item and its island without
/// the island closing on the way. Quickshell configs use the same
/// 200 ms (decisions.md m6-w2-surface).
pub const HOVER_GRACE: Duration = Duration::from_millis(200);

/// (M6) The `trigger: hover` popups the router opened (spec.md 6.3): it
/// writes their `open` as the pointer comes and goes.
#[derive(Debug, Default)]
struct HoverPopups {
    /// Each open one, and its grace: `None` while the pointer is on it
    /// or its anchor, else when it closes unless the pointer comes back
    /// (`Some(None)`: a grace too long to end).
    open: HashMap<NodeId, Option<Option<Instant>>>,
    /// Each waiting for its `delay` while the pointer rests on its
    /// anchor, and when it opens (`None`: a delay too long to end).
    pending: HashMap<NodeId, Option<Instant>>,
    /// The values of `open` written to each and not yet seen in logic's
    /// diffs ([`Router::observe`]), oldest first.
    written: HashMap<NodeId, VecDeque<bool>>,
    /// Closed by the compositor (`popup_done`) or a click away while the
    /// pointer was on its anchor: it opens again only once the pointer
    /// has left that anchor.
    dismissed: HashSet<NodeId>,
    /// (m6-owner-fixes) Those whose `open:` the program holds true
    /// (`Prop::Hold`): open, and kept open, whatever the pointer does.
    held: HashSet<NodeId>,
}

/// (M6) Whether `node` is a `trigger: hover` popup (never a tooltip's).
fn is_hover_popup(tree: &SceneTree, node: NodeId) -> bool {
    !tree.is_overlay(node)
        && tree.get(node).is_some_and(|n| {
            n.kind == NodeKind::Popup
                && matches!(n.get(Prop::Trigger), Some(PropValue::Keyword(k)) if k == "hover")
        })
}

/// (M6) `node`'s tree ancestors, nearest first: for a popup, the element
/// it is anchored to and up, across the popups it nests in.
fn ancestors(tree: &SceneTree, node: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut up = tree.get(node).and_then(|n| n.parent);
    while let Some(p) = up {
        if out.len() >= 256 || out.contains(&p) {
            break;
        }
        out.push(p);
        up = tree.get(p).and_then(|n| n.parent);
    }
    out
}

/// (M6) The surface a popup opens from: the nearest surface node above
/// it (a bar, a panel, the popup it nests in). One `trigger: hover`
/// popup is open from each at a time.
fn opened_from(tree: &SceneTree, popup: NodeId) -> Option<NodeId> {
    ancestors(tree, popup)
        .into_iter()
        .find(|a| tree.get(*a).is_some_and(|n| n.kind.is_surface()))
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
    ///
    /// (keyboard) What it asks of lists (`results.next()`) waits for it to
    /// be applied, and [`Router::settle`] does each in turn.
    pub fn observe(&mut self, diff: &SceneDiff) {
        self.asks.extend_from_slice(&diff.asks);
        // (flex) `focus:` turned true: focus moves there (`settle`).
        for op in &diff.ops {
            if let SceneOp::SetProp {
                id,
                prop: Prop::Focus,
                value: PropValue::Bool(true),
                ..
            } = op
            {
                self.focus_turned.push(*id);
            }
        }
        // (M6) `open` on a `trigger: hover` popup: the router's own
        // writes coming back, in order, or logic's own value (a reload
        // remounted it open or closed), which the router takes on.
        for op in &diff.ops {
            let SceneOp::SetProp {
                id,
                prop: Prop::Open,
                value: PropValue::Bool(on),
                ..
            } = op
            else {
                continue;
            };
            let h = &mut self.hover_popups;
            let answer = h
                .written
                .get(id)
                .and_then(|w| w.iter().position(|v| v == on));
            match (answer, h.written.get_mut(id)) {
                (Some(i), Some(w)) => {
                    w.drain(..=i);
                    if w.is_empty() {
                        h.written.remove(id);
                    }
                }
                _ => {
                    h.written.remove(id);
                    if *on {
                        h.open.entry(*id).or_insert(None);
                    } else {
                        h.open.remove(id);
                    }
                }
            }
        }
        // (m6-owner-fixes) `open:` on a `trigger: hover` popup.
        for op in &diff.ops {
            if let SceneOp::SetProp {
                id,
                prop: Prop::Hold,
                value,
                ..
            } = op
            {
                if *value == PropValue::Bool(true) {
                    self.hover_popups.held.insert(*id);
                } else {
                    self.hover_popups.held.remove(id);
                }
            }
        }
        // (lockmon) Inputs that share one text with their twins.
        for op in &diff.ops {
            if let SceneOp::SetProp {
                id,
                prop: Prop::Twin,
                value,
                ..
            } = op
            {
                match value {
                    PropValue::Text(k) => {
                        self.twins.insert(*id, k.clone());
                    }
                    _ => {
                        self.twins.remove(id);
                    }
                }
            }
        }
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
        // (keyboard) A `value` a key wrote coming back, or logic's own.
        if !self.value_flights.is_empty() {
            for op in &diff.ops {
                let SceneOp::SetProp {
                    id,
                    prop: Prop::Value,
                    value,
                    ..
                } = op
                else {
                    continue;
                };
                let Some(f) = self.value_flights.get_mut(id) else {
                    continue;
                };
                match f.pending.iter().position(|p| p == value) {
                    Some(i) if *value != f.latest => {
                        f.pending.drain(..=i);
                    }
                    _ => {
                        self.value_flights.remove(id);
                    }
                }
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
        self.keyboard.remove(&surface);
        self.surfaces.remove(&surface);
        self.hovered.remove(&surface);
        self.still.remove(&surface);
        // A press held on it ends with it: its chain is let go (heard at
        // the next flush), or the renderer would keep a popup's root
        // pressed, and its whole box as its input region, into the next
        // time it opens (m6-owner-fixes review, bug 15).
        self.release(surface);
        self.right_down.remove(&surface);
        self.middle_down.remove(&surface);
        // (flex) Its focus is let go as a keyboard leave lets it go: the
        // node is told it lost focus, or it would keep its caret into
        // the next time its surface opens, beside the one focus gives
        // then (two carets after a reopen).
        self.set_focus(surface, None);
        self.pending_paste.remove(&surface);
        self.pointer.remove(&surface);
        if self.drag.as_ref().is_some_and(|d| d.origin == surface) {
            self.drag = None;
        }
        if self.offer.as_ref().is_some_and(|o| o.surface == surface) {
            self.offer = None;
        }
    }

    /// (m6-owner-fixes) Whether a button that went down on `surface` is
    /// still held (the compositor keeps the pointer there until it is
    /// let go).
    pub fn held(&self, surface: SurfaceId) -> bool {
        self.pressed.contains_key(&surface)
            || self.right_down.contains_key(&surface)
            || self.middle_down.contains_key(&surface)
    }

    /// (flex) Whether the left button went down on `surface` and is still
    /// held: its hovered chain is latched to the one it went down on.
    pub fn left_held(&self, surface: SurfaceId) -> bool {
        self.pressed.contains_key(&surface)
    }

    /// (M4) The last pointer position on `surface`, while the pointer is
    /// over it (`parallax` and `tilt` read it when they flatten).
    pub fn pointer(&self, surface: SurfaceId) -> Option<LogicalPoint> {
        self.pointer.get(&surface).copied()
    }

    /// (M4) The drag in flight: a press on a `drag:` source that moved
    /// [`DRAG_THRESHOLD`], until it is dropped or cancelled.
    pub fn drag(&self) -> Option<DragView> {
        let d = self.drag.as_ref().filter(|d| d.active)?;
        Some(DragView {
            source: d.source,
            surface: d.surface,
            pointer: d.pointer,
            velocity: d.velocity,
            target: d.target.map(|t| t.0),
            index: d.target.map(|t| t.1),
        })
    }

    /// (M4) The node a drop on `surface` would land on now (a drag of
    /// ours or another program's), if any accepts it: the surface
    /// manager accepts the compositor's offer only then.
    pub fn drop_target(&self, surface: SurfaceId) -> Option<NodeId> {
        if let Some(d) = self
            .drag
            .as_ref()
            .filter(|d| d.active && d.surface == surface)
        {
            return d.target.map(|t| t.0);
        }
        self.offer
            .as_ref()
            .filter(|o| o.surface == surface)
            .and_then(|o| o.target.map(|t| t.0))
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

    /// (report) The `input` with keyboard focus on `surface`, if one
    /// has it, and whether it is a password input (`type: password`), as
    /// `tree` holds it: a glitch report keeps no count of a password's
    /// keys (docs/architecture.md, "Glitch reports"). The router holds
    /// no tree of its own, so the caller passes the renderer's.
    pub fn focused_input(&self, surface: SurfaceId, tree: &SceneTree) -> Option<InputKind> {
        let node = tree.get(self.focused(surface)?)?;
        if node.kind != NodeKind::Input {
            return None;
        }
        let password =
            matches!(node.get(Prop::InputType), Some(PropValue::Keyword(k)) if k == "password");
        Some(if password {
            InputKind::Password
        } else {
            InputKind::Text
        })
    }

    /// The selected row of `list`.
    pub fn selected(&self, list: NodeId) -> Option<NodeId> {
        self.selected.get(&list).copied()
    }

    /// The hovered chain of `surface`, innermost first.
    pub fn hovered(&self, surface: SurfaceId) -> &[NodeId] {
        self.hovered.get(&surface).map_or(&[], Vec::as_slice)
    }

    /// (M5) Pick mode on or off (the inspector, `inspect.pick`). While
    /// it is on, pointer input (motion, buttons, scrolls, a click away)
    /// reaches no handler: the host gives it to
    /// [`Renderer::pick_input`] instead; the keyboard routes as before
    /// and no surface's keyboard interactivity changes. Turning it on
    /// ends every hover and press (their flags go off), a slider drag, a
    /// selection by pointer and a drag in flight; turning it off
    /// rebuilds the hover from where the pointer is (a hit test), so a
    /// `trigger: hover` popup under it stays open. Returns what logic
    /// hears of it.
    pub fn set_picking(&mut self, on: bool, scene: &mut dyn InputScene) -> Vec<Intent> {
        if self.picking != on {
            self.picking = on;
            if on {
                let surfaces: Vec<SurfaceId> = self.surfaces.keys().copied().collect();
                for s in surfaces {
                    self.hover(s, Vec::new());
                    self.release(s);
                    self.end_drags(scene, s);
                    self.right_down.remove(&s);
                    self.middle_down.remove(&s);
                }
                self.cancel_drag(scene);
                // Another program's drag keeps its surface, with no
                // target while picking (inspect-r2-4).
                if let Some(o) = &mut self.offer {
                    o.target = None;
                }
            } else {
                // (M6) The pointer's hover comes back at once, so a
                // `trigger: hover` popup under it is not closed by the
                // flush below as if the pointer had gone.
                let at: Vec<(SurfaceId, LogicalPoint)> =
                    self.pointer.iter().map(|(s, p)| (*s, *p)).collect();
                for (s, p) in at {
                    let Some(&root) = self.surfaces.get(&s) else {
                        continue;
                    };
                    let c = scene.hit(s, p);
                    self.hover(s, if c.is_empty() { vec![root] } else { c });
                }
            }
        }
        self.flush(scene)
    }

    /// (M6) Opens and closes the `trigger: hover` popups whose time came
    /// (a `delay`, the [`HOVER_GRACE`]); returns what logic hears of it.
    /// The host calls it when [`Router::next_wake`] comes, and after a
    /// surface detaches.
    pub fn tick(&mut self, scene: &mut dyn InputScene) -> Vec<Intent> {
        self.flush(scene)
    }

    /// Reads `now` in place of the wall clock (`None`: the wall clock
    /// again): hover delays and grace run on the time of a host that
    /// runs on a virtual clock, the evaluation sandbox's and the web
    /// host's (docs/decisions.md eval-sandbox, live-examples).
    pub fn set_now(&mut self, now: Option<Instant>) {
        self.now = now;
    }

    /// The time hover delays, grace and edits are measured by.
    fn now(&self) -> Instant {
        if let Some(now) = self.now {
            return now;
        }
        Instant::now()
    }

    /// (M6) When [`Router::tick`] must run: the earliest `delay` or
    /// grace of a `trigger: hover` popup to end.
    pub fn next_wake(&self) -> Option<Instant> {
        let h = &self.hover_popups;
        h.open
            .values()
            .flatten()
            .chain(h.pending.values())
            .flatten()
            .min()
            .copied()
    }

    /// (M6) The `trigger: hover` popups follow the pointer (spec.md 6.3).
    /// One opens while the pointer is on the element it is anchored to
    /// (the innermost such element, after its `delay`), closing at once
    /// any other opened from the same surface. An open one stays while
    /// the pointer is on that element or on anything above it in the
    /// tree (the popup, a popup opened from it), or while a press that
    /// went down there is held (a slider's drag), and for
    /// [`HOVER_GRACE`] after; then it closes, with the popups opened from
    /// it first. In pick mode nothing opens and the graces end.
    fn hover_popups(&mut self, tree: Option<&SceneTree>, now: Instant) {
        let Some(tree) = tree else {
            return;
        };
        // Only live `trigger: hover` popups are the router's (a reload
        // may have made one the program's).
        let ours = |n: &NodeId| tree.contains_live(*n) && is_hover_popup(tree, *n);
        let h = &mut self.hover_popups;
        h.open.retain(|n, _| ours(n));
        h.pending.retain(|n, _| ours(n));
        h.written.retain(|n, _| tree.contains_live(*n));
        h.dismissed.retain(|n| ours(n));
        h.held.retain(|n| ours(n));
        if self.picking {
            let graced: Vec<NodeId> = self
                .hover_popups
                .open
                .iter()
                .filter(|(_, due)| due.is_some())
                .map(|(n, _)| *n)
                .collect();
            for p in graced {
                self.close_hover_popup(tree, p);
            }
            self.hover_popups.pending.clear();
            return;
        }
        // Where the pointer is: each hovered chain (a held press keeps
        // its chain hovered), and from a popup's root on up through the
        // element it is anchored to.
        let mut under: HashSet<NodeId> = HashSet::new();
        for chain in self.hovered.values() {
            under.extend(chain.iter().copied());
            if let Some(&root) = chain.last() {
                under.extend(ancestors(tree, root));
            }
        }
        // What the pointer opens: the innermost hovered element's
        // `trigger: hover` popup, one for each surface it opens from.
        let mut want: HashMap<NodeId, NodeId> = HashMap::new();
        for chain in self.hovered.values() {
            for &n in chain {
                let Some(node) = tree.get(n) else {
                    continue;
                };
                for &c in &node.children {
                    if is_hover_popup(tree, c)
                        && let Some(from) = opened_from(tree, c)
                    {
                        want.entry(from).or_insert(c);
                    }
                }
            }
        }
        // (m6-owner-fixes) A popup the program holds open opens at once,
        // before what the pointer opens from the same surface.
        let mut held: Vec<NodeId> = self.hover_popups.held.iter().copied().collect();
        held.sort();
        for p in held {
            if let Some(from) = opened_from(tree, p) {
                want.insert(from, p);
            }
        }
        let held = &self.hover_popups.held;
        let wanted = |p: &NodeId| want.values().any(|w| w == p);
        self.hover_popups.pending.retain(|p, _| wanted(p));
        self.hover_popups.dismissed.retain(|p| wanted(p));
        let mut opening = Vec::new();
        for (&from, &p) in &want {
            if self.hover_popups.open.contains_key(&p)
                || (self.hover_popups.dismissed.contains(&p) && !held.contains(&p))
            {
                continue;
            }
            let delay = match tree.get(p).and_then(|n| n.get(Prop::Delay)) {
                Some(PropValue::Duration(d)) if !held.contains(&p) => *d,
                _ => Duration::ZERO,
            };
            if !delay.is_zero() {
                let due = *self
                    .hover_popups
                    .pending
                    .entry(p)
                    .or_insert(now.checked_add(delay));
                if due.is_none_or(|d| d > now) {
                    continue;
                }
            }
            self.hover_popups.pending.remove(&p);
            opening.push((from, p));
        }
        for (from, p) in opening {
            let others: Vec<NodeId> = self
                .hover_popups
                .open
                .keys()
                .copied()
                .filter(|q| *q != p && opened_from(tree, *q) == Some(from))
                .collect();
            for q in others {
                self.close_hover_popup(tree, q);
            }
            self.hover_popups.open.insert(p, None);
            self.write_open(p, true);
        }
        let open: Vec<NodeId> = self.hover_popups.open.keys().copied().collect();
        for p in open {
            let anchor = tree.get(p).and_then(|n| n.parent);
            let Some(due) = self.hover_popups.open.get_mut(&p) else {
                continue; // closed with the popup it was opened from
            };
            if anchor.is_some_and(|a| under.contains(&a)) || self.hover_popups.held.contains(&p) {
                *due = None;
                continue;
            }
            let grace = match tree.get(p).and_then(|n| n.get(Prop::Grace)) {
                Some(PropValue::Duration(d)) => *d,
                _ => HOVER_GRACE,
            };
            if due
                .get_or_insert(now.checked_add(grace))
                .is_some_and(|d| d <= now)
            {
                self.close_hover_popup(tree, p);
            }
        }
    }

    /// (M6) Closes `popup`, a `trigger: hover` popup the router opened,
    /// and before it those opened from inside it, innermost first.
    fn close_hover_popup(&mut self, tree: &SceneTree, popup: NodeId) {
        let mut inner: Vec<(usize, NodeId)> = self
            .hover_popups
            .open
            .keys()
            .filter(|q| **q != popup)
            .filter_map(|q| {
                let up = ancestors(tree, *q);
                up.contains(&popup).then_some((up.len(), *q))
            })
            .collect();
        inner.sort_by_key(|q| std::cmp::Reverse(q.0));
        for p in inner.into_iter().map(|(_, q)| q).chain([popup]) {
            self.hover_popups.open.remove(&p);
            self.write_open(p, false);
        }
    }

    /// (M6) Writes `open` of a `trigger: hover` popup, remembering it
    /// until logic's diff shows it ([`Router::observe`]).
    fn write_open(&mut self, popup: NodeId, on: bool) {
        let w = self.hover_popups.written.entry(popup).or_default();
        w.push_back(on);
        // Logic answers every write; a bound keeps a stalled logic thread
        // from growing it.
        if w.len() > 64 {
            w.pop_front();
        }
        self.out.push(Intent::Write {
            node: popup,
            prop: Prop::Open,
            value: PropValue::Bool(on),
        });
    }

    /// (M6) The compositor closed `popup`'s surface (`popup_done`), or a
    /// click went past it: a `trigger: hover` popup the router opened
    /// closes (with those opened from it), and opens again only once the
    /// pointer has left its anchor.
    fn hover_popup_dismissed(&mut self, tree: Option<&SceneTree>, popup: NodeId) {
        let Some(tree) = tree else {
            return;
        };
        if !self.hover_popups.open.contains_key(&popup) {
            return;
        }
        self.close_hover_popup(tree, popup);
        self.hover_popups.dismissed.insert(popup);
    }

    /// (M5) Whether pick mode is on ([`Router::set_picking`]).
    pub fn picking(&self) -> bool {
        self.picking
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
        // (still-pointer) A wheel, a button or a key moves what is under
        // the pointer on purpose. A pointer's own motion is seen by where
        // it is (`repoint`).
        if !matches!(
            event,
            InputEvent::PointerEnter { .. }
                | InputEvent::PointerLeave { .. }
                | InputEvent::PointerMotion { .. }
        ) {
            self.still.clear();
        }
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
    ///
    /// (keyboard) Then what the diff asked of lists, in order, each from
    /// where the one before left the selection: after the first-row rule,
    /// so `query = q; results.select(2)` ends on row 2 of what is mounted.
    /// So are `node.focus()`, `node.reveal()` and a scroll's position, in
    /// the same order: what asks after a `focus()` finds it there.
    pub fn settle(&mut self, scene: &mut dyn InputScene) -> Vec<Intent> {
        self.prune(scene.tree());
        self.refocus(scene);
        self.land(scene);
        self.select_first_rows(scene);
        let mut refocused = false;
        for (node, ask) in std::mem::take(&mut self.asks) {
            match ask {
                NodeAsk::List(ask) => self.list_ask(scene, node, ask),
                NodeAsk::Focus => {
                    self.focus_ask(scene, node);
                    refocused = true;
                }
                NodeAsk::Reveal => scene.scroll_move(node, ScrollMove::Reveal),
                NodeAsk::ScrollTo { y, x } => scene.scroll_move(node, ScrollMove::To { y, x }),
                NodeAsk::ScrollBy { dy, dx } => scene.scroll_move(node, ScrollMove::By { dy, dx }),
            }
        }
        self.focus_follows_shown(&*scene);
        // An `input` a `focus()` reached starts its `nav` list.
        if refocused {
            self.select_first_rows(scene);
        }
        self.index_selections(scene.tree());
        self.flush(scene)
    }

    /// (still-pointer) What is under the still pointer on `surface` may
    /// have changed (a scrolled list, a row gone, an animation): its hover
    /// follows the frame just painted, as if the pointer had moved. A held
    /// press keeps the chain it went down on, and pick mode's pointer is
    /// the inspector's. Returns what logic hears of it, nothing when the
    /// same chain is under it.
    ///
    /// Under a still pointer hover makes one move at most [`STILL_MOVES`]
    /// times, until the pointer moves, other input comes or the surface
    /// rests ([`STILL_REST`]): a node that moves itself off the pointer when
    /// hovered flips a few times and stays, so its surface comes to rest
    /// (spec.md 6.3).
    pub fn repoint(&mut self, surface: SurfaceId, scene: &mut dyn InputScene) -> Vec<Intent> {
        let (Some(&root), Some(&at)) = (self.surfaces.get(&surface), self.pointer.get(&surface))
        else {
            return Vec::new();
        };
        if self.picking || self.pressed.contains_key(&surface) {
            return Vec::new();
        }
        let t = self.now();
        let still = self.still.entry(surface).or_insert_with(|| StillPointer {
            at,
            moved: t,
            moves: HashMap::new(),
            refused: None,
        });
        if still.at != at {
            still.at = at;
            still.moved = t;
            still.moves.clear();
            still.refused = None;
        }
        let hit = scene.hit(surface, at);
        let now = if hit.is_empty() { vec![root] } else { hit };
        if self.hovered(surface) == now.as_slice() {
            return Vec::new();
        }
        let from = self.hovered(surface).first().copied();
        // A node gone keeps nothing.
        let live = from.is_some_and(|n| scene.tree().is_some_and(|t| t.contains_live(n)));
        let pair = (from, now[0]);
        let boxes = (
            from.and_then(|n| scene.hit_box(surface, n)),
            scene.hit_box(surface, now[0]),
        );
        if let Some(still) = self.still.get_mut(&surface) {
            // At rest: what moved is something else on the surface.
            if live
                && still
                    .refused
                    .as_ref()
                    .is_some_and(|r| r.0 == pair && r.1 == boxes)
            {
                return Vec::new();
            }
            if t.saturating_duration_since(still.moved) >= STILL_REST {
                still.moves.clear();
            }
            still.moved = t;
            let made = still.moves.entry(pair).or_default();
            if live && *made >= STILL_MOVES {
                still.refused = Some((pair, boxes));
                return Vec::new();
            }
            *made += 1;
            still.refused = None;
        }
        self.prune(scene.tree());
        self.hover(surface, now);
        self.flush(scene)
    }

    /// (still-pointer) `surface` moved on the screen under a pointer that
    /// did not: where the pointer is on it moves by `(dx, dy)`, whatever the
    /// compositor has said yet, or the next [`Router::repoint`] tests a point
    /// that is somewhere else now. It is not the pointer moving, so the moves
    /// made under it still count. True if a pointer was on it.
    pub fn shift_pointer(&mut self, surface: SurfaceId, dx: f32, dy: f32) -> bool {
        let Some(at) = self.pointer.get_mut(&surface) else {
            return false;
        };
        at.x += dx;
        at.y += dy;
        if let Some(still) = self.still.get_mut(&surface) {
            still.at.x += dx;
            still.at.y += dy;
        }
        true
    }

    /// (flex) Reactive `focus:`: a node whose `focus:` turned true takes
    /// the keyboard focus of its surface, if that surface has the
    /// keyboard (one that has not yet takes its first `focus: true` node
    /// when it enters). The last to turn true on a surface wins.
    fn refocus(&mut self, scene: &mut dyn InputScene) {
        for node in std::mem::take(&mut self.focus_turned) {
            self.focus_holders(scene, node)
                .into_iter()
                .for_each(|s| self.focus_on(scene, s, node));
        }
    }

    /// (keyboard) Focus does not stay on a node no longer shown or hittable: it
    /// moves as Tab would, to the surface on one that is closing, and shows
    /// nothing moving, since a fade is no reason to scroll. When that node
    /// shows again focus comes back, so a launcher opened again mid-fade types
    /// on, its `focus: true` having never turned.
    fn focus_follows_shown(&mut self, scene: &dyn InputScene) {
        let Some(tree) = scene.tree() else {
            return;
        };
        let focus = &self.focus;
        self.focus_lost
            .retain(|s, (lost, placed)| focus.get(s) == Some(placed) && tree.contains_live(*lost));
        let back: Vec<(SurfaceId, NodeId)> = self
            .focus_lost
            .iter()
            .filter(|(s, (lost, _))| {
                self.surfaces
                    .get(s)
                    .is_some_and(|root| holds_focus(tree, *root, *lost))
            })
            .map(|(s, (lost, _))| (*s, *lost))
            .collect();
        for (s, node) in back {
            self.focus_lost.remove(&s);
            self.set_focus(s, Some(node));
        }
        let gone: Vec<(SurfaceId, NodeId, NodeId)> = self
            .focus
            .iter()
            .filter_map(|(s, n)| {
                let root = *self.surfaces.get(s)?;
                (!holds_focus(tree, root, *n)).then_some((*s, root, *n))
            })
            .collect();
        for (s, root, node) in gone {
            let closing = tree
                .get(root)
                .is_some_and(|r| matches!(r.get(Prop::Open), Some(PropValue::Bool(false))));
            let next = (!closing)
                .then(|| next_focusable(tree, root, node, false))
                .flatten()
                .unwrap_or(root);
            let lost = match self.focus_lost.get(&s) {
                Some((lost, placed)) if *placed == node => *lost,
                _ => node,
            };
            self.focus_lost.insert(s, (lost, next));
            self.set_focus(s, Some(next));
        }
    }

    /// (keyboard) The surfaces showing `node` that hold the keyboard.
    fn focus_holders(&self, scene: &dyn InputScene, node: NodeId) -> Vec<SurfaceId> {
        let Some(tree) = scene.tree().filter(|t| t.contains_live(node)) else {
            return Vec::new();
        };
        let Some(root) = tree.root_of(node) else {
            return Vec::new();
        };
        self.surfaces
            .iter()
            .filter(|(s, r)| {
                **r == root && (self.keyboard.contains(s) || self.focus.contains_key(s))
            })
            .map(|(s, _)| *s)
            .collect()
    }

    /// (keyboard) `node.focus()`: focus moves there now on a surface that
    /// holds the keyboard, and when the keyboard comes on one that does not.
    fn focus_ask(&mut self, scene: &mut dyn InputScene, node: NodeId) {
        let holders = self.focus_holders(scene, node);
        if holders.is_empty() {
            let root = scene
                .tree()
                .filter(|t| t.contains_live(node))
                .and_then(|t| t.root_of(node));
            if let Some(root) = root {
                self.focus_wanted.insert(root, node);
            }
        }
        for s in holders {
            self.focus_on(scene, s, node);
        }
    }

    /// (keyboard) Focus moves to `node`, and every `scroll` and `list` around
    /// it shows it. A click uses [`Router::set_focus`] alone: what is under
    /// the pointer stays there.
    fn focus_on(&mut self, scene: &mut dyn InputScene, surface: SurfaceId, node: NodeId) {
        self.set_focus(surface, Some(node));
        scene.scroll_move(node, ScrollMove::Reveal);
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

    /// (keyboard) A handler asked `list` to move its selection (or
    /// activate): as the arrow keys do it, from where it is now, a move in
    /// flight included.
    fn list_ask(&mut self, scene: &mut dyn InputScene, list: NodeId, ask: ListAsk) {
        let Some(tree) = scene.tree() else {
            return;
        };
        let Some(node) = tree.get(list).filter(|n| n.kind == NodeKind::List) else {
            return;
        };
        let shape = ListShape::of(node);
        let w = window_rows(tree, list);
        if w.count == 0 {
            return;
        }
        let cur = self.place_in(list, &w);
        let last = w.count - 1;
        let to = match ask {
            ListAsk::Next => shape.step(cur, w.first, last, true),
            ListAsk::Prev => shape.step(cur, w.first, last, false),
            ListAsk::First => 0,
            ListAsk::Last => last,
            ListAsk::Select(i) => u32::try_from(i.max(0)).unwrap_or(u32::MAX).min(last),
            ListAsk::Activate => {
                self.activate_selected(scene, list, &w);
                return;
            }
        };
        self.moved.insert(list);
        self.go_to(scene, list, &w, to);
    }

    /// The global index `list`'s selection is at or on its way to.
    fn place_in(&self, list: NodeId, w: &WindowRows) -> Option<u32> {
        self.away
            .get(&list)
            .map(|a| a.index)
            .or_else(|| self.selected.get(&list).and_then(|r| w.index_of(*r)))
    }

    /// What Return does to `list`: activates its selected row, or the first
    /// mounted row with none; a selection on its way when it lands.
    fn activate_selected(&mut self, scene: &mut dyn InputScene, list: NodeId, w: &WindowRows) {
        if let Some(a) = self.away.get_mut(&list) {
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
    }

    /// (keyboard) Tells logic of each list whose selection is somewhere new
    /// since it last heard.
    fn tell_indexes(&mut self) {
        let now = |l: &NodeId| match self.away.get(l) {
            Some(a) => Some(a.index),
            None => self
                .selected
                .contains_key(l)
                .then(|| self.sel_index.get(l).copied())
                .flatten(),
        };
        let (told, out) = (&mut self.told_index, &mut self.out);
        told.retain(|l, _| {
            let held = now(l).is_some();
            if !held {
                out.push(Intent::SelectedIndex {
                    list: *l,
                    index: None,
                });
            }
            held
        });
        for l in self.selected.keys().chain(self.away.keys()) {
            let Some(i) = now(l) else { continue };
            if told.insert(*l, i) != Some(i) {
                out.push(Intent::SelectedIndex {
                    list: *l,
                    index: Some(i),
                });
            }
        }
    }

    /// Widgets draw hover, press, focus and selection at once.
    fn flush(&mut self, scene: &mut dyn InputScene) -> Vec<Intent> {
        self.hover_popups(scene.tree(), self.now());
        self.tell_indexes();
        // (window) A window root's `focused` is the compositor's focus
        // (`activated`), which the host tells from its configure: the
        // keyboard's focus moving to a button in it, or to a popup of
        // it, leaves the window focused.
        let tree = scene.tree();
        self.out.retain(|i| {
            !matches!(i, Intent::Flag { node, flag: Flag::Focused, .. } if is_window(tree, *node))
        });
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
        // (M5) Pick mode: the pointer is the inspector's.
        if self.picking {
            match event {
                InputEvent::PointerEnter { position, .. }
                | InputEvent::PointerMotion { position, .. } => {
                    self.pointer.insert(surface, *position);
                    return;
                }
                InputEvent::PointerLeave { .. } => {
                    self.pointer.remove(&surface);
                    return;
                }
                InputEvent::PointerButton { .. } | InputEvent::PointerAxis { .. } => return,
                // (window) The compositor's close of a window is no
                // pick: it closes it, as out of pick mode.
                InputEvent::ClickAway { .. } if !is_window(scene.tree(), root) => return,
                // A drag (another program's) finds no drop target: the
                // surface manager declines its offer and no `on drop`
                // runs. The surface it is over is kept, so its motion
                // finds a target again once pick mode ends
                // (inspect-r2-4).
                InputEvent::DragEnter { .. } | InputEvent::DragMotion { .. } => {
                    self.offer = Some(Offer {
                        surface,
                        target: None,
                    });
                    return;
                }
                InputEvent::DragDrop { .. } => {
                    self.offer = None;
                    return;
                }
                _ => {}
            }
        }
        self.prune(scene.tree());
        let chain = |scene: &dyn InputScene, at: LogicalPoint| {
            let c = scene.hit(surface, at);
            if c.is_empty() { vec![root] } else { c }
        };
        match event {
            InputEvent::PointerEnter { position, .. }
            | InputEvent::PointerMotion { position, .. } => {
                self.pointer.insert(surface, *position);
                if let InputEvent::PointerMotion { time, .. } = event
                    && self.drag_motion(scene, surface, *position, Some(*time))
                {
                    return;
                }
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
                // A drag leaving its surface with the button held is the
                // compositor taking it over (`wl_data_device`): it goes on
                // by `Drag*` events, the source back in its box meanwhile.
                if let Some(d) = self.drag.as_mut().filter(|d| d.surface == surface) {
                    if d.active {
                        d.handed = true;
                        d.target = None;
                        let source = d.source;
                        scene.lift(source, None);
                    } else {
                        self.drag = None;
                    }
                }
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
                    self.click_away_from_others(surface, root, &under, scene);
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
                let lift = (vertical.stop || horizontal.stop) && kind == ScrollKind::Touch;
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
                            dx: 0.0,
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
                // (flex) Sideways motion scrolls a `scroll` along x.
                let input = ScrollInput {
                    dx: dx as f32,
                    dy: dy as f32,
                    kind,
                    time: *time,
                };
                scene.scroll_input(surface, *position, input);
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
                // or a click had moved it. (keyboard) A press that came before
                // the keyboard, on a `button`, `slider` or `segmented`, does not
                // beat a `focus: true`, on a bar that takes it `on_demand`.
                let beaten = !self.keyboard.contains(&surface)
                    && scene.tree().is_some_and(|t| {
                        self.focus.get(&surface).is_some_and(|f| {
                            t.get(*f).is_some_and(|n| !claims_click_focus(n))
                                && first_with_focus(t, root).is_some()
                        })
                    });
                if self.focus.contains_key(&surface) && !beaten {
                    self.keyboard.insert(surface);
                    return;
                }
                self.keyboard.insert(surface);
                // (oi-lock) The keyboard moved here from another
                // monitor's lock: its field's twin here, else the node a
                // `focus()` named while it was away, else the input with
                // `focus: true`, else the surface.
                let twin = self.focused_twin.as_ref().and_then(|k| {
                    let tree = scene.tree()?;
                    let mut here: Vec<NodeId> = self
                        .twins
                        .iter()
                        .filter(|(n, key)| *key == k && tree.root_of(**n) == Some(root))
                        .map(|(n, _)| *n)
                        .collect();
                    here.sort();
                    here.first().copied()
                });
                let wanted = self
                    .focus_wanted
                    .remove(&root)
                    .filter(|n| scene.tree().is_some_and(|t| t.contains_live(*n)));
                let first = twin
                    .or(wanted)
                    .or_else(|| scene.tree().and_then(|t| first_with_focus(t, root)))
                    .unwrap_or(root);
                self.focus_on(scene, surface, first);
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
                self.keyboard.remove(&surface);
                self.set_focus(surface, None);
                self.pending_paste.remove(&surface);
                // Focus loss closes a surface bound `open: <-> x`; (window)
                // not a window, which the compositor focuses and unfocuses
                // as any app's.
                if open_two_way(scene.tree(), root) && !is_window(scene.tree(), root) {
                    self.close(scene, root);
                }
            }
            // (window) Also the compositor's close of a window
            // (`xdg_toplevel.close`), which the surface manager reports so.
            InputEvent::ClickAway { .. } => {
                if open_two_way(scene.tree(), root) {
                    self.close(scene, root);
                }
                self.hover_popup_dismissed(scene.tree(), root);
            }
            InputEvent::Paste { text, .. } => {
                // Only into the input that asked, while it still has the
                // keyboard (focus moved on: the paste is dropped).
                let Some(input) = self.pending_paste.remove(&surface) else {
                    return;
                };
                let is_input = scene
                    .tree()
                    .and_then(|t| t.get(input))
                    .is_some_and(|n| n.kind == NodeKind::Input);
                if self.focus.get(&surface) != Some(&input) || !is_input {
                    return;
                }
                let (now, caret, pending) = self.typed_text(input, scene);
                let edit = crate::widgets::pasted(&now, caret, text);
                self.apply_edit(input, edit, pending, scene);
            }
            InputEvent::Key { key, .. } => {
                if key.state == ButtonState::Pressed {
                    // Escape cancels a drag in flight (and does nothing
                    // else: the surface stays open).
                    if key.name == "Escape" && self.drag.as_ref().is_some_and(|d| d.active) {
                        self.cancel_drag(scene);
                        return;
                    }
                    self.key(surface, root, key, scene);
                } else {
                    // (keyboard) Heard as a press is, and claims nothing.
                    let focus = self.focus.get(&surface).copied().unwrap_or(root);
                    self.event(
                        focus,
                        NodeEvent::KeyRelease {
                            name: key.name.clone(),
                            text: key.text.clone(),
                            modifiers: key.modifiers,
                        },
                    );
                }
            }
            // (M4) `wl_data_device`: another program's drag, or ours
            // carried by the compositor across surfaces.
            InputEvent::DragEnter { at, kinds, .. } => {
                if self.drag.as_ref().is_some_and(|d| d.handed) && kinds.is_empty() {
                    self.drag_motion(scene, surface, *at, None);
                } else {
                    let target = self.drop_target_at(scene, surface, *at, Carried::Outside);
                    self.offer = Some(Offer { surface, target });
                }
            }
            InputEvent::DragMotion { at, .. } => {
                if self.drag.as_ref().is_some_and(|d| d.handed) {
                    self.drag_motion(scene, surface, *at, None);
                } else if self.offer.as_ref().is_some_and(|o| o.surface == surface) {
                    let target = self.drop_target_at(scene, surface, *at, Carried::Outside);
                    self.offer = Some(Offer { surface, target });
                }
            }
            InputEvent::DragLeave { .. } => {
                if let Some(d) = self
                    .drag
                    .as_mut()
                    .filter(|d| d.handed && d.surface == surface)
                {
                    d.target = None;
                    if d.origin == surface {
                        let source = d.source;
                        scene.lift(source, None);
                    }
                }
                if self.offer.as_ref().is_some_and(|o| o.surface == surface) {
                    self.offer = None;
                }
            }
            InputEvent::DragDrop { at, payload, .. } => {
                let carried = match payload {
                    strand_scene::DropPayload::Node(n) => match self.drag.as_ref() {
                        Some(d) if d.source == *n => Some((d.kind.clone(), *n)),
                        // Ours, but routing lost it (its surface went):
                        // the source's own type.
                        _ => scene.tree().and_then(|t| {
                            let k = strand_scene::drag_type(t.get(*n)?.get(Prop::Drag)?)?;
                            Some((k.to_string(), *n))
                        }),
                    },
                    strand_scene::DropPayload::External { .. } => None,
                };
                let target = match &carried {
                    Some((kind, n)) => {
                        self.drop_landing_at(scene, surface, *at, Carried::Value(kind, *n))
                    }
                    None => self.drop_landing_at(scene, surface, *at, Carried::Outside),
                };
                if let Some((node, at, hidden)) = target {
                    self.event(
                        node,
                        NodeEvent::Drop {
                            payload: payload.clone(),
                            at,
                            hidden,
                        },
                    );
                }
                self.offer = None;
                if let Some(d) = self.drag.take() {
                    self.dragged = d.active;
                    scene.lift(d.source, None);
                }
            }
        }
    }

    /// (M4) The pointer at `at` on `surface` (`time`: a pointer motion's,
    /// `None` for a `Drag*` event): a press on a `drag:` source becomes a
    /// drag past [`DRAG_THRESHOLD`], and a drag moves its source and
    /// finds where it would land. Returns true if a drag took the motion.
    fn drag_motion(
        &mut self,
        scene: &mut dyn InputScene,
        surface: SurfaceId,
        at: LogicalPoint,
        time: Option<u32>,
    ) -> bool {
        let Some(mut d) = self.drag.take() else {
            return false;
        };
        if time.is_some() && d.handed {
            // Pointer motion on a surface while the compositor carries
            // the drag: not ours.
            self.drag = Some(d);
            return false;
        }
        if !d.active {
            let (dx, dy) = (at.x - d.start.x, at.y - d.start.y);
            if surface != d.origin || (dx * dx + dy * dy).sqrt() < DRAG_THRESHOLD {
                self.drag = Some(d);
                return false;
            }
            d.active = true;
            // A drag, not a press: the press's slider drag or text
            // selection ends here.
            self.end_drags(scene, surface);
        }
        if let (Some(t), Some(t0)) = (time, d.time)
            && t > t0
        {
            let dt = (t - t0) as f32 / 1000.0;
            let v = LogicalPoint::new((at.x - d.pointer.x) / dt, (at.y - d.pointer.y) / dt);
            // Smoothed over a few motions.
            d.velocity = LogicalPoint::new(
                d.velocity.x * 0.5 + v.x * 0.5,
                d.velocity.y * 0.5 + v.y * 0.5,
            );
        }
        d.time = time.or(d.time);
        d.surface = surface;
        d.pointer = at;
        if surface == d.origin {
            scene.lift(
                d.source,
                Some(LogicalPoint::new(at.x - d.start.x, at.y - d.start.y)),
            );
        }
        d.target = self.drop_target_at(scene, surface, at, Carried::Value(&d.kind, d.source));
        self.drag = Some(d);
        true
    }

    /// (M4) Ends the drag in flight without a drop: its source springs
    /// back to its box.
    fn cancel_drag(&mut self, scene: &mut dyn InputScene) {
        if let Some(d) = self.drag.take() {
            self.dragged = d.active;
            scene.lift(d.source, None);
        }
    }

    /// (M4) The node a drop at `at` on `surface` lands on and its index:
    /// the innermost node under the pointer whose `on drop` takes what is
    /// carried (`Prop::Accepts`; never the dragged source or anything in
    /// it), and where among its rows (or, for a node without rows, its
    /// parent's) the pointer is.
    fn drop_target_at(
        &self,
        scene: &dyn InputScene,
        surface: SurfaceId,
        at: LogicalPoint,
        carried: Carried<'_>,
    ) -> Option<(NodeId, u32)> {
        self.drop_landing_at(scene, surface, at, carried)
            .map(|(node, index, _)| (node, index))
    }

    /// (M6) [`Router::drop_target_at`] with the rows its index could not
    /// see ([`HiddenRows`]), for the drop itself.
    fn drop_landing_at(
        &self,
        scene: &dyn InputScene,
        surface: SurfaceId,
        at: LogicalPoint,
        carried: Carried<'_>,
    ) -> Option<(NodeId, u32, Option<HiddenRows>)> {
        let tree = scene.tree()?;
        let inside = |n: NodeId, of: NodeId| {
            let mut cur = Some(n);
            while let Some(c) = cur {
                if c == of {
                    return true;
                }
                cur = tree.get(c).and_then(|x| x.parent);
            }
            false
        };
        let takes = |n: NodeId| {
            let Some(PropValue::List(acc)) = tree.get(n).and_then(|x| x.get(Prop::Accepts)) else {
                return false;
            };
            acc.iter().any(|a| match (a, carried) {
                (PropValue::Keyword(k), _) if k.as_str() == "any" => true,
                (PropValue::Keyword(k), Carried::Value(ty, _)) => k.as_str() == ty,
                (PropValue::Keyword(k), Carried::Outside) => k.as_str() == "Drop",
                _ => false,
            })
        };
        let source = match carried {
            Carried::Value(_, s) => Some(s),
            Carried::Outside => None,
        };
        let hits = match source {
            Some(s) => scene.hit_under(surface, at, s),
            None => scene.hit(surface, at),
        };
        let target = hits
            .into_iter()
            .filter(|n| source.is_none_or(|s| !inside(*n, s)))
            .find(|n| takes(*n))?;
        let (index, hidden) = drop_index(tree, scene, surface, target, at, source);
        Some((target, index, hidden))
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
                    // A click focuses the `input`, `list` or node with a
                    // `focus:` under it. (keyboard) With none, a `button`,
                    // `slider` or `segmented`, unless an `input` or `list`
                    // has focus: they keep the keys, so a launcher's button
                    // leaves the typing in its field and a menu's row its
                    // arrows.
                    let target = scene.tree().and_then(|tree| {
                        let root = self.surfaces.get(&surface).copied();
                        let of = |n: &NodeId| tree.get(*n).filter(|_| Some(*n) != root);
                        let holds_keys = self
                            .focus
                            .get(&surface)
                            .and_then(|f| tree.get(*f))
                            .is_some_and(|f| matches!(f.kind, NodeKind::Input | NodeKind::List));
                        under
                            .iter()
                            .find(|n| of(n).is_some_and(claims_click_focus))
                            .or_else(|| {
                                under.iter().find(|n| {
                                    !holds_keys
                                        && of(n).is_some_and(|n| {
                                            !no_hit(n) && focusable_by_kind(n.kind)
                                        })
                                })
                            })
                            .copied()
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
                    // A press on (or in) a `drag:` source may become a
                    // drag (past the threshold); a slider it holds drags
                    // the slider instead.
                    self.dragged = false;
                    self.drag = if self.dragging.contains_key(&surface) {
                        None
                    } else {
                        scene.tree().and_then(|t| {
                            under.iter().find_map(|n| {
                                let k = strand_scene::drag_type(t.get(*n)?.get(Prop::Drag)?)?;
                                Some(Drag {
                                    source: *n,
                                    kind: k.to_string(),
                                    origin: surface,
                                    start: at,
                                    surface,
                                    pointer: at,
                                    velocity: LogicalPoint::new(0.0, 0.0),
                                    time: None,
                                    active: false,
                                    handed: false,
                                    target: None,
                                })
                            })
                        })
                    };
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
                    // A drag ends: dropped on the node that takes it, or
                    // back to its box; a release that ends a drag clicks
                    // nothing.
                    // (A drag not past its threshold is dropped here too.)
                    if let Some(d) = self.drag.take()
                        && d.active
                    {
                        let target = if d.handed {
                            None
                        } else {
                            self.drop_landing_at(
                                scene,
                                surface,
                                at,
                                Carried::Value(&d.kind, d.source),
                            )
                        };
                        if let Some((node, index, hidden)) = target {
                            self.event(
                                node,
                                NodeEvent::Drop {
                                    payload: strand_scene::DropPayload::Node(d.source),
                                    at: index,
                                    hidden,
                                },
                            );
                        }
                        scene.lift(d.source, None);
                        self.dragged = true;
                    }
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
        if b == button::LEFT && std::mem::take(&mut self.dragged) {
            return;
        }
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
        self.twins.retain(|n, _| live(*n));
        // (oi-lock) The lock ended: no field is left to carry on in.
        if self
            .focused_twin
            .as_ref()
            .is_some_and(|k| !self.twins.values().any(|v| v == k))
        {
            self.focused_twin = None;
        }
        self.focus.retain(|_, n| live(*n));
        self.focus_wanted.retain(|_, n| live(*n));
        self.focus_lost.retain(|_, (n, _)| live(*n));
        self.value_flights.retain(|n, _| live(*n));
        self.pending_paste.retain(|_, n| live(*n));
    }

    /// Moves keyboard focus on `surface` to `node` (`focused`).
    fn set_focus(&mut self, surface: SurfaceId, node: Option<NodeId>) {
        // (oi-lock) Focus taken away (the keyboard left) keeps the field
        // it was in for the next lock surface the keyboard enters.
        if let Some(n) = node {
            self.focused_twin = self.twins.get(&n).cloned();
        }
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
    ///
    /// (integration-r8) Likewise from every other open popup with a
    /// two-way `open` ([`InputScene::popup_open`]): a popup's grab ends
    /// only on a press on another client's surface, so a press on the
    /// bar it hangs from, or on another of Strand's surfaces, reaches
    /// Strand and is the click away spec.md names. Not from a popup the
    /// press landed in or under (one nested in it is still its own),
    /// nor when it landed on the popup's anchor (`under`, the hit chain,
    /// holds it): what the anchor does with a press (`on click { open =
    /// !open }`) decides, as it would with the popup closed.
    fn click_away_from_others(
        &mut self,
        surface: SurfaceId,
        root: NodeId,
        under: &[NodeId],
        scene: &dyn InputScene,
    ) {
        let tree = scene.tree();
        let away = |r: NodeId| {
            if scene.exclusive_open(r) {
                return true;
            }
            if !scene.popup_open(r) {
                return false;
            }
            let Some(t) = tree else { return false };
            let inside = t.is_ancestor(r, root);
            let anchor = t.get(r).and_then(|n| n.parent);
            let on_anchor = anchor.is_some_and(|a| under.contains(&a));
            !inside && !on_anchor
        };
        let mut others: Vec<NodeId> = self
            .surfaces
            .iter()
            .filter(|(s, r)| **s != surface && **r != root)
            .map(|(_, r)| *r)
            .filter(|r| away(*r))
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
        self.focus_follows_shown(&*scene);
        let focus = self.focus.get(&surface).copied().unwrap_or(root);
        // (keyboard) A claim comes before every default and every other
        // handler: its key goes to it and nowhere else.
        if let Some((node, handler)) = scene.tree().and_then(|t| claim_of(t, root, focus, key)) {
            self.event(
                node,
                NodeEvent::Claim {
                    handler,
                    key: key.clone(),
                },
            );
            return;
        }
        let node = scene.tree().and_then(|t| t.get(focus));
        let kind = node.map(|n| n.kind);
        let nav = match node.and_then(|n| n.get(Prop::Nav)) {
            Some(PropValue::Node(l)) => Some(*l),
            _ => None,
        };
        // (window) Escape is a window's own key: only the compositor (or
        // `w.close()`) closes it.
        let open = open_two_way(scene.tree(), root) && !is_window(scene.tree(), root);
        self.event(
            focus,
            NodeEvent::Key {
                name: key.name.clone(),
                text: key.text.clone(),
                modifiers: key.modifiers,
                repeat: key.repeat,
            },
        );
        if key.name == "Escape" && open {
            self.close(scene, root);
            return;
        }
        // (flex) Tab and Shift+Tab move focus between what the surface can
        // focus, in tree order, round from the last to the first; a config
        // that wants them claims them (spec.md 6.7), and moves focus itself
        // with `focus:` or `node.focus()`.
        if matches!(key.name.as_str(), "Tab" | "ISO_Left_Tab" | "KP_Tab")
            && !key.modifiers.ctrl
            && !key.modifiers.alt
            && !key.modifiers.logo
        {
            let back = key.name == "ISO_Left_Tab" || key.modifiers.shift;
            if let Some(next) = scene
                .tree()
                .and_then(|t| next_focusable(t, root, focus, back))
            {
                self.focus_on(scene, surface, next);
                return;
            }
        }
        // (keyboard) The keys a focused node acts on, Ctrl, Alt and Super
        // left alone. First an arrow that is not its own, inside a container
        // with `arrows:`.
        if !key.modifiers.ctrl && !key.modifiers.alt && !key.modifiers.logo {
            if let Some(dir) = Dir::of(key)
                && !scene
                    .tree()
                    .is_some_and(|t| arrow_is_its_own(t, focus, nav.is_some(), dir))
                && let Some(next) = arrow_target(&*scene, surface, root, focus, dir)
            {
                self.focus_on(scene, surface, next);
                return;
            }
            let took = focus != root
                && match kind {
                    Some(NodeKind::Slider) => self.slider_key(scene, focus, key),
                    Some(NodeKind::Segmented) => self.segmented_key(scene, focus, key),
                    Some(NodeKind::Scroll) => scroll_key(scene, focus, key),
                    _ => false,
                };
            if took {
                return;
            }
            // Return or space does what a click does, once: holding it is
            // not many clicks. An `input` and a `list` have their own.
            if focus != root
                && !key.repeat
                && !matches!(kind, Some(NodeKind::Input | NodeKind::List))
                && matches!(
                    key.name.as_str(),
                    "Return" | "KP_Enter" | "space" | "KP_Space"
                )
            {
                self.press(scene, focus);
                return;
            }
        }
        let list = match kind {
            Some(NodeKind::List) => Some(focus),
            _ => nav,
        };
        let shape = list
            .and_then(|l| scene.tree().and_then(|t| t.get(l)))
            .map_or_else(ListShape::default, ListShape::of);
        // Submenus (tray menus): Right on a row that holds a popup opens
        // it; Left closes a popup that opened from another popup. (keyboard)
        // Down and Up on a horizontal list, whose Right and Left move it; a
        // grid uses all four.
        if kind != Some(NodeKind::Input) && shape.cols == 1 {
            let tree = scene.tree();
            let (opens, closes) = if shape.horizontal {
                (["Down", "KP_Down"], ["Up", "KP_Up"])
            } else {
                (["Right", "KP_Right"], ["Left", "KP_Left"])
            };
            match key.name.as_str() {
                k if opens.contains(&k) => {
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
                k if closes.contains(&k) && open && tree.is_some_and(|t| nested_popup(t, root)) => {
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
            let cur = self.place_in(list, &w);
            let last = w.count.saturating_sub(1);
            // (keyboard) A view of rows: of lines, in a grid.
            let page = scene
                .rows_in_view(list)
                .unwrap_or(1)
                .max(1)
                .saturating_mul(shape.cols);
            // An `input` keeps Left and Right for its caret, unless a claim
            // takes them.
            let sideways = kind != Some(NodeKind::Input) && (shape.horizontal || shape.cols > 1);
            // In an `input`, Home and End move its caret (Shift extends
            // the selection); Ctrl+Home and Ctrl+End go to the list's
            // ends. On a focused list they go there with any modifiers.
            let ends = kind != Some(NodeKind::Input) || key.modifiers.ctrl;
            // (keyboard) Space pages a focused list; in an `input` it is text.
            let spaced = matches!(key.name.as_str(), "space" | "KP_Space")
                && kind == Some(NodeKind::List)
                && !key.modifiers.ctrl
                && !key.modifiers.alt
                && !key.modifiers.logo;
            let to = match key.name.as_str() {
                _ if spaced && key.modifiers.shift => {
                    Some(cur.map_or(w.first, |i| i.saturating_sub(page)))
                }
                _ if spaced => Some(cur.map_or(w.first, |i| i.saturating_add(page).min(last))),
                "Down" | "KP_Down" if !shape.horizontal => {
                    Some(shape.line(cur, w.first, last, true))
                }
                "Up" | "KP_Up" if !shape.horizontal => Some(shape.line(cur, w.first, last, false)),
                "Right" | "KP_Right" if sideways => Some(shape.beside(cur, w.first, last, true)),
                "Left" | "KP_Left" if sideways => Some(shape.beside(cur, w.first, last, false)),
                "Page_Down" | "Next" | "KP_Page_Down" | "KP_Next" => {
                    Some(cur.map_or(w.first, |i| i.saturating_add(page).min(last)))
                }
                "Page_Up" | "Prior" | "KP_Page_Up" | "KP_Prior" => {
                    Some(cur.map_or(w.first, |i| i.saturating_sub(page)))
                }
                "Home" | "KP_Home" if ends => Some(0),
                "End" | "KP_End" if ends => Some(last),
                "Return" | "KP_Enter" => {
                    self.activate_selected(scene, list, &w);
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
            // Return in an input with no `nav:` list activates the input
            // itself, as it activates a list's selected row: `on activate`
            // means the same on both (the lock's password field submits).
            if matches!(key.name.as_str(), "Return" | "KP_Enter") {
                self.event(focus, NodeEvent::Activate);
                return;
            }
            let password = scene.tree().and_then(|t| t.get(focus)).is_some_and(|n| {
                matches!(n.get(Prop::InputType), Some(PropValue::Keyword(k)) if k == "password")
            });
            let (now, caret, pending) = self.typed_text(focus, scene);
            // Shortcuts match the Latin keysym of a non-Latin layout's key
            // (Ctrl+V with `ru` active pastes).
            let edit = crate::widgets::edit(
                &now,
                caret,
                key.shortcut(),
                &key.text,
                key.modifiers,
                password,
            );
            match edit {
                Edit::Copy(t) => {
                    self.emit(Intent::Clipboard(ClipboardAsk::Copy { surface, text: t }))
                }
                Edit::Cut {
                    copied,
                    text: now,
                    caret,
                } => {
                    self.emit(Intent::Clipboard(ClipboardAsk::Copy {
                        surface,
                        text: copied,
                    }));
                    self.apply_edit(focus, Edit::Changed(now, caret), pending, scene);
                }
                Edit::Paste => {
                    self.pending_paste.insert(surface, focus);
                    self.emit(Intent::Clipboard(ClipboardAsk::Paste { surface }));
                }
                edit => self.apply_edit(focus, edit, pending, scene),
            }
        }
    }

    /// (keyboard) Return or space on focused `node`: as a click on it, its
    /// row in a `list` selected and activated too. On the row itself, just what
    /// a pointer's click there does: its click, then the row's `on activate` or
    /// the list's.
    fn press(&mut self, scene: &mut dyn InputScene, node: NodeId) {
        let row = scene.tree().and_then(|t| {
            let mut chain = ancestors(t, node);
            chain.insert(0, node);
            list_row(Some(t), &chain)
        });
        let on_row = row.is_some_and(|(_, row)| row == node);
        self.event(
            node,
            if on_row {
                NodeEvent::Click
            } else {
                NodeEvent::Press
            },
        );
        if let Some((list, row)) = row {
            self.moved.insert(list);
            self.select(scene, list, Some(row));
            self.event(row, NodeEvent::Activate);
        }
    }

    /// (keyboard) A `value` a key builds on: the last it wrote, while logic
    /// has not answered, else what the scene shows.
    fn value_now(&self, scene: &dyn InputScene, node: NodeId) -> Option<PropValue> {
        let shown = scene.tree()?.get(node)?.get(Prop::Value).cloned();
        let now = self.now();
        let flying = self.value_flights.get(&node).filter(|f| {
            now.saturating_duration_since(f.at) < EDIT_IN_FLIGHT
                && (f.pending.is_empty() || shown.as_ref() != Some(&f.latest))
        });
        flying.map(|f| f.latest.clone()).or(shown)
    }

    /// (keyboard) A key writes `value` to `node`, as a drag or a click does.
    fn write_value(&mut self, node: NodeId, value: PropValue) {
        let at = self.now();
        let f = self.value_flights.entry(node).or_insert(ValueFlight {
            latest: value.clone(),
            pending: Vec::new(),
            at,
        });
        if f.latest != value {
            let was = std::mem::replace(&mut f.latest, value.clone());
            f.pending.push(was);
        }
        f.at = at;
        self.emit(Intent::Write {
            node,
            prop: Prop::Value,
            value,
        });
    }

    /// (keyboard) A focused `slider`'s keys. Whether `key` is one; one whose
    /// `value` is not bound two-way writes nothing, as a drag does not.
    fn slider_key(&mut self, scene: &mut dyn InputScene, slider: NodeId, key: &KeyInput) -> bool {
        let by = match key.name.as_str() {
            "Left" | "KP_Left" | "Down" | "KP_Down" => Some(-1.0),
            "Right" | "KP_Right" | "Up" | "KP_Up" => Some(1.0),
            "Page_Down" | "Next" | "KP_Page_Down" | "KP_Next" => Some(-10.0),
            "Page_Up" | "Prior" | "KP_Page_Up" | "KP_Prior" => Some(10.0),
            "Home" | "KP_Home" | "End" | "KP_End" => None,
            _ => return false,
        };
        if !value_two_way(scene.tree(), slider) {
            return false;
        }
        let Some(n) = scene.tree().and_then(|t| t.get(slider)) else {
            return true;
        };
        let Some(range) = crate::range::Spec::of(n.get(Prop::Range)).map(|s| s.fixed()) else {
            return true;
        };
        let step = match n.get(Prop::Step) {
            Some(PropValue::Number(s)) if *s > 0.0 && s.is_finite() => Some(*s),
            _ => None,
        };
        let now = match self.value_now(scene, slider) {
            Some(PropValue::Number(v)) => v,
            _ => range.lo,
        };
        // The fraction it goes to: a step in the range's units, the way the
        // range grows.
        let f = match by {
            Some(by) => {
                let pace = step.unwrap_or(KEY_SLIDER_STEP * (range.hi - range.lo).abs());
                let up = if range.hi < range.lo { -1.0 } else { 1.0 };
                range.frac(now + by * pace * up)
            }
            None if matches!(key.name.as_str(), "Home" | "KP_Home") => Some(0.0),
            None => Some(1.0),
        };
        let Some(f) = f else {
            return true;
        };
        let to = step.and_then(|s| range.snap(f, s)).unwrap_or(range.at(f));
        if to != now {
            self.write_value(slider, PropValue::Number(to));
        }
        true
    }

    /// (keyboard) A focused `segmented`'s keys: Left and Right choose the
    /// option beside the chosen one, stopping at the ends.
    fn segmented_key(&mut self, scene: &mut dyn InputScene, seg: NodeId, key: &KeyInput) -> bool {
        let forward = match key.name.as_str() {
            "Right" | "KP_Right" => true,
            "Left" | "KP_Left" => false,
            _ => return false,
        };
        if !value_two_way(scene.tree(), seg) {
            return false;
        }
        let opts = crate::widgets::options(
            scene
                .tree()
                .and_then(|t| t.get(seg))
                .and_then(|n| n.get(Prop::Options)),
        );
        let now = self
            .value_now(scene, seg)
            .and_then(|v| opts.iter().position(|o| *o == v));
        let to = match (now, forward) {
            (Some(i), true) => (i + 1).min(opts.len().saturating_sub(1)),
            (Some(i), false) => i.saturating_sub(1),
            (None, _) => 0,
        };
        if let Some(v) = opts.get(to).filter(|_| now != Some(to)) {
            self.write_value(seg, v.clone());
        }
        true
    }

    /// The text an edit of `input` builds on, its caret, and what the
    /// scene may show meanwhile ([`InFlight::pending`]). Keys typed (and
    /// pastes arriving) faster than logic answers build on the last
    /// write; once the scene shows it, or a text of logic's own, they
    /// build on the scene.
    ///
    /// (lockmon) An input with twins (the lock's field on each monitor,
    /// one text between them) builds on the newest write any of them made
    /// that logic has not answered yet: keys typed on one monitor right
    /// after the keyboard moved there from another are never typed on the
    /// text before the other's last keys.
    fn typed_text(&self, input: NodeId, scene: &dyn InputScene) -> (String, Caret, Vec<String>) {
        let text = match scene
            .tree()
            .and_then(|t| t.get(input))
            .and_then(|n| n.get(Prop::Text))
        {
            Some(PropValue::Text(t)) => t.clone(),
            _ => String::new(),
        };
        let in_flight = self
            .with_twins(input)
            .into_iter()
            .filter_map(|n| self.edits.get(&n))
            .filter(|e| {
                e.latest != text && e.at.elapsed() < EDIT_IN_FLIGHT && e.pending.contains(&text)
            })
            .max_by_key(|e| e.at);
        // (`observe` already dropped what logic answered.)
        let pending = in_flight.map_or_else(|| vec![text.clone()], |e| e.pending.clone());
        let now = in_flight.map_or(text, |e| e.latest.clone());
        let caret = scene
            .caret(input)
            .unwrap_or(Caret::at(now.len()))
            .clamped(&now);
        (now, caret, pending)
    }

    /// `input` and its twins (`Prop::Twin`), itself first.
    fn with_twins(&self, input: NodeId) -> Vec<NodeId> {
        let mut out = vec![input];
        if let Some(key) = self.twins.get(&input) {
            out.extend(
                self.twins
                    .iter()
                    .filter(|(n, k)| **n != input && *k == key)
                    .map(|(n, _)| *n),
            );
        }
        out
    }

    /// Applies a caret move or a text change to `input`: a change is
    /// written (two-way `text`) and remembered as in flight. (lockmon)
    /// Its twins' carets follow, so keys typed on another monitor's
    /// field go where the last ones went.
    fn apply_edit(
        &mut self,
        input: NodeId,
        edit: Edit,
        mut pending: Vec<String>,
        scene: &mut dyn InputScene,
    ) {
        match edit {
            Edit::Moved(c) => {
                for n in self.with_twins(input) {
                    scene.set_caret(n, Some(c));
                }
            }
            Edit::Changed(now, c) => {
                for n in self.with_twins(input) {
                    scene.set_caret(n, Some(c));
                }
                pending.push(now.clone());
                self.edits.insert(
                    input,
                    InFlight {
                        latest: now.clone(),
                        pending,
                        at: self.now(),
                    },
                );
                self.emit(Intent::Write {
                    node: input,
                    prop: Prop::Text,
                    value: PropValue::Text(now),
                });
            }
            Edit::None | Edit::Copy(_) | Edit::Cut { .. } | Edit::Paste => {}
        }
    }

    /// Moves `slider` to the pointer at `at` (its value from where `at`
    /// falls along its track, 0 to 1) and writes it, in the slider's
    /// `range` (graph-follow: `range: 0, 100` writes 0 to 100); `last`
    /// ends the drag.
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
        // Where the slider is drawn: its laid-out box moved by the paint
        // offsets and transforms of it and its ancestors (a card that
        // springs by `x`).
        let at = scene.to_laid(surface, slider, at);
        // Pressed, so drawn with its larger knob: the same span. (flex)
        // Laid out taller than wide, it runs upward: 0 at the bottom.
        let upright = *self.drag_upright.entry(slider).or_insert_with(|| {
            scene
                .tree()
                .and_then(|t| t.get(slider))
                .map_or(r.h > r.w, |n| {
                    crate::widgets::upright(n, f64::from(r.w), f64::from(r.h))
                })
        });
        let v = if upright {
            let (y0, y1) = crate::widgets::slider_span(r.y as f64, (r.y + r.h) as f64, true, 1.0);
            ((y1 - at.y as f64) / (y1 - y0).max(1.0)).clamp(0.0, 1.0) as f32
        } else {
            let (x0, x1) = crate::widgets::slider_span(r.x as f64, (r.x + r.w) as f64, true, 1.0);
            ((at.x as f64 - x0) / (x1 - x0).max(1.0)).clamp(0.0, 1.0) as f32
        };
        if !v.is_finite() {
            return;
        }
        let node = scene.tree().and_then(|t| t.get(slider));
        // (review) A range whose end is not known now (a field not set
        // yet) writes nothing: 0 to 1 would write fractions into a place
        // of other units.
        let range =
            crate::range::Spec::of(node.and_then(|n| n.get(Prop::Range))).map(|s| s.fixed());
        // (flex) `step:` snaps the knob and what it writes to whole steps
        // of the range's units.
        let step = match node.and_then(|n| n.get(Prop::Step)) {
            Some(PropValue::Number(s)) => Some(*s),
            _ => None,
        };
        let snapped = range.zip(step).and_then(|(r, s)| r.snap(v, s));
        let v = range.zip(snapped).and_then(|(r, s)| r.frac(s)).unwrap_or(v);
        scene.set_drag(slider, (!last).then_some(v));
        let Some(range) = range else {
            return;
        };
        self.emit(Intent::Write {
            node: slider,
            prop: Prop::Value,
            value: PropValue::Number(snapped.unwrap_or_else(|| range.at(v))),
        });
    }

    /// Ends a slider drag and a selection by pointer on `surface`.
    fn end_drags(&mut self, scene: &mut dyn InputScene, surface: SurfaceId) {
        if let Some(slider) = self.dragging.remove(&surface) {
            scene.set_drag(slider, None);
            self.drag_upright.remove(&slider);
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
        let at = scene.to_laid(surface, seg, at);
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

/// (keyboard) A kind of node focusable with no `focus:` written.
fn focusable_by_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Input
            | NodeKind::List
            | NodeKind::Button
            | NodeKind::Slider
            | NodeKind::Segmented
    )
}

/// (keyboard) Whether `focus:` is written on `n`, with any value.
fn focus_written(n: &crate::tree::Node) -> bool {
    n.get(Prop::Focus)
        .is_some_and(|v| !matches!(v, PropValue::Unset))
}

/// (keyboard) `hit: none`: it takes no input, while its children take theirs.
fn no_hit(n: &crate::tree::Node) -> bool {
    matches!(n.get(Prop::Hit), Some(PropValue::Keyword(k)) if k == "none")
}

/// (keyboard) Whether Tab and the arrows can land on `n`.
fn focusable(n: &crate::tree::Node) -> bool {
    !no_hit(n) && (focusable_by_kind(n.kind) || focus_written(n))
}

/// (keyboard) Whether focus can stay on `node` under `root`: the surface always
/// can, and what is alive, takes input, and is not at `opacity: 0` with
/// everything around it shown.
fn holds_focus(tree: &SceneTree, root: NodeId, node: NodeId) -> bool {
    if node == root {
        return true;
    }
    if !tree.contains_live(node) || tree.get(node).is_none_or(no_hit) {
        return false;
    }
    let mut cur = Some(node);
    while let Some(id) = cur {
        let Some(n) = tree.get(id) else {
            return false;
        };
        if hidden(n) {
            return false;
        }
        if id == root {
            break;
        }
        cur = n.parent;
    }
    true
}

/// (keyboard) `opacity: 0`: not drawn and not hit, its subtree with it.
fn hidden(n: &crate::tree::Node) -> bool {
    matches!(n.get(Prop::Opacity), Some(PropValue::Number(o)) if *o <= 0.0)
}

/// (keyboard) Whether a click on `n` always focuses it: a `button`, `slider`
/// or `segmented` with no `focus:` does so only when nothing holds the keys.
fn claims_click_focus(n: &crate::tree::Node) -> bool {
    !no_hit(n) && (matches!(n.kind, NodeKind::Input | NodeKind::List) || focus_written(n))
}

/// (keyboard) The nodes under `root` keys can focus, in tree order. Nested
/// surfaces (popups) are their own, and what is not shown is skipped: a node
/// leaving or at `opacity: 0`, its subtree with it (spec.md 6.7). A `list` is
/// one stop: what is in its rows is one only with a `focus:` written. `keep`
/// is listed too, focusable, shown or not, so a walk goes on from it; with
/// `sealed`, what is inside another container with `arrows:` is left out.
fn focusables(tree: &SceneTree, root: NodeId, keep: NodeId, sealed: bool) -> Vec<NodeId> {
    let mut all = Vec::new();
    let mut stack = vec![(root, false, false)];
    while let Some((id, was_hidden, in_list)) = stack.pop() {
        let Some(n) = tree.get(id) else { continue };
        if id != root && (n.kind.is_surface() || !tree.contains_live(id)) {
            continue;
        }
        let is_hidden = was_hidden || hidden(n);
        let stops = id != root && !is_hidden && focusable(n) && (!in_list || focus_written(n));
        if id == keep || stops {
            all.push(id);
        }
        if sealed && id != root && arrows_of(n).is_some() {
            continue;
        }
        let in_list = in_list || n.kind == NodeKind::List;
        stack.extend(n.children.iter().rev().map(|c| (*c, is_hidden, in_list)));
    }
    all
}

/// (flex) What is focusable after `from` under `root` in tree order
/// (before it with `back`), round from the end. `None` when there is no
/// other.
fn next_focusable(tree: &SceneTree, root: NodeId, from: NodeId, back: bool) -> Option<NodeId> {
    let all = focusables(tree, root, from, false);
    let at = all.iter().position(|n| *n == from);
    let len = all.len();
    let i = match (at, back) {
        (Some(i), false) => (i + 1) % len.max(1),
        (Some(i), true) => (i + len - 1) % len.max(1),
        (None, false) => 0,
        (None, true) => len.checked_sub(1)?,
    };
    all.get(i).copied().filter(|n| *n != from)
}

/// (keyboard) An arrow key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn of(key: &KeyInput) -> Option<Dir> {
        Some(match key.name.as_str() {
            "Up" | "KP_Up" => Dir::Up,
            "Down" | "KP_Down" => Dir::Down,
            "Left" | "KP_Left" => Dir::Left,
            "Right" | "KP_Right" => Dir::Right,
            _ => return None,
        })
    }

    fn sideways(self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }
}

/// (keyboard) `n`'s `arrows:`: whether it wraps, if it has one.
fn arrows_of(n: &crate::tree::Node) -> Option<bool> {
    match n.get(Prop::Arrows) {
        Some(PropValue::Keyword(k)) if k == "focus" => Some(false),
        Some(PropValue::Keyword(k)) if k == "focus_wrap" => Some(true),
        _ => None,
    }
}

/// (keyboard) Whether focused `node` uses arrow `dir` itself, so `arrows:`
/// around it does not get it: an `input`'s caret, or its `nav` list's rows,
/// a `list`, a `slider` and a `segmented` bound two-way, and a `scroll` along
/// the way it runs, with Up and Down on one along x too ([`scroll_key`]).
fn arrow_is_its_own(tree: &SceneTree, node: NodeId, nav: bool, dir: Dir) -> bool {
    let Some(n) = tree.get(node) else {
        return false;
    };
    match n.kind {
        NodeKind::Input => dir.sideways() || nav,
        NodeKind::List => true,
        NodeKind::Slider => value_two_way(Some(tree), node),
        NodeKind::Segmented => dir.sideways() && value_two_way(Some(tree), node),
        NodeKind::Scroll => !dir.sideways() || crate::layout::scroll_axes(n)[0],
        _ => false,
    }
}

/// (keyboard) Where arrow `dir` moves focus from `from`, inside the nearest
/// container around it with `arrows:` and no further: the focusable node
/// wholly past it that way whose box is nearest, first of those level with it
/// (`None` when there is none). The container wrapping, one level with it at
/// the far end. A container that is itself focused goes to its first.
fn arrow_target(
    scene: &dyn InputScene,
    surface: SurfaceId,
    root: NodeId,
    from: NodeId,
    dir: Dir,
) -> Option<NodeId> {
    let tree = scene.tree()?;
    let mut scope = from;
    let wrap = loop {
        let n = tree.get(scope)?;
        if let Some(wrap) = arrows_of(n) {
            break wrap;
        }
        if scope == root {
            return None;
        }
        scope = n.parent?;
    };
    let shown = |r: &LogicalRect| r.w > 0.0 && r.h > 0.0;
    let all: Vec<(NodeId, LogicalRect)> = focusables(tree, scope, from, true)
        .into_iter()
        .filter(|n| *n != from)
        .filter_map(|n| Some((n, scene.node_rect(surface, n).filter(shown)?)))
        .collect();
    if from == scope {
        return all.first().map(|(n, _)| *n);
    }
    let at = scene.node_rect(surface, from)?;
    // A box as (start, end) along the way it moves and across it.
    let spans = |r: &LogicalRect| {
        let (x, y) = ((r.x, r.x + r.w), (r.y, r.y + r.h));
        match dir {
            Dir::Right => (x, y),
            Dir::Left => ((-x.1, -x.0), y),
            Dir::Down => (y, x),
            Dir::Up => ((-y.1, -y.0), x),
        }
    };
    let (along, across) = spans(&at);
    let mid = (across.0 + across.1) / 2.0;
    let scored = |r: &LogicalRect| {
        let (a, c) = spans(r);
        let apart = (c.0 - across.1).max(across.0 - c.1).max(0.0);
        let level = c.0 < across.1 && across.0 < c.1;
        let off_mid = ((c.0 + c.1) / 2.0 - mid).abs();
        (a.0, level, apart, off_mid)
    };
    let nearest = all
        .iter()
        .filter(|(_, r)| scored(r).0 >= along.1 - 1.0)
        .map(|(n, r)| {
            let (start, level, apart, off_mid) = scored(r);
            let gap = (start - along.1).max(0.0);
            (*n, !level, gap + 2.0 * apart, off_mid)
        })
        .min_by(|a, b| {
            a.1.cmp(&b.1)
                .then(a.2.total_cmp(&b.2))
                .then(a.3.total_cmp(&b.3))
        })
        .map(|(n, ..)| n);
    if nearest.is_some() || !wrap {
        return nearest;
    }
    all.iter()
        .filter(|(_, r)| scored(r).1)
        .min_by(|(_, a), (_, b)| {
            let (a, b) = (scored(a), scored(b));
            a.0.total_cmp(&b.0).then(a.3.total_cmp(&b.3))
        })
        .map(|(n, _)| *n)
}

/// (keyboard) A focused `scroll`'s keys: a step, a view, an end. Whether `key`
/// is one. An arrow across the way it runs is not, on a `scroll` with one way
/// (but the way a wheel goes: Up and Down on one along x).
fn scroll_key(scene: &mut dyn InputScene, node: NodeId, key: &KeyInput) -> bool {
    let axes = scene
        .tree()
        .and_then(|t| t.get(node))
        .map_or([false, true], crate::layout::scroll_axes);
    let by = |dy: f32, dx: f32| ScrollMove::By {
        dy: (dx == 0.0).then_some(dy),
        dx: (dx != 0.0).then_some(dx),
    };
    let mv = match key.name.as_str() {
        "Up" | "KP_Up" => by(-KEY_SCROLL_STEP, 0.0),
        "Down" | "KP_Down" => by(KEY_SCROLL_STEP, 0.0),
        "Left" | "KP_Left" if axes[0] => by(0.0, -KEY_SCROLL_STEP),
        "Right" | "KP_Right" if axes[0] => by(0.0, KEY_SCROLL_STEP),
        "Page_Up" | "Prior" | "KP_Page_Up" | "KP_Prior" => ScrollMove::Pages(-1.0),
        "Page_Down" | "Next" | "KP_Page_Down" | "KP_Next" => ScrollMove::Pages(1.0),
        "space" | "KP_Space" => ScrollMove::Pages(if key.modifiers.shift { -1.0 } else { 1.0 }),
        "Home" | "KP_Home" => ScrollMove::To {
            y: Some(0.0),
            x: None,
        },
        "End" | "KP_End" => ScrollMove::To {
            y: Some(f32::INFINITY),
            x: None,
        },
        _ => return false,
    };
    scene.scroll_move(node, mv);
    true
}

/// (keyboard) The claim `key` goes to: the innermost node from `from` out
/// to its surface `root` with an active claim of it, and that claim's
/// number there.
fn claim_of(tree: &SceneTree, root: NodeId, from: NodeId, key: &KeyInput) -> Option<(NodeId, u32)> {
    let mut cur = Some(from);
    while let Some(id) = cur {
        let n = tree.get(id)?;
        if let Some(PropValue::KeyClaims(claims)) = n.get(Prop::KeyClaims)
            && let Some(handler) = strand_scene::claiming(claims, key)
        {
            return Some((id, handler));
        }
        if id == root {
            return None;
        }
        cur = n.parent;
    }
    None
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

/// (window) True if the surface node `root` is a `window`.
fn is_window(tree: Option<&SceneTree>, root: NodeId) -> bool {
    tree.and_then(|t| t.get(root))
        .is_some_and(|n| n.kind == NodeKind::Window)
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

/// (keyboard) How a `list`'s selection moves, from its props.
#[derive(Clone, Copy, Debug)]
struct ListShape {
    /// `wrap: true`: one move past either end goes round.
    wrap: bool,
    /// `axis: horizontal`.
    horizontal: bool,
    /// `columns:`, 1 without.
    cols: u32,
}

impl Default for ListShape {
    fn default() -> Self {
        Self {
            wrap: false,
            horizontal: false,
            cols: 1,
        }
    }
}

impl ListShape {
    fn of(node: &crate::tree::Node) -> Self {
        Self {
            wrap: matches!(node.get(Prop::Wrap), Some(PropValue::Bool(true))),
            horizontal: crate::layout::list_horizontal(node),
            cols: crate::layout::list_columns(node),
        }
    }

    /// One row on or back along a line: in a grid, not past the line's end
    /// but with `wrap`, which carries on to the next line and round.
    fn beside(self, cur: Option<u32>, first: u32, last: u32, forward: bool) -> u32 {
        if self.cols == 1 || self.wrap {
            return self.step(cur, first, last, forward);
        }
        let Some(i) = cur else {
            return first;
        };
        let at = i % self.cols;
        match forward {
            true if at + 1 == self.cols || i >= last => i.min(last),
            true => i + 1,
            false if at == 0 => i,
            false => i - 1,
        }
    }

    /// One line on (`down`) or back from `cur`, the same column: in a grid,
    /// from above an empty cell of a short last line to its last row; off
    /// either end nowhere, or with `wrap` to that column on the other end.
    /// A line is a row with no `columns:`.
    fn line(self, cur: Option<u32>, first: u32, last: u32, down: bool) -> u32 {
        let n = self.cols;
        if n == 1 {
            return self.step(cur, first, last, down);
        }
        let Some(i) = cur else {
            return first;
        };
        let i = i.min(last);
        if down {
            match i.saturating_add(n) {
                to if to <= last => to,
                _ if i / n < last / n => last,
                _ if self.wrap => i % n,
                _ => i,
            }
        } else {
            match i.checked_sub(n) {
                Some(to) => to,
                None if self.wrap => (last / n * n + i % n).min(last),
                None => i,
            }
        }
    }

    /// One row on (`forward`) or back from `cur` among rows up to `last`,
    /// `first` with no selection.
    fn step(self, cur: Option<u32>, first: u32, last: u32, forward: bool) -> u32 {
        let Some(i) = cur else {
            return first;
        };
        match (forward, self.wrap) {
            (true, true) if i >= last => 0,
            (true, _) => i.saturating_add(1).min(last),
            (false, true) if i == 0 => last,
            (false, _) => i.saturating_sub(1).min(last),
        }
    }
}

/// (M4) Where a drop at `at` lands in `target`: the global index among
/// its rows (its live children; a windowed list's `row_first` added) of
/// the first row whose middle is past the pointer, counted without the
/// dragged item: in the source's own list, the row that is or holds the
/// `source` (equal items beside it are counted: `move` leaves them in
/// place); (M5) in another list, its first copy of the dragged item (a
/// row holding a `drag:` source of the same `drag_identity`, as the
/// same pin on another monitor's dock; the first, as `move` takes the
/// first equal item out). A row holds an item as its list's own only
/// when no other list (a `list`, or a `Prop::DropRows` element) holds
/// it as a row between them: a kanban board's column holding the
/// dragged card is a row of the board, counted, not the card's row,
/// while a dock item whose icon sits beside its own `for` of window dots
/// (the icon `Prop::NotRow`) is the icon's row. So a row moved within its own list, or onto
/// another surface's copy of that list, lands where it is let go,
/// `pins.move(p.app, at)`. (M6) A copy unmounted before a windowed
/// list's mounted rows is not seen here: the index comes with that
/// list's [`HiddenRows`] when the drop is from another list and no
/// mounted copy was left out before the pointer, and logic, which holds
/// every item, leaves out such a copy. Rows mounted but not laid out (out
/// of view) count by their place; past the last laid-out row lands after
/// it, and a target none of whose rows is laid out lands at its end.
/// Only a `list`, or a target whose children are drag sources, holds
/// rows; any other target (childless, or holding its own content such as
/// a dock icon's image and label) is placed in its parent's rows: its
/// own index, plus one past its middle. Rows run
/// along x in a `row` (or when they are spread wider than tall), else y.
fn drop_index(
    tree: &SceneTree,
    scene: &dyn InputScene,
    surface: SurfaceId,
    target: NodeId,
    at: LogicalPoint,
    source: Option<NodeId>,
) -> (u32, Option<HiddenRows>) {
    // (M5) The dragged value's identity: a row holding a `drag:` source
    // with it is a copy of the dragged item (one pin on the dock of each
    // monitor); another list's first copy is left out as the source is
    // in its own, so a drop handed to another surface counts what
    // `pins.move` counts.
    let identity = source
        .and_then(|s| tree.get(s)?.get(Prop::Drag))
        .and_then(strand_scene::drag_identity);
    // A node whose children are another list's rows: a `list`, or an
    // element the compiler marks as holding a `for`'s rows
    // (`Prop::DropRows`). A drag source under one belongs to that list,
    // not to a list further out (a card in a kanban column, not the
    // board's row of columns).
    let boundary = |n: NodeId| {
        tree.get(n).is_some_and(|x| {
            x.kind == NodeKind::List || matches!(x.get(Prop::DropRows), Some(PropValue::Bool(true)))
        })
    };
    // A child of a boundary that is its own content, not one of its
    // `for`'s rows (`Prop::NotRow`: a dock item's icon beside its `for`
    // of window dots).
    let not_row = |c: NodeId| {
        tree.get(c)
            .is_some_and(|x| matches!(x.get(Prop::NotRow), Some(PropValue::Bool(true))))
    };
    // The row of list `n` that is or holds node `s` as one of `n`'s own
    // items: the child of `n` on `s`'s path up, when no other list holds
    // it as a row between them (a wrapper such as `box { image { drag:
    // p } }`, or `col { for w in p.windows {…} image { drag: p } }`
    // whose icon is its own content, is not one).
    let row_in = |n: NodeId, s: NodeId| -> Option<NodeId> {
        let mut row = s;
        let mut up = tree.get(s)?.parent;
        while let Some(x) = up {
            if x == n {
                return Some(row);
            }
            if boundary(x) && !not_row(row) {
                return None;
            }
            row = x;
            up = tree.get(x)?.parent;
        }
        None
    };
    // Whether row `r` of list `n` is a copy of the dragged item: it is,
    // or holds as `n`'s item, a `drag:` source of the dragged identity.
    // Another list inside it (a column of cards) holds its own items;
    // only that list's own content (`Prop::NotRow`) is searched.
    let copy = |n: NodeId, r: NodeId| -> bool {
        let Some(id) = identity else {
            return false;
        };
        let mut stack = vec![r];
        while let Some(c) = stack.pop() {
            let Some(x) = tree.get(c).filter(|_| tree.contains_live(c)) else {
                continue;
            };
            if x.get(Prop::Drag).and_then(strand_scene::drag_identity) == Some(id)
                && row_in(n, c) == Some(r)
            {
                return true;
            }
            if boundary(c) {
                stack.extend(x.children.iter().copied().filter(|k| not_row(*k)));
            } else {
                stack.extend(x.children.iter().copied());
            }
        }
        false
    };
    // A list's rows without the dragged item: in the source's own list,
    // the row that is or holds the source as that list's item (an equal
    // item beside it is another row, which `move` leaves in place); in
    // another list (the same pins on another monitor's dock), its first
    // copy, as `move` takes the first equal item out. A row of an outer
    // list that holds the source's list (a kanban column) is counted.
    // `keep` (a per-item target) is counted.
    let rows_of = |n: NodeId, keep: Option<NodeId>| -> Counted {
        let live: Vec<NodeId> = tree.get(n).map_or_else(Vec::new, |x| {
            x.children
                .iter()
                .copied()
                .filter(|r| tree.contains_live(*r))
                .collect()
        });
        let own = source
            .and_then(|s| row_in(n, s))
            .filter(|r| live.contains(r));
        let out = own
            .or_else(|| live.iter().copied().find(|r| copy(n, *r)))
            .filter(|r| Some(*r) != keep);
        let rows = live
            .iter()
            .copied()
            .filter(|r| Some(*r) != source && Some(*r) != out)
            .collect();
        Counted {
            rows,
            live,
            out,
            own: own.is_some(),
        }
    };
    let first = |n: NodeId| match tree.get(n).and_then(|x| x.get(Prop::RowFirst)) {
        Some(PropValue::Number(f)) if f.is_finite() && *f >= 0.0 => *f as u32,
        _ => 0,
    };
    // (M6) The rows of `n` before its mounted window, when `at` (`k` of
    // `c.rows` before the pointer) may still have to leave out a copy
    // among them: the drop is a `drag:` source of another list (in its
    // own list the source's row, mounted, is the one left out), and no
    // mounted copy was left out before the pointer (a copy before the
    // window would have been the first, and left out in its place, for
    // the same count). Whether the copy left out is before the pointer:
    // by its middle along the rows (`x`) when it is laid out, as the
    // pointer is placed among the others, else by its place among them
    // (before the row `k`).
    let hidden = |n: NodeId, c: &Counted, k: usize, x: Option<bool>| -> Option<HiddenRows> {
        let before = first(n);
        if identity.is_none() || c.own || before == 0 {
            return None;
        }
        let place = |r: NodeId| c.live.iter().position(|x| *x == r);
        let out_before = c.out.is_some_and(|o| {
            let laid = x.zip(scene.node_rect(surface, o));
            match (laid, c.rows.get(k)) {
                (Some((true, r)), _) => at.x > r.x + r.w / 2.0,
                (Some((false, r)), _) => at.y > r.y + r.h / 2.0,
                (None, Some(next)) => place(o) < place(*next),
                (None, None) => true,
            }
        });
        (!out_before).then_some(HiddenRows { list: n, before })
    };
    let rects = |rows: &[NodeId]| -> Vec<LogicalRect> {
        rows.iter()
            .filter_map(|r| scene.node_rect(surface, *r))
            .collect()
    };
    let along_x = |n: NodeId, rs: &[LogicalRect]| {
        if tree.get(n).is_some_and(|x| x.kind == NodeKind::Row) {
            return true;
        }
        match (rs.first(), rs.last()) {
            (Some(a), Some(b)) if rs.len() > 1 => (b.x - a.x).abs() > (b.y - a.y).abs(),
            _ => false,
        }
    };
    // A target holds rows when it is a `list`, when the compiler says its
    // direct child is a `for` (`Prop::DropRows`), or when its children
    // are drag sources (a `for` of `drag:` items, the dragged one among
    // them);
    // otherwise its children are its own content (an icon and a label in
    // a per-item `on drop`) and it is itself a row of its parent.
    let holds_rows = tree.get(target).is_some_and(|x| {
        x.kind == NodeKind::List
            || matches!(x.get(Prop::DropRows), Some(PropValue::Bool(true)))
            || x.children.iter().any(|c| {
                tree.contains_live(*c) && tree.get(*c).is_some_and(|n| n.get(Prop::Drag).is_some())
            })
    });
    let counted = rows_of(target, None);
    let rows = &counted.rows;
    if holds_rows {
        // Only the rows in view (and a little overscan) are laid out; a
        // windowed list mounts more on either side. The laid-out ones
        // keep their places among all of them.
        let laid: Vec<(usize, LogicalRect)> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| scene.node_rect(surface, *r).map(|b| (i, b)))
            .collect();
        let rs: Vec<LogicalRect> = laid.iter().map(|(_, b)| *b).collect();
        let x = along_x(target, &rs);
        let i = laid
            .iter()
            .find(|(_, r)| {
                if x {
                    at.x <= r.x + r.w / 2.0
                } else {
                    at.y <= r.y + r.h / 2.0
                }
            })
            .map_or_else(
                || laid.last().map_or(rows.len(), |(i, _)| i + 1),
                |(i, _)| *i,
            );
        return (
            first(target) + i as u32,
            hidden(target, &counted, i, Some(x)),
        );
    }
    // A target without rows: its place among its parent's rows.
    let Some(parent) = tree.get(target).and_then(|x| x.parent) else {
        return (0, None);
    };
    // The target itself is counted even when it is a copy of the
    // dragged item; dropped on its own copy, the item stays where that
    // copy is.
    let counted = rows_of(parent, Some(target));
    let siblings = &counted.rows;
    let Some(i) = siblings.iter().position(|r| *r == target) else {
        return (first(parent), None);
    };
    if copy(parent, target) {
        return (first(parent) + i as u32, hidden(parent, &counted, i, None));
    }
    let rs = rects(siblings);
    let x = along_x(parent, &rs);
    let after = scene.node_rect(surface, target).is_some_and(|r| {
        if x {
            at.x > r.x + r.w / 2.0
        } else {
            at.y > r.y + r.h / 2.0
        }
    });
    let k = i + usize::from(after);
    (
        first(parent) + k as u32,
        hidden(parent, &counted, k, Some(x)),
    )
}

/// (M6) A list's rows as `drop_index` counts them: `rows` (its `live`
/// children without the source and `out`), `out` (the copy of the
/// dragged item left out, or the source's own row) and `own` (whether
/// the source is one of its rows).
struct Counted {
    rows: Vec<NodeId>,
    live: Vec<NodeId>,
    out: Option<NodeId>,
    own: bool,
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

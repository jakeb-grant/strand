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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use strand_scene::input::button;
use strand_scene::{
    ButtonState, InputEvent, KeyInput, LogicalPoint, LogicalRect, Modifiers, NodeId, NodeKind,
    Prop, PropValue, SceneDiff, SceneOp, SurfaceId,
};

use crate::renderer::Renderer;
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
    fn tree(&self) -> Option<&SceneTree> {
        None
    }
    /// Scrolls list `list` so `row` is in view.
    fn reveal(&mut self, list: NodeId, row: NodeId) {
        let _ = (list, row);
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
    fn tree(&self) -> Option<&SceneTree> {
        Some(Renderer::tree(self))
    }
    fn reveal(&mut self, list: NodeId, row: NodeId) {
        self.scroll_into_view(list, row);
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
    pub fn observe(&mut self, diff: &SceneDiff) {
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
    /// selected left) selects its first, so `selected` shows which row
    /// Return will `activate`. Returns what logic hears of it.
    pub fn settle(&mut self, scene: &mut dyn InputScene) -> Vec<Intent> {
        self.prune(scene.tree());
        self.select_first_rows(scene);
        self.flush(scene)
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
            if self.selected.contains_key(list) {
                continue;
            }
            let first = tree
                .get(*list)
                .and_then(|l| l.children.iter().copied().find(|r| tree.contains_live(*r)));
            if let Some(row) = first {
                want.push((*list, row));
            }
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
                if self.pressed.contains_key(&surface) {
                    // A drag: the slider follows the pointer, a held
                    // button in an `input` extends the selection.
                    if let Some(&slider) = self.dragging.get(&surface) {
                        self.drag(scene, surface, slider, *position, false);
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
                ..
            } => {
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
                if dy == 0.0 && dx == 0.0 {
                    return;
                }
                let under = chain(scene, *position);
                if dy != 0.0 {
                    scene.scroll(surface, *position, dy as f32);
                }
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
                        self.drag(scene, surface, slider, at, false);
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
                        self.drag(scene, surface, slider, at, true);
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
        if let Some(list) = list {
            let rows = scene
                .tree()
                .and_then(|t| t.get(list))
                .map(|n| n.children.clone())
                .unwrap_or_default();
            let cur = self
                .selected
                .get(&list)
                .and_then(|r| rows.iter().position(|x| x == r));
            match key.name.as_str() {
                "Down" | "Up" | "KP_Down" | "KP_Up" => {
                    let down = key.name.ends_with("Down");
                    let next = match (cur, down) {
                        (None, _) => rows.first(),
                        (Some(i), true) => rows.get((i + 1).min(rows.len().saturating_sub(1))),
                        (Some(i), false) => rows.get(i.saturating_sub(1)),
                    };
                    if let Some(&row) = next {
                        self.select(scene, list, Some(row));
                    }
                    return;
                }
                "Return" | "KP_Enter" => {
                    let row = cur.and_then(|i| rows.get(i)).or(rows.first()).copied();
                    if let Some(row) = row {
                        self.event(row, NodeEvent::Activate);
                    }
                    return;
                }
                _ => {}
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
    fn drag(
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

    /// Selects `row` in `list` (`selected`), scrolled into view.
    fn select(&mut self, scene: &mut dyn InputScene, list: NodeId, row: Option<NodeId>) {
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
            scene.reveal(list, r);
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

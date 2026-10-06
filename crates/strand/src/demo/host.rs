//! The surface host: `strand-surface` calls it, it forwards to the
//! renderer (`docs/architecture.md`, "Render loop"), and under
//! `STRAND_LOG=damage` it logs the damage of every painted frame, and a
//! `dropped` line for a painted frame whose commit then failed.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use calloop::channel::Sender;
use strand_compiler::instantiate::NodeFlag;
use strand_render::Renderer;
use strand_render::SceneTree;
use strand_scene::{
    ButtonState, Damage, InputEvent, KeyInput, LogicalPoint, NodeId, NodeKind, PaintTarget,
    Painter, Prop, PropValue, Scale, Size, SurfaceId,
};
use strand_surface::{Monitor, SurfaceHost};

use strand_scene::input::button;

use crate::run::{NodeEvent, ScreenInfo, ToLogic};

#[derive(Debug)]
pub struct Host {
    pub renderer: Renderer,
    log_damage: bool,
    /// `strand run`: what the logic thread hears about.
    logic: Option<Forward>,
    /// Tests: told of every paint and monitor change (`bench.rs`,
    /// `fuzz.rs`).
    #[cfg(test)]
    pub(crate) probe: Option<ProbeHandle>,
}

/// What the latency benchmark watches on the main thread.
#[cfg(test)]
pub(crate) trait Probe {
    /// `surface` was painted at `scale` (`drew`: with damage, so
    /// committed).
    fn painted(&self, surface: SurfaceId, drew: bool, scale: Scale, renderer: &Renderer);
    /// `surface` was configured (its first configure: the layer
    /// surface's round trip is over).
    fn configured(&self, surface: SurfaceId);
    /// A monitor was plugged in or changed.
    fn monitor(&self);
    /// A frame of `surface` was painted with damage (it is committed):
    /// the whole buffer, as the compositor gets it (`fuzz.rs`).
    fn frame(&self, _surface: SurfaceId, _target: &PaintTarget<'_>) {}
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ProbeHandle(pub std::rc::Rc<dyn Probe>);

#[cfg(test)]
impl std::fmt::Debug for ProbeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProbeHandle")
    }
}

/// What input routing asks of the scene: the hit chain under a point,
/// scrolling, and the tree (focus, `nav`, list rows, `open`). The
/// renderer implements it; tests give just a hit function.
pub(crate) trait InputScene {
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
}

/// A scene that only hit-tests (tests).
#[cfg(test)]
pub(crate) struct HitOnly<F>(pub F);

#[cfg(test)]
impl<F: Fn(SurfaceId, LogicalPoint) -> Vec<NodeId>> InputScene for HitOnly<F> {
    fn hit(&self, surface: SurfaceId, at: LogicalPoint) -> Vec<NodeId> {
        (self.0)(surface, at)
    }
}

/// How long an `input` trusts the text it last wrote over the text the
/// scene shows (the write is still on its way through logic).
const EDIT_IN_FLIGHT: Duration = Duration::from_millis(500);

/// The surface layer's facts for a logic thread (`strand run`): monitors
/// as the `screens` service, surface sizes, and surface-level input.
#[derive(Debug)]
pub(crate) struct Forward {
    tx: Sender<ToLogic>,
    /// Monitors in plug order; `false`: unplugged, kept for 30 s with its
    /// place, so a monitor that comes back keeps its position (and the
    /// first one stays `screens.focused`).
    monitors: Vec<(Monitor, bool)>,
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
    /// The selected row of each list arrows have moved in.
    selected: HashMap<NodeId, NodeId>,
    /// The text each `input` last wrote, and when.
    edits: HashMap<NodeId, (String, Instant)>,
}

impl Forward {
    pub(crate) fn new(tx: Sender<ToLogic>) -> Self {
        Self {
            tx,
            monitors: Vec::new(),
            surfaces: HashMap::new(),
            hovered: HashMap::new(),
            pressed: HashMap::new(),
            right_down: HashMap::new(),
            middle_down: HashMap::new(),
            focus: HashMap::new(),
            selected: HashMap::new(),
            edits: HashMap::new(),
        }
    }

    fn send(&self, msg: ToLogic) {
        // The logic thread gone ends the run on the diff channel.
        let _ = self.tx.send(msg);
    }

    fn screens(&self) {
        let list = self
            .monitors
            .iter()
            .filter(|(_, plugged)| *plugged)
            .map(|(m, _)| ScreenInfo {
                id: m.id.as_str().to_string(),
                name: m
                    .connector
                    .clone()
                    .unwrap_or_else(|| m.id.as_str().to_string()),
                make: m.make.clone(),
                model: m.model.clone(),
                description: m.description.clone(),
                scale: m.scale.as_f64(),
                width: m.logical_size.map_or(0.0, |s| s.0 as f64),
                height: m.logical_size.map_or(0.0, |s| s.1 as f64),
            })
            .collect();
        self.send(ToLogic::Screens(list));
    }

    pub(crate) fn attached(&mut self, surface: SurfaceId, node: NodeId) {
        self.surfaces.insert(surface, node);
    }

    /// A surface's logical size: its buffer size over its scale.
    pub(crate) fn configured(&self, surface: SurfaceId, size: Size, scale: Scale) {
        if let Some(&node) = self.surfaces.get(&surface) {
            let s = scale.as_f64();
            self.send(ToLogic::Size {
                node,
                width: (size.w as f64 / s) as f32,
                height: (size.h as f64 / s) as f32,
            });
        }
    }

    pub(crate) fn detached(&mut self, surface: SurfaceId) {
        self.surfaces.remove(&surface);
        self.hovered.remove(&surface);
        self.pressed.remove(&surface);
        self.right_down.remove(&surface);
        self.middle_down.remove(&surface);
        self.focus.remove(&surface);
    }

    /// Plugged in, or back within 30 s (`reconnected`) at its old place.
    pub(crate) fn monitor_added(&mut self, monitor: &Monitor) {
        match self.monitors.iter_mut().find(|(m, _)| m.id == monitor.id) {
            Some(entry) => *entry = (monitor.clone(), true),
            None => self.monitors.push((monitor.clone(), true)),
        }
        self.screens();
    }

    pub(crate) fn monitor_changed(&mut self, monitor: &Monitor) {
        match self.monitors.iter_mut().find(|(m, _)| m.id == monitor.id) {
            Some(entry) => *entry = (monitor.clone(), true),
            None => self.monitors.push((monitor.clone(), true)),
        }
        self.screens();
    }

    pub(crate) fn monitor_removed(&mut self, monitor: &Monitor) {
        if let Some(entry) = self.monitors.iter_mut().find(|(m, _)| m.id == monitor.id) {
            entry.1 = false;
        }
        self.screens();
    }

    /// Unplugged 30 s ago and not back: its place and its bar's state go.
    pub(crate) fn monitor_forgotten(&mut self, monitor: &Monitor) {
        self.monitors.retain(|(m, _)| m.id != monitor.id);
        self.send(ToLogic::Forget(monitor.id.as_str().to_string()));
    }

    /// Pointer input on the node under the pointer (the scene's hit
    /// chain, innermost first, ending at the surface's node): every node
    /// on the chain is `hover`ed (a row is hovered while a child is), a
    /// left button held marks the chain under it `pressed` (and latches
    /// `hover` there until it is released, as a drag does), a release is
    /// `click`/`secondary`/`middle` on the innermost node that both the
    /// press and the release were over (logic bubbles it to the nearest
    /// handler: innermost first, `propagate()` passes it on; a release
    /// with no press on this surface clicks nothing), and a click on a
    /// `list` row also `activate`s it. A scroll scrolls the innermost
    /// `scroll`/`list` under the pointer and is `scroll(dy, dx)` on the
    /// innermost node.
    ///
    /// Keyboard input goes to the focused node of the focused surface:
    /// the first node with `focus: true` when the surface gets the
    /// keyboard, or an `input`/`list` clicked. Keys are `key(k)` on it;
    /// an `input` edits its `text` (a two-way write) and routes arrows to
    /// the list its `nav` names, as a focused `list` takes them itself:
    /// Up/Down move the selection (`selected`, scrolled into view) and
    /// Return `activate`s the selected row. Escape, and losing the
    /// keyboard, write `open: false` on a surface bound two-way (a popup
    /// also gets `dismiss`).
    pub(crate) fn input(&mut self, event: &InputEvent, scene: &mut dyn InputScene) {
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
                    return;
                }
                let now = chain(scene, *position);
                self.hover(surface, now);
            }
            InputEvent::PointerLeave { .. } => {
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
                // The chain the matching press went down on: a release
                // clicks the innermost node on both chains (pressed on
                // one button, released on its sibling: their row), and
                // nothing without a press on this surface.
                let down = match (*b, *state) {
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
                if *b == button::LEFT {
                    match state {
                        ButtonState::Pressed => {
                            self.hover(surface, under.clone());
                            for &n in under.iter().rev() {
                                self.send(ToLogic::Flag {
                                    node: n,
                                    flag: NodeFlag::Pressed,
                                    on: true,
                                });
                            }
                            self.pressed.insert(surface, under.clone());
                            // A click focuses the `input` or `list` under it.
                            let target = scene.tree().and_then(|tree| {
                                under.iter().copied().find(|n| {
                                    tree.get(*n).is_some_and(|n| {
                                        matches!(n.kind, NodeKind::Input | NodeKind::List)
                                    })
                                })
                            });
                            // A click on the list an `input` steers with
                            // `nav` leaves the typing on the input.
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
                        }
                        ButtonState::Released => {
                            self.release(surface);
                            self.hover(surface, under.clone());
                        }
                    }
                }
                if *state != ButtonState::Released {
                    return;
                }
                let event = match *b {
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
                self.send(ToLogic::Event { node, event });
                if let Some((list, row)) = activate {
                    self.select(scene, list, Some(row));
                    self.send(ToLogic::Event {
                        node: row,
                        event: NodeEvent::Activate,
                    });
                }
            }
            InputEvent::PointerAxis {
                horizontal,
                vertical,
                position,
                ..
            } => {
                let under = chain(scene, *position);
                if vertical.pixels != 0.0 {
                    scene.scroll(surface, *position, vertical.pixels as f32);
                }
                self.send(ToLogic::Event {
                    node: under[0],
                    event: NodeEvent::Scroll {
                        dy: vertical.pixels,
                        dx: horizontal.pixels,
                    },
                });
            }
            InputEvent::KeyboardEnter { .. } => {
                let first = scene
                    .tree()
                    .and_then(|t| first_with_focus(t, root))
                    .unwrap_or(root);
                self.set_focus(surface, Some(first));
            }
            InputEvent::KeyboardLeave { .. } => {
                self.set_focus(surface, None);
                // Focus loss closes a surface bound `open: <-> x`.
                if bound_open(scene.tree(), root) {
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

    /// Forgets selections and edits of nodes that are gone (a list
    /// refilled by its `for`, an `input` in an `if` that turned false,
    /// a reload), and a selection whose row left its list.
    fn prune(&mut self, tree: Option<&SceneTree>) {
        let Some(tree) = tree else {
            return;
        };
        self.selected.retain(|list, row| {
            tree.get(*list)
                .is_some_and(|l| tree.get(*row).is_some_and(|r| r.parent == Some(l.id)))
        });
        self.edits.retain(|n, _| tree.contains(*n));
        self.focus.retain(|_, n| tree.contains(*n));
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
            self.send(ToLogic::Flag {
                node: o,
                flag: NodeFlag::Focused,
                on: false,
            });
        }
        if let Some(n) = node {
            self.send(ToLogic::Flag {
                node: n,
                flag: NodeFlag::Focused,
                on: true,
            });
        }
    }

    /// `open: false` on the surface node `root` (Escape, click-away, focus
    /// loss); a popup is also told `dismiss`.
    fn close(&mut self, scene: &dyn InputScene, root: NodeId) {
        self.send(ToLogic::Write {
            node: root,
            prop: Prop::Open,
            value: PropValue::Bool(false),
        });
        if scene
            .tree()
            .and_then(|t| t.get(root))
            .is_some_and(|n| n.kind == NodeKind::Popup)
        {
            self.send(ToLogic::Event {
                node: root,
                event: NodeEvent::Dismiss,
            });
        }
    }

    /// A key pressed on `surface` (see [`Forward::input`]).
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
        let open = bound_open(scene.tree(), root);
        self.send(ToLogic::Event {
            node: focus,
            event: NodeEvent::Key {
                name: key.name.clone(),
                text: key.text.clone(),
                modifiers: key.modifiers,
            },
        });
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
                        self.send(ToLogic::Event {
                            node: row,
                            event: NodeEvent::Activate,
                        });
                    }
                    return;
                }
                _ => {}
            }
        }
        if kind == Some(NodeKind::Input) {
            let m = key.modifiers;
            let mut now = match self.edits.get(&focus) {
                Some((sent, at)) if *sent != text && at.elapsed() < EDIT_IN_FLIGHT => sent.clone(),
                _ => text,
            };
            let edited = if key.name == "BackSpace" {
                now.pop().is_some()
            } else if !key.text.is_empty() && !m.ctrl && !m.alt && !m.logo {
                now.push_str(&key.text);
                true
            } else {
                false
            };
            if edited {
                self.edits.insert(focus, (now.clone(), Instant::now()));
                self.send(ToLogic::Write {
                    node: focus,
                    prop: Prop::Text,
                    value: PropValue::Text(now),
                });
            }
        }
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
            self.send(ToLogic::Flag {
                node: o,
                flag: NodeFlag::Selected,
                on: false,
            });
        }
        if let Some(r) = row {
            self.send(ToLogic::Flag {
                node: r,
                flag: NodeFlag::Selected,
                on: true,
            });
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
                self.send(ToLogic::Flag {
                    node: n,
                    flag: NodeFlag::Hover,
                    on: false,
                });
            }
        }
        for &n in now.iter().rev() {
            if !was.contains(&n) {
                self.send(ToLogic::Flag {
                    node: n,
                    flag: NodeFlag::Hover,
                    on: true,
                });
            }
        }
        if !now.is_empty() {
            self.hovered.insert(surface, now);
        }
    }

    /// The left button is up (or the pointer left): nothing is pressed.
    fn release(&mut self, surface: SurfaceId) {
        for n in self.pressed.remove(&surface).unwrap_or_default() {
            self.send(ToLogic::Flag {
                node: n,
                flag: NodeFlag::Pressed,
                on: false,
            });
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

/// True if the surface node `root` has `open` (bound two-way or not:
/// a write to a one-way `open` is refused by logic).
fn bound_open(tree: Option<&SceneTree>, root: NodeId) -> bool {
    tree.and_then(|t| t.get(root))
        .is_some_and(|n| n.get(Prop::Open).is_some())
}

/// The `list` and its row on a hit chain (innermost first).
fn list_row(tree: Option<&SceneTree>, chain: &[NodeId]) -> Option<(NodeId, NodeId)> {
    let tree = tree?;
    chain.windows(2).find_map(|w| {
        let parent = tree.get(w[1])?;
        (parent.kind == NodeKind::List).then_some((w[1], w[0]))
    })
}

impl Host {
    pub fn new(renderer: Renderer, log_damage: bool) -> Self {
        Self {
            renderer,
            log_damage,
            logic: None,
            #[cfg(test)]
            probe: None,
        }
    }

    /// Forward monitors, surface sizes and surface input to a logic
    /// thread (`strand run`).
    pub fn forwarding(mut self, tx: Sender<ToLogic>) -> Self {
        self.logic = Some(Forward::new(tx));
        self.renderer.set_query_wait(strand_render::QUERY_WAIT);
        self
    }

    /// Hands laid-out sizes that changed to logic (`self.width`).
    pub(crate) fn forward_facts(&mut self) {
        let facts = self.renderer.take_layout_facts();
        if !facts.is_empty()
            && let Some(f) = &self.logic
        {
            f.send(ToLogic::Layout {
                seq: self.renderer.layout_seq(),
                sizes: facts,
            });
        }
    }
}

impl Painter for Host {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let damage = self.renderer.paint(surface, target);
        self.forward_facts();
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.painted(surface, !damage.is_empty(), target.scale, &self.renderer);
            if !damage.is_empty() {
                p.0.frame(surface, target);
            }
        }
        if !damage.is_empty() && self.log_damage {
            let rects: Vec<String> = damage
                .rects()
                .iter()
                .map(|r| format!("{}x{}+{}+{}", r.w, r.h, r.x, r.y))
                .collect();
            // One line per painted frame, parsed by scripts/m0-exit.sh;
            // strand-surface commits it unless `frame_dropped` follows.
            eprintln!(
                "strand: damage surface={} buffer={}x{} scale={} age={} area={} rects={}",
                surface.0,
                target.size.w,
                target.size.h,
                target.scale.as_f64(),
                target.age,
                damage.area(),
                rects.join(","),
            );
        }
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.renderer.wants_frame(surface)
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.renderer.opaque_region(surface)
    }
}

impl SurfaceHost for Host {
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, monitor: Option<&Monitor>) {
        if let Some(m) = monitor {
            log::info!(
                "surface {} on {}",
                surface.0,
                m.connector.as_deref().unwrap_or(m.id.as_str())
            );
        }
        self.renderer.attach_surface(surface, node);
        if let Some(f) = &mut self.logic {
            f.attached(surface, node);
        }
    }

    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.configured(surface);
        }
        self.renderer.configure_surface(surface, size, scale);
        if let Some(f) = &self.logic {
            f.configured(surface, size, scale);
        }
        // Its first layout's sizes: a container query answers before the
        // first frame (the renderer holds it for that).
        self.forward_facts();
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        log::info!("surface {} detached", surface.0);
        self.renderer.detach_surface(surface);
        if let Some(f) = &mut self.logic {
            f.detached(surface);
        }
    }

    fn monitor_added(&mut self, monitor: &Monitor, _reconnected: bool) {
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.monitor();
        }
        if let Some(f) = &mut self.logic {
            f.monitor_added(monitor);
        }
    }

    fn monitor_changed(&mut self, monitor: &Monitor) {
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.monitor();
        }
        if let Some(f) = &mut self.logic {
            f.monitor_changed(monitor);
        }
    }

    fn monitor_removed(&mut self, monitor: &Monitor) {
        if let Some(f) = &mut self.logic {
            f.monitor_removed(monitor);
        }
    }

    fn monitor_forgotten(&mut self, monitor: &Monitor) {
        if let Some(f) = &mut self.logic {
            f.monitor_forgotten(monitor);
        }
    }

    fn input(&mut self, event: &InputEvent) {
        if let Some(f) = &mut self.logic {
            f.input(event, &mut self.renderer);
        }
        // A scroll lays out again: its sizes go with it.
        self.forward_facts();
    }

    fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        self.renderer.frame_deadline(surface)
    }

    fn frame_dropped(&mut self, surface: SurfaceId) {
        if self.log_damage {
            eprintln!("strand: dropped surface={}", surface.0);
        }
        self.renderer.invalidate(surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::AxisDelta;
    use strand_surface::MonitorId;

    fn monitor(model: &str, connector: &str) -> Monitor {
        Monitor {
            id: MonitorId::new("Make", model, "Display"),
            connector: Some(connector.into()),
            make: "Make".into(),
            model: model.into(),
            description: "Display".into(),
            scale: Scale::ONE,
            logical_size: Some((1920, 1080)),
            position: None,
        }
    }

    /// Read what the forwarder sent, through a calloop loop.
    fn drain(rx: &mut calloop::EventLoop<'static, Vec<ToLogic>>) -> Vec<ToLogic> {
        let mut out = Vec::new();
        rx.dispatch(Some(std::time::Duration::ZERO), &mut out)
            .unwrap();
        out
    }

    fn setup() -> (Forward, calloop::EventLoop<'static, Vec<ToLogic>>) {
        let (tx, rx) = calloop::channel::channel();
        let el = calloop::EventLoop::<Vec<ToLogic>>::try_new().unwrap();
        el.handle()
            .insert_source(rx, |e, _, out: &mut Vec<ToLogic>| {
                if let calloop::channel::Event::Msg(m) = e {
                    out.push(m);
                }
            })
            .unwrap();
        (Forward::new(tx), el)
    }

    /// Surface-level input and sizes become the logic thread's messages
    /// on the surface's node: hover on enter, pressed while the left
    /// button is down, `click`/`secondary` on release, `scroll(dy, dx)`,
    /// the logical size (buffer over scale); other buttons and motion
    /// send nothing, nor does a surface the forwarder does not know.
    #[test]
    fn surface_input_becomes_logic_messages() {
        let (mut f, mut el) = setup();
        let s = SurfaceId(7);
        let node = NodeId::new(3, 0);
        f.attached(s, node);
        f.configured(s, Size::new(2560, 50), Scale::from_f64(1.25).unwrap());
        let at = LogicalPoint::new(10.0, 5.0);
        let button = |b, state| InputEvent::PointerButton {
            surface: s,
            position: at,
            button: b,
            state,
            time: 0,
        };
        let root_only = |_: SurfaceId, _: LogicalPoint| Vec::new();
        for e in [
            InputEvent::PointerEnter {
                surface: s,
                position: at,
            },
            InputEvent::PointerMotion {
                surface: s,
                position: at,
                time: 0,
            },
            button(button::LEFT, ButtonState::Pressed),
            button(button::LEFT, ButtonState::Released),
            button(button::RIGHT, ButtonState::Pressed),
            button(button::RIGHT, ButtonState::Released),
            button(button::MIDDLE, ButtonState::Released),
            InputEvent::PointerAxis {
                surface: s,
                position: at,
                horizontal: AxisDelta::default(),
                vertical: AxisDelta {
                    pixels: 15.0,
                    value120: 120,
                    stop: false,
                },
                source: None,
                time: 0,
            },
            InputEvent::PointerLeave { surface: s },
            InputEvent::PointerEnter {
                surface: SurfaceId(8),
                position: at,
            },
        ] {
            f.input(&e, &mut HitOnly(root_only));
        }
        let flag = |flag, on| ToLogic::Flag { node, flag, on };
        let event = |event: NodeEvent| ToLogic::Event { node, event };
        assert_eq!(
            drain(&mut el),
            vec![
                ToLogic::Size {
                    node,
                    width: 2048.0,
                    height: 40.0
                },
                flag(NodeFlag::Hover, true),
                flag(NodeFlag::Pressed, true),
                flag(NodeFlag::Pressed, false),
                event(NodeEvent::Click),
                event(NodeEvent::Secondary),
                event(NodeEvent::Scroll { dy: 15.0, dx: 0.0 }),
                flag(NodeFlag::Hover, false),
            ]
        );
        f.detached(s);
        f.input(
            &button(button::LEFT, ButtonState::Released),
            &mut HitOnly(root_only),
        );
        assert!(drain(&mut el).is_empty());
    }

    /// Inside a surface the node under the pointer gets the input: its
    /// whole chain is hovered (left nodes lose it), the chain under a
    /// press is pressed and keeps `hover` until the release, and clicks
    /// and scrolls go to the innermost node.
    #[test]
    fn input_goes_to_the_hit_node() {
        let (mut f, mut el) = setup();
        let s = SurfaceId(1);
        let (root, row, a, b) = (
            NodeId::new(1, 0),
            NodeId::new(2, 0),
            NodeId::new(3, 0),
            NodeId::new(4, 0),
        );
        f.attached(s, root);
        // x < 10: over `a` in `row`; x < 20: over `b` in `row`; else bare.
        let hit = move |_: SurfaceId, p: LogicalPoint| {
            if p.x < 10.0 {
                vec![a, row, root]
            } else if p.x < 20.0 {
                vec![b, row, root]
            } else {
                vec![root]
            }
        };
        let at = |x| LogicalPoint::new(x, 1.0);
        let motion = |x| InputEvent::PointerMotion {
            surface: s,
            position: at(x),
            time: 0,
        };
        let button = |x, state| InputEvent::PointerButton {
            surface: s,
            position: at(x),
            button: button::LEFT,
            state,
            time: 0,
        };
        f.input(
            &InputEvent::PointerEnter {
                surface: s,
                position: at(5.0),
            },
            &mut HitOnly(hit),
        );
        f.input(&motion(15.0), &mut HitOnly(hit));
        f.input(&button(15.0, ButtonState::Pressed), &mut HitOnly(hit));
        // Dragging out keeps hover latched on the pressed chain.
        f.input(&motion(30.0), &mut HitOnly(hit));
        f.input(&button(30.0, ButtonState::Released), &mut HitOnly(hit));
        let flag = |node, flag, on| ToLogic::Flag { node, flag, on };
        use NodeFlag::{Hover, Pressed};
        assert_eq!(
            drain(&mut el),
            vec![
                flag(root, Hover, true),
                flag(row, Hover, true),
                flag(a, Hover, true),
                flag(a, Hover, false),
                flag(b, Hover, true),
                flag(root, Pressed, true),
                flag(row, Pressed, true),
                flag(b, Pressed, true),
                flag(b, Pressed, false),
                flag(row, Pressed, false),
                flag(root, Pressed, false),
                flag(b, Hover, false),
                flag(row, Hover, false),
                ToLogic::Event {
                    node: root,
                    event: NodeEvent::Click
                },
            ]
        );
        // Pressed on `a`, released on `b`: their row is clicked, not `b`.
        f.input(&button(5.0, ButtonState::Pressed), &mut HitOnly(hit));
        f.input(&button(15.0, ButtonState::Released), &mut HitOnly(hit));
        let clicks: Vec<ToLogic> = drain(&mut el)
            .into_iter()
            .filter(|m| matches!(m, ToLogic::Event { .. }))
            .collect();
        assert_eq!(
            clicks,
            [ToLogic::Event {
                node: row,
                event: NodeEvent::Click
            }]
        );
        // A release with no press (the press was on another surface):
        // no click.
        f.input(&button(5.0, ButtonState::Released), &mut HitOnly(hit));
        assert!(
            !drain(&mut el)
                .iter()
                .any(|m| matches!(m, ToLogic::Event { .. }))
        );
        // Scrolls go to the innermost node under the pointer, `dy` from
        // the vertical axis and `dx` from the horizontal one.
        let scroll = |x, dy, dx| InputEvent::PointerAxis {
            surface: s,
            position: at(x),
            horizontal: AxisDelta {
                pixels: dx,
                value120: 0,
                stop: false,
            },
            vertical: AxisDelta {
                pixels: dy,
                value120: 0,
                stop: false,
            },
            source: None,
            time: 0,
        };
        f.input(&scroll(15.0, 3.0, -2.0), &mut HitOnly(hit));
        f.input(&scroll(5.0, -1.0, 0.0), &mut HitOnly(hit));
        f.input(&scroll(30.0, 0.0, 4.0), &mut HitOnly(hit));
        // Right clicks are `secondary` on the innermost node under both
        // the press and the release, like left clicks.
        let right = |x, state| InputEvent::PointerButton {
            surface: s,
            position: at(x),
            button: button::RIGHT,
            state,
            time: 0,
        };
        f.input(&right(15.0, ButtonState::Pressed), &mut HitOnly(hit));
        f.input(&right(15.0, ButtonState::Released), &mut HitOnly(hit));
        f.input(&right(5.0, ButtonState::Pressed), &mut HitOnly(hit));
        f.input(&right(15.0, ButtonState::Released), &mut HitOnly(hit));
        // A right release with no right press: nothing.
        f.input(&right(15.0, ButtonState::Released), &mut HitOnly(hit));
        let events: Vec<ToLogic> = drain(&mut el)
            .into_iter()
            .filter(|m| matches!(m, ToLogic::Event { .. }))
            .collect();
        let event = |node, event| ToLogic::Event { node, event };
        assert_eq!(
            events,
            [
                event(b, NodeEvent::Scroll { dy: 3.0, dx: -2.0 }),
                event(a, NodeEvent::Scroll { dy: -1.0, dx: 0.0 }),
                event(root, NodeEvent::Scroll { dy: 0.0, dx: 4.0 }),
                event(b, NodeEvent::Secondary),
                event(row, NodeEvent::Secondary),
            ]
        );
    }

    /// Keyboard routing on a launcher-like panel: focus goes to the
    /// `focus: true` input, typing writes its `text`, arrows move the
    /// selection of the list its `nav` names, Return activates the
    /// selected row, Escape and focus loss write `open: false`; a click on
    /// a list row activates it.
    #[test]
    fn keys_go_to_the_focused_input_and_its_list() {
        use strand_scene::{Color, Modifiers, NodeKind, SceneDiff};
        let data = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(data),
        ]));
        let mut r = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let id = |i| NodeId::new(i, 0);
        let (panel, col, input, list) = (id(0), id(1), id(2), id(3));
        let rows = [id(4), id(5), id(6)];
        let mut d = SceneDiff::new();
        d.create(panel, NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, PropValue::Number(200.0))
            .set(panel, Prop::Height, PropValue::Number(120.0))
            .set(panel, Prop::Open, PropValue::Bool(true))
            .set(panel, Prop::Bg, PropValue::Color(Color::WHITE))
            .create(col, NodeKind::Col, Some(panel), 0)
            .create(input, NodeKind::Input, Some(col), 0)
            .set(input, Prop::Focus, PropValue::Bool(true))
            .set(input, Prop::Nav, PropValue::Node(list))
            .set(input, Prop::Text, PropValue::Text(String::new()))
            .create(list, NodeKind::List, Some(col), 1);
        for (i, row) in rows.iter().enumerate() {
            d.create(*row, NodeKind::Row, Some(list), i as u32).set(
                *row,
                Prop::Height,
                PropValue::Number(20.0),
            );
        }
        assert!(r.apply(d).is_empty());
        let s = SurfaceId(1);
        r.attach_surface(s, panel);
        let mut px = vec![0u8; 200 * 120 * 4];
        let mut t = PaintTarget::new(&mut px, Size::new(200, 120), 800, Scale::ONE, 0).unwrap();
        r.paint(s, &mut t);

        let (mut f, mut el) = setup();
        f.attached(s, panel);
        let key = |name: &str, text: &str| InputEvent::Key {
            surface: s,
            key: KeyInput {
                name: name.into(),
                text: text.into(),
                state: ButtonState::Pressed,
                repeat: false,
                modifiers: Modifiers::default(),
                time: 0,
            },
        };
        f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
        for k in [
            key("f", "f"),
            key("o", "o"),
            key("BackSpace", ""),
            key("Down", ""),
            key("Down", ""),
            key("Return", ""),
            key("Escape", ""),
        ] {
            f.input(&k, &mut r);
        }
        f.input(&InputEvent::KeyboardLeave { surface: s }, &mut r);
        let msgs: Vec<ToLogic> = drain(&mut el)
            .into_iter()
            .filter(|m| {
                !matches!(
                    m,
                    ToLogic::Event {
                        event: NodeEvent::Key { .. },
                        ..
                    }
                )
            })
            .collect();
        let flag = |node, flag, on| ToLogic::Flag { node, flag, on };
        let write = |node, prop, value| ToLogic::Write { node, prop, value };
        let text = |t: &str| write(input, Prop::Text, PropValue::Text(t.into()));
        let close = write(panel, Prop::Open, PropValue::Bool(false));
        assert_eq!(
            msgs,
            vec![
                flag(input, NodeFlag::Focused, true),
                text("f"),
                // Typed before logic answered: from what was written.
                text("fo"),
                text("f"),
                flag(rows[0], NodeFlag::Selected, true),
                flag(rows[0], NodeFlag::Selected, false),
                flag(rows[1], NodeFlag::Selected, true),
                ToLogic::Event {
                    node: rows[1],
                    event: NodeEvent::Activate
                },
                close.clone(),
                flag(input, NodeFlag::Focused, false),
                close,
            ]
        );
        // Keys reach `on key` on the focused node, as `key(k)`.
        f.input(&InputEvent::KeyboardEnter { surface: s }, &mut r);
        f.input(&key("a", "a"), &mut r);
        assert!(drain(&mut el).contains(&ToLogic::Event {
            node: input,
            event: NodeEvent::Key {
                name: "a".into(),
                text: "a".into(),
                modifiers: Modifiers::default()
            }
        }));
        // A click on a list row: clicked, selected and activated.
        let row = r.boxes(s).unwrap().rects[&rows[2]];
        let at = LogicalPoint::new(row.x + 5.0, row.y + 5.0);
        let button = |state| InputEvent::PointerButton {
            surface: s,
            position: at,
            button: button::LEFT,
            state,
            time: 0,
        };
        f.input(&button(ButtonState::Pressed), &mut r);
        f.input(&button(ButtonState::Released), &mut r);
        let msgs = drain(&mut el);
        assert!(msgs.contains(&ToLogic::Event {
            node: rows[2],
            event: NodeEvent::Click
        }));
        assert!(msgs.contains(&flag(rows[2], NodeFlag::Selected, true)));
        assert!(msgs.contains(&ToLogic::Event {
            node: rows[2],
            event: NodeEvent::Activate
        }));
        // The click on the list the input steers left the typing there.
        assert!(!msgs.contains(&flag(list, NodeFlag::Focused, true)));
        f.input(&key("b", "b"), &mut r);
        assert!(
            drain(&mut el).iter().any(
                |m| matches!(m, ToLogic::Write { node, prop: Prop::Text, .. } if *node == input)
            )
        );
        // Rows refilled (the selected one gone): the selection is
        // forgotten, and Down starts again from the first row.
        let mut d = SceneDiff::new();
        d.push(strand_scene::SceneOp::Remove { id: rows[2] });
        let fresh = id(7);
        d.create(fresh, NodeKind::Row, Some(list), 2).set(
            fresh,
            Prop::Height,
            PropValue::Number(20.0),
        );
        assert!(r.apply(d).is_empty());
        f.input(&key("Down", ""), &mut r);
        let msgs = drain(&mut el);
        assert!(
            msgs.contains(&flag(rows[0], NodeFlag::Selected, true)),
            "{msgs:?}"
        );
        assert!(!f.selected.values().any(|r| *r == rows[2]));
    }

    /// Monitors are `screens` in plug order; one that comes back within
    /// 30 s keeps its place (so the first stays `screens.focused`), and a
    /// forgotten one is dropped and named to the logic thread.
    #[test]
    fn a_returning_monitor_keeps_its_place() {
        let (mut f, mut el) = setup();
        let (a, b) = (monitor("A", "DP-1"), monitor("B", "DP-2"));
        let names = |msgs: Vec<ToLogic>| -> Vec<Vec<String>> {
            msgs.into_iter()
                .map(|m| match m {
                    ToLogic::Screens(list) => list.into_iter().map(|s| s.name).collect(),
                    ToLogic::Forget(id) => vec![format!("forget {id}")],
                    m => panic!("{m:?}"),
                })
                .collect()
        };
        f.monitor_added(&a);
        f.monitor_added(&b);
        f.monitor_removed(&a);
        f.monitor_added(&a);
        assert_eq!(
            names(drain(&mut el)),
            [
                vec!["DP-1"],
                vec!["DP-1", "DP-2"],
                vec!["DP-2"],
                vec!["DP-1", "DP-2"],
            ]
        );
        f.monitor_removed(&a);
        f.monitor_forgotten(&a);
        f.monitor_added(&a);
        assert_eq!(
            names(drain(&mut el)),
            [
                vec!["DP-2".to_string()],
                vec![format!("forget {}", a.id.as_str())],
                vec!["DP-2".to_string(), "DP-1".to_string()],
            ]
        );
    }
}

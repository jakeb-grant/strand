//! The surface host: `strand-surface` calls it, it forwards to the
//! renderer (`docs/architecture.md`, "Render loop"), and under
//! `STRAND_LOG=damage` it logs the damage of every painted frame, and a
//! `dropped` line for a painted frame whose commit then failed.

use std::collections::HashMap;
use std::time::Instant;

use calloop::channel::Sender;
use strand_compiler::instantiate::NodeFlag;
use strand_render::{Flag, InputScene, Intent, NodeEvent as RouteEvent, Renderer, Router};
use strand_scene::{
    BlurRegion, CompositorCaps, Damage, InputEvent, NodeId, PaintTarget, Painter, Prop, Scale,
    SceneDiff, Size, SurfaceId,
};
use strand_surface::{Monitor, SurfaceHost};

use crate::run::{NodeEvent, PickEvent, ScreenInfo, ToLogic};

#[derive(Debug)]
pub struct Host {
    pub renderer: Renderer,
    log_damage: bool,
    /// `strand run`: what the logic thread hears about.
    logic: Option<Forward>,
    /// Wakes the main loop to hand surface changes the renderer made
    /// while the surface manager called in (a configure, a paint, text
    /// collected meanwhile) to the manager at once.
    wake: Option<calloop::ping::Ping>,
    /// The node each surface shows (for the blur fallback's diagnostic).
    roots: HashMap<SurfaceId, NodeId>,
    /// (M4) Where each surface's buffer lies on its output
    /// (`SurfaceHost::surface_placed`): a press becomes the tray's click
    /// point with it.
    origins: HashMap<SurfaceId, (i32, i32)>,
    /// (M4) The output each surface is on (its monitor's identity), for
    /// render's surface origins (shared-element morphs across surfaces).
    outputs: HashMap<SurfaceId, String>,
    /// (m6-owner-fixes, B-d) The pointer the compositor still has on a
    /// surface, taken onto a popup of it that mapped (or grew) under it
    /// ([`Host::rebase_pointer`]).
    rebase: Option<Rebase>,
    /// The surfaces idle before an input event, kept between events so
    /// pointer motion allocates nothing.
    idle: Vec<SurfaceId>,
    /// The blur ladder's last rung: says once why `blur` draws its tint.
    blur_fallback: BlurFallback,
    /// (M4) Handed-off surfaces the manager had to destroy: the GPU host
    /// sends `Release` for each (`run/gpu.rs`).
    pub gpu_released: Vec<SurfaceId>,
    /// (M4) `strand run`: the producers of the visible fed nodes (a
    /// `spectrum`'s audio tap), changed after a paint or a diff changes
    /// them.
    pub(crate) feeds: Option<crate::run::feeds::Feeds>,
    /// The session lock's content and its built-in fallback (`strand
    /// run`'s `run/lock.rs`).
    pub(crate) lock: crate::run::lock::LockScreen,
    /// Tests: told of every paint and monitor change (`bench.rs`,
    /// `fuzz.rs`).
    #[cfg(test)]
    pub(crate) probe: Option<ProbeHandle>,
}

/// (m6-owner-fixes, B-d) A popup mapped, or grew, under a pointer that
/// has not moved since: the compositor still has the pointer on the
/// surface it opens from (sway sends no enter until the pointer moves),
/// while it is over the popup. Strand takes it onto the popup itself, so
/// `hover` reads as the pointer is: the events the compositor sends
/// `from` go to `to`, moved by `d`, until it moves the pointer itself.
#[derive(Clone, Copy, Debug)]
struct Rebase {
    /// The surface the compositor has the pointer on.
    from: SurfaceId,
    /// The popup it is over.
    to: SurfaceId,
    /// From `from`'s coordinates to `to`'s (logical).
    d: (f32, f32),
    /// The pointer on `from`, as last heard.
    at: strand_scene::LogicalPoint,
    /// The compositor sent `from` a leave: an enter of `to` in the same
    /// batch of events ends the rebase where the compositor took over;
    /// none by the batch's end ([`Host::settle_rebase`]), and the pointer
    /// went to another client's surface.
    left: bool,
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

/// The surface layer's facts for a logic thread (`strand run`): monitors
/// as the `screens` service, surface sizes, and what input routing
/// (`strand_render::Router`, on this thread) asks of logic.
#[derive(Debug)]
pub(crate) struct Forward {
    tx: Sender<ToLogic>,
    /// Monitors in plug order; `false`: unplugged, kept for 30 s with its
    /// place, so a monitor that comes back keeps its position (and the
    /// first one stays `screens.focused`).
    monitors: Vec<(Monitor, bool)>,
    /// The node each surface shows.
    surfaces: HashMap<SurfaceId, NodeId>,
    /// Hover, press, focus, selection and edits.
    router: Router,
    /// (M5) In pick mode, the surface the pointer is over: pick mode
    /// ends when it unmaps.
    picking_on: Option<SurfaceId>,
    /// (M5) The sequence number of the last `ToMain::PickMode` logic
    /// sent, which a `PickEvent::Ended` carries back so logic can tell
    /// an end it has since overtaken.
    pick_seq: u64,
    /// (M5) The compositor serial of the last button or key press
    /// (`SurfaceHost::press_serial`): the press the node events it
    /// causes carry to logic, for xdg-activation tokens.
    press: Option<u32>,
    /// (M5) `press` was told since the last input event: the next event
    /// is that press. A click away carries a press only then (the
    /// catcher's, told before it); one without (a popup the compositor
    /// dismissed, the user having clicked another client) carries none.
    fresh: bool,
}

impl Forward {
    pub(crate) fn new(tx: Sender<ToLogic>) -> Self {
        Self {
            tx,
            monitors: Vec::new(),
            surfaces: HashMap::new(),
            router: Router::new(),
            picking_on: None,
            pick_seq: 0,
            press: None,
            fresh: false,
        }
    }

    /// (M5) A button or key press with `serial` comes next
    /// (`SurfaceHost::press_serial`).
    pub(crate) fn pressed(&mut self, serial: u32) {
        self.press = Some(serial);
        self.fresh = true;
    }

    /// (M5) An input event went elsewhere (the fallback lock took it).
    pub(crate) fn passed(&mut self) {
        self.fresh = false;
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
        self.router.attached(surface, node);
    }

    /// A surface's logical size: its buffer size over its scale, sent
    /// only when logic reads the surface node's size (`watched`: it
    /// carries `Prop::Watch`, as [`Renderer::take_layout_facts`] asks of
    /// every node). A surface sized to its content resizes whenever its
    /// text changes width (a popup's `pct(cpu.usage)` going from `9%`
    /// to `10%`); nobody measuring it, that wakes logic for nothing
    /// (`docs/decisions.md`, laptop-trim). A node watched later gets
    /// its size from the renderer's layout facts at once.
    pub(crate) fn configured(
        &self,
        surface: SurfaceId,
        size: Size,
        scale: Scale,
        watched: impl Fn(NodeId) -> bool,
    ) {
        if let Some(&node) = self.surfaces.get(&surface)
            && watched(node)
        {
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
        self.router.detached(surface);
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

    /// Routes `event` (`strand_render::Router::handle`) and sends logic
    /// what it asks for: flags, node events (bubbled by logic to the
    /// nearest handler) and two-way writes.
    pub(crate) fn input(&mut self, event: &InputEvent, scene: &mut dyn InputScene) {
        let press = press_of(event, self.press, std::mem::take(&mut self.fresh));
        for intent in self.router.handle(event, scene) {
            if let Some(msg) = to_logic(intent, press) {
                self.send(msg);
            }
        }
    }
}

/// (M5) The press whose handlers `event`'s node events run for, with its
/// serial (`serial`, the last press's): a button's press or release (the
/// click its press started), a key press or its repeat, a click away
/// that is a press (`fresh`: its serial was told just before it, the
/// catcher's). None for a scroll, a drag, a key release, or a click away
/// the compositor sent (a popup's `done`: the press was on another
/// client, and `serial` an older one of ours).
fn press_of(
    event: &InputEvent,
    serial: Option<u32>,
    fresh: bool,
) -> Option<strand_compiler::vm::InputCtx> {
    let surface = match event {
        InputEvent::PointerButton { surface, .. } => *surface,
        InputEvent::ClickAway { surface } if fresh => *surface,
        InputEvent::Key { surface, key } if key.state == strand_scene::ButtonState::Pressed => {
            *surface
        }
        _ => return None,
    };
    Some(strand_compiler::vm::InputCtx {
        surface: surface.0,
        serial: serial?,
    })
}

/// What routing asks of logic, as the logic thread's message, its node
/// events carrying `press` (M5). `None` for an intent logic cannot take
/// yet: a drop (M4), until S-lists adds `NodeEvent::Drop` on the logic
/// side.
fn to_logic(intent: Intent, press: Option<strand_compiler::vm::InputCtx>) -> Option<ToLogic> {
    Some(match intent {
        Intent::Flag { node, flag, on } => ToLogic::Flag {
            node,
            flag: match flag {
                Flag::Hover => NodeFlag::Hover,
                Flag::Pressed => NodeFlag::Pressed,
                Flag::Focused => NodeFlag::Focused,
                Flag::Selected => NodeFlag::Selected,
            },
            on,
        },
        Intent::Event { node, event } => ToLogic::Event {
            node,
            input: press,
            event: match event {
                RouteEvent::Click => NodeEvent::Click,
                RouteEvent::Secondary => NodeEvent::Secondary,
                RouteEvent::Middle => NodeEvent::Middle,
                RouteEvent::Scroll { dy, dx } => NodeEvent::Scroll { dy, dx },
                RouteEvent::Activate => NodeEvent::Activate,
                RouteEvent::Key {
                    name,
                    text,
                    modifiers,
                } => NodeEvent::Key {
                    name,
                    text,
                    modifiers,
                },
                RouteEvent::Dismiss => NodeEvent::Dismiss,
                RouteEvent::Drop {
                    payload,
                    at,
                    hidden,
                } => NodeEvent::Drop {
                    payload,
                    at,
                    hidden,
                },
            },
        },
        Intent::Write { node, prop, value } => ToLogic::Write {
            node,
            prop,
            value,
            input: press,
        },
    })
}

/// The tray's click point for a press at `position` on a surface whose
/// buffer lies at `origin` on its output: the bottom-left corner of the
/// pressed node's box `rect` (in the surface), where an app placing a
/// menu of its own puts its top-left corner (and flips it up from a
/// bottom bar), in the output's logical pixels; the press itself when no
/// node was hit. `scale` is the surface's delegated pose scale: the
/// compositor draws the content at rest that much smaller from the
/// surface's top-left corner (`strand_render::pose`).
fn click_point(
    origin: (i32, i32),
    rect: Option<strand_scene::LogicalRect>,
    position: strand_scene::LogicalPoint,
    scale: f32,
) -> (i32, i32) {
    let (x, y) = rect.map_or((position.x, position.y), |r| (r.x, r.y + r.h));
    let scale = if scale.is_finite() && scale > 0.0 {
        f64::from(scale)
    } else {
        1.0
    };
    let at = |o: i32, v: f32| {
        (f64::from(o) + (f64::from(v) * scale).round())
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    };
    (at(origin.0, x), at(origin.1, y))
}

/// A monitor's logical size: no content-sized surface on it is larger.
fn monitor_bounds(m: &Monitor) -> Option<strand_scene::LogicalSize> {
    m.logical_size
        .filter(|(w, h)| *w > 0 && *h > 0)
        .map(|(w, h)| strand_scene::LogicalSize::new(w as f32, h as f32))
}

/// The blur ladder's last rung (design.md, "Blur ladder"): when the
/// compositor cannot blur, `blur` draws its tint, and the first frame
/// that asks for blur says why, once (a warning and a `strand watch`
/// notice; the inspector's per-node answer is M5's). The reason is the
/// compositor's, the same for every node with `blur`.
#[derive(Debug, Default)]
struct BlurFallback {
    /// The compositor's capabilities (`None`: not reported yet; on
    /// Hyprland the reason names `strand compositor-rules`).
    caps: Option<CompositorCaps>,
    /// (M4) How many blur regions each surface's last frame had: its
    /// `blur` boxes are told to logic when that changes.
    regions: HashMap<SurfaceId, usize>,
    said: bool,
}

impl BlurFallback {
    /// True while a frame that asks for blur would be said: not yet said,
    /// and the compositor is known not to blur. Checked before anything
    /// is asked of the renderer, so a compositor that blurs (or one not
    /// reported yet) costs a paint nothing.
    #[cfg(test)]
    fn pending(&self) -> bool {
        !self.said
            && self
                .caps
                .is_some_and(|c| strand_surface::caps::blur_fallback_reason(&c).is_some())
    }

    /// The diagnostic for a frame of the surface `ns` that asks for blur:
    /// once, and only when the compositor is known not to blur. The
    /// reason says what `blur` draws instead and that `blur_fallback:
    /// none` turns it off (that is the user's choice, not a fallback).
    fn frame(&mut self, ns: &str) -> Option<String> {
        if self.said {
            return None;
        }
        let reason = strand_surface::caps::blur_fallback_reason(&self.caps?)?;
        self.said = true;
        Some(format!("no blur behind {ns}: {reason}"))
    }
}

/// The nodes of `root`'s surface (nested surfaces left out) that ask for
/// blur: each with its kind and whether it draws the tint fallback.
fn blur_nodes(
    tree: &strand_render::SceneTree,
    root: NodeId,
) -> Vec<(NodeId, strand_scene::NodeKind, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let Some(n) = tree.get(id) else {
            continue;
        };
        if id != root && n.kind.is_surface() {
            continue;
        }
        let blur = match n.get(Prop::Blur) {
            Some(strand_scene::PropValue::Number(b)) => *b > 0.0,
            Some(_) => true,
            None => false,
        };
        if blur {
            let none = matches!(
                n.get(Prop::BlurFallback),
                Some(strand_scene::PropValue::Keyword(k)) if k == "none"
            );
            out.push((id, n.kind, !none));
        }
        stack.extend(n.children.iter().rev());
    }
    out
}

impl Host {
    pub fn new(mut renderer: Renderer, log_damage: bool) -> Self {
        // A compositor answers a resize: a content-sized surface waits for
        // it rather than painting one frame at its old size.
        renderer.set_resize_wait(strand_render::RESIZE_WAIT);
        Self {
            renderer,
            log_damage,
            logic: None,
            wake: None,
            roots: HashMap::new(),
            origins: HashMap::new(),
            outputs: HashMap::new(),
            rebase: None,
            idle: Vec::new(),
            blur_fallback: BlurFallback::default(),
            gpu_released: Vec::new(),
            feeds: None,
            lock: crate::run::lock::LockScreen::default(),
            #[cfg(test)]
            probe: None,
        }
    }

    /// Says once, as a warning and a `strand watch` notice, why `blur`
    /// falls back to its tint, the first time a frame of `surface` asks
    /// the compositor to blur and it cannot; and (M4, the inspector's
    /// half) tells logic the surface's `blur` boxes whenever their count
    /// changes, for a notice per box naming its place in the source.
    fn note_blur_fallback(&mut self, surface: SurfaceId) {
        if !self
            .blur_fallback
            .caps
            .is_some_and(|c| !c.background_effect)
        {
            return;
        }
        let regions = self.renderer.blur_region(surface).len();
        let before = self.blur_fallback.regions.insert(surface, regions);
        if regions == 0 || before == Some(regions) {
            return;
        }
        let root = self.roots.get(&surface).copied();
        let ns = root
            .and_then(|n| self.renderer.surface_spec(n))
            .map_or_else(|| format!("surface {}", surface.0), |s| s.namespace());
        if let Some(text) = self.blur_fallback.frame(&ns) {
            log::warn!("{text}");
            if let Some(f) = &self.logic {
                f.send(ToLogic::Notice(text));
            }
        }
        let why = self
            .blur_fallback
            .caps
            .as_ref()
            .and_then(strand_surface::caps::blur_missing);
        if let (Some(root), Some(why), Some(f)) = (root, why, &self.logic) {
            let nodes = blur_nodes(self.renderer.tree(), root);
            if !nodes.is_empty() {
                f.send(ToLogic::BlurFallback {
                    surface: ns,
                    nodes,
                    why,
                });
            }
        }
    }

    /// Forward monitors, surface sizes and surface input to a logic
    /// thread (`strand run`).
    pub fn forwarding(mut self, tx: Sender<ToLogic>) -> Self {
        self.logic = Some(Forward::new(tx));
        self.renderer.set_query_wait(strand_render::QUERY_WAIT);
        self
    }

    /// Wake the main loop with `ping` (its handler syncs surface
    /// changes, see `demo::text_ready`) whenever a call from the surface
    /// manager leaves surface changes behind.
    pub fn waking(mut self, ping: calloop::ping::Ping) -> Self {
        self.wake = Some(ping);
        self
    }

    fn wake_if_changed(&self) {
        if self.renderer.has_surface_changes()
            && let Some(p) = &self.wake
        {
            p.ping();
        }
    }

    /// (M5) Pick mode on or off (`inspect.pick`): the router passes
    /// pointer input to no handler and the renderer outlines the node
    /// under the pointer (at once, where the pointer already is); off,
    /// the outline goes. `seq` is the `ToMain::PickMode`'s.
    pub(crate) fn set_picking(&mut self, on: bool, seq: u64) {
        let Some(f) = &mut self.logic else {
            return;
        };
        f.pick_seq = seq;
        for intent in f.router.set_picking(on, &mut self.renderer) {
            if let Some(msg) = to_logic(intent, None) {
                f.send(msg);
            }
        }
        if !on {
            f.picking_on = None;
            self.renderer.pick_ended();
            return;
        }
        let under = f
            .surfaces
            .keys()
            .find_map(|s| Some((*s, f.router.pointer(*s)?)));
        if let Some((surface, position)) = under {
            f.picking_on = Some(surface);
            self.renderer.pick_input(&InputEvent::PointerMotion {
                surface,
                position,
                time: 0,
            });
        }
    }

    /// (M6) The `trigger: hover` popups whose delay or grace is up open
    /// or close (`Router::tick`): logic hears of it, and the next one's
    /// time is the renderer's input wake. Run on every wake of the loop
    /// ([`crate::demo::text_ready`]) and after a surface detaches.
    pub(crate) fn tick_input(&mut self) {
        let Some(f) = &mut self.logic else {
            return;
        };
        for intent in f.router.tick(&mut self.renderer) {
            if let Some(msg) = to_logic(intent, None) {
                f.send(msg);
            }
        }
        self.renderer.set_input_wake(f.router.next_wake());
    }

    /// (M5) Pick mode ends on main's side (the surface picked on
    /// unmapped, a lock): logic is told.
    fn end_pick(&mut self) {
        if !self.logic.as_ref().is_some_and(|f| f.router.picking()) {
            return;
        }
        let seq = self.logic.as_ref().map_or(0, |f| f.pick_seq);
        self.set_picking(false, seq);
        if let Some(f) = &self.logic {
            f.send(ToLogic::Picked(PickEvent::Ended { seq }));
        }
    }

    /// (M5) A lock is asked for or held: pick mode ends and nothing is
    /// outlined.
    pub(crate) fn lock_started(&mut self) {
        self.end_pick();
        self.renderer.set_highlight(None);
    }

    /// Logic's next diff, before the renderer applies it: `input` texts
    /// it sets answer (or override) the router's edits in flight.
    pub(crate) fn observe(&mut self, diff: &SceneDiff) {
        if let Some(f) = &mut self.logic {
            f.router.observe(diff);
        }
    }

    /// Applies logic's diff to the renderer; returns the ops that failed.
    /// Fed nodes' producers follow it at once: a diff can change what a
    /// shown node is fed by with no frame following (a resting
    /// `spectrum` draws the same dots whichever device its source names,
    /// so `update` finds nothing to paint and no paint's
    /// [`Host::painted`] would ever retarget its tap).
    pub(crate) fn apply_diff(&mut self, diff: SceneDiff) -> Vec<strand_render::SceneError> {
        let errors = self.renderer.apply(diff);
        if let Some(feeds) = &mut self.feeds {
            crate::run::feeds::sync(&mut self.renderer, feeds);
        }
        errors
    }

    /// After logic's diff is applied: what routing settles on the new
    /// scene (`Router::settle`: a focused input's list selects its first
    /// row) goes to logic.
    pub(crate) fn settle_input(&mut self) {
        if let Some(f) = &mut self.logic {
            for intent in f.router.settle(&mut self.renderer) {
                if let Some(msg) = to_logic(intent, None) {
                    f.send(msg);
                }
            }
            // (M6) Settling forgets the hover popups logic removed, and
            // their graces.
            self.renderer.set_input_wake(f.router.next_wake());
        }
    }

    /// Asks logic for the rows of virtualised lists that scrolled past
    /// their mounted rows (`ToLogic::ListWindow`).
    pub(crate) fn forward_list_windows(&mut self) {
        let Some(f) = &self.logic else {
            return;
        };
        for (list, rows) in self.renderer.take_list_windows() {
            f.send(ToLogic::ListWindow {
                list,
                first: rows.start,
                count: rows.end.saturating_sub(rows.start),
            });
        }
    }

    /// Hands laid-out boxes that changed to logic (`self.width`, `id.box`).
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

impl Host {
    /// What follows every painted frame, the CPU's or (M4) one the GPU
    /// thread presents (`run/gpu.rs`): the lock's and the blur
    /// fallback's bookkeeping, effect notices, layout facts and list
    /// windows to logic, and fed nodes' producers. The frame's `damage`
    /// log line is [`Host::log_frame`]'s: a CPU frame's at once, a
    /// presented one's when the GPU thread says it presented it.
    pub(crate) fn painted(&mut self, surface: SurfaceId, damage: &Damage) {
        self.lock.painted(surface, damage);
        self.note_blur_fallback(surface);
        // (M4) CPU fallbacks for GPU effects, once a run each.
        for text in self.renderer.take_effect_notices() {
            log::warn!("{text}");
            if let Some(f) = &self.logic {
                f.send(ToLogic::Notice(text));
            }
        }
        self.forward_facts();
        // Virtualised lists scrolled past their mounted rows ask logic
        // for the rows they show.
        self.forward_list_windows();
        // Fed nodes shown or hidden: their producers start or stop.
        if let Some(feeds) = &mut self.feeds {
            crate::run::feeds::sync(&mut self.renderer, feeds);
        }
        self.rebase_pointer(surface);
        self.wake_if_changed();
    }

    /// (m6-owner-fixes, B-d) `popup` painted a frame: if the pointer is
    /// still on the surface it opens from (directly or through other
    /// popups), on the same output, and over where `popup` now takes
    /// input, the pointer is taken onto it ([`Rebase`]): the surface it
    /// was on hears a leave and the popup an enter, as the compositor
    /// would send them on the pointer's next motion. Not while a button
    /// is held there (the compositor keeps the pointer on the pressed
    /// surface), during a drag, or in pick mode.
    fn rebase_pointer(&mut self, popup: SurfaceId) {
        if self.rebase.is_some() || self.lock.shown_on(popup) {
            return;
        }
        let Some(f) = &self.logic else {
            return;
        };
        if f.router.picking() || f.router.drag().is_some() {
            return;
        }
        let Some(&to_node) = f.surfaces.get(&popup) else {
            return;
        };
        let Some((from, at)) = f
            .surfaces
            .keys()
            .copied()
            .filter(|s| *s != popup)
            .find_map(|s| Some((s, f.router.pointer(s)?)))
        else {
            return;
        };
        let Some(&from_node) = f.surfaces.get(&from) else {
            return;
        };
        if f.router.held(from) || self.outputs.get(&from) != self.outputs.get(&popup) {
            return;
        }
        let tree = self.renderer.tree();
        if tree.get(to_node).map(|n| n.kind) != Some(strand_scene::NodeKind::Popup) {
            return;
        }
        let mut up = tree.get(to_node).and_then(|n| n.parent);
        let mut opens_from = false;
        for _ in 0..4096 {
            match up {
                Some(n) if n == from_node => {
                    opens_from = true;
                    break;
                }
                Some(n) => up = tree.get(n).and_then(|n| n.parent),
                None => break,
            }
        }
        let (Some(&a), Some(&b)) = (self.origins.get(&from), self.origins.get(&popup)) else {
            return;
        };
        if !opens_from {
            return;
        }
        let d = ((a.0 - b.0) as f32, (a.1 - b.1) as f32);
        let q = strand_scene::LogicalPoint::new(at.x + d.0, at.y + d.1);
        if !self.renderer.takes_input_at(popup, q) {
            return;
        }
        log::debug!(
            "pointer taken from surface {} onto popup {} under it",
            from.0,
            popup.0
        );
        // Heard as the compositor's own would be, before the rebase
        // starts: `rebase_event` would take them for the compositor's.
        self.input(&InputEvent::PointerLeave { surface: from });
        self.input(&InputEvent::PointerEnter {
            surface: popup,
            position: q,
        });
        self.rebase = Some(Rebase {
            from,
            to: popup,
            d,
            at,
            left: false,
        });
    }

    /// (m6-owner-fixes, B-d) The end of a batch of compositor events
    /// (the main loop calls it after each dispatch): a rebase whose
    /// `from` the compositor left with no enter of the popup in the same
    /// batch ends, as the pointer went to another client's surface; the
    /// popup hears the leave.
    pub(crate) fn settle_rebase(&mut self) {
        if let Some(r) = self.rebase.filter(|r| r.left) {
            self.rebase = None;
            self.input(&InputEvent::PointerLeave { surface: r.to });
        }
    }

    /// (m6-owner-fixes, B-d) What `event` becomes while the pointer is
    /// taken onto a popup ([`Rebase`]); `None`: `event` itself. The
    /// compositor's events for the surface it still has the pointer on go
    /// to the popup, moved, while the pointer is over the popup (or a
    /// button went down there); its leave of that surface is the one
    /// already heard, kept until the batch's end
    /// ([`Host::settle_rebase`]). Its own enter of the popup ends the
    /// rebase, and so does any other enter or leave, or a motion off the
    /// popup: the popup hears a leave first.
    fn rebase_event(&mut self, event: &InputEvent) -> Option<Vec<InputEvent>> {
        use strand_scene::LogicalPoint;
        let r = self.rebase?;
        let moved = |p: &LogicalPoint| LogicalPoint::new(p.x + r.d.0, p.y + r.d.1);
        let leave = InputEvent::PointerLeave { surface: r.to };
        match event {
            InputEvent::PointerLeave { surface } if *surface == r.from => {
                if let Some(r) = &mut self.rebase {
                    r.left = true;
                }
                Some(Vec::new())
            }
            InputEvent::PointerEnter { surface, position } if *surface == r.to => {
                self.rebase = None;
                Some(vec![InputEvent::PointerMotion {
                    surface: r.to,
                    position: *position,
                    time: 0,
                }])
            }
            InputEvent::PointerLeave { surface } if *surface == r.to => {
                self.rebase = None;
                None
            }
            InputEvent::PointerEnter { .. } | InputEvent::PointerLeave { .. } => {
                self.rebase = None;
                Some(vec![leave, event.clone()])
            }
            InputEvent::PointerMotion {
                surface,
                position,
                time,
            } if *surface == r.from => {
                let q = moved(position);
                let held = self.logic.as_ref().is_some_and(|f| f.router.held(r.to));
                if held || self.renderer.takes_input_at(r.to, q) {
                    if let Some(r) = &mut self.rebase {
                        r.at = *position;
                    }
                    Some(vec![InputEvent::PointerMotion {
                        surface: r.to,
                        position: q,
                        time: *time,
                    }])
                } else {
                    self.rebase = None;
                    Some(vec![
                        leave,
                        InputEvent::PointerEnter {
                            surface: r.from,
                            position: *position,
                        },
                        event.clone(),
                    ])
                }
            }
            InputEvent::PointerButton {
                surface,
                position,
                button,
                state,
                time,
            } if *surface == r.from => Some(vec![InputEvent::PointerButton {
                surface: r.to,
                position: moved(position),
                button: *button,
                state: *state,
                time: *time,
            }]),
            InputEvent::PointerAxis {
                surface,
                position,
                horizontal,
                vertical,
                source,
                time,
            } if *surface == r.from => Some(vec![InputEvent::PointerAxis {
                surface: r.to,
                position: moved(position),
                horizontal: *horizontal,
                vertical: *vertical,
                source: *source,
                time: *time,
            }]),
            _ => None,
        }
    }

    /// The `damage` log line of a frame shown (with `--log-damage`).
    pub(crate) fn log_frame(
        &self,
        surface: SurfaceId,
        damage: &Damage,
        size: Size,
        scale: Scale,
        age: u8,
    ) {
        if !damage.is_empty() && self.log_damage {
            let rects: Vec<String> = damage
                .rects()
                .iter()
                .map(|r| format!("{}x{}+{}+{}", r.w, r.h, r.x, r.y))
                .collect();
            // One line per painted frame, parsed by scripts/m0-exit.sh;
            // strand-surface commits it unless `frame_dropped` follows.
            // `gaps=`: frames so far that showed a list's unmounted rows
            // (always 0; the M4 exit's per-frame check); `stalls=`:
            // frames so far that held a list's view at its mounted rows
            // while the scroll went on; `top=`: the first row (global
            // index) a list showed in the last such frame; `pending=1`:
            // text or images were still on their workers (the arrival
            // paints another frame); `flatten=`, `wait=` and `raster=`:
            // the last CPU paint's milliseconds laying out and flattening
            // (0 when it reused its scene), of that waiting for the text
            // worker, and drawing.
            let lists = self.renderer.list_frames();
            let times = self.renderer.last_paint_times();
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
            eprintln!(
                "strand: damage surface={} buffer={}x{} scale={} age={} area={} rects={} gaps={} stalls={} top={} pending={} flatten={:.2} wait={:.2} raster={:.2}",
                surface.0,
                size.w,
                size.h,
                scale.as_f64(),
                age,
                damage.area(),
                rects.join(","),
                lists.gaps,
                lists.stalls,
                lists.top_row,
                u8::from(self.renderer.work_pending()),
                ms(times.flatten),
                ms(times.text_wait),
                ms(times.raster),
            );
        }
    }
}

impl Host {
    /// (M4) Hands render where `surface` lies on its output, once it is
    /// placed (a popup, with no monitor of its own, takes its parent's).
    fn send_origin(&mut self, surface: SurfaceId) {
        if let Some(&(x, y)) = self.origins.get(&surface) {
            let at = strand_scene::LogicalPoint::new(x as f32, y as f32);
            let output = self.outputs.get(&surface).cloned();
            self.renderer
                .set_surface_origin(surface, Some((output, at)));
        }
    }

    /// The tray's click point `event` sets, if any: on a press on a
    /// placed surface, [`click_point`] of the innermost node hit; on a
    /// press on a surface not placed yet (a popup before its configure,
    /// a monitor with no logical size) or a key press (an action the
    /// keyboard triggers), (0, 0), the point when none is known, so an
    /// action never sends an earlier press's point from another surface.
    fn press_point(&self, event: &InputEvent) -> Option<(i32, i32)> {
        let (surface, position) = match event {
            InputEvent::PointerButton {
                surface,
                position,
                state: strand_scene::ButtonState::Pressed,
                ..
            } => (surface, position),
            InputEvent::Key { key, .. } if key.state == strand_scene::ButtonState::Pressed => {
                return Some((0, 0));
            }
            _ => return None,
        };
        let Some(&origin) = self.origins.get(surface) else {
            return Some((0, 0));
        };
        let rect = self
            .renderer
            .hit(*surface, *position)
            .first()
            .and_then(|n| self.renderer.node_rect(*surface, *n));
        let scale = self
            .renderer
            .delegated_pose(*surface)
            .map_or(1.0, |p| p.scale);
        Some(click_point(origin, rect, *position, scale))
    }
}

impl Painter for Host {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        // The lock's built-in fallback, when it shows (`run/lock.rs`).
        if let Some(damage) = self.lock.paint(surface, target) {
            return damage;
        }
        let damage = self.renderer.paint(surface, target);
        self.painted(surface, &damage);
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.painted(surface, !damage.is_empty(), target.scale, &self.renderer);
            if !damage.is_empty() {
                p.0.frame(surface, target);
            }
        }
        self.log_frame(surface, &damage, target.size, target.scale, target.age);
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.lock
            .wants_frame(surface)
            .unwrap_or_else(|| self.renderer.wants_frame(surface))
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        if self.lock.shown_on(surface) {
            return Damage::new();
        }
        self.renderer.opaque_region(surface)
    }

    fn blur_region(&self, surface: SurfaceId) -> Vec<BlurRegion> {
        if self.lock.shown_on(surface) {
            return Vec::new();
        }
        self.renderer.blur_region(surface)
    }

    fn surface_pose(&self, surface: SurfaceId) -> Option<strand_scene::SurfacePose> {
        self.renderer.surface_pose(surface)
    }

    fn input_region(&self, surface: SurfaceId) -> Option<Vec<strand_scene::LogicalRect>> {
        if self.lock.shown_on(surface) {
            return None;
        }
        self.renderer.input_region(surface)
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
        if let Some(m) = monitor {
            self.outputs.insert(surface, m.id.as_str().to_string());
        }
        let kind = self.renderer.tree().get(node).map(|n| n.kind);
        self.lock.attached(surface, node, kind);
        self.roots.insert(surface, node);
        self.renderer
            .set_surface_bounds(surface, monitor.and_then(monitor_bounds));
        if let Some(f) = &mut self.logic {
            f.attached(surface, node);
        }
        self.wake_if_changed();
    }

    fn surface_entered(&mut self, surface: SurfaceId, monitor: &Monitor) {
        self.renderer
            .set_surface_bounds(surface, monitor_bounds(monitor));
        self.outputs
            .insert(surface, monitor.id.as_str().to_string());
        self.send_origin(surface);
        self.wake_if_changed();
    }

    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.configured(surface);
        }
        self.renderer.configure_surface(surface, size, scale);
        self.lock.configured(surface);
        if let Some(f) = &self.logic {
            let tree = self.renderer.tree();
            f.configured(surface, size, scale, |node| {
                tree.get(node).is_some_and(|n| n.get(Prop::Watch).is_some())
            });
        }
        // Its first layout's sizes: a container query answers before the
        // first frame (the renderer holds it for that).
        self.forward_facts();
        self.wake_if_changed();
    }

    fn surface_placed(&mut self, surface: SurfaceId, origin: (i32, i32)) {
        self.origins.insert(surface, origin);
        self.send_origin(surface);
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        log::info!("surface {} detached", surface.0);
        self.lock.detached(surface);
        self.renderer.detach_surface(surface);
        // Its fed nodes are hidden now, whether or not a paint follows.
        if let Some(feeds) = &mut self.feeds {
            crate::run::feeds::sync(&mut self.renderer, feeds);
        }
        self.roots.remove(&surface);
        self.origins.remove(&surface);
        self.outputs.remove(&surface);
        self.blur_fallback.regions.remove(&surface);
        // (M5) Pick mode ends with the surface it picks on.
        if self
            .logic
            .as_ref()
            .is_some_and(|f| f.picking_on == Some(surface))
        {
            self.end_pick();
        }
        if let Some(f) = &mut self.logic {
            f.detached(surface);
        }
        // (m6-owner-fixes, B-d) A popup the pointer was taken onto went:
        // the pointer is back on the surface the compositor has it on,
        // unless the compositor left that surface.
        if let Some(r) = self.rebase.filter(|r| r.to == surface || r.from == surface) {
            self.rebase = None;
            if r.to == surface && !r.left {
                self.input(&InputEvent::PointerEnter {
                    surface: r.from,
                    position: r.at,
                });
            }
        }
        // (M6) The pointer gone with the surface may start a hover
        // popup's grace.
        self.tick_input();
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

    fn drop_accepted(&self, surface: SurfaceId) -> bool {
        self.logic
            .as_ref()
            .is_some_and(|f| f.router.drop_target(surface).is_some())
    }

    fn drag_source(&self, surface: SurfaceId) -> Option<NodeId> {
        let d = self.logic.as_ref()?.router.drag()?;
        (d.surface == surface).then_some(d.source)
    }

    fn drag_image(&mut self, surface: SurfaceId, node: NodeId) -> Option<strand_scene::DragImage> {
        self.renderer.drag_image(surface, node)
    }

    fn drag_data(&self, node: NodeId) -> Option<strand_scene::DropPayload> {
        strand_scene::drag_export(self.renderer.tree().get(node)?.get(Prop::Drag)?)
    }

    fn press_serial(&mut self, serial: u32) {
        if let Some(f) = &mut self.logic {
            f.pressed(serial);
        }
    }

    /// (gap 31) The focused surface's keyboard, for `keyboard`: exact
    /// while one of ours (the lock screen, a launcher) has the keys.
    fn keyboard_state(&mut self, state: Option<&strand_surface::KeyboardState>) {
        strand_services::keyboard::focused(state.map(|s| strand_services::keyboard::Focused {
            caps_lock: s.caps_lock,
            num_lock: s.num_lock,
            layout: s.layout.clone(),
        }));
    }

    fn input(&mut self, event: &InputEvent) {
        // (m6-owner-fixes, B-d) The pointer taken onto a popup under it.
        if let Some(events) = self.rebase_event(event) {
            for e in &events {
                self.input(e);
            }
            return;
        }
        // Tests wait on this line before clicking a surface just mapped.
        if let InputEvent::PointerEnter { surface, .. } = event {
            log::debug!("pointer entered surface {}", surface.0);
        }
        // The fallback lock takes its surface's input (`run/lock.rs`).
        if self.lock.input(event) {
            if let Some(f) = &mut self.logic {
                f.passed();
            }
            if let Some(p) = &self.wake {
                p.ping();
            }
            return;
        }
        // A press sets the point tray actions it causes send the app.
        if let Some((x, y)) = self.press_point(event) {
            strand_services::tray::set_click_point(x, y);
        }
        // Surfaces already wanting a frame (animating) get it anyway.
        let mut idle = std::mem::take(&mut self.idle);
        idle.clear();
        if let Some(f) = &self.logic {
            idle.extend(
                f.surfaces
                    .keys()
                    .copied()
                    .filter(|s| !self.renderer.wants_frame(*s)),
            );
        }
        if let Some(f) = &mut self.logic {
            f.input(event, &mut self.renderer);
            // (M5) Pick mode: the inspector has the pointer.
            if f.router.picking() {
                match event {
                    InputEvent::PointerEnter { surface, .. }
                    | InputEvent::PointerMotion { surface, .. } => f.picking_on = Some(*surface),
                    InputEvent::PointerLeave { .. } => f.picking_on = None,
                    _ => {}
                }
                if let Some((surface, node)) = self.renderer.pick_input(event) {
                    let seq = f.pick_seq;
                    f.send(ToLogic::Picked(PickEvent::Picked { surface, node, seq }));
                }
            }
            // (M4) `parallax` and `tilt` follow the router's pointer.
            let s = event.surface();
            self.renderer.set_pointer(s, f.router.pointer(s));
            // (M6) A hover popup's delay or grace ending wakes the loop
            // (`tick_input`).
            self.renderer.set_input_wake(f.router.next_wake());
        }
        // A scroll lays out again: its sizes go with it.
        self.forward_facts();
        self.wake_if_changed();
        // Input that moved something on an idle surface with no logic
        // diff to follow (a key scrolling a list to the row it selects)
        // still needs a frame: only then is the loop woken.
        let woke = idle.iter().any(|s| self.renderer.wants_frame(*s));
        self.idle = idle;
        if woke && let Some(p) = &self.wake {
            p.ping();
        }
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

    fn lock_changed(&mut self, state: strand_surface::LockState) {
        log::info!("session lock: {state:?}");
        self.lock.changed(state);
        if let Some(f) = &self.logic {
            f.send(ToLogic::LockState(crate::run::lock::session_lock(state)));
        }
        if let Some(p) = &self.wake {
            p.ping();
        }
    }

    fn compositor_caps(&mut self, caps: &CompositorCaps) {
        // The blur ladder's first rung: with `ext-background-effect-v1`
        // confirmed, `blur` draws no tint (the compositor blurs).
        self.renderer.set_compositor_blur(caps.background_effect);
        // Compositor-animated poses (M4): surface roots' fades, scales
        // and small moves go to the alpha modifier, the viewport and
        // the margins where the compositor has the first two.
        self.renderer.set_compositor_poses(caps.delegates_poses());
        // Hyprland draws a layer surface stretched to the box it
        // arranged, whatever its viewport: a root's scale is painted
        // there (decisions.md, m4-surface-w2). Known by its globals.
        self.renderer.set_compositor_pose_scale(!caps.hyprland);
        self.blur_fallback.caps = Some(*caps);
        // Shown surfaces repaint with or without the tint: the main loop
        // polls them (a report is rare: once, and on a change).
        if let Some(p) = &self.wake {
            p.ping();
        }
    }

    fn gpu_release(&mut self, surface: SurfaceId) {
        self.gpu_released.push(surface);
        if let Some(p) = &self.wake {
            p.ping();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_render::HitOnly;
    use strand_scene::input::button;
    use strand_scene::{AxisDelta, ButtonState, LogicalPoint};
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

    /// The blur ladder's last rung says why once: not before the
    /// compositor's capabilities are known, never when it blurs, and on
    /// Hyprland it names `strand compositor-rules`.
    #[test]
    fn the_blur_fallback_is_said_once_with_its_reason() {
        let mut b = BlurFallback::default();
        assert!(!b.pending());
        assert_eq!(b.frame("strand-Top"), None, "capabilities unknown");
        b.caps = Some(CompositorCaps {
            background_effect: true,
            ..CompositorCaps::default()
        });
        assert!(!b.pending());
        assert_eq!(b.frame("strand-Top"), None, "the compositor blurs");
        b.caps = Some(CompositorCaps::default());
        assert!(b.pending());
        let text = b.frame("strand-Top").unwrap();
        assert!(text.contains("strand-Top") && text.contains("ext-background-effect-v1"));
        assert!(text.contains("blur_fallback: none"), "{text}");
        assert!(!text.contains("compositor-rules"));
        assert!(!b.pending(), "said");
        assert_eq!(b.frame("strand-Dock"), None, "once");
        let mut b = BlurFallback {
            caps: Some(CompositorCaps {
                hyprland: true,
                ..CompositorCaps::default()
            }),
            ..BlurFallback::default()
        };
        assert!(
            b.frame("strand-Top")
                .unwrap()
                .contains("strand compositor-rules")
        );
    }

    /// (M4) The blur ladder's inspector half, on the host's side: a
    /// surface's `blur` boxes (not a nested popup's), each saying whether
    /// it draws the tint, are told to logic with why, when the frame's
    /// blur regions first appear and again when their count changes;
    /// not while the capabilities are unknown or the compositor blurs.
    #[test]
    fn a_surfaces_blur_boxes_are_told_to_logic() {
        use std::time::Duration;
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (tx, rx) = calloop::channel::channel();
        let mut host = Host::new(renderer, false).forwarding(tx);
        let num = strand_scene::PropValue::Number;
        let (panel, tinted, plain, bare, popup, inner) = (
            NodeId::new(0, 0),
            NodeId::new(1, 0),
            NodeId::new(2, 0),
            NodeId::new(3, 0),
            NodeId::new(4, 0),
            NodeId::new(5, 0),
        );
        let mut d = SceneDiff::new();
        d.create(panel, strand_scene::NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, num(100.0))
            .set(panel, Prop::Height, num(100.0))
            .create(tinted, strand_scene::NodeKind::Box, Some(panel), 0)
            .set(tinted, Prop::Width, num(40.0))
            .set(tinted, Prop::Height, num(40.0))
            .set(tinted, Prop::Blur, num(24.0))
            .create(plain, strand_scene::NodeKind::Box, Some(panel), 1)
            .set(plain, Prop::Width, num(40.0))
            .set(plain, Prop::Height, num(40.0))
            .set(plain, Prop::Blur, num(8.0))
            .set(
                plain,
                Prop::BlurFallback,
                strand_scene::PropValue::Keyword("none".into()),
            )
            .create(bare, strand_scene::NodeKind::Box, Some(panel), 2)
            .set(bare, Prop::Width, num(10.0))
            .set(bare, Prop::Height, num(10.0))
            .create(popup, strand_scene::NodeKind::Popup, Some(panel), 3)
            .create(inner, strand_scene::NodeKind::Box, Some(popup), 0)
            .set(inner, Prop::Blur, num(8.0));
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, panel, None);
        host.surface_configured(s, Size::new(100, 100), Scale::ONE);
        let paint = |host: &mut Host| {
            let mut px = vec![0u8; 100 * 100 * 4];
            let t = PaintTarget::new(&mut px, Size::new(100, 100), 400, Scale::ONE, 0).unwrap();
            let mut t = t.at(Duration::from_secs(1));
            let damage = host.paint(s, &mut t);
            host.painted(s, &damage);
        };
        let told = |rx: &calloop::channel::Channel<ToLogic>| {
            std::iter::from_fn(|| rx.try_recv().ok())
                .filter_map(|m| match m {
                    ToLogic::BlurFallback {
                        surface,
                        nodes,
                        why,
                    } => Some((surface, nodes, why)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        paint(&mut host);
        assert!(told(&rx).is_empty(), "capabilities unknown");
        host.compositor_caps(&CompositorCaps {
            background_effect: true,
            ..CompositorCaps::default()
        });
        host.renderer.invalidate(s);
        paint(&mut host);
        assert!(told(&rx).is_empty(), "the compositor blurs");
        host.compositor_caps(&CompositorCaps::default());
        host.renderer.invalidate(s);
        paint(&mut host);
        let got = told(&rx);
        assert_eq!(got.len(), 1, "{got:?}");
        let (surface, nodes, why) = &got[0];
        assert_eq!(surface, "strand-panel");
        assert_eq!(
            nodes,
            &[
                (tinted, strand_scene::NodeKind::Box, true),
                (plain, strand_scene::NodeKind::Box, false)
            ]
        );
        assert!(why.contains("ext-background-effect-v1"), "{why}");
        host.renderer.invalidate(s);
        paint(&mut host);
        assert!(told(&rx).is_empty(), "the same boxes: told once");
    }

    /// Compositor-animated poses follow the capabilities: with the alpha
    /// modifier and the viewporter a panel's entering fade and scale are
    /// handed to the surface manager as a pose (and not painted), on
    /// Hyprland the fade only, and without them it is painted and no pose
    /// is reported.
    #[test]
    fn poses_are_delegated_only_with_the_protocols() {
        use std::time::Duration;
        let pose = |caps: CompositorCaps, hyprland: bool| {
            let font = std::fs::read(strand_text::test_font_path()).unwrap();
            let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
                std::sync::Arc::new(font),
            ]));
            let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
            let mut host = Host::new(renderer, false);
            host.compositor_caps(&CompositorCaps { hyprland, ..caps });
            let panel = NodeId::new(0, 0);
            let num = strand_scene::PropValue::Number;
            let mut d = SceneDiff::new();
            d.create(panel, strand_scene::NodeKind::Panel, None, 0)
                .set(panel, Prop::Width, num(100.0))
                .set(panel, Prop::Height, num(100.0))
                .set(
                    panel,
                    Prop::Anchor,
                    strand_scene::PropValue::Keyword("top_right".into()),
                )
                .set(
                    panel,
                    Prop::Enter,
                    strand_scene::PropValue::Pose(vec![
                        (Prop::Opacity, num(0.0)),
                        (Prop::Scale, num(0.5)),
                    ]),
                );
            assert!(host.renderer.apply(d).is_empty());
            let s = SurfaceId(1);
            host.surface_attached(s, panel, None);
            host.surface_configured(s, Size::new(100, 100), Scale::ONE);
            let mut px = vec![0u8; 100 * 100 * 4];
            let t = PaintTarget::new(&mut px, Size::new(100, 100), 400, Scale::ONE, 0).unwrap();
            let mut t = t.at(Duration::from_secs(1));
            assert!(!host.paint(s, &mut t).is_empty());
            host.surface_pose(s)
        };
        let delegating = CompositorCaps {
            alpha_modifier: true,
            viewporter: true,
            ..CompositorCaps::default()
        };
        let p = pose(delegating, false).expect("a pose");
        assert!(p.opacity < 0.5 && p.scale < 0.8, "{p:?}");
        // Hyprland stretches a layer surface to its box: only the fade
        // (and offsets) go to it, the scale is painted.
        let p = pose(delegating, true).expect("a pose");
        assert!(p.opacity < 0.5 && p.scale == 1.0, "{p:?}");
        assert_eq!(pose(CompositorCaps::default(), false), None);
        let no_alpha = CompositorCaps {
            viewporter: true,
            ..CompositorCaps::default()
        };
        assert_eq!(pose(no_alpha, false), None);
    }

    /// (M4) A press sets the tray's click point: the bottom-left corner
    /// of the innermost node pressed, on the output (the surface's
    /// origin added); the surface's box where no child was hit; nothing
    /// for a surface not placed yet, gone, or a release. Rounded to
    /// logical pixels.
    #[test]
    fn a_press_sets_the_tray_click_point() {
        use std::time::Duration;
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let mut host = Host::new(renderer, false);
        let (panel, icon) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let num = strand_scene::PropValue::Number;
        let mut d = SceneDiff::new();
        d.create(panel, strand_scene::NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, num(100.0))
            .set(panel, Prop::Height, num(100.0))
            .create(icon, strand_scene::NodeKind::Box, Some(panel), 0)
            .set(icon, Prop::Width, num(20.0))
            .set(icon, Prop::Height, num(20.0));
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, panel, None);
        host.surface_configured(s, Size::new(100, 100), Scale::ONE);
        let mut px = vec![0u8; 100 * 100 * 4];
        let t = PaintTarget::new(&mut px, Size::new(100, 100), 400, Scale::ONE, 0).unwrap();
        let mut t = t.at(Duration::from_secs(1));
        assert!(!host.paint(s, &mut t).is_empty());
        let press = |x: f32, y: f32| InputEvent::PointerButton {
            surface: s,
            position: strand_scene::LogicalPoint::new(x, y),
            button: button::RIGHT,
            state: strand_scene::ButtonState::Pressed,
            time: 0,
        };
        // Not placed yet: the point when none is known, not the last
        // press's.
        assert_eq!(host.press_point(&press(5.0, 5.0)), Some((0, 0)));
        host.surface_placed(s, (1800, 40));
        assert_eq!(host.press_point(&press(5.0, 5.0)), Some((1800, 60)));
        assert_eq!(host.press_point(&press(50.0, 80.0)), Some((1800, 140)));
        let release = InputEvent::PointerButton {
            surface: s,
            position: strand_scene::LogicalPoint::new(5.0, 5.0),
            button: button::RIGHT,
            state: strand_scene::ButtonState::Released,
            time: 0,
        };
        assert_eq!(host.press_point(&release), None);
        // A key press (keyboard activation) has no point either.
        let key = |state| InputEvent::Key {
            surface: s,
            key: strand_scene::KeyInput {
                name: "Return".into(),
                text: String::new(),
                state,
                repeat: false,
                modifiers: strand_scene::Modifiers::default(),
                time: 0,
            },
        };
        assert_eq!(
            host.press_point(&key(strand_scene::ButtonState::Pressed)),
            Some((0, 0))
        );
        assert_eq!(
            host.press_point(&key(strand_scene::ButtonState::Released)),
            None
        );
        host.surface_detached(s);
        assert_eq!(host.press_point(&press(5.0, 5.0)), Some((0, 0)));
        assert_eq!(
            click_point(
                (10, 20),
                None,
                strand_scene::LogicalPoint::new(3.4, 7.6),
                1.0
            ),
            (13, 28)
        );
        // Under a delegated scale the corner is drawn that much nearer
        // the surface's top-left corner: (40, 100) at 0.8 is (32, 80).
        assert_eq!(
            click_point(
                (1800, 40),
                Some(strand_scene::LogicalRect::new(40.0, 80.0, 20.0, 20.0)),
                strand_scene::LogicalPoint::new(45.0, 85.0),
                0.8
            ),
            (1832, 120)
        );
    }

    /// (M4) A drop the Router found a target for reaches logic as
    /// `on drop`, payload, index and (M6) hidden rows unchanged.
    #[test]
    fn drops_are_forwarded_to_logic() {
        let node = strand_scene::NodeId::new(1, 0);
        let payload = strand_scene::DropPayload::Node(strand_scene::NodeId::new(2, 0));
        let hidden = Some(strand_render::HiddenRows {
            list: node,
            before: 40,
        });
        let drop = Intent::Event {
            node,
            event: RouteEvent::Drop {
                payload: payload.clone(),
                at: 3,
                hidden,
            },
        };
        assert_eq!(
            to_logic(drop, None),
            Some(ToLogic::Event {
                input: None,
                node,
                event: NodeEvent::Drop {
                    payload,
                    at: 3,
                    hidden
                },
            })
        );
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
    /// the logical size (buffer over scale) of a watched node; other
    /// buttons and motion send nothing, nor does a surface the forwarder
    /// does not know.
    #[test]
    fn surface_input_becomes_logic_messages() {
        let (mut f, mut el) = setup();
        let s = SurfaceId(7);
        let node = NodeId::new(3, 0);
        f.attached(s, node);
        f.configured(
            s,
            Size::new(2560, 50),
            Scale::from_f64(1.25).unwrap(),
            |n| n == node,
        );
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
        let event = |event: NodeEvent| ToLogic::Event {
            input: None,
            node,
            event,
        };
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
                event(NodeEvent::Scroll { dy: 1.0, dx: 0.0 }),
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

    /// (M5, S-platform) A node event carries the press that caused it:
    /// a click its press's serial (told before the press), a secondary
    /// click its own, a key press its own; a scroll, and a key release,
    /// none.
    #[test]
    fn a_press_serial_rides_the_events_it_causes() {
        use strand_compiler::vm::InputCtx;
        let (mut f, mut el) = setup();
        let s = SurfaceId(7);
        let node = NodeId::new(3, 0);
        f.attached(s, node);
        let at = LogicalPoint::new(10.0, 5.0);
        let button = |b, state| InputEvent::PointerButton {
            surface: s,
            position: at,
            button: b,
            state,
            time: 0,
        };
        let root_only = |_: SurfaceId, _: LogicalPoint| Vec::new();
        // What `SurfaceHost::press_serial` sets, before each press.
        f.pressed(41);
        f.input(
            &button(button::LEFT, ButtonState::Pressed),
            &mut HitOnly(root_only),
        );
        f.input(
            &button(button::LEFT, ButtonState::Released),
            &mut HitOnly(root_only),
        );
        f.pressed(42);
        f.input(
            &button(button::RIGHT, ButtonState::Pressed),
            &mut HitOnly(root_only),
        );
        f.input(
            &button(button::RIGHT, ButtonState::Released),
            &mut HitOnly(root_only),
        );
        f.input(
            &InputEvent::PointerAxis {
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
            &mut HitOnly(root_only),
        );
        let events: Vec<(NodeEvent, Option<InputCtx>)> = drain(&mut el)
            .into_iter()
            .filter_map(|m| match m {
                ToLogic::Event { event, input, .. } => Some((event, input)),
                _ => None,
            })
            .collect();
        let press = |serial| Some(InputCtx { surface: 7, serial });
        assert_eq!(
            events,
            [
                (NodeEvent::Click, press(41)),
                (NodeEvent::Secondary, press(42)),
                (NodeEvent::Scroll { dy: 1.0, dx: 0.0 }, None),
            ]
        );
        let key = |state| InputEvent::Key {
            surface: s,
            key: strand_scene::KeyInput {
                name: "Return".into(),
                text: String::new(),
                state,
                repeat: false,
                modifiers: Default::default(),
                time: 0,
            },
        };
        assert_eq!(
            press_of(&key(ButtonState::Pressed), Some(43), true),
            press(43)
        );
        assert_eq!(press_of(&key(ButtonState::Released), Some(43), true), None);
        assert_eq!(
            press_of(&key(ButtonState::Pressed), None, true),
            None,
            "no serial yet"
        );
    }

    /// (M5, S-platform) A click away on the catcher is a press of ours
    /// (its serial told just before it) and carries it; a click away the
    /// compositor sent (a popup's `done`, the user having clicked another
    /// client) carries none, not the last press of ours.
    #[test]
    fn only_a_click_away_that_is_a_press_carries_one() {
        use strand_compiler::vm::InputCtx;
        let s = SurfaceId(7);
        let away = InputEvent::ClickAway { surface: s };
        assert_eq!(
            press_of(&away, Some(50), true),
            Some(InputCtx {
                surface: 7,
                serial: 50
            })
        );
        assert_eq!(press_of(&away, Some(50), false), None);

        // Through `Forward`: the freshness goes with the first event.
        let (mut f, _el) = setup();
        f.pressed(51);
        let root_only = |_: SurfaceId, _: LogicalPoint| Vec::new();
        f.input(
            &InputEvent::PointerButton {
                surface: s,
                position: LogicalPoint::new(1.0, 1.0),
                button: button::LEFT,
                state: ButtonState::Pressed,
                time: 0,
            },
            &mut HitOnly(root_only),
        );
        assert!(!f.fresh, "the press was its event");
        assert_eq!(press_of(&away, f.press, f.fresh), None);
        f.pressed(52);
        assert!(f.fresh);
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

    /// Input wakes the main loop only when it leaves an idle surface
    /// wanting a frame (a wheel step on a still list): while the surface
    /// already wants frames (the step's spring in flight), more input
    /// does not ping again.
    #[test]
    fn input_wakes_the_loop_only_for_an_idle_surface() {
        use std::time::Duration;
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (tx, _rx) = calloop::channel::channel();
        let mut el = calloop::EventLoop::<u32>::try_new().unwrap();
        let (ping, source) = calloop::ping::make_ping().unwrap();
        el.handle()
            .insert_source(source, |_, _, n: &mut u32| *n += 1)
            .unwrap();
        let mut pings = || {
            let mut n = 0;
            el.dispatch(Some(Duration::ZERO), &mut n).unwrap();
            n
        };
        let mut host = Host::new(renderer, false).forwarding(tx).waking(ping);
        let (panel, list) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let num = strand_scene::PropValue::Number;
        let mut d = SceneDiff::new();
        d.create(panel, strand_scene::NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, num(100.0))
            .set(panel, Prop::Height, num(100.0))
            .create(list, strand_scene::NodeKind::List, Some(panel), 0)
            .set(list, Prop::Height, num(100.0));
        for i in 0..40 {
            let row = NodeId::new(10 + i, 0);
            d.create(row, strand_scene::NodeKind::Box, Some(list), i)
                .set(row, Prop::Height, num(20.0));
        }
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, panel, None);
        host.surface_configured(s, Size::new(100, 100), Scale::ONE);
        let mut px = vec![0u8; 100 * 100 * 4];
        let mut paint = |host: &mut Host, ms: u64| {
            let t = PaintTarget::new(&mut px, Size::new(100, 100), 400, Scale::ONE, 0).unwrap();
            let mut t = t.at(Duration::from_millis(ms));
            host.paint(s, &mut t);
        };
        paint(&mut host, 1000);
        let at = strand_scene::LogicalPoint::new(50.0, 50.0);
        host.input(&InputEvent::PointerEnter {
            surface: s,
            position: at,
        });
        // The surface manager has synced the surface (its changes would
        // ping on their own).
        host.renderer.take_surface_changes();
        let _ = pings();
        assert!(!host.renderer.wants_frame(s));
        let wheel = InputEvent::PointerAxis {
            surface: s,
            position: at,
            horizontal: AxisDelta::default(),
            vertical: AxisDelta {
                pixels: 15.0,
                value120: 120,
                stop: false,
            },
            source: None,
            time: 1010,
        };
        // A wheel step on the still list: a ping.
        host.input(&wheel);
        assert!(host.renderer.wants_frame(s));
        assert_eq!(pings(), 1);
        // Another while it springs, and pointer motion: none.
        host.input(&wheel);
        host.input(&InputEvent::PointerMotion {
            surface: s,
            position: at,
            time: 1020,
        });
        assert_eq!(pings(), 0);
        // Settled, a step pings again.
        let mut ms = 1016;
        while host.renderer.wants_frame(s) {
            paint(&mut host, ms);
            ms += 16;
            assert!(ms < 10_000, "the wheel never settled");
        }
        host.renderer.take_surface_changes();
        let _ = pings();
        host.input(&wheel);
        assert_eq!(pings(), 1);
    }

    /// (M4) The host hands render where each surface lies and on which
    /// output (shared-element morphs across surfaces): a layer surface
    /// takes its monitor's identity, a popup with no monitor its parent's,
    /// whichever is placed first, and follows the parent to another one.
    #[test]
    fn surfaces_are_placed_on_their_outputs_for_render() {
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let mut host = Host::new(renderer, false);
        let (bar, item, popup) = (NodeId::new(0, 0), NodeId::new(1, 0), NodeId::new(2, 0));
        let mut d = SceneDiff::new();
        d.create(bar, strand_scene::NodeKind::Bar, None, 0)
            .create(item, strand_scene::NodeKind::Box, Some(bar), 0)
            .create(popup, strand_scene::NodeKind::Popup, Some(item), 0);
        assert!(host.renderer.apply(d).is_empty());
        let (s, p) = (SurfaceId(1), SurfaceId(2));
        let dp1 = monitor("A", "DP-1");
        host.surface_attached(s, bar, Some(&dp1));
        host.surface_attached(p, popup, None);
        assert_eq!(host.renderer.surface_origin(s), None, "not placed yet");
        let origin = |host: &Host, id| {
            host.renderer
                .surface_origin(id)
                .map(|o| (o.output.clone(), o.at.x, o.at.y))
        };
        // The popup placed before its parent: no output yet, then its
        // parent's once the parent is placed.
        host.surface_placed(p, (40, 36));
        assert_eq!(origin(&host, p), None, "its parent is not placed");
        host.surface_placed(s, (0, 0));
        let name = dp1.id.as_str().to_string();
        assert_eq!(origin(&host, s), Some((name.clone(), 0.0, 0.0)));
        assert_eq!(origin(&host, p), Some((name, 40.0, 36.0)));
        // The parent moved to another monitor: the popup follows it.
        let hdmi = monitor("B", "HDMI-A-1");
        host.surface_entered(s, &hdmi);
        let hdmi_name = hdmi.id.as_str().to_string();
        assert_eq!(origin(&host, s), Some((hdmi_name.clone(), 0.0, 0.0)));
        assert_eq!(origin(&host, p), Some((hdmi_name, 40.0, 36.0)));
        host.surface_detached(p);
        assert_eq!(host.renderer.surface_origin(p), None);
    }

    /// (M4) A still aurora and capped particles (CPU fallbacks for GPU
    /// effects) are said once each, as a `strand watch` notice, the first
    /// time a frame draws them with no GPU to draw them (in a `gpu`
    /// build, one whose device was refused).
    #[test]
    fn effect_fallbacks_are_said_once_as_notices() {
        use std::time::Duration;
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        #[allow(unused_mut)]
        let mut renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        #[cfg(feature = "gpu")]
        renderer.deliver_gpu(strand_gpu::GpuReply::Unavailable(strand_gpu::GpuError {
            kind: strand_gpu::GpuErrorKind::NoAdapter,
            message: "no adapter (test)".into(),
        }));
        let (f, mut el) = setup();
        let mut host = Host::new(renderer, false);
        host.logic = Some(f);
        let (bar, aurora, field) = (NodeId::new(0, 0), NodeId::new(1, 0), NodeId::new(2, 0));
        let num = strand_scene::PropValue::Number;
        let mut d = SceneDiff::new();
        d.create(bar, strand_scene::NodeKind::Bar, None, 0)
            .create(aurora, strand_scene::NodeKind::Effect, Some(bar), 0)
            .set(
                aurora,
                Prop::Style,
                strand_scene::PropValue::Keyword("aurora".into()),
            )
            .set(aurora, Prop::Size, num(20.0))
            .create(field, strand_scene::NodeKind::Particles, Some(bar), 1)
            .set(field, Prop::Rate, num(5000.0))
            .set(
                field,
                Prop::Life,
                strand_scene::PropValue::Duration(Duration::from_secs(1)),
            )
            .set(field, Prop::Size, num(20.0));
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, bar, None);
        host.surface_configured(s, Size::new(100, 20), Scale::ONE);
        let mut px = vec![0u8; 100 * 20 * 4];
        let mut paint = |host: &mut Host, ms: u64| {
            let t = PaintTarget::new(&mut px, Size::new(100, 20), 400, Scale::ONE, 0).unwrap();
            let mut t = t.at(Duration::from_millis(ms));
            host.paint(s, &mut t);
        };
        paint(&mut host, 1000);
        let notices: Vec<String> = drain(&mut el)
            .into_iter()
            .filter_map(|m| match m {
                ToLogic::Notice(n) => Some(n),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].contains("aurora") && notices[1].contains("1,000"));
        paint(&mut host, 1016);
        assert!(
            !drain(&mut el)
                .iter()
                .any(|m| matches!(m, ToLogic::Notice(_))),
            "once"
        );
    }

    /// A surface sized to its content resizes whenever its text changes
    /// width (a popup's `pct(cpu.usage)` crossing `10%` on a quiet
    /// machine); logic hears of the size only once a binding reads it
    /// (`Prop::Watch`), so a cpu sample wakes it once, not twice
    /// (`docs/decisions.md`, laptop-trim). Independent of the machine:
    /// the widths are the ones `5%` and `13%` take in the test font.
    #[test]
    fn an_unread_surface_size_does_not_wake_logic() {
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (tx, rx) = calloop::channel::channel();
        let mut el = calloop::EventLoop::<Vec<ToLogic>>::try_new().unwrap();
        el.handle()
            .insert_source(rx, |e, _, out: &mut Vec<ToLogic>| {
                if let calloop::channel::Event::Msg(m) = e {
                    out.push(m);
                }
            })
            .unwrap();
        let mut host = Host::new(renderer, false).forwarding(tx);
        let popup = NodeId::new(0, 0);
        let mut d = SceneDiff::new();
        d.create(popup, strand_scene::NodeKind::Panel, None, 0);
        d.set(popup, Prop::Height, strand_scene::PropValue::Number(16.0));
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, popup, None);
        for w in [21, 29, 21, 29, 21] {
            host.surface_configured(s, Size::new(w, 16), Scale::ONE);
        }
        let sent = drain(&mut el);
        assert!(sent.is_empty(), "nobody reads the popup's size: {sent:?}");
        // `popup.width` read from now on: its size goes to logic.
        let mut d = SceneDiff::new();
        d.set(
            popup,
            Prop::Watch,
            strand_scene::PropValue::Keyword("size".into()),
        );
        assert!(host.renderer.apply(d).is_empty());
        host.surface_configured(s, Size::new(29, 16), Scale::ONE);
        let sent = drain(&mut el);
        let size = ToLogic::Size {
            node: popup,
            width: 29.0,
            height: 16.0,
        };
        assert!(sent.contains(&size), "{sent:?}");
    }

    /// (M4) What the surface manager asks while a drag is in flight: the
    /// `drag:` node dragged on a surface (so it can hand the drag to the
    /// compositor when the pointer leaves), and whether a drop there now
    /// would land (so it accepts the compositor's offer only then: a
    /// `Pin` over the list that takes `Pin`s, but not text from another
    /// program); the drop itself goes to logic.
    #[test]
    fn the_surface_manager_sees_the_routers_drag() {
        use strand_scene::{DropKind, DropPayload, NodeKind, PaintTarget, PropValue};
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (tx, mut el) = setup_channel();
        let mut host = Host::new(renderer, false).forwarding(tx);
        let (panel, col) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let rows = [NodeId::new(2, 0), NodeId::new(3, 0)];
        let pin = || PropValue::Keyword("Pin".into());
        let mut d = SceneDiff::new();
        d.create(panel, NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, PropValue::Number(100.0))
            .set(panel, Prop::Height, PropValue::Number(100.0))
            .create(col, NodeKind::Col, Some(panel), 0)
            .set(col, Prop::Accepts, PropValue::List(vec![pin()]));
        for (i, r) in rows.iter().enumerate() {
            d.create(*r, NodeKind::Box, Some(col), i as u32)
                .set(*r, Prop::Height, PropValue::Number(40.0))
                .set(*r, Prop::Drag, pin());
        }
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, panel, None);
        host.surface_configured(s, Size::new(100, 100), Scale::ONE);
        let mut px = vec![0u8; 100 * 100 * 4];
        let mut t = PaintTarget::new(&mut px, Size::new(100, 100), 400, Scale::ONE, 0).unwrap();
        host.paint(s, &mut t);
        assert_eq!(host.drag_source(s), None);
        let at = |y| LogicalPoint::new(20.0, y);
        let button = |y, state| InputEvent::PointerButton {
            surface: s,
            position: at(y),
            button: button::LEFT,
            state,
            time: 0,
        };
        let motion = |y| InputEvent::PointerMotion {
            surface: s,
            position: at(y),
            time: 0,
        };
        host.input(&motion(10.0));
        host.input(&button(10.0, ButtonState::Pressed));
        assert_eq!(host.drag_source(s), None, "a press is not a drag yet");
        host.input(&motion(70.0));
        assert_eq!(host.drag_source(s), Some(rows[0]));
        assert_eq!(host.drag_source(SurfaceId(2)), None);
        assert!(host.drop_accepted(s));
        drain(&mut el);
        host.input(&button(70.0, ButtonState::Released));
        let sent = drain(&mut el);
        assert!(
            sent.contains(&ToLogic::Event {
                input: None,
                node: col,
                event: NodeEvent::Drop {
                    payload: DropPayload::Node(rows[0]),
                    at: 1,
                    hidden: None
                }
            }),
            "{sent:?}"
        );
        assert_eq!(host.drag_source(s), None);
        // Text from another program: nothing here takes it.
        host.input(&InputEvent::DragEnter {
            surface: s,
            at: at(50.0),
            kinds: vec![DropKind::Text],
        });
        assert!(!host.drop_accepted(s));
    }

    /// (m4-integration-w2) A frame the GPU thread presents is followed
    /// by what follows a CPU frame (`Host::painted`, called by
    /// `run/gpu.rs`): a box whose size logic reads (`self.width`) grows
    /// while presented, `paint_gpu` lays it out, and its size reaches
    /// logic.
    #[cfg(feature = "gpu")]
    #[test]
    fn a_presented_frame_hands_its_layout_facts_to_logic() {
        use std::time::Duration;
        use strand_scene::{Backend, NodeKind, PropValue};
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (tx, mut el) = setup_channel();
        let mut host = Host::new(renderer, false).forwarding(tx);
        let (panel, child) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let mut d = SceneDiff::new();
        d.create(panel, NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, PropValue::Number(200.0))
            .set(panel, Prop::Height, PropValue::Number(100.0))
            .create(child, NodeKind::Box, Some(panel), 0)
            .set(child, Prop::Height, PropValue::Number(20.0))
            .set(child, Prop::Watch, PropValue::Keyword("size".into()));
        assert!(host.renderer.apply(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, panel, None);
        host.surface_configured(s, Size::new(200, 100), Scale::ONE);
        let _ = drain(&mut el);
        host.renderer.promote_now(s);
        host.renderer.set_backend(s, Backend::GpuPresent);
        // The box grows while the GPU presents the surface.
        let mut d = SceneDiff::new();
        d.set(child, Prop::Height, PropValue::Number(30.0));
        assert!(host.renderer.apply(d).is_empty());
        let frame = host
            .renderer
            .paint_gpu(s, Duration::from_secs(1))
            .expect("the switch repaints");
        host.painted(s, &Damage::full(frame.size));
        let sent = drain(&mut el);
        assert!(
            sent.iter()
                .any(|m| matches!(m, ToLogic::Layout { sizes, .. }
                if sizes.iter().any(|f| f.0 == child && f.1.h == 30.0))),
            "{sent:?}"
        );
    }

    /// (m5-open-spectrum) A diff naming another device for a shown,
    /// resting spectrum retargets its tap at once. No frame follows it
    /// (the dots look the same whichever device), so with taps synced
    /// only after paints the tap stayed on device 0, `audio.sink` before
    /// the service knew the default sink, and the tone never showed
    /// (GitHub runs 38065740030, 38076927579 and 38116737469).
    #[test]
    fn a_resting_spectrum_follows_its_device_without_a_frame() {
        use crate::run::feeds::{Feeds, Want};
        use std::time::Duration;
        use strand_scene::{NodeKind, PropValue};
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let mut host = Host::new(renderer, false);
        let (tx, _rx) = calloop::channel::channel();
        host.feeds = Some(Feeds::new(tx));
        let (panel, spectrum) = (NodeId::new(0, 0), NodeId::new(1, 0));
        let mut d = SceneDiff::new();
        d.create(panel, NodeKind::Panel, None, 0)
            .set(panel, Prop::Width, PropValue::Number(80.0))
            .set(panel, Prop::Height, PropValue::Number(40.0))
            .create(spectrum, NodeKind::Spectrum, Some(panel), 0)
            .set(spectrum, Prop::Source, PropValue::Text("0".into()))
            .set(spectrum, Prop::Width, PropValue::Number(64.0))
            .set(spectrum, Prop::Height, PropValue::Number(32.0));
        assert!(host.apply_diff(d).is_empty());
        let s = SurfaceId(1);
        host.surface_attached(s, panel, None);
        host.surface_configured(s, Size::new(80, 40), Scale::ONE);
        let mut px = vec![0u8; 80 * 40 * 4];
        let t = PaintTarget::new(&mut px, Size::new(80, 40), 320, Scale::ONE, 0).unwrap();
        let mut t = t.at(Duration::from_secs(1));
        assert!(!host.paint(s, &mut t).is_empty());
        let tapped = |host: &Host| host.feeds.as_ref().unwrap().tapped();
        assert_eq!(tapped(&host), [(spectrum, Want::Spectrum(0))]);
        // The default sink is known: logic names its id.
        let mut d = SceneDiff::new();
        d.set(spectrum, Prop::Source, PropValue::Text("42".into()));
        assert!(host.apply_diff(d).is_empty());
        assert!(
            !host.wants_frame(s),
            "the resting dots are unchanged: no frame follows"
        );
        assert_eq!(tapped(&host), [(spectrum, Want::Spectrum(42))]);
    }

    /// (M5, S-inspect; inspect-r1-7) Pick mode follows the pointer from
    /// a bar onto its popup: the popup's node is outlined and picked
    /// there, the pick naming the popup's surface. The popup unmapping
    /// while the pointer is on the bar leaves pick mode on; unmapping
    /// while the pointer is on it ends pick mode, and logic is told with
    /// the last `PickMode`'s sequence number.
    #[test]
    fn pick_mode_follows_the_pointer_onto_a_popup_and_ends_with_it() {
        use std::time::Duration;
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (tx, mut el) = setup_channel();
        let mut host = Host::new(renderer, false).forwarding(tx);
        let num = strand_scene::PropValue::Number;
        let (bar, popup, inner, again) = (
            NodeId::new(0, 0),
            NodeId::new(1, 0),
            NodeId::new(2, 0),
            NodeId::new(3, 0),
        );
        let mut d = SceneDiff::new();
        d.create(bar, strand_scene::NodeKind::Bar, None, 0)
            .set(bar, Prop::Height, num(40.0))
            .create(popup, strand_scene::NodeKind::Popup, None, 1)
            .set(popup, Prop::Width, num(100.0))
            .set(popup, Prop::Height, num(100.0))
            .create(inner, strand_scene::NodeKind::Box, Some(popup), 0)
            .set(inner, Prop::X, num(20.0))
            .set(inner, Prop::Y, num(20.0))
            .set(inner, Prop::Width, num(40.0))
            .set(inner, Prop::Height, num(40.0));
        assert!(host.renderer.apply(d).is_empty());
        let (sb, sp) = (SurfaceId(1), SurfaceId(2));
        host.surface_attached(sb, bar, None);
        host.surface_configured(sb, Size::new(100, 40), Scale::ONE);
        host.surface_attached(sp, popup, None);
        host.surface_configured(sp, Size::new(100, 100), Scale::ONE);
        let mut px = vec![0u8; 100 * 100 * 4];
        let mut paint = |host: &mut Host, s: SurfaceId, h: u32| {
            let t = PaintTarget::new(
                &mut px[..(100 * h * 4) as usize],
                Size::new(100, h),
                400,
                Scale::ONE,
                0,
            )
            .unwrap();
            let mut t = t.at(Duration::from_millis(1000));
            host.paint(s, &mut t);
        };
        paint(&mut host, sb, 40);
        paint(&mut host, sp, 100);
        let _ = drain_channel(&mut el);
        host.set_picking(true, 7);
        let at = LogicalPoint::new(30.0, 30.0);
        host.input(&InputEvent::PointerEnter {
            surface: sp,
            position: at,
        });
        let press = |state| InputEvent::PointerButton {
            surface: sp,
            position: at,
            button: button::LEFT,
            state,
            time: 1,
        };
        host.input(&press(ButtonState::Pressed));
        host.input(&press(ButtonState::Released));
        let got = drain_channel(&mut el);
        assert!(
            got.contains(&ToLogic::Picked(PickEvent::Picked {
                surface: sp,
                node: Some(inner),
                seq: 7,
            })),
            "the popup's node is picked on the popup: {got:?}"
        );
        assert!(
            !got.iter().any(|m| matches!(m, ToLogic::Event { .. })),
            "no handler hears the pick: {got:?}"
        );
        // Back on the bar, the popup closes: pick mode stays.
        host.input(&InputEvent::PointerLeave { surface: sp });
        host.input(&InputEvent::PointerEnter {
            surface: sb,
            position: LogicalPoint::new(5.0, 5.0),
        });
        host.surface_detached(sp);
        let got = drain_channel(&mut el);
        assert!(
            !got.iter()
                .any(|m| matches!(m, ToLogic::Picked(PickEvent::Ended { .. }))),
            "{got:?}"
        );
        assert!(host.logic.as_ref().is_some_and(|f| f.router.picking()));
        // A popup again, the pointer on it when it closes: pick mode ends.
        host.renderer.apply({
            let mut d = SceneDiff::new();
            d.create(again, strand_scene::NodeKind::Popup, None, 2)
                .set(again, Prop::Width, num(100.0))
                .set(again, Prop::Height, num(100.0));
            d
        });
        let sq = SurfaceId(3);
        host.surface_attached(sq, again, None);
        host.surface_configured(sq, Size::new(100, 100), Scale::ONE);
        host.input(&InputEvent::PointerLeave { surface: sb });
        host.input(&InputEvent::PointerEnter {
            surface: sq,
            position: at,
        });
        host.surface_detached(sq);
        let got = drain_channel(&mut el);
        assert!(
            got.contains(&ToLogic::Picked(PickEvent::Ended { seq: 7 })),
            "{got:?}"
        );
        assert!(!host.logic.as_ref().is_some_and(|f| f.router.picking()));
    }

    fn drain_channel(el: &mut calloop::EventLoop<'static, Vec<ToLogic>>) -> Vec<ToLogic> {
        let mut out = Vec::new();
        el.dispatch(Some(std::time::Duration::ZERO), &mut out)
            .unwrap();
        out
    }

    fn setup_channel() -> (
        calloop::channel::Sender<ToLogic>,
        calloop::EventLoop<'static, Vec<ToLogic>>,
    ) {
        let (tx, rx) = calloop::channel::channel();
        let el = calloop::EventLoop::<Vec<ToLogic>>::try_new().unwrap();
        el.handle()
            .insert_source(rx, |e, _, out: &mut Vec<ToLogic>| {
                if let calloop::channel::Event::Msg(m) = e {
                    out.push(m);
                }
            })
            .unwrap();
        (tx, el)
    }

    /// Two items on a bar, each holding a `trigger: hover` popup: the
    /// host, its renderer and router, with logic's channel, the bar's
    /// surface attached and the pointer on item `a`.
    struct Kept {
        host: Host,
        rx: calloop::channel::Channel<ToLogic>,
        a: NodeId,
        pop_a: NodeId,
        pop_b: NodeId,
        bar_s: SurfaceId,
        a_at: f32,
        b_at: f32,
    }

    impl Kept {
        fn new() -> Kept {
            use strand_scene::{NodeKind, PropValue};
            let font = std::fs::read(strand_text::test_font_path()).unwrap();
            let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
                std::sync::Arc::new(font),
            ]));
            let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
            let (tx, rx) = calloop::channel::channel();
            let mut host = Host::new(renderer, false).forwarding(tx);
            let n = NodeId::new;
            let (bar, row, a, b, pop_a, pop_b) =
                (n(0, 0), n(1, 0), n(2, 0), n(3, 0), n(4, 0), n(5, 0));
            let num = PropValue::Number;
            let mut d = SceneDiff::new();
            d.create(bar, NodeKind::Bar, None, 0)
                .set(bar, Prop::Height, num(30.0))
                .create(row, NodeKind::Row, Some(bar), 0)
                .create(a, NodeKind::Box, Some(row), 0)
                .set(a, Prop::Width, num(20.0))
                .set(a, Prop::Height, num(20.0))
                .create(b, NodeKind::Box, Some(row), 1)
                .set(b, Prop::Width, num(20.0))
                .set(b, Prop::Height, num(20.0));
            for (i, (item, pop)) in [(a, pop_a), (b, pop_b)].into_iter().enumerate() {
                d.create(pop, NodeKind::Popup, Some(item), 0)
                    .set(pop, Prop::Open, PropValue::Bool(false))
                    .set(pop, Prop::Trigger, PropValue::Keyword("hover".into()))
                    .create(n(10 + i as u32, 0), NodeKind::Box, Some(pop), 0)
                    .set(n(10 + i as u32, 0), Prop::Width, num(40.0))
                    .set(n(10 + i as u32, 0), Prop::Height, num(40.0));
            }
            assert!(host.renderer.apply(d).is_empty());
            let bar_s = SurfaceId(1);
            host.surface_attached(bar_s, bar, None);
            host.surface_configured(bar_s, Size::new(200, 30), Scale::ONE);
            // A paint lays the bar out and records its hit shapes.
            let mut px = vec![0u8; 200 * 30 * 4];
            let t = PaintTarget::new(&mut px, Size::new(200, 30), 800, Scale::ONE, 0).unwrap();
            host.paint(bar_s, &mut t.at(std::time::Duration::from_secs(1)));
            // Where each item is, by the renderer's own hit test.
            let first = |host: &mut Host, node| {
                (0..200)
                    .map(|x| x as f32 + 0.5)
                    .find(|x| {
                        host.renderer
                            .hit(bar_s, LogicalPoint::new(*x, 10.0))
                            .first()
                            == Some(&node)
                    })
                    .unwrap_or_else(|| panic!("{node:?} is not on the bar"))
            };
            let (a_at, b_at) = (first(&mut host, a), first(&mut host, b));
            host.input(&InputEvent::PointerEnter {
                surface: bar_s,
                position: LogicalPoint::new(a_at, 10.0),
            });
            Kept {
                host,
                rx,
                a,
                pop_a,
                pop_b,
                bar_s,
                a_at,
                b_at,
            }
        }

        /// The hover flags logic was told since the last call.
        fn hover_flags(&self) -> Vec<(NodeId, bool)> {
            self.told()
                .into_iter()
                .filter_map(|m| match m {
                    ToLogic::Flag {
                        node,
                        flag: NodeFlag::Hover,
                        on,
                    } => Some((node, on)),
                    _ => None,
                })
                .collect()
        }

        /// What logic was told since the last call.
        fn told(&self) -> Vec<ToLogic> {
            std::iter::from_fn(|| self.rx.try_recv().ok()).collect()
        }

        /// A popup's surface shows.
        fn show(&mut self, surface: u32, popup: NodeId) {
            let s = SurfaceId(surface);
            self.host.surface_attached(s, popup, None);
            self.host
                .surface_configured(s, Size::new(40, 40), Scale::ONE);
        }

        /// The pointer moves along the bar to `x`.
        fn to(&mut self, x: f32) {
            self.host.input(&InputEvent::PointerMotion {
                surface: self.bar_s,
                position: LogicalPoint::new(x, 10.0),
                time: 0,
            });
        }
    }

    /// (M6, spec.md 6.3, decisions.md m6-w2-surface) The router opens
    /// and closes `trigger: hover` popups by writing their `open`, which
    /// the host hands logic as a write with no press (so the diff that
    /// opens one carries none and it never grabs): onto `a`, `a`'s opens;
    /// straight on to `b`, `a`'s closes and `b`'s opens in one step; the
    /// anchors' own `hover` is told as before.
    #[test]
    fn the_router_opens_hover_popups_through_logic() {
        let mut k = Kept::new();
        let writes = |told: Vec<ToLogic>| -> Vec<(NodeId, bool, bool)> {
            told.into_iter()
                .filter_map(|m| match m {
                    ToLogic::Write {
                        node,
                        prop: Prop::Open,
                        value: strand_scene::PropValue::Bool(on),
                        input,
                    } => Some((node, on, input.is_some())),
                    _ => None,
                })
                .collect()
        };
        let told = k.told();
        assert!(
            told.iter().any(|m| matches!(m, ToLogic::Flag { node, flag: NodeFlag::Hover, on: true } if *node == k.a)),
            "a is hovered: {told:?}"
        );
        assert_eq!(writes(told), [(k.pop_a, true, false)]);
        k.show(2, k.pop_a);
        k.to(k.b_at);
        assert_eq!(
            writes(k.told()),
            [(k.pop_a, false, false), (k.pop_b, true, false)]
        );
        assert_eq!(k.host.renderer.next_wake(), None, "no grace left");
    }

    impl Kept {
        /// `a`'s popup shows on surface 2 with its card drawn, placed
        /// over the pointer resting on `a`, and paints: the pointer is
        /// taken onto it (`Host::rebase_pointer`).
        fn cover_a(&mut self) -> (SurfaceId, NodeId, LogicalPoint) {
            let card = NodeId::new(10, 0);
            let mut d = SceneDiff::new();
            d.set(
                card,
                Prop::Bg,
                strand_scene::PropValue::Color(strand_scene::Color::from_hex("#00ff00").unwrap()),
            );
            assert!(self.host.renderer.apply(d).is_empty());
            let s = SurfaceId(2);
            self.show(2, self.pop_a);
            // The popup's top-left 10 px left of the pointer, on the
            // bar's row.
            let left = self.a_at as i32 - 10;
            self.host.surface_placed(self.bar_s, (0, 0));
            self.host.surface_placed(s, (left, 0));
            let _ = self.told();
            let mut px = vec![0u8; 40 * 40 * 4];
            let t = PaintTarget::new(&mut px, Size::new(40, 40), 160, Scale::ONE, 0).unwrap();
            self.host
                .paint(s, &mut t.at(std::time::Duration::from_secs(1)));
            let at = LogicalPoint::new(self.a_at - left as f32, 10.0);
            (s, card, at)
        }
    }

    /// (m6-owner-fixes review, B-d) A popup mapped under the still
    /// pointer takes it: the bar hears its leave (its item is no longer
    /// hovered, spec.md 6.3) and the popup its enter, and the rebase
    /// lasts past the events that start it, so a click without moving
    /// goes to the popup. Before, the rebase took its own leave and
    /// enter for the compositor's: the bar heard no leave and the rebase
    /// ended at once.
    #[test]
    fn a_popup_mapped_under_a_still_pointer_takes_it_from_the_bar() {
        let mut k = Kept::new();
        let (s, card, at) = k.cover_a();
        let flags = k.hover_flags();
        assert!(flags.contains(&(k.a, false)), "a is left: {flags:?}");
        assert!(
            flags.contains(&(card, true)),
            "the card is hovered: {flags:?}"
        );
        assert!(k.host.rebase.is_some(), "the rebase holds");
        // A click where the pointer rests (the compositor still sends
        // it to the bar) is the card's.
        let press = |state| InputEvent::PointerButton {
            surface: k.bar_s,
            position: LogicalPoint::new(k.a_at, 10.0),
            button: button::LEFT,
            state,
            time: 1,
        };
        k.host.input(&press(ButtonState::Pressed));
        k.host.input(&press(ButtonState::Released));
        let pressed: Vec<NodeId> = k
            .told()
            .into_iter()
            .filter_map(|m| match m {
                ToLogic::Flag {
                    node,
                    flag: NodeFlag::Pressed,
                    on: true,
                } => Some(node),
                _ => None,
            })
            .collect();
        assert!(pressed.contains(&card), "{pressed:?}");
        assert!(!pressed.contains(&k.a), "{pressed:?}");
        // The compositor takes over at the next motion: leave and enter
        // in one batch, and the card stays hovered.
        k.host.input(&InputEvent::PointerLeave { surface: k.bar_s });
        k.host.input(&InputEvent::PointerEnter {
            surface: s,
            position: at,
        });
        k.host.settle_rebase();
        assert!(k.host.rebase.is_none());
        let flags = k.hover_flags();
        assert!(!flags.contains(&(card, false)), "{flags:?}");
    }

    /// (m6-owner-fixes review, B-d) The compositor taking the pointer
    /// from the bar to another client's surface ends a rebase at the end
    /// of its batch of events: the card is left, and the popup going
    /// later does not hover the bar under a pointer that is elsewhere.
    #[test]
    fn a_rebase_ends_when_the_compositor_takes_the_pointer_elsewhere() {
        let mut k = Kept::new();
        let (s, card, _) = k.cover_a();
        let _ = k.told();
        k.host.input(&InputEvent::PointerLeave { surface: k.bar_s });
        k.host.settle_rebase();
        assert!(k.host.rebase.is_none(), "the rebase ends");
        let flags = k.hover_flags();
        assert!(
            flags.contains(&(card, false)),
            "the card is left: {flags:?}"
        );
        k.host.surface_detached(s);
        let flags = k.hover_flags();
        assert!(!flags.contains(&(k.a, true)), "a is not hovered: {flags:?}");
    }

    /// (m6-owner-fixes review, spec.md 6.3) Only a pointer on a surface
    /// the popup opens from is taken onto it: `b`'s popup painting over
    /// `a`'s, its sibling, where the pointer rests leaves the pointer
    /// where the compositor has it (a sibling's stacking is the
    /// compositor's).
    #[test]
    fn a_popup_over_its_sibling_leaves_the_pointer_where_it_is() {
        let mut k = Kept::new();
        let mut d = SceneDiff::new();
        d.set(
            NodeId::new(11, 0),
            Prop::Bg,
            strand_scene::PropValue::Color(strand_scene::Color::from_hex("#00ff00").unwrap()),
        );
        assert!(k.host.renderer.apply(d).is_empty());
        let (sa, sb) = (SurfaceId(2), SurfaceId(3));
        k.show(2, k.pop_a);
        k.host.surface_placed(sa, (0, 30));
        k.host.input(&InputEvent::PointerLeave { surface: k.bar_s });
        k.host.input(&InputEvent::PointerEnter {
            surface: sa,
            position: LogicalPoint::new(10.0, 10.0),
        });
        k.show(3, k.pop_b);
        k.host.surface_placed(sb, (0, 30));
        let _ = k.told();
        let mut px = vec![0u8; 40 * 40 * 4];
        let t = PaintTarget::new(&mut px, Size::new(40, 40), 160, Scale::ONE, 0).unwrap();
        k.host
            .paint(sb, &mut t.at(std::time::Duration::from_secs(1)));
        assert!(k.host.rebase.is_none());
        assert_eq!(k.hover_flags(), [], "no hover moves");
    }

    /// (M6, decisions.md m6-w2-surface) A graced hover popup logic
    /// removes (with its anchor) takes its grace with it when the host
    /// settles the router on the new scene, and the renderer's input
    /// wake goes with it: the loop does not wake for a grace that no
    /// longer exists.
    #[test]
    fn settling_a_removed_graced_node_drops_its_wake() {
        let mut k = Kept::new();
        k.show(2, k.pop_a);
        k.to(k.a_at + 100.0);
        assert!(k.hover_flags().contains(&(k.a, false)), "a's hover goes");
        assert!(
            k.host.renderer.next_wake().is_some(),
            "the grace's end wakes"
        );
        let mut d = SceneDiff::new();
        d.remove(k.a);
        k.host.observe(&d);
        assert!(k.host.apply_diff(d).is_empty());
        k.host.settle_input();
        assert_eq!(
            k.host.renderer.next_wake(),
            None,
            "no wake for a removed grace"
        );
    }
}

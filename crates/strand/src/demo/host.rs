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

use crate::run::{NodeEvent, ScreenInfo, ToLogic};

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
    /// The surfaces idle before an input event, kept between events so
    /// pointer motion allocates nothing.
    idle: Vec<SurfaceId>,
    /// The blur ladder's last rung: says once why `blur` draws its tint.
    blur_fallback: BlurFallback,
    /// (M4) Handed-off surfaces the manager had to destroy: the GPU host
    /// sends `Release` for each (`run/gpu.rs`).
    pub gpu_released: Vec<SurfaceId>,
    /// (M4) `strand run`: the producers of the visible fed nodes (a
    /// `spectrum`'s audio tap), changed after a paint changes them.
    pub(crate) feeds: Option<crate::run::feeds::Feeds>,
    /// The session lock's content and its built-in fallback (`strand
    /// run`'s `run/lock.rs`).
    pub(crate) lock: crate::run::lock::LockScreen,
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
}

impl Forward {
    pub(crate) fn new(tx: Sender<ToLogic>) -> Self {
        Self {
            tx,
            monitors: Vec::new(),
            surfaces: HashMap::new(),
            router: Router::new(),
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
        for intent in self.router.handle(event, scene) {
            if let Some(msg) = to_logic(intent) {
                self.send(msg);
            }
        }
    }
}

/// What routing asks of logic, as the logic thread's message. `None`
/// for an intent logic cannot take yet: a drop (M4), until S-lists adds
/// `NodeEvent::Drop` on the logic side.
fn to_logic(intent: Intent) -> Option<ToLogic> {
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
                RouteEvent::Drop { payload, at } => NodeEvent::Drop { payload, at },
            },
        },
        Intent::Write { node, prop, value } => ToLogic::Write { node, prop, value },
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
    said: bool,
}

impl BlurFallback {
    /// True while a frame that asks for blur would be said: not yet said,
    /// and the compositor is known not to blur. Checked before anything
    /// is asked of the renderer, so a compositor that blurs (or one not
    /// reported yet) costs a paint nothing.
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
    /// the compositor to blur and it cannot.
    fn note_blur_fallback(&mut self, surface: SurfaceId) {
        if !self.blur_fallback.pending() || self.renderer.blur_region(surface).is_empty() {
            return;
        }
        let ns = self
            .roots
            .get(&surface)
            .and_then(|n| self.renderer.surface_spec(*n))
            .map_or_else(|| format!("surface {}", surface.0), |s| s.namespace());
        if let Some(text) = self.blur_fallback.frame(&ns) {
            log::warn!("{text}");
            if let Some(f) = &self.logic {
                f.send(ToLogic::Notice(text));
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

    /// Logic's next diff, before the renderer applies it: `input` texts
    /// it sets answer (or override) the router's edits in flight.
    pub(crate) fn observe(&mut self, diff: &SceneDiff) {
        if let Some(f) = &mut self.logic {
            f.router.observe(diff);
        }
    }

    /// After logic's diff is applied: what routing settles on the new
    /// scene (`Router::settle`: a focused input's list selects its first
    /// row) goes to logic.
    pub(crate) fn settle_input(&mut self) {
        if let Some(f) = &mut self.logic {
            for intent in f.router.settle(&mut self.renderer) {
                if let Some(msg) = to_logic(intent) {
                    f.send(msg);
                }
            }
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
        self.wake_if_changed();
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
            // index) a list showed in the last such frame.
            let lists = self.renderer.list_frames();
            eprintln!(
                "strand: damage surface={} buffer={}x{} scale={} age={} area={} rects={} gaps={} stalls={} top={}",
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
            );
        }
    }
}

impl Host {
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

    fn drop_accepted(&self, surface: SurfaceId) -> bool {
        self.logic
            .as_ref()
            .is_some_and(|f| f.router.drop_target(surface).is_some())
    }

    fn drag_source(&self, surface: SurfaceId) -> Option<NodeId> {
        let d = self.logic.as_ref()?.router.drag()?;
        (d.surface == surface).then_some(d.source)
    }

    fn input(&mut self, event: &InputEvent) {
        // The fallback lock takes its surface's input (`run/lock.rs`).
        if self.lock.input(event) {
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
            // (M4) `parallax` and `tilt` follow the router's pointer.
            let s = event.surface();
            self.renderer.set_pointer(s, f.router.pointer(s));
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
    /// `on drop`, payload and index unchanged.
    #[test]
    fn drops_are_forwarded_to_logic() {
        let node = strand_scene::NodeId::new(1, 0);
        let payload = strand_scene::DropPayload::Node(strand_scene::NodeId::new(2, 0));
        let drop = Intent::Event {
            node,
            event: RouteEvent::Drop {
                payload: payload.clone(),
                at: 3,
            },
        };
        assert_eq!(
            to_logic(drop),
            Some(ToLogic::Event {
                node,
                event: NodeEvent::Drop { payload, at: 3 },
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

    /// (M4) A still aurora and capped particles (CPU fallbacks for GPU
    /// effects) are said once each, as a `strand watch` notice, the first
    /// time a frame draws them.
    #[test]
    fn effect_fallbacks_are_said_once_as_notices() {
        use std::time::Duration;
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
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
                node: col,
                event: NodeEvent::Drop {
                    payload: DropPayload::Node(rows[0]),
                    at: 1
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
                if sizes.iter().any(|f| f.0 == child && f.2 == 30.0))),
            "{sent:?}"
        );
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
}

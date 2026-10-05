//! The surface host: `strand-surface` calls it, it forwards to the
//! renderer (`docs/architecture.md`, "Render loop"), and under
//! `STRAND_LOG=damage` it logs the damage of every painted frame, and a
//! `dropped` line for a painted frame whose commit then failed.

use std::collections::HashMap;
use std::time::Instant;

use calloop::channel::Sender;
use strand_compiler::instantiate::NodeFlag;
use strand_render::Renderer;
use strand_scene::{
    ButtonState, Damage, InputEvent, LogicalPoint, NodeId, PaintTarget, Painter, Scale, Size,
    SurfaceId,
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
    /// Tests: told of every paint and monitor change (`bench.rs`).
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

    /// Pointer input on the node under the pointer (`hit`: render's hit
    /// chain, innermost first, ending at the surface's node): every node
    /// on the chain is `hover`ed (a row is hovered while a child is), a
    /// left button held marks the chain under it `pressed` (and latches
    /// `hover` there until it is released, as a drag does), a left or
    /// right release is `click`/`secondary` on the innermost node that
    /// both the press and the release were over (logic bubbles it to the
    /// nearest handler; a release with no press on this surface clicks
    /// nothing), a scroll is `scroll(dy, dx)` on the innermost node. Other buttons have no design event and are dropped.
    pub(crate) fn input(
        &mut self,
        event: &InputEvent,
        hit: &dyn Fn(SurfaceId, LogicalPoint) -> Vec<NodeId>,
    ) {
        let surface = event.surface();
        let Some(&root) = self.surfaces.get(&surface) else {
            return;
        };
        let chain = |at: LogicalPoint| {
            let c = hit(surface, at);
            if c.is_empty() { vec![root] } else { c }
        };
        match event {
            InputEvent::PointerEnter { position, .. }
            | InputEvent::PointerMotion { position, .. } => {
                if self.pressed.contains_key(&surface) {
                    return;
                }
                let now = chain(*position);
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
                let under = chain(*position);
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
                    _ => return,
                };
                let Some(down) = down else {
                    return;
                };
                let Some(&node) = under.iter().find(|n| down.contains(n)) else {
                    return;
                };
                self.send(ToLogic::Event { node, event });
            }
            InputEvent::PointerAxis {
                horizontal,
                vertical,
                position,
                ..
            } => {
                let under = chain(*position);
                self.send(ToLogic::Event {
                    node: under[0],
                    event: NodeEvent::Scroll {
                        dy: vertical.pixels,
                        dx: horizontal.pixels,
                    },
                });
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
        self
    }
}

impl Painter for Host {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let damage = self.renderer.paint(surface, target);
        #[cfg(test)]
        if let Some(p) = &self.probe {
            p.0.painted(surface, !damage.is_empty(), target.scale, &self.renderer);
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
        let renderer = &self.renderer;
        if let Some(f) = &mut self.logic {
            f.input(event, &|surface, at| renderer.hit(surface, at));
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
            f.input(&e, &root_only);
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
        f.input(&button(button::LEFT, ButtonState::Released), &root_only);
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
            &hit,
        );
        f.input(&motion(15.0), &hit);
        f.input(&button(15.0, ButtonState::Pressed), &hit);
        // Dragging out keeps hover latched on the pressed chain.
        f.input(&motion(30.0), &hit);
        f.input(&button(30.0, ButtonState::Released), &hit);
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
        f.input(&button(5.0, ButtonState::Pressed), &hit);
        f.input(&button(15.0, ButtonState::Released), &hit);
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
        f.input(&button(5.0, ButtonState::Released), &hit);
        assert!(
            !drain(&mut el)
                .iter()
                .any(|m| matches!(m, ToLogic::Event { .. }))
        );
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

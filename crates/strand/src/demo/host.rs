//! The surface host: `strand-surface` calls it, it forwards to the
//! renderer (`docs/architecture.md`, "Render loop"), and under
//! `STRAND_LOG=damage` it logs the damage of every painted frame, and a
//! `dropped` line for a painted frame whose commit then failed.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::Instant;

use strand_render::Renderer;
use strand_scene::{
    ButtonState, Damage, InputEvent, NodeId, PaintTarget, Painter, Scale, Size, SurfaceId,
};
use strand_surface::{Monitor, SurfaceHost};

use strand_scene::input::button;

use crate::run::{ScreenInfo, ToLogic};

#[derive(Debug)]
pub struct Host {
    pub renderer: Renderer,
    log_damage: bool,
    /// `strand run`: what the logic thread hears about.
    logic: Option<Forward>,
}

/// The surface layer's facts for a logic thread (`strand run`).
#[derive(Debug)]
struct Forward {
    tx: Sender<ToLogic>,
    /// Plugged-in monitors, in plug order.
    monitors: Vec<Monitor>,
    /// The node each surface shows.
    surfaces: HashMap<SurfaceId, NodeId>,
}

impl Forward {
    fn send(&self, msg: ToLogic) {
        // The logic thread gone ends the run on the diff channel.
        let _ = self.tx.send(msg);
    }

    fn screens(&self) {
        let list = self
            .monitors
            .iter()
            .map(|m| ScreenInfo {
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
}

impl Host {
    pub fn new(renderer: Renderer, log_damage: bool) -> Self {
        Self {
            renderer,
            log_damage,
            logic: None,
        }
    }

    /// Forward monitors, surface sizes and surface input to a logic
    /// thread (`strand run`).
    pub fn forwarding(mut self, tx: Sender<ToLogic>) -> Self {
        self.logic = Some(Forward {
            tx,
            monitors: Vec::new(),
            surfaces: HashMap::new(),
        });
        self
    }
}

impl Painter for Host {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let damage = self.renderer.paint(surface, target);
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
            f.surfaces.insert(surface, node);
        }
    }

    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        self.renderer.configure_surface(surface, size, scale);
        if let Some(f) = &self.logic
            && let Some(&node) = f.surfaces.get(&surface)
        {
            let s = scale.as_f32();
            f.send(ToLogic::Size {
                node,
                width: size.w as f32 / s,
                height: size.h as f32 / s,
            });
        }
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.renderer.detach_surface(surface);
        if let Some(f) = &mut self.logic {
            f.surfaces.remove(&surface);
        }
    }

    fn monitor_added(&mut self, monitor: &Monitor, _reconnected: bool) {
        if let Some(f) = &mut self.logic {
            f.monitors.retain(|m| m.id != monitor.id);
            f.monitors.push(monitor.clone());
            f.screens();
        }
    }

    fn monitor_changed(&mut self, monitor: &Monitor) {
        if let Some(f) = &mut self.logic {
            match f.monitors.iter_mut().find(|m| m.id == monitor.id) {
                Some(m) => *m = monitor.clone(),
                None => f.monitors.push(monitor.clone()),
            }
            f.screens();
        }
    }

    fn monitor_removed(&mut self, monitor: &Monitor) {
        if let Some(f) = &mut self.logic {
            f.monitors.retain(|m| m.id != monitor.id);
            f.screens();
        }
    }

    fn monitor_forgotten(&mut self, monitor: &Monitor) {
        if let Some(f) = &self.logic {
            f.send(ToLogic::Forget(monitor.id.as_str().to_string()));
        }
    }

    /// Surface-level input until render hit-tests inside surfaces (M2):
    /// the pointer over a surface is its node's `hover`, a left or right
    /// release is `click`/`secondary` on it, a scroll is `scroll(dy, dx)`.
    fn input(&mut self, event: &InputEvent) {
        let Some(f) = &self.logic else { return };
        let Some(&node) = f.surfaces.get(&event.surface()) else {
            return;
        };
        match event {
            InputEvent::PointerEnter { .. } => f.send(ToLogic::Hover { node, on: true }),
            InputEvent::PointerLeave { .. } => f.send(ToLogic::Hover { node, on: false }),
            InputEvent::PointerButton {
                button: b,
                state: ButtonState::Released,
                ..
            } => {
                let name = match *b {
                    button::LEFT => "click",
                    button::RIGHT => "secondary",
                    button::MIDDLE => "middle",
                    _ => return,
                };
                f.send(ToLogic::Event {
                    node,
                    name,
                    args: Vec::new(),
                });
            }
            InputEvent::PointerAxis {
                horizontal,
                vertical,
                ..
            } => f.send(ToLogic::Event {
                node,
                name: "scroll",
                args: vec![vertical.pixels, horizontal.pixels],
            }),
            _ => {}
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

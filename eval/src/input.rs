//! Pointer input through the host's router (docs/eval.md, "Input"): the
//! events a surface manager would send, and what routing asks of logic,
//! mapped as crates/strand/src/demo/host.rs `to_logic` (line 316) and
//! crates/strand/src/run/shell.rs `Shell::handle` map them. Actions the
//! shell runs are recorded by the mock host, never executed.

use strand_compiler::instantiate::{Instance, NodeFlag};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_render::{Flag, Intent, NodeEvent};
use strand_scene::input::button;
use strand_scene::{AxisDelta, ButtonState, InputEvent, LogicalPoint, SurfaceId};

use crate::steps::Button;

/// Where the pointer is.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pointer {
    pub surface: Option<SurfaceId>,
    pub at: LogicalPoint,
}

pub fn code(b: Button) -> u32 {
    match b {
        Button::Left => button::LEFT,
        Button::Right => button::RIGHT,
        Button::Middle => button::MIDDLE,
    }
}

impl Pointer {
    /// The events that take the pointer to `at` on `surface` (leaving
    /// the surface it was on, entering the new one).
    pub fn move_to(&mut self, surface: SurfaceId, at: LogicalPoint, time: u32) -> Vec<InputEvent> {
        let mut out = Vec::new();
        if self.surface != Some(surface) {
            if let Some(old) = self.surface {
                out.push(InputEvent::PointerLeave { surface: old });
            }
            out.push(InputEvent::PointerEnter {
                surface,
                position: at,
            });
        }
        out.push(InputEvent::PointerMotion {
            surface,
            position: at,
            time,
        });
        self.surface = Some(surface);
        self.at = at;
        out
    }

    /// (eval-live) The event that takes the pointer off the surface it
    /// is on, as the compositor's `wl_pointer.leave` when it moves onto
    /// no surface of the shell; `None` when it is on none.
    pub fn leave(&mut self) -> Option<InputEvent> {
        let surface = self.surface.take()?;
        Some(InputEvent::PointerLeave { surface })
    }

    pub fn button(&self, b: Button, state: ButtonState, time: u32) -> Option<InputEvent> {
        Some(InputEvent::PointerButton {
            surface: self.surface?,
            position: self.at,
            button: code(b),
            state,
            time,
        })
    }

    /// A touchpad's fingers moving `dy` logical pixels (positive down)
    /// in one axis frame, `stop` when they lift with it (an
    /// `axis_stop`, which may fling).
    pub fn swipe(&self, dy: f64, stop: bool, time: u32) -> Option<InputEvent> {
        Some(InputEvent::PointerAxis {
            surface: self.surface?,
            position: self.at,
            horizontal: AxisDelta::default(),
            vertical: AxisDelta {
                pixels: dy,
                value120: 0,
                stop,
            },
            source: Some(strand_scene::AxisSource::Finger),
            time,
        })
    }

    /// A wheel scroll of `dy` notches (positive down).
    pub fn scroll(&self, dy: f64, time: u32) -> Option<InputEvent> {
        Some(InputEvent::PointerAxis {
            surface: self.surface?,
            position: self.at,
            horizontal: AxisDelta::default(),
            vertical: AxisDelta {
                // (integration-r7) The pixels a compositor sends with a
                // detent (libinput's legacy wheel step, decisions.md
                // wave3-pixels), the eval's own constant.
                pixels: dy * crate::checks::teleport::NOTCH_PX,
                value120: (dy * 120.0).round() as i32,
                stop: false,
            },
            source: Some(strand_scene::AxisSource::Wheel),
            time,
        })
    }
}

/// Hands routing's intents to logic; whether logic took any (a step at
/// the same time follows). What it refused is returned as notes.
pub fn to_logic(
    inst: &Instance,
    host: &SchemaHost,
    intents: Vec<Intent>,
    notes: &mut Vec<String>,
) -> bool {
    let mut any = false;
    for intent in intents {
        match intent {
            Intent::Flag { node, flag, on } => {
                let flag = match flag {
                    Flag::Hover => NodeFlag::Hover,
                    Flag::Pressed => NodeFlag::Pressed,
                    Flag::Focused => NodeFlag::Focused,
                    Flag::Selected => NodeFlag::Selected,
                };
                inst.set_flag(node, flag, on);
                any = true;
            }
            Intent::Event { node, event } => {
                let (name, args) = match event {
                    NodeEvent::Click => ("click", Vec::new()),
                    NodeEvent::Secondary => ("secondary", Vec::new()),
                    NodeEvent::Middle => ("middle", Vec::new()),
                    NodeEvent::Activate => ("activate", Vec::new()),
                    NodeEvent::Dismiss => ("dismiss", Vec::new()),
                    NodeEvent::Scroll { dy, dx } => {
                        ("scroll", vec![Value::float(dy), Value::float(dx)])
                    }
                    NodeEvent::Key {
                        name,
                        text,
                        modifiers: m,
                    } => (
                        "key",
                        vec![host.record(
                            "Key",
                            &[
                                ("name", Value::text(name.as_str())),
                                ("text", Value::text(text.as_str())),
                                ("ctrl", Value::Bool(m.ctrl)),
                                ("shift", Value::Bool(m.shift)),
                                ("alt", Value::Bool(m.alt)),
                                ("logo", Value::Bool(m.logo)),
                            ],
                        )],
                    ),
                    NodeEvent::Drop { .. } => {
                        notes.push("a drop is not delivered by the sandbox yet".into());
                        continue;
                    }
                };
                inst.event(node, name, args);
                any = true;
            }
            Intent::Write { node, prop, value } => {
                if let Err(e) = inst.write(node, prop, value) {
                    notes.push(format!("write to {prop}: {e}"));
                }
                any = true;
            }
            // (report) A paste's ask is answered by a `paste` step (the
            // text reaches only the input that asked); a copy stays the
            // host's.
            Intent::Clipboard(strand_scene::ClipboardAsk::Paste { .. }) => {}
            Intent::Clipboard(_) => notes.push("the clipboard is the host's".into()),
        }
    }
    any
}

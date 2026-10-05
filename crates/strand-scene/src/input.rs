//! Input events: what `strand-surface` reads from Wayland and hands to
//! render (hit testing on the rounded shape, `hover`/`pressed`) and, as
//! node events, to logic ("Render → logic is `InputEvent`s").
//!
//! Positions are surface-local logical pixels (what `wl_pointer` reports),
//! so they are independent of the buffer scale. Wayland serials stay in
//! `strand-surface`. Keyboard events join with `keyboard: on_demand |
//! exclusive` surfaces.

use crate::{LogicalPoint, SurfaceId};

/// Linux evdev button codes (`linux/input-event-codes.h`) as `wl_pointer`
/// reports them.
pub mod button {
    pub const LEFT: u32 = 0x110;
    pub const RIGHT: u32 = 0x111;
    pub const MIDDLE: u32 = 0x112;
}

/// Whether a button went down or up.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// What produced a scroll.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AxisSource {
    Wheel,
    Finger,
    Continuous,
    WheelTilt,
}

/// Scroll along one axis within one pointer frame.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct AxisDelta {
    /// Logical pixels.
    pub pixels: f64,
    /// Wheel detents in 1/120 steps (`axis_value120`; legacy discrete
    /// steps are converted ×120). 0 for touchpads.
    pub value120: i32,
    /// The scroll on this axis stopped (kinetic scrolling may start).
    pub stop: bool,
}

impl AxisDelta {
    pub fn is_zero(&self) -> bool {
        self.pixels == 0.0 && self.value120 == 0 && !self.stop
    }
}

/// An input event on one of our surfaces.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// The pointer entered `surface` at `position`.
    PointerEnter {
        surface: SurfaceId,
        position: LogicalPoint,
    },
    /// The pointer left `surface`.
    PointerLeave { surface: SurfaceId },
    PointerMotion {
        surface: SurfaceId,
        position: LogicalPoint,
        /// Milliseconds, from the compositor's clock.
        time: u32,
    },
    PointerButton {
        surface: SurfaceId,
        position: LogicalPoint,
        /// evdev code, see [`button`].
        button: u32,
        state: ButtonState,
        time: u32,
    },
    /// Scrolling; `vertical.pixels > 0` scrolls down.
    PointerAxis {
        surface: SurfaceId,
        position: LogicalPoint,
        horizontal: AxisDelta,
        vertical: AxisDelta,
        source: Option<AxisSource>,
        time: u32,
    },
}

impl InputEvent {
    /// The surface the event happened on.
    pub fn surface(&self) -> SurfaceId {
        match self {
            Self::PointerEnter { surface, .. }
            | Self::PointerLeave { surface }
            | Self::PointerMotion { surface, .. }
            | Self::PointerButton { surface, .. }
            | Self::PointerAxis { surface, .. } => *surface,
        }
    }
}

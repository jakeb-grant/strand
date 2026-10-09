//! Surface manager.
//!
//! Owns the Wayland connection on the main (render + surface) thread with
//! smithay-client-toolkit on calloop: a layer-shell surface per output for
//! each surface node (a `bar` on every monitor), output hotplug with monitor
//! identity (make + model + description), a 2–3 buffer `wl_shm` pool per
//! surface with buffer age, exact `damage_buffer` and `set_opaque_region`,
//! `wp_fractional_scale_v1` + `wp_viewporter` (integer buffer scale as the
//! fallback), frame callbacks only while something is dirty or unsettled,
//! `wp_presentation` timing through a [`FrameClock`] with paints locked to
//! the refresh rate, and pointer input as [`InputEvent`]s (the types live
//! in `strand-scene`).
//!
//! M4 adds compositor capabilities ([`caps`]), the blur ladder's
//! `ext-background-effect-v1` rung, single-pixel scrims and `attach`
//! fillets. The session lock (`ext-session-lock`,
//! `manager/session_lock.rs`) is released only for a
//! `strand_auth::UnlockToken` ([`State::unlock`]), and taken only after
//! [`State::enable_session_lock`]. Compositor-animated poses follow.
//!
//! See `docs/architecture.md`, "strand-surface" and "Render loop".

pub mod blur;
pub mod caps;
pub mod clock;
pub mod input;
mod manager;
pub mod monitor;
pub mod placement;
pub mod shm;
pub mod solid;

pub use clock::{FakeClock, FrameClock, Presentation, PresentationClock};
pub use input::{AxisDelta, AxisSource, ButtonState, InputEvent};
pub use manager::{
    Config, GRAB_WINDOW, LOCK_FALLBACK_NODE, LockError, LockState, RepaintHandle, Request, State,
    Stats, SurfaceError, SurfaceHost, SurfaceInfo, SurfaceManager,
};
pub use monitor::{MONITOR_RETENTION, Monitor, MonitorId, identity_description};
pub use placement::{Anchors, LayerConfig, PlacementError, layer_config};
pub use shm::MAX_BUFFERS;

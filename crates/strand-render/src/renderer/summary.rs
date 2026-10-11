//! (M5, S-tools) Frame timing for `strand report`: p50/p95/p99 over a
//! surface's last 600 frames, presented and discarded
//! (docs/architecture.md, "M5 interfaces"). Inert until S-tools lands
//! it: nothing is recorded and no summary is given.

use std::time::Duration;

use strand_scene::SurfaceId;

use super::Renderer;

/// One surface's frame timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameSummary {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    /// Frames presented and discarded among those summarised.
    pub presented: u64,
    pub discarded: u64,
}

/// The frames each surface drew (nothing recorded yet).
#[derive(Debug, Default)]
pub(crate) struct FrameStats {}

impl Renderer {
    /// `surface`'s frame timing (none yet).
    pub fn frame_summary(&self, surface: SurfaceId) -> Option<FrameSummary> {
        let FrameStats {} = &self.frame_stats;
        let _ = surface;
        None
    }
}

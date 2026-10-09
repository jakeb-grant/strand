//! (M4) Per-surface clocks: the nodes that read time on each surface's
//! last fresh frame, which keep its frame loop running while they are
//! drawn (design.md, "Runtime changes these need", item 4).

use std::collections::HashMap;

use strand_scene::{NodeId, SurfaceId};

/// What each surface's last fresh frame drew that runs on a clock.
#[derive(Debug, Default)]
pub(crate) struct Clocks {
    surfaces: HashMap<SurfaceId, Vec<NodeId>>,
}

impl Clocks {
    /// The clocked nodes `surface`'s fresh frame drew (none: its loop
    /// may stop).
    pub(crate) fn drawn(&mut self, surface: SurfaceId, nodes: Vec<NodeId>) {
        if nodes.is_empty() {
            self.surfaces.remove(&surface);
        } else {
            self.surfaces.insert(surface, nodes);
        }
    }

    /// True while `surface` drew a clocked node in its last fresh frame:
    /// every frame of it repaints them.
    pub(crate) fn running(&self, surface: SurfaceId) -> bool {
        self.surfaces.contains_key(&surface)
    }

    /// Forgets surfaces `keep` rejects (detached).
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(SurfaceId) -> bool) {
        self.surfaces.retain(|s, _| keep(*s));
    }
}

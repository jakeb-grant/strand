//! (M4) Surface hand-off to the GPU thread's WSI (docs/architecture.md,
//! "`strand-gpu`", "Surface hand-off"). One `wl_surface` moves between
//! `wl_shm` and the WSI; it is never recreated.
//!
//! - [`State::raw_handles`] (feature `gpu`): the connection's `wl_display`
//!   and the surface's `wl_surface` as `raw-window-handle` handles, for
//!   `GpuRequest::Attach`.
//! - [`State::hand_off`]: from then the GPU thread is the surface's only
//!   committer. The manager stops attaching shm buffers (they are freed),
//!   requesting frame callbacks and calling `paint` for it, and never
//!   commits it: configures are still acked (sctk acks them) and reported
//!   (`surface_configured`, so the host sends `Resize`), and pending state
//!   (input region, layer state) is set without a commit; the next
//!   present applies it.
//! - [`State::take_back`] after the GPU thread's `Released`: the next
//!   shm frame is painted in full (`age` 0), frame callbacks resume.
//! - A handed-off surface the manager has to destroy is kept alive (its
//!   `wl_surface` with it), [`SurfaceHost::gpu_release`] is called, and it
//!   is destroyed at `take_back`, so the swapchain always goes first.
//!
//! Mounted inside the manager (it reads the surface records).

use super::*;

/// A surface's Wayland handles for the WSI.
#[cfg(feature = "gpu")]
#[derive(Copy, Clone, Debug)]
pub struct RawHandles {
    pub display: raw_window_handle::RawDisplayHandle,
    pub window: raw_window_handle::RawWindowHandle,
}

/// Which surfaces the GPU thread commits, and the destroyed ones kept
/// alive until it lets go of them.
#[derive(Default)]
pub(super) struct HandOffs {
    on: BTreeSet<SurfaceId>,
    doomed: BTreeMap<SurfaceId, Surface>,
}

impl HandOffs {
    pub(super) fn has(&self, id: SurfaceId) -> bool {
        self.on.contains(&id)
    }

    /// `s` is being destroyed: kept if it is handed off (`None` returned),
    /// else handed back to be dropped.
    pub(super) fn keep(&mut self, s: Surface) -> Option<Surface> {
        if self.on.remove(&s.id) {
            self.doomed.insert(s.id, s);
            None
        } else {
            Some(s)
        }
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// The connection's `wl_display` and `surface`'s `wl_surface` as raw
    /// handles (`None`: no such surface, or not configured yet). They stay
    /// valid until [`State::take_back`] for a surface handed off.
    #[cfg(feature = "gpu")]
    pub fn raw_handles(&self, surface: SurfaceId) -> Option<RawHandles> {
        use raw_window_handle::{
            RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
        };
        use wayland_client::Proxy;
        // A lock surface is never presented by the GPU thread: the lock's
        // own commits must not wait on it.
        let s = self
            .surfaces
            .get(&surface)
            .filter(|s| s.configured && !matches!(s.role, Role::Lock(_)))?;
        let display = std::ptr::NonNull::new(self.conn.backend().display_ptr().cast())?;
        let window = std::ptr::NonNull::new(s.wl().id().as_ptr().cast())?;
        Some(RawHandles {
            display: RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
            window: RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
        })
    }

    /// The GPU thread presents `surface` from now on and is its only
    /// committer. False if there is no such surface.
    pub fn hand_off(&mut self, surface: SurfaceId) -> bool {
        self.cancel_deadline(surface);
        self.dirty.remove(&surface);
        let Some(s) = self
            .surfaces
            .get_mut(&surface)
            .filter(|s| !matches!(s.role, Role::Lock(_)))
        else {
            return false;
        };
        // Busy buffers stay valid for the compositor (their memory is
        // never written again); the rest is freed while presented.
        s.buffers.destroy();
        s.callback_pending = false;
        s.in_flight = None;
        s.repaint = false;
        self.gpu.on.insert(surface);
        true
    }

    /// The GPU thread let go of `surface` (`Released`): a surface
    /// destroyed meanwhile goes now; else the manager paints and commits
    /// it again, starting with a full frame.
    pub fn take_back(&mut self, surface: SurfaceId) {
        if let Some(s) = self.gpu.doomed.remove(&surface) {
            // Its swapchain is gone: the wl_surface can go.
            drop(s);
            return;
        }
        if !self.gpu.on.remove(&surface) {
            return;
        }
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        // Everything the WSI's commits replaced is sent again with the
        // first shm frame.
        s.geometry_dirty = true;
        s.opaque.clear();
        s.blur_sent = None;
        s.last_damage.clear();
        s.repaint = true;
        self.mark(surface);
    }

    /// The GPU thread commits `surface` ([`State::hand_off`]).
    pub fn is_handed_off(&self, surface: SurfaceId) -> bool {
        self.gpu.has(surface)
    }

    /// In `draw` for a handed-off surface: no paint and no commit, but a
    /// new size or scale is reported, so the host resizes the swapchain.
    pub(super) fn draw_handed_off(&mut self, id: SurfaceId) {
        self.update_geometry(id);
    }
}

//! (M4) Surface hand-off to the GPU thread's WSI (docs/architecture.md,
//! "`strand-gpu`", "Surface hand-off"). One `wl_surface` moves between
//! `wl_shm` and the WSI; it is never recreated.
//!
//! - [`State::raw_handles`] (feature `gpu`): the connection's `wl_display`
//!   and the surface's `wl_surface` as `raw-window-handle` handles, for
//!   `GpuRequest::Attach`. They are lent: from then until
//!   [`State::take_back`] the `wl_surface` outlives a destroy (below),
//!   since the GPU thread may build a swapchain on it before the
//!   `Attached` reply comes back. From then the manager attaches no
//!   buffer to it (configures are still acked with bare commits): a WSI
//!   with explicit sync (`wp_linux_drm_syncobj_v1`, Mesa's Vulkan on
//!   Hyprland) owns the surface's buffers once it builds a swapchain on
//!   it, and an shm buffer committed then has no acquire point, a
//!   protocol error that ends the connection (decisions.md
//!   syncobj-lend). Its frames resume at [`State::take_back`].
//! - [`State::hand_off`]: from then the GPU thread is the surface's only
//!   committer. The manager stops attaching shm buffers (they are freed),
//!   requesting frame callbacks and calling `paint` for it, and never
//!   commits it: configures are still acked (sctk acks them) and reported
//!   (`surface_configured`, so the host sends `Resize`), and pending state
//!   (input region, layer state) is set without a commit; the next
//!   present applies it. [`State::sync_presented`], after each frame the
//!   host sends the GPU thread, sets that frame's opaque, input and blur
//!   regions the same way.
//! - [`State::take_back`] after the GPU thread's `Released` (or when it
//!   attached the surface for readback, or ended): a handed-off surface's
//!   next shm frame is painted in full (`age` 0), frame callbacks resume;
//!   a lent one is no longer lent and paints the frames it skipped.
//! - A lent or handed-off surface the manager has to destroy is kept
//!   alive (its `wl_surface` with it), [`SurfaceHost::gpu_release`] is
//!   called, and it is destroyed at `take_back`, so the swapchain always
//!   goes first.
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

/// Which surfaces the GPU thread commits, which it holds handles of, and
/// the destroyed ones kept alive until it lets go of them.
#[derive(Default)]
pub(super) struct HandOffs {
    on: BTreeSet<SurfaceId>,
    /// Handles given out ([`State::raw_handles`]), not handed off yet.
    lent: BTreeSet<SurfaceId>,
    doomed: BTreeMap<SurfaceId, Surface>,
}

impl HandOffs {
    /// The GPU thread commits `id`.
    pub(super) fn has(&self, id: SurfaceId) -> bool {
        self.on.contains(&id)
    }

    /// `id`'s handles are lent and it is not handed off yet: nobody
    /// commits a buffer to it.
    pub(super) fn is_lent(&self, id: SurfaceId) -> bool {
        self.lent.contains(&id)
    }

    /// `s` is being destroyed: kept if it is lent or handed off (`None`
    /// returned), else handed back to be dropped.
    pub(super) fn keep(&mut self, s: Surface) -> Option<Surface> {
        let lent = self.lent.remove(&s.id);
        if self.on.remove(&s.id) || lent {
            self.doomed.insert(s.id, s);
            None
        } else {
            Some(s)
        }
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// The connection's `wl_display` and `surface`'s `wl_surface` as raw
    /// handles (`None`: no such surface, or not configured yet), lent:
    /// they stay valid until [`State::take_back`], which the host calls
    /// once the GPU thread holds no swapchain on them (a destroy meanwhile
    /// goes through [`SurfaceHost::gpu_release`]).
    #[cfg(feature = "gpu")]
    pub fn raw_handles(&mut self, surface: SurfaceId) -> Option<RawHandles> {
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
        if !self.gpu.on.contains(&surface) {
            self.gpu.lent.insert(surface);
        }
        Some(RawHandles {
            display: RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
            window: RawWindowHandle::Wayland(WaylandWindowHandle::new(window)),
        })
    }

    /// The GPU thread presents `surface` from now on and is its only
    /// committer. False if there is no such surface.
    pub fn hand_off(&mut self, surface: SurfaceId) -> bool {
        self.cancel_deadline(surface);
        self.cancel_give_up(surface);
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
        self.gpu.lent.remove(&surface);
        self.gpu.on.insert(surface);
        // Render paints its poses into the GPU's frames from now on:
        // the compositor's goes back to identity with the first present.
        self.clear_pose(surface);
        true
    }

    /// The GPU thread let go of `surface` (`Released`, an `Attached` for
    /// readback, or the thread ended): a surface destroyed meanwhile goes
    /// now; a lent one is no longer lent; a handed-off one is painted and
    /// committed by the manager again, starting with a full frame.
    pub fn take_back(&mut self, surface: SurfaceId) {
        let lent = self.gpu.lent.remove(&surface);
        if let Some(s) = self.gpu.doomed.remove(&surface) {
            // Its swapchain is gone: the wl_surface can go.
            drop(s);
            return;
        }
        if !self.gpu.on.remove(&surface) {
            if lent && let Some(s) = self.surfaces.get_mut(&surface) {
                // Frames skipped while lent (`draw`) are drawn now; its
                // buffers were never handed over, so their ages hold.
                s.repaint = true;
                self.mark(surface);
            }
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

    /// After the host sent the GPU thread a frame of handed-off
    /// `surface`: what that frame asks of the compositor besides pixels
    /// (its opaque region, the input region of a hover popup that draws
    /// nothing itself, its blur region) is set as pending state when it
    /// changed, so the present commits it with the frame. Without it a
    /// surface keeps the regions of its last shm frame: an island that
    /// grew on the GPU took the pointer only where it was before, and
    /// lost hover as soon as the pointer left that (decisions.md
    /// gpu-regions, owner-testing.md bug 23).
    pub fn sync_presented(&mut self, surface: SurfaceId) {
        if !self.gpu.has(surface) {
            return;
        }
        let Some(scale) = self.surfaces.get(&surface).map(|s| s.scale) else {
            return;
        };
        self.sync_opaque(surface, scale);
        self.sync_painted_input(surface);
        if self.blurs() {
            let rects = self.blur_rects(surface, scale);
            self.set_blur(surface, rects);
        }
    }

    /// In `draw` for a handed-off surface: no paint and no commit, but a
    /// new size or scale is reported, so the host resizes the swapchain,
    /// and set as pending state (buffer scale or viewport destination, a
    /// popup's window geometry), so the next present commits it with the
    /// new buffer.
    pub(super) fn draw_handed_off(&mut self, id: SurfaceId) {
        if !self.update_geometry(id) {
            return;
        }
        if let Some(s) = self.surfaces.get_mut(&id) {
            super::commit::send_geometry(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    use strand_fake_wayland::{Fake, SurfaceGlobals};
    use strand_scene::{Damage, NodeKind, PaintTarget, Prop, PropValue};
    use wayland_client::Connection;

    use super::*;

    /// Paints each surface once, in full.
    #[derive(Default)]
    struct Once(HashSet<SurfaceId>);

    impl Painter for Once {
        fn paint(&mut self, id: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
            self.0.insert(id);
            Damage::full(target.size)
        }
        fn wants_frame(&self, id: SurfaceId) -> bool {
            !self.0.contains(&id)
        }
    }

    impl SurfaceHost for Once {}

    const PANEL: NodeId = NodeId::new(3, 0);

    fn commits(fake: &Fake) -> usize {
        fake.surfaces()
            .iter()
            .filter(|s| s.namespace.as_deref() == Some("strand-Menu"))
            .map(|s| s.commits)
            .sum()
    }

    /// A popup grab makes its layer surface `exclusive` (and the grab's
    /// end gives the keyboard back) with a commit; on a handed-off
    /// surface that is pending state for the GPU thread's next present,
    /// never a commit of the main thread's.
    #[test]
    fn a_popup_grab_does_not_commit_a_handed_off_surface() {
        let fake = Fake::compositor(SurfaceGlobals::default());
        let conn = Connection::from_socket(fake.connect()).expect("a connection to the fake");
        let mut mgr = SurfaceManager::with_connection(conn, Once::default(), Config::default())
            .expect("surface manager starts");
        let props: HashMap<Prop, PropValue> = [
            (Prop::Name, PropValue::Text("Menu".into())),
            (Prop::Anchor, PropValue::Keyword("top_left".into())),
            (Prop::Width, PropValue::Number(200.0)),
            (Prop::Height, PropValue::Number(100.0)),
        ]
        .into_iter()
        .collect();
        let spec = SurfaceSpec::resolve(NodeKind::Panel, |p| props.get(&p));
        mgr.state_mut()
            .apply_surface_change(PANEL, SurfaceChange::Created(spec));
        let ok = mgr
            .dispatch_until(Duration::from_secs(10), |_| {
                fake.layer("strand-Menu")
                    .first()
                    .is_some_and(|s| s.buffer_commits > 0)
            })
            .expect("dispatch");
        assert!(ok, "the panel never painted");
        let id = mgr.state().surfaces_of(PANEL)[0];
        // A CPU surface: the grab commits.
        let before = commits(&fake);
        mgr.state_mut().set_grab_keyboard(id, true);
        mgr.state_mut().set_grab_keyboard(id, false);
        // Called outside a dispatch here: sent now.
        mgr.state().conn.flush().expect("flush");
        // Nothing comes back to wake the loop: let the fake read them.
        mgr.dispatch_until(Duration::from_millis(200), |_| false)
            .expect("dispatch");
        assert_eq!(
            commits(&fake),
            before + 2,
            "the grab was not committed on a CPU surface"
        );
        // Handed off: nothing from the main thread.
        assert!(mgr.state_mut().hand_off(id));
        let before = commits(&fake);
        mgr.state_mut().set_grab_keyboard(id, true);
        mgr.state().conn.flush().expect("flush");
        mgr.dispatch_until(Duration::from_millis(200), |_| false)
            .expect("dispatch");
        mgr.state_mut().set_grab_keyboard(id, false);
        mgr.state().conn.flush().expect("flush");
        mgr.dispatch_until(Duration::from_millis(200), |_| false)
            .expect("dispatch");
        assert_eq!(commits(&fake), before, "committed a handed-off surface");
    }
}

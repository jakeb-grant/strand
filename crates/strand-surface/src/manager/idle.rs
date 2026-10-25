//! (services-round) `inhibit_idle: true` on a surface: while it is shown
//! the compositor does not go idle. Each of its Wayland surfaces (one per
//! monitor, a popup's own) carries a `zwp_idle_inhibitor_v1`, made when
//! the surface is or the spec turns it on, destroyed when the spec turns
//! it off or the surface goes. The protocol makes an inhibitor count only
//! while its surface is visible, which is what "while it is shown" means;
//! a compositor without `zwp_idle_inhibit_manager_v1` gets none.

use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::{self, ZwpIdleInhibitManagerV1},
    zwp_idle_inhibitor_v1::{self, ZwpIdleInhibitorV1},
};

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    /// Makes or destroys surface `id`'s inhibitor as its spec says.
    pub(super) fn sync_idle_inhibitor(&mut self, id: SurfaceId) {
        let want = self
            .surfaces
            .get(&id)
            .and_then(|s| self.specs.get(&s.node))
            .is_some_and(|spec| spec.inhibit_idle && spec.open);
        match (want, self.inhibitors.contains_key(&id)) {
            (true, false) => {
                let (Some(manager), Some(s)) = (&self.idle_inhibit, self.surfaces.get(&id)) else {
                    return;
                };
                let inhibitor = manager.create_inhibitor(s.wl(), &self.qh, StrandGlobal);
                self.inhibitors.insert(id, inhibitor);
            }
            (false, true) => self.drop_idle_inhibitor(id),
            _ => {}
        }
    }

    /// Destroys surface `id`'s inhibitor, if it has one (before the
    /// surface itself goes).
    pub(super) fn drop_idle_inhibitor(&mut self, id: SurfaceId) {
        if let Some(i) = self.inhibitors.remove(&id) {
            i.destroy();
        }
    }

    /// Whether surface `id` holds an inhibitor now (tests).
    pub(super) fn inhibits_idle(&self, id: SurfaceId) -> bool {
        self.inhibitors.contains_key(&id)
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<ZwpIdleInhibitManagerV1, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &ZwpIdleInhibitManagerV1,
        _: zwp_idle_inhibit_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<ZwpIdleInhibitorV1, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &ZwpIdleInhibitorV1,
        _: zwp_idle_inhibitor_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

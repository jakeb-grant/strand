//! Registry, shm and the protocol objects this crate binds itself
//! (viewporter, fractional scale, presentation).

use super::*;

impl<H: SurfaceHost + 'static> ShmHandler for State<H> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl<H: SurfaceHost + 'static> ProvidesRegistryState for State<H> {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(@<H: SurfaceHost + 'static> State<H>);
delegate_dispatch2!(@<H: SurfaceHost + 'static> State<H>);

// ---- our own protocol objects ------------------------------------------------

impl<H: SurfaceHost + 'static> Dispatch2<WpViewporter, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpViewporter,
        _: wp_viewporter::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpViewport, State<H>> for SurfaceTag {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpViewport,
        _: wp_viewport::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpFractionalScaleManagerV1, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpFractionalScaleManagerV1,
        _: wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpFractionalScaleV1, State<H>> for SurfaceTag {
    fn event(
        &self,
        state: &mut State<H>,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let Some(scale) = Scale::new(scale) else {
                return;
            };
            if let Some(s) = state.surfaces.get_mut(&self.0)
                && s.scale != scale
            {
                s.scale = scale;
                state.mark(self.0);
            }
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpPresentation, State<H>> for StrandGlobal {
    fn event(
        &self,
        state: &mut State<H>,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock.set_clock_id(clk_id);
        }
    }
}

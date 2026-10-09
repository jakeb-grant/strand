//! Outputs: hotplug, monitor identity and expiry of remembered monitors.

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    pub(super) fn output_added(&mut self, output: wl_output::WlOutput) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let global = info.id;
        let now = Instant::now();
        self.forget_expired(now);
        let plugged = self.monitors.plug(
            global,
            &info.make,
            &info.model,
            info.description.as_deref().unwrap_or(""),
            info.name.clone(),
            now,
        );
        let monitor = self
            .monitors
            .set_geometry(global, output_geometry(&info))
            .unwrap_or(plugged.monitor);
        self.output_globals.insert(output.id(), global);
        self.outputs.insert(global, output);
        self.host.monitor_added(&monitor, plugged.reconnected);
        self.reconcile_all();
    }

    pub(super) fn output_removed(&mut self, output: &wl_output::WlOutput) {
        let Some(global) = self.output_globals.remove(&output.id()) else {
            return;
        };
        self.outputs.remove(&global);
        let ids: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.output == Some(global) || s.requested_output == Some(global))
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.destroy_surface(id);
        }
        if let Some(monitor) = self.monitors.unplug(global, Instant::now()) {
            self.host.monitor_removed(&monitor);
            self.arm_expiry();
        }
        // `screens: focused` surfaces it showed come back on another one.
        self.reconcile_all();
    }

    pub(super) fn arm_expiry(&mut self) {
        if self.expiry_timer.is_some() {
            return;
        }
        let Some(at) = self.monitors.next_expiry() else {
            return;
        };
        let token =
            self.handle
                .insert_source(Timer::from_deadline(at), |_, _, state: &mut State<H>| {
                    state.forget_expired(Instant::now());
                    match state.monitors.next_expiry() {
                        Some(next) => TimeoutAction::ToInstant(next),
                        None => {
                            state.expiry_timer = None;
                            TimeoutAction::Drop
                        }
                    }
                });
        match token {
            Ok(t) => self.expiry_timer = Some(t),
            Err(e) => log::warn!("cannot arm the monitor expiry timer: {}", e.error),
        }
    }

    pub(super) fn forget_expired(&mut self, now: Instant) {
        for monitor in self.monitors.expire(now) {
            self.ids
                .retain(|(_, p), _| *p != Placement::Monitor(monitor.id.clone()));
            self.host.monitor_forgotten(&monitor);
        }
    }
}

/// A monitor's scale, logical size and position from its output info.
pub(super) fn output_geometry(info: &smithay_client_toolkit::output::OutputInfo) -> Geometry {
    let integer = Scale::from_integer(info.scale_factor.max(1) as u32).unwrap_or(Scale::ONE);
    Geometry {
        scale: estimate_scale(info).unwrap_or(integer),
        logical_size: info.logical_size,
        position: info.logical_position,
    }
}

/// The output's fractional scale from its current mode and xdg-output
/// logical size (1920 px shown as 1280 logical → 1.5).
pub(super) fn estimate_scale(info: &smithay_client_toolkit::output::OutputInfo) -> Option<Scale> {
    let mode = info.modes.iter().find(|m| m.current)?;
    let (lw, lh) = info.logical_size?;
    let physical = mode.dimensions.0.max(mode.dimensions.1);
    let logical = lw.max(lh);
    if physical <= 0 || logical <= 0 {
        return None;
    }
    Scale::new(((physical as f64 * 120.0) / logical as f64).round() as u32)
}

impl<H: SurfaceHost + 'static> OutputHandler for State<H> {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.output_added(output);
    }

    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // A monitor whose make, model or description changed is a
        // different monitor.
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let Some(global) = self.output_globals.get(&output.id()).copied() else {
            return;
        };
        let same = self
            .monitors
            .id_of(global)
            .and_then(|id| self.monitors.get(id))
            .is_some_and(|m| {
                m.make == info.make
                    && m.model == info.model
                    && m.description == info.description.clone().unwrap_or_default()
            });
        if !same {
            self.output_removed(&output);
            self.output_added(output);
        } else if let Some(monitor) = self.monitors.set_geometry(global, output_geometry(&info)) {
            self.host.monitor_changed(&monitor);
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.output_removed(&output);
    }
}

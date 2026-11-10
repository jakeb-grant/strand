//! Outputs: hotplug, monitor identity and expiry of remembered monitors.

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    /// (output-id-order) A `wl_output` appeared: it is numbered among the
    /// monitors of its identity by its connector's name
    /// ([`Monitors::renumbering`]), and any of them whose id that
    /// changes is unplugged and plugged in again with its new id, as a
    /// monitor whose description changed is, so its surfaces are made
    /// for the new id. `left` is the identity it had before, when its
    /// description changed ([`Monitors::renumbering_after`]).
    pub(super) fn output_added(&mut self, output: wl_output::WlOutput, left: Option<MonitorId>) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        self.forget_expired(Instant::now());
        let known = self.host.output_ids();
        self.monitors.set_known(known);
        let plan = self.monitors.renumbering_after(
            Some(Joining {
                make: &info.make,
                model: &info.model,
                description: info.description.as_deref().unwrap_or(""),
                connector: info.name.as_deref(),
            }),
            left.as_ref(),
        );
        let moved = self.unplug_renamed(&plan.renamed);
        self.attach(output, plan.joining);
        for (output, id) in moved {
            self.attach(output, Some(id));
        }
    }

    /// Plugs `output` in as `id` (the plain identity when `None`).
    fn attach(&mut self, output: wl_output::WlOutput, id: Option<MonitorId>) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let global = info.id;
        let now = Instant::now();
        self.forget_expired(now);
        let description = info.description.as_deref().unwrap_or("");
        let id = id.unwrap_or_else(|| {
            MonitorId::new(
                &info.make,
                &info.model,
                identity_description(description, info.name.as_deref()),
            )
        });
        let plugged = self.monitors.plug_as(
            global,
            &info.make,
            &info.model,
            description,
            info.name.clone(),
            id,
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
        self.add_catchers_on(global);
    }

    /// (output-id-order) Unplugs the monitors `renamed` names, for them
    /// to be plugged in again with their new ids.
    fn unplug_renamed(
        &mut self,
        renamed: &[(u32, MonitorId)],
    ) -> Vec<(wl_output::WlOutput, MonitorId)> {
        let mut moved = Vec::new();
        for (global, id) in renamed {
            if let Some(output) = self.outputs.get(global).cloned() {
                self.detach(&output);
                moved.push((output, id.clone()));
            }
        }
        moved
    }

    /// (output-id-order) Gives every monitor plugged in the id the
    /// numbering gives it now ([`Monitors::renumbering`]); with no list
    /// from the outputs service, only the monitors of `left`'s identity
    /// ([`Monitors::renumbering_after`]).
    fn renumber(&mut self, left: Option<&MonitorId>) {
        let plan = self.monitors.renumbering_after(None, left);
        for (output, id) in self.unplug_renamed(&plan.renamed) {
            self.attach(output, Some(id));
        }
    }

    /// (output-id-order) The outputs service's numbering changed
    /// (`SurfaceHost::output_ids`: it started, or a monitor was plugged
    /// in, unplugged or turned off): the monitors shown take its ids. A
    /// list gone empty (no run of `outputs` up: nobody reads it) renames
    /// nothing; the next plug or unplug numbers its own identity without
    /// it, and no other ([`Monitors::renumbering_after`]).
    pub(super) fn sync_monitor_ids(&mut self) {
        let known = self.host.output_ids();
        if known.is_empty() || self.monitors.known() == known.as_slice() {
            return;
        }
        self.monitors.set_known(known);
        self.renumber(None);
    }

    /// The identity before numbering of the monitor `output` shows.
    pub(super) fn base_of(&self, output: &wl_output::WlOutput) -> Option<MonitorId> {
        let global = self.output_globals.get(&output.id())?;
        self.monitors.base_of(*global)
    }

    /// A `wl_output` went away: its monitor is unplugged, and the others
    /// of its identity numbered without it.
    pub(super) fn output_removed(&mut self, output: &wl_output::WlOutput) {
        let left = self.base_of(output);
        self.detach(output);
        let known = self.host.output_ids();
        self.monitors.set_known(known);
        self.renumber(left.as_ref());
    }

    fn detach(&mut self, output: &wl_output::WlOutput) {
        let Some(global) = self.output_globals.remove(&output.id()) else {
            return;
        };
        self.outputs.remove(&global);
        // (window) A window, and a popup in one, stays: the compositor
        // moves it to another output (its enter says which), and made
        // again it would lose its maximize, tiling and the size the
        // user gave it.
        let in_window: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.output == Some(global))
            .filter(|s| {
                self.root_layer(s.id)
                    .and_then(|r| self.surfaces.get(&r))
                    .is_some_and(|r| matches!(r.role, Role::Window(_)))
            })
            .map(|s| s.id)
            .collect();
        for id in &in_window {
            if let Some(s) = self.surfaces.get_mut(id) {
                s.output = None;
                s.monitor = None;
            }
        }
        let ids: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.output == Some(global) || s.requested_output == Some(global))
            .filter(|s| !in_window.contains(&s.id))
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

    /// (m6-integration-w1) Output `global`'s scale or size changed: a
    /// surface on it the compositor has not shown a frame of yet takes
    /// its scale from the output again, as at creation. The compositor
    /// told it its preferred scale when it was made, and may not tell it
    /// again before it is mapped: sway 1.9 re-sends it on a scale change
    /// only to the surfaces it has mapped, so a bar made on a new monitor
    /// just before kanshi (or `swaymsg output … scale`) scales it stayed
    /// blurry at the old scale. A preference sent after this still wins.
    pub(super) fn rescale_unshown_on(&mut self, global: u32) {
        let unshown: Vec<(SurfaceId, bool)> = self
            .surfaces
            .values()
            .filter(|s| s.output == Some(global) && !s.shown())
            .map(|s| (s.id, s.is_fractional()))
            .collect();
        for (id, fractional) in unshown {
            let (scale, integer_scale) = self.initial_scale(Some(global), fractional);
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            if s.scale != scale || s.integer_scale != integer_scale {
                s.scale = scale;
                s.integer_scale = integer_scale;
                // Resolved with the next paint (see `update_geometry`).
                self.mark(id);
            }
        }
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
        self.output_added(output, None);
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
            let left = self.base_of(&output);
            self.detach(&output);
            self.output_added(output, left);
        } else if let Some(monitor) = self.monitors.set_geometry(global, output_geometry(&info)) {
            self.rescale_unshown_on(global);
            self.host.monitor_changed(&monitor);
            // (output-change) A layer surface is fitted to its output when
            // its config is made, and no spec need change with the output:
            // what it was fitted to is out of date, up or down.
            self.refit_layers_on(&monitor.id);
            self.place_layers_on(&monitor.id);
            // (screen-position) A monitor moved can change which output
            // is leftmost, the lock's fallback (`lock_output`), as logic's
            // `screens.focused` follows it on `monitor_changed`: re-place
            // the lock's content (nothing while no lock is asked for).
            self.sync_lock_surfaces();
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

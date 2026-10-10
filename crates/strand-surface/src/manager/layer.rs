//! Layer surfaces: creation, in-place reconfiguration, configure and
//! close, and destruction.

use super::outputs::estimate_scale;
use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    /// Pushes a changed spec to `node`'s live surfaces in place.
    pub(super) fn reconfigure(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node).cloned() else {
            return;
        };
        let spec = &spec;
        if spec.kind == NodeKind::Popup {
            self.reconfigure_popups(node);
            return;
        }
        let new = self.layer_config_of(node, spec);
        let under = wants_under(spec);
        // The layer its catcher goes on: one below the surface's with a
        // scrim, else the surface's own.
        let under_layer = new
            .as_ref()
            .ok()
            .and_then(|c| Under::of_layer(spec, c))
            .map(|u| u.layer);
        let ids = self.surfaces_of(node);
        for id in ids {
            match (under, self.under_of(id)) {
                // A catcher goes under the surface (or one that starts or
                // stops catching clicks, which changes its namespace):
                // made again, catcher first (`reconcile` follows).
                (Some(_), None) => {
                    self.destroy_surface(id);
                    continue;
                }
                (Some((clicks, _)), Some((had, _))) if clicks != had => {
                    self.destroy_surface(id);
                    continue;
                }
                // A scrim that comes or goes on a surface whose layer
                // stays (one the user put on `overlay` or `bottom`)
                // moves its catcher: a layer is fixed at creation.
                (Some(_), Some(_))
                    if under_layer != self.catcher_layer(id)
                        && new.as_ref().ok().map(|c| c.layer) == self.layer_of(id) =>
                {
                    self.recreate_catcher(id, spec)
                }
                (Some((_, scrim)), Some(_)) => self.recolor_scrim(id, scrim),
                (None, Some(_)) => self.destroy_catcher(id),
                (None, None) => {}
            }
            let Ok(mut config) = new.clone() else {
                self.destroy_surface(id);
                continue;
            };
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            if let Some(size) = s
                .monitor
                .as_ref()
                .and_then(|m| self.monitors.get(m))
                .and_then(|m| m.logical_size)
            {
                config.fit(size);
            }
            if s.config == config {
                continue;
            }
            if s.config.layer != config.layer || s.config.namespace != config.namespace {
                // Layer and namespace are fixed at creation.
                self.destroy_surface(id);
                continue;
            }
            let Role::Layer(layer) = &s.role else {
                continue;
            };
            apply_layer_config(layer, &config);
            // A pose in flight keeps its offset (M4, `pose.rs`).
            if s.pose.offset != LogicalPoint::new(0.0, 0.0) {
                let [t, r, b, l] = crate::placement::posed_margin(&config, s.pose.offset);
                layer.set_margin(t, r, b, l);
            }
            if self.grab_keyboard.contains(&id) {
                layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            }
            s.config = config;
            layer.commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
            self.update_catcher(id);
        }
    }

    /// The initial buffer scale for a surface on `output` (`None`: wherever
    /// the compositor puts it) until the compositor states its preference.
    pub(super) fn initial_scale(&self, output: Option<u32>, fractional: bool) -> (Scale, i32) {
        let infos: Vec<_> = match output {
            Some(g) => self
                .outputs
                .get(&g)
                .and_then(|o| self.output_state.info(o))
                .into_iter()
                .collect(),
            None => self
                .outputs
                .values()
                .filter_map(|o| self.output_state.info(o))
                .collect(),
        };
        // Unknown output: a value every output shares, else the default.
        fn shared<T: PartialEq + Copy>(mut v: impl Iterator<Item = T>) -> Option<T> {
            let first = v.next()?;
            v.all(|x| x == first).then_some(first)
        }
        let integer_scale = shared(infos.iter().map(|i| i.scale_factor.max(1))).unwrap_or(1);
        let fallback = Scale::from_integer(integer_scale as u32).unwrap_or(Scale::ONE);
        let scale = if fractional {
            // From the output's mode and logical size, so the first frame
            // is already sharp.
            shared(infos.iter().map(estimate_scale))
                .flatten()
                .unwrap_or(fallback)
        } else {
            fallback
        };
        (scale, integer_scale)
    }

    pub(super) fn create_surface(
        &mut self,
        node: NodeId,
        spec: &SurfaceSpec,
        placement: Placement,
        global: Option<u32>,
    ) {
        let mut config = match self.layer_config_of(node, spec) {
            Ok(c) => c,
            Err(PlacementError::AutoSize(_) | PlacementError::NotLayerSurface(_)) => return,
        };
        let output = match global {
            Some(g) => match self.outputs.get(&g) {
                Some(o) => Some(o.clone()),
                None => return,
            },
            None => None,
        };
        let monitor = global
            .and_then(|g| self.monitors.id_of(g))
            .and_then(|id| self.monitors.get(id))
            .cloned();
        if let Some(size) = monitor.as_ref().and_then(|m| m.logical_size) {
            config.fit(size);
        }
        let key = (node, placement.clone());
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        if let Some(under) = Under::of_layer(spec, &config) {
            self.create_catcher(id, node, &under, global);
        }
        let wl = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            to_sctk_layer(config.layer),
            Some(config.namespace.clone()),
            output.as_ref(),
        );
        apply_layer_config(&layer, &config);
        // A viewport whenever the viewporter is there: the fractional
        // path sizes the surface with it, and a pose's scale (M4) sets
        // its destination on either path.
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl, &self.qh, SurfaceTag(id)));
        let fractional = match (&viewport, &self.fractional_manager) {
            (Some(_), Some(fm)) => Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            _ => None,
        };
        let (scale, integer_scale) = self.initial_scale(global, fractional.is_some());
        // An OSD is click-through (design example d): an empty input region
        // lets clicks reach the windows beneath it. A shadowed surface
        // takes input on its box only (set once its size is known).
        let click_through = config.click_through;
        if click_through {
            match Region::new(&self.compositor) {
                Ok(region) => wl.set_input_region(Some(region.wl_region())),
                Err(e) => log::warn!("{}: no input region: {e}", config.namespace),
            }
        }
        layer.commit();
        self.by_wl.insert(wl.id(), id);
        let surface = Surface {
            id,
            generation,
            node,
            kind: spec.kind,
            placement,
            monitor: monitor.as_ref().map(|m| m.id.clone()),
            output: global,
            requested_output: global,
            role: Role::Layer(layer),
            config,
            viewport,
            fractional,
            configured: false,
            logical: (0, 0),
            scale,
            reported_scale: None,
            integer_scale,
            buffers: ShmBuffers::new(id, self.max_buffers),
            geometry_dirty: true,
            callback_pending: false,
            commit_seq: 0,
            in_flight: None,
            ack_pending: false,
            repaint: true,
            opaque: Vec::new(),
            blur: None,
            blur_sent: Some(Vec::new()),
            pose: strand_scene::SurfacePose::IDENTITY,
            alpha: None,
            last_damage: Vec::new(),
            click_through,
            input_region: click_through.then_some(None),
            stats: Stats {
                bare_commits: 1,
                ..Stats::default()
            },
        };
        self.stats.bare_commits += 1;
        self.surfaces.insert(id, surface);
        self.host.surface_attached(id, node, monitor.as_ref());
    }

    pub(super) fn destroy_surface(&mut self, id: SurfaceId) {
        // Popups nested in it go first, innermost first (a popup must be
        // the topmost when it is destroyed).
        let children: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|c| c.role.popup_parent() == Some(id))
            .map(|c| c.id)
            .collect();
        for c in children {
            self.destroy_surface(c);
        }
        self.destroy_catcher(id);
        let Some(mut s) = self.surfaces.remove(&id) else {
            return;
        };
        self.by_wl.remove(&s.wl().id());
        self.dirty.remove(&id);
        if self.keyboard_focus == Some(id) {
            self.keyboard_focus = None;
            // A compositor may skip the leave for a destroyed surface:
            // the release would then never reach us.
            self.stop_repeat();
        }
        self.cancel_deadline(id);
        s.buffers.destroy();
        if let Some(f) = s.fractional.take() {
            f.destroy();
        }
        if let Some(v) = s.viewport.take() {
            v.destroy();
        }
        if let Some(b) = s.blur.take() {
            b.destroy();
        }
        if let Some(a) = s.alpha.take() {
            a.destroy();
        }
        // Dropping the layer surface destroys it and its wl_surface.
        drop(s);
        self.clock.forget(id);
        self.host.surface_detached(id);
        // `grab_focus` may name it still: the sync moves it on and tells
        // the new target (not the gone one).
        self.sync_popup_keyboard();
    }

    pub(super) fn destroy_node_surfaces(&mut self, node: NodeId) {
        for id in self.surfaces_of(node) {
            self.destroy_surface(id);
        }
    }
}

pub(super) fn to_sctk_layer(layer: Layer) -> wlr_layer::Layer {
    match layer {
        Layer::Background => wlr_layer::Layer::Background,
        Layer::Bottom => wlr_layer::Layer::Bottom,
        Layer::Top => wlr_layer::Layer::Top,
        Layer::Overlay => wlr_layer::Layer::Overlay,
    }
}

pub(super) fn apply_layer_config(layer: &LayerSurface, c: &LayerConfig) {
    let mut anchor = wlr_layer::Anchor::empty();
    anchor.set(wlr_layer::Anchor::TOP, c.anchors.top);
    anchor.set(wlr_layer::Anchor::BOTTOM, c.anchors.bottom);
    anchor.set(wlr_layer::Anchor::LEFT, c.anchors.left);
    anchor.set(wlr_layer::Anchor::RIGHT, c.anchors.right);
    layer.set_anchor(anchor);
    layer.set_size(c.width, c.height);
    layer.set_exclusive_zone(c.exclusive_zone);
    let [t, r, b, l] = c.margin;
    layer.set_margin(t, r, b, l);
    layer.set_keyboard_interactivity(match c.keyboard {
        Keyboard::None => KeyboardInteractivity::None,
        Keyboard::OnDemand => KeyboardInteractivity::OnDemand,
        Keyboard::Exclusive => KeyboardInteractivity::Exclusive,
    });
}

impl<H: SurfaceHost + 'static> LayerShellHandler for State<H> {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(id) = self.catcher_for(layer.wl_surface()) {
            self.catcher_closed(id, layer.wl_surface());
            return;
        }
        // The compositor took it away (usually its output is going). It
        // comes back when its output does or its spec changes.
        if let Some(id) = self.surface_for(layer.wl_surface()) {
            self.stats.closed += 1;
            self.destroy_surface(id);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(id) = self.catcher_for(layer.wl_surface()) {
            self.configure_catcher(id, layer.wl_surface(), configure.new_size);
            return;
        }
        let Some(id) = self.surface_for(layer.wl_surface()) else {
            return;
        };
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        let (w, h) = configure.new_size;
        // 0 means "your choice": what we asked for.
        let w = if w == 0 { s.config.width.max(1) } else { w };
        let h = if h == 0 { s.config.height.max(1) } else { h };
        self.configured(id, (w, h));
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// A layer surface or popup was configured at `(w, h)` logical pixels
    /// (its whole buffer).
    pub(super) fn configured(&mut self, id: SurfaceId, (w, h): (u32, u32)) {
        self.stats.configures += 1;
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        s.stats.configures += 1;
        if s.logical != (w, h) {
            s.geometry_dirty = true;
        }
        s.logical = (w, h);
        let region = s.config.input_region((w, h));
        if region != s.input_region {
            s.input_region = region;
            let wl = s.wl().clone();
            let ns = s.config.namespace.clone();
            match region {
                None => wl.set_input_region(None),
                Some(rect) => match Region::new(&self.compositor) {
                    Ok(r) => {
                        if let Some((x, y, w, h)) = rect {
                            r.add(x, y, w, h);
                        }
                        wl.set_input_region(Some(r.wl_region()));
                    }
                    Err(e) => log::warn!("{ns}: no input region: {e}"),
                },
            }
        }
        let first = !s.configured;
        s.configured = true;
        s.ack_pending = true;
        if first {
            s.repaint = true;
        }
        // The size it got may not be the one it asked for.
        self.update_catcher(id);
        // Size and scale are resolved once, right before the next paint.
        self.mark(id);
    }
}

//! Click-away catchers: transparent layer surfaces that report a press
//! outside an open `keyboard: exclusive` surface.

use super::*;

/// A transparent layer surface over an output's usable area, mapped with
/// an open `keyboard: exclusive` surface whose `open` is two-way (the
/// design's launcher): a press on it is a click outside that surface,
/// reported as [`InputEvent::ClickAway`] on it. The order of surfaces in
/// one layer is undefined (wlr-layer-shell; sway 1.9 puts the older one
/// on top for input), so it does not rely on being below: its input
/// region has a hole where that surface's box is, computed as the
/// compositor arranges both in the same area (exclusive zone 0, all four
/// edges). It takes no keyboard; clicks on bars (outside the usable
/// area) do not reach it (the router closes the surface on a press on
/// any other Strand surface, its own bar included). Every other output
/// that shows no surface of the same node gets a catcher of its own,
/// over the whole output (exclusive zone -1, bars included) and with no
/// hole.
pub(super) struct Catcher {
    pub(super) layer: LayerSurface,
    /// On the output of the surface it serves, with a hole for it.
    pub(super) primary: bool,
    /// The output it is on (the global's name), if one was asked for.
    pub(super) output: Option<u32>,
    pub(super) viewport: Option<WpViewport>,
    /// Its one transparent buffer: 1×1 scaled up by the viewport, or the
    /// output's size without viewporter.
    pub(super) buffer: Option<(RawPool, wl_buffer::WlBuffer)>,
    /// Its configured size: the output's usable area, where the surface
    /// it serves is arranged too.
    pub(super) size: Option<(u32, u32)>,
    /// The hole its input region leaves (that surface's box), as last
    /// sent.
    pub(super) hole: Option<(i32, i32, i32, i32)>,
}

impl Catcher {
    fn destroy(&mut self) {
        if let Some((_, b)) = self.buffer.take() {
            b.destroy();
        }
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
    }
}

/// User data of a catcher's buffer (nothing to track: it is never
/// written after it is created).
#[derive(Debug)]
pub struct CatcherBuffer;

/// True if a surface of `spec` gets a click-away catcher under it.
pub(super) fn wants_catcher(spec: &SurfaceSpec) -> bool {
    spec.layer.is_some()
        && spec.kind != NodeKind::Bar
        && spec.keyboard == Keyboard::Exclusive
        && spec.open_two_way
        && spec.open
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Maps click-away catchers for surface `id` on output `global`, on
    /// its layer: one on that output, and one over each other output
    /// (see [`Catcher`]).
    pub(super) fn create_catcher(
        &mut self,
        id: SurfaceId,
        node: NodeId,
        config: &LayerConfig,
        global: Option<u32>,
    ) {
        self.destroy_catcher(id);
        let output = global.and_then(|g| self.outputs.get(&g)).cloned();
        let primary = self.new_catcher(id, config, (global, output.as_ref()), true);
        self.catchers.insert(id, vec![primary]);
        if let Some(g) = global {
            self.add_secondary_catchers(id, node, config, g);
        }
    }

    /// Adds a catcher over every output but `global` (where surface `id`
    /// is) that shows no surface of `node`, and takes the node's other
    /// surfaces' catchers off `global`.
    pub(super) fn add_secondary_catchers(
        &mut self,
        id: SurfaceId,
        node: NodeId,
        config: &LayerConfig,
        global: u32,
    ) {
        let siblings: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.node == node && s.id != id)
            .map(|s| s.id)
            .collect();
        let shown: BTreeSet<u32> = siblings
            .iter()
            .filter_map(|s| self.surfaces.get(s).and_then(|s| s.output))
            .collect();
        for sid in &siblings {
            let gone: Vec<Catcher> = match self.catchers.get_mut(sid) {
                Some(list) => {
                    let (gone, keep) = std::mem::take(list)
                        .into_iter()
                        .partition(|c| !c.primary && c.output == Some(global));
                    *list = keep;
                    gone
                }
                None => Vec::new(),
            };
            for mut c in gone {
                self.catcher_of.remove(&c.layer.wl_surface().id());
                c.destroy();
            }
        }
        let others: Vec<(u32, wl_output::WlOutput)> = self
            .outputs
            .iter()
            .filter(|(g, _)| **g != global && !shown.contains(*g))
            .map(|(g, o)| (*g, o.clone()))
            .collect();
        for (g, o) in others {
            let c = self.new_catcher(id, config, (Some(g), Some(&o)), false);
            self.catchers.entry(id).or_default().push(c);
        }
    }

    pub(super) fn new_catcher(
        &mut self,
        id: SurfaceId,
        config: &LayerConfig,
        (global, output): (Option<u32>, Option<&wl_output::WlOutput>),
        primary: bool,
    ) -> Catcher {
        let wl = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            to_sctk_layer(config.layer),
            Some(format!("{}-click-away", config.namespace)),
            output,
        );
        layer.set_anchor(
            wlr_layer::Anchor::TOP
                | wlr_layer::Anchor::BOTTOM
                | wlr_layer::Anchor::LEFT
                | wlr_layer::Anchor::RIGHT,
        );
        layer.set_size(0, 0);
        // The primary one shares the usable area its surface is arranged
        // in (its hole is computed there); the others span their whole
        // output, bars included.
        layer.set_exclusive_zone(if primary { 0 } else { -1 });
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl, &self.qh, SurfaceTag(id)));
        self.catcher_of.insert(wl.id(), id);
        Catcher {
            layer,
            primary,
            output: global,
            viewport,
            buffer: None,
            size: None,
            hole: None,
        }
    }

    pub(super) fn destroy_catcher(&mut self, id: SurfaceId) {
        for mut c in self.catchers.remove(&id).unwrap_or_default() {
            self.catcher_of.remove(&c.layer.wl_surface().id());
            c.destroy();
        }
    }

    /// The compositor closed catcher `wl` of surface `id` (its output is
    /// going): only it goes, unless it was the one on that surface's own
    /// output.
    pub(super) fn catcher_closed(&mut self, id: SurfaceId, wl: &wl_surface::WlSurface) {
        let Some(list) = self.catchers.get_mut(&id) else {
            return;
        };
        let Some(i) = list.iter().position(|c| c.layer.wl_surface() == wl) else {
            return;
        };
        if list[i].primary {
            self.destroy_catcher(id);
            return;
        }
        let mut c = list.remove(i);
        self.catcher_of.remove(&wl.id());
        c.destroy();
    }

    /// Catcher `wl` of surface `id` was configured at `(w, h)`: it maps
    /// with one transparent buffer covering that.
    pub(super) fn configure_catcher(
        &mut self,
        id: SurfaceId,
        wl: &wl_surface::WlSurface,
        (w, h): (u32, u32),
    ) {
        let Some(c) = self
            .catchers
            .get_mut(&id)
            .and_then(|v| v.iter_mut().find(|c| c.layer.wl_surface() == wl))
        else {
            return;
        };
        let (bw, bh) = if c.viewport.is_some() { (1, 1) } else { (w, h) };
        let (Ok(bw), Ok(bh)) = (i32::try_from(bw.max(1)), i32::try_from(bh.max(1))) else {
            return;
        };
        let Some(len) = (bw as usize)
            .checked_mul(bh as usize)
            .and_then(|n| n.checked_mul(4))
        else {
            return;
        };
        let pool = match RawPool::new(len, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("no buffer for a click-away catcher: {e}");
                return;
            }
        };
        let mut pool = pool;
        // A fresh pool is zeroed: fully transparent.
        let buffer = pool.create_buffer(
            0,
            bw,
            bh,
            bw * 4,
            wl_shm::Format::Argb8888,
            CatcherBuffer,
            &self.qh,
        );
        let wl = c.layer.wl_surface();
        if let Some(v) = &c.viewport {
            v.set_destination(clamp_i32(w.max(1)), clamp_i32(h.max(1)));
        }
        wl.attach(Some(&buffer), 0, 0);
        wl.damage_buffer(0, 0, bw, bh);
        if let Some((_, old)) = c.buffer.replace((pool, buffer)) {
            old.destroy();
        }
        c.size = Some((w, h));
        c.hole = None;
        if !c.primary {
            // No hole: its input region is all of it (the default).
            wl.commit();
            return;
        }
        // Committed with its input region.
        self.update_catcher(id);
    }

    /// Sets catcher `id`'s input region: all of it but the box of the
    /// surface it serves (one pixel wider each way, so the box's edge
    /// never closes it), then commits.
    pub(super) fn update_catcher(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        let size = if s.configured {
            s.logical
        } else {
            (s.config.width, s.config.height)
        };
        let Some(c) = self
            .catchers
            .get_mut(&id)
            .and_then(|v| v.iter_mut().find(|c| c.primary))
        else {
            return;
        };
        let Some(area) = c.size else {
            return;
        };
        let hole = s.config.box_in(size, area);
        if c.hole == Some(hole) {
            return;
        }
        let wl = c.layer.wl_surface();
        match Region::new(&self.compositor) {
            Ok(r) => {
                r.add(0, 0, clamp_i32(area.0), clamp_i32(area.1));
                let (x, y, w, h) = hole;
                r.subtract(
                    x.saturating_sub(1),
                    y.saturating_sub(1),
                    w.saturating_add(2),
                    h.saturating_add(2),
                );
                wl.set_input_region(Some(r.wl_region()));
                c.hole = Some(hole);
            }
            Err(e) => log::warn!("no input region for a click-away catcher: {e}"),
        }
        wl.commit();
    }

    /// The surface the click-away catcher `wl` serves.
    pub(super) fn catcher_for(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.catcher_of.get(&wl.id()).copied()
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<wl_buffer::WlBuffer, State<H>> for CatcherBuffer {
    fn event(
        &self,
        _: &mut State<H>,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

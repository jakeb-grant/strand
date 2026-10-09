//! Click-away catchers and scrims: full-area layer surfaces under an open
//! surface. A catcher reports a press outside an open `keyboard:
//! exclusive` surface; a scrim (`scrim: $shadow.alpha(0.3)` on a panel
//! or popup) dims what is beneath. They are the same surface: a panel
//! with both gets one, visible, that catches clicks (architecture.md,
//! "Solid surfaces").

use super::scrim::{ScrimObject, shm_solid};
use super::*;
use crate::solid::{SolidBuffer, solid_buffer};
use strand_scene::Color;

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
///
/// A scrim is the primary catcher made visible: one solid colour
/// ([`crate::solid`]: a single-pixel buffer the viewporter scales, else
/// shm). A surface with a scrim and no click-away gets only the primary
/// one, with an empty input region (clicks pass through; a popup's own
/// grab closes it); a popup's goes on its root layer surface's layer and
/// output.
pub(super) struct Catcher {
    pub(super) layer: LayerSurface,
    /// On the output of the surface it serves, with a hole for it.
    pub(super) primary: bool,
    /// The output it is on (the global's name), if one was asked for.
    pub(super) output: Option<u32>,
    pub(super) viewport: Option<WpViewport>,
    /// It reports presses as [`InputEvent::ClickAway`] (else its input
    /// region is empty: a scrim alone).
    pub(super) clicks: bool,
    /// Its colour (`None`: transparent).
    pub(super) scrim: Option<Color>,
    /// Its one buffer (with its pool, for shm): a single pixel or 1×1
    /// scaled up by the viewport, or the output's size without
    /// viewporter.
    pub(super) buffer: Option<(Option<RawPool>, wl_buffer::WlBuffer)>,
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

/// True if a surface of `spec` gets a click-away catcher under it.
pub(super) fn wants_catcher(spec: &SurfaceSpec) -> bool {
    spec.layer.is_some()
        && spec.kind != NodeKind::Bar
        && spec.keyboard == Keyboard::Exclusive
        && spec.open_two_way
        && spec.open
}

/// The scrim an open surface of `spec` gets under it: `scrim:` on an
/// open `panel` or `popup` (`SurfaceSpec::resolve` leaves it `None`
/// elsewhere).
pub(super) fn wants_scrim(spec: &SurfaceSpec) -> Option<Color> {
    spec.scrim.filter(|_| spec.open)
}

/// What a layer surface of `spec` gets under it: `(clicks, scrim)`, or
/// `None` for nothing.
pub(super) fn wants_under(spec: &SurfaceSpec) -> Option<(bool, Option<Color>)> {
    let clicks = wants_catcher(spec);
    let scrim = wants_scrim(spec);
    (clicks || scrim.is_some()).then_some((clicks, scrim))
}

/// Where a catcher goes: the layer and namespace of the surface it
/// serves (a popup's: its root layer surface's layer), and what it does.
#[derive(Clone, Debug)]
pub(super) struct Under {
    pub(super) layer: Layer,
    pub(super) namespace: String,
    pub(super) clicks: bool,
    pub(super) scrim: Option<Color>,
}

impl Under {
    /// For a layer surface with `config`, if `spec` asks for one. It is
    /// transparent: a panel's scrim is its subsurface
    /// (`manager/scrim.rs`), over the area this catcher is configured to.
    pub(super) fn of_layer(spec: &SurfaceSpec, config: &LayerConfig) -> Option<Under> {
        let (clicks, _) = wants_under(spec)?;
        Some(Under {
            layer: config.layer,
            namespace: config.namespace.clone(),
            clicks,
            scrim: None,
        })
    }

    /// The catcher's namespace: `-click-away` when it catches clicks,
    /// else `-scrim`.
    fn namespace(&self) -> String {
        let what = if self.clicks { "click-away" } else { "scrim" };
        format!("{}-{what}", self.namespace)
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Maps click-away catchers for surface `id` on output `global`, on
    /// its layer: one on that output, and one over each other output
    /// (see [`Catcher`]).
    pub(super) fn create_catcher(
        &mut self,
        id: SurfaceId,
        node: NodeId,
        under: &Under,
        global: Option<u32>,
    ) {
        self.destroy_catcher(id);
        let output = global.and_then(|g| self.outputs.get(&g)).cloned();
        let primary = self.new_catcher(id, under, (global, output.as_ref()), true);
        self.catchers.insert(id, vec![primary]);
        if let Some(g) = global {
            self.add_secondary_catchers(id, node, under, g);
        }
    }

    /// Adds a catcher over every output but `global` (where surface `id`
    /// is) that shows no surface of `node`, and takes the node's other
    /// surfaces' catchers off `global`. A scrim alone has none (it dims
    /// its own output only).
    pub(super) fn add_secondary_catchers(
        &mut self,
        id: SurfaceId,
        node: NodeId,
        under: &Under,
        global: u32,
    ) {
        if !under.clicks {
            return;
        }
        // Secondary catchers are transparent.
        let under = Under {
            scrim: None,
            ..under.clone()
        };
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
            let c = self.new_catcher(id, &under, (Some(g), Some(&o)), false);
            self.catchers.entry(id).or_default().push(c);
        }
    }

    pub(super) fn new_catcher(
        &mut self,
        id: SurfaceId,
        under: &Under,
        (global, output): (Option<u32>, Option<&wl_output::WlOutput>),
        primary: bool,
    ) -> Catcher {
        let wl = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            to_sctk_layer(under.layer),
            Some(under.namespace()),
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
        if !under.clicks {
            // A scrim alone: clicks pass through it.
            match Region::new(&self.compositor) {
                Ok(r) => wl.set_input_region(Some(r.wl_region())),
                Err(e) => log::warn!("{}: no input region: {e}", under.namespace()),
            }
        }
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
            clicks: under.clicks,
            scrim: under.scrim,
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
    /// with one buffer of its colour covering that.
    pub(super) fn configure_catcher(
        &mut self,
        id: SurfaceId,
        wl: &wl_surface::WlSurface,
        (w, h): (u32, u32),
    ) {
        let single_pixel = self.single_pixel.clone();
        let Some(c) = self
            .catchers
            .get_mut(&id)
            .and_then(|v| v.iter_mut().find(|c| c.layer.wl_surface() == wl))
        else {
            return;
        };
        let color = c.scrim.unwrap_or_default();
        let solid = solid_buffer(color, (w, h), single_pixel.is_some(), c.viewport.is_some());
        let (buffer, (bw, bh)) = match (solid, &single_pixel) {
            (SolidBuffer::SinglePixel([r, g, b, a]), Some(sp)) => (
                (
                    None,
                    sp.create_u32_rgba_buffer(r, g, b, a, &self.qh, ScrimObject),
                ),
                (1, 1),
            ),
            (
                SolidBuffer::Shm {
                    width,
                    height,
                    pixel,
                },
                _,
            ) => match shm_solid(&self.shm, &self.qh, width, height, pixel) {
                Some(made) => made,
                None => return,
            },
            // Not reached: a single pixel is chosen only with the manager.
            (SolidBuffer::SinglePixel(_), None) => return,
        };
        let wl = c.layer.wl_surface();
        if let Some(v) = &c.viewport {
            v.set_destination(clamp_i32(w.max(1)), clamp_i32(h.max(1)));
        }
        wl.attach(Some(&buffer.1), 0, 0);
        wl.damage_buffer(0, 0, bw, bh);
        if let Some((_, old)) = c.buffer.replace(buffer) {
            old.destroy();
        }
        c.size = Some((w, h));
        c.hole = None;
        if !c.primary || !c.clicks {
            // No hole: its input region is all of it (the default), or
            // empty for a scrim alone (set at creation).
            wl.commit();
            self.place_scrim(id);
            return;
        }
        // Committed with its input region.
        self.update_catcher(id);
        self.place_scrim(id);
    }

    /// Recolours surface `id`'s scrim in place (its catcher stays).
    pub(super) fn recolor_scrim(&mut self, id: SurfaceId, scrim: Option<Color>) {
        let Some(c) = self
            .catchers
            .get_mut(&id)
            .and_then(|v| v.iter_mut().find(|c| c.primary))
        else {
            return;
        };
        if c.scrim == scrim {
            return;
        }
        c.scrim = scrim;
        let wl = c.layer.wl_surface().clone();
        if let Some(size) = c.size {
            self.configure_catcher(id, &wl, size);
        }
    }

    /// What surface `id`'s primary catcher is: `(clicks, scrim)`.
    pub(super) fn under_of(&self, id: SurfaceId) -> Option<(bool, Option<Color>)> {
        self.catchers
            .get(&id)?
            .iter()
            .find(|c| c.primary)
            .map(|c| (c.clicks, c.scrim))
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
            .and_then(|v| v.iter_mut().find(|c| c.primary && c.clicks))
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

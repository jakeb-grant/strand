//! A panel's scrim: one solid colour under the panel over the output's
//! usable area (design.md: "Scrim and dim behind popups", a single-pixel
//! buffer).
//!
//! It is a subsurface of the panel placed below it, so it is always under
//! the panel: two layer surfaces on one layer stack in an order the
//! protocol leaves open (sway 1.9 puts the older on top, wlroots' scene
//! graph and niri the newer), while a subsurface's place below its parent
//! is fixed by `wl_subsurface`. It covers the area a full-anchored
//! surface with exclusive zone 0 gets, which the panel's transparent
//! catcher (`manager/catcher.rs`, the click-away catcher when the panel
//! has one) is configured to: the panel is arranged in the same area, so
//! the scrim's position is minus the panel's position there
//! ([`LayerConfig::position_in`]). Its input region is empty: presses go
//! to the catcher beneath (or through, for a scrim alone).

use super::*;
use crate::solid::{SolidBuffer, solid_buffer};
use strand_scene::Color;
use wayland_client::protocol::{wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface};

/// A panel's scrim.
pub(super) struct Scrim {
    pub(super) color: Color,
    /// Its surface, made when it is first placed.
    parts: Option<Parts>,
    /// What was last sent: its position in the panel, its size and
    /// colour.
    placed: Option<Placed>,
}

/// A scrim's position in its panel, its size and its colour.
type Placed = ((i32, i32), (u32, u32), Color);

struct Parts {
    wl: wl_surface::WlSurface,
    sub: WlSubsurface,
    viewport: Option<WpViewport>,
    buffer: Option<(Option<RawPool>, wl_buffer::WlBuffer)>,
}

impl Scrim {
    pub(super) fn new(color: Color) -> Self {
        Scrim {
            color,
            parts: None,
            placed: None,
        }
    }

    /// Mapped in its colour.
    pub(super) fn shown(&self) -> Option<Color> {
        self.placed.map(|(_, _, c)| c)
    }

    pub(super) fn destroy(&mut self) {
        let Some(p) = self.parts.take() else {
            return;
        };
        p.sub.destroy();
        if let Some(v) = p.viewport {
            v.destroy();
        }
        if let Some((_, b)) = p.buffer {
            b.destroy();
        }
        p.wl.destroy();
        self.placed = None;
    }
}

/// User data of the scrim's buffers and subsurface (nothing to track).
#[derive(Debug)]
pub struct ScrimObject;

impl<H: SurfaceHost + 'static> State<H> {
    /// Gives panel `id` a scrim in `color` (`None`: none), placed as soon
    /// as the panel and its catcher are configured.
    pub(super) fn set_panel_scrim(&mut self, id: SurfaceId, color: Option<Color>) {
        match color {
            None => {
                if let Some(mut s) = self.scrims.remove(&id) {
                    s.destroy();
                    self.commit_for_scrim(id);
                }
            }
            Some(c) => {
                self.scrims
                    .entry(id)
                    .and_modify(|s| s.color = c)
                    .or_insert_with(|| Scrim::new(c));
                self.place_scrim(id);
            }
        }
    }

    /// Places (or recolours) panel `id`'s scrim once both the panel and
    /// its catcher know their sizes; sends nothing when nothing changed.
    pub(super) fn place_scrim(&mut self, id: SurfaceId) {
        if !self.scrims.contains_key(&id) {
            return;
        }
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        if !s.configured {
            return;
        }
        let Some(area) = self
            .catchers
            .get(&id)
            .and_then(|v| v.iter().find(|c| c.primary))
            .and_then(|c| c.size)
        else {
            return;
        };
        let (x, y) = s.config.position_in(s.logical, area);
        let at = (x.saturating_neg(), y.saturating_neg());
        let panel = s.wl().clone();
        let Some(sub) = self.subcompositor.clone() else {
            log::debug!("{}: no wl_subcompositor, no scrim", s.config.namespace);
            return;
        };
        let compositor = &self.compositor;
        let single_pixel = self.single_pixel.clone();
        let viewporter = self.viewporter.clone();
        let qh = self.qh.clone();
        let Some(scrim) = self.scrims.get_mut(&id) else {
            return;
        };
        let color = scrim.color;
        if scrim.placed == Some((at, area, color)) {
            return;
        }
        let parts = scrim.parts.get_or_insert_with(|| {
            let wl = compositor.create_surface(&qh);
            let subsurface = sub.get_subsurface(&wl, &panel, &qh, ScrimObject);
            subsurface.place_below(&panel);
            // Its buffer shows when it commits; its position still waits
            // for the panel's commit.
            subsurface.set_desync();
            if let Ok(r) = Region::new(compositor) {
                wl.set_input_region(Some(r.wl_region()));
            }
            let viewport = viewporter
                .as_ref()
                .map(|vp| vp.get_viewport(&wl, &qh, SurfaceTag(id)));
            Parts {
                wl,
                sub: subsurface,
                viewport,
                buffer: None,
            }
        });
        let recolour = scrim.placed.map(|(_, size, c)| (size, c)) != Some((area, color));
        if recolour {
            let solid = solid_buffer(
                color,
                area,
                single_pixel.is_some(),
                parts.viewport.is_some(),
            );
            let made = match (solid, &single_pixel) {
                (SolidBuffer::SinglePixel([r, g, b, a]), Some(sp)) => Some((
                    (
                        None,
                        sp.create_u32_rgba_buffer(r, g, b, a, &qh, ScrimObject),
                    ),
                    (1, 1),
                )),
                (
                    SolidBuffer::Shm {
                        width,
                        height,
                        pixel,
                    },
                    _,
                ) => shm_solid(&self.shm, &qh, width, height, pixel),
                (SolidBuffer::SinglePixel(_), None) => None,
            };
            let Some((buffer, (bw, bh))) = made else {
                return;
            };
            if let Some(v) = &parts.viewport {
                v.set_destination(clamp_i32(area.0.max(1)), clamp_i32(area.1.max(1)));
            }
            parts.wl.attach(Some(&buffer.1), 0, 0);
            parts.wl.damage_buffer(0, 0, bw, bh);
            if let Some((_, old)) = parts.buffer.replace(buffer) {
                old.destroy();
            }
            parts.wl.commit();
        }
        parts.sub.set_position(at.0, at.1);
        scrim.placed = Some((at, area, color));
        self.commit_for_scrim(id);
    }

    /// Applies the scrim's new position (or its removal) with a bare
    /// commit of panel `id`, unless a frame of the panel is on its way
    /// (it carries it) or the panel has no buffer yet (its first one
    /// will).
    fn commit_for_scrim(&mut self, id: SurfaceId) {
        let pending = self.dirty.contains(&id);
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if pending || s.ack_pending || s.stats.commits == 0 {
            return;
        }
        s.wl().commit();
        s.stats.bare_commits += 1;
        self.stats.bare_commits += 1;
    }

    /// The scrim mapped under surface `id`, in its colour: a panel's
    /// subsurface, or a popup's layer surface.
    pub(super) fn scrim_shown(&self, id: SurfaceId) -> Option<Color> {
        if let Some(s) = self.scrims.get(&id) {
            return s.shown();
        }
        self.catchers
            .get(&id)?
            .iter()
            .find(|c| c.primary && c.buffer.is_some())
            .and_then(|c| c.scrim)
    }

    pub(super) fn destroy_scrim(&mut self, id: SurfaceId) {
        if let Some(mut s) = self.scrims.remove(&id) {
            s.destroy();
        }
    }
}

/// A `width × height` ARGB8888 shm buffer of `pixel`, with its pool.
#[allow(clippy::type_complexity)]
pub(super) fn shm_solid<H: SurfaceHost + 'static>(
    shm: &Shm,
    qh: &QueueHandle<State<H>>,
    width: u32,
    height: u32,
    pixel: [u8; 4],
) -> Option<((Option<RawPool>, wl_buffer::WlBuffer), (i32, i32))> {
    let (Ok(bw), Ok(bh)) = (i32::try_from(width), i32::try_from(height)) else {
        return None;
    };
    let len = (bw as usize)
        .checked_mul(bh as usize)
        .and_then(|n| n.checked_mul(4))?;
    let mut pool = match RawPool::new(len, shm) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("no buffer for a scrim or catcher: {e}");
            return None;
        }
    };
    // A fresh pool is zeroed: fully transparent.
    if pixel != [0; 4] {
        for px in pool.mmap()[..len].chunks_exact_mut(4) {
            px.copy_from_slice(&pixel);
        }
    }
    let buffer = pool.create_buffer(0, bw, bh, bw * 4, wl_shm::Format::Argb8888, ScrimObject, qh);
    Some(((Some(pool), buffer), (bw, bh)))
}

impl<H: SurfaceHost + 'static> Dispatch2<wl_buffer::WlBuffer, State<H>> for ScrimObject {
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

impl<H: SurfaceHost + 'static> Dispatch2<WlSubsurface, State<H>> for ScrimObject {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WlSubsurface,
        _: wayland_client::protocol::wl_subsurface::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WlSubcompositor, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WlSubcompositor,
        _: wayland_client::protocol::wl_subcompositor::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

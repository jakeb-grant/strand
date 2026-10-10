//! The blur ladder's first rung on the wire: each surface's blur region
//! (from [`Painter::blur_region`], as [`crate::blur::region_rects`])
//! goes to `ext_background_effect_surface_v1.set_blur_region` with the
//! buffer commit of the frame that drew it, only when it changed, and a
//! null region once nothing asks for blur any more.

use super::*;
use crate::blur::{BlurRect, region_rects};

impl<H: SurfaceHost + 'static> State<H> {
    /// The compositor blurs: `ext_background_effect_manager_v1` is bound
    /// and its capabilities name blur.
    pub(super) fn blurs(&self) -> bool {
        self.background_effect.is_some() && self.offered.caps().background_effect
    }

    /// Brings surface `id`'s blur region up to date with what its last
    /// frame asks for, outside a paint (the compositor started blurring):
    /// sends it with a bare commit when it changed.
    pub(super) fn resend_blur(&mut self, id: SurfaceId) {
        let rects = match self.surfaces.get(&id) {
            Some(s) if s.mapped() => self.blur_rects(id, s.scale),
            _ => return,
        };
        if self.set_blur(id, rects)
            && let Some(s) = self.surfaces.get_mut(&id)
        {
            s.wl().commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
        }
    }

    /// The rectangles of what surface `id`'s last frame asks the
    /// compositor to blur (painted at `scale`), in the coordinates of its
    /// pose's scale ([`Surface::pose_factor`]), each edge to the nearest
    /// pixel: the bands down a rounded corner stay edge to edge (rounding
    /// inwards would open a one-pixel unblurred line between two), at
    /// most half a pixel past the shrunken shape.
    pub(super) fn blur_rects(&self, id: SurfaceId, scale: Scale) -> Vec<BlurRect> {
        let rects = region_rects(&self.host.blur_region(id), scale);
        let factor = self
            .surfaces
            .get(&id)
            .map_or((1.0, 1.0), |s| s.pose_factor());
        if factor == (1.0, 1.0) {
            return rects;
        }
        let mut out: Vec<BlurRect> = rects
            .into_iter()
            .filter_map(|(x, y, w, h)| {
                super::pose::posed_rect(
                    (i64::from(x), i64::from(y), i64::from(w), i64::from(h)),
                    factor,
                    false,
                )
            })
            .collect();
        out.dedup();
        out
    }

    /// Sets surface `id`'s pending blur region to `rects` if they differ
    /// from what it last sent; the caller commits. Returns whether a
    /// request went out.
    pub(super) fn set_blur(&mut self, id: SurfaceId, rects: Vec<BlurRect>) -> bool {
        if !self.blurs() {
            return false;
        }
        let Some(manager) = self.background_effect.as_ref() else {
            return false;
        };
        let Some(s) = self.surfaces.get_mut(&id) else {
            return false;
        };
        if s.blur_sent.as_ref() == Some(&rects) {
            return false;
        }
        if rects.is_empty() && s.blur.is_none() {
            // Never set: the compositor's initial region is empty.
            s.blur_sent = Some(rects);
            return false;
        }
        let effect = match &s.blur {
            Some(e) => e.clone(),
            None => {
                let e = manager.get_background_effect(s.wl(), &self.qh, SurfaceTag(id));
                s.blur = Some(e.clone());
                e
            }
        };
        if rects.is_empty() {
            effect.set_blur_region(None);
        } else {
            match Region::new(&self.compositor) {
                Ok(region) => {
                    for &(x, y, w, h) in &rects {
                        region.add(x, y, w, h);
                    }
                    // Copy semantics: the region may go at once.
                    effect.set_blur_region(Some(region.wl_region()));
                }
                Err(e) => {
                    log::warn!("{}: no blur region: {e}", s.config.namespace);
                    return false;
                }
            }
        }
        s.blur_sent = Some(rects);
        s.stats.blur_updates += 1;
        self.stats.blur_updates += 1;
        true
    }

    /// The compositor's blur capability changed: regions are sent again
    /// from what each surface's last frame asks for (a compositor that
    /// stopped blurring dropped them; one that starts has none).
    pub(super) fn blur_capability_changed(&mut self) {
        let ids: Vec<SurfaceId> = self.surfaces.keys().copied().collect();
        for id in &ids {
            if let Some(s) = self.surfaces.get_mut(id) {
                s.blur_sent = None;
            }
        }
        if self.blurs() {
            for id in ids {
                self.resend_blur(id);
            }
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<ExtBackgroundEffectSurfaceV1, State<H>> for SurfaceTag {
    fn event(
        &self,
        _: &mut State<H>,
        _: &ExtBackgroundEffectSurfaceV1,
        _: wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

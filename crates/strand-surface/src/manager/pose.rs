//! (M4) Compositor-animated poses (design.md): the pose render reports
//! for a surface each frame (`Painter::surface_pose`) is set as pending
//! surface state with that frame's commit, or with a bare commit of its
//! own when the frame drew nothing: opacity through
//! `wp_alpha_modifier_v1`, scale through the viewport's destination size
//! and the offset through the layer surface's margins. Render delegates
//! only what a surface's placement lets the compositor apply exactly
//! (`strand_render`'s `pose.rs`); a part this surface cannot take (an
//! offset on a popup, or on an axis it is centred on) is ignored here.
//!
//! While the compositor scales a surface, its surface-local coordinates
//! are the destination's, not the buffer's logical ones: the pointer
//! arrives in them and the opaque, input and blur regions are read in
//! them. So the manager maps every region it sends through the pose's
//! scale ([`Surface::pose_factor`]) and every position it hands the host
//! back (`State::send_input`), and the host hit-tests the content at
//! rest it painted.

use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::{
    self, WpAlphaModifierSurfaceV1,
};

use super::*;
use strand_scene::SurfacePose;

/// `wp_alpha_modifier_surface_v1.set_multiplier`'s value for `opacity`:
/// `u32::MAX` is fully opaque.
pub(super) fn multiplier(opacity: f32) -> u32 {
    let o = if opacity.is_finite() {
        opacity.clamp(0.0, 1.0)
    } else {
        1.0
    };
    (f64::from(o) * f64::from(u32::MAX)).round() as u32
}

/// The viewport destination for a surface of `logical` size under
/// `scale`: at least one pixel each way.
pub(super) fn destination((w, h): (u32, u32), scale: f32) -> (i32, i32) {
    let d = |v: u32| ((v as f32 * scale).round() as i64).clamp(1, i64::from(i32::MAX)) as i32;
    (d(w), d(h))
}

/// A logical rectangle (`x, y, w, h`) of a surface at rest in the
/// surface-local coordinates of its pose's scale `(fx, fy)`
/// ([`Surface::pose_factor`]): with `inner`, the largest whole-pixel
/// rectangle inside it (an opaque or blur region must not claim a pixel
/// the shrunken content does not cover), else the nearest one. `None`
/// when nothing is left.
pub(super) fn posed_rect(
    (x, y, w, h): (i64, i64, i64, i64),
    (fx, fy): (f64, f64),
    inner: bool,
) -> Option<(i32, i32, i32, i32)> {
    let edge = |v: i64, f: f64, up: bool| {
        let v = v as f64 * f;
        let v = match (inner, up) {
            (false, _) => v.round(),
            (true, true) => v.ceil(),
            (true, false) => v.floor(),
        };
        v.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i64
    };
    let (x0, y0) = (edge(x, fx, true), edge(y, fy, true));
    let (x1, y1) = (
        edge(x.saturating_add(w), fx, false),
        edge(y.saturating_add(h), fy, false),
    );
    let dim = |a: i64, b: i64| i32::try_from(b - a).ok().filter(|d| *d > 0);
    Some((x0 as i32, y0 as i32, dim(x0, x1)?, dim(y0, y1)?))
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Sets `id`'s pose for its next commit if render reports a new one;
    /// returns true when something was set (a commit must follow for it
    /// to show). A lock surface takes none.
    pub(super) fn sync_pose(&mut self, id: SurfaceId) -> bool {
        let want = self.host.surface_pose(id).unwrap_or(SurfacePose::IDENTITY);
        let Some(s) = self.surfaces.get_mut(&id) else {
            return false;
        };
        if matches!(s.role, Role::Lock(_)) || want == s.pose {
            return false;
        }
        let old = std::mem::replace(&mut s.pose, want);
        if want.opacity != old.opacity
            && let Some(am) = &self.alpha_modifier
        {
            let wl = s.role.wl().clone();
            let qh = &self.qh;
            let alpha = s
                .alpha
                .get_or_insert_with(|| am.get_surface(&wl, qh, SurfaceTag(id)));
            alpha.set_multiplier(multiplier(want.opacity));
        }
        let rescaled = want.scale != old.scale;
        if rescaled {
            s.send_destination();
        }
        let moved = want.offset != old.offset
            && if let Role::Layer(layer) = &s.role {
                let [t, r, b, l] = crate::placement::posed_margin(&s.config, want.offset);
                layer.set_margin(t, r, b, l);
                true
            } else {
                false
            };
        s.stats.poses += 1;
        self.stats.poses += 1;
        if rescaled {
            // Read in the destination's coordinates now.
            self.sync_input_region(id);
        }
        if moved {
            // Where it lies now (the tray's click point).
            self.place_layer(id);
        }
        true
    }

    /// (M4) Drops `id`'s compositor pose as pending state, committed by
    /// whoever commits it next: for a surface the GPU is about to
    /// present, whose poses render paints into its frames
    /// (architecture.md, "Surface hand-off").
    pub fn clear_pose(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if s.pose == SurfacePose::IDENTITY {
            return;
        }
        let old = std::mem::replace(&mut s.pose, SurfacePose::IDENTITY);
        if let Some(alpha) = &s.alpha {
            alpha.set_multiplier(u32::MAX);
        }
        if old.scale != 1.0 {
            s.send_destination();
        }
        if let Role::Layer(layer) = &s.role {
            let [t, r, b, l] = s.config.margin;
            layer.set_margin(t, r, b, l);
        }
        if old.scale != 1.0 {
            self.sync_input_region(id);
        }
    }
}

impl Surface {
    /// How the compositor scales this surface's coordinates under its
    /// pose, per axis: the viewport destination over the logical size
    /// (1 at rest, without a viewport, or before a configure).
    pub(super) fn pose_factor(&self) -> (f64, f64) {
        let (w, h) = self.logical;
        if self.viewport.is_none() || self.pose.scale == 1.0 || w == 0 || h == 0 {
            return (1.0, 1.0);
        }
        let (dw, dh) = destination(self.logical, self.pose.scale);
        (f64::from(dw) / f64::from(w), f64::from(dh) / f64::from(h))
    }

    /// Sets the viewport's destination for the current logical size and
    /// pose scale: the logical size on the fractional path, unset on the
    /// integer one at rest (its buffer scale sizes it).
    pub(super) fn send_destination(&self) {
        let Some(vp) = &self.viewport else {
            return;
        };
        if self.logical == (0, 0) {
            return;
        }
        if self.is_fractional() || self.pose.scale != 1.0 {
            let (w, h) = destination(self.logical, self.pose.scale);
            vp.set_destination(w, h);
        } else {
            vp.set_destination(-1, -1);
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpAlphaModifierSurfaceV1, State<H>> for SurfaceTag {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpAlphaModifierSurfaceV1,
        _: wp_alpha_modifier_surface_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipliers_and_destinations() {
        assert_eq!(multiplier(1.0), u32::MAX);
        assert_eq!(multiplier(0.0), 0);
        assert_eq!(multiplier(f32::NAN), u32::MAX);
        assert_eq!(multiplier(2.0), u32::MAX);
        assert!(multiplier(0.5).abs_diff(u32::MAX / 2) <= 1);
        assert_eq!(destination((300, 200), 0.8), (240, 160));
        assert_eq!(destination((300, 200), 0.0), (1, 1));
        assert_eq!(destination((300, 200), 1.0), (300, 200));
    }

    #[test]
    fn regions_follow_the_pose_scale() {
        // A 200 × 100 box 20 px inside its shadow, at half size.
        let half = (0.5, 0.5);
        assert_eq!(
            posed_rect((20, 20, 200, 100), half, true),
            Some((10, 10, 100, 50))
        );
        // Inner rounds inwards, nearest rounds to the nearest pixel.
        let f = (0.9, 0.9);
        assert_eq!(posed_rect((5, 5, 10, 10), f, true), Some((5, 5, 8, 8)));
        assert_eq!(posed_rect((5, 5, 10, 10), f, false), Some((5, 5, 9, 9)));
        // At rest nothing moves; what vanishes is dropped.
        assert_eq!(
            posed_rect((3, 4, 5, 6), (1.0, 1.0), true),
            Some((3, 4, 5, 6))
        );
        assert_eq!(posed_rect((3, 3, 1, 1), (0.01, 0.01), true), None);
        assert_eq!(posed_rect((0, 0, 0, 10), (1.0, 1.0), false), None);
    }
}

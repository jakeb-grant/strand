//! (M4) Where each surface lies on its output, for the host
//! ([`SurfaceHost::surface_placed`]): a press on a surface becomes an
//! output position with it (the tray's click point, which apps placing a
//! menu of their own read).
//!
//! A layer surface's position is never told to its client, so it is
//! derived as the compositor arranges one (`LayerConfig::position_in`)
//! over the whole output: other surfaces' exclusive zones are not known
//! here, so a panel anchored beside a bar is placed as if the bar were
//! not there (off by the bar's zone on that axis). It is placed at the
//! margins its compositor pose's offset set (`placement::posed_margin`),
//! again whenever that offset or its output's logical size changes. A
//! popup's comes from
//! its configure, which gives its box relative to its parent's window
//! geometry (a layer surface's is the surface; a popup's is its box).

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    /// Places layer surface `id` on its monitor for its configured size.
    pub(super) fn place_layer(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        if !matches!(s.role, Role::Layer(_)) {
            return;
        }
        let area = s
            .monitor
            .as_ref()
            .and_then(|m| self.monitors.get(m))
            .and_then(|m| m.logical_size)
            .filter(|(w, h)| *w > 0 && *h > 0)
            .map(|(w, h)| (w as u32, h as u32));
        // At the margins a pose's offset moved it to (M4): a root's
        // static `x`/`y` on a corner panel is delegated for good.
        let mut config = s.config.clone();
        config.margin = crate::placement::posed_margin(&s.config, s.pose.offset);
        let origin = area.map(|area| config.position_in(s.logical, area));
        self.set_origin(id, origin);
    }

    /// Places again the layer surfaces on `monitor`, whose logical size
    /// may have changed with no configure (a panel of a fixed size).
    pub(super) fn place_layers_on(&mut self, monitor: &MonitorId) {
        let ids: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.monitor.as_ref() == Some(monitor))
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.place_layer(id);
        }
    }

    /// Places popup `id` from its configure's position `at`: its box's
    /// top-left corner relative to its parent's window geometry.
    pub(super) fn place_popup(&mut self, id: SurfaceId, (x, y): (i32, i32)) {
        let Some(Role::Popup { parent, config, .. }) = self.surfaces.get(&id).map(|s| &s.role)
        else {
            return;
        };
        let [t, _, _, l] = config.overhang;
        let origin = self.surfaces.get(parent).and_then(|p| {
            let (px, py) = p.origin?;
            // A popup parent's window geometry is its box, inside its
            // overhang; a layer surface's is the whole surface.
            let (gx, gy) = match &p.role {
                Role::Popup { config, .. } => (config.overhang[3], config.overhang[0]),
                _ => (0, 0),
            };
            Some((
                px.saturating_add(gx).saturating_add(x).saturating_sub(l),
                py.saturating_add(gy).saturating_add(y).saturating_sub(t),
            ))
        });
        self.set_origin(id, origin);
    }

    fn set_origin(&mut self, id: SurfaceId, origin: Option<(i32, i32)>) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if s.origin == origin {
            return;
        }
        s.origin = origin;
        if let Some(o) = origin {
            self.host.surface_placed(id, o);
        }
    }
}

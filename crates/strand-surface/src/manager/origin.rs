//! (M4) Where each surface lies in the compositor's logical layout, for
//! the host ([`SurfaceHost::surface_placed`]): a press on a surface
//! becomes a screen position with it (the tray's click point, which apps
//! placing a menu of their own read as global coordinates).
//!
//! A layer surface's position is never told to its client, so it is
//! derived as the compositor arranges one (`placement::arranged_area`,
//! `LayerConfig::position_in_rect`): in its output's usable area, which
//! our own surfaces' exclusive zones take from (a panel anchored beside
//! a bar lands past the bar's zone), then moved by the output's position
//! in the layout. Other programs' exclusive zones are not known to a
//! client and are not taken out. It is placed at the margins its
//! compositor pose's offset set (`placement::posed_margin`), again
//! whenever that offset, its output's geometry or an exclusive zone on
//! its output changes (one that comes, goes, changes size or leaves with
//! its surface: `Surface::zone_on` remembers where it was counted). A popup's comes from its configure, which gives
//! its box relative to its parent's window geometry (a layer surface's
//! is the surface; a popup's is its box).

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    /// Places layer surface `id` on its monitor for its configured size.
    /// The other layer surfaces on the monitor its exclusive zone takes
    /// from now, and on the one it took from when last placed, are placed
    /// again: their usable area changed with it (a zone that grew, shrank
    /// to none, or moved to another output with its surface).
    pub(super) fn place_layer(&mut self, id: SurfaceId) {
        self.place_layer_only(id);
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        let now = match s.role {
            Role::Layer(_) if s.config.exclusive_zone > 0 => s.monitor.clone(),
            _ => None,
        };
        let before = std::mem::replace(&mut s.zone_on, now.clone());
        let before = before.filter(|b| now.as_ref() != Some(b));
        for m in [now, before].into_iter().flatten() {
            for other in self.layers_on(&m) {
                if other != id {
                    self.place_layer_only(other);
                }
            }
        }
    }

    /// The layer surfaces on `monitor`, in the order they were made.
    fn layers_on(&self, monitor: &MonitorId) -> Vec<SurfaceId> {
        self.surfaces
            .values()
            .filter(|s| matches!(s.role, Role::Layer(_)) && s.monitor.as_ref() == Some(monitor))
            .map(|s| s.id)
            .collect()
    }

    fn place_layer_only(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        if !matches!(s.role, Role::Layer(_)) {
            return;
        }
        let Some(monitor) = s.monitor.as_ref().and_then(|m| self.monitors.get(m)) else {
            self.set_origin(id, None);
            return;
        };
        let full = monitor
            .logical_size
            .filter(|(w, h)| *w > 0 && *h > 0)
            .map(|(w, h)| (w as u32, h as u32));
        let (px, py) = monitor.position.unwrap_or((0, 0));
        // Our surfaces on that output whose zones take from its area.
        let mid = monitor.id.clone();
        let configs: Vec<(SurfaceId, &crate::placement::LayerConfig)> = self
            .surfaces
            .values()
            .filter(|o| matches!(o.role, Role::Layer(_)) && o.monitor.as_ref() == Some(&mid))
            .map(|o| (o.id, &o.config))
            .collect();
        // At the margins a pose's offset moved it to (M4): a root's
        // static `x`/`y` on a corner panel is delegated for good.
        let mut config = s.config.clone();
        config.margin = crate::placement::posed_margin(&s.config, s.pose.offset);
        let origin = full.map(|full| {
            let area = crate::placement::arranged_area(full, &configs, &id);
            let (x, y) = config.position_in_rect(s.logical, area);
            (x.saturating_add(px), y.saturating_add(py))
        });
        self.set_origin(id, origin);
    }

    /// Places again the layer surfaces on `monitor`, whose logical size
    /// or position may have changed with no configure (a panel of a
    /// fixed size), or whose usable area changed (an exclusive surface
    /// went).
    pub(super) fn place_layers_on(&mut self, monitor: &MonitorId) {
        for id in self.layers_on(monitor) {
            self.place_layer_only(id);
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

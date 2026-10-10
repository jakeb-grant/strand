//! The fake's `xdg_wm_base`, for popups only: enough for the surface
//! manager to map an `xdg_popup` (on a layer surface or another popup),
//! configured at its positioner's size on its first commit. Grabs are
//! accepted and ignored; keyboard focus moves only by [`crate::Cmd`]s.

use std::sync::Mutex;

use wayland_protocols::xdg::shell::server::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_server::backend::ObjectId;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::Server;

/// A popup waiting for (or past) its first configure.
pub(crate) struct PopupState {
    pub(crate) xdg_surface: XdgSurface,
    pub(crate) popup: XdgPopup,
    pub(crate) size: (i32, i32),
    pub(crate) configured: bool,
}

impl PopupState {
    /// Configures it at its size, once (on its surface's first commit).
    pub(crate) fn configure(&mut self, serial: u32) {
        if self.configured || !self.popup.is_alive() || !self.xdg_surface.is_alive() {
            return;
        }
        self.configured = true;
        self.popup.configure(0, 0, self.size.0, self.size.1);
        self.xdg_surface.configure(serial);
    }
}

pub(crate) fn create_global(dh: &DisplayHandle) {
    dh.create_global::<Server, XdgWmBase, ()>(6, ());
}

impl GlobalDispatch<XdgWmBase, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgWmBase>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<XdgWmBase, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgWmBase,
        request: xdg_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::CreatePositioner { id } => {
                init.init(id, Mutex::new((1, 1)));
            }
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                init.init(id, surface.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgPositioner, Mutex<(i32, i32)>> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgPositioner,
        request: xdg_positioner::Request,
        size: &Mutex<(i32, i32)>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let xdg_positioner::Request::SetSize { width, height } = request
            && let Ok(mut s) = size.lock()
        {
            *s = (width.max(1), height.max(1));
        }
    }
}

impl Dispatch<XdgSurface, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        xdg_surface: &XdgSurface,
        request: xdg_surface::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_surface::Request::GetPopup { id, positioner, .. } = request {
            let popup = init.init(id, ());
            let size = positioner
                .data::<Mutex<(i32, i32)>>()
                .and_then(|m| m.lock().ok().map(|s| *s))
                .unwrap_or((1, 1));
            state.surf.set_popup(
                surface,
                PopupState {
                    xdg_surface: xdg_surface.clone(),
                    popup,
                    size,
                    configured: false,
                },
            );
        }
    }
}

impl Dispatch<XdgPopup, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        popup: &XdgPopup,
        request: xdg_popup::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // A reposition is answered at once, in place (no configure: the
        // manager's popups keep their size).
        if let xdg_popup::Request::Reposition { token, .. } = request {
            popup.repositioned(token);
        }
    }
}

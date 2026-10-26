//! The fake's `xdg_wm_base`: enough for the surface manager to map an
//! `xdg_popup` (on a layer surface, another popup or a window),
//! configured at its positioner's size on its first commit, and at its
//! new positioner's size when it is repositioned (xdg_popup v3: a
//! `repositioned`, then the popup's and its surface's configure, at once
//! once it was configured, else on its first commit). Grabs are accepted
//! and ignored; keyboard focus moves only by [`crate::Cmd`]s.
//!
//! (oi-window) And an `xdg_toplevel` (strand's `window`): its first
//! configure answers its first commit, as a floating compositor's does (no
//! size, the window's own, activated) or, with
//! [`crate::SurfaceGlobals::window_configure_on_cmd`], as the last
//! [`crate::Cmd::ConfigureWindow`] for its app id says (waiting for one);
//! each later [`crate::Cmd::ConfigureWindow`] configures it at once
//! ([`WindowConfigure`]: a size, the states, the bounds), and
//! [`crate::Cmd::CloseWindow`] is the compositor's close; the last window
//! of an app id gone, what was kept for it is forgotten. What the window
//! asks (its title, app id, size limits, window geometry, acks and its
//! maximize, fullscreen, minimize, move and resize) is recorded
//! ([`ToplevelRecord`]), never answered: the fake's states change only by
//! commands, as a test or the eval surface leg decide them.

use std::sync::Mutex;

use wayland_protocols::xdg::shell::server::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
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

    /// Repositioned at `size` (`xdg_popup.reposition`, answered with
    /// `token`): configured again at it now if it was configured (`serial`
    /// is that configure's), else at its first commit, as a compositor
    /// answers a reposition with the new geometry.
    pub(crate) fn reposition(&mut self, token: u32, size: (i32, i32), serial: u32) -> bool {
        self.size = size;
        if !self.popup.is_alive() || !self.xdg_surface.is_alive() {
            return false;
        }
        self.popup.repositioned(token);
        if !self.configured {
            return false;
        }
        self.popup.configure(0, 0, size.0, size.1);
        self.xdg_surface.configure(serial);
        true
    }
}

/// (oi-window) What the fake says of a window in an `xdg_toplevel`
/// configure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowConfigure {
    /// The size it gives (0 on an axis: the window's own choice).
    pub size: (u32, u32),
    pub maximized: bool,
    pub fullscreen: bool,
    /// Tiled on all four edges.
    pub tiled: bool,
    /// Focused.
    pub activated: bool,
    /// An interactive resize is under way: the size is the compositor's.
    pub resizing: bool,
    /// `configure_bounds` (`None`: not sent).
    pub bounds: Option<(u32, u32)>,
}

impl Default for WindowConfigure {
    /// A new floating window's: no size, focused.
    fn default() -> Self {
        WindowConfigure {
            size: (0, 0),
            maximized: false,
            fullscreen: false,
            tiled: false,
            activated: true,
            resizing: false,
            bounds: None,
        }
    }
}

/// (oi-window) What a window asked of the fake and was told
/// ([`crate::SurfaceRecord::toplevel`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToplevelRecord {
    pub title: Option<String>,
    pub app_id: Option<String>,
    /// `set_min_size` and `set_max_size` as last asked (0: none).
    pub min_size: (i32, i32),
    pub max_size: (i32, i32),
    /// Its window geometry (`xdg_surface.set_window_geometry`: x, y,
    /// width, height) as of its last commit.
    pub geometry: Option<(i32, i32, i32, i32)>,
    /// Every configure sent, with its serial, in order.
    pub configures: Vec<(u32, WindowConfigure)>,
    /// Every `ack_configure`'s serial, in order.
    pub acked: Vec<u32>,
    /// What it asked the compositor to do, in order: `maximize`,
    /// `unmaximize`, `fullscreen`, `unfullscreen`, `minimize`,
    /// `move <serial>`, `resize <serial> <edge>`, `menu <serial>`.
    pub asks: Vec<String>,
    /// The compositor's closes sent.
    pub closed: usize,
}

/// (oi-window) A live toplevel's role objects.
pub(crate) struct ToplevelLive {
    pub(crate) xdg_surface: XdgSurface,
    pub(crate) toplevel: XdgToplevel,
    /// Its first commit came (the one a first configure answers).
    pub(crate) committed: bool,
    /// It was configured at least once.
    pub(crate) configured: bool,
}

impl ToplevelLive {
    /// Sends `c` as configure `serial` (the first one also says the
    /// window-manager capabilities: all four).
    pub(crate) fn configure(&mut self, c: &WindowConfigure, serial: u32) {
        if !self.toplevel.is_alive() || !self.xdg_surface.is_alive() {
            return;
        }
        let v = self.toplevel.version();
        if let Some((w, h)) = c.bounds
            && v >= 4
        {
            self.toplevel.configure_bounds(w as i32, h as i32);
        }
        if !self.configured && v >= 5 {
            use xdg_toplevel::WmCapabilities as Cap;
            let caps = [
                Cap::WindowMenu,
                Cap::Maximize,
                Cap::Fullscreen,
                Cap::Minimize,
            ];
            self.toplevel.wm_capabilities(
                caps.iter()
                    .flat_map(|c| u32::from(*c).to_ne_bytes())
                    .collect(),
            );
        }
        use xdg_toplevel::State;
        let mut states = Vec::new();
        for (on, state) in [
            (c.maximized, State::Maximized),
            (c.fullscreen, State::Fullscreen),
            (c.resizing, State::Resizing),
            (c.activated, State::Activated),
        ] {
            if on {
                states.push(state);
            }
        }
        if c.tiled && v >= 2 {
            states.extend([
                State::TiledLeft,
                State::TiledRight,
                State::TiledTop,
                State::TiledBottom,
            ]);
        }
        self.toplevel.configure(
            c.size.0 as i32,
            c.size.1 as i32,
            states
                .iter()
                .flat_map(|s| u32::from(*s).to_ne_bytes())
                .collect(),
        );
        self.xdg_surface.configure(serial);
        self.configured = true;
    }
}

/// The positioner's size.
fn positioner_size(positioner: &XdgPositioner) -> (i32, i32) {
    positioner
        .data::<Mutex<(i32, i32)>>()
        .and_then(|m| m.lock().ok().map(|s| *s))
        .unwrap_or((1, 1))
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
        match request {
            xdg_surface::Request::GetPopup { id, positioner, .. } => {
                let popup = init.init(id, surface.clone());
                let size = positioner_size(&positioner);
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
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel = init.init(id, surface.clone());
                state.surf.set_toplevel(
                    surface,
                    ToplevelLive {
                        xdg_surface: xdg_surface.clone(),
                        toplevel,
                        committed: false,
                        configured: false,
                    },
                );
            }
            xdg_surface::Request::SetWindowGeometry {
                x,
                y,
                width,
                height,
            } => state.surf.set_geometry(surface, (x, y, width, height)),
            xdg_surface::Request::AckConfigure { serial } => {
                state
                    .surf
                    .toplevel_record(surface, |t| t.acked.push(serial));
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgToplevel, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgToplevel,
        request: xdg_toplevel::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_toplevel::Request as R;
        let ask = |state: &mut Self, what: String| {
            state.surf.toplevel_record(surface, |t| t.asks.push(what));
        };
        match request {
            R::SetTitle { title } => state
                .surf
                .toplevel_record(surface, |t| t.title = Some(title)),
            R::SetAppId { app_id } => state
                .surf
                .toplevel_record(surface, |t| t.app_id = Some(app_id)),
            R::SetMinSize { width, height } => state
                .surf
                .toplevel_record(surface, |t| t.min_size = (width, height)),
            R::SetMaxSize { width, height } => state
                .surf
                .toplevel_record(surface, |t| t.max_size = (width, height)),
            R::SetMaximized => ask(state, "maximize".into()),
            R::UnsetMaximized => ask(state, "unmaximize".into()),
            R::SetFullscreen { .. } => ask(state, "fullscreen".into()),
            R::UnsetFullscreen => ask(state, "unfullscreen".into()),
            R::SetMinimized => ask(state, "minimize".into()),
            R::Move { serial, .. } => ask(state, format!("move {serial}")),
            R::Resize { serial, edges, .. } => {
                let edge = edges.into_result().map_or(0, u32::from);
                ask(state, format!("resize {serial} {edge}"));
            }
            R::ShowWindowMenu { serial, .. } => ask(state, format!("menu {serial}")),
            R::Destroy => state.surf.drop_toplevel(surface),
            _ => {}
        }
    }
}

impl Dispatch<XdgPopup, ObjectId> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgPopup,
        request: xdg_popup::Request,
        surface: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // A reposition is answered at once, in place, at the new
        // positioner's size.
        if let xdg_popup::Request::Reposition { positioner, token } = request {
            state
                .surf
                .reposition_popup(surface, token, positioner_size(&positioner));
        }
    }
}

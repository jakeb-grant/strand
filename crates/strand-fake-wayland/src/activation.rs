//! (M5) `xdg_activation_v1` ([`crate::SurfaceGlobals::activation`]):
//! every token a client asks for is recorded with what it was asked with
//! ([`ActivationRequest`], [`crate::Fake::activation_requests`]) and
//! answered at its `commit` with `fake-token-<n>`.

use std::sync::Mutex;

use wayland_protocols::xdg::activation::v1::server::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::{self, XdgActivationV1},
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New};

use crate::Server;

/// A token as a client asked for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationRequest {
    /// `set_serial`'s serial.
    pub serial: Option<u32>,
    /// `set_serial` named a seat.
    pub seat: bool,
    /// `set_surface`'s surface: its layer namespace, `""` for a surface
    /// that is not a layer surface.
    pub surface: Option<String>,
    pub app_id: Option<String>,
    /// What the fake answered.
    pub token: String,
}

pub(crate) fn create_global(dh: &DisplayHandle) {
    dh.create_global::<Server, XdgActivationV1, ()>(1, ());
}

impl GlobalDispatch<XdgActivationV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgActivationV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<XdgActivationV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &XdgActivationV1,
        request: xdg_activation_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_activation_v1::Request::GetActivationToken { id } = request {
            init.init(id, Mutex::new(ActivationRequest::default()));
        }
    }
}

impl Dispatch<XdgActivationTokenV1, Mutex<ActivationRequest>> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        token: &XdgActivationTokenV1,
        request: xdg_activation_token_v1::Request,
        asked: &Mutex<ActivationRequest>,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let Ok(mut asked) = asked.lock() else {
            return;
        };
        match request {
            xdg_activation_token_v1::Request::SetSerial { serial, .. } => {
                asked.serial = Some(serial);
                asked.seat = true;
            }
            xdg_activation_token_v1::Request::SetAppId { app_id } => asked.app_id = Some(app_id),
            xdg_activation_token_v1::Request::SetSurface { surface } => {
                asked.surface = Some(state.surf.namespace_of(&surface).unwrap_or_default());
            }
            xdg_activation_token_v1::Request::Commit => {
                state.tokens += 1;
                asked.token = format!("fake-token-{}", state.tokens);
                if let Ok(mut log) = state.activation_log.lock() {
                    log.push(asked.clone());
                }
                if !state.hold_tokens {
                    token.done(asked.token.clone());
                }
            }
            _ => {}
        }
    }
}

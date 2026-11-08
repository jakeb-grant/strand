//! (outputs-resync) Output management: `zwlr_output_manager_v1` (v4,
//! [`crate::FakeBuilder::output_management`]), one head per output name
//! (`Fake <name>`, one 1920x1080 mode at 60 Hz, preferred; laid out left
//! to right), and every `wl_output` sending its current mode. Two shapes:
//! wlroots', where a change is the head's new values and a manager
//! `done`, and Hyprland 0.57's, where a change is the head's values from
//! before it and no `done` (a manager bound after it gets the outputs as
//! they are). [`crate::Cmd::MoveOutput`] moves an output;
//! [`crate::Cmd::CustomMode`] sets one to a mode not in its list, which
//! Hyprland names no current mode for. A configuration is answered
//! `succeeded` and changes nothing.

use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_server::protocol::wl_output;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::Server;

/// Which output management a fake offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputManagement {
    /// None.
    #[default]
    None,
    /// wlroots' (sway's): a change and a `done`.
    Wlroots,
    /// Hyprland 0.57's: a change sent with the values from before it, and
    /// no `done`.
    Lagging,
}

/// The width of the one mode, and of each output in the layout.
const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;
const REFRESH: i32 = 60_000;

/// One output as the compositor has it.
#[derive(Clone, Debug)]
pub(crate) struct Head {
    name: String,
    position: (i32, i32),
    /// A mode not in its list (width, height).
    custom: Option<(i32, i32)>,
}

/// The output-management side of the fake's state.
#[derive(Default)]
pub(crate) struct State {
    pub(crate) kind: OutputManagement,
    pub(crate) heads: Vec<Head>,
    /// Each bound manager, with what it was sent, by head.
    managers: Vec<(ZwlrOutputManagerV1, Vec<ZwlrOutputHeadV1>)>,
    serial: u32,
}

impl State {
    pub(crate) fn new(kind: OutputManagement, names: &[String]) -> State {
        State {
            kind,
            heads: names
                .iter()
                .enumerate()
                .map(|(i, n)| Head {
                    name: n.clone(),
                    position: (WIDTH * i as i32, 0),
                    custom: None,
                })
                .collect(),
            ..State::default()
        }
    }

    /// The current mode `wl_output` `index` sends: width, height, mHz.
    pub(crate) fn current(&self, index: usize) -> Option<(i32, i32, i32)> {
        let h = self.heads.get(index)?;
        let (w, hh) = h.custom.unwrap_or((WIDTH, HEIGHT));
        Some((w, hh, REFRESH))
    }

    /// Output `index` moves to `(x, y)`.
    pub(crate) fn move_output(&mut self, index: usize, x: i32, y: i32) {
        let Some(h) = self.heads.get_mut(index) else {
            return;
        };
        let before = std::mem::replace(&mut h.position, (x, y));
        let sent = match self.kind {
            OutputManagement::Lagging => before,
            _ => (x, y),
        };
        self.serial += 1;
        for (m, heads) in &self.managers {
            if let Some(head) = heads.get(index) {
                head.position(sent.0, sent.1);
                if self.kind != OutputManagement::Lagging {
                    m.done(self.serial);
                }
            }
        }
    }

    /// Output `index` shows `(w, h)`, a mode not in its list (before any
    /// client binds).
    pub(crate) fn custom_mode(&mut self, index: usize, w: i32, h: i32) {
        if let Some(head) = self.heads.get_mut(index) {
            head.custom = Some((w, h));
        }
    }
}

pub(crate) fn create_globals(dh: &DisplayHandle, kind: OutputManagement) {
    if kind != OutputManagement::None {
        dh.create_global::<Server, ZwlrOutputManagerV1, ()>(4, ());
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for Server {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let m = init.init(resource, ());
        let mut sent = Vec::new();
        for h in &state.om.heads {
            let Ok(head) =
                client.create_resource::<ZwlrOutputHeadV1, (), Server>(dh, m.version(), ())
            else {
                continue;
            };
            m.head(&head);
            head.name(h.name.clone());
            head.description(format!("Fake {}", h.name));
            head.make("Fake".into());
            head.model(h.name.clone());
            let Ok(mode) =
                client.create_resource::<ZwlrOutputModeV1, (), Server>(dh, m.version(), ())
            else {
                continue;
            };
            head.mode(&mode);
            mode.size(WIDTH, HEIGHT);
            mode.refresh(REFRESH);
            mode.preferred();
            head.enabled(1);
            if h.custom.is_none() {
                head.current_mode(&mode);
            }
            head.position(h.position.0, h.position.1);
            head.transform(wl_output::Transform::Normal);
            head.scale(1.0);
            sent.push(head);
        }
        state.om.serial += 1;
        m.done(state.om.serial);
        state.om.managers.push((m, sent));
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, .. } => {
                init.init(id, ());
            }
            // Hyprland 0.57 never answers `stop`, and goes on sending the
            // stopped manager its changes.
            zwlr_output_manager_v1::Request::Stop if state.om.kind != OutputManagement::Lagging => {
                manager.finished();
                state.om.managers.retain(|(m, _)| m != manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputHeadV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrOutputHeadV1,
        _: zwlr_output_head_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputModeV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrOutputModeV1,
        _: zwlr_output_mode_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        config: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, .. } => {
                init.init(id, ());
            }
            zwlr_output_configuration_v1::Request::Apply
            | zwlr_output_configuration_v1::Request::Test => config.succeeded(),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrOutputConfigurationHeadV1,
        _: zwlr_output_configuration_head_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

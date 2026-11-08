//! (outputs-resync) Output management: `zwlr_output_manager_v1` (v4,
//! [`crate::FakeBuilder::output_management`]), one head per output name
//! (`Fake <name>`, one 1920x1080 mode at 60 Hz, preferred; laid out left
//! to right), every `wl_output` sending its current mode, and
//! `zxdg_output_manager_v1` (v3) giving each output its logical place.
//! Three shapes: wlroots', where a change is the head's new values and a
//! manager `done`; and two of Hyprland 0.57's, whose `stop` is never
//! answered and whose stopped managers are sent a `done` at every change
//! (its `ensureMonitorSent` on heads released), while the manager bound
//! gets either the head's values from before the change and no `done`
//! (`Lagging`) or nothing at all (`Silent`): a manager bound after the
//! change gets the outputs as they are. In every shape a change moves
//! the output's xdg-output place, ended by its `wl_output.done`.
//! [`crate::Cmd::MoveOutput`] moves an output; [`crate::Cmd::CustomMode`]
//! sets one to a mode not in its list, which Hyprland names no current
//! mode for; [`crate::Cmd::AddOutput`] plugs one in showing such a mode,
//! its head and `done` sent before a client can bind its `wl_output`.
//!
//! (outputs-apply) A configuration's places are taken as the compositor
//! takes them. wlroots moves each output at once, a change and a `done`
//! as above, then answers `succeeded`. Hyprland 0.57 (its
//! `OutputManagement.cpp` and `MonitorRuleManager::get`) saves the
//! places in the manager the configuration was made on, sends that
//! manager a `done`, then sets each output where the first manager still
//! connected, in the order they were bound, that saved a place for it
//! says, as a change of the shape above, and answers `succeeded` whatever
//! it did: a manager bound after one that saved a place is not followed.
//! [`crate::Cmd::ShadowOutput`] is another client's earlier manager
//! saving one. [`crate::Fake::output_managers`] counts the managers bound
//! and still connected, and the stopped ones among them.
//! [`crate::FakeBuilder::mirrors`] gives an output a head on and no
//! `wl_output`, as Hyprland does a mirrored monitor: its head names no
//! current mode, and Hyprland's shapes save a place a configuration
//! gives it and leave it where it is (the monitor stays a mirror).

use std::sync::{Arc, Mutex};

use wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_v1::ZxdgOutputV1;
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use wayland_server::backend::{ClientId, ObjectId};
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
    /// Hyprland 0.57's for a change that emits no mode change (a move
    /// made with `hyprctl eval 'hl.monitor(…)'`): nothing to the manager
    /// bound, a `done` to each one stopped.
    Silent,
}

impl OutputManagement {
    /// Hyprland's: `stop` unanswered, stopped managers sent `done`s.
    fn hyprland(self) -> bool {
        matches!(self, OutputManagement::Lagging | OutputManagement::Silent)
    }
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
    /// (review round 6) It mirrors another ([`crate::FakeBuilder::mirrors`]).
    mirror: bool,
}

impl Head {
    fn size(&self) -> (i32, i32) {
        self.custom.unwrap_or((WIDTH, HEIGHT))
    }
}

/// A bound manager, with what it was sent, by head.
struct Manager {
    resource: ZwlrOutputManagerV1,
    heads: Vec<Option<ZwlrOutputHeadV1>>,
    /// Asked to stop (Hyprland's shapes, which never answer it).
    stopped: bool,
    /// (outputs-apply) Hyprland's shapes: the place each output was given
    /// by a configuration made on this manager, by output index.
    saved: Vec<(usize, (i32, i32))>,
}

/// (outputs-apply) An enabled head of a configuration: its output's
/// index, and the place it was given.
type Placed = (usize, Option<(i32, i32)>);

/// (outputs-apply) A configuration: the manager it was made on, and its
/// enabled heads.
pub(crate) struct Config {
    manager: ObjectId,
    heads: Mutex<Vec<Placed>>,
}

/// The output-management side of the fake's state.
#[derive(Default)]
pub(crate) struct State {
    pub(crate) kind: OutputManagement,
    pub(crate) heads: Vec<Head>,
    managers: Vec<Manager>,
    /// The xdg-outputs given, with the index of their output.
    xdg: Vec<(usize, ZxdgOutputV1)>,
    serial: u32,
    /// (outputs-apply) Places another client's earlier manager saved
    /// ([`crate::Cmd::ShadowOutput`]), which Hyprland's shapes follow
    /// before any of this client's.
    shadow: Vec<(usize, (i32, i32))>,
    /// Managers bound so far.
    bound: usize,
}

/// (outputs-apply) The output managers bound ([`crate::Fake::output_managers`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManagerCount {
    /// Bound so far, by every client.
    pub bound: usize,
    /// Bound and still connected.
    pub live: usize,
    /// Of those, asked to stop and (Hyprland's shapes) never finished.
    pub stopped: usize,
}

impl State {
    pub(crate) fn new(kind: OutputManagement, names: &[String], mirrors: &[&str]) -> State {
        State {
            kind,
            heads: names
                .iter()
                .enumerate()
                .map(|(i, n)| Head {
                    name: n.clone(),
                    position: (WIDTH * i as i32, 0),
                    custom: None,
                    mirror: mirrors.contains(&n.as_str()),
                })
                .collect(),
            ..State::default()
        }
    }

    /// The current mode `wl_output` `index` sends: width, height, mHz.
    pub(crate) fn current(&self, index: usize) -> Option<(i32, i32, i32)> {
        let (w, h) = self.heads.get(index)?.size();
        Some((w, h, REFRESH))
    }

    /// An xdg-output for output `index`: its logical place and size.
    pub(crate) fn xdg_output(&mut self, index: usize, x: ZxdgOutputV1) {
        if let Some(h) = self.heads.get(index) {
            x.logical_position(h.position.0, h.position.1);
            let (w, hh) = h.size();
            x.logical_size(w, hh);
        }
        self.xdg.push((index, x));
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    /// Output `index` moves to `(x, y)`; `outputs` are the `wl_output`s
    /// bound, with the index of their output.
    pub(crate) fn move_output(
        &mut self,
        index: usize,
        x: i32,
        y: i32,
        outputs: &[(usize, wl_output::WlOutput)],
    ) {
        let Some(h) = self.heads.get_mut(index) else {
            return;
        };
        let before = std::mem::replace(&mut h.position, (x, y));
        let serial = self.next_serial();
        for m in self.managers.iter().filter(|m| m.resource.is_alive()) {
            match (self.kind, m.stopped) {
                (OutputManagement::Wlroots, _) => {
                    if let Some(Some(head)) = m.heads.get(index) {
                        head.position(x, y);
                    }
                    m.resource.done(serial);
                }
                (OutputManagement::Lagging, false) => {
                    if let Some(Some(head)) = m.heads.get(index) {
                        head.position(before.0, before.1);
                    }
                }
                (_, true) => m.resource.done(serial),
                _ => {}
            }
        }
        for (_, xdg) in self.xdg.iter().filter(|(i, x)| *i == index && x.is_alive()) {
            xdg.logical_position(x, y);
        }
        for (_, o) in outputs.iter().filter(|(i, _)| *i == index) {
            if o.version() >= 2 {
                o.done();
            }
        }
    }

    /// (outputs-apply) Another client's earlier manager saves `(x, y)`
    /// for output `index`, and the output moves there (Hyprland's shapes;
    /// wlroots' only moves it).
    pub(crate) fn shadow_output(
        &mut self,
        index: usize,
        x: i32,
        y: i32,
        outputs: &[(usize, wl_output::WlOutput)],
    ) {
        if self.kind.hyprland() {
            self.shadow.retain(|(i, _)| *i != index);
            self.shadow.push((index, (x, y)));
        }
        self.move_output(index, x, y, outputs);
    }

    /// (outputs-apply) Configuration `config` applied: see the module
    /// docs. Answers `succeeded` after the outputs moved.
    fn apply(
        &mut self,
        config: &ZwlrOutputConfigurationV1,
        data: &Config,
        outputs: &[(usize, wl_output::WlOutput)],
    ) {
        let asked: Vec<(usize, (i32, i32))> = data
            .heads
            .lock()
            .map(|h| h.iter().filter_map(|&(i, p)| Some((i, p?))).collect())
            .unwrap_or_default();
        if !self.kind.hyprland() {
            for (i, (x, y)) in asked {
                if self.heads.get(i).is_some_and(|h| h.position != (x, y)) {
                    self.move_output(i, x, y, outputs);
                }
            }
            config.succeeded();
            return;
        }
        if let Some(m) = self
            .managers
            .iter_mut()
            .find(|m| m.resource.id() == data.manager)
        {
            for (i, p) in asked {
                m.saved.retain(|(j, _)| *j != i);
                m.saved.push((i, p));
            }
        }
        let serial = self.next_serial();
        if let Some(m) = self
            .managers
            .iter()
            .find(|m| m.resource.id() == data.manager)
        {
            m.resource.done(serial);
        }
        // The rule reload: the first saved place wins, but for a mirror,
        // which stays where it is.
        for i in 0..self.heads.len() {
            if self.heads[i].mirror {
                continue;
            }
            let first = self.shadow.iter().find(|(j, _)| *j == i).or_else(|| {
                self.managers
                    .iter()
                    .filter(|m| m.resource.is_alive())
                    .find_map(|m| m.saved.iter().find(|(j, _)| *j == i))
            });
            if let Some(&(_, (x, y))) = first
                && self.heads[i].position != (x, y)
            {
                self.move_output(i, x, y, outputs);
            }
        }
        config.succeeded();
    }

    /// The managers counted ([`ManagerCount`]).
    pub(crate) fn managers(&self) -> ManagerCount {
        let live = self.managers.iter().filter(|m| m.resource.is_alive());
        ManagerCount {
            bound: self.bound,
            live: live.clone().count(),
            stopped: live.filter(|m| m.stopped).count(),
        }
    }

    /// Output `index` shows `(w, h)`, a mode not in its list (before any
    /// client binds).
    pub(crate) fn custom_mode(&mut self, index: usize, w: i32, h: i32) {
        if let Some(head) = self.heads.get_mut(index) {
            head.custom = Some((w, h));
        }
    }

    /// Output `name` is plugged in as output `index` (its `wl_output`
    /// global just made), right of the others, showing `(w, h)`, a mode
    /// not in its list: each manager bound gets its head and a `done`,
    /// each stopped one a `done` (Hyprland's `ensureMonitorSent`).
    pub(crate) fn add_output(&mut self, dh: &DisplayHandle, name: &str, w: i32, h: i32) {
        let x = self
            .heads
            .iter()
            .map(|h| h.position.0 + h.size().0)
            .max()
            .unwrap_or(0);
        let head = Head {
            name: name.into(),
            position: (x, 0),
            custom: Some((w, h)),
            mirror: false,
        };
        let serial = self.next_serial();
        for m in self.managers.iter_mut().filter(|m| m.resource.is_alive()) {
            if !m.stopped {
                m.heads.push(send_head(dh, &m.resource, &head));
            }
            m.resource.done(serial);
        }
        self.heads.push(head);
    }
}

/// Sends manager `m` head `h`, all its data and its one mode.
fn send_head(dh: &DisplayHandle, m: &ZwlrOutputManagerV1, h: &Head) -> Option<ZwlrOutputHeadV1> {
    let client = m.client()?;
    let head = client
        .create_resource::<ZwlrOutputHeadV1, (), Server>(dh, m.version(), ())
        .ok()?;
    m.head(&head);
    head.name(h.name.clone());
    head.description(format!("Fake {}", h.name));
    head.make("Fake".into());
    head.model(h.name.clone());
    let mode = client
        .create_resource::<ZwlrOutputModeV1, (), Server>(dh, m.version(), ())
        .ok()?;
    head.mode(&mode);
    mode.size(WIDTH, HEIGHT);
    mode.refresh(REFRESH);
    mode.preferred();
    head.enabled(1);
    if h.custom.is_none() && !h.mirror {
        head.current_mode(&mode);
    }
    head.position(h.position.0, h.position.1);
    head.transform(wl_output::Transform::Normal);
    head.scale(1.0);
    Some(head)
}

/// The manager, and xdg-output unless the surface globals offer it.
pub(crate) fn create_globals(dh: &DisplayHandle, kind: OutputManagement, xdg_output: bool) {
    if kind != OutputManagement::None {
        dh.create_global::<Server, ZwlrOutputManagerV1, ()>(4, ());
        if !xdg_output {
            dh.create_global::<
                Server,
                wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1,
                (),
            >(3, ());
        }
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, ()> for Server {
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let m = init.init(resource, ());
        let heads = state.om.heads.clone();
        let sent = heads.iter().map(|h| send_head(dh, &m, h)).collect();
        let serial = state.om.next_serial();
        m.done(serial);
        state.om.bound += 1;
        state.om.managers.push(Manager {
            resource: m,
            heads: sent,
            stopped: false,
            saved: Vec::new(),
        });
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
                init.init(
                    id,
                    Arc::new(Config {
                        manager: manager.id(),
                        heads: Mutex::new(Vec::new()),
                    }),
                );
            }
            // Hyprland 0.57 never answers `stop`, and goes on sending the
            // stopped manager a `done` at each change.
            zwlr_output_manager_v1::Request::Stop if state.om.kind.hyprland() => {
                for m in state
                    .om
                    .managers
                    .iter_mut()
                    .filter(|m| m.resource == *manager)
                {
                    m.stopped = true;
                }
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state.om.managers.retain(|m| m.resource != *manager);
            }
            _ => {}
        }
    }

    /// A client gone: Hyprland erases its managers, and what they saved.
    fn destroyed(state: &mut Self, _: ClientId, manager: &ZwlrOutputManagerV1, _: &()) {
        state.om.managers.retain(|m| m.resource != *manager);
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

impl Dispatch<ZwlrOutputConfigurationV1, Arc<Config>> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        config: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &Arc<Config>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                // Any manager's head names its output (Hyprland looks
                // heads up among every manager's).
                let index = state.om.managers.iter().find_map(|m| {
                    m.heads
                        .iter()
                        .position(|h| h.as_ref().is_some_and(|h| *h == head))
                });
                let slot = data.heads.lock().map_or(0, |mut h| {
                    h.push((index.unwrap_or(usize::MAX), None));
                    h.len() - 1
                });
                init.init(id, (data.clone(), slot));
            }
            zwlr_output_configuration_v1::Request::Apply => {
                let outputs = state.outputs.clone();
                state.om.apply(config, data, &outputs);
            }
            zwlr_output_configuration_v1::Request::Test => config.succeeded(),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, (Arc<Config>, usize)> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        (config, slot): &(Arc<Config>, usize),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_output_configuration_head_v1::Request::SetPosition { x, y } = request
            && let Ok(mut heads) = config.heads.lock()
            && let Some(h) = heads.get_mut(*slot)
        {
            h.1 = Some((x, y));
        }
    }
}

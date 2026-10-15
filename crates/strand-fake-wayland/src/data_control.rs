//! (m6-service-gaps) Data control: `ext_data_control_manager_v1` and
//! `zwlr_data_control_manager_v1` ([`crate::FakeBuilder::data_control`]),
//! the shape of the clipboard managers' protocol. One selection, shared
//! by both: a client's source (either protocol) or one the fake serves
//! itself ([`crate::Cmd::SetSelection`], as another program copying).
//! Every device is told of each new selection with an offer of its
//! types; an offer's `receive` is passed to the selection's source (its
//! `send`, the fd straight through) or, for the fake's own, written on a
//! thread. A replaced client source is `cancelled`. Primary selections
//! are ignored. [`crate::Fake::selection`] says what the selection
//! offers and which protocol set it.

use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use wayland_protocols::ext::data_control::v1::server::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::Server;

/// Which data-control managers a fake offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataControl {
    /// Neither: a compositor without a clipboard manager protocol.
    #[default]
    None,
    /// `ext-data-control-v1` only.
    Ext,
    /// `wlr-data-control` (version 2) only.
    Wlr,
    /// Both.
    Both,
}

/// What the selection offers and which protocol set it: `ext`, `wlr`
/// or `fake` ([`crate::Cmd::SetSelection`]).
pub type Selection = (Vec<String>, String);

/// A source's offered types.
type Mimes = Mutex<Vec<String>>;

enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone)]
enum Owner {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
    Fake(Arc<Vec<(String, Vec<u8>)>>),
}

impl Owner {
    fn protocol(&self) -> &'static str {
        match self {
            Owner::Ext(_) => "ext",
            Owner::Wlr(_) => "wlr",
            Owner::Fake(_) => "fake",
        }
    }

    fn mimes(&self) -> Vec<String> {
        let of = |m: Option<&Mimes>| m.and_then(|m| m.lock().ok().map(|m| m.clone()));
        match self {
            Owner::Ext(s) => of(s.data::<Mimes>()).unwrap_or_default(),
            Owner::Wlr(s) => of(s.data::<Mimes>()).unwrap_or_default(),
            Owner::Fake(o) => o.iter().map(|(m, _)| m.clone()).collect(),
        }
    }

    fn same(&self, other: &Owner) -> bool {
        match (self, other) {
            (Owner::Ext(a), Owner::Ext(b)) => a == b,
            (Owner::Wlr(a), Owner::Wlr(b)) => a == b,
            (Owner::Fake(a), Owner::Fake(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn cancelled(&self) {
        match self {
            Owner::Ext(s) => s.cancelled(),
            Owner::Wlr(s) => s.cancelled(),
            Owner::Fake(_) => {}
        }
    }

    fn send(&self, mime: String, fd: OwnedFd) {
        match self {
            Owner::Ext(s) => s.send(mime, fd.as_fd()),
            Owner::Wlr(s) => s.send(mime, fd.as_fd()),
            Owner::Fake(offers) => {
                let bytes = offers
                    .iter()
                    .find(|(m, _)| *m == mime)
                    .map(|(_, b)| b.clone());
                if let Some(bytes) = bytes {
                    std::thread::spawn(move || {
                        let _ = std::fs::File::from(fd).write_all(&bytes);
                    });
                }
            }
        }
    }
}

/// The data-control side of the fake's state.
#[derive(Default)]
pub(crate) struct State {
    devices: Vec<Device>,
    selection: Option<Owner>,
    /// Bumped at each selection; an offer of an older one reads nothing.
    generation: u64,
    pub(crate) log: Arc<Mutex<Option<Selection>>>,
}

pub(crate) fn create_globals(dh: &DisplayHandle, which: DataControl) {
    if matches!(which, DataControl::Ext | DataControl::Both) {
        dh.create_global::<Server, ExtDataControlManagerV1, ()>(1, ());
    }
    if matches!(which, DataControl::Wlr | DataControl::Both) {
        dh.create_global::<Server, ZwlrDataControlManagerV1, ()>(2, ());
    }
}

impl State {
    /// Tells `device` of the selection.
    fn announce(&self, dh: &DisplayHandle, device: &Device) {
        let Some(from) = &self.selection else {
            match device {
                Device::Ext(d) => d.selection(None),
                Device::Wlr(d) => d.selection(None),
            }
            return;
        };
        let mimes = from.mimes();
        match device {
            Device::Ext(d) => {
                let Some(client) = d.client() else { return };
                let Ok(offer) = client.create_resource::<ExtDataControlOfferV1, u64, Server>(
                    dh,
                    d.version(),
                    self.generation,
                ) else {
                    return;
                };
                d.data_offer(&offer);
                for m in mimes {
                    offer.offer(m);
                }
                d.selection(Some(&offer));
            }
            Device::Wlr(d) => {
                let Some(client) = d.client() else { return };
                let Ok(offer) = client.create_resource::<ZwlrDataControlOfferV1, u64, Server>(
                    dh,
                    d.version(),
                    self.generation,
                ) else {
                    return;
                };
                d.data_offer(&offer);
                for m in mimes {
                    offer.offer(m);
                }
                d.selection(Some(&offer));
            }
        }
    }

    /// The selection is `from` now (`None`: empty).
    fn set(&mut self, dh: &DisplayHandle, from: Option<Owner>) {
        if let Some(old) = self.selection.take()
            && !from.as_ref().is_some_and(|f| f.same(&old))
        {
            old.cancelled();
        }
        self.generation += 1;
        if let Ok(mut log) = self.log.lock() {
            *log = from.as_ref().map(|f| (f.mimes(), f.protocol().to_string()));
        }
        self.selection = from;
        self.devices.retain(|d| match d {
            Device::Ext(d) => d.is_alive(),
            Device::Wlr(d) => d.is_alive(),
        });
        for d in &self.devices {
            self.announce(dh, d);
        }
    }

    /// The fake copies `offers`, as another program would.
    pub(crate) fn set_own(&mut self, dh: &DisplayHandle, offers: Vec<(String, Vec<u8>)>) {
        self.set(dh, Some(Owner::Fake(Arc::new(offers))));
    }

    fn receive(&self, generation: u64, mime: String, fd: OwnedFd) {
        if generation == self.generation
            && let Some(from) = &self.selection
        {
            from.send(mime, fd);
        }
    }

    /// A source went: if it was the selection, the selection is empty.
    fn source_gone(&mut self, dh: &DisplayHandle, gone: &Owner) {
        if self.selection.as_ref().is_some_and(|s| s.same(gone)) {
            self.selection = None;
            self.set(dh, None);
        }
    }
}

impl GlobalDispatch<ExtDataControlManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtDataControlManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtDataControlManagerV1,
        request: ext_data_control_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_data_control_manager_v1::Request::CreateDataSource { id } => {
                init.init(id, Mimes::default());
            }
            ext_data_control_manager_v1::Request::GetDataDevice { id, .. } => {
                let d = Device::Ext(init.init(id, ()));
                state.dc.announce(dh, &d);
                state.dc.devices.push(d);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        device: &ExtDataControlDeviceV1,
        request: ext_data_control_device_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_data_control_device_v1::Request::SetSelection { source } => {
                state.dc.set(dh, source.map(Owner::Ext));
            }
            ext_data_control_device_v1::Request::Destroy => {
                state
                    .dc
                    .devices
                    .retain(|d| !matches!(d, Device::Ext(o) if o == device));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Mimes> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        source: &ExtDataControlSourceV1,
        request: ext_data_control_source_v1::Request,
        mimes: &Mimes,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_data_control_source_v1::Request::Offer { mime_type } => {
                if let Ok(mut m) = mimes.lock() {
                    m.push(mime_type);
                }
            }
            ext_data_control_source_v1::Request::Destroy => {
                state.dc.source_gone(dh, &Owner::Ext(source.clone()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, u64> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtDataControlOfferV1,
        request: ext_data_control_offer_v1::Request,
        generation: &u64,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let ext_data_control_offer_v1::Request::Receive { mime_type, fd } = request {
            state.dc.receive(*generation, mime_type, fd);
        }
    }
}

impl GlobalDispatch<ZwlrDataControlManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrDataControlManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrDataControlManagerV1,
        request: zwlr_data_control_manager_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_data_control_manager_v1::Request::CreateDataSource { id } => {
                init.init(id, Mimes::default());
            }
            zwlr_data_control_manager_v1::Request::GetDataDevice { id, .. } => {
                let d = Device::Wlr(init.init(id, ()));
                state.dc.announce(dh, &d);
                state.dc.devices.push(d);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        device: &ZwlrDataControlDeviceV1,
        request: zwlr_data_control_device_v1::Request,
        _: &(),
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_data_control_device_v1::Request::SetSelection { source } => {
                state.dc.set(dh, source.map(Owner::Wlr));
            }
            zwlr_data_control_device_v1::Request::Destroy => {
                state
                    .dc
                    .devices
                    .retain(|d| !matches!(d, Device::Wlr(o) if o == device));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, Mimes> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        source: &ZwlrDataControlSourceV1,
        request: zwlr_data_control_source_v1::Request,
        mimes: &Mimes,
        dh: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_data_control_source_v1::Request::Offer { mime_type } => {
                if let Ok(mut m) = mimes.lock() {
                    m.push(mime_type);
                }
            }
            zwlr_data_control_source_v1::Request::Destroy => {
                state.dc.source_gone(dh, &Owner::Wlr(source.clone()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, u64> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwlrDataControlOfferV1,
        request: zwlr_data_control_offer_v1::Request,
        generation: &u64,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_data_control_offer_v1::Request::Receive { mime_type, fd } = request {
            state.dc.receive(*generation, mime_type, fd);
        }
    }
}

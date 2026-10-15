//! One data-control client over either protocol:
//! `ext_data_control_manager_v1` where the compositor offers it, else
//! `zwlr_data_control_manager_v1` at version 1 (no primary selection).
//! The run (`mod.rs`) watches the selection with it; the keeper
//! (`keeper.rs`) sets it. Both sleep in `poll(2)` on the connection
//! ([`wait`]).

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::protocol::{wl_callback, wl_display, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// Why there is no clipboard: what `clipboard.failed` says.
pub const NO_DATA_CONTROL: &str =
    "the compositor offers no data control (ext-data-control-v1 or wlr-data-control)";

/// The data-control manager bound.
#[derive(Debug, Clone)]
pub enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

/// A seat's data-control device.
#[derive(Debug, Clone)]
pub enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

/// An offer of the selection (another program's, or our own copy's).
#[derive(Debug, Clone, PartialEq)]
pub enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

/// A source we offer the selection from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Manager {
    /// The protocol, as the log names it.
    pub fn protocol(&self) -> &'static str {
        match self {
            Manager::Ext(_) => "ext-data-control-v1",
            Manager::Wlr(_) => "wlr-data-control",
        }
    }
}

impl Offer {
    /// Asks the offer's source to write it as `mime` to `fd`.
    pub fn receive(&self, mime: &str, fd: BorrowedFd<'_>) {
        match self {
            Offer::Ext(o) => o.receive(mime.to_string(), fd),
            Offer::Wlr(o) => o.receive(mime.to_string(), fd),
        }
    }

    pub fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

impl Source {
    pub fn id(&self) -> ObjectId {
        match self {
            Source::Ext(s) => s.id(),
            Source::Wlr(s) => s.id(),
        }
    }

    pub fn destroy(&self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
}

/// What the connection said, in order.
#[derive(Debug)]
pub enum Ev {
    /// The selection changed: its offer and the types it offers (`None`:
    /// empty). `at_bind`: the selection as it stood when the device was
    /// made (told before the sync after it came back), not a copy made
    /// since.
    Selection {
        offer: Option<(Offer, Vec<String>)>,
        at_bind: bool,
    },
    /// A program asks for our source's data as `mime`, through `fd`.
    Send {
        source: ObjectId,
        mime: String,
        fd: OwnedFd,
    },
    /// Our source is no longer the selection.
    Cancelled(ObjectId),
}

/// The connection's state (the `Dispatch` target).
#[derive(Debug, Default)]
pub struct Wl {
    display: Option<wl_display::WlDisplay>,
    registry: Option<wl_registry::WlRegistry>,
    /// Globals announced: ext manager, wlr manager, the first seat (name,
    /// version).
    ext: Option<(u32, u32)>,
    wlr: Option<(u32, u32)>,
    seat_global: Option<(u32, u32)>,
    /// The registry's first sync is answered.
    pub synced: bool,
    /// Bound once synced ([`Wl::bind`]).
    bound: bool,
    pub manager: Option<Manager>,
    seat: Option<wl_seat::WlSeat>,
    pub device: Option<Device>,
    /// Syncs sent after making a device and not yet answered: until they
    /// are, a selection told is the one the device found ([`Settle`]).
    settles: u32,
    /// Offers announced, with the types each offers.
    offers: HashMap<ObjectId, (Offer, Vec<String>)>,
    pub events: Vec<Ev>,
    /// The manager or the seat went away, or the device finished and
    /// cannot be made again: no clipboard any more.
    pub lost: bool,
}

/// Connects to the display at `socket` and asks for the globals.
pub fn connect(socket: &Path) -> Result<(Connection, EventQueue<Wl>, Wl), String> {
    let stream = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let queue: EventQueue<Wl> = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    let registry = display.get_registry(&qh, ());
    display.sync(&qh, ());
    let wl = Wl {
        display: Some(display.clone()),
        registry: Some(registry),
        ..Wl::default()
    };
    Ok((conn, queue, wl))
}

impl Wl {
    /// Once the registry's first sync is answered, binds the manager
    /// (`ext-data-control-v1` where offered) and the first seat, and gets
    /// the seat's device. Whether there is a clipboard now.
    pub fn bind(&mut self, qh: &QueueHandle<Wl>) -> bool {
        if self.synced
            && !self.bound
            && let Some(registry) = self.registry.clone()
        {
            self.bound = true;
            self.manager = match (self.ext, self.wlr) {
                (Some((name, _)), _) => Some(Manager::Ext(registry.bind(name, 1, qh, ()))),
                (None, Some((name, _))) => Some(Manager::Wlr(registry.bind(name, 1, qh, ()))),
                _ => None,
            };
            self.seat = self
                .seat_global
                .map(|(name, version)| registry.bind(name, version.min(7), qh, ()));
            self.make_device(qh);
            if let Some(m) = &self.manager {
                log::debug!("clipboard: data control over {}", m.protocol());
            }
        }
        self.available()
    }

    /// The clipboard can be watched and set.
    pub fn available(&self) -> bool {
        self.device.is_some() && !self.lost
    }

    /// The registry's first sync is answered, and so is the sync after
    /// the device was made: the selection it found has been told.
    pub fn settled(&self) -> bool {
        self.synced && self.settles == 0
    }

    /// Gets the seat's device, then a sync: the compositor tells a new
    /// device the selection at once, so what comes before that sync's
    /// answer is the selection found, not a copy.
    fn make_device(&mut self, qh: &QueueHandle<Wl>) {
        self.device = match (&self.manager, &self.seat) {
            (Some(Manager::Ext(m)), Some(s)) => Some(Device::Ext(m.get_data_device(s, qh, ()))),
            (Some(Manager::Wlr(m)), Some(s)) => Some(Device::Wlr(m.get_data_device(s, qh, ()))),
            _ => None,
        };
        if self.device.is_some()
            && let Some(display) = &self.display
        {
            display.sync(qh, Settle);
            self.settles += 1;
        }
    }

    /// A new source offering `mimes`, made the selection.
    pub fn set_selection(&mut self, qh: &QueueHandle<Wl>, mimes: &[String]) -> Option<Source> {
        let (manager, device) = (self.manager.as_ref()?, self.device.as_ref()?);
        let source = match manager {
            Manager::Ext(m) => {
                let s = m.create_data_source(qh, ());
                for t in mimes {
                    s.offer(t.clone());
                }
                Source::Ext(s)
            }
            Manager::Wlr(m) => {
                let s = m.create_data_source(qh, ());
                for t in mimes {
                    s.offer(t.clone());
                }
                Source::Wlr(s)
            }
        };
        match (device, &source) {
            (Device::Ext(d), Source::Ext(s)) => d.set_selection(Some(s)),
            (Device::Wlr(d), Source::Wlr(s)) => d.set_selection(Some(s)),
            _ => {}
        }
        Some(source)
    }

    /// The device finished (another client of the seat took it over, or
    /// the seat went): made again while the seat lives, else lost.
    fn device_finished(&mut self, qh: &QueueHandle<Wl>) {
        match self.device.take() {
            Some(Device::Ext(d)) => d.destroy(),
            Some(Device::Wlr(d)) => d.destroy(),
            None => {}
        }
        if self.seat.as_ref().is_some_and(|s| s.is_alive()) && !self.lost {
            self.make_device(qh);
        }
        if self.device.is_none() {
            self.lost = true;
        }
    }

    fn selection(&mut self, id: Option<ObjectId>) {
        let offer = id.and_then(|id| self.offers.remove(&id));
        // Offers announced but never made the selection go.
        for (_, (o, _)) in self.offers.drain() {
            o.destroy();
        }
        self.events.push(Ev::Selection {
            offer,
            at_bind: self.settles > 0,
        });
    }

    /// A primary selection's offer: never read.
    fn primary(&mut self, id: Option<ObjectId>) {
        if let Some((o, _)) = id.and_then(|id| self.offers.remove(&id)) {
            o.destroy();
        }
    }
}

/// Flushes, then sleeps in `poll(2)` until the connection, or one of
/// `extra` (each a descriptor and what to wait for), is ready or
/// `deadline` passes, and reads the connection. The revents of `extra`,
/// in order. `Err`: the connection ended.
pub fn wait(
    conn: &Connection,
    queue: &mut EventQueue<Wl>,
    wl: &mut Wl,
    extra: &[(BorrowedFd<'_>, PollFlags)],
    deadline: Option<Instant>,
) -> Result<Vec<PollFlags>, String> {
    queue.dispatch_pending(wl).map_err(|e| e.to_string())?;
    // A full socket buffer is not an error: wait until it drains.
    let unflushed = match conn.flush() {
        Ok(()) => false,
        Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => true,
        Err(e) => return Err(e.to_string()),
    };
    let Some(guard) = queue.prepare_read() else {
        return Ok(vec![PollFlags::empty(); extra.len()]);
    };
    let (readable, revents) = {
        let fd = guard.connection_fd();
        let flags = if unflushed {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::IN
        };
        let mut fds = vec![PollFd::new(&fd, flags)];
        fds.extend(extra.iter().map(|(f, w)| PollFd::new(f, *w)));
        let timeout = deadline.map(|d| timespec(d.saturating_duration_since(Instant::now())));
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) => (
                fds[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                fds[1..].iter().map(PollFd::revents).collect(),
            ),
            Err(rustix::io::Errno::INTR) => (false, vec![PollFlags::empty(); extra.len()]),
            Err(e) => return Err(io::Error::from(e).to_string()),
        }
    };
    if readable {
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("the Wayland connection ended: {e}")),
        }
    } else {
        drop(guard);
    }
    queue.dispatch_pending(wl).map_err(|e| e.to_string())?;
    Ok(revents)
}

fn timespec(d: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: d.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: d.subsec_nanos().into(),
    }
}

/// A descriptor as [`wait`] takes it.
pub fn polled(fd: &impl AsFd, flags: PollFlags) -> (BorrowedFd<'_>, PollFlags) {
    (fd.as_fd(), flags)
}

impl Dispatch<wl_registry::WlRegistry, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == ExtDataControlManagerV1::interface().name && state.ext.is_none() {
                    state.ext = Some((name, version));
                } else if interface == ZwlrDataControlManagerV1::interface().name
                    && state.wlr.is_none()
                {
                    state.wlr = Some((name, version));
                } else if interface == wl_seat::WlSeat::interface().name
                    && state.seat_global.is_none()
                {
                    state.seat_global = Some((name, version));
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                let bound_manager = match &state.manager {
                    Some(Manager::Ext(_)) => state.ext.map(|g| g.0),
                    Some(Manager::Wlr(_)) => state.wlr.map(|g| g.0),
                    None => None,
                };
                if bound_manager == Some(name)
                    || (state.seat.is_some() && state.seat_global.map(|g| g.0) == Some(name))
                {
                    state.lost = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = true;
        }
    }
}

/// The sync after a device was made ([`Wl::make_device`]).
#[derive(Debug)]
pub struct Settle;

impl Dispatch<wl_callback::WlCallback, Settle> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &Settle,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.settles = state.settles.saturating_sub(1);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event as E;
        match event {
            E::DataOffer { id } => {
                state.offers.insert(id.id(), (Offer::Ext(id), Vec::new()));
            }
            E::Selection { id } => state.selection(id.map(|o| o.id())),
            E::PrimarySelection { id } => state.primary(id.map(|o| o.id())),
            E::Finished => state.device_finished(qh),
            _ => {}
        }
    }

    event_created_child!(Wl, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event as E;
        match event {
            E::DataOffer { id } => {
                state.offers.insert(id.id(), (Offer::Wlr(id), Vec::new()));
            }
            E::Selection { id } => state.selection(id.map(|o| o.id())),
            E::PrimarySelection { id } => state.primary(id.map(|o| o.id())),
            E::Finished => state.device_finished(qh),
            _ => {}
        }
    }

    event_created_child!(Wl, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for Wl {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, m)) = state.offers.get_mut(&offer.id())
        {
            m.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for Wl {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, m)) = state.offers.get_mut(&offer.id())
        {
            m.push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for Wl {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.events.push(Ev::Send {
                    source: source.id(),
                    mime: mime_type,
                    fd,
                });
            }
            ext_data_control_source_v1::Event::Cancelled => {
                state.events.push(Ev::Cancelled(source.id()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for Wl {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.events.push(Ev::Send {
                    source: source.id(),
                    mime: mime_type,
                    fd,
                });
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                state.events.push(Ev::Cancelled(source.id()));
            }
            _ => {}
        }
    }
}

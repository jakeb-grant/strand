//! (M6) A test client of the compositor's clipboard over
//! `zwlr_data_control_manager_v1` (wlr-data-control, which sway 1.9,
//! Hyprland, niri and labwc offer): it sees the selection as another
//! program would (what it offers, and its bytes read through a pipe),
//! and sets it as another program (serving reads itself). Shared by the
//! clipboard tests of `strand-services`, `strand-surface` (copy and
//! paste in an `input`) and `strand` (`#[path]`).
//!
//! It runs on a thread of its own: a test whose own thread serves the
//! selection (a surface manager it dispatches) never blocks on it, so
//! reads come back on a channel the test waits on while it dispatches.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// The text types a copy from here offers.
pub const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// What the client saw of the selection.
#[derive(Debug, Default, Clone)]
pub struct Seen {
    /// What the current selection offers (empty: none).
    pub mimes: Vec<String>,
    /// How many selections it was told of (the one at bind included).
    pub selections: u64,
    /// Its own copies another program took over.
    pub cancelled: u64,
    /// Reads of its own copies (by MIME type).
    pub served: Vec<String>,
}

/// What a copy offers: each MIME type with its bytes.
type Offers = Vec<(String, Vec<u8>)>;

enum Cmd {
    Copy(Offers),
    Read(String, mpsc::Sender<Option<Vec<u8>>>),
    Stop,
}

/// The client (see the module docs); stops when dropped.
pub struct DataControl {
    tx: mpsc::Sender<Cmd>,
    seen: Arc<Mutex<Seen>>,
    thread: Option<JoinHandle<()>>,
}

struct State {
    seen: Arc<Mutex<Seen>>,
    /// Offers announced and the types each offers.
    offers: Vec<(ZwlrDataControlOfferV1, Vec<String>)>,
    current: Option<ZwlrDataControlOfferV1>,
    /// Our copy: its source and what it offers.
    source: Option<(ZwlrDataControlSourceV1, Offers)>,
}

impl DataControl {
    /// Connects through `conn`; `None` when the compositor offers no
    /// wlr-data-control.
    pub fn connect(conn: Connection) -> Option<DataControl> {
        let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ()).ok()?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).ok()?;
        let device = manager.get_data_device(&seat, &qh, ());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut state = State {
            seen: seen.clone(),
            offers: Vec::new(),
            current: None,
            source: None,
        };
        queue.roundtrip(&mut state).ok()?;
        let (tx, rx) = mpsc::channel::<Cmd>();
        let thread = std::thread::spawn(move || {
            loop {
                loop {
                    match rx.try_recv() {
                        Ok(Cmd::Copy(offers)) => {
                            let source = manager.create_data_source(&qh, ());
                            for (m, _) in &offers {
                                source.offer(m.clone());
                            }
                            device.set_selection(Some(&source));
                            if let Some((old, _)) = state.source.replace((source, offers)) {
                                old.destroy();
                            }
                        }
                        Ok(Cmd::Read(mime, reply)) => {
                            let Some(offer) = state.current.clone() else {
                                let _ = reply.send(None);
                                continue;
                            };
                            let Ok((mut r, w)) = std::io::pipe() else {
                                let _ = reply.send(None);
                                continue;
                            };
                            offer.receive(mime, OwnedFd::from(w).as_fd());
                            // Read on a thread of its own: the writer may
                            // be the test's own thread.
                            std::thread::spawn(move || {
                                let mut out = Vec::new();
                                let ok = r.read_to_end(&mut out).is_ok();
                                let _ = reply.send(ok.then_some(out));
                            });
                        }
                        Ok(Cmd::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => break,
                    }
                }
                if conn.flush().is_err() {
                    return;
                }
                if queue.dispatch_pending(&mut state).is_err() {
                    return;
                }
                let Some(guard) = queue.prepare_read() else {
                    continue;
                };
                let readable = {
                    let fd = guard.connection_fd();
                    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                    let t = rustix::event::Timespec {
                        tv_sec: 0,
                        tv_nsec: 10_000_000,
                    };
                    matches!(rustix::event::poll(&mut fds, Some(&t)), Ok(n) if n > 0)
                };
                if readable {
                    if guard.read().is_err() {
                        return;
                    }
                } else {
                    drop(guard);
                }
                if queue.dispatch_pending(&mut state).is_err() {
                    return;
                }
            }
        });
        Some(DataControl {
            tx,
            seen,
            thread: Some(thread),
        })
    }

    /// What it saw so far.
    pub fn seen(&self) -> Seen {
        self.seen.lock().unwrap().clone()
    }

    /// Waits (up to `limit`) until `done` holds of what it saw.
    pub fn wait(&self, limit: Duration, done: impl Fn(&Seen) -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if done(&self.seen()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done(&self.seen())
    }

    /// Sets the selection, as another program would, to `offers` (MIME
    /// type and bytes each).
    pub fn copy(&self, offers: &[(&str, &[u8])]) {
        let offers = offers
            .iter()
            .map(|(m, b)| ((*m).to_string(), b.to_vec()))
            .collect();
        let _ = self.tx.send(Cmd::Copy(offers));
    }

    /// Sets the selection to `text` as every text type.
    pub fn copy_text(&self, text: &str) {
        let offers: Vec<(&str, &[u8])> = TEXT.iter().map(|m| (*m, text.as_bytes())).collect();
        self.copy(&offers);
    }

    /// Starts reading the selection as `mime`: the bytes arrive on the
    /// channel (`None`: nothing to read, or the read failed).
    pub fn read(&self, mime: &str) -> mpsc::Receiver<Option<Vec<u8>>> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Cmd::Read(mime.to_string(), tx));
        rx
    }

    /// Reads the selection as `mime`, waiting up to `limit` (for a
    /// selection served by another thread).
    pub fn read_now(&self, mime: &str, limit: Duration) -> Option<Vec<u8>> {
        self.read(mime).recv_timeout(limit).ok().flatten()
    }
}

impl Drop for DataControl {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwlrDataControlManagerV1);

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event as E;
        match event {
            E::DataOffer { id } => state.offers.push((id, Vec::new())),
            E::Selection { id } => {
                if let Some(old) = state.current.take()
                    && Some(&old) != id.as_ref()
                {
                    state.offers.retain(|(o, _)| *o != old);
                    old.destroy();
                }
                let mimes = id
                    .as_ref()
                    .and_then(|o| state.offers.iter().find(|(x, _)| x == o))
                    .map(|(_, m)| m.clone())
                    .unwrap_or_default();
                state.current = id;
                let mut seen = state.seen.lock().unwrap();
                seen.mimes = mimes;
                seen.selections += 1;
            }
            E::PrimarySelection { id: Some(o) } => {
                state.offers.retain(|(x, _)| *x != o);
                o.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, m)) = state.offers.iter_mut().find(|(o, _)| o == offer)
        {
            m.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
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
                let bytes = state
                    .source
                    .as_ref()
                    .filter(|(s, _)| s == source)
                    .and_then(|(_, o)| o.iter().find(|(m, _)| *m == mime_type))
                    .map(|(_, b)| b.clone());
                state.seen.lock().unwrap().served.push(mime_type);
                if let Some(bytes) = bytes {
                    std::thread::spawn(move || {
                        let mut f = std::fs::File::from(fd);
                        let _ = f.write_all(&bytes);
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if state.source.as_ref().is_some_and(|(s, _)| s == source) {
                    state.source = None;
                }
                source.destroy();
                state.seen.lock().unwrap().cancelled += 1;
            }
            _ => {}
        }
    }
}

/// The id of `p`, for logs.
pub fn id_of(p: &impl Proxy) -> String {
    p.id().to_string()
}

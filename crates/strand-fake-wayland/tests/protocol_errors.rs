//! (audit-fake-strict) The fake answers what xdg-shell, layer-shell and
//! wl_surface make protocol errors as a compositor does, by posting the
//! error (the client is disconnected), so the tests that trust it see
//! the ordering mistakes a real compositor kills the shell for. Each
//! test is a raw client making one such mistake, then the same steps
//! made in order, which the fake accepts.

use std::os::fd::AsFd;

use strand_fake_wayland::{Fake, SurfaceGlobals};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{
    xdg_popup::XdgPopup,
    xdg_positioner::XdgPositioner,
    xdg_surface::{self, XdgSurface},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

/// The client: the last configure serial of each kind.
#[derive(Default)]
struct Client {
    layer_serial: Option<u32>,
    xdg_serial: Option<u32>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
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

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            state.layer_serial = Some(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for Client {
    fn event(
        state: &mut Self,
        _: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.xdg_serial = Some(serial);
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_seat::WlSeat);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);
delegate_noop!(Client: ignore XdgWmBase);
delegate_noop!(Client: ignore XdgPositioner);
delegate_noop!(Client: ignore XdgPopup);
delegate_noop!(Client: ignore ZwlrLayerShellV1);

/// A raw client of the fake.
struct Raw {
    _fake: Fake,
    conn: Connection,
    queue: EventQueue<Client>,
    qh: QueueHandle<Client>,
    client: Client,
    compositor: wl_compositor::WlCompositor,
    wm_base: XdgWmBase,
    layer_shell: ZwlrLayerShellV1,
    shm: wl_shm::WlShm,
    seat: wl_seat::WlSeat,
}

impl Raw {
    fn start() -> Raw {
        let fake = Fake::builder()
            .toplevel_list(false)
            .surfaces(SurfaceGlobals::default())
            .seats(1)
            .start();
        let conn = Connection::from_socket(fake.connect()).unwrap();
        let (globals, queue) = registry_queue_init::<Client>(&conn).unwrap();
        let qh = queue.handle();
        Raw {
            compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
            wm_base: globals.bind(&qh, 1..=6, ()).unwrap(),
            layer_shell: globals.bind(&qh, 1..=4, ()).unwrap(),
            shm: globals.bind(&qh, 1..=1, ()).unwrap(),
            seat: globals.bind(&qh, 1..=7, ()).unwrap(),
            _fake: fake,
            conn,
            queue,
            qh,
            client: Client::default(),
        }
    }

    /// A roundtrip: `Ok` while the fake keeps the client.
    fn roundtrip(&mut self) -> Result<(), wayland_client::DispatchError> {
        self.queue.roundtrip(&mut self.client).map(|_| ())
    }

    /// The protocol error the fake posted: its code and interface.
    fn error(&mut self) -> (u32, String) {
        assert!(self.roundtrip().is_err(), "the fake kept the client");
        let e = self.conn.protocol_error().expect("a protocol error");
        (e.code, e.object_interface)
    }

    /// A layer surface, configured and acked when `ack`.
    fn layer(&mut self, ack: bool) -> (wl_surface::WlSurface, ZwlrLayerSurfaceV1) {
        let wl = self.compositor.create_surface(&self.qh, ());
        let layer = self.layer_shell.get_layer_surface(
            &wl,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "test".into(),
            &self.qh,
            (),
        );
        layer.set_size(100, 100);
        wl.commit();
        self.roundtrip().unwrap();
        if ack {
            layer.ack_configure(self.client.layer_serial.expect("configured"));
        }
        (wl, layer)
    }

    /// A popup on `parent` (an xdg surface's) or, with `None`, on
    /// `layer`; configured and acked.
    fn popup(
        &mut self,
        parent: Option<&XdgSurface>,
        layer: &ZwlrLayerSurfaceV1,
    ) -> (wl_surface::WlSurface, XdgSurface, XdgPopup) {
        let wl = self.compositor.create_surface(&self.qh, ());
        let xdg = self.wm_base.get_xdg_surface(&wl, &self.qh, ());
        let positioner = self.wm_base.create_positioner(&self.qh, ());
        positioner.set_size(50, 50);
        positioner.set_anchor_rect(0, 0, 10, 10);
        let popup = xdg.get_popup(parent, &positioner, &self.qh, ());
        if parent.is_none() {
            layer.get_popup(&popup);
        }
        (wl, xdg, popup)
    }

    /// Commits `wl` empty and acks its configure.
    fn configure(&mut self, wl: &wl_surface::WlSurface, xdg: &XdgSurface) {
        self.client.xdg_serial = None;
        wl.commit();
        self.roundtrip().unwrap();
        xdg.ack_configure(self.client.xdg_serial.expect("configured"));
    }

    /// A `w`×`h` shm buffer.
    fn buffer(&mut self, w: i32, h: i32) -> wl_buffer::WlBuffer {
        let file = tempfile::tempfile().unwrap();
        file.set_len((w * h * 4) as u64).unwrap();
        let pool = self.shm.create_pool(file.as_fd(), w * h * 4, &self.qh, ());
        let b = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        b
    }
}

/// xdg_wm_base's `not_the_topmost_popup`.
const NOT_TOPMOST: u32 = xdg_wm_base::Error::NotTheTopmostPopup as u32;

/// A popup destroyed while a popup on it is not is an error; the child
/// destroyed first is not.
#[test]
fn a_parent_popup_destroyed_before_its_child_is_an_error() {
    for in_order in [true, false] {
        let mut raw = Raw::start();
        let (_, layer) = raw.layer(true);
        let (p_wl, p_xdg, p) = raw.popup(None, &layer);
        raw.configure(&p_wl, &p_xdg);
        let (c_wl, c_xdg, c) = raw.popup(Some(&p_xdg), &layer);
        raw.configure(&c_wl, &c_xdg);
        raw.roundtrip().unwrap();
        if in_order {
            c.destroy();
            p.destroy();
            raw.roundtrip().unwrap();
        } else {
            p.destroy();
            assert_eq!(raw.error(), (NOT_TOPMOST, "xdg_wm_base".into()));
        }
    }
}

/// A grab whose parent is not the seat's topmost grabbing popup is an
/// error; one on it is not, and nor is one after the grabbing popup went.
#[test]
fn a_grab_off_the_topmost_grabbing_popup_is_an_error() {
    let mut raw = Raw::start();
    let (_, layer) = raw.layer(true);
    let (_, p_xdg, p) = raw.popup(None, &layer);
    p.grab(&raw.seat, 1);
    let (_, _, c) = raw.popup(Some(&p_xdg), &layer);
    c.grab(&raw.seat, 1);
    raw.roundtrip().unwrap();
    // The chain gone, another may grab.
    c.destroy();
    p.destroy();
    let (_, _, q) = raw.popup(None, &layer);
    q.grab(&raw.seat, 1);
    raw.roundtrip().unwrap();
    // A second chain while one grabs.
    let (_, _, r) = raw.popup(None, &layer);
    r.grab(&raw.seat, 1);
    assert_eq!(raw.error(), (NOT_TOPMOST, "xdg_wm_base".into()));
}

/// A popup's buffer before its first `ack_configure` is xdg_surface's
/// `unconfigured_buffer`; after it, it is accepted.
#[test]
fn a_popup_buffer_before_its_ack_is_an_error() {
    for acked in [true, false] {
        let mut raw = Raw::start();
        let (_, layer) = raw.layer(true);
        let (wl, xdg, _p) = raw.popup(None, &layer);
        if acked {
            raw.configure(&wl, &xdg);
        }
        let b = raw.buffer(50, 50);
        wl.attach(Some(&b), 0, 0);
        wl.commit();
        if acked {
            raw.roundtrip().unwrap();
        } else {
            let code = xdg_surface::Error::UnconfiguredBuffer as u32;
            assert_eq!(raw.error(), (code, "xdg_surface".into()));
        }
    }
}

/// A layer surface's buffer before its first `ack_configure` is an
/// error; after it, it is accepted.
#[test]
fn a_layer_buffer_before_its_ack_is_an_error() {
    for acked in [true, false] {
        let mut raw = Raw::start();
        let (wl, _layer) = raw.layer(acked);
        let b = raw.buffer(100, 100);
        wl.attach(Some(&b), 0, 0);
        wl.commit();
        if acked {
            raw.roundtrip().unwrap();
        } else {
            let code = zwlr_layer_surface_v1::Error::InvalidSurfaceState as u32;
            assert_eq!(raw.error(), (code, "zwlr_layer_surface_v1".into()));
        }
    }
}

/// A buffer whose size is not a multiple of its scale is
/// `wl_surface.invalid_size`.
#[test]
fn a_buffer_not_a_multiple_of_its_scale_is_an_error() {
    for (w, ok) in [(100, true), (101, false)] {
        let mut raw = Raw::start();
        let (wl, _layer) = raw.layer(true);
        let b = raw.buffer(w, 100);
        wl.set_buffer_scale(2);
        wl.attach(Some(&b), 0, 0);
        wl.commit();
        if ok {
            raw.roundtrip().unwrap();
        } else {
            let code = wl_surface::Error::InvalidSize as u32;
            assert_eq!(raw.error(), (code, "wl_surface".into()));
        }
    }
}

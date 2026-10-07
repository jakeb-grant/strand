//! A tiny Wayland client with one xdg toplevel (a 64×64 shm buffer), for
//! putting real windows on a test compositor.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use rustix::event::{PollFd, PollFlags};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const SIZE: i32 = 64;

struct State {
    surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    closed: Arc<AtomicBool>,
}

/// A mapped toplevel; closes when dropped.
pub struct TestWindow {
    conn: Connection,
    toplevel: xdg_toplevel::XdgToplevel,
    stop: Arc<AtomicBool>,
    /// The compositor asked it to close (it then unmaps itself).
    pub closed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestWindow {
    /// Connects to the display at `socket` and maps a toplevel.
    pub fn open(socket: &Path, app_id: &str, title: &str) -> TestWindow {
        let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
        let (globals, mut queue) = registry_queue_init::<State>(&conn).unwrap();
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
        let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=5, ()).unwrap();
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_app_id(app_id.into());
        toplevel.set_title(title.into());
        surface.commit();

        let len = (SIZE * SIZE * 4) as usize;
        let fd = rustix::fs::memfd_create("strand-test-window", rustix::fs::MemfdFlags::CLOEXEC)
            .unwrap();
        rustix::fs::ftruncate(&fd, len as u64).unwrap();
        let pool = shm.create_pool(fd.as_fd(), len as i32, &qh, ());
        let buffer = pool.create_buffer(0, SIZE, SIZE, SIZE * 4, wl_shm::Format::Argb8888, &qh, ());
        let closed = Arc::new(AtomicBool::new(false));
        let mut state = State {
            surface,
            buffer,
            closed: closed.clone(),
        };
        queue.roundtrip(&mut state).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let _keep = (fd, pool, xdg);
            while !thread_stop.load(Ordering::SeqCst) {
                if queue.dispatch_pending(&mut state).is_err() || queue.flush().is_err() {
                    return;
                }
                let Some(guard) = queue.prepare_read() else {
                    continue;
                };
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                let timeout = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 20_000_000,
                };
                if rustix::event::poll(&mut fds, Some(&timeout)).unwrap_or(0) > 0 {
                    if guard.read().is_err() {
                        return;
                    }
                } else {
                    drop(guard);
                }
            }
        });
        TestWindow {
            conn,
            toplevel,
            stop,
            closed,
            thread: Some(thread),
        }
    }

    /// Changes the title.
    pub fn set_title(&self, title: &str) {
        self.toplevel.set_title(title.into());
        self.conn.flush().unwrap();
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.toplevel.destroy();
        let _ = self.conn.flush();
        self.stop.store(true, Ordering::SeqCst);
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

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            if !state.closed.load(Ordering::SeqCst) {
                state.surface.attach(Some(&state.buffer), 0, 0);
                state.surface.damage_buffer(0, 0, SIZE, SIZE);
                state.surface.commit();
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            // Unmap: a null buffer.
            state.closed.store(true, Ordering::SeqCst);
            state.surface.attach(None, 0, 0);
            state.surface.commit();
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

//! A tiny Wayland client with one xdg toplevel (an shm buffer,
//! transparent or one solid colour, 64×64 until the compositor configures
//! a size and then that size, as a real client fills its tile), for
//! putting real windows on a test compositor.

use std::os::fd::{AsFd, OwnedFd};
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

/// A filled shm buffer and what keeps it.
struct Buffer {
    buffer: wl_buffer::WlBuffer,
    _pool: wl_shm_pool::WlShmPool,
    _fd: OwnedFd,
    size: (i32, i32),
}

impl Buffer {
    /// A `w × h` buffer of `rgba` (straight; stored premultiplied as
    /// ARGB8888, BGRA in memory).
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        (w, h): (i32, i32),
        rgba: [u8; 4],
    ) -> Buffer {
        let len = (w * h * 4) as usize;
        let fd = rustix::fs::memfd_create("strand-test-window", rustix::fs::MemfdFlags::CLOEXEC)
            .unwrap();
        rustix::fs::ftruncate(&fd, len as u64).unwrap();
        if rgba != [0; 4] {
            let a = rgba[3] as u32;
            let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            let px = [pm(rgba[2]), pm(rgba[1]), pm(rgba[0]), rgba[3]];
            let fill = px.repeat(len / 4);
            rustix::io::pwrite(&fd, &fill, 0).unwrap();
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
        Buffer {
            buffer,
            _pool: pool,
            _fd: fd,
            size: (w, h),
        }
    }
}

struct State {
    surface: wl_surface::WlSurface,
    shm: wl_shm::WlShm,
    rgba: [u8; 4],
    buffer: Buffer,
    /// The size the last toplevel configure asked for (0: the client's
    /// choice).
    configured: (i32, i32),
    closed: Arc<AtomicBool>,
    activated: Arc<AtomicBool>,
    maximized: Arc<AtomicBool>,
    fullscreen: Arc<AtomicBool>,
}

/// A mapped toplevel; closes when dropped.
pub struct TestWindow {
    conn: Connection,
    toplevel: xdg_toplevel::XdgToplevel,
    stop: Arc<AtomicBool>,
    /// The compositor asked it to close (it then unmaps itself).
    pub closed: Arc<AtomicBool>,
    /// The compositor's last configure said it is activated (has the
    /// keyboard focus): the client's own view, independent of any
    /// compositor IPC or foreign-toplevel protocol.
    pub activated: Arc<AtomicBool>,
    /// The last configure said it is maximized (the client's own view).
    pub maximized: Arc<AtomicBool>,
    /// The last configure said it is fullscreen (the client's own view).
    pub fullscreen: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestWindow {
    /// Connects to the display at `socket` and maps a toplevel.
    pub fn open(socket: &Path, app_id: &str, title: &str) -> TestWindow {
        Self::open_filled(socket, app_id, title, [0; 4])
    }

    /// [`TestWindow::open`], its buffer filled with `rgba` (straight
    /// RGBA; stored premultiplied as ARGB8888, BGRA in memory).
    pub fn open_filled(socket: &Path, app_id: &str, title: &str, rgba: [u8; 4]) -> TestWindow {
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

        let buffer = Buffer::new(&shm, &qh, (SIZE, SIZE), rgba);
        let closed = Arc::new(AtomicBool::new(false));
        let activated = Arc::new(AtomicBool::new(false));
        let maximized = Arc::new(AtomicBool::new(false));
        let fullscreen = Arc::new(AtomicBool::new(false));
        let mut state = State {
            surface,
            shm,
            rgba,
            buffer,
            configured: (0, 0),
            closed: closed.clone(),
            activated: activated.clone(),
            maximized: maximized.clone(),
            fullscreen: fullscreen.clone(),
        };
        queue.roundtrip(&mut state).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let _keep = xdg;
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
            activated,
            maximized,
            fullscreen,
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
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            if !state.closed.load(Ordering::SeqCst) {
                // The configured size, each side the client's own (the
                // last buffer's) where the compositor leaves it to us.
                let (cw, ch) = state.configured;
                let (bw, bh) = state.buffer.size;
                let size = (
                    if cw > 0 { cw.min(4096) } else { bw },
                    if ch > 0 { ch.min(4096) } else { bh },
                );
                if size != state.buffer.size {
                    state.buffer = Buffer::new(&state.shm, qh, size, state.rgba);
                }
                state.surface.attach(Some(&state.buffer.buffer), 0, 0);
                state.surface.damage_buffer(0, 0, size.0, size.1);
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
        match event {
            xdg_toplevel::Event::Close => {
                // Unmap: a null buffer.
                state.closed.store(true, Ordering::SeqCst);
                state.surface.attach(None, 0, 0);
                state.surface.commit();
            }
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                state.configured = (width, height);
                let has = |s: xdg_toplevel::State| {
                    states
                        .chunks_exact(4)
                        .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == u32::from(s))
                };
                state
                    .activated
                    .store(has(xdg_toplevel::State::Activated), Ordering::SeqCst);
                state
                    .maximized
                    .store(has(xdg_toplevel::State::Maximized), Ordering::SeqCst);
                state
                    .fullscreen
                    .store(has(xdg_toplevel::State::Fullscreen), Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

//! `finished` after `locked`, which sway 1.9 never sends to a live lock
//! client, driven by a fake compositor (wayland-server) on a socket pair:
//! no real compositor and no real session is involved, so unlike
//! tests/session_lock.rs this runs everywhere.
//!
//! The fake is strict in the way the protocol allows: a lock it sent
//! `locked` to stays the session's locker until the client destroys the
//! object, `finished` or not, and while it lives every new lock is
//! refused. The client must therefore answer `finished` with
//! `unlock_and_destroy` (the protocol's reply after `locked`; `destroy`
//! would be a protocol error) before its new lock can be granted.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};
use strand_scene::{Damage, PaintTarget, Painter, SurfaceId};
use strand_surface::{Config, LockState, SurfaceHost, SurfaceManager};
use wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1::{self, ExtSessionLockManagerV1},
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_server::backend::ClientData;
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};

const WAIT: Duration = Duration::from_secs(5);

// ---- the fake compositor ----------------------------------------------------

enum Cmd {
    /// `finished` on the lock that holds the session.
    End,
}

#[derive(Default)]
struct Server {
    next: u32,
    /// The session's locker: sent `locked`, not destroyed yet.
    held: Option<(u32, ExtSessionLockV1)>,
    /// What the client asked and the fake answered, in order.
    log: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn note(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }

    fn apply(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::End => {
                if let Some((n, lock)) = &self.held {
                    lock.finished();
                    self.note(format!("finished {n}"));
                }
            }
        }
    }
}

struct Fake {
    tx: mpsc::Sender<Cmd>,
    log: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct NoData;
impl ClientData for NoData {}

impl Fake {
    /// Starts the fake on one end of a socket pair; the other is the
    /// client's.
    fn start() -> (Fake, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_log, thread_stop) = (log.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let dh = display.handle();
            dh.create_global::<Server, wl_compositor::WlCompositor, ()>(4, ());
            dh.create_global::<Server, wl_shm::WlShm, ()>(1, ());
            dh.create_global::<Server, ZwlrLayerShellV1, ()>(4, ());
            dh.create_global::<Server, ExtSessionLockManagerV1, ()>(1, ());
            display
                .handle()
                .insert_client(ours, Arc::new(NoData))
                .unwrap();
            let mut state = Server {
                log: thread_log,
                ..Server::default()
            };
            while !thread_stop.load(Ordering::SeqCst) {
                while let Ok(cmd) = rx.try_recv() {
                    state.apply(cmd);
                }
                if display.dispatch_clients(&mut state).is_err() {
                    break;
                }
                let _ = display.flush_clients();
                let poll_fd = display.backend().poll_fd().try_clone_to_owned().unwrap();
                let mut fds = [PollFd::new(&poll_fd, PollFlags::IN)];
                let timeout = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 5_000_000,
                };
                let _ = rustix::event::poll(&mut fds, Some(&timeout));
            }
        });
        (
            Fake {
                tx,
                log,
                stop,
                thread: Some(thread),
            },
            theirs,
        )
    }

    fn cmd(&self, cmd: Cmd) {
        self.tx.send(cmd).unwrap();
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

macro_rules! plain_global {
    ($iface:ty) => {
        impl GlobalDispatch<$iface, ()> for Server {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$iface>,
                _: &(),
                data_init: &mut DataInit<'_, Self>,
            ) {
                data_init.init(resource, ());
            }
        }
    };
}

plain_global!(wl_compositor::WlCompositor);
plain_global!(ZwlrLayerShellV1);
plain_global!(ExtSessionLockManagerV1);

impl GlobalDispatch<wl_shm::WlShm, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

/// Objects whose requests the fake ignores.
macro_rules! inert {
    ($iface:ty, $req:ty) => {
        impl Dispatch<$iface, ()> for Server {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$iface,
                _: $req,
                _: &(),
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}

inert!(wl_region::WlRegion, wl_region::Request);
inert!(wl_callback::WlCallback, wl_callback::Request);
inert!(wl_buffer::WlBuffer, wl_buffer::Request);
inert!(ZwlrLayerShellV1, zwlr_layer_shell_v1::Request);
inert!(
    ExtSessionLockSurfaceV1,
    ext_session_lock_surface_v1::Request
);

impl Dispatch<wl_compositor::WlCompositor, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, ());
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_surface::Request::Frame { callback } = request {
            data_init.init(callback, ());
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer { id, .. } = request {
            data_init.init(id, ());
        }
    }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtSessionLockManagerV1,
        request: ext_session_lock_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_session_lock_manager_v1::Request::Lock { id } = request {
            state.next += 1;
            let n = state.next;
            let lock = data_init.init(id, n);
            state.note(format!("lock {n}"));
            if state.held.is_some() {
                // Another lock object still holds the session.
                lock.finished();
                state.note(format!("refused {n}"));
            } else {
                lock.locked();
                state.note(format!("locked {n}"));
                state.held = Some((n, lock));
            }
        }
    }
}

impl Dispatch<ExtSessionLockV1, u32> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtSessionLockV1,
        request: ext_session_lock_v1::Request,
        n: &u32,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let gone = match request {
            ext_session_lock_v1::Request::GetLockSurface { id, .. } => {
                data_init.init(id, ());
                false
            }
            ext_session_lock_v1::Request::Destroy => {
                state.note(format!("destroy {n}"));
                true
            }
            ext_session_lock_v1::Request::UnlockAndDestroy => {
                state.note(format!("unlock_and_destroy {n}"));
                true
            }
            _ => false,
        };
        if gone && state.held.as_ref().is_some_and(|(h, _)| h == n) {
            state.held = None;
        }
    }
}

// ---- the client ---------------------------------------------------------------

#[derive(Default)]
struct Host {
    locks: Vec<LockState>,
}

impl Painter for Host {
    fn paint(&mut self, _: SurfaceId, _: &mut PaintTarget<'_>) -> Damage {
        Damage::new()
    }

    fn wants_frame(&self, _: SurfaceId) -> bool {
        false
    }
}

impl SurfaceHost for Host {
    fn lock_changed(&mut self, state: LockState) {
        self.locks.push(state);
    }
}

fn wait_locks(mgr: &mut SurfaceManager<Host>, fake: &Fake, want: &[LockState], log: &[&str]) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        mgr.dispatch(Some(Duration::from_millis(10))).unwrap();
        if mgr.state().host().locks == want && fake.log() == log {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "lock states {:?} (want {want:?}), fake saw {:?} (want {log:?})",
            mgr.state().host().locks,
            fake.log()
        );
    }
}

/// The compositor ends a lock it held: the client answers with
/// `unlock_and_destroy` on that object and then asks for a new lock,
/// which the compositor grants because the old object is gone. A second
/// end is answered the same way and not asked again (once per lock
/// session).
#[test]
fn finished_after_locked_is_answered_and_a_new_lock_is_granted() {
    use LockState::{Finished, Locked};
    let (fake, socket) = Fake::start();
    let conn = wayland_client::Connection::from_socket(socket).unwrap();
    let mut mgr = SurfaceManager::with_connection(conn, Host::default(), Config::default())
        .expect("the lock client connects to the fake");
    mgr.state_mut().enable_session_lock();
    mgr.state_mut().lock().unwrap();
    wait_locks(&mut mgr, &fake, &[Locked], &["lock 1", "locked 1"]);

    fake.cmd(Cmd::End);
    wait_locks(
        &mut mgr,
        &fake,
        &[Locked, Finished, Locked],
        &[
            "lock 1",
            "locked 1",
            "finished 1",
            "unlock_and_destroy 1",
            "lock 2",
            "locked 2",
        ],
    );
    assert!(mgr.state().is_locked(), "the new lock holds the session");

    // The one asked for again ends too: answered, not asked a third time.
    fake.cmd(Cmd::End);
    wait_locks(
        &mut mgr,
        &fake,
        &[Locked, Finished, Locked, Finished],
        &[
            "lock 1",
            "locked 1",
            "finished 1",
            "unlock_and_destroy 1",
            "lock 2",
            "locked 2",
            "finished 2",
            "unlock_and_destroy 2",
        ],
    );
    for _ in 0..20 {
        mgr.dispatch(Some(Duration::from_millis(10))).unwrap();
    }
    assert!(!mgr.state().is_locked());
    assert_eq!(fake.log().len(), 8, "no third lock: {:?}", fake.log());
}

//! A Wayland proxy between strand and the lock VM's sway that can end a
//! lock sway granted: sway 1.9 never sends `finished` to a lock it sent
//! `locked` to, so the fault "the compositor ended the lock" is played
//! here. Every byte and file descriptor passes through unchanged; the
//! proxy only reads the requests that make a lock (`wl_display.get_registry`,
//! `wl_registry.bind` of `ext_session_lock_manager_v1`, its `lock`) to
//! know the lock's object id, and once [`Proxy::end`] is called it adds
//! one `ext_session_lock_v1.finished` (event 1) after that lock's
//! `locked` (event 0), between two whole messages.
//!
//! The client then answers `unlock_and_destroy` and asks for a new lock,
//! which reach sway as they would from any client: sway unlocks and
//! locks again, as a compositor that had really ended the lock would.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The ids the client's requests made, per connection.
#[derive(Default)]
struct Ids {
    registries: HashSet<u32>,
    managers: HashSet<u32>,
    locks: HashSet<u32>,
}

#[derive(Default)]
struct Shared {
    /// [`Proxy::end`] was called.
    armed: AtomicBool,
    /// The `finished` went out.
    sent: AtomicBool,
}

pub struct Proxy {
    path: PathBuf,
    shared: Arc<Shared>,
}

impl Proxy {
    /// Listens at `dir/name` and forwards each client to `upstream`.
    pub fn start(dir: &Path, name: &str, upstream: PathBuf) -> Proxy {
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let shared = Arc::new(Shared::default());
        let s = Arc::clone(&shared);
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { return };
                let Ok(server) = UnixStream::connect(&upstream) else {
                    return;
                };
                let ids = Arc::new(Mutex::new(Ids::default()));
                let (c, sv) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                let ids2 = Arc::clone(&ids);
                std::thread::spawn(move || requests(c, sv, ids2));
                let s = Arc::clone(&s);
                std::thread::spawn(move || events(server, client, ids, s));
            }
        });
        Proxy { path, shared }
    }

    /// The socket's name, for `WAYLAND_DISPLAY` (beside the real one).
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// From now on, the next lock sway grants (or the one it granted)
    /// is ended once.
    pub fn end(&self) {
        self.shared.armed.store(true, Ordering::Release);
    }

    /// The `finished` went out.
    pub fn ended(&self) -> bool {
        self.shared.sent.load(Ordering::Acquire)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn word(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]]))
}

/// Whole messages at the front of `buf`: their total length.
fn whole(buf: &[u8]) -> usize {
    let mut at = 0;
    while let Some(h) = word(buf, at + 4) {
        let size = (h >> 16) as usize;
        if size < 8 || at + size > buf.len() {
            break;
        }
        at += size;
    }
    at
}

/// Client to compositor: forwarded as read, the lock's ids noted first
/// (so they are known before any event about them can come back).
fn requests(client: UnixStream, server: UnixStream, ids: Arc<Mutex<Ids>>) {
    let mut buf = vec![0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let mut fds = Vec::new();
        let n = match recv(client.as_raw_fd(), &mut buf, &mut fds) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        let end = whole(&pending);
        note(&pending[..end], &ids);
        pending.drain(..end);
        if send(server.as_raw_fd(), &buf[..n], &fds).is_err() {
            break;
        }
    }
    let _ = server.shutdown(std::net::Shutdown::Both);
    let _ = client.shutdown(std::net::Shutdown::Both);
}

/// Notes the ids of whole requests `msgs`.
fn note(msgs: &[u8], ids: &Mutex<Ids>) {
    let mut ids = ids.lock().unwrap();
    let mut at = 0;
    while let (Some(obj), Some(h)) = (word(msgs, at), word(msgs, at + 4)) {
        let (size, op) = ((h >> 16) as usize, h & 0xffff);
        let body = &msgs[at + 8..at + size];
        if obj == 1 && op == 1 {
            // wl_display.get_registry(new_id)
            ids.registries.extend(word(body, 0));
        } else if ids.registries.contains(&obj) && op == 0 {
            // wl_registry.bind(name, interface: string, version, new_id)
            if let Some(len) = word(body, 4) {
                let len = len as usize;
                let padded = len.div_ceil(4) * 4;
                let iface = body.get(8..8 + len.saturating_sub(1)).unwrap_or_default();
                if iface == b"ext_session_lock_manager_v1" {
                    ids.managers.extend(word(body, 8 + padded + 4));
                }
            }
        } else if ids.managers.contains(&obj) && op == 1 {
            // ext_session_lock_manager_v1.lock(new_id)
            ids.locks.extend(word(body, 0));
        }
        at += size;
    }
}

/// Compositor to client: whole messages only, so `finished` goes in
/// between two of them.
fn events(server: UnixStream, client: UnixStream, ids: Arc<Mutex<Ids>>, shared: Arc<Shared>) {
    let mut buf = vec![0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    // A lock sway sent `locked` to.
    let mut granted: Option<u32> = None;
    loop {
        let mut poll = libc::pollfd {
            fd: server.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let r = unsafe { libc::poll(&mut poll, 1, 50) };
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
        if r > 0 {
            let n = match recv(server.as_raw_fd(), &mut buf, &mut fds) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            pending.extend_from_slice(&buf[..n]);
            let end = whole(&pending);
            if end == 0 {
                continue;
            }
            {
                let ids = ids.lock().unwrap();
                let mut at = 0;
                while at < end {
                    let (obj, h) = (word(&pending, at).unwrap(), word(&pending, at + 4).unwrap());
                    if h & 0xffff == 0 && ids.locks.contains(&obj) && granted.is_none() {
                        granted = Some(obj);
                    }
                    at += (h >> 16) as usize;
                }
            }
            let out: Vec<u8> = pending.drain(..end).collect();
            if send(client.as_raw_fd(), &out, &fds).is_err() {
                break;
            }
            fds.clear();
        }
        if let Some(lock) = granted
            && shared.armed.load(Ordering::Acquire)
            && !shared.sent.load(Ordering::Acquire)
        {
            // ext_session_lock_v1.finished: no arguments.
            let mut msg = Vec::with_capacity(8);
            msg.extend_from_slice(&lock.to_ne_bytes());
            msg.extend_from_slice(&((8u32 << 16) | 1).to_ne_bytes());
            if send(client.as_raw_fd(), &msg, &[]).is_err() {
                break;
            }
            shared.sent.store(true, Ordering::Release);
        }
    }
    let _ = server.shutdown(std::net::Shutdown::Both);
    let _ = client.shutdown(std::net::Shutdown::Both);
}

/// `recvmsg` with the descriptors it carried.
fn recv(fd: RawFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = [0u64; 64];
    // SAFETY: a zeroed msghdr is valid; its pointers are set below to
    // buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    let n = loop {
        // SAFETY: `msg` points at live buffers of the stated sizes.
        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // SAFETY: the control buffer was filled by the kernel; the CMSG
    // macros walk it within `msg_controllen`.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok(n)
}

/// `sendmsg` of all of `bytes`, `fds` with the first part.
fn send(fd: RawFd, bytes: &[u8], fds: &[OwnedFd]) -> io::Result<()> {
    let mut at = 0;
    let mut with_fds = !fds.is_empty();
    while at < bytes.len() || with_fds {
        let rest = &bytes[at..];
        let mut iov = libc::iovec {
            iov_base: rest.as_ptr() as *mut _,
            iov_len: rest.len(),
        };
        let mut control = [0u64; 64];
        // SAFETY: as in `recv`.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if with_fds {
            let raw: Vec<RawFd> = fds.iter().map(|f| f.as_raw_fd()).collect();
            let len = std::mem::size_of_val(raw.as_slice()) as u32;
            msg.msg_control = control.as_mut_ptr().cast();
            // SAFETY: CMSG_SPACE of at most 28 descriptors fits the
            // 512-byte buffer; the header and data are written inside it.
            unsafe {
                msg.msg_controllen = libc::CMSG_SPACE(len) as _;
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(len) as _;
                std::ptr::copy_nonoverlapping(
                    raw.as_ptr(),
                    libc::CMSG_DATA(c) as *mut RawFd,
                    raw.len(),
                );
            }
        }
        // SAFETY: `msg` points at live buffers of the stated sizes.
        let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        with_fds = false;
        at += n as usize;
    }
    Ok(())
}

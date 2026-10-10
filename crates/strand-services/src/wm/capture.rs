//! (M4) Window thumbnails (design.md: "`thumbnail w` via
//! ext-image-copy-capture").
//!
//! A [`CaptureTap`] ([`capture_window`]) asks for a window's frames, by its
//! window id (`windows.all[i].id`), at most a given size. Taps are
//! process-wide, like the audio level taps: the binary takes one per
//! visible `thumbnail` and feeds render what it gets. The running
//! compositor service ([`super::drive`]) maps each tap's window to its
//! `ext-foreign-toplevel-list-v1` identifier ([`super::Window::toplevel`])
//! and hands the list to its `strand-toplevel` thread
//! ([`super::protocol::ProtoCmd::Capture`]), the connection that owns the
//! toplevel handles. No tap, no session: a hidden thumbnail captures
//! nothing.
//!
//! On that thread, [`Captures`] keeps one `ext_image_copy_capture_session_v1`
//! per captured toplevel (from
//! `ext_foreign_toplevel_image_capture_source_manager_v1`) with one shm
//! buffer at the session's constraints (`buffer_size`, an ARGB8888 or
//! XRGB8888 `shm_format`, `done`), and at most one frame in flight. A
//! frame's `ready` reads the buffer, scales it down to cover the largest
//! size its taps asked for, and calls them on that thread. The next frame
//! is asked for at most [`MAX_FPS`] times a second; the compositor
//! answers it only once the window has changed (the protocol's damage
//! tracking), so a still window costs no frames. A `failed` frame for new
//! constraints waits for the session's next `done`; a stopped session
//! ends, and is replaced after [`RETRY`] if its window is still wanted
//! and listed (a compositor may stop a session for reasons of its own;
//! a closed window leaves the list, and its wants with it); another
//! failure retries after [`RETRY`].
//!
//! A frame's `transform` (sent before `ready`) says how the buffer's
//! contents are transformed, as a `wl_surface` buffer transform does: the
//! frame is turned upright by its inverse before it is scaled, so a
//! window drawn with a rotated or flipped buffer (or on a rotated output)
//! shows as it looks on screen. Sessions are made without
//! `paint_cursors`, so the protocol leaves the pointer out: a thumbnail
//! shows the window, not where the pointer was over it (cursor sessions
//! are not used).
//!
//! A tap whose window is no longer listed (it closed) is called once
//! with `None`, so its thumbnail stops showing the window's last frame.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use super::protocol::Client;

/// The most frames a second a thumbnail asks for.
pub const MAX_FPS: u32 = 15;

/// How long a capture that failed for no stated reason waits to retry.
pub const RETRY: Duration = Duration::from_secs(1);

/// The largest buffer a session takes (a side), whatever the compositor
/// asks.
const MAX_SIDE: u32 = 8192;

/// One captured frame: premultiplied BGRA (ARGB8888 in memory), rows
/// tightly packed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[u8]>,
}

type TapFn = Arc<dyn Fn(Option<&CaptureFrame>) + Send + Sync>;

#[derive(Default)]
struct Taps {
    next: u64,
    taps: Vec<(u64, String, (u32, u32), TapFn)>,
    /// The running compositor services' pokes: they re-read the taps.
    pokes: Vec<(u64, Box<dyn Fn() + Send + Sync>)>,
}

static TAPS: Mutex<Option<Taps>> = Mutex::new(None);

fn with_taps<R>(f: impl FnOnce(&mut Taps) -> R) -> R {
    let mut g = TAPS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Taps::default))
}

fn poke() {
    with_taps(|t| {
        for (_, p) in &t.pokes {
            p();
        }
    });
}

/// A subscription to a window's frames: while it lives (and the
/// compositor service runs), the window is captured and the tap's
/// function called with each frame, on the `strand-toplevel` thread (it
/// must not block), and with `None` once the window it was capturing is
/// no longer listed. Dropping it stops the capture when no other tap
/// wants that window.
pub struct CaptureTap {
    id: u64,
}

impl std::fmt::Debug for CaptureTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureTap").field("id", &self.id).finish()
    }
}

impl Drop for CaptureTap {
    fn drop(&mut self) {
        with_taps(|t| t.taps.retain(|(id, ..)| *id != self.id));
        poke();
    }
}

/// Captures the window with id `window` (`windows.all[i].id`), its frames
/// scaled down to cover `max` (physical pixels; 0 is no limit); `f` gets
/// `None` when the window it was captured from is gone.
pub fn capture_window(
    window: &str,
    max: (u32, u32),
    f: impl Fn(Option<&CaptureFrame>) + Send + Sync + 'static,
) -> CaptureTap {
    let id = with_taps(|t| {
        t.next += 1;
        let id = t.next;
        t.taps.push((id, window.to_string(), max, Arc::new(f)));
        id
    });
    poke();
    CaptureTap { id }
}

/// Removes a poke when dropped.
pub(crate) struct PokeGuard(u64);

impl Drop for PokeGuard {
    fn drop(&mut self) {
        with_taps(|t| t.pokes.retain(|(id, _)| *id != self.0));
    }
}

/// Calls `f` whenever the taps change (a running service re-reads them).
pub(crate) fn on_taps_changed(f: impl Fn() + Send + Sync + 'static) -> PokeGuard {
    let id = with_taps(|t| {
        t.next += 1;
        t.pokes.push((t.next, Box::new(f)));
        t.next
    });
    PokeGuard(id)
}

/// One tap as the protocol thread serves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Want {
    pub tap: u64,
    /// The toplevel's `ext-foreign-toplevel-list-v1` identifier.
    pub identifier: String,
    pub max: (u32, u32),
}

/// The taps whose window `windows` (id, toplevel identifier) knows, by
/// identifier.
pub(crate) fn wants<'a>(
    windows: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Vec<Want> {
    let by_id: HashMap<&str, &str> = windows
        .into_iter()
        .filter_map(|(id, t)| Some((id, t?)))
        .collect();
    with_taps(|t| {
        t.taps
            .iter()
            .filter_map(|(tap, window, max, _)| {
                Some(Want {
                    tap: *tap,
                    identifier: by_id.get(window.as_str())?.to_string(),
                    max: *max,
                })
            })
            .collect()
    })
}

/// Calls tap `tap`'s function, if it still lives.
fn deliver(tap: u64, frame: Option<&CaptureFrame>) {
    let f = with_taps(|t| {
        t.taps
            .iter()
            .find(|(id, ..)| *id == tap)
            .map(|(.., f)| f.clone())
    });
    if let Some(f) = f {
        f(frame);
    }
}

/// `src` (`w × h`, `stride` bytes a row, ARGB8888 or XRGB8888 in memory)
/// as a tight premultiplied BGRA frame scaled down (box filter) to cover
/// `max`.
pub fn downscale(
    src: &[u8],
    w: u32,
    h: u32,
    stride: u32,
    opaque: bool,
    max: (u32, u32),
) -> CaptureFrame {
    let k = if max.0 == 0 || max.1 == 0 {
        1.0
    } else {
        (max.0 as f64 / w as f64)
            .max(max.1 as f64 / h as f64)
            .min(1.0)
    };
    let (dw, dh) = (
        ((w as f64 * k).round() as u32).clamp(1, w.max(1)),
        ((h as f64 * k).round() as u32).clamp(1, h.max(1)),
    );
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh {
        let (y0, y1) = (y * h / dh, ((y + 1) * h).div_ceil(dh).max(y * h / dh + 1));
        for x in 0..dw {
            let (x0, x1) = (x * w / dw, ((x + 1) * w).div_ceil(dw).max(x * w / dw + 1));
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for sy in y0..y1.min(h) {
                for sx in x0..x1.min(w) {
                    let i = (sy * stride + sx * 4) as usize;
                    if let Some(p) = src.get(i..i + 4) {
                        for c in 0..4 {
                            acc[c] += p[c] as u32;
                        }
                        n += 1;
                    }
                }
            }
            let o = ((y * dw + x) * 4) as usize;
            for c in 0..4 {
                if let Some(v) = (acc[c] + n / 2).checked_div(n) {
                    out[o + c] = v as u8;
                }
            }
            if opaque {
                out[o + 3] = 255;
            }
        }
    }
    CaptureFrame {
        width: dw,
        height: dh,
        pixels: out.into(),
    }
}

/// A session's shm buffer.
#[derive(Debug)]
struct Shm {
    fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
    format: wl_shm::Format,
}

impl Drop for Shm {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// One captured toplevel.
#[derive(Debug)]
struct Session {
    key: u64,
    identifier: String,
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    pending_size: Option<(u32, u32)>,
    pending_formats: Vec<wl_shm::Format>,
    shm: Option<Shm>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// The frame in flight's `transform` (normal unless it says).
    transform: wl_output::Transform,
    /// When the next frame may be asked for (`None`: wait for `done`).
    next_at: Option<Instant>,
    /// The session stopped and ended: when a new one may replace it.
    stopped: Option<Instant>,
}

impl Session {
    /// The compositor stopped it: it ends, to be replaced after
    /// [`RETRY`].
    fn stop(&mut self) {
        if self.stopped.is_none() {
            self.end();
            self.stopped = Some(Instant::now() + RETRY);
        }
    }

    fn end(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        self.shm = None;
        self.session.destroy();
        self.source.destroy();
    }
}

/// The thumbnails' sessions on the `strand-toplevel` thread.
#[derive(Debug, Default)]
pub(crate) struct Captures {
    pub(crate) source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    pub(crate) copy_manager: Option<ExtImageCopyCaptureManagerV1>,
    pub(crate) shm: Option<wl_shm::WlShm>,
    /// What the service asked for ([`super::protocol::ProtoCmd::Capture`]).
    pub(crate) wants: Vec<Want>,
    sessions: Vec<Session>,
    /// The taps the last step served: one missing from the wants since,
    /// and still alive, lost its window (it gets `None`).
    served: Vec<u64>,
    next_key: u64,
    /// Frames delivered so far (tests).
    pub(crate) delivered: u64,
}

impl Captures {
    /// When the loop must wake to ask for a frame.
    /// (Or to replace a stopped session.)
    pub(crate) fn due(&self) -> Option<Instant> {
        self.sessions
            .iter()
            .filter_map(|s| match s.stopped {
                Some(at) => Some(at),
                None if s.frame.is_none() && s.shm.is_some() => s.next_at,
                None => None,
            })
            .min()
    }

    fn session_mut(&mut self, key: u64) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.key == key)
    }
}

impl Client {
    /// Binds the capture globals the registry offers (`wl_shm` and the
    /// two capture managers); others are ignored.
    pub(crate) fn bind_capture_global(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        let caps = &mut self.captures;
        if interface == wl_shm::WlShm::interface().name && caps.shm.is_none() {
            caps.shm = Some(registry.bind(name, version.min(1), qh, ()));
        } else if interface == ExtForeignToplevelImageCaptureSourceManagerV1::interface().name
            && caps.source_manager.is_none()
        {
            caps.source_manager = Some(registry.bind(name, version.min(1), qh, ()));
        } else if interface == ExtImageCopyCaptureManagerV1::interface().name
            && caps.copy_manager.is_none()
        {
            caps.copy_manager = Some(registry.bind(name, version.min(1), qh, ()));
        }
    }

    /// Starts the sessions the wants need (their toplevel known), ends
    /// the ones no tap wants, and asks for the frames that are due.
    pub(crate) fn captures_step(&mut self, qh: &QueueHandle<Self>, now: Instant) {
        let caps = &mut self.captures;
        let served: Vec<u64> = caps.wants.iter().map(|w| w.tap).collect();
        for gone in caps.served.iter().filter(|t| !served.contains(t)) {
            // A dropped tap is not called: only one whose window went.
            deliver(*gone, None);
        }
        caps.served = served;
        caps.sessions.retain_mut(|s| {
            if let Some(at) = s.stopped {
                // Ended already; gone once it may be replaced.
                return at > now && caps.wants.iter().any(|w| w.identifier == s.identifier);
            }
            let keep = caps.wants.iter().any(|w| w.identifier == s.identifier);
            if !keep {
                s.end();
            }
            keep
        });
        if let (Some(src), Some(copy)) = (&caps.source_manager, &caps.copy_manager) {
            for w in &caps.wants {
                if caps.sessions.iter().any(|s| s.identifier == w.identifier) {
                    continue;
                }
                let Some(handle) = self.toplevels.iter().find_map(|(_, t)| {
                    t.current
                        .as_ref()
                        .is_some_and(|c| c.identifier == w.identifier)
                        .then_some(&t.handle)
                }) else {
                    continue;
                };
                caps.next_key += 1;
                let key = caps.next_key;
                let source = src.create_source(handle, qh, ());
                let session = copy.create_session(&source, Options::empty(), qh, key);
                caps.sessions.push(Session {
                    key,
                    identifier: w.identifier.clone(),
                    source,
                    session,
                    pending_size: None,
                    pending_formats: Vec::new(),
                    shm: None,
                    frame: None,
                    transform: wl_output::Transform::Normal,
                    next_at: None,
                    stopped: None,
                });
            }
        }
        for s in &mut caps.sessions {
            let due = s.next_at.is_some_and(|t| t <= now);
            if s.stopped.is_some() || s.frame.is_some() || !due {
                continue;
            }
            let Some(shm) = &s.shm else {
                continue;
            };
            let frame = s.session.create_frame(qh, s.key);
            frame.attach_buffer(&shm.buffer);
            frame.damage_buffer(0, 0, shm.width as i32, shm.height as i32);
            frame.capture();
            s.frame = Some(frame);
            s.transform = wl_output::Transform::Normal;
            s.next_at = None;
        }
    }

    /// The session's constraints are complete: a buffer at them.
    fn session_done(&mut self, key: u64, qh: &QueueHandle<Self>) {
        let shm_global = self.captures.shm.clone();
        let Some(s) = self.captures.session_mut(key) else {
            return;
        };
        // A batch ends here, whatever order its events came in (the
        // protocol fixes only that `done` is last); the next one starts
        // empty.
        let size = s.pending_size.take();
        let formats = std::mem::take(&mut s.pending_formats);
        let Some((w, h)) =
            size.filter(|(w, h)| (1..=MAX_SIDE).contains(w) && (1..=MAX_SIDE).contains(h))
        else {
            return;
        };
        let format = [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888]
            .into_iter()
            .find(|f| formats.contains(f));
        let (Some(format), Some(shm_global)) = (format, shm_global) else {
            log::debug!("thumbnail {}: no shm format we read", s.identifier);
            return;
        };
        let current = s
            .shm
            .as_ref()
            .is_some_and(|b| (b.width, b.height, b.format) == (w, h, format));
        if !current {
            s.shm = None;
            match make_shm(&shm_global, w, h, format, qh) {
                Ok(shm) => s.shm = Some(shm),
                Err(e) => {
                    log::warn!("thumbnail {}: no buffer: {e}", s.identifier);
                    return;
                }
            }
        }
        if s.frame.is_none() && s.next_at.is_none() {
            s.next_at = Some(Instant::now());
        }
    }

    /// A frame is ready: its pixels to the session's taps.
    fn frame_ready(&mut self, key: u64) {
        let wants: Vec<Want> = self.captures.wants.clone();
        let Some(s) = self.captures.session_mut(key) else {
            return;
        };
        if let Some(f) = s.frame.take() {
            f.destroy();
        }
        s.next_at = Some(Instant::now() + Duration::from_secs(1) / MAX_FPS);
        let Some(shm) = &s.shm else {
            return;
        };
        let stride = shm.width * 4;
        let mut data = vec![0u8; (stride * shm.height) as usize];
        if rustix::io::pread(shm.fd.as_fd(), &mut data[..], 0).is_err() {
            return;
        }
        let (data, width, height) = upright(data, shm.width, shm.height, s.transform);
        let taps: Vec<&Want> = wants
            .iter()
            .filter(|w| w.identifier == s.identifier)
            .collect();
        let max = taps.iter().fold((0u32, 0u32), |m, w| {
            if w.max.0 == 0 || w.max.1 == 0 {
                (u32::MAX, u32::MAX)
            } else {
                (m.0.max(w.max.0), m.1.max(w.max.1))
            }
        });
        let max = if max == (u32::MAX, u32::MAX) {
            (0, 0)
        } else {
            max
        };
        let opaque = shm.format == wl_shm::Format::Xrgb8888;
        let frame = downscale(&data, width, height, width * 4, opaque, max);
        for w in taps {
            deliver(w.tap, Some(&frame));
        }
        self.captures.delivered += 1;
    }
}

/// A `w × h` buffer of 4-byte pixels (rows tightly packed) whose
/// contents are transformed by `t` (a `wl_output.transform`, the way a
/// `wl_surface` buffer transform is: rotations counter-clockwise, a flip
/// about the vertical axis first), turned upright: the pixels and their
/// upright size. Each upright pixel `(x, y)` (`W × H`) is the buffer's
/// pixel at `t` applied to it.
pub fn upright(src: Vec<u8>, w: u32, h: u32, t: wl_output::Transform) -> (Vec<u8>, u32, u32) {
    use wl_output::Transform as T;
    if t == T::Normal || src.len() < (w as usize) * (h as usize) * 4 {
        return (src, w, h);
    }
    let turned = matches!(t, T::_90 | T::_270 | T::Flipped90 | T::Flipped270);
    let (uw, uh) = if turned { (h, w) } else { (w, h) };
    let (mw, mh) = (uw as usize - 1, uh as usize - 1);
    let mut out = vec![0u8; src.len()];
    for y in 0..uh as usize {
        for x in 0..uw as usize {
            let (bx, by) = match t {
                T::_90 => (y, mw - x),
                T::_180 => (mw - x, mh - y),
                T::_270 => (mh - y, x),
                T::Flipped => (mw - x, y),
                T::Flipped90 => (y, x),
                T::Flipped180 => (x, mh - y),
                T::Flipped270 => (mh - y, mw - x),
                _ => (x, y),
            };
            let i = (by * w as usize + bx) * 4;
            let o = (y * uw as usize + x) * 4;
            out[o..o + 4].copy_from_slice(&src[i..i + 4]);
        }
    }
    (out, uw, uh)
}

/// A memfd-backed shm buffer of `w × h` in `format`.
fn make_shm(
    shm: &wl_shm::WlShm,
    w: u32,
    h: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<Client>,
) -> std::io::Result<Shm> {
    let size = (w as u64) * (h as u64) * 4;
    let fd = rustix::fs::memfd_create(
        "strand-thumbnail",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )?;
    rustix::fs::ftruncate(&fd, size)?;
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, format, qh, ());
    Ok(Shm {
        fd,
        pool,
        buffer,
        width: w,
        height: h,
        format,
    })
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u64> for Client {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        key: &u64,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(s) = state.captures.session_mut(*key) {
                    s.pending_size = Some((width, height));
                }
            }
            Event::ShmFormat {
                format: WEnum::Value(f),
            } => {
                if let Some(s) = state.captures.session_mut(*key) {
                    s.pending_formats.push(f);
                }
            }
            Event::Done => state.session_done(*key, qh),
            Event::Stopped => {
                if let Some(s) = state.captures.session_mut(*key) {
                    s.stop();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for Client {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        key: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Transform {
                transform: WEnum::Value(t),
            } => {
                if let Some(s) = state.captures.session_mut(*key) {
                    s.transform = t;
                }
            }
            Event::Ready => state.frame_ready(*key),
            Event::Failed { reason } => {
                let Some(s) = state.captures.session_mut(*key) else {
                    return;
                };
                if let Some(f) = s.frame.take() {
                    f.destroy();
                }
                match reason {
                    // New constraints follow, then `done`.
                    WEnum::Value(FailureReason::BufferConstraints) => s.next_at = None,
                    WEnum::Value(FailureReason::Stopped) => s.stop(),
                    _ => s.next_at = Some(Instant::now() + RETRY),
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ExtImageCaptureSourceV1,
        _: ext_image_capture_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _: wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureManagerV1,
        _: ext_image_copy_capture_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Client {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_scale_down_to_cover_and_keep_alpha() {
        // 4 × 2, two colours side by side, XRGB with junk alpha.
        let mut src = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                let c: [u8; 4] = if x < 2 {
                    [10, 20, 30, 7]
                } else {
                    [50, 60, 70, 9]
                };
                src.extend_from_slice(&c);
            }
        }
        let f = downscale(&src, 4, 2, 16, true, (2, 1));
        assert_eq!((f.width, f.height), (2, 1));
        assert_eq!(&f.pixels[..], &[10, 20, 30, 255, 50, 60, 70, 255]);
        // Covering a box of another shape: the larger scale wins.
        let f = downscale(&src, 4, 2, 16, false, (1, 2));
        assert_eq!((f.width, f.height), (4, 2), "never scaled up");
        let f = downscale(&src, 4, 2, 16, false, (0, 0));
        assert_eq!((f.width, f.height), (4, 2));
        assert_eq!(f.pixels[3], 7, "ARGB keeps its alpha");
    }

    /// Each transform turns the buffer upright: a 3 × 2 upright image
    /// (pixels 0..6 in reading order) transformed as the protocol says
    /// comes back as it was.
    #[test]
    fn frames_are_turned_upright() {
        use wl_output::Transform as T;
        // The upright image, and each transform's buffer of it, written
        // out by hand (rotations counter-clockwise, a flip first).
        let upright_px = [0u8, 1, 2, 3, 4, 5];
        let cases: [(T, (u32, u32), [u8; 6]); 8] = [
            (T::Normal, (3, 2), [0, 1, 2, 3, 4, 5]),
            (T::_90, (2, 3), [2, 5, 1, 4, 0, 3]),
            (T::_180, (3, 2), [5, 4, 3, 2, 1, 0]),
            (T::_270, (2, 3), [3, 0, 4, 1, 5, 2]),
            (T::Flipped, (3, 2), [2, 1, 0, 5, 4, 3]),
            (T::Flipped90, (2, 3), [0, 3, 1, 4, 2, 5]),
            (T::Flipped180, (3, 2), [3, 4, 5, 0, 1, 2]),
            (T::Flipped270, (2, 3), [5, 2, 4, 1, 3, 0]),
        ];
        for (t, (w, h), buffer) in cases {
            let src: Vec<u8> = buffer.iter().flat_map(|&v| [v, v, v, 255]).collect();
            let (out, uw, uh) = upright(src, w, h, t);
            assert_eq!((uw, uh), (3, 2), "{t:?}");
            let got: Vec<u8> = out.chunks(4).map(|p| p[0]).collect();
            assert_eq!(got, upright_px, "{t:?}");
        }
    }

    #[test]
    fn taps_resolve_to_toplevel_identifiers() {
        let tap = capture_window("w-resolve", (10, 10), |_| {});
        let none = capture_window("w-unknown", (0, 0), |_| {});
        let w = wants([("w-resolve", Some("ident-a")), ("w-other", Some("ident-b"))]);
        let ours: Vec<_> = w.iter().filter(|w| w.identifier == "ident-a").collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].max, (10, 10));
        assert!(!w.iter().any(|w| w.tap == none.id), "a window not shown");
        let id = tap.id;
        drop(tap);
        assert!(
            !wants([("w-resolve", Some("ident-a"))])
                .iter()
                .any(|w| w.tap == id)
        );
    }
}

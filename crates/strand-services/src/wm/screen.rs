//! (services-round) Screen thumbnails: `thumbnail screen` (design.md:
//! "`thumbnail w` via ext-image-copy-capture", a screen as its source).
//!
//! A [`ScreenShot`] ([`capture_screen`]) takes one frame of a monitor, by
//! its connector name (`Screen.name`), scaled down to cover a size: the
//! binary takes one each time a `thumbnail` of a screen is shown, so a
//! shown screen thumbnail is a still of the moment it was shown (a
//! thumbnail of the screen it is on would otherwise show itself, again
//! and again). It runs on a `strand-screenshot` thread with a Wayland
//! connection of its own to the display `wm::configure` names (else
//! `$WAYLAND_DISPLAY`): the compositor service need not run, and a
//! screen thumbnail costs nothing once its frame is taken. It copies
//! the frame
//!
//! - through `ext-image-copy-capture-v1` with an
//!   `ext_output_image_capture_source_manager_v1` source when the
//!   compositor offers both (sway 1.10, Hyprland, niri, COSMIC), the
//!   pointer left out (no `paint_cursors`);
//! - else through `wlr-screencopy-unstable-v1` (sway 1.9, labwc, older
//!   Hyprland), without the cursor overlay, its `y_invert` flag turned
//!   upright.
//!
//! Either way the frame is read from a memfd shm buffer in ARGB8888 or
//! XRGB8888 (the first the compositor offers of the two), scaled down
//! ([`super::capture::downscale`]) and turned upright by the output's
//! transform ([`super::capture::upright`]). Its function is called once,
//! with `None` when the monitor is not there, the compositor offers
//! neither protocol, refuses or takes longer than [`SHOT_WAIT`].
//! Dropping the shot before then cancels it: its function is not called.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::WaylandTarget;
use super::capture::{CaptureFrame, downscale, upright};

/// How long a shot may take, from the connection to the frame, before it
/// is given up (its function gets `None`).
pub const SHOT_WAIT: Duration = Duration::from_secs(3);

/// The largest buffer a shot takes (a side), whatever the compositor
/// asks.
const MAX_SIDE: u32 = 16384;

/// A screen being captured: dropping it cancels the shot.
#[derive(Debug)]
pub struct ScreenShot {
    cancelled: Arc<AtomicBool>,
}

impl Drop for ScreenShot {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Takes one frame of the monitor named `connector` (`Screen.name`,
/// such as `DP-1`), scaled down to cover `max` (physical pixels; 0 is no
/// limit), and calls `f` with it on the `strand-screenshot` thread (or
/// with `None`: see the module docs). Not called once the shot is
/// dropped.
pub fn capture_screen(
    connector: &str,
    max: (u32, u32),
    f: impl FnOnce(Option<CaptureFrame>) + Send + 'static,
) -> ScreenShot {
    let cancelled = Arc::new(AtomicBool::new(false));
    let c = cancelled.clone();
    let connector = connector.to_string();
    let target = super::service::wayland_target();
    let spawned = std::thread::Builder::new()
        .name("strand-screenshot".into())
        .spawn(move || {
            let frame = match shoot(&target, &connector, max) {
                Ok(frame) => Some(frame),
                Err(e) => {
                    log::info!("screen {connector}: no frame: {e}");
                    None
                }
            };
            if !c.load(Ordering::SeqCst) {
                f(frame);
            }
        });
    if let Err(e) = spawned {
        log::warn!("screen shot: no thread: {e}");
    }
    ScreenShot { cancelled }
}

/// What the shot's connection holds.
#[derive(Default)]
struct Shot {
    shm: Option<wl_shm::WlShm>,
    output_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    copy: Option<ExtImageCopyCaptureManagerV1>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    /// Every output, with its name once told.
    outputs: Vec<(wl_output::WlOutput, Option<String>, wl_output::Transform)>,
    /// The buffer the compositor asks for: size, formats, stride
    /// (screencopy's), and that the list is complete.
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    stride: Option<u32>,
    constraints_done: bool,
    /// The frame's transform (ext) or `y_invert` (screencopy).
    transform: Option<wl_output::Transform>,
    y_invert: bool,
    ready: bool,
    failed: Option<String>,
}

/// The format to read in: ARGB8888 or XRGB8888, the first offered.
fn pick_format(formats: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    formats
        .iter()
        .copied()
        .find(|f| matches!(f, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888))
}

/// Dispatches until `done` holds, failure, or the deadline.
fn until(
    conn: &Connection,
    queue: &mut EventQueue<Shot>,
    shot: &mut Shot,
    deadline: Instant,
    done: impl Fn(&Shot) -> bool,
) -> Result<(), String> {
    loop {
        queue
            .dispatch_pending(shot)
            .map_err(|e| format!("the connection failed: {e}"))?;
        if let Some(why) = &shot.failed {
            return Err(why.clone());
        }
        if done(shot) {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no frame within {SHOT_WAIT:?}"));
        }
        conn.flush()
            .map_err(|e| format!("the connection failed: {e}"))?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let fd = guard.connection_fd();
        let mut pfd = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let ts = rustix::event::Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        match rustix::event::poll(&mut pfd, Some(&ts)) {
            Ok(0) => drop(guard),
            Ok(_) => {
                guard
                    .read()
                    .map_err(|e| format!("the connection failed: {e}"))?;
            }
            Err(rustix::io::Errno::INTR) => drop(guard),
            Err(e) => return Err(format!("poll: {e}")),
        }
    }
}

/// A memfd-backed shm buffer.
struct Buffer {
    fd: OwnedFd,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    len: usize,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

fn buffer(
    shm: &wl_shm::WlShm,
    (w, h): (u32, u32),
    stride: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<Shot>,
) -> Result<Buffer, String> {
    let len = (stride as usize) * (h as usize);
    let size = i32::try_from(len).map_err(|_| "the frame is too large".to_string())?;
    let fd = rustix::fs::memfd_create("strand-screenshot", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| format!("memfd: {e}"))?;
    rustix::fs::ftruncate(&fd, len as u64).map_err(|e| format!("memfd: {e}"))?;
    let pool = shm.create_pool(fd.as_fd(), size, qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
    Ok(Buffer {
        fd,
        pool,
        buffer,
        len,
    })
}

/// The buffer's bytes.
fn read(b: &Buffer) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; b.len];
    let mut at = 0;
    while at < out.len() {
        match rustix::io::pread(&b.fd, &mut out[at..], at as u64) {
            Ok(0) => break,
            Ok(n) => at += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(format!("reading the frame: {e}")),
        }
    }
    Ok(out)
}

/// `rows` (`h` rows of `stride` bytes) upside down.
fn flip_rows(rows: &mut [u8], stride: usize, h: usize) {
    for y in 0..h / 2 {
        let (top, bottom) = rows.split_at_mut((h - 1 - y) * stride);
        top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
    }
}

fn socket(target: &WaylandTarget) -> Result<std::path::PathBuf, String> {
    match target {
        WaylandTarget::Socket(p) => Ok(p.clone()),
        WaylandTarget::Env => super::protocol::display_socket(
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )
        .map_err(|e| e.to_string()),
    }
}

/// One frame of `connector`.
fn shoot(target: &WaylandTarget, connector: &str, max: (u32, u32)) -> Result<CaptureFrame, String> {
    let deadline = Instant::now() + SHOT_WAIT;
    let path = socket(target)?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let mut queue: EventQueue<Shot> = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());
    let mut shot = Shot::default();
    // The globals, then the outputs' names.
    queue.roundtrip(&mut shot).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut shot).map_err(|e| e.to_string())?;
    let (output, transform) = shot
        .outputs
        .iter()
        .find(|(_, name, _)| name.as_deref() == Some(connector))
        .map(|(o, _, t)| (o.clone(), *t))
        .ok_or_else(|| format!("no monitor named {connector}"))?;
    let shm = shot.shm.clone().ok_or("the compositor offers no wl_shm")?;
    let (pixels, (w, h), stride, opaque, turn) =
        if let (Some(sources), Some(copy)) = (shot.output_sources.clone(), shot.copy.clone()) {
            let source = sources.create_source(&output, &qh, ());
            let session = copy.create_session(&source, Options::empty(), &qh, ());
            let r = (|| {
                until(&conn, &mut queue, &mut shot, deadline, |s| {
                    s.constraints_done
                })?;
                let size = shot.size.ok_or("no buffer size")?;
                let format = pick_format(&shot.formats).ok_or("no ARGB8888 or XRGB8888")?;
                if size.0 == 0 || size.1 == 0 || size.0 > MAX_SIDE || size.1 > MAX_SIDE {
                    return Err(format!("a buffer of {size:?}"));
                }
                let b = buffer(&shm, size, size.0 * 4, format, &qh)?;
                let frame = session.create_frame(&qh, ());
                frame.attach_buffer(&b.buffer);
                frame.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
                frame.capture();
                let r = until(&conn, &mut queue, &mut shot, deadline, |s| s.ready);
                frame.destroy();
                r?;
                let bytes = read(&b)?;
                Ok((
                    bytes,
                    size,
                    size.0 * 4,
                    format == wl_shm::Format::Xrgb8888,
                    shot.transform.unwrap_or(wl_output::Transform::Normal),
                ))
            })();
            session.destroy();
            source.destroy();
            r?
        } else if let Some(manager) = shot.screencopy.clone() {
            let frame = manager.capture_output(0, &output, &qh, ());
            let r = (|| {
                // Version 3 ends the list with `buffer_done`; before, the
                // one `buffer` event is all.
                let v3 = frame.version() >= 3;
                until(&conn, &mut queue, &mut shot, deadline, |s| {
                    if v3 {
                        s.constraints_done
                    } else {
                        s.size.is_some()
                    }
                })?;
                let size = shot.size.ok_or("no buffer size")?;
                let format = pick_format(&shot.formats).ok_or("no ARGB8888 or XRGB8888")?;
                let stride = shot.stride.unwrap_or(size.0 * 4).max(size.0 * 4);
                if size.0 == 0 || size.1 == 0 || size.0 > MAX_SIDE || size.1 > MAX_SIDE {
                    return Err(format!("a buffer of {size:?}"));
                }
                let b = buffer(&shm, size, stride, format, &qh)?;
                frame.copy(&b.buffer);
                until(&conn, &mut queue, &mut shot, deadline, |s| s.ready)?;
                let mut bytes = read(&b)?;
                if shot.y_invert {
                    flip_rows(&mut bytes, stride as usize, size.1 as usize);
                }
                Ok((
                    bytes,
                    size,
                    stride,
                    format == wl_shm::Format::Xrgb8888,
                    transform,
                ))
            })();
            frame.destroy();
            r?
        } else {
            return Err(
                "the compositor offers neither ext-image-copy-capture nor wlr-screencopy".into(),
            );
        };
    let _ = conn.flush();
    let turned = matches!(
        turn,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    );
    let raw_max = if turned { (max.1, max.0) } else { max };
    let small = downscale(&pixels, w, h, stride, opaque, raw_max);
    if turn == wl_output::Transform::Normal {
        return Ok(small);
    }
    let (pixels, width, height) = upright(small.pixels.to_vec(), small.width, small.height, turn);
    Ok(CaptureFrame {
        width,
        height,
        pixels: pixels.into(),
    })
}

impl Dispatch<wl_registry::WlRegistry, ()> for Shot {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        if interface == wl_shm::WlShm::interface().name && state.shm.is_none() {
            state.shm = Some(registry.bind(name, version.min(1), qh, ()));
        } else if interface == wl_output::WlOutput::interface().name && version >= 4 {
            let o = registry.bind(name, version.min(4), qh, ());
            state.outputs.push((o, None, wl_output::Transform::Normal));
        } else if interface == ExtOutputImageCaptureSourceManagerV1::interface().name {
            state.output_sources = Some(registry.bind(name, version.min(1), qh, ()));
        } else if interface == ExtImageCopyCaptureManagerV1::interface().name {
            state.copy = Some(registry.bind(name, version.min(1), qh, ()));
        } else if interface == ZwlrScreencopyManagerV1::interface().name {
            state.screencopy = Some(registry.bind(name, version.min(3), qh, ()));
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Shot {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.iter_mut().find(|(o, ..)| o == output) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.1 = Some(name),
            wl_output::Event::Geometry {
                transform: WEnum::Value(t),
                ..
            } => entry.2 = t,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Shot {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => state.size = Some((width, height)),
            Event::ShmFormat {
                format: WEnum::Value(f),
            } => state.formats.push(f),
            Event::Done => state.constraints_done = true,
            Event::Stopped => {
                state.failed = Some("the compositor stopped the capture".into());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Shot {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Transform {
                transform: WEnum::Value(t),
            } => state.transform = Some(t),
            Event::Ready => state.ready = true,
            Event::Failed { reason } => {
                state.failed = Some(format!("the frame failed: {reason:?}"));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Shot {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(f),
                width,
                height,
                stride,
            } => {
                state.formats.push(f);
                if pick_format(&[f]).is_some() || state.size.is_none() {
                    state.size = Some((width, height));
                    state.stride = Some(stride);
                }
            }
            Event::BufferDone => state.constraints_done = true,
            Event::Flags {
                flags: WEnum::Value(f),
            } => {
                state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            Event::Ready { .. } => state.ready = true,
            Event::Failed => state.failed = Some("the compositor refused the copy".into()),
            _ => {}
        }
    }
}

/// Requests and events with nothing to do.
macro_rules! ignore {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for Shot {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
    ZwlrScreencopyManagerV1
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_flip_upside_down() {
        let mut rows = vec![1u8, 1, 2, 2, 3, 3];
        flip_rows(&mut rows, 2, 3);
        assert_eq!(rows, [3, 3, 2, 2, 1, 1]);
        let mut even = vec![1u8, 2, 3, 4];
        flip_rows(&mut even, 1, 4);
        assert_eq!(even, [4, 3, 2, 1]);
    }

    #[test]
    fn only_formats_read_as_bgra_are_picked() {
        use wl_shm::Format as F;
        assert_eq!(pick_format(&[F::Rgb565, F::Xrgb8888]), Some(F::Xrgb8888));
        assert_eq!(pick_format(&[F::Argb8888, F::Xrgb8888]), Some(F::Argb8888));
        assert_eq!(pick_format(&[F::Abgr8888]), None);
    }
}

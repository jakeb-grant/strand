//! The capture side of the fake: `ext_foreign_toplevel_image_capture_source_manager_v1`
//! and `ext_image_copy_capture_manager_v1` (sway 1.9 in CI offers
//! neither), for `strand-services`' thumbnail sessions.
//!
//! Every toplevel has a size (64 × 48 until [`crate::Cmd::ResizeToplevel`])
//! and a colour ([`crate::Cmd::Paint`]); a session offers its size in
//! ARGB8888 and XRGB8888, and a frame is filled with the colour, as
//! premultiplied ARGB8888 in the client's shm buffer. Like a real
//! compositor, a session's first frame is ready at once and a later one
//! only once its toplevel changed (a paint). A frame whose buffer does
//! not match the size fails with `buffer_constraints`; a resize sends the
//! new constraints and fails the frame in flight; a closed toplevel stops
//! its sessions. What happens is logged in [`crate::Fake::captures`]:
//! `session <ident>`, `frame <ident>`, `failed <ident>`, `stopped
//! <ident>`, `end <ident>` (the client destroyed its session).

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use wayland_protocols::ext::foreign_toplevel_list::v1::server::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::image_capture_source::v1::server::{
    ext_foreign_toplevel_image_capture_source_manager_v1::{
        self, ExtForeignToplevelImageCaptureSourceManagerV1,
    },
    ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
};
use wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_server::backend::{ClientId, ObjectId};
use wayland_server::protocol::{wl_buffer, wl_shm};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::Server;

/// A toplevel's size until resized.
pub const DEFAULT_SIZE: (u32, u32) = (64, 48);

/// A client's shm buffer, as its pool's fd and layout.
#[derive(Debug)]
pub(crate) struct ShmBuf {
    pub(crate) fd: Arc<OwnedFd>,
    pub(crate) offset: i32,
    pub(crate) stride: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// One session.
struct Session {
    ident: String,
    session: ExtImageCopyCaptureSessionV1,
    /// The toplevel changed since the last frame (true at first).
    damaged: bool,
    /// A frame captured before a change: ready at the next one.
    waiting: Option<(ExtImageCopyCaptureFrameV1, wl_buffer::WlBuffer)>,
}

/// The capture state.
#[derive(Default)]
pub(crate) struct Capture {
    sessions: Vec<Session>,
    sizes: HashMap<String, (u32, u32)>,
    colours: HashMap<String, [u8; 4]>,
    pub(crate) log: Arc<Mutex<Vec<String>>>,
}

impl Capture {
    fn note(&self, line: String) {
        if let Ok(mut l) = self.log.lock() {
            l.push(line);
        }
    }

    fn size(&self, ident: &str) -> (u32, u32) {
        self.sizes.get(ident).copied().unwrap_or(DEFAULT_SIZE)
    }

    fn send_constraints(&self, session: &ExtImageCopyCaptureSessionV1, ident: &str) {
        let (w, h) = self.size(ident);
        session.buffer_size(w, h);
        session.shm_format(wl_shm::Format::Argb8888);
        session.shm_format(wl_shm::Format::Xrgb8888);
        session.done();
    }
}

impl Server {
    /// `ident` drew in `colour`: its sessions' waiting frames are ready.
    pub(crate) fn capture_paint(&mut self, ident: &str, colour: [u8; 4]) {
        self.capture.colours.insert(ident.to_string(), colour);
        let mut ready = Vec::new();
        for s in &mut self.capture.sessions {
            if s.ident == ident {
                s.damaged = true;
                if let Some(w) = s.waiting.take() {
                    ready.push(w);
                }
            }
        }
        let shm = std::mem::take(&mut self.shm_buffers);
        for (frame, buffer) in ready {
            self.capture_fill(&shm, ident, &frame, &buffer);
        }
        self.shm_buffers = shm;
    }

    /// `ident` resized: new constraints, and the frame in flight fails.
    pub(crate) fn capture_resize(&mut self, ident: &str, w: u32, h: u32) {
        self.capture.sizes.insert(ident.to_string(), (w, h));
        for s in &mut self.capture.sessions {
            if s.ident == ident {
                if let Some((f, _)) = s.waiting.take() {
                    f.failed(FailureReason::BufferConstraints);
                }
                s.damaged = true;
            }
        }
        for s in &self.capture.sessions {
            if s.ident == ident {
                self.capture.send_constraints(&s.session, ident);
            }
        }
    }

    /// `ident` closed: its sessions stop.
    pub(crate) fn capture_closed(&mut self, ident: &str) {
        let mut stopped = 0;
        for s in &mut self.capture.sessions {
            if s.ident == ident {
                if let Some((f, _)) = s.waiting.take() {
                    f.failed(FailureReason::Stopped);
                }
                s.session.stopped();
                stopped += 1;
            }
        }
        for _ in 0..stopped {
            self.capture.note(format!("stopped {ident}"));
        }
    }

    /// Fills `buffer` with `ident`'s colour and readies `frame`, or fails
    /// it when the buffer does not match.
    fn capture_fill(
        &mut self,
        shm: &HashMap<ObjectId, ShmBuf>,
        ident: &str,
        frame: &ExtImageCopyCaptureFrameV1,
        buffer: &wl_buffer::WlBuffer,
    ) {
        let (w, h) = self.capture.size(ident);
        let Some(b) = shm
            .get(&buffer.id())
            .filter(|b| (b.width, b.height) == (w as i32, h as i32) && b.stride >= w as i32 * 4)
        else {
            frame.failed(FailureReason::BufferConstraints);
            self.capture.note(format!("failed {ident}"));
            return;
        };
        // Premultiplied ARGB8888 in memory: B, G, R, A.
        let [r, g, bl, a] = self
            .capture
            .colours
            .get(ident)
            .copied()
            .unwrap_or([128, 128, 128, 255]);
        let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
        let px = [m(bl), m(g), m(r), a];
        let row: Vec<u8> = px.iter().copied().cycle().take(w as usize * 4).collect();
        for y in 0..h as i64 {
            let at = b.offset as i64 + y * b.stride as i64;
            let _ = rustix::io::pwrite(b.fd.as_fd(), &row, at as u64);
        }
        frame.damage(0, 0, w as i32, h as i32);
        frame.presentation_time(0, 0, 0);
        frame.ready();
        for s in &mut self.capture.sessions {
            if s.ident == ident {
                s.damaged = false;
            }
        }
        self.capture.note(format!("frame {ident}"));
    }
}

impl GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource {
            source,
            toplevel_handle,
        } = request
        {
            let ident = ident_of(state, &toplevel_handle);
            init.init(source, ident);
        }
    }
}

fn ident_of(state: &Server, handle: &ExtForeignToplevelHandleV1) -> String {
    state
        .toplevels
        .iter()
        .find(|t| t.handles.contains(handle))
        .map(|t| t.ident.clone())
        .unwrap_or_default()
}

impl Dispatch<ExtImageCaptureSourceV1, String> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCaptureSourceV1,
        _: ext_image_capture_source_v1::Request,
        _: &String,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<ExtImageCopyCaptureManagerV1, ()> for Server {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtImageCopyCaptureManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_manager_v1::Request::CreateSession {
            session, source, ..
        } = request
        {
            let ident = source.data::<String>().cloned().unwrap_or_default();
            let session = init.init(session, ident.clone());
            state.capture.note(format!("session {ident}"));
            if state.toplevels.iter().any(|t| t.ident == ident) {
                state.capture.send_constraints(&session, &ident);
            } else {
                session.stopped();
                state.capture.note(format!("stopped {ident}"));
            }
            state.capture.sessions.push(Session {
                ident,
                session,
                damaged: true,
                waiting: None,
            });
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, String> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        ident: &String,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = request {
            init.init(
                frame,
                FrameData {
                    ident: ident.clone(),
                    buffer: Mutex::new(None),
                },
            );
        }
    }

    fn destroyed(
        state: &mut Self,
        _: ClientId,
        session: &ExtImageCopyCaptureSessionV1,
        ident: &String,
    ) {
        state.capture.sessions.retain(|s| &s.session != session);
        state.capture.note(format!("end {ident}"));
    }
}

/// A frame's toplevel and attached buffer.
pub(crate) struct FrameData {
    ident: String,
    buffer: Mutex<Option<wl_buffer::WlBuffer>>,
}

impl Dispatch<ExtImageCopyCaptureFrameV1, FrameData> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Request;
        match request {
            Request::AttachBuffer { buffer } => {
                if let Ok(mut b) = data.buffer.lock() {
                    *b = Some(buffer);
                }
            }
            Request::Capture => {
                let Some(buffer) = data.buffer.lock().ok().and_then(|b| b.clone()) else {
                    return;
                };
                let ident = data.ident.clone();
                let damaged = state
                    .capture
                    .sessions
                    .iter()
                    .any(|s| s.ident == ident && s.damaged);
                if !state.toplevels.iter().any(|t| t.ident == ident) {
                    frame.failed(FailureReason::Stopped);
                } else if damaged {
                    let shm = std::mem::take(&mut state.shm_buffers);
                    state.capture_fill(&shm, &ident, frame, &buffer);
                    state.shm_buffers = shm;
                } else if let Some(s) = state
                    .capture
                    .sessions
                    .iter_mut()
                    .find(|s| s.ident == ident && s.waiting.is_none())
                {
                    s.waiting = Some((frame.clone(), buffer));
                }
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _: ClientId, frame: &ExtImageCopyCaptureFrameV1, _: &FrameData) {
        for s in &mut state.capture.sessions {
            if s.waiting.as_ref().is_some_and(|(f, _)| f == frame) {
                s.waiting = None;
            }
        }
    }
}

/// Creates the capture globals (and `wl_shm` when no surface globals do).
pub(crate) fn create_globals(dh: &DisplayHandle, shm: bool) {
    dh.create_global::<Server, ExtForeignToplevelImageCaptureSourceManagerV1, ()>(1, ());
    dh.create_global::<Server, ExtImageCopyCaptureManagerV1, ()>(1, ());
    if shm {
        dh.create_global::<Server, wl_shm::WlShm, ()>(1, ());
    }
}

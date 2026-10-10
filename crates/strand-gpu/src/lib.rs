//! The GPU backend (docs/architecture.md, "`strand-gpu`").
//!
//! One thread, `strand-gpu`, owns one wgpu device for the whole process,
//! started on the first demand and ending with the device. vello_cpu into
//! `wl_shm` stays the default for every surface; the GPU draws a surface
//! only while it animates a large area (render's promotion), and shader
//! passes. Nothing here runs, and no Vulkan library is mapped, until
//! [`Gpu::spawn`] is called.
//!
//! - [`Gpu`]: the handle the binary owns. [`Gpu::send`] never blocks;
//!   [`Gpu::try_recv`] is drained when the waker's ping fires on the main
//!   loop. Dropping it (or [`GpuRequest::Shutdown`]) drops every surface,
//!   pipeline and texture, the device and the instance, and the thread
//!   ends after its [`GpuReply::Exited`].
//! - [`Frame`]: what render lowers a display list into (fills, strokes as
//!   fills, images by upload id, clips, layers, shader passes); the GPU
//!   thread builds the vello_gpu scene from it.
//! - [`Readback`]: a frame or pass read back as `wl_shm` ARGB8888 rows.
//!
//! Colours are straight sRGB as the scene has them (not the CPU raster's
//! red–blue swap); uploads are the CPU's pixmaps, which carry that swap,
//! and are swapped back when they reach the GPU. Frames are rendered into
//! a `Bgra8Unorm` texture, whose bytes are the `wl_shm` order, so a
//! readback is a copy.
//!
//! A software adapter (lavapipe) counts as no device unless
//! [`GpuOptions::software`] (`STRAND_GPU_SOFTWARE=1`).

use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

pub use strand_scene::{AdapterInfo, Scale, ShaderPass, Size, SurfaceId};
pub use vello_common::color::{AlphaColor, Srgb};
pub use vello_common::kurbo;
pub use vello_common::peniko;
pub use vello_common::pixmap::Pixmap;

mod bundled;
mod device;
mod draw;
mod pass;
mod present;
mod readback;
mod thread;

/// How the GPU is started.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuOptions {
    /// Accept a software adapter (`DeviceType::Cpu`, lavapipe). Off for
    /// users: it would draw on the CPU and keep about 80 MiB mapped after
    /// the device drops. The lavapipe test tier sets it.
    pub software: bool,
}

impl GpuOptions {
    /// The environment variable that accepts a software adapter.
    pub const SOFTWARE_ENV: &'static str = "STRAND_GPU_SOFTWARE";

    /// Options from the environment (`STRAND_GPU_SOFTWARE=1`).
    pub fn from_env() -> Self {
        Self {
            software: std::env::var(Self::SOFTWARE_ENV).is_ok_and(|v| v == "1"),
        }
    }
}

/// A surface's Wayland handles for the WSI: the connection's `wl_display`
/// and the surface's `wl_surface`, as `raw-window-handle` handles
/// (`State::raw_handles` in strand-surface).
#[derive(Debug)]
pub struct RawHandles {
    pub display: raw_window_handle::RawDisplayHandle,
    pub window: raw_window_handle::RawWindowHandle,
}

// SAFETY: the handles are pointers to libwayland-client objects, which
// are thread-safe (libwayland locks the display); the surface manager
// keeps the `wl_surface` and the connection alive until the GPU thread
// has answered `Release` (docs/architecture.md, "Surface hand-off"),
// so the pointers outlive every use on the GPU thread.
unsafe impl Send for RawHandles {}

/// What the GPU thread is asked to do. Sending never blocks.
#[derive(Debug)]
pub enum GpuRequest {
    /// Draw `surface` from now on. With `handles` the thread tries the
    /// WSI first and answers [`GpuMode::Present`] if it can present
    /// (premultiplied or opaque alpha, a non-sRGB `Bgra8Unorm` or
    /// `Rgba8Unorm` format); else, and without handles, it reads frames
    /// back ([`GpuMode::Readback`]).
    Attach {
        surface: SurfaceId,
        handles: Option<RawHandles>,
        size: Size,
        scale: Scale,
        opaque: bool,
    },
    /// The surface's buffer size or scale changed; the next frame is at
    /// that size.
    Resize {
        surface: SurfaceId,
        size: Size,
        scale: Scale,
    },
    /// Stop drawing `surface`: its swapchain (if any) is dropped before
    /// the answer, [`GpuReply::Released`].
    Release(SurfaceId),
    /// One frame of an attached surface.
    Frame(Frame),
    /// A shader pass on a surface the GPU does not draw: drawn offscreen
    /// at `size` and read back ([`GpuReply::PassPixels`]).
    Pass(PassFrame),
    /// Drop everything and end the thread ([`GpuReply::Exited`]).
    Shutdown,
}

/// How an attached surface is drawn.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GpuMode {
    /// Presented through wgpu's WSI: the GPU thread commits the surface.
    Present,
    /// Rendered offscreen and read back ([`GpuReply::Pixels`]); the main
    /// thread commits it as a CPU frame with full damage.
    Readback,
}

/// What the GPU thread answers.
#[derive(Clone, Debug)]
pub enum GpuReply {
    /// The device is up.
    Ready(AdapterInfo),
    /// No device: no Vulkan adapter, a software one without
    /// [`GpuOptions::software`], or the device failed to start. The
    /// thread then ends ([`GpuReply::Exited`] follows).
    Unavailable(GpuError),
    /// `surface` is attached and drawn in `mode`.
    Attached { surface: SurfaceId, mode: GpuMode },
    /// `surface`'s swapchain is gone; the manager may commit it again.
    Released(SurfaceId),
    /// A presented frame went to the compositor at `at`: the surface's
    /// frame callback.
    Presented { surface: SurfaceId, at: Instant },
    /// A readback frame's pixels.
    Pixels {
        surface: SurfaceId,
        frame: u64,
        pixels: Readback,
    },
    /// A pass's pixels.
    PassPixels {
        key: u64,
        frame: u64,
        pixels: Readback,
    },
    /// A frame or pass failed (an invalid shader, a vello error); the
    /// device is still up. `surface` is the frame's, `None` for a pass.
    Failed {
        surface: Option<SurfaceId>,
        key: Option<u64>,
        error: GpuError,
    },
    /// The device was lost; the thread ends ([`GpuReply::Exited`]
    /// follows).
    Lost(GpuError),
    /// The thread is ending: everything it held is dropped.
    Exited,
}

/// Why the GPU could not do something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuError {
    pub kind: GpuErrorKind,
    pub message: String,
}

/// The kind of a [`GpuError`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GpuErrorKind {
    /// No Vulkan adapter.
    NoAdapter,
    /// Only a software adapter, and software was not accepted.
    Software,
    /// The device could not be created.
    Device,
    /// The device was lost (a driver reset, a panic on the GPU thread).
    Lost,
    /// A frame or pass failed to render.
    Render,
    /// A shader failed to compile on the device.
    Shader,
}

impl GpuError {
    pub(crate) fn new(kind: GpuErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GpuError {}

/// One frame of an attached surface, in physical pixels from the
/// surface's origin (what render lowers its display list into).
#[derive(Clone, Debug)]
pub struct Frame {
    pub surface: SurfaceId,
    /// Render's frame number (echoed in [`GpuReply::Pixels`]).
    pub id: u64,
    pub size: Size,
    pub scale: Scale,
    pub ops: Vec<Op>,
    /// Pixmaps the ops draw that the GPU does not have yet (or has at an
    /// older generation).
    pub uploads: Vec<Upload>,
    /// Uploads render no longer draws: the GPU frees them before this
    /// frame's uploads.
    pub retire: Vec<u64>,
    /// Background the frame is cleared to (transparent for a
    /// translucent surface).
    pub clear: AlphaColor<Srgb>,
}

/// A shader pass drawn offscreen for a surface the GPU does not draw
/// (docs/architecture.md: "a pass is drawn offscreen by the GPU at its
/// bounds and read back into the CPU frame as a raster item").
#[derive(Clone, Debug)]
pub struct PassFrame {
    /// Render's key for the pass (its node).
    pub key: u64,
    /// Render's frame number (echoed in [`GpuReply::PassPixels`]).
    pub id: u64,
    /// The pass's box in buffer pixels.
    pub size: Size,
    pub pass: ShaderPass,
    /// Passes run after `pass`, each reading the one before's pixels
    /// (a `filter:` list of several bundled passes).
    pub then: Vec<ShaderPass>,
    pub globals: PassGlobals,
    /// What the pass reads as `strand_input`, `size` pixels in the CPU
    /// raster's byte order (`Bgra8Unorm`, premultiplied): a `filter:`
    /// pass's subtree, a backdrop pass's backdrop; `None` for 1×1
    /// transparent.
    pub input: Option<Arc<Pixmap>>,
}

/// What `@group(0)`'s `strand` uniform holds for a pass.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct PassGlobals {
    /// Seconds since the node appeared (0 under `reduced_motion`).
    pub time: f32,
    pub scale: f32,
    /// The pointer in buffer pixels relative to the box, or -1, -1.
    pub pointer: [f32; 2],
}

/// A pixmap the GPU keeps (images, atlas pages, gradients, shadows,
/// masks and CPU-drawn groups), keyed by `id`; a new `generation`
/// replaces the old pixels. It lives on the GPU until a frame retires it
/// ([`Frame::retire`]) or the device drops.
#[derive(Clone, Debug)]
pub struct Upload {
    pub id: u64,
    pub generation: u64,
    /// Premultiplied pixels with red and blue swapped (the CPU raster's
    /// order); swapped back when uploaded.
    pub pixmap: Arc<Pixmap>,
}

/// A paint.
#[derive(Clone, Debug, PartialEq)]
pub enum Brush {
    Solid(AlphaColor<Srgb>),
    Gradient(peniko::Gradient),
}

/// A layer: its group composited with `blend` and `opacity`, clipped to
/// `clip`, blurred by `blur` (a standard deviation in buffer pixels).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    pub clip: Option<kurbo::BezPath>,
    pub blend: Option<peniko::BlendMode>,
    pub opacity: Option<f32>,
    pub blur: Option<f32>,
}

/// One drawing command. Coordinates are buffer pixels under the current
/// transform ([`Op::Transform`], identity at the start).
#[derive(Clone, Debug)]
pub enum Op {
    /// Sets the transform for what follows (clips and layers take the one
    /// in force when they are pushed).
    Transform(kurbo::Affine),
    PushClip(kurbo::BezPath),
    /// A clip filled even-odd (a shadow's ring outside its box); popped
    /// by [`Op::PopClip`].
    PushClipEvenOdd(kurbo::BezPath),
    PopClip,
    PushLayer(Layer),
    PopLayer,
    /// Fills `path` with `brush` (its geometry under `brush_transform`,
    /// relative to the current transform).
    Fill {
        path: kurbo::BezPath,
        brush: Brush,
        brush_transform: kurbo::Affine,
        even_odd: bool,
    },
    /// Fills `rect` with upload `image`, placed by `image_transform`
    /// (image pixels to user space); `tint` paints its alpha in a colour
    /// (glyphs, symbolic icons); `smooth` samples bilinearly.
    Image {
        rect: kurbo::Rect,
        image: u64,
        image_transform: kurbo::Affine,
        tint: Option<AlphaColor<Srgb>>,
        smooth: bool,
    },
    /// A shader pass drawn over `bounds` (buffer pixels, under the
    /// current transform).
    Pass {
        pass: ShaderPass,
        bounds: kurbo::Rect,
        globals: PassGlobals,
    },
}

/// Pixels read back: premultiplied `Bgra8Unorm` rows (the `wl_shm`
/// ARGB8888 byte order), each `stride` bytes (wgpu pads rows to 256).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Readback {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bytes: Vec<u8>,
}

impl Readback {
    /// Row `y`'s `width * 4` bytes.
    pub fn row(&self, y: u32) -> &[u8] {
        let start = (y * self.stride) as usize;
        let end = start + self.width as usize * 4;
        self.bytes.get(start..end).unwrap_or(&[])
    }

    /// Copies the pixels into `dst` (rows `dst_stride` bytes apart) at
    /// `(x, y)`, clipped to `dst_size`.
    pub fn copy_into(&self, dst: &mut [u8], dst_stride: usize, dst_size: Size, x: i64, y: i64) {
        for row in 0..self.height as i64 {
            let ty = y + row;
            if ty < 0 || ty >= dst_size.h as i64 {
                continue;
            }
            let x0 = x.max(0);
            let x1 = (x + self.width as i64).min(dst_size.w as i64);
            if x1 <= x0 {
                return;
            }
            let src = self.row(row as u32);
            let s0 = ((x0 - x) * 4) as usize;
            let s1 = ((x1 - x) * 4) as usize;
            let d0 = ty as usize * dst_stride + x0 as usize * 4;
            let d1 = d0 + (s1 - s0);
            if let (Some(d), Some(s)) = (dst.get_mut(d0..d1), src.get(s0..s1)) {
                d.copy_from_slice(s);
            }
        }
    }
}

/// The handle the binary owns (docs/architecture.md, "`strand-gpu`").
pub struct Gpu {
    tx: mpsc::Sender<GpuRequest>,
    rx: mpsc::Receiver<GpuReply>,
    thread: Option<JoinHandle<()>>,
    exited: bool,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu")
            .field("exited", &self.exited)
            .finish_non_exhaustive()
    }
}

/// The GPU thread's name.
pub const THREAD_NAME: &str = "strand-gpu";

impl Gpu {
    /// Starts the thread, which creates the instance, adapter and device
    /// off the main thread and answers [`GpuReply::Ready`] or
    /// [`GpuReply::Unavailable`]. `waker` is called after every reply.
    /// A thread that cannot be spawned answers `Unavailable` at once.
    pub fn spawn(waker: Box<dyn Fn() + Send>, opts: GpuOptions) -> Gpu {
        let (tx, req_rx) = mpsc::channel();
        let (reply_tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name(THREAD_NAME.into()).spawn({
            let reply_tx = reply_tx.clone();
            move || thread::run(req_rx, reply_tx, waker, opts)
        });
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                let _ = reply_tx.send(GpuReply::Unavailable(GpuError::new(
                    GpuErrorKind::Device,
                    format!("cannot start the GPU thread: {e}"),
                )));
                let _ = reply_tx.send(GpuReply::Exited);
                None
            }
        };
        Gpu {
            tx,
            rx,
            thread,
            exited: false,
        }
    }

    /// Queues a request; never blocks. A request after the thread ended
    /// is dropped.
    pub fn send(&self, req: GpuRequest) {
        let _ = self.tx.send(req);
    }

    /// The next reply, if one is waiting.
    pub fn try_recv(&mut self) -> Option<GpuReply> {
        let r = self.rx.try_recv().ok();
        if matches!(r, Some(GpuReply::Exited)) {
            self.exited = true;
        }
        r
    }

    /// True once the thread answered [`GpuReply::Exited`] (or could not
    /// start).
    pub fn exited(&self) -> bool {
        self.exited || self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    /// Asks the thread to end and joins it once it has said so; returns
    /// false (without blocking) while it is still dropping the device:
    /// call again after its `Exited` reply.
    pub fn try_join(&mut self) -> bool {
        let _ = self.tx.send(GpuRequest::Shutdown);
        match &self.thread {
            Some(t) if !t.is_finished() => false,
            Some(_) => {
                if let Some(t) = self.thread.take() {
                    let _ = t.join();
                }
                true
            }
            None => true,
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // The thread drops everything and ends on its own; it is detached
        // rather than joined so a slow driver never blocks the main
        // thread (the binary joins it with `try_join` when it can wait).
        let _ = self.tx.send(GpuRequest::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_rows_copy_clipped() {
        let rb = Readback {
            width: 2,
            height: 2,
            stride: 256,
            bytes: {
                let mut b = vec![0u8; 512];
                b[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
                b[256..264].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
                b
            },
        };
        assert_eq!(rb.row(1), &[9, 10, 11, 12, 13, 14, 15, 16]);
        let mut dst = vec![0u8; 3 * 4 * 2];
        rb.copy_into(&mut dst, 12, Size::new(3, 2), 2, 1);
        // Only the top-left pixel lands, at (2, 1).
        assert_eq!(&dst[12 + 8..12 + 12], &[1, 2, 3, 4]);
        assert_eq!(dst.iter().filter(|b| **b != 0).count(), 4);
    }

    #[test]
    fn options_read_the_software_variable() {
        assert_eq!(GpuOptions::SOFTWARE_ENV, "STRAND_GPU_SOFTWARE");
        assert!(!GpuOptions::default().software);
    }
}

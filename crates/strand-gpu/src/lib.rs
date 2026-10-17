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
use std::time::{Duration, Instant};

pub use strand_scene::{AdapterInfo, Scale, ShaderPass, Size, SurfaceId};
pub use vello_common::color::{AlphaColor, Srgb};
pub use vello_common::kurbo;
pub use vello_common::peniko;
pub use vello_common::pixmap::Pixmap;

mod bundled;
mod device;
mod draw;
mod lru;
mod pass;
mod present;
mod readback;
mod thread;
mod timer;

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

/// (gpu-first review) Whether a device can exist in this process: a
/// software adapter is accepted ([`GpuOptions::software`]), or there is a
/// DRM render node (`/dev/dri/renderD*`) for a hardware driver to open.
/// With neither, nothing need be probed: the binary does not warm a
/// device, so no Vulkan instance is made and no driver is mapped.
pub fn device_possible(opts: &GpuOptions) -> bool {
    opts.software || render_node_in(std::path::Path::new("/dev/dri"))
}

/// Whether `dir` holds a DRM render node (`renderD*`).
fn render_node_in(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("renderD"))
        })
    })
}

/// The Vulkan loader's variable that keeps drivers out of enumeration
/// (loader 1.3.234 on): a comma-separated list of globs matched against
/// the drivers' manifest file names.
pub const LOADER_DISABLE_ENV: &str = "VK_LOADER_DRIVERS_DISABLE";

/// The variables with which a person picks the Vulkan drivers
/// themselves: strand then leaves the loader as they set it.
pub const LOADER_CHOICE_ENVS: [&str; 3] = [
    "VK_DRIVER_FILES",
    "VK_ICD_FILENAMES",
    "VK_LOADER_DRIVERS_SELECT",
];

/// The software drivers' manifests: Mesa's lavapipe (`lvp_icd*.json`)
/// and SwiftShader (`vk_swiftshader_icd.json`).
const SOFTWARE_DRIVERS: [&str; 2] = ["*lvp_icd*", "*swiftshader*"];

/// The hardware drivers' manifests by the PCI vendor they drive: Intel's
/// (ANV, HASVK), AMD's (RADV, which links LLVM, and AMDVLK), NVIDIA's
/// (NVK, the proprietary driver) and virtio-gpu's (Venus, gfxstream).
/// Drivers not named here are never kept out.
const VENDOR_DRIVERS: [(u16, &[&str]); 4] = [
    (0x8086, &["*intel_icd*", "*intel_hasvk_icd*"]),
    (0x1002, &["*radeon_icd*", "*amd_icd*"]),
    (0x10de, &["*nouveau_icd*", "*nvidia_icd*"]),
    (0x1af4, &["*virtio_icd*", "*gfxstream_vk_icd*"]),
];

/// The drivers of GPUs that are not PCI devices (Apple's, Qualcomm's,
/// Arm's, Imagination's, Broadcom's): kept out where every render node
/// is a PCI device with a vendor read, since none of them is there.
const PLATFORM_DRIVERS: [&str; 5] = [
    "*asahi_icd*",
    "*freedreno_icd*",
    "*panfrost_icd*",
    "*powervr_mesa_icd*",
    "*broadcom_icd*",
];

/// The PCI vendors of the machine's render nodes
/// (`/sys/class/drm/renderD*/device/vendor`; none without the DRM
/// class); `None` when they cannot all be read (no driver is then kept
/// out for want of its GPU).
pub fn render_vendors() -> Option<Vec<u16>> {
    vendors_in(std::path::Path::new("/sys/class/drm"))
}

fn vendors_in(sys: &std::path::Path) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(sys) {
        Ok(e) => e,
        // No DRM class at all: no GPU.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(out),
        Err(_) => return None,
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_str().is_some_and(|n| n.starts_with("renderD")) {
            continue;
        }
        let text = std::fs::read_to_string(e.path().join("device/vendor")).ok()?;
        let hex = text.trim().trim_start_matches("0x");
        out.push(u16::from_str_radix(hex, 16).ok()?);
    }
    Some(out)
}

/// (gpu-first review) What [`LOADER_DISABLE_ENV`] must be for the
/// process, given the render nodes' `vendors` ([`render_vendors`]),
/// `inherited` (its value when strand started) and whether the person
/// chose drivers ([`LOADER_CHOICE_ENVS`]): the software drivers kept out
/// unless a software adapter is accepted, and the hardware drivers of
/// vendors with no render node here. A probe then never loads lavapipe,
/// nor RADV on a machine with no AMD GPU, and the LLVM they link (about
/// 54 MB resident, mapped for good once loaded), nor NVIDIA's driver on
/// a laptop that has its manifest but not its GPU. `None` leaves it as
/// it is: the person chose the drivers, or nothing is to be added.
pub fn loader_disable(
    opts: &GpuOptions,
    vendors: Option<&[u16]>,
    inherited: Option<&std::ffi::OsStr>,
    chosen: bool,
) -> Option<std::ffi::OsString> {
    if chosen {
        return None;
    }
    let mut globs: Vec<&str> = Vec::new();
    if !opts.software {
        globs.extend(SOFTWARE_DRIVERS);
    }
    if let Some(vendors) = vendors {
        for (vendor, drivers) in VENDOR_DRIVERS {
            if !vendors.contains(&vendor) {
                globs.extend(drivers.iter().copied());
            }
        }
        globs.extend(PLATFORM_DRIVERS);
    }
    let had = inherited.and_then(|v| v.to_str()).unwrap_or("").trim();
    let have: Vec<&str> = had.split(',').map(str::trim).collect();
    globs.retain(|g| !have.contains(g));
    if globs.is_empty() {
        return None;
    }
    let mut out = String::from(had);
    for g in globs {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(g);
    }
    Some(out.into())
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
    /// A presented frame went to the compositor: the surface's frame
    /// callback. `frame` is render's frame number; `at` is when the GPU
    /// thread had submitted the frame's work, taken before `present`
    /// (which may block on the compositor's pacing); `waited` is the part
    /// of the GPU thread's span that is not the frame's cost: the time
    /// the request sat in the thread's queue behind other work (another
    /// surface's frame, its `present`), and the block on the swapchain
    /// for an image (FIFO pacing). The thread never waits for the GPU to
    /// run a presented frame: `ran` gives how long the GPU ran earlier
    /// frames of this surface, by frame number, as their timestamps come
    /// (this frame's comes with a later `Presented`, and a frame that
    /// found both of its surface's timers busy never comes; on a device
    /// without timestamps, this frame itself at zero). A frame's time is
    /// `at` less its `paint`'s start less `waited`, plus its `ran`.
    Presented {
        surface: SurfaceId,
        frame: u64,
        at: Instant,
        waited: Duration,
        ran: Vec<(u64, Duration)>,
    },
    /// A readback frame's pixels, after the GPU ran it; `waited` is the
    /// time its request sat in the thread's queue behind other work.
    Pixels {
        surface: SurfaceId,
        frame: u64,
        pixels: Readback,
        waited: Duration,
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
    /// Each request with when it was sent: the thread times a frame's
    /// wait in its queue (`GpuReply::Presented`'s `waited`).
    tx: mpsc::Sender<(Instant, GpuRequest)>,
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
        Self::spawn_with(waker, opts, |b, f| b.spawn(f))
    }

    /// [`Gpu::spawn`] with the thread started by `start` (tests make it
    /// fail). The waker waits in a slot the thread takes it from, so a
    /// thread that never started leaves it here to wake the loop for
    /// the `Unavailable` and `Exited` replies (m4-audit).
    fn spawn_with(
        waker: Box<dyn Fn() + Send>,
        opts: GpuOptions,
        start: impl FnOnce(
            std::thread::Builder,
            Box<dyn FnOnce() + Send>,
        ) -> std::io::Result<JoinHandle<()>>,
    ) -> Gpu {
        let (tx, req_rx) = mpsc::channel();
        let (reply_tx, rx) = mpsc::channel();
        let slot = Arc::new(std::sync::Mutex::new(Some(waker)));
        let thread = start(
            std::thread::Builder::new().name(THREAD_NAME.into()),
            Box::new({
                let (reply_tx, slot) = (reply_tx.clone(), slot.clone());
                move || {
                    let waker = slot.lock().ok().and_then(|mut w| w.take());
                    if let Some(waker) = waker {
                        thread::run(req_rx, reply_tx, waker, opts);
                    }
                }
            }),
        );
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                let _ = reply_tx.send(GpuReply::Unavailable(GpuError::new(
                    GpuErrorKind::Device,
                    format!("cannot start the GPU thread: {e}"),
                )));
                let _ = reply_tx.send(GpuReply::Exited);
                if let Some(waker) = slot.lock().ok().and_then(|mut w| w.take()) {
                    waker();
                }
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
        let _ = self.tx.send((Instant::now(), req));
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
        self.send(GpuRequest::Shutdown);
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
        self.send(GpuRequest::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (m4-audit) A thread that cannot be spawned answers `Unavailable`
    /// and `Exited` and wakes the loop to read them, as every reply does.
    #[test]
    fn a_thread_that_cannot_start_wakes_the_loop_for_its_replies() {
        let (ping, pings) = mpsc::channel();
        let mut gpu = Gpu::spawn_with(
            Box::new(move || {
                let _ = ping.send(());
            }),
            GpuOptions::default(),
            |_, _| Err(std::io::Error::other("no threads left")),
        );
        assert_eq!(pings.try_recv(), Ok(()), "the loop is woken");
        match gpu.try_recv() {
            Some(GpuReply::Unavailable(e)) => {
                assert_eq!(e.kind, GpuErrorKind::Device);
                assert!(e.message.contains("no threads left"), "{e}");
            }
            other => panic!("not Unavailable: {other:?}"),
        }
        assert!(matches!(gpu.try_recv(), Some(GpuReply::Exited)));
        assert!(gpu.try_recv().is_none());
    }

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

    /// (gpu-first review) A device can exist with a render node, or with
    /// a software adapter accepted; with neither nothing is probed.
    #[test]
    fn a_device_is_possible_with_a_render_node_or_software() {
        let dir = std::env::temp_dir().join(format!("strand-gpu-dri-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!render_node_in(&dir), "empty");
        std::fs::write(dir.join("card0"), b"").unwrap();
        assert!(!render_node_in(&dir), "a KMS card is not a render node");
        std::fs::write(dir.join("renderD128"), b"").unwrap();
        assert!(render_node_in(&dir));
        assert!(!render_node_in(&dir.join("missing")));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(device_possible(&GpuOptions { software: true }));
    }

    /// (gpu-first review) The loader is told to leave out the software
    /// drivers unless software is accepted, and the drivers of vendors
    /// with no render node here, unless the person chose the drivers; an
    /// inherited list is kept and added to.
    #[test]
    fn the_drivers_no_device_here_needs_are_kept_out_of_the_loader() {
        use std::ffi::OsStr;
        let hw = GpuOptions { software: false };
        let sw = GpuOptions { software: true };
        let s = |v: Option<std::ffi::OsString>| v.map(|v| v.to_string_lossy().into_owned());
        const PLATFORM: &str =
            "*asahi_icd*,*freedreno_icd*,*panfrost_icd*,*powervr_mesa_icd*,*broadcom_icd*";
        const VIRTIO: &str = "*virtio_icd*,*gfxstream_vk_icd*";
        // An Intel laptop (the owner's: NVIDIA's manifest installed too).
        assert_eq!(
            s(loader_disable(&hw, Some(&[0x8086]), None, false)),
            Some(format!(
                "*lvp_icd*,*swiftshader*,*radeon_icd*,*amd_icd*,*nouveau_icd*,*nvidia_icd*,\
                 {VIRTIO},{PLATFORM}"
            ))
        );
        // Intel and NVIDIA (a hybrid laptop), software accepted.
        assert_eq!(
            s(loader_disable(&sw, Some(&[0x8086, 0x10de]), None, false)),
            Some(format!("*radeon_icd*,*amd_icd*,{VIRTIO},{PLATFORM}"))
        );
        // No render node at all: every vendor's driver and lavapipe.
        assert_eq!(
            s(loader_disable(&hw, Some(&[]), None, false)),
            Some(format!(
                "*lvp_icd*,*swiftshader*,*intel_icd*,*intel_hasvk_icd*,*radeon_icd*,*amd_icd*,\
                 *nouveau_icd*,*nvidia_icd*,{VIRTIO},{PLATFORM}"
            ))
        );
        // Vendors unknown: only the software drivers.
        assert_eq!(
            s(loader_disable(&hw, None, None, false)).as_deref(),
            Some("*lvp_icd*,*swiftshader*")
        );
        assert_eq!(
            loader_disable(&sw, None, None, false),
            None,
            "nothing to add"
        );
        // An inherited list is kept, and nothing is added twice.
        assert_eq!(
            s(loader_disable(&hw, None, Some(OsStr::new("*foo*")), false)).as_deref(),
            Some("*foo*,*lvp_icd*,*swiftshader*")
        );
        assert_eq!(
            s(loader_disable(&hw, None, Some(OsStr::new(" ")), false)).as_deref(),
            Some("*lvp_icd*,*swiftshader*")
        );
        let already = OsStr::new("*lvp_icd*,*swiftshader*");
        assert_eq!(loader_disable(&hw, None, Some(already), false), None);
        assert_eq!(
            loader_disable(&hw, Some(&[]), None, true),
            None,
            "the person chose"
        );
    }

    /// The render nodes' vendors come from sysfs; one unreadable makes
    /// them unknown.
    #[test]
    fn render_vendors_are_read_from_sysfs() {
        let dir = std::env::temp_dir().join(format!("strand-gpu-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("card0/device")).unwrap();
        std::fs::write(dir.join("card0/device/vendor"), "0x1002\n").unwrap();
        assert_eq!(
            vendors_in(&dir),
            Some(vec![]),
            "a card is not a render node"
        );
        std::fs::create_dir_all(dir.join("renderD128/device")).unwrap();
        std::fs::write(dir.join("renderD128/device/vendor"), "0x8086\n").unwrap();
        assert_eq!(vendors_in(&dir), Some(vec![0x8086]));
        std::fs::create_dir_all(dir.join("renderD129")).unwrap();
        assert_eq!(vendors_in(&dir), None, "unreadable");
        assert_eq!(vendors_in(&dir.join("missing")), Some(vec![]), "no DRM");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

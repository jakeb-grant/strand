//! The GPU leg (docs/eval.md, "CPU and GPU"): a run whose surfaces are
//! drawn by strand-gpu, as a `GpuPresent` surface's frames are in the
//! shell, on whatever Vulkan device the environment has (lavapipe in the
//! container and CI, with `STRAND_GPU_SOFTWARE=1`).
//!
//! The host side follows crates/strand-render/tests/gpu_host/mod.rs
//! (`Host`, `device`, lines 19-142) and gpu.rs
//! `a_presented_surface_crossfades_on_the_gpu_like_the_cpu` (`draw`,
//! lines 954-976): the device is warmed before the first surface (the
//! shell's warm start, `Renderer::warm_gpu`), every surface is promoted
//! as it is attached and set to `GpuPresent`, and each frame
//! `Renderer::paint_gpu` lowers is drawn and read back before the tick
//! goes on (the thread attaches a surface without Wayland handles for
//! readback), so the run stays a function of the scenario.
//!
//! What the leg leaves out, on purpose: promotion's wall-clock rules
//! (`set_promotion(false)`: a surface stays on the GPU for the whole
//! run), the replies' timing (`Presented` is never delivered, so the
//! frame-time statistics are the CPU's business) and `shader` passes
//! (requests other than frames are dropped, and noted).

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use strand_gpu::{Frame, Gpu, GpuOptions, GpuReply, GpuRequest};
use strand_render::Renderer;
use strand_scene::{BackendChange, Scale, Size, SurfaceId};

use crate::Error;

/// How long a reply may take (a cold lavapipe device compiles its
/// pipelines on the first frame: well under this).
pub const WAIT: Duration = Duration::from_secs(60);

/// The variable that makes a missing device a failure instead of a skip
/// (CI and the container set it).
pub const REQUIRE_ENV: &str = "STRAND_REQUIRE_GPU";

/// Whether a missing device fails (`STRAND_REQUIRE_GPU=1`).
pub fn required() -> bool {
    std::env::var(REQUIRE_ENV).as_deref() == Ok("1")
}

/// A running GPU thread and its replies.
#[derive(Debug)]
pub struct Device {
    gpu: Gpu,
    pings: mpsc::Receiver<()>,
    /// Requests the renderer made that the leg does not serve (passes),
    /// counted by kind for the run's notes.
    pub dropped: Vec<String>,
}

impl Device {
    /// Starts the device as the shell's warm start does and hands its
    /// `Ready` to `r`. `Err(Error::NoDevice)` when none starts.
    pub fn start(r: &mut Renderer) -> Result<Device, Error> {
        r.warm_gpu();
        let _ = r.take_backend_changes();
        let (ping, pings) = mpsc::channel();
        let gpu = Gpu::spawn(
            Box::new(move || {
                let _ = ping.send(());
            }),
            GpuOptions::from_env(),
        );
        let mut d = Device {
            gpu,
            pings,
            dropped: Vec::new(),
        };
        let reply = d.until(|m| {
            matches!(
                m,
                GpuReply::Ready(_) | GpuReply::Unavailable(_) | GpuReply::Exited
            )
        })?;
        match reply {
            GpuReply::Ready(_) => {
                r.deliver_gpu(reply);
                Ok(d)
            }
            GpuReply::Unavailable(e) => Err(Error::NoDevice(e.to_string())),
            _ => Err(Error::NoDevice("the GPU thread ended at its start".into())),
        }
    }

    /// A device of its own, which no renderer drives (its `Ready` is
    /// dropped): the `redraw` check's, where each frame is drawn as on a
    /// GPU holding nothing from earlier ones ([`Fresh`]).
    pub fn bare() -> Result<Device, Error> {
        let (ping, pings) = mpsc::channel();
        let gpu = Gpu::spawn(
            Box::new(move || {
                let _ = ping.send(());
            }),
            GpuOptions::from_env(),
        );
        let mut d = Device {
            gpu,
            pings,
            dropped: Vec::new(),
        };
        match d.until(|m| {
            matches!(
                m,
                GpuReply::Ready(_) | GpuReply::Unavailable(_) | GpuReply::Exited
            )
        })? {
            GpuReply::Ready(_) => Ok(d),
            GpuReply::Unavailable(e) => Err(Error::NoDevice(e.to_string())),
            _ => Err(Error::NoDevice("the GPU thread ended at its start".into())),
        }
    }

    /// Waits for a reply `want` accepts; the others are dropped (the
    /// leg answers the renderer itself, see the module docs).
    fn until(&mut self, want: impl Fn(&GpuReply) -> bool) -> Result<GpuReply, Error> {
        let deadline = Instant::now() + WAIT;
        loop {
            while let Some(reply) = self.gpu.try_recv() {
                if want(&reply) {
                    return Ok(reply);
                }
                if let GpuReply::Failed { error, .. } = &reply {
                    return Err(Error::Run(format!("the GPU failed a frame: {error}")));
                }
                if matches!(reply, GpuReply::Lost(_) | GpuReply::Exited) {
                    return Err(Error::Run(format!("the GPU thread ended: {reply:?}")));
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Run(format!("no GPU reply within {WAIT:?}")));
            }
            let _ = self.pings.recv_timeout(left.min(Duration::from_millis(50)));
        }
    }

    /// `surface`, just attached and configured, drawn by the GPU from
    /// its next frame.
    pub fn attach(&mut self, r: &mut Renderer, surface: SurfaceId, size: Size, scale: Scale) {
        r.promote_now(surface);
        self.changes(r);
        r.set_backend(surface, strand_scene::Backend::GpuPresent);
        self.gpu.send(GpuRequest::Attach {
            surface,
            handles: None,
            size,
            scale,
            opaque: false,
        });
    }

    /// `surface` configured at a new size or scale.
    pub fn resize(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        self.gpu.send(GpuRequest::Resize {
            surface,
            size,
            scale,
        });
    }

    /// `surface` back on the CPU, or gone.
    pub fn release(&mut self, r: &mut Renderer, surface: SurfaceId, gone: bool) {
        if !gone {
            r.set_backend(surface, strand_scene::Backend::Cpu);
        }
        self.gpu.send(GpuRequest::Release(surface));
    }

    /// What the renderer asked of the host since: promotions are the
    /// leg's own (made in `attach`), passes and commits are dropped.
    pub fn changes(&mut self, r: &mut Renderer) {
        for c in r.take_backend_changes() {
            if let BackendChange::Demote(s) = c {
                self.dropped.push(format!("a demotion of surface {}", s.0));
            }
        }
        for q in r.take_gpu_requests() {
            let kind = match q {
                GpuRequest::Commit(_) => continue,
                GpuRequest::Pass(_) => "a shader pass",
                _ => "a request",
            };
            self.dropped.push(kind.to_string());
        }
    }

    /// Draws `frame` and copies its pixels into `pixels` (premultiplied
    /// BGRA, `size`, rows `size.w * 4` apart), as a CPU frame leaves
    /// them.
    pub fn draw(&mut self, frame: Frame, pixels: &mut [u8], size: Size) -> Result<(), Error> {
        let id = frame.id;
        self.gpu.send(GpuRequest::Frame(frame));
        let reply = self.until(|m| matches!(m, GpuReply::Pixels { frame, .. } if *frame == id))?;
        if let GpuReply::Pixels { pixels: px, .. } = reply {
            px.copy_into(pixels, size.w as usize * 4, size, 0, 0);
        }
        Ok(())
    }

    /// Ends the thread and waits for it (at most [`WAIT`]): a process
    /// ending under a running lavapipe thread can crash in the driver's
    /// static destructors (docs/decisions.md gpu-first, exit).
    pub fn finish(mut self) {
        let deadline = Instant::now() + WAIT;
        while !self.gpu.try_join() && Instant::now() < deadline {
            while self.gpu.try_recv().is_some() {}
            let _ = self.pings.recv_timeout(Duration::from_millis(20));
        }
    }
}

/// The `redraw` check's side of the GPU leg: a [`Device::bare`] drawing
/// each presented frame again as `Renderer::fresh_gpu_frame` lowers it
/// (every pixmap uploaded with it), retiring that frame's uploads with
/// the next, so no frame draws with anything an earlier one left.
#[derive(Debug)]
pub struct Fresh {
    dev: Device,
    /// Each surface attached, at its size and scale.
    attached: HashMap<SurfaceId, (Size, Scale)>,
    /// The last frame's uploads, retired by the next.
    held: Vec<u64>,
    next_upload: u64,
    next_frame: u64,
}

impl Fresh {
    pub fn start() -> Result<Fresh, Error> {
        Ok(Fresh {
            dev: Device::bare()?,
            attached: HashMap::new(),
            held: Vec::new(),
            next_upload: 0,
            next_frame: 0,
        })
    }

    /// `surface`'s last frame drawn fresh into `pixels` (premultiplied
    /// BGRA, `size`); false when `r` keeps none of that size.
    pub fn draw(
        &mut self,
        r: &Renderer,
        surface: SurfaceId,
        size: Size,
        scale: Scale,
        pixels: &mut [u8],
    ) -> Result<bool, Error> {
        self.next_frame += 1;
        let Some(mut frame) = r.fresh_gpu_frame(surface, self.next_frame, self.next_upload) else {
            return Ok(false);
        };
        if frame.size != size || frame.scale != scale {
            return Ok(false);
        }
        match self.attached.get(&surface) {
            None => {
                self.dev.gpu.send(GpuRequest::Attach {
                    surface,
                    handles: None,
                    size,
                    scale,
                    opaque: false,
                });
                self.attached.insert(surface, (size, scale));
            }
            Some(at) if *at != (size, scale) => {
                self.dev.resize(surface, size, scale);
                self.attached.insert(surface, (size, scale));
            }
            Some(_) => {}
        }
        frame.retire = std::mem::take(&mut self.held);
        self.held = frame.uploads.iter().map(|u| u.id).collect();
        self.next_upload = self.held.iter().copied().fold(self.next_upload, u64::max);
        self.dev.draw(frame, pixels, size)?;
        Ok(true)
    }

    /// Ends its thread ([`Device::finish`]).
    pub fn finish(self) {
        self.dev.finish();
    }
}

//! The `strand-gpu` thread: waits on its channel (on presents, inside
//! `get_current_texture`, and on a readback's or pass's GPU work, at
//! most `HUNG_AFTER`; never on a presented frame's), never on a timer;
//! it does not decide when to stop. A panic in the device's code counts
//! as a lost device.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vello_common::TextureId;

use crate::device::Device;
use crate::draw::{self, DrawnPass, Held, Images};
use crate::lru::Lru;
use crate::pass::{self, Passes};
use crate::present::{self, Blit, Presented, Swapchain};
use crate::readback::ReadBuffer;
use crate::timer::Timer;
use crate::{
    Frame, GpuError, GpuErrorKind, GpuMode, GpuOptions, GpuReply, GpuRequest, Op, PassFrame, Scale,
    Size, SurfaceId, Upload,
};

/// The format frames are rendered in: straight colours, `wl_shm` bytes.
const FRAME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// Pass sizes whose readback buffers are kept.
const PASS_READS: usize = 16;

struct Reply<'a> {
    tx: &'a mpsc::Sender<GpuReply>,
    waker: &'a (dyn Fn() + Send),
}

impl Reply<'_> {
    fn send(&self, r: GpuReply) {
        let _ = self.tx.send(r);
        (self.waker)();
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".into())
}

/// The answer a frame or pass still owes if handling it panics: every
/// one is answered, so render stops waiting for it.
fn owed(req: &GpuRequest) -> Option<(Option<SurfaceId>, Option<u64>)> {
    match req {
        GpuRequest::Frame(f) => Some((Some(f.surface), None)),
        GpuRequest::Pass(p) => Some((None, Some(p.key))),
        _ => None,
    }
}

/// Runs `handle`; a panic in it (wgpu panics when a driver resets a hung
/// device under `poll`: ANV after a few seconds, before `HUNG_AFTER`)
/// marks the device lost and answers what the request owed with
/// `Failed`.
fn handle_caught(
    lost: &AtomicBool,
    owed: Option<(Option<SurfaceId>, Option<u64>)>,
    reply: &Reply<'_>,
    handle: impl FnOnce(),
) {
    let Err(p) = catch_unwind(AssertUnwindSafe(handle)) else {
        return;
    };
    let why = panic_text(&*p);
    log::warn!("the GPU thread panicked: {why}");
    lost.store(true, Ordering::SeqCst);
    if let Some((surface, key)) = owed {
        reply.send(GpuReply::Failed {
            surface,
            key,
            error: GpuError::new(GpuErrorKind::Lost, format!("the GPU panicked: {why}")),
        });
    }
}

pub(crate) fn run(
    rx: mpsc::Receiver<(Instant, GpuRequest)>,
    tx: mpsc::Sender<GpuReply>,
    waker: Box<dyn Fn() + Send>,
    opts: GpuOptions,
) {
    let reply = Reply {
        tx: &tx,
        waker: &*waker,
    };
    let opened = catch_unwind(|| Device::open(&opts)).unwrap_or_else(|p| {
        Err(GpuError::new(
            GpuErrorKind::Device,
            format!("the GPU failed to start: {}", panic_text(&*p)),
        ))
    });
    let dev = match opened {
        Ok(d) => d,
        Err(e) => {
            reply.send(GpuReply::Unavailable(e));
            reply.send(GpuReply::Exited);
            return;
        }
    };
    let info = dev.info.clone();
    let state = catch_unwind(AssertUnwindSafe(|| State::new(&dev)));
    let Ok(mut state) = state else {
        reply.send(GpuReply::Unavailable(GpuError::new(
            GpuErrorKind::Device,
            format!("the GPU renderer failed to start on {}", info.name),
        )));
        drop(dev);
        reply.send(GpuReply::Exited);
        return;
    };
    reply.send(GpuReply::Ready(info));
    while let Ok((sent, req)) = rx.recv() {
        if matches!(req, GpuRequest::Shutdown) {
            break;
        }
        let queued = sent.elapsed();
        let owed = owed(&req);
        handle_caught(&dev.lost, owed, &reply, || {
            state.handle(&dev, req, queued, &reply)
        });
        if dev.is_lost() {
            let why = if dev.hung.load(Ordering::SeqCst) {
                "stopped answering"
            } else {
                "was lost"
            };
            reply.send(GpuReply::Lost(GpuError::new(
                GpuErrorKind::Lost,
                format!("the GPU device {why} ({})", dev.info.name),
            )));
            break;
        }
    }
    if dev.hung.load(Ordering::SeqCst) {
        // A hung queue: dropping the swapchains, the renderer or the
        // device would wait for it, so they are leaked (with the driver's
        // spinning queue) and the thread ends now. The surfaces' owners
        // may commit them again once they hear `Exited`: nothing here
        // presents any more.
        std::mem::forget(state);
        std::mem::forget(dev);
        reply.send(GpuReply::Exited);
        return;
    }
    // Swapchains before their surfaces' owners hear anything, then the
    // renderer, the device and the instance.
    drop(state);
    drop(dev);
    reply.send(GpuReply::Exited);
}

/// A frame texture and what reads it back, for one size.
struct FrameTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

struct Target {
    size: Size,
    #[allow(dead_code)]
    scale: Scale,
    present: Option<Presented>,
    /// A presented surface's GPU timestamps (none without them).
    timer: Option<Timer>,
    frame: Option<FrameTarget>,
    read: Option<ReadBuffer>,
}

struct State {
    renderer: vello_gpu::Renderer,
    resources: vello_gpu::Resources,
    scene: vello_gpu::Scene,
    images: Images,
    passes: Passes,
    blit: Blit,
    targets: HashMap<SurfaceId, Target>,
    /// Pass readback buffers by size: a pass's read finishes before the
    /// next pass is drawn, so passes of one size share a buffer, and the
    /// sizes kept are bounded (a remounted node is a new pass key, and a
    /// shown shader keeps the device alive).
    pass_reads: Lru<(u32, u32), ReadBuffer>,
}

impl State {
    fn new(dev: &Device) -> Self {
        let (renderer, resources) = vello_gpu::Renderer::new(
            &dev.device,
            &vello_gpu::RenderTargetConfig {
                format: FRAME_FORMAT,
                width: 1,
                height: 1,
            },
        );
        State {
            renderer,
            resources,
            scene: vello_gpu::Scene::new(1, 1),
            images: Images::new(),
            passes: Passes::new(dev),
            blit: Blit::default(),
            targets: HashMap::new(),
            pass_reads: Lru::new(PASS_READS),
        }
    }

    /// Handles a request that sat `queued` in the channel.
    fn handle(&mut self, dev: &Device, req: GpuRequest, queued: Duration, reply: &Reply<'_>) {
        match req {
            GpuRequest::Attach {
                surface,
                handles,
                size,
                scale,
                opaque,
            } => {
                let present = handles.and_then(|h| {
                    match present::try_surface(dev, h, size.w, size.h, opaque) {
                        Ok(p) => Some(p),
                        Err(why) => {
                            log::info!("GPU: {surface:?} is read back: {why}");
                            None
                        }
                    }
                });
                let mode = if present.is_some() {
                    GpuMode::Present
                } else {
                    GpuMode::Readback
                };
                let timer = present.as_ref().and_then(|_| Timer::new(dev));
                self.targets.insert(
                    surface,
                    Target {
                        size,
                        scale,
                        present,
                        timer,
                        frame: None,
                        read: None,
                    },
                );
                reply.send(GpuReply::Attached { surface, mode });
            }
            GpuRequest::Resize {
                surface,
                size,
                scale,
            } => {
                if let Some(t) = self.targets.get_mut(&surface) {
                    t.size = size;
                    t.scale = scale;
                    if let Some(p) = &mut t.present {
                        p.resize(dev, size.w, size.h);
                    }
                }
            }
            GpuRequest::Release(surface) => {
                // The swapchain goes before the answer: the manager may
                // destroy or commit the `wl_surface` once it hears it.
                self.targets.remove(&surface);
                reply.send(GpuReply::Released(surface));
            }
            GpuRequest::Frame(frame) => self.frame(dev, frame, queued, reply),
            GpuRequest::Pass(pass) => self.pass(dev, pass, reply),
            GpuRequest::Shutdown => {}
        }
    }

    /// Uploads the frame's new pixmaps, then frees the retired ones and
    /// those replaced.
    ///
    /// In that order: vello_gpu writes an image through the queue, which
    /// runs before this frame's encoder, while it clears a destroyed
    /// image's atlas slot in the encoder. A slot freed first and handed
    /// to a new image in the same frame would be cleared after the new
    /// pixels were written (a re-rasterised island drawn as nothing).
    fn upload(
        &mut self,
        dev: &Device,
        encoder: &mut wgpu::CommandEncoder,
        uploads: &[Upload],
        retire: &[u64],
    ) {
        let mut freed = Vec::new();
        for u in uploads {
            if let Some(h) = self.images.get(&u.id) {
                if h.generation == u.generation {
                    continue;
                }
                freed.push(h.id);
                self.images.remove(&u.id);
            }
            if u.pixmap.width() == 0 || u.pixmap.height() == 0 {
                continue;
            }
            let px = draw::unswap(&u.pixmap);
            let id = self.renderer.upload_image(
                &mut self.resources,
                &dev.device,
                &dev.queue,
                encoder,
                &px,
            );
            self.images.insert(
                u.id,
                Held {
                    generation: u.generation,
                    id,
                },
            );
        }
        for id in retire {
            if let Some(h) = self.images.remove(id) {
                freed.push(h.id);
            }
        }
        for old in freed {
            self.renderer
                .destroy_image(&mut self.resources, encoder, old);
        }
    }

    /// Draws a frame's passes into their own textures, in op order.
    fn draw_passes(
        &mut self,
        dev: &Device,
        encoder: &mut wgpu::CommandEncoder,
        ops: &[Op],
        bindings: &mut vello_gpu::TextureBindings,
        keep: &mut Vec<pass::Drawn>,
    ) -> (Vec<Option<DrawnPass>>, Option<GpuError>) {
        let mut out = Vec::new();
        let mut failed = None;
        for op in ops {
            let Op::Pass {
                pass,
                bounds,
                globals,
            } = op
            else {
                continue;
            };
            let (w, h) = (bounds.width().ceil(), bounds.height().ceil());
            if !(w >= 1.0 && h >= 1.0 && w <= 16384.0 && h <= 16384.0) {
                out.push(None);
                continue;
            }
            match self
                .passes
                .draw(dev, encoder, pass, w as u32, h as u32, *globals, None)
            {
                Ok(Some(d)) => {
                    let id = TextureId(out.len() as u64 + 1);
                    bindings.insert(id, d.view.clone());
                    out.push(Some(DrawnPass {
                        texture: id,
                        width: d.width as u16,
                        height: d.height as u16,
                    }));
                    keep.push(d);
                }
                Ok(None) => out.push(None),
                Err(e) => {
                    failed.get_or_insert(e);
                    out.push(None);
                }
            }
        }
        (out, failed)
    }

    /// Draws a frame; every frame is answered exactly once (`Presented`,
    /// `Pixels` or `Failed`), since the host sends a surface's next frame
    /// only after the answer to its last. A frame whose inline pass fails
    /// is answered `Failed` alone.
    fn frame(&mut self, dev: &Device, frame: Frame, queued: Duration, reply: &Reply<'_>) {
        let surface = frame.surface;
        if let Err(error) = self.draw_frame(dev, frame, queued, reply) {
            reply.send(GpuReply::Failed {
                surface: Some(surface),
                key: None,
                error,
            });
        }
    }

    fn draw_frame(
        &mut self,
        dev: &Device,
        frame: Frame,
        queued: Duration,
        reply: &Reply<'_>,
    ) -> Result<(), GpuError> {
        let surface = frame.surface;
        let Some(target) = self.targets.get(&surface) else {
            return Err(render_error(format!("{surface:?} is not attached")));
        };
        let (w, h) = (frame.size.w, frame.size.h);
        let max = dev
            .device
            .limits()
            .max_texture_dimension_2d
            .min(u32::from(u16::MAX));
        if w == 0 || h == 0 || w > max || h > max {
            return Err(render_error(format!(
                "a {w}×{h} frame of {surface:?} is outside the device's 1–{max} pixels"
            )));
        }
        let presenting = target.present.is_some();
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("strand frame"),
            });
        // A presented frame's GPU time starts with its first command.
        let timed = target.timer.as_ref().and_then(|t| t.begin(&mut encoder));
        self.upload(dev, &mut encoder, &frame.uploads, &frame.retire);
        let mut bindings = vello_gpu::TextureBindings::new();
        let mut keep = Vec::new();
        let (passes, pass_error) =
            self.draw_passes(dev, &mut encoder, &frame.ops, &mut bindings, &mut keep);
        if let Some(e) = pass_error {
            // The frame ends here with one answer, `Failed`: render takes
            // it back to the CPU, which draws it. A frame both failed and
            // presented would be demoted for good with its pixels on
            // screen (m4-audit). The uploads and frees already recorded
            // still run, so the image table stays true.
            dev.queue.submit([encoder.finish()]);
            drop(keep);
            return Err(e);
        }
        self.scene.reset_and_resize(w as u16, h as u16);
        let missing = draw::build(&mut self.scene, &frame.ops, &self.images, &passes);
        if missing.images > 0 {
            log::debug!(
                "GPU frame of {surface:?}: {} images not uploaded",
                missing.images
            );
        }
        // The frame texture at this size.
        let Some(target) = self.targets.get_mut(&surface) else {
            return Err(render_error(format!("{surface:?} is not attached")));
        };
        if target
            .frame
            .as_ref()
            .is_none_or(|f| f.width != w || f.height != h)
        {
            let texture = dev.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("strand frame"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FRAME_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            target.frame = Some(FrameTarget {
                texture,
                view,
                width: w,
                height: h,
            });
        }
        let Some(ft) = target.frame.as_ref() else {
            return Err(render_error("no frame texture"));
        };
        let size = vello_gpu::RenderSize {
            width: w as u16,
            height: h as u16,
        };
        let rendered = self.renderer.render(
            &self.scene,
            &mut self.resources,
            &dev.device,
            &dev.queue,
            &mut encoder,
            &size,
            &ft.view,
            None,
            &bindings,
            vello_gpu::TargetInit::Clear(vello_gpu::ClearSettings::Viewport { color: frame.clear }),
        );
        if let Err(e) = rendered {
            return Err(render_error(e.to_string()));
        }
        if presenting {
            let Some(p) = target.present.as_mut() else {
                return Err(render_error("no swapchain"));
            };
            let presented = present_frame(
                dev,
                &mut self.blit,
                p,
                target.timer.as_mut().map(|t| (t, timed)),
                encoder,
                &ft.view,
                (surface, frame.id),
                queued,
            );
            drop(keep);
            reply.send(presented?);
            Ok(())
        } else {
            if target.read.as_ref().is_none_or(|r| !r.fits(w, h)) {
                target.read = Some(ReadBuffer::new(dev, w, h));
            }
            let Some(read) = target.read.as_ref() else {
                return Err(render_error("no readback buffer"));
            };
            read.record(&mut encoder, &ft.texture);
            dev.queue.submit([encoder.finish()]);
            drop(keep);
            let pixels = read.read(dev)?;
            reply.send(GpuReply::Pixels {
                surface,
                frame: frame.id,
                pixels,
                waited: queued,
            });
            Ok(())
        }
    }

    fn pass(&mut self, dev: &Device, p: PassFrame, reply: &Reply<'_>) {
        let (w, h) = (p.size.w, p.size.h);
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("strand pass"),
            });
        // Its input (a filter's subtree, a backdrop): the CPU raster's
        // bytes are `Bgra8Unorm`'s, so they go up as they are.
        let input = p.input.as_ref().and_then(|px| input_texture(dev, px));
        let mut drawn = match self.passes.draw(
            dev,
            &mut encoder,
            &p.pass,
            w,
            h,
            p.globals,
            input.as_ref().map(|(_, v)| v),
        ) {
            Ok(Some(d)) => d,
            Ok(None) => {
                // A zero or over-large size: answered, so render stops
                // waiting for it.
                reply.send(GpuReply::Failed {
                    surface: None,
                    key: Some(p.key),
                    error: render_error(format!("a {w}×{h} pass draws nothing")),
                });
                return;
            }
            Err(e) => {
                reply.send(GpuReply::Failed {
                    surface: None,
                    key: Some(p.key),
                    error: e,
                });
                return;
            }
        };
        // The passes after it, each on the one before's pixels (the
        // textures live until the encoder is submitted).
        let mut keep = Vec::new();
        for next in &p.then {
            match self
                .passes
                .draw(dev, &mut encoder, next, w, h, p.globals, Some(&drawn.view))
            {
                Ok(Some(d)) => keep.push(std::mem::replace(&mut drawn, d)),
                Ok(None) => {}
                Err(e) => {
                    reply.send(GpuReply::Failed {
                        surface: None,
                        key: Some(p.key),
                        error: e,
                    });
                    return;
                }
            }
        }
        let read = self
            .pass_reads
            .get_or_insert_with((w, h), || ReadBuffer::new(dev, w, h));
        read.record(&mut encoder, &drawn.texture);
        dev.queue.submit([encoder.finish()]);
        drop(keep);
        match read.read(dev) {
            Ok(pixels) => reply.send(GpuReply::PassPixels {
                key: p.key,
                frame: p.id,
                pixels,
            }),
            Err(e) => reply.send(GpuReply::Failed {
                surface: None,
                key: Some(p.key),
                error: e,
            }),
        }
    }
}

/// A pass's input pixels as a texture (none for an empty pixmap).
fn input_texture(dev: &Device, px: &crate::Pixmap) -> Option<(wgpu::Texture, wgpu::TextureView)> {
    let (w, h) = (u32::from(px.width()), u32::from(px.height()));
    let max = dev.device.limits().max_texture_dimension_2d;
    if w == 0 || h == 0 || w > max || h > max {
        return None;
    }
    let size = wgpu::Extent3d {
        width: w,
        height: h,
        depth_or_array_layers: 1,
    };
    let texture = dev.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("strand pass input"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: pass::FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    dev.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        px.data_as_u8_slice(),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Some((texture, view))
}

fn render_error(message: impl Into<String>) -> GpuError {
    GpuError::new(GpuErrorKind::Render, message)
}

/// The end of a presented frame: acquires an image (that block, with the
/// frame's time in the queue, is `waited`), blits the frame into it,
/// submits, takes `at`, presents, and answers without waiting for the GPU
/// to run it: the timer (with the slot `begin` took for this frame, if
/// any) reads its run time later, and the answer carries those of earlier
/// frames that have come. A device without timestamps times this frame's
/// GPU run at zero.
#[allow(clippy::too_many_arguments)]
fn present_frame<C: Swapchain>(
    dev: &Device,
    blit: &mut Blit,
    chain: &mut C,
    timer: Option<(&mut Timer, Option<usize>)>,
    mut encoder: wgpu::CommandEncoder,
    src: &wgpu::TextureView,
    (surface, frame): (SurfaceId, u64),
    queued: Duration,
) -> Result<GpuReply, GpuError> {
    let acquiring = Instant::now();
    let image = chain.acquire(dev);
    let waited = queued + acquiring.elapsed();
    let Some((image, dst, format)) = image else {
        return Err(render_error("no swapchain image"));
    };
    blit.record(dev, &mut encoder, src, &dst, format);
    if let Some((t, Some(slot))) = &timer {
        t.end(&mut encoder, *slot);
    }
    dev.queue.submit([encoder.finish()]);
    let at = Instant::now();
    chain.present(dev, image);
    let ran = match timer {
        Some((t, slot)) => {
            if let Some(slot) = slot {
                t.submitted(slot, frame);
            }
            t.ran(dev)
        }
        None => vec![(frame, Duration::ZERO)],
    };
    Ok(GpuReply::Presented {
        surface,
        frame,
        at,
        waited,
        ran,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A swapchain that blocks like a FIFO one: `acquire` and `present`
    /// sleep. It notes when `present` came and whether the GPU had run
    /// everything by then.
    struct Paced {
        image: wgpu::Texture,
        acquire: Duration,
        present: Duration,
        presented: Option<(Instant, bool)>,
    }

    impl Swapchain for Paced {
        type Image = ();
        fn acquire(&mut self, _: &Device) -> Option<((), wgpu::TextureView, wgpu::TextureFormat)> {
            std::thread::sleep(self.acquire);
            let view = self
                .image
                .create_view(&wgpu::TextureViewDescriptor::default());
            Some(((), view, FRAME_FORMAT))
        }
        fn present(&mut self, dev: &Device, _: ()) {
            let idle = dev
                .device
                .poll(wgpu::PollType::Poll)
                .is_ok_and(|s| s.is_queue_empty());
            self.presented = Some((Instant::now(), idle));
            std::thread::sleep(self.present);
        }
    }

    fn texture(dev: &Device, side: u32, usage: wgpu::TextureUsages) -> wgpu::Texture {
        dev.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FRAME_FORMAT,
            usage,
            view_formats: &[],
        })
    }

    /// (m5-tools review round 3) `draw_frame`'s present path, on a
    /// swapchain that blocks 30 ms to acquire and 100 ms to present, for
    /// a frame that sat 50 ms in the queue: it presents while the GPU
    /// still runs the frame (the thread never waits on a presented
    /// frame's work, so another surface's frame starts at once), `at` is
    /// taken before `present`, `waited` is the queue and the acquire and
    /// nothing after, and the GPU's run comes later from the timestamps,
    /// within submit to finish. Lavapipe runs a submission on its own
    /// thread: 32 clears of a 4096² texture still run at `present`.
    #[test]
    fn a_presented_frame_is_answered_before_the_gpu_ran_it() {
        let dev = match Device::open(&GpuOptions::from_env()) {
            Ok(d) => d,
            Err(e) => {
                let required = std::env::var("STRAND_REQUIRE_GPU").is_ok_and(|v| v == "1");
                assert!(!required, "STRAND_REQUIRE_GPU=1 but no GPU: {e}");
                eprintln!("skipped: no GPU ({e})");
                return;
            }
        };
        let mut timer = Timer::new(&dev);
        if dev.info.software {
            assert!(timer.is_some(), "lavapipe has timestamps");
        }
        let big = texture(
            &dev,
            4096,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let view = big.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let slot = timer.as_ref().and_then(|t| t.begin(&mut encoder));
        for i in 0..32 {
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(i) / 32.0,
                            g: 0.5,
                            b: 0.25,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..wgpu::RenderPassDescriptor::default()
            });
        }
        let mut chain = Paced {
            image: texture(&dev, 64, wgpu::TextureUsages::RENDER_ATTACHMENT),
            acquire: Duration::from_millis(30),
            present: Duration::from_millis(100),
            presented: None,
        };
        let queued = Duration::from_millis(50);
        let answer = present_frame(
            &dev,
            &mut Blit::default(),
            &mut chain,
            timer.as_mut().map(|t| (t, slot)),
            encoder,
            &view,
            (SurfaceId(1), 7),
            queued,
        )
        .unwrap();
        let GpuReply::Presented {
            surface,
            frame,
            at,
            waited,
            mut ran,
        } = answer
        else {
            panic!("expected Presented, got {answer:?}");
        };
        assert_eq!((surface, frame), (SurfaceId(1), 7));
        let (presented, idle) = chain.presented.expect("presented");
        assert!(!idle, "the thread waited for the GPU before `present`");
        assert!(at <= presented, "`at` taken after `present`");
        let paced = queued + chain.acquire;
        assert!(
            waited >= paced && waited < paced + Duration::from_millis(50),
            "`waited` is the queue and the acquire: {waited:?}"
        );
        dev.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(60)),
            })
            .unwrap();
        let finished = Instant::now();
        let Some(t) = timer.as_mut() else {
            assert_eq!(ran, [(7, Duration::ZERO)], "no timestamps: zero");
            return;
        };
        ran.extend(t.ran(&dev));
        let gpu = match ran.as_slice() {
            [(7, gpu)] => *gpu,
            other => panic!("frame 7's run once, got {other:?}"),
        };
        assert!(
            gpu > Duration::from_millis(1) && gpu <= finished - at,
            "the GPU's run is within submit to finish: {gpu:?}"
        );
        assert!(t.ran(&dev).is_empty(), "each run comes once");
    }

    fn replies(
        owed: Option<(Option<SurfaceId>, Option<u64>)>,
        panics: bool,
    ) -> (bool, Vec<GpuReply>) {
        let (tx, rx) = mpsc::channel();
        let lost = AtomicBool::new(false);
        let woke = std::sync::atomic::AtomicUsize::new(0);
        let waker = || {
            woke.fetch_add(1, Ordering::SeqCst);
        };
        let reply = Reply {
            tx: &tx,
            waker: &waker,
        };
        handle_caught(&lost, owed, &reply, || {
            if panics {
                panic!("Error in Device::poll: Parent device is lost");
            }
        });
        drop(tx);
        let got: Vec<GpuReply> = rx.iter().collect();
        assert_eq!(
            woke.load(Ordering::SeqCst),
            got.len(),
            "each reply wakes the loop"
        );
        (lost.load(Ordering::SeqCst), got)
    }

    /// (m4-integration-w3) A pass or frame whose handling panics (wgpu's
    /// `poll` after a hardware driver reset a hung device) is still
    /// answered, so render never waits on it, and the device counts as
    /// lost.
    #[test]
    fn a_panicking_pass_or_frame_is_answered_and_loses_the_device() {
        let (lost, got) = replies(Some((None, Some(9))), true);
        assert!(lost);
        match got.as_slice() {
            [
                GpuReply::Failed {
                    surface: None,
                    key: Some(9),
                    error,
                },
            ] => {
                assert_eq!(error.kind, GpuErrorKind::Lost);
                assert!(error.message.contains("Parent device is lost"), "{error}");
            }
            other => panic!("expected the pass's Failed, got {other:?}"),
        }
        let (lost, got) = replies(Some((Some(SurfaceId(3)), None)), true);
        assert!(lost);
        assert!(
            matches!(
                got.as_slice(),
                [GpuReply::Failed {
                    surface: Some(SurfaceId(3)),
                    key: None,
                    ..
                }]
            ),
            "{got:?}"
        );
        // A request that owes nothing (a release, say) only loses the device.
        let (lost, got) = replies(None, true);
        assert!(lost && got.is_empty(), "{got:?}");
        // No panic: nothing added, the device kept.
        let (lost, got) = replies(Some((None, Some(9))), false);
        assert!(!lost && got.is_empty(), "{got:?}");
        assert_eq!(owed(&GpuRequest::Release(SurfaceId(1))), None);
        assert_eq!(owed(&GpuRequest::Shutdown), None);
    }
}

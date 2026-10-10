//! The `strand-gpu` thread: waits on its channel (and on presents, inside
//! `get_current_texture`), never on a timer; it does not decide when to
//! stop. A panic in the device's code counts as a lost device.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use vello_common::TextureId;

use crate::device::Device;
use crate::draw::{self, DrawnPass, Held, Images};
use crate::lru::Lru;
use crate::pass::{self, Passes};
use crate::present::{self, Blit, Presented};
use crate::readback::ReadBuffer;
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
    rx: mpsc::Receiver<GpuRequest>,
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
    while let Ok(req) = rx.recv() {
        if matches!(req, GpuRequest::Shutdown) {
            break;
        }
        let owed = owed(&req);
        handle_caught(&dev.lost, owed, &reply, || state.handle(&dev, req, &reply));
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

    fn handle(&mut self, dev: &Device, req: GpuRequest, reply: &Reply<'_>) {
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
                self.targets.insert(
                    surface,
                    Target {
                        size,
                        scale,
                        present,
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
            GpuRequest::Frame(frame) => self.frame(dev, frame, reply),
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

    /// Draws a frame; every frame is answered (`Presented`, `Pixels` or
    /// `Failed`), since the host sends a surface's next frame only after
    /// the answer to its last.
    fn frame(&mut self, dev: &Device, frame: Frame, reply: &Reply<'_>) {
        let surface = frame.surface;
        if let Err(error) = self.draw_frame(dev, frame, reply) {
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
        self.upload(dev, &mut encoder, &frame.uploads, &frame.retire);
        let mut bindings = vello_gpu::TextureBindings::new();
        let mut keep = Vec::new();
        let (passes, pass_error) =
            self.draw_passes(dev, &mut encoder, &frame.ops, &mut bindings, &mut keep);
        if let Some(e) = pass_error {
            reply.send(GpuReply::Failed {
                surface: Some(surface),
                key: None,
                error: e,
            });
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
            let tex = match p.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                other => {
                    log::debug!("GPU present of {surface:?}: {other:?}; reconfiguring");
                    p.surface.configure(&dev.device, &p.config);
                    match p.surface.get_current_texture() {
                        wgpu::CurrentSurfaceTexture::Success(t)
                        | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                        _ => None,
                    }
                }
            };
            let Some(tex) = tex else {
                return Err(render_error("no swapchain image"));
            };
            let dst = tex
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            self.blit
                .record(dev, &mut encoder, &ft.view, &dst, p.config.format);
            dev.queue.submit([encoder.finish()]);
            dev.queue.present(tex);
            drop(keep);
            reply.send(GpuReply::Presented {
                surface,
                at: Instant::now(),
            });
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

#[cfg(test)]
mod tests {
    use super::*;

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

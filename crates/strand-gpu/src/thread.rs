//! The `strand-gpu` thread: waits on its channel (and on presents, inside
//! `get_current_texture`), never on a timer; it does not decide when to
//! stop. A panic in the device's code counts as a lost device.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

use vello_common::TextureId;

use crate::device::Device;
use crate::draw::{self, DrawnPass, Held, Images};
use crate::pass::{self, Passes};
use crate::present::{self, Blit, Presented};
use crate::readback::ReadBuffer;
use crate::{
    Frame, GpuError, GpuErrorKind, GpuMode, GpuOptions, GpuReply, GpuRequest, Op, PassFrame, Scale,
    Size, SurfaceId, Upload,
};

/// The format frames are rendered in: straight colours, `wl_shm` bytes.
const FRAME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

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
        let r = catch_unwind(AssertUnwindSafe(|| state.handle(&dev, req, &reply)));
        if let Err(p) = r {
            log::warn!("the GPU thread panicked: {}", panic_text(&*p));
            dev.lost.store(true, Ordering::SeqCst);
        }
        if dev.is_lost() {
            reply.send(GpuReply::Lost(GpuError::new(
                GpuErrorKind::Lost,
                format!("the GPU device was lost ({})", dev.info.name),
            )));
            break;
        }
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
    pass_reads: HashMap<u64, ReadBuffer>,
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
            pass_reads: HashMap::new(),
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

    /// Uploads the frame's new pixmaps.
    fn upload(&mut self, dev: &Device, encoder: &mut wgpu::CommandEncoder, uploads: &[Upload]) {
        for u in uploads {
            if let Some(h) = self.images.get(&u.id) {
                if h.generation == u.generation {
                    continue;
                }
                let old = h.id;
                self.renderer
                    .destroy_image(&mut self.resources, encoder, old);
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
                .draw(dev, encoder, pass, w as u32, h as u32, *globals)
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

    fn frame(&mut self, dev: &Device, frame: Frame, reply: &Reply<'_>) {
        let surface = frame.surface;
        let Some(target) = self.targets.get(&surface) else {
            return;
        };
        let (w, h) = (frame.size.w, frame.size.h);
        let max = dev
            .device
            .limits()
            .max_texture_dimension_2d
            .min(u32::from(u16::MAX));
        if w == 0 || h == 0 || w > max || h > max {
            return;
        }
        let presenting = target.present.is_some();
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("strand frame"),
            });
        self.upload(dev, &mut encoder, &frame.uploads);
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
            return;
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
            return;
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
            reply.send(GpuReply::Failed {
                surface: Some(surface),
                key: None,
                error: GpuError::new(GpuErrorKind::Render, e.to_string()),
            });
            return;
        }
        if presenting {
            let Some(p) = target.present.as_mut() else {
                return;
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
                reply.send(GpuReply::Failed {
                    surface: Some(surface),
                    key: None,
                    error: GpuError::new(GpuErrorKind::Render, "no swapchain image"),
                });
                return;
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
        } else {
            if target.read.as_ref().is_none_or(|r| !r.fits(w, h)) {
                target.read = Some(ReadBuffer::new(dev, w, h));
            }
            let Some(read) = target.read.as_ref() else {
                return;
            };
            read.record(&mut encoder, &ft.texture);
            dev.queue.submit([encoder.finish()]);
            drop(keep);
            match read.read(dev) {
                Ok(pixels) => reply.send(GpuReply::Pixels {
                    surface,
                    frame: frame.id,
                    pixels,
                }),
                Err(e) => reply.send(GpuReply::Failed {
                    surface: Some(surface),
                    key: None,
                    error: e,
                }),
            }
        }
    }

    fn pass(&mut self, dev: &Device, p: PassFrame, reply: &Reply<'_>) {
        let (w, h) = (p.size.w, p.size.h);
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("strand pass"),
            });
        let drawn = match self
            .passes
            .draw(dev, &mut encoder, &p.pass, w, h, p.globals)
        {
            Ok(Some(d)) => d,
            Ok(None) => return,
            Err(e) => {
                reply.send(GpuReply::Failed {
                    surface: None,
                    key: Some(p.key),
                    error: e,
                });
                return;
            }
        };
        let read = self
            .pass_reads
            .entry(p.key)
            .or_insert_with(|| ReadBuffer::new(dev, w, h));
        if !read.fits(w, h) {
            *read = ReadBuffer::new(dev, w, h);
        }
        read.record(&mut encoder, &drawn.texture);
        dev.queue.submit([encoder.finish()]);
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

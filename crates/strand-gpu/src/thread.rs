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
use crate::draw::{self, DrawnPass, Held, Images, Sampled};
use crate::lru::Lru;
use crate::pass::{self, Passes};
use crate::present::{self, Blit, Presented, Swapchain};
use crate::readback::ReadBuffer;
use crate::timer::Timer;
use crate::{
    Frame, FrameSnapshot, GpuError, GpuErrorKind, GpuMode, GpuOptions, GpuReply, GpuRequest, Op,
    PassFrame, Scale, Size, SurfaceId, Upload,
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
    // (gpu-first) What a first frame would build (vello_gpu's resources,
    // Mesa's pipeline variants, the blits), built now: the device starts
    // with the shell, off its frame path, and the first promoted frame
    // is not the one that pays. A failure here is the start's.
    let warmed = catch_unwind(AssertUnwindSafe(|| state.warm(&dev)));
    if warmed.is_err() || dev.is_lost() {
        reply.send(GpuReply::Unavailable(GpuError::new(
            GpuErrorKind::Device,
            format!("the GPU failed its first frame on {}", info.name),
        )));
        drop(state);
        drop(dev);
        reply.send(GpuReply::Exited);
        return;
    }
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

/// (perf-gpu-retain) The texture id a scene samples the surface's
/// snapshot by (passes are numbered from 1).
const SNAPSHOT_TEXTURE: TextureId = TextureId(u64::MAX);

struct Target {
    size: Size,
    #[allow(dead_code)]
    scale: Scale,
    present: Option<Presented>,
    /// A presented surface's GPU timestamps (none without them).
    timer: Option<Timer>,
    frame: Option<FrameTarget>,
    read: Option<ReadBuffer>,
    /// (perf-gpu-retain) The old frame a crossfade blends from, copied
    /// from `frame` ([`FrameSnapshot::Take`]) and held until a frame
    /// holds none: one surface-sized texture while the fade lasts.
    snapshot: Option<FrameTarget>,
    /// (perf-gpu-retain) The textures its last frame's backdrops drew
    /// what is behind them and their passes into, but those it shows,
    /// reused by the next frame's of the same size; those the next frame
    /// does not use are freed (review: the pass outputs too, so a
    /// backdrop drawn every frame allocates nothing).
    backdrops: pass::Pool,
    /// (perf-gpu-retain) Its last frame's backdrops' pixels by key
    /// ([`Op::Backdrop`]'s `key`): a backdrop of the next frame with the
    /// same key is drawn from them, nothing rendered or run; those the
    /// next frame does not draw are freed.
    kept: Vec<(u64, pass::Drawn)>,
}

/// (perf-gpu-retain) A surface's backdrop textures while a frame draws.
#[derive(Default)]
struct Backdrops {
    /// The textures its backdrops render and run passes into.
    pool: pass::Pool,
    /// Its last frame's backdrops' pixels by key, and this frame's.
    kept: Vec<(u64, pass::Drawn)>,
    now: Vec<(u64, pass::Drawn)>,
    /// This frame's backdrops' pixels drawn anew every frame (key 0),
    /// back in the pool once the frame no longer reads them.
    once: Vec<pass::Drawn>,
}

impl Target {
    fn new(size: Size, scale: Scale, present: Option<Presented>, timer: Option<Timer>) -> Self {
        Target {
            size,
            scale,
            present,
            timer,
            frame: None,
            read: None,
            snapshot: None,
            backdrops: pass::Pool::default(),
            kept: Vec::new(),
        }
    }
}

/// (perf-gpu-retain) The warm-up frame's ops for what the GPU draws in
/// place of the CPU's uploads: a shadow in its ring, a radial gradient,
/// and a backdrop blur and glass in an additive layer.
fn warm_retained() -> Vec<Op> {
    use crate::kurbo::{Affine, BezPath, Rect, Shape};
    use crate::{AlphaColor, Brush, Layer, PassGlobals, ShaderPass, peniko};
    use strand_scene::{Bundled, ShaderInput, ShaderRef};
    let blue: AlphaColor<crate::Srgb> = AlphaColor::new([0.2, 0.4, 0.9, 1.0]);
    let pink: AlphaColor<crate::Srgb> = AlphaColor::new([0.9, 0.5, 0.7, 0.8]);
    let mut ring = BezPath::new();
    ring.extend(Rect::new(0.0, 0.0, 64.0, 64.0).path_elements(0.1));
    ring.extend(Rect::new(16.0, 16.0, 48.0, 48.0).path_elements(0.1));
    let radial = peniko::Gradient::new_radial((32.0, 32.0), 24.0).with_stops([blue, pink]);
    let behind = vec![Op::Fill {
        path: Rect::new(0.0, 0.0, 64.0, 64.0).to_path(0.1),
        brush: Brush::Gradient(radial),
        brush_transform: Affine::IDENTITY,
        even_odd: false,
    }];
    let pass = |b: Bundled, u: &[f32]| ShaderPass {
        code: ShaderRef::Bundled(b),
        uniforms: std::sync::Arc::from(u),
        input: ShaderInput::Content,
    };
    let backdrop = |x0: f64, b: Bundled, u: &[f32]| Op::Backdrop {
        region: Rect::new(x0, 0.0, x0 + 32.0, 32.0),
        ops: behind.clone(),
        pass: pass(b, u),
        then: Vec::new(),
        globals: PassGlobals {
            time: 0.0,
            scale: 1.0,
            pointer: [8.0, 8.0],
        },
        key: 0,
        // A blur at half size, as a small one is drawn.
        reduce: if b == Bundled::BackdropBlur { 2 } else { 1 },
    };
    let mut ops = behind.clone();
    ops.extend([
        Op::PushClipEvenOdd(ring),
        Op::BlurredRect {
            rect: Rect::new(16.0, 16.0, 48.0, 48.0),
            radius: 8.0,
            std_dev: 6.0,
            color: AlphaColor::new([0.0, 0.0, 0.0, 0.4]),
        },
        Op::PopClip,
        Op::PushLayer(Layer {
            blend: Some(peniko::BlendMode::new(
                peniko::Mix::Normal,
                peniko::Compose::Plus,
            )),
            opacity: Some(0.5),
            ..Default::default()
        }),
        backdrop(0.0, Bundled::BackdropBlur, &[4.0]),
        backdrop(32.0, Bundled::Glass, &[12.0, 12.0, 0.6, 2.0]),
        Op::PopLayer,
    ]);
    ops
}

/// A texture frames, snapshots and backdrops are drawn in or copied to.
fn frame_texture(dev: &Device, width: u32, height: u32, label: &str) -> FrameTarget {
    let texture = dev.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FRAME_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    FrameTarget {
        texture,
        view,
        width,
        height,
    }
}

struct State {
    renderer: vello_gpu::Renderer,
    resources: vello_gpu::Resources,
    scene: vello_gpu::Scene,
    /// (perf-gpu-retain) The scene a backdrop's ops are built in.
    inner: vello_gpu::Scene,
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
            inner: vello_gpu::Scene::new(1, 1),
            images: Images::new(),
            passes: Passes::new(dev),
            blit: Blit::default(),
            targets: HashMap::new(),
            pass_reads: Lru::new(PASS_READS),
        }
    }

    /// (gpu-first) Draws a small frame offscreen (fills, a gradient, an
    /// image, a clip and a layer) and records the present blit for both
    /// swapchain formats, then waits for the GPU: the pipelines and
    /// resources a first frame builds are built. Nothing is answered and
    /// nothing is kept but what any frame keeps. (perf-gpu-retain) The
    /// frame also draws what a first animation's frame may: a box shadow
    /// in its even-odd ring, a radial gradient, a backdrop blur and glass
    /// in an additive layer, and a second frame a crossfade's snapshot
    /// taken from the first, so none of their pipelines is built in a
    /// shown frame.
    fn warm(&mut self, dev: &Device) {
        use crate::kurbo::{Affine, Rect, Shape};
        use crate::{AlphaColor, Brush, Layer, peniko};
        const WARM: SurfaceId = SurfaceId(u32::MAX);
        const IMAGE: u64 = u64::MAX;
        let size = Size::new(64, 64);
        self.targets
            .insert(WARM, Target::new(size, Scale::ONE, None, None));
        let blue = AlphaColor::new([0.2, 0.4, 0.9, 1.0]);
        let pink = AlphaColor::new([0.9, 0.5, 0.7, 0.8]);
        let card = Rect::new(4.0, 4.0, 60.0, 60.0)
            .to_rounded_rect(12.0)
            .to_path(0.1);
        let gradient =
            peniko::Gradient::new_linear((0.0, 0.0), (64.0, 64.0)).with_stops([blue, pink]);
        let ops = vec![
            Op::Fill {
                path: card.clone(),
                brush: Brush::Solid(blue),
                brush_transform: Affine::IDENTITY,
                even_odd: false,
            },
            Op::PushClip(card.clone()),
            Op::PushLayer(Layer {
                clip: None,
                blend: None,
                opacity: Some(0.5),
                blur: None,
            }),
            Op::Fill {
                path: Rect::new(8.0, 8.0, 56.0, 32.0).to_path(0.1),
                brush: Brush::Gradient(gradient),
                brush_transform: Affine::IDENTITY,
                even_odd: false,
            },
            Op::PopLayer,
            Op::Image {
                rect: Rect::new(8.0, 36.0, 24.0, 52.0),
                image: IMAGE,
                image_transform: Affine::scale(4.0).then_translate((8.0, 36.0).into()),
                tint: Some(pink),
                smooth: true,
            },
            Op::Image {
                rect: Rect::new(32.0, 36.0, 48.0, 52.0),
                image: IMAGE,
                image_transform: Affine::scale(4.0).then_translate((32.0, 36.0).into()),
                tint: None,
                smooth: false,
            },
            Op::PopClip,
        ];
        let ops = [ops, warm_retained()].concat();
        let frame = Frame {
            surface: WARM,
            id: 0,
            size,
            scale: Scale::ONE,
            ops,
            uploads: vec![Upload {
                id: IMAGE,
                generation: 0,
                pixmap: std::sync::Arc::new(crate::Pixmap::new(4, 4)),
            }],
            retire: Vec::new(),
            clear: AlphaColor::TRANSPARENT,
            snapshot: FrameSnapshot::None,
        };
        // Its `Pixels` (a readback, which waits for the GPU) goes nowhere.
        let (tx, _rx) = mpsc::channel();
        let quiet = Reply {
            tx: &tx,
            waker: &|| {},
        };
        if let Err(e) = self.draw_frame(dev, frame, Duration::ZERO, &quiet, &mut Phases::new()) {
            log::debug!("GPU: the warm-up frame: {e}");
        }
        // A crossfade's first frame: the snapshot taken and drawn under
        // the new frame.
        let whole = Rect::new(0.0, 0.0, 64.0, 64.0);
        let fade = Frame {
            surface: WARM,
            id: 1,
            size,
            scale: Scale::ONE,
            ops: vec![
                Op::PushLayer(Layer {
                    opacity: Some(0.5),
                    ..Default::default()
                }),
                Op::Snapshot { rect: whole },
                Op::PopLayer,
            ],
            uploads: Vec::new(),
            retire: Vec::new(),
            clear: AlphaColor::TRANSPARENT,
            snapshot: FrameSnapshot::Take,
        };
        if let Err(e) = self.draw_frame(dev, fade, Duration::ZERO, &quiet, &mut Phases::new()) {
            log::debug!("GPU: the warm-up crossfade: {e}");
        }
        self.targets.remove(&WARM);
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("strand warm-up"),
            });
        if let Some(h) = self.images.remove(&IMAGE) {
            self.renderer
                .destroy_image(&mut self.resources, &mut encoder, h.id);
        }
        // The present blit, for both formats a swapchain may have.
        let texture = |format, usage| {
            dev.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("strand warm-up"),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        let src = texture(FRAME_FORMAT, wgpu::TextureUsages::TEXTURE_BINDING);
        for format in [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ] {
            let dst = texture(format, wgpu::TextureUsages::RENDER_ATTACHMENT);
            self.blit.record(dev, &mut encoder, &src, &dst, format);
        }
        dev.queue.submit([encoder.finish()]);
        if let Err(e) = crate::readback::finish(dev, None, "the warm-up") {
            log::debug!("GPU: {e}");
        }
    }

    /// Handles a request that sat `queued` in the channel.
    fn handle(&mut self, dev: &Device, req: GpuRequest, queued: Duration, reply: &Reply<'_>) {
        let start = Instant::now();
        let what = match &req {
            GpuRequest::Attach { .. } => Some("an attach"),
            GpuRequest::Resize { .. } => Some("a resize"),
            GpuRequest::Release(_) => Some("a release"),
            GpuRequest::Commit(_) => Some("a commit"),
            GpuRequest::Pass(_) => Some("a pass"),
            _ => None,
        };
        self.handle_request(dev, req, queued, reply);
        if let Some(what) = what
            && start.elapsed() >= SLOW_FRAME
        {
            log::debug!(
                "GPU: {what} took {:.1} ms",
                start.elapsed().as_secs_f64() * 1e3
            );
        }
    }

    fn handle_request(
        &mut self,
        dev: &Device,
        req: GpuRequest,
        queued: Duration,
        reply: &Reply<'_>,
    ) {
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
                self.targets
                    .insert(surface, Target::new(size, scale, present, timer));
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
            GpuRequest::Commit(surface) => self.commit(dev, surface),
            GpuRequest::Frame(frame) => self.frame(dev, frame, queued, reply),
            GpuRequest::Pass(pass) => self.pass(dev, pass, reply),
            GpuRequest::Shutdown => {}
        }
    }

    /// (gpu-first review 2) Presents `surface`'s last frame again: its
    /// commit carries the surface state the manager set since (a new
    /// size), whose configure the renderer holds the next frame for.
    fn commit(&mut self, dev: &Device, surface: SurfaceId) {
        let Some(t) = self.targets.get_mut(&surface) else {
            return;
        };
        let (Some(p), Some(ft)) = (t.present.as_mut(), t.frame.as_ref()) else {
            return;
        };
        if (ft.width, ft.height) != (p.config.width, p.config.height) {
            return;
        }
        let mut encoder = dev
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("strand commit"),
            });
        let Some((image, dst, format)) = p.acquire(dev) else {
            return;
        };
        self.blit.record(dev, &mut encoder, &ft.view, &dst, format);
        dev.queue.submit([encoder.finish()]);
        p.present(dev, image);
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

    /// Draws the [`Op::Pass`]es among `ops` (and inside their backdrops)
    /// into their own textures, numbering them as [`Op::passes_in`]
    /// does from `*next` (a backdrop's number is skipped here: its
    /// pixels are drawn by [`State::draw_backdrops`]). A pass that fails
    /// draws nothing; the first failure is returned.
    #[allow(clippy::too_many_arguments)]
    fn draw_passes(
        &mut self,
        dev: &Device,
        encoder: &mut wgpu::CommandEncoder,
        ops: &[Op],
        next: &mut usize,
        out: &mut [Option<DrawnPass>],
        bindings: &mut vello_gpu::TextureBindings,
        keep: &mut Vec<pass::Drawn>,
    ) -> Option<GpuError> {
        let mut failed = None;
        for op in ops {
            match op {
                Op::Pass {
                    pass,
                    bounds,
                    globals,
                } => {
                    let at = *next;
                    *next += 1;
                    let (w, h) = (bounds.width().ceil(), bounds.height().ceil());
                    if !(w >= 1.0 && h >= 1.0 && w <= 16384.0 && h <= 16384.0) {
                        continue;
                    }
                    match self
                        .passes
                        .draw(dev, encoder, pass, w as u32, h as u32, *globals, None)
                    {
                        Ok(Some(d)) => {
                            let id = TextureId(at as u64 + 1);
                            bindings.insert(id, d.view.clone());
                            if let Some(slot) = out.get_mut(at) {
                                *slot = Some(DrawnPass {
                                    texture: id,
                                    width: d.width as u16,
                                    height: d.height as u16,
                                });
                            }
                            keep.push(d);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            failed.get_or_insert(e);
                        }
                    }
                }
                Op::Backdrop { ops: inner, .. } => {
                    *next += 1;
                    if let Some(e) =
                        self.draw_passes(dev, encoder, inner, next, out, bindings, keep)
                    {
                        failed.get_or_insert(e);
                    }
                }
                _ => {}
            }
        }
        failed
    }

    /// (perf-gpu-retain) Draws the backdrops among `ops`, each after the
    /// backdrops inside its own ops: what is behind it (its ops) into a
    /// texture of its region (over `reduce`), then its passes over that,
    /// in a submission of its own (vello stages a render's buffers
    /// through the queue, so two renders share one only if submitted
    /// apart). Numbered as [`State::draw_passes`]; every texture comes
    /// from the surface's pool `tex`, and goes back to it once read.
    #[allow(clippy::too_many_arguments)]
    fn draw_backdrops(
        &mut self,
        dev: &Device,
        ops: &[Op],
        next: &mut usize,
        out: &mut [Option<DrawnPass>],
        bindings: &mut vello_gpu::TextureBindings,
        tex: &mut Backdrops,
    ) -> Result<(), GpuError> {
        for op in ops {
            match op {
                Op::Pass { .. } => *next += 1,
                Op::Backdrop {
                    region,
                    ops: inner,
                    pass,
                    then,
                    globals,
                    key,
                    reduce,
                } => {
                    let at = *next;
                    *next += 1;
                    // The same pixels as the last frame's: drawn from them.
                    let found = (*key != 0)
                        .then(|| tex.kept.iter().position(|(k, _)| k == key))
                        .flatten();
                    if let Some(i) = found {
                        *next += Op::passes_in(inner);
                        let (k, d) = tex.kept.swap_remove(i);
                        let id = TextureId(at as u64 + 1);
                        bindings.insert(id, d.view.clone());
                        if let Some(slot) = out.get_mut(at) {
                            *slot = Some(DrawnPass {
                                texture: id,
                                width: d.width as u16,
                                height: d.height as u16,
                            });
                        }
                        tex.now.push((k, d));
                        continue;
                    }
                    self.draw_backdrops(dev, inner, next, out, bindings, tex)?;
                    let max = dev
                        .device
                        .limits()
                        .max_texture_dimension_2d
                        .min(u32::from(u16::MAX));
                    let k = f64::from((*reduce).clamp(1, 16));
                    let (x, y) = (region.x0.round(), region.y0.round());
                    let (w, h) = (region.width().round(), region.height().round());
                    let (w, h) = ((w / k).ceil(), (h / k).ceil());
                    if !(w >= 1.0 && h >= 1.0 && w <= f64::from(max) && h <= f64::from(max)) {
                        continue;
                    }
                    let (w, h) = (w as u32, h as u32);
                    let target = tex.pool.take(dev, w, h);
                    self.inner.reset_and_resize(w as u16, h as u16);
                    let sampled = Sampled {
                        passes: out,
                        snapshot: None,
                    };
                    let missing = draw::build(
                        &mut self.inner,
                        inner,
                        &self.images,
                        &sampled,
                        at + 1,
                        vello_common::kurbo::Affine::scale(1.0 / k) * kurbo_translate(-x, -y),
                    );
                    if missing.images > 0 {
                        log::debug!("GPU backdrop: {} images not uploaded", missing.images);
                    }
                    let mut encoder =
                        dev.device
                            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: Some("strand backdrop"),
                            });
                    let rendered = self.renderer.render(
                        &self.inner,
                        &mut self.resources,
                        &dev.device,
                        &dev.queue,
                        &mut encoder,
                        &vello_gpu::RenderSize {
                            width: w as u16,
                            height: h as u16,
                        },
                        &target.view,
                        None,
                        bindings,
                        vello_gpu::TargetInit::Clear(vello_gpu::ClearSettings::Viewport {
                            color: crate::AlphaColor::TRANSPARENT,
                        }),
                    );
                    if let Err(e) = rendered {
                        dev.queue.submit([encoder.finish()]);
                        tex.pool.give(target);
                        return Err(render_error(e.to_string()));
                    }
                    // Each pass reads the one before's pixels, which go
                    // back to the pool once read, what is behind first.
                    let mut drawn = Some(target);
                    for p in std::iter::once(pass).chain(then) {
                        let Some(input) = drawn.take() else { break };
                        match self.passes.draw_pooled(
                            dev,
                            &mut encoder,
                            p,
                            *globals,
                            input,
                            &mut tex.pool,
                        ) {
                            Ok(d) => drawn = d,
                            Err(e) => {
                                dev.queue.submit([encoder.finish()]);
                                return Err(e);
                            }
                        }
                    }
                    dev.queue.submit([encoder.finish()]);
                    if let Some(d) = drawn {
                        let id = TextureId(at as u64 + 1);
                        bindings.insert(id, d.view.clone());
                        if let Some(slot) = out.get_mut(at) {
                            *slot = Some(DrawnPass {
                                texture: id,
                                width: d.width as u16,
                                height: d.height as u16,
                            });
                        }
                        if *key != 0 {
                            tex.now.push((*key, d));
                        } else {
                            tex.once.push(d);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// (perf-gpu-retain) Takes, keeps or frees `surface`'s snapshot as a
    /// `w × h` frame asks ([`Frame::snapshot`]): a take copies its last
    /// frame, in `encoder` before the frame draws over it.
    fn snapshot(
        &mut self,
        dev: &Device,
        encoder: &mut wgpu::CommandEncoder,
        surface: SurfaceId,
        how: FrameSnapshot,
        (w, h): (u32, u32),
    ) {
        let Some(t) = self.targets.get_mut(&surface) else {
            return;
        };
        match how {
            FrameSnapshot::None => t.snapshot = None,
            FrameSnapshot::Keep => {}
            FrameSnapshot::Take => {
                let Some(ft) = t.frame.as_ref().filter(|f| f.width == w && f.height == h) else {
                    log::debug!("GPU: {surface:?} has no {w}×{h} frame to snapshot");
                    t.snapshot = None;
                    return;
                };
                let snap = match t.snapshot.take() {
                    Some(s) if s.width == w && s.height == h => s,
                    _ => frame_texture(dev, w, h, "strand snapshot"),
                };
                encoder.copy_texture_to_texture(
                    ft.texture.as_image_copy(),
                    snap.texture.as_image_copy(),
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
                t.snapshot = Some(snap);
            }
        }
    }

    /// Draws a frame; every frame is answered exactly once (`Presented`,
    /// `Pixels` or `Failed`), since the host sends a surface's next frame
    /// only after the answer to its last. A frame whose inline pass fails
    /// is answered `Failed` alone.
    fn frame(&mut self, dev: &Device, frame: Frame, queued: Duration, reply: &Reply<'_>) {
        let surface = frame.surface;
        let id = frame.id;
        let mut ph = Phases::new();
        if let Err(error) = self.draw_frame(dev, frame, queued, reply, &mut ph) {
            reply.send(GpuReply::Failed {
                surface: Some(surface),
                key: None,
                error,
            });
        }
        ph.done(surface, id);
    }

    fn draw_frame(
        &mut self,
        dev: &Device,
        frame: Frame,
        queued: Duration,
        reply: &Reply<'_>,
        ph: &mut Phases,
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
        // Before anything draws into the frame texture: the snapshot of
        // the frame it holds.
        self.snapshot(dev, &mut encoder, surface, frame.snapshot, (w, h));
        self.upload(dev, &mut encoder, &frame.uploads, &frame.retire);
        ph.mark("upload");
        ph.uploads = (
            frame.uploads.len(),
            frame
                .uploads
                .iter()
                .map(|u| u64::from(u.pixmap.width()) * u64::from(u.pixmap.height()))
                .sum(),
        );
        let mut bindings = vello_gpu::TextureBindings::new();
        let mut keep = Vec::new();
        let mut drawn: Vec<Option<DrawnPass>> = vec![None; Op::passes_in(&frame.ops)];
        let pass_error = self.draw_passes(
            dev,
            &mut encoder,
            &frame.ops,
            &mut 0,
            &mut drawn,
            &mut bindings,
            &mut keep,
        );
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
        ph.mark("passes");
        // (perf-gpu-retain) Backdrops: what the uploads and passes above
        // draw is submitted first, each backdrop's render after, the
        // frame's own last.
        if frame.ops.iter().any(|o| matches!(o, Op::Backdrop { .. })) {
            dev.queue.submit([encoder.finish()]);
            let mut tex = self
                .targets
                .get_mut(&surface)
                .map(|t| Backdrops {
                    pool: std::mem::take(&mut t.backdrops),
                    kept: std::mem::take(&mut t.kept),
                    ..Backdrops::default()
                })
                .unwrap_or_default();
            // (review) The last frame's pixels no backdrop of this frame
            // keeps are free for this frame's to draw into.
            let mut keys = Vec::new();
            backdrop_keys(&frame.ops, &mut keys);
            for (k, d) in std::mem::take(&mut tex.kept) {
                if keys.contains(&k) {
                    tex.kept.push((k, d));
                } else {
                    tex.pool.spare(d);
                }
            }
            let result =
                self.draw_backdrops(dev, &frame.ops, &mut 0, &mut drawn, &mut bindings, &mut tex);
            // What this frame drew anew is read by its scene, submitted
            // after it: the next frame's to reuse.
            for d in std::mem::take(&mut tex.once) {
                tex.pool.give(d);
            }
            tex.pool.end_frame();
            if let Some(t) = self.targets.get_mut(&surface) {
                t.backdrops = std::mem::take(&mut tex.pool);
                t.kept = std::mem::take(&mut tex.now);
            }
            drop(tex);
            if let Err(e) = result {
                drop(keep);
                return Err(e);
            }
            encoder = dev
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("strand frame"),
                });
            ph.mark("backdrops");
        } else if let Some(t) = self.targets.get_mut(&surface) {
            t.backdrops = pass::Pool::default();
            t.kept.clear();
        }
        self.scene.reset_and_resize(w as u16, h as u16);
        let snapshot = self
            .targets
            .get(&surface)
            .and_then(|t| t.snapshot.as_ref())
            .map(|s| {
                bindings.insert(SNAPSHOT_TEXTURE, s.view.clone());
                DrawnPass {
                    texture: SNAPSHOT_TEXTURE,
                    width: s.width as u16,
                    height: s.height as u16,
                }
            });
        let sampled = Sampled {
            passes: &drawn,
            snapshot,
        };
        let missing = draw::build(
            &mut self.scene,
            &frame.ops,
            &self.images,
            &sampled,
            0,
            kurbo_translate(0.0, 0.0),
        );
        ph.mark("build");
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
            target.frame = Some(frame_texture(dev, w, h, "strand frame"));
        }
        let Some(ft) = target.frame.as_ref() else {
            return Err(render_error("no frame texture"));
        };
        ph.mark("texture");
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
        ph.mark("render");
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
                Some(ph),
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
            ph.mark("submit");
            drop(keep);
            let pixels = read.read(dev)?;
            ph.mark("read");
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

/// (gpu-first) Where a frame's time on this thread went, for the log's
/// slow-frame line: wall-clock marks, and the thread's own run and
/// run-queue time from `/proc/thread-self/schedstat` (a frame slow while
/// the thread waited for a core is the host's load, one slow while it
/// ran is ours).
struct Phases {
    start: Instant,
    sched: Option<(u64, u64, u64)>,
    marks: Vec<(&'static str, Instant)>,
    /// Images uploaded and their pixels.
    uploads: (usize, u64),
}

/// A frame slower than this on the thread is logged with its phases
/// (render's main thread logs a paint as slow from the same figure).
pub const SLOW_FRAME: Duration = Duration::from_millis(40);

/// (perf-bench) Whether every frame's phases are kept for
/// [`take_frame_phases`] ([`record_frame_phases`]).
static RECORDING: AtomicBool = AtomicBool::new(false);

/// (perf-bench) The frames' phases kept while recording, at most
/// [`PHASES_KEPT`] (the oldest go first).
static RECORDED: std::sync::Mutex<Vec<FramePhases>> = std::sync::Mutex::new(Vec::new());

/// (perf-bench) Frames' phases kept while recording, at most.
const PHASES_KEPT: usize = 4096;

/// (perf-bench) Where one frame's time on the GPU thread went: its
/// surface and render's frame number, the whole span, each phase as the
/// thread names it (`upload`, `passes`, `build` (the vello_gpu scene
/// encoded from the ops), `texture`, `render` (vello_gpu's preparation
/// and recording), then `submit` and `read` for a readback, or the
/// present's own marks), and the uploads with their pixels. Wall-clock,
/// advisory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FramePhases {
    pub surface: Option<SurfaceId>,
    pub frame: u64,
    pub total: Duration,
    pub phases: Vec<(&'static str, Duration)>,
    pub uploads: (usize, u64),
}

/// (perf-bench) Keeps every frame's phases from now on (`true`) or stops
/// and forgets them (`false`), for a benchmark ([`take_frame_phases`]).
/// Off by default; off, a frame pays one atomic load for it.
pub fn record_frame_phases(on: bool) {
    RECORDING.store(on, Ordering::Relaxed);
    if !on && let Ok(mut kept) = RECORDED.lock() {
        kept.clear();
    }
}

/// (perf-bench) The frames' phases kept since the last call, oldest first.
pub fn take_frame_phases() -> Vec<FramePhases> {
    RECORDED
        .lock()
        .map(|mut kept| std::mem::take(&mut *kept))
        .unwrap_or_default()
}

/// This thread's time on a core and waiting for one (ns), and its major
/// page faults (the disk read code or data back in).
pub fn thread_stats() -> Option<(u64, u64, u64)> {
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let mut it = s.split_whitespace().map(|v| v.parse::<u64>().ok());
    let (ran, queued) = (it.next()??, it.next()??);
    let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
    Some((ran, queued, majflt(&stat)?))
}

/// The major faults of a `/proc/…/stat` line: after the `(comm)`, whose
/// name may hold spaces and parentheses, the tenth field (field 12).
fn majflt(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(9)?.parse().ok()
}

/// `thread_stats` since `before`, for a log line.
pub fn thread_stats_since(before: Option<(u64, u64, u64)>) -> String {
    match (before, thread_stats()) {
        (Some((r0, q0, f0)), Some((r1, q1, f1))) => format!(
            " ran={:.1} runqueue={:.1} majflt={}",
            r1.saturating_sub(r0) as f64 / 1e6,
            q1.saturating_sub(q0) as f64 / 1e6,
            f1.saturating_sub(f0)
        ),
        _ => String::new(),
    }
}

impl Phases {
    fn new() -> Self {
        let on = log::log_enabled!(log::Level::Debug) || RECORDING.load(Ordering::Relaxed);
        Phases {
            start: Instant::now(),
            sched: on.then(thread_stats).flatten(),
            marks: if on {
                Vec::with_capacity(8)
            } else {
                Vec::new()
            },
            uploads: (0, 0),
        }
    }

    /// Marks the end of a phase (only while the debug log is on).
    fn mark(&mut self, what: &'static str) {
        if self.marks.capacity() > 0 {
            self.marks.push((what, Instant::now()));
        }
    }

    /// Logs the frame's phases if it was slow (and keeps them while
    /// recording, [`record_frame_phases`]).
    fn done(&self, surface: SurfaceId, frame: u64) {
        let took = self.start.elapsed();
        if RECORDING.load(Ordering::Relaxed) {
            let mut last = self.start;
            let phases = self
                .marks
                .iter()
                .map(|(what, at)| {
                    let d = at.saturating_duration_since(last);
                    last = *at;
                    (*what, d)
                })
                .collect();
            if let Ok(mut kept) = RECORDED.lock() {
                if kept.len() >= PHASES_KEPT {
                    kept.remove(0);
                }
                kept.push(FramePhases {
                    surface: Some(surface),
                    frame,
                    total: took,
                    phases,
                    uploads: self.uploads,
                });
            }
        }
        if took < SLOW_FRAME || !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let mut last = self.start;
        let mut parts = String::new();
        for (what, at) in &self.marks {
            parts.push_str(&format!(
                " {what}={:.1}",
                at.saturating_duration_since(last).as_secs_f64() * 1e3
            ));
            last = *at;
        }
        let sched = thread_stats_since(self.sched);
        log::debug!(
            "GPU: a slow frame of {surface:?}: {:.1} ms:{parts}{sched} uploads={} ({} px)",
            took.as_secs_f64() * 1e3,
            self.uploads.0,
            self.uploads.1
        );
    }
}

/// (perf-gpu-retain review) The keys of the backdrops among `ops`,
/// those inside backdrops included.
fn backdrop_keys(ops: &[Op], out: &mut Vec<u64>) {
    for op in ops {
        if let Op::Backdrop { key, ops, .. } = op {
            if *key != 0 {
                out.push(*key);
            }
            backdrop_keys(ops, out);
        }
    }
}

fn kurbo_translate(x: f64, y: f64) -> vello_common::kurbo::Affine {
    vello_common::kurbo::Affine::translate((x, y))
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
    mut ph: Option<&mut Phases>,
) -> Result<GpuReply, GpuError> {
    let acquiring = Instant::now();
    let image = chain.acquire(dev);
    let waited = queued + acquiring.elapsed();
    if let Some(p) = ph.as_deref_mut() {
        p.mark("acquire");
    }
    let Some((image, dst, format)) = image else {
        return Err(render_error("no swapchain image"));
    };
    blit.record(dev, &mut encoder, src, &dst, format);
    if let Some((t, Some(slot))) = &timer {
        t.end(&mut encoder, *slot);
    }
    dev.queue.submit([encoder.finish()]);
    let at = Instant::now();
    if let Some(p) = ph.as_deref_mut() {
        p.mark("submit");
    }
    chain.present(dev, image);
    if let Some(p) = ph {
        p.mark("present");
    }
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
            None,
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

    /// (gpu-first review 2) The slow-frame line's figures: this thread's
    /// time on a core grows with its work, and the major faults are read
    /// past a thread name with spaces and parentheses.
    #[test]
    fn a_threads_scheduling_figures_are_read() {
        let before = thread_stats().expect("/proc/thread-self");
        let spin = Instant::now();
        let mut x = 0u64;
        while spin.elapsed() < Duration::from_millis(20) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let after = thread_stats().expect("/proc/thread-self");
        assert!(after.0 >= before.0 + 10_000_000, "{before:?} {after:?}");
        assert!(after.2 >= before.2);
        let line = "42 (a (b) c) S 1 42 42 0 -1 4194560 812 0 7 0 1 2 0 0 20 0 1 0";
        assert_eq!(majflt(line), Some(7));
        assert_eq!(majflt("42 (x"), None);
        assert!(thread_stats_since(Some(before)).contains(" ran="));
        assert_eq!(thread_stats_since(None), "");
    }

    /// (perf-gpu-retain review) A backdrop drawn anew every frame (its
    /// key changes, as when what is behind it moves) makes no texture
    /// once the surface's pool has its two: what is behind it and its
    /// pixels, which swap roles each frame (a blur's two steps draw into
    /// them in turn). At a quarter of its size it pools its own; drawn
    /// again from its last pixels (the same key) it holds those alone,
    /// and a frame without backdrops frees them.
    #[test]
    fn a_backdrop_drawn_every_frame_reuses_its_textures() {
        let dev = match Device::open(&GpuOptions::from_env()) {
            Ok(d) => d,
            Err(e) => {
                let required = std::env::var("STRAND_REQUIRE_GPU").is_ok_and(|v| v == "1");
                assert!(!required, "STRAND_REQUIRE_GPU=1 but no GPU: {e}");
                eprintln!("skipped: no GPU ({e})");
                return;
            }
        };
        use crate::kurbo::{Affine, Rect, Shape};
        use strand_scene::{Bundled, ShaderInput, ShaderRef};
        let mut st = State::new(&dev);
        let s = SurfaceId(1);
        let size = Size::new(64, 32);
        st.targets
            .insert(s, Target::new(size, Scale::ONE, None, None));
        let (tx, rx) = mpsc::channel();
        let waker = || {};
        let reply = Reply {
            tx: &tx,
            waker: &waker,
        };
        let fill = Op::Fill {
            path: Rect::new(0.0, 0.0, 32.0, 32.0).to_path(0.1),
            brush: crate::Brush::Solid(crate::AlphaColor::new([1.0, 0.0, 0.0, 1.0])),
            brush_transform: Affine::IDENTITY,
            even_odd: false,
        };
        let frame = |id: u64, key: u64, reduce: u32, backdrop: bool| Frame {
            surface: s,
            id,
            size,
            scale: Scale::ONE,
            ops: if backdrop {
                vec![
                    fill.clone(),
                    Op::Backdrop {
                        region: Rect::new(8.0, 4.0, 56.0, 28.0),
                        ops: vec![fill.clone()],
                        pass: crate::ShaderPass {
                            code: ShaderRef::Bundled(Bundled::BackdropBlur),
                            uniforms: std::sync::Arc::from([3.0f32].as_slice()),
                            input: ShaderInput::Content,
                        },
                        then: Vec::new(),
                        globals: crate::PassGlobals::default(),
                        key,
                        reduce,
                    },
                ]
            } else {
                vec![fill.clone()]
            },
            uploads: Vec::new(),
            retire: Vec::new(),
            clear: crate::AlphaColor::TRANSPARENT,
            snapshot: FrameSnapshot::None,
        };
        let made = || pass::TARGETS_MADE.with(std::cell::Cell::get);
        let held = |st: &State| {
            st.targets
                .get(&s)
                .map_or(0, |t| t.backdrops.len() + t.kept.len())
        };
        let mut id = 0;
        let mut draw = |st: &mut State, key: u64, reduce: u32, backdrop: bool| {
            id += 1;
            st.frame(
                &dev,
                frame(id, key, reduce, backdrop),
                Duration::ZERO,
                &reply,
            );
            match rx.try_recv() {
                Ok(GpuReply::Pixels { .. }) => {}
                other => panic!("frame {id}: {other:?}"),
            }
        };
        for reduce in [1, 4] {
            for key in 1..=2 {
                draw(&mut st, key + u64::from(reduce) * 100, reduce, true);
            }
            let before = made();
            for key in 3..=8 {
                draw(&mut st, key + u64::from(reduce) * 100, reduce, true);
                assert_eq!(made(), before, "reduce {reduce}, key {key}: a texture made");
                assert_eq!(held(&st), 2, "reduce {reduce}, key {key}");
            }
            // The same key: its last pixels, alone.
            draw(&mut st, 8 + u64::from(reduce) * 100, reduce, true);
            assert_eq!(made(), before);
            assert_eq!(held(&st), 1, "reduce {reduce}: kept alone");
        }
        draw(&mut st, 0, 1, false);
        assert_eq!(held(&st), 0, "freed");
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

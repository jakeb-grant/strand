//! M4 wave-0 GPU spike (docs/decisions.md, m4-gpu-spike). Not part of the
//! strand workspace; throwaway code that answers three questions:
//!
//! - `present`: can wgpu 30 (Vulkan) + vello_gpu 0.3 present to a
//!   wlr-layer-shell surface on the compositor in WAYLAND_DISPLAY?
//! - `readback`: render offscreen, read back, attach as a wl_shm buffer.
//! - `mem`: PSS, threads and Vulkan mappings before the device, with the
//!   device up after a few frames, and after everything is dropped.
//!
//! Usage: gpu-present present|readback|mem [FRAMES]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use vello_common::kurbo::{Affine, Circle, Rect, Shape};
use vello_common::peniko::color::{AlphaColor, Srgb};
use vello_gpu::Scene;
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

const W: u32 = 256;
const H: u32 = 128;

struct App {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    configured: bool,
}

fn field(s: &str, name: &str) -> u64 {
    s.lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
/// Mapped files whose path names a Vulkan/Mesa/LLVM library.
fn vk_maps() -> Vec<String> {
    let s = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let mut v: Vec<String> = s
        .lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .filter(|p| {
            [
                "vulkan", "lvp", "LLVM", "mesa", "gallium", "libdrm", "libz3", "libelf", "libedit",
            ]
            .iter()
            .any(|k| p.contains(k))
        })
        .map(|p| p.rsplit('/').next().unwrap_or(p).to_owned())
        .collect();
    v.sort();
    v.dedup();
    v
}
fn report(label: &str) {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    // SAFETY: glibc's malloc_trim has no preconditions.
    unsafe { malloc_trim(0) };
    let roll = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let maps = vk_maps();
    println!(
        "MEM {label:<30} pss={:>7} KiB anon={:>7} KiB rss={:>7} KiB threads={:>2} vk_libs={} {:?}",
        field(&roll, "Pss:"),
        field(&roll, "Pss_Anon:"),
        field(&roll, "Rss:"),
        field(&status, "Threads:"),
        maps.len(),
        maps
    );
}

fn scene(t: f64) -> Scene {
    let mut s = Scene::new(W as u16, H as u16);
    s.set_paint(AlphaColor::<Srgb>::from_rgba8(30, 30, 46, 255));
    s.fill_rect(&Rect::new(0.0, 0.0, W as f64, H as f64));
    s.set_paint(AlphaColor::<Srgb>::from_rgba8(243, 139, 168, 255));
    s.set_transform(Affine::translate((t * 2.0 % 128.0, 0.0)));
    s.fill_path(&Circle::new((64.0, 64.0), 40.0).to_path(0.1));
    s
}

struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu(surface: Option<&wgpu::Surface<'_>>, instance: wgpu::Instance) -> Gpu {
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: surface,
        ..Default::default()
    }))
    .expect("adapter");
    let info = adapter.get_info();
    println!(
        "ADAPTER {} ({:?}, {:?}, driver {} {})",
        info.name, info.device_type, info.backend, info.driver, info.driver_info
    );
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("device");
    Gpu {
        instance,
        adapter,
        device,
        queue,
    }
}

fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "present".into());
    let frames: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    report("start");

    let conn = Connection::connect_to_env().expect("wayland");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry");
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm");
    let mut app = App {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm,
        configured: false,
    };
    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("gpu-spike"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_size(W, H);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();
    while !app.configured {
        queue.blocking_dispatch(&mut app).expect("dispatch");
    }
    report("wayland up, no gpu");

    match mode.as_str() {
        "present" => present(&conn, &layer, frames),
        "readback" => readback(&mut app, &mut queue, &layer, frames),
        "mem" => mem(frames),
        m => panic!("unknown mode {m}"),
    }
    let _ = queue.roundtrip(&mut app);
}

fn hold() {
    if let Ok(p) = std::env::var("SPIKE_HOLD_MS") {
        std::thread::sleep(std::time::Duration::from_millis(p.parse().unwrap_or(0)));
    }
}

fn present(conn: &Connection, layer: &LayerSurface, frames: u32) {
    let display = conn.backend().display_ptr() as *mut c_void;
    let wl = layer.wl_surface().id().as_ptr() as *mut c_void;
    let inst = instance();
    let t0 = Instant::now();
    // SAFETY: the connection and the surface outlive the wgpu surface (dropped below).
    let surface = unsafe {
        inst.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                NonNull::new(display).expect("display"),
            ))),
            raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(
                NonNull::new(wl).expect("surface"),
            )),
        })
    }
    .expect("create_surface");
    let g = gpu(Some(&surface), inst);
    let caps = surface.get_capabilities(&g.adapter);
    println!(
        "SURFACE formats={:?} present_modes={:?} alpha={:?}",
        caps.formats, caps.present_modes, caps.alpha_modes
    );
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| {
            matches!(
                f,
                wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
            )
        })
        .unwrap_or(caps.formats[0]);
    surface.configure(
        &g.device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: W,
            height: H,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        },
    );
    let (mut r, mut res) = vello_gpu::Renderer::new(
        &g.device,
        &vello_gpu::RenderTargetConfig {
            format,
            width: W as u16,
            height: H as u16,
        },
    );
    let size = vello_gpu::RenderSize {
        width: W as u16,
        height: H as u16,
    };
    let depth = vello_gpu::Renderer::create_depth_texture_view(&g.device, &size);
    println!(
        "SETUP {:.1} ms (instance+surface+adapter+device+renderer)",
        t0.elapsed().as_secs_f64() * 1e3
    );
    report("device+renderer up");
    let t1 = Instant::now();
    for i in 0..frames {
        let tex = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => panic!("get_current_texture: {other:?}"),
        };
        let view = tex.texture.create_view(&Default::default());
        let mut enc = g.device.create_command_encoder(&Default::default());
        r.render(
            &scene(i as f64),
            &mut res,
            &g.device,
            &g.queue,
            &mut enc,
            &size,
            &view,
            Some(&depth),
            &vello_gpu::TextureBindings::new(),
            vello_gpu::TargetInit::Clear(vello_gpu::ClearSettings::default()),
        )
        .expect("render");
        g.queue.submit([enc.finish()]);
        g.queue.present(tex);
    }
    let dt = t1.elapsed().as_secs_f64();
    println!(
        "PRESENTED {frames} frames in {:.1} ms ({:.2} ms/frame)",
        dt * 1e3,
        dt * 1e3 / frames as f64
    );
    report("after frames");
    hold();
    drop((r, res, depth));
    drop(surface);
    drop(g);
    report("all dropped");
}

/// Renders offscreen and reads back (the path `readback` and `mem` share).
struct Offscreen {
    g: Gpu,
    r: vello_gpu::Renderer,
    res: vello_gpu::Resources,
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    depth: wgpu::TextureView,
    buf: wgpu::Buffer,
}
impl Offscreen {
    fn new() -> Self {
        let g = gpu(None, instance());
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let tex = g.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let (r, res) = vello_gpu::Renderer::new(
            &g.device,
            &vello_gpu::RenderTargetConfig {
                format,
                width: W as u16,
                height: H as u16,
            },
        );
        let depth = vello_gpu::Renderer::create_depth_texture_view(
            &g.device,
            &vello_gpu::RenderSize {
                width: W as u16,
                height: H as u16,
            },
        );
        let buf = g.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(W * 4) * u64::from(H),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            g,
            r,
            res,
            tex,
            view,
            depth,
            buf,
        }
    }
    /// One frame into `out` (premultiplied RGBA rows, W*4 stride).
    fn frame(&mut self, t: f64, out: &mut [u8]) {
        let g = &self.g;
        let mut enc = g.device.create_command_encoder(&Default::default());
        self.r
            .render(
                &scene(t),
                &mut self.res,
                &g.device,
                &g.queue,
                &mut enc,
                &vello_gpu::RenderSize {
                    width: W as u16,
                    height: H as u16,
                },
                &self.view,
                Some(&self.depth),
                &vello_gpu::TextureBindings::new(),
                vello_gpu::TargetInit::Clear(vello_gpu::ClearSettings::default()),
            )
            .expect("render");
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(W * 4),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
        );
        g.queue.submit([enc.finish()]);
        self.buf
            .slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        g.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        out.copy_from_slice(&self.buf.slice(..).get_mapped_range().expect("range"));
        self.buf.unmap();
    }
}

fn readback(
    app: &mut App,
    queue: &mut wayland_client::EventQueue<App>,
    layer: &LayerSurface,
    frames: u32,
) {
    let t0 = Instant::now();
    let mut off = Offscreen::new();
    println!(
        "SETUP {:.1} ms (instance+adapter+device+renderer)",
        t0.elapsed().as_secs_f64() * 1e3
    );
    report("device+renderer up");
    let mut pool = SlotPool::new((W * H * 4 * 2) as usize, &app.shm).expect("pool");
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    let t1 = Instant::now();
    for i in 0..frames {
        off.frame(i as f64, &mut rgba);
        let (buffer, canvas) = pool
            .create_buffer(W as i32, H as i32, (W * 4) as i32, wl_shm::Format::Argb8888)
            .expect("buffer");
        // RGBA (premultiplied) -> little-endian ARGB8888, i.e. BGRA bytes.
        for (d, s) in canvas.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        let wl = layer.wl_surface();
        buffer.attach_to(wl).expect("attach");
        wl.damage_buffer(0, 0, W as i32, H as i32);
        wl.commit();
        queue.roundtrip(app).expect("roundtrip");
    }
    let dt = t1.elapsed().as_secs_f64();
    println!(
        "READBACK {frames} frames in {:.1} ms ({:.2} ms/frame)",
        dt * 1e3,
        dt * 1e3 / frames as f64
    );
    report("after frames");
    hold();
    drop(off);
    report("all dropped");
}

fn mem(frames: u32) {
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    for round in 0..3 {
        let t0 = Instant::now();
        let mut off = Offscreen::new();
        let up = t0.elapsed();
        for i in 0..frames {
            off.frame(i as f64, &mut rgba);
        }
        println!(
            "ROUND {round}: device up in {:.1} ms",
            up.as_secs_f64() * 1e3
        );
        report(&format!("round {round} device+{frames} frames"));
        let Offscreen {
            g,
            r,
            res,
            tex,
            view,
            depth,
            buf,
        } = off;
        drop((r, res, tex, view, depth, buf));
        let Gpu {
            instance,
            adapter,
            device,
            queue,
        } = g;
        drop((queue, device, adapter));
        report(&format!("round {round} device dropped"));
        drop(instance);
        std::thread::sleep(std::time::Duration::from_millis(200));
        report(&format!("round {round} instance dropped"));
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {}
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configured = true;
    }
}
impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
delegate_registry!(App);
delegate_dispatch2!(App);

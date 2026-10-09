//! Spike only (docs/decisions.md, m4-gpu-spike): links wgpu, naga and
//! vello_gpu into the strand binary, reachable from main behind a runtime
//! check, so `.text` and cold PSS can be measured. Never committed into
//! crates/strand; spikes/text-measure/apply.sh copies it in for a build.

use std::ffi::c_void;
use std::ptr::NonNull;

use vello_common::filter_effects::{EdgeMode, Filter, FilterPrimitive};
use vello_common::kurbo::{Affine, BezPath, Circle, Rect, Shape, Stroke};
use vello_common::peniko::color::{AlphaColor, Srgb};
use vello_common::peniko::{BlendMode, Compose, Mix};

pub fn run() -> Result<(), String> {
    // naga: what `strand check` would do with a user's .wgsl.
    let src = std::env::var("STRAND_GPU_SPIKE_WGSL").unwrap_or_default();
    let module = naga::front::wgsl::parse_str(&src).map_err(|e| e.emit_to_string(&src))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| e.emit_to_string(&src))?;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    // The WSI path: a surface from raw Wayland handles.
    let surface = match (
        std::env::var("STRAND_GPU_SPIKE_DPY"),
        std::env::var("STRAND_GPU_SPIKE_SURF"),
    ) {
        (Ok(d), Ok(s)) => {
            let d = usize::from_str_radix(&d, 16).map_err(|e| e.to_string())? as *mut c_void;
            let s = usize::from_str_radix(&s, 16).map_err(|e| e.to_string())? as *mut c_void;
            // SAFETY: spike only; the pointers come from the caller.
            Some(
                unsafe {
                    instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: Some(raw_window_handle::RawDisplayHandle::Wayland(
                            raw_window_handle::WaylandDisplayHandle::new(
                                NonNull::new(d).ok_or("null")?,
                            ),
                        )),
                        raw_window_handle: raw_window_handle::RawWindowHandle::Wayland(
                            raw_window_handle::WaylandWindowHandle::new(
                                NonNull::new(s).ok_or("null")?,
                            ),
                        ),
                    })
                }
                .map_err(|e| e.to_string())?,
            )
        }
        _ => None,
    };
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: surface.as_ref(),
        ..Default::default()
    }))
    .map_err(|e| e.to_string())?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| e.to_string())?;
    // A user shader module through wgpu (naga again, inside wgpu-core).
    let _sm = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(src.as_str().into()),
    });
    let (w, h) = (256u16, 128u16);
    let format = if let Some(surface) = &surface {
        let caps = surface.get_capabilities(&adapter);
        let format = *caps.formats.first().ok_or("no format")?;
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: w.into(),
                height: h.into(),
                present_mode: wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
                color_space: wgpu::SurfaceColorSpace::Auto,
            },
        );
        format
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: w.into(),
            height: h.into(),
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
    let (mut renderer, mut res) = vello_gpu::Renderer::new(
        &device,
        &vello_gpu::RenderTargetConfig {
            format,
            width: w,
            height: h,
        },
    );
    let size = vello_gpu::RenderSize {
        width: w,
        height: h,
    };
    let depth = vello_gpu::Renderer::create_depth_texture_view(&device, &size);

    let mut scene = vello_gpu::Scene::new(w, h);
    scene.set_paint(AlphaColor::<Srgb>::from_rgba8(30, 30, 46, 255));
    scene.fill_rect(&Rect::new(0.0, 0.0, 256.0, 128.0));
    scene.fill_blurred_rounded_rect(&Rect::new(20.0, 20.0, 120.0, 100.0), 8.0, 4.0, false);
    let clip: BezPath = Circle::new((64.0, 64.0), 40.0).to_path(0.1);
    scene.push_layer(
        Some(&clip),
        Some(BlendMode::new(Mix::Multiply, Compose::SrcOver)),
        Some(0.5),
        None,
        None,
    );
    scene.set_transform(Affine::rotate(0.3));
    scene.set_stroke(Stroke::new(3.0));
    scene.stroke_path(&clip);
    scene.pop_layer();
    scene.push_filter_layer(Filter::from_primitive(FilterPrimitive::GaussianBlur {
        std_deviation: 3.0,
        edge_mode: EdgeMode::None,
    }));
    scene.fill_path(&clip);
    scene.pop_layer();

    let mut enc = device.create_command_encoder(&Default::default());
    renderer
        .render(
            &scene,
            &mut res,
            &device,
            &queue,
            &mut enc,
            &size,
            &view,
            Some(&depth),
            &vello_gpu::TextureBindings::new(),
            vello_gpu::TargetInit::Clear(vello_gpu::ClearSettings::default()),
        )
        .map_err(|e| e.to_string())?;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(w) * 4 * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(u32::from(w) * 4),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: w.into(),
            height: h.into(),
            depth_or_array_layers: 1,
        },
    );
    queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| e.to_string())?;
    let sum: u64 = buf
        .slice(..)
        .get_mapped_range()
        .map_err(|e| e.to_string())?
        .iter()
        .map(|&b| u64::from(b))
        .sum();
    eprintln!(
        "gpu spike: {} rendered, checksum {sum}",
        adapter.get_info().name
    );
    if let Some(surface) = &surface {
        if let wgpu::CurrentSurfaceTexture::Success(t) = surface.get_current_texture() {
            queue.present(t);
        }
    }
    Ok(())
}

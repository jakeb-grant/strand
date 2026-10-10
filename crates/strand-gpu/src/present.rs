//! Presenting through wgpu's WSI (`GpuMode::Present`): a surface from the
//! manager's raw Wayland handles, configured `Fifo`, fed by a blit of the
//! frame texture. Frames send full damage (design.md).

use std::collections::HashMap;

use crate::RawHandles;
use crate::device::Device;

/// The blit from the frame texture into a swapchain image: straight
/// colours in, straight colours out, whatever the swapchain's byte order.
const BLIT: &str = "
@group(0) @binding(0) var src: texture_2d<f32>;
struct V { @builtin(position) pos: vec4<f32> }
@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return V(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0));
}
@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    return textureLoad(src, vec2<i32>(floor(v.pos.xy)), 0);
}
";

/// A surface the GPU thread presents to.
pub(crate) struct Presented {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
}

/// Why presenting is not possible, for the log.
pub(crate) fn try_surface(
    dev: &Device,
    handles: RawHandles,
    width: u32,
    height: u32,
    opaque: bool,
) -> Result<Presented, String> {
    // SAFETY: the handles name a live `wl_display` and `wl_surface`; the
    // manager keeps both alive until the GPU thread has answered
    // `Release` for this surface, and the wgpu surface is dropped before
    // that answer (`RawHandles`).
    let surface = unsafe {
        dev.instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(handles.display),
                raw_window_handle: handles.window,
            })
    }
    .map_err(|e| format!("cannot create a WSI surface: {e}"))?;
    let caps = surface.get_capabilities(&dev.adapter);
    if caps.formats.is_empty() {
        return Err("the WSI cannot present to this surface".into());
    }
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| {
            matches!(
                f,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
            )
        })
        .ok_or_else(|| format!("no non-sRGB 8-bit format among {:?}", caps.formats))?;
    let alpha_mode = if caps
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else if opaque && caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        return Err(format!(
            "no premultiplied alpha among {:?}",
            caps.alpha_modes
        ));
    };
    if !caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
        return Err("no Fifo present mode".into());
    }
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: width.max(1),
        height: height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    let (_, err) = dev.scoped(|| surface.configure(&dev.device, &config));
    if let Some(e) = err {
        return Err(format!("cannot configure the WSI surface: {e}"));
    }
    Ok(Presented { surface, config })
}

impl Presented {
    pub fn resize(&mut self, dev: &Device, width: u32, height: u32) {
        if self.config.width == width.max(1) && self.config.height == height.max(1) {
            return;
        }
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&dev.device, &self.config);
    }
}

/// Blit pipelines by swapchain format.
#[derive(Default)]
pub(crate) struct Blit {
    pipelines: HashMap<wgpu::TextureFormat, (wgpu::RenderPipeline, wgpu::BindGroupLayout)>,
}

impl Blit {
    /// Records the copy of `src` into `dst`.
    pub fn record(
        &mut self,
        dev: &Device,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) {
        let d = &dev.device;
        let (pipeline, layout) = self.pipelines.entry(format).or_insert_with(|| {
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("strand blit"),
                source: wgpu::ShaderSource::Wgsl(BLIT.into()),
            });
            let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("strand blit"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                }],
            });
            let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("strand blit"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("strand blit"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            (pipeline, layout)
        });
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("strand blit"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(src),
            }],
        });
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("strand blit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(pipeline);
        rp.set_bind_group(0, &group, &[]);
        rp.draw(0..3, 0..1);
    }
}

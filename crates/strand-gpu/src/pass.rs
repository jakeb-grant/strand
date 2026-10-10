//! Shader passes (docs/architecture.md, "`strand-gpu`", the ABI): a
//! `.wgsl` file's one `@fragment` entry over a box, with Strand's vertex
//! stage, Strand's `@group(0)` (`strand`, `strand_input`,
//! `strand_sampler`) and the file's `u_*` uniforms in `@group(1)`, one
//! `var<uniform>` per binding, each filled from its slot of
//! `ShaderPass::uniforms` at the device's uniform offset alignment.
//!
//! Bundled passes are drawn by their CPU versions until their WGSL lands
//! (M4 wave 3): they compile to nothing here.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use strand_scene::{ShaderCode, ShaderRef};

use crate::device::Device;
use crate::{GpuError, GpuErrorKind, PassGlobals, ShaderPass};

/// The vertex stage Strand appends to every module: one triangle covering
/// the box, `uv` 0..1 over it. (The name is Strand's: a file may not
/// define a function called `strand_vertex_main`.)
pub(crate) const VERTEX: &str = "
@vertex
fn strand_vertex_main(@builtin(vertex_index) i: u32) -> StrandVertex {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: StrandVertex;
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}
";

/// The format passes draw in: straight colours whose bytes are the
/// `wl_shm` order, sampled by vello as an external texture.
pub(crate) const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// Bytes of `@group(0) @binding(0)`'s `Strand` (24, padded to 32).
const GLOBALS_BYTES: u64 = 32;

/// Bytes bound per `u_*` uniform (a `vec4<f32>` at most).
const SLOT_BYTES: u64 = 16;

struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    layout1: wgpu::BindGroupLayout,
    /// `(binding, offset in floats, floats)` per slot.
    slots: Vec<(u32, u32, u32)>,
}

/// Compiled pipelines and the bindings every pass shares.
pub(crate) struct Passes {
    layout0: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// 1×1 transparent: `strand_input` of a pass with no input.
    empty: wgpu::TextureView,
    pipelines: HashMap<u64, Result<Pipeline, GpuError>>,
}

/// A pass drawn into its own texture.
pub(crate) struct Drawn {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

fn code_key(code: &ShaderCode) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    code.wgsl.hash(&mut h);
    code.uniforms.hash(&mut h);
    h.finish()
}

impl Passes {
    pub fn new(dev: &Device) -> Self {
        let d = &dev.device;
        let layout0 = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("strand pass group 0"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("strand pass sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let empty = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("strand empty input"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Created zeroed: transparent.
        let empty = empty.create_view(&wgpu::TextureViewDescriptor::default());
        Passes {
            layout0,
            sampler,
            empty,
            pipelines: HashMap::new(),
        }
    }

    fn pipeline(&mut self, dev: &Device, code: &ShaderCode) -> Result<&Pipeline, GpuError> {
        let key = code_key(code);
        let entry = self
            .pipelines
            .entry(key)
            .or_insert_with(|| compile(dev, &self.layout0, code));
        entry.as_ref().map_err(Clone::clone)
    }

    /// Draws `pass` into a new `width × height` texture. Bundled passes
    /// (wave 3) and zero sizes draw nothing (`Ok(None)`).
    pub fn draw(
        &mut self,
        dev: &Device,
        encoder: &mut wgpu::CommandEncoder,
        pass: &ShaderPass,
        width: u32,
        height: u32,
        globals: PassGlobals,
    ) -> Result<Option<Drawn>, GpuError> {
        let code = match &pass.code {
            ShaderRef::File(code) => code.clone(),
            ShaderRef::Bundled(_) => return Ok(None),
        };
        let max = dev.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return Ok(None);
        }
        let align =
            u64::from(dev.device.limits().min_uniform_buffer_offset_alignment).max(SLOT_BYTES);
        let layout0 = self.layout0.clone();
        let (sampler, empty) = (self.sampler.clone(), self.empty.clone());
        let p = self.pipeline(dev, &code)?;
        let d = &dev.device;
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("strand pass"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // `Strand`: time, scale, size, pointer.
        let mut g = [0f32; (GLOBALS_BYTES / 4) as usize];
        g[0] = globals.time;
        g[1] = globals.scale;
        g[2] = width as f32;
        g[3] = height as f32;
        g[4] = globals.pointer[0];
        g[5] = globals.pointer[1];
        let globals_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strand pass globals"),
            size: GLOBALS_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        dev.queue.write_buffer(&globals_buf, 0, &floats_bytes(&g));
        let group0 = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("strand pass group 0"),
            layout: &layout0,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&empty),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        // Each slot at its own aligned range; a uniform without a value
        // (fewer floats than the code has) is zero-filled.
        let n = p.slots.len().max(1) as u64;
        let ubuf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strand pass uniforms"),
            size: n * align,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut bytes = vec![0u8; (n * align) as usize];
        for (i, (_, offset, floats)) in p.slots.iter().enumerate() {
            for k in 0..*floats {
                let v = pass
                    .uniforms
                    .get((offset + k) as usize)
                    .copied()
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0);
                let at = i * align as usize + k as usize * 4;
                bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        dev.queue.write_buffer(&ubuf, 0, &bytes);
        let entries: Vec<wgpu::BindGroupEntry<'_>> = p
            .slots
            .iter()
            .enumerate()
            .map(|(i, (binding, _, _))| wgpu::BindGroupEntry {
                binding: *binding,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &ubuf,
                    offset: i as u64 * align,
                    size: wgpu::BufferSize::new(SLOT_BYTES),
                }),
            })
            .collect();
        let group1 = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("strand pass group 1"),
            layout: &p.layout1,
            entries: &entries,
        });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("strand pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
            rp.set_pipeline(&p.pipeline);
            rp.set_bind_group(0, &group0, &[]);
            rp.set_bind_group(1, &group1, &[]);
            rp.draw(0..3, 0..1);
        }
        Ok(Some(Drawn {
            texture,
            view,
            width,
            height,
        }))
    }
}

fn floats_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// The module the GPU compiles: the prelude, the file, Strand's vertex
/// stage.
pub(crate) fn module_source(code: &ShaderCode) -> String {
    let mut s = code.module();
    s.push_str(VERTEX);
    s
}

fn compile(
    dev: &Device,
    layout0: &wgpu::BindGroupLayout,
    code: &ShaderCode,
) -> Result<Pipeline, GpuError> {
    let d = &dev.device;
    let src = module_source(code);
    let (out, err) = dev.scoped(|| {
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(code.path.as_str()),
            source: wgpu::ShaderSource::Wgsl(src.as_str().into()),
        });
        let entries: Vec<wgpu::BindGroupLayoutEntry> = code
            .uniforms
            .iter()
            .map(|u| wgpu::BindGroupLayoutEntry {
                binding: u.binding,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            })
            .collect();
        let layout1 = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("strand pass group 1"),
            entries: &entries,
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("strand pass"),
            bind_group_layouts: &[Some(layout0), Some(&layout1)],
            immediate_size: 0,
        });
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(code.path.as_str()),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("strand_vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: None,
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
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
        (pipeline, layout1)
    });
    if let Some(e) = err {
        return Err(GpuError::new(
            GpuErrorKind::Shader,
            format!("{}: {e}", code.path),
        ));
    }
    let (pipeline, layout1) = out;
    Ok(Pipeline {
        pipeline,
        layout1,
        slots: code
            .uniforms
            .iter()
            .map(|u| (u.binding, u.offset, u.ty.floats()))
            .collect(),
    })
}

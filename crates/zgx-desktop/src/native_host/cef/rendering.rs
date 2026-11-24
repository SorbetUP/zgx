use std::sync::{Arc, Mutex};

use zgx_gpu::viewport::{Viewport, ViewportFrame};

/// One bounded CEF frame slot. New paints replace frames the renderer has
/// not consumed yet; CEF's callback buffer is copied before returning.
#[derive(Default)]
pub(super) struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub revision: u64,
    pub consumed: u64,
}

pub(super) type SharedFrame = Arc<Mutex<Frame>>;

pub(super) struct Painter {
    pub frame: SharedFrame,
    size: (u32, u32),
    gpu: Option<Blit>,
}

struct Blit {
    pipeline: wgpu::RenderPipeline,
    format: wgpu::TextureFormat,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: Option<(wgpu::Texture, wgpu::BindGroup, (u32, u32))>,
}

impl Viewport for Painter {
    fn render(&mut self, frame: &mut ViewportFrame<'_>) {
        let size = (frame.size.0.max(1), frame.size.1.max(1));
        self.size = size;
        let blit = match &mut self.gpu {
            Some(blit) if blit.format == frame.format => blit,
            _ => self.gpu.insert(Blit::new(frame.device, frame.format)),
        };
        let mut shared = self.frame.lock().unwrap_or_else(|poison| poison.into_inner());
        let source_size = (shared.width, shared.height);
        if source_size.0 > 0 && source_size.1 > 0 && shared.revision != shared.consumed {
            let fresh = blit.texture.as_ref().is_none_or(|(_, _, at)| *at != source_size);
            if fresh {
                let texture = frame.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("CEF page BGRA"),
                    size: wgpu::Extent3d { width: source_size.0, height: source_size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                let bind = frame.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("CEF page BGRA"),
                    layout: &blit.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&blit.sampler) },
                    ],
                });
                blit.texture = Some((texture, bind, source_size));
            }
            if let Some((texture, _, _)) = &blit.texture {
                // CEF returns tightly packed premultiplied BGRA pixels.
                frame.queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    &shared.pixels,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(source_size.0 * 4), rows_per_image: Some(source_size.1) },
                    wgpu::Extent3d { width: source_size.0, height: source_size.1, depth_or_array_layers: 1 },
                );
            }
            shared.consumed = shared.revision;
        }
        drop(shared);
        let mut pass = frame.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("CEF page"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame.target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        if let Some((_, bind, _)) = &blit.texture {
            pass.set_pipeline(&blit.pipeline);
            pass.set_bind_group(0, bind, &[]);
            pass.draw(0..3, 0..1);
        }
    }

    fn device_changed(&mut self) {
        self.gpu = None;
    }
}

impl Blit {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("CEF page"),
            entries: &[
                wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None },
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("CEF page"),
            source: wgpu::ShaderSource::Wgsl(r#"
                @group(0) @binding(0) var page: texture_2d<f32>;
                @group(0) @binding(1) var linear: sampler;
                struct Out { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> }
                @vertex fn vertex(@builtin(vertex_index) i: u32) -> Out {
                    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                    return Out(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
                }
                @fragment fn fragment(in: Out) -> @location(0) vec4<f32> { return textureSample(page, linear, in.uv); }
            "#.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("CEF page"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("CEF page"), layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vertex"), compilation_options: Default::default(), buffers: &[] },
            fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fragment"), compilation_options: Default::default(), targets: &[Some(format.into())] }),
            primitive: Default::default(), depth_stencil: None, multisample: Default::default(), multiview_mask: None, cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor { label: Some("CEF page"), mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, ..Default::default() });
        Self { pipeline, format, layout, sampler, texture: None }
    }
}

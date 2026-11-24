use cef::{Browser, ImplBrowser, ImplBrowserHost};
use std::sync::{Arc, Mutex};
use std::{cell::RefCell, rc::Rc};

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
    page_size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
    browser: Rc<RefCell<Option<Browser>>>,
}

impl Painter {
    pub(super) fn new(
        frame: SharedFrame,
        page_size: Arc<Mutex<(i32, i32)>>,
        scale: Arc<Mutex<f32>>,
        browser: Rc<RefCell<Option<Browser>>>,
    ) -> Self {
        Self {
            frame,
            size: (1024, 768),
            gpu: None,
            page_size,
            scale,
            browser,
        }
    }
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
        let scale = frame.scale.max(0.01);
        *self
            .scale
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = scale;
        let logical_size = (
            (size.0 as f32 / scale).round().max(1.) as i32,
            (size.1 as f32 / scale).round().max(1.) as i32,
        );
        let changed = {
            let mut page_size = self
                .page_size
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if *page_size == logical_size {
                false
            } else {
                *page_size = logical_size;
                true
            }
        };
        if changed {
            if let Some(browser) = self.browser.borrow().as_ref()
                && let Some(host) = browser.host()
            {
                host.was_resized();
            }
        }
        let blit = match &mut self.gpu {
            Some(blit) if blit.format == frame.format => blit,
            _ => self.gpu.insert(Blit::new(frame.device, frame.format)),
        };
        let mut shared = self
            .frame
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let source_size = (shared.width, shared.height);
        if source_size.0 > 0 && source_size.1 > 0 && shared.revision != shared.consumed {
            let fresh = blit
                .texture
                .as_ref()
                .is_none_or(|(_, _, at)| *at != source_size);
            if fresh {
                let texture = frame.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("CEF page BGRA"),
                    size: wgpu::Extent3d {
                        width: source_size.0,
                        height: source_size.1,
                        depth_or_array_layers: 1,
                    },
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
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&blit.sampler),
                        },
                    ],
                });
                blit.texture = Some((texture, bind, source_size));
            }
            if let Some((texture, _, _)) = &blit.texture {
                // CEF returns tightly packed premultiplied BGRA pixels.
                frame.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &shared.pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(source_size.0 * 4),
                        rows_per_image: Some(source_size.1),
                    },
                    wgpu::Extent3d {
                        width: source_size.0,
                        height: source_size.1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            shared.consumed = shared.revision;
        }
        drop(shared);
        let mut pass = frame
            .encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("CEF page"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame.target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
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
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("CEF page"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("CEF page"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("CEF page"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            format,
            layout,
            sampler,
            texture: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn painter_composes_cef_bgra_pixels_into_the_zgx_target() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("a headless wgpu adapter is required for the CEF painter pixel test");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("CEF painter pixel test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        }))
        .expect("request test device");

        // Two rows of CEF BGRA pixels: red/green, then blue/white.
        let source = Arc::new(Mutex::new(Frame {
            width: 2,
            height: 2,
            pixels: vec![
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ],
            revision: 1,
            consumed: 0,
        }));
        let mut painter = Painter::new(
            source,
            Arc::new(Mutex::new((2, 2))),
            Arc::new(Mutex::new(1.)),
            Rc::new(RefCell::new(None)),
        );
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("CEF painter readback target"),
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("CEF painter pixel test"),
        });
        painter.render(&mut ViewportFrame::new(
            &device,
            &queue,
            &mut encoder,
            &target_view,
            wgpu::TextureFormat::Rgba8Unorm,
            (2, 2),
            1.,
        ));

        // wgpu copy rows must be 256-byte aligned, even for this tiny target.
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("CEF painter pixel readback"),
            size: 512,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(2),
                },
            },
            wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let (sent, received) = std::sync::mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sent.send(result);
            });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("wait for GPU readback");
        received
            .recv()
            .expect("readback callback")
            .expect("map readback");
        let mapped = readback.slice(..).get_mapped_range();
        let pixels = [
            &mapped[0..4],
            &mapped[4..8],
            &mapped[256..260],
            &mapped[260..264],
        ];
        assert_eq!(pixels[0], [255, 0, 0, 255], "top-left CEF pixel");
        assert_eq!(pixels[1], [0, 255, 0, 255], "top-right CEF pixel");
        assert_eq!(pixels[2], [0, 0, 255, 255], "bottom-left CEF pixel");
        assert_eq!(pixels[3], [255, 255, 255, 255], "bottom-right CEF pixel");
    }
}

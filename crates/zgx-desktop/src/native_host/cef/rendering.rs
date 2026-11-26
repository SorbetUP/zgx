use cef::{Browser, ImplBrowser, ImplBrowserHost, Rect};
use std::sync::{Arc, Mutex};
use std::{cell::{Cell, RefCell}, rc::Rc};

use zgx_gpu::viewport::{Viewport, ViewportFrame};

/// One bounded CEF frame slot. New paints replace frames the renderer has
/// not consumed yet; CEF's callback buffer is copied before returning.
#[derive(Default)]
pub(super) struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// Dirty rects of the last paint (clamped to the surface). Empty means
    /// the whole frame changed. Used to upload only the changed region.
    pub dirty: Vec<Rect>,
    pub revision: u64,
    pub consumed: u64,
}

pub(super) type SharedFrame = Arc<Mutex<Frame>>;

#[derive(Default)]
pub(super) struct PopupFrame {
    pub frame: Frame,
    pub rect: (i32, i32, i32, i32),
    pub visible: bool,
}

pub(super) type SharedPopup = Arc<Mutex<PopupFrame>>;

/// A CEF paint clamped to the surface it is drawn into: the buffer is never
/// larger than the view (or popup) it belongs to, and its dirty rects are
/// clipped to that same bound.
pub(super) struct ClampedPaint {
    pub width: u32,
    pub height: u32,
    pub dirty_rects: Vec<Rect>,
}

/// Clamp a CEF `on_paint` buffer to `bound` (the view size for page paints,
/// the popup rect for popup paints). Returns `None` — the paint is ignored —
/// when the buffer is empty (`width`/`height` <= 0) or larger than the bound,
/// which happens while a resize is still in flight; oversized paints are
/// logged once. Dirty rects are clipped to the bound and empty ones dropped.
pub(super) fn clamp_paint(
    width: i32,
    height: i32,
    dirty_rects: &[Rect],
    bound: (i32, i32),
) -> Option<ClampedPaint> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let bound = (bound.0.max(1), bound.1.max(1));
    if width > bound.0 || height > bound.1 {
        log_oversized_paint_once(width, height, bound);
        return None;
    }
    let dirty_rects = dirty_rects
        .iter()
        .map(|rect| clamp_rect(rect, bound))
        .filter(|rect| rect.width > 0 && rect.height > 0)
        .collect();
    Some(ClampedPaint {
        width: width as u32,
        height: height as u32,
        dirty_rects,
    })
}

/// Union bounding box of the dirty rects, as `(x, y, w, h)` in frame pixels.
/// `None` (or a full-frame box) when nothing is dirty means "everything".
fn dirty_union(dirty: &[Rect], size: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let mut iter = dirty.iter().filter(|r| r.width > 0 && r.height > 0);
    let first = iter.next()?;
    let mut x0 = first.x.max(0);
    let mut y0 = first.y.max(0);
    let mut x1 = (first.x + first.width).min(size.0 as i32);
    let mut y1 = (first.y + first.height).min(size.1 as i32);
    for r in iter {
        x0 = x0.min(r.x.max(0));
        y0 = y0.min(r.y.max(0));
        x1 = x1.max((r.x + r.width).min(size.0 as i32));
        y1 = y1.max((r.y + r.height).min(size.1 as i32));
    }
    let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.max(0), y1.max(0));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Clip `rect` to `bound`, keeping it inside `[0, bound]` on both axes.
fn clamp_rect(rect: &Rect, bound: (i32, i32)) -> Rect {
    let x = rect.x.clamp(0, bound.0);
    let y = rect.y.clamp(0, bound.1);
    let right = (rect.x + rect.width).clamp(0, bound.0);
    let bottom = (rect.y + rect.height).clamp(0, bound.1);
    Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

fn log_oversized_paint_once(width: i32, height: i32, bound: (i32, i32)) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("CEF paint {width}x{height} exceeds the {bound:?} surface; ignoring it");
    }
}

/// Release the CPU-side copies as soon as CEF confirms that a browser is
/// closed. A viewport may briefly outlive its browser while ZGX tears down the
/// scene, so dropping only the browser handle is not enough to release these
/// potentially large allocations promptly.
pub(super) fn release_frame_buffers(frame: &SharedFrame, popup: &SharedPopup) {
    if let Ok(mut frame) = frame.lock() {
        frame.width = 0;
        frame.height = 0;
        frame.pixels = Vec::new();
        frame.consumed = frame.revision;
    }
    if let Ok(mut popup) = popup.lock() {
        popup.frame.width = 0;
        popup.frame.height = 0;
        popup.frame.pixels = Vec::new();
        popup.frame.consumed = popup.frame.revision;
        popup.rect = (0, 0, 0, 0);
        popup.visible = false;
    }
}

pub(super) struct Painter {
    pub frame: SharedFrame,
    pub popup: SharedPopup,
    pub visible: Rc<Cell<u32>>,
    size: (u32, u32),
    gpu: Option<Blit>,
    popup_gpu: Option<Blit>,
    page_size: Arc<Mutex<(i32, i32)>>,
    scale: Arc<Mutex<f32>>,
    browser: Rc<RefCell<Option<Browser>>>,
}

impl Painter {
    pub(super) fn new(
        frame: SharedFrame,
        popup: SharedPopup,
        page_size: Arc<Mutex<(i32, i32)>>,
        scale: Arc<Mutex<f32>>,
        browser: Rc<RefCell<Option<Browser>>>,
        visible: Rc<Cell<u32>>,
    ) -> Self {
        Self {
            frame,
            popup,
            visible,
            size: (1024, 768),
            gpu: None,
            popup_gpu: None,
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
        if changed
            && let Some(browser) = self.browser.borrow().as_ref()
            && let Some(host) = browser.host()
        {
            host.was_resized();
        }
        let blit = match &mut self.gpu {
            Some(blit) if blit.format == frame.format => blit,
            _ => self.gpu.insert(Blit::new(frame.device, frame.format)),
        };
        let popup_blit = match &mut self.popup_gpu {
            Some(blit) if blit.format == frame.format => blit,
            _ => self.popup_gpu.insert(Blit::new(frame.device, frame.format)),
        };
        let mut shared = self
            .frame
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let source_size = (shared.width, shared.height);
        // A hidden view holds no GPU texture: drop it and swallow the frame
        // so hidden pages release their video memory.
        let hidden = self.visible.get() == 0;
        if hidden {
            blit.texture = None;
            shared.consumed = shared.revision;
        }
        if !hidden && source_size.0 > 0 && source_size.1 > 0 && shared.revision != shared.consumed {
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
                // Upload only the dirty region when it is much smaller than
                // the whole frame: write_texture needs a 256-byte-aligned row
                // pitch, so the region is staged into a pitched buffer.
                let full = source_size.0 as u64 * source_size.1 as u64;
                let region = dirty_union(&shared.dirty, source_size);
                let region_area = region.map_or(full, |r| r.2 as u64 * r.3 as u64);
                if let Some((rx, ry, rw, rh)) = region.filter(|_| region_area * 10 < full * 6) {
                    let pitch = (rw as usize * 4 + 255) & !255;
                    let mut staging = vec![0u8; pitch * rh as usize];
                    for row in 0..rh as usize {
                        let src = ((ry as usize + row) * source_size.0 as usize + rx as usize) * 4;
                        let dst = row * pitch;
                        staging[dst..dst + rw as usize * 4]
                            .copy_from_slice(&shared.pixels[src..src + rw as usize * 4]);
                    }
                    frame.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: rx, y: ry, z: 0 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &staging,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(pitch as u32),
                            rows_per_image: Some(rh),
                        },
                        wgpu::Extent3d {
                            width: rw,
                            height: rh,
                            depth_or_array_layers: 1,
                        },
                    );
                } else {
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
            }
            shared.consumed = shared.revision;
        }
        drop(shared);
        let mut popup = self
            .popup
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let popup_visible = popup.visible
            && popup.rect.2 > 0
            && popup.rect.3 > 0
            && popup.frame.width > 0
            && popup.frame.height > 0;
        let popup_size = (popup.frame.width, popup.frame.height);
        if popup_visible && popup.frame.revision != popup.frame.consumed {
            let fresh = popup_blit
                .texture
                .as_ref()
                .is_none_or(|(_, _, at)| *at != popup_size);
            if fresh {
                let texture = frame.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("CEF popup BGRA"),
                    size: wgpu::Extent3d {
                        width: popup_size.0,
                        height: popup_size.1,
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
                    label: Some("CEF popup BGRA"),
                    layout: &popup_blit.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&popup_blit.sampler),
                        },
                    ],
                });
                popup_blit.texture = Some((texture, bind, popup_size));
            }
            if let Some((texture, _, _)) = &popup_blit.texture {
                frame.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &popup.frame.pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(popup_size.0 * 4),
                        rows_per_image: Some(popup_size.1),
                    },
                    wgpu::Extent3d {
                        width: popup_size.0,
                        height: popup_size.1,
                        depth_or_array_layers: 1,
                    },
                );
            }
            popup.frame.consumed = popup.frame.revision;
        }
        let popup_rect = popup.rect;
        drop(popup);
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
        if popup_visible && let Some((_, bind, _)) = &popup_blit.texture {
            let scale = scale.max(0.01);
            let x = (popup_rect.0 as f32 * scale).max(0.0);
            let y = (popup_rect.1 as f32 * scale).max(0.0);
            let width = (popup_rect.2 as f32 * scale).max(1.0);
            let height = (popup_rect.3 as f32 * scale).max(1.0);
            pass.set_viewport(x, y, width, height, 0.0, 1.0);
            pass.set_scissor_rect(
                x.floor() as u32,
                y.floor() as u32,
                width.ceil() as u32,
                height.ceil() as u32,
            );
            pass.set_pipeline(&popup_blit.pipeline);
            pass.set_bind_group(0, bind, &[]);
            pass.draw(0..3, 0..1);
        }
    }

    fn device_changed(&mut self) {
        self.gpu = None;
        self.popup_gpu = None;
        {
            let mut frame = self
                .frame
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            frame.consumed = frame.revision.wrapping_sub(1);
        }
        {
            let mut popup = self
                .popup
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            popup.frame.consumed = popup.frame.revision.wrapping_sub(1);
        }
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
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
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
    fn paints_are_clamped_to_the_surface_and_oversized_ones_dropped() {
        let bound = (100, 80);
        let paint = clamp_paint(
            100,
            80,
            &[
                Rect {
                    x: 90,
                    y: 70,
                    width: 20,
                    height: 20,
                },
                Rect {
                    x: -5,
                    y: -5,
                    width: 10,
                    height: 10,
                },
                Rect {
                    x: 40,
                    y: 40,
                    width: 0,
                    height: 8,
                },
            ],
            bound,
        )
        .expect("a view-sized paint is accepted");
        assert_eq!((paint.width, paint.height), (100, 80));
        let rects: Vec<(i32, i32, i32, i32)> = paint
            .dirty_rects
            .iter()
            .map(|rect| (rect.x, rect.y, rect.width, rect.height))
            .collect();
        assert_eq!(rects, [(90, 70, 10, 10), (0, 0, 5, 5)]);

        // Empty and negative buffers are ignored silently.
        assert!(clamp_paint(0, 80, &[], bound).is_none());
        assert!(clamp_paint(100, 0, &[], bound).is_none());
        assert!(clamp_paint(-3, -4, &[], bound).is_none());
        // Buffers larger than the surface are ignored (logged once).
        assert!(clamp_paint(101, 80, &[], bound).is_none());
        assert!(clamp_paint(100, 81, &[], bound).is_none());
        // A degenerate surface still clamps to at least 1x1.
        let paint = clamp_paint(1, 1, &[], (0, 0)).expect("1x1 paint fits a degenerate surface");
        assert_eq!((paint.width, paint.height), (1, 1));
        assert!(clamp_paint(2, 1, &[], (0, 0)).is_none());
        // Smaller buffers pass through untouched, with no dirty rects.
        let paint = clamp_paint(50, 40, &[], bound).expect("smaller paint fits");
        assert_eq!((paint.width, paint.height), (50, 40));
        assert!(paint.dirty_rects.is_empty());
    }

    #[test]
    fn closing_browser_releases_view_and_popup_cpu_frames() {
        let frame = Arc::new(Mutex::new( Frame {
            width: 32,
            height: 32,
            pixels: vec![255; 32 * 32 * 4],
            revision: 4,
            consumed: 3,
            dirty: Vec::new(),
        }));
        let popup = Arc::new(Mutex::new(PopupFrame {
             frame: Frame {
                width: 8,
                height: 8,
                pixels: vec![127; 8 * 8 * 4],
                revision: 2,
                consumed: 1,
                dirty: Vec::new(),
            },
            rect: (10, 12, 8, 8),
            visible: true,
        }));

        release_frame_buffers(&frame, &popup);

        let frame = frame.lock().unwrap();
        assert_eq!((frame.width, frame.height), (0, 0));
        assert!(frame.pixels.is_empty());
        assert_eq!(frame.pixels.capacity(), 0);
        assert_eq!(frame.consumed, frame.revision);

        let popup = popup.lock().unwrap();
        assert_eq!((popup.frame.width, popup.frame.height), (0, 0));
        assert!(popup.frame.pixels.is_empty());
        assert_eq!(popup.frame.pixels.capacity(), 0);
        assert_eq!(popup.rect, (0, 0, 0, 0));
        assert!(!popup.visible);
        assert_eq!(popup.frame.consumed, popup.frame.revision);
    }

    #[test]
    fn dirty_union_boxes_and_clips_the_changed_region() {
        let rect = |x, y, w, h| Rect { x, y, width: w, height: h };
        assert_eq!(dirty_union(&[], (100, 100)), None);
        assert_eq!(dirty_union(&[rect(10, 10, 5, 5), rect(20, 20, 5, 5)], (100, 100)), Some((10, 10, 15, 15)));
        // Clipped to the frame bounds.
        assert_eq!(dirty_union(&[rect(90, 90, 50, 50)], (100, 100)), Some((90, 90, 10, 10)));
        // Empty rects are ignored.
        assert_eq!(dirty_union(&[rect(0, 0, 0, 0), rect(5, 5, 4, 4)], (100, 100)), Some((5, 5, 4, 4)));
    }

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
        let source = Arc::new(Mutex::new( Frame {
            width: 2,
            height: 2,
            pixels: vec![
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ],
            revision: 1,
            consumed: 1,
            dirty: Vec::new(),
        }));
        let popup = Arc::new(Mutex::new(PopupFrame {
             frame: Frame {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 255, 255],
                revision: 1,
                consumed: 1,
                dirty: Vec::new(),
            },
            rect: (1, 0, 1, 1),
            visible: true,
        }));
        let mut painter = Painter::new(
            source,
            popup,
            Arc::new(Mutex::new((2, 2))),
            Arc::new(Mutex::new(1.)),
            Rc::new(RefCell::new(None)),
            Rc::new(Cell::new(1)),
        );
        // Simulate device loss after the current page and popup frames were
        // already consumed. They must be re-uploaded without a new CEF paint.
        painter.device_changed();
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
        assert_eq!(
            pixels[1],
            [255, 0, 0, 255],
            "popup overlays top-right pixel"
        );
        assert_eq!(pixels[2], [0, 0, 255, 255], "bottom-left CEF pixel");
        assert_eq!(pixels[3], [255, 255, 255, 255], "bottom-right CEF pixel");
    }
}

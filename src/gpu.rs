//! wgpu setup and the single instanced-quad pipeline everything is drawn with.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

/// One quad. `mode` 0 draws `color` flat; 1 samples the atlas as an alpha mask.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Inst {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub mode: u32,
    pub _pad: [u32; 3],
}

impl Inst {
    pub fn solid(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> Self {
        Inst { rect: [x, y, w, h], uv: [0.0; 4], color, mode: 0, _pad: [0; 3] }
    }
    pub fn glyph(rect: [f32; 4], uv: [f32; 4], color: [f32; 4]) -> Self {
        Inst { rect, uv, color, mode: 1, _pad: [0; 3] }
    }
}

/// What happened when we tried to put a frame on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameStatus {
    Presented,
    /// Transient — skip this frame, nothing to fix.
    Skipped,
    /// The swapchain is stale; reconfigure the surface and draw again.
    NeedsReconfigure,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    screen: [f32; 2],
    _pad: [f32; 2],
}

pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    inst_buf: wgpu::Buffer,
    inst_cap: usize,
    atlas_tex: wgpu::Texture,
    atlas_size: u32,
    sampler: wgpu::Sampler,
}

const INITIAL_INSTANCES: usize = 8192;

impl Gpu {
    pub fn new(window: Arc<Window>, atlas_size: u32) -> Result<Self> {
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(window.clone()),
        ));
        let surface = instance.create_surface(window.clone())?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .context("no suitable GPU adapter")?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("koma device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("failed to create GPU device")?;

        let caps = surface.get_capabilities(&adapter);
        let mut config = surface
            .get_default_config(&adapter, w, h)
            .context("surface is not supported by this adapter")?;
        // An sRGB target lets the GPU do the encode, so `theme::to_linear`
        // values land as the colours we actually picked.
        if let Some(srgb) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        let format = config.format;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("koma shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: atlas_size,
                height: atlas_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Nearest keeps glyph edges crisp; the atlas is already at device scale.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("koma bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
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

        let bind_group = make_bind_group(&device, &bgl, &globals_buf, &atlas_tex, &sampler);

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("koma layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let attrs = wgpu::vertex_attr_array![
            0 => Float32x4,
            1 => Float32x4,
            2 => Float32x4,
            3 => Uint32,
        ];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("koma pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Inst>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (INITIAL_INSTANCES * std::mem::size_of::<Inst>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Gpu {
            surface,
            device,
            queue,
            config,
            pipeline,
            bgl,
            bind_group,
            globals_buf,
            inst_buf,
            inst_cap: INITIAL_INSTANCES,
            atlas_tex,
            atlas_size,
            sampler,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Uploads the rows of the atlas that changed since the last frame.
    pub fn upload_atlas(&mut self, atlas: &mut crate::font::Atlas) {
        let Some((y0, y1)) = atlas.dirty.take() else {
            return;
        };
        let y1 = y1.min(self.atlas_size);
        if y0 >= y1 {
            return;
        }
        let rows = y1 - y0;
        let start = (y0 * self.atlas_size) as usize;
        let end = (y1 * self.atlas_size) as usize;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &atlas.data[start..end],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.atlas_size),
                rows_per_image: Some(rows),
            },
            wgpu::Extent3d { width: self.atlas_size, height: rows, depth_or_array_layers: 1 },
        );
    }

    fn ensure_capacity(&mut self, n: usize) {
        if n <= self.inst_cap {
            return;
        }
        let cap = n.next_power_of_two();
        self.inst_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (cap * std::mem::size_of::<Inst>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.inst_cap = cap;
    }

    pub fn render(&mut self, instances: &[Inst], clear: [f32; 4]) -> FrameStatus {
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            // Nothing to present right now; the next redraw will retry.
            Cst::Timeout | Cst::Occluded => return FrameStatus::Skipped,
            Cst::Outdated | Cst::Lost | Cst::Validation => return FrameStatus::NeedsReconfigure,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.queue.write_buffer(
            &self.globals_buf,
            0,
            bytemuck::bytes_of(&Globals {
                screen: [self.config.width as f32, self.config.height as f32],
                _pad: [0.0; 2],
            }),
        );
        self.ensure_capacity(instances.len());
        if !instances.is_empty() {
            self.queue.write_buffer(&self.inst_buf, 0, bytemuck::cast_slice(instances));
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0] as f64,
                            g: clear[1] as f64,
                            b: clear[2] as f64,
                            a: clear[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.inst_buf.slice(..));
                pass.draw(0..4, 0..instances.len() as u32);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        FrameStatus::Presented
    }

    /// Recreates the atlas texture (used when the font size changes enough that
    /// the packer is reset).
    pub fn rebuild_bind_group(&mut self) {
        self.bind_group = make_bind_group(
            &self.device,
            &self.bgl,
            &self.globals_buf,
            &self.atlas_tex,
            &self.sampler,
        );
    }
}

fn make_bind_group(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    atlas: &wgpu::Texture,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    let view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("koma bind group"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}

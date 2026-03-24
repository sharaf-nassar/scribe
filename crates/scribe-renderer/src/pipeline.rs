use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, BlendState, Buffer, BufferBindingType,
    BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, CommandEncoder, Device,
    FragmentState, MultisampleState, PipelineCompilationOptions, PipelineLayoutDescriptor,
    PrimitiveState, PrimitiveTopology, Queue, RenderPipeline, RenderPipelineDescriptor, Sampler,
    SamplerBindingType, ShaderModuleDescriptor, ShaderStages, TextureFormat, TextureSampleType,
    TextureView, TextureViewDimension, VertexAttribute, VertexBufferLayout, VertexFormat,
    VertexState, VertexStepMode,
};

use crate::types::CellInstance;

/// Initial instance buffer capacity (number of cells).
const INITIAL_INSTANCE_CAPACITY: u32 = 10_000;

/// Size of the uniform buffer in bytes: viewport `vec2<f32>` + cell size `vec2<f32>`.
const UNIFORM_BUFFER_SIZE: u64 = 16;

/// Configuration needed to create a [`TerminalPipeline`].
pub struct PipelineConfig<'a> {
    pub device: &'a Device,
    pub queue: &'a Queue,
    pub surface_format: TextureFormat,
    pub atlas_view: &'a TextureView,
    pub atlas_sampler: &'a Sampler,
    pub viewport_size: (u32, u32),
    pub cell_size: (f32, f32),
}

/// wgpu render pipeline for terminal cell rendering.
///
/// Draws instanced quads -- one per terminal cell -- using a glyph atlas
/// texture and per-instance colour / UV data.
pub struct TerminalPipeline {
    render_pipeline: RenderPipeline,
    bind_group: BindGroup,
    bind_group_layout: BindGroupLayout,
    uniform_buffer: Buffer,
    instance_buffer: Buffer,
    instance_count: u32,
    max_instances: u32,
}

impl TerminalPipeline {
    /// Create the render pipeline, bind groups, and instance buffer.
    pub fn new(cfg: &PipelineConfig<'_>) -> Self {
        let shader = cfg.device.create_shader_module(ShaderModuleDescriptor {
            label: Some("terminal shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/terminal.wgsl").into()),
        });

        let uniform_buffer = create_uniform_buffer(cfg.device);
        write_uniforms(cfg.queue, &uniform_buffer, cfg.viewport_size, cfg.cell_size);

        let bind_group_layout = create_bind_group_layout(cfg.device);

        let bind_group = create_bind_group(
            cfg.device,
            &bind_group_layout,
            &uniform_buffer,
            cfg.atlas_view,
            cfg.atlas_sampler,
        );

        let render_pipeline =
            create_render_pipeline(cfg.device, cfg.surface_format, &bind_group_layout, &shader);

        let instance_buffer = create_instance_buffer(cfg.device, INITIAL_INSTANCE_CAPACITY);

        Self {
            render_pipeline,
            bind_group,
            bind_group_layout,
            uniform_buffer,
            instance_buffer,
            instance_count: 0,
            max_instances: INITIAL_INSTANCE_CAPACITY,
        }
    }

    /// Upload instance data to the GPU, growing the buffer if needed.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "instance count is bounded by buffer capacity which fits in u32"
    )]
    pub fn update_instances(&mut self, device: &Device, queue: &Queue, instances: &[CellInstance]) {
        let count = instances.len() as u32;

        if count > self.max_instances {
            let new_capacity = count.max(self.max_instances.saturating_mul(2));
            self.instance_buffer = create_instance_buffer(device, new_capacity);
            self.max_instances = new_capacity;
        }

        self.instance_count = count;

        if !instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(instances));
        }
    }

    /// Update the viewport and cell-size uniforms.
    pub fn update_viewport(&self, queue: &Queue, viewport: (u32, u32), cell_size: (f32, f32)) {
        write_uniforms(queue, &self.uniform_buffer, viewport, cell_size);
    }

    /// Record a render pass that draws all terminal cells with a black clear color.
    pub fn render(&self, encoder: &mut CommandEncoder, target: &TextureView) {
        self.render_with_clear(encoder, target, [0.0, 0.0, 0.0, 1.0]);
    }

    /// Record a render pass that draws all terminal cells with the given clear color.
    pub fn render_with_clear(
        &self,
        encoder: &mut CommandEncoder,
        target: &TextureView,
        clear_color: [f32; 4],
    ) {
        let clear = rgba_to_wgpu_color(clear_color);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terminal render pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });

        pass.set_pipeline(&self.render_pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        pass.draw(0..6, 0..self.instance_count);
    }

    /// Recreate the bind group (e.g. after atlas texture changes).
    pub fn rebuild_bind_group(
        &mut self,
        device: &Device,
        atlas_view: &TextureView,
        atlas_sampler: &Sampler,
    ) {
        self.bind_group = create_bind_group(
            device,
            &self.bind_group_layout,
            &self.uniform_buffer,
            atlas_view,
            atlas_sampler,
        );
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Create the uniform buffer for viewport and cell-size data.
fn create_uniform_buffer(device: &Device) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("terminal uniforms"),
        size: UNIFORM_BUFFER_SIZE,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Build the bind-group layout with uniform, texture, and sampler bindings.
fn create_bind_group_layout(device: &Device) -> BindGroupLayout {
    device.create_bind_group_layout(&BindGroupLayoutDescriptor {
        label: Some("terminal bind group layout"),
        entries: &[
            BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 1,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: true },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 2,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Sampler(SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// Create the wgpu render pipeline.
fn create_render_pipeline(
    device: &Device,
    surface_format: TextureFormat,
    bind_group_layout: &BindGroupLayout,
    shader: &wgpu::ShaderModule,
) -> RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("terminal pipeline layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("terminal render pipeline"),
        layout: Some(&pipeline_layout),
        vertex: VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: PipelineCompilationOptions::default(),
            buffers: &[instance_buffer_layout()],
        },
        fragment: Some(FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: PipelineCompilationOptions::default(),
            targets: &[Some(ColorTargetState {
                format: surface_format,
                blend: Some(BlendState::ALPHA_BLENDING),
                write_mask: ColorWrites::ALL,
            })],
        }),
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleList,
            ..PrimitiveState::default()
        },
        depth_stencil: None,
        multisample: MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Describe the per-instance vertex buffer layout for `CellInstance`.
fn instance_buffer_layout() -> VertexBufferLayout<'static> {
    const ATTRS: &[VertexAttribute] = &[
        VertexAttribute { format: VertexFormat::Float32x2, offset: 0, shader_location: 0 }, // pos
        VertexAttribute { format: VertexFormat::Float32x2, offset: 8, shader_location: 1 }, // size
        VertexAttribute { format: VertexFormat::Float32x2, offset: 16, shader_location: 2 }, // uv_min
        VertexAttribute { format: VertexFormat::Float32x2, offset: 24, shader_location: 3 }, // uv_max
        VertexAttribute { format: VertexFormat::Float32x4, offset: 32, shader_location: 4 }, // fg_color
        VertexAttribute { format: VertexFormat::Float32x4, offset: 48, shader_location: 5 }, // bg_color
    ];

    VertexBufferLayout {
        array_stride: std::mem::size_of::<CellInstance>() as u64,
        step_mode: VertexStepMode::Instance,
        attributes: ATTRS,
    }
}

/// Write viewport and cell-size data into the uniform buffer.
#[allow(
    clippy::cast_precision_loss,
    reason = "viewport dimensions are small enough (< 2^23) to fit exactly in f32"
)]
fn write_uniforms(queue: &Queue, buffer: &Buffer, viewport: (u32, u32), cell_size: (f32, f32)) {
    let data: [f32; 4] = [viewport.0 as f32, viewport.1 as f32, cell_size.0, cell_size.1];
    queue.write_buffer(buffer, 0, bytemuck::cast_slice(&data));
}

/// Create an instance buffer with room for `capacity` `CellInstance` entries.
fn create_instance_buffer(device: &Device, capacity: u32) -> Buffer {
    let size = u64::from(capacity) * std::mem::size_of::<CellInstance>() as u64;
    device.create_buffer(&BufferDescriptor {
        label: Some("terminal instance buffer"),
        size,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Create the bind group with the uniform buffer, atlas texture view, and sampler.
fn create_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    uniform_buffer: &Buffer,
    atlas_view: &TextureView,
    atlas_sampler: &Sampler,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("terminal bind group"),
        layout,
        entries: &[
            BindGroupEntry { binding: 0, resource: uniform_buffer.as_entire_binding() },
            BindGroupEntry { binding: 1, resource: BindingResource::TextureView(atlas_view) },
            BindGroupEntry { binding: 2, resource: BindingResource::Sampler(atlas_sampler) },
        ],
    })
}

/// Convert `[f32; 4]` (already in linear space) to `wgpu::Color`.
fn rgba_to_wgpu_color(color: [f32; 4]) -> wgpu::Color {
    wgpu::Color {
        r: f64::from(color.first().copied().unwrap_or(0.0)),
        g: f64::from(color.get(1).copied().unwrap_or(0.0)),
        b: f64::from(color.get(2).copied().unwrap_or(0.0)),
        a: f64::from(color.get(3).copied().unwrap_or(1.0)),
    }
}

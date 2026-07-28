//! Splash screen rendered while waiting for the first PTY output.
//!
//! Uses a dedicated, lightweight wgpu pipeline that draws the Scribe logo
//! centred on a black background.  The pipeline is entirely separate from the
//! terminal pipeline so there is no texture-switching overhead on the hot path.

use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, Buffer, BufferDescriptor, BufferUsages,
    ColorTargetState, ColorWrites, CommandEncoder, Device, Extent3d, FragmentState,
    MultisampleState, Origin3d, PipelineCompilationOptions, PipelineLayoutDescriptor,
    PrimitiveState, PrimitiveTopology, Queue, RenderPipeline, RenderPipelineDescriptor, Sampler,
    SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor, ShaderStages,
    TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor,
    TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView,
    TextureViewDescriptor, TextureViewDimension, VertexState,
};

/// Embedded logo PNG bytes (compiled into the binary at build time).
static LOGO_PNG: &[u8] = include_bytes!("../../../dist/scribe-icon-512.png");

/// Display size of the logo in logical pixels (fits comfortably on any screen).
const LOGO_DISPLAY_PX: f32 = 256.0;

/// Size of the uniform buffer: viewport vec2 + logo vec2 = 4 × f32 = 16 bytes.
const UNIFORM_BYTES: u64 = 16;

/// Decoded logo pixel data and dimensions.
struct LogoImage {
    /// RGBA8 pixels, row-major.
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

/// Decode the embedded PNG.
///
/// The icon ships as RGB (no alpha).  We expand to RGBA8 so the GPU
/// texture format is always `Rgba8UnormSrgb`.
fn decode_logo() -> Result<LogoImage, SplashError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(LOGO_PNG));
    let mut reader = decoder.read_info().map_err(SplashError::PngDecode)?;

    let info = reader.info();
    let width = info.width;
    let height = info.height;
    let color_type = info.color_type;

    let mut raw = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut raw).map_err(SplashError::PngDecode)?;
    // `frame.buffer_size()` is guaranteed ≤ `output_buffer_size()` by the png crate.
    let frame_end = frame.buffer_size().min(raw.len());
    let raw: &[u8] = raw.get(..frame_end).unwrap_or_default();

    // Convert to RGBA8 regardless of source colour type.
    let pixels = match color_type {
        png::ColorType::Rgba => raw.to_vec(),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(raw.len() / 3 * 4);
            for chunk in raw.chunks(3) {
                let r = chunk.first().copied().unwrap_or(0);
                let g = chunk.get(1).copied().unwrap_or(0);
                let b = chunk.get(2).copied().unwrap_or(0);
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);
                rgba.push(255);
            }
            rgba
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(raw.len() / 2 * 4);
            for chunk in raw.chunks(2) {
                let luma = chunk.first().copied().unwrap_or(0);
                let alpha = chunk.get(1).copied().unwrap_or(255);
                rgba.push(luma);
                rgba.push(luma);
                rgba.push(luma);
                rgba.push(alpha);
            }
            rgba
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(raw.len() * 4);
            for &luma in raw {
                rgba.push(luma);
                rgba.push(luma);
                rgba.push(luma);
                rgba.push(255);
            }
            rgba
        }
        // Indexed colour is unusual for icons; fall back to a small magenta square
        // rather than panicking or leaving garbage.
        png::ColorType::Indexed => {
            tracing::warn!("splash logo uses indexed colour; rendering placeholder");
            let size = (width * height * 4) as usize;
            let mut rgba = vec![0u8; size];
            for chunk in rgba.chunks_mut(4) {
                if let Some(r) = chunk.get_mut(0) {
                    *r = 255;
                }
                if let Some(a) = chunk.get_mut(3) {
                    *a = 255;
                }
            }
            rgba
        }
    };

    Ok(LogoImage { pixels, width, height })
}

/// Upload pixel data as a 2-D RGBA8 sRGB texture.
fn upload_texture(device: &Device, queue: &Queue, image: &LogoImage) -> (Texture, TextureView) {
    let size = Extent3d { width: image.width, height: image.height, depth_or_array_layers: 1 };

    let texture = device.create_texture(&TextureDescriptor {
        label: Some("splash logo texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8UnormSrgb,
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        view_formats: &[],
    });

    queue.write_texture(
        TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        &image.pixels,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(image.width * 4),
            rows_per_image: Some(image.height),
        },
        size,
    );

    let view = texture.create_view(&TextureViewDescriptor::default());
    (texture, view)
}

/// Create the sampler for the logo texture (linear, clamp-to-edge).
fn create_sampler(device: &Device) -> Sampler {
    device.create_sampler(&SamplerDescriptor {
        label: Some("splash sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..SamplerDescriptor::default()
    })
}

fn create_bind_group_layout(device: &Device) -> BindGroupLayout {
    device.create_bind_group_layout(&BindGroupLayoutDescriptor {
        label: Some("splash bind group layout"),
        entries: &[
            // binding 0: uniforms (viewport + logo size)
            BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX,
                ty: BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            // binding 1: logo texture
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
            // binding 2: sampler
            BindGroupLayoutEntry {
                binding: 2,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Sampler(SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

fn create_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    uniform_buf: &Buffer,
    logo_view: &TextureView,
    sampler: &Sampler,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("splash bind group"),
        layout,
        entries: &[
            BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
            BindGroupEntry { binding: 1, resource: BindingResource::TextureView(logo_view) },
            BindGroupEntry { binding: 2, resource: BindingResource::Sampler(sampler) },
        ],
    })
}

fn create_pipeline(
    device: &Device,
    surface_format: TextureFormat,
    layout: &BindGroupLayout,
) -> RenderPipeline {
    let shader = device.create_shader_module(ShaderModuleDescriptor {
        label: Some("splash shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/splash.wgsl").into()),
    });

    let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("splash pipeline layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("splash render pipeline"),
        layout: Some(&pipeline_layout),
        vertex: VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: PipelineCompilationOptions::default(),
            // No vertex buffer — positions are generated in the shader.
            buffers: &[],
        },
        fragment: Some(FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: PipelineCompilationOptions::default(),
            targets: &[Some(ColorTargetState {
                format: surface_format,
                // Logo may have alpha; blend over the black background.
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// GPU resources for the splash screen.
///
/// Drop this once `splash_active` becomes `false` to free the logo texture.
pub struct SplashRenderer {
    /// We hold the texture to prevent it being freed while the view is live.
    _logo_texture: Texture,
    uniform_buf: Buffer,
    bind_group: BindGroup,
    pipeline: RenderPipeline,
    /// Viewport size at construction time; updated via [`Self::update_viewport`].
    viewport: (u32, u32),
}

impl SplashRenderer {
    /// Decode the embedded logo PNG, upload it to the GPU, and build the
    /// splash render pipeline.
    ///
    /// Returns `Err` if the PNG cannot be decoded; the caller should log the
    /// error and skip the splash rather than crashing.
    pub fn new(
        device: &Device,
        queue: &Queue,
        surface_format: TextureFormat,
        viewport: (u32, u32),
    ) -> Result<Self, SplashError> {
        let image = decode_logo()?;
        let (logo_texture, logo_view) = upload_texture(device, queue, &image);
        let sampler = create_sampler(device);

        let layout = create_bind_group_layout(device);

        let uniform_buf = device.create_buffer(&BufferDescriptor {
            label: Some("splash uniform buffer"),
            size: UNIFORM_BYTES,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        write_uniforms(queue, &uniform_buf, viewport);

        let bind_group = create_bind_group(device, &layout, &uniform_buf, &logo_view, &sampler);
        let pipeline = create_pipeline(device, surface_format, &layout);

        Ok(Self { _logo_texture: logo_texture, uniform_buf, bind_group, pipeline, viewport })
    }

    /// Update the uniform buffer when the window is resized.
    pub fn update_viewport(&mut self, queue: &Queue, viewport: (u32, u32)) {
        self.viewport = viewport;
        write_uniforms(queue, &self.uniform_buf, viewport);
    }

    /// Record a render pass that clears to black and draws the centred logo.
    pub fn render(&self, encoder: &mut CommandEncoder, target: &TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("splash render pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        // 6 vertices (2 triangles), 1 instance — no vertex buffer needed.
        pass.draw(0..6, 0..1);
    }
}

// ---------------------------------------------------------------------------
// Uniform writer
// ---------------------------------------------------------------------------

/// Write `[viewport_w, viewport_h, logo_display_w, logo_display_h]` to the
/// uniform buffer.
fn write_uniforms(queue: &Queue, buf: &Buffer, viewport: (u32, u32)) {
    let data: [f32; 4] = [
        f32::from(u16::try_from(viewport.0).unwrap_or(u16::MAX)),
        f32::from(u16::try_from(viewport.1).unwrap_or(u16::MAX)),
        LOGO_DISPLAY_PX,
        LOGO_DISPLAY_PX,
    ];
    queue.write_buffer(buf, 0, bytemuck::cast_slice(&data));
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while setting up the splash screen.
#[derive(Debug, thiserror::Error)]
pub enum SplashError {
    #[error("PNG decode failed: {0}")]
    PngDecode(#[from] png::DecodingError),
}

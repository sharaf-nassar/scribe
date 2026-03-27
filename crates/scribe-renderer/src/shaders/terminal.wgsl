struct Uniforms {
    viewport: vec2<f32>,
    cell_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var atlas_texture: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct CellInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
    @location(4) fg_color: vec4<f32>,
    @location(5) bg_color: vec4<f32>,
    @location(6) corner_radius: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg: vec4<f32>,
    @location(2) bg: vec4<f32>,
    @location(3) local_pos: vec2<f32>,
    @location(4) @interpolate(flat) quad_size_px: vec2<f32>,
    @location(5) @interpolate(flat) inst_corner_r: f32,
};

@vertex fn vs_main(
    @builtin(vertex_index) vi: u32,
    instance: CellInstance,
) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let corner = corners[vi];
    let quad_size = select(uniforms.cell_size, instance.size, instance.size.x > 0.0);
    let pixel_pos = instance.pos + corner * quad_size;
    let clip_pos = vec2(
        (pixel_pos.x / uniforms.viewport.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.viewport.y) * 2.0,
    );
    let uv = mix(instance.uv_min, instance.uv_max, corner);

    var out: VertexOutput;
    out.position = vec4(clip_pos, 0.0, 1.0);
    out.uv = uv;
    out.fg = instance.fg_color;
    out.bg = instance.bg_color;
    out.local_pos = corner;
    out.quad_size_px = quad_size;
    out.inst_corner_r = instance.corner_radius;
    return out;
}

fn sdf_rounded_rect(p: vec2<f32>, half_size: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half_size + vec2(r, r);
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let glyph_alpha = textureSample(atlas_texture, atlas_sampler, in.uv).a;
    let base = mix(in.bg, in.fg, glyph_alpha);

    // Corner radius below 0.5px is treated as sharp (no rounding).
    // Callers must use corner_radius >= 0.5 for visible rounding.
    if in.inst_corner_r < 0.5 {
        return base;
    }

    // SDF rounded rect clip
    let local = (in.local_pos - vec2(0.5)) * in.quad_size_px;
    let half = in.quad_size_px * 0.5;
    let dist = sdf_rounded_rect(local, half, in.inst_corner_r);
    let aa = 1.0 - smoothstep(-0.5, 0.5, dist);
    return vec4(base.rgb, base.a * aa);
}

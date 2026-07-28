// Splash screen shader.
//
// Draws a single centered textured quad using the logo texture.
// The uniform contains [viewport_w, viewport_h, logo_w, logo_h] (all f32).
// The vertex shader generates a two-triangle quad (6 vertices) from
// vertex_index alone — no vertex buffer needed.

struct Uniforms {
    viewport: vec2<f32>,
    logo:     vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var logo_texture: texture_2d<f32>;
@group(0) @binding(2) var logo_sampler: sampler;

struct VertexOutput {
    @builtin(position) pos: vec4<f32>,
    @location(0)       uv:  vec2<f32>,
};

@vertex fn vs_main(@builtin(vertex_index) vi: u32) -> VertexOutput {
    // Unit-square corners for a two-triangle quad (CCW winding).
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let c = corners[vi];

    // Pixel top-left corner of the centered logo.
    let half_vp  = u.viewport * 0.5;
    let half_logo = u.logo   * 0.5;
    let origin   = half_vp - half_logo;

    // Pixel position of this vertex.
    let pixel = origin + c * u.logo;

    // Convert to clip space: x in [-1, 1], y flipped (pixel 0 = top).
    let clip = vec2(
        (pixel.x / u.viewport.x) * 2.0 - 1.0,
        1.0 - (pixel.y / u.viewport.y) * 2.0,
    );

    var out: VertexOutput;
    out.pos = vec4(clip, 0.0, 1.0);
    out.uv  = c;
    return out;
}

@fragment fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(logo_texture, logo_sampler, in.uv);
}

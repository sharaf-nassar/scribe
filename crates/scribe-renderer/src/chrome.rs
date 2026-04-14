use crate::types::CellInstance;

#[derive(Clone, Copy)]
pub struct QuadRect {
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// Build a solid-color chrome quad (no glyph, no rounding).
pub fn solid_quad(
    x_pos: f32,
    y_pos: f32,
    width: f32,
    height: f32,
    color: [f32; 4],
) -> CellInstance {
    CellInstance {
        pos: [x_pos, y_pos],
        size: [width, height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: 0.0,
    }
}

/// Build a rounded chrome quad with the given corner radius.
pub fn rounded_quad(rect: QuadRect, color: [f32; 4], radius: f32) -> CellInstance {
    CellInstance {
        pos: rect.pos,
        size: rect.size,
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: radius,
    }
}

use crate::types::CellInstance;

/// Build a solid-color chrome quad (no glyph, no rounding).
#[allow(
    clippy::many_single_char_names,
    reason = "x/y/w/h are conventional 2-D geometry shorthands"
)]
pub fn solid_quad(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> CellInstance {
    CellInstance {
        pos: [x, y],
        size: [w, h],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: 0.0,
        _pad: 0.0,
    }
}

/// Build a rounded chrome quad with the given corner radius.
#[allow(
    clippy::many_single_char_names,
    reason = "x/y/w/h are conventional 2-D geometry shorthands"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "convenience constructor needs position, size, color, and radius"
)]
pub fn rounded_quad(x: f32, y: f32, w: f32, h: f32, color: [f32; 4], radius: f32) -> CellInstance {
    CellInstance {
        pos: [x, y],
        size: [w, h],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: radius,
        _pad: 0.0,
    }
}

/// GPU instance data for a single terminal cell.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CellInstance {
    pub pos: [f32; 2],
    /// Per-instance quad size override. `[0.0, 0.0]` means "use `uniforms.cell_size`".
    pub size: [f32; 2],
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    pub fg_color: [f32; 4],
    pub bg_color: [f32; 4],
}

#[derive(Debug, Clone, Copy)]
pub struct CellSize {
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

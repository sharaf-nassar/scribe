//! A GPUI paint-quad overlay proof for Scribe's procedural box drawing.

#[path = "../../../crates/scribe-renderer/src/box_drawing.rs"]
mod box_drawing;

use gpui::{
    App, Bounds, Context, Render, Window, WindowBounds, WindowOptions, canvas, div, fill, point,
    prelude::*, px, rgb, size,
};
use gpui_platform::application;

const CELL_WIDTH: u32 = 16;
const CELL_HEIGHT: u32 = 24;
const COLUMNS: usize = 8;
const DEMO_GLYPHS: &[char] = &[
    '┌', '─', '┬', '┐', '├', '┼', '┤', '│', '└', '┴', '┘', '━', '┃', '╋', '╔', '╗', '╚', '╝', '▀',
    '▄', '█', '▌', '▐', '░', '▒', '▓', '▖', '▗', '▘', '▝', '▚', '▞',
];

struct BoxDrawingDemo;

impl Render for BoxDrawingDemo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x111827))
            .p_4()
            .child("GPUI paint-quad procedural box-drawing overlay")
            .child(
                canvas(
                    |_bounds, _window, _cx| (),
                    |bounds, (), window, _cx| paint_demo(bounds, window),
                )
                .w_full()
                .h(px(120.)),
            )
    }
}

fn paint_demo(bounds: Bounds<gpui::Pixels>, window: &mut Window) {
    let origin = point(bounds.left(), bounds.top() + px(32.));

    for (index, glyph) in DEMO_GLYPHS.iter().copied().enumerate() {
        let column = index % COLUMNS;
        let row = index / COLUMNS;
        let cell_origin = origin
            + point(px((column as u32 * CELL_WIDTH) as f32), px((row as u32 * CELL_HEIGHT) as f32));
        paint_procedural_cell(glyph, cell_origin, window);
    }
}

/// Routes a terminal cell through the same procedural rasterizer as the
/// renderer, then emits only GPUI paint quads for its opaque mask runs.
fn paint_procedural_cell(glyph: char, origin: gpui::Point<gpui::Pixels>, window: &mut Window) {
    if !box_drawing::is_box_drawing(glyph) {
        return;
    }

    let Some((width, height, mask)) = box_drawing::render(glyph, CELL_WIDTH, CELL_HEIGHT) else {
        return;
    };

    for y in 0..height {
        let mut x = 0;
        while x < width {
            let alpha = alpha_at(&mask, width, x, y);
            if alpha == 0 {
                x += 1;
                continue;
            }

            let run_start = x;
            x += 1;
            while x < width && alpha_at(&mask, width, x, y) == alpha {
                x += 1;
            }

            window.paint_quad(fill(
                Bounds::new(
                    origin + point(px(run_start as f32), px(y as f32)),
                    size(px((x - run_start) as f32), px(1.)),
                ),
                rgb(0xe5e7eb).alpha(f32::from(alpha) / f32::from(u8::MAX)),
            ));
        }
    }
}

fn alpha_at(mask: &[u8], width: u32, x: u32, y: u32) -> u8 {
    let pixel_index = ((y * width + x) * 4 + 3) as usize;
    mask[pixel_index]
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(420.), px(230.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| BoxDrawingDemo),
        )
        .expect("GPUI should open the box-drawing spike window");
        cx.activate(true);
    });
}

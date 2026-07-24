//! Opens a transparent GPUI window and changes its rendered background alpha live.

use gpui::{
    App, Bounds, ClickEvent, Context, Render, Window, WindowBackgroundAppearance, WindowBounds,
    WindowOptions, div, prelude::*, px, rgba, size, white,
};
use gpui_platform::application;

const INITIAL_OPACITY: f32 = 0.65;
const OPACITY_STEP: f32 = 0.1;

struct OpacitySpike {
    opacity: f32,
}

impl OpacitySpike {
    fn decrease_opacity(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.opacity = (self.opacity - OPACITY_STEP).max(OPACITY_STEP);
        cx.notify();
    }

    fn increase_opacity(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.opacity = (self.opacity + OPACITY_STEP).min(1.0);
        cx.notify();
    }
}

impl Render for OpacitySpike {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let alpha = (self.opacity * 255.0).round() as u32;
        let background = rgba(0x17203300 | alpha);

        div()
            .size_full()
            .bg(background)
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .text_color(white())
            .child("GPUI window opacity spike")
            .child(format!("live opacity: {:.0}%", self.opacity * 100.0))
            .child(
                div()
                    .id("decrease-opacity")
                    .cursor_pointer()
                    .px_3()
                    .py_2()
                    .bg(rgba(0xffffff33))
                    .child("Decrease opacity")
                    .on_click(cx.listener(Self::decrease_opacity)),
            )
            .child(
                div()
                    .id("increase-opacity")
                    .cursor_pointer()
                    .px_3()
                    .py_2()
                    .bg(rgba(0xffffff33))
                    .child("Increase opacity")
                    .on_click(cx.listener(Self::increase_opacity)),
            )
    }
}

fn main() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(520.0), px(260.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |_, cx| cx.new(|_| OpacitySpike { opacity: INITIAL_OPACITY }),
        )
        .expect("GPUI should open the opacity spike window");
        cx.activate(true);
    });
}

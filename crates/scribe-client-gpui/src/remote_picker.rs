//! GPUI paint surface for the remote-connect picker.

use gpui::{AnyElement, Rgba, div, prelude::*, px};
use scribe_common::theme::ChromeColors;

use crate::{remote::PickerView, tab_bar::srgba};

/// Theme-derived colours for the remote-connect picker.
#[derive(Clone, Copy)]
pub struct RemotePickerColors {
    backdrop: Rgba,
    panel: Rgba,
    border: Rgba,
    title: Rgba,
    text: Rgba,
    dim: Rgba,
    selected: Rgba,
}

impl From<&ChromeColors> for RemotePickerColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            backdrop: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.62 },
            panel: Rgba { a: 0.98, ..srgba(chrome.tab_bar_active_bg) },
            border: Rgba { a: 0.45, ..srgba(chrome.accent) },
            title: srgba(chrome.tab_text_active),
            text: srgba(chrome.tab_text),
            dim: Rgba { a: 0.55, ..srgba(chrome.tab_text) },
            selected: Rgba { a: 0.22, ..srgba(chrome.accent) },
        }
    }
}

/// Paint a picker snapshot as a centered, keyboard-owned overlay.
#[must_use]
pub fn remote_picker_overlay(view: &PickerView, colors: &RemotePickerColors) -> AnyElement {
    let rows = view.rows.iter().enumerate().map(|(index, row)| {
        let selected = view.selectable && index == view.selected;
        div()
            .px_3()
            .py_1()
            .bg(if selected { colors.selected } else { Rgba::default() })
            .text_sm()
            .text_color(if row.dim { colors.dim } else { colors.text })
            .child(row.text.clone())
    });
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.backdrop)
        .child(
            div()
                .w(px(520.0))
                .max_w(px(520.0))
                .p_4()
                .flex()
                .flex_col()
                .gap_2()
                .bg(colors.panel)
                .border_1()
                .border_color(colors.border)
                .rounded(px(4.0))
                .child(div().text_lg().text_color(colors.title).child(view.title.clone()))
                .children(
                    view.subtitle
                        .as_ref()
                        .map(|text| div().text_sm().text_color(colors.text).child(text.clone())),
                )
                .child(div().flex().flex_col().gap_1().children(rows))
                .children(
                    view.footer.as_ref().map(|text| {
                        div().mt_2().text_xs().text_color(colors.dim).child(text.clone())
                    }),
                ),
        )
        .into_any_element()
}

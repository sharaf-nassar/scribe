//! Workspace-notes hover preview for the GPUI client rebuild.
//!
//! The winit client painted this preview as GPU quads over a grid
//! ([`workspace_notes_preview.rs`](../../scribe-client/src/workspace_notes_preview.rs)),
//! returning hit rects the shell tested. This port keeps the pure sizing/wrap
//! logic — column width from the longest note or editor line, visual-row wrap,
//! and caret line/column geometry (FR-022) — while painting the surfaces with
//! GPUI elements. It has two modes: a read-only list of active-note summaries
//! with an overflow "+N more" row and a bottom-right "+" affordance (FR-001),
//! and an inline editor (FR-002) that renders the shared draft with a caret,
//! an optional server-error row, and a scroll indicator. Clicking the
//! affordance, a note row, or the editor emits a
//! [`WorkspaceNotesPreviewAction`] for the shell to route.

use gpui::{Context, EventEmitter, Rgba, div, prelude::*, px};
use scribe_common::theme::ChromeColors;

use crate::tab_bar::srgba;
use crate::workspace_notes::{AddingNoteState, WorkspaceNoteSummary};

/// Maximum note rows shown before the preview stops listing (overflow becomes a
/// "+N more" row).
pub const MAX_PREVIEW_ROWS: usize = 12;
/// Minimum content columns the preview sizes to.
pub const MIN_PREVIEW_COLS: usize = 22;
/// Maximum content columns the preview grows to.
pub const MAX_PREVIEW_COLS: usize = 64;
/// Horizontal padding, in cells, on each side of the preview content.
pub const PAD_COLS: usize = 1;
/// Leading indent (in cells) the inline editor reserves for its accent "›"
/// prompt plus one separating space.
pub const EDITOR_PREFIX_COLS: usize = 2;
/// Fallback maximum editor rows when the caller passes no pane-derived cap.
pub const MIN_EDITOR_ROWS: usize = 1;

/// A control the user activated in the preview, routed by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceNotesPreviewAction {
    /// The "+" affordance was clicked — open the inline editor (FR-002).
    OpenEditor,
    /// A note row was clicked — archive it as done.
    ArchiveNote(String),
    /// The inline editor row was clicked — take focus, absorbing the click so
    /// it never archives a note behind it (FR-011).
    FocusEditor,
}

impl EventEmitter<WorkspaceNotesPreviewAction> for WorkspaceNotesPreviewView {}

/// The number of content columns the preview should size to, given the longest
/// note summary (or editor line) it must fit. Ported from the winit
/// `PreviewLayout::new` width computation.
#[must_use]
pub fn preview_cols(summaries: &[WorkspaceNoteSummary], editor: Option<&AddingNoteState>) -> usize {
    let longest = summaries
        .iter()
        .map(|summary| summary.text.chars().count().saturating_add(2))
        .max()
        .unwrap_or_else(|| "No active notes".chars().count());
    let editor_longest = editor.map_or(0, |state| longest_visible_line_chars(&state.draft_text));
    let longest = longest.max(editor_longest.saturating_add(EDITOR_PREFIX_COLS));
    longest.saturating_add(PAD_COLS * 2).clamp(MIN_PREVIEW_COLS, MAX_PREVIEW_COLS)
}

/// The editor content width (columns available for wrapped text) at a given
/// preview column count.
#[must_use]
pub fn editor_content_cols(cols: usize) -> usize {
    cols.saturating_sub(PAD_COLS * 2 + EDITOR_PREFIX_COLS).max(1)
}

/// The number of visible editor rows for `text` wrapped at `content_cols`,
/// clamped to `[MIN_EDITOR_ROWS, cap]`. Ported from the winit `editor_rows`
/// computation.
#[must_use]
pub fn editor_rows(text: &str, content_cols: usize, cap: Option<usize>) -> usize {
    let needed = wrapped_row_count(text, content_cols);
    let cap = cap.unwrap_or(MAX_PREVIEW_ROWS).max(MIN_EDITOR_ROWS);
    needed.max(MIN_EDITOR_ROWS).min(cap)
}

/// Resolved GPUI colours for the preview.
#[derive(Clone, Copy)]
pub struct WorkspaceNotesPreviewColors {
    bg: Rgba,
    border: Rgba,
    affordance_hover_bg: Rgba,
    text: Rgba,
    hover_text: Rgba,
    muted: Rgba,
    accent: Rgba,
    error_text: Rgba,
}

impl From<&ChromeColors> for WorkspaceNotesPreviewColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            bg: with_alpha(srgba(lighten(chrome.tab_bar_bg, 0.018)), 0.96),
            border: with_alpha(srgba(chrome.tab_separator), 0.92),
            affordance_hover_bg: with_alpha(srgba(chrome.accent), 0.30),
            text: srgba(chrome.tab_text),
            hover_text: srgba(chrome.tab_text_active),
            muted: with_alpha(srgba(chrome.tab_text), 0.64),
            accent: srgba(chrome.accent),
            error_text: with_alpha(srgba(chrome.accent), 0.92),
        }
    }
}

/// The hover-preview view.
///
/// Holds the summaries and (optional) inline-editor buffer the shell feeds it,
/// and paints either the read-only list plus "+" affordance or the inline
/// editor. The pure sizing/wrap helpers ([`preview_cols`], [`editor_rows`],
/// [`wrap_text_for_editor`]) keep the geometry testable.
pub struct WorkspaceNotesPreviewView {
    colors: WorkspaceNotesPreviewColors,
    summaries: Vec<WorkspaceNoteSummary>,
    total_count: usize,
    hovered_note_id: Option<String>,
    inline_editor: Option<AddingNoteState>,
    affordance_hovered: bool,
    max_editor_rows: Option<usize>,
}

impl WorkspaceNotesPreviewView {
    /// Build an empty read-only preview.
    #[must_use]
    pub fn new(colors: WorkspaceNotesPreviewColors) -> Self {
        Self {
            colors,
            summaries: Vec::new(),
            total_count: 0,
            hovered_note_id: None,
            inline_editor: None,
            affordance_hovered: false,
            max_editor_rows: None,
        }
    }

    /// Feed the active-note summaries and total count to the preview.
    pub fn set_summaries(
        &mut self,
        summaries: Vec<WorkspaceNoteSummary>,
        total_count: usize,
        cx: &mut Context<Self>,
    ) {
        self.summaries = summaries;
        self.total_count = total_count;
        cx.notify();
    }

    /// Set (or clear) the inline-editor buffer, switching the preview into or
    /// out of editing mode.
    pub fn set_inline_editor(&mut self, editor: Option<AddingNoteState>, cx: &mut Context<Self>) {
        self.inline_editor = editor;
        cx.notify();
    }

    /// Set the pane-derived cap on editor rows (FR-019).
    pub fn set_max_editor_rows(&mut self, max: Option<usize>, cx: &mut Context<Self>) {
        self.max_editor_rows = max;
        cx.notify();
    }

    /// Set (or clear) which note row is drawn as hovered.
    pub fn set_hovered_note(&mut self, note_id: Option<String>, cx: &mut Context<Self>) {
        self.hovered_note_id = note_id;
        cx.notify();
    }

    /// Set whether the "+" affordance is drawn in its hovered state.
    pub fn set_affordance_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.affordance_hovered = hovered;
        cx.notify();
    }

    /// Whether the preview is currently rendering the inline editor.
    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.inline_editor.is_some()
    }

    fn render_note_row(
        &self,
        index: usize,
        summary: &WorkspaceNoteSummary,
        content_cols: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let hovered = self.hovered_note_id.as_deref() == Some(summary.note_id.as_str());
        let text = format!("- {}", single_line(&summary.text, content_cols.saturating_sub(2)));
        let fg = if hovered { colors.hover_text } else { colors.text };
        let note_id = summary.note_id.clone();
        let mut row = div()
            .id(("wn-preview-note", index))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .text_sm()
            .text_color(fg);
        if hovered {
            // Accent bar signals the hovered row without a background tint.
            row = row.child(div().w(px(2.0)).h(px(14.0)).bg(colors.accent));
        }
        row.child(div().flex_1().child(text))
            .hover(move |s| s.text_color(colors.hover_text))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                ctx.stop_propagation();
                ctx.emit(WorkspaceNotesPreviewAction::ArchiveNote(note_id.clone()));
                this.hovered_note_id = None;
            }))
            .into_any_element()
    }

    fn render_affordance(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let hovered = self.affordance_hovered;
        let (bg, border, fg) = if hovered {
            (colors.affordance_hover_bg, colors.accent, colors.hover_text)
        } else {
            (colors.bg, colors.border, colors.muted)
        };
        div()
            .flex()
            .justify_end()
            .child(
                div()
                    .id("wn-preview-affordance")
                    .px_2()
                    .rounded_sm()
                    .bg(bg)
                    .border_1()
                    .border_color(border)
                    .text_sm()
                    .text_color(fg)
                    .hover(move |s| s.bg(colors.affordance_hover_bg))
                    .child("+")
                    .on_click(cx.listener(|_, _, _win, ctx| {
                        ctx.stop_propagation();
                        ctx.emit(WorkspaceNotesPreviewAction::OpenEditor);
                    })),
            )
            .into_any_element()
    }

    fn render_editor(&self, state: &AddingNoteState, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let cols = preview_cols(&self.summaries, Some(state));
        let content_cols = editor_content_cols(cols);
        let rows = editor_rows(&state.draft_text, content_cols, self.max_editor_rows);
        let wrapped = wrap_text_for_editor(&state.draft_text, content_cols);
        let caret_line = caret_line_index(&state.draft_text, state.caret_byte, content_cols);
        let scroll = state.scroll_offset_rows.min(wrapped.len().saturating_sub(rows));

        let mut lines = Vec::new();
        for visual_idx in 0..rows {
            let line_index = scroll + visual_idx;
            let Some(line) = wrapped.get(line_index) else { continue };
            let mut row = div().flex().items_center().text_sm().text_color(colors.text);
            if line_index == caret_line {
                let caret_col =
                    caret_visible_col(&state.draft_text, state.caret_byte, content_cols);
                let (head, tail) = split_at_char(line, caret_col);
                row = row
                    .child(div().child(head))
                    .child(div().w(px(2.0)).h(px(14.0)).bg(colors.accent))
                    .child(div().child(tail));
            } else {
                row = row.child(div().child(line.clone()));
            }
            lines.push(row.into_any_element());
        }

        let mut editor = div()
            .id("wn-preview-editor")
            .w_full()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_1()
                    .child(div().text_sm().text_color(colors.accent).child("›"))
                    .child(div().flex().flex_col().flex_1().children(lines)),
            )
            .on_click(cx.listener(|_, _, _win, ctx| {
                ctx.stop_propagation();
                ctx.emit(WorkspaceNotesPreviewAction::FocusEditor);
            }));
        if let Some(error) = &state.last_server_error {
            editor = editor
                .child(div().text_xs().text_color(colors.error_text).child(single_line(error, 64)));
        }
        editor.into_any_element()
    }
}

impl Render for WorkspaceNotesPreviewView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let cols = preview_cols(&self.summaries, self.inline_editor.as_ref());
        let content_cols = cols.saturating_sub(PAD_COLS * 2);

        let mut rows = Vec::new();
        if self.summaries.is_empty() {
            rows.push(
                div()
                    .text_sm()
                    .text_color(colors.muted)
                    .child("No active notes")
                    .into_any_element(),
            );
        } else {
            let visible = self.summaries.len().min(MAX_PREVIEW_ROWS);
            let summaries: Vec<_> = self.summaries.iter().take(visible).cloned().collect();
            for (index, summary) in summaries.iter().enumerate() {
                rows.push(self.render_note_row(index, summary, content_cols, cx));
            }
            let overflow = self.total_count.saturating_sub(visible);
            if overflow > 0 {
                rows.push(
                    div()
                        .text_sm()
                        .text_color(colors.muted)
                        .child(format!("+{overflow} more"))
                        .into_any_element(),
                );
            }
        }

        let bottom = if let Some(state) = self.inline_editor.clone() {
            self.render_editor(&state, cx)
        } else {
            self.render_affordance(cx)
        };

        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_1()
            .bg(colors.bg)
            .border_1()
            .border_color(colors.border)
            .rounded_sm()
            .shadow_md()
            .children(rows)
            .child(bottom)
    }
}

/// Flatten whitespace runs to single spaces, cap at `max_chars`, and append an
/// ellipsis when truncated. Ported from the winit preview `single_line`.
#[must_use]
pub fn single_line(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut previous_was_space = false;
    let mut truncated = false;
    for ch in text.chars() {
        let next = if ch.is_whitespace() { ' ' } else { ch };
        if next == ' ' && (previous_was_space || out.is_empty()) {
            previous_was_space = true;
            continue;
        }
        if count >= max_chars {
            truncated = true;
            break;
        }
        out.push(next);
        count += 1;
        previous_was_space = next == ' ';
    }
    if truncated {
        out.push_str("...");
    }
    out
}

fn split_at_char(text: &str, char_col: usize) -> (String, String) {
    let byte = text.char_indices().nth(char_col).map_or(text.len(), |(i, _)| i);
    (text[..byte].to_owned(), text[byte..].to_owned())
}

fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha.clamp(0.0, 1.0), ..color }
}

fn lighten(color: [f32; 4], amount: f32) -> [f32; 4] {
    [
        (color[0] + amount).min(1.0),
        (color[1] + amount).min(1.0),
        (color[2] + amount).min(1.0),
        color[3],
    ]
}

/// Visual row count for `text` wrapped at `cols` columns; explicit `\n` breaks a
/// row and long lines wrap at the column boundary. Ported from the winit
/// `wrapped_row_count`.
#[must_use]
pub fn wrapped_row_count(text: &str, cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    let mut rows = 1usize;
    let mut col = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            rows += 1;
            col = 0;
        } else {
            col += 1;
            if col >= cols {
                rows += 1;
                col = 0;
            }
        }
    }
    rows
}

/// Wrap `text` into visual lines suitable for one-per-row rendering. Returns at
/// least one (possibly empty) line. Ported from the winit `wrap_text_for_editor`.
#[must_use]
pub fn wrap_text_for_editor(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(String::new());
            col = 0;
            continue;
        }
        if col >= cols {
            lines.push(String::new());
            col = 0;
        }
        if let Some(last) = lines.last_mut() {
            last.push(ch);
            col += 1;
        }
    }
    lines
}

/// Longest visible-line length (chars), split on explicit `\n`. Ported from the
/// winit `longest_visible_line_chars`.
#[must_use]
pub fn longest_visible_line_chars(text: &str) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            longest = longest.max(current);
            current = 0;
        } else {
            current += 1;
        }
    }
    longest.max(current)
}

/// Visual line index (0-based) for the caret at `caret_byte`. Ported from the
/// winit `caret_line_index`.
#[must_use]
pub fn caret_line_index(text: &str, caret_byte: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    let mut lines = 0usize;
    let mut col = 0usize;
    let mut bytes = 0usize;
    for ch in text.chars() {
        if bytes >= caret_byte {
            return lines;
        }
        if ch == '\n' {
            lines += 1;
            col = 0;
        } else {
            col += 1;
            if col >= cols {
                lines += 1;
                col = 0;
            }
        }
        bytes += ch.len_utf8();
    }
    lines
}

/// Column (0-based) inside the caret's visual line. Ported from the winit
/// `caret_visible_col`.
#[must_use]
pub fn caret_visible_col(text: &str, caret_byte: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    let mut col = 0usize;
    let mut bytes = 0usize;
    for ch in text.chars() {
        if bytes >= caret_byte {
            return col;
        }
        if ch == '\n' {
            col = 0;
        } else {
            col += 1;
            if col >= cols {
                col = 0;
            }
        }
        bytes += ch.len_utf8();
    }
    col
}

#[cfg(test)]
mod tests;

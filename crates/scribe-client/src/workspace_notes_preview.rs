//! GPU-rendered workspace notes hover preview.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::workspace_notes::WorkspaceNoteSummary;

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

const MAX_PREVIEW_ROWS: usize = 12;
const MIN_PREVIEW_COLS: usize = 22;
const MAX_PREVIEW_COLS: usize = 64;
const PAD_COLS: usize = 1;
const MAX_GRID_UNITS: usize = 65_535;

pub struct WorkspaceNotesPreviewBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub anchor: Rect,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub summaries: &'a [WorkspaceNoteSummary],
    pub total_count: usize,
    pub hovered_note_id: Option<&'a str>,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceNotesPreviewInteraction {
    pub rect: Rect,
    pub note_targets: Vec<WorkspaceNotesPreviewNoteTarget>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceNotesPreviewNoteTarget {
    pub note_id: String,
    pub rect: Rect,
}

pub fn build_workspace_notes_preview(
    ctx: WorkspaceNotesPreviewBuildContext<'_>,
) -> Option<WorkspaceNotesPreviewInteraction> {
    let WorkspaceNotesPreviewBuildContext {
        out,
        anchor,
        viewport,
        cell_size,
        chrome,
        summaries,
        total_count,
        hovered_note_id,
        resolve_glyph,
    } = ctx;
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    let layout = PreviewLayout::new(anchor, viewport, cell_size, summaries, total_count);
    let interaction = layout.interaction(cell_size, summaries);
    let colors = PreviewColors::from_chrome(chrome);
    let mut renderer = PreviewRenderer { out, layout, colors, cell_size, resolve_glyph };
    renderer.draw_background();
    renderer.draw_rows(summaries, total_count, hovered_note_id);
    Some(interaction)
}

#[derive(Clone, Copy)]
struct PreviewLayout {
    rect: Rect,
    cols: usize,
    visible_note_rows: usize,
    overflow: usize,
}

impl PreviewLayout {
    fn new(
        anchor: Rect,
        viewport: Rect,
        cell_size: (f32, f32),
        summaries: &[WorkspaceNoteSummary],
        total_count: usize,
    ) -> Self {
        let longest = summaries
            .iter()
            .map(|summary| summary.text.chars().count().saturating_add(2))
            .max()
            .unwrap_or("No active notes".chars().count());
        let cols = longest.saturating_add(PAD_COLS * 2).clamp(MIN_PREVIEW_COLS, MAX_PREVIEW_COLS);
        let visible_note_rows =
            if summaries.is_empty() { 0 } else { summaries.len().min(MAX_PREVIEW_ROWS) };
        let overflow = total_count.saturating_sub(visible_note_rows);
        let row_count =
            if summaries.is_empty() { 1 } else { visible_note_rows + usize::from(overflow > 0) };
        let width = grid_width(cols, cell_size.0);
        let height = grid_height(row_count.saturating_add(2), cell_size.1);
        let x = clamp_axis(anchor.x, width, viewport.x, viewport.x + viewport.width);
        let below_y = anchor.y + anchor.height;
        let y = if below_y + height <= viewport.y + viewport.height {
            below_y
        } else {
            (anchor.y - height).max(viewport.y)
        };
        Self { rect: Rect { x, y, width, height }, cols, visible_note_rows, overflow }
    }

    fn interaction(
        &self,
        cell_size: (f32, f32),
        summaries: &[WorkspaceNoteSummary],
    ) -> WorkspaceNotesPreviewInteraction {
        let note_targets = summaries
            .iter()
            .take(self.visible_note_rows)
            .enumerate()
            .map(|(row_index, summary)| WorkspaceNotesPreviewNoteTarget {
                note_id: summary.note_id.clone(),
                rect: Rect {
                    x: self.rect.x,
                    y: self.rect.y + grid_height(row_index + 1, cell_size.1),
                    width: self.rect.width,
                    height: cell_size.1,
                },
            })
            .collect();
        WorkspaceNotesPreviewInteraction { rect: self.rect, note_targets }
    }
}

#[derive(Clone, Copy)]
struct PreviewColors {
    bg: [f32; 4],
    border: [f32; 4],
    row_hover_bg: [f32; 4],
    text: [f32; 4],
    hover_text: [f32; 4],
    muted: [f32; 4],
}

impl PreviewColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        Self {
            bg: srgb_to_linear_rgba(with_alpha(lighten(chrome.tab_bar_bg, 0.018), 0.96)),
            border: srgb_to_linear_rgba(with_alpha(chrome.tab_separator, 0.92)),
            row_hover_bg: srgb_to_linear_rgba(with_alpha(chrome.accent, 0.22)),
            text: srgb_to_linear_rgba(chrome.tab_text),
            hover_text: srgb_to_linear_rgba(chrome.tab_text_active),
            muted: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.64)),
        }
    }
}

struct PreviewRenderer<'a, 'b> {
    out: &'a mut Vec<CellInstance>,
    layout: PreviewLayout,
    colors: PreviewColors,
    cell_size: (f32, f32),
    resolve_glyph: &'a mut GlyphResolver<'b>,
}

impl PreviewRenderer<'_, '_> {
    fn draw_background(&mut self) {
        self.push_solid_rect(self.layout.rect, self.colors.bg);
        let rect = self.layout.rect;
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.0 },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y + rect.height - 1.0, width: rect.width, height: 1.0 },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y, width: 1.0, height: rect.height },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: rect.x + rect.width - 1.0, y: rect.y, width: 1.0, height: rect.height },
            self.colors.border,
        );
    }

    fn draw_rows(
        &mut self,
        summaries: &[WorkspaceNoteSummary],
        total_count: usize,
        hovered_note_id: Option<&str>,
    ) {
        if summaries.is_empty() {
            self.emit_text("No active notes", 1, PAD_COLS, self.colors.muted);
            return;
        }

        for (row_index, summary) in summaries.iter().take(self.layout.visible_note_rows).enumerate()
        {
            let row = row_index + 1;
            let hovered = hovered_note_id == Some(summary.note_id.as_str());
            if hovered {
                self.draw_row_hover(row);
            }
            let text =
                format!("- {}", single_line(&summary.text, self.content_cols().saturating_sub(2)));
            let fg = if hovered { self.colors.hover_text } else { self.colors.text };
            self.emit_text(&text, row, PAD_COLS, fg);
        }

        if self.layout.overflow > 0 {
            let row = self.layout.visible_note_rows + 1;
            let text =
                format!("+{} more", total_count.saturating_sub(self.layout.visible_note_rows));
            self.emit_text(&text, row, PAD_COLS, self.colors.muted);
        }
    }

    fn content_cols(&self) -> usize {
        self.layout.cols.saturating_sub(PAD_COLS * 2)
    }

    fn draw_row_hover(&mut self, row: usize) {
        self.push_solid_rect(
            Rect {
                x: self.layout.rect.x + 1.0,
                y: self.layout.rect.y + grid_height(row, self.cell_size.1),
                width: (self.layout.rect.width - 2.0).max(0.0),
                height: self.cell_size.1,
            },
            self.colors.row_hover_bg,
        );
    }

    fn push_solid_rect(&mut self, rect: Rect, color: [f32; 4]) {
        self.out.push(CellInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: color,
            bg_color: color,
            corner_radius: 0.0,
        });
    }

    fn emit_text(&mut self, text: &str, row: usize, start_col: usize, fg: [f32; 4]) {
        let y = self.layout.rect.y + grid_height(row, self.cell_size.1);
        for (idx, ch) in text.chars().enumerate() {
            let col = start_col + idx;
            if col >= self.layout.cols {
                break;
            }
            let x = self.layout.rect.x + grid_width(col, self.cell_size.0);
            let (uv_min, uv_max) = (self.resolve_glyph)(ch);
            self.out.push(CellInstance {
                pos: [x, y],
                size: [0.0, 0.0],
                uv_min,
                uv_max,
                fg_color: fg,
                bg_color: self.colors.bg,
                corner_radius: 0.0,
            });
        }
    }
}

fn single_line(text: &str, max_chars: usize) -> String {
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

fn clamp_axis(value: f32, size: f32, min: f32, max: f32) -> f32 {
    let max_value = (max - size).max(min);
    value.clamp(min, max_value)
}

fn render_grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn grid_width(cols: usize, cell_w: f32) -> f32 {
    f32::from(render_grid_units(cols)) * cell_w
}

fn grid_height(rows: usize, cell_h: f32) -> f32 {
    f32::from(render_grid_units(rows)) * cell_h
}

fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    let [red, green, blue, _] = color;
    [red, green, blue, alpha.clamp(0.0, 1.0)]
}

fn lighten(color: [f32; 4], amount: f32) -> [f32; 4] {
    let [red, green, blue, alpha] = color;
    [(red + amount).min(1.0), (green + amount).min(1.0), (blue + amount).min(1.0), alpha]
}

//! GPU-rendered workspace notes hover preview.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::workspace_notes::{AddingNoteState, WorkspaceNoteSummary};

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

const MAX_PREVIEW_ROWS: usize = 12;
const MIN_PREVIEW_COLS: usize = 22;
const MAX_PREVIEW_COLS: usize = 64;
const PAD_COLS: usize = 1;
const MAX_GRID_UNITS: usize = 65_535;
/// Width (in terminal cells) of the bordered "+" affordance at the bottom-right
/// of the read-only preview. Per spec FR-001 and UX-002 the affordance is ~2 cols
/// wide; we use 3 so the "+" glyph can sit visually centered with one cell of
/// border padding on each side.
const AFFORDANCE_COLS: usize = 3;
/// Right-edge inset (in cells) between the affordance's right border and the
/// preview's right inner border (UX-002).
const AFFORDANCE_RIGHT_INSET: usize = 1;
/// Minimum vertical rows the inline editor reserves (1 row of text plus an
/// optional error row is added on demand).
const MIN_EDITOR_ROWS: usize = 1;
/// Maximum cells of error text rendered next to the editor row.
const MAX_EDITOR_ERROR_CHARS: usize = 64;

pub struct WorkspaceNotesPreviewBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub anchor: Rect,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub summaries: &'a [WorkspaceNoteSummary],
    pub total_count: usize,
    pub hovered_note_id: Option<&'a str>,
    /// The current workspace's inline editor state if it is in "adding note"
    /// state (FR-002 / FR-021). When `Some`, the preview renders the editor row
    /// in place of the "+" affordance. Taken as `&mut` so the build pass can
    /// snap `scroll_offset_rows` to keep the caret visible using the layout's
    /// real content-width (FR-022) — the actual `cols` is computed inside
    /// [`PreviewLayout::new`] and isn't available to callers.
    pub inline_editor: Option<&'a mut AddingNoteState>,
    /// True when the pointer is currently over the "+" affordance, used to draw
    /// its hover visual state. Ignored when `inline_editor` is `Some`.
    pub affordance_hovered: bool,
    /// Maximum rows the preview can grow to. Clamped to 3/4 of the focused pane
    /// per FR-019 by the caller; passing `None` falls back to `MAX_PREVIEW_ROWS`.
    pub max_editor_rows: Option<usize>,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceNotesPreviewInteraction {
    pub rect: Rect,
    pub note_targets: Vec<WorkspaceNotesPreviewNoteTarget>,
    /// Hit-rect for the bottom-right "+" affordance (FR-001) when the preview is
    /// in its read-only state. `None` when the preview is currently rendering the
    /// inline editor (FR-002 hides the affordance during editing).
    pub affordance_rect: Option<Rect>,
    /// Hit-rect for the inline editor row when in "adding note" state. `None`
    /// when read-only. Used to absorb clicks (FR-011) so they don't archive a
    /// note row.
    pub editor_rect: Option<Rect>,
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
        mut inline_editor,
        affordance_hovered,
        max_editor_rows,
        resolve_glyph,
    } = ctx;
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    let layout = PreviewLayout::new(&PreviewLayoutInputs {
        anchor,
        viewport,
        cell_size,
        summaries,
        total_count,
        inline_editor: inline_editor.as_deref(),
        max_editor_rows,
    });
    // FR-022 first input: snap scroll-to-caret using the layout's actual content
    // width and editor row budget — not an external estimate — so the caret is
    // always brought into view regardless of how the layout clamped `cols`.
    let content_cols = layout.cols.saturating_sub(PAD_COLS * 2).max(1);
    if let Some(state) = inline_editor.as_deref_mut() {
        state.clamp_scroll_to_caret(content_cols, layout.editor_rows.max(1));
    }
    let interaction = layout.interaction(cell_size, summaries);
    let colors = PreviewColors::from_chrome(chrome);
    let mut renderer = PreviewRenderer { out, layout, colors, cell_size, resolve_glyph };
    renderer.draw_background();
    renderer.draw_rows(summaries, total_count, hovered_note_id);
    if let Some(state) = inline_editor.as_deref() {
        renderer.draw_editor_row(state);
    } else {
        renderer.draw_affordance(affordance_hovered);
    }
    Some(interaction)
}

#[derive(Clone, Copy)]
struct PreviewLayout {
    rect: Rect,
    cols: usize,
    visible_note_rows: usize,
    overflow: usize,
    /// Row index (0-based, within the preview's grid) of the affordance row, or
    /// the inline editor's first row when in editing mode.
    bottom_zone_row: usize,
    /// Total rows the editor occupies (≥ 1). Equals 0 in read-only mode.
    editor_rows: usize,
    /// Whether the editor surfaces a server error row directly below the input.
    has_editor_error: bool,
}

#[derive(Clone, Copy)]
struct EditorTextArgs<'a> {
    state: &'a AddingNoteState,
    wrapped: &'a [String],
    scroll: usize,
    editor_rows: usize,
    start_row: usize,
    caret_line_idx: usize,
    content_cols: usize,
}

#[derive(Clone, Copy)]
struct EditorScrollbarArgs {
    editor_x: f32,
    editor_y: f32,
    editor_w: f32,
    editor_h: f32,
    total_lines: usize,
    editor_rows: usize,
    scroll: usize,
}

#[derive(Clone, Copy)]
struct PreviewLayoutInputs<'a> {
    anchor: Rect,
    viewport: Rect,
    cell_size: (f32, f32),
    summaries: &'a [WorkspaceNoteSummary],
    total_count: usize,
    inline_editor: Option<&'a AddingNoteState>,
    max_editor_rows: Option<usize>,
}

impl PreviewLayout {
    fn new(inputs: &PreviewLayoutInputs<'_>) -> Self {
        let &PreviewLayoutInputs {
            anchor,
            viewport,
            cell_size,
            summaries,
            total_count,
            inline_editor,
            max_editor_rows,
        } = inputs;
        let longest = summaries
            .iter()
            .map(|summary| summary.text.chars().count().saturating_add(2))
            .max()
            .unwrap_or("No active notes".chars().count());
        let editor_longest_line =
            inline_editor.map_or(0, |state| longest_visible_line_chars(&state.draft_text));
        let longest = longest.max(editor_longest_line.saturating_add(2));
        let cols = longest.saturating_add(PAD_COLS * 2).clamp(MIN_PREVIEW_COLS, MAX_PREVIEW_COLS);
        let visible_note_rows =
            if summaries.is_empty() { 0 } else { summaries.len().min(MAX_PREVIEW_ROWS) };
        let overflow = total_count.saturating_sub(visible_note_rows);
        let note_zone_rows =
            if summaries.is_empty() { 1 } else { visible_note_rows + usize::from(overflow > 0) };

        // Read-only layout reserves: 1 top pad + note_zone + 1 spacer + 1 affordance + 1 bottom pad
        // Editing layout reserves:    1 top pad + note_zone + 1 spacer + editor_rows (+1 if error) + 1 bottom pad
        let editor_rows = inline_editor.map_or(0, |state| {
            let content_width = cols.saturating_sub(PAD_COLS * 2).max(1);
            let needed = wrapped_row_count(&state.draft_text, content_width);
            let cap = max_editor_rows.unwrap_or(MAX_PREVIEW_ROWS).max(MIN_EDITOR_ROWS);
            needed.max(MIN_EDITOR_ROWS).min(cap)
        });
        let has_editor_error =
            inline_editor.and_then(|state| state.last_server_error.as_ref()).is_some();
        let bottom_zone_extra = if inline_editor.is_some() {
            editor_rows + usize::from(has_editor_error)
        } else {
            1 // affordance row
        };
        let total_rows = 1 /* top pad */
            + note_zone_rows
            + 1 /* spacer between notes and bottom zone */
            + bottom_zone_extra
            + 1 /* bottom pad */;

        let bottom_zone_row = 1 + note_zone_rows + 1;

        let width = grid_width(cols, cell_size.0);
        let height = grid_height(total_rows, cell_size.1);
        let x = clamp_axis(anchor.x, width, viewport.x, viewport.x + viewport.width);
        let below_y = anchor.y + anchor.height;
        let y = if below_y + height <= viewport.y + viewport.height {
            below_y
        } else {
            (anchor.y - height).max(viewport.y)
        };
        Self {
            rect: Rect { x, y, width, height },
            cols,
            visible_note_rows,
            overflow,
            bottom_zone_row,
            editor_rows,
            has_editor_error,
        }
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
        let (affordance_rect, editor_rect) = if self.editor_rows == 0 {
            // Read-only mode → affordance rect populated.
            let aff_x = self.rect.x
                + grid_width(
                    self.cols.saturating_sub(AFFORDANCE_RIGHT_INSET + AFFORDANCE_COLS),
                    cell_size.0,
                );
            let aff_y = self.rect.y + grid_height(self.bottom_zone_row, cell_size.1);
            (
                Some(Rect {
                    x: aff_x,
                    y: aff_y,
                    width: grid_width(AFFORDANCE_COLS, cell_size.0),
                    height: cell_size.1,
                }),
                None,
            )
        } else {
            let ed_y = self.rect.y + grid_height(self.bottom_zone_row, cell_size.1);
            (
                None,
                Some(Rect {
                    x: self.rect.x + 1.0,
                    y: ed_y,
                    width: (self.rect.width - 2.0).max(0.0),
                    height: grid_height(self.editor_rows, cell_size.1),
                }),
            )
        };
        WorkspaceNotesPreviewInteraction {
            rect: self.rect,
            note_targets,
            affordance_rect,
            editor_rect,
        }
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
    accent: [f32; 4],
    editor_bg: [f32; 4],
    error_text: [f32; 4],
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
            accent: srgb_to_linear_rgba(chrome.accent),
            editor_bg: srgb_to_linear_rgba(with_alpha(lighten(chrome.tab_bar_bg, 0.040), 0.98)),
            error_text: srgb_to_linear_rgba(with_alpha(chrome.accent, 0.92)),
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

    fn draw_affordance(&mut self, hovered: bool) {
        let row = self.layout.bottom_zone_row;
        let cell_w = self.cell_size.0;
        let cell_h = self.cell_size.1;
        let left_col = self.layout.cols.saturating_sub(AFFORDANCE_RIGHT_INSET + AFFORDANCE_COLS);
        let x = self.layout.rect.x + grid_width(left_col, cell_w);
        let y = self.layout.rect.y + grid_height(row, cell_h);
        let width = grid_width(AFFORDANCE_COLS, cell_w);
        let height = cell_h;
        let (bg_color, border_color, glyph_fg) = if hovered {
            (self.colors.row_hover_bg, self.colors.accent, self.colors.hover_text)
        } else {
            (self.colors.bg, self.colors.border, self.colors.muted)
        };
        // Inner background (slightly elevated vs the preview bg so the button
        // reads as a distinct surface).
        self.push_solid_rect(Rect { x, y, width, height }, bg_color);
        // Border edges (1px).
        self.push_solid_rect(Rect { x, y, width, height: 1.0 }, border_color);
        self.push_solid_rect(Rect { x, y: y + height - 1.0, width, height: 1.0 }, border_color);
        self.push_solid_rect(Rect { x, y, width: 1.0, height }, border_color);
        self.push_solid_rect(Rect { x: x + width - 1.0, y, width: 1.0, height }, border_color);
        // Center "+" glyph in the middle column of the affordance.
        let glyph_col = left_col + (AFFORDANCE_COLS / 2);
        self.emit_text("+", row, glyph_col, glyph_fg);
    }

    fn draw_editor_row(&mut self, state: &AddingNoteState) {
        let cell_h = self.cell_size.1;
        let start_row = self.layout.bottom_zone_row;
        let editor_rows = self.layout.editor_rows.max(1);
        let editor_x = self.layout.rect.x + 1.0;
        let editor_y = self.layout.rect.y + grid_height(start_row, cell_h);
        let editor_w = (self.layout.rect.width - 2.0).max(0.0);
        let editor_h = grid_height(editor_rows, cell_h);

        // Editor background — slightly elevated to read as editable.
        self.push_solid_rect(
            Rect { x: editor_x, y: editor_y, width: editor_w, height: editor_h },
            self.colors.editor_bg,
        );
        // Editor border (top + bottom edges).
        self.push_solid_rect(
            Rect { x: editor_x, y: editor_y, width: editor_w, height: 1.0 },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: editor_x, y: editor_y + editor_h - 1.0, width: editor_w, height: 1.0 },
            self.colors.border,
        );

        // Render wrapped lines.
        let content_cols = self.content_cols().max(1);
        let wrapped = wrap_text_for_editor(&state.draft_text, content_cols);
        let total_lines = wrapped.len();
        let scroll = state.scroll_offset_rows.min(total_lines.saturating_sub(editor_rows));
        let caret_line_idx = caret_line_index(&state.draft_text, state.caret_byte, content_cols);
        self.draw_editor_text(EditorTextArgs {
            state,
            wrapped: &wrapped,
            scroll,
            editor_rows,
            start_row,
            caret_line_idx,
            content_cols,
        });

        // FR-022 third input: overlay scrollbar inside the editor when content
        // exceeds the visible row budget. Static thin indicator for now; the
        // full `ScrollbarState` fade-animation reuse is a follow-up polish.
        if total_lines > editor_rows {
            self.draw_editor_scrollbar(EditorScrollbarArgs {
                editor_x,
                editor_y,
                editor_w,
                editor_h,
                total_lines,
                editor_rows,
                scroll,
            });
        }

        // Optional error row below the editor.
        let error =
            self.layout.has_editor_error.then_some(state.last_server_error.as_deref()).flatten();
        if let Some(err) = error {
            let row = start_row + editor_rows;
            let truncated: String = err.chars().take(MAX_EDITOR_ERROR_CHARS).collect();
            self.emit_text(&truncated, row, PAD_COLS, self.colors.error_text);
        }
    }

    fn draw_editor_text(&mut self, args: EditorTextArgs<'_>) {
        let EditorTextArgs {
            state,
            wrapped,
            scroll,
            editor_rows,
            start_row,
            caret_line_idx,
            content_cols,
        } = args;
        let cell_w = self.cell_size.0;
        let cell_h = self.cell_size.1;
        for visual_idx in 0..editor_rows {
            let line_index = scroll + visual_idx;
            let Some(line) = wrapped.get(line_index) else { continue };
            let row = start_row + visual_idx;
            self.emit_text(line, row, PAD_COLS, self.colors.text);
            if line_index != caret_line_idx {
                continue;
            }
            let caret_col = caret_visible_col(&state.draft_text, state.caret_byte, content_cols);
            let caret_x = self.layout.rect.x + grid_width(PAD_COLS + caret_col, cell_w);
            let caret_y = self.layout.rect.y + grid_height(row, cell_h);
            self.push_solid_rect(
                Rect { x: caret_x, y: caret_y, width: 1.5, height: cell_h },
                self.colors.accent,
            );
        }
    }

    fn draw_editor_scrollbar(&mut self, args: EditorScrollbarArgs) {
        let EditorScrollbarArgs {
            editor_x,
            editor_y,
            editor_w,
            editor_h,
            total_lines,
            editor_rows,
            scroll,
        } = args;
        let cell_h = self.cell_size.1;
        let track_x = editor_x + editor_w - 3.0;
        let track_w = 2.0;
        let track_y = editor_y + 1.0;
        let track_h = (editor_h - 2.0).max(0.0);
        let thumb_ratio = ratio_f32(editor_rows, total_lines);
        let thumb_h = (track_h * thumb_ratio).max(cell_h.min(track_h));
        let max_offset = total_lines.saturating_sub(editor_rows).max(1);
        let scroll_ratio = ratio_f32(scroll, max_offset).clamp(0.0, 1.0);
        let thumb_y = track_y + (track_h - thumb_h) * scroll_ratio;
        self.push_solid_rect(
            Rect { x: track_x, y: thumb_y, width: track_w, height: thumb_h },
            self.colors.accent,
        );
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

/// Lossless ratio of two non-negative cell counts. Both inputs are clamped to
/// `u16::MAX` (matching the renderer's grid-unit limit) before conversion to
/// `f32`, so the resulting division is exact. Returns 0.0 when the denominator
/// is zero.
fn ratio_f32(num: usize, denom: usize) -> f32 {
    let n = u16::try_from(num.min(usize::from(u16::MAX))).unwrap_or(u16::MAX);
    let d = u16::try_from(denom.min(usize::from(u16::MAX))).unwrap_or(u16::MAX);
    if d == 0 { 0.0 } else { f32::from(n) / f32::from(d) }
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

/// Compute the visual row count needed to render `text` wrapped at `cols`
/// content-columns. Explicit `\n` characters break to a new visual row; long
/// lines wrap at the column boundary.
fn wrapped_row_count(text: &str, cols: usize) -> usize {
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

/// Wrap `text` into visual lines suitable for emitting one per row in the
/// editor area. Returns at least one (possibly empty) line.
fn wrap_text_for_editor(text: &str, cols: usize) -> Vec<String> {
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

/// Longest visible-line length (in characters) inside `text`, where lines are
/// split on explicit `\n`. Used to size the preview width when the inline
/// editor has wider content than the read-only notes.
fn longest_visible_line_chars(text: &str) -> usize {
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

/// Visual line index (0-based among `wrap_text_for_editor` output) for the
/// caret, given the caret's byte offset into the unwrapped text.
fn caret_line_index(text: &str, caret_byte: usize, cols: usize) -> usize {
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

/// Column inside the caret's visual line (0-based).
fn caret_visible_col(text: &str, caret_byte: usize, cols: usize) -> usize {
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

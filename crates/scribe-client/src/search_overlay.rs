//! Find-in-scrollback overlay state.
//!
//! Tracks whether the search overlay is visible, the current query text,
//! match results received from the server, and which match is highlighted.

use scribe_common::protocol::SearchMatch;
use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

const SEARCH_OVERLAY_MARGIN: f32 = 14.0;
const SEARCH_OVERLAY_MIN_COLS: usize = 24;
const SEARCH_OVERLAY_MAX_COLS: usize = 56;

/// UI state for the find-in-scrollback overlay.
pub struct SearchOverlay {
    active: bool,
    query: String,
    matches: Vec<SearchMatch>,
    current_match: usize,
}

impl SearchOverlay {
    /// Creates a new inactive overlay with empty state.
    pub fn new() -> Self {
        Self { active: false, query: String::new(), matches: Vec::new(), current_match: 0 }
    }

    /// Opens the overlay, clearing any previous query and results.
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.matches.clear();
        self.current_match = 0;
    }

    /// Closes the overlay and resets all state.
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.matches.clear();
        self.current_match = 0;
    }

    /// Appends a character to the search query.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
    }

    /// Removes the last character from the search query, if any.
    pub fn pop_char(&mut self) {
        self.query.pop();
    }

    /// Clears the search query text.
    pub fn clear_query(&mut self) {
        self.query.clear();
    }

    /// Returns the current search query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns whether the overlay is currently visible.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Replaces the match results and resets the highlight to the first match.
    pub fn set_results(&mut self, matches: Vec<SearchMatch>) {
        self.matches = matches;
        self.current_match = 0;
    }

    /// Clears any previous match results.
    pub fn clear_results(&mut self) {
        self.matches.clear();
        self.current_match = 0;
    }

    /// Advances to the next match, wrapping around to the first.
    /// No-op when there are no matches.
    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current_match = (self.current_match + 1) % self.matches.len();
    }

    /// Goes back to the previous match, wrapping around to the last.
    /// No-op when there are no matches.
    pub fn prev_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        self.current_match = (self.current_match + count - 1) % count;
    }

    /// Returns the currently highlighted match, if any.
    pub fn current_match(&self) -> Option<&SearchMatch> {
        self.matches.get(self.current_match)
    }

    /// Returns the index of the currently highlighted match.
    pub fn current_match_index(&self) -> usize {
        self.current_match
    }

    /// Returns all matches.
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// Returns the total number of matches.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Build GPU instances for the active search overlay.
    #[allow(
        clippy::too_many_arguments,
        reason = "overlay builder needs output vec, viewport, cell size, chrome colors, and glyph resolver"
    )]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "overlay dimensions are derived from viewport and cell sizes and fit within usize/f32 bounds"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "overlay builder emits background, border, and two text rows"
    )]
    pub fn build_instances(
        &self,
        out: &mut Vec<CellInstance>,
        viewport: Rect,
        cell_size: (f32, f32),
        chrome: &ChromeColors,
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        if !self.active {
            return;
        }

        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let available_cols =
            ((viewport.width - 2.0 * SEARCH_OVERLAY_MARGIN) / cell_w).floor().max(0.0) as usize;
        if available_cols < SEARCH_OVERLAY_MIN_COLS {
            return;
        }

        let colors = SearchOverlayColors::from_chrome(chrome);
        let header = if self.query.is_empty() {
            String::from("Find")
        } else if self.matches.is_empty() {
            String::from("Find  no matches")
        } else {
            format!("Find  {}/{}", self.current_match_index() + 1, self.match_count())
        };
        let query_text = if self.query.is_empty() {
            String::from("Type to search scrollback")
        } else {
            self.query.clone()
        };

        let desired_cols =
            header.chars().count().max(query_text.chars().count() + 2).saturating_add(2);
        let overlay_cols = desired_cols
            .clamp(SEARCH_OVERLAY_MIN_COLS, available_cols.min(SEARCH_OVERLAY_MAX_COLS));
        let overlay_width = overlay_cols as f32 * cell_w;
        let overlay_height = 2.0 * cell_h;
        let overlay_x =
            (viewport.x + viewport.width - overlay_width - SEARCH_OVERLAY_MARGIN).max(viewport.x);
        let overlay_y = viewport.y + SEARCH_OVERLAY_MARGIN;
        let overlay =
            Rect { x: overlay_x, y: overlay_y, width: overlay_width, height: overlay_height };

        push_solid_rect(out, overlay, colors.bg);
        push_solid_rect(
            out,
            Rect { x: overlay.x, y: overlay.y, width: overlay.width, height: 1.0 },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect {
                x: overlay.x,
                y: overlay.y + overlay.height - 1.0,
                width: overlay.width,
                height: 1.0,
            },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect { x: overlay.x, y: overlay.y, width: 1.0, height: overlay.height },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect {
                x: overlay.x + overlay.width - 1.0,
                y: overlay.y,
                width: 1.0,
                height: overlay.height,
            },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect {
                x: overlay.x + 1.0,
                y: overlay.y + cell_h,
                width: (overlay.width - 2.0).max(0.0),
                height: (cell_h - 1.0).max(0.0),
            },
            colors.input_bg,
        );

        let header_cols = overlay_cols.saturating_sub(2);
        emit_text_line(
            out,
            &tail_chars(&header, header_cols),
            overlay.x + cell_w,
            overlay.y,
            colors.header_fg,
            colors.bg,
            cell_w,
            resolve_glyph,
        );

        let query_cols = overlay_cols.saturating_sub(3);
        let visible_query = tail_chars(&query_text, query_cols);
        emit_text_line(
            out,
            "/",
            overlay.x + cell_w,
            overlay.y + cell_h,
            colors.border,
            colors.input_bg,
            cell_w,
            resolve_glyph,
        );
        emit_text_line(
            out,
            &visible_query,
            overlay.x + 2.0 * cell_w,
            overlay.y + cell_h,
            if self.query.is_empty() { colors.placeholder_fg } else { colors.query_fg },
            colors.input_bg,
            cell_w,
            resolve_glyph,
        );
    }
}

struct SearchOverlayColors {
    bg: [f32; 4],
    input_bg: [f32; 4],
    border: [f32; 4],
    header_fg: [f32; 4],
    query_fg: [f32; 4],
    placeholder_fg: [f32; 4],
}

impl SearchOverlayColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        let mut bg = srgb_to_linear_rgba(chrome.tab_bar_active_bg);
        bg[3] = 0.96;

        let mut input_bg = srgb_to_linear_rgba(chrome.status_bar_bg);
        input_bg[3] = 0.98;

        let border = srgb_to_linear_rgba(chrome.accent);
        let header_fg = srgb_to_linear_rgba(chrome.tab_text_active);
        let query_fg = srgb_to_linear_rgba(chrome.status_bar_text);

        let mut placeholder_fg = query_fg;
        placeholder_fg[3] *= 0.7;

        Self { bg, input_bg, border, header_fg, query_fg, placeholder_fg }
    }
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_owned();
    }

    let start = chars.len().saturating_sub(max_chars);
    chars.get(start..).unwrap_or(&[]).iter().collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "text emission needs output vec, text, position, colors, cell width, and glyph resolver"
)]
fn emit_text_line(
    out: &mut Vec<CellInstance>,
    text: &str,
    x: f32,
    y: f32,
    fg_color: [f32; 4],
    bg_color: [f32; 4],
    cell_w: f32,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let mut cursor_x = x;
    for ch in text.chars() {
        let (uv_min, uv_max) = resolve_glyph(ch);
        out.push(CellInstance {
            pos: [cursor_x, y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color,
            bg_color,
            corner_radius: 0.0,
            _pad: 0.0,
        });
        cursor_x += cell_w;
    }
}

fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: 0.0,
        _pad: 0.0,
    });
}

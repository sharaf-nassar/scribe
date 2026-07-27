//! GPUI paint path for a display-only terminal [`Content`](crate::terminal::Content) snapshot.
//!
//! Every visible property of a terminal cell is resolved here, on the live
//! paint call, in the order the legacy wgpu renderer used: cell backgrounds
//! first, then the procedural box-drawing overlay, then shaped glyph runs.
//! Colours come from the ported `TerminalColors` SGR semantics, glyph coverage
//! from an explicit Nerd-Font-first fallback chain, and ligatures from the
//! `calt` OpenType feature gated on `appearance.ligatures`.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, ElementInputHandler, Entity, FocusHandle, Font, FontFallbacks, FontFeatures,
    FontStyle, FontWeight, Pixels, Rgba, StrikethroughStyle, TextAlign, TextRun, UnderlineStyle,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use scribe_client_gpui::{
    box_drawing,
    color::{TerminalColors, linear_to_srgb_rgba},
    layout::Rect,
    opacity::{opaque_slot, scale_alpha},
    preedit::{Ime, PreeditGeometry, PreeditOverlay, PreeditState, compute_overlay},
    restore_replay::round_positive_f32_to_u16,
    scrollbar::{
        CommandMark, ScrollMetrics, ScrollbarHandle, ScrollbarLayout, ScrollbarQuad,
        ScrollbarStyle, build_scrollbar_render,
    },
    search::{MatchHighlight, MatchHighlightColors},
    selection::SelectionSpan,
    split_scroll,
};
use scribe_common::config::AppearanceConfig;

use crate::terminal::{Cell, Content, CursorPlacement, Flags, ViewportPoint};

/// Smallest font size the grid will paint at, so a bad `appearance.font_size`
/// edit (0, negative) can never collapse the grid to nothing.
const MIN_FONT_SIZE: f32 = 6.0;

/// Row height as a multiple of the font size, before `appearance.line_padding`
/// is added. Matches the ~1.35 leading the legacy atlas used for its cell box.
const LINE_HEIGHT_RATIO: f32 = 1.35;

/// Cell advance as a multiple of the font size for the monospace grid. The
/// legacy renderer measured this from the shaped glyph; the display-only spike
/// approximates it so a live font-size edit still moves the reported cell size.
const CELL_WIDTH_RATIO: f32 = 0.6;

/// The glyph fallback chain, in the order the legacy cosmic-text atlas used
/// (`SCRIBE_COMMON_FALLBACKS` in `crates/scribe-renderer/src/atlas.rs`).
///
/// Nerd Font symbol families come first so a powerline or devicon codepoint
/// resolves to the icon the user installed rather than to whatever generic
/// symbol font the platform happens to rank higher. `Unifont Sample` is
/// deliberately absent: its private-use mappings turn an unavailable icon into
/// an unrelated sample glyph, which is worse than a visible tofu box.
const FONT_FALLBACKS: &[&str] = &[
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "Nerd Font Symbols Mono",
    "Nerd Font Symbols",
    "Noto Sans",
    "DejaVu Sans",
    "FreeSans",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "FreeMono",
    "Noto Sans Symbols",
    "Noto Sans Symbols2",
    "Noto Color Emoji",
];

/// Thickness of the underline and strikethrough rules, as a fraction of the
/// font size, floored at one pixel so they never vanish at small sizes.
const DECORATION_RATIO: f32 = 1.0 / 14.0;

/// The font metrics the terminal grid paints with.
///
/// Derived from the live `[appearance]` config on every config reload, so a
/// saved `font` / `font_size` / `line_padding` / `ligatures` / `font_weight`
/// edit repaints the grid without a restart instead of staying frozen at the
/// value read during startup.
#[derive(Debug, Clone, PartialEq)]
pub struct GridFont {
    /// Font family passed to GPUI's text system.
    pub family: String,
    /// Glyph size in pixels, clamped to at least [`MIN_FONT_SIZE`].
    pub size: f32,
    /// Row height in pixels, including `appearance.line_padding`.
    pub line_height: f32,
    /// Whether contextual alternates (`calt`) stay enabled, from
    /// `appearance.ligatures`.
    pub ligatures: bool,
    /// Weight for normal cells, from `appearance.font_weight`.
    pub weight: u16,
    /// Weight for BOLD cells, from `appearance.font_weight_bold`.
    pub weight_bold: u16,
}

impl GridFont {
    /// Derive the paint metrics from the live appearance config.
    #[must_use]
    pub fn from_appearance(appearance: &AppearanceConfig) -> Self {
        let size = appearance.font_size.max(MIN_FONT_SIZE);
        Self {
            family: appearance.font.clone(),
            size,
            line_height: size.mul_add(LINE_HEIGHT_RATIO, f32::from(appearance.line_padding)),
            ligatures: appearance.ligatures,
            weight: appearance.font_weight,
            weight_bold: appearance.font_weight_bold,
        }
    }

    /// The per-cell advance width reported to the server in `TerminalSize`.
    #[must_use]
    pub fn cell_width(&self) -> f32 {
        self.size * CELL_WIDTH_RATIO
    }

    /// The OpenType features every terminal run is shaped with.
    ///
    /// `appearance.ligatures = false` disables `calt`, the feature that
    /// produces the contextual multi-cell forms (`=>`, `!=`) in programming
    /// fonts; nothing else about shaping changes.
    #[must_use]
    pub fn features(&self) -> FontFeatures {
        if self.ligatures { FontFeatures::default() } else { FontFeatures::disable_ligatures() }
    }

    /// The ordered [`FONT_FALLBACKS`] chain, carried on every run so GPUI's
    /// platform text system cannot substitute its own ordering.
    #[must_use]
    pub fn fallbacks() -> FontFallbacks {
        FontFallbacks::from_fonts(FONT_FALLBACKS.iter().map(|name| (*name).to_owned()).collect())
    }

    /// The [`Font`] a cell with these attributes is shaped with.
    #[must_use]
    pub fn font_for(&self, flags: Flags) -> Font {
        let weight = if flags.contains(Flags::BOLD) { self.weight_bold } else { self.weight };
        Font {
            family: self.family.clone().into(),
            features: self.features(),
            fallbacks: Some(Self::fallbacks()),
            weight: FontWeight(f32::from(weight)),
            style: if flags.contains(Flags::ITALIC) {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
        }
    }

    /// Underline / strikethrough rule thickness for this font size.
    fn decoration_thickness(&self) -> Pixels {
        px((self.size * DECORATION_RATIO).max(1.0))
    }
}

impl Default for GridFont {
    fn default() -> Self {
        Self::from_appearance(&AppearanceConfig::default())
    }
}

/// The four style variants a row's runs are shaped with, built once per frame.
///
/// Cloning a [`Font`] clones four `Arc`s, so materialising the variants up
/// front keeps the per-cell run construction to reference bumps.
struct FontVariants {
    regular: Font,
    bold: Font,
    italic: Font,
    bold_italic: Font,
}

impl FontVariants {
    fn new(font: &GridFont) -> Self {
        Self {
            regular: font.font_for(Flags::empty()),
            bold: font.font_for(Flags::BOLD),
            italic: font.font_for(Flags::ITALIC),
            bold_italic: font.font_for(Flags::BOLD | Flags::ITALIC),
        }
    }

    fn select(&self, flags: Flags) -> &Font {
        match (flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC)) {
            (true, true) => &self.bold_italic,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (false, false) => &self.regular,
        }
    }
}

/// The theme colours the terminal grid paints with.
///
/// `background` is the window-level fill, already carrying the
/// `appearance.opacity` alpha. `cells` is the ported SGR resolver that turns
/// each cell's raw colour fields — including the theme's default foreground —
/// into the colour actually painted, and `opacity` is folded into per-cell
/// backgrounds only, exactly as the legacy renderer's
/// `apply_opacity_to_instances` did; glyph colours are never scaled, so text
/// stays readable through a translucent window.
#[derive(Clone)]
pub struct GridColors {
    /// Grid background, alpha-scaled by `appearance.opacity`.
    pub background: Rgba,
    /// Theme-derived palette resolving each cell's raw fg/bg/attrs.
    pub cells: Arc<TerminalColors>,
    /// Live `appearance.opacity`, applied to painted cell backgrounds.
    pub opacity: f32,
}

/// Where the last painted frame put the terminal grid, in window coordinates.
///
/// The shell needs this to answer a mouse event: a right-click resolves to a
/// grid cell (smart selection) and a left-click may land on the split-scroll
/// jump chip, and neither can be derived from the event alone because the grid
/// sits under a titlebar of theme-dependent height. The canvas records its own
/// bounds on every frame instead, and both live on the GPUI thread so a plain
/// `Cell` is the whole synchronisation story.
pub type GridBounds = Rc<std::cell::Cell<Option<Bounds<Pixels>>>>;

/// Record a freshly measured rect into a [`GridBounds`] cell and report whether
/// it moved.
///
/// A measuring canvas learns its rect during *prepaint*, which runs after the
/// `render` that produced it — so the render pass reacting to a window resize
/// still reads the pre-resize rect, and nothing schedules another one. The
/// callers that publish geometry off the measured area therefore need the write
/// itself to tell them the area changed, so they can ask for the follow-up
/// frame that observes it.
pub fn record_grid_area(area: &GridBounds, bounds: Bounds<Pixels>) -> bool {
    let moved = area.get() != Some(bounds);
    area.set(Some(bounds));
    moved
}

/// Everything the focused pane's paint pass needs to serve the OS input method.
///
/// GPUI only accepts an input handler *during paint* and only for the frame it
/// is registered on, so the focused pane re-registers the window's [`Ime`]
/// entity on every frame from inside its grid canvas. Handing it the cursor
/// cell's rect — rather than the whole grid's — is what lets
/// [`Ime::bounds_for_range`](scribe_client_gpui::preedit::Ime) put the OS
/// candidate list under the composition point.
#[derive(Clone)]
pub struct ImePaint {
    /// The window's keyboard focus handle; GPUI drops the registration unless
    /// this handle is the focused one.
    pub focus_handle: FocusHandle,
    /// The composition state machine the platform delivers marked and
    /// committed text to.
    pub ime: Entity<Ime>,
    /// The focused pane's live cursor and viewport placement, or `None` before
    /// any session is attached (there is nowhere to compose into yet).
    pub placement: Option<CursorPlacement>,
    /// The in-flight composition to draw, cloned out of [`Self::ime`] by the
    /// view so paint needs no entity read.
    pub preedit: Option<PreeditState>,
}

/// Everything one pane's overlay scrollbar needs from the frame it is drawn in.
///
/// The pane's pixel rect is deliberately absent: only the grid canvas knows it,
/// and the whole point of the overlay is that it hugs the right edge of the
/// cells rather than reserving a gutter the layout would have to subtract. The
/// view therefore supplies the *data* (viewport metrics, the command marks the
/// OSC 133 stream produced, the theme palette) plus the cross-frame fade state,
/// and paint supplies the geometry.
#[derive(Clone)]
pub struct ScrollbarPaint {
    /// This pane's fade / hover / drag state, owned by the view so the
    /// animation survives the element being rebuilt every frame.
    pub state: ScrollbarHandle,
    /// Live viewport measurements the thumb is sized and placed from.
    pub metrics: ScrollMetrics,
    /// Command boundaries to tick, in absolute scrollback rows. Already
    /// trim-shifted by the prompt-mark store, so a tick names a surviving row.
    pub marks: Vec<CommandMark>,
    /// Theme-derived thumb and tick colours.
    pub style: ScrollbarStyle,
}

/// Paints the current terminal grid with fixed-width rows.
pub struct TerminalElement {
    content: Content,
    font: GridFont,
    colors: GridColors,
    /// Find-overlay match spans for this frame, already projected onto the
    /// visible viewport. Empty whenever the overlay is closed or the server
    /// reported no on-screen match.
    highlights: Vec<MatchHighlight>,
    /// Accent colours a highlighted span is painted with.
    highlight_colors: MatchHighlightColors,
    /// Mouse-selection runs for this frame, already projected onto the visible
    /// viewport. Empty whenever the pane holds no selection.
    selection: Vec<SelectionSpan>,
    /// IME plumbing for this frame. `None` on every unfocused pane: only one
    /// pane composes, and registering two handlers would race for the platform
    /// slot.
    ime: Option<ImePaint>,
    /// Overlay-scrollbar inputs for this frame. `None` before a session is
    /// attached to the pane, which is the only case with no viewport to
    /// describe.
    scrollbar: Option<ScrollbarPaint>,
    bounds_sink: GridBounds,
}

impl TerminalElement {
    /// Captures one stable terminal snapshot for this render pass, painted with
    /// the font metrics and theme colours resolved from the live config.
    ///
    /// `bounds_sink` receives the grid's window-space rect on every frame.
    pub fn new(
        content: Content,
        font: GridFont,
        colors: GridColors,
        highlight_colors: MatchHighlightColors,
        bounds_sink: GridBounds,
    ) -> Self {
        Self {
            content,
            font,
            colors,
            highlights: Vec::new(),
            highlight_colors,
            selection: Vec::new(),
            ime: None,
            scrollbar: None,
            bounds_sink,
        }
    }

    /// Draw this pane's overlay scrollbar on top of the finished grid.
    ///
    /// Every pane showing a session passes one: the thumb is per-pane state
    /// (each pane scrolls its own scrollback), unlike the IME registration
    /// which is a window-wide singleton.
    #[must_use]
    pub fn with_scrollbar(mut self, scrollbar: ScrollbarPaint) -> Self {
        self.scrollbar = Some(scrollbar);
        self
    }

    /// Serve the OS input method from this pane's paint pass.
    ///
    /// Only the focused pane calls this: the platform holds one input handler
    /// per window, and the composition belongs to the pane the keystrokes are
    /// going to.
    #[must_use]
    pub fn with_ime(mut self, ime: ImePaint) -> Self {
        self.ime = Some(ime);
        self
    }

    /// Highlight `highlights` as find matches on top of the resolved cells.
    ///
    /// The spans are folded into the per-cell colours the paint path already
    /// resolves, rather than drawn as separate quads over the finished grid:
    /// the current match has to invert its foreground for contrast, which only
    /// the cell-accurate resolve step can do.
    #[must_use]
    pub fn with_highlights(mut self, highlights: Vec<MatchHighlight>) -> Self {
        self.highlights = highlights;
        self
    }

    /// Paint `selection` as the active mouse selection under the find matches.
    ///
    /// Selection is folded into the same per-cell resolve the find highlights
    /// use, and applied first, so a find match inside a selection still reads
    /// as a match — the search accent wins the overlap, exactly as it does in
    /// the winit client.
    #[must_use]
    pub fn with_selection(mut self, selection: Vec<SelectionSpan>) -> Self {
        self.selection = selection;
        self
    }

    /// Builds the GPUI element tree for the visible terminal grid.
    ///
    /// The grid itself is one low-level canvas rather than a div per row: a
    /// cell-accurate terminal needs background quads, the box-drawing overlay,
    /// and shaped glyph runs painted in that order into the same pass, which a
    /// styled-div tree cannot express.
    pub fn paint(self) -> impl IntoElement {
        let background = self.colors.background;
        let bounds_sink = Rc::clone(&self.bounds_sink);
        div().size_full().overflow_hidden().bg(background).child(
            canvas(
                move |bounds, _window, _cx| bounds_sink.set(Some(bounds)),
                move |bounds, (), window, cx| self.paint_grid(bounds, window, cx),
            )
            .size_full(),
        )
    }

    /// Paint one terminal snapshot into `bounds`.
    ///
    /// Order matters and mirrors the legacy renderer: cell backgrounds, then
    /// the procedural box-drawing overlay, then shaped text. Box-drawing cells
    /// are blanked out of the shaped text so the overlay is the only thing that
    /// draws them — that is what removes the font's sub-pixel bearing gaps.
    fn paint_grid(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let cell_width = self.font.cell_width();
        let line_height = px(self.font.line_height);
        if cell_width <= 0.0 || line_height <= px(0.0) {
            return;
        }
        let default_bg = linear_to_srgb_rgba(self.colors.cells.default_bg());
        let variants = FontVariants::new(&self.font);
        let thickness = self.font.decoration_thickness();
        let mut resolved: Vec<ResolvedCell> = Vec::new();

        for (row_index, row) in self.content.rows.iter().enumerate() {
            let top = bounds.top() + line_height * grid_f32(row_index);
            if top >= bounds.bottom() {
                break;
            }
            let geometry =
                CellGeometry { left: bounds.left(), top, width: cell_width, height: line_height };

            resolved.clear();
            resolved.extend(row.iter().map(|cell| {
                let (fg, bg) =
                    self.colors.cells.resolve_cell_colors_srgb(cell.fg, cell.bg, cell.flags);
                let painted_bg = (!slots_equal(bg, default_bg))
                    .then(|| scale_alpha(opaque_slot(bg), self.colors.opacity));
                ResolvedCell { fg: opaque_slot(fg), bg: painted_bg, flags: cell.flags }
            }));
            self.apply_selection(row_index, &mut resolved);
            self.apply_highlights(row_index, default_bg, &mut resolved);

            paint_cell_backgrounds(&resolved, geometry, window);
            paint_box_drawing(row, &resolved, geometry, window);
            paint_row_text(
                &RowPaint {
                    cells: row,
                    resolved: &resolved,
                    font: &self.font,
                    variants: &variants,
                    thickness,
                    geometry,
                },
                window,
                cx,
            );
        }

        let overlay = OverlayGeometry {
            bounds,
            cell_width,
            line_height,
            accent: opaque_slot(linear_to_srgb_rgba(
                self.colors
                    .cells
                    .resolve_color(vte::ansi::Color::Named(vte::ansi::NamedColor::Cursor)),
            )),
        };
        self.paint_vi_cursor(overlay, window);
        self.paint_split_scroll(overlay, window);
        // Last of the grid overlays: the scrollbar floats over the cells (it
        // reserves no gutter), so anything painted after it would sit on top of
        // the thumb. The IME registration below draws nothing.
        self.paint_scrollbar(bounds, window);
        self.serve_ime(overlay, window, cx);
    }

    /// Draw the pane's overlay scrollbar: the thumb plus one tick per command
    /// boundary the OSC 133 stream reported.
    ///
    /// `bounds` is the grid canvas, which is exactly the pane's content area —
    /// the GPUI client puts its tab strip in the window titlebar rather than at
    /// the top of each pane, so the scrollbar track needs no tab-bar inset and
    /// spans the full painted height.
    ///
    /// Nothing is drawn while the fade has settled to invisible or the pane has
    /// no scrollback: an unscrolled pane must look exactly as it did before the
    /// overlay existed.
    fn paint_scrollbar(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let Some(scrollbar) = self.scrollbar.as_ref() else {
            return;
        };
        let Ok(mut state) = scrollbar.state.try_borrow_mut() else {
            return;
        };
        let layout = ScrollbarLayout {
            pane_rect: Rect {
                x: f32::from(bounds.left()),
                y: f32::from(bounds.top()),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
            },
            metrics: scrollbar.metrics,
            tab_bar_height: 0.0,
        };
        let Some(render) =
            build_scrollbar_render(&layout, &scrollbar.marks, &mut state, &scrollbar.style)
        else {
            return;
        };
        paint_scrollbar_quad(&render.thumb, window);
        for tick in &render.ticks {
            paint_scrollbar_quad(tick, window);
        }
    }

    /// Register the window's IME handler for the next frame and draw the
    /// in-flight composition on top of the grid.
    ///
    /// Registration happens unconditionally (there is no composition to wait
    /// for — the handler is how one starts at all); the overlay only draws once
    /// the platform has marked text and the anchor is still on screen.
    fn serve_ime(&self, overlay: OverlayGeometry, window: &mut Window, cx: &mut App) {
        let Some(ime) = self.ime.as_ref() else {
            return;
        };
        let cursor_cell = ime
            .placement
            .and_then(|placement| cursor_cell_bounds(overlay, placement))
            .unwrap_or_else(|| {
                // No pane attached yet: hang the candidate window off the
                // grid's first row rather than dropping the registration, so
                // the input method still has a live target.
                Bounds::new(overlay.bounds.origin, size(px(0.0), overlay.line_height))
            });
        window.handle_input(
            &ime.focus_handle,
            ElementInputHandler::new(cursor_cell, ime.ime.clone()),
            cx,
        );
        let (Some(state), Some(placement)) = (ime.preedit.as_ref(), ime.placement) else {
            return;
        };
        let geometry = PreeditGeometry {
            grid_origin_px: [f32::from(overlay.bounds.left()), f32::from(overlay.bounds.top())],
            cell_px: [overlay.cell_width, f32::from(overlay.line_height)],
            columns: placement.columns,
            screen_lines: placement.screen_lines,
            display_offset: placement.display_offset,
            viewport_top_abs_row: placement.viewport_top_abs_row,
        };
        let Some(preedit) = compute_overlay(state, geometry) else {
            return;
        };
        // `accent` is the theme's foreground: the cursor slot resolves to it,
        // and it is what the legacy renderer drew the composition and its rule
        // with.
        self.paint_preedit(&preedit, overlay.accent, window, cx);
    }

    /// Draw one preedit composition over the grid, in the legacy renderer's
    /// three layers: an opaque backdrop that hides the cells underneath, the
    /// composition glyphs, and the underline that marks the text as unconfirmed.
    ///
    /// The grid itself is never mutated — the composition is purely an
    /// occlusion, so cancelling it leaves the pane exactly as it was.
    fn paint_preedit(
        &self,
        overlay: &PreeditOverlay,
        foreground: Rgba,
        window: &mut Window,
        cx: &mut App,
    ) {
        let PreeditOverlay { origin_px, cell_px, text, max_cells } = overlay;
        let [cell_width, cell_height] = *cell_px;
        if cell_width <= 0.0 || cell_height <= 0.0 {
            return;
        }
        let (text, cells) = clip_preedit(text, *max_cells);
        if cells == 0 {
            return;
        }
        let origin = point(px(origin_px[0]), px(origin_px[1]));
        let span = size(px(cell_width * f32::from(cells)), px(cell_height));
        // Opaque on purpose even under a translucent window: the backdrop's job
        // is to hide the cells the composition sits on, and a scaled alpha
        // would let them read through the glyphs.
        let background = opaque_slot(linear_to_srgb_rgba(self.colors.cells.default_bg()));

        window.paint_quad(fill(Bounds::new(origin, span), background));
        let run = TextRun {
            len: text.len(),
            font: self.font.font_for(Flags::empty()),
            color: foreground.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // No `force_width`: the composition is not on the cell grid yet, so its
        // glyphs keep their natural advances and the underline below is sized
        // from the same `unicode_width` budget `clip_preedit` spent.
        window
            .text_system()
            .shape_line(text.into(), px(self.font.size), &[run], None)
            .paint(origin, px(cell_height), TextAlign::Left, None, window, cx)
            .ok();
        let rule = self.font.decoration_thickness();
        window.paint_quad(fill(
            Bounds::new(point(origin.x, origin.y + px(cell_height) - rule), size(span.width, rule)),
            foreground,
        ));
    }

    /// Recolour the cells of `row_index` that a find match covers.
    ///
    /// The current match takes the opaque accent with a contrast foreground so
    /// it reads as the cursor of the match list; every other match blends the
    /// accent into whatever background that cell already had, which keeps a
    /// match legible on top of coloured shell output. A highlighted cell always
    /// paints a background, even where the cell was keeping the window's own
    /// fill, or the match would be invisible on an untouched row.
    fn apply_highlights(
        &self,
        row_index: usize,
        default_bg: [f32; 4],
        resolved: &mut [ResolvedCell],
    ) {
        if self.highlights.is_empty() {
            return;
        }
        let window_bg = scale_alpha(opaque_slot(default_bg), self.colors.opacity);
        for span in self.highlights.iter().filter(|span| span.row == row_index) {
            let Some(cells) = resolved.get_mut(span.start_col..=span.end_col) else {
                continue;
            };
            for cell in cells {
                self.highlight_cell(cell, span.current, window_bg);
            }
        }
    }

    /// Recolour the cells of `row_index` that the mouse selection covers.
    ///
    /// Both channels come from the theme's `selection` / `selection_foreground`
    /// keys rather than from a blend, so selected text keeps a single, uniform
    /// look across coloured shell output — which is what makes a selection
    /// readable as one contiguous region.
    fn apply_selection(&self, row_index: usize, resolved: &mut [ResolvedCell]) {
        if self.selection.is_empty() {
            return;
        }
        let cells_theme = &self.colors.cells;
        let bg = opaque_slot(linear_to_srgb_rgba(cells_theme.selection_bg()));
        let fg = opaque_slot(linear_to_srgb_rgba(cells_theme.selection_fg()));
        for span in self.selection.iter().filter(|span| span.row == row_index) {
            let Some(cells) = resolved.get_mut(span.start_col..=span.end_col) else {
                continue;
            };
            for cell in cells {
                cell.bg = Some(scale_alpha(bg, self.colors.opacity));
                cell.fg = fg;
            }
        }
    }

    /// Recolour one cell covered by a find match.
    fn highlight_cell(&self, cell: &mut ResolvedCell, current: bool, window_bg: Rgba) {
        if current {
            cell.bg = Some(self.highlight_colors.current_bg);
            cell.fg = self.highlight_colors.current_fg;
            return;
        }
        cell.bg = Some(self.highlight_colors.blend_passive(cell.bg.unwrap_or(window_bg)));
    }

    /// Outline the vi / copy-mode cursor so the keyboard cursor is visible
    /// while it moves independently of the shell cursor.
    ///
    /// It is drawn as a hollow box rather than a filled block: the cell under
    /// it still has to be readable, and a filled block would hide exactly the
    /// character the user navigated to.
    fn paint_vi_cursor(&self, overlay: OverlayGeometry, window: &mut Window) {
        let Some(cursor) = self.content.vi_cursor else {
            return;
        };
        let OverlayGeometry { bounds, cell_width, line_height, accent } = overlay;
        let left = bounds.left() + px(cell_width * grid_f32(cursor.col));
        let top = bounds.top() + line_height * grid_f32(cursor.row);
        if top >= bounds.bottom() {
            return;
        }
        let thickness = px(VI_CURSOR_THICKNESS);
        let width = px(cell_width);
        for edge in [
            Bounds::new(point(left, top), size(width, thickness)),
            Bounds::new(point(left, top + line_height - thickness), size(width, thickness)),
            Bounds::new(point(left, top), size(thickness, line_height)),
            Bounds::new(point(left + width - thickness, top), size(thickness, line_height)),
        ] {
            window.paint_quad(fill(edge, accent));
        }
    }

    /// Draw the split-scroll divider and the jump-to-bottom chip.
    ///
    /// The rows themselves already carry the split — the snapshot's trailing
    /// [`Content::pin_rows`] come from the live screen — so all that is left to
    /// paint is the seam between the two regions and the affordance that
    /// collapses it.
    fn paint_split_scroll(&self, overlay: OverlayGeometry, window: &mut Window) {
        if self.content.pin_rows == 0 {
            return;
        }
        let OverlayGeometry { bounds, cell_width, line_height, accent } = overlay;
        let geometry = split_scroll::compute_geometry(
            content_rect(bounds, self.content.rows.len(), line_height),
            f32::from(line_height) * grid_f32(self.content.pin_rows),
        );
        window.paint_quad(fill(to_bounds(geometry.divider), accent));

        let chip = to_bounds(geometry.jump_button);
        window.paint_quad(fill(chip, Rgba { a: accent.a * CHIP_BACKDROP_ALPHA, ..accent }));
        // A down chevron built from two strokes: the chip has to read as "jump
        // to the bottom" without pulling a glyph run into the canvas pass.
        let arm = px(cell_width.max(MIN_CHEVRON_ARM));
        let mid_x = chip.left() + chip.size.width / 2.;
        let mid_y = chip.top() + chip.size.height / 2.;
        let thickness = px(VI_CURSOR_THICKNESS);
        for stroke in [
            Bounds::new(point(mid_x - arm, mid_y - thickness), size(arm, thickness * 2.)),
            Bounds::new(point(mid_x, mid_y - thickness), size(arm, thickness * 2.)),
        ] {
            window.paint_quad(fill(stroke, accent));
        }
    }
}

/// The per-frame geometry and palette the two grid overlays share.
#[derive(Clone, Copy)]
struct OverlayGeometry {
    bounds: Bounds<Pixels>,
    cell_width: f32,
    line_height: Pixels,
    accent: Rgba,
}

/// Line thickness for the vi-cursor box and the jump chip's chevron.
const VI_CURSOR_THICKNESS: f32 = 1.5;

/// How much of the accent colour the jump chip's backdrop keeps, so the chip
/// reads as a control rather than as a solid block of foreground.
const CHIP_BACKDROP_ALPHA: f32 = 0.35;

/// Floor for the chevron's arm length, so the chip stays legible at small
/// font sizes where the cell advance shrinks below it.
const MIN_CHEVRON_ARM: f32 = 5.0;

/// The window-space rect of the shell cursor's cell, or `None` when the cursor
/// is not on a painted row.
///
/// This is the OS candidate window's anchor, so it follows the *live* cursor
/// rather than the composition anchor: the popup should appear where the next
/// composition will start even before one exists.
fn cursor_cell_bounds(
    overlay: OverlayGeometry,
    placement: CursorPlacement,
) -> Option<Bounds<Pixels>> {
    if placement.display_offset > 0 {
        return None;
    }
    let visible_line = placement.abs_row.checked_sub(placement.viewport_top_abs_row)?;
    if visible_line >= placement.screen_lines {
        return None;
    }
    let left = overlay.bounds.left() + px(overlay.cell_width * grid_f32(placement.col));
    let top = overlay.bounds.top() + overlay.line_height * grid_f32(visible_line);
    if top >= overlay.bounds.bottom() {
        return None;
    }
    Some(Bounds::new(point(left, top), size(px(overlay.cell_width), overlay.line_height)))
}

/// Trim a composition to the cells left on its row, returning the drawable
/// prefix and the number of cells it occupies.
///
/// Widths come from the same `unicode_width` budget
/// [`preedit_cell_width`](scribe_client_gpui::preedit::preedit_cell_width)
/// spends — wide CJK glyphs claim two cells, zero-width marks ride the previous
/// base glyph, and a leading mark with no base is dropped — so the underline
/// and the backdrop are exactly as wide as the glyphs the shaper lays down.
fn clip_preedit(text: &str, max_cells: u16) -> (String, u16) {
    let mut clipped = String::with_capacity(text.len());
    let mut cells: u16 = 0;
    for ch in text.chars() {
        let Ok(width) = u16::try_from(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
        else {
            continue;
        };
        if width == 0 {
            // A combining mark rides the glyph before it; with no base glyph
            // yet there is nothing to attach to, so it is dropped.
            if cells > 0 {
                clipped.push(ch);
            }
            continue;
        }
        if cells.saturating_add(width) > max_cells {
            break;
        }
        clipped.push(ch);
        cells += width;
    }
    (clipped, cells)
}

/// The rect the painted rows occupy, in the same f32 window space
/// [`split_scroll`] computes its geometry in.
fn content_rect(bounds: Bounds<Pixels>, rows: usize, line_height: Pixels) -> Rect {
    Rect {
        x: f32::from(bounds.left()),
        y: f32::from(bounds.top()),
        width: f32::from(bounds.size.width),
        height: f32::from(line_height) * grid_f32(rows),
    }
}

/// Lower a [`Rect`] back onto GPUI's typed pixel bounds.
fn to_bounds(rect: Rect) -> Bounds<Pixels> {
    Bounds::new(point(px(rect.x), px(rect.y)), size(px(rect.width), px(rect.height)))
}

/// Lower one rounded scrollbar quad onto the window.
///
/// The colour already carries the fade opacity folded into its alpha, and the
/// window's `appearance.opacity` is deliberately *not* applied on top: the
/// scrollbar is foreground chrome drawn over an already-translucent grid, so
/// scaling it a second time would fade the overlay out of a translucent window.
fn paint_scrollbar_quad(quad: &ScrollbarQuad, window: &mut Window) {
    window.paint_quad(
        fill(to_bounds(quad.rect), opaque_slot(quad.color)).corner_radii(px(quad.corner_radius)),
    );
}

/// The grid cell a window-space pointer position falls on.
///
/// Returns `None` when the pointer is outside the grid or the metrics are
/// degenerate, so a click on the titlebar or the status bar can never be
/// mistaken for a click on row 0.
#[must_use]
pub fn cell_at(
    bounds: Bounds<Pixels>,
    font: &GridFont,
    position: gpui::Point<Pixels>,
) -> Option<ViewportPoint> {
    let cell_width = font.cell_width();
    if cell_width <= 0.0 || font.line_height <= 0.0 || !bounds.contains(&position) {
        return None;
    }
    Some(ViewportPoint {
        row: cell_index(f32::from(position.y - bounds.top()), font.line_height),
        col: cell_index(f32::from(position.x - bounds.left()), cell_width),
    })
}

/// Which cell an axis offset falls in, resolved by binary search so no
/// float-to-int cast is needed (the workspace denies the lossy-cast lints).
fn cell_index(offset: f32, cell_size: f32) -> usize {
    if cell_size <= 0.0 || !offset.is_finite() || offset <= 0.0 {
        return 0;
    }
    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if f32::from(mid) * cell_size <= offset {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    usize::from(low)
}

/// Whether a window-space position lands on the split-scroll jump chip.
///
/// The chip is painted inside the grid canvas, so the shell cannot hit-test it
/// with a GPUI child element; it re-derives the same geometry the paint pass
/// used and asks [`split_scroll::hit_test_jump_btn`].
#[must_use]
pub fn hits_jump_chip(
    bounds: Bounds<Pixels>,
    font: &GridFont,
    rows: usize,
    pin_rows: usize,
    position: gpui::Point<Pixels>,
) -> bool {
    if pin_rows == 0 || font.line_height <= 0.0 {
        return false;
    }
    let line_height = px(font.line_height);
    let geometry = split_scroll::compute_geometry(
        content_rect(bounds, rows, line_height),
        font.line_height * grid_f32(pin_rows),
    );
    split_scroll::hit_test_jump_btn(&geometry, f32::from(position.x), f32::from(position.y))
}

/// Whether two theme slots are the same colour, compared by bit pattern.
///
/// Both sides come out of the same resolver, so equal colours are bit-for-bit
/// equal; an epsilon would risk collapsing two genuinely distinct palette
/// entries into "keeps the window background".
fn slots_equal(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(left, right)| left.to_bits() == right.to_bits())
}

/// The per-cell colours and style the paint path resolved for one cell.
#[derive(Clone, Copy)]
struct ResolvedCell {
    fg: Rgba,
    /// `None` when the cell keeps the window background, so the grid's own
    /// (possibly translucent) fill shows through instead of being painted over.
    bg: Option<Rgba>,
    flags: Flags,
}

/// Geometry shared by every cell of one painted frame.
#[derive(Clone, Copy)]
struct CellGeometry {
    left: Pixels,
    top: Pixels,
    width: f32,
    height: Pixels,
}

impl CellGeometry {
    /// Left edge of `column`, in window space.
    fn column_left(self, column: usize) -> Pixels {
        self.left + px(self.width * grid_f32(column))
    }
}

/// Fill the background of every cell that does not keep the window background,
/// merging horizontally adjacent cells of the same colour into one quad.
fn paint_cell_backgrounds(resolved: &[ResolvedCell], geometry: CellGeometry, window: &mut Window) {
    let mut column = 0;
    while column < resolved.len() {
        let Some(bg) = resolved.get(column).and_then(|cell| cell.bg) else {
            column += 1;
            continue;
        };
        let start = column;
        column += 1;
        while resolved.get(column).and_then(|cell| cell.bg).is_some_and(|next| next == bg) {
            column += 1;
        }
        window.paint_quad(fill(
            Bounds::new(
                point(geometry.column_left(start), geometry.top),
                size(px(geometry.width * grid_f32(column - start)), geometry.height),
            ),
            bg,
        ));
    }
}

/// Overlay the procedural box-drawing mask for every cell that carries one.
///
/// The mask is rasterized in integer pixels and its quads are then scaled onto
/// the cell's exact fractional rect, so a full-cell stroke lands precisely on
/// the neighbouring cell's edge and a run of box characters tiles with no seam
/// regardless of font size.
fn paint_box_drawing(
    row: &[Cell],
    resolved: &[ResolvedCell],
    geometry: CellGeometry,
    window: &mut Window,
) {
    let mask_w = mask_extent(geometry.width);
    let mask_h = mask_extent(f32::from(geometry.height));
    let scale_x = geometry.width / mask_f32(mask_w);
    let scale_y = f32::from(geometry.height) / mask_f32(mask_h);

    for (column, cell) in row.iter().enumerate() {
        if !box_drawing::is_box_drawing(cell.c) {
            continue;
        }
        let (Some(quads), Some(fg)) = (
            box_drawing::mask_quads(cell.c, mask_w, mask_h),
            resolved.get(column).map(|resolved| resolved.fg),
        ) else {
            continue;
        };
        let origin_x = geometry.column_left(column);
        for quad in quads {
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        origin_x + px(mask_f32(quad.x) * scale_x),
                        geometry.top + px(mask_f32(quad.y) * scale_y),
                    ),
                    size(px(mask_f32(quad.width) * scale_x), px(mask_f32(quad.height) * scale_y)),
                ),
                Rgba { a: fg.a * (f32::from(quad.alpha) / f32::from(u8::MAX)), ..fg },
            ));
        }
    }
}

/// Everything one row's glyph pass needs, gathered so the shared per-frame
/// state (fonts, decoration thickness) is built once and borrowed per row.
struct RowPaint<'a> {
    cells: &'a [Cell],
    resolved: &'a [ResolvedCell],
    font: &'a GridFont,
    variants: &'a FontVariants,
    thickness: Pixels,
    geometry: CellGeometry,
}

/// Shape and paint one row's glyphs.
///
/// The whole row is shaped as a single line so contextual ligatures can form
/// across cells, with `force_width` pinning every advancing glyph to the next
/// grid column — that combination keeps a multi-cell ligature's outline intact
/// without letting the row drift off the grid.
fn paint_row_text(row: &RowPaint<'_>, window: &mut Window, cx: &mut App) {
    let Some(last) = row.cells.iter().rposition(is_painted_cell) else {
        return;
    };

    let mut text = String::with_capacity(last + 1);
    let mut runs: Vec<TextRun> = Vec::new();
    let thickness = row.thickness;

    for (column, cell) in row.cells.iter().take(last + 1).enumerate() {
        let Some(style) = row.resolved.get(column).copied() else {
            continue;
        };
        let start = text.len();
        text.push(shaped_char(cell.c));
        let len = text.len() - start;
        let color = style.fg.into();
        let run = TextRun {
            len,
            font: row.variants.select(style.flags).clone(),
            color,
            background_color: None,
            underline: style.flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
                thickness,
                color: Some(color),
                wavy: style.flags.contains(Flags::UNDERCURL),
            }),
            strikethrough: style
                .flags
                .contains(Flags::STRIKEOUT)
                .then_some(StrikethroughStyle { thickness, color: Some(color) }),
        };
        match runs.last_mut() {
            Some(previous) if run_matches(previous, &run) => previous.len += len,
            _ => runs.push(run),
        }
    }

    let geometry = row.geometry;
    window
        .text_system()
        .shape_line(text.into(), px(row.font.size), &runs, Some(px(geometry.width)))
        .paint(
            point(geometry.left, geometry.top),
            geometry.height,
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
}

/// Whether two adjacent runs are stylistically identical and can be merged.
///
/// Merging matters for more than run count: shaping only forms a ligature
/// within a single run, so `!=` written in one SGR state must stay one run.
fn run_matches(a: &TextRun, b: &TextRun) -> bool {
    a.font == b.font
        && a.color == b.color
        && a.underline == b.underline
        && a.strikethrough == b.strikethrough
}

/// Whether a cell contributes anything to the shaped line, used to trim a
/// row's blank tail before shaping it. A blank cell still counts when it
/// carries an underline or strikethrough, which TUIs use to draw rules.
fn is_painted_cell(cell: &Cell) -> bool {
    !matches!(cell.c, ' ' | '\0') || cell.flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
}

/// The character handed to the shaper for one cell.
///
/// Box-drawing codepoints become spaces because the overlay already painted
/// them, and the space keeps every following cell on its own grid column.
/// Stray control characters are blanked so a malformed stream can never make
/// `shape_line` see a newline.
fn shaped_char(c: char) -> char {
    if box_drawing::is_box_drawing(c) || c.is_control() { ' ' } else { c }
}

/// The integer mask resolution used to rasterize one cell of box drawing.
///
/// Floored at two pixels so a degenerate cell size still produces a mask the
/// rasterizer can draw into. The rounding goes through the shared
/// [`round_positive_f32_to_u16`] conversion rather than a float-to-int cast.
fn mask_extent(pixels: f32) -> u32 {
    u32::from(round_positive_f32_to_u16(pixels).max(2))
}

/// Widen a grid index for pixel arithmetic without a lossy cast.
fn grid_f32(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// Widen a mask coordinate for pixel arithmetic without a lossy cast.
fn mask_f32(value: u32) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        CELL_WIDTH_RATIO, FONT_FALLBACKS, FontVariants, GridBounds, GridColors, GridFont,
        MIN_FONT_SIZE, ResolvedCell, TerminalElement, cell_at, hits_jump_chip, is_painted_cell,
        record_grid_area, shaped_char,
    };
    use gpui::{Bounds, FontStyle, FontWeight, Rgba, point, px, size};
    use scribe_client_gpui::color::TerminalColors;
    use scribe_client_gpui::search::{MatchHighlight, MatchHighlightColors};
    use scribe_common::config::AppearanceConfig;
    use scribe_common::theme::minimal_dark;

    use crate::terminal::{Cell, Content, Flags, ViewportPoint};

    /// A 400x300 grid at (10, 20), the shape the shell hands the paint path.
    fn grid_bounds() -> Bounds<gpui::Pixels> {
        Bounds::new(point(px(10.), px(20.)), size(px(400.), px(300.)))
    }

    // @lat: [[test#GPUI Terminal Viewport#A moved grid area asks for a republish]]
    #[test]
    fn a_moved_grid_area_asks_for_a_republish() {
        let area = GridBounds::default();
        // The very first measurement is a move: nothing has been published yet.
        assert!(record_grid_area(&area, grid_bounds()));
        assert_eq!(area.get(), Some(grid_bounds()));
        // An idle repaint measures the same rect and must not schedule work —
        // every frame would otherwise defer a republish forever.
        assert!(!record_grid_area(&area, grid_bounds()));
        // A resize (or a chrome band appearing) is the case the render path
        // cannot see for itself, because it reads the area one frame stale.
        let resized = Bounds::new(point(px(10.), px(20.)), size(px(720.), px(300.)));
        assert!(record_grid_area(&area, resized));
        assert_eq!(area.get(), Some(resized));
        // A pure move with an unchanged size still counts: the recorded rect is
        // also what a pointer position is lowered through.
        let moved = Bounds::new(point(px(0.), px(0.)), size(px(720.), px(300.)));
        assert!(record_grid_area(&area, moved));
    }

    // @lat: [[test#GPUI Terminal Viewport#Pointer positions lower onto grid cells]]
    #[test]
    fn pointer_positions_lower_onto_grid_cells() {
        let font = GridFont::from_appearance(&AppearanceConfig {
            font_size: 10.0,
            line_padding: 0,
            ..AppearanceConfig::default()
        });
        let bounds = grid_bounds();
        // 10pt: 6px advance, 13.5px rows.
        assert_eq!(
            cell_at(bounds, &font, point(px(10.), px(20.))),
            Some(ViewportPoint { row: 0, col: 0 })
        );
        assert_eq!(
            cell_at(bounds, &font, point(px(10. + 18.5), px(20. + 27.5))),
            Some(ViewportPoint { row: 2, col: 3 })
        );
        // Outside the grid the answer is "no cell", not row 0 — a click on the
        // titlebar must never resolve to the first terminal row.
        assert!(cell_at(bounds, &font, point(px(9.), px(20.))).is_none());
        assert!(cell_at(bounds, &font, point(px(10.), px(321.))).is_none());
    }

    // @lat: [[test#GPUI Terminal Viewport#The jump chip is only hit while the pin is up]]
    #[test]
    fn jump_chip_is_only_hit_while_the_pin_is_up() {
        let font = GridFont::from_appearance(&AppearanceConfig {
            font_size: 10.0,
            line_padding: 0,
            ..AppearanceConfig::default()
        });
        let bounds = grid_bounds();
        // 20 rows of 13.5px with a 5-row pin: the divider sits at y = 20 + 202.5
        // - 1, and the chip is docked just above it at the right edge.
        let chip_x = 10. + 400. - 6. - 14.;
        let chip_y = 20. + (13.5 * 15.) - 1. - 4. - 12.;
        assert!(hits_jump_chip(bounds, &font, 20, 5, point(px(chip_x), px(chip_y))));
        // The same point is inert without a pin, so an unsplit grid passes the
        // click through to the terminal.
        assert!(!hits_jump_chip(bounds, &font, 20, 0, point(px(chip_x), px(chip_y))));
        // A point in the middle of the scrollback portion misses the chip.
        assert!(!hits_jump_chip(bounds, &font, 20, 5, point(px(200.), px(100.))));
    }

    // @lat: [[test#GPUI Client Headless Suites#Config live reload#Grid font tracks the live appearance config]]
    #[test]
    fn grid_font_tracks_appearance_edits() {
        let mut appearance = AppearanceConfig {
            font: "Fira Code".to_owned(),
            font_size: 18.0,
            line_padding: 4,
            ..AppearanceConfig::default()
        };
        let font = GridFont::from_appearance(&appearance);
        assert_eq!(font.family, "Fira Code");
        assert!((font.size - 18.0).abs() < f32::EPSILON);
        assert!((font.line_height - 18.0f32.mul_add(1.35, 4.0)).abs() < f32::EPSILON);
        assert!((font.cell_width() - 18.0 * CELL_WIDTH_RATIO).abs() < f32::EPSILON);

        // A nonsense size is clamped rather than collapsing the grid.
        appearance.font_size = 0.0;
        let clamped = GridFont::from_appearance(&appearance);
        assert!((clamped.size - MIN_FONT_SIZE).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Ligature shaping follows appearance.ligatures]]
    #[test]
    fn ligature_setting_drives_the_calt_feature() {
        let mut appearance = AppearanceConfig { ligatures: true, ..AppearanceConfig::default() };
        let on = GridFont::from_appearance(&appearance);
        assert!(on.ligatures);
        assert_eq!(on.features().is_calt_enabled(), None, "calt stays at the font default");

        appearance.ligatures = false;
        let off = GridFont::from_appearance(&appearance);
        assert!(!off.ligatures);
        assert_eq!(off.features().is_calt_enabled(), Some(false), "calt is explicitly disabled");

        // The feature must travel on the run the paint path shapes with, not
        // just on the metrics struct.
        assert_eq!(off.font_for(Flags::empty()).features.is_calt_enabled(), Some(false));
        assert_eq!(
            FontVariants::new(&off).select(Flags::BOLD).features.is_calt_enabled(),
            Some(false)
        );
    }

    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Every run carries the Nerd Font fallback chain]]
    #[test]
    fn runs_carry_the_nerd_font_fallback_chain() {
        let appearance = AppearanceConfig {
            font: "JetBrains Mono".to_owned(),
            font_weight: 400,
            font_weight_bold: 700,
            ..AppearanceConfig::default()
        };
        let font = GridFont::from_appearance(&appearance);
        let variants = FontVariants::new(&font);

        for flags in [Flags::empty(), Flags::BOLD, Flags::ITALIC, Flags::BOLD | Flags::ITALIC] {
            let run_font = variants.select(flags);
            assert_eq!(run_font.family.as_ref(), "JetBrains Mono");
            let fallbacks = run_font.fallbacks.clone().expect("every run carries fallbacks");
            assert_eq!(fallbacks.fallback_list(), FONT_FALLBACKS);
            // Nerd Font symbol families outrank the generic symbol fonts, and
            // the private-use `Unifont Sample` never appears.
            assert_eq!(
                fallbacks.fallback_list().first().map(String::as_str),
                Some("Symbols Nerd Font Mono")
            );
            assert!(!fallbacks.fallback_list().iter().any(|name| name == "Unifont Sample"));
        }

        assert_eq!(variants.select(Flags::BOLD).weight, FontWeight(700.0));
        assert_eq!(variants.select(Flags::empty()).weight, FontWeight(400.0));
        assert_eq!(variants.select(Flags::ITALIC).style, FontStyle::Italic);
        assert_eq!(variants.select(Flags::empty()).style, FontStyle::Normal);
    }

    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Box-drawing cells leave the shaped text]]
    #[test]
    fn box_drawing_cells_are_blanked_before_shaping() {
        // The overlay owns these codepoints; a space holds their grid column so
        // the glyphs after them keep their cell origins.
        assert_eq!(shaped_char('\u{2500}'), ' ');
        assert_eq!(shaped_char('\u{2588}'), ' ');
        // A stray control byte can never reach `shape_line`, which panics on a
        // newline in debug builds.
        assert_eq!(shaped_char('\n'), ' ');
        assert_eq!(shaped_char('a'), 'a');
        assert_eq!(shaped_char('\u{e0b0}'), '\u{e0b0}', "Nerd Font glyphs still shape");

        // Row trimming keeps decorated blanks so an underline rule survives.
        let blank = Cell::default();
        assert!(!is_painted_cell(&blank));
        assert!(is_painted_cell(&Cell { c: 'x', ..blank }));
        assert!(is_painted_cell(&Cell { flags: Flags::UNDERLINE, ..blank }));
    }

    /// Build a one-row element whose cells all keep the window background.
    fn element_with_highlights(highlights: Vec<MatchHighlight>) -> TerminalElement {
        let theme = minimal_dark();
        let mut cells = TerminalColors::new();
        cells.set_theme(&theme);
        let colors = GridColors {
            background: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            cells: Arc::new(cells),
            opacity: 1.0,
        };
        let content = Content { rows: vec![vec![Cell::default(); 8]], ..Content::default() };
        TerminalElement::new(
            content,
            GridFont::default(),
            colors,
            MatchHighlightColors::from_chrome(&theme.chrome),
            GridBounds::default(),
        )
        .with_highlights(highlights)
    }

    // @lat: [[test#GPUI Client Headless Suites#Find overlay#Matches recolour the cells they cover]]
    #[test]
    fn find_matches_recolour_only_the_cells_they_cover() {
        let element = element_with_highlights(vec![
            MatchHighlight { row: 0, start_col: 1, end_col: 2, current: true },
            MatchHighlight { row: 0, start_col: 5, end_col: 5, current: false },
            MatchHighlight { row: 1, start_col: 0, end_col: 7, current: false },
        ]);
        let default_bg = [0.0, 0.0, 0.0, 1.0];
        let plain = ResolvedCell {
            fg: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            bg: None,
            flags: Flags::empty(),
        };
        let mut resolved = vec![plain; 8];
        element.apply_highlights(0, default_bg, &mut resolved);

        let accent = MatchHighlightColors::from_chrome(&minimal_dark().chrome);
        // The current match takes the opaque accent plus its contrast text.
        for cell in resolved.iter().skip(1).take(2) {
            assert_eq!(cell.bg, Some(accent.current_bg));
            assert_eq!(cell.fg, accent.current_fg);
        }
        // A non-current match tints its own background and keeps its text.
        let window_bg = super::opaque_slot(default_bg);
        assert_eq!(resolved[5].bg, Some(accent.blend_passive(window_bg)));
        assert_eq!(resolved[5].fg, plain.fg);
        // Everything outside a span, and every span on another row, is untouched.
        for column in [0, 3, 4, 6, 7] {
            assert!(resolved[column].bg.is_none());
            assert_eq!(resolved[column].fg, plain.fg);
        }
    }
}

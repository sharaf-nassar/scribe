//! Running-window corpus for layered terminal image placement rendering.

use std::{
    cell::{Cell as StateCell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::Arc,
};

use gpui::{
    App, AppContext as _, Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, Rgba, Styled as _, TitlebarOptions, Window,
    WindowBounds, WindowOptions, canvas, div, px, size,
};
use gpui_platform::application;
use scribe_client::{
    color::TerminalColors,
    gpui_image_lifecycle::GpuiImageCache,
    search::{MatchHighlight, MatchHighlightColors},
    selection::SelectionSpan,
    terminal_image_scene::{CommittedImageScene, KITTY_IMAGE_PLACEHOLDER, LiveImageDefinition},
};
use scribe_common::{
    config::CursorShape,
    ids::SessionId,
    terminal_images::{
        CellExtent, PixelRect, PlaceholderMetadata, TerminalCellAnchor, TerminalGridEffect,
        TerminalImageCellClip, TerminalImageDefinition, TerminalImageDelete,
        TerminalImageDeleteScope, TerminalImageGeneration, TerminalImageId, TerminalImagePlacement,
        TerminalImagePlacementKind, TerminalImageProtocol, TerminalPlacementId, TerminalScreenKind,
    },
    theme::minimal_dark,
};
use vte::ansi::{Color, Rgb};

use crate::{
    terminal::{Cell, Content, Flags, ShellCursor, ShellCursorShape, ViewportPoint},
    terminal_element::{
        CursorPaint, GridBounds, GridColors, GridFont, TerminalElement, TerminalImagesPaint,
    },
};

const WINDOW_WIDTH: f32 = 840.0;
const WINDOW_HEIGHT: f32 = 500.0;
const GENERATION: TerminalImageGeneration = TerminalImageGeneration(1);
const FIXTURE_PROJECTED_GPU_BYTES: u64 = 91_136;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeStage {
    Initial,
    Pressure,
    FirstScroll,
    RepeatedScroll,
    ResizedClip,
    OffMarginResized,
    OffMarginScrolled,
    PaneClosed,
}

struct RendererProbe {
    focus: FocusHandle,
    session_id: SessionId,
    cache: Rc<RefCell<GpuiImageCache>>,
    scene: Arc<CommittedImageScene>,
    pressure_scene: Arc<CommittedImageScene>,
    first_scroll_scene: Arc<CommittedImageScene>,
    repeated_scroll_scene: Arc<CommittedImageScene>,
    resized_clip_scene: Arc<CommittedImageScene>,
    off_margin_resized_scene: Arc<CommittedImageScene>,
    off_margin_scrolled_scene: Arc<CommittedImageScene>,
    content: Arc<Content>,
    stage: ProbeStage,
    ready_logged: Rc<StateCell<bool>>,
    pressure_logged: Rc<StateCell<bool>>,
    close_logged: Rc<StateCell<bool>>,
    grid_bounds: GridBounds,
}

impl RendererProbe {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let scene = Arc::new(fixture_scene());
        let pressure_scene = Arc::new(fixture_pressure_scene());
        let first_scroll_scene = Arc::new(fixture_scrolled_scene(false));
        let repeated_scroll_scene = Arc::new(fixture_scrolled_scene(true));
        let resized_clip_scene = Arc::new(fixture_resized_scene());
        let off_margin_resized_scene = Arc::new(fixture_off_margin_scene(false));
        let off_margin_scrolled_scene = Arc::new(fixture_off_margin_scene(true));
        Self {
            focus,
            session_id: SessionId::new(),
            cache: Rc::new(RefCell::new(GpuiImageCache::with_projected_gpu_limit(
                FIXTURE_PROJECTED_GPU_BYTES,
            ))),
            scene,
            pressure_scene,
            first_scroll_scene,
            repeated_scroll_scene,
            resized_clip_scene,
            off_margin_resized_scene,
            off_margin_scrolled_scene,
            content: Arc::new(fixture_content()),
            stage: ProbeStage::Initial,
            ready_logged: Rc::new(StateCell::new(false)),
            pressure_logged: Rc::new(StateCell::new(false)),
            close_logged: Rc::new(StateCell::new(false)),
            grid_bounds: GridBounds::default(),
        }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_ref() {
            "d" => match self.cache.borrow_mut().invalidate_atlas(window) {
                Ok(()) => {
                    tracing::info!("terminal image renderer device-loss atlas invalidated");
                    cx.notify();
                }
                Err(error) => tracing::error!(%error, "renderer device-loss invalidation failed"),
            },
            "e" => match self.evict_cache(window) {
                Ok(()) => {
                    let stats = self.cache.borrow().stats();
                    tracing::info!(
                        atlas_drops = stats.atlas_drops,
                        final_reference_drops = stats.final_reference_drops,
                        "terminal image renderer cache evicted"
                    );
                    cx.notify();
                }
                Err(error) => tracing::error!(%error, "renderer cache eviction failed"),
            },
            "q" => {
                self.stage = ProbeStage::Pressure;
                cx.notify();
            }
            "s" => self.show_first_scroll(cx),
            "r" => self.show_repeated_scroll(cx),
            "z" => self.show_resized_clip(cx),
            "o" => self.show_off_margin_resized(cx),
            "u" => self.show_off_margin_scrolled(cx),
            "x" => log_delete_evidence(),
            "p" => {
                self.stage = ProbeStage::PaneClosed;
                tracing::info!("terminal image renderer pane close stage");
                cx.notify();
            }
            _ => {}
        }
    }

    fn show_first_scroll(&mut self, cx: &mut Context<Self>) {
        self.stage = ProbeStage::FirstScroll;
        if let Some(placement) = fixture_mapping_placement(&self.first_scroll_scene, 1) {
            tracing::info!(
                anchor_row = placement.anchor.row,
                source_y = placement.source.y,
                source_height = placement.source.height,
                destination_rows = placement.destination.rows,
                pixel_offset_y = placement.pixel_offset_y,
                clip_top = placement.cell_clip.map_or(-1, |clip| clip.top),
                clip_bottom = placement.cell_clip.map_or(-1, |clip| clip.bottom),
                "terminal image renderer first scroll mapping"
            );
        }
        cx.notify();
    }

    fn show_repeated_scroll(&mut self, cx: &mut Context<Self>) {
        self.stage = ProbeStage::RepeatedScroll;
        if let Some(placement) = fixture_mapping_placement(&self.repeated_scroll_scene, 1) {
            tracing::info!(
                anchor_row = placement.anchor.row,
                source_y = placement.source.y,
                source_height = placement.source.height,
                destination_rows = placement.destination.rows,
                pixel_offset_y = placement.pixel_offset_y,
                clip_top = placement.cell_clip.map_or(-1, |clip| clip.top),
                clip_bottom = placement.cell_clip.map_or(-1, |clip| clip.bottom),
                "terminal image renderer repeated scroll mapping"
            );
        }
        cx.notify();
    }

    fn show_resized_clip(&mut self, cx: &mut Context<Self>) {
        self.stage = ProbeStage::ResizedClip;
        if let Some(placement) = fixture_mapping_placement(&self.resized_clip_scene, 1) {
            tracing::info!(
                source_y = placement.source.y,
                source_height = placement.source.height,
                destination_rows = placement.destination.rows,
                pixel_offset_y = placement.pixel_offset_y,
                clip_top = placement.cell_clip.map_or(-1, |clip| clip.top),
                clip_bottom = placement.cell_clip.map_or(-1, |clip| clip.bottom),
                clip_right = placement.cell_clip.map_or(-1, |clip| clip.right),
                "terminal image renderer resize clip mapping"
            );
        }
        cx.notify();
    }

    fn show_off_margin_resized(&mut self, cx: &mut Context<Self>) {
        self.stage = ProbeStage::OffMarginResized;
        log_off_margin_mapping(&self.off_margin_resized_scene, true);
        cx.notify();
    }

    fn show_off_margin_scrolled(&mut self, cx: &mut Context<Self>) {
        self.stage = ProbeStage::OffMarginScrolled;
        log_off_margin_mapping(&self.off_margin_scrolled_scene, false);
        cx.notify();
    }

    fn evict_cache(
        &self,
        window: &mut Window,
    ) -> Result<(), scribe_client::gpui_image_lifecycle::GpuiImageError> {
        self.cache.borrow_mut().clear(window)
    }

    fn marker(&self) -> impl IntoElement {
        let cache = Rc::clone(&self.cache);
        let ready_logged = Rc::clone(&self.ready_logged);
        let pressure_logged = Rc::clone(&self.pressure_logged);
        let close_logged = Rc::clone(&self.close_logged);
        let grid_bounds = Rc::clone(&self.grid_bounds);
        let stage = self.stage;
        canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                let stats = cache.borrow().stats();
                if !ready_logged.replace(true) {
                    let bounds = grid_bounds.get().unwrap_or_default();
                    let font = GridFont::default();
                    tracing::info!(
                        scale_factor = window.scale_factor(),
                        grid_left = f32::from(bounds.left()),
                        grid_top = f32::from(bounds.top()),
                        cell_width = font.cell_width(),
                        line_height = font.line_height,
                        render_images_created = stats.render_images_created,
                        projected_gpu_bytes = cache.borrow().projected_gpu_bytes(),
                        "terminal image renderer ready"
                    );
                }
                if stage == ProbeStage::Pressure
                    && stats.pressure_rejections > 0
                    && !pressure_logged.replace(true)
                {
                    tracing::info!(
                        pressure_rejections = stats.pressure_rejections,
                        render_images_created = stats.render_images_created,
                        atlas_drops = stats.atlas_drops,
                        projected_gpu_bytes = cache.borrow().projected_gpu_bytes(),
                        "terminal image renderer pressure rejected without eviction"
                    );
                }
                if stage == ProbeStage::PaneClosed
                    && cache.borrow().projected_gpu_bytes() == 0
                    && !close_logged.replace(true)
                {
                    tracing::info!(
                        final_reference_drops = stats.final_reference_drops,
                        "terminal image renderer pane cache dropped"
                    );
                }
            },
        )
        .absolute()
        .size_full()
    }
}

impl Render for RendererProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = minimal_dark();
        let mut terminal_colors = TerminalColors::new();
        terminal_colors.set_theme(&theme);
        let colors = GridColors {
            background: Rgba { r: 0.02, g: 0.02, b: 0.025, a: 1.0 },
            cells: Arc::new(terminal_colors),
            opacity: 1.0,
        };
        let (session_id, scene, active_sessions) = match self.stage {
            ProbeStage::Initial => {
                (Some(self.session_id), Arc::clone(&self.scene), HashSet::from([self.session_id]))
            }
            ProbeStage::Pressure => (
                Some(self.session_id),
                Arc::clone(&self.pressure_scene),
                HashSet::from([self.session_id]),
            ),
            ProbeStage::FirstScroll => (
                Some(self.session_id),
                Arc::clone(&self.first_scroll_scene),
                HashSet::from([self.session_id]),
            ),
            ProbeStage::RepeatedScroll => (
                Some(self.session_id),
                Arc::clone(&self.repeated_scroll_scene),
                HashSet::from([self.session_id]),
            ),
            ProbeStage::ResizedClip => (
                Some(self.session_id),
                Arc::clone(&self.resized_clip_scene),
                HashSet::from([self.session_id]),
            ),
            ProbeStage::OffMarginResized => (
                Some(self.session_id),
                Arc::clone(&self.off_margin_resized_scene),
                HashSet::from([self.session_id]),
            ),
            ProbeStage::OffMarginScrolled => (
                Some(self.session_id),
                Arc::clone(&self.off_margin_scrolled_scene),
                HashSet::from([self.session_id]),
            ),
            ProbeStage::PaneClosed => {
                (None, Arc::new(CommittedImageScene::default()), HashSet::new())
            }
        };
        let element = TerminalElement::new(
            Arc::clone(&self.content),
            GridFont::default(),
            colors,
            MatchHighlightColors::from_chrome(&theme.chrome),
            Rc::clone(&self.grid_bounds),
        )
        .with_cursor(Some(CursorPaint { visible: true, shape: CursorShape::Block }))
        .with_selection(vec![SelectionSpan { row: 9, start_col: 31, end_col: 32 }])
        .with_highlights(vec![MatchHighlight { row: 9, start_col: 32, end_col: 32, current: true }])
        .with_terminal_images(TerminalImagesPaint {
            session_id,
            scene,
            cache: Rc::clone(&self.cache),
            active_sessions: Rc::new(active_sessions),
        });

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key))
            .relative()
            .size_full()
            .p(px(24.0))
            .bg(Rgba { r: 0.02, g: 0.02, b: 0.025, a: 1.0 })
            .child(element.paint())
            .child(self.marker())
    }
}

fn fixture_mapping_placement(
    scene: &CommittedImageScene,
    placement_id: u64,
) -> Option<&TerminalImagePlacement> {
    scene.placements().iter().find(|placement| placement.id == TerminalPlacementId(placement_id))
}

fn log_off_margin_mapping(scene: &CommittedImageScene, resized: bool) {
    let Some(placement) = fixture_mapping_placement(scene, 23) else { return };
    let clip = placement.cell_clip;
    if resized {
        tracing::info!(
            anchor_row = placement.anchor.row,
            source_y = placement.source.y,
            pixel_offset_x = placement.pixel_offset_x,
            pixel_offset_y = placement.pixel_offset_y,
            clip_top = clip.map_or(-1, |value| value.top),
            clip_left = clip.map_or(-1, |value| value.left),
            clip_bottom = clip.map_or(-1, |value| value.bottom),
            clip_right = clip.map_or(-1, |value| value.right),
            "terminal image renderer off-margin resized mapping"
        );
    } else {
        tracing::info!(
            anchor_row = placement.anchor.row,
            source_y = placement.source.y,
            pixel_offset_x = placement.pixel_offset_x,
            pixel_offset_y = placement.pixel_offset_y,
            clip_top = clip.map_or(-1, |value| value.top),
            clip_left = clip.map_or(-1, |value| value.left),
            clip_bottom = clip.map_or(-1, |value| value.bottom),
            clip_right = clip.map_or(-1, |value| value.right),
            "terminal image renderer off-margin scrolled mapping"
        );
    }
}

fn fixture_content() -> Content {
    let mut rows = vec![vec![Cell::default(); 80]; 24];
    write_text(&mut rows, 0, 1, "CROP     SCALE     ALPHA");
    write_text(&mut rows, 7, 1, "D DEEP       N NEGATIVE       P POSITIVE       S SIXEL");
    write_text(&mut rows, 14, 1, "PLACEHOLDER 32-BIT ID + INHERITANCE");

    let background = Color::Spec(Rgb { r: 45, g: 65, b: 110 });
    if let Some(phase_row) = rows.get_mut(8) {
        paint_background_ranges(phase_row, background);
        for (column, marker) in [(2, 'D'), (16, '\u{2501}'), (28, 'P'), (41, 'S')] {
            if let Some(cell) = phase_row.get_mut(column) {
                cell.c = marker;
                cell.flags = Flags::BOLD;
            }
        }
    }
    if let Some(cell) = rows.get_mut(9).and_then(|row| row.get_mut(42)) {
        cell.c = '\u{2501}';
        cell.flags = Flags::BOLD;
    }

    for (row, source_row) in rows.iter_mut().skip(16).take(2).zip(['\u{0305}', '\u{030d}']) {
        for (cell, source_column) in row.iter_mut().skip(2).take(2).zip(['\u{0305}', '\u{030d}']) {
            *cell = Cell {
                c: KITTY_IMAGE_PLACEHOLDER,
                fg: Color::Spec(Rgb { r: 0, g: 0, b: 42 }),
                bg: Color::Spec(Rgb { r: 85, g: 30, b: 105 }),
                zerowidth: [source_row, source_column, '\u{030e}'],
                zerowidth_len: 3,
                ..Cell::default()
            };
        }
    }
    // Inheritance path: row mark on first cell, then no marks to its right.
    if let Some(row) = rows.get_mut(20) {
        if let Some(cell) = row.get_mut(2) {
            *cell = Cell {
                c: KITTY_IMAGE_PLACEHOLDER,
                fg: Color::Indexed(43),
                bg: Color::Spec(Rgb { r: 85, g: 30, b: 105 }),
                zerowidth: ['\u{0305}', '\u{0305}', '\0'],
                zerowidth_len: 2,
                ..Cell::default()
            };
        }
        if let Some(cell) = row.get_mut(3) {
            *cell = Cell {
                c: KITTY_IMAGE_PLACEHOLDER,
                fg: Color::Indexed(43),
                bg: Color::Spec(Rgb { r: 85, g: 30, b: 105 }),
                ..Cell::default()
            };
        }
    }
    Content {
        rows,
        shell_cursor: Some(ShellCursor {
            point: ViewportPoint { row: 8, col: 29 },
            shape: ShellCursorShape::Block,
        }),
        ..Content::default()
    }
}

fn paint_background_ranges(row: &mut [Cell], background: Color) {
    for (start, end) in [(1, 7), (14, 20), (27, 33), (40, 46)] {
        let Some(cells) = row.get_mut(start..=end) else { continue };
        for cell in cells {
            cell.bg = background;
        }
    }
}

fn write_text(rows: &mut [Vec<Cell>], row: usize, column: usize, text: &str) {
    let Some(target) = rows.get_mut(row) else { return };
    for (cell, character) in target.iter_mut().skip(column).zip(text.chars()) {
        cell.c = character;
        cell.flags = Flags::BOLD;
    }
}

fn fixture_scene() -> CommittedImageScene {
    let definitions = fixture_definitions();
    let placements = vec![
        with_y_offset(
            classic_placement(
                (1, 1),
                (1, 1),
                (4, 4),
                PixelRect { x: 32, y: 0, width: 32, height: 64 },
                -1,
            ),
            8,
        ),
        classic_placement((2, 2), (1, 9), (6, 4), full_source(16, 16), -1),
        classic_placement((3, 3), (1, 18), (6, 4), full_source(24, 24), 0),
        classic_placement((4, 4), (8, 1), (7, 3), full_source(8, 8), -1_073_741_825),
        classic_placement((5, 5), (8, 14), (7, 3), full_source(8, 8), -1),
        classic_placement((6, 6), (8, 27), (7, 3), full_source(8, 8), 0),
        sixel_placement((7, 7), (8, 40), (7, 3), full_source(8, 8)),
        sixel_placement((11, 11), (8, 42), (5, 3), full_source(8, 8)),
        with_offsets(classic_placement((22, 2), (1, 55), (4, 2), full_source(16, 16), -1), 5, 8),
        with_offsets(classic_placement((23, 2), (12, 50), (4, 2), full_source(16, 16), -1), 5, 8),
        placeholder_placement((8, 33_554_474), (2, 2), full_source(64, 64), 32),
        placeholder_placement((42, 43), (2, 1), PixelRect { x: 0, y: 0, width: 32, height: 32 }, 8),
        placeholder_placement(
            (43, 43),
            (2, 1),
            PixelRect { x: 32, y: 0, width: 32, height: 32 },
            8,
        ),
    ];
    CommittedImageScene {
        generation: Some(GENERATION),
        definitions,
        primary_placements: placements,
        active_screen: TerminalScreenKind::Primary,
        ..CommittedImageScene::default()
    }
}

fn fixture_pressure_scene() -> CommittedImageScene {
    let mut scene = fixture_scene();
    if let Some(definition) = solid(99_999_999, 8, 8, [255, 255, 255, 255]) {
        scene.definitions.push(definition);
        scene.primary_placements.push(classic_placement(
            (99_999_999, 99_999_999),
            (12, 55),
            (4, 2),
            full_source(8, 8),
            0,
        ));
    }
    scene
}

/// Scroll margins are half-open, so `bottom: 5` covers rows 1..=4 — exactly the
/// four destination rows of the offset placement, leaving its pixel-offset
/// spill row outside the margin so partial clipping stays observable.
fn fixture_scrolled_scene(repeated: bool) -> CommittedImageScene {
    let mut scene = fixture_scene();
    scene.apply_grid_effect(&TerminalGridEffect::Scroll { top: 1, bottom: 5, rows: 1 });
    scene.apply_grid_effect(&TerminalGridEffect::Scroll { top: 8, bottom: 10, rows: 1 });
    if repeated {
        scene.apply_grid_effect(&TerminalGridEffect::Scroll { top: 1, bottom: 5, rows: 1 });
    }
    scene
}

fn fixture_resized_scene() -> CommittedImageScene {
    let mut scene = fixture_scrolled_scene(true);
    scene.apply_grid_effect(&TerminalGridEffect::ResizeClip { columns: 57, rows: 2 });
    scene
}

fn fixture_off_margin_scene(scrolled: bool) -> CommittedImageScene {
    let mut scene = fixture_scene();
    scene.apply_grid_effect(&TerminalGridEffect::ResizeClip { columns: 57, rows: 16 });
    if scrolled {
        scene.apply_grid_effect(&TerminalGridEffect::Scroll { top: 1, bottom: 5, rows: 1 });
    }
    scene
}

fn deletion_fixture_scene() -> CommittedImageScene {
    let definitions = [
        solid(5, 8, 8, [40, 210, 80, 255]),
        placeholder_quadrants(33_554_474, 64, 64),
        solid(99, 8, 8, [255, 255, 255, 255]),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let retained_rgba_bytes = definitions.iter().map(|item| item.metadata.rgba_bytes).sum();
    let mut physical = classic_placement((5, 5), (8, 14), (7, 3), full_source(8, 8), -1);
    physical.cell_clip = Some(TerminalImageCellClip { top: 8, left: 15, bottom: 10, right: 18 });
    CommittedImageScene {
        generation: Some(GENERATION),
        definitions,
        primary_placements: vec![
            physical,
            placeholder_placement((8, 33_554_474), (2, 2), full_source(64, 64), 32),
        ],
        active_screen: TerminalScreenKind::Primary,
        retained_rgba_bytes,
        ..CommittedImageScene::default()
    }
}

fn deleted_scene(delete: TerminalImageDelete) -> CommittedImageScene {
    let mut scene = deletion_fixture_scene();
    scene.apply_delete(delete, None);
    scene
}

fn delete_evidence(
    scope: TerminalImageDeleteScope,
    image_id: Option<u64>,
    placement_id: Option<u64>,
    coordinate: Option<i32>,
    free_image_data: bool,
) -> CommittedImageScene {
    deleted_scene(TerminalImageDelete {
        scope,
        image_id: image_id.map(TerminalImageId),
        placement_id: placement_id.map(TerminalPlacementId),
        coordinate,
        free_image_data,
    })
}

fn log_delete_evidence() {
    let row_outside = delete_evidence(TerminalImageDeleteScope::Row, None, None, Some(10), false);
    let row_inside = delete_evidence(TerminalImageDeleteScope::Row, None, None, Some(9), false);
    let column_outside =
        delete_evidence(TerminalImageDeleteScope::Column, None, None, Some(14), false);
    let column_inside =
        delete_evidence(TerminalImageDeleteScope::Column, None, None, Some(16), false);
    let cell_inside = delete_evidence(TerminalImageDeleteScope::Cell, None, None, Some(9), false);
    let virtual_placement = delete_evidence(
        TerminalImageDeleteScope::Placement,
        Some(33_554_474),
        Some(8),
        None,
        false,
    );
    let z_index = delete_evidence(TerminalImageDeleteScope::ZIndex, None, None, Some(-1), false);
    tracing::info!(
        row_outside_placements = row_outside.placements().len(),
        row_inside_placements = row_inside.placements().len(),
        column_outside_placements = column_outside.placements().len(),
        column_inside_placements = column_inside.placements().len(),
        cell_inside_placements = cell_inside.placements().len(),
        virtual_placement_placements = virtual_placement.placements().len(),
        z_index_placements = z_index.placements().len(),
        "terminal image renderer deletion evidence"
    );
    log_hard_delete_evidence();
}

fn log_hard_delete_evidence() {
    let cell = delete_evidence(TerminalImageDeleteScope::Cell, None, None, Some(9), true);
    let row = delete_evidence(TerminalImageDeleteScope::Row, None, None, Some(9), true);
    let column = delete_evidence(TerminalImageDeleteScope::Column, None, None, Some(16), true);
    let z_index = delete_evidence(TerminalImageDeleteScope::ZIndex, None, None, Some(-1), true);
    let placement =
        delete_evidence(TerminalImageDeleteScope::Placement, Some(5), Some(5), None, true);
    let all = delete_evidence(TerminalImageDeleteScope::AllPlacements, None, None, None, true);
    let virtual_image =
        delete_evidence(TerminalImageDeleteScope::Image, Some(33_554_474), None, None, true);
    let unplaced_image =
        delete_evidence(TerminalImageDeleteScope::Image, Some(99), None, None, true);
    tracing::info!(
        cell_unplaced_definition = u8::from(has_definition(&cell, 99)),
        cell_hard_definitions = cell.definitions.len(),
        row_unplaced_definition = u8::from(has_definition(&row, 99)),
        row_hard_definitions = row.definitions.len(),
        column_unplaced_definition = u8::from(has_definition(&column, 99)),
        column_hard_definitions = column.definitions.len(),
        z_index_unplaced_definition = u8::from(has_definition(&z_index, 99)),
        z_index_hard_definitions = z_index.definitions.len(),
        placement_unplaced_definition = u8::from(has_definition(&placement, 99)),
        placement_hard_definitions = placement.definitions.len(),
        all_hard_placements = all.placements().len(),
        all_hard_definitions = all.definitions.len(),
        virtual_image_hard_placements = virtual_image.placements().len(),
        virtual_image_hard_definitions = virtual_image.definitions.len(),
        unplaced_image_hard_placements = unplaced_image.placements().len(),
        unplaced_image_hard_definitions = unplaced_image.definitions.len(),
        unplaced_image_hard_target_present = u8::from(has_definition(&unplaced_image, 99)),
        "terminal image renderer hard deletion evidence"
    );
}

fn has_definition(scene: &CommittedImageScene, image_id: u64) -> bool {
    scene.definitions.iter().any(|definition| definition.metadata.id == TerminalImageId(image_id))
}

fn fixture_definitions() -> Vec<LiveImageDefinition> {
    [
        quadrants(1, 64, 64),
        solid(2, 16, 16, [0, 190, 230, 255]),
        solid(3, 24, 24, [255, 0, 170, 128]),
        solid(4, 8, 8, [220, 40, 40, 255]),
        solid(5, 8, 8, [40, 210, 80, 255]),
        solid(6, 8, 8, [30, 90, 230, 255]),
        solid(7, 8, 8, [235, 190, 25, 255]),
        solid(11, 8, 8, [180, 30, 210, 255]),
        placeholder_quadrants(33_554_474, 64, 64),
        quadrants(43, 64, 32),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn with_y_offset(
    mut placement: TerminalImagePlacement,
    pixel_offset_y: u16,
) -> TerminalImagePlacement {
    placement.pixel_offset_y = pixel_offset_y;
    placement
}

fn with_offsets(
    mut placement: TerminalImagePlacement,
    pixel_offset_x: u16,
    pixel_offset_y: u16,
) -> TerminalImagePlacement {
    placement.pixel_offset_x = pixel_offset_x;
    placement.pixel_offset_y = pixel_offset_y;
    placement
}

fn placeholder_quadrants(id: u64, width: u32, height: u32) -> Option<LiveImageDefinition> {
    image_definition(id, width, height, |x, y| match (x < width / 2, y < height / 2) {
        (true, true) => [0, 0, 0, 0],
        (false, true) => [0, 255, 0, 255],
        (true, false) => [0, 0, 255, 255],
        (false, false) => [255, 255, 0, 255],
    })
}

fn classic_placement(
    ids: (u64, u64),
    anchor: (i32, u16),
    extent: (u16, u16),
    source: PixelRect,
    z_index: i32,
) -> TerminalImagePlacement {
    TerminalImagePlacement {
        id: TerminalPlacementId(ids.0),
        image_id: TerminalImageId(ids.1),
        generation: GENERATION,
        protocol: TerminalImageProtocol::Kitty,
        kind: TerminalImagePlacementKind::KittyClassic,
        anchor: TerminalCellAnchor { row: anchor.0, column: anchor.1 },
        source,
        destination: CellExtent { columns: extent.0, rows: extent.1 },
        pixel_offset_x: 0,
        pixel_offset_y: 0,
        z_index,
        scrolls_with_grid: true,
        move_cursor: false,
        cell_clip: None,
        placeholder: None,
    }
}

fn sixel_placement(
    ids: (u64, u64),
    anchor: (i32, u16),
    extent: (u16, u16),
    source: PixelRect,
) -> TerminalImagePlacement {
    TerminalImagePlacement {
        protocol: TerminalImageProtocol::Sixel,
        kind: TerminalImagePlacementKind::Sixel,
        ..classic_placement(ids, anchor, extent, source, 0)
    }
}

fn placeholder_placement(
    ids: (u64, u64),
    extent: (u16, u16),
    source: PixelRect,
    identity_bits: u8,
) -> TerminalImagePlacement {
    TerminalImagePlacement {
        id: TerminalPlacementId(ids.0),
        image_id: TerminalImageId(ids.1),
        generation: GENERATION,
        protocol: TerminalImageProtocol::Kitty,
        kind: TerminalImagePlacementKind::KittyUnicodePlaceholder,
        anchor: TerminalCellAnchor { row: 0, column: 0 },
        source,
        destination: CellExtent { columns: extent.0, rows: extent.1 },
        pixel_offset_x: 0,
        pixel_offset_y: 0,
        z_index: 0,
        scrolls_with_grid: true,
        move_cursor: false,
        cell_clip: None,
        placeholder: Some(PlaceholderMetadata {
            image_identity_bits: identity_bits,
            placement_id_in_underline: false,
            // Reserved compatibility byte: the transparent fixture proves it
            // does not become a second image-opacity channel.
            background_alpha: 255,
        }),
    }
}

fn full_source(width: u32, height: u32) -> PixelRect {
    PixelRect { x: 0, y: 0, width, height }
}

fn solid(id: u64, width: u32, height: u32, rgba: [u8; 4]) -> Option<LiveImageDefinition> {
    image_definition(id, width, height, |_, _| rgba)
}

fn quadrants(id: u64, width: u32, height: u32) -> Option<LiveImageDefinition> {
    image_definition(id, width, height, |x, y| match (x < width / 2, y < height / 2) {
        (true, true) => [255, 0, 0, 255],
        (false, true) => [0, 255, 0, 255],
        (true, false) => [0, 0, 255, 255],
        (false, false) => [255, 255, 0, 255],
    })
}

fn image_definition(
    id: u64,
    width: u32,
    height: u32,
    pixel: impl Fn(u32, u32) -> [u8; 4],
) -> Option<LiveImageDefinition> {
    let Ok(metadata) =
        TerminalImageDefinition::new(TerminalImageId(id), GENERATION, width, height, true)
    else {
        return None;
    };
    let mut rgba = Vec::with_capacity(usize::try_from(metadata.rgba_bytes).unwrap_or(0));
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&pixel(x, y));
        }
    }
    Some(LiveImageDefinition { metadata, rgba: Arc::from(rgba) })
}

/// Run the isolated terminal-image renderer corpus window.
pub fn run() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Scribe Terminal Image Renderer".into()),
                    ..Default::default()
                }),
                app_id: Some("scribe-terminal-image-renderer".to_owned()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| RendererProbe::new(window, cx)),
        ) {
            Ok(_) => cx.activate(true),
            Err(error) => tracing::error!(%error, "failed to open renderer probe window"),
        }
    });
}

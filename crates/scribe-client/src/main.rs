//! Scribe terminal client -- multi-pane winit + wgpu terminal emulator.

mod ai_indicator;
mod clipboard_cleanup;
mod close_dialog;
mod config;
mod context_menu;
mod divider;
mod input;
mod ipc_client;
mod layout;
mod mouse_reporting;
mod mouse_state;
mod pane;
mod scrollbar;
mod search_overlay;
mod selection;
mod splash;
mod status_bar;
mod sys_stats;
mod tab_bar;
mod update_dialog;
mod url_detect;
mod window_state;
mod workspace_layout;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use scribe_common::config::{ContentPadding, ScribeConfig, resolve_theme};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::UpdateProgressState;
use scribe_common::theme::Theme;
use scribe_renderer::TerminalRenderer;
use scribe_renderer::types::GridSize;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::Window;

use crate::ai_indicator::AiStateTracker;
use crate::divider::DividerDrag;
use crate::input::{KeyAction, LayoutAction};
use crate::ipc_client::{ClientCommand, UiEvent};
use crate::layout::{PaneEdges, PaneId, Rect};
use crate::pane::Pane;
use crate::workspace_layout::WindowLayout;

/// GPU resources shared across all panes.
struct GpuContext {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    /// Single shared renderer with the glyph atlas and pipeline.
    renderer: TerminalRenderer,
    /// Splash-screen renderer, present until the first PTY output arrives.
    splash: Option<splash::SplashRenderer>,
}

/// Intermediate result from read-only scrollbar hit-testing, used to
/// split the borrow of `self.panes` into immutable then mutable phases.
enum ScrollbarAction {
    StartDrag { display_offset: usize },
    JumpTo { delta: i32 },
}

/// State for an in-progress tab drag-reorder operation.
struct TabDrag {
    /// Workspace the dragged tab belongs to.
    workspace_id: WorkspaceId,
    /// Current tab index of the dragged tab (updated on live reorder).
    tab_index: usize,
    /// Cursor X at drag start (used for threshold detection).
    start_x: f32,
    /// Cursor Y at drag start (used for threshold detection).
    start_y: f32,
    /// Current cursor X (updated on mouse move).
    cursor_x: f32,
    /// Current cursor Y (updated on mouse move).
    cursor_y: f32,
    /// `true` once the cursor has moved more than 5 px from the start.
    dragging: bool,
    /// Cursor X minus tab left edge at drag start; keeps the tab under the cursor.
    grab_offset_x: f32,
}

/// Application state for the winit event loop.
#[allow(
    clippy::struct_excessive_bools,
    reason = "App tracks independent boolean flags: animation, splash, cursor visibility, blink"
)]
struct App {
    // Window identity
    /// Window ID from CLI arg (if provided) or assigned by the server.
    window_id: Option<WindowId>,

    // Config + Theme
    config: ScribeConfig,
    theme: Theme,

    // Window + GPU
    window: Option<Arc<Window>>,
    gpu: Option<GpuContext>,

    // IPC
    cmd_tx: Option<Sender<ClientCommand>>,

    // Layout
    window_layout: WindowLayout,
    panes: HashMap<PaneId, Pane>,
    session_to_pane: HashMap<SessionId, PaneId>,
    /// Temporary session IDs awaiting server confirmation, in creation order.
    pending_sessions: VecDeque<SessionId>,

    // Divider drag
    divider_drag: Option<DividerDrag>,
    workspace_divider_drag: Option<workspace_layout::WorkspaceDividerDrag>,

    /// Active scrollbar drag state (pane ID being dragged).
    scrollbar_drag_pane: Option<layout::PaneId>,

    // Text selection
    /// Active text selection, set on mouse press and extended on move.
    active_selection: Option<selection::SelectionRange>,
    /// Whether the left mouse button is currently held (for drag detection).
    mouse_selecting: bool,
    /// Click state for single/double/triple click classification.
    mouse_click: mouse_state::MouseClickState,
    /// Word bounds from the initial double-click (for drag-by-word).
    word_drag_anchor: Option<(selection::SelectionPoint, selection::SelectionPoint)>,

    // Connection state
    /// Whether the IPC connection to the server is alive.
    server_connected: bool,

    // AI state
    ai_tracker: AiStateTracker,
    animation_running: bool,
    animation_stop: Arc<AtomicBool>,

    // Input state
    modifiers: ModifiersState,
    /// Parsed keybindings (hot-reloaded with config).
    bindings: input::Bindings,

    // Clipboard
    clipboard: Option<arboard::Clipboard>,

    // Zoom
    /// Runtime zoom delta in font-size points, clamped to [-7, +7].
    zoom_level: i8,

    // Search
    search_overlay: search_overlay::SearchOverlay,

    // Close dialog overlay (shown on window close request)
    close_dialog: Option<close_dialog::CloseDialog>,

    // Update state
    /// Available update version and URL. Set by `UpdateAvailable`, cleared on dismiss.
    update_available: Option<(String, String)>,
    /// Current update progress state. Set by `UpdateProgress`, cleared on completion/failure.
    update_progress: Option<UpdateProgressState>,
    /// Active update confirmation dialog (shown when user clicks the update button).
    update_dialog: Option<update_dialog::UpdateDialog>,
    /// Clickable update button rect in tab bars `(workspace_id, rect)` (updated each frame).
    tab_bar_update_targets: Vec<(WorkspaceId, layout::Rect)>,

    // Context menu overlay (shown on right-click)
    context_menu: Option<context_menu::ContextMenu>,

    /// Whether the splash screen is still showing.
    /// Set to `true` on init; cleared after the splash has been visible for
    /// [`MIN_SPLASH_DURATION`] and content is ready to display.
    splash_active: bool,

    /// Set during init; cleared after the first rendered splash frame
    /// triggers `ListSessions` so that session discovery happens while the
    /// splash is visible rather than before it renders.
    splash_needs_list_sessions: bool,

    /// Instant when the splash first rendered, used to enforce a minimum
    /// display duration so the compositor has time to present it.
    splash_first_rendered: Option<Instant>,

    /// Content (snapshot or PTY output) has arrived while the splash is
    /// still active.  Dismissal is deferred until [`MIN_SPLASH_DURATION`]
    /// has elapsed since [`splash_first_rendered`].
    splash_content_ready: bool,

    // Pre-created wgpu instance (created before event loop)
    wgpu_instance: wgpu::Instance,

    // Event loop proxy for the IPC thread (consumed on init)
    proxy: Option<EventLoopProxy<UiEvent>>,

    /// Cloned proxy for the animation timer thread.
    animation_proxy: Option<EventLoopProxy<UiEvent>>,

    /// Last recorded cursor position for divider drag.
    last_cursor_pos: Option<(f32, f32)>,

    /// Last animation tick time.
    last_tick: Instant,

    /// Whether the cursor is currently visible (toggled by blink timer).
    cursor_visible: bool,
    /// Whether cursor blinking is enabled (from config).
    cursor_blink_enabled: bool,
    /// Time of last blink toggle.
    blink_timer: Instant,

    /// Current opacity (0.0-1.0). Applied to clear color and cell backgrounds.
    opacity: f32,
    /// Whether the window was created with transparency support.
    window_transparent: bool,

    /// Per-window geometry registry (multi-window support).
    window_registry: window_state::WindowRegistry,
    /// Loaded geometry to apply during init (consumed once).
    saved_geometry: Option<window_state::WindowGeometry>,
    /// When set, a geometry save is pending (debounced).
    geometry_save_pending: Option<Instant>,
    /// When set, a resize IPC flush is pending (debounced).
    resize_pending: Option<Instant>,

    /// `true` after a workspace tree has been received from the server,
    /// suppressing the legacy `split_direction` fallback in
    /// `handle_workspace_info`.
    received_workspace_tree: bool,
    /// Clickable tab rects `(workspace_id, tab_index, rect)` (updated each frame).
    tab_hit_targets: Vec<(WorkspaceId, usize, layout::Rect)>,
    /// Close button rects `(workspace_id, tab_index, rect)` (updated each frame).
    tab_close_hit_targets: Vec<(WorkspaceId, usize, layout::Rect)>,
    /// Which tab's close button is currently hovered: `(workspace_id, tab_index)`.
    hovered_tab_close: Option<(WorkspaceId, usize)>,
    /// Active tab drag state for reordering.
    tab_drag: Option<TabDrag>,
    /// Per-tab pixel X offsets for the slide animation on the drag workspace.
    tab_drag_offsets: Vec<f32>,
    /// Clickable equalize rects from tab bars `(workspace_id, rect)` (updated each frame).
    tab_bar_equalize_targets: Vec<(WorkspaceId, layout::Rect)>,
    /// Clickable rect for the status bar gear icon (updated each frame).
    status_bar_gear_rect: Option<layout::Rect>,
    /// Clickable rect for the status bar equalize icon (updated each frame).
    status_bar_equalize_rect: Option<layout::Rect>,

    /// System hostname for the window-level status bar (fetched once at startup).
    hostname: String,
    /// System resource stats collector for the status bar.
    sys_stats: sys_stats::SystemStatsCollector,

    /// Per-pane URL span caches (dirty-flag lazy refresh).
    url_caches: HashMap<PaneId, url_detect::PaneUrlCache>,
    /// The URL span the cursor is currently hovering over, if any.
    hovered_url: Option<url_detect::UrlSpan>,

    /// Config file watcher -- kept alive for its side-effect of sending
    /// `UiEvent::ConfigChanged` events.
    #[allow(dead_code, reason = "watcher must be stored to keep receiving file-system events")]
    _config_watcher: Option<notify::RecommendedWatcher>,
}

impl App {
    #[allow(
        clippy::too_many_lines,
        reason = "App::new initialises all fields; splitting adds no clarity"
    )]
    fn new(
        wgpu_instance: wgpu::Instance,
        proxy: EventLoopProxy<UiEvent>,
        window_id: Option<WindowId>,
    ) -> Self {
        let animation_proxy = proxy.clone();
        let watcher_proxy = proxy.clone();

        let config = scribe_common::config::load_config().unwrap_or_else(|e| {
            tracing::warn!("failed to load config: {e}, using defaults");
            ScribeConfig::default()
        });
        let theme = resolve_theme(&config);

        let config_watcher = config::start_config_watcher(watcher_proxy);

        let initial_workspace_id = WorkspaceId::new();
        let initial_accent = theme.chrome.accent;
        let cursor_blink_enabled = config.appearance.cursor_blink;

        let opacity = config.appearance.opacity;
        let window_transparent = config.appearance.opacity < 1.0;
        let bindings = input::Bindings::parse(&config.keybindings);
        let window_registry = window_state::WindowRegistry::new();
        let saved_geometry = window_id.map(|wid| window_registry.load(wid));
        let claude_states = config.terminal.claude_states.clone();

        Self {
            window_id,
            config,
            theme,
            window: None,
            gpu: None,
            cmd_tx: None,
            window_layout: WindowLayout::new(initial_workspace_id, Some(initial_accent)),
            panes: HashMap::new(),
            session_to_pane: HashMap::new(),
            pending_sessions: VecDeque::new(),
            divider_drag: None,
            workspace_divider_drag: None,
            scrollbar_drag_pane: None,
            active_selection: None,
            mouse_selecting: false,
            mouse_click: mouse_state::MouseClickState::new(),
            word_drag_anchor: None,
            server_connected: false,
            ai_tracker: AiStateTracker::new(claude_states),
            animation_running: false,
            animation_stop: Arc::new(AtomicBool::new(false)),
            modifiers: ModifiersState::default(),
            bindings,
            clipboard: arboard::Clipboard::new()
                .map_err(|e| {
                    tracing::warn!("clipboard unavailable: {e}");
                })
                .ok(),
            zoom_level: 0,
            search_overlay: search_overlay::SearchOverlay::new(),
            close_dialog: None,
            update_available: None,
            update_progress: None,
            update_dialog: None,
            tab_bar_update_targets: Vec::new(),
            context_menu: None,
            splash_active: true,
            splash_needs_list_sessions: true,
            splash_first_rendered: None,
            splash_content_ready: false,
            wgpu_instance,
            proxy: Some(proxy),
            animation_proxy: Some(animation_proxy),
            last_cursor_pos: None,
            last_tick: Instant::now(),
            cursor_visible: true,
            cursor_blink_enabled,
            blink_timer: Instant::now(),
            opacity,
            window_transparent,
            window_registry,
            saved_geometry,
            geometry_save_pending: None,
            resize_pending: None,
            received_workspace_tree: false,
            tab_hit_targets: Vec::new(),
            tab_close_hit_targets: Vec::new(),
            hovered_tab_close: None,
            tab_drag: None,
            tab_drag_offsets: Vec::new(),
            tab_bar_equalize_targets: Vec::new(),
            status_bar_gear_rect: None,
            status_bar_equalize_rect: None,
            hostname: read_hostname(),
            sys_stats: sys_stats::SystemStatsCollector::new(),
            url_caches: HashMap::new(),
            hovered_url: None,
            _config_watcher: config_watcher,
        }
    }
}

// ---------------------------------------------------------------------------
// ApplicationHandler implementation
// ---------------------------------------------------------------------------

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // Already initialised (e.g. redundant resumed event).
            return;
        }

        if let Err(e) = self.init_gpu_and_terminal(event_loop) {
            tracing::error!(error = %e, "failed to initialise GPU / terminal");
            event_loop.exit();
            return;
        }

        // Restore the settings window if it was open when the app last exited.
        // Only for fresh launches (no --window-id), not spawned child windows.
        if self.window_id.is_none() {
            restore_settings_if_open();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::PtyOutput { session_id, data } => {
                self.handle_pty_output(session_id, &data);
            }
            UiEvent::ScreenSnapshot { session_id, snapshot } => {
                self.handle_screen_snapshot(session_id, &snapshot);
            }
            UiEvent::SessionCreated { session_id, .. } => {
                self.handle_session_created(session_id);
            }
            UiEvent::SessionExited { session_id, .. } => {
                self.handle_session_exited(session_id);
            }
            UiEvent::AiStateChanged { session_id, ai_state } => {
                self.handle_ai_state_changed(session_id, ai_state);
            }
            UiEvent::AiStateCleared { session_id } => {
                self.ai_tracker.remove(session_id);
                self.request_redraw();
            }
            UiEvent::CwdChanged { session_id, cwd } => {
                self.handle_cwd_changed(session_id, cwd);
            }
            UiEvent::TitleChanged { session_id, title } => {
                self.handle_title_changed(session_id, &title);
            }
            UiEvent::GitBranch { session_id, branch } => {
                self.handle_git_branch(session_id, branch);
            }
            UiEvent::WorkspaceInfo { workspace_id, name, accent_color, split_direction } => {
                self.handle_workspace_info(workspace_id, name, &accent_color, split_direction);
            }
            UiEvent::SessionList { sessions, workspace_tree } => {
                self.handle_session_list(&sessions, workspace_tree.as_ref());
            }
            UiEvent::WorkspaceNamed { workspace_id, name } => {
                self.handle_workspace_named(workspace_id, &name);
            }
            UiEvent::ConfigChanged => {
                self.handle_config_changed();
            }
            UiEvent::ServerDisconnected => {
                tracing::info!("server disconnected, exiting");
                self.server_connected = false;
                self.flush_geometry_now();
                self.request_redraw();
                event_loop.exit();
            }
            UiEvent::AnimationTick => {
                self.handle_animation_tick();
            }
            UiEvent::Welcome { window_id, other_windows } => {
                self.handle_welcome(event_loop, window_id, &other_windows);
            }
            UiEvent::QuitRequested => {
                self.handle_quit_requested(event_loop);
            }
            UiEvent::UpdateAvailable { version, release_url } => {
                self.update_available = Some((version, release_url));
                self.request_redraw();
            }
            UiEvent::UpdateProgress { state } => {
                self.update_progress = Some(state);
                self.request_redraw();
            } // QuitAllFromDialog / CloseWindow / CancelClose were removed —
              // the close dialog is now an in-app GPU overlay (close_dialog module).
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.cursor_blink_enabled {
            let elapsed = self.blink_timer.elapsed();
            if elapsed >= BLINK_INTERVAL {
                // Blink interval elapsed — request a redraw so `handle_redraw`
                // toggles `cursor_visible` and paints the new state.
                self.request_redraw();
            } else {
                let remaining = BLINK_INTERVAL.saturating_sub(elapsed);
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + remaining));
            }
        }
        // When blink is disabled, don't set ControlFlow — let winit use its default (Wait).

        self.flush_geometry_if_due();
        self.flush_resize_if_due();
    }

    #[allow(
        clippy::too_many_lines,
        reason = "dispatches close/update dialog intercepts and main event variants; splitting adds indirection"
    )]
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        // When the close dialog is active, intercept input events and route
        // them to the dialog instead of the terminal / layout handlers.
        if self.close_dialog.is_some() {
            match event {
                WindowEvent::CloseRequested => {
                    self.handle_close_requested(event_loop);
                }
                WindowEvent::RedrawRequested => self.handle_redraw(),
                WindowEvent::Resized(size) => {
                    self.handle_resize(size);
                    self.mark_geometry_dirty();
                }
                WindowEvent::Moved(_) => self.mark_geometry_dirty(),
                WindowEvent::ModifiersChanged(new_mods) => {
                    self.modifiers = new_mods.state();
                }
                WindowEvent::KeyboardInput { event: ref key_event, .. } => {
                    self.handle_dialog_keyboard(key_event, event_loop);
                }
                WindowEvent::MouseInput {
                    state: winit::event::ElementState::Pressed,
                    button: winit::event::MouseButton::Left,
                    ..
                } => {
                    self.handle_dialog_click(event_loop);
                }
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "cursor position from winit is f64 but fits in f32"
                )]
                WindowEvent::CursorMoved { position, .. } => {
                    self.last_cursor_pos = Some((position.x as f32, position.y as f32));
                    self.handle_dialog_hover();
                }
                _ => {}
            }
            return;
        }

        // When the update dialog is active, intercept input events and route
        // them to the update dialog instead of the terminal / layout handlers.
        if self.update_dialog.is_some() {
            match event {
                WindowEvent::CloseRequested => {
                    self.handle_close_requested(event_loop);
                }
                WindowEvent::RedrawRequested => self.handle_redraw(),
                WindowEvent::Resized(size) => {
                    self.handle_resize(size);
                    self.mark_geometry_dirty();
                }
                WindowEvent::Moved(_) => self.mark_geometry_dirty(),
                WindowEvent::ModifiersChanged(new_mods) => {
                    self.modifiers = new_mods.state();
                }
                WindowEvent::KeyboardInput { event: ref key_event, .. } => {
                    self.handle_update_dialog_keyboard(key_event);
                }
                WindowEvent::MouseInput {
                    state: winit::event::ElementState::Pressed,
                    button: winit::event::MouseButton::Left,
                    ..
                } => {
                    self.handle_update_dialog_click();
                }
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "cursor position from winit is f64 but fits in f32"
                )]
                WindowEvent::CursorMoved { position, .. } => {
                    self.last_cursor_pos = Some((position.x as f32, position.y as f32));
                    self.handle_update_dialog_hover();
                }
                _ => {}
            }
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                self.handle_close_requested(event_loop);
            }
            WindowEvent::RedrawRequested => self.handle_redraw(),
            WindowEvent::Resized(size) => {
                self.handle_resize(size);
                self.mark_geometry_dirty();
            }
            WindowEvent::Moved(_) => self.mark_geometry_dirty(),
            WindowEvent::ModifiersChanged(new_mods) => {
                self.modifiers = new_mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_keyboard(&event),
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(state, button);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_mouse_wheel(delta);
            }
            WindowEvent::CursorMoved { position, .. } => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "cursor position from winit is f64 but fits in f32"
                )]
                {
                    self.last_cursor_pos = Some((position.x as f32, position.y as f32));
                }
                self.handle_cursor_moved();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

impl App {
    /// Initialise the window, wgpu surface/device/queue, renderer, layout,
    /// and IPC thread.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn init_gpu_and_terminal(&mut self, event_loop: &ActiveEventLoop) -> Result<(), InitError> {
        let mut attrs = Window::default_attributes().with_title("Scribe");
        if self.window_transparent {
            attrs = attrs.with_transparent(true);
            tracing::info!(opacity = self.opacity, "window transparency enabled");
        }
        let window = Arc::new(event_loop.create_window(attrs).map_err(InitError::Window)?);

        // Restore saved window geometry (position, size, maximized state).
        if let Some(geom) = self.saved_geometry.take() {
            window_state::apply_window_geometry(event_loop, &window, &geom);
        }

        let surface =
            self.wgpu_instance.create_surface(Arc::clone(&window)).map_err(InitError::Surface)?;

        let (device, queue, surface_config) = configure_device_and_surface(
            &self.wgpu_instance,
            &surface,
            &window,
            self.window_transparent,
        )?;

        let size = window.inner_size();
        let font_params = scribe_renderer::atlas::FontParams {
            family: self.config.appearance.font.clone(),
            size: self.config.appearance.font_size,
            weight: self.config.appearance.font_weight,
            weight_bold: self.config.appearance.font_weight_bold,
            ligatures: self.config.appearance.ligatures,
            line_padding: self.config.appearance.line_padding,
        };
        let mut renderer = TerminalRenderer::new(
            &device,
            &queue,
            surface_config.format,
            &font_params,
            (size.width, size.height),
        );

        renderer.set_theme(&self.theme);

        // Start IPC thread (proxy was created before run_app).
        let proxy = self.proxy.take().ok_or(InitError::ProxyConsumed)?;
        let cmd_tx = ipc_client::start_ipc_thread(proxy, self.window_id);

        // `ListSessions` is deferred to the first splash frame — see
        // `handle_redraw`.  This guarantees the splash is visible before
        // session content arrives, avoiding a flash of restored content.

        let splash = match splash::SplashRenderer::new(
            &device,
            &queue,
            surface_config.format,
            (size.width, size.height),
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "splash screen unavailable; skipping");
                None
            }
        };

        self.gpu = Some(GpuContext { surface, device, queue, surface_config, renderer, splash });
        self.cmd_tx = Some(cmd_tx);
        self.window = Some(window);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

impl App {
    /// Feed PTY output bytes into the correct pane, then request a redraw.
    /// Apply a screen snapshot to a pane by converting it to ANSI escape
    /// sequences and feeding them through the normal VTE pipeline.
    /// This restores visible terminal content on reconnect.
    fn handle_screen_snapshot(
        &mut self,
        session_id: SessionId,
        snapshot: &scribe_common::screen::ScreenSnapshot,
    ) {
        if snapshot.cols == 0 || snapshot.rows == 0 {
            tracing::warn!(%session_id, "snapshot has zero dimensions, skipping");
            return;
        }

        let non_empty = snapshot.cells.iter().filter(|c| c.c != ' ' && c.c != '\0').count();
        let first_char = snapshot.cells.iter().find(|c| c.c != ' ' && c.c != '\0');
        tracing::info!(
            %session_id,
            cols = snapshot.cols,
            rows = snapshot.rows,
            cells = snapshot.cells.len(),
            non_empty,
            first_char = ?first_char.map(|c| c.c),
            scrollback_rows = snapshot.scrollback_rows,
            "applying screen snapshot"
        );

        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else {
            tracing::warn!(%session_id, "snapshot: no pane for session");
            return;
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            tracing::warn!(%session_id, "snapshot: pane not found");
            return;
        };

        // The snapshot was captured with the server's (old) term dimensions
        // which may differ from this pane's current grid.  If the snapshot
        // has more columns than the pane, the ANSI output would wrap lines
        // and misalign all content.  Fix: temporarily resize the pane's term
        // to match the snapshot, feed the ANSI, then resize back so
        // alacritty_terminal reflows to the actual pane dimensions.
        let pane_grid = pane.grid;
        let dims_match = pane_grid.cols == snapshot.cols && pane_grid.rows == snapshot.rows;

        if !dims_match {
            tracing::info!(
                snap_cols = snapshot.cols,
                snap_rows = snapshot.rows,
                pane_cols = pane_grid.cols,
                pane_rows = pane_grid.rows,
                "snapshot dimensions differ from pane — resizing term temporarily"
            );
            pane.resize_term_only(snapshot.cols, snapshot.rows);
        }

        let ansi = snapshot_to_ansi(snapshot);
        tracing::info!(ansi_len = ansi.len(), "feeding snapshot ANSI to pane");
        pane.feed_output(&ansi);

        if !dims_match {
            pane.resize_term_only(pane_grid.cols, pane_grid.rows);
        }

        // Mark content as ready so the splash can be dismissed once it has
        // been visible for MIN_SPLASH_DURATION.  The actual dismissal happens
        // in `handle_redraw` to avoid submitting the terminal-content frame
        // before the compositor has presented the splash frame.
        if self.splash_active {
            self.splash_content_ready = true;
        }

        self.request_redraw();
    }

    fn handle_pty_output(&mut self, session_id: SessionId, bytes: &[u8]) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        pane.feed_output(bytes);
        // Invalidate the URL cache so it re-scans on next hover check.
        if let Some(cache) = self.url_caches.get_mut(&pane_id) {
            cache.mark_dirty();
        }

        // Mark content as ready (same deferred-dismiss as screen snapshots).
        if self.splash_active {
            self.splash_content_ready = true;
        }

        self.request_redraw();
    }

    /// Send `ListSessions` once after the first splash frame renders.
    ///
    /// On a local Unix socket, the full IPC round-trip (`ListSessions` →
    /// `SessionList` → `AttachSessions` → `ScreenSnapshot`) completes in under
    /// 1 ms, while the compositor's first frame callback takes ~16 ms.
    /// Deferring this send until after the splash is on-screen prevents the
    /// session content from arriving before the splash has been displayed.
    fn send_deferred_list_sessions(&mut self) {
        if !self.splash_needs_list_sessions {
            return;
        }
        self.splash_needs_list_sessions = false;
        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::ListSessions);
        }
    }

    /// Handle `SessionList` response from the server.
    ///
    /// If the server has existing sessions, reattach to them — restoring one
    /// tab per session in the correct workspace.  Multiple server-side
    /// workspaces are reconstructed by splitting the window layout.
    /// If no sessions exist, fall back to creating a fresh session.
    fn handle_session_list(
        &mut self,
        sessions: &[scribe_common::protocol::SessionInfo],
        workspace_tree: Option<&scribe_common::protocol::WorkspaceTreeNode>,
    ) {
        let Some(tx) = self.cmd_tx.clone() else { return };

        self.server_connected = true;

        // Reset per-reconnect state so that a second reconnect (if ever
        // supported in-process) does not carry stale flags.
        self.received_workspace_tree = false;

        if sessions.is_empty() {
            self.create_initial_session();
            return;
        }

        tracing::info!(count = sessions.len(), "reattaching to existing sessions");

        // SessionId is Copy — collect independently for each command instead of
        // cloning the Vec.
        let attach_ids: Vec<SessionId> = sessions.iter().map(|s| s.session_id).collect();
        send_command(&tx, ClientCommand::AttachSessions { session_ids: attach_ids });

        // Build a metadata lookup so panes can be initialised with the
        // last-known title and CWD instead of defaulting to "shell".
        let metadata: HashMap<SessionId, (Option<&str>, Option<&std::path::PathBuf>)> =
            sessions.iter().map(|s| (s.session_id, (s.title.as_deref(), s.cwd.as_ref()))).collect();

        // -- Group sessions by workspace ------------------------------------
        let mut groups: HashMap<WorkspaceId, Vec<SessionId>> = HashMap::new();
        for info in sessions {
            groups.entry(info.workspace_id).or_default().push(info.session_id);
        }
        // Workspaces that actually have live sessions.
        let live_workspace_ids: HashSet<WorkspaceId> = groups.keys().copied().collect();

        // -- Reconstruct workspaces -----------------------------------------
        if let Some(tree) = workspace_tree {
            self.reconstruct_from_tree(tree, &live_workspace_ids);
        } else {
            self.reconstruct_fallback(sessions);
        }

        // Use the layout's own leaf order to determine workspace iteration.
        // When a tree was provided, this preserves the exact spatial order;
        // when falling back, it matches the construction order.
        let workspace_order = self.window_layout.workspace_ids_in_order();

        // -- Add tabs and create panes --------------------------------------
        let Some(&first_ws) = workspace_order.first() else { return };
        let Some(first_sessions) = groups.get(&first_ws) else { return };
        let Some(&first_sid) = first_sessions.first() else { return };

        let Some(first_pane_id) = self.window_layout.add_tab(first_ws, first_sid) else {
            return;
        };
        let Some((_geo_id, pane_rect, grid)) = self.first_pane_geometry() else { return };

        let mut first_pane =
            Pane::new(pane_rect, grid, first_sid, first_ws, PaneEdges::all_external());
        apply_session_metadata(&mut first_pane, &metadata);
        self.panes.insert(first_pane_id, first_pane);
        self.url_caches.insert(first_pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(first_sid, first_pane_id);

        // Collect remaining (workspace, session) pairs across all workspaces.
        let remaining: Vec<(WorkspaceId, SessionId)> = workspace_order
            .iter()
            .flat_map(|&ws_id| {
                let skip = usize::from(ws_id == first_ws);
                groups
                    .get(&ws_id)
                    .into_iter()
                    .flat_map(move |sids| sids.iter().skip(skip).map(move |&sid| (ws_id, sid)))
            })
            .collect();

        for (ws_id, sid) in remaining {
            let Some(pane_id) = self.window_layout.add_tab(ws_id, sid) else {
                continue;
            };
            let mut pane = Pane::new(pane_rect, grid, sid, ws_id, PaneEdges::all_external());
            apply_session_metadata(&mut pane, &metadata);
            self.panes.insert(pane_id, pane);
            self.url_caches.insert(pane_id, url_detect::PaneUrlCache::new());
            self.session_to_pane.insert(sid, pane_id);
        }

        // Subscribe to output from all restored sessions.
        let subscribe_ids: Vec<SessionId> = sessions.iter().map(|s| s.session_id).collect();
        send_command(&tx, ClientCommand::Subscribe { session_ids: subscribe_ids });

        // Recompute pane geometry for each workspace and send the correct
        // grid dimensions to the server.
        self.resize_all_workspace_panes();

        // Splash stays active until the first ScreenSnapshot or PtyOutput
        // arrives, giving a brief visual transition even on reconnect.
        self.request_redraw();
    }

    /// Reconstruct the workspace layout from a server-provided tree.
    ///
    /// Prunes any tree leaves whose workspace has no live sessions (stale
    /// entries from a previous client that closed workspaces after its last
    /// tree report).
    fn reconstruct_from_tree(
        &mut self,
        tree: &scribe_common::protocol::WorkspaceTreeNode,
        live_ids: &HashSet<WorkspaceId>,
    ) {
        self.window_layout = workspace_layout::WindowLayout::from_tree(tree);
        self.received_workspace_tree = true;

        // Prune stale leaves.
        let tree_ids = self.window_layout.workspace_ids_in_order();
        for id in tree_ids {
            if !live_ids.contains(&id) {
                self.window_layout.remove_workspace(id);
            }
        }

        tracing::info!("reconstructed workspace layout from server tree");
    }

    /// Fallback reconstruction for old servers that don't send a workspace
    /// tree.  Builds a linear chain and relies on `WorkspaceInfo` direction
    /// patches to fix what they can.
    fn reconstruct_fallback(&mut self, sessions: &[scribe_common::protocol::SessionInfo]) {
        let mut workspace_order: Vec<WorkspaceId> = Vec::new();
        for info in sessions {
            if !workspace_order.contains(&info.workspace_id) {
                workspace_order.push(info.workspace_id);
            }
        }
        let default_ws = self.window_layout.focused_workspace_id();
        if let Some(&first_ws) = workspace_order.first() {
            self.window_layout.set_workspace_id(default_ws, first_ws);
        }
        for &ws_id in workspace_order.get(1..).unwrap_or_default() {
            self.window_layout.split_workspace_with_id(
                layout::SplitDirection::Vertical,
                None,
                ws_id,
            );
        }
    }

    /// Create the initial session + pane for a fresh start (no existing sessions).
    fn create_initial_session(&mut self) {
        let Some(tx) = &self.cmd_tx else { return };

        let workspace_id = self.window_layout.focused_workspace_id();
        let session_id = SessionId::new();

        let Some(pane_id) = self.window_layout.add_tab(workspace_id, session_id) else { return };

        let Some((_first_id, pane_rect, grid)) = self.first_pane_geometry() else { return };
        let pane = Pane::new(pane_rect, grid, session_id, workspace_id, PaneEdges::all_external());

        send_command(
            tx,
            ClientCommand::CreateSession { workspace_id, split_direction: None, cwd: None },
        );
        self.panes.insert(pane_id, pane);
        self.url_caches.insert(pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);
        send_resize(tx, session_id, grid.cols, grid.rows);

        // Seed the server with the initial (single-leaf) tree.
        self.report_workspace_tree();
    }

    /// Compute the single-row tab bar height from cell height + configured padding.
    fn effective_tab_bar_height(&self) -> f32 {
        let cell_h = self.gpu.as_ref().map_or(20.0, |gpu| gpu.renderer.cell_size().height);
        cell_h + self.config.appearance.tab_bar_padding
    }

    /// Compute the tab bar height for a specific workspace, accounting for
    /// multi-row stacking based on tab count and workspace width.
    #[allow(
        clippy::cast_precision_loss,
        reason = "workspace name length is a small positive integer fitting in f32"
    )]
    fn tab_bar_height_for(&self, workspace_id: WorkspaceId, ws_rect: Rect) -> f32 {
        let (cell_w, cell_h) = self.gpu.as_ref().map_or((8.0, 20.0), |g| {
            let c = g.renderer.cell_size();
            (c.width, c.height)
        });
        let row_h = cell_h + self.config.appearance.tab_bar_padding;
        let tab_count =
            self.window_layout.find_workspace(workspace_id).map_or(1, |ws| ws.tabs.len().max(1));
        let badge_cols = tab_bar::badge_columns(
            self.window_layout.find_workspace(workspace_id).and_then(|ws| ws.name.as_deref()),
            self.window_layout.workspace_count() > 1,
        );
        tab_bar::compute_tab_bar_height(
            tab_count,
            ws_rect.width,
            self.config.appearance.tab_width,
            cell_w,
            row_h,
            badge_cols,
        )
    }

    /// Compute the tab bar height for the currently focused workspace.
    ///
    /// Used by scrollbar and selection hit-testing where only the focused
    /// pane is relevant.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn focused_workspace_tab_bar_height(&self) -> f32 {
        let Some(gpu) = &self.gpu else { return self.effective_tab_bar_height() };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let ws_id = self.window_layout.focused_workspace_id();
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == ws_id).map_or(ws_viewport, |(_, r)| *r);
        self.tab_bar_height_for(ws_id, ws_rect)
    }

    /// Compute the pane ID, rect, and grid size for the first pane of the
    /// active tab. Returns `None` if GPU or layout state is unavailable.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn first_pane_geometry(&self) -> Option<(PaneId, Rect, GridSize)> {
        let gpu = self.gpu.as_ref()?;
        let window = self.window.as_ref()?;
        let size = window.inner_size();
        #[allow(
            clippy::cast_precision_loss,
            reason = "viewport dimensions are small enough to fit in f32"
        )]
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: size.width as f32,
            height: (size.height as f32 - status_bar::STATUS_BAR_HEIGHT).max(1.0),
        };
        let cell = gpu.renderer.cell_size();

        let ws_rects = self.window_layout.compute_workspace_rects(viewport);
        let &(first_ws_id, ws_rect) =
            ws_rects.first().map_or(&(self.window_layout.focused_workspace_id(), viewport), |p| p);

        let tab = self.window_layout.active_tab()?;
        let pane_rects = tab.pane_layout.compute_rects(ws_rect);
        let &(pane_id, pane_rect, pane_edges) = pane_rects.first()?;
        let tab_bar_h = self.tab_bar_height_for(first_ws_id, ws_rect);
        let grid = pane::compute_pane_grid(
            pane_rect,
            cell.width,
            cell.height,
            tab_bar_h,
            &pane::effective_padding(&self.config.appearance.content_padding, pane_edges),
        );
        Some((pane_id, pane_rect, grid))
    }

    /// Handle server confirming session creation.
    ///
    /// Pops the oldest pending (temporary) session ID, rebinds the pane and
    /// tab state to the real server-assigned session ID, and subscribes for
    /// PTY output.
    fn handle_session_created(&mut self, session_id: SessionId) {
        tracing::info!(session = %session_id, "session created");

        let Some(old_session_id) = self.pending_sessions.pop_front() else {
            tracing::warn!("SessionCreated with no pending session");
            return;
        };

        // Rebind the pane.
        if let Some(pane_id) = self.session_to_pane.remove(&old_session_id) {
            self.session_to_pane.insert(session_id, pane_id);
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.session_id = session_id;
            }
        }

        // Update the workspace tab state so it references the real session.
        self.window_layout.update_tab_session(old_session_id, session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::Subscribe { session_ids: vec![session_id] });
        }
    }

    /// Handle session exit.
    fn handle_session_exited(&mut self, session_id: SessionId) {
        tracing::info!(session = %session_id, "session exited");
        self.ai_tracker.remove(session_id);

        let Some(pane_id) = self.session_to_pane.remove(&session_id) else { return };

        // Find which workspace owns this session and remove its tab.
        let ws_id = self.window_layout.workspace_for_session(session_id);

        // Check the pane's own tab (not the active tab) for the pane count.
        // If focus is on a different tab, active_tab() would check the wrong layout.
        let can_close = ws_id
            .and_then(|wid| self.window_layout.find_workspace(wid))
            .and_then(|ws| ws.tabs.iter().find(|t| t.pane_layout.all_pane_ids().contains(&pane_id)))
            .is_some_and(|tab| tab.pane_layout.all_pane_ids().len() > 1);

        if !can_close {
            // Only one pane in the exiting session's tab; remove the tab from
            // the workspace.
            self.remove_tab_and_cleanup_workspace(ws_id, session_id);
            self.panes.remove(&pane_id);
            self.url_caches.remove(&pane_id);
            self.request_redraw();
            return;
        }

        self.panes.remove(&pane_id);
        self.url_caches.remove(&pane_id);

        // Close the pane in the tab that owns it, not necessarily the active tab.
        self.close_exited_pane_in_tab(ws_id, pane_id);

        self.resize_after_layout_change();
        self.request_redraw();
    }

    /// Remove a pane from whichever tab in `ws_id` owns it, updating
    /// `focused_pane` if necessary.  A no-op when `ws_id` is `None` or the
    /// pane cannot be found.
    fn close_exited_pane_in_tab(&mut self, ws_id: Option<WorkspaceId>, pane_id: PaneId) {
        let Some(wid) = ws_id else { return };
        let Some(ws) = self.window_layout.find_workspace_mut(wid) else { return };
        let Some(tab) =
            ws.tabs.iter_mut().find(|t| t.pane_layout.all_pane_ids().contains(&pane_id))
        else {
            return;
        };
        if tab.pane_layout.close_pane(pane_id) && tab.focused_pane == pane_id {
            tab.focused_pane = tab.pane_layout.next_pane(pane_id);
        }
    }

    /// Remove a session's tab and clean up the workspace if it becomes empty.
    fn remove_tab_and_cleanup_workspace(
        &mut self,
        ws_id: Option<WorkspaceId>,
        session_id: SessionId,
    ) {
        let Some(wid) = ws_id else { return };
        self.window_layout.remove_tab(wid, session_id);

        // If the workspace is now empty, remove it from the layout tree
        // so it doesn't linger as a blank region.
        let empty = self.window_layout.is_workspace_empty(wid);
        if empty && self.window_layout.remove_workspace(wid) {
            self.resize_all_workspace_panes();
            self.report_workspace_tree();
        }
    }

    /// Handle AI state change from server.
    fn handle_ai_state_changed(
        &mut self,
        session_id: SessionId,
        ai_state: scribe_common::ai_state::AiProcessState,
    ) {
        if !self.config.terminal.claude_code_integration {
            return;
        }
        self.ai_tracker.update(session_id, ai_state);
        tracing::debug!(session = %session_id, "AI state updated");

        if self.ai_tracker.needs_animation() && !self.animation_running {
            self.start_animation_timer();
        }

        self.request_redraw();
    }

    /// Handle animation timer tick.
    fn handle_animation_tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.ai_tracker.tick(dt);

        // Tick scrollbar fade for all panes.
        let mut scrollbar_animating = false;
        for pane in self.panes.values_mut() {
            let display_offset = pane.term.grid().display_offset();
            if pane.scrollbar_state.tick_fade(display_offset) {
                scrollbar_animating = true;
            }
        }

        // Tick tab slide offsets — exponential decay toward zero.
        let dragging_tab_idx = self.tab_drag.as_ref().filter(|d| d.dragging).map(|d| d.tab_index);
        let mut tab_animating = false;
        for (i, offset) in self.tab_drag_offsets.iter_mut().enumerate() {
            if Some(i) == dragging_tab_idx {
                // Dragged tab is cursor-driven, not decayed.
                continue;
            }
            if *offset == 0.0 {
                continue;
            }
            *offset *= (1.0 - 10.0 * dt).max(0.0);
            if offset.abs() < 0.5 {
                *offset = 0.0;
            }
            if *offset != 0.0 {
                tab_animating = true;
            }
        }
        if !tab_animating && self.tab_drag.is_none() {
            self.tab_drag_offsets.clear();
        }

        let drag_active = self.tab_drag.as_ref().is_some_and(|d| d.dragging);
        if !self.ai_tracker.needs_animation()
            && !scrollbar_animating
            && !tab_animating
            && !drag_active
        {
            self.animation_running = false;
            self.animation_stop.store(false, Ordering::Relaxed);
        }

        self.request_redraw();
    }

    /// Handle CWD change for a session — store on the pane.
    fn handle_cwd_changed(&mut self, session_id: SessionId, cwd: std::path::PathBuf) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, ?cwd, "CWD changed");
        pane.cwd = Some(cwd);
    }

    /// Handle title change for a session — update pane title.
    fn handle_title_changed(&mut self, session_id: SessionId, title: &str) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, %title, "title changed");
        title.clone_into(&mut pane.title);
    }

    /// Handle git branch change for a session — store on the pane.
    fn handle_git_branch(&mut self, session_id: SessionId, branch: Option<String>) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        tracing::debug!(%session_id, ?branch, "git branch updated");
        pane.git_branch = branch;
    }

    /// Handle full workspace info from server — update name, accent color,
    /// and (on reconnect) the split direction of the parent split node.
    fn handle_workspace_info(
        &mut self,
        workspace_id: WorkspaceId,
        name: Option<String>,
        accent_color: &str,
        split_direction: Option<scribe_common::protocol::LayoutDirection>,
    ) {
        tracing::debug!(%workspace_id, ?name, %accent_color, ?split_direction, "workspace info received");
        if let Some(ws) = self.window_layout.find_workspace_mut(workspace_id) {
            ws.name = name;
            if let Some(color) = parse_hex_color(accent_color) {
                ws.accent_color = color;
            }
        }

        // Apply the persisted split direction so reconnected workspace
        // layouts match the original arrangement.  Skip when a full
        // workspace tree was received — the tree already has the correct
        // directions and ratios.
        if !self.received_workspace_tree {
            if let Some(dir) = split_direction {
                self.window_layout
                    .update_split_direction_for(workspace_id, from_layout_direction(dir));
            }
        }
    }

    /// Handle workspace auto-naming — update the workspace slot and pane names.
    fn handle_workspace_named(&mut self, workspace_id: WorkspaceId, name: &str) {
        tracing::debug!(%workspace_id, %name, "workspace named");

        // Update the workspace slot name.
        if let Some(ws) = self.window_layout.find_workspace_mut(workspace_id) {
            ws.name = Some(name.to_owned());
        }

        for pane in self.panes.values_mut() {
            if pane.workspace_id == workspace_id {
                pane.workspace_name = Some(name.to_owned());
            }
        }
    }

    /// Reload config from disk and apply changed settings.
    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "sequential comparison of all hot-reloadable config fields in one method"
    )]
    fn handle_config_changed(&mut self) {
        let new_config = match scribe_common::config::load_config() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config reload failed: {e}");
                return;
            }
        };

        // Keep a reference to the old config for comparisons before we move
        // new_config into self.config at the end of the method.
        let old = &self.config;

        // -- Theme (conditional) --
        if old.appearance.theme != new_config.appearance.theme {
            let new_theme = resolve_theme(&new_config);
            if let Some(gpu) = &mut self.gpu {
                gpu.renderer.set_theme(&new_theme);
            }
            self.theme = new_theme;
            // Theme affects cell colors — all pane caches are stale.
            for pane in self.panes.values_mut() {
                pane.content_dirty = true;
            }
        }

        // -- Font params --
        let font_changed = old.appearance.font != new_config.appearance.font
            || (old.appearance.font_size - new_config.appearance.font_size).abs() > f32::EPSILON
            || old.appearance.font_weight != new_config.appearance.font_weight
            || old.appearance.font_weight_bold != new_config.appearance.font_weight_bold
            || old.appearance.ligatures != new_config.appearance.ligatures
            || old.appearance.line_padding != new_config.appearance.line_padding;

        if font_changed {
            if let Some(gpu) = &mut self.gpu {
                let params = scribe_renderer::atlas::FontParams {
                    family: new_config.appearance.font.clone(),
                    size: new_config.appearance.font_size,
                    weight: new_config.appearance.font_weight,
                    weight_bold: new_config.appearance.font_weight_bold,
                    ligatures: new_config.appearance.ligatures,
                    line_padding: new_config.appearance.line_padding,
                };
                gpu.renderer.rebuild_atlas(&gpu.device, &gpu.queue, &params);
            }
        }

        // -- Cursor shape --
        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.set_cursor_shape(new_config.appearance.cursor_shape);
        }

        // -- Cursor blink --
        self.cursor_blink_enabled = new_config.appearance.cursor_blink;
        if !self.cursor_blink_enabled {
            self.cursor_visible = true;
        }

        // -- Opacity --
        if (old.appearance.opacity - new_config.appearance.opacity).abs() > f32::EPSILON {
            if !self.window_transparent && new_config.appearance.opacity < 1.0 {
                tracing::warn!(
                    "opacity < 1.0 requires restart to take effect \
                     (window was created without transparency)"
                );
            } else {
                self.opacity = new_config.appearance.opacity;
            }
        }

        // -- Keybindings --
        self.bindings = input::Bindings::parse(&new_config.keybindings);

        // -- Tab bar height / layout --
        let tab_bar_changed =
            (old.appearance.tab_bar_padding - new_config.appearance.tab_bar_padding).abs()
                > f32::EPSILON
                || old.appearance.tab_width != new_config.appearance.tab_width;

        // -- Content padding --
        let old_pad = &old.appearance.content_padding;
        let new_pad = &new_config.appearance.content_padding;
        let padding_changed = (old_pad.top - new_pad.top).abs() > f32::EPSILON
            || (old_pad.right - new_pad.right).abs() > f32::EPSILON
            || (old_pad.bottom - new_pad.bottom).abs() > f32::EPSILON
            || (old_pad.left - new_pad.left).abs() > f32::EPSILON;

        self.ai_tracker.reconfigure(new_config.terminal.claude_states.clone());
        self.config = new_config;

        // Notify the server so it can apply config changes that affect
        // server-side state (e.g. scrollback_lines on live sessions).
        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::ConfigReloaded);
        }

        // When tab bar height, font, or content padding changes, the grid row/col
        // count changes and every pane must be resized so the server PTY gets the
        // correct dimensions.  Use the all-workspaces variant so that visible but
        // unfocused workspaces also pick up the new cell metrics.
        if font_changed || tab_bar_changed || padding_changed {
            self.resize_all_workspace_panes();
        }

        tracing::info!("config hot-reloaded");
        self.request_redraw();
    }

    /// Detect and correct a stale `surface_config` / renderer viewport,
    /// **and** stale pane grids.
    ///
    /// On some compositors `set_maximized(true)` at startup applies the
    /// maximized size to the window without delivering a `Resized` event,
    /// leaving the wgpu surface and shader viewport at the pre-maximize
    /// dimensions.  This check syncs them before the frame is painted.
    ///
    /// Additionally, pane grids can become stale when the surface was
    /// resized before panes existed (during the splash screen) or when
    /// `set_maximized(true)` is applied asynchronously by the compositor
    /// after panes have already been created at the pre-maximize size.
    /// A second pass detects stale grids and triggers a resize.
    fn sync_surface_to_window(&mut self) {
        let Some(window) = &self.window else { return };
        let actual = window.inner_size();
        if actual.width == 0 || actual.height == 0 {
            return;
        }
        let mismatched = self.gpu.as_ref().is_some_and(|gpu| {
            actual.width != gpu.surface_config.width || actual.height != gpu.surface_config.height
        });
        if mismatched {
            tracing::info!(
                actual_w = actual.width,
                actual_h = actual.height,
                "surface config out of sync with window — forcing resize"
            );
            self.handle_resize(actual);
            // handle_resize already calls resize_all_workspace_panes,
            // so pane grids are updated — no need for the staleness check.
            return;
        }

        // Surface matches window, but pane grids may still be stale.
        // This happens when the Resized event (or a previous sync) updated
        // surface_config while no panes existed yet — resize_all_workspace_panes
        // was a no-op.  Panes created later inherit the wrong grid.
        self.sync_pane_grids_if_stale();
    }

    /// Check whether any pane's grid dimensions are out of sync with what
    /// the current `surface_config` layout geometry would produce, and fix
    /// them if so.
    ///
    /// Computes expected grids directly from `surface_config` (not from
    /// `pane.rect`, which may itself be stale from the previous frame).
    /// Typically iterates over 1–4 panes; the actual resize only triggers
    /// when a mismatch is found, which normally happens at most once during
    /// startup.
    fn sync_pane_grids_if_stale(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        let Some(gpu) = &self.gpu else { return };
        let cell = gpu.renderer.cell_size();
        let ws_viewport = workspace_viewport(&gpu.surface_config);

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let expected_rects = collect_expected_pane_rects(&self.window_layout, &ws_rects);

        // Build a ws_id → tab_bar_height map for staleness checking.
        let ws_heights: std::collections::HashMap<WorkspaceId, f32> = ws_rects
            .iter()
            .map(|(ws_id, ws_rect)| (*ws_id, self.tab_bar_height_for(*ws_id, *ws_rect)))
            .collect();

        let any_stale = expected_rects.iter().any(|(pid, rect, edges)| {
            self.panes.get(pid).is_some_and(|pane| {
                let tbh = ws_heights
                    .get(&pane.workspace_id)
                    .copied()
                    .unwrap_or_else(|| self.effective_tab_bar_height());
                let expected = pane::compute_pane_grid(
                    *rect,
                    cell.width,
                    cell.height,
                    tbh,
                    &pane::effective_padding(&self.config.appearance.content_padding, *edges),
                );
                pane.grid.cols != expected.cols || pane.grid.rows != expected.rows
            })
        });

        if any_stale {
            tracing::info!("pane grids out of sync with layout — forcing pane resize");
            self.resize_all_workspace_panes();
        }
    }

    /// Render one frame: splash while waiting for PTY output, terminal after.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "render loop collects chrome + content + dividers + AI borders sequentially"
    )]
    #[allow(
        clippy::cognitive_complexity,
        reason = "temporary diagnostic tracing for splash bug — remove after fix"
    )]
    fn handle_redraw(&mut self) {
        self.sync_surface_to_window();

        let Some(gpu) = &mut self.gpu else { return };

        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(tex)
            | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => tex,
            other => {
                tracing::warn!(?other, "failed to acquire surface texture");
                return;
            }
        };

        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        // -- Splash dismiss check ------------------------------------------------
        if self.splash_active && self.splash_content_ready {
            let elapsed_ok =
                self.splash_first_rendered.is_some_and(|t| t.elapsed() >= MIN_SPLASH_DURATION);
            if elapsed_ok {
                self.splash_active = false;
                gpu.splash = None;
            }
        }

        // -- Splash render -------------------------------------------------------
        if self.splash_active {
            if let Some(splash) = &gpu.splash {
                let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("splash encoder"),
                });
                splash.render(&mut enc, &view);
                gpu.queue.submit(std::iter::once(enc.finish()));
                frame.present();
            }

            if self.splash_first_rendered.is_none() {
                self.splash_first_rendered = Some(Instant::now());
            }

            self.send_deferred_list_sessions();

            if self.splash_content_ready {
                self.request_redraw();
            }

            return;
        }

        let full_viewport = viewport_rect(&gpu.surface_config);
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let cell_size = (gpu.renderer.cell_size().width, gpu.renderer.cell_size().height);

        // Get pane rects and dividers from ALL workspaces' active tabs.
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let focused_ws_id = self.window_layout.focused_workspace_id();
        let multi_workspace = self.window_layout.workspace_count() > 1;
        let mut pane_rects: Vec<(PaneId, Rect)> = Vec::new();
        let mut dividers = Vec::new();
        let mut focused_pane = PaneId::from_raw(u32::MAX);
        let mut focus_split_direction = None;
        let mut ws_tab_bar_data: Vec<tab_bar::WorkspaceTabBarData> = Vec::new();

        // Linearise ANSI palette early — needed both for tab AI indicators and
        // pane border colours computed during the workspace loop.
        let linear_ansi = linearise_ansi_colors(&self.theme.ansi_colors);
        let ansi_colors = &linear_ansi;
        let ai_enabled = self.config.terminal.claude_code_integration;

        for (ws_id, ws_rect) in &ws_rects {
            let ws = self.window_layout.find_workspace(*ws_id);
            let Some(ws) = ws else { continue };
            let Some(tab) = ws.active_tab() else { continue };

            let rects_with_edges = tab.pane_layout.compute_rects(*ws_rect);
            pane_rects.extend(rects_with_edges.iter().map(|&(id, rect, _)| (id, rect)));
            dividers.extend(divider::collect_dividers(tab.pane_layout.root(), *ws_rect));
            if *ws_id == focused_ws_id {
                focused_pane = tab.focused_pane;
                focus_split_direction = tab.pane_layout.parent_split_direction(tab.focused_pane);
            }

            // Collect tab data for this workspace's tab bar.
            let tabs: Vec<tab_bar::TabData> = ws
                .tabs
                .iter()
                .enumerate()
                .map(|(i, ts)| {
                    let title = self
                        .session_to_pane
                        .get(&ts.session_id)
                        .and_then(|pid| self.panes.get(pid))
                        .map_or_else(|| format!("tab {}", i + 1), |p| p.title.clone());
                    let ai_indicator = ai_enabled
                        .then(|| self.ai_tracker.tab_indicator_color(ts.session_id, ansi_colors))
                        .flatten();
                    tab_bar::TabData { title, is_active: i == ws.active_tab, ai_indicator }
                })
                .collect();

            let badge = if multi_workspace {
                let name = ws.name.clone().unwrap_or_else(|| String::from("workspace"));
                Some((name, scribe_renderer::srgb_to_linear_rgba(ws.accent_color)))
            } else {
                None
            };

            let has_multiple_panes = tab.pane_layout.all_pane_ids().len() > 1;

            // Compute per-workspace tab bar height (may be multi-row).
            let row_h = cell_size.1 + self.config.appearance.tab_bar_padding;
            let badge_cols_for_h = tab_bar::badge_columns(ws.name.as_deref(), multi_workspace);
            #[allow(
                clippy::cast_precision_loss,
                reason = "badge col count is a small positive integer fitting in f32"
            )]
            let ws_tab_bar_h = tab_bar::compute_tab_bar_height(
                ws.tabs.len(),
                ws_rect.width,
                self.config.appearance.tab_width,
                cell_size.0,
                row_h,
                badge_cols_for_h,
            );

            // Compute tabs_per_row to determine whether the active tab is on row 0.
            let cell_w = cell_size.0;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "width / cell_w yields a small positive value fitting in usize"
            )]
            let total_cols = if cell_w > 0.0 { (ws_rect.width / cell_w) as usize } else { 0 };
            // show_gear is always false in the render path, so gear_cols is 0.
            // equalize_cols matches build_tab_bar_text: 2 when has_multiple_panes.
            let gear_cols: usize = 0;
            let equalize_cols: usize = if has_multiple_panes { 2 } else { 0 };
            let tab_w = usize::from(self.config.appearance.tab_width).max(1);
            let available_for_tabs = total_cols
                .saturating_sub(badge_cols_for_h)
                .saturating_sub(gear_cols)
                .saturating_sub(equalize_cols);
            let tabs_per_row = (available_for_tabs / tab_w).max(1);
            let active_tab_pixel_range = compute_active_tab_pixel_range(
                ws_rect.x,
                ws.active_tab,
                self.config.appearance.tab_width,
                badge_cols_for_h,
                tabs_per_row,
                cell_w,
            );

            ws_tab_bar_data.push(tab_bar::WorkspaceTabBarData {
                ws_id: *ws_id,
                ws_rect: *ws_rect,
                tabs,
                badge,
                has_multiple_panes,
                tab_bar_height: ws_tab_bar_h,
                active_tab_pixel_range,
            });
        }

        // Collect workspace dividers (needs the full viewport, not per-workspace).
        let ws_dividers = self.window_layout.collect_workspace_dividers(ws_viewport);

        // Workspace-aggregated AI border colours: for each pane, find the
        // workspace it belongs to and pick the highest-priority AI state.
        let border_colors: HashMap<PaneId, [f32; 4]> = if ai_enabled {
            pane_rects
                .iter()
                .filter_map(|(pane_id, _)| {
                    let pane = self.panes.get(pane_id)?;
                    let ws_id = self.window_layout.workspace_for_session(pane.session_id)?;
                    let ws = self.window_layout.find_workspace(ws_id)?;
                    let session_ids: Vec<SessionId> =
                        ws.tabs.iter().map(|t| t.session_id).collect();
                    let color =
                        self.ai_tracker.workspace_border_color(&session_ids, ansi_colors)?;
                    Some((*pane_id, color))
                })
                .collect()
        } else {
            HashMap::new()
        };

        let tab_colors = tab_bar::TabBarColors::from(&self.theme.chrome);
        let sb_colors = status_bar::StatusBarColors::from_theme(&self.theme.chrome, ansi_colors);
        let divider_color = scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.divider);
        let accent_color = scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.accent);

        // Gather focused pane data for the window-level status bar.
        let focused_pane_cwd = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .and_then(|p| p.cwd.clone());
        let focused_pane_git = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .and_then(|p| p.git_branch.clone());
        let focused_ws_name = self.window_layout.focused_workspace().and_then(|ws| ws.name.clone());
        let session_count = self.panes.len();

        // Toggle cursor blink state before building instances.
        if self.cursor_blink_enabled && self.blink_timer.elapsed() >= BLINK_INTERVAL {
            self.cursor_visible = !self.cursor_visible;
            self.blink_timer = Instant::now();
        }
        let cursor_visible = self.cursor_visible;

        // Sync pane rects with the freshly-computed layout so that content,
        // scrollbars, and hit-testing all use the same authoritative geometry.
        // This prevents stale `pane.rect` values (from async window geometry
        // changes) from placing content at the wrong offset.
        for (pane_id, rect) in &pane_rects {
            if let Some(pane) = self.panes.get_mut(pane_id) {
                pane.rect = *rect;
            }
        }

        let scrollbar_width = self.config.appearance.scrollbar_width.clamp(2.0, 20.0);
        let scrollbar_color = self.config.appearance.scrollbar_color.as_ref().map_or(
            self.theme.chrome.scrollbar,
            |hex| {
                scribe_common::theme::hex_to_rgba(hex).map_or(
                    self.theme.chrome.scrollbar,
                    |mut c| {
                        c[3] = 0.4;
                        c
                    },
                )
            },
        );

        let indicator_h = self.config.terminal.indicator_height.clamp(1.0, 10.0);
        let frame_layout = FrameLayout {
            pane_rects: &pane_rects,
            dividers: &dividers,
            ws_dividers: &ws_dividers,
            ws_tab_bar_data: &ws_tab_bar_data,
            cell_size,
            focused_pane,
            focus_split_direction,
            padding: &self.config.appearance.content_padding,
        };
        let frame_style = FrameStyle {
            border_colors: &border_colors,
            tab_colors: &tab_colors,
            divider_color,
            accent_color,
            scrollbar_width,
            scrollbar_color,
            indicator_height: indicator_h,
        };
        let update_version = self.update_available.as_ref().map(|(v, _)| v.as_str());
        let frame_interaction = FrameInteraction {
            cursor_visible,
            tab_width: self.config.appearance.tab_width,
            active_selection: self.active_selection.as_ref(),
            hovered_tab_close: self.hovered_tab_close,
            tab_drag: self.tab_drag.as_ref(),
            tab_drag_offsets: &self.tab_drag_offsets,
            update_available: update_version,
            update_progress: self.update_progress.as_ref(),
        };
        let (mut all_instances, tab_hits, tab_close_hits, tab_eq_hits, tab_upd_hits) =
            build_all_instances(
                &mut gpu.renderer,
                &gpu.device,
                &gpu.queue,
                &mut self.panes,
                &frame_layout,
                &frame_style,
                &frame_interaction,
            );
        self.tab_hit_targets = tab_hits;
        self.tab_close_hit_targets = tab_close_hits;
        self.tab_bar_equalize_targets = tab_eq_hits;
        self.tab_bar_update_targets = tab_upd_hits;

        // URL underlines — rendered on top of terminal content, below tab bars.
        {
            let ws_tab_bar_heights: HashMap<WorkspaceId, f32> =
                ws_tab_bar_data.iter().map(|d| (d.ws_id, d.tab_bar_height)).collect();
            let fallback_tbh = ws_tab_bar_data.first().map_or(0.0, |d| d.tab_bar_height);
            apply_url_underlines(
                &mut all_instances,
                &mut self.url_caches,
                &self.panes,
                &pane_rects,
                &ws_tab_bar_heights,
                fallback_tbh,
                cell_size,
                self.hovered_url.as_ref(),
                &self.config.appearance.content_padding,
            );
        }

        // Window-level status bar spanning the full window width.
        {
            let time_str = current_time_str();
            self.sys_stats.maybe_refresh();
            let sb_data = status_bar::StatusBarData {
                connected: self.server_connected,
                show_equalize: multi_workspace,
                workspace_name: focused_ws_name.as_deref(),
                cwd: focused_pane_cwd.as_deref(),
                git_branch: focused_pane_git.as_deref(),
                session_count,
                hostname: &self.hostname,
                time: &time_str,
                sys_stats: Some(self.sys_stats.stats()),
                stats_config: Some(&self.config.terminal.status_bar_stats),
            };
            let mut resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            let sb_hits = status_bar::build_status_bar(
                &mut all_instances,
                full_viewport,
                cell_size,
                &sb_colors,
                &sb_data,
                &mut resolve_glyph,
            );
            self.status_bar_gear_rect = sb_hits.gear_rect;
            self.status_bar_equalize_rect = sb_hits.equalize_rect;
        }

        // Close dialog overlay (rendered on top of everything).
        if let Some(dialog) = &mut self.close_dialog {
            let mut resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            dialog.build_instances(
                &mut all_instances,
                full_viewport,
                cell_size,
                &self.theme.chrome,
                &mut resolve_glyph,
            );
        }

        // Update dialog overlay (rendered on top of everything, below close dialog).
        if let Some(dialog) = &mut self.update_dialog {
            let mut resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            dialog.build_instances(
                &mut all_instances,
                full_viewport,
                cell_size,
                &self.theme.chrome,
                &mut resolve_glyph,
            );
        }

        // Context menu overlay (rendered on top of close dialog).
        if let Some(menu) = &mut self.context_menu {
            let mut resolve_glyph =
                |ch: char| gpu.renderer.resolve_glyph(&gpu.device, &gpu.queue, ch);
            menu.build_instances(
                &mut all_instances,
                full_viewport,
                cell_size,
                &self.theme.chrome,
                &mut resolve_glyph,
            );
        }

        if self.opacity < 1.0 {
            apply_opacity_to_instances(&mut all_instances, self.opacity);
        }

        gpu.renderer.pipeline_mut().update_instances(&gpu.device, &gpu.queue, &all_instances);

        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("multi-pane encoder"),
        });

        #[allow(
            clippy::indexing_slicing,
            reason = "fixed-size [f32; 4] array, index 3 always valid"
        )]
        let clear_color = {
            let mut c = gpu.renderer.default_bg();
            c[3] *= self.opacity;
            c
        };
        gpu.renderer.pipeline_mut().render_with_clear(&mut encoder, &view, clear_color);
        gpu.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }

    /// Reconfigure the surface and renderer on window resize.
    fn handle_resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }

        let Some(gpu) = &mut self.gpu else { return };

        gpu.surface_config.width = size.width;
        gpu.surface_config.height = size.height;
        gpu.surface.configure(&gpu.device, &gpu.surface_config);

        // Resize the shared renderer's viewport and pipeline uniforms.
        // The returned grid size is not needed here — individual panes compute
        // their own grid dimensions from their rects below.
        let _ = gpu.renderer.resize(&gpu.queue, (size.width, size.height));

        // Keep the splash uniform in sync so the logo stays centred.
        if let Some(splash) = &mut gpu.splash {
            splash.update_viewport(&gpu.queue, (size.width, size.height));
        }

        self.resize_all_workspace_panes();
        self.request_redraw();
    }

    /// Translate a keyboard event and forward it to the correct handler.
    fn handle_keyboard(&mut self, event: &winit::event::KeyEvent) {
        // Dismiss context menu on any key press.
        if self.context_menu.is_some() && event.state == winit::event::ElementState::Pressed {
            use winit::keyboard::{Key, NamedKey};
            if event.logical_key == Key::Named(NamedKey::Escape) {
                self.context_menu = None;
                self.request_redraw();
                return;
            }
        }

        let Some(action) = input::translate_key_action(event, self.modifiers, &self.bindings)
        else {
            return;
        };

        // Reset cursor to visible on any keypress so it doesn't stay hidden mid-blink.
        self.cursor_visible = true;
        self.blink_timer = Instant::now();
        self.request_redraw();

        match action {
            KeyAction::Terminal(bytes) => self.handle_terminal_key(bytes),
            KeyAction::Layout(layout_action) => self.handle_layout_action(layout_action),
            KeyAction::OpenSettings => self.open_settings(),
            KeyAction::OpenFind => self.handle_open_find(),
        }
    }

    fn handle_terminal_key(&mut self, bytes: Vec<u8>) {
        let Some(tx) = self.cmd_tx.clone() else { return };
        let focused_pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
        let sid = pane.session_id;

        let scrolled_up = pane.term.grid().display_offset() > 0;
        if scrolled_up {
            pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            pane.scrollbar_state.on_scroll_action();
            pane.content_dirty = true;
        }
        if scrolled_up {
            self.ensure_animation_running();
        }

        if tx.send(ClientCommand::KeyInput { session_id: sid, data: bytes }).is_err() {
            tracing::warn!("IPC channel closed; keyboard input dropped");
        }

        // Clear "waiting for input / permission" indicators on real keystrokes.
        if self.config.terminal.claude_code_integration {
            self.ai_tracker.clear_attention_states(sid);
        }
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "flat match dispatch on LayoutAction variants; each arm is trivial"
    )]
    fn handle_layout_action(&mut self, action: LayoutAction) {
        match action {
            // Panes
            LayoutAction::SplitVertical => {
                self.handle_split(layout::SplitDirection::Horizontal);
            }
            LayoutAction::SplitHorizontal => {
                self.handle_split(layout::SplitDirection::Vertical);
            }
            LayoutAction::ClosePane => self.handle_close_pane(),
            LayoutAction::FocusNext => self.handle_focus_next(),
            LayoutAction::FocusLeft => {
                self.handle_focus_directional(layout::FocusDirection::Left);
            }
            LayoutAction::FocusRight => {
                self.handle_focus_directional(layout::FocusDirection::Right);
            }
            LayoutAction::FocusUp => {
                self.handle_focus_directional(layout::FocusDirection::Up);
            }
            LayoutAction::FocusDown => {
                self.handle_focus_directional(layout::FocusDirection::Down);
            }

            // Workspaces
            LayoutAction::WorkspaceSplitVertical => {
                self.handle_workspace_split(layout::SplitDirection::Horizontal);
            }
            LayoutAction::WorkspaceSplitHorizontal => {
                self.handle_workspace_split(layout::SplitDirection::Vertical);
            }
            LayoutAction::CycleWorkspaceFocus => {
                if self.window_layout.cycle_workspace_focus() {
                    self.request_redraw();
                }
            }
            LayoutAction::NewWindow => self.handle_new_window(),

            // Tabs
            LayoutAction::NewTab => self.handle_new_tab(),
            LayoutAction::CloseTab => self.handle_close_tab(),
            LayoutAction::NextTab => self.handle_next_tab(),
            LayoutAction::PrevTab => self.handle_prev_tab(),
            LayoutAction::SelectTab(idx) => self.handle_select_tab(idx),

            // Clipboard
            LayoutAction::CopySelection => self.perform_copy(),
            LayoutAction::PasteClipboard => self.perform_paste(),

            // Navigation
            LayoutAction::ScrollUp => self.handle_scroll_up(),
            LayoutAction::ScrollDown => self.handle_scroll_down(),
            LayoutAction::ScrollTop => self.handle_scroll_top(),
            LayoutAction::ScrollBottom => self.handle_scroll_bottom(),

            // View
            LayoutAction::ZoomIn => self.zoom_step(1),
            LayoutAction::ZoomOut => self.zoom_step(-1),
            LayoutAction::ZoomReset => self.zoom_reset(),
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn handle_split(&mut self, direction: layout::SplitDirection) {
        // Extract focused pane and its CWD before the split mutates layout.
        let focused = match self.window_layout.active_tab() {
            Some(active) => active.focused_pane,
            None => return,
        };
        let inherited_cwd = self.panes.get(&focused).and_then(|p| p.cwd.clone());
        let workspace_id = self.window_layout.focused_workspace_id();

        // Perform the split (mutable borrow).
        let new_pane_id = match self.window_layout.active_tab_mut() {
            Some(active) => match active.pane_layout.split_pane(focused, direction) {
                Some(id) => id,
                None => return,
            },
            None => return,
        };

        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let session_id = SessionId::new();
        let cell = gpu.renderer.cell_size();

        // Compute workspace rect.
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == workspace_id).map_or(ws_viewport, |(_, r)| *r);

        // Compute pane rects from the updated layout (immutable borrow).
        let rects = match self.window_layout.active_tab() {
            Some(active) => active.pane_layout.compute_rects(ws_rect),
            None => return,
        };

        let (new_rect, new_edges) = rects
            .iter()
            .find(|(id, _, _)| *id == new_pane_id)
            .map_or((ws_rect, PaneEdges::all_external()), |&(_, r, e)| (r, e));

        let tab_bar_h = self.tab_bar_height_for(workspace_id, ws_rect);
        let grid = pane::compute_pane_grid(
            new_rect,
            cell.width,
            cell.height,
            tab_bar_h,
            &pane::effective_padding(&self.config.appearance.content_padding, new_edges),
        );
        let pane = Pane::new(new_rect, grid, session_id, workspace_id, new_edges);

        self.panes.insert(new_pane_id, pane);
        self.url_caches.insert(new_pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(session_id, new_pane_id);
        self.pending_sessions.push_back(session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(
                tx,
                ClientCommand::CreateSession {
                    workspace_id,
                    split_direction: None,
                    cwd: inherited_cwd,
                },
            );
        }

        self.resize_all_panes_from_rects(&rects, &ws_rects);

        if let Some(active) = self.window_layout.active_tab_mut() {
            active.focused_pane = new_pane_id;
        }
        self.request_redraw();
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn handle_workspace_split(&mut self, direction: layout::SplitDirection) {
        let Some(gpu) = &self.gpu else { return };
        let accent = Some(self.theme.chrome.accent);
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let cell = gpu.renderer.cell_size();

        // Split the window layout tree, creating a new workspace region.
        let Some(new_workspace_id) = self.window_layout.split_workspace(direction, accent) else {
            return;
        };

        // Add an initial tab+pane to the new workspace.
        let session_id = SessionId::new();
        let Some(pane_id) = self.window_layout.add_tab(new_workspace_id, session_id) else {
            return;
        };

        // Compute workspace rects for the updated layout.
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect = ws_rects
            .iter()
            .find(|(wid, _)| *wid == new_workspace_id)
            .map_or(ws_viewport, |(_, r)| *r);

        let tab_bar_h = self.tab_bar_height_for(new_workspace_id, ws_rect);
        let grid = pane::compute_pane_grid(
            ws_rect,
            cell.width,
            cell.height,
            tab_bar_h,
            &self.config.appearance.content_padding,
        );
        let pane =
            Pane::new(ws_rect, grid, session_id, new_workspace_id, PaneEdges::all_external());

        self.panes.insert(pane_id, pane);
        self.url_caches.insert(pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(
                tx,
                ClientCommand::CreateSession {
                    workspace_id: new_workspace_id,
                    split_direction: Some(to_layout_direction(direction)),
                    cwd: None,
                },
            );
        }

        // Resize panes in ALL workspaces since the window was re-divided.
        self.resize_all_workspace_panes();

        // Report the updated tree to the server for persistence.
        self.report_workspace_tree();

        self.request_redraw();
    }

    fn handle_close_pane(&mut self) {
        // Extract focused pane and pane count (immutable borrow).
        let (pane_id, pane_count) = match self.window_layout.active_tab() {
            Some(active) => (active.focused_pane, active.pane_layout.all_pane_ids().len()),
            None => return,
        };

        if pane_count <= 1 {
            return;
        }

        // Close the pane in the layout (mutable borrow).
        let closed = match self.window_layout.active_tab_mut() {
            Some(active) => active.pane_layout.close_pane(pane_id),
            None => return,
        };

        if !closed {
            return;
        }

        if let Some(pane) = self.panes.remove(&pane_id) {
            self.session_to_pane.remove(&pane.session_id);
            self.url_caches.remove(&pane_id);
            if let Some(tx) = &self.cmd_tx {
                send_command(tx, ClientCommand::CloseSession { session_id: pane.session_id });
            }
        }

        // Update focused pane (mutable borrow).
        if let Some(active) = self.window_layout.active_tab_mut() {
            active.focused_pane = active.pane_layout.next_pane(pane_id);
        }
        self.resize_after_layout_change();
        self.request_redraw();
    }

    fn handle_focus_next(&mut self) {
        let Some(active) = self.window_layout.active_tab_mut() else { return };
        let current = active.focused_pane;
        active.focused_pane = active.pane_layout.next_pane(current);
        tracing::debug!(from = %current, to = %active.focused_pane, "focus cycled");
        self.request_redraw();
    }

    // -----------------------------------------------------------------------
    // Directional pane focus
    // -----------------------------------------------------------------------

    fn handle_focus_directional(&mut self, direction: layout::FocusDirection) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let ws_id = self.window_layout.focused_workspace_id();
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == ws_id).map_or(ws_viewport, |(_, r)| *r);

        let Some(active) = self.window_layout.active_tab() else { return };
        let current = active.focused_pane;
        let rects = active.pane_layout.compute_rects(ws_rect);

        if let Some(target) = active.pane_layout.find_pane_in_direction(current, direction, &rects)
        {
            if let Some(active_mut) = self.window_layout.active_tab_mut() {
                active_mut.focused_pane = target;
                self.request_redraw();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tab management
    // -----------------------------------------------------------------------

    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn handle_new_tab(&mut self) {
        // Capture the focused pane's CWD before add_tab changes the active tab.
        let inherited_cwd = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .and_then(|p| p.cwd.clone());

        let workspace_id = self.window_layout.focused_workspace_id();
        let session_id = SessionId::new();

        let Some(pane_id) = self.window_layout.add_tab(workspace_id, session_id) else { return };

        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let cell = gpu.renderer.cell_size();

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == workspace_id).map_or(ws_viewport, |(_, r)| *r);

        let tab_bar_h = self.tab_bar_height_for(workspace_id, ws_rect);
        let grid = pane::compute_pane_grid(
            ws_rect,
            cell.width,
            cell.height,
            tab_bar_h,
            &self.config.appearance.content_padding,
        );
        let pane = Pane::new(ws_rect, grid, session_id, workspace_id, PaneEdges::all_external());

        self.panes.insert(pane_id, pane);
        self.url_caches.insert(pane_id, url_detect::PaneUrlCache::new());
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(
                tx,
                ClientCommand::CreateSession {
                    workspace_id,
                    split_direction: None,
                    cwd: inherited_cwd,
                },
            );
        }

        self.resize_all_workspace_panes();
        self.request_redraw();
    }

    fn handle_close_tab(&mut self) {
        let ws_id = self.window_layout.focused_workspace_id();

        // Need at least 2 tabs to close one (don't close the last tab).
        let Some(ws) = self.window_layout.focused_workspace() else { return };
        if ws.tab_count() <= 1 {
            return;
        }

        // Get the session ID for the active tab.
        let Some(active) = self.window_layout.active_tab() else { return };
        let session_id = active.session_id;

        // Collect pane IDs and their session IDs for cleanup.
        let pane_ids: Vec<PaneId> = match self.window_layout.active_tab() {
            Some(t) => t.pane_layout.all_pane_ids(),
            None => return,
        };

        // Collect sessions to close before mutating self.panes.
        let sessions_to_close: Vec<SessionId> =
            pane_ids.iter().filter_map(|pid| self.panes.get(pid).map(|p| p.session_id)).collect();

        // Remove pane state.
        for pid in &pane_ids {
            if let Some(pane) = self.panes.remove(pid) {
                self.session_to_pane.remove(&pane.session_id);
            }
            self.url_caches.remove(pid);
        }

        // Tell the server to close each session.
        for sid in sessions_to_close {
            if let Some(tx) = &self.cmd_tx {
                send_command(tx, ClientCommand::CloseSession { session_id: sid });
            }
        }

        // Remove the tab from the workspace.
        self.window_layout.remove_tab(ws_id, session_id);

        self.resize_all_workspace_panes();
        self.request_redraw();
    }

    /// Close a specific tab by workspace ID and tab index (e.g. via close button click).
    ///
    /// No-op if the workspace does not exist, the index is out of bounds, or
    /// this would close the last tab in the workspace.
    fn close_tab_by_index(&mut self, ws_id: WorkspaceId, tab_idx: usize) {
        let Some(ws) = self.window_layout.find_workspace(ws_id) else { return };
        if ws.tab_count() <= 1 {
            return;
        }
        let Some(tab) = ws.tabs.get(tab_idx) else { return };
        let session_id = tab.session_id;

        let pane_ids: Vec<PaneId> = self
            .window_layout
            .find_workspace(ws_id)
            .and_then(|w| w.tabs.get(tab_idx))
            .map(|t| t.pane_layout.all_pane_ids())
            .unwrap_or_default();

        let sessions_to_close: Vec<SessionId> =
            pane_ids.iter().filter_map(|pid| self.panes.get(pid).map(|p| p.session_id)).collect();

        for pid in &pane_ids {
            if let Some(pane) = self.panes.remove(pid) {
                self.session_to_pane.remove(&pane.session_id);
            }
            self.url_caches.remove(pid);
        }

        for sid in sessions_to_close {
            if let Some(tx) = &self.cmd_tx {
                send_command(tx, ClientCommand::CloseSession { session_id: sid });
            }
        }

        self.window_layout.remove_tab(ws_id, session_id);
        self.resize_all_workspace_panes();
        self.request_redraw();
    }

    /// Switch the active tab in a workspace, saving and restoring per-tab
    /// selection state. Returns `true` if the active tab actually changed.
    fn switch_active_tab(&mut self, workspace_id: WorkspaceId, new_index: usize) -> bool {
        // Save current selection to outgoing tab.
        if let Some(tab) = self.window_layout.active_tab_for_workspace_mut(workspace_id) {
            tab.selection = self.active_selection.take();
        }
        // Clear transient drag state.
        self.mouse_selecting = false;
        self.word_drag_anchor = None;
        // Perform the switch.
        let changed = self.window_layout.set_active_tab(workspace_id, new_index);
        if changed {
            // Restore selection from incoming tab.
            if let Some(tab) = self.window_layout.active_tab_for_workspace_mut(workspace_id) {
                self.active_selection = tab.selection.take();
            }
        } else {
            // Switch didn't happen — restore selection to original tab.
            if let Some(tab) = self.window_layout.active_tab_for_workspace_mut(workspace_id) {
                tab.selection = self.active_selection;
            }
        }
        changed
    }

    fn handle_next_tab(&mut self) {
        let ws_id = self.window_layout.focused_workspace_id();
        let Some(ws) = self.window_layout.focused_workspace() else { return };
        let next_idx = ws.next_tab_index();
        if self.switch_active_tab(ws_id, next_idx) {
            self.request_redraw();
        }
    }

    fn handle_prev_tab(&mut self) {
        let ws_id = self.window_layout.focused_workspace_id();
        let Some(ws) = self.window_layout.focused_workspace() else { return };
        let prev_idx = ws.prev_tab_index();
        if self.switch_active_tab(ws_id, prev_idx) {
            self.request_redraw();
        }
    }

    fn handle_select_tab(&mut self, index: usize) {
        let ws_id = self.window_layout.focused_workspace_id();
        if self.switch_active_tab(ws_id, index) {
            self.request_redraw();
        }
    }

    // -----------------------------------------------------------------------
    // Clipboard
    // -----------------------------------------------------------------------

    fn perform_copy(&mut self) {
        self.finalize_copy();
    }

    /// Extract selected text, apply cleanup if Claude Code is active, and
    /// write the result to the system clipboard.
    fn finalize_copy(&mut self) {
        let Some(sel) = self.active_selection else { return };
        if sel.is_empty() {
            return;
        }

        // Extract text and determine AI state while panes/tracker are borrowed.
        let (raw, claude_active) = {
            let Some(tab) = self.window_layout.active_tab() else {
                return;
            };
            let Some(pane) = self.panes.get(&tab.focused_pane) else {
                return;
            };
            let text = selection::extract_text(&pane.term, &sel);
            let ai = self.ai_tracker.get(pane.session_id).is_some();
            (text, ai)
        };

        if raw.is_empty() {
            return;
        }

        let text = clipboard_cleanup::prepare_copy_text(
            &raw,
            claude_active,
            self.config.terminal.claude_copy_cleanup,
        );

        let Some(cb) = &mut self.clipboard else { return };
        if let Err(e) = cb.set_text(text) {
            tracing::warn!("clipboard write failed: {e}");
        }
    }

    fn perform_paste(&mut self) {
        let text = {
            let Some(cb) = &mut self.clipboard else {
                tracing::debug!("clipboard not available");
                return;
            };
            match cb.get_text() {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!("clipboard read failed: {e}");
                    return;
                }
            }
        };
        self.send_paste_data(text);
    }

    /// Send paste text to the focused pane, wrapping in bracketed-paste
    /// sequences when the terminal has enabled that mode.
    ///
    /// This is the shared core used by both clipboard paste and (when added)
    /// primary-selection paste so the two paths stay in sync.
    fn send_paste_data(&mut self, text: String) {
        if text.is_empty() {
            return;
        }

        let Some(tx) = self.cmd_tx.clone() else { return };
        let focused_pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let scrolled_up = {
            let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
            let offset = pane.term.grid().display_offset();
            if offset > 0 {
                pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
                pane.scrollbar_state.on_scroll_action();
                pane.content_dirty = true;
            }
            offset > 0
        };
        if scrolled_up {
            self.ensure_animation_running();
        }

        let Some(pane) = self.panes.get(&focused_pane_id) else { return };

        // Wrap in bracketed paste sequences when the shell has enabled the mode.
        // This prevents newlines in pasted text from executing commands prematurely.
        let bracketed =
            pane.term.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE);
        let data = if bracketed {
            let mut buf = b"\x1b[200~".to_vec();
            buf.extend_from_slice(text.as_bytes());
            buf.extend_from_slice(b"\x1b[201~");
            buf
        } else {
            text.into_bytes()
        };

        if tx.send(ClientCommand::KeyInput { session_id: pane.session_id, data }).is_err() {
            tracing::warn!("IPC channel closed; paste dropped");
        }
    }

    // -----------------------------------------------------------------------
    // Scrollback
    // -----------------------------------------------------------------------

    fn handle_scroll_up(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::PageUp);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_down(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::PageDown);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_top(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_bottom(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "scroll delta is a small float value that fits in i32"
    )]
    fn handle_mouse_wheel(&mut self, delta: winit::event::MouseScrollDelta) {
        use alacritty_terminal::term::TermMode;
        let natural = self.config.terminal.natural_scroll;
        let raw_lines = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => {
                // 3 terminal lines per scroll tick.
                (y * 3.0) as i32
            }
            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                let Some(gpu) = &self.gpu else { return };
                let cell_h = gpu.renderer.cell_size().height;
                if cell_h <= 0.0 {
                    return;
                }
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "pixel delta from winit is f64 but fits in f32"
                )]
                let y = pos.y as f32;
                (y / cell_h).round() as i32
            }
        };
        // In natural mode, use the OS delta as-is. In traditional mode,
        // invert so that scrolling the wheel "up" moves into history.
        let lines = if natural { raw_lines } else { -raw_lines };

        if lines == 0 {
            return;
        }

        // Scroll the pane under the mouse cursor, falling back to the focused pane.
        let target = self
            .pane_id_at_cursor()
            .or_else(|| self.window_layout.active_tab().map(|tab| tab.focused_pane));
        let Some(pane_id) = target else { return };

        // Check focused-pane terminal modes before mutating.
        let (mouse_mode, alt_screen, alt_scroll, sgr_mode) = {
            let Some(pane) = self.panes.get(&pane_id) else { return };
            (
                pane.term.mode().contains(TermMode::MOUSE_MODE),
                pane.term.mode().contains(TermMode::ALT_SCREEN),
                pane.term.mode().contains(TermMode::ALTERNATE_SCROLL),
                pane.term.mode().contains(TermMode::SGR_MOUSE),
            )
        };

        // Priority 1: mouse mode — encode scroll as button 64/65 and send to PTY.
        if mouse_mode {
            self.send_scroll_to_pty(lines, sgr_mode);
            return;
        }

        // Priority 2: alternate screen + alternate scroll — send arrow key sequences.
        if alt_screen && alt_scroll {
            let count = lines.unsigned_abs() as usize;
            let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            let data: Vec<u8> = seq.iter().copied().cycle().take(seq.len() * count).collect();
            self.send_bytes_to_focused_pane(data);
            return;
        }

        // Priority 3: normal scrollback scroll.
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    /// Return the `PaneId` of the pane under the current mouse cursor, if any.
    fn pane_id_at_cursor(&self) -> Option<PaneId> {
        let (x, y) = self.last_cursor_pos?;
        let gpu = self.gpu.as_ref()?;
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);

        for (ws_id, ws_rect) in &ws_rects {
            if !ws_rect.contains(x, y) {
                continue;
            }
            let tab = self.window_layout.find_workspace(*ws_id).and_then(|ws| ws.active_tab());
            let Some(tab) = tab else { continue };
            let pane_rects = tab.pane_layout.compute_rects(*ws_rect);
            if let Some((pane_id, _, _)) = pane_rects.iter().find(|(_, r, _)| r.contains(x, y)) {
                return Some(*pane_id);
            }
        }
        None
    }

    /// Start the animation timer if not already running (needed for scrollbar fade).
    fn ensure_animation_running(&mut self) {
        if !self.animation_running {
            self.start_animation_timer();
        }
    }

    // -----------------------------------------------------------------------
    // Zoom
    // -----------------------------------------------------------------------

    fn zoom_step(&mut self, delta: i8) {
        self.zoom_level = self.zoom_level.saturating_add(delta).clamp(-7, 7);
        self.apply_zoom();
    }

    fn zoom_reset(&mut self) {
        self.zoom_level = 0;
        self.apply_zoom();
    }

    fn apply_zoom(&mut self) {
        let Some(gpu) = &mut self.gpu else { return };
        let size = self.config.appearance.font_size + f32::from(self.zoom_level);
        let params = scribe_renderer::atlas::FontParams {
            family: self.config.appearance.font.clone(),
            size: size.max(6.0),
            weight: self.config.appearance.font_weight,
            weight_bold: self.config.appearance.font_weight_bold,
            ligatures: self.config.appearance.ligatures,
            line_padding: self.config.appearance.line_padding,
        };
        gpu.renderer.rebuild_atlas(&gpu.device, &gpu.queue, &params);
        self.resize_all_workspace_panes();
        self.request_redraw();
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    fn handle_open_find(&mut self) {
        self.search_overlay.open();
        self.request_redraw();
        tracing::debug!("find overlay opened (rendering not yet implemented)");
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    /// Open the settings webview window via the persistent GTK thread.
    ///
    /// Send the current workspace split tree to the server so it can be
    /// persisted for reconnect and handoff.
    fn report_workspace_tree(&self) {
        if let Some(tx) = &self.cmd_tx {
            let tree = self.window_layout.to_tree();
            send_command(tx, ClientCommand::ReportWorkspaceTree { tree });
        }
    }

    /// Open the settings window or focus it if already running.
    #[allow(
        clippy::unused_self,
        reason = "called as self.open_settings() from input handlers that hold &mut self"
    )]
    fn open_settings(&mut self) {
        open_or_focus_settings();
    }

    // -----------------------------------------------------------------------
    // Mouse
    // -----------------------------------------------------------------------

    fn handle_mouse_input(
        &mut self,
        state: winit::event::ElementState,
        button: winit::event::MouseButton,
    ) {
        use winit::event::{ElementState, MouseButton};
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => self.handle_left_press(),
            (MouseButton::Left, ElementState::Released) => {
                let had_ws_drag = self.workspace_divider_drag.is_some();
                self.divider_drag = None;
                self.workspace_divider_drag = None;
                self.end_scrollbar_drag();
                if self.try_forward_mouse_release(MouseButton::Left) {
                    // Mouse mode is active: send release to PTY but still
                    // clear the selecting flag so selection drag ends cleanly.
                    self.mouse_selecting = false;
                } else {
                    self.handle_mouse_release();
                }
                if had_ws_drag {
                    self.report_workspace_tree();
                }
            }
            (MouseButton::Middle, ElementState::Pressed) => {
                if !self.try_forward_mouse_press(MouseButton::Middle) {
                    self.perform_primary_paste();
                }
            }
            (MouseButton::Middle, ElementState::Released) => {
                self.try_forward_mouse_release(MouseButton::Middle);
            }
            (MouseButton::Right, ElementState::Pressed) => {
                if !self.try_forward_mouse_press(MouseButton::Right) {
                    self.open_context_menu();
                }
            }
            (MouseButton::Right, ElementState::Released) => {
                self.try_forward_mouse_release(MouseButton::Right);
            }
            _ => {}
        }
    }

    /// Try to send a mouse button press to the focused pane's PTY.
    ///
    /// Returns `true` and sends the event when the focused pane has mouse mode
    /// enabled and Shift is not held. Returns `false` when the caller should
    /// fall through to normal handling.
    fn try_forward_mouse_press(&self, button: winit::event::MouseButton) -> bool {
        let Some((x, y)) = self.last_cursor_pos else { return false };
        let mouse_mode = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .is_some_and(pane::Pane::has_mouse_mode);
        if !mouse_mode || self.modifiers.shift_key() {
            return false;
        }
        let Some((col, row)) = self.pixel_to_term_cell(x, y) else { return false };
        let sgr = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .is_some_and(|p| p.term.mode().contains(alacritty_terminal::term::TermMode::SGR_MOUSE));
        let data = mouse_reporting::encode_mouse_press(button, col, row, self.modifiers, sgr);
        if !data.is_empty() {
            self.send_bytes_to_focused_pane(data);
            return true;
        }
        false
    }

    /// Try to send a mouse button release to the focused pane's PTY.
    ///
    /// Returns `true` when the event was forwarded (mouse mode active, Shift
    /// not held). Returns `false` when the caller should fall through.
    fn try_forward_mouse_release(&self, button: winit::event::MouseButton) -> bool {
        let Some((x, y)) = self.last_cursor_pos else { return false };
        let mouse_mode = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .is_some_and(pane::Pane::has_mouse_mode);
        if !mouse_mode || self.modifiers.shift_key() {
            return false;
        }
        let Some((col, row)) = self.pixel_to_term_cell(x, y) else { return false };
        let sgr = self
            .window_layout
            .active_tab()
            .and_then(|t| self.panes.get(&t.focused_pane))
            .is_some_and(|p| p.term.mode().contains(alacritty_terminal::term::TermMode::SGR_MOUSE));
        let data = mouse_reporting::encode_mouse_release(button, col, row, self.modifiers, sgr);
        if !data.is_empty() {
            self.send_bytes_to_focused_pane(data);
            return true;
        }
        false
    }

    /// Handle left mouse button press, routing through context menu if open.
    fn handle_left_press(&mut self) {
        if self.context_menu.is_none() {
            self.handle_mouse_press();
            return;
        }
        let Some((x, y)) = self.last_cursor_pos else {
            self.context_menu = None;
            self.request_redraw();
            return;
        };
        let inside = self.context_menu.as_ref().is_some_and(|m| m.click_is_inside(x, y));
        if inside {
            let action = self.context_menu.as_ref().and_then(|m| m.click(x, y));
            self.context_menu = None;
            if let Some(a) = action {
                self.dispatch_context_menu_action(a);
            }
        } else {
            self.context_menu = None;
            self.request_redraw();
            self.handle_mouse_press();
        }
    }

    /// Handle a left-button press: click-to-focus pane/workspace, or start a
    /// divider drag.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "click dispatch: gear, equalize, update button, tab close, tab drag, scrollbar, divider, selection"
    )]
    fn handle_mouse_press(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);

        // Check for status-bar gear icon click (opens settings).
        if let Some(gear) = self.status_bar_gear_rect {
            if gear.contains(x, y) {
                self.open_settings();
                return;
            }
        }

        // Check for status-bar equalize click (equalize all workspace ratios).
        if let Some(eq_rect) = self.status_bar_equalize_rect {
            if eq_rect.contains(x, y) {
                self.window_layout.equalize_all_workspace_ratios();
                self.resize_all_workspace_panes();
                self.report_workspace_tree();
                self.request_redraw();
                return;
            }
        }

        // Check for tab bar update button click (opens update confirmation dialog).
        if self.tab_bar_update_targets.iter().any(|(_, rect)| rect.contains(x, y)) {
            self.open_update_dialog();
            return;
        }

        // Check for tab bar equalize click (equalize pane ratios in that workspace).
        if let Some((ws_id, _)) =
            self.tab_bar_equalize_targets.iter().find(|(_, rect)| rect.contains(x, y)).copied()
        {
            let tab =
                self.window_layout.find_workspace_mut(ws_id).and_then(|ws| ws.active_tab_mut());
            if let Some(tab) = tab {
                tab.pane_layout.equalize_all_ratios();
            }
            self.resize_after_layout_change();
            self.request_redraw();
            return;
        }

        // Check for tab close button click (before tab switch).
        if let Some((ws_id, tab_idx)) = self
            .tab_close_hit_targets
            .iter()
            .find_map(|(ws_id, idx, rect)| rect.contains(x, y).then_some((*ws_id, *idx)))
        {
            self.close_tab_by_index(ws_id, tab_idx);
            return;
        }

        // Check for tab bar click: start a drag candidate (switch on release if no drag).
        if let Some((ws_id, tab_idx)) = self
            .tab_hit_targets
            .iter()
            .find_map(|(ws_id, idx, rect)| rect.contains(x, y).then_some((*ws_id, *idx)))
        {
            self.tab_drag = Some(TabDrag {
                workspace_id: ws_id,
                tab_index: tab_idx,
                start_x: x,
                start_y: y,
                cursor_x: x,
                cursor_y: y,
                dragging: false,
                grab_offset_x: 0.0,
            });
            return;
        }

        // Check for scrollbar click (before divider, before selection).
        if self.try_start_scrollbar_interaction(x, y) {
            return;
        }

        // Check for workspace divider drag (before pane divider).
        if self.try_start_workspace_divider_drag(x, y, ws_viewport) {
            return;
        }

        // Check for divider drag first (within the focused workspace).
        if self.try_start_divider_drag(x, y, &ws_rects) {
            return;
        }

        // Click-to-focus: find which pane the click landed in.
        self.focus_pane_at(x, y, &ws_rects);

        // Ctrl+click opens hovered URL in the system browser.
        if self.try_open_hovered_url() {
            return;
        }

        // Forward left-button press to PTY when mouse mode is active.
        // Shift bypass: held Shift falls through to normal selection.
        if self.try_forward_mouse_press(winit::event::MouseButton::Left) {
            return;
        }

        // Shift+click extends an existing selection instead of starting a new one.
        if self.modifiers.shift_key() && self.active_selection.is_some() {
            self.extend_selection_to(x, y);
            return;
        }

        // Start selection with click-count classification.
        self.start_selection(x, y);
        let click_kind = self.mouse_click.record_press(x, y);
        match click_kind {
            mouse_state::ClickKind::Single => {}
            mouse_state::ClickKind::Double => self.start_selection_word(x, y),
            mouse_state::ClickKind::Triple => self.start_selection_line(x, y),
        }
    }

    /// Try to start a divider drag in the focused workspace. Returns `true`
    /// if a divider was hit.
    fn try_start_divider_drag(&mut self, x: f32, y: f32, ws_rects: &[(WorkspaceId, Rect)]) -> bool {
        let focused_ws_id = self.window_layout.focused_workspace_id();
        let Some((_, ws_rect)) = ws_rects.iter().find(|(wid, _)| *wid == focused_ws_id) else {
            return false;
        };
        let tab = self.window_layout.find_workspace(focused_ws_id).and_then(|ws| ws.active_tab());
        let Some(tab) = tab else { return false };

        let dividers = divider::collect_dividers(tab.pane_layout.root(), *ws_rect);
        if let Some(hit) = divider::hit_test_divider(&dividers, x, y) {
            self.divider_drag = Some(divider::start_drag(hit, *ws_rect));
            return true;
        }
        false
    }

    /// Try to start a workspace divider drag. Returns `true` if a divider was hit.
    fn try_start_workspace_divider_drag(&mut self, x: f32, y: f32, ws_viewport: Rect) -> bool {
        let ws_dividers = self.window_layout.collect_workspace_dividers(ws_viewport);
        if let Some(hit) = workspace_layout::hit_test_workspace_divider(&ws_dividers, x, y) {
            self.workspace_divider_drag = Some(workspace_layout::start_workspace_drag(hit));
            return true;
        }
        false
    }

    /// Finalize a scrollbar drag on mouse-release.
    fn end_scrollbar_drag(&mut self) {
        let Some(pane_id) = self.scrollbar_drag_pane.take() else { return };
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.scrollbar_state.on_drag_end();
        }
        self.request_redraw();
    }

    /// Try to start a scrollbar click or drag. Returns `true` if the
    /// scrollbar was hit.
    fn try_start_scrollbar_interaction(&mut self, x: f32, y: f32) -> bool {
        let tab = self.window_layout.active_tab();
        let Some(tab) = tab else { return false };
        let focused_pane_id = tab.focused_pane;
        let scrollbar_width = self.config.appearance.scrollbar_width.clamp(2.0, 20.0);
        let tab_bar_h = self.focused_workspace_tab_bar_height();

        // Phase 1: read-only queries (immutable borrow of self.panes).
        let action = {
            let Some(pane) = self.panes.get(&focused_pane_id) else { return false };
            if !scrollbar::hit_test_scrollbar(pane, x, y, scrollbar_width, tab_bar_h) {
                return false;
            }

            let display_offset = pane.term.grid().display_offset();

            if scrollbar::hit_test_thumb(pane, x, y, scrollbar_width, tab_bar_h) {
                ScrollbarAction::StartDrag { display_offset }
            } else {
                let target =
                    scrollbar::offset_from_track_click(pane, y, scrollbar_width, tab_bar_h);
                #[allow(
                    clippy::cast_possible_wrap,
                    clippy::cast_possible_truncation,
                    reason = "display offsets are small positive values that fit in i32"
                )]
                let delta = target as i32 - display_offset as i32;
                ScrollbarAction::JumpTo { delta }
            }
        };
        // Immutable borrow dropped here.

        // Phase 2: mutate (mutable borrow of self.panes).
        let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return false };
        match action {
            ScrollbarAction::StartDrag { display_offset } => {
                pane.scrollbar_state.drag = Some(scrollbar::ScrollbarDrag {
                    start_mouse_y: y,
                    start_display_offset: display_offset,
                });
                pane.scrollbar_state.opacity = 1.0;
                pane.scrollbar_state.fade_start = None;
                self.scrollbar_drag_pane = Some(focused_pane_id);
            }
            ScrollbarAction::JumpTo { delta } => {
                pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
                pane.scrollbar_state.on_scroll_action();
                pane.content_dirty = true;
            }
        }

        self.ensure_animation_running();
        self.request_redraw();
        true
    }

    /// Handle scrollbar drag movement.
    fn handle_scrollbar_drag(&mut self, pane_id: layout::PaneId) {
        let Some((_, y)) = self.last_cursor_pos else { return };
        let scrollbar_width = self.config.appearance.scrollbar_width.clamp(2.0, 20.0);
        let tab_bar_h = self.focused_workspace_tab_bar_height();

        // Phase 1: read-only — compute the scroll delta.
        let delta = {
            let Some(pane) = self.panes.get(&pane_id) else { return };
            let Some(drag) = pane.scrollbar_state.drag.as_ref() else {
                self.scrollbar_drag_pane = None;
                return;
            };
            let target_offset =
                scrollbar::offset_from_drag(pane, drag, y, scrollbar_width, tab_bar_h);
            let current_offset = pane.term.grid().display_offset();
            #[allow(
                clippy::cast_possible_wrap,
                clippy::cast_possible_truncation,
                reason = "display offsets are small positive values that fit in i32"
            )]
            {
                target_offset as i32 - current_offset as i32
            }
        };

        // Phase 2: mutate.
        if delta != 0 {
            let Some(pane) = self.panes.get_mut(&pane_id) else { return };
            pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
            pane.content_dirty = true;
        }
        self.request_redraw();
    }

    /// Update scrollbar hover state for the focused pane.
    fn update_scrollbar_hover(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let tab = self.window_layout.active_tab();
        let Some(tab) = tab else { return };
        let focused_pane_id = tab.focused_pane;
        let scrollbar_width = self.config.appearance.scrollbar_width.clamp(2.0, 20.0);
        let tab_bar_h = self.focused_workspace_tab_bar_height();

        let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
        let in_zone = scrollbar::hit_test_scrollbar(pane, x, y, scrollbar_width, tab_bar_h);

        let was_hovering = pane.scrollbar_state.hover;
        if in_zone && !was_hovering {
            pane.scrollbar_state.on_hover_enter();
            self.ensure_animation_running();
            self.request_redraw();
        } else if !in_zone && was_hovering {
            pane.scrollbar_state.on_hover_leave();
            self.ensure_animation_running();
            self.request_redraw();
        }
    }

    /// Switch focus to whichever pane contains the point `(x, y)`.
    fn focus_pane_at(&mut self, x: f32, y: f32, ws_rects: &[(WorkspaceId, Rect)]) {
        for (ws_id, ws_rect) in ws_rects {
            if !ws_rect.contains(x, y) {
                continue;
            }
            let tab = self.window_layout.find_workspace(*ws_id).and_then(|ws| ws.active_tab());
            let Some(tab) = tab else { continue };

            let pane_rects = tab.pane_layout.compute_rects(*ws_rect);
            let hit = pane_rects.iter().find(|(_, r, _)| r.contains(x, y));
            let Some((clicked_pane, _, _)) = hit else { continue };

            // Switch workspace focus if needed.
            self.window_layout.set_focused_workspace(*ws_id);

            // Switch pane focus within the workspace.
            if let Some(active) = self.window_layout.active_tab_mut() {
                active.focused_pane = *clicked_pane;
            }
            self.request_redraw();
            return;
        }
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential dispatch for multiple independent drag/hover states; each branch is simple"
    )]
    fn handle_cursor_moved(&mut self) {
        // Scrollbar drag takes highest priority.
        if let Some(pane_id) = self.scrollbar_drag_pane {
            self.handle_scrollbar_drag(pane_id);
            return;
        }

        // Extend active text selection while mouse is held.
        self.extend_selection();

        // Edge-scroll while selecting: scroll pane when cursor is near top/bottom edge.
        self.maybe_edge_scroll();

        // Update context menu hover if open.
        self.maybe_update_context_menu_hover();

        // Update scrollbar hover state for the focused pane.
        self.update_scrollbar_hover();

        let Some((x, y)) = self.last_cursor_pos else { return };

        // Workspace divider drag (checked before pane divider drag).
        if let Some(drag) = self.workspace_divider_drag {
            let mouse_pos = match drag.direction {
                layout::SplitDirection::Horizontal => x,
                layout::SplitDirection::Vertical => y,
            };
            let new_ratio = workspace_layout::workspace_drag_ratio(&drag, mouse_pos);
            let _ = self.window_layout.set_workspace_ratio(
                drag.first_workspace,
                drag.second_workspace,
                new_ratio,
            );
            self.resize_all_workspace_panes();
            self.request_redraw();
            return;
        }

        // Pane divider drag.
        if let Some(drag) = self.divider_drag {
            let mouse_pos = match drag.direction {
                layout::SplitDirection::Horizontal => x,
                layout::SplitDirection::Vertical => y,
            };

            let new_ratio = divider::drag_ratio(&drag, mouse_pos);

            if let Some(tab) = self.window_layout.active_tab_mut() {
                let _ = tab.pane_layout.set_ratio_for_pane(drag.first_pane, new_ratio);
            }

            self.resize_after_layout_change();
            self.request_redraw();
            return;
        }

        // Tab drag update.
        if self.tab_drag.is_some() {
            self.update_tab_drag(x, y);
            return;
        }

        // Update tab close hover state.
        let new_hover = self
            .tab_close_hit_targets
            .iter()
            .find_map(|(ws_id, idx, rect)| rect.contains(x, y).then_some((*ws_id, *idx)));
        if new_hover != self.hovered_tab_close {
            self.hovered_tab_close = new_hover;
            self.request_redraw();
        }

        // Forward motion events to PTY when mouse motion reporting is active.
        self.maybe_forward_mouse_motion(x, y);

        // No drag active — update cursor icon based on divider hover.
        self.update_hover_cursor(x, y);

        // Refresh URL hover state when not dragging a selection.
        if !self.mouse_selecting && self.refresh_hovered_url() {
            self.request_redraw();
        }
    }

    /// Forward a mouse motion event to the focused pane's PTY when the
    /// terminal has enabled motion reporting.
    ///
    /// Sends when:
    /// - `MOUSE_MOTION` (mode 1003) is set — all pointer movement is reported.
    /// - `MOUSE_DRAG` (mode 1002) is set and a button is held (`mouse_selecting`).
    fn maybe_forward_mouse_motion(&self, x: f32, y: f32) {
        use alacritty_terminal::term::TermMode;
        let params = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
            let mode = pane.term.mode();
            let all_motion = mode.contains(TermMode::MOUSE_MOTION);
            let drag_motion = mode.contains(TermMode::MOUSE_DRAG) && self.mouse_selecting;
            if !all_motion && !drag_motion {
                return;
            }
            let sgr = mode.contains(TermMode::SGR_MOUSE);
            let button_held =
                if self.mouse_selecting { Some(winit::event::MouseButton::Left) } else { None };
            (sgr, button_held)
        };
        let (sgr, button_held) = params;
        let Some((col, row)) = self.pixel_to_term_cell(x, y) else { return };
        let data = mouse_reporting::encode_mouse_motion(col, row, button_held, self.modifiers, sgr);
        self.send_bytes_to_focused_pane(data);
    }

    /// Set the window cursor icon based on whether the pointer is hovering over
    /// a divider. Resets to the default arrow cursor when not over any divider.
    fn update_hover_cursor(&self, x: f32, y: f32) {
        let Some(gpu) = &self.gpu else { return };
        let Some(window) = &self.window else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);

        // Check workspace dividers first.
        let ws_dividers = self.window_layout.collect_workspace_dividers(ws_viewport);
        if let Some(hit) = workspace_layout::hit_test_workspace_divider(&ws_dividers, x, y) {
            let icon = match hit.direction {
                layout::SplitDirection::Horizontal => winit::window::CursorIcon::ColResize,
                layout::SplitDirection::Vertical => winit::window::CursorIcon::RowResize,
            };
            window.set_cursor(icon);
            return;
        }

        // Check pane dividers in the focused workspace.
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let focused_ws_id = self.window_layout.focused_workspace_id();
        let focused_ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == focused_ws_id).map(|(_, r)| *r);
        let focused_tab =
            self.window_layout.find_workspace(focused_ws_id).and_then(|ws| ws.active_tab());
        if let (Some(ws_rect), Some(tab)) = (focused_ws_rect, focused_tab) {
            let dividers = divider::collect_dividers(tab.pane_layout.root(), ws_rect);
            if let Some(hit) = divider::hit_test_divider(&dividers, x, y) {
                let icon = match hit.direction {
                    layout::SplitDirection::Horizontal => winit::window::CursorIcon::ColResize,
                    layout::SplitDirection::Vertical => winit::window::CursorIcon::RowResize,
                };
                window.set_cursor(icon);
                return;
            }
        }

        // Check if the cursor is over terminal content in any pane.
        if self.cursor_over_terminal_content(x, y, &ws_rects) {
            let icon = if self.hovered_url.is_some() && self.modifiers.control_key() {
                winit::window::CursorIcon::Pointer
            } else {
                winit::window::CursorIcon::Text
            };
            window.set_cursor(icon);
            return;
        }

        // Not over any divider or terminal content — reset to default.
        window.set_cursor(winit::window::CursorIcon::Default);
    }

    /// Return `true` if pixel `(x, y)` is inside any pane's terminal content area
    /// (below the tab bar) across all given workspace rects.
    #[allow(
        clippy::excessive_nesting,
        reason = "nested iteration over workspaces and pane rects to find hit-tested pane; extraction would obscure logic"
    )]
    fn cursor_over_terminal_content(
        &self,
        x: f32,
        y: f32,
        ws_rects: &[(WorkspaceId, Rect)],
    ) -> bool {
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        for (ws_id, ws_rect) in ws_rects {
            if !ws_rect.contains(x, y) {
                continue;
            }
            let tab = self.window_layout.find_workspace(*ws_id).and_then(|ws| ws.active_tab());
            let Some(tab) = tab else { continue };
            let pane_rects = tab.pane_layout.compute_rects(*ws_rect);
            for (_, pane_rect, _) in &pane_rects {
                let content_top = pane_rect.y + tab_bar_h;
                if x >= pane_rect.x
                    && x < pane_rect.x + pane_rect.width
                    && y >= content_top
                    && y < pane_rect.y + pane_rect.height
                {
                    return true;
                }
            }
        }
        false
    }

    // -------------------------------------------------------------------
    // URL hover helpers
    // -------------------------------------------------------------------

    /// Refresh the hovered URL based on the current cursor position.
    ///
    /// Returns `true` if the hovered URL changed (caller should request redraw).
    fn refresh_hovered_url(&mut self) -> bool {
        let new_url = self.compute_hovered_url();
        let changed = url_span_changed(self.hovered_url.as_ref(), new_url.as_ref());
        self.hovered_url = new_url;
        changed
    }

    /// Compute the URL span under the cursor without mutating `hovered_url`.
    fn compute_hovered_url(&mut self) -> Option<url_detect::UrlSpan> {
        let (cx, cy) = self.last_cursor_pos?;
        let point = self.cursor_to_grid(cx, cy)?;
        let pane_id = self.window_layout.active_tab()?.focused_pane;
        // Pass panes and url_caches as separate parameters so the borrow
        // checker can see they are independent — no unsafe needed.
        hovered_url_at(point, pane_id, &self.panes, &mut self.url_caches)
    }

    /// If Ctrl is held and a URL is hovered, open it in the default browser.
    ///
    /// Returns `true` if a URL was opened.
    pub fn try_open_hovered_url(&mut self) -> bool {
        if !self.modifiers.control_key() {
            return false;
        }
        if let Some(ref span) = self.hovered_url {
            let url = span.url.clone();
            url_detect::open_url(&url);
            return true;
        }
        false
    }

    // -------------------------------------------------------------------
    // Text selection helpers
    // -------------------------------------------------------------------

    /// Begin a new text selection at the given pixel position.
    fn start_selection(&mut self, x: f32, y: f32) {
        self.mouse_selecting = true;
        self.active_selection = None;
        self.word_drag_anchor = None;
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        self.active_selection = Some(selection::SelectionRange::cell(point, point));
    }

    /// Begin a word-granularity selection (double-click).
    fn start_selection_word(&mut self, x: f32, y: f32) {
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        let pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let (start, end) = selection::word_bounds_at(&pane.term, point);
        self.active_selection = Some(selection::SelectionRange::word(start, end));
        self.word_drag_anchor = Some((start, end));
        self.mouse_selecting = true;
        self.request_redraw();
    }

    /// Begin a line-granularity selection (triple-click).
    fn start_selection_line(&mut self, x: f32, y: f32) {
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        let pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let (start, end) = selection::line_bounds_at(&pane.term, point.row);
        self.active_selection = Some(selection::SelectionRange::line(start, end));
        self.mouse_selecting = true;
        self.request_redraw();
    }

    /// Extend the active selection to the given pixel position (shift+click).
    fn extend_selection_to(&mut self, x: f32, y: f32) {
        let Some(sel) = self.active_selection else { return };
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        self.active_selection = Some(selection::SelectionRange::cell(sel.start, point));
        self.request_redraw();
    }

    /// Extend the in-progress selection to the current cursor position.
    fn extend_selection(&mut self) {
        if !self.mouse_selecting {
            return;
        }
        let Some(sel) = self.active_selection else { return };
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        match sel.mode {
            mouse_state::SelectionMode::Cell => {
                self.active_selection = Some(selection::SelectionRange::cell(sel.start, point));
                self.request_redraw();
            }
            mouse_state::SelectionMode::Word => {
                self.extend_selection_word(point);
            }
            mouse_state::SelectionMode::Line => {
                self.extend_selection_line(sel.start, point);
            }
        }
    }

    fn extend_selection_word(&mut self, point: selection::SelectionPoint) {
        let Some((anchor_start, anchor_end)) = self.word_drag_anchor else { return };
        let pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let Some(pane) = self.panes.get(&pane_id) else { return };
        let new_sel = selection::extend_by_word(&pane.term, anchor_start, anchor_end, point);
        self.active_selection = Some(new_sel);
        self.request_redraw();
    }

    fn extend_selection_line(
        &mut self,
        anchor_start: selection::SelectionPoint,
        point: selection::SelectionPoint,
    ) {
        let pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let Some(pane) = self.panes.get(&pane_id) else { return };
        // When dragging upward the drag point is above the anchor line.
        // Extend from the start of the drag row to the end of the anchor row.
        // When dragging downward (or same row), extend from the start of the
        // anchor row to the end of the drag row.
        let new_sel = if point.row < anchor_start.row {
            let (drag_line_start, _) = selection::line_bounds_at(&pane.term, point.row);
            let (_, anchor_line_end) = selection::line_bounds_at(&pane.term, anchor_start.row);
            selection::SelectionRange::line(drag_line_start, anchor_line_end)
        } else {
            let (anchor_line_start, _) = selection::line_bounds_at(&pane.term, anchor_start.row);
            let (_, drag_line_end) = selection::line_bounds_at(&pane.term, point.row);
            selection::SelectionRange::line(anchor_line_start, drag_line_end)
        };
        self.active_selection = Some(new_sel);
        self.request_redraw();
    }

    /// Finalize selection on mouse release and auto-copy if enabled.
    fn handle_mouse_release(&mut self) {
        self.mouse_selecting = false;
        self.finish_tab_drag();
        if !self.config.terminal.copy_on_select {
            return;
        }
        self.finalize_copy();
        #[cfg(target_os = "linux")]
        self.set_primary_selection();
    }

    /// Scroll the focused pane if the cursor is near the top/bottom edge during drag selection.
    fn maybe_edge_scroll(&mut self) {
        if !self.mouse_selecting {
            return;
        }
        let Some((_, cursor_y)) = self.last_cursor_pos else { return };
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        let Some(pane_rect) = self.focused_pane_rect() else { return };
        let content_top = pane_rect.y + tab_bar_h;
        let content_bottom = pane_rect.y + pane_rect.height;
        let Some(delta) = mouse_state::edge_scroll_delta(cursor_y, content_top, content_bottom)
        else {
            return;
        };
        let pane_id = self.window_layout.active_tab().map(|t| t.focused_pane);
        let Some(pane_id) = pane_id else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
        pane.scrollbar_state.on_scroll_action();
        pane.content_dirty = true;
        self.ensure_animation_running();
        self.request_redraw();
    }

    /// Update context menu hover state from current cursor position.
    fn maybe_update_context_menu_hover(&mut self) {
        let Some((cx, cy)) = self.last_cursor_pos else { return };
        let changed = self.context_menu.as_mut().is_some_and(|m| m.update_hover(cx, cy));
        if changed {
            self.request_redraw();
        }
    }

    /// Paste from the primary selection on Linux (X11/Wayland primary selection).
    #[cfg(target_os = "linux")]
    fn perform_primary_paste(&mut self) {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let text = {
            let Some(cb) = &mut self.clipboard else {
                tracing::debug!("clipboard not available");
                return;
            };
            match cb.get().clipboard(LinuxClipboardKind::Primary).text() {
                Ok(t) => t,
                Err(e) => {
                    tracing::debug!("primary selection read failed: {e}");
                    return;
                }
            }
        };
        if text.is_empty() {
            return;
        }
        let Some(tx) = self.cmd_tx.clone() else { return };
        let focused_pane_id = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            tab.focused_pane
        };
        let scrolled_up = {
            let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
            let offset = pane.term.grid().display_offset();
            if offset > 0 {
                pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
                pane.scrollbar_state.on_scroll_action();
                pane.content_dirty = true;
            }
            offset > 0
        };
        if scrolled_up {
            self.ensure_animation_running();
        }
        let Some(pane) = self.panes.get(&focused_pane_id) else { return };
        let bracketed =
            pane.term.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE);
        let data = if bracketed {
            let mut buf = b"\x1b[200~".to_vec();
            buf.extend_from_slice(text.as_bytes());
            buf.extend_from_slice(b"\x1b[201~");
            buf
        } else {
            text.into_bytes()
        };
        if tx.send(ClientCommand::KeyInput { session_id: pane.session_id, data }).is_err() {
            tracing::warn!("IPC channel closed; primary paste dropped");
        }
    }

    /// Paste from primary selection on non-Linux (falls back to regular clipboard).
    #[cfg(not(target_os = "linux"))]
    fn perform_primary_paste(&mut self) {
        self.perform_paste();
    }

    /// Write the current selection text to the Linux primary selection buffer.
    #[cfg(target_os = "linux")]
    fn set_primary_selection(&mut self) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let Some(sel) = self.active_selection else { return };
        if sel.is_empty() {
            return;
        }
        let (raw, claude_active) = {
            let Some(tab) = self.window_layout.active_tab() else { return };
            let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
            let text = selection::extract_text(&pane.term, &sel);
            let ai = self.ai_tracker.get(pane.session_id).is_some();
            (text, ai)
        };
        if raw.is_empty() {
            return;
        }
        let text = clipboard_cleanup::prepare_copy_text(
            &raw,
            claude_active,
            self.config.terminal.claude_copy_cleanup,
        );
        let Some(cb) = &mut self.clipboard else { return };
        if let Err(e) = cb.set().clipboard(LinuxClipboardKind::Primary).text(text) {
            tracing::debug!("primary selection write failed: {e}");
        }
    }

    /// Open the right-click context menu at the current cursor position.
    fn open_context_menu(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let has_selection =
            self.active_selection.is_some_and(|s: selection::SelectionRange| !s.is_empty());
        let url = self.hovered_url.as_ref().map(|s| s.url.clone());
        self.context_menu = Some(context_menu::ContextMenu::new(x, y, has_selection, url));
        self.request_redraw();
    }

    /// Dispatch an action selected from the context menu.
    fn dispatch_context_menu_action(&mut self, action: context_menu::ContextMenuAction) {
        match action {
            context_menu::ContextMenuAction::Copy => self.finalize_copy(),
            context_menu::ContextMenuAction::Paste => self.perform_paste(),
            context_menu::ContextMenuAction::SelectAll => self.select_all(),
            context_menu::ContextMenuAction::OpenUrl(url) => url_detect::open_url(&url),
        }
        self.request_redraw();
    }

    /// Select all content in the focused pane (viewport + scrollback).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "history_size and screen_lines are bounded by scrollback_lines (≤ 100_000), fit in i32"
    )]
    fn select_all(&mut self) {
        use alacritty_terminal::grid::Dimensions as _;
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
        let history = pane.term.grid().history_size() as i32;
        let last_row = pane.term.grid().screen_lines().saturating_sub(1) as i32;
        let last_col = pane.term.grid().columns().saturating_sub(1);
        let start = selection::SelectionPoint { row: -history, col: 0 };
        let end = selection::SelectionPoint { row: last_row, col: last_col };
        self.active_selection = Some(selection::SelectionRange::cell(start, end));
        self.request_redraw();
    }

    /// Update the in-progress tab drag position and threshold.
    fn update_tab_drag(&mut self, x: f32, y: f32) {
        let (was_dragging, now_dragging, ws_id, tab_idx) = {
            let Some(drag) = self.tab_drag.as_mut() else { return };
            drag.cursor_x = x;
            drag.cursor_y = y;
            let was = drag.dragging;
            if !drag.dragging {
                let dx = x - drag.start_x;
                let dy = y - drag.start_y;
                drag.dragging = dx * dx + dy * dy > 25.0;
            }
            (was, drag.dragging, drag.workspace_id, drag.tab_index)
        };
        // On threshold first exceeded: compute grab offset, init offsets, set cursor.
        if !was_dragging && now_dragging {
            // Find the hit rect for this tab to compute grab offset.
            let tab_left = self
                .tab_hit_targets
                .iter()
                .find_map(|(w, i, r)| (*w == ws_id && *i == tab_idx).then_some(r.x))
                .unwrap_or(x);
            if let Some(state) = self.tab_drag.as_mut() {
                state.grab_offset_x = x - tab_left;
            }
            let tab_count = self
                .window_layout
                .find_workspace(ws_id)
                .map_or(0, workspace_layout::WorkspaceSlot::tab_count);
            self.tab_drag_offsets = vec![0.0; tab_count];
            if let Some(window) = &self.window {
                window.set_cursor(winit::window::CursorIcon::Grabbing);
            }
        }
        if now_dragging {
            self.try_live_reorder_tab();
            self.request_redraw();
        }
    }

    /// Live-reorder the dragged tab as the cursor crosses tab boundaries.
    fn try_live_reorder_tab(&mut self) {
        let (ws_id, from, cursor_x, cursor_y) = {
            let Some(drag) = &self.tab_drag else { return };
            if !drag.dragging {
                return;
            }
            (drag.workspace_id, drag.tab_index, drag.cursor_x, drag.cursor_y)
        };

        let to = self.tab_hit_targets.iter().find_map(|(ws, idx, rect)| {
            (*ws == ws_id && rect.contains(cursor_x, cursor_y)).then_some(*idx)
        });
        let Some(to) = to else { return };
        if from == to {
            return;
        }

        let cell_w = self.gpu.as_ref().map_or(0.0, |g| g.renderer.cell_size().width);
        let tab_w_px = f32::from(self.config.appearance.tab_width) * cell_w;

        if let Some(ws) = self.window_layout.find_workspace_mut(ws_id) {
            ws.reorder_tab(from, to);
        }

        // Rearrange offsets Vec to follow the reordered tab.
        if let Some(dragged_offset) = self.tab_drag_offsets.get(from).copied() {
            self.tab_drag_offsets.remove(from);
            let insert_at = to.min(self.tab_drag_offsets.len());
            self.tab_drag_offsets.insert(insert_at, dragged_offset);
        }

        // Give displaced tabs a starting offset so they animate to their new slot.
        displace_tab_offsets(&mut self.tab_drag_offsets, from, to, tab_w_px);

        if let Some(drag) = self.tab_drag.as_mut() {
            drag.tab_index = to;
        }

        self.ensure_animation_running();
    }

    /// Complete the tab drag on mouse release: set release animation offset or switch tab.
    fn finish_tab_drag(&mut self) {
        let Some(drag) = self.tab_drag.take() else { return };
        if drag.dragging {
            self.apply_release_offset(&drag);
            self.ensure_animation_running();
            self.window_layout.set_focused_workspace(drag.workspace_id);
        } else {
            // No drag threshold reached — treat as a plain tab click.
            if self.switch_active_tab(drag.workspace_id, drag.tab_index) {
                self.window_layout.set_focused_workspace(drag.workspace_id);
            }
        }
        if let Some(window) = &self.window {
            window.set_cursor(winit::window::CursorIcon::Default);
        }
        self.request_redraw();
    }

    /// Set the release animation offset for the dragged tab after drop.
    fn apply_release_offset(&mut self, drag: &TabDrag) {
        let release_offset = self
            .tab_hit_targets
            .iter()
            .find(|(ws, idx, _)| *ws == drag.workspace_id && *idx == drag.tab_index)
            .map(|(_, _, rect)| drag.cursor_x - drag.grab_offset_x - rect.x);
        if let Some(offset_val) = release_offset {
            if let Some(offset) = self.tab_drag_offsets.get_mut(drag.tab_index) {
                *offset = offset_val;
            }
        }
    }

    /// Convert a pixel position to an absolute grid cell in the focused pane.
    ///
    /// The returned row is an absolute grid line (negative = scrollback),
    /// incorporating the pane's current `display_offset`.
    fn cursor_to_grid(&self, x: f32, y: f32) -> Option<selection::SelectionPoint> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        let pane_rect = self.focused_pane_rect()?;
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        let tab = self.window_layout.active_tab()?;
        let pane = self.panes.get(&tab.focused_pane)?;
        let display_offset = pane.term.grid().display_offset();
        selection::pixel_to_grid(
            x,
            y,
            pane_rect,
            cell.width,
            cell.height,
            tab_bar_h,
            display_offset,
            &pane::effective_padding(&self.config.appearance.content_padding, pane.edges),
        )
    }

    /// Compute the screen rect of the currently focused pane.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn focused_pane_rect(&self) -> Option<Rect> {
        let gpu = self.gpu.as_ref()?;
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let focused_ws = self.window_layout.focused_workspace_id();
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == focused_ws).map_or(ws_viewport, |(_, r)| *r);
        let tab = self.window_layout.active_tab()?;
        let pane_rects = tab.pane_layout.compute_rects(ws_rect);
        let (_, rect, _) = pane_rects.iter().find(|(pid, _, _)| *pid == tab.focused_pane)?;
        Some(*rect)
    }

    // -----------------------------------------------------------------------
    // Mouse reporting helpers
    // -----------------------------------------------------------------------

    /// Encode a scroll event as a mouse button sequence and send it to the
    /// focused pane's PTY.
    ///
    /// `lines` > 0 means scroll up (button 64), < 0 means scroll down (65).
    fn send_scroll_to_pty(&self, lines: i32, sgr_mode: bool) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some((col, row)) = self.pixel_to_term_cell(x, y) else { return };
        let data =
            mouse_reporting::encode_mouse_scroll(lines > 0, col, row, self.modifiers, sgr_mode);
        self.send_bytes_to_focused_pane(data);
    }

    /// Convert pixel `(x, y)` to a 0-indexed `(col, row)` within the focused
    /// pane's terminal viewport.
    ///
    /// Returns `None` when no GPU context is available, the cursor is outside
    /// the content area, or cell dimensions are zero.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "pixel / cell_size yields a small positive value fitting in u16"
    )]
    fn pixel_to_term_cell(&self, x: f32, y: f32) -> Option<(u16, u16)> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        if cell.width <= 0.0 || cell.height <= 0.0 {
            return None;
        }
        let tab_bar_h = self.focused_workspace_tab_bar_height();
        let tab = self.window_layout.active_tab()?;
        let pane = self.panes.get(&tab.focused_pane)?;
        let padding = pane::effective_padding(&self.config.appearance.content_padding, pane.edges);
        let (content_x, content_y) = pane.content_offset(tab_bar_h, &padding);
        let rel_x = x - content_x;
        let rel_y = y - content_y;
        if rel_x < 0.0 || rel_y < 0.0 {
            return None;
        }
        let col = ((rel_x / cell.width) as u16).min(pane.grid.cols.saturating_sub(1));
        let row = ((rel_y / cell.height) as u16).min(pane.grid.rows.saturating_sub(1));
        Some((col, row))
    }

    /// Send raw bytes to the focused pane's PTY session.
    fn send_bytes_to_focused_pane(&self, data: Vec<u8>) {
        let Some(tx) = self.cmd_tx.clone() else { return };
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
        send_command(&tx, ClientCommand::KeyInput { session_id: pane.session_id, data });
    }
}

// ---------------------------------------------------------------------------
// Tab drag helpers
// ---------------------------------------------------------------------------

/// Adjust per-tab slide offsets when a drag reorders tabs from `from` to `to`.
///
/// Tabs that are displaced by the reorder receive an initial offset so they
/// appear to start at their old position and then animate back to their new
/// logical position via exponential decay.
fn displace_tab_offsets(offsets: &mut [f32], from: usize, to: usize, tab_w_px: f32) {
    if from < to {
        // Dragged right: tabs in [from, to) are pushed one slot left.
        for offset in offsets.get_mut(from..to).unwrap_or(&mut []) {
            *offset += tab_w_px;
        }
    } else {
        // Dragged left: tabs in (to, from] are pushed one slot right.
        let end = from.min(offsets.len().saturating_sub(1));
        for offset in offsets.get_mut((to + 1)..=end).unwrap_or(&mut []) {
            *offset -= tab_w_px;
        }
    }
}

// ---------------------------------------------------------------------------
// Layout / resize helpers
// ---------------------------------------------------------------------------

impl App {
    /// Resize all panes to their computed rects and notify the server.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn resize_all_panes_from_rects(
        &mut self,
        rects: &[(PaneId, Rect, PaneEdges)],
        ws_rects: &[(WorkspaceId, Rect)],
    ) {
        let Some(gpu) = &self.gpu else { return };
        let cell = gpu.renderer.cell_size();

        // Build per-workspace tab bar heights so each pane uses the correct height.
        let ws_heights: std::collections::HashMap<WorkspaceId, f32> = ws_rects
            .iter()
            .map(|(ws_id, ws_rect)| (*ws_id, self.tab_bar_height_for(*ws_id, *ws_rect)))
            .collect();
        let fallback_h = self.effective_tab_bar_height();

        for (pane_id, rect, edges) in rects {
            let Some(pane) = self.panes.get_mut(pane_id) else { continue };
            pane.edges = *edges;
            let tab_bar_h = ws_heights.get(&pane.workspace_id).copied().unwrap_or(fallback_h);
            let eff_pad = pane::effective_padding(&self.config.appearance.content_padding, *edges);
            let grid = pane::compute_pane_grid(*rect, cell.width, cell.height, tab_bar_h, &eff_pad);
            pane.resize(*rect, grid);
        }
        self.resize_pending = Some(Instant::now());
    }

    /// Recompute rects and resize all panes after a layout change.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn resize_after_layout_change(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);

        let rects = self.window_layout.active_tab().map_or_else(Vec::new, |tab| {
            let ws_rect = ws_rects
                .iter()
                .find(|(wid, _)| *wid == self.window_layout.focused_workspace_id())
                .map_or(ws_viewport, |(_, r)| *r);
            tab.pane_layout.compute_rects(ws_rect)
        });

        self.resize_all_panes_from_rects(&rects, &ws_rects);
    }

    /// Recompute rects and resize panes in all workspaces.
    ///
    /// Used after workspace splits where the window is re-divided and every
    /// workspace region changes size.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn resize_all_workspace_panes(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let all_pane_rects: Vec<_> = ws_rects
            .iter()
            .filter_map(|(ws_id, ws_rect)| {
                let tab = self.window_layout.find_workspace(*ws_id)?.active_tab()?;
                Some(tab.pane_layout.compute_rects(*ws_rect))
            })
            .flatten()
            .collect();

        self.resize_all_panes_from_rects(&all_pane_rects, &ws_rects);
    }

    /// Request a redraw from winit.
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    // -- Window geometry persistence ----------------------------------------

    /// Mark that window geometry has changed and should be saved after debounce.
    fn mark_geometry_dirty(&mut self) {
        if self.geometry_save_pending.is_none() {
            self.geometry_save_pending = Some(Instant::now());
        }
    }

    /// Flush geometry to disk if the debounce interval has elapsed.
    fn flush_geometry_if_due(&mut self) {
        if self.geometry_save_pending.is_some_and(|t| t.elapsed() >= GEOMETRY_DEBOUNCE) {
            self.flush_geometry_now();
        }
    }

    /// Send any pending resize IPC messages if the debounce interval has elapsed.
    fn flush_resize_if_due(&mut self) {
        if self.resize_pending.is_none_or(|t| t.elapsed() < RESIZE_DEBOUNCE) {
            return;
        }
        let Some(tx) = &self.cmd_tx else {
            self.resize_pending = None;
            return;
        };
        let tx = tx.clone();
        for pane in self.panes.values_mut() {
            if pane.last_sent_grid != Some(pane.grid) {
                send_resize(&tx, pane.session_id, pane.grid.cols, pane.grid.rows);
                pane.last_sent_grid = Some(pane.grid);
            }
        }
        self.resize_pending = None;
    }

    // ── Multi-window ──────────────────────────────────────────────

    /// Handle `Welcome` from the server — store our window ID, apply saved
    /// geometry, and spawn other windows that need to be restored.
    fn handle_welcome(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        other_windows: &[WindowId],
    ) {
        self.window_id = Some(window_id);
        tracing::info!(%window_id, others = other_windows.len(), "assigned window ID");

        // If we didn't have a window_id at startup (fresh launch), load
        // geometry now that the server has assigned one. Uses the full
        // restore path (position + size + maximized) so that a restart
        // without --window-id still places the window correctly.
        if self.saved_geometry.is_none() {
            self.restore_geometry_from_registry(event_loop, window_id);
        }

        for &other_wid in other_windows {
            spawn_client_process(other_wid);
        }
    }

    /// Load saved geometry from the per-window registry and apply it.
    ///
    /// Called when the server assigns a window ID that wasn't known at
    /// startup (fresh launch / restart without `--window-id`).  If the
    /// server assigned an existing window ID (sessions are being restored),
    /// this is a resume scenario and full geometry (position + size +
    /// maximized) is restored.  For truly fresh installs the registry has
    /// no entry, so no geometry is applied and the OS decides placement.
    fn restore_geometry_from_registry(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
    ) {
        let loaded = self.window_registry.load(window_id);
        let has_saved = loaded.x.is_some() || loaded.maximized;
        let geom =
            if has_saved { Some(loaded) } else { self.window_registry.migrate_legacy(window_id) };
        if let (Some(geom), Some(window)) = (geom, &self.window) {
            window_state::apply_window_geometry(event_loop, window, &geom);
        }
    }

    /// Spawn a new window as a separate OS process.
    #[allow(clippy::unused_self, reason = "method for consistency with other handle_* methods")]
    fn handle_new_window(&mut self) {
        let new_id = WindowId::new();
        spawn_client_process(new_id);
        tracing::info!(%new_id, "spawning new window");
    }

    /// Handle the close button. Opens the in-app close dialog overlay
    /// with choices: cancel, quit Scribe (all windows), or kill this window.
    ///
    /// The dialog renders as a GPU overlay and intercepts all input events
    /// until the user makes a selection or presses Escape.
    fn handle_close_requested(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            self.flush_geometry_now();
            event_loop.exit();
            return;
        }

        // If the dialog is already open, treat another close request as cancel.
        if self.close_dialog.is_some() {
            return;
        }

        let session_count = self.panes.len();
        self.close_dialog = Some(close_dialog::CloseDialog::new(session_count));
        self.request_redraw();
    }

    /// Process a [`close_dialog::CloseAction`] from the in-app close dialog.
    fn handle_close_action(
        &mut self,
        action: close_dialog::CloseAction,
        event_loop: &ActiveEventLoop,
    ) {
        self.close_dialog = None;
        match action {
            close_dialog::CloseAction::QuitAll => self.handle_quit_all(event_loop),
            close_dialog::CloseAction::CloseWindow => self.handle_close_window(event_loop),
            close_dialog::CloseAction::Cancel => self.request_redraw(),
        }
    }

    // -------------------------------------------------------------------
    // Close dialog input handlers
    // -------------------------------------------------------------------

    /// Handle keyboard input while the close dialog is active.
    fn handle_dialog_keyboard(
        &mut self,
        event: &winit::event::KeyEvent,
        event_loop: &ActiveEventLoop,
    ) {
        use winit::keyboard::{Key, NamedKey};

        if event.state != winit::event::ElementState::Pressed {
            return;
        }

        match event.logical_key {
            Key::Named(NamedKey::Escape) => {
                let action = close_dialog::CloseAction::Cancel;
                self.handle_close_action(action, event_loop);
            }
            Key::Named(NamedKey::Enter) => {
                let action = self
                    .close_dialog
                    .as_ref()
                    .map_or(close_dialog::CloseAction::Cancel, close_dialog::CloseDialog::confirm);
                self.handle_close_action(action, event_loop);
            }
            Key::Named(NamedKey::Tab) => {
                self.cycle_dialog_focus();
            }
            _ => {}
        }
    }

    /// Handle mouse click while the close dialog is active.
    fn handle_dialog_click(&mut self, event_loop: &ActiveEventLoop) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let action = self.close_dialog.as_ref().and_then(|d| d.click(x, y));
        if let Some(action) = action {
            self.handle_close_action(action, event_loop);
        }
    }

    /// Handle mouse hover while the close dialog is active.
    fn handle_dialog_hover(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        if let Some(dialog) = &mut self.close_dialog {
            if dialog.update_hover(x, y) {
                self.request_redraw();
            }
        }
    }

    /// Cycle dialog button focus (Tab / Shift+Tab).
    fn cycle_dialog_focus(&mut self) {
        let Some(dialog) = &mut self.close_dialog else { return };
        if self.modifiers.shift_key() {
            dialog.focus_prev();
        } else {
            dialog.focus_next();
        }
        self.request_redraw();
    }

    // -------------------------------------------------------------------
    // Update dialog input handlers
    // -------------------------------------------------------------------

    /// Open the update confirmation dialog.
    fn open_update_dialog(&mut self) {
        if self.update_dialog.is_some() {
            return;
        }
        let Some((version, release_url)) = self.update_available.clone() else { return };
        self.update_dialog = Some(update_dialog::UpdateDialog::new(version, release_url));
        self.request_redraw();
    }

    /// Process an [`update_dialog::UpdateAction`] from the in-app update dialog.
    fn handle_update_action(&mut self, action: update_dialog::UpdateAction) {
        self.update_dialog = None;
        match action {
            update_dialog::UpdateAction::Confirm => {
                tracing::info!("user confirmed update");
                self.update_available = None;
                if let Some(tx) = &self.cmd_tx {
                    send_command(tx, ClientCommand::TriggerUpdate);
                }
            }
            update_dialog::UpdateAction::Dismiss => {
                tracing::info!("user dismissed update");
                self.update_available = None;
                self.update_progress = None;
                if let Some(tx) = &self.cmd_tx {
                    send_command(tx, ClientCommand::DismissUpdate);
                }
            }
        }
        self.request_redraw();
    }

    /// Handle keyboard input while the update dialog is active.
    fn handle_update_dialog_keyboard(&mut self, event: &winit::event::KeyEvent) {
        use winit::keyboard::{Key, NamedKey};

        if event.state != winit::event::ElementState::Pressed {
            return;
        }

        match event.logical_key {
            Key::Named(NamedKey::Escape) => {
                let action = update_dialog::UpdateAction::Dismiss;
                self.handle_update_action(action);
            }
            Key::Named(NamedKey::Enter) => {
                let action = self.update_dialog.as_ref().map_or(
                    update_dialog::UpdateAction::Dismiss,
                    update_dialog::UpdateDialog::confirm,
                );
                self.handle_update_action(action);
            }
            Key::Named(NamedKey::Tab) => {
                self.cycle_update_dialog_focus();
            }
            _ => {}
        }
    }

    /// Handle mouse click while the update dialog is active.
    fn handle_update_dialog_click(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        let action = self.update_dialog.as_ref().and_then(|d| d.click(x, y));
        if let Some(action) = action {
            self.handle_update_action(action);
        }
    }

    /// Handle mouse hover while the update dialog is active.
    fn handle_update_dialog_hover(&mut self) {
        let Some((x, y)) = self.last_cursor_pos else { return };
        if let Some(dialog) = &mut self.update_dialog {
            if dialog.update_hover(x, y) {
                self.request_redraw();
            }
        }
    }

    /// Cycle update dialog button focus (Tab / Shift+Tab).
    fn cycle_update_dialog_focus(&mut self) {
        let Some(dialog) = &mut self.update_dialog else { return };
        if self.modifiers.shift_key() {
            dialog.focus_prev();
        } else {
            dialog.focus_next();
        }
        self.request_redraw();
    }

    /// Handle `QuitRequested` from the server — save state and close.
    ///
    /// Does NOT re-broadcast `QuitAll`: only the originating window
    /// (via `handle_quit_all`) sends that.
    fn handle_quit_requested(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("quit requested by another window — saving and exiting");
        self.flush_geometry_now();
        event_loop.exit();
    }

    /// User chose "Quit Scribe" — broadcast to all windows, then exit.
    fn handle_quit_all(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("quit all — broadcasting to other windows");
        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::QuitAll);
        }
        quit_settings_process();
        self.flush_geometry_now();
        event_loop.exit();
    }

    /// User chose "Close this window only" — tell the server to destroy all
    /// sessions belonging to this window, remove geometry file, and exit.
    fn handle_close_window(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("closing window permanently");
        // Tell the server to destroy all sessions owned by this window so
        // they don't get resurrected on the next launch.
        if let Some(wid) = self.window_id {
            if let Some(tx) = &self.cmd_tx {
                send_command(tx, ClientCommand::CloseWindow { window_id: wid });
            }
            self.window_registry.remove(wid);
        }
        event_loop.exit();
    }

    fn flush_geometry_now(&mut self) {
        let Some(window) = &self.window else { return };
        let Some(wid) = self.window_id else { return };
        let geom = window_state::capture_window_geometry(window);
        if let Err(e) = self.window_registry.save(wid, &geom) {
            tracing::warn!("failed to persist window geometry: {e}");
        }
        self.geometry_save_pending = None;
    }

    /// Start the animation timer thread for AI state pulsing.
    fn start_animation_timer(&mut self) {
        if self.animation_running {
            return;
        }
        self.animation_running = true;
        self.animation_stop.store(true, Ordering::Relaxed);
        self.last_tick = Instant::now();

        let Some(proxy) = self.animation_proxy.clone() else { return };
        let stop = Arc::clone(&self.animation_stop);
        std::thread::spawn(move || run_animation_loop(proxy, stop));
    }
}

// ---------------------------------------------------------------------------
// Animation timer
// ---------------------------------------------------------------------------

/// Run the 30 fps animation loop, sending `AnimationTick` events to the
/// winit event loop until it closes.
///
/// The proxy is passed by value because this function runs on a dedicated
/// thread that must own the proxy for its `'static` lifetime.
#[allow(
    clippy::needless_pass_by_value,
    reason = "proxy must be owned by this thread; it is moved into std::thread::spawn"
)]
fn run_animation_loop(proxy: EventLoopProxy<UiEvent>, stop: Arc<AtomicBool>) {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(33));
        if !stop.load(Ordering::Relaxed) {
            break;
        }
        if proxy.send_event(UiEvent::AnimationTick).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Session metadata helpers
// ---------------------------------------------------------------------------

/// Apply stored title and CWD from a metadata lookup to a newly created pane.
/// Called during reconnection so panes display the last-known tab name instead
/// of the default "shell".
fn apply_session_metadata(
    pane: &mut Pane,
    metadata: &HashMap<SessionId, (Option<&str>, Option<&std::path::PathBuf>)>,
) {
    if let Some(&(title, cwd)) = metadata.get(&pane.session_id) {
        if let Some(title) = title {
            title.clone_into(&mut pane.title);
        }
        if let Some(cwd) = cwd {
            pane.cwd = Some((*cwd).clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Instance compositing
// ---------------------------------------------------------------------------

/// Compute the pixel X range `(start, end)` of the active tab on row 0 of the tab bar.
///
/// Returns `None` when the active tab is on a row other than row 0 (multi-row bar),
/// when there are no tabs, or when the cell width is zero.
#[allow(
    clippy::cast_precision_loss,
    reason = "column counts are small positive integers fitting in f32"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "all parameters are needed to compute the active tab pixel range for bg coloring"
)]
fn compute_active_tab_pixel_range(
    ws_rect_x: f32,
    active_tab_idx: usize,
    tab_width: u16,
    badge_cols: usize,
    tabs_per_row: usize,
    cell_w: f32,
) -> Option<(f32, f32)> {
    if cell_w <= 0.0 || tabs_per_row == 0 {
        return None;
    }
    // Active tab must be on row 0.
    if active_tab_idx >= tabs_per_row {
        return None;
    }
    let tab_w = usize::from(tab_width).max(1);
    let start_col = badge_cols + active_tab_idx * tab_w;
    let end_col = start_col + tab_w;
    Some((ws_rect_x + start_col as f32 * cell_w, ws_rect_x + end_col as f32 * cell_w))
}

/// Look up the tab bar height for a pane by its workspace id.
/// Falls back to the first available workspace height, or 0.0.
fn pane_tab_bar_h(
    pane_ws: WorkspaceId,
    heights: &HashMap<WorkspaceId, f32>,
    ws_data: &[tab_bar::WorkspaceTabBarData],
) -> f32 {
    heights
        .get(&pane_ws)
        .copied()
        .unwrap_or_else(|| ws_data.first().map_or(0.0, |d| d.tab_bar_height))
}

/// Look up the active tab pixel X range for a pane's workspace.
fn pane_active_range(
    pane_ws: WorkspaceId,
    ws_data: &[tab_bar::WorkspaceTabBarData],
) -> Option<(f32, f32)> {
    ws_data.iter().find(|d| d.ws_id == pane_ws)?.active_tab_pixel_range
}

/// Minimum time the splash screen stays visible, ensuring the compositor
/// presents it before the terminal content frame overwrites it.  On X11,
/// `request_redraw` does not respect vsync pacing, so without a floor the
/// splash and content frames can both land in the same vsync window and
/// only the content frame is ever displayed.
const MIN_SPLASH_DURATION: Duration = Duration::from_millis(50);

/// Cursor blink interval (530ms matches xterm/VTE).
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Debounce interval for geometry saves (move/resize events fire rapidly).
const GEOMETRY_DEBOUNCE: Duration = Duration::from_millis(500);

/// Debounce interval for resize IPC sends (window drag fires rapidly).
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(30);

/// Dimming factor applied to RGB channels of unfocused pane content.
const UNFOCUSED_DIM: f32 = 0.85;

/// Collect all cell instances (tab bars + terminals + dividers + AI borders)
/// into one buffer.
/// `(workspace_id, tab_index, clickable_rect)` for tab bar click handling.
type TabHitTargets = Vec<(WorkspaceId, usize, layout::Rect)>;

/// `(workspace_id, equalize_rect)` for tab bar equalize button click handling.
type TabEqualizeTargets = Vec<(WorkspaceId, layout::Rect)>;

/// `(workspace_id, update_rect)` for tab bar update button click handling.
type TabUpdateTargets = Vec<(WorkspaceId, layout::Rect)>;

/// Layout and focus state passed to [`build_all_instances`].
struct FrameLayout<'a> {
    pane_rects: &'a [(PaneId, Rect)],
    dividers: &'a [divider::Divider],
    ws_dividers: &'a [workspace_layout::WorkspaceDivider],
    ws_tab_bar_data: &'a [tab_bar::WorkspaceTabBarData],
    cell_size: (f32, f32),
    focused_pane: PaneId,
    focus_split_direction: Option<layout::SplitDirection>,
    padding: &'a ContentPadding,
}

/// Colors and visual styling passed to [`build_all_instances`].
struct FrameStyle<'a> {
    border_colors: &'a HashMap<PaneId, [f32; 4]>,
    tab_colors: &'a tab_bar::TabBarColors,
    divider_color: [f32; 4],
    accent_color: [f32; 4],
    scrollbar_width: f32,
    scrollbar_color: [f32; 4],
    indicator_height: f32,
}

/// Interaction state passed to [`build_all_instances`].
struct FrameInteraction<'a> {
    cursor_visible: bool,
    tab_width: u16,
    active_selection: Option<&'a selection::SelectionRange>,
    hovered_tab_close: Option<(WorkspaceId, usize)>,
    tab_drag: Option<&'a TabDrag>,
    tab_drag_offsets: &'a [f32],
    /// Version string of available update. `None` when no update available.
    update_available: Option<&'a str>,
    /// Current update progress state. `None` when idle.
    update_progress: Option<&'a UpdateProgressState>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "renderer, device, queue, panes plus 3 grouped structs — splitting further would add indirection"
)]
#[allow(
    clippy::too_many_lines,
    reason = "single render-pass collector: tab bars, terminals, dividers, AI borders"
)]
#[allow(
    clippy::cognitive_complexity,
    reason = "single render-pass function; extracting sub-functions would add indirection without clarity"
)]
#[allow(
    clippy::excessive_nesting,
    reason = "damage-tracking cache check adds one nesting level inside the pane loop; extracting would require passing many parameters"
)]
fn build_all_instances(
    renderer: &mut TerminalRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    panes: &mut HashMap<PaneId, Pane>,
    layout: &FrameLayout<'_>,
    style: &FrameStyle<'_>,
    interaction: &FrameInteraction<'_>,
) -> (
    Vec<scribe_renderer::types::CellInstance>,
    TabHitTargets,
    TabHitTargets,
    TabEqualizeTargets,
    TabUpdateTargets,
) {
    // Build a workspace-id → tab_bar_height lookup for per-pane height queries.
    let ws_tab_bar_heights: HashMap<WorkspaceId, f32> =
        layout.ws_tab_bar_data.iter().map(|d| (d.ws_id, d.tab_bar_height)).collect();

    // Pre-allocate based on a typical 80x24 grid per pane plus tab bar and
    // border quads, to avoid repeated reallocations during the per-pane loops.
    let estimated_per_pane = 80 * 24 + 80 + 4;
    let mut all_instances = Vec::with_capacity(layout.pane_rects.len() * estimated_per_pane);

    let default_bg = renderer.default_bg();

    // Terminal content first — tab bar is drawn on top afterwards so that any
    // content that bleeds into the tab bar region (rounding, partial rows) is
    // covered by the opaque tab bar background.
    let has_multiple_panes = layout.pane_rects.len() > 1;
    let selection_colors = (renderer.selection_bg(), renderer.selection_fg());
    // Non-empty selection for the focused pane (precompute to avoid nesting).
    let effective_selection = interaction.active_selection.filter(|s| !s.is_empty());
    for (pane_id, _) in layout.pane_rects {
        if let Some(pane) = panes.get_mut(pane_id) {
            let tbh =
                pane_tab_bar_h(pane.workspace_id, &ws_tab_bar_heights, layout.ws_tab_bar_data);
            let offset = pane.content_offset(tbh, layout.padding);
            // Only the focused pane shows the blinking cursor; unfocused panes hide it.
            let is_focused = *pane_id == layout.focused_pane;
            let pane_cursor_visible = is_focused && interaction.cursor_visible;
            let pane_has_selection = is_focused && effective_selection.is_some();

            // Background fill covering the full content area — emitted before
            // cell instances so cells draw on top.  Covers remainder pixels at
            // right/bottom edges left by floor-division of pixels by cell size.
            let dim = has_multiple_panes && !is_focused;
            push_pane_bg_fill(&mut all_instances, pane, tbh, (default_bg, layout.padding, dim));

            // Skip rebuilding instances when the pane content and all
            // rendering context (cursor blink, focus, selection) are unchanged.
            let needs_rebuild = pane.content_dirty
                || pane.last_cursor_visible != Some(pane_cursor_visible)
                || pane.last_was_focused != Some(is_focused)
                || pane.last_had_selection != pane_has_selection;

            if needs_rebuild {
                let mut instances = renderer.build_instances_at(
                    device,
                    queue,
                    &mut pane.term,
                    offset,
                    pane_cursor_visible,
                );
                if let Some(sel) = effective_selection.filter(|_| is_focused) {
                    let disp_off = pane.term.grid().display_offset();
                    apply_selection_highlight(
                        &mut instances,
                        offset,
                        layout.cell_size,
                        sel,
                        selection_colors,
                        disp_off,
                    );
                }
                if dim {
                    dim_instances(&mut instances);
                }
                all_instances.extend_from_slice(&instances);
                std::mem::swap(&mut pane.last_instances, &mut instances);
                pane.content_dirty = false;
                pane.last_cursor_visible = Some(pane_cursor_visible);
                pane.last_was_focused = Some(is_focused);
                pane.last_had_selection = pane_has_selection;
            } else {
                all_instances.extend_from_slice(&pane.last_instances);
            }
        }
    }

    // Tab bar backgrounds (drawn after terminal content so the opaque bar
    // always covers any stray cells that extend into its area).
    // The separator is drawn later, after build_tab_bar_text, using the exact
    // active-tab column range returned by the render pass (Issue 2 fix).
    for (pane_id, pane_rect) in layout.pane_rects {
        let (tbh, active_range) = panes.get(pane_id).map_or_else(
            || {
                let h = layout.ws_tab_bar_data.first().map_or(0.0, |d| d.tab_bar_height);
                (h, None)
            },
            |p| {
                let h = pane_tab_bar_h(p.workspace_id, &ws_tab_bar_heights, layout.ws_tab_bar_data);
                let r = pane_active_range(p.workspace_id, layout.ws_tab_bar_data);
                (h, r)
            },
        );
        tab_bar::build_tab_bar_bg(
            &mut all_instances,
            *pane_rect,
            layout.cell_size,
            style.tab_colors,
            tbh,
            active_range,
        );
    }

    // Tab bar text — rendered once per workspace, spanning the full workspace width.
    let mut tab_hit_targets: Vec<(WorkspaceId, usize, layout::Rect)> = Vec::new();
    let mut tab_close_hit_targets: Vec<(WorkspaceId, usize, layout::Rect)> = Vec::new();
    let mut tab_equalize_targets: TabEqualizeTargets = Vec::new();
    let mut tab_update_targets: TabUpdateTargets = Vec::new();
    for ws_data in layout.ws_tab_bar_data {
        let tbh = ws_data.tab_bar_height;
        let tab_bar_rect = layout::Rect {
            x: ws_data.ws_rect.x,
            y: ws_data.ws_rect.y,
            width: ws_data.ws_rect.width,
            height: tbh,
        };
        let badge = ws_data.badge.as_ref().map(|(name, color)| (name.as_str(), *color));
        let mut resolve_glyph = |ch: char| renderer.resolve_glyph(device, queue, ch);
        // Pass the hovered close index only for this workspace.
        let ws_hovered_close = interaction
            .hovered_tab_close
            .and_then(|(ws, idx)| if ws == ws_data.ws_id { Some(idx) } else { None });
        // Drag state for this workspace only.
        let ws_drag =
            interaction.tab_drag.filter(|d| d.workspace_id == ws_data.ws_id && d.dragging);
        let ws_tab_offsets = if ws_drag.is_some() { interaction.tab_drag_offsets } else { &[] };
        let ws_dragging_tab = ws_drag.map(|d| d.tab_index);
        let ws_drag_cursor_x = ws_drag.map_or(0.0, |d| d.cursor_x);
        let ws_drag_grab_offset = ws_drag.map_or(0.0, |d| d.grab_offset_x);
        let mut params = tab_bar::TabBarTextParams {
            rect: tab_bar_rect,
            cell_size: layout.cell_size,
            tabs: &ws_data.tabs,
            badge,
            show_gear: false,
            show_equalize: ws_data.has_multiple_panes,
            colors: style.tab_colors,
            resolve_glyph: &mut resolve_glyph,
            tab_bar_height: tbh,
            indicator_height: style.indicator_height,
            tab_width: interaction.tab_width,
            update_available: interaction.update_available,
            update_progress: interaction.update_progress,
            hovered_tab_close: ws_hovered_close,
            tab_offsets: ws_tab_offsets,
            dragging_tab: ws_dragging_tab,
            drag_cursor_x: ws_drag_cursor_x,
            drag_grab_offset: ws_drag_grab_offset,
            accent_color: style.accent_color,
        };
        let (text_instances, hit_targets) = tab_bar::build_tab_bar_text(&mut params);
        all_instances.extend(text_instances);
        for (tab_idx, rect) in hit_targets.tab_rects {
            tab_hit_targets.push((ws_data.ws_id, tab_idx, rect));
        }
        for (tab_idx, rect) in hit_targets.close_rects {
            tab_close_hit_targets.push((ws_data.ws_id, tab_idx, rect));
        }
        if let Some(eq_rect) = hit_targets.equalize_rect {
            tab_equalize_targets.push((ws_data.ws_id, eq_rect));
        }
        if let Some(upd_rect) = hit_targets.update_rect {
            tab_update_targets.push((ws_data.ws_id, upd_rect));
        }

        // Draw the bottom separator using the exact active-tab column range
        // returned by the render pass.  This avoids the pre-computation error
        // where update-button columns were not accounted for (Issue 2 fix).
        let (cell_w, _) = layout.cell_size;
        #[allow(
            clippy::cast_precision_loss,
            reason = "column indices are small positive integers fitting in f32"
        )]
        let exact_active_range = hit_targets.active_tab_col_range.map(|(sc, ec)| {
            (ws_data.ws_rect.x + sc as f32 * cell_w, ws_data.ws_rect.x + ec as f32 * cell_w)
        });
        tab_bar::build_tab_bar_separator(
            &mut all_instances,
            tab_bar_rect,
            layout.cell_size,
            style.divider_color,
            tbh,
            exact_active_range,
        );
    }

    // Pane dividers.
    divider::build_divider_instances(&mut all_instances, layout.dividers, style.divider_color);
    // Workspace dividers — rendered directly as single quads.
    for ws_div in layout.ws_dividers {
        all_instances.push(scribe_renderer::types::CellInstance {
            pos: [ws_div.rect.x, ws_div.rect.y],
            size: [ws_div.rect.width, ws_div.rect.height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: style.divider_color,
            bg_color: style.divider_color,
        });
    }

    // Scrollbar overlays.
    for (pane_id, _) in layout.pane_rects {
        if let Some(pane) = panes.get(pane_id) {
            let tbh =
                pane_tab_bar_h(pane.workspace_id, &ws_tab_bar_heights, layout.ws_tab_bar_data);
            scrollbar::build_scrollbar_instances(
                &mut all_instances,
                pane,
                style.scrollbar_width,
                style.scrollbar_color,
                tbh,
            );
        }
    }

    // Focus border on the focused pane's leading edge.
    if has_multiple_panes {
        if let Some((_, focused_rect)) =
            layout.pane_rects.iter().find(|(id, _)| *id == layout.focused_pane)
        {
            divider::build_focus_border(
                &mut all_instances,
                *focused_rect,
                layout.focus_split_direction,
                style.accent_color,
                layout.cell_size,
            );
        }
    }

    // AI state border overlays (rendered last so they appear on top).
    // Border wraps the terminal content area only, excluding the tab bar.
    for (pane_id, pane_rect) in layout.pane_rects {
        if let Some(&color) = style.border_colors.get(pane_id) {
            let tbh = panes.get(pane_id).map_or_else(
                || layout.ws_tab_bar_data.first().map_or(0.0, |d| d.tab_bar_height),
                |p| pane_tab_bar_h(p.workspace_id, &ws_tab_bar_heights, layout.ws_tab_bar_data),
            );
            let border = ai_indicator::build_border_instances(*pane_rect, color, tbh);
            all_instances.extend(border);
        }
    }

    (
        all_instances,
        tab_hit_targets,
        tab_close_hit_targets,
        tab_equalize_targets,
        tab_update_targets,
    )
}

/// Emit one solid-colour quad covering a pane's full content area.
///
/// Covers remainder pixels at right/bottom edges left by floor-dividing pixel
/// dimensions by cell size.  Must be pushed before cell instances so cells
/// render on top.  Applies unfocused dimming when `dim` is true.
#[allow(clippy::indexing_slicing, reason = "fixed-size [f32; 4] array, indices 0-2 always valid")]
fn push_pane_bg_fill(
    out: &mut Vec<scribe_renderer::types::CellInstance>,
    pane: &Pane,
    tab_bar_height: f32,
    bg_and_dim: ([f32; 4], &ContentPadding, bool),
) {
    let (default_bg, padding, dim) = bg_and_dim;
    let eff_pad = pane::effective_padding(padding, pane.edges);
    let content_x = pane.rect.x + eff_pad.left;
    let content_y = pane.rect.y + tab_bar_height + eff_pad.top;
    let content_w = (pane.rect.width - eff_pad.left - eff_pad.right).max(0.0);
    let content_h = (pane.rect.height - tab_bar_height - eff_pad.top - eff_pad.bottom).max(0.0);
    if content_w <= 0.0 || content_h <= 0.0 {
        return;
    }
    let mut bg = default_bg;
    if dim {
        bg[0] *= UNFOCUSED_DIM;
        bg[1] *= UNFOCUSED_DIM;
        bg[2] *= UNFOCUSED_DIM;
    }
    out.push(scribe_renderer::types::CellInstance {
        pos: [content_x, content_y],
        size: [content_w, content_h],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: bg,
        bg_color: bg,
    });
}

/// Apply window opacity to cell background alpha values.
///
/// Foreground glyphs are left fully opaque so text remains readable.
#[allow(clippy::indexing_slicing, reason = "fixed-size [f32; 4] array, index 3 always valid")]
fn apply_opacity_to_instances(
    instances: &mut [scribe_renderer::types::CellInstance],
    opacity: f32,
) {
    for inst in instances {
        inst.bg_color[3] *= opacity;
    }
}

/// Dim cell instances for unfocused panes by scaling RGB channels.
///
/// Alpha is left unchanged so partially transparent elements remain correct.
fn dim_instances(instances: &mut [scribe_renderer::types::CellInstance]) {
    for inst in instances {
        dim_color(&mut inst.fg_color);
        dim_color(&mut inst.bg_color);
    }
}

/// Multiply the RGB channels of a colour by [`UNFOCUSED_DIM`], keeping alpha.
#[allow(clippy::indexing_slicing, reason = "fixed-size [f32; 4] array, indices 0-2 always valid")]
fn dim_color(color: &mut [f32; 4]) {
    color[0] *= UNFOCUSED_DIM;
    color[1] *= UNFOCUSED_DIM;
    color[2] *= UNFOCUSED_DIM;
}

/// Selection highlight colors: `(background, foreground)`.
type SelectionColors = ([f32; 4], [f32; 4]);

/// Apply selection highlight to cell instances for the focused pane.
///
/// Reverse-maps each instance's pixel position to absolute grid coordinates
/// and checks whether it falls within the selection range.  Selected cells
/// get the selection background and foreground colors applied.
#[allow(
    clippy::too_many_arguments,
    reason = "needs offset, cell size, selection, colors, and scroll offset for absolute coordinate mapping"
)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "grid coordinates derived from pixel / cell_size are small positive values; \
              display_offset bounded by scrollback_lines (≤ 100_000)"
)]
fn apply_selection_highlight(
    instances: &mut [scribe_renderer::types::CellInstance],
    offset: (f32, f32),
    cell_size: (f32, f32),
    sel: &selection::SelectionRange,
    colors: SelectionColors,
    display_offset: usize,
) {
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    let offset_i32 = display_offset as i32;
    for inst in instances {
        // Skip overlay quads (beam/underline cursor) — they have non-zero size.
        if inst.size[0] != 0.0 || inst.size[1] != 0.0 {
            continue;
        }
        let col = ((inst.pos[0] - offset.0) / cell_w + 0.5) as usize;
        let screen_row = ((inst.pos[1] - offset.1) / cell_h + 0.5) as i32;
        // Convert screen row to absolute grid line to match selection coordinates.
        let grid_row = screen_row - offset_i32;
        if sel.contains_cell(grid_row, col) {
            inst.bg_color = colors.0;
            inst.fg_color = colors.1;
        }
    }
}

/// Refresh the URL cache for `pane_id` and return the URL span at `point`.
///
/// `panes` and `url_caches` are passed as separate parameters so the borrow
/// checker can verify they are independent — the same pattern used by
/// `apply_url_underlines`.
fn hovered_url_at(
    point: selection::SelectionPoint,
    pane_id: PaneId,
    panes: &HashMap<PaneId, Pane>,
    url_caches: &mut HashMap<PaneId, url_detect::PaneUrlCache>,
) -> Option<url_detect::UrlSpan> {
    let pane = panes.get(&pane_id)?;
    let cache = url_caches.get_mut(&pane_id)?;
    cache.refresh(&pane.term);
    cache.url_at(point.row, point.col).map(|span| url_detect::UrlSpan {
        row: span.row,
        col_start: span.col_start,
        col_end: span.col_end,
        url: span.url.clone(),
    })
}

/// Return `true` if two `Option<UrlSpan>` values point to different URL spans.
fn url_span_changed(old: Option<&url_detect::UrlSpan>, new: Option<&url_detect::UrlSpan>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(prev), Some(next)) => prev.row != next.row || prev.col_start != next.col_start,
        _ => true,
    }
}

/// Underline thickness for URL spans (pixels).
const URL_UNDERLINE_HEIGHT: f32 = 1.5;

/// URL underline color (unhovered): subtle blue tint.
const URL_UNDERLINE_COLOR: [f32; 4] = [0.5, 0.7, 1.0, 0.7];

/// URL underline color when the cursor is over this URL.
const URL_UNDERLINE_HOVER_COLOR: [f32; 4] = [0.4, 0.8, 1.0, 1.0];

/// Push URL underline quad instances for all visible URL spans in each pane.
///
/// Refreshes dirty URL caches before rendering (lazy re-scan). For each URL
/// span, a thin horizontal bar is drawn at the bottom of each spanned cell.
#[allow(
    clippy::too_many_arguments,
    reason = "needs pane data, url caches, layout geometry, and hovered url for rendering"
)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "display_offset bounded by scrollback_lines (100_000), screen_row checked >= 0"
)]
fn apply_url_underlines(
    instances: &mut Vec<scribe_renderer::types::CellInstance>,
    url_caches: &mut HashMap<PaneId, url_detect::PaneUrlCache>,
    panes: &HashMap<PaneId, Pane>,
    pane_rects: &[(PaneId, Rect)],
    ws_tab_bar_heights: &HashMap<WorkspaceId, f32>,
    fallback_tbh: f32,
    cell_size: (f32, f32),
    hovered_url: Option<&url_detect::UrlSpan>,
    padding: &ContentPadding,
) {
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }
    let ul_h = URL_UNDERLINE_HEIGHT.max(1.0);

    for (pane_id, _) in pane_rects {
        let Some(pane) = panes.get(pane_id) else { continue };
        let tbh = ws_tab_bar_heights.get(&pane.workspace_id).copied().unwrap_or(fallback_tbh);
        let offset = pane.content_offset(tbh, padding);
        let display_offset = pane.term.grid().display_offset() as i32;

        let Some(cache) = url_caches.get_mut(pane_id) else { continue };
        // `panes` and `url_caches` are separate parameters — no aliasing.
        cache.refresh(&pane.term);

        for span in cache.visible_spans() {
            let is_hovered =
                hovered_url.is_some_and(|h| h.row == span.row && h.col_start == span.col_start);
            let color = if is_hovered { URL_UNDERLINE_HOVER_COLOR } else { URL_UNDERLINE_COLOR };

            // Convert absolute row to screen row.
            let screen_row = span.row + display_offset;
            if screen_row < 0 {
                continue;
            }
            #[allow(
                clippy::cast_precision_loss,
                reason = "screen_row bounded by terminal rows (≤ 100_000); col values bounded by terminal columns"
            )]
            let y_top = offset.1 + screen_row as f32 * cell_h + cell_h - ul_h;
            if span.col_start > span.col_end {
                continue;
            }
            #[allow(
                clippy::cast_precision_loss,
                reason = "col values bounded by terminal columns (≤ 500), precision loss negligible"
            )]
            let (span_cols, col_x) =
                ((span.col_end - span.col_start + 1) as f32, span.col_start as f32);
            let x = offset.0 + col_x * cell_w;

            instances.push(scribe_renderer::types::CellInstance {
                pos: [x, y_top],
                size: [span_cols * cell_w, ul_h],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: color,
                bg_color: color,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// wgpu device / surface configuration
// ---------------------------------------------------------------------------

/// Select the best `CompositeAlphaMode` for the given transparency requirement.
///
/// When transparency is needed, prefer `PreMultiplied` then `PostMultiplied`.
/// Falls back to the first available mode (or `Auto`) when no preferred mode
/// is available or transparency is not required.
fn select_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
    transparent: bool,
) -> wgpu::CompositeAlphaMode {
    if transparent {
        if modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            return wgpu::CompositeAlphaMode::PreMultiplied;
        }
        if modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            return wgpu::CompositeAlphaMode::PostMultiplied;
        }
        tracing::warn!("no transparency-capable alpha mode available");
    }
    modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
}

/// Request adapter, create device + queue, and configure the surface.
fn configure_device_and_surface(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'_>,
    window: &Window,
    transparent: bool,
) -> Result<(wgpu::Device, wgpu::Queue, wgpu::SurfaceConfiguration), InitError> {
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: Some(surface),
        force_fallback_adapter: false,
    }))
    .map_err(|e| InitError::Adapter(e.to_string()))?;

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("scribe device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    }))
    .map_err(|e| InitError::Device(e.to_string()))?;

    let size = window.inner_size();
    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .find(|f| f.is_srgb())
        .or_else(|| caps.formats.first())
        .copied()
        .ok_or(InitError::NoSurfaceFormat)?;
    let alpha_mode = select_alpha_mode(&caps.alpha_modes, transparent);

    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };

    surface.configure(&device, &config);
    Ok((device, queue, config))
}

// ---------------------------------------------------------------------------
// Initialisation error
// ---------------------------------------------------------------------------

/// Errors that can occur during GPU / terminal initialisation.
#[derive(Debug, thiserror::Error)]
enum InitError {
    #[error("window creation failed: {0}")]
    Window(winit::error::OsError),
    #[error("surface creation failed: {0}")]
    Surface(wgpu::CreateSurfaceError),
    #[error("adapter request failed: {0}")]
    Adapter(String),
    #[error("device request failed: {0}")]
    Device(String),
    #[error("no compatible surface format")]
    NoSurfaceFormat,
    #[error("event loop proxy already consumed")]
    ProxyConsumed,
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn send_resize(tx: &Sender<ClientCommand>, session_id: SessionId, cols: u16, rows: u16) {
    if tx.send(ClientCommand::Resize { session_id, cols, rows }).is_err() {
        tracing::warn!("IPC channel closed; resize dropped");
    }
}

fn send_command(tx: &Sender<ClientCommand>, cmd: ClientCommand) {
    if tx.send(cmd).is_err() {
        tracing::warn!("IPC channel closed; command dropped");
    }
}

// ---------------------------------------------------------------------------
// Direction conversion helpers
// ---------------------------------------------------------------------------

/// Convert client-side `SplitDirection` to the protocol `LayoutDirection`.
fn to_layout_direction(d: layout::SplitDirection) -> scribe_common::protocol::LayoutDirection {
    match d {
        layout::SplitDirection::Horizontal => scribe_common::protocol::LayoutDirection::Horizontal,
        layout::SplitDirection::Vertical => scribe_common::protocol::LayoutDirection::Vertical,
    }
}

/// Convert protocol `LayoutDirection` back to client-side `SplitDirection`.
fn from_layout_direction(d: scribe_common::protocol::LayoutDirection) -> layout::SplitDirection {
    match d {
        scribe_common::protocol::LayoutDirection::Horizontal => layout::SplitDirection::Horizontal,
        scribe_common::protocol::LayoutDirection::Vertical => layout::SplitDirection::Vertical,
    }
}

/// Build a viewport `Rect` from the surface configuration.
#[allow(clippy::cast_precision_loss, reason = "viewport dimensions are small enough to fit in f32")]
fn viewport_rect(config: &wgpu::SurfaceConfiguration) -> Rect {
    Rect { x: 0.0, y: 0.0, width: config.width as f32, height: config.height as f32 }
}

/// Return the viewport rect available to workspaces — full surface minus
/// the window-level status bar at the bottom.
#[allow(clippy::cast_precision_loss, reason = "viewport dimensions are small enough to fit in f32")]
fn workspace_viewport(config: &wgpu::SurfaceConfiguration) -> Rect {
    Rect {
        x: 0.0,
        y: 0.0,
        width: config.width as f32,
        height: (config.height as f32 - status_bar::STATUS_BAR_HEIGHT).max(1.0),
    }
}

/// Collect the expected `(PaneId, Rect, PaneEdges)` tuples for every active tab in
/// every workspace, using the provided workspace rects.
///
/// This flattens the workspace → tab → pane hierarchy into a single vec
/// so callers can iterate without deep nesting.
fn collect_expected_pane_rects(
    layout: &workspace_layout::WindowLayout,
    ws_rects: &[(WorkspaceId, Rect)],
) -> Vec<(PaneId, Rect, PaneEdges)> {
    ws_rects
        .iter()
        .filter_map(|(ws_id, ws_rect)| {
            let tab = layout.find_workspace(*ws_id)?.active_tab()?;
            Some(tab.pane_layout.compute_rects(*ws_rect))
        })
        .flatten()
        .collect()
}

/// Read the system hostname via `gethostname(2)`, falling back to "localhost".
#[allow(
    unsafe_code,
    reason = "gethostname writes into a caller-owned buffer with a known size limit"
)]
fn read_hostname() -> String {
    let mut buf = [0u8; 256];
    #[allow(clippy::cast_possible_wrap, reason = "buffer length fits in libc::c_int / size_t")]
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.get(..len).map_or_else(
            || String::from("localhost"),
            |slice| String::from_utf8_lossy(slice).into_owned(),
        )
    } else {
        String::from("localhost")
    }
}

/// Format the current local time as `HH:MM`.
///
/// Uses `libc::localtime_r` (the reentrant POSIX API) for timezone-aware
/// local time. The two `unsafe` calls are sound because `localtime_r` writes
/// into a caller-owned `tm` struct and does not share mutable state.
#[allow(unsafe_code, reason = "localtime_r is the reentrant POSIX API; we own the tm struct")]
fn current_time_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());

    #[allow(clippy::cast_possible_wrap, reason = "current unix timestamp fits in i64 time_t")]
    let time_t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&raw const time_t, &raw mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// Tracks the "current" SGR state while emitting ANSI for a snapshot.
///
/// Allows diff-based emission: only emit a new SGR escape when the next cell's
/// attributes differ from the currently-active attributes, avoiding a full
/// `\x1b[0m` reset for every cell.
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors CellFlags — terminal SGR attributes are inherently boolean flags"
)]
struct SgrState {
    fg: scribe_common::screen::ScreenColor,
    bg: scribe_common::screen::ScreenColor,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
}

impl SgrState {
    /// Initial state: all flags off, colors are the terminal defaults
    /// (`Named(256)` = Foreground, `Named(257)` = Background in alacritty's
    /// `NamedColor` numbering).
    fn default_state() -> Self {
        Self {
            fg: scribe_common::screen::ScreenColor::Named(256),
            bg: scribe_common::screen::ScreenColor::Named(257),
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            hidden: false,
            strikethrough: false,
        }
    }

    /// Returns `true` if the cell's attributes exactly match the current state.
    fn matches(&self, cell: &scribe_common::screen::ScreenCell) -> bool {
        self.fg == cell.fg
            && self.bg == cell.bg
            && self.bold == cell.flags.bold
            && self.dim == cell.flags.dim
            && self.italic == cell.flags.italic
            && self.underline == cell.flags.underline
            && self.inverse == cell.flags.inverse
            && self.hidden == cell.flags.hidden
            && self.strikethrough == cell.flags.strikethrough
    }

    /// Update state to match the given cell's attributes.
    fn update(&mut self, cell: &scribe_common::screen::ScreenCell) {
        self.fg = cell.fg;
        self.bg = cell.bg;
        self.bold = cell.flags.bold;
        self.dim = cell.flags.dim;
        self.italic = cell.flags.italic;
        self.underline = cell.flags.underline;
        self.inverse = cell.flags.inverse;
        self.hidden = cell.flags.hidden;
        self.strikethrough = cell.flags.strikethrough;
    }
}

/// Convert a `ScreenSnapshot` to ANSI escape sequences that reproduce the
/// visible screen content when fed through a VTE parser.
///
/// Used to restore terminal content on reconnect: the server's `Term` has
/// the full state, and this converts it to bytes the client's `Term` can
/// process through the normal `pane.feed_output()` path.
fn snapshot_to_ansi(snapshot: &scribe_common::screen::ScreenSnapshot) -> Vec<u8> {
    use std::fmt::Write as _;

    let cols = usize::from(snapshot.cols);
    let scrollback_rows = snapshot.scrollback_rows as usize;
    let visible_rows = usize::from(snapshot.rows);

    let mut buf = String::with_capacity((scrollback_rows + visible_rows) * cols * 4);

    // If the server was in alternate screen mode, switch the client into it
    // so that subsequent PTY output (which assumes alt screen) lands in the
    // correct buffer.  Without this, apps like Claude Code that use alt screen
    // produce ghost cursors and broken exit behaviour after reconnect.
    if snapshot.alt_screen {
        buf.push_str("\x1b[?1049h");
    }

    // Hide cursor, move home, clear screen, reset attributes.
    buf.push_str("\x1b[?25l\x1b[H\x1b[2J\x1b[0m");

    let mut is_first_row = true;

    // SGR diff state: start from the known-reset state (we just emitted \x1b[0m
    // above), so the first cell will only emit SGR if it differs from defaults.
    let mut sgr_state = SgrState::default_state();

    // --- Scrollback lines (oldest first) ---
    // As these overflow the visible area, they naturally flow into the
    // client Term's scrollback buffer — the same mechanism as normal use.
    for row in 0..scrollback_rows {
        if !is_first_row {
            buf.push_str("\r\n");
        }
        is_first_row = false;
        write_snapshot_row(&mut buf, &snapshot.scrollback, row, cols, &mut sgr_state);
    }

    // --- Visible lines ---
    for row in 0..visible_rows {
        if !is_first_row {
            buf.push_str("\r\n");
        }
        is_first_row = false;
        write_snapshot_row(&mut buf, &snapshot.cells, row, cols, &mut sgr_state);
    }

    // Reset attributes, position cursor, show cursor if visible.
    buf.push_str("\x1b[0m");
    #[allow(clippy::let_underscore_must_use, reason = "write! to String is infallible")]
    let _ = write!(
        buf,
        "\x1b[{};{}H",
        u32::from(snapshot.cursor_row) + 1,
        u32::from(snapshot.cursor_col) + 1,
    );
    // For alt screen snapshots, leave the cursor hidden and skip DECSCUSR —
    // the alt screen app (e.g. Claude Code, vim) will control cursor
    // visibility and shape through its own live PTY output.  Emitting them
    // here causes a "double cursor": the terminal cursor overlaps with the
    // app's own drawn cursor.
    if !snapshot.alt_screen {
        if snapshot.cursor_visible {
            buf.push_str("\x1b[?25h");
        }
        // Restore cursor shape via DECSCUSR so reconnect preserves the style
        // that was active in the session (e.g. beam in a text editor).
        let decscusr = match snapshot.cursor_style {
            scribe_common::screen::CursorStyle::Block => "\x1b[2 q",
            scribe_common::screen::CursorStyle::Beam => "\x1b[6 q",
            scribe_common::screen::CursorStyle::Underline => "\x1b[4 q",
            scribe_common::screen::CursorStyle::HollowBlock => "\x1b[1 q",
        };
        buf.push_str(decscusr);
    }

    buf.into_bytes()
}

/// Write a single row of cells as ANSI escape sequences.
///
/// `sgr_state` tracks the currently-active SGR attributes across calls so that
/// unchanged runs of cells can skip emitting a redundant escape sequence.
fn write_snapshot_row(
    buf: &mut String,
    cells: &[scribe_common::screen::ScreenCell],
    row: usize,
    cols: usize,
    sgr_state: &mut SgrState,
) {
    for col in 0..cols {
        let idx = row * cols + col;
        let Some(cell) = cells.get(idx) else { break };

        // Skip spacer cells for wide characters.
        let is_wide_spacer =
            col > 0 && cells.get(row * cols + col - 1).is_some_and(|c| c.flags.wide);
        if is_wide_spacer {
            continue;
        }

        // Only emit SGR when this cell's attributes differ from the current
        // state.  Terminals preserve SGR across line breaks, so the state
        // carries over between rows without resetting.
        if !sgr_state.matches(cell) {
            write_sgr(buf, cell);
            sgr_state.update(cell);
        }

        // Write the character (space for null/empty cells).
        if cell.c == '\0' || cell.c == ' ' {
            buf.push(' ');
        } else {
            buf.push(cell.c);
        }
    }
}

/// Write SGR escape sequences for a cell's foreground, background, and flags.
fn write_sgr(buf: &mut String, cell: &scribe_common::screen::ScreenCell) {
    buf.push_str("\x1b[0"); // reset, then append attributes

    let f = &cell.flags;
    if f.bold {
        buf.push_str(";1");
    }
    if f.dim {
        buf.push_str(";2");
    }
    if f.italic {
        buf.push_str(";3");
    }
    if f.underline {
        buf.push_str(";4");
    }
    if f.inverse {
        buf.push_str(";7");
    }
    if f.hidden {
        buf.push_str(";8");
    }
    if f.strikethrough {
        buf.push_str(";9");
    }

    write_color_sgr(buf, cell.fg, true);
    write_color_sgr(buf, cell.bg, false);

    buf.push('m');
}

/// Append the SGR parameters for a single color (foreground or background).
///
/// `NamedColor` values: 0–7 = normal ANSI, 8–15 = bright ANSI,
/// 256 = Foreground, 257 = Background, 258 = Cursor, 259–266 = dim variants.
/// Values >= 16 use the terminal default colour (SGR 39/49).
#[allow(clippy::let_underscore_must_use, reason = "write! to String is infallible")]
fn write_color_sgr(buf: &mut String, color: scribe_common::screen::ScreenColor, foreground: bool) {
    use scribe_common::screen::ScreenColor;
    use std::fmt::Write as _;

    match color {
        ScreenColor::Named(n) if n < 8 => {
            let base: u32 = if foreground { 30 } else { 40 };
            let _ = write!(buf, ";{}", base + u32::from(n));
        }
        ScreenColor::Named(n) if n < 16 => {
            let base: u32 = if foreground { 90 } else { 100 };
            let _ = write!(buf, ";{}", base + u32::from(n - 8));
        }
        ScreenColor::Named(_) => {
            // Foreground (256), Background (257), Cursor (258), Dim* (259+)
            // — use the terminal's default colour.
            buf.push_str(if foreground { ";39" } else { ";49" });
        }
        ScreenColor::Indexed(idx) => {
            let prefix = if foreground { "38" } else { "48" };
            let _ = write!(buf, ";{prefix};5;{idx}");
        }
        ScreenColor::Rgb { r, g, b } => {
            let prefix = if foreground { "38" } else { "48" };
            let _ = write!(buf, ";{prefix};2;{r};{g};{b}");
        }
    }
}

/// Parse a `#RRGGBB` hex colour string into an `[f32; 4]` RGBA array.
///
/// Returns `None` if the string is not a valid 6-digit hex colour.
/// Convert sRGB ANSI color array to linear space for GPU rendering.
fn linearise_ansi_colors(ansi: &[[f32; 4]; 16]) -> [[f32; 4]; 16] {
    let mut out = *ansi;
    for c in &mut out {
        *c = scribe_renderer::srgb_to_linear_rgba(*c);
    }
    out
}

fn parse_hex_color(hex_str: &str) -> Option<[f32; 4]> {
    let hex = hex_str.strip_prefix('#').unwrap_or(hex_str);
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
    let green = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
    let blue = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;

    #[allow(
        clippy::cast_lossless,
        reason = "u8 to f32 is always lossless but clippy pedantic flags it"
    )]
    Some([red as f32 / 255.0, green as f32 / 255.0, blue as f32 / 255.0, 1.0])
}

// ---------------------------------------------------------------------------
// main()
// ---------------------------------------------------------------------------

/// Tell the settings process (if running) to quit.
fn quit_settings_process() {
    let socket_path = scribe_common::socket::settings_socket_path();
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
        use std::io::Write as _;
        if let Err(e) = stream.write_all(b"{\"cmd\":\"quit\"}\n") {
            tracing::warn!("failed to send quit command to settings: {e}");
        } else {
            tracing::debug!("sent quit to settings process");
        }
    }
}

/// Open the settings window or focus it if already running.
///
/// Tries to connect to the settings socket. If connected, sends a focus
/// command. If not, spawns the `scribe-settings` binary.
fn open_or_focus_settings() {
    let socket_path = scribe_common::socket::settings_socket_path();

    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
        use std::io::Write as _;
        if let Err(e) = stream.write_all(b"{\"cmd\":\"focus\"}\n") {
            tracing::warn!("failed to send focus command to settings: {e}");
        } else {
            tracing::debug!("sent focus to existing settings process");
        }
    } else {
        spawn_settings_process();
    }
}

/// Spawn the `scribe-settings` binary as a detached process.
fn spawn_settings_process() {
    let exe =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("scribe-settings"));
    let settings_exe = exe.with_file_name("scribe-settings");

    match std::process::Command::new(&settings_exe).spawn() {
        Ok(child) => {
            tracing::info!(pid = child.id(), "spawned settings process");
        }
        Err(e) => {
            tracing::warn!(exe = %settings_exe.display(), "failed to spawn settings: {e}");
        }
    }
}

/// Check if settings was open at last exit and restore it.
///
/// Reads the state file directly (read-only, no `StateStore` retained).
fn restore_settings_if_open() {
    #[derive(serde::Deserialize)]
    struct SettingsOpenCheck {
        #[serde(default)]
        open: bool,
    }

    let Some(state_dir) = dirs::state_dir() else {
        return;
    };
    let path = state_dir.join("scribe").join("settings_state.toml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };

    if let Ok(state) = toml::from_str::<SettingsOpenCheck>(&content) {
        if state.open {
            // Kill any stale settings process so the newly-installed binary
            // is used (e.g. after dpkg upgrade + server handoff).
            let socket_path = scribe_common::socket::settings_socket_path();
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
                use std::io::Write as _;
                drop(stream.write_all(b"{\"cmd\":\"quit\"}\n"));
                drop(stream);
                // Brief pause for the old process to exit and release the socket.
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            spawn_settings_process();
        }
    }
}

/// Spawn a new `scribe-client` process with the given window ID.
fn spawn_client_process(window_id: WindowId) {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("scribe-client"));
    let id_str = window_id.to_full_string();

    match std::process::Command::new(&exe).arg("--window-id").arg(&id_str).spawn() {
        Ok(child) => {
            tracing::info!(pid = child.id(), %window_id, "spawned new window process");
        }
        Err(e) => {
            tracing::warn!(exe = %exe.display(), %window_id, "failed to spawn window: {e}");
        }
    }
}

/// Parse `--window-id <uuid>` from CLI arguments.
fn parse_window_id() -> Option<WindowId> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args.get(i).map(String::as_str) == Some("--window-id") {
            if let Some(val) = args.get(i + 1) {
                return val.parse::<WindowId>().ok();
            }
        }
        i += 1;
    }
    None
}

#[allow(clippy::expect_used, reason = "event loop and wgpu instance creation are process-fatal")]
fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let event_loop =
        EventLoop::<UiEvent>::with_user_event().build().expect("failed to create event loop");

    let proxy = event_loop.create_proxy();

    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });

    let window_id = parse_window_id();
    let mut app = App::new(wgpu_instance, proxy, window_id);

    event_loop.run_app(&mut app).expect("event loop exited with error");
}

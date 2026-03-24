//! Scribe terminal client -- multi-pane winit + wgpu terminal emulator.

mod ai_indicator;
mod clipboard_cleanup;
mod close_dialog;
mod config;
mod divider;
mod input;
mod ipc_client;
mod layout;
mod pane;
mod scrollbar;
mod search_overlay;
mod selection;
mod splash;
mod status_bar;
mod tab_bar;
mod window_state;
mod workspace_layout;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use scribe_common::config::{ScribeConfig, resolve_theme};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
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
use crate::layout::{PaneId, Rect};
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

    /// Active scrollbar drag state (pane ID being dragged).
    scrollbar_drag_pane: Option<layout::PaneId>,

    // Text selection
    /// Active text selection, set on mouse press and extended on move.
    active_selection: Option<selection::SelectionRange>,
    /// Whether the left mouse button is currently held (for drag detection).
    mouse_selecting: bool,

    // Connection state
    /// Whether the IPC connection to the server is alive.
    server_connected: bool,

    // AI state
    ai_tracker: AiStateTracker,
    animation_running: bool,

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

    /// `true` after a workspace tree has been received from the server,
    /// suppressing the legacy `split_direction` fallback in
    /// `handle_workspace_info`.
    received_workspace_tree: bool,
    /// Clickable rect for the status bar gear icon (updated each frame).
    status_bar_gear_rect: Option<layout::Rect>,

    /// System hostname for the window-level status bar (fetched once at startup).
    hostname: String,

    /// Config file watcher -- kept alive for its side-effect of sending
    /// `UiEvent::ConfigChanged` events.
    #[allow(dead_code, reason = "watcher must be stored to keep receiving file-system events")]
    _config_watcher: Option<notify::RecommendedWatcher>,
}

impl App {
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
            scrollbar_drag_pane: None,
            active_selection: None,
            mouse_selecting: false,
            server_connected: false,
            ai_tracker: AiStateTracker::new(),
            animation_running: false,
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
            received_workspace_tree: false,
            status_bar_gear_rect: None,
            hostname: read_hostname(),
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
    }

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
            Ok(s) => {
                tracing::warn!("SPLASH DIAG: splash renderer created OK");
                Some(s)
            }
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
        let non_empty = snapshot.cells.iter().filter(|c| c.c != ' ' && c.c != '\0').count();
        let first_char = snapshot.cells.iter().find(|c| c.c != ' ' && c.c != '\0');
        tracing::info!(
            %session_id,
            cols = snapshot.cols,
            rows = snapshot.rows,
            cells = snapshot.cells.len(),
            non_empty,
            first_char = ?first_char.map(|c| c.c),
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
            tracing::warn!("SPLASH DIAG: screen snapshot arrived, marking content ready");
            self.splash_content_ready = true;
        }

        self.request_redraw();
    }

    fn handle_pty_output(&mut self, session_id: SessionId, bytes: &[u8]) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        pane.feed_output(bytes);

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

        let session_ids: Vec<SessionId> = sessions.iter().map(|s| s.session_id).collect();
        send_command(&tx, ClientCommand::AttachSessions { session_ids: session_ids.clone() });

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

        let first_pane = Pane::new(pane_rect, grid, first_sid, first_ws);
        self.panes.insert(first_pane_id, first_pane);
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
            let pane = Pane::new(pane_rect, grid, sid, ws_id);
            self.panes.insert(pane_id, pane);
            self.session_to_pane.insert(sid, pane_id);
        }

        // Subscribe to output from all restored sessions.
        send_command(&tx, ClientCommand::Subscribe { session_ids });

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
        let pane = Pane::new(pane_rect, grid, session_id, workspace_id);

        send_command(tx, ClientCommand::CreateSession { workspace_id, split_direction: None });
        self.panes.insert(pane_id, pane);
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);
        send_resize(tx, session_id, grid.cols, grid.rows);

        // Seed the server with the initial (single-leaf) tree.
        self.report_workspace_tree();
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
        let ws_rect = ws_rects.first().map_or(viewport, |(_wid, r)| *r);

        let tab = self.window_layout.active_tab()?;
        let pane_rects = tab.pane_layout.compute_rects(ws_rect);
        let &(pane_id, pane_rect) = pane_rects.first()?;
        let grid = pane::compute_pane_grid(pane_rect, cell.width, cell.height);
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

        // Check the active tab's pane layout for this pane.
        let can_close = self
            .window_layout
            .active_tab()
            .is_some_and(|tab| tab.pane_layout.all_pane_ids().len() > 1);

        if !can_close {
            // Only one pane in the active tab; remove the tab from the workspace.
            self.remove_tab_and_cleanup_workspace(ws_id, session_id);
            self.panes.remove(&pane_id);
            self.request_redraw();
            return;
        }

        self.panes.remove(&pane_id);

        if let Some(tab) = self.window_layout.active_tab_mut() {
            if tab.pane_layout.close_pane(pane_id) && tab.focused_pane == pane_id {
                tab.focused_pane = tab.pane_layout.next_pane(pane_id);
            }
        }

        self.resize_after_layout_change();
        self.request_redraw();
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

        if !self.ai_tracker.needs_animation() && !scrollbar_animating {
            self.animation_running = false;
            // Timer thread will see the flag and stop.
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

        self.config = new_config;

        tracing::info!("config hot-reloaded");
        self.request_redraw();
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
                tracing::warn!("SPLASH DIAG: dismissing splash (timer expired)");
                self.splash_active = false;
                gpu.splash = None;
            }
        }

        // -- Splash render -------------------------------------------------------
        if self.splash_active {
            tracing::warn!(
                renderer = gpu.splash.is_some(),
                content_ready = self.splash_content_ready,
                "SPLASH DIAG: rendering splash frame"
            );
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

        tracing::warn!(panes = self.panes.len(), "SPLASH DIAG: rendering terminal content");

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

            pane_rects.extend(tab.pane_layout.compute_rects(*ws_rect));
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
                Some((name, ws.accent_color))
            } else {
                None
            };

            ws_tab_bar_data.push(tab_bar::WorkspaceTabBarData { ws_rect: *ws_rect, tabs, badge });
        }

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

        let mut all_instances = build_all_instances(
            &mut gpu.renderer,
            &gpu.device,
            &gpu.queue,
            &mut self.panes,
            &pane_rects,
            &dividers,
            cell_size,
            focused_pane,
            &border_colors,
            &tab_colors,
            &ws_tab_bar_data,
            divider_color,
            accent_color,
            scrollbar_width,
            scrollbar_color,
            focus_split_direction,
            cursor_visible,
        );

        // Window-level status bar spanning the full window width.
        {
            let time_str = current_time_str();
            let sb_data = status_bar::StatusBarData {
                connected: self.server_connected,
                workspace_name: focused_ws_name.as_deref(),
                cwd: focused_pane_cwd.as_deref(),
                git_branch: focused_pane_git.as_deref(),
                session_count,
                hostname: &self.hostname,
                time: &time_str,
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
        let _grid = gpu.renderer.resize(&gpu.queue, (size.width, size.height));

        // Keep the splash uniform in sync so the logo stays centred.
        if let Some(splash) = &mut gpu.splash {
            splash.update_viewport(&gpu.queue, (size.width, size.height));
        }

        self.resize_all_workspace_panes();
        self.request_redraw();
    }

    /// Translate a keyboard event and forward it to the correct handler.
    fn handle_keyboard(&mut self, event: &winit::event::KeyEvent) {
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
        let Some(tx) = &self.cmd_tx else { return };
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };
        let sid = pane.session_id;

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
        // Extract focused pane from active tab (immutable borrow).
        let focused = match self.window_layout.active_tab() {
            Some(active) => active.focused_pane,
            None => return,
        };
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

        let new_rect = rects.iter().find(|(id, _)| *id == new_pane_id).map_or(ws_rect, |(_, r)| *r);

        let grid = pane::compute_pane_grid(new_rect, cell.width, cell.height);
        let pane = Pane::new(new_rect, grid, session_id, workspace_id);

        self.panes.insert(new_pane_id, pane);
        self.session_to_pane.insert(session_id, new_pane_id);
        self.pending_sessions.push_back(session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::CreateSession { workspace_id, split_direction: None });
        }

        self.resize_all_panes_from_rects(&rects);

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

        let grid = pane::compute_pane_grid(ws_rect, cell.width, cell.height);
        let pane = Pane::new(ws_rect, grid, session_id, new_workspace_id);

        self.panes.insert(pane_id, pane);
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(
                tx,
                ClientCommand::CreateSession {
                    workspace_id: new_workspace_id,
                    split_direction: Some(to_layout_direction(direction)),
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
        let workspace_id = self.window_layout.focused_workspace_id();
        let session_id = SessionId::new();

        let Some(pane_id) = self.window_layout.add_tab(workspace_id, session_id) else { return };

        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);
        let cell = gpu.renderer.cell_size();

        let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == workspace_id).map_or(ws_viewport, |(_, r)| *r);

        let grid = pane::compute_pane_grid(ws_rect, cell.width, cell.height);
        let pane = Pane::new(ws_rect, grid, session_id, workspace_id);

        self.panes.insert(pane_id, pane);
        self.session_to_pane.insert(session_id, pane_id);
        self.pending_sessions.push_back(session_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::CreateSession { workspace_id, split_direction: None });
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

    fn handle_next_tab(&mut self) {
        let ws_id = self.window_layout.focused_workspace_id();
        let Some(ws) = self.window_layout.focused_workspace() else { return };
        let next_idx = ws.next_tab_index();
        if self.window_layout.set_active_tab(ws_id, next_idx) {
            self.request_redraw();
        }
    }

    fn handle_prev_tab(&mut self) {
        let ws_id = self.window_layout.focused_workspace_id();
        let Some(ws) = self.window_layout.focused_workspace() else { return };
        let prev_idx = ws.prev_tab_index();
        if self.window_layout.set_active_tab(ws_id, prev_idx) {
            self.request_redraw();
        }
    }

    fn handle_select_tab(&mut self, index: usize) {
        let ws_id = self.window_layout.focused_workspace_id();
        if self.window_layout.set_active_tab(ws_id, index) {
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

        if text.is_empty() {
            return;
        }

        let Some(tx) = &self.cmd_tx else { return };
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };

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
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_down(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::PageDown);
        pane.scrollbar_state.on_scroll_action();
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_top(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        pane.scrollbar_state.on_scroll_action();
        self.ensure_animation_running();
        self.request_redraw();
    }

    fn handle_scroll_bottom(&mut self) {
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        pane.scrollbar_state.on_scroll_action();
        self.ensure_animation_running();
        self.request_redraw();
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "scroll delta is a small float value that fits in i32"
    )]
    fn handle_mouse_wheel(&mut self, delta: winit::event::MouseScrollDelta) {
        let lines = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => {
                // 3 terminal lines per scroll tick.
                -(y * 3.0) as i32
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
                -(y / cell_h).round() as i32
            }
        };

        if lines == 0 {
            return;
        }

        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get_mut(&tab.focused_pane) else { return };
        pane.term.scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        pane.scrollbar_state.on_scroll_action();
        self.ensure_animation_running();
        self.request_redraw();
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
        if button != winit::event::MouseButton::Left {
            return;
        }

        match state {
            winit::event::ElementState::Pressed => self.handle_mouse_press(),
            winit::event::ElementState::Released => {
                self.divider_drag = None;
                self.end_scrollbar_drag();
                self.handle_mouse_release();
            }
        }
    }

    /// Handle a left-button press: click-to-focus pane/workspace, or start a
    /// divider drag.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
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

        // Check for scrollbar click (before divider, before selection).
        if self.try_start_scrollbar_interaction(x, y) {
            return;
        }

        // Check for divider drag first (within the focused workspace).
        if self.try_start_divider_drag(x, y, &ws_rects) {
            return;
        }

        // Click-to-focus: find which pane the click landed in.
        self.focus_pane_at(x, y, &ws_rects);

        // Start a new text selection.
        self.start_selection(x, y);
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

        // Phase 1: read-only queries (immutable borrow of self.panes).
        let action = {
            let Some(pane) = self.panes.get(&focused_pane_id) else { return false };
            if !scrollbar::hit_test_scrollbar(pane, x, y, scrollbar_width) {
                return false;
            }

            let display_offset = pane.term.grid().display_offset();

            if scrollbar::hit_test_thumb(pane, x, y, scrollbar_width) {
                ScrollbarAction::StartDrag { display_offset }
            } else {
                let target = scrollbar::offset_from_track_click(pane, y, scrollbar_width);
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

        // Phase 1: read-only — compute the scroll delta.
        let delta = {
            let Some(pane) = self.panes.get(&pane_id) else { return };
            let Some(drag) = pane.scrollbar_state.drag.as_ref() else {
                self.scrollbar_drag_pane = None;
                return;
            };
            let target_offset = scrollbar::offset_from_drag(pane, drag, y, scrollbar_width);
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

        let Some(pane) = self.panes.get_mut(&focused_pane_id) else { return };
        let in_zone = scrollbar::hit_test_scrollbar(pane, x, y, scrollbar_width);

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
            let hit = pane_rects.iter().find(|(_, r)| r.contains(x, y));
            let Some((clicked_pane, _)) = hit else { continue };

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

    fn handle_cursor_moved(&mut self) {
        // Scrollbar drag takes highest priority.
        if let Some(pane_id) = self.scrollbar_drag_pane {
            self.handle_scrollbar_drag(pane_id);
            return;
        }

        // Extend active text selection while mouse is held.
        self.extend_selection();

        // Update scrollbar hover state for the focused pane.
        self.update_scrollbar_hover();

        // Divider drag.
        let Some(drag) = self.divider_drag else { return };
        let Some((x, y)) = self.last_cursor_pos else { return };

        let mouse_pos = match drag.direction {
            layout::SplitDirection::Horizontal => x,
            layout::SplitDirection::Vertical => y,
        };

        let new_ratio = divider::drag_ratio(&drag, mouse_pos);

        if let Some(tab) = self.window_layout.active_tab_mut() {
            let _ = tab.pane_layout.adjust_ratio(drag.first_pane, new_ratio - 0.5);
        }

        self.resize_after_layout_change();
        self.request_redraw();
    }

    // -------------------------------------------------------------------
    // Text selection helpers
    // -------------------------------------------------------------------

    /// Begin a new text selection at the given pixel position.
    fn start_selection(&mut self, x: f32, y: f32) {
        self.mouse_selecting = true;
        self.active_selection = None;
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        self.active_selection = Some(selection::SelectionRange { start: point, end: point });
    }

    /// Extend the in-progress selection to the current cursor position.
    fn extend_selection(&mut self) {
        if !self.mouse_selecting {
            return;
        }
        let Some(sel) = self.active_selection else { return };
        let Some((x, y)) = self.last_cursor_pos else { return };
        let Some(point) = self.cursor_to_grid(x, y) else { return };
        self.active_selection = Some(selection::SelectionRange { start: sel.start, end: point });
        self.request_redraw();
    }

    /// Finalize selection on mouse release and auto-copy if enabled.
    fn handle_mouse_release(&mut self) {
        self.mouse_selecting = false;
        if !self.config.terminal.copy_on_select {
            return;
        }
        self.finalize_copy();
    }

    /// Convert a pixel position to a grid cell in the focused pane.
    fn cursor_to_grid(&self, x: f32, y: f32) -> Option<selection::SelectionPoint> {
        let gpu = self.gpu.as_ref()?;
        let cell = gpu.renderer.cell_size();
        let pane_rect = self.focused_pane_rect()?;
        selection::pixel_to_grid(x, y, pane_rect, cell.width, cell.height)
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
        let (_, rect) = pane_rects.iter().find(|(pid, _)| *pid == tab.focused_pane)?;
        Some(*rect)
    }
}

// ---------------------------------------------------------------------------
// Layout / resize helpers
// ---------------------------------------------------------------------------

impl App {
    /// Resize all panes to their computed rects and notify the server.
    fn resize_all_panes_from_rects(&mut self, rects: &[(PaneId, Rect)]) {
        let Some(gpu) = &self.gpu else { return };
        let cell = gpu.renderer.cell_size();

        for (pane_id, rect) in rects {
            let Some(pane) = self.panes.get_mut(pane_id) else { continue };
            let grid = pane::compute_pane_grid(*rect, cell.width, cell.height);
            let new_grid = pane.resize(*rect, grid);
            let Some(tx) = &self.cmd_tx else { continue };
            send_resize(tx, pane.session_id, new_grid.cols, new_grid.rows);
        }
    }

    /// Recompute rects and resize all panes after a layout change.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
    )]
    fn resize_after_layout_change(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let ws_viewport = workspace_viewport(&gpu.surface_config);

        let rects = if let Some(tab) = self.window_layout.active_tab() {
            // Compute workspace rect for the focused workspace.
            let ws_rects = self.window_layout.compute_workspace_rects(ws_viewport);
            let ws_rect = ws_rects
                .iter()
                .find(|(wid, _)| *wid == self.window_layout.focused_workspace_id())
                .map_or(ws_viewport, |(_, r)| *r);
            tab.pane_layout.compute_rects(ws_rect)
        } else {
            Vec::new()
        };

        // Need to drop gpu borrow before calling resize_all_panes_from_rects
        // which borrows self mutably.
        self.resize_all_panes_from_rects(&rects);
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

        self.resize_all_panes_from_rects(&all_pane_rects);
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
    /// startup (fresh launch / restart without `--window-id`).
    ///
    /// Only size and maximized state are restored — position is left to
    /// the OS.  Position restoration is reserved for explicit `--window-id`
    /// launches (handled in `resumed()` via `saved_geometry`), where the
    /// user intentionally resumes a specific window.
    fn restore_geometry_from_registry(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
    ) {
        let loaded = self.window_registry.load(window_id);
        let has_saved = loaded.x.is_some() || loaded.maximized;
        let geom =
            if has_saved { Some(loaded) } else { self.window_registry.migrate_legacy(window_id) };
        if let (Some(mut geom), Some(window)) = (geom, &self.window) {
            // Clear position so the OS decides placement.
            geom.x = None;
            geom.y = None;
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
        self.flush_geometry_now();
        event_loop.exit();
    }

    /// User chose "Close this window only" — remove geometry file and exit.
    fn handle_close_window(&mut self, event_loop: &ActiveEventLoop) {
        tracing::info!("closing window permanently");
        // Remove the per-window geometry file since this window is gone.
        if let Some(wid) = self.window_id {
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
        self.last_tick = Instant::now();

        let Some(proxy) = self.animation_proxy.clone() else { return };
        std::thread::spawn(move || run_animation_loop(proxy));
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
fn run_animation_loop(proxy: EventLoopProxy<UiEvent>) {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(33));
        if proxy.send_event(UiEvent::AnimationTick).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Instance compositing
// ---------------------------------------------------------------------------

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

/// Dimming factor applied to RGB channels of unfocused pane content.
const UNFOCUSED_DIM: f32 = 0.85;

/// Collect all cell instances (tab bars + terminals + dividers + AI borders)
/// into one buffer.
#[allow(
    clippy::too_many_arguments,
    reason = "needs all render context: renderer, GPU resources, panes, layout data, AI state"
)]
#[allow(
    clippy::too_many_lines,
    reason = "single render-pass collector: tab bars, terminals, dividers, AI borders"
)]
fn build_all_instances(
    renderer: &mut TerminalRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    panes: &mut HashMap<PaneId, Pane>,
    pane_rects: &[(PaneId, Rect)],
    dividers: &[divider::Divider],
    cell_size: (f32, f32),
    focused_pane: PaneId,
    border_colors: &HashMap<PaneId, [f32; 4]>,
    tab_colors: &tab_bar::TabBarColors,
    ws_tab_bar_data: &[tab_bar::WorkspaceTabBarData],
    divider_color: [f32; 4],
    accent_color: [f32; 4],
    scrollbar_width: f32,
    scrollbar_color: [f32; 4],
    focus_split_direction: Option<layout::SplitDirection>,
    cursor_visible: bool,
) -> Vec<scribe_renderer::types::CellInstance> {
    // Pre-allocate based on a typical 80x24 grid per pane plus tab bar and
    // border quads, to avoid repeated reallocations during the per-pane loops.
    let estimated_per_pane = 80 * 24 + 80 + 4;
    let mut all_instances = Vec::with_capacity(pane_rects.len() * estimated_per_pane);

    // Tab bar backgrounds + bottom separator.
    for (_pane_id, pane_rect) in pane_rects {
        tab_bar::build_tab_bar_bg(&mut all_instances, *pane_rect, cell_size, tab_colors);
        tab_bar::build_tab_bar_separator(&mut all_instances, *pane_rect, cell_size, divider_color);
    }

    // Tab bar text — rendered once per workspace, spanning the full workspace width.
    for ws_data in ws_tab_bar_data {
        let tab_bar_rect = layout::Rect {
            x: ws_data.ws_rect.x,
            y: ws_data.ws_rect.y,
            width: ws_data.ws_rect.width,
            height: tab_bar::TAB_BAR_HEIGHT,
        };
        let badge = ws_data.badge.as_ref().map(|(name, color)| (name.as_str(), *color));
        let mut resolve_glyph = |ch: char| renderer.resolve_glyph(device, queue, ch);
        let mut params = tab_bar::TabBarTextParams {
            rect: tab_bar_rect,
            cell_size,
            tabs: &ws_data.tabs,
            badge,
            show_gear: false,
            colors: tab_colors,
            resolve_glyph: &mut resolve_glyph,
        };
        let (text_instances, _hit_targets) = tab_bar::build_tab_bar_text(&mut params);
        all_instances.extend(text_instances);
    }

    // Terminal content — dim unfocused panes by multiplying RGB (not alpha).
    let has_multiple_panes = pane_rects.len() > 1;
    for (pane_id, _) in pane_rects {
        if let Some(pane) = panes.get_mut(pane_id) {
            let offset = pane.content_offset();
            // Only the focused pane shows the blinking cursor; unfocused panes hide it.
            let pane_cursor_visible = *pane_id == focused_pane && cursor_visible;
            let mut instances = renderer.build_instances_at(
                device,
                queue,
                &mut pane.term,
                offset,
                pane_cursor_visible,
            );
            if has_multiple_panes && *pane_id != focused_pane {
                dim_instances(&mut instances);
            }
            all_instances.extend(instances);
        }
    }

    // Dividers.
    divider::build_divider_instances(&mut all_instances, dividers, cell_size, divider_color);

    // Scrollbar overlays.
    for (pane_id, _) in pane_rects {
        if let Some(pane) = panes.get(pane_id) {
            scrollbar::build_scrollbar_instances(
                &mut all_instances,
                pane,
                scrollbar_width,
                scrollbar_color,
            );
        }
    }

    // Focus border on the focused pane's leading edge.
    if has_multiple_panes {
        if let Some((_, focused_rect)) = pane_rects.iter().find(|(id, _)| *id == focused_pane) {
            divider::build_focus_border(
                &mut all_instances,
                *focused_rect,
                focus_split_direction,
                accent_color,
                cell_size,
            );
        }
    }

    // AI state border overlays (rendered last so they appear on top).
    for (pane_id, pane_rect) in pane_rects {
        if let Some(&color) = border_colors.get(pane_id) {
            let border = ai_indicator::build_border_instances(*pane_rect, color);
            all_instances.extend(border);
        }
    }

    all_instances
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
    let format = caps.formats.first().copied().ok_or(InitError::NoSurfaceFormat)?;
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

/// Convert a `ScreenSnapshot` to ANSI escape sequences that reproduce the
/// visible screen content when fed through a VTE parser.
///
/// Used to restore terminal content on reconnect: the server's `Term` has
/// the full state, and this converts it to bytes the client's `Term` can
/// process through the normal `pane.feed_output()` path.
fn snapshot_to_ansi(snapshot: &scribe_common::screen::ScreenSnapshot) -> Vec<u8> {
    use std::fmt::Write as _;

    let mut buf =
        String::with_capacity(usize::from(snapshot.cols) * usize::from(snapshot.rows) * 4);

    // If the server was in alternate screen mode, switch the client into it
    // so that subsequent PTY output (which assumes alt screen) lands in the
    // correct buffer.  Without this, apps like Claude Code that use alt screen
    // produce ghost cursors and broken exit behaviour after reconnect.
    if snapshot.alt_screen {
        buf.push_str("\x1b[?1049h");
    }

    // Hide cursor, move home, clear screen, reset attributes.
    buf.push_str("\x1b[?25l\x1b[H\x1b[2J\x1b[0m");

    let cols = usize::from(snapshot.cols);

    for row in 0..usize::from(snapshot.rows) {
        if row > 0 {
            buf.push_str("\r\n");
        }
        for col in 0..cols {
            let idx = row * cols + col;
            let Some(cell) = snapshot.cells.get(idx) else { break };

            // Skip spacer cells for wide characters.
            let is_wide_spacer =
                col > 0 && snapshot.cells.get(row * cols + col - 1).is_some_and(|c| c.flags.wide);
            if is_wide_spacer {
                continue;
            }

            // Build SGR (Select Graphic Rendition) sequence.
            write_sgr(&mut buf, cell);

            // Write the character (space for null/empty cells).
            if cell.c == '\0' || cell.c == ' ' {
                buf.push(' ');
            } else {
                buf.push(cell.c);
            }
        }
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
    if snapshot.cursor_visible {
        buf.push_str("\x1b[?25h");
    }

    buf.into_bytes()
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
            open_or_focus_settings();
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

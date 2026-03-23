//! Scribe terminal client -- multi-pane winit + wgpu terminal emulator.

mod ai_indicator;
mod config;
mod divider;
mod input;
mod ipc_client;
mod layout;
mod pane;
mod splash;
mod status_bar;
mod tab_bar;
mod workspace_layout;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use scribe_common::config::{ScribeConfig, resolve_theme};
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::theme::Theme;
use scribe_renderer::TerminalRenderer;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::ai_indicator::AiStateTracker;
use crate::divider::DividerDrag;
use crate::input::{KeyAction, LayoutAction};
use crate::ipc_client::{ClientCommand, UiEvent};
use crate::layout::{LayoutTree, PaneId, Rect};
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

/// Application state for the winit event loop.
struct App {
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

    // Divider drag
    divider_drag: Option<DividerDrag>,

    // AI state
    ai_tracker: AiStateTracker,
    animation_running: bool,

    // Input state
    modifiers: ModifiersState,

    /// Whether the splash screen is still showing.
    /// Set to `true` on init; cleared when the first `PtyOutput` arrives.
    splash_active: bool,

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

    /// Config file watcher -- kept alive for its side-effect of sending
    /// `UiEvent::ConfigChanged` events.
    #[allow(dead_code, reason = "watcher must be stored to keep receiving file-system events")]
    _config_watcher: Option<notify::RecommendedWatcher>,
}

impl App {
    fn new(wgpu_instance: wgpu::Instance, proxy: EventLoopProxy<UiEvent>) -> Self {
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

        Self {
            config,
            theme,
            window: None,
            gpu: None,
            cmd_tx: None,
            window_layout: WindowLayout::new(initial_workspace_id, Some(initial_accent)),
            panes: HashMap::new(),
            session_to_pane: HashMap::new(),
            divider_drag: None,
            ai_tracker: AiStateTracker::new(),
            animation_running: false,
            modifiers: ModifiersState::default(),
            splash_active: true,
            wgpu_instance,
            proxy: Some(proxy),
            animation_proxy: Some(animation_proxy),
            last_cursor_pos: None,
            last_tick: Instant::now(),
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
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::PtyOutput { session_id, data } => {
                self.handle_pty_output(session_id, &data);
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
            UiEvent::WorkspaceInfo { workspace_id, name, accent_color } => {
                self.handle_workspace_info(workspace_id, name, &accent_color);
            }
            UiEvent::WorkspaceNamed { workspace_id, name } => {
                self.handle_workspace_named(workspace_id, &name);
            }
            UiEvent::ConfigChanged => {
                self.handle_config_changed();
            }
            UiEvent::ServerDisconnected => {
                tracing::info!("server disconnected, exiting");
                event_loop.exit();
            }
            UiEvent::AnimationTick => {
                self.handle_animation_tick();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.handle_redraw(),
            WindowEvent::Resized(size) => self.handle_resize(size),
            WindowEvent::ModifiersChanged(new_mods) => {
                self.modifiers = new_mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_keyboard(&event),
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(state, button);
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
        let attrs = Window::default_attributes().with_title("Scribe");
        let window = Arc::new(event_loop.create_window(attrs).map_err(InitError::Window)?);

        let surface =
            self.wgpu_instance.create_surface(Arc::clone(&window)).map_err(InitError::Surface)?;

        let (device, queue, surface_config) =
            configure_device_and_surface(&self.wgpu_instance, &surface, &window)?;

        let size = window.inner_size();
        let mut renderer = TerminalRenderer::new(
            &device,
            &queue,
            surface_config.format,
            self.config.appearance.font_size,
            (size.width, size.height),
        );

        renderer.set_theme(&self.theme);

        let cell = renderer.cell_size();

        // Start IPC thread (proxy was created before run_app).
        let proxy = self.proxy.take().ok_or(InitError::ProxyConsumed)?;
        let cmd_tx = ipc_client::start_ipc_thread(proxy);

        // Create the initial pane within the initial workspace.
        let workspace_id = self.window_layout.focused_workspace_id();
        let session_id = SessionId::new();
        let initial_id = LayoutTree::initial_pane_id();
        let viewport_rect =
            Rect { x: 0.0, y: 0.0, width: size.width as f32, height: size.height as f32 };

        // Add a tab to the workspace and get its pane layout rect.
        self.window_layout.add_tab(workspace_id, session_id);

        let ws_rects = self.window_layout.compute_workspace_rects(viewport_rect);
        let ws_rect = ws_rects.first().map_or(viewport_rect, |(_wid, r)| *r);

        if let Some(tab) = self.window_layout.active_tab() {
            let pane_rects = tab.pane_layout.compute_rects(ws_rect);

            if let Some((_pane_id, pane_rect)) = pane_rects.first() {
                let grid = pane::compute_pane_grid(*pane_rect, cell.width, cell.height);
                let pane = Pane::new(*pane_rect, grid, session_id, workspace_id, initial_id);

                send_command(&cmd_tx, ClientCommand::CreateSession { workspace_id });

                self.panes.insert(initial_id, pane);
                self.session_to_pane.insert(session_id, initial_id);

                send_resize(&cmd_tx, session_id, grid.cols, grid.rows);
            }
        }

        let splash = match splash::SplashRenderer::new(
            &device,
            &queue,
            surface_config.format,
            (size.width, size.height),
        ) {
            Ok(s) => {
                tracing::debug!("splash screen initialised");
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
    fn handle_pty_output(&mut self, session_id: SessionId, bytes: &[u8]) {
        let Some(pane_id) = self.session_to_pane.get(&session_id).copied() else { return };
        let Some(pane) = self.panes.get_mut(&pane_id) else { return };
        pane.feed_output(bytes);

        // First PTY output dismisses the splash screen and drops the GPU
        // resources that were only needed for it.
        if self.splash_active {
            self.splash_active = false;
            if let Some(gpu) = &mut self.gpu {
                gpu.splash = None;
            }
        }

        self.request_redraw();
    }

    /// Handle server confirming session creation.
    fn handle_session_created(&mut self, session_id: SessionId) {
        tracing::info!(session = %session_id, "session created");

        let pane_to_bind = self.find_unconfirmed_pane();

        if let Some((pane_id, old_session_id)) = pane_to_bind {
            self.session_to_pane.remove(&old_session_id);
            self.session_to_pane.insert(session_id, pane_id);
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.session_id = session_id;
            }

            if let Some(tx) = &self.cmd_tx {
                send_command(tx, ClientCommand::Subscribe { session_ids: vec![session_id] });
            }
        }
    }

    /// Handle session exit.
    fn handle_session_exited(&mut self, session_id: SessionId) {
        tracing::info!(session = %session_id, "session exited");

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
            if let Some(wid) = ws_id {
                self.window_layout.remove_tab(wid, session_id);
            }
            self.panes.remove(&pane_id);
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

    /// Handle AI state change from server.
    fn handle_ai_state_changed(
        &mut self,
        session_id: SessionId,
        ai_state: scribe_common::ai_state::AiProcessState,
    ) {
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

        if !self.ai_tracker.needs_animation() {
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

    /// Handle full workspace info from server — update name and accent color.
    fn handle_workspace_info(
        &mut self,
        workspace_id: WorkspaceId,
        name: Option<String>,
        accent_color: &str,
    ) {
        tracing::debug!(%workspace_id, ?name, %accent_color, "workspace info received");
        if let Some(ws) = self.window_layout.find_workspace_mut(workspace_id) {
            ws.name = name;
            if let Some(color) = parse_hex_color(accent_color) {
                ws.accent_color = color;
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

    /// Reload config from disk and apply theme changes.
    fn handle_config_changed(&mut self) {
        let new_config = match scribe_common::config::load_config() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config reload failed: {e}");
                return;
            }
        };

        let new_theme = resolve_theme(&new_config);

        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.set_theme(&new_theme);
        }

        self.theme = new_theme;
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

        // While the splash is active, render the logo instead of the terminal.
        if self.splash_active {
            if let Some(splash) = &gpu.splash {
                let mut encoder =
                    gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("splash encoder"),
                    });
                splash.render(&mut encoder, &view);
                gpu.queue.submit(std::iter::once(encoder.finish()));
                frame.present();
                return;
            }
            // Splash renderer unavailable (decode failed): fall through to
            // normal rendering, which will produce a black frame via its own
            // clear colour.
        }

        let viewport = viewport_rect(&gpu.surface_config);
        let cell_size = (gpu.renderer.cell_size().width, gpu.renderer.cell_size().height);

        // Get pane rects and dividers from the active tab's layout tree.
        let (pane_rects, dividers, focused_pane, focus_split_direction) =
            if let Some(tab) = self.window_layout.active_tab() {
                // Compute the workspace rect for the focused workspace.
                let ws_rects = self.window_layout.compute_workspace_rects(viewport);
                let ws_rect = ws_rects
                    .iter()
                    .find(|(wid, _)| *wid == self.window_layout.focused_workspace_id())
                    .map_or(viewport, |(_, r)| *r);

                let rects = tab.pane_layout.compute_rects(ws_rect);
                let divs = divider::collect_dividers(tab.pane_layout.root(), ws_rect);
                let fp = tab.focused_pane;
                let fsd = tab.pane_layout.parent_split_direction(fp);
                (rects, divs, fp, fsd)
            } else {
                (Vec::new(), Vec::new(), LayoutTree::initial_pane_id(), None)
            };

        let linear_ansi = linearise_ansi_colors(&self.theme.ansi_colors);
        let ansi_colors = &linear_ansi;
        let ai_accent = scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.accent);
        let border_colors: HashMap<PaneId, [f32; 4]> = pane_rects
            .iter()
            .filter_map(|(pane_id, _)| {
                let pane = self.panes.get(pane_id)?;
                let color =
                    self.ai_tracker.border_color(pane.session_id, ansi_colors, ai_accent)?;
                Some((*pane_id, color))
            })
            .collect();

        let tab_colors = tab_bar::TabBarColors::from(&self.theme.chrome);
        let sb_colors = status_bar::StatusBarColors::from_theme(&self.theme.chrome, ansi_colors);
        let pane_count = self.panes.len();
        let divider_color = scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.divider);
        let accent_color = scribe_renderer::srgb_to_linear_rgba(self.theme.chrome.accent);

        let all_instances = build_all_instances(
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
            &sb_colors,
            pane_count,
            divider_color,
            accent_color,
            focus_split_direction,
        );

        gpu.renderer.pipeline_mut().update_instances(&gpu.device, &gpu.queue, &all_instances);

        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("multi-pane encoder"),
        });

        let clear_color = gpu.renderer.default_bg();
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

        self.resize_after_layout_change();
        self.request_redraw();
    }

    /// Translate a keyboard event and forward it to the correct handler.
    fn handle_keyboard(&mut self, event: &winit::event::KeyEvent) {
        let Some(action) = input::translate_key_action(event, self.modifiers) else {
            return;
        };

        match action {
            KeyAction::Terminal(bytes) => self.handle_terminal_key(bytes),
            KeyAction::Layout(layout_action) => self.handle_layout_action(layout_action),
            KeyAction::OpenSettings => self.open_settings(),
        }
    }

    fn handle_terminal_key(&self, bytes: Vec<u8>) {
        let Some(tx) = &self.cmd_tx else { return };
        let Some(tab) = self.window_layout.active_tab() else { return };
        let Some(pane) = self.panes.get(&tab.focused_pane) else { return };

        if tx.send(ClientCommand::KeyInput { session_id: pane.session_id, data: bytes }).is_err() {
            tracing::warn!("IPC channel closed; keyboard input dropped");
        }
    }

    fn handle_layout_action(&mut self, action: LayoutAction) {
        match action {
            LayoutAction::SplitVertical => {
                self.handle_split(layout::SplitDirection::Horizontal);
            }
            LayoutAction::SplitHorizontal => {
                self.handle_split(layout::SplitDirection::Vertical);
            }
            LayoutAction::ClosePane => self.handle_close_pane(),
            LayoutAction::FocusNext => self.handle_focus_next(),
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
        let viewport = viewport_rect(&gpu.surface_config);
        let session_id = SessionId::new();
        let cell = gpu.renderer.cell_size();

        // Compute workspace rect.
        let ws_rects = self.window_layout.compute_workspace_rects(viewport);
        let ws_rect =
            ws_rects.iter().find(|(wid, _)| *wid == workspace_id).map_or(viewport, |(_, r)| *r);

        // Compute pane rects from the updated layout (immutable borrow).
        let rects = match self.window_layout.active_tab() {
            Some(active) => active.pane_layout.compute_rects(ws_rect),
            None => return,
        };

        let new_rect = rects.iter().find(|(id, _)| *id == new_pane_id).map_or(ws_rect, |(_, r)| *r);

        let grid = pane::compute_pane_grid(new_rect, cell.width, cell.height);
        let pane = Pane::new(new_rect, grid, session_id, workspace_id, new_pane_id);

        self.panes.insert(new_pane_id, pane);
        self.session_to_pane.insert(session_id, new_pane_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::CreateSession { workspace_id });
        }

        self.resize_all_panes_from_rects(&rects);

        if let Some(active) = self.window_layout.active_tab_mut() {
            active.focused_pane = new_pane_id;
        }
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
    // Settings
    // -----------------------------------------------------------------------

    /// Open the settings webview window on a background thread.
    ///
    /// Serializes the current config as JSON and passes it to the settings UI.
    /// Changes made in the settings UI trigger a config file write, which the
    /// file watcher picks up and sends `ConfigChanged` back to the main loop.
    fn open_settings(&self) {
        let config_json = serde_json::to_string(&self.config).unwrap_or_else(|e| {
            tracing::warn!("failed to serialize config for settings: {e}");
            String::from("{}")
        });

        if let Err(e) = scribe_settings::open_settings_window(config_json, |change_json| {
            tracing::debug!("settings change: {change_json}");

            // Parse the change and apply it to the config file.
            // The file watcher will pick up the change and trigger a reload.
            if let Err(apply_err) = apply_settings_change(&change_json) {
                tracing::warn!("failed to apply settings change: {apply_err}");
            }
        }) {
            tracing::warn!("failed to open settings window: {e}");
        }
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
            winit::event::ElementState::Pressed => {
                let Some((x, y)) = self.last_cursor_pos else { return };
                let Some(gpu) = &self.gpu else { return };
                let viewport = viewport_rect(&gpu.surface_config);

                let Some(tab) = self.window_layout.active_tab() else { return };
                let ws_rects = self.window_layout.compute_workspace_rects(viewport);
                let ws_rect = ws_rects
                    .iter()
                    .find(|(wid, _)| *wid == self.window_layout.focused_workspace_id())
                    .map_or(viewport, |(_, r)| *r);

                let dividers = divider::collect_dividers(tab.pane_layout.root(), ws_rect);

                if let Some(hit) = divider::hit_test_divider(&dividers, x, y) {
                    self.divider_drag = Some(divider::start_drag(hit, ws_rect));
                }
            }
            winit::event::ElementState::Released => {
                self.divider_drag = None;
            }
        }
    }

    fn handle_cursor_moved(&mut self) {
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
        let viewport = viewport_rect(&gpu.surface_config);

        let rects = if let Some(tab) = self.window_layout.active_tab() {
            // Compute workspace rect for the focused workspace.
            let ws_rects = self.window_layout.compute_workspace_rects(viewport);
            let ws_rect = ws_rects
                .iter()
                .find(|(wid, _)| *wid == self.window_layout.focused_workspace_id())
                .map_or(viewport, |(_, r)| *r);
            tab.pane_layout.compute_rects(ws_rect)
        } else {
            Vec::new()
        };

        // Need to drop gpu borrow before calling resize_all_panes_from_rects
        // which borrows self mutably.
        self.resize_all_panes_from_rects(&rects);
    }

    /// Find a pane whose session has not been confirmed by the server yet.
    fn find_unconfirmed_pane(&self) -> Option<(PaneId, SessionId)> {
        self.panes
            .values()
            .find(|p| self.session_to_pane.get(&p.session_id).is_some_and(|pid| *pid == p.id))
            .map(|p| (p.id, p.session_id))
    }

    /// Request a redraw from winit.
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
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

/// Dimming factor applied to RGB channels of unfocused pane content.
const UNFOCUSED_DIM: f32 = 0.85;

/// Collect all cell instances (tab bars + terminals + dividers + AI borders)
/// into one buffer.
#[allow(
    clippy::too_many_arguments,
    reason = "needs all render context: renderer, GPU resources, panes, layout data, AI state"
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
    sb_colors: &status_bar::StatusBarColors,
    pane_count: usize,
    divider_color: [f32; 4],
    accent_color: [f32; 4],
    focus_split_direction: Option<layout::SplitDirection>,
) -> Vec<scribe_renderer::types::CellInstance> {
    // Pre-allocate based on a typical 80x24 grid per pane plus tab bar and
    // border quads, to avoid repeated reallocations during the per-pane loops.
    let estimated_per_pane = 80 * 24 + 80 + 4;
    let mut all_instances = Vec::with_capacity(pane_rects.len() * estimated_per_pane);

    // Tab bar backgrounds.
    for (_pane_id, pane_rect) in pane_rects {
        tab_bar::build_tab_bar_bg(&mut all_instances, *pane_rect, cell_size, tab_colors);
    }

    // Terminal content — dim unfocused panes by multiplying RGB (not alpha).
    let has_multiple_panes = pane_rects.len() > 1;
    for (pane_id, _) in pane_rects {
        if let Some(pane) = panes.get_mut(pane_id) {
            let offset = pane.content_offset();
            let mut instances = renderer.build_instances_at(device, queue, &mut pane.term, offset);
            if has_multiple_panes && *pane_id != focused_pane {
                dim_instances(&mut instances);
            }
            all_instances.extend(instances);
        }
    }

    // Dividers.
    divider::build_divider_instances(&mut all_instances, dividers, cell_size, divider_color);

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

    // Status bars — collect owned data first to avoid borrow conflicts
    // between pane references and the mutable renderer.
    let sb_data: Vec<_> = pane_rects
        .iter()
        .filter_map(|(pane_id, pane_rect)| {
            let pane = panes.get(pane_id)?;
            Some((
                *pane_rect,
                pane.title.clone(),
                pane.git_branch.clone(),
                (pane.grid.cols, pane.grid.rows),
            ))
        })
        .collect();

    for (pane_rect, title, git_branch, grid_size) in &sb_data {
        let data = status_bar::StatusBarData {
            connected: true,
            shell_name: title,
            pane_count,
            git_branch: git_branch.as_deref(),
            grid_size: *grid_size,
        };
        let mut resolve_glyph = |ch: char| renderer.resolve_glyph(device, queue, ch);
        status_bar::build_status_bar(
            &mut all_instances,
            *pane_rect,
            cell_size,
            sb_colors,
            &data,
            &mut resolve_glyph,
        );
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

/// Request adapter, create device + queue, and configure the surface.
fn configure_device_and_surface(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'_>,
    window: &Window,
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
    let alpha_mode = caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto);

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

/// Build a viewport `Rect` from the surface configuration.
#[allow(clippy::cast_precision_loss, reason = "viewport dimensions are small enough to fit in f32")]
fn viewport_rect(config: &wgpu::SurfaceConfiguration) -> Rect {
    Rect { x: 0.0, y: 0.0, width: config.width as f32, height: config.height as f32 }
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
// Settings change application
// ---------------------------------------------------------------------------

/// Apply a single settings change from the webview to the config file.
///
/// Parses the JSON change message, loads the current config, applies the
/// change, and writes the updated config back. The file watcher will detect
/// the change and trigger a `ConfigChanged` event.
fn apply_settings_change(change_json: &str) -> Result<(), String> {
    let msg: serde_json::Value =
        serde_json::from_str(change_json).map_err(|e| format!("invalid JSON: {e}"))?;

    let key = msg
        .get("key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| String::from("missing 'key' field"))?;

    let value = msg.get("value").ok_or_else(|| String::from("missing 'value' field"))?;

    let mut config =
        scribe_common::config::load_config().map_err(|e| format!("failed to load config: {e}"))?;

    apply_config_key(&mut config, key, value)?;

    scribe_common::config::save_config(&config).map_err(|e| format!("failed to save config: {e}"))
}

/// Apply a single dotted key + value to the config struct.
#[allow(clippy::too_many_lines, reason = "exhaustive key matching requires one arm per setting")]
fn apply_config_key(
    config: &mut scribe_common::config::ScribeConfig,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match key {
        // -- Appearance -------------------------------------------------------
        "appearance.font_family" => {
            value
                .as_str()
                .ok_or("font_family must be a string")?
                .clone_into(&mut config.appearance.font);
        }
        "appearance.font_size" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "font_size is a small positive float"
            )]
            let v = value.as_f64().ok_or("font_size must be a number")? as f32;
            config.appearance.font_size = v;
        }
        "appearance.font_weight" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "font weight is a small positive integer (100-900)"
            )]
            let v = value.as_f64().ok_or("font_weight must be a number")? as u16;
            config.appearance.font_weight = v;
        }
        "appearance.bold_weight" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "bold weight is a small positive integer (100-900)"
            )]
            let v = value.as_f64().ok_or("bold_weight must be a number")? as u16;
            config.appearance.font_weight_bold = v;
        }
        "appearance.ligatures" => {
            config.appearance.ligatures = value.as_bool().ok_or("ligatures must be a boolean")?;
        }
        "appearance.line_padding" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "line padding is a small non-negative integer"
            )]
            let v = value.as_f64().ok_or("line_padding must be a number")? as u16;
            config.appearance.line_padding = v;
        }
        "appearance.cursor_shape" => {
            let shape_str = value.as_str().ok_or("cursor_shape must be a string")?;
            let shape: scribe_common::config::CursorShape =
                serde_json::from_value(serde_json::Value::String(shape_str.to_owned()))
                    .map_err(|e| format!("invalid cursor shape: {e}"))?;
            config.appearance.cursor_shape = shape;
        }
        "appearance.cursor_blink" => {
            config.appearance.cursor_blink =
                value.as_bool().ok_or("cursor_blink must be a boolean")?;
        }
        "appearance.opacity" => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "opacity is a float between 0.0 and 1.0"
            )]
            let v = value.as_f64().ok_or("opacity must be a number")? as f32;
            config.appearance.opacity = v;
        }
        // -- Theme preset -----------------------------------------------------
        "theme.preset" => {
            let preset = value.as_str().ok_or("theme preset must be a string")?;
            // Convert preset name: "minimal_dark" -> "minimal-dark"
            config.appearance.theme = preset.replace('_', "-");
        }
        // -- Terminal ---------------------------------------------------------
        "terminal.scrollback_lines" => {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "scrollback is a non-negative integer within u32 range"
            )]
            let v = value.as_f64().ok_or("scrollback_lines must be a number")? as u32;
            config.terminal.scrollback_lines = v;
        }
        "terminal.shell" => {
            value.as_str().ok_or("shell must be a string")?.clone_into(&mut config.terminal.shell);
        }
        // -- Workspaces -------------------------------------------------------
        "workspaces.add_root" => {
            // The webview sends an empty string as a placeholder; in a real
            // implementation a file picker dialog would provide the path.
            tracing::debug!("workspace add_root requested (file picker not yet implemented)");
        }
        "workspaces.remove_root" => {
            let path = value.as_str().ok_or("remove_root value must be a string")?;
            config.workspaces.roots.retain(|r| r != path);
        }
        _ => {
            tracing::debug!(key, "unhandled settings key");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// main()
// ---------------------------------------------------------------------------

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

    let mut app = App::new(wgpu_instance, proxy);

    event_loop.run_app(&mut app).expect("event loop exited with error");
}

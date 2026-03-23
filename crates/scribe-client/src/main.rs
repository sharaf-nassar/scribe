//! Scribe terminal client -- multi-pane winit + wgpu terminal emulator.

mod ai_indicator;
mod divider;
mod input;
mod ipc_client;
mod layout;
mod pane;
mod splash;
mod tab_bar;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use scribe_common::ids::{SessionId, WorkspaceId};
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

/// Default font size for the terminal renderer (points).
const FONT_SIZE: f32 = 14.0;

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
    // Window + GPU
    window: Option<Arc<Window>>,
    gpu: Option<GpuContext>,

    // IPC
    cmd_tx: Option<Sender<ClientCommand>>,

    // Layout
    layout: LayoutTree,
    panes: HashMap<PaneId, Pane>,
    session_to_pane: HashMap<SessionId, PaneId>,
    focused_pane: PaneId,

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
}

impl App {
    fn new(wgpu_instance: wgpu::Instance, proxy: EventLoopProxy<UiEvent>) -> Self {
        let animation_proxy = proxy.clone();
        Self {
            window: None,
            gpu: None,
            cmd_tx: None,
            layout: LayoutTree::new(),
            panes: HashMap::new(),
            session_to_pane: HashMap::new(),
            focused_pane: LayoutTree::initial_pane_id(),
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
        let renderer = TerminalRenderer::new(
            &device,
            &queue,
            surface_config.format,
            FONT_SIZE,
            (size.width, size.height),
        );

        let cell = renderer.cell_size();

        // Start IPC thread (proxy was created before run_app).
        let proxy = self.proxy.take().ok_or(InitError::ProxyConsumed)?;
        let cmd_tx = ipc_client::start_ipc_thread(proxy);

        // Create the initial pane.
        let initial_id = LayoutTree::initial_pane_id();
        let viewport_rect =
            Rect { x: 0.0, y: 0.0, width: size.width as f32, height: size.height as f32 };

        let rects = self.layout.compute_rects(viewport_rect);

        if let Some((_pane_id, pane_rect)) = rects.first() {
            let workspace_id = WorkspaceId::new();
            let session_id = SessionId::new();
            let grid = pane::compute_pane_grid(*pane_rect, cell.width, cell.height);

            let pane = Pane::new(*pane_rect, grid, session_id, workspace_id, initial_id);

            send_command(&cmd_tx, ClientCommand::CreateSession { workspace_id });

            self.panes.insert(initial_id, pane);
            self.session_to_pane.insert(session_id, initial_id);
            self.focused_pane = initial_id;

            send_resize(&cmd_tx, session_id, grid.cols, grid.rows);
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

        if self.layout.all_pane_ids().len() <= 1 {
            return;
        }

        self.panes.remove(&pane_id);
        if self.layout.close_pane(pane_id) && self.focused_pane == pane_id {
            self.focused_pane = self.layout.next_pane(pane_id);
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

    /// Render one frame: splash while waiting for PTY output, terminal after.
    #[allow(
        clippy::cast_precision_loss,
        reason = "viewport dimensions are small enough to fit in f32"
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
        let pane_rects = self.layout.compute_rects(viewport);
        let focused_pane = self.focused_pane;
        let dividers = divider::collect_dividers(self.layout.root(), viewport);
        let cell_size = (gpu.renderer.cell_size().width, gpu.renderer.cell_size().height);

        // Collect per-pane AI border colours.
        let border_colors: HashMap<PaneId, [f32; 4]> = pane_rects
            .iter()
            .filter_map(|(pane_id, _)| {
                let pane = self.panes.get(pane_id)?;
                let color = self.ai_tracker.border_color(pane.session_id)?;
                Some((*pane_id, color))
            })
            .collect();

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
        );

        gpu.renderer.pipeline_mut().update_instances(&gpu.device, &gpu.queue, &all_instances);

        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("multi-pane encoder"),
        });

        gpu.renderer.pipeline_mut().render(&mut encoder, &view);
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
        }
    }

    fn handle_terminal_key(&self, bytes: Vec<u8>) {
        let Some(tx) = &self.cmd_tx else { return };
        let Some(pane) = self.panes.get(&self.focused_pane) else { return };

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
        let focused = self.focused_pane;
        let Some(new_pane_id) = self.layout.split_pane(focused, direction) else { return };
        let Some(gpu) = &self.gpu else { return };

        let viewport = viewport_rect(&gpu.surface_config);
        let rects = self.layout.compute_rects(viewport);

        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let cell = gpu.renderer.cell_size();

        let new_rect =
            rects.iter().find(|(id, _)| *id == new_pane_id).map_or(viewport, |(_, r)| *r);

        let grid = pane::compute_pane_grid(new_rect, cell.width, cell.height);
        let pane = Pane::new(new_rect, grid, session_id, workspace_id, new_pane_id);

        self.panes.insert(new_pane_id, pane);
        self.session_to_pane.insert(session_id, new_pane_id);

        if let Some(tx) = &self.cmd_tx {
            send_command(tx, ClientCommand::CreateSession { workspace_id });
        }

        self.resize_all_panes_from_rects(&rects);
        self.focused_pane = new_pane_id;
        self.request_redraw();
    }

    fn handle_close_pane(&mut self) {
        let pane_id = self.focused_pane;
        if self.layout.all_pane_ids().len() <= 1 {
            return;
        }

        if !self.layout.close_pane(pane_id) {
            return;
        }

        if let Some(pane) = self.panes.remove(&pane_id) {
            self.session_to_pane.remove(&pane.session_id);
            if let Some(tx) = &self.cmd_tx {
                send_command(tx, ClientCommand::CloseSession { session_id: pane.session_id });
            }
        }

        self.focused_pane = self.layout.next_pane(pane_id);
        self.resize_after_layout_change();
        self.request_redraw();
    }

    fn handle_focus_next(&mut self) {
        let current = self.focused_pane;
        self.focused_pane = self.layout.next_pane(current);
        tracing::debug!(from = %current, to = %self.focused_pane, "focus cycled");
        self.request_redraw();
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
                let dividers = divider::collect_dividers(self.layout.root(), viewport);

                if let Some(hit) = divider::hit_test_divider(&dividers, x, y) {
                    self.divider_drag = Some(divider::start_drag(hit, viewport));
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
        let _ = self.layout.adjust_ratio(drag.first_pane, new_ratio - 0.5);

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
    fn resize_after_layout_change(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let viewport = viewport_rect(&gpu.surface_config);
        let rects = self.layout.compute_rects(viewport);
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
) -> Vec<scribe_renderer::types::CellInstance> {
    // Pre-allocate based on a typical 80x24 grid per pane plus tab bar and
    // border quads, to avoid repeated reallocations during the per-pane loops.
    let estimated_per_pane = 80 * 24 + 80 + 4;
    let mut all_instances = Vec::with_capacity(pane_rects.len() * estimated_per_pane);

    // Tab bar backgrounds.
    for (pane_id, pane_rect) in pane_rects {
        let focused = *pane_id == focused_pane;
        tab_bar::build_tab_bar_bg(&mut all_instances, *pane_rect, cell_size, focused);
    }

    // Terminal content.
    for (pane_id, _) in pane_rects {
        if let Some(pane) = panes.get_mut(pane_id) {
            let offset = pane.content_offset();
            let instances = renderer.build_instances_at(device, queue, &mut pane.term, offset);
            all_instances.extend(instances);
        }
    }

    // Dividers.
    divider::build_divider_instances(&mut all_instances, dividers, cell_size);

    // AI state border overlays (rendered last so they appear on top).
    for (pane_id, pane_rect) in pane_rects {
        if let Some(&color) = border_colors.get(pane_id) {
            let border = ai_indicator::build_border_instances(*pane_rect, color);
            all_instances.extend(border);
        }
    }

    all_instances
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
